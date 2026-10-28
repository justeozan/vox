//! Orca backend — everything comes from the `orca` CLI (`--json`), nothing from
//! its internals. `worktree ps` is the one call that carries both workspaces and
//! their agents (state, original prompt, last answer), `repo list` adds repos
//! that have never had a worktree, and `agent launch` starts an agent as a tab
//! the user can watch. Output is mapped onto the same `Workspace` / `TargetCandidate`
//! shapes Conductor fills, so nothing downstream knows which backend it is on.
//!
//! Every call has a hard timeout and fails soft: an Orca that is closed or
//! wedged looks exactly like a missing Conductor DB — empty state, one log line.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::home;
use crate::watch::iso_at;
use crate::workspace::{clean_snippet, SessionInfo, TargetCandidate, Workspace};

/// A healthy call takes ~0.1-0.2s; this only bounds a hung one.
const CLI_TIMEOUT: Duration = Duration::from_secs(5);
/// `ps` caps its page at a default well below what a power user accumulates.
const PS_LIMIT: &str = "500";
/// A failed turn older than this is history, not something to recap at launch.
const ERROR_FRESH_MS: u64 = 12 * 3600 * 1000;

// ── CLI plumbing ─────────────────────────────────────────────────────────────

fn orca_bin() -> Option<PathBuf> {
    // A Dock/Finder launch has a minimal PATH, so look inside the app bundle too.
    crate::daemons::find_bin(
        "orca",
        &[
            PathBuf::from("/Applications/Orca.app/Contents/Resources/bin"),
            home().join("Applications/Orca.app/Contents/Resources/bin"),
        ],
    )
}

/// Orca's envelope is `{ok, result}` or `{ok:false, error:{code,message}}`, on
/// stdout even when the exit code is non-zero. Returns the `result`.
fn parse_response(stdout: &[u8], stderr: &str) -> Result<Value, String> {
    let v: Value = match serde_json::from_slice(stdout) {
        Ok(v) => v,
        Err(_) => {
            let detail: String = stderr.trim().chars().take(200).collect();
            return Err(if detail.is_empty() { "unparseable orca output".into() } else { detail });
        }
    };
    if v.get("ok").and_then(|b| b.as_bool()) == Some(false) {
        let e = v.get("error");
        let code = e.and_then(|e| e.get("code")).and_then(|c| c.as_str()).unwrap_or("error");
        let msg = e.and_then(|e| e.get("message")).and_then(|c| c.as_str()).unwrap_or("");
        // `message` often just repeats the code.
        return Err(if msg.is_empty() || msg == code { code.to_string() } else { format!("{code}: {msg}") });
    }
    Ok(v.get("result").cloned().unwrap_or(v))
}

fn run_json(args: &[&str]) -> Result<Value, String> {
    let bin = orca_bin().ok_or_else(|| "orca CLI not found".to_string())?;
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;

    // Drain both pipes while waiting: `worktree ps` is ~85 KB, past the 64 KB
    // kernel pipe buffer, so waiting first would block the child forever.
    let mut out = child.stdout.take().ok_or("no stdout")?;
    let mut err = child.stderr.take().ok_or("no stderr")?;
    let out_t = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = out.read_to_end(&mut b);
        b
    });
    let err_t = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = err.read_to_end(&mut b);
        b
    });

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() > CLI_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                // The readers are left to finish on their own: a grandchild may
                // still hold the pipe open.
                return Err(format!("`orca {}` timed out", args.first().copied().unwrap_or("")));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(e.to_string()),
        }
    }
    let stdout = out_t.join().unwrap_or_default();
    let stderr = String::from_utf8_lossy(&err_t.join().unwrap_or_default()).to_string();
    parse_response(&stdout, &stderr)
}

static WARNED: AtomicBool = AtomicBool::new(false);

/// Log the first failure of a streak, stay quiet while it persists, and re-arm
/// on success — the watcher polls every few seconds and must not flood the log.
fn soft<T>(r: Result<T, String>) -> Option<T> {
    match r {
        Ok(v) => {
            WARNED.store(false, Ordering::Relaxed);
            Some(v)
        }
        Err(e) => {
            if !WARNED.swap(true, Ordering::Relaxed) {
                println!("[vox] orca unavailable ({e}) — workspace state will be empty");
            }
            None
        }
    }
}

// ── Parsing `orca … --json` ──────────────────────────────────────────────────

fn s(v: &Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

fn ms(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(|x| x.as_u64().or_else(|| x.as_f64().map(|f| f as u64))).unwrap_or(0)
}

/// What an Orca agent is doing. `blocked` (a permission prompt) and `waiting`
/// (a question) both mean the same thing to the user: it needs you.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentState {
    Working,
    Waiting,
    Done,
    /// A state this build doesn't know. Never announced, never claimed quiet.
    Other,
}

impl AgentState {
    fn parse(s: &str) -> Self {
        match s {
            "working" => AgentState::Working,
            "blocked" | "waiting" => AgentState::Waiting,
            "done" => AgentState::Done,
            _ => AgentState::Other,
        }
    }
}

#[derive(Clone, Debug)]
pub struct OrcaAgent {
    pub pane_key: String,
    pub state: AgentState,
    /// Verdict of the turn that ended (`success`, `failure`, `cancellation`, …).
    /// Empty when unknown — which is never read as success.
    pub outcome: String,
    pub interrupted: bool,
    pub agent_type: String,
    /// What this agent was asked.
    pub prompt: String,
    pub last_message: String,
    /// When the agent entered its current state (ms). For `Done`: when it finished.
    pub state_started_ms: u64,
    pub updated_ms: u64,
}

impl OrcaAgent {
    /// A turn that ended badly, as opposed to one the user stopped.
    pub fn failed(&self) -> bool {
        matches!(self.outcome.as_str(), "failure" | "interruption")
    }

    /// A stop the user asked for is not news.
    pub fn cancelled(&self) -> bool {
        self.interrupted || self.outcome == "cancellation"
    }

    /// Conductor's vocabulary (`working`/`waiting`/`error`/`idle`), which the
    /// recap, the grounding block and the carousel already speak.
    pub fn status(&self, now_ms: u64) -> &'static str {
        match self.state {
            AgentState::Working => "working",
            AgentState::Waiting => "waiting",
            AgentState::Done => {
                if self.failed() && now_ms.saturating_sub(self.state_started_ms) < ERROR_FRESH_MS {
                    "error"
                } else {
                    "idle"
                }
            }
            AgentState::Other => "unknown",
        }
    }

    /// When this agent last said or settled something, in ms.
    fn activity_ms(&self) -> u64 {
        if self.state == AgentState::Done {
            self.state_started_ms
        } else {
            self.updated_ms
        }
    }
}

#[derive(Clone, Debug)]
pub struct OrcaWorktree {
    pub repo_id: String,
    pub repo: String,
    pub path: String,
    pub branch: String,
    pub display_name: String,
    pub is_main: bool,
    pub last_activity_ms: u64,
    pub unread: bool,
    pub agents: Vec<OrcaAgent>,
}

#[derive(Clone, Debug)]
pub struct OrcaRepo {
    pub id: String,
    pub path: String,
    pub name: String,
    pub added_ms: u64,
}

fn parse_agent(a: &Value) -> OrcaAgent {
    let main = a.get("mainAgent");
    OrcaAgent {
        pane_key: s(a, "paneKey"),
        state: AgentState::parse(&s(a, "state")),
        outcome: main.map(|m| s(m, "outcome")).unwrap_or_default(),
        interrupted: a.get("interrupted").and_then(|b| b.as_bool()).unwrap_or(false),
        agent_type: s(a, "agentType"),
        prompt: s(a, "prompt"),
        last_message: s(a, "lastAssistantMessage"),
        state_started_ms: ms(a, "stateStartedAt"),
        updated_ms: ms(a, "updatedAt"),
    }
}

/// `refs/heads/feat/x` → `feat/x`.
fn short_branch(b: &str) -> String {
    b.strip_prefix("refs/heads/").unwrap_or(b).to_string()
}

/// The `result` of `worktree ps`. Archived worktrees and ones living on another
/// host (a paired server) are dropped here, once, so nothing downstream can
/// forget to.
pub fn parse_ps(result: &Value) -> Vec<OrcaWorktree> {
    let Some(rows) = result.get("worktrees").and_then(|w| w.as_array()) else {
        return vec![];
    };
    rows.iter()
        .filter(|w| !w.get("isArchived").and_then(|b| b.as_bool()).unwrap_or(false))
        .filter(|w| matches!(w.get("hostId").and_then(|h| h.as_str()), None | Some("local")))
        .map(|w| OrcaWorktree {
            repo_id: s(w, "repoId"),
            repo: s(w, "repo"),
            path: s(w, "path"),
            branch: short_branch(&s(w, "branch")),
            display_name: s(w, "displayName"),
            is_main: w.get("isMainWorktree").and_then(|b| b.as_bool()).unwrap_or(false),
            last_activity_ms: ms(w, "lastActivityAt"),
            unread: w.get("unread").and_then(|b| b.as_bool()).unwrap_or(false),
            agents: w
                .get("agents")
                .and_then(|a| a.as_array())
                .map(|a| a.iter().map(parse_agent).collect())
                .unwrap_or_default(),
        })
        .collect()
}

pub fn parse_repos(result: &Value) -> Vec<OrcaRepo> {
    result
        .get("repos")
        .and_then(|r| r.as_array())
        .map(|rows| {
            rows.iter()
                .map(|r| OrcaRepo {
                    id: s(r, "id"),
                    path: s(r, "path"),
                    name: s(r, "displayName"),
                    added_ms: ms(r, "addedAt"),
                })
                .collect()
        })
        .unwrap_or_default()
}

// One turn reads the state several times (grounding, target names, target
// resolution, the launch cap) and each CLI call is ~0.1-0.2s, which a voice
// loop would feel. The watcher refreshes `ps` every few seconds anyway, so
// everything else may reuse its answer for a moment.
const PS_TTL: Duration = Duration::from_secs(5);
const REPOS_TTL: Duration = Duration::from_secs(30);

static PS_CACHE: Mutex<Option<(Instant, Vec<OrcaWorktree>)>> = Mutex::new(None);
static REPOS_CACHE: Mutex<Option<(Instant, Vec<OrcaRepo>)>> = Mutex::new(None);

/// Live workspace + agent snapshot, at most `PS_TTL` old. `None` when Orca can't
/// be reached.
pub fn ps() -> Option<Vec<OrcaWorktree>> {
    if let Some((at, w)) = PS_CACHE.lock().unwrap().as_ref() {
        if at.elapsed() < PS_TTL {
            return Some(w.clone());
        }
    }
    ps_fresh()
}

/// Always asks Orca. The watcher uses this: an announcement can't be late by a
/// cache's age, and its result is what keeps `ps` warm for everyone else.
pub fn ps_fresh() -> Option<Vec<OrcaWorktree>> {
    let w = soft(run_json(&["worktree", "ps", "--json", "--limit", PS_LIMIT])).map(|r| parse_ps(&r))?;
    *PS_CACHE.lock().unwrap() = Some((Instant::now(), w.clone()));
    Some(w)
}

fn repos() -> Option<Vec<OrcaRepo>> {
    if let Some((at, r)) = REPOS_CACHE.lock().unwrap().as_ref() {
        if at.elapsed() < REPOS_TTL {
            return Some(r.clone());
        }
    }
    let r = soft(run_json(&["repo", "list", "--json"])).map(|r| parse_repos(&r))?;
    *REPOS_CACHE.lock().unwrap() = Some((Instant::now(), r.clone()));
    Some(r)
}

// ── Mapping onto the shared shapes ───────────────────────────────────────────

fn now_ms() -> u64 {
    crate::now_unix() * 1000
}

/// `YYYY-MM-DD HH:MM:SS`, the sortable shape `TargetCandidate::ts` is compared in.
pub fn ts_from_ms(ms: u64) -> String {
    iso_at(ms / 1000).replace('T', " ").chars().take(19).collect()
}

/// The agent that best represents a worktree in a one-line recap: one that needs
/// the user beats one that is working beats one that is done; ties go to the
/// freshest.
fn lead_agent(w: &OrcaWorktree) -> Option<&OrcaAgent> {
    let rank = |a: &OrcaAgent| match a.state {
        AgentState::Waiting => 2,
        AgentState::Working => 1,
        _ => 0,
    };
    w.agents.iter().max_by_key(|a| (rank(a), a.updated_ms))
}

fn session_of(w: &OrcaWorktree, now_ms: u64) -> Option<SessionInfo> {
    let a = lead_agent(w)?;
    let preview = a.last_message.trim();
    let ask = clean_snippet(&a.prompt);
    Some(SessionInfo {
        agent: a.agent_type.clone(),
        status: a.status(now_ms).to_string(),
        unread_count: i64::from(w.unread),
        last_activity: (a.activity_ms() > 0).then(|| iso_at(a.activity_ms() / 1000)),
        preview: (!preview.is_empty()).then(|| preview.chars().take(300).collect()),
        original_ask: (ask.chars().count() > 3).then(|| ask.chars().take(150).collect()),
    })
}

/// Worktrees with an agent on them, freshest first. A worktree nobody has
/// prompted says nothing a recap or the grounding block could use.
pub fn workspaces_from(wts: &[OrcaWorktree], now_ms: u64, max_items: usize) -> Vec<Workspace> {
    let activity = |w: &OrcaWorktree| {
        w.agents.iter().map(|a| a.updated_ms).max().unwrap_or(0).max(w.last_activity_ms)
    };
    let mut rows: Vec<&OrcaWorktree> = wts.iter().filter(|w| !w.agents.is_empty()).collect();
    rows.sort_by_key(|w| std::cmp::Reverse(activity(w)));
    rows.into_iter()
        .take(max_items)
        .map(|w| Workspace {
            project: if w.repo.is_empty() { w.display_name.clone() } else { w.repo.clone() },
            branch: w.branch.clone(),
            path: w.path.clone(),
            session: session_of(w, now_ms),
        })
        .collect()
}

pub fn read_state(max_items: usize) -> Vec<Workspace> {
    ps().map(|w| workspaces_from(&w, now_ms(), max_items)).unwrap_or_default()
}

/// Launch targets: every non-main worktree plus every repo. The main checkout
/// is represented by its repo row ("main" is a useless thing to say aloud), and
/// a repo with no worktree activity is still launchable — same as Conductor.
pub fn candidates_from(wts: &[OrcaWorktree], repos: &[OrcaRepo]) -> Vec<TargetCandidate> {
    // The repo-list name is what the user sees in Orca; use it on worktree rows
    // too, because target resolution joins the two by name.
    let repo_name = |w: &OrcaWorktree| {
        repos.iter().find(|r| r.id == w.repo_id).map(|r| r.name.clone()).unwrap_or_else(|| w.repo.clone())
    };
    let mut out: Vec<TargetCandidate> = wts
        .iter()
        .filter(|w| !w.is_main && !w.path.is_empty())
        .map(|w| TargetCandidate {
            name: if w.display_name.is_empty() { w.branch.clone() } else { w.display_name.clone() },
            kind: "worktree",
            repo: repo_name(w),
            path: w.path.clone(),
            branch: w.branch.clone(),
            ts: ts_from_ms(w.last_activity_ms),
            live: w.agents.iter().any(|a| a.state == AgentState::Working),
        })
        .collect();
    out.extend(repos.iter().map(|r| {
        let main = wts.iter().find(|w| w.is_main && w.repo_id == r.id);
        let latest = wts.iter().filter(|w| w.repo_id == r.id).map(|w| w.last_activity_ms).max();
        TargetCandidate {
            name: r.name.clone(),
            kind: "repo",
            repo: r.name.clone(),
            path: r.path.clone(),
            branch: main.map(|w| w.branch.clone()).unwrap_or_default(),
            ts: ts_from_ms(latest.unwrap_or(r.added_ms)),
            live: false,
        }
    }));
    out.sort_by(|a, b| b.ts.cmp(&a.ts));
    out
}

pub fn catalog() -> Vec<TargetCandidate> {
    // Cold cache: ask for both at once rather than back to back.
    let repos_t = std::thread::spawn(repos);
    let wts = ps();
    let repos = repos_t.join().ok().flatten();
    let (Some(wts), Some(repos)) = (wts, repos) else { return vec![] };
    candidates_from(&wts, &repos)
        .into_iter()
        // A row whose directory is gone would send an agent nowhere.
        .filter(|c| !c.name.is_empty() && !c.path.is_empty() && std::path::Path::new(&c.path).exists())
        .collect()
}

/// Agents currently working, across all of Orca. They share one rate limit and
/// one laptop with whatever Vox launches, so the concurrency cap counts them.
pub fn working_count(wts: &[OrcaWorktree]) -> usize {
    wts.iter().flat_map(|w| &w.agents).filter(|a| a.state == AgentState::Working).count()
}

/// Agents working in exactly this directory.
pub fn working_in(wts: &[OrcaWorktree], path: &str) -> usize {
    wts.iter()
        .filter(|w| w.path == path)
        .flat_map(|w| &w.agents)
        .filter(|a| a.state == AgentState::Working)
        .count()
}

// ── Watching ─────────────────────────────────────────────────────────────────

/// What the watcher remembers about one agent between polls.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentSnap {
    pub state: AgentState,
    pub outcome: String,
    pub cancelled: bool,
    pub state_started_ms: u64,
    pub last_message: String,
    pub project: String,
    pub path: String,
    pub branch: String,
    pub agent_type: String,
    pub prompt: String,
}

/// Flatten a poll to (pane key, snapshot). The pane key is the agent's stable
/// identity — a worktree can host several agents.
pub fn snapshots(wts: &[OrcaWorktree]) -> Vec<(String, AgentSnap)> {
    wts.iter()
        .flat_map(|w| {
            w.agents.iter().filter(|a| !a.pane_key.is_empty()).map(move |a| {
                (
                    a.pane_key.clone(),
                    AgentSnap {
                        state: a.state,
                        outcome: a.outcome.clone(),
                        cancelled: a.cancelled(),
                        state_started_ms: a.state_started_ms,
                        last_message: a.last_message.clone(),
                        project: if w.repo.is_empty() { w.display_name.clone() } else { w.repo.clone() },
                        path: w.path.clone(),
                        branch: w.branch.clone(),
                        agent_type: a.agent_type.clone(),
                        prompt: a.prompt.clone(),
                    },
                )
            })
        })
        .collect()
}

/// What an agent's transition means to the user, if anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Finished,
    Errored,
    NeedsInput,
}

/// Compare an agent with how it looked on the previous poll.
///
/// Keyed on `state_started_ms` advancing, not only on the state value: a poll
/// can straddle a whole turn (`working → done → working`), and then the state
/// reads the same on both sides while the timestamp has moved on — exactly the
/// signal the Conductor watcher gets from its message watermark.
///
/// `was == None` is an agent seen for the first time AFTER the baseline (one
/// that started, and maybe even finished, between two polls). It counts only if
/// it settled after the watcher started; anything older is history.
pub fn detect(was: Option<&AgentSnap>, now: &AgentSnap, boot_ms: u64) -> Option<Event> {
    let fresh = |t: u64| t >= boot_ms;
    match now.state {
        AgentState::Done => {
            let new_turn = match was {
                Some(w) => w.state != AgentState::Done || now.state_started_ms > w.state_started_ms,
                None => fresh(now.state_started_ms),
            };
            if !new_turn || now.cancelled {
                None
            } else if matches!(now.outcome.as_str(), "failure" | "interruption") {
                Some(Event::Errored)
            } else {
                Some(Event::Finished)
            }
        }
        AgentState::Waiting => {
            let new_wait = match was {
                Some(w) => w.state != AgentState::Waiting || now.state_started_ms > w.state_started_ms,
                None => fresh(now.state_started_ms),
            };
            new_wait.then_some(Event::NeedsInput)
        }
        AgentState::Working | AgentState::Other => None,
    }
}

/// Is the agent still where `detect` saw it settle? Re-checked a tick later so
/// a `done` that bounces back to `working` is never announced.
pub fn still_settled(ev: Event, started_ms: u64, now: &AgentSnap) -> bool {
    let want = match ev {
        Event::Finished | Event::Errored => AgentState::Done,
        Event::NeedsInput => AgentState::Waiting,
    };
    now.state == want && now.state_started_ms == started_ms
}

// ── Launching & stopping ─────────────────────────────────────────────────────

/// An agent Vox started in Orca — just enough to find its tab again.
#[derive(Clone, Debug)]
pub struct LaunchRecord {
    /// Runtime-issued handle for `terminal close --terminal`.
    pub handle: String,
    pub path: String,
    pub at: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Launched {
    /// "terminal" | "structured" (a chat session).
    pub kind: String,
    pub handle: String,
    pub worktree_id: String,
    /// Whether the brief reached the agent (`journaled`, `handed-to-terminal`,
    /// `unconfirmed`, …).
    pub prompt_outcome: String,
    pub warning: Option<String>,
}

/// The `result` of `agent launch`.
pub fn parse_launch(result: &Value) -> Result<Launched, String> {
    let outcome = result.get("outcome").ok_or("launch returned no outcome")?;
    let handle = s(outcome, "handle");
    if handle.is_empty() {
        return Err("launch returned no handle".into());
    }
    Ok(Launched {
        kind: s(outcome, "kind"),
        handle,
        worktree_id: s(result, "worktreeId"),
        prompt_outcome: result.get("prompt").map(|p| s(p, "outcome")).unwrap_or_default(),
        warning: result.get("warning").and_then(|w| w.as_str()).map(String::from),
    })
}

/// Start Claude in the worktree at `path`, as a visible Orca tab. The prompt is
/// the headless brief, typed into the agent as if by the user.
pub fn launch(path: &str, prompt: &str) -> Result<Launched, String> {
    // `--flag=value`: Orca's parser treats a bare value starting with `--` as
    // the next flag, and a drafted prompt can start with anything.
    let worktree = format!("--worktree=path:{path}");
    let prompt = format!("--prompt={prompt}");
    // `--surface terminal`, not chat: Orca's chat sessions have no CLI handle
    // that `terminal close` accepts, so "stop" could never reach them. A
    // terminal agent is tracked by `worktree ps` exactly the same way.
    let result = run_json(&[
        "agent", "launch", "--agent", "claude", "--surface", "terminal", &worktree, &prompt, "--json",
    ])?;
    parse_launch(&result)
}

/// Close the agent's whole tab. Orca stops the process it owns; without
/// `--tab` an empty tab would be left behind.
pub fn close_terminal(handle: &str) -> Result<(), String> {
    run_json(&["terminal", "close", &format!("--terminal={handle}"), "--tab", "--json"]).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Shapes copied from real `orca worktree ps --json` output, anonymised.
    fn ps_fixture() -> Value {
        json!({
            "worktrees": [
                {
                    "worktreeId": "r1::/w/app-porto", "repoId": "r1", "hostId": "local",
                    "repo": "app", "path": "/w/app-porto",
                    "branch": "refs/heads/feat/login", "isArchived": false, "isMainWorktree": false,
                    "displayName": "Login redirect", "workspaceStatus": "in-progress",
                    "lastActivityAt": 1793030080449u64, "unread": true, "status": "working",
                    "agents": [
                        {
                            "paneKey": "pane-a", "state": "working", "agentType": "claude",
                            "prompt": "Fix the login redirect that lands on /404. Then add a test.",
                            "lastAssistantMessage": "Looking at the callback handler now.",
                            "interrupted": false,
                            "mainAgent": { "state": "working", "stateStartedAt": 1793029423556u64 },
                            "stateStartedAt": 1793029423556u64, "updatedAt": 1793030093977u64
                        },
                        {
                            "paneKey": "pane-b", "state": "done", "agentType": "claude",
                            "prompt": "Rename the env vars.", "lastAssistantMessage": "Renamed them all.",
                            "interrupted": false,
                            "mainAgent": { "state": "done", "outcome": "success", "stateStartedAt": 1793000000000u64 },
                            "stateStartedAt": 1793000000000u64, "updatedAt": 1793000000500u64
                        }
                    ]
                },
                {
                    "worktreeId": "r1::/w/app", "repoId": "r1", "hostId": "local",
                    "repo": "app", "path": "/w/app", "branch": "refs/heads/main",
                    "isArchived": false, "isMainWorktree": true, "displayName": "main",
                    "lastActivityAt": 1700000000000u64, "unread": false, "agents": []
                },
                {
                    "worktreeId": "r1::/w/old", "repoId": "r1", "hostId": "local",
                    "repo": "app", "path": "/w/old", "branch": "refs/heads/old",
                    "isArchived": true, "isMainWorktree": false, "displayName": "Old",
                    "lastActivityAt": 1, "agents": [
                        { "paneKey": "p", "state": "working", "agentType": "claude" }
                    ]
                },
                {
                    "worktreeId": "r2::/srv/x", "repoId": "r2", "hostId": "runtime:abc",
                    "repo": "remote", "path": "/srv/x", "branch": "refs/heads/main",
                    "isArchived": false, "isMainWorktree": true, "displayName": "main",
                    "lastActivityAt": 1, "agents": []
                }
            ]
        })
    }

    fn repos_fixture() -> Value {
        json!({ "repos": [
            { "id": "r1", "path": "/w/app", "displayName": "app", "addedAt": 1790000000000u64, "kind": "git" },
            { "id": "r3", "path": "/w/quiet", "displayName": "quiet", "addedAt": 1780000000000u64, "kind": "git" }
        ]})
    }

    #[test]
    fn ps_skips_archived_and_remote_rows() {
        let wts = parse_ps(&ps_fixture());
        let paths: Vec<&str> = wts.iter().map(|w| w.path.as_str()).collect();
        assert_eq!(paths, ["/w/app-porto", "/w/app"]);
        assert_eq!(wts[0].branch, "feat/login");
    }

    #[test]
    fn states_map_onto_the_conductor_vocabulary() {
        let now = 1793030100000u64;
        let mk = |state: &str, outcome: &str, started: u64| OrcaAgent {
            pane_key: "p".into(),
            state: AgentState::parse(state),
            outcome: outcome.into(),
            interrupted: false,
            agent_type: "claude".into(),
            prompt: String::new(),
            last_message: String::new(),
            state_started_ms: started,
            updated_ms: started,
        };
        assert_eq!(mk("working", "", now).status(now), "working");
        // `blocked` (permission) and `waiting` (question) both need the user.
        assert_eq!(mk("blocked", "", now).status(now), "waiting");
        assert_eq!(mk("waiting", "", now).status(now), "waiting");
        assert_eq!(mk("done", "success", now).status(now), "idle");
        assert_eq!(mk("done", "failure", now).status(now), "error");
        // An old failure is history.
        assert_eq!(mk("done", "failure", now - 2 * ERROR_FRESH_MS).status(now), "idle");
        assert_eq!(mk("???", "", now).status(now), "unknown");
    }

    #[test]
    fn workspaces_carry_the_ask_and_the_last_answer() {
        let wts = parse_ps(&ps_fixture());
        let ws = workspaces_from(&wts, 1793030100000, 10);
        // The main checkout has no agent, so it isn't a workspace line.
        assert_eq!(ws.len(), 1);
        let w = &ws[0];
        assert_eq!(w.project, "app");
        assert_eq!(w.branch, "feat/login");
        let sess = w.session.as_ref().unwrap();
        // A working agent outranks a finished one on the same worktree.
        assert_eq!(sess.status, "working");
        assert_eq!(sess.agent, "claude");
        assert_eq!(sess.original_ask.as_deref(), Some("Fix the login redirect that lands on /404."));
        assert_eq!(sess.preview.as_deref(), Some("Looking at the callback handler now."));
        assert_eq!(sess.unread_count, 1);
    }

    #[test]
    fn a_finished_agent_is_fresh_to_the_recap_via_iso_activity() {
        let mut wts = parse_ps(&ps_fixture());
        wts[0].agents.remove(0); // leave only the finished one
        let ws = workspaces_from(&wts, 1793030100000, 10);
        let sess = ws[0].session.as_ref().unwrap();
        assert_eq!(sess.status, "idle");
        assert_eq!(sess.last_activity.as_deref(), Some(iso_at(1793000000).as_str()));
    }

    /// The recap's phrasing logic is shared with Conductor; this pins that Orca
    /// data lands in the right branches of it.
    #[test]
    fn the_recap_speaks_waiting_done_and_working_from_orca_data() {
        let now = now_ms();
        let agent = |key: &str, state: &str, msg: &str, started: u64| {
            json!({
                "paneKey": key, "state": state, "agentType": "claude", "prompt": "Ship the thing please.",
                "lastAssistantMessage": msg, "interrupted": false,
                "mainAgent": { "state": state, "outcome": "success", "stateStartedAt": started },
                "stateStartedAt": started, "updatedAt": started
            })
        };
        let wt = |repo: &str, agents: Vec<Value>| {
            json!({ "repoId": repo, "hostId": "local", "repo": repo, "path": format!("/w/{repo}"),
                    "branch": "refs/heads/b", "isArchived": false, "isMainWorktree": false,
                    "displayName": repo, "lastActivityAt": now, "agents": agents })
        };
        let wts = parse_ps(&json!({ "worktrees": [
            wt("alpha", vec![agent("a", "blocked", "Can I delete the old table?", now)]),
            wt("beta", vec![agent("b", "done", "Shipped it, all green.", now - 60_000)]),
            wt("gamma", vec![agent("c", "working", "On it.", now)]),
            wt("delta", vec![agent("d", "done", "Shipped long ago.", now - 3 * 86_400_000)]),
        ]}));
        let ws = workspaces_from(&wts, now, 10);
        let out = crate::workspace::build_brief_sentences(&ws, true, 0);
        let said = |p: &str| out.iter().find(|(_, w)| w.as_deref() == Some(p)).map(|(t, _)| t.clone());
        // Phrasing rotates with position; the wording of each case does not.
        let (alpha, beta, gamma) = (said("alpha").unwrap(), said("beta").unwrap(), said("gamma").unwrap());
        assert!(alpha.contains("waiting") || alpha.contains("blocked") || alpha.contains("needs you"), "{alpha}");
        assert!(beta.contains("done") || beta.contains("ready") || beta.contains("finished"), "{beta}");
        assert!(gamma.contains("working") || gamma.contains("on it") || gamma.contains("moving"), "{gamma}");
        // Finished days ago: grouped as quiet, not re-announced as fresh.
        assert!(out.iter().any(|(t, _)| t.contains("delta") && t.starts_with("Nothing new")));
    }

    #[test]
    fn catalog_has_repos_and_non_main_worktrees() {
        let wts = parse_ps(&ps_fixture());
        let repos = parse_repos(&repos_fixture());
        let cat = candidates_from(&wts, &repos);
        let names: Vec<(&str, &str)> = cat.iter().map(|c| (c.kind, c.name.as_str())).collect();
        assert!(names.contains(&("worktree", "Login redirect")));
        assert!(names.contains(&("repo", "app")));
        // No worktree activity at all — still launchable.
        assert!(names.contains(&("repo", "quiet")));
        // "main" is the repo row, not a target of its own.
        assert!(!names.iter().any(|(_, n)| *n == "main"));
        let wt = cat.iter().find(|c| c.kind == "worktree").unwrap();
        assert_eq!(wt.repo, "app");
        assert!(wt.live);
        assert_eq!(wt.branch, "feat/login");
        let app = cat.iter().find(|c| c.kind == "repo" && c.name == "app").unwrap();
        assert_eq!(app.branch, "main");
        assert_eq!(app.path, "/w/app");
        // Newest first, compared as plain strings.
        assert!(cat.windows(2).all(|p| p[0].ts >= p[1].ts));
    }

    #[test]
    fn ts_from_ms_is_sortable_and_shaped_like_conductors() {
        assert_eq!(ts_from_ms(0), "1970-01-01 00:00:00");
        assert!(ts_from_ms(1793030080449) > ts_from_ms(1793000000000));
    }

    #[test]
    fn counts_only_working_agents() {
        let wts = parse_ps(&ps_fixture());
        assert_eq!(working_count(&wts), 1);
        assert_eq!(working_in(&wts, "/w/app-porto"), 1);
        assert_eq!(working_in(&wts, "/w/app"), 0);
    }

    fn snap(state: AgentState, outcome: &str, started: u64) -> AgentSnap {
        AgentSnap {
            state,
            outcome: outcome.into(),
            cancelled: outcome == "cancellation",
            state_started_ms: started,
            last_message: "done".into(),
            project: "app".into(),
            path: "/w/app".into(),
            branch: "b".into(),
            agent_type: "claude".into(),
            prompt: "p".into(),
        }
    }

    #[test]
    fn working_to_done_is_a_finish() {
        let was = snap(AgentState::Working, "", 100);
        let now = snap(AgentState::Done, "success", 200);
        assert_eq!(detect(Some(&was), &now, 0), Some(Event::Finished));
    }

    #[test]
    fn working_to_done_with_failure_is_an_error() {
        let was = snap(AgentState::Working, "", 100);
        let now = snap(AgentState::Done, "failure", 200);
        assert_eq!(detect(Some(&was), &now, 0), Some(Event::Errored));
    }

    #[test]
    fn a_stopped_agent_is_not_announced() {
        let was = snap(AgentState::Working, "", 100);
        let now = snap(AgentState::Done, "cancellation", 200);
        assert_eq!(detect(Some(&was), &now, 0), None);
    }

    #[test]
    fn a_missed_turn_still_counts_when_the_timestamp_advanced() {
        // done → (working → done between two polls) → done: same state, newer stamp.
        let was = snap(AgentState::Done, "success", 200);
        let now = snap(AgentState::Done, "success", 900);
        assert_eq!(detect(Some(&was), &now, 0), Some(Event::Finished));
    }

    #[test]
    fn an_unchanged_done_agent_is_history() {
        let was = snap(AgentState::Done, "success", 200);
        assert_eq!(detect(Some(&was), &was.clone(), 0), None);
    }

    #[test]
    fn blocked_or_waiting_needs_input_once() {
        let was = snap(AgentState::Working, "", 100);
        let now = snap(AgentState::Waiting, "", 200);
        assert_eq!(detect(Some(&was), &now, 0), Some(Event::NeedsInput));
        // Still waiting on the same question: no repeat.
        assert_eq!(detect(Some(&now), &now.clone(), 0), None);
    }

    #[test]
    fn a_newly_seen_agent_counts_only_if_it_settled_after_boot() {
        let old = snap(AgentState::Done, "success", 100);
        let new = snap(AgentState::Done, "success", 5_000);
        assert_eq!(detect(None, &old, 1_000), None);
        assert_eq!(detect(None, &new, 1_000), Some(Event::Finished));
        // A working agent that appears is simply not news yet.
        assert_eq!(detect(None, &snap(AgentState::Working, "", 5_000), 1_000), None);
    }

    #[test]
    fn a_bounce_back_to_working_cancels_the_candidate() {
        let done = snap(AgentState::Done, "success", 200);
        assert!(still_settled(Event::Finished, 200, &done));
        // Back to work, or finished again later: not the candidate we saw.
        assert!(!still_settled(Event::Finished, 200, &snap(AgentState::Working, "", 300)));
        assert!(!still_settled(Event::Finished, 200, &snap(AgentState::Done, "success", 900)));
        assert!(still_settled(Event::NeedsInput, 200, &snap(AgentState::Waiting, "", 200)));
    }

    #[test]
    fn snapshots_key_on_pane_and_carry_the_worktree() {
        let snaps = snapshots(&parse_ps(&ps_fixture()));
        let keys: Vec<&str> = snaps.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["pane-a", "pane-b"]);
        assert_eq!(snaps[0].1.project, "app");
        assert_eq!(snaps[0].1.path, "/w/app-porto");
        assert_eq!(snaps[1].1.outcome, "success");
    }

    #[test]
    fn envelope_errors_surface_their_code() {
        let err = br#"{"ok":false,"error":{"code":"selector_not_found","message":"selector_not_found"}}"#;
        assert_eq!(parse_response(err, "").unwrap_err(), "selector_not_found");
        let ok = br#"{"id":"x","ok":true,"result":{"repos":[]}}"#;
        assert_eq!(parse_response(ok, "").unwrap(), json!({ "repos": [] }));
        assert!(parse_response(b"not json", "runtime_unavailable").unwrap_err().contains("runtime"));
    }

    #[test]
    fn launch_result_gives_the_handle_to_close_later() {
        let r = json!({
            "outcome": { "kind": "terminal", "handle": "term_abc" },
            "receipt": { "detail": "Chat UI off" },
            "worktreeId": "r1::/w/app",
            "prompt": { "outcome": "handed-to-terminal" },
            "warning": "slow start"
        });
        let l = parse_launch(&r).unwrap();
        assert_eq!(l.handle, "term_abc");
        assert_eq!(l.kind, "terminal");
        assert_eq!(l.prompt_outcome, "handed-to-terminal");
        assert_eq!(l.warning.as_deref(), Some("slow start"));
        assert!(parse_launch(&json!({ "outcome": {} })).is_err());
        assert!(parse_launch(&json!({})).is_err());
    }

    /// Read-only against the real Orca — run explicitly with
    /// `cargo test -- --ignored --nocapture live_orca`.
    #[test]
    #[ignore]
    fn live_orca() {
        let wts = ps().expect("orca not reachable");
        println!("{} worktrees, {} agents working", wts.len(), working_count(&wts));
        for w in workspaces_from(&wts, now_ms(), 10) {
            let s = w.session.as_ref().unwrap();
            println!("  {:20} {:8} {:8} ask={:?} last={:?}", w.project, s.agent, s.status, s.original_ask, s.preview.as_deref().map(|p| p.chars().take(40).collect::<String>()));
        }
        for (sentence, _) in crate::workspace::build_brief_sentences(&workspaces_from(&wts, now_ms(), 10), true, 0) {
            println!("  recap: {sentence}");
        }
        let cat = catalog();
        println!("{} launchable targets", cat.len());
        for c in cat.iter().take(12) {
            println!("  {:9} {:30} repo={:20} live={} ts={} {}", c.kind, c.name, c.repo, c.live, c.ts, c.path);
        }
        assert!(!cat.is_empty());
    }
}
