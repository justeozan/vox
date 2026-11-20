//! Conductor backend — worktree state read out of the app's own SQLite file.
//!
//! Kept because Conductor has no API: there is no CLI, no socket and no
//! documented schema, so the only way in is `sqlite3 -readonly` against the
//! app's private database and a set of guesses about what its columns mean.
//! Vox never writes to it.
//!
//! Three workarounds live here and nowhere else, which is the clearest argument
//! for the Orca backend being the default:
//!
//!  * `sessions.unread_count` is 0 on every row Conductor writes, so the "this
//!    agent is done" signal has to be reconstructed from message timestamps;
//!  * `updated_at` is written in TWO formats — ISO from the app, space-separated
//!    from its own SQL triggers — which sort against each other incorrectly;
//!  * an assistant message is a JSON envelope whose shape depends on the agent,
//!    so getting one line of text out means unwrapping three possible layouts.
//!
//! Orca answers all three directly. See `orca.rs`.

use std::process::Command;

use serde_json::Value;

use crate::ade::{AgentState, SessionInfo, Snapshot, TargetCandidate, Workspace};
use crate::home;

/// An idle agent counts as "just finished" only if it said something inside this
/// window. Without it, a worktree whose agent went quiet on Tuesday would be
/// announced as freshly finished at every launch.
const FRESH_MS: u64 = 12 * 3600 * 1000;

fn db_path() -> std::path::PathBuf {
    home().join("Library/Application Support/com.conductor.app/conductor.db")
}

pub fn installed() -> bool {
    db_path().exists()
}

fn sql(query: &str) -> Vec<Value> {
    let db = db_path();
    if !db.exists() {
        return vec![];
    }
    let out = Command::new("sqlite3")
        .args(["-readonly", "-json", &db.to_string_lossy(), query])
        .output();
    match out {
        Ok(o) if o.status.success() => serde_json::from_slice(&o.stdout).unwrap_or_default(),
        _ => vec![],
    }
}

fn s(v: &Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

/// A sortable SQL expression for one of Conductor's `updated_at` columns.
///
/// Conductor writes these in TWO formats: the app writes ISO
/// (`2026-08-06T20:22:54.826Z`) while its own SQL triggers write
/// `datetime('now')` (`2026-08-06 18:27:10`). Sorting the raw column is
/// lexicographic, and 'T' (0x54) > ' ' (0x20), so EVERY trigger-written row
/// sinks below every app-written one regardless of time — off by up to a full
/// day. Normalizing the separator and dropping the sub-second tail makes both
/// formats compare correctly in SQL; `iso_to_millis` then does the same job in
/// Rust, which is what the rest of the crate actually compares.
fn ts_key(col: &str) -> String {
    format!("substr(replace({col},'T',' '),1,19)")
}

/// Either of Conductor's timestamp formats → epoch millis. 0 when unparseable.
///
/// Parsing at the boundary is the real fix for the sort bug above: downstream
/// code compares numbers, so neither format can outrank the other by accident.
fn iso_to_millis(raw: &str) -> u64 {
    let b = raw.as_bytes();
    if b.len() < 19 {
        return 0;
    }
    let num = |a: usize, z: usize| raw.get(a..z).and_then(|t| t.parse::<i64>().ok()).unwrap_or(0);
    let (y, m, d) = (num(0, 4), num(5, 7), num(8, 10));
    let (h, mi, sec) = (num(11, 13), num(14, 16), num(17, 19));
    if y == 0 || m == 0 || d == 0 {
        return 0;
    }
    // days_from_civil — Howard Hinnant's algorithm, the inverse of the formatter
    // this crate used to carry. No chrono dependency.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let frac = if b.len() > 22 && b[19] == b'.' { num(20, 23) } else { 0 };
    ((days * 86_400 + h * 3600 + mi * 60 + sec) * 1000 + frac).max(0) as u64
}

// Assistant messages are JSON envelopes that differ by agent type — extract the
// last human-meaningful text.
fn extract_last_assistant_text(raw: &str) -> Option<String> {
    match serde_json::from_str::<Value>(raw) {
        Ok(obj) => {
            let typ = obj.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if typ == "system" {
                return None;
            }
            if typ == "error" {
                let content = obj.get("content").map(|c| c.to_string()).unwrap_or_default();
                let content: String = content.trim_matches('"').chars().take(200).collect();
                return Some(format!("[erreur] {content}"));
            }
            // Claude SDK format
            if let Some(content) = obj.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array()) {
                let texts: Vec<String> = content
                    .iter()
                    .filter(|c| c.get("type").and_then(|t| t.as_str()) == Some("text"))
                    .filter_map(|c| c.get("text").and_then(|t| t.as_str()))
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect();
                if !texts.is_empty() {
                    return Some(texts.join(" ").chars().take(300).collect());
                }
                return None;
            }
            // ACP/codex flat format
            if let Some(content) = obj.get("content").and_then(|c| c.as_str()) {
                let t = content.trim();
                if !t.is_empty() {
                    return Some(t.chars().take(300).collect());
                }
            }
            None
        }
        Err(_) => {
            let t = raw.trim();
            if t.len() > 4 {
                Some(t.chars().take(300).collect())
            } else {
                None
            }
        }
    }
}

pub fn last_assistant_message(session_id: &str) -> Option<String> {
    last_assistant_entry(session_id).map(|(text, _)| text)
}

/// The newest human-meaningful assistant text AND when it was sent.
///
/// The timestamp is what tells "this agent just finished" apart from "this agent
/// has been idle since Tuesday". Conductor's own `unread_count` flag would be
/// the natural signal, but it is 0 on every row in practice — which is why the
/// recap's "agent is done" line never fired.
fn last_assistant_entry(session_id: &str) -> Option<(String, u64)> {
    let rows = sql(&format!(
        "SELECT content, sent_at FROM session_messages \
         WHERE session_id='{session_id}' AND role='assistant' \
         ORDER BY sent_at DESC LIMIT 40;"
    ));
    rows.iter().find_map(|row| {
        extract_last_assistant_text(&s(row, "content"))
            .map(|t| (t, iso_to_millis(&s(row, "sent_at"))))
    })
}

/// The session's original ask: first meaningful user message.
pub fn first_user_message(session_id: &str) -> Option<String> {
    let rows = sql(&format!(
        "SELECT content FROM session_messages \
         WHERE session_id='{session_id}' AND role='user' \
         ORDER BY sent_at ASC LIMIT 3;"
    ));
    rows.iter().find_map(|row| {
        let raw = s(row, "content");
        let text = match serde_json::from_str::<Value>(&raw) {
            Ok(v) => v
                .get("content")
                .and_then(|c| c.as_str())
                .or_else(|| v.get("text").and_then(|c| c.as_str()))
                .or_else(|| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| raw.clone()),
            Err(_) => raw.clone(),
        };
        let t = crate::ade::clean_snippet(&text);
        if t.chars().count() > 3 {
            Some(t.chars().take(150).collect())
        } else {
            None
        }
    })
}

/// Conductor's status vocabulary → Vox's.
///
/// `idle` is the lossy one: Conductor reports it whether the agent finished a
/// moment ago or last week, so the caller promotes a FRESH idle to `Done` from
/// the message watermark. Orca just says `done`.
fn agent_state(status: &str, fresh_answer: bool) -> AgentState {
    match status {
        "working" => AgentState::Working,
        "error" => AgentState::Errored,
        // Conductor's real status set includes 'waiting' — agent blocked on user
        // input or a permission prompt, the OPPOSITE of quiet.
        "waiting" => AgentState::Blocked,
        "idle" if fresh_answer => AgentState::Done,
        _ => AgentState::Idle,
    }
}

// ── Reads ────────────────────────────────────────────────────────────────────

pub fn read_state(max_items: usize) -> Option<Vec<Workspace>> {
    if !installed() {
        return None;
    }
    let workspaces = sql(&format!(
        "SELECT w.id, w.branch, w.workspace_name, w.workspace_path, r.name AS project_name, \
           (SELECT s.id FROM sessions s \
             WHERE s.workspace_id = w.id AND s.is_hidden = 0 \
             ORDER BY s.updated_at DESC LIMIT 1) AS session_id \
         FROM workspaces w \
         LEFT JOIN repos r ON w.repository_id = r.id \
         WHERE w.state = 'ready' AND w.derived_status = 'in-progress' \
           AND (r.hidden IS NULL OR r.hidden = 0) \
         ORDER BY {TS} DESC LIMIT {max_items};",
        TS = ts_key("w.updated_at")
    ));
    if workspaces.is_empty() {
        // `sqlite3 -readonly` cannot recover a stale WAL, so this is also what
        // "Conductor is closed" looks like. Reporting no answer lets the
        // dispatcher try the other backend instead of claiming an empty world.
        return None;
    }

    Some(
        workspaces
            .iter()
            .map(|w| {
                let session_id = s(w, "session_id");
                let session = if session_id.is_empty() {
                    None
                } else {
                    let entry = last_assistant_entry(&session_id);
                    let fresh = entry
                        .as_ref()
                        .map(|(text, at)| {
                            !text.trim().is_empty()
                                && *at > crate::now_unix().saturating_mul(1000).saturating_sub(FRESH_MS)
                        })
                        .unwrap_or(false);
                    sql(&format!(
                        "SELECT id, status, agent_type, unread_count FROM sessions WHERE id='{session_id}';"
                    ))
                    .first()
                    .map(|row| SessionInfo {
                        agent: s(row, "agent_type"),
                        state: agent_state(&s(row, "status"), fresh),
                        last_activity: entry.as_ref().map(|(_, at)| *at),
                        preview: entry.as_ref().map(|(t, _)| t.clone()),
                        original_ask: first_user_message(&session_id),
                        unread: row.get("unread_count").and_then(|v| v.as_i64()).unwrap_or(0) > 0,
                    })
                };
                Workspace {
                    project: s(w, "project_name"),
                    branch: s(w, "branch"),
                    path: s(w, "workspace_path"),
                    session,
                }
            })
            .collect(),
    )
}

/// Every launchable target, most recently touched first.
///
/// One query (~12ms on a 375MB DB), no name filtering in SQL — matching happens
/// in Rust (see `targets`), which both removes the string-interpolation
/// injection surface and makes fuzzy matching possible.
///
/// Deliberately does NOT filter on `derived_status`: a worktree Conductor marks
/// `done` or `in-review` is still a perfectly good place to send an agent, and
/// excluding those is exactly why most of the user's repos used to be
/// unreachable by voice.
pub fn catalog() -> Option<Vec<TargetCandidate>> {
    if !installed() {
        return None;
    }
    let rows = sql(&format!(
        "SELECT COALESCE(NULLIF(w.workspace_name,''), w.directory_name) AS name, \
                'worktree' AS kind, COALESCE(r.name,'') AS repo, \
                w.workspace_path AS path, COALESCE(w.branch,'') AS branch, \
                {TSW} AS ts, \
                (SELECT COUNT(*) FROM sessions s WHERE s.workspace_id = w.id \
                   AND s.is_hidden = 0 AND s.status = 'working') AS live \
           FROM workspaces w LEFT JOIN repos r ON w.repository_id = r.id \
          WHERE w.state = 'ready' AND w.workspace_path IS NOT NULL \
            AND (r.hidden IS NULL OR r.hidden = 0) \
         UNION ALL \
         SELECT r.name, 'repo', r.name, r.root_path, COALESCE(r.default_branch,''), \
                {TSR}, 0 \
           FROM repos r \
          WHERE (r.hidden IS NULL OR r.hidden = 0) \
            AND r.root_path IS NOT NULL AND r.root_path <> '' \
          ORDER BY ts DESC;",
        TSW = ts_key("w.updated_at"),
        TSR = ts_key("r.updated_at"),
    ));
    if rows.is_empty() {
        return None;
    }

    Some(
        rows.iter()
            .map(|r| TargetCandidate {
                name: s(r, "name"),
                kind: if s(r, "kind") == "repo" { "repo" } else { "worktree" },
                repo: s(r, "repo"),
                path: s(r, "path"),
                branch: s(r, "branch"),
                ts: iso_to_millis(&s(r, "ts")),
                live: r.get("live").and_then(|v| v.as_i64()).unwrap_or(0) > 0,
            })
            // A row whose directory is gone would send an agent nowhere.
            .filter(|c| {
                !c.name.is_empty() && !c.path.is_empty() && std::path::Path::new(&c.path).exists()
            })
            .collect(),
    )
}

/// One watcher tick. Deliberately NOT filtered on `derived_status`: a workspace
/// flips to `done` at the very moment its agent finishes, so filtering to
/// `in-progress` would drop exactly the rows the watcher exists to notice.
/// (Measured on a real DB: 7 of 13 live sessions sit in `done` workspaces.)
pub fn poll(limit: usize) -> Option<Vec<Snapshot>> {
    if !installed() {
        return None;
    }
    let rows = sql(&format!(
        "SELECT s.id AS sid, s.status AS status, s.unread_count AS unread, \
                w.directory_name AS dir, w.workspace_path AS path, \
                COALESCE(w.branch,'') AS branch, \
                s.agent_type AS agent, COALESCE(r.name,'') AS project, \
                (SELECT max(m.sent_at) FROM session_messages m \
                   WHERE m.session_id = s.id AND m.role = 'assistant') AS last_asst \
           FROM sessions s JOIN workspaces w ON s.workspace_id = w.id \
           LEFT JOIN repos r ON w.repository_id = r.id \
          WHERE s.is_hidden = 0 AND w.state = 'ready' \
            AND (r.hidden IS NULL OR r.hidden = 0) \
          ORDER BY {TS} DESC LIMIT {limit};",
        TS = ts_key("s.updated_at")
    ));
    if rows.is_empty() {
        return None;
    }

    Some(
        rows.iter()
            .filter(|r| !s(r, "sid").is_empty())
            .map(|r| {
                let watermark = iso_to_millis(&s(r, "last_asst"));
                let project = match s(r, "project") {
                    p if p.is_empty() => s(r, "dir"),
                    p => p,
                };
                Snapshot {
                    key: s(r, "sid"),
                    // A fresh watermark is what promotes `idle` to `Done`; the
                    // watcher's own "did the watermark advance" test supplies
                    // the recency, so `true` here is not a claim of freshness.
                    state: agent_state(&s(r, "status"), watermark > 0),
                    watermark,
                    project,
                    path: s(r, "path"),
                    branch: s(r, "branch"),
                    agent: s(r, "agent"),
                    // Fetching bodies costs a query per row; the watcher asks
                    // for them only on the one session it decides to announce.
                    answer: None,
                    original_ask: None,
                }
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sort bug this parser exists to kill: Conductor's two formats have to
    /// compare correctly against each other, which they never did as strings.
    #[test]
    fn both_conductor_timestamp_formats_parse_to_comparable_millis() {
        let app = iso_to_millis("2026-08-06T18:27:11.000Z");
        let trigger = iso_to_millis("2026-08-06 18:27:10");
        assert!(app > trigger, "app={app} trigger={trigger}");
        // As raw strings the trigger row wrongly sorts first: ' ' < 'T'.
        assert!("2026-08-06 18:27:10" < "2026-08-06T18:27:11.000Z");
    }

    #[test]
    fn iso_to_millis_round_trips_a_known_instant() {
        // 2026-01-01T00:00:00Z — checked against `date -u -j -f`.
        assert_eq!(iso_to_millis("2026-01-01T00:00:00.000Z"), 1_767_225_600_000);
        assert_eq!(iso_to_millis("2026-01-01 00:00:00"), 1_767_225_600_000);
        // Sub-second precision is kept.
        assert_eq!(iso_to_millis("2026-01-01T00:00:00.826Z") - 1_767_225_600_000, 826);
    }

    #[test]
    fn unparseable_timestamps_are_zero_not_now() {
        // A row with no assistant message yet must never look newer than one
        // that has one, which is what any "default to now" scheme would do.
        assert_eq!(iso_to_millis(""), 0);
        assert_eq!(iso_to_millis("nonsense"), 0);
        assert_eq!(iso_to_millis("0000-00-00 00:00:00"), 0);
        assert!(iso_to_millis("2026-01-01 00:00:00") > iso_to_millis(""));
    }

    #[test]
    fn idle_is_only_done_when_a_message_backs_it() {
        assert_eq!(agent_state("idle", true), AgentState::Done);
        assert_eq!(agent_state("idle", false), AgentState::Idle);
        assert_eq!(agent_state("working", false), AgentState::Working);
        assert_eq!(agent_state("error", false), AgentState::Errored);
        assert_eq!(agent_state("waiting", false), AgentState::Blocked);
        assert_eq!(agent_state("whatever", true), AgentState::Idle);
    }

    #[test]
    fn assistant_envelopes_are_unwrapped_per_agent_format() {
        let claude = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Tests pass."}]}}"#;
        assert_eq!(extract_last_assistant_text(claude).as_deref(), Some("Tests pass."));
        let codex = r#"{"type":"assistant","content":"Done."}"#;
        assert_eq!(extract_last_assistant_text(codex).as_deref(), Some("Done."));
        let err = r#"{"type":"error","content":"boom"}"#;
        assert!(extract_last_assistant_text(err).unwrap().starts_with("[erreur]"));
        // System envelopes carry no human-meaningful text.
        assert_eq!(extract_last_assistant_text(r#"{"type":"system"}"#), None);
    }

    #[test]
    fn ts_key_normalizes_both_formats_in_sql() {
        assert_eq!(ts_key("w.updated_at"), "substr(replace(w.updated_at,'T',' '),1,19)");
    }
}
