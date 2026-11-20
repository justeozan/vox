//! Workspace/agent knowledge, independent of where it comes from.
//!
//! The data is read from Orca's CLI (default) or Conductor's SQLite DB — the
//! `backend` setting — and mapped onto the same `Workspace` / `SessionInfo` /
//! `TargetCandidate` structs, so grounding, target resolution and the startup
//! recap never care which one is behind them. This module owns those structs,
//! the backend dispatch, and the recap sentences built from them.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

use crate::speech::SpeechItem;
use crate::AppState;

// ── Backend ──────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Orca,
    Conductor,
}

impl Backend {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "orca" => Some(Backend::Orca),
            "conductor" => Some(Backend::Conductor),
            _ => None,
        }
    }

    /// Product name, for the system prompt and spoken lines.
    pub fn label(self) -> &'static str {
        match self {
            Backend::Orca => "Orca",
            Backend::Conductor => "Conductor",
        }
    }
}

/// The active backend, read straight from settings (see `persisted_backend`):
/// callers here hold no AppState, and re-reading means a change in the settings
/// panel applies on the very next turn without a restart.
pub fn backend() -> Backend {
    Backend::parse(&crate::persisted_backend()).unwrap_or(Backend::Orca)
}

/// Active workspaces with their latest agent session, most recent first.
pub fn read_state(max_items: usize) -> Vec<Workspace> {
    match backend() {
        Backend::Orca => crate::orca::read_state(max_items),
        Backend::Conductor => crate::conductor::read_state(max_items),
    }
}

/// Every launchable target, most recently touched first.
pub fn catalog() -> Vec<TargetCandidate> {
    match backend() {
        Backend::Orca => crate::orca::catalog(),
        Backend::Conductor => crate::conductor::catalog(),
    }
}

// ── Shared shapes ────────────────────────────────────────────────────────────

pub struct SessionInfo {
    pub agent: String,
    pub status: String,
    pub unread_count: i64,
    /// ISO timestamp of the last assistant message (Orca: when the agent last
    /// settled), for freshness checks.
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

/// One thing an agent can be launched on: a worktree, or a repo's
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
    /// Normalized `YYYY-MM-DD HH:MM:SS` timestamp, safe to compare as a string
    /// (see `conductor::ts_key`, `orca::ts_from_ms`).
    pub ts: String,
    /// A UI session (Orca/Conductor) is `working` in this exact worktree right now.
    pub live: bool,
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
    // Both backends hand over ISO-Z (Conductor's `sent_at`; Orca's ms mapped
    // through `iso_at`), so a string comparison against a computed cutoff is
    // both correct and dependency-free.
    let cutoff = crate::watch::iso_at(crate::now_unix().saturating_sub(FRESH_SECS));
    at > cutoff.as_str()
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
pub(crate) fn build_brief_sentences(ws: &[Workspace], en: bool, seed: usize) -> Vec<(String, Option<String>)> {
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    let mut quiet: Vec<String> = Vec::new();
    let question_words =
        Regex::new(r"(?i)\b(peux|veux|est-ce|c'est quoi|comment|dois-je|can|should|which|what)\b").unwrap();
    let error_tag = Regex::new(r"^\[erreur\]\s*").unwrap();

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
            || question_words.is_match(&preview);
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
                let detail = error_tag.replace(&preview, "").to_string();
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
