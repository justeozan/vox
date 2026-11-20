//! Hermes connection — speaks Hermes Agent's latest replies out loud.
//!
//! Hermes keeps every conversation in `~/.hermes/state.db` (SQLite, WAL). Like
//! `conductor.rs`, this reads it read-only through the system `sqlite3` CLI, so
//! there is no bundled SQLite and nothing Hermes can be blocked by.
//!
//! A *reply* is an `assistant` row with `finish_reason` `stop` (or `length`):
//! that is the end of a turn. Rows ending in `tool_calls` are mid-turn
//! narration and are ignored. Detection keys on the monotonically increasing
//! `messages.id`, so — like the Conductor watermark — it is idempotent and
//! survives a poll straddling several turns.
//!
//! Replies go through the same queue as agent results (`announce`), so they
//! inherit everything for free: waits for silence, ⌥Space parking, the
//! "résumé / mot pour mot" setting, batching, and "il a dit quoi ?" follow-ups.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tauri::AppHandle;

use crate::announce::{self, AnnounceKind, Announcement};
use crate::{home, now_unix, AppState};

/// Name shown in headlines and the result store.
pub const PROJECT: &str = "Hermes";

const TICK: Duration = Duration::from_secs(3);
/// Backed-off interval while Hermes isn't installed / its DB is missing.
const ABSENT_TICK: Duration = Duration::from_secs(30);
/// Let the startup recap cover the current world before reporting on it.
const FIRST_TICK_DELAY: Duration = Duration::from_secs(5);
/// After a long gap (Mac asleep, Vox paused) don't read out a backlog: only the
/// newest few replies are spoken, the rest are skipped.
const MAX_PER_TICK: usize = 3;
/// Sessions that are machinery, not a conversation with the user.
const IGNORED_SOURCES: &str = "'subagent','cron','oneshot','tool','api_server'";

/// `$HERMES_HOME` if set (Hermes' own override), else `~/.hermes`.
fn db_path() -> PathBuf {
    std::env::var_os("HERMES_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".hermes"))
        .join("state.db")
}

pub fn is_installed() -> bool {
    db_path().exists()
}

fn sql(query: &str) -> Option<Vec<Value>> {
    let out = Command::new("sqlite3")
        .args(["-readonly", "-json", &db_path().to_string_lossy(), query])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // sqlite3 prints nothing (not `[]`) for an empty result set.
    if out.stdout.iter().all(|b| b.is_ascii_whitespace()) {
        return Some(vec![]);
    }
    serde_json::from_slice(&out.stdout).ok()
}

fn latest_id() -> Option<i64> {
    sql("SELECT COALESCE(MAX(id),0) AS m FROM messages")?
        .first()?
        .get("m")?
        .as_i64()
}

#[derive(Debug, PartialEq)]
struct Reply {
    id: i64,
    session: String,
    text: String,
    ask: Option<String>,
}

/// Replies newer than `after`, oldest first.
fn replies_after(after: i64) -> Option<Vec<Reply>> {
    let q = format!(
        "SELECT m.id AS id, m.session_id AS session, m.content AS content, \
           (SELECT substr(u.content,1,400) FROM messages u \
              WHERE u.session_id = m.session_id AND u.role = 'user' AND u.id < m.id \
                AND COALESCE(u.display_kind,'') = '' \
              ORDER BY u.id DESC LIMIT 1) AS ask \
         FROM messages m JOIN sessions s ON s.id = m.session_id \
         WHERE m.id > {after} AND m.role = 'assistant' \
           AND m.finish_reason IN ('stop','length') \
           AND COALESCE(m.active,1) = 1 AND COALESCE(m.display_kind,'') = '' \
           AND TRIM(COALESCE(m.content,'')) <> '' \
           AND s.source NOT IN ({IGNORED_SOURCES}) \
         ORDER BY m.id"
    );
    let rows = sql(&q)?;
    Some(
        rows.iter()
            .filter_map(|r| {
                Some(Reply {
                    id: r.get("id")?.as_i64()?,
                    session: r.get("session")?.as_str()?.to_string(),
                    text: r.get("content")?.as_str()?.trim().to_string(),
                    ask: r
                        .get("ask")
                        .and_then(|v| v.as_str())
                        .map(crate::workspace::clean_snippet)
                        .filter(|a| a.chars().count() > 3)
                        .map(|a| a.chars().take(150).collect()),
                })
            })
            .collect(),
    )
}

pub fn start(app: AppHandle, state: Arc<AppState>) {
    std::thread::spawn(move || {
        std::thread::sleep(FIRST_TICK_DELAY);
        // `None` until the first successful read: whatever Hermes already said
        // is history, not news.
        let mut cursor: Option<i64> = None;
        loop {
            std::thread::sleep(tick(&app, &state, &mut cursor));
        }
    });
}

fn tick(app: &AppHandle, state: &Arc<AppState>, cursor: &mut Option<i64>) -> Duration {
    if !is_installed() {
        *cursor = None;
        return ABSENT_TICK;
    }
    let Some(latest) = latest_id() else { return TICK };
    let Some(seen) = *cursor else {
        *cursor = Some(latest);
        println!("[vox] hermes watcher baselined at message {latest}");
        return TICK;
    };
    if latest <= seen {
        return TICK;
    }
    // Always advance, even when disabled: switching the setting on later must
    // not dump everything said in the meantime.
    *cursor = Some(latest);
    if !state.settings.lock().unwrap().hermes_voice {
        return TICK;
    }
    let Some(mut replies) = replies_after(seen) else {
        // Transient read failure (DB busy): retry the same range next tick.
        *cursor = Some(seen);
        return TICK;
    };
    if replies.len() > MAX_PER_TICK {
        replies.drain(..replies.len() - MAX_PER_TICK);
    }
    for r in replies {
        promote(app, state, r);
    }
    TICK
}

fn promote(app: &AppHandle, state: &Arc<AppState>, r: Reply) {
    announce::push_result(
        state,
        announce::AgentResult {
            project: PROJECT.to_string(),
            source: "hermes",
            task: None,
            original_ask: r.ask.clone(),
            answer_full: r.text.chars().take(8000).collect(),
            outcome: "success".into(),
            finished_unix: now_unix(),
        },
    );
    announce::enqueue(
        app,
        state,
        Announcement {
            id: 0,
            kind: AnnounceKind::HermesReply,
            key: format!("hermes:{}:{}", r.session, r.id),
            project: PROJECT.to_string(),
            answer: r.text,
            original_ask: r.ask,
            created_unix: now_unix(),
            attempts: 0,
            not_before: 0,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machinery_sessions_are_ignored_but_conversations_are_not() {
        for s in ["subagent", "cron", "oneshot", "tool", "api_server"] {
            assert!(IGNORED_SOURCES.contains(&format!("'{s}'")), "{s}");
        }
        for s in ["local", "desktop", "telegram", "cli", "gui"] {
            assert!(!IGNORED_SOURCES.contains(&format!("'{s}'")), "{s}");
        }
    }

    /// The real query against a miniature of Hermes' schema: only finished
    /// turns of real conversations come back, with the user's question attached.
    #[test]
    fn only_final_replies_of_conversations_are_returned() {
        let dir = std::env::temp_dir().join(format!("vox-hermes-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("state.db");
        let script = "\
CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT NOT NULL);
CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT, role TEXT, content TEXT,
  finish_reason TEXT, active INTEGER NOT NULL DEFAULT 1, display_kind TEXT);
INSERT INTO sessions VALUES ('chat','local'),('sub','subagent');
INSERT INTO messages (session_id,role,content,finish_reason,display_kind) VALUES
 ('chat','user','Quelle heure est-il ?',NULL,NULL),
 ('chat','assistant','Je regarde.','tool_calls',NULL),
 ('chat','assistant','Il est midi.','stop',NULL),
 ('chat','user','[ASYNC DELEGATION BATCH COMPLETE]',NULL,'async_delegation_complete'),
 ('chat','assistant','Erreur.','stop','failed_turn'),
 ('chat','assistant','   ','stop',NULL),
 ('sub','assistant','Résultat interne','stop',NULL),
 ('chat','assistant','Coupé','length',NULL);";
        let st = Command::new("sqlite3").arg(&db).arg(script).status().unwrap();
        assert!(st.success());
        std::env::set_var("HERMES_HOME", &dir);

        assert_eq!(latest_id(), Some(8));
        let got = replies_after(0).unwrap();
        let texts: Vec<_> = got.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, ["Il est midi.", "Coupé"], "got {got:?}");
        assert_eq!(got[0].ask.as_deref(), Some("Quelle heure est-il ?"));
        assert!(replies_after(8).unwrap().is_empty());

        std::env::remove_var("HERMES_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
