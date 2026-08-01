//! Conductor workspace state — queries the app's SQLite DB read-only via the
//! system `sqlite3` CLI (no bundled sqlite dep), then builds the startup-brief
//! sentences: deterministic per-worktree lines (varied phrasing, quiet repos
//! grouped) streamed into a speech session, followed by an LLM recommendation
//! grounded in each worktree's ORIGINAL ask.

use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

use crate::speech::SpeechItem;
use crate::{home, AppState};

pub struct SessionInfo {
    pub agent: String,
    pub status: String,
    pub unread_count: i64,
    pub preview: Option<String>,
    /// The user's first message in the session — what this worktree was
    /// originally asked to do. Grounds progress advice.
    pub original_ask: Option<String>,
}

pub struct Workspace {
    pub project: String,
    pub branch: String,
    pub path: String,
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

fn last_assistant_message(session_id: &str) -> Option<String> {
    let rows = sql(&format!(
        "SELECT content, sent_at FROM session_messages \
         WHERE session_id='{session_id}' AND role='assistant' \
         ORDER BY sent_at DESC LIMIT 40;"
    ));
    rows.iter()
        .find_map(|row| extract_last_assistant_text(&s(row, "content")))
}

/// The session's original ask: first meaningful user message.
fn first_user_message(session_id: &str) -> Option<String> {
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

/// Resolve a spoken worktree/project name to its filesystem path. Matches the
/// repo name, directory name, workspace name, or branch, most recent first.
pub fn find_workspace_path(name: &str) -> Option<String> {
    let needle = name.trim().to_lowercase().replace('\'', "''");
    if needle.is_empty() || needle.chars().count() > 80 {
        return None;
    }
    // Exact name matches MUST outrank the fuzzy branch match — otherwise a
    // substring hit on another repo's branch could beat the exact repo name
    // and send a prompt (and an agent) to the wrong worktree.
    let rows = sql(&format!(
        "SELECT w.workspace_path, \
           (lower(r.name) = '{needle}' \
            OR lower(w.directory_name) = '{needle}' \
            OR lower(w.workspace_name) = '{needle}') AS exact_match \
         FROM workspaces w \
         LEFT JOIN repos r ON w.repository_id = r.id \
         WHERE w.state = 'ready' AND w.workspace_path IS NOT NULL AND ( \
            lower(r.name) = '{needle}' \
            OR lower(w.directory_name) = '{needle}' \
            OR lower(w.workspace_name) = '{needle}' \
            OR lower(w.branch) LIKE '%{needle}%') \
         ORDER BY exact_match DESC, w.updated_at DESC LIMIT 1;"
    ));
    rows.first()
        .map(|r| s(r, "workspace_path"))
        .filter(|p| !p.is_empty() && std::path::Path::new(p).exists())
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

// ── Brief sentences — varied phrasing, quiet repos grouped ───────────────────

fn join_names(names: &[String], en: bool) -> String {
    let and = if en { "and" } else { "et" };
    match names.len() {
        0 => String::new(),
        1 => names[0].clone(),
        2 => format!("{} {and} {}", names[0], names[1]),
        _ if names.len() <= 4 => {
            let (head, last) = names.split_at(names.len() - 1);
            format!("{} {and} {}", head.join(", "), last[0])
        }
        n => {
            let others = if en { "others" } else { "autres" };
            format!("{}, {} {and} {} {others}", names[0], names[1], n - 2)
        }
    }
}

/// Build the deterministic recap sentences, each tagged with the workspace it
/// talks about. `seed` rotates the phrasing between runs so consecutive
/// recaps don't sound copy-pasted; quiet worktrees collapse into ONE sentence.
fn build_brief_sentences(ws: &[Workspace], en: bool, seed: usize) -> Vec<(String, Option<String>)> {
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    let mut quiet: Vec<String> = Vec::new();

    for (i, w) in ws.iter().enumerate() {
        let Some(sess) = w.session.as_ref() else { continue };
        let p = &w.project;
        let a = &sess.agent;
        let raw_preview = sess.preview.as_deref().unwrap_or("");
        let preview = clean_snippet(raw_preview);
        // Detect questions on the RAW preview: clean_snippet keeps only the
        // first sentence, which drops the trailing "…anything else?" that the
        // "summary. Question?" agent pattern ends with.
        let looks_like_question = raw_preview.trim_end().ends_with('?')
            || Regex::new(r"(?i)\b(peux|veux|est-ce|c'est quoi|comment|dois-je|can|should|which|what)\b")
                .unwrap()
                .is_match(&preview);
        let v = seed + i;

        let sentence = match sess.status.as_str() {
            "working" => Some(match v % 3 {
                0 if en => format!("Agent {a} is still working on {p}."),
                1 if en => format!("{p}: agent {a} is on it right now."),
                _ if en => format!("Work is moving on {p} with agent {a}."),
                0 => format!("L'agent {a} bosse encore sur {p}."),
                1 => format!("{p} : l'agent {a} est toujours au travail."),
                _ => format!("Ça avance sur {p}, l'agent {a} est dessus."),
            }),
            "error" => {
                let detail = Regex::new(r"^\[erreur\]\s*")
                    .unwrap()
                    .replace(&preview, "")
                    .to_string();
                Some(match v % 3 {
                    0 if en => format!("There's an error on {p}."),
                    1 if en => format!("Heads up — {p} errored out."),
                    _ if en => format!("{p} hit a problem."),
                    0 => {
                        let d = if detail.is_empty() { "agent en échec".into() } else { detail };
                        format!("Il y a une erreur sur {p}, {d}.")
                    }
                    1 => format!("Attention, {p} est en erreur."),
                    _ => format!("{p} a un souci, jette un œil."),
                })
            }
            "idle" => {
                if looks_like_question && !preview.is_empty() {
                    Some(match v % 3 {
                        0 if en => format!("{p} is waiting for your answer."),
                        1 if en => format!("The agent on {p} asked you something."),
                        _ if en => format!("There's a pending question on {p}."),
                        0 => format!("{p} attend ta réponse : {preview}"),
                        1 => format!("L'agent de {p} te pose une question : {preview}"),
                        _ => format!("Question en attente sur {p} : {preview}"),
                    })
                } else if sess.unread_count > 0 {
                    Some(match v % 3 {
                        0 if en => format!("The agent on {p} is done — you'll need to test it."),
                        1 if en => format!("{p} is ready for you to try."),
                        _ if en => format!("{p} finished; give it a quick test."),
                        0 => format!("L'agent sur {p} a terminé, il te faudra tester."),
                        1 => format!("{p} est prêt, à tester quand tu veux."),
                        _ => format!("C'est terminé sur {p}, un petit test s'impose."),
                    })
                } else {
                    // Nothing actionable — collapse into the grouped sentence.
                    quiet.push(p.clone());
                    None
                }
            }
            // Conductor's real status set includes 'waiting' (agent blocked
            // on user input/permission) — the OPPOSITE of quiet.
            "waiting" => Some(match v % 3 {
                0 if en => format!("{p} is waiting on you to continue."),
                1 if en => format!("The agent on {p} is blocked, waiting for your input."),
                _ if en => format!("{p} needs you before it can move on."),
                0 => format!("{p} attend ton feu vert pour continuer."),
                1 => format!("L'agent sur {p} est bloqué, il attend ta réponse."),
                _ => format!("{p} a besoin de toi pour avancer."),
            }),
            // Unknown status: say nothing rather than wrongly claim all-quiet.
            _ => None,
        };
        if let Some(text) = sentence {
            out.push((text, Some(p.clone())));
        }
    }

    if !quiet.is_empty() {
        let list = join_names(&quiet, en);
        let text = match seed % 3 {
            0 if en => format!("Nothing new on {list}."),
            1 if en => format!("All quiet on {list}."),
            _ if en => format!("{list}: nothing to report."),
            0 => format!("Rien de nouveau sur {list}."),
            1 => format!("Toujours calme côté {list}."),
            _ => format!("{list} : rien à signaler."),
        };
        // Tag with the first quiet project so the carousel still tracks.
        out.push((text, quiet.first().cloned()));
    }

    out
}

// ── Startup brief ────────────────────────────────────────────────────────────

pub fn speak_startup_brief(app: &AppHandle, state: &Arc<AppState>) {
    if state.startup_brief_done.swap(true, Ordering::SeqCst) {
        return;
    }
    // Generation-based cancellation: any ⌥Space/barge-in AFTER this point
    // kills the brief for good — nothing can re-arm it (the old bool flag
    // could be cleared by the next voice turn, resurrecting killed advice).
    let gen0 = state.interrupt_gen.load(Ordering::SeqCst);

    let en = state.settings.lock().unwrap().language == "en";

    let ws = read_state(10);
    if ws.is_empty() {
        return;
    }
    let seed = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) / 60) as usize;
    let sentences = build_brief_sentences(&ws, en, seed);
    if sentences.is_empty() {
        return;
    }

    // Carousel cards for the renderer — sent before anything is spoken.
    let cards: Vec<Value> = ws
        .iter()
        .map(|w| {
            json!({
                "project": w.project,
                "branch": w.branch,
                "agent": w.session.as_ref().map(|s| s.agent.clone()).unwrap_or_default(),
                "status": w.session.as_ref().map(|s| s.status.clone()).unwrap_or_else(|| "none".into()),
                "unread": w.session.as_ref().map(|s| s.unread_count).unwrap_or(0),
                "preview": w.session.as_ref().and_then(|s| s.preview.as_deref()).map(clean_snippet).unwrap_or_default(),
            })
        })
        .collect();
    let _ = app.emit("workspaces-data", json!(cards));

    // Violet "awakening" visual immediately — speech streams in behind it.
    let _ = app.emit("startup-brief-starting", ());

    let interrupted = || state.interrupt_gen.load(Ordering::SeqCst) != gen0;
    if interrupted() {
        let _ = app.emit("speaking-done", ());
        return;
    }

    // Everything below streams into one speech session: the deterministic
    // sentences start playing right away while the LLM writes its advice.
    let session = crate::speech::start_session(app.clone(), state.clone());

    let intro = match seed % 3 {
        0 if en => "Hey, here's the recap.",
        1 if en => "Quick status round-up.",
        _ if en => "Here's where things stand.",
        0 => "Bonjour, voici le récap.",
        1 => "Salut ! Petit point sur tes worktrees.",
        _ => "C'est parti pour le récap.",
    };
    let _ = session.send(SpeechItem::Sentence { text: intro.into(), workspace: None });

    for (text, workspace) in sentences {
        if interrupted() {
            break;
        }
        let _ = session.send(SpeechItem::Sentence { text, workspace });
    }

    // LLM advice grounded in each worktree's original ask — streamed sentence
    // by sentence into the same session while earlier lines are being spoken.
    if !interrupted() {
        let model = state.settings.lock().unwrap().model.clone();
        let feed = ws
            .iter()
            .filter_map(|w| {
                let sess = w.session.as_ref()?;
                let ask = sess.original_ask.as_deref().unwrap_or("?");
                let last = sess.preview.as_deref().map(clean_snippet).unwrap_or_default();
                let unread = if sess.unread_count > 0 {
                    if en { ", unread" } else { ", non lu" }
                } else {
                    ""
                };
                Some(format!(
                    "- {} ({}{unread}) | {} \"{ask}\" | {} \"{last}\"",
                    w.project,
                    sess.status,
                    if en { "asked:" } else { "demande :" },
                    if en { "last:" } else { "dernier :" },
                ))
            })
            .collect::<Vec<_>>()
            .join("\n");

        let system = if en {
            "You are Vox, spoken English. From the data: 1) Recommend WHICH worktree to handle first and why, one short sentence. \
             2) If a finished worktree seems to satisfy its original ask, propose the next step in one sentence starting with \"I can prompt <project> to …\". \
             Max 3 short sentences total. Never invent a project not in the list."
        } else {
            "Tu es Vox, français oral. À partir des données : 1) Recommande LE worktree à traiter en premier et pourquoi, une phrase courte. \
             2) Si un worktree terminé semble répondre à sa demande initiale, propose la suite en une phrase commençant par « Je peux prompter <projet> pour … ». \
             Maximum 3 phrases courtes au total. N'invente aucun projet hors liste."
        };
        let user = if en {
            format!("Worktrees:\n{feed}\n\nYour recommendation?")
        } else {
            format!("Worktrees :\n{feed}\n\nTa recommandation ?")
        };

        // The cancel callback aborts the SSE read loop itself, so an
        // interrupted brief releases the speech session (and speak_lock)
        // promptly instead of holding them for the full generation.
        let _ = crate::llm::chat_once_stream(&model, system, &user, 120, &interrupted, |sentence| {
            if !interrupted() {
                let _ = session.send(SpeechItem::Sentence { text: sentence, workspace: None });
            }
        });
    }

    let _ = session.send(SpeechItem::End);
}
