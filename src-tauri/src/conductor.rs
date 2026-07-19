//! Conductor workspace state — port of conductor-state.js. Queries the app's
//! SQLite DB read-only via the system `sqlite3` CLI (no bundled sqlite dep),
//! then builds the deterministic startup-brief sentences.

use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use regex::Regex;
use serde_json::Value;
use tauri::{AppHandle, Emitter};

use crate::{home, AppState};

pub struct SessionInfo {
    pub agent: String,
    pub status: String,
    pub unread_count: i64,
    pub preview: Option<String>,
}

pub struct Workspace {
    pub project: String,
    pub session: Option<SessionInfo>,
}

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

fn s(v: &Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

// Assistant messages are JSON envelopes that differ by agent type — extract
// the last human-meaningful text (see conductor-state.js for the format zoo).
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

fn last_assistant_message(session_id: &str) -> Option<String> {
    let rows = sql(&format!(
        "SELECT content, sent_at FROM session_messages \
         WHERE session_id='{session_id}' AND role='assistant' \
         ORDER BY sent_at DESC LIMIT 40;"
    ));
    rows.iter()
        .find_map(|row| extract_last_assistant_text(&s(row, "content")))
}

pub fn read_state(max_items: usize) -> Vec<Workspace> {
    let workspaces = sql(&format!(
        "SELECT w.id, w.branch, w.workspace_name, r.name AS project_name, \
           (SELECT s.id FROM sessions s \
             WHERE s.workspace_id = w.id AND s.is_hidden = 0 \
             ORDER BY s.updated_at DESC LIMIT 1) AS session_id \
         FROM workspaces w \
         LEFT JOIN repos r ON w.repository_id = r.id \
         WHERE w.state = 'ready' AND w.derived_status = 'in-progress' \
         ORDER BY w.updated_at DESC LIMIT {max_items};"
    ));

    workspaces
        .iter()
        .map(|w| {
            let session_id = s(w, "session_id");
            let session = if session_id.is_empty() {
                None
            } else {
                sql(&format!(
                    "SELECT id, status, agent_type, unread_count FROM sessions WHERE id='{session_id}';"
                ))
                .first()
                .map(|row| SessionInfo {
                    agent: s(row, "agent_type"),
                    status: s(row, "status"),
                    unread_count: row.get("unread_count").and_then(|v| v.as_i64()).unwrap_or(0),
                    preview: last_assistant_message(&session_id),
                })
            };
            Workspace { project: s(w, "project_name"), session }
        })
        .collect()
}

// ── Snippet cleaning (port of cleanActivitySnippet) ──────────────────────────

pub fn clean_snippet(raw: &str) -> String {
    let mut t = raw.to_string();
    for (pat, repl) in [
        (r"(?s)```.*?```", ""),
        (r"`[^`\n]+`", ""),
        (r"\*{1,3}([^*\n]+)\*{1,3}", "$1"),
        (r"_{1,2}([^_\n]+)_{1,2}", "$1"),
        (r"(?m)^#{1,6}\s+", ""),
        (r"[|#]", ""),
        (r"\s+", " "),
    ] {
        t = Regex::new(pat).unwrap().replace_all(&t, repl).to_string();
    }
    let t = t.trim();
    // Keep only the first complete sentence
    if let Some(m) = Regex::new(r"^(.{10,120}?[.!?])").unwrap().captures(t) {
        return m[1].trim().to_string();
    }
    t.chars().take(80).collect::<String>().trim().to_string()
}

// Turn one workspace into a spoken sentence. Deterministic — no LLM involved,
// so it cannot hallucinate a project name or invent a status. Language-aware:
// in English mode we don't quote the (often French) agent preview, so the
// recap stays fully English instead of code-switching mid-sentence.
fn workspace_sentence(w: &Workspace, en: bool) -> Option<String> {
    let s = w.session.as_ref()?;
    let preview = s.preview.as_deref().map(clean_snippet).unwrap_or_default();
    let looks_like_question = preview.ends_with('?')
        || Regex::new(r"(?i)\b(peux|veux|est-ce|c'est quoi|comment|dois-je|can|should|which|what)\b")
            .unwrap()
            .is_match(&preview);

    match s.status.as_str() {
        "working" => Some(if en {
            format!("Agent {} is still working on {}.", s.agent, w.project)
        } else {
            format!("L'agent {} bosse encore sur {}.", s.agent, w.project)
        }),
        "error" => {
            let detail = Regex::new(r"^\[erreur\]\s*")
                .unwrap()
                .replace(&preview, "")
                .to_string();
            if en {
                Some(format!("There's an error on {}.", w.project))
            } else {
                let detail = if detail.is_empty() { "agent en échec".into() } else { detail };
                Some(format!("Il y a une erreur sur {}, {detail}.", w.project))
            }
        }
        "idle" => {
            if looks_like_question && !preview.is_empty() {
                Some(if en {
                    format!("{} is waiting for your answer.", w.project)
                } else {
                    format!("{} attend ta réponse : {preview}", w.project)
                })
            } else if s.unread_count > 0 {
                Some(if en {
                    format!("The agent on {} is done — you'll need to test it.", w.project)
                } else {
                    format!("L'agent sur {} a terminé, il te faudra tester.", w.project)
                })
            } else if !preview.is_empty() {
                Some(if en {
                    format!("{} looks good, up to you whether to keep going.", w.project)
                } else {
                    format!("Tout est bon sur {}, à toi de voir si tu veux enchaîner.", w.project)
                })
            } else {
                None
            }
        }
        _ => None,
    }
}

pub fn speak_startup_brief(app: &AppHandle, state: &Arc<AppState>) {
    if state.startup_brief_done.swap(true, Ordering::SeqCst) {
        return;
    }
    state.interrupt.store(false, Ordering::SeqCst);

    let en = state.settings.lock().unwrap().language == "en";

    let ws = read_state(10);
    if ws.is_empty() {
        return;
    }
    let sentences: Vec<String> = ws.iter().filter_map(|w| workspace_sentence(w, en)).collect();
    if sentences.is_empty() {
        return;
    }

    // Show the violet "awakening" visual NOW, while we prepare the recap —
    // the LLM recommendation below can take a couple of seconds, and the boot
    // state used to only appear right before the first spoken word.
    let _ = app.emit("startup-brief-starting", ());
    if aborted(app, state) {
        return;
    }

    // The per-worktree part is deterministic. Ask the LLM only for a single
    // closing recommendation — small surface, low hallucination risk.
    let model = state.settings.lock().unwrap().model.clone();
    let feed = ws
        .iter()
        .map(|w| match &w.session {
            Some(s) => format!(
                "- {} ({}, {}{})",
                w.project,
                s.agent,
                s.status,
                if s.unread_count > 0 {
                    if en { ", unread" } else { ", non lu" }
                } else {
                    ""
                }
            ),
            None => format!("- {} (?, ?)", w.project),
        })
        .collect::<Vec<_>>()
        .join("\n");

    let (system, user) = if en {
        (
            "You are Vox. Reply in ONE short spoken English sentence. \
             Your only job: recommend WHICH project from the list below the user should focus on now, and why in 5 words max. \
             Never invent a project that isn't in the list. Priority: errors first, then pending questions, then results to test. \
             Example: \"I'd start with findy, there's an opencode error.\"",
            format!("Projects:\n{feed}\n\nRecommendation?"),
        )
    } else {
        (
            "Tu es Vox. Tu réponds en UNE seule phrase courte en français oral. \
             Ton unique job : recommander sur QUEL projet de la liste ci-dessous l'utilisateur devrait se concentrer maintenant, et pourquoi en 5 mots max. \
             Interdiction absolue d'inventer un projet qui n'est pas dans la liste. Priorité aux erreurs, puis aux questions en attente, puis aux résultats à tester. \
             Exemple : \"Je te conseille de commencer par findy, y a une erreur opencode.\"",
            format!("Projets :\n{feed}\n\nRecommandation ?"),
        )
    };

    let recommendation = crate::llm::chat_once(&model, system, &user, 60).unwrap_or_default();
    if aborted(app, state) {
        return;
    }

    let mut parts = vec![if en {
        "Hey, here's the recap.".to_string()
    } else {
        "Bonjour, voici le récap.".to_string()
    }];
    parts.extend(sentences);
    if !recommendation.is_empty() {
        parts.push(recommendation);
    }
    let brief = parts.join(" ");

    // Let the renderer settle on its "AI awakening" intro before the first word.
    std::thread::sleep(Duration::from_millis(900));
    if aborted(app, state) {
        return;
    }

    crate::speech::speak(app, state, &brief);
}

/// True if the user hit ⌥Space to skip the recap. Resets the pill to a resting
/// state so it doesn't stay stuck on the violet boot visual.
fn aborted(app: &AppHandle, state: &Arc<AppState>) -> bool {
    if state.interrupt.load(Ordering::SeqCst) {
        let _ = app.emit("speaking-done", ());
        true
    } else {
        false
    }
}
