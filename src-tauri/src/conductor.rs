//! Conductor workspace state — queries the app's SQLite DB read-only via the
//! system `sqlite3` CLI (no bundled sqlite dep) and maps it onto the shared
//! `Workspace` / `TargetCandidate` shapes of `workspace`.

use std::process::Command;

use serde_json::Value;

use crate::home;
use crate::workspace::{clean_snippet, SessionInfo, TargetCandidate, Workspace};

fn sql(query: &str) -> Vec<Value> {
    let db = home().join("Library/Application Support/com.conductor.app/conductor.db");
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

/// Run a read-only query for another module (the watcher builds its own).
pub fn sql_public(query: &str) -> Vec<Value> {
    sql(query)
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
/// day. That silently corrupts both "most recent worktree wins" tie-breaks and
/// which worktrees the model is even shown. Normalizing the separator and
/// dropping the sub-second tail makes both formats compare correctly.
fn ts_key(col: &str) -> String {
    format!("substr(replace({col},'T',' '),1,19)")
}

// Assistant messages are JSON envelopes that differ by agent type — extract
// the last human-meaningful text.
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
/// The timestamp is what tells "this agent just finished" apart from "this
/// agent has been idle since Tuesday". Conductor's own `unread_count` flag
/// would be the natural signal, but it is 0 on every row in practice — which
/// is why the recap's "agent is done" line never fired.
pub fn last_assistant_entry(session_id: &str) -> Option<(String, String)> {
    let rows = sql(&format!(
        "SELECT content, sent_at FROM session_messages \
         WHERE session_id='{session_id}' AND role='assistant' \
         ORDER BY sent_at DESC LIMIT 40;"
    ));
    rows.iter().find_map(|row| {
        extract_last_assistant_text(&s(row, "content")).map(|t| (t, s(row, "sent_at")))
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
        let t = clean_snippet(&text);
        if t.chars().count() > 3 {
            Some(t.chars().take(150).collect())
        } else {
            None
        }
    })
}

pub fn read_state(max_items: usize) -> Vec<Workspace> {
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

    workspaces
        .iter()
        .map(|w| {
            let session_id = s(w, "session_id");
            let session = if session_id.is_empty() {
                None
            } else {
                let entry = last_assistant_entry(&session_id);
                sql(&format!(
                    "SELECT id, status, agent_type, unread_count FROM sessions WHERE id='{session_id}';"
                ))
                .first()
                .map(|row| SessionInfo {
                    agent: s(row, "agent_type"),
                    status: s(row, "status"),
                    unread_count: row.get("unread_count").and_then(|v| v.as_i64()).unwrap_or(0),
                    last_activity: entry.as_ref().map(|(_, at)| at.clone()),
                    preview: entry.as_ref().map(|(t, _)| t.clone()),
                    original_ask: first_user_message(&session_id),
                })
            };
            Workspace {
                project: s(w, "project_name"),
                branch: s(w, "branch"),
                path: s(w, "workspace_path"),
                session,
            }
        })
        .collect()
}

/// Every launchable target, most recently touched first.
///
/// One query (~12ms on a 375MB DB), no name filtering in SQL — matching happens
/// in Rust (see `targets`), which both removes the string-interpolation
/// injection surface and makes fuzzy matching possible.
///
/// Deliberately does NOT filter on `derived_status`: a worktree that Conductor
/// marks `done` or `in-review` is still a perfectly good place to send an
/// agent, and excluding those is exactly why most of the user's repos used to
/// be unreachable by voice.
pub fn catalog() -> Vec<TargetCandidate> {
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

    rows.iter()
        .map(|r| TargetCandidate {
            name: s(r, "name"),
            kind: if s(r, "kind") == "repo" { "repo" } else { "worktree" },
            repo: s(r, "repo"),
            path: s(r, "path"),
            branch: s(r, "branch"),
            ts: s(r, "ts"),
            live: r.get("live").and_then(|v| v.as_i64()).unwrap_or(0) > 0,
        })
        // A row whose directory is gone would send an agent nowhere.
        .filter(|c| {
            !c.name.is_empty() && !c.path.is_empty() && std::path::Path::new(&c.path).exists()
        })
        .collect()
}