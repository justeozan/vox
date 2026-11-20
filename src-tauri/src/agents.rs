//! Agents launched by voice, in one of two modes — and, crucially, their ANSWER
//! is captured either way.
//!
//! **Native** (the default when Orca is available) hands the work to the ADE:
//! `orca terminal create --command claude` in an existing worktree, or
//! `orca worktree create --agent claude --prompt …` for a fresh one. The agent
//! runs in a real terminal the user can scroll back through and take over, and
//! its answer comes back through the watcher like any other Orca agent's. Vox
//! holds only the slot and the task text.
//!
//! **Headless** runs `claude --print --output-format stream-json --verbose` as
//! Vox's own child, so we get incremental assistant text (which survives a
//! timeout kill) plus an explicit terminal envelope telling us whether it really
//! succeeded. Nothing about it is visible in the ADE — which is the trade: it is
//! the right shape for a thirty-second errand nobody wants a worktree card for,
//! and the wrong shape for anything you might want to supervise.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use tauri::{AppHandle, Emitter};

use crate::ade::Snapshot;
use crate::announce::AnnounceKind;
use crate::targets::{Target, TargetSource};
use crate::AppState;

/// Each headless agent is a full Claude Code process (node runtime, MCP servers,
/// file watchers) and they all contend on one rate limit. Four already saturates
/// a laptop that is also running Ollama and a speech model.
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

/// Which agent CLI a native launch starts in the ADE terminal. Orca knows
/// `claude`, `codex`, `opencode`, `omp`, `pi`, `grok` and any other installed TUI
/// agent; Vox has no opinion beyond the default.
fn native_agent() -> String {
    std::env::var("VOX_ADE_AGENT")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "claude".into())
}

/// Grace period before an agent aimed at a NON-active target actually starts.
/// ⌥Space during it cancels the launch. Two seconds against a ten-minute task is
/// free, and it reuses interrupt_gen — no extra conversational turn.
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

/// Where the agent actually runs, and therefore who reports its answer.
#[derive(Clone, Debug)]
pub enum JobKind {
    /// Vox's own child process. Vox owns the lifecycle and reads stdout.
    Headless { pid: Option<u32> },
    /// A real ADE agent in a terminal. The watcher reports the answer; the
    /// handle is kept so "stop" can interrupt it without closing the tab.
    Native { terminal: String },
}

impl JobKind {
    fn as_str(&self) -> &'static str {
        match self {
            JobKind::Headless { .. } => "headless",
            JobKind::Native { .. } => "native",
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
    pub kind: JobKind,
}

#[derive(Debug)]
pub enum LaunchError {
    NoCli,
    /// Native mode was asked for but no ADE can host an agent.
    NoAde,
    BadCwd(String),
    TooMany(usize),
    Cancelled,
    SpawnFailed(String),
}

/// How an agent's run ended. The distinction between `Failed` and `TimedOut`
/// only exists because the wait loop keeps the exit status and a `timed_out`
/// flag — previously a kill and a clean exit were indistinguishable.
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

/// The launch mode in force, after reconciling the setting with reality.
///
/// A machine running Conductor cannot host a native agent — Conductor has no
/// API — so the setting degrades to headless rather than failing the launch.
fn native_mode(state: &Arc<AppState>) -> bool {
    let want = state.settings.lock().unwrap().agent_launch == "native";
    if want && !crate::orca::installed() {
        println!("[vox] native launch asked for, but no ADE can host an agent — using headless");
        return false;
    }
    want
}

fn emit_agents(app: &AppHandle, state: &Arc<AppState>) {
    let jobs = state.agents.lock().unwrap();
    let list: Vec<Value> = jobs
        .iter()
        .map(|j| {
            json!({
                "id": j.id,
                "label": j.label,
                "phase": j.phase.as_str(),
                "mode": j.kind.as_str(),
                "task": j.task.chars().take(80).collect::<String>(),
                "elapsed_s": j.started.elapsed().as_secs(),
            })
        })
        .collect();
    // A SUPERSET of the old payload: the renderer only reads `count`, so this
    // stays backward compatible with an un-updated webview.
    let _ = app.emit("agent-status", json!({ "count": list.len(), "agents": list }));
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

fn set_kind(state: &Arc<AppState>, id: u64, kind: JobKind) {
    let mut jobs = state.agents.lock().unwrap();
    if let Some(j) = jobs.iter_mut().find(|j| j.id == id) {
        j.kind = kind;
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

/// Hand a running NATIVE job over to the watcher, which has just seen its agent
/// settle in the ADE.
///
/// Claiming is what keeps one finished agent from being announced twice: without
/// it the watcher would report an anonymous "findy is done" for work Vox itself
/// started and can describe properly.
pub fn claim_native(state: &Arc<AppState>, path: &str) -> Option<AgentJob> {
    let mut jobs = state.agents.lock().unwrap();
    let idx = jobs.iter().position(|j| {
        matches!(j.kind, JobKind::Native { .. }) && j.phase == AgentPhase::Running && j.path == path
    })?;
    Some(jobs.remove(idx))
}

/// Report a claimed native job, with the task Vox was actually asked for.
pub fn finish_native(
    app: &AppHandle,
    state: &Arc<AppState>,
    job: AgentJob,
    snap: &Snapshot,
    kind: AnnounceKind,
) {
    emit_agents(app, state);
    let secs = job.started.elapsed().as_secs();
    let answer = snap.answer.clone().unwrap_or_default();
    let outcome = match kind {
        AnnounceKind::Errored => AgentOutcome::Failed { code: -1, detail: "ade reported an error".into() },
        _ if answer.trim().is_empty() => AgentOutcome::Empty,
        _ => AgentOutcome::Success { text: answer },
    };
    println!("[vox] native agent {} on {} → {} ({secs}s)", job.id, job.label, outcome.as_str());
    let _ = app.emit(
        "agent-finished",
        json!({
            "id": job.id,
            "label": job.label,
            "outcome": outcome.as_str(),
            "duration_s": secs,
            "snippet": outcome.text().map(crate::ade::clean_snippet).unwrap_or_default(),
        }),
    );
    crate::announce::agent_finished(app, state, &job, outcome);
}

/// Number of agents Vox currently has running.
pub fn running_count(state: &Arc<AppState>) -> usize {
    state.agents.lock().unwrap().len()
}

/// What Vox has running RIGHT NOW, for the system prompt.
///
/// Without this the model has no grounded answer to "how are my agents doing?" —
/// it only sees the ADE's worktree state, which for a headless agent by
/// construction never contains it. It would then either invent an answer or say
/// nothing is running while three agents are working.
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

/// Agents already working in this exact directory.
pub fn count_in(state: &Arc<AppState>, path: &str) -> usize {
    state.agents.lock().unwrap().iter().filter(|j| j.path == path).count()
}

/// Stop every running agent. The hard undo — worth more than a confirmation
/// prompt, because it also fixes mistakes a confirmation would never catch
/// (right repo, wrong task).
///
/// A native agent is INTERRUPTED, not killed: its terminal stays open so the
/// user can read what it got through and pick the thread back up. Closing the
/// tab would throw away the one thing native mode exists to provide.
pub fn kill_all(app: &AppHandle, state: &Arc<AppState>) -> usize {
    let jobs: Vec<AgentJob> = state.agents.lock().unwrap().clone();
    let mut killed = 0;
    for j in &jobs {
        let ok = match &j.kind {
            // The supervisor thread owns the Child; signal by pid instead so we
            // don't need to share it across threads.
            JobKind::Headless { pid: Some(pid) } => Command::new("kill")
                .arg("-TERM")
                .arg(pid.to_string())
                .status()
                .map(|s| s.success())
                .unwrap_or(false),
            JobKind::Native { terminal } if !terminal.is_empty() => {
                crate::orca::interrupt(terminal)
            }
            // Drafting agents have no pid or handle yet; interrupt_gen cancels
            // them below.
            _ => false,
        };
        if ok {
            killed += 1;
        }
    }
    state.interrupt_gen.fetch_add(1, Ordering::SeqCst);
    emit_agents(app, state);
    killed
}

// ── stdout capture (headless only) ───────────────────────────────────────────

#[derive(Default)]
struct Capture {
    /// Latest assistant text seen (replaced, not appended — the final message is
    /// the summary we want).
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
            c.result_subtype = v.get("subtype").and_then(|s| s.as_str()).map(String::from);
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
/// inside the worker, long after the tool handler had returned "launching!" — so
/// a dead path or a missing CLI produced a confident lie and total silence.
pub fn launch(
    app: &AppHandle,
    state: &Arc<AppState>,
    target: &Target,
    task: &str,
) -> Result<AgentJob, LaunchError> {
    let native = native_mode(state);
    let claude = if native {
        if !crate::orca::installed() {
            return Err(LaunchError::NoAde);
        }
        None
    } else {
        match state.paths.lock().unwrap().claude_cli.clone() {
            Some(c) => Some(c),
            None => return Err(LaunchError::NoCli),
        }
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
            kind: if native {
                JobKind::Native { terminal: String::new() }
            } else {
                JobKind::Headless { pid: None }
            },
        });
        id
    };
    emit_agents(app, state);

    let job = state.agents.lock().unwrap().iter().find(|j| j.id == id).cloned();
    let Some(job) = job else {
        return Err(LaunchError::SpawnFailed("job vanished".into()));
    };

    if !native {
        // Tell the watcher this directory is ours for the next couple of minutes,
        // so it never reports the same work from the other side.
        crate::watch::note_vox_launch(state, &target.path);
    }

    let needs_window = target.source != TargetSource::Active;
    let gen0 = state.interrupt_gen.load(Ordering::SeqCst);
    let (app_c, state_c) = (app.clone(), state.clone());
    let (task_s, label) = (task.to_string(), target.label.clone());
    let target_c = target.clone();
    std::thread::spawn(move || {
        // Cancel window — ⌥Space within CANCEL_WINDOW aborts a launch aimed at a
        // repo other than the active one, before anything is written anywhere.
        if needs_window {
            std::thread::sleep(CANCEL_WINDOW);
            if state_c.interrupt_gen.load(Ordering::SeqCst) != gen0 {
                println!("[vox] agent on {label} cancelled inside the launch window");
                remove_job(&app_c, &state_c, id);
                return;
            }
        }

        // Expand the short spoken task into a real prompt. Off the latency path:
        // the confirmation has already been spoken by now, so this costs nothing
        // the user can perceive.
        let prompt = crate::llm::draft_prompt(&state_c, &label, &task_s);
        set_phase(&app_c, &state_c, id, AgentPhase::Running);

        match claude {
            Some(claude) => supervise_headless(app_c, state_c, id, claude, target_c.path, prompt, label),
            None => supervise_native(app_c, state_c, id, target_c, prompt, label, task_s),
        }
    });

    Ok(job)
}

// ── Native supervision ───────────────────────────────────────────────────────

/// Hand the work to the ADE, then wait for the watcher to claim the job.
///
/// Vox does no lifecycle management here: the agent belongs to Orca. The only
/// job of this thread is to reap the slot if the answer never arrives, so one
/// lost agent cannot permanently eat a concurrency slot.
fn supervise_native(
    app: AppHandle,
    state: Arc<AppState>,
    id: u64,
    target: Target,
    prompt: String,
    label: String,
    task: String,
) {
    let agent = native_agent();
    // A repo target gets a fresh worktree: running an unsupervised agent in a
    // repo's main checkout dirties the tree that worktrees exist to protect.
    // A worktree target gets a new terminal in the tree the user named.
    let launched = match &target.source {
        TargetSource::RepoRoot { repo } => {
            crate::orca::launch_new_worktree(&target.path, &slug(&task, repo), &agent, &prompt)
        }
        _ => crate::orca::launch_in(&target.path, &agent, &prompt, "Vox"),
    };

    let launched = match launched {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[vox] native launch on {label} failed: {e}");
            finish_headless(&app, &state, id, AgentOutcome::SpawnError { detail: e });
            return;
        }
    };

    set_kind(&state, id, JobKind::Native { terminal: launched.terminal.clone() });
    // The path can change under us: a repo target becomes a brand-new worktree,
    // and the watcher matches jobs BY PATH. Without this the claim would never
    // fire and every native launch on a repo would be announced anonymously.
    {
        let mut jobs = state.agents.lock().unwrap();
        if let Some(j) = jobs.iter_mut().find(|j| j.id == id) {
            j.path = launched.path.clone();
        }
    }
    emit_agents(&app, &state);

    // Say in Orca's own UI where this came from, so a worktree that appears out
    // of nowhere is explainable.
    crate::orca::set_comment(&launched.path, &format!("Vox: {}", trim(&task, 90)));

    let _ = app.emit(
        "delegation",
        json!({ "project": label, "path": launched.path, "prompt": prompt, "mode": "native" }),
    );

    // Reap: the watcher normally claims this job long before the timeout. If it
    // never does — the pane was closed, Orca restarted, the agent was taken over
    // by hand — drop the slot quietly rather than announcing a result we do not
    // have.
    let deadline = Instant::now() + agent_timeout();
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(2));
        let still_ours = state.agents.lock().unwrap().iter().any(|j| j.id == id);
        if !still_ours {
            return;
        }
    }
    println!("[vox] native agent {id} on {label} never settled — releasing the slot");
    remove_job(&app, &state, id);
}

/// A filesystem- and Orca-safe worktree name derived from the spoken task.
fn slug(task: &str, repo: &str) -> String {
    let mut out = String::new();
    for c in task.chars() {
        if c.is_ascii_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
        if out.chars().count() >= 32 {
            break;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        format!("vox-{repo}")
    } else {
        format!("vox-{out}")
    }
}

fn trim(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// ── Headless supervision ─────────────────────────────────────────────────────

fn supervise_headless(
    app: AppHandle,
    state: Arc<AppState>,
    id: u64,
    claude: std::path::PathBuf,
    cwd: String,
    prompt: String,
    label: String,
) {
    let cap = Arc::new(Mutex::new(Capture::default()));
    // `--` so a drafted prompt starting with '-' (bullet lists!) can't be parsed
    // as a CLI flag. `--verbose` is MANDATORY with stream-json under --print:
    // without it the CLI exits immediately with a usage error.
    let spawned = Command::new(&claude)
        .args([
            "--print",
            "--output-format",
            "stream-json",
            "--verbose",
            "--dangerously-skip-permissions",
            "--",
            &prompt,
        ])
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let mut child: Child = match spawned {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[vox] agent spawn failed: {e}");
            finish_headless(&app, &state, id, AgentOutcome::SpawnError { detail: e.to_string() });
            return;
        }
    };
    set_kind(&state, id, JobKind::Headless { pid: Some(child.id()) });

    // Echo the EXACT prompt that was sent, only once the process really exists.
    // What Vox says out loud is one short sentence, so this is the only place the
    // full drafted prompt is ever visible — and the ADE cannot show it, since a
    // headless agent writes nothing into it.
    let _ = app.emit(
        "delegation",
        json!({ "project": label, "path": cwd, "prompt": prompt, "mode": "headless" }),
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
    // signals the claude process, and its grandchildren (bash, node) inherit the
    // stdout fd — so the reader may never observe EOF.
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
    let subtype_bad = c.result_subtype.as_deref().map(|s| s != "success").unwrap_or(false);
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
    finish_headless(&app, &state, id, outcome);
}

fn finish_headless(app: &AppHandle, state: &Arc<AppState>, id: u64, outcome: AgentOutcome) {
    let job = remove_job(app, state, id);
    let (label, secs) = job
        .as_ref()
        .map(|j| (j.label.clone(), j.started.elapsed().as_secs()))
        .unwrap_or_else(|| (String::new(), 0));
    println!("[vox] agent {id} on {label} → {} ({secs}s)", outcome.as_str());

    let snippet = outcome.text().map(crate::ade::clean_snippet).unwrap_or_default();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_filesystem_safe_and_bounded() {
        assert_eq!(slug("corriger les tests qui échouent", "findy"), "vox-corriger-les-tests-qui-chouent");
        assert_eq!(slug("fix login!!!", "findy"), "vox-fix-login");
        // Never empty, never just punctuation.
        assert_eq!(slug("...", "findy"), "vox-findy");
        assert_eq!(slug("", "findy"), "vox-findy");
        assert!(slug(&"a".repeat(200), "findy").chars().count() <= 36);
        assert!(slug("Fix Login", "r").chars().all(|c| c.is_ascii_lowercase() || c == '-'));
    }

    /// A native job is claimed by path, and only while it is actually running —
    /// claiming a drafting job would announce a result before the agent existed.
    #[test]
    fn only_running_native_jobs_are_claimable() {
        let job = |phase, kind| AgentJob {
            id: 1,
            label: "findy".into(),
            repo: "findy".into(),
            path: "/tmp/findy".into(),
            task: "fix".into(),
            phase,
            started: Instant::now(),
            kind,
        };
        let claimable = |j: &AgentJob, path: &str| {
            matches!(j.kind, JobKind::Native { .. }) && j.phase == AgentPhase::Running && j.path == path
        };
        assert!(claimable(&job(AgentPhase::Running, JobKind::Native { terminal: "h".into() }), "/tmp/findy"));
        assert!(!claimable(&job(AgentPhase::Drafting, JobKind::Native { terminal: String::new() }), "/tmp/findy"));
        assert!(!claimable(&job(AgentPhase::Running, JobKind::Headless { pid: Some(1) }), "/tmp/findy"));
        assert!(!claimable(&job(AgentPhase::Running, JobKind::Native { terminal: "h".into() }), "/elsewhere"));
    }

    #[test]
    fn job_modes_are_reported_to_the_renderer() {
        assert_eq!(JobKind::Native { terminal: "h".into() }.as_str(), "native");
        assert_eq!(JobKind::Headless { pid: None }.as_str(), "headless");
    }
}
