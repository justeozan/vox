//! Watcher — notices when an agent finishes, so Vox can tell you without being
//! asked. One thread polls whichever backend is selected.
//!
//! **Conductor.** Detection keys on `max(session_messages.sent_at)` per session,
//! NOT on the `working → idle` status transition. Two reasons, both
//! load-bearing:
//!
//!  * a poll tick can straddle an entire turn (`working → idle → working`), so
//!    a status edge is simply missed;
//!  * `sessions.unread_count` — Conductor's own "there's something new here"
//!    flag — is 0 on every row in practice, so anything built on it never fires.
//!
//! The message watermark advances exactly once per assistant turn, which makes
//! the detector naturally idempotent and self-debouncing.
//!
//! **Orca.** One `worktree ps` per tick gives every agent's state. An agent
//! finishing is its `state` reaching `done` with `stateStartedAt` newer than
//! before (see `orca::detect`) — the timestamp, like the watermark above, is
//! what survives a poll straddling a whole turn.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tauri::{AppHandle, Emitter};

use crate::announce::{self, AnnounceKind, Announcement};
use crate::orca::{self, AgentSnap, Event};
use crate::workspace::{self, Backend};
use crate::{now_unix, AppState};

/// Base poll interval. The query costs ~13ms, so this is ~0.3% duty.
const TICK: Duration = Duration::from_secs(4);
/// Backed-off interval after a long quiet stretch.
const SLOW_TICK: Duration = Duration::from_secs(15);
/// Unchanged ticks before backing off.
const QUIET_TICKS: u32 = 20;
/// Give the startup recap time to cover the current world before we start
/// reporting on it.
const FIRST_TICK_DELAY: Duration = Duration::from_secs(5);
/// How long a Vox-launched cwd suppresses Conductor announcements for the same
/// directory.
const VOX_CWD_WINDOW: u64 = 120;
/// Carousel refresh cadence.
const CARDS_EVERY: Duration = Duration::from_secs(30);

#[derive(Clone, PartialEq)]
struct SessSnap {
    status: String,
    last_asst: String,
    project: String,
    dir: String,
    path: String,
    dstatus: String,
    agent: String,
    branch: String,
}

/// One cheap query per tick — versus `read_state`'s 1 + 3N subprocess spawns.
///
/// Deliberately NOT filtered on `derived_status`: a workspace flips to `done`
/// at the very moment its agent finishes, so filtering to `in-progress` would
/// drop exactly the rows we exist to notice. (Measured on a real DB: 7 of 13
/// live sessions sit in `done` workspaces.)
fn poll_rows() -> Vec<serde_json::Value> {
    crate::conductor::sql_public(
        "SELECT s.id AS sid, s.status AS status, s.unread_count AS unread, \
                w.directory_name AS dir, w.workspace_path AS path, \
                w.derived_status AS dstatus, COALESCE(w.branch,'') AS branch, \
                s.agent_type AS agent, COALESCE(r.name,'') AS project, \
                (SELECT max(m.sent_at) FROM session_messages m \
                   WHERE m.session_id = s.id AND m.role = 'assistant') AS last_asst \
           FROM sessions s JOIN workspaces w ON s.workspace_id = w.id \
           LEFT JOIN repos r ON w.repository_id = r.id \
          WHERE s.is_hidden = 0 AND w.state = 'ready' \
            AND (r.hidden IS NULL OR r.hidden = 0) \
          ORDER BY strftime('%Y-%m-%dT%H:%M:%fZ', s.updated_at) DESC LIMIT 60;",
    )
}

pub fn start(app: AppHandle, state: Arc<AppState>) {
    std::thread::spawn(move || {
        std::thread::sleep(FIRST_TICK_DELAY);

        let mut backend = workspace::backend();
        let mut cond = ConductorWatch::new();
        let mut orca = OrcaWatch::new();
        loop {
            // Re-read each tick so flipping the setting takes effect live. The
            // new backend starts from a fresh baseline: whatever it already
            // shows is the world as it was, not news.
            let now_backend = workspace::backend();
            if now_backend != backend {
                println!("[vox] watcher switching to {}", now_backend.label());
                backend = now_backend;
                cond = ConductorWatch::new();
                orca = OrcaWatch::new();
            }
            let wait = match backend {
                Backend::Conductor => cond.tick(&app, &state),
                Backend::Orca => orca.tick(&app, &state),
            };
            std::thread::sleep(wait);
        }
    });
}

struct ConductorWatch {
    prev: HashMap<String, SessSnap>,
    // Candidates seen once, promoted only if they're still settled next
    // tick — a session can touch `idle` for a moment between tool calls.
    pending: HashMap<String, SessSnap>,
    baselined: bool,
    boot_watermark: String,
    quiet: u32,
    last_cards: Instant,
}

impl ConductorWatch {
    fn new() -> Self {
        Self {
            prev: HashMap::new(),
            pending: HashMap::new(),
            baselined: false,
            boot_watermark: String::new(),
            quiet: 0,
            last_cards: Instant::now() - CARDS_EVERY,
        }
    }

    /// One poll. Returns how long to sleep before the next.
    fn tick(&mut self, app: &AppHandle, state: &Arc<AppState>) -> Duration {
        let rows = poll_rows();
        if rows.is_empty() {
            // sqlite3 -readonly can't recover a stale WAL, so this is also
            // what "Conductor is closed" looks like. Failing quiet is right.
            return SLOW_TICK;
        }

        let mut changed = false;
        for row in &rows {
            let sid = s(row, "sid");
            if sid.is_empty() {
                continue;
            }
            let now = SessSnap {
                status: s(row, "status"),
                last_asst: s(row, "last_asst"),
                project: s(row, "project"),
                dir: s(row, "dir"),
                path: s(row, "path"),
                dstatus: s(row, "dstatus"),
                agent: s(row, "agent"),
                branch: s(row, "branch"),
            };

            let Some(was) = self.prev.get(&sid).cloned() else {
                // Never announce a session we're seeing for the first time.
                // Covers cold start, a Conductor restart, an un-hidden repo,
                // and rows drifting in and out of the LIMIT 60 window.
                self.prev.insert(sid, now);
                continue;
            };
            if now != was {
                changed = true;
            }

            if !self.baselined {
                self.prev.insert(sid, now);
                continue;
            }

            let new_answer = !now.last_asst.is_empty() && now.last_asst > was.last_asst;
            let kind = if now.status == "idle" && new_answer {
                Some(AnnounceKind::Finished)
            } else if now.status == "error" && was.status != "error" {
                Some(AnnounceKind::Errored)
            } else if now.status == "waiting" && was.status != "waiting" {
                Some(AnnounceKind::NeedsInput)
            } else {
                None
            };

            // Anything from before we started watching is history, not news.
            let stale = !now.last_asst.is_empty() && now.last_asst < self.boot_watermark;

            match kind {
                Some(k) if !stale => {
                    // Settle: promote only once the candidate is unchanged
                    // for a second tick. Back to `working` drops it, and the
                    // real finish re-arms the edge later.
                    match self.pending.get(&sid) {
                        Some(p) if p.last_asst == now.last_asst && now.status != "working" => {
                            self.pending.remove(&sid);
                            promote(app, state, &sid, &now, k);
                        }
                        _ => {
                            self.pending.insert(sid.clone(), now.clone());
                        }
                    }
                }
                _ => {
                    self.pending.remove(&sid);
                }
            }
            // Unconditional, including for suppressed cases — an edge must
            // never be able to re-fire.
            self.prev.insert(sid, now);
        }

        if !self.baselined {
            self.baselined = true;
            self.boot_watermark = iso_now();
            println!("[vox] conductor watcher baselined on {} sessions", self.prev.len());
        }

        // The carousel is only ever populated by `workspaces-data`, which
        // until now only the startup brief emitted — so an announcement had
        // no card to highlight. Feed it from the poll rows we already have,
        // never from read_state().
        if self.last_cards.elapsed() >= CARDS_EVERY {
            emit_cards(app, &rows);
            self.last_cards = Instant::now();
        }

        self.quiet = if changed { 0 } else { self.quiet.saturating_add(1) };
        if self.quiet >= QUIET_TICKS { SLOW_TICK } else { TICK }
    }
}

struct OrcaWatch {
    prev: HashMap<String, AgentSnap>,
    /// (event, state_started_ms) seen once; promoted if the agent is still
    /// exactly there a tick later.
    pending: HashMap<String, (Event, u64)>,
    baselined: bool,
    boot_ms: u64,
    quiet: u32,
    last_cards: Instant,
    last_working: Option<usize>,
}

impl OrcaWatch {
    fn new() -> Self {
        Self {
            prev: HashMap::new(),
            pending: HashMap::new(),
            baselined: false,
            boot_ms: 0,
            quiet: 0,
            last_cards: Instant::now() - CARDS_EVERY,
            last_working: None,
        }
    }

    fn tick(&mut self, app: &AppHandle, state: &Arc<AppState>) -> Duration {
        // Orca closed or wedged: orca.rs logged it once; stay quiet and slow.
        let Some(wts) = orca::ps_fresh() else { return SLOW_TICK };

        let mut changed = false;
        for (key, now) in orca::snapshots(&wts) {
            let was = self.prev.get(&key).cloned();
            if was.as_ref() != Some(&now) {
                changed = true;
            }
            if self.baselined {
                // Settle first: a candidate from last tick is promoted only if
                // the agent is still exactly where it was (a `done` that flips
                // straight back to `working` is a hiccup, not a finish).
                let mut promoted = false;
                if let Some((ev, started)) = self.pending.remove(&key) {
                    if orca::still_settled(ev, started, &now) {
                        promote_orca(app, state, &key, &now, ev);
                        promoted = true;
                    }
                }
                if !promoted {
                    if let Some(ev) = orca::detect(was.as_ref(), &now, self.boot_ms) {
                        self.pending.insert(key.clone(), (ev, now.state_started_ms));
                    }
                }
            }
            // Unconditional: an edge must never be able to re-fire.
            self.prev.insert(key, now);
        }

        if !self.baselined {
            self.baselined = true;
            self.boot_ms = now_unix() * 1000;
            println!("[vox] orca watcher baselined on {} agents", self.prev.len());
        }

        // Keep the badge honest about work happening in Orca, not only Vox's.
        let working = orca::working_count(&wts);
        if self.last_working != Some(working) {
            self.last_working = Some(working);
            state.orca_working.store(working, std::sync::atomic::Ordering::SeqCst);
            crate::agents::emit_agents(app, state);
        }

        if self.last_cards.elapsed() >= CARDS_EVERY {
            emit_orca_cards(app, &wts);
            self.last_cards = Instant::now();
        }

        self.quiet = if changed { 0 } else { self.quiet.saturating_add(1) };
        if self.quiet >= QUIET_TICKS { SLOW_TICK } else { TICK }
    }
}

/// Queue the announcement for an Orca agent that finished, errored or is
/// waiting. Unlike the Conductor path there is no "Vox launched it" guard: a
/// Vox-launched agent lives in Orca too, and this is the only place its answer
/// is ever spoken.
fn promote_orca(app: &AppHandle, state: &Arc<AppState>, key: &str, snap: &AgentSnap, ev: Event) {
    let answer = snap.last_message.trim().to_string();
    if answer.is_empty() && ev == Event::Finished {
        return;
    }
    let ask = Some(workspace::clean_snippet(&snap.prompt))
        .filter(|a| a.chars().count() > 3)
        .map(|a| a.chars().take(150).collect::<String>());
    let kind = match ev {
        Event::Finished => AnnounceKind::Finished,
        Event::Errored => AnnounceKind::Errored,
        Event::NeedsInput => AnnounceKind::NeedsInput,
    };

    announce::push_result(
        state,
        announce::AgentResult {
            project: snap.project.clone(),
            source: "orca",
            task: None,
            original_ask: ask.clone(),
            answer_full: answer.chars().take(8000).collect(),
            outcome: match ev {
                Event::Errored => "failed".into(),
                Event::NeedsInput => "waiting".into(),
                Event::Finished => "success".into(),
            },
            finished_unix: now_unix(),
        },
    );

    announce::enqueue(
        app,
        state,
        Announcement {
            id: 0,
            kind,
            // The timestamp makes each TURN its own announcement: the dedup
            // window would otherwise swallow a second answer from the same agent.
            key: format!("orca:{key}:{}", snap.state_started_ms),
            project: snap.project.clone(),
            answer,
            original_ask: ask,
            created_unix: now_unix(),
            attempts: 0,
            not_before: 0,
        },
    );
}

fn emit_orca_cards(app: &AppHandle, wts: &[orca::OrcaWorktree]) {
    let mut seen: HashSet<String> = HashSet::new();
    let cards: Vec<serde_json::Value> = orca::workspaces_from(wts, now_unix() * 1000, 40)
        .into_iter()
        .filter(|w| !w.project.is_empty() && seen.insert(w.project.clone()))
        .take(10)
        .map(|w| {
            let sess = w.session.as_ref();
            json!({
                "project": w.project,
                "branch": w.branch,
                "agent": sess.map(|s| s.agent.clone()).unwrap_or_default(),
                "status": sess.map(|s| s.status.clone()).unwrap_or_else(|| "none".into()),
                "unread": sess.map(|s| s.unread_count).unwrap_or(0),
                // As for Conductor: the announcement itself carries the content.
                "preview": "",
            })
        })
        .collect();
    if !cards.is_empty() {
        let _ = app.emit("workspaces-data", json!(cards));
    }
}

/// A promoted candidate: fetch the message bodies (the only place we pay for
/// them) and queue the announcement.
fn promote(app: &AppHandle, state: &Arc<AppState>, sid: &str, snap: &SessSnap, kind: AnnounceKind) {
    // Vox's own agents run as bare `claude --print` and write nothing to
    // conductor.db, so they structurally cannot show up here. This guard is
    // belt-and-braces against a future Conductor SDK bridge — and it logs
    // rather than dropping silently.
    if recently_launched_here(state, &snap.path) {
        println!("[vox] skipping {} — Vox launched an agent there moments ago", snap.project);
        return;
    }

    let answer = crate::conductor::last_assistant_message(sid).unwrap_or_default();
    if answer.trim().is_empty() && kind == AnnounceKind::Finished {
        return;
    }
    let ask = crate::conductor::first_user_message(sid);
    let project = if snap.project.is_empty() { snap.dir.clone() } else { snap.project.clone() };

    announce::push_result(
        state,
        announce::AgentResult {
            project: project.clone(),
            source: "conductor",
            task: None,
            original_ask: ask.clone(),
            answer_full: answer.chars().take(8000).collect(),
            outcome: match kind {
                AnnounceKind::Errored => "failed".into(),
                AnnounceKind::NeedsInput => "waiting".into(),
                _ => "success".into(),
            },
            finished_unix: now_unix(),
        },
    );

    announce::enqueue(
        app,
        state,
        Announcement {
            id: 0,
            kind,
            key: format!("conductor:{sid}"),
            project,
            answer,
            original_ask: ask,
            created_unix: now_unix(),
            attempts: 0,
            not_before: 0,
        },
    );
}

fn recently_launched_here(state: &Arc<AppState>, path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    let now = now_unix();
    let mut seen = state.vox_cwds.lock().unwrap();
    seen.retain(|(_, at)| now.saturating_sub(*at) < VOX_CWD_WINDOW);
    seen.iter().any(|(p, _)| p == path)
}

/// Record that Vox just started an agent in this directory.
pub fn note_vox_launch(state: &Arc<AppState>, path: &str) {
    let mut seen = state.vox_cwds.lock().unwrap();
    seen.push((path.to_string(), now_unix()));
    let now = now_unix();
    seen.retain(|(_, at)| now.saturating_sub(*at) < VOX_CWD_WINDOW);
}

fn emit_cards(app: &AppHandle, rows: &[serde_json::Value]) {
    let mut seen: HashSet<String> = HashSet::new();
    let cards: Vec<serde_json::Value> = rows
        .iter()
        .filter(|r| {
            let p = s(r, "project");
            !p.is_empty() && seen.insert(p)
        })
        .take(10)
        .map(|r| {
            json!({
                "project": s(r, "project"),
                "branch": s(r, "branch"),
                "agent": s(r, "agent"),
                "status": s(r, "status"),
                "unread": r.get("unread").and_then(|v| v.as_i64()).unwrap_or(0),
                // Deliberately empty: filling this would need one extra query
                // per row, and the announcement itself carries the content.
                "preview": "",
            })
        })
        .collect();
    if !cards.is_empty() {
        let _ = app.emit("workspaces-data", json!(cards));
    }
}

fn s(v: &serde_json::Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

/// UTC now in the same ISO shape Conductor writes into `session_messages`.
fn iso_now() -> String {
    iso_at(now_unix())
}

/// Format a unix timestamp the way Conductor writes `session_messages.sent_at`
/// (`2026-08-06T20:42:27.970Z`), so the two can be compared as plain strings.
pub fn iso_at(secs: u64) -> String {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, mi, sec) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Civil-from-days (Howard Hinnant's algorithm) — no chrono dependency.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{sec:02}.000Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The watermark must be string-comparable against `session_messages.sent_at`,
    /// which is uniformly `2026-08-06T20:42:27.970Z`.
    #[test]
    fn iso_now_matches_conductors_format() {
        let t = iso_now();
        assert_eq!(t.len(), 24, "got {t}");
        assert!(t.ends_with('Z') && t.contains('T'), "got {t}");
        let (date, time) = t.split_once('T').unwrap();
        assert_eq!(date.len(), 10);
        assert!(time.starts_with(|c: char| c.is_ascii_digit()));
        // Lexicographic ordering must match chronological ordering.
        assert!(t.as_str() > "2020-01-01T00:00:00.000Z");
        assert!(t.as_str() < "2099-01-01T00:00:00.000Z");
    }

    #[test]
    fn a_new_answer_is_a_string_comparison_on_iso_timestamps() {
        let older = "2026-08-06T19:09:05.011Z";
        let newer = "2026-08-06T20:42:27.970Z";
        assert!(newer > older);
        // Empty means "no assistant message yet" and must never look newer.
        assert!(!(String::new().as_str() > older));
    }
}
