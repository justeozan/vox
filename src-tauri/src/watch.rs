//! ADE watcher — notices when an agent you started *inside Orca or Conductor*
//! finishes, so Vox can tell you without being asked.
//!
//! Detection keys on a per-session WATERMARK that advances exactly once per
//! assistant turn, not on a `working → idle` status edge. Two reasons, both
//! load-bearing:
//!
//!  * a poll tick can straddle an entire turn (`working → idle → working`), so a
//!    status edge is simply missed;
//!  * Conductor's own "there's something new here" flag (`unread_count`) is 0 on
//!    every row in practice, so anything built on it never fires.
//!
//! A watermark makes the detector naturally idempotent and self-debouncing: the
//! same answer cannot be announced twice, because the second look sees the same
//! number.
//!
//! On Orca the poll is also PUSHED. Orca rewrites
//! `agent-hooks/last-status.json` whenever an agent pane changes state, so its
//! mtime is a real edge — the watcher wakes on it instead of discovering the
//! same thing up to a full tick later. Only the mtime is read; the data still
//! comes from `orca worktree ps`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tauri::{AppHandle, Emitter};

use crate::ade::{self, AgentState, Snapshot};
use crate::announce::{self, AnnounceKind, Announcement};
use crate::{now_unix, AppState};

/// Base poll interval. One `worktree ps` costs ~90ms, so this is ~2% duty.
const TICK: Duration = Duration::from_secs(4);
/// Backed-off interval after a long quiet stretch.
const SLOW_TICK: Duration = Duration::from_secs(15);
/// Unchanged ticks before backing off.
const QUIET_TICKS: u32 = 20;
/// Granularity of the sleep, and how often Orca's push file is checked. Short
/// enough that a pushed edge feels immediate, long enough to be free.
const SLICE: Duration = Duration::from_millis(500);
/// Give the startup recap time to cover the current world before we start
/// reporting on it.
const FIRST_TICK_DELAY: Duration = Duration::from_secs(5);
/// How long a Vox headless launch suppresses ADE announcements for the same
/// directory. A headless agent writes nothing the ADE can see, so this only
/// guards against double-reporting if it ever did.
const VOX_CWD_WINDOW: u64 = 120;
/// Carousel refresh cadence.
const CARDS_EVERY: Duration = Duration::from_secs(30);
/// Sessions to look at per tick.
const POLL_LIMIT: usize = 60;

pub fn start(app: AppHandle, state: Arc<AppState>) {
    std::thread::spawn(move || {
        std::thread::sleep(FIRST_TICK_DELAY);

        let mut prev: HashMap<String, Snapshot> = HashMap::new();
        // Candidates seen once, promoted only if they're still settled next
        // tick — a session can touch a finished state for a moment between tool
        // calls.
        let mut pending: HashMap<String, Snapshot> = HashMap::new();
        let mut baselined = false;
        let mut boot_watermark = 0u64;
        let mut quiet = 0u32;
        let mut last_cards = Instant::now() - CARDS_EVERY;
        let mut next_poll = Instant::now();
        let mut hooks_seen = crate::orca::hooks_mtime();

        loop {
            // Wake early when Orca says something changed.
            let pushed = {
                let now = crate::orca::hooks_mtime();
                let moved = now.is_some() && now != hooks_seen;
                if moved {
                    hooks_seen = now;
                }
                moved
            };
            if !pushed && Instant::now() < next_poll {
                std::thread::sleep(SLICE);
                continue;
            }

            let snaps = ade::poll(POLL_LIMIT);
            if snaps.is_empty() {
                // No backend could answer: Orca closed, or `sqlite3 -readonly`
                // unable to recover a stale WAL. Failing quiet is right.
                next_poll = Instant::now() + SLOW_TICK;
                continue;
            }

            let mut changed = false;
            for now in snaps.iter() {
                let Some(was) = prev.get(&now.key).cloned() else {
                    // Never announce a session we're seeing for the first time.
                    // Covers cold start, an ADE restart, an un-hidden repo, and
                    // rows drifting in and out of the poll window.
                    prev.insert(now.key.clone(), now.clone());
                    continue;
                };
                if *now != was {
                    changed = true;
                }
                if !baselined {
                    prev.insert(now.key.clone(), now.clone());
                    continue;
                }

                let advanced = now.watermark > was.watermark;
                let kind = match now.state {
                    // The watermark test is what stops a session that simply
                    // STAYS done from being announced on every single tick.
                    AgentState::Done if advanced => Some(AnnounceKind::Finished),
                    AgentState::Errored if was.state != AgentState::Errored => {
                        Some(AnnounceKind::Errored)
                    }
                    AgentState::Blocked if was.state != AgentState::Blocked => {
                        Some(AnnounceKind::NeedsInput)
                    }
                    _ => None,
                };

                // Anything from before we started watching is history, not news.
                let stale = now.watermark > 0 && now.watermark < boot_watermark;

                match kind {
                    Some(k) if !stale => {
                        // Settle: promote only once the candidate is unchanged
                        // for a second tick. Back to `working` drops it, and the
                        // real finish re-arms the edge later.
                        match pending.get(&now.key) {
                            Some(p)
                                if p.watermark == now.watermark
                                    && now.state != AgentState::Working =>
                            {
                                pending.remove(&now.key);
                                promote(&app, &state, now, k);
                            }
                            _ => {
                                pending.insert(now.key.clone(), now.clone());
                            }
                        }
                    }
                    _ => {
                        pending.remove(&now.key);
                    }
                }
                // Unconditional, including for suppressed cases — an edge must
                // never be able to re-fire.
                prev.insert(now.key.clone(), now.clone());
            }

            if !baselined {
                baselined = true;
                boot_watermark = now_unix().saturating_mul(1000);
                println!(
                    "[vox] {} watcher baselined on {} sessions",
                    ade::label(),
                    prev.len()
                );
            }

            // The carousel is only ever populated by `workspaces-data`, which
            // until now only the startup brief emitted — so an announcement had
            // no card to highlight. Feed it from the snapshots we already have,
            // never from a second read.
            if last_cards.elapsed() >= CARDS_EVERY {
                emit_cards(&app, &snaps);
                last_cards = Instant::now();
            }

            quiet = if changed { 0 } else { quiet.saturating_add(1) };
            next_poll = Instant::now() + if quiet >= QUIET_TICKS { SLOW_TICK } else { TICK };
        }
    });
}

/// A promoted candidate: queue the announcement.
///
/// When Vox launched this work itself in NATIVE mode, the agent is a real ADE
/// agent and shows up here like any other — so the job is claimed and reported
/// with Vox's own framing (it knows the task it asked for) instead of being
/// announced twice or anonymously.
fn promote(app: &AppHandle, state: &Arc<AppState>, snap: &Snapshot, kind: AnnounceKind) {
    if let Some(job) = crate::agents::claim_native(state, &snap.path) {
        crate::agents::finish_native(app, state, job, snap, kind);
        return;
    }
    // A headless Vox agent writes nothing an ADE can see, so it structurally
    // cannot show up here. Belt-and-braces, and it logs rather than dropping
    // silently.
    if recently_launched_here(state, &snap.path) {
        println!("[vox] skipping {} — Vox launched an agent there moments ago", snap.project);
        return;
    }

    let answer = ade::answer_for(snap).unwrap_or_default();
    if answer.trim().is_empty() && kind == AnnounceKind::Finished {
        return;
    }
    let ask = ade::ask_for(snap);
    let project = snap.project.clone();

    announce::push_result(
        state,
        announce::AgentResult {
            project: project.clone(),
            source: ade::source_slug(),
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
            key: format!("{}:{}", ade::source_slug(), snap.key),
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

/// Record that Vox just started a headless agent in this directory.
pub fn note_vox_launch(state: &Arc<AppState>, path: &str) {
    let mut seen = state.vox_cwds.lock().unwrap();
    seen.push((path.to_string(), now_unix()));
    let now = now_unix();
    seen.retain(|(_, at)| now.saturating_sub(*at) < VOX_CWD_WINDOW);
}

fn emit_cards(app: &AppHandle, snaps: &[Snapshot]) {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let cards: Vec<serde_json::Value> = snaps
        .iter()
        .filter(|s| !s.project.is_empty() && seen.insert(s.project.clone()))
        .take(10)
        .map(ade::snapshot_card)
        .collect();
    if !cards.is_empty() {
        let _ = app.emit("workspaces-data", json!(cards));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(key: &str, state: AgentState, watermark: u64) -> Snapshot {
        Snapshot {
            key: key.into(),
            state,
            watermark,
            project: "findy".into(),
            path: "/tmp/findy".into(),
            branch: "main".into(),
            agent: "claude".into(),
            answer: Some("All green.".into()),
            original_ask: Some("fix the tests".into()),
        }
    }

    /// The rule the whole module exists for, extracted so it can be tested
    /// without a live ADE: an edge fires once and only once.
    fn edge(was: &Snapshot, now: &Snapshot) -> Option<AnnounceKind> {
        let advanced = now.watermark > was.watermark;
        match now.state {
            AgentState::Done if advanced => Some(AnnounceKind::Finished),
            AgentState::Errored if was.state != AgentState::Errored => Some(AnnounceKind::Errored),
            AgentState::Blocked if was.state != AgentState::Blocked => {
                Some(AnnounceKind::NeedsInput)
            }
            _ => None,
        }
    }

    #[test]
    fn a_finished_turn_fires_once() {
        let working = snap("s", AgentState::Working, 100);
        let done = snap("s", AgentState::Done, 200);
        assert_eq!(edge(&working, &done), Some(AnnounceKind::Finished));
        // Still done on the next tick, same watermark: silence.
        assert_eq!(edge(&done, &done), None);
    }

    /// The bug a status-edge detector has and a watermark detector does not:
    /// a tick that straddles `working → done → working` still reports the turn
    /// on the following tick, because the number moved.
    #[test]
    fn a_second_turn_in_the_same_session_fires_again() {
        let first = snap("s", AgentState::Done, 200);
        let second = snap("s", AgentState::Done, 300);
        assert_eq!(edge(&first, &second), Some(AnnounceKind::Finished));
    }

    #[test]
    fn errors_and_blocks_fire_on_the_state_edge_not_the_watermark() {
        let working = snap("s", AgentState::Working, 100);
        let errored = snap("s", AgentState::Errored, 100);
        assert_eq!(edge(&working, &errored), Some(AnnounceKind::Errored));
        // Already reported — don't nag every tick.
        assert_eq!(edge(&errored, &errored), None);

        let blocked = snap("s", AgentState::Blocked, 100);
        assert_eq!(edge(&working, &blocked), Some(AnnounceKind::NeedsInput));
        assert_eq!(edge(&blocked, &blocked), None);
    }

    #[test]
    fn an_idle_session_is_never_news() {
        let idle = snap("s", AgentState::Idle, 500);
        assert_eq!(edge(&snap("s", AgentState::Working, 100), &idle), None);
    }

    /// A done state with no new answer behind it must stay quiet — this is what
    /// keeps a cold start from announcing every worktree that finished days ago.
    #[test]
    fn done_without_an_advancing_watermark_is_quiet() {
        let a = snap("s", AgentState::Done, 200);
        let b = snap("s", AgentState::Done, 150);
        assert_eq!(edge(&a, &b), None);
    }
}
