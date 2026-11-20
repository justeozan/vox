//! Orca backend — worktree state and agent launches through the `orca` CLI.
//!
//! One `orca worktree ps --json` call (~90ms) carries the whole picture: every
//! worktree, its repo, branch and path, plus each agent pane's state, the prompt
//! it was given and its last message. That single call replaces Conductor's
//! 1 + 3N `sqlite3` spawns against a private database file, and with it three
//! workarounds this crate used to need — see `conductor.rs` for their epitaphs.
//!
//! Everything here treats the CLI as the contract. `orca agent-context --json`
//! publishes the command schema, `orca skills get orca-cli` the usage rules, and
//! both are versioned with the app; the runtime's unix socket and WebSocket
//! endpoint in `orca-runtime.json` are deliberately NOT used, because they carry
//! an auth token and no compatibility promise.

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::ade::{AgentState, SessionInfo, Snapshot, TargetCandidate, Workspace};
use crate::home;

/// Rows to ask for. Well past any real worktree count; `truncated` in the reply
/// tells us if it wasn't enough.
const PS_LIMIT: usize = 500;

/// How long a `worktree ps` reply is reused.
///
/// One voice turn reads the catalog, the workspace state and the launchable
/// names — three reads that were three independent CLI spawns. They all want the
/// same 90ms answer within milliseconds of each other, so a short TTL collapses
/// them into one without ever serving state a user could perceive as stale.
const PS_TTL: Duration = Duration::from_millis(1500);

// ── CLI resolution ───────────────────────────────────────────────────────────

/// The `orca` executable, probed once.
///
/// Order matters. The app bundle comes BEFORE `$PATH` because Orca's installed
/// `/usr/local/bin/orca` is a root-owned symlink with `lrwx------` permissions:
/// a process that cannot `readlink` it gets `Unable to determine Orca.app path
/// from symlink` instead of a working CLI, and a sandboxed or hardened-runtime
/// Vox is exactly such a process. Going to the bundle first sidesteps it, and
/// validating each candidate with `--version` means a broken one falls through
/// rather than poisoning every later call.
///
/// (On Linux a bare `orca` on `$PATH` is the GNOME screen reader, which would
/// start reading the desktop aloud — a spectacular failure for a voice app. Vox
/// is macOS-only, and bundle-first keeps it that way by construction.)
fn cli() -> Option<&'static PathBuf> {
    static CLI: OnceLock<Option<PathBuf>> = OnceLock::new();
    CLI.get_or_init(|| {
        let mut candidates: Vec<PathBuf> = Vec::new();
        // Orca exports this for managed WSL sessions and dev checkouts.
        if let Ok(explicit) = std::env::var("ORCA_CLI_COMMAND") {
            if !explicit.trim().is_empty() {
                candidates.push(PathBuf::from(explicit));
            }
        }
        const BUNDLE: &str = "Orca.app/Contents/Resources/bin/orca";
        candidates.push(PathBuf::from("/Applications").join(BUNDLE));
        candidates.push(home().join("Applications").join(BUNDLE));
        if let Some(p) = crate::daemons::find_bin("orca", &[]) {
            candidates.push(p);
        }

        for c in candidates {
            if !c.is_file() {
                continue;
            }
            let ok = Command::new(&c)
                .arg("--version")
                .output()
                .map(|o| o.status.success() && !o.stdout.is_empty())
                .unwrap_or(false);
            if ok {
                println!("[vox] orca CLI: {}", c.display());
                return Some(c);
            }
            eprintln!("[vox] orca CLI at {} did not answer --version", c.display());
        }
        None
    })
    .as_ref()
}

/// Is Orca available at all? Cheap after the first call.
pub fn installed() -> bool {
    cli().is_some()
}

// ── CLI plumbing ─────────────────────────────────────────────────────────────

/// Run an `orca` subcommand and return its `result` payload.
///
/// The CLI exits 0 even when it fails — errors arrive as `{"ok":false,"error":…}`
/// in the JSON envelope — so the exit status is never the signal. Checking `ok`
/// is the only correct test, and the error code is logged rather than swallowed
/// so a wrong selector is debuggable from Vox's console.
fn run(args: &[&str]) -> Option<Value> {
    let exe = cli()?;
    let out = Command::new(exe).args(args).output().ok()?;
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    if v.get("ok").and_then(|b| b.as_bool()) == Some(true) {
        return v.get("result").cloned();
    }
    let code = v
        .pointer("/error/code")
        .and_then(|c| c.as_str())
        .unwrap_or("no_json_envelope");
    eprintln!("[vox] orca {} → {code}", args.join(" "));
    None
}

/// When a `worktree ps` reply was taken, and the rows it returned.
type PsCache = (Instant, Vec<Value>);

/// `worktree ps` rows, cached for `PS_TTL`.
fn ps() -> Option<Vec<Value>> {
    static CACHE: OnceLock<Mutex<Option<PsCache>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));

    if let Some((at, rows)) = cache.lock().unwrap().as_ref() {
        if at.elapsed() < PS_TTL {
            return Some(rows.clone());
        }
    }

    let limit = PS_LIMIT.to_string();
    let result = run(&["worktree", "ps", "--limit", &limit, "--json"])?;
    if result.get("truncated").and_then(|t| t.as_bool()) == Some(true) {
        let total = result.get("totalCount").and_then(|t| t.as_i64()).unwrap_or(-1);
        eprintln!("[vox] orca worktree ps truncated at {PS_LIMIT} of {total} — some targets are unreachable by voice");
    }
    let rows: Vec<Value> = result.get("worktrees")?.as_array()?.clone();
    *cache.lock().unwrap() = Some((Instant::now(), rows.clone()));
    Some(rows)
}

fn s(v: &Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

fn n(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(|x| x.as_u64()).unwrap_or(0)
}

/// `refs/heads/main` → `main`. Orca reports the full ref; nobody says "refs".
fn short_branch(raw: &str) -> String {
    raw.strip_prefix("refs/heads/").unwrap_or(raw).to_string()
}

/// Orca's own agent vocabulary, mapped onto Vox's.
///
/// `interrupted` folds into `Idle` on purpose: a turn the user themselves cut
/// short is not news to announce back to them.
fn agent_state(raw: &str) -> AgentState {
    match raw {
        "working" => AgentState::Working,
        "done" => AgentState::Done,
        "blocked" => AgentState::Blocked,
        "error" => AgentState::Errored,
        _ => AgentState::Idle,
    }
}

/// The agent pane that speaks for a worktree: the most recently updated one.
///
/// A worktree can hold several agent panes (a finished Codex next to a blocked
/// Claude). Newest-wins matches what Orca's own card shows and what Conductor
/// did with its sessions.
fn lead_agent(row: &Value) -> Option<&Value> {
    row.get("agents")?
        .as_array()?
        .iter()
        .max_by_key(|a| n(a, "updatedAt").max(n(a, "stateStartedAt")))
}

fn session_of(row: &Value) -> Option<SessionInfo> {
    let a = lead_agent(row)?;
    // A worktree-level `permission` status is Orca telling us a pane is holding
    // a permission prompt. That outranks the pane's own state, which can still
    // read `working` while the prompt waits.
    let state = if s(row, "status") == "permission" {
        AgentState::Blocked
    } else {
        agent_state(&s(a, "state"))
    };
    let text = |k: &str, cap: usize| -> Option<String> {
        let raw = s(a, k);
        let t = raw.trim();
        (!t.is_empty()).then(|| t.chars().take(cap).collect())
    };
    Some(SessionInfo {
        agent: s(a, "agentType"),
        state,
        last_activity: Some(n(a, "updatedAt")),
        preview: text("lastAssistantMessage", 300),
        // Orca hands us the launching prompt directly. Conductor had to dig the
        // first user message out of a message table and unwrap three possible
        // JSON envelopes to find the same string.
        original_ask: text("prompt", 150),
        unread: row.get("unread").and_then(|u| u.as_bool()).unwrap_or(false),
    })
}

fn live(row: &Value) -> bool {
    row.get("agents")
        .and_then(|a| a.as_array())
        .map(|a| a.iter().any(|x| s(x, "state") == "working"))
        .unwrap_or(false)
}

// ── Reads ────────────────────────────────────────────────────────────────────

/// Worktrees worth talking about, the ones with agents first.
///
/// Ordering by "has an agent" before recency is the point: most of a busy
/// machine's worktrees are just open shells, and sorting purely by activity
/// would fill all ten recap slots with them and leave the one agent that
/// actually finished unmentioned.
pub fn read_state(max_items: usize) -> Option<Vec<Workspace>> {
    let rows = ps()?;
    let mut keep: Vec<&Value> = rows
        .iter()
        .filter(|r| !r.get("isArchived").and_then(|b| b.as_bool()).unwrap_or(false))
        .filter(|r| {
            let has_agent = r
                .get("agents")
                .and_then(|a| a.as_array())
                .map(|a| !a.is_empty())
                .unwrap_or(false);
            has_agent || s(r, "workspaceStatus") == "in-progress"
        })
        .collect();
    keep.sort_by_key(|r| {
        let has_agent = r
            .get("agents")
            .and_then(|x| x.as_array())
            .map(|x| !x.is_empty())
            .unwrap_or(false);
        std::cmp::Reverse((has_agent, n(r, "lastActivityAt")))
    });
    Some(
        keep.into_iter()
            .take(max_items)
            .map(|r| Workspace {
                project: s(r, "repo"),
                branch: short_branch(&s(r, "branch")),
                path: s(r, "path"),
                session: session_of(r),
            })
            .collect(),
    )
}

/// Every launchable target, most recently touched first.
///
/// `ps` already lists each repo's main checkout (`isMainWorktree`), so the repo
/// tier costs nothing extra — no second `repo list` call.
///
/// A worktree is listed under BOTH names it has, because they serve different
/// sentences. Orca's `displayName` is the branch, which is what you'd read off
/// the card ("add-mobile-apk-build") but also what a branch convention turns
/// into an unsayable mouthful ("justeozan/devis-site-avec-fonctionnalites"). The
/// directory basename is Orca's generated codename — "surabaya", "tyler" — short,
/// pronounceable, and what a person actually says out loud. Emitting both means
/// either lands; `targets` collapses two rows pointing at one repo into a single
/// answer rather than calling it ambiguous.
pub fn catalog() -> Option<Vec<TargetCandidate>> {
    let rows = ps()?;
    let mut out: Vec<TargetCandidate> = Vec::new();
    for r in rows
        .iter()
        .filter(|r| !r.get("isArchived").and_then(|b| b.as_bool()).unwrap_or(false))
    {
        let is_main = r.get("isMainWorktree").and_then(|b| b.as_bool()).unwrap_or(false);
        let repo = s(r, "repo");
        let path = s(r, "path");
        let mut push = |name: String, kind: &'static str| {
            // A row whose directory is gone would send an agent nowhere.
            if name.is_empty() || path.is_empty() || !std::path::Path::new(&path).exists() {
                return;
            }
            out.push(TargetCandidate {
                name,
                kind,
                repo: repo.clone(),
                path: path.clone(),
                branch: short_branch(&s(r, "branch")),
                ts: n(r, "lastActivityAt"),
                live: live(r),
            });
        };
        if is_main {
            push(repo.clone(), "repo");
            continue;
        }
        let display = s(r, "displayName");
        push(display.clone(), "worktree");
        let codename = std::path::Path::new(&path)
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default();
        if !codename.is_empty() && !codename.eq_ignore_ascii_case(&display) {
            push(codename, "worktree");
        }
    }
    out.sort_by_key(|c| std::cmp::Reverse(c.ts));
    Some(out)
}

/// One watcher tick: a snapshot per agent pane.
///
/// Keyed on `paneKey` rather than the worktree, so two agents in one checkout
/// are two independently-announceable sessions. The watermark is the pane's
/// `updatedAt`, which advances once per turn — the same idempotent, naturally
/// debounced signal the Conductor watcher had to reconstruct from a message
/// table's `max(sent_at)`.
pub fn poll(limit: usize) -> Option<Vec<Snapshot>> {
    let rows = ps()?;
    let mut out: Vec<Snapshot> = Vec::new();
    for r in &rows {
        let Some(agents) = r.get("agents").and_then(|a| a.as_array()) else { continue };
        let blocked = s(r, "status") == "permission";
        for a in agents {
            let key = s(a, "paneKey");
            if key.is_empty() {
                continue;
            }
            let answer = s(a, "lastAssistantMessage");
            let ask = s(a, "prompt");
            out.push(Snapshot {
                key,
                state: if blocked { AgentState::Blocked } else { agent_state(&s(a, "state")) },
                watermark: n(a, "updatedAt").max(n(a, "stateStartedAt")),
                project: s(r, "repo"),
                path: s(r, "path"),
                branch: short_branch(&s(r, "branch")),
                agent: s(a, "agentType"),
                answer: (!answer.trim().is_empty()).then(|| answer.chars().take(8000).collect()),
                original_ask: (!ask.trim().is_empty()).then(|| ask.chars().take(150).collect()),
            });
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.watermark));
    out.truncate(limit);
    Some(out)
}

// ── Push signal ──────────────────────────────────────────────────────────────

/// Orca's agent-status journal. It rewrites this file every time an agent pane
/// changes state, which makes its mtime a genuine push edge — the watcher wakes
/// on it instead of discovering the same thing up to a full tick later.
///
/// Only the mtime is read, never the contents: the file sits beside Orca's
/// tokens and cookies, and `worktree ps` already returns everything in it
/// through a supported interface.
pub fn hooks_mtime() -> Option<u64> {
    let p = user_data().join("agent-hooks/last-status.json");
    let meta = std::fs::metadata(p).ok()?;
    let modified = meta.modified().ok()?;
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

fn user_data() -> PathBuf {
    std::env::var("ORCA_USER_DATA_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join("Library/Application Support/orca"))
}

// ── Launching ────────────────────────────────────────────────────────────────

/// A launched Orca agent. No pid and no stdout: the agent runs inside Orca, in a
/// real terminal the user can take over, and its answer comes back through the
/// watcher like any other Orca agent's.
#[derive(Clone, Debug)]
pub struct Launched {
    pub worktree_id: String,
    pub terminal: String,
    pub path: String,
}

/// `terminal wait --for tui-idle`, then the send. A prompt typed into a TUI that
/// is still starting is simply lost, so the wait is not optional.
///
/// A timed-out wait still prints a normal result, so `satisfied` is the signal —
/// not the fact that something was printed.
fn send_prompt(terminal: &str, prompt: &str) -> Result<(), String> {
    let waited = run(&[
        "terminal", "wait", "--terminal", terminal, "--for", "tui-idle", "--timeout-ms", "60000",
        "--json",
    ]);
    let satisfied = waited
        .as_ref()
        .and_then(|v| find_bool(v, "satisfied"))
        .unwrap_or(false);
    if !satisfied {
        return Err(format!("{terminal} never became ready"));
    }
    let sent = run(&[
        "terminal", "send", "--terminal", terminal, "--text", prompt, "--enter", "--wait-submit",
        "10", "--json",
    ])
    .ok_or_else(|| format!("send to {terminal} was refused"))?;
    // `accepted` proves input acceptance, not a started turn — but a refused
    // send is the only failure worth reporting out loud. Never resend on
    // silence: the CLI's own guidance is that a bare `input_accepted` receipt
    // means "unproven", not "failed".
    if find_bool(&sent, "accepted") == Some(false) {
        return Err(format!("{terminal} did not accept the prompt"));
    }
    Ok(())
}

/// Find a boolean by key anywhere in a result tree.
///
/// The receipts nest differently across Orca versions (`satisfied` has lived at
/// the top level and under `wait`), and guessing one path would break silently
/// on the other — which for `satisfied` means sending a prompt into a TUI that
/// never came up.
fn find_bool(v: &Value, key: &str) -> Option<bool> {
    match v {
        Value::Object(map) => {
            if let Some(b) = map.get(key).and_then(|x| x.as_bool()) {
                return Some(b);
            }
            map.values().find_map(|x| find_bool(x, key))
        }
        Value::Array(items) => items.iter().find_map(|x| find_bool(x, key)),
        _ => None,
    }
}

/// Start `agent` in the checkout at `path` and hand it `prompt`.
///
/// Used for a worktree target: the work belongs in the tree the user named, and
/// `terminal create` is the documented way to put a fresh agent in an existing
/// checkout (`worktree create` would make a new one).
pub fn launch_in(path: &str, agent: &str, prompt: &str, title: &str) -> Result<Launched, String> {
    let selector = format!("path:{path}");
    let created = run(&[
        "terminal", "create", "--worktree", &selector, "--title", title, "--command", agent,
        "--json",
    ])
    .ok_or_else(|| format!("Orca refused a terminal in {path}"))?;
    let terminal = find_str(&created, "handle")
        .ok_or_else(|| "Orca returned no terminal handle".to_string())?;
    send_prompt(&terminal, prompt)?;
    Ok(Launched {
        worktree_id: find_str(&created, "worktreeId").unwrap_or_default(),
        terminal,
        path: path.to_string(),
    })
}

/// Create a fresh worktree on `repo_path` with `agent` already working on
/// `prompt` — one call, and the agent lands in the first terminal.
///
/// This is the right shape for a repo target: running an unsupervised agent in a
/// repo's main checkout dirties the tree that worktrees exist to protect.
pub fn launch_new_worktree(
    repo_path: &str,
    name: &str,
    agent: &str,
    prompt: &str,
) -> Result<Launched, String> {
    let selector = format!("path:{repo_path}");
    let created = run(&[
        "worktree", "create", "--repo", &selector, "--name", name, "--no-parent", "--agent", agent,
        "--prompt", prompt, "--json",
    ])
    .ok_or_else(|| format!("Orca refused a worktree on {repo_path}"))?;
    // Newer runtimes name the agent's terminal directly; older ones only expose
    // the startup terminal, and folder-backed repos may expose neither.
    let terminal = created
        .get("agentTerminalHandle")
        .and_then(|h| h.as_str())
        .map(String::from)
        .or_else(|| find_str(&created, "handle"))
        .unwrap_or_default();
    let worktree_id = created
        .pointer("/worktree/id")
        .and_then(|i| i.as_str())
        .map(String::from)
        .unwrap_or_default();
    // The id is `<repoId>::<worktreePath>`; the path half is where the agent is.
    let path = worktree_id
        .split_once("::")
        .map(|(_, p)| p.to_string())
        .or_else(|| find_str(&created, "path"))
        .unwrap_or_else(|| repo_path.to_string());
    Ok(Launched { worktree_id, terminal, path })
}

fn find_str(v: &Value, key: &str) -> Option<String> {
    match v {
        Value::Object(map) => {
            if let Some(s) = map.get(key).and_then(|x| x.as_str()) {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
            map.values().find_map(|x| find_str(x, key))
        }
        Value::Array(items) => items.iter().find_map(|x| find_str(x, key)),
        _ => None,
    }
}

/// Interrupt an agent Vox started, without closing the terminal — the user can
/// still read what it did and pick the work back up.
pub fn interrupt(terminal: &str) -> bool {
    run(&["terminal", "send", "--terminal", terminal, "--interrupt", "--json"]).is_some()
}

/// Leave a status line on the worktree card, so Orca's own UI shows that this
/// work came from Vox.
pub fn set_comment(path: &str, comment: &str) {
    let selector = format!("path:{path}");
    let _ = run(&["worktree", "set", "--worktree", &selector, "--comment", comment, "--json"]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row() -> Value {
        json!({
            "repo": "findy",
            "path": "/tmp",
            "branch": "refs/heads/fix-login",
            "displayName": "fix-login",
            "isMainWorktree": false,
            "isArchived": false,
            "workspaceStatus": "in-progress",
            "lastActivityAt": 1_790_000_000_000u64,
            "status": "working",
            "unread": true,
            "agents": [
                {"paneKey": "a:1", "state": "done", "agentType": "codex",
                 "prompt": "fix the login", "lastAssistantMessage": "Done.",
                 "updatedAt": 1_000u64},
                {"paneKey": "a:2", "state": "working", "agentType": "claude",
                 "prompt": "add tests", "lastAssistantMessage": "Working on it.",
                 "updatedAt": 2_000u64},
            ]
        })
    }

    #[test]
    fn branch_refs_are_stripped() {
        assert_eq!(short_branch("refs/heads/main"), "main");
        assert_eq!(short_branch("main"), "main");
        // A detached-ish ref that isn't a head must survive unmangled.
        assert_eq!(short_branch("refs/tags/v1"), "refs/tags/v1");
    }

    #[test]
    fn orca_states_map_onto_vox_states() {
        assert_eq!(agent_state("working"), AgentState::Working);
        assert_eq!(agent_state("done"), AgentState::Done);
        assert_eq!(agent_state("blocked"), AgentState::Blocked);
        assert_eq!(agent_state("error"), AgentState::Errored);
        assert_eq!(agent_state("idle"), AgentState::Idle);
        // A turn the user cut short is not news.
        assert_eq!(agent_state("interrupted"), AgentState::Idle);
        // An unknown future state must be quiet, never mis-announced.
        assert_eq!(agent_state("teleporting"), AgentState::Idle);
    }

    #[test]
    fn the_newest_agent_pane_speaks_for_the_worktree() {
        let r = row();
        let a = lead_agent(&r).unwrap();
        assert_eq!(s(a, "paneKey"), "a:2");
        let sess = session_of(&r).unwrap();
        assert_eq!(sess.agent, "claude");
        assert_eq!(sess.state, AgentState::Working);
        assert_eq!(sess.original_ask.as_deref(), Some("add tests"));
        assert!(sess.unread);
    }

    /// A permission prompt is the whole reason to interrupt the user, and the
    /// pane can still read `working` while it waits.
    #[test]
    fn a_permission_prompt_outranks_the_pane_state() {
        let mut r = row();
        r["status"] = json!("permission");
        assert_eq!(session_of(&r).unwrap().state, AgentState::Blocked);
    }

    #[test]
    fn every_agent_pane_is_independently_announceable() {
        let snaps: Vec<Snapshot> = {
            let rows = vec![row()];
            let mut out = Vec::new();
            for r in &rows {
                for a in r["agents"].as_array().unwrap() {
                    out.push(Snapshot {
                        key: s(a, "paneKey"),
                        state: agent_state(&s(a, "state")),
                        watermark: n(a, "updatedAt"),
                        project: s(r, "repo"),
                        path: s(r, "path"),
                        branch: short_branch(&s(r, "branch")),
                        agent: s(a, "agentType"),
                        answer: Some(s(a, "lastAssistantMessage")),
                        original_ask: Some(s(a, "prompt")),
                    });
                }
            }
            out
        };
        assert_eq!(snaps.len(), 2, "two panes in one checkout are two sessions");
        assert_ne!(snaps[0].key, snaps[1].key);
    }

    /// The receipts nest differently across Orca versions, so the readers have
    /// to find their field wherever it lives.
    #[test]
    fn receipt_fields_are_found_at_any_depth() {
        assert_eq!(find_bool(&json!({"satisfied": true}), "satisfied"), Some(true));
        assert_eq!(find_bool(&json!({"wait": {"satisfied": false}}), "satisfied"), Some(false));
        assert_eq!(find_bool(&json!({"a": [{"b": {"accepted": true}}]}), "accepted"), Some(true));
        assert_eq!(find_bool(&json!({"nope": 1}), "satisfied"), None);
        assert_eq!(find_str(&json!({"t": {"handle": "h-1"}}), "handle").as_deref(), Some("h-1"));
        // An empty string is not a handle.
        assert_eq!(find_str(&json!({"handle": ""}), "handle"), None);
    }

    /// Hits the real Orca CLI — run explicitly with
    /// `cargo test -- --ignored --nocapture live_catalog`.
    #[test]
    #[ignore]
    fn live_catalog() {
        assert!(installed(), "orca CLI not found");
        let cat = catalog().expect("orca answered nothing — is it running?");
        println!("{} launchable targets", cat.len());
        for c in &cat {
            println!("  {:9} {:26} repo={:22} live={} {}", c.kind, c.name, c.repo, c.live, c.path);
        }
        assert!(!cat.is_empty());
        assert!(cat.iter().all(|c| std::path::Path::new(&c.path).exists()));
        let ws = read_state(10).unwrap();
        println!("\n{} worktrees in the recap", ws.len());
        for w in &ws {
            println!("  {:22} {:?}", w.project, w.session.as_ref().map(|s| s.state));
        }
    }
}
