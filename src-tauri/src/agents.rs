//! Agents launched by voice into any repo, tracked in a job registry.
//!
//! Two ways to run one, by `backend`:
//!  * Conductor: a headless `claude --print --output-format stream-json
//!    --verbose` subprocess. We get incremental assistant text (which survives a
//!    timeout kill) plus an explicit terminal envelope telling us whether it
//!    actually succeeded — and that ANSWER is what the announcement pipeline
//!    speaks.
//!  * Orca: `orca agent launch` opens the agent as a tab in Orca. Orca owns the
//!    process, so there is no child to supervise here; the watcher announces the
//!    answer when the agent finishes, exactly as for any agent the user started.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

use crate::targets::{Target, TargetSource};
use crate::workspace::Backend;
use crate::AppState;

/// Each agent is a full Claude Code process (node runtime, MCP servers, file
/// watchers) and they all contend on one rate limit. Four already saturates a
/// laptop that is also running Ollama and a speech model.
fn max_agents() -> usize {
    std::env::var("VOX_MAX_AGENTS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(4)
}

/// Real coding work takes minutes, not seconds. Overridable for demos via
/// VOX_AGENT_TIMEOUT (seconds).
fn agent_timeout() -> Duration {
    let secs = std::env::var("VOX_AGENT_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(600);
    Duration::from_secs(secs)
}

/// Grace period before an agent aimed at a NON-active target actually starts.
/// ⌥Space during it cancels the launch. Two seconds against a ten-minute task
/// is free, and it reuses interrupt_gen — no extra conversational turn.
const CANCEL_WINDOW: Duration = Duration::from_secs(2);

/// Cap on captured stdout. Well past any spoken answer; the reader keeps
/// draining past it (see `read_stream`) so the pipe can never fill.
const MAX_CAPTURE: usize = 256 * 1024;

// ── Job registry ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AgentPhase {
    /// Slot reserved: inside the cancel window, or drafting the full prompt.
    Drafting,
    Running,
    Done,
}

impl AgentPhase {
    fn as_str(self) -> &'static str {
        match self {
            AgentPhase::Drafting => "drafting",
            AgentPhase::Running => "running",
            AgentPhase::Done => "done",
        }
    }
}

#[derive(Clone, Debug)]
pub struct AgentJob {
    pub id: u64,
    /// Spoken name: "findy", "orivo (porto)".
    pub label: String,
    pub repo: String,
    pub path: String,
    /// The short task, so "what was it doing?" can be answered.
    pub task: String,
    pub phase: AgentPhase,
    pub started: Instant,
    pub pid: Option<u32>,
}

#[derive(Debug)]
pub enum LaunchError {
    NoCli,
    /// Orca backend, but its CLI is missing or its runtime isn't answering.
    NoOrca,
    BadCwd(String),
    TooMany(usize),
    Cancelled,
    SpawnFailed(String),
}

/// How an agent's run ended. The distinction between `Failed` and `TimedOut`
/// only exists because the wait loop now keeps the exit status and a
/// `timed_out` flag — previously a kill and a clean exit were indistinguishable.
#[derive(Clone, Debug)]
pub enum AgentOutcome {
    Success { text: String },
    Empty,
    Failed { code: i32, detail: String },
    TimedOut { partial: Option<String> },
    SpawnError { detail: String },
}

impl AgentOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentOutcome::Success { .. } => "success",
            AgentOutcome::Empty => "empty",
            AgentOutcome::Failed { .. } => "failed",
            AgentOutcome::TimedOut { .. } => "timeout",
            AgentOutcome::SpawnError { .. } => "spawn_error",
        }
    }

    /// The agent's own words, if it produced any.
    pub fn text(&self) -> Option<&str> {
        match self {
            AgentOutcome::Success { text } => Some(text),
            AgentOutcome::TimedOut { partial } => partial.as_deref(),
            _ => None,
        }
    }
}

pub fn emit_agents(app: &AppHandle, state: &Arc<AppState>) {
    let jobs = state.agents.lock().unwrap();
    let list: Vec<Value> = jobs
        .iter()
        .map(|j| {
            json!({
                "id": j.id,
                "label": j.label,
                "phase": j.phase.as_str(),
                "task": j.task.chars().take(80).collect::<String>(),
                "elapsed_s": j.started.elapsed().as_secs(),
            })
        })
        .collect();
    // With Orca, agents run outside Vox: the badge also counts what the watcher
    // saw working there (Vox's own pending launches are in `list` already).
    let in_orca = if crate::workspace::backend() == Backend::Orca {
        state.orca_working.load(Ordering::SeqCst)
    } else {
        0
    };
    // A SUPERSET of the old payload: the renderer only reads `count`, so this
    // stays backward compatible with an un-updated webview.
    let _ = app.emit("agent-status", json!({ "count": list.len() + in_orca, "agents": list }));
}

fn set_phase(app: &AppHandle, state: &Arc<AppState>, id: u64, phase: AgentPhase) {
    {
        let mut jobs = state.agents.lock().unwrap();
        if let Some(j) = jobs.iter_mut().find(|j| j.id == id) {
            j.phase = phase;
        }
    }
    emit_agents(app, state);
}

fn set_pid(state: &Arc<AppState>, id: u64, pid: Option<u32>) {
    let mut jobs = state.agents.lock().unwrap();
    if let Some(j) = jobs.iter_mut().find(|j| j.id == id) {
        j.pid = pid;
    }
}

fn remove_job(app: &AppHandle, state: &Arc<AppState>, id: u64) -> Option<AgentJob> {
    let job = {
        let mut jobs = state.agents.lock().unwrap();
        let idx = jobs.iter().position(|j| j.id == id)?;
        Some(jobs.remove(idx))
    };
    emit_agents(app, state);
    job
}

/// Number of agents Vox currently has running.
pub fn running_count(state: &Arc<AppState>) -> usize {
    state.agents.lock().unwrap().len()
}

/// What Vox has running RIGHT NOW, for the system prompt.
///
/// Without this the model has no grounded answer to "how are my agents doing?" —
/// it only sees Conductor's worktree state, which by construction never contains
/// the headless agents Vox itself launched. It would then either invent an
/// answer or say nothing is running while three agents are working.
pub fn status_block(state: &Arc<AppState>, en: bool) -> String {
    let jobs = state.agents.lock().unwrap();
    if jobs.is_empty() {
        return String::new();
    }
    let mut out = String::from(if en {
        "\n\nAGENTS VOX IS RUNNING RIGHT NOW (this is the ONLY source for questions about them):"
    } else {
        "\n\nAGENTS QUE VOX FAIT TOURNER MAINTENANT (seule source pour les questions à leur sujet) :"
    });
    for j in jobs.iter() {
        let secs = j.started.elapsed().as_secs();
        let elapsed = match (en, secs) {
            (true, s) if s < 60 => format!("{s}s"),
            (true, s) => format!("{} min", s / 60),
            (false, s) if s < 60 => format!("{s} s"),
            (false, s) => format!("{} min", s / 60),
        };
        let phase = match (j.phase, en) {
            (AgentPhase::Drafting, true) => "starting",
            (AgentPhase::Drafting, false) => "démarrage",
            (AgentPhase::Running, true) => "working",
            (AgentPhase::Running, false) => "en cours",
            (_, true) => "finishing",
            (_, false) => "termine",
        };
        let task: String = j.task.chars().take(90).collect();
        out.push_str(&format!("\n- {} ({phase}, {elapsed}) — {task}", j.label));
    }
    out.push_str(if en {
        "\nIf asked what's running, how long, or whether something is done, answer from THIS list. Say the count and the names. Nothing here is finished yet."
    } else {
        "\nSi on te demande ce qui tourne, depuis combien de temps, ou si c'est fini, réponds à partir de CETTE liste. Donne le nombre et les noms. Rien ici n'est encore terminé."
    });
    out
}

/// Tool rules a voice-launched agent may never use, even though it runs with
/// --dangerously-skip-permissions (nobody is there to approve a prompt in
/// --print mode). A misheard command must not be able to publish, force-push or
/// wipe anything. Deny rules still win over the skip flag — verified against the
/// real CLI — so this is a hard stop, not a hint.
const AGENT_DENIED_TOOLS: &[&str] = &[
    "Bash(git push:*)",
    "Bash(git reset --hard:*)",
    "Bash(git clean:*)",
    "Bash(git branch -D:*)",
    "Bash(rm -rf:*)",
    "Bash(rm -fr:*)",
    "Bash(sudo:*)",
    "Bash(gh pr merge:*)",
    "Bash(gh release:*)",
    "Bash(npm publish:*)",
    "Bash(cargo publish:*)",
];

/// Agents already working in this exact directory.
pub fn count_in(state: &Arc<AppState>, path: &str) -> usize {
    // Local jobs: Conductor's headless agents, or an Orca launch still drafting.
    let local = state.agents.lock().unwrap().iter().filter(|j| j.path == path).count();
    if crate::workspace::backend() == Backend::Orca {
        let in_orca = crate::orca::ps().map(|w| crate::orca::working_in(&w, path)).unwrap_or(0);
        return local + in_orca;
    }
    local
}

/// Stop every agent Vox started. The hard undo — worth more than a confirmation
/// prompt, because it also fixes mistakes a confirmation would never catch
/// (right repo, wrong task).
pub fn kill_all(app: &AppHandle, state: &Arc<AppState>) -> usize {
    kill_local(app, state) + kill_orca(state)
}

/// Close the Orca tabs of agents Vox launched that are still busy.
///
/// Only those: a tab whose agent already finished holds output the user may
/// still want to read, and agents the user started in Orca themselves are not
/// Vox's to stop. The handle was recorded at launch; if Orca refuses the close
/// (a chat-surface session may not be a closable terminal) we say so in the log
/// rather than count it — it has to be stopped in Orca.
fn kill_orca(state: &Arc<AppState>) -> usize {
    // A tab from yesterday's session is not what "stop" means.
    const RECORD_TTL: u64 = 6 * 3600;
    let now = crate::now_unix();
    let records: Vec<crate::orca::LaunchRecord> = {
        let mut r = state.orca_launches.lock().unwrap();
        r.retain(|l| now.saturating_sub(l.at) < RECORD_TTL);
        r.clone()
    };
    if records.is_empty() {
        return 0;
    }
    let Some(wts) = crate::orca::ps() else {
        println!("[vox] stop: Orca isn't answering — stop the agents in Orca");
        return 0;
    };
    let mut closed = 0;
    for rec in records {
        if crate::orca::working_in(&wts, &rec.path) == 0 {
            continue;
        }
        match crate::orca::close_terminal(&rec.handle) {
            Ok(()) => {
                closed += 1;
                state.orca_launches.lock().unwrap().retain(|l| l.handle != rec.handle);
            }
            Err(e) => println!(
                "[vox] stop: couldn't close {} ({e}) — stop that agent in Orca",
                rec.path
            ),
        }
    }
    closed
}

/// Kill the headless agents (Conductor path) and cancel launches still inside
/// their window. This alone is what runs at app exit: quitting Vox must not
/// close agents that live in Orca — they don't depend on it.
pub fn kill_local(app: &AppHandle, state: &Arc<AppState>) -> usize {
    let jobs: Vec<AgentJob> = state.agents.lock().unwrap().clone();
    let mut killed = 0;
    for j in &jobs {
        if let Some(pid) = j.pid {
            // The supervisor thread owns the Child; signal by pid instead so we
            // don't need to share it across threads.
            let ok = Command::new("kill")
                .arg("-TERM")
                .arg(pid.to_string())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok {
                killed += 1;
            }
        }
    }
    // Drafting agents have no pid yet; interrupt_gen already cancels them.
    state.interrupt_gen.fetch_add(1, Ordering::SeqCst);
    emit_agents(app, state);
    killed
}

// ── stdout capture ───────────────────────────────────────────────────────────

#[derive(Default)]
struct Capture {
    /// Latest assistant text seen (replaced, not appended — the final message
    /// is the summary we want).
    last_text: String,
    /// The `result` envelope's payload, when it arrives.
    result_text: Option<String>,
    result_subtype: Option<String>,
    result_is_error: bool,
    /// Non-JSON fallback, if the CLI's schema ever drifts.
    raw_tail: String,
    stderr_tail: String,
    bytes: usize,
    /// Set once we've decided the stream isn't line-delimited JSON at all.
    plain_text: bool,
}

/// Drain a child stream into `cap`. NEVER returns early on the size cap:
/// stopping the read would let the 64KB kernel pipe buffer fill and block the
/// agent forever.
fn read_stream<R: std::io::Read + Send + 'static>(
    stream: R,
    cap: Arc<Mutex<Capture>>,
    is_stderr: bool,
) {
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            let mut c = cap.lock().unwrap();
            c.bytes += line.len();
            if is_stderr {
                let trimmed: String = line.chars().take(200).collect();
                eprintln!("[agent] {trimmed}");
                if c.stderr_tail.chars().count() < 400 {
                    c.stderr_tail.push_str(&trimmed);
                    c.stderr_tail.push('\n');
                }
                continue;
            }
            if c.bytes > MAX_CAPTURE {
                continue; // keep draining, stop storing
            }
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            if c.plain_text {
                c.raw_tail.push_str(t);
                c.raw_tail.push('\n');
                continue;
            }
            match serde_json::from_str::<Value>(t) {
                Ok(v) => absorb(&mut c, &v),
                Err(_) => {
                    // First unparseable line: the format isn't what we expect.
                    // Log loudly once, then treat the rest as plain text rather
                    // than silently reporting every agent as empty.
                    eprintln!("[vox] agent stdout is not stream-json — falling back to raw text");
                    c.plain_text = true;
                    c.raw_tail.push_str(t);
                    c.raw_tail.push('\n');
                }
            }
        }
    });
}

fn absorb(c: &mut Capture, v: &Value) {
    match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "assistant" => {
            if let Some(content) = v.pointer("/message/content").and_then(|c| c.as_array()) {
                let text: String = content
                    .iter()
                    .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n")
                    .trim()
                    .to_string();
                if !text.is_empty() {
                    c.last_text = text;
                }
            }
        }
        "result" => {
            c.result_subtype =
                v.get("subtype").and_then(|s| s.as_str()).map(String::from);
            c.result_is_error = v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false);
            if let Some(r) = v.get("result").and_then(|r| r.as_str()) {
                let r = r.trim();
                if !r.is_empty() {
                    c.result_text = Some(r.to_string());
                }
            }
        }
        _ => {}
    }
}

// ── Launch ───────────────────────────────────────────────────────────────────

/// Launch an agent in the ACTIVE project (legacy entry point).
pub fn spawn_agent(app: &AppHandle, state: &Arc<AppState>, task: &str) -> Result<AgentJob, LaunchError> {
    let path = state.active_project.lock().unwrap().clone();
    let label = std::path::Path::new(&path)
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| "the current project".into());
    let target = Target {
        label,
        path,
        source: TargetSource::Active,
        confidence: crate::targets::MatchKind::Exact,
        live_session: false,
    };
    launch(app, state, &target, task)
}

/// Reserve a slot and start an agent.
///
/// Pre-flight validation happens HERE, on the caller's thread, so a failure is
/// still speakable in the same breath as the request. Previously `spawn()` ran
/// inside the worker, long after the tool handler had returned "launching!" —
/// so a dead path or a missing CLI produced a confident lie and total silence.
pub fn launch(
    app: &AppHandle,
    state: &Arc<AppState>,
    target: &Target,
    task: &str,
) -> Result<AgentJob, LaunchError> {
    if crate::workspace::backend() == Backend::Orca {
        return launch_orca(app, state, target, task);
    }
    let claude = state.paths.lock().unwrap().claude_cli.clone();
    let Some(claude) = claude else {
        return Err(LaunchError::NoCli);
    };
    if !std::path::Path::new(&target.path).exists() {
        return Err(LaunchError::BadCwd(target.label.clone()));
    }

    let id = {
        let mut jobs = state.agents.lock().unwrap();
        let cap = max_agents();
        if jobs.len() >= cap {
            return Err(LaunchError::TooMany(cap));
        }
        let id = state.agent_seq.fetch_add(1, Ordering::SeqCst) + 1;
        jobs.push(AgentJob {
            id,
            label: target.label.clone(),
            repo: match &target.source {
                TargetSource::Worktree { repo, .. } | TargetSource::RepoRoot { repo } => repo.clone(),
                _ => target.label.clone(),
            },
            path: target.path.clone(),
            task: task.to_string(),
            // Reserved before the process exists so the badge appears instantly
            // and the concurrency cap counts agents that are still drafting.
            phase: AgentPhase::Drafting,
            started: Instant::now(),
            pid: None,
        });
        id
    };
    emit_agents(app, state);

    let job = state.agents.lock().unwrap().iter().find(|j| j.id == id).cloned();
    let Some(job) = job else {
        return Err(LaunchError::SpawnFailed("job vanished".into()));
    };

    // Tell the Conductor watcher this directory is ours for the next couple of
    // minutes, so it never reports the same work from the other side.
    crate::watch::note_vox_launch(state, &target.path);

    let needs_window = target.source != TargetSource::Active;
    let gen0 = state.interrupt_gen.load(Ordering::SeqCst);
    let (app_c, state_c) = (app.clone(), state.clone());
    let (cwd, task_s, label) = (target.path.clone(), task.to_string(), target.label.clone());
    std::thread::spawn(move || {
        supervise(app_c, state_c, id, claude, cwd, task_s, label, needs_window, gen0);
    });

    Ok(job)
}

/// Orca flavour of `launch`. Same promise — everything that can be checked is
/// checked on the caller's thread, so a failure is speakable in the same breath —
/// but the agent itself starts through the CLI instead of a child process.
fn launch_orca(
    app: &AppHandle,
    state: &Arc<AppState>,
    target: &Target,
    task: &str,
) -> Result<AgentJob, LaunchError> {
    if !std::path::Path::new(&target.path).exists() {
        return Err(LaunchError::BadCwd(target.label.clone()));
    }
    // One call both proves Orca is answering and feeds the cap: every agent
    // working in Orca shares the rate limit and the laptop, whoever started it.
    let working = crate::orca::ps()
        .map(|w| crate::orca::working_count(&w))
        .ok_or(LaunchError::NoOrca)?;

    let id = {
        let mut jobs = state.agents.lock().unwrap();
        let cap = max_agents();
        if working + jobs.len() >= cap {
            return Err(LaunchError::TooMany(cap));
        }
        let id = state.agent_seq.fetch_add(1, Ordering::SeqCst) + 1;
        // Held only while the launch is pending (cancel window, prompt drafting,
        // the CLI call): it keeps the badge honest and makes a burst of voice
        // commands count against the cap before Orca can report them.
        jobs.push(AgentJob {
            id,
            label: target.label.clone(),
            repo: match &target.source {
                TargetSource::Worktree { repo, .. } | TargetSource::RepoRoot { repo } => repo.clone(),
                _ => target.label.clone(),
            },
            path: target.path.clone(),
            task: task.to_string(),
            phase: AgentPhase::Drafting,
            started: Instant::now(),
            pid: None,
        });
        id
    };
    emit_agents(app, state);

    let job = state.agents.lock().unwrap().iter().find(|j| j.id == id).cloned();
    let Some(job) = job else {
        return Err(LaunchError::SpawnFailed("job vanished".into()));
    };

    let needs_window = target.source != TargetSource::Active;
    let gen0 = state.interrupt_gen.load(Ordering::SeqCst);
    let (app_c, state_c) = (app.clone(), state.clone());
    let (path, task_s, label) = (target.path.clone(), task.to_string(), target.label.clone());
    std::thread::spawn(move || {
        supervise_orca(app_c, state_c, id, path, task_s, label, needs_window, gen0);
    });

    Ok(job)
}

#[allow(clippy::too_many_arguments)]
fn supervise_orca(
    app: AppHandle,
    state: Arc<AppState>,
    id: u64,
    path: String,
    task: String,
    label: String,
    needs_window: bool,
    gen0: u64,
) {
    if needs_window {
        std::thread::sleep(CANCEL_WINDOW);
        if state.interrupt_gen.load(Ordering::SeqCst) != gen0 {
            println!("[vox] agent on {label} cancelled inside the launch window");
            remove_job(&app, &state, id);
            return;
        }
    }

    let prompt = crate::llm::draft_prompt(&state, &label, &task);
    set_phase(&app, &state, id, AgentPhase::Running);

    match crate::orca::launch(&path, &prompt) {
        Ok(l) => {
            println!(
                "[vox] orca agent {id} on {label} → {} {} (prompt: {})",
                l.kind, l.handle, l.prompt_outcome
            );
            if let Some(w) = &l.warning {
                println!("[vox] orca launch warning: {w}");
            }
            state.orca_launches.lock().unwrap().push(crate::orca::LaunchRecord {
                handle: l.handle,
                path: path.clone(),
                at: crate::now_unix(),
            });
            // The same full-prompt echo the headless path emits: what Vox says is
            // one short sentence, and this is the only place the brief is visible.
            let _ = app.emit("delegation", json!({ "project": label, "path": path, "prompt": prompt }));
            // Not `finish`: the agent is only starting. Orca owns it now, and the
            // watcher speaks its answer when its state turns `done`.
            remove_job(&app, &state, id);
        }
        Err(e) => {
            eprintln!("[vox] orca agent launch failed: {e}");
            finish(&app, &state, id, AgentOutcome::SpawnError { detail: e });
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn supervise(
    app: AppHandle,
    state: Arc<AppState>,
    id: u64,
    claude: std::path::PathBuf,
    cwd: String,
    task: String,
    label: String,
    needs_window: bool,
    gen0: u64,
) {
    // Cancel window — ⌥Space within CANCEL_WINDOW aborts a launch aimed at a
    // repo other than the active one, before anything is written anywhere.
    if needs_window {
        std::thread::sleep(CANCEL_WINDOW);
        if state.interrupt_gen.load(Ordering::SeqCst) != gen0 {
            println!("[vox] agent on {label} cancelled inside the launch window");
            remove_job(&app, &state, id);
            return;
        }
    }

    // Expand the short spoken task into a real prompt. Off the latency path:
    // the confirmation has already been spoken by now, so this costs nothing
    // the user can perceive.
    let prompt = crate::llm::draft_prompt(&state, &label, &task);
    set_phase(&app, &state, id, AgentPhase::Running);

    let cap = Arc::new(Mutex::new(Capture::default()));
    // `--` so a drafted prompt starting with '-' (bullet lists!) can't be
    // parsed as a CLI flag. `--verbose` is MANDATORY with stream-json under
    // --print: without it the CLI exits immediately with a usage error.
    let spawned = Command::new(&claude)
        .args([
            "--print",
            "--output-format",
            "stream-json",
            "--verbose",
            "--dangerously-skip-permissions",
            // Variadic: swallows every value up to the `--` below.
            "--disallowedTools",
        ])
        .args(AGENT_DENIED_TOOLS)
        .args(["--", &prompt])
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let mut child: Child = match spawned {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[vox] agent spawn failed: {e}");
            finish(&app, &state, id, AgentOutcome::SpawnError { detail: e.to_string() });
            return;
        }
    };
    set_pid(&state, id, Some(child.id()));

    // Echo the EXACT prompt that was sent, only once the process really exists.
    // Carried over from the delegation echo on main: what Vox says out loud is
    // one short sentence, so this is the only place the full drafted prompt is
    // ever visible — and Conductor cannot show it, since a headless agent
    // writes nothing to its database.
    let _ = app.emit(
        "delegation",
        json!({ "project": label, "path": cwd, "prompt": prompt }),
    );

    if let Some(out) = child.stdout.take() {
        read_stream(out, cap.clone(), false);
    }
    if let Some(err) = child.stderr.take() {
        read_stream(err, cap.clone(), true);
    }

    let start = Instant::now();
    let timeout = agent_timeout();
    let mut timed_out = false;
    let mut exit_code: Option<i32> = None;
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                exit_code = st.code();
                break;
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    timed_out = true;
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(_) => break,
        }
    }

    // The last stdout lines can land after try_wait reports the exit. Wait for
    // the byte count to settle rather than join()ing the reader: kill() only
    // signals the claude process, and its grandchildren (bash, node) inherit
    // the stdout fd — so the reader may never observe EOF.
    let mut last = 0usize;
    for _ in 0..5 {
        let n = cap.lock().unwrap().bytes;
        if n == last && n > 0 {
            break;
        }
        last = n;
        std::thread::sleep(Duration::from_millis(300));
    }

    let c = cap.lock().unwrap();
    let body = c
        .result_text
        .clone()
        .filter(|t| !t.trim().is_empty())
        .or_else(|| Some(c.last_text.clone()).filter(|t| !t.trim().is_empty()))
        .or_else(|| Some(c.raw_tail.trim().to_string()).filter(|t| !t.is_empty()));
    let subtype_bad = c
        .result_subtype
        .as_deref()
        .map(|s| s != "success")
        .unwrap_or(false);
    let is_error = c.result_is_error || subtype_bad;
    let detail = if c.stderr_tail.trim().is_empty() {
        c.result_subtype.clone().unwrap_or_else(|| "unknown".into())
    } else {
        c.stderr_tail.trim().chars().take(200).collect()
    };
    drop(c);

    let outcome = if timed_out {
        AgentOutcome::TimedOut { partial: body }
    } else if is_error || exit_code.unwrap_or(0) != 0 {
        AgentOutcome::Failed { code: exit_code.unwrap_or(-1), detail }
    } else {
        match body {
            Some(t) if !t.trim().is_empty() => AgentOutcome::Success { text: t },
            _ => AgentOutcome::Empty,
        }
    };
    finish(&app, &state, id, outcome);
}

fn finish(app: &AppHandle, state: &Arc<AppState>, id: u64, outcome: AgentOutcome) {
    let job = remove_job(app, state, id);
    let (label, secs) = job
        .as_ref()
        .map(|j| (j.label.clone(), j.started.elapsed().as_secs()))
        .unwrap_or_else(|| (String::new(), 0));
    println!("[vox] agent {id} on {label} → {} ({secs}s)", outcome.as_str());

    let snippet = outcome
        .text()
        .map(crate::workspace::clean_snippet)
        .unwrap_or_default();
    let _ = app.emit(
        "agent-finished",
        json!({
            "id": id,
            "label": label,
            "outcome": outcome.as_str(),
            "duration_s": secs,
            "snippet": snippet,
        }),
    );

    if let Some(job) = job {
        crate::announce::agent_finished(app, state, &job, outcome);
    }
}
