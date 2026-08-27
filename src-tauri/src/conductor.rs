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
    /// ISO timestamp of the last assistant message, for freshness checks.
    pub last_activity: Option<String>,
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

/// One thing an agent can be launched on: a Conductor worktree, or a repo's
/// main checkout. Deliberately wider than `read_state` — that one only surfaces
/// `in-progress` worktrees, which is the right filter for "what's going on"
/// but far too narrow for "where can I send an agent".
#[derive(Clone, Debug)]
pub struct TargetCandidate {
    /// Spoken/typed name: the worktree codename, or the repo name.
    pub name: String,
    /// "worktree" | "repo"
    pub kind: &'static str,
    pub repo: String,
    pub path: String,
    pub branch: String,
    /// Normalized timestamp, safe to compare as a string. See `ts_key`.
    pub ts: String,
    /// A Conductor UI session is `working` in this exact worktree right now.
    pub live: bool,
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

/// An idle agent counts as "just finished" only if it actually said something
/// recently.
///
/// This replaces a check on `unread_count > 0`, which reads correctly but is
/// dead: that column is 0 on every session row Conductor writes, so the recap's
/// "the agent on X is done" line could never fire and finished worktrees were
/// folded into the "nothing new" group instead. Without the freshness window,
/// though, a worktree whose agent went idle on Tuesday would be announced as
/// freshly finished at every launch.
fn finished_recently(sess: &SessionInfo) -> bool {
    const FRESH_SECS: u64 = 12 * 3600;
    let Some(at) = sess.last_activity.as_deref() else { return false };
    if sess.preview.as_deref().map(|p| p.trim().is_empty()).unwrap_or(true) {
        return false;
    }
    // `sent_at` is uniformly ISO-Z, so a prefix comparison against a computed
    // cutoff is both correct and dependency-free.
    let cutoff = crate::watch::iso_at(crate::now_unix().saturating_sub(FRESH_SECS));
    at > cutoff.as_str()
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

/// Names the model may target, repos first (that's what people actually say),
/// then worktree codenames. Deduped case-insensitively and capped — this goes
/// into every system prompt, so it has to stay a single short line.
pub fn launchable_names(max: usize) -> Vec<String> {
    let cat = catalog();
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for want_repo in [true, false] {
        for c in &cat {
            if (c.kind == "repo") != want_repo {
                continue;
            }
            let key = c.name.to_lowercase();
            if seen.insert(key) {
                out.push(c.name.clone());
            }
            if out.len() >= max {
                return out;
            }
        }
    }
    out
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
                } else if finished_recently(sess) {
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

/// Returns true if a recap was actually spoken (false: already done, no
/// workspaces, nothing to say, or aborted before speech).
pub fn speak_startup_brief(app: &AppHandle, state: &Arc<AppState>) -> bool {
    if state.startup_brief_done.swap(true, Ordering::SeqCst) {
        return false;
    }
    // Generation-based cancellation: any ⌥Space/barge-in AFTER this point
    // kills the brief for good — nothing can re-arm it (the old bool flag
    // could be cleared by the next voice turn, resurrecting killed advice).
    let gen0 = state.interrupt_gen.load(Ordering::SeqCst);

    let en = state.settings.lock().unwrap().language == "en";

    let ws = read_state(10);
    if ws.is_empty() {
        return false;
    }
    let seed = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) / 60) as usize;
    let sentences = build_brief_sentences(&ws, en, seed);
    if sentences.is_empty() {
        return false;
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
        return false;
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
    true
}
