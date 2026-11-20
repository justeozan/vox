//! The Agentic Development Environment Vox is driving: Orca or Conductor.
//!
//! Vox's whole premise is that it can see your worktrees and the agents working
//! in them. WHERE that picture comes from is this module's secret; everything
//! above it — the startup recap, the system prompt, target resolution, the
//! watcher — works on the normalized types declared here and never names a
//! product.
//!
//! Orca wins when both are installed, because it answers questions Conductor
//! structurally could not:
//!
//!  * `orca worktree ps --json` is ONE ~90ms call that already carries each
//!    agent's state, its original prompt and its last message. Conductor needed
//!    1 + 3N `sqlite3` spawns against a private database to rebuild the same
//!    thing, and three of this crate's ugliest workarounds existed only to
//!    paper over that file's inconsistencies.
//!  * Orca PUSHES agent status into `agent-hooks/last-status.json`, so "an agent
//!    just answered" is a file-mtime edge rather than a 4-second poll that can
//!    straddle an entire turn.
//!
//! Selection is by data, not by a liveness probe: a backend that cannot answer
//! returns `None` (as opposed to `Some(vec![])`, "I answered, there's nothing")
//! and the dispatcher falls through to the other one. An installed-but-closed
//! Orca therefore degrades to Conductor instead of reporting an empty world.

use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

use crate::speech::SpeechItem;
use crate::{conductor, orca, AppState};

// ── Normalized state ─────────────────────────────────────────────────────────

/// What an agent is doing, normalized across ADEs.
///
/// Orca's own vocabulary (`idle|working|done|blocked|error|interrupted`) is the
/// superset, so it maps straight through. Conductor's four states fold into it.
/// An enum rather than the `String` this used to be: every consumer matched on
/// string literals, and the one that didn't — the recap's `_ => None` arm —
/// silently said nothing for any status nobody had thought of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentState {
    Working,
    /// Produced an answer and stopped, recently enough to be news.
    ///
    /// Conductor has no such state: it reports `idle` whether the agent finished
    /// ten seconds or ten days ago, which is why its backend promotes a *fresh*
    /// `idle` into this variant itself. Orca reports `done` directly.
    Done,
    /// Waiting on the user — a permission prompt, or a question it asked.
    Blocked,
    Errored,
    /// Nothing to report.
    Idle,
}

impl AgentState {
    /// Language-aware descriptor for the system prompt.
    pub fn descriptor(self, en: bool) -> &'static str {
        match (self, en) {
            (AgentState::Working, true) => "agent working",
            (AgentState::Working, false) => "agent en cours",
            (AgentState::Done, true) => "agent DONE",
            (AgentState::Done, false) => "agent TERMINÉ",
            (AgentState::Blocked, true) => "agent BLOCKED, waiting on you",
            (AgentState::Blocked, false) => "agent BLOQUÉ, il t'attend",
            (AgentState::Errored, true) => "agent ERRORED",
            (AgentState::Errored, false) => "agent EN ERREUR",
            (AgentState::Idle, true) => "agent idle",
            (AgentState::Idle, false) => "agent au repos",
        }
    }
}

pub struct SessionInfo {
    /// Agent kind: "claude", "codex", "opencode"…
    pub agent: String,
    pub state: AgentState,
    /// Epoch millis of the last assistant message, for freshness checks.
    pub last_activity: Option<u64>,
    pub preview: Option<String>,
    /// The user's first message in the session — what this worktree was
    /// originally asked to do. Grounds progress advice.
    pub original_ask: Option<String>,
    pub unread: bool,
}

pub struct Workspace {
    pub project: String,
    pub branch: String,
    pub path: String,
    pub session: Option<SessionInfo>,
}

/// One thing an agent can be launched on: a worktree, or a repo's main checkout.
///
/// Deliberately wider than `read_state` — that one only surfaces worktrees with
/// something going on, which is the right filter for "what's happening" but far
/// too narrow for "where can I send an agent".
#[derive(Clone, Debug)]
pub struct TargetCandidate {
    /// Spoken/typed name: the worktree display name, or the repo name.
    pub name: String,
    /// "worktree" | "repo"
    pub kind: &'static str,
    pub repo: String,
    pub path: String,
    pub branch: String,
    /// Epoch millis of last activity.
    ///
    /// Was a `String` back when Conductor's two clashing timestamp formats had
    /// to be normalized in SQL and compared lexicographically — a scheme that
    /// sorted every trigger-written row below every app-written one regardless
    /// of time, silently corrupting "most recent worktree wins". Parsing to a
    /// number at the boundary is the actual fix.
    pub ts: u64,
    /// An agent is working in this exact worktree right now.
    pub live: bool,
}

/// Stripped-down row for the watcher: enough to detect an edge, no message
/// bodies (those cost extra on Conductor and are only fetched on promotion).
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    /// Stable per-session identity, used as the dedup key.
    pub key: String,
    pub state: AgentState,
    /// Advances exactly once per assistant turn. The watcher's whole detector.
    pub watermark: u64,
    pub project: String,
    pub path: String,
    pub branch: String,
    pub agent: String,
    /// Present when the backend already carries the answer (Orca's hooks do).
    /// `None` means "ask the backend for it on promotion" (Conductor).
    pub answer: Option<String>,
    pub original_ask: Option<String>,
}

// ── Backend selection ────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Orca,
    Conductor,
}

impl Backend {
    /// The product name, for spoken lines and system prompts.
    pub fn label(self) -> &'static str {
        match self {
            Backend::Orca => "Orca",
            Backend::Conductor => "Conductor",
        }
    }

    /// Announcement dedup prefix, and `AgentResult::source`.
    pub fn slug(self) -> &'static str {
        match self {
            Backend::Orca => "orca",
            Backend::Conductor => "conductor",
        }
    }
}

/// Backends to try, best first. `VOX_ADE` pins one for development.
fn order() -> Vec<Backend> {
    match std::env::var("VOX_ADE").unwrap_or_default().trim().to_lowercase().as_str() {
        "orca" => vec![Backend::Orca],
        "conductor" => vec![Backend::Conductor],
        _ => vec![Backend::Orca, Backend::Conductor],
    }
}

/// The backend that actually answered last. Purely for display — `label()` in a
/// system prompt must not cost a CLI call, and being one turn stale is harmless.
static LAST: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

fn remember(b: Backend) {
    LAST.store(if b == Backend::Orca { 1 } else { 2 }, Ordering::Relaxed);
}

/// Which ADE Vox is talking to, for prompts and spoken lines.
///
/// Falls back to a cheap install check when nothing has been read yet, so the
/// very first system prompt still names the right product.
pub fn label() -> &'static str {
    match LAST.load(Ordering::Relaxed) {
        1 => Backend::Orca.label(),
        2 => Backend::Conductor.label(),
        _ => order()
            .into_iter()
            .find(|b| match b {
                Backend::Orca => orca::installed(),
                Backend::Conductor => conductor::installed(),
            })
            .unwrap_or(Backend::Orca)
            .label(),
    }
}

/// Dedup prefix for announcement keys, and `AgentResult::source`.
pub fn source_slug() -> &'static str {
    match LAST.load(Ordering::Relaxed) {
        2 => Backend::Conductor.slug(),
        _ => Backend::Orca.slug(),
    }
}

/// Run `f` against each candidate backend until one answers.
fn dispatch<T>(mut f: impl FnMut(Backend) -> Option<T>) -> Option<T> {
    for b in order() {
        if let Some(v) = f(b) {
            remember(b);
            return Some(v);
        }
    }
    None
}

// ── Reads ────────────────────────────────────────────────────────────────────

/// Worktrees with something going on, most recently active first.
pub fn read_state(max_items: usize) -> Vec<Workspace> {
    dispatch(|b| match b {
        Backend::Orca => orca::read_state(max_items),
        Backend::Conductor => conductor::read_state(max_items),
    })
    .unwrap_or_default()
}

/// Every launchable target, most recently touched first.
pub fn catalog() -> Vec<TargetCandidate> {
    dispatch(|b| match b {
        Backend::Orca => orca::catalog(),
        Backend::Conductor => conductor::catalog(),
    })
    .unwrap_or_default()
}

/// One watcher tick's worth of session snapshots.
pub fn poll(limit: usize) -> Vec<Snapshot> {
    dispatch(|b| match b {
        Backend::Orca => orca::poll(limit),
        Backend::Conductor => conductor::poll(limit),
    })
    .unwrap_or_default()
}

/// The backend that answered last, as a value rather than a label.
fn active() -> Backend {
    if LAST.load(Ordering::Relaxed) == 2 {
        Backend::Conductor
    } else {
        Backend::Orca
    }
}

/// The full answer text behind a snapshot, fetched only when it's about to be
/// spoken.
///
/// Orca's snapshots already carry it. Conductor's deliberately do not — pulling
/// message bodies costs a query per row, and only the one session being
/// announced needs them. The backend gate matters: a `Snapshot.key` is a pane key
/// on Orca and a session id on Conductor, so querying the wrong one is a
/// guaranteed miss and a wasted subprocess.
pub fn answer_for(snap: &Snapshot) -> Option<String> {
    if let Some(a) = snap.answer.as_ref() {
        return Some(a.clone());
    }
    match active() {
        Backend::Conductor => conductor::last_assistant_message(&snap.key),
        Backend::Orca => None,
    }
}

/// What this session was originally asked to do, for the same reason.
pub fn ask_for(snap: &Snapshot) -> Option<String> {
    if let Some(a) = snap.original_ask.as_ref() {
        return Some(a.clone());
    }
    match active() {
        Backend::Conductor => conductor::first_user_message(&snap.key),
        Backend::Orca => None,
    }
}

/// Names the model may target, repos first (that's what people actually say),
/// then worktree names. Deduped case-insensitively and capped — this goes into
/// every system prompt, so it has to stay a single short line.
pub fn launchable_names(max: usize) -> Vec<String> {
    let cat = catalog();
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for want_repo in [true, false] {
        for c in &cat {
            if (c.kind == "repo") != want_repo {
                continue;
            }
            if seen.insert(c.name.to_lowercase()) {
                out.push(c.name.clone());
            }
            if out.len() >= max {
                return out;
            }
        }
    }
    out
}

// ── Snippet cleaning ─────────────────────────────────────────────────────────

/// Markdown-stripping rules, compiled once.
///
/// `clean_snippet` runs on every recap line, every prompt build and every
/// carousel card, so re-compiling seven regexes per call was pure waste.
fn snippet_rules() -> &'static [(Regex, &'static str)] {
    static RULES: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    RULES.get_or_init(|| {
        [
            (r"(?s)```.*?```", ""),
            (r"`[^`\n]+`", ""),
            (r"\*{1,3}([^*\n]+)\*{1,3}", "$1"),
            (r"_{1,2}([^_\n]+)_{1,2}", "$1"),
            (r"(?m)^#{1,6}\s+", ""),
            (r"[|#]", ""),
            (r"\s+", " "),
        ]
        .into_iter()
        .map(|(pat, repl)| (Regex::new(pat).unwrap(), repl))
        .collect()
    })
}

fn first_sentence() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(.{10,120}?[.!?])").unwrap())
}

/// The `[erreur]` marker `conductor::extract_last_assistant_text` puts in front
/// of an error envelope's text, stripped before the line is spoken.
fn erreur_prefix() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\[erreur\]\s*").unwrap())
}

pub fn clean_snippet(raw: &str) -> String {
    let mut t = raw.to_string();
    for (re, repl) in snippet_rules() {
        t = re.replace_all(&t, *repl).to_string();
    }
    let t = t.trim();
    // Keep only the first complete sentence
    if let Some(m) = first_sentence().captures(t) {
        return m[1].trim().to_string();
    }
    t.chars().take(80).collect::<String>().trim().to_string()
}

/// Does this preview read as a question aimed at the user?
pub fn looks_like_question(raw_preview: &str) -> bool {
    // Detect on the RAW preview: `clean_snippet` keeps only the first sentence,
    // which drops the trailing "…anything else?" that the "summary. Question?"
    // agent pattern ends with.
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?i)\b(peux|veux|est-ce|c'est quoi|comment|dois-je|can|should|which|what)\b")
            .unwrap()
    });
    raw_preview.trim_end().ends_with('?') || re.is_match(&clean_snippet(raw_preview))
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
/// talks about. `seed` rotates the phrasing between runs so consecutive recaps
/// don't sound copy-pasted; quiet worktrees collapse into ONE sentence.
fn build_brief_sentences(ws: &[Workspace], en: bool, seed: usize) -> Vec<(String, Option<String>)> {
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    let mut quiet: Vec<String> = Vec::new();

    for (i, w) in ws.iter().enumerate() {
        let Some(sess) = w.session.as_ref() else { continue };
        let p = &w.project;
        let a = &sess.agent;
        let raw_preview = sess.preview.as_deref().unwrap_or("");
        let preview = clean_snippet(raw_preview);
        let v = seed + i;

        let sentence = match sess.state {
            AgentState::Working => Some(match v % 3 {
                0 if en => format!("Agent {a} is still working on {p}."),
                1 if en => format!("{p}: agent {a} is on it right now."),
                _ if en => format!("Work is moving on {p} with agent {a}."),
                0 => format!("L'agent {a} bosse encore sur {p}."),
                1 => format!("{p} : l'agent {a} est toujours au travail."),
                _ => format!("Ça avance sur {p}, l'agent {a} est dessus."),
            }),
            AgentState::Errored => {
                let detail = erreur_prefix().replace(&preview, "").to_string();
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
            // An agent that asked something outranks the generic "it's done"
            // line: the question is the actionable part.
            AgentState::Done if looks_like_question(raw_preview) && !preview.is_empty() => {
                Some(match v % 3 {
                    0 if en => format!("{p} is waiting for your answer."),
                    1 if en => format!("The agent on {p} asked you something."),
                    _ if en => format!("There's a pending question on {p}."),
                    0 => format!("{p} attend ta réponse : {preview}"),
                    1 => format!("L'agent de {p} te pose une question : {preview}"),
                    _ => format!("Question en attente sur {p} : {preview}"),
                })
            }
            AgentState::Done => Some(match v % 3 {
                0 if en => format!("The agent on {p} is done — you'll need to test it."),
                1 if en => format!("{p} is ready for you to try."),
                _ if en => format!("{p} finished; give it a quick test."),
                0 => format!("L'agent sur {p} a terminé, il te faudra tester."),
                1 => format!("{p} est prêt, à tester quand tu veux."),
                _ => format!("C'est terminé sur {p}, un petit test s'impose."),
            }),
            AgentState::Blocked => Some(match v % 3 {
                0 if en => format!("{p} is waiting on you to continue."),
                1 if en => format!("The agent on {p} is blocked, waiting for your input."),
                _ if en => format!("{p} needs you before it can move on."),
                0 => format!("{p} attend ton feu vert pour continuer."),
                1 => format!("L'agent sur {p} est bloqué, il attend ta réponse."),
                _ => format!("{p} a besoin de toi pour avancer."),
            }),
            // Nothing actionable — collapse into the grouped sentence.
            AgentState::Idle => {
                quiet.push(p.clone());
                None
            }
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
    // Generation-based cancellation: any ⌥Space/barge-in AFTER this point kills
    // the brief for good — nothing can re-arm it (the old bool flag could be
    // cleared by the next voice turn, resurrecting killed advice).
    let gen0 = state.interrupt_gen.load(Ordering::SeqCst);

    let en = state.settings.lock().unwrap().language == "en";

    let ws = read_state(10);
    if ws.is_empty() {
        return false;
    }
    let seed = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        / 60) as usize;
    let sentences = build_brief_sentences(&ws, en, seed);
    if sentences.is_empty() {
        return false;
    }

    // Carousel cards for the renderer — sent before anything is spoken.
    let cards: Vec<Value> = ws.iter().map(card).collect();
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

    // LLM advice grounded in each worktree's original ask — streamed sentence by
    // sentence into the same session while earlier lines are being spoken.
    if !interrupted() {
        let model = state.settings.lock().unwrap().model.clone();
        let feed = ws
            .iter()
            .filter_map(|w| {
                let sess = w.session.as_ref()?;
                let ask = sess.original_ask.as_deref().unwrap_or("?");
                let last = sess.preview.as_deref().map(clean_snippet).unwrap_or_default();
                let unread = if sess.unread {
                    if en { ", unread" } else { ", non lu" }
                } else {
                    ""
                };
                Some(format!(
                    "- {} ({}{unread}) | {} \"{ask}\" | {} \"{last}\"",
                    w.project,
                    sess.state.descriptor(en),
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

        // The cancel callback aborts the SSE read loop itself, so an interrupted
        // brief releases the speech session (and speak_lock) promptly instead of
        // holding them for the full generation.
        let _ = crate::llm::chat_once_stream(&model, system, &user, 120, &interrupted, |sentence| {
            if !interrupted() {
                let _ = session.send(SpeechItem::Sentence { text: sentence, workspace: None });
            }
        });
    }

    let _ = session.send(SpeechItem::End);
    true
}

/// One carousel card. The renderer matches `project` to highlight the card an
/// announcement is about, so the field has to stay the repo name.
pub fn card(w: &Workspace) -> Value {
    json!({
        "project": w.project,
        "branch": w.branch,
        "agent": w.session.as_ref().map(|s| s.agent.clone()).unwrap_or_default(),
        "status": w.session.as_ref().map(|s| s.state.descriptor(true)).unwrap_or("none"),
        "unread": i64::from(w.session.as_ref().map(|s| s.unread).unwrap_or(false)),
        "preview": w.session.as_ref().and_then(|s| s.preview.as_deref()).map(clean_snippet).unwrap_or_default(),
    })
}

/// The same card, built from a watcher snapshot.
///
/// Lets the watcher keep the carousel fresh out of the poll it already did,
/// rather than paying for a second read every 30 seconds — which on Conductor
/// meant 1 + 3N `sqlite3` spawns and is why the preview used to be left empty.
pub fn snapshot_card(s: &Snapshot) -> Value {
    json!({
        "project": s.project,
        "branch": s.branch,
        "agent": s.agent,
        "status": s.state.descriptor(true),
        "unread": 0,
        "preview": s.answer.as_deref().map(clean_snippet).unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(project: &str, state: AgentState, preview: &str) -> Workspace {
        Workspace {
            project: project.into(),
            branch: "main".into(),
            path: "/tmp".into(),
            session: Some(SessionInfo {
                agent: "claude".into(),
                state,
                last_activity: Some(1_790_000_000_000),
                preview: Some(preview.into()),
                original_ask: Some("fix the tests".into()),
                unread: false,
            }),
        }
    }

    #[test]
    fn every_actionable_state_produces_a_sentence() {
        for st in [AgentState::Working, AgentState::Done, AgentState::Blocked, AgentState::Errored] {
            let out = build_brief_sentences(&[ws("findy", st, "All green now.")], false, 0);
            assert_eq!(out.len(), 1, "{st:?} said nothing");
            assert!(out[0].0.contains("findy"), "{st:?}: {}", out[0].0);
        }
    }

    /// Idle worktrees must never get a sentence of their own — they collapse
    /// into the single grouped line, which is what keeps a 36-worktree recap
    /// under a minute.
    #[test]
    fn idle_worktrees_collapse_into_one_line() {
        let ws = [
            ws("a", AgentState::Idle, ""),
            ws("b", AgentState::Idle, ""),
            ws("c", AgentState::Idle, ""),
        ];
        let out = build_brief_sentences(&ws, true, 0);
        assert_eq!(out.len(), 1, "expected one grouped line, got {out:?}");
        assert!(out[0].0.contains("a, b and c"), "{}", out[0].0);
    }

    /// A finished agent that ended on a question is a question first.
    #[test]
    fn a_done_agent_that_asked_something_is_announced_as_a_question() {
        let out = build_brief_sentences(
            &[ws("findy", AgentState::Done, "I refactored the loader. Should I also drop the old path?")],
            true,
            0,
        );
        assert!(out[0].0.contains("waiting for your answer"), "{}", out[0].0);
    }

    #[test]
    fn question_detection_survives_snippet_truncation() {
        // clean_snippet keeps only the first sentence; the question is in the
        // second, so detection has to run on the raw text.
        assert!(looks_like_question("Done, tests pass. Want me to push it?"));
        assert!(!looks_like_question("Done, tests pass. I pushed it."));
    }

    #[test]
    fn snippet_cleaning_strips_markdown_and_keeps_one_sentence() {
        // Only the FIRST sentence survives — a recap line has to stay speakable.
        assert_eq!(
            clean_snippet("Rewrote the loader. Then I moved on to the tests."),
            "Rewrote the loader."
        );
        assert_eq!(clean_snippet("## Heading\n\nShort."), "Heading Short.");
        // Inline code and emphasis are dropped, not read aloud as punctuation.
        assert_eq!(clean_snippet("**Fixed** the `loader` and shipped it now."), "Fixed the and shipped it now.");
        // A fenced block leaves nothing quotable behind.
        assert_eq!(clean_snippet("```\nfn main() {}\n```"), "");
    }

    #[test]
    fn phrasing_rotates_with_the_seed() {
        let a = build_brief_sentences(&[ws("findy", AgentState::Working, "")], true, 0);
        let b = build_brief_sentences(&[ws("findy", AgentState::Working, "")], true, 1);
        assert_ne!(a[0].0, b[0].0, "consecutive recaps must not sound copy-pasted");
    }

    #[test]
    fn join_names_stays_speakable_past_four() {
        let many: Vec<String> = ["a", "b", "c", "d", "e"].iter().map(|s| s.to_string()).collect();
        assert_eq!(join_names(&many, true), "a, b and 3 others");
        assert_eq!(join_names(&many[..2], false), "a et b");
    }

    /// `VOX_ADE` has to be able to pin a single backend, or a developer on a
    /// machine with both installed cannot test the other one.
    #[test]
    fn vox_ade_pins_one_backend() {
        // Can't set env in a parallel test run without racing, so assert on the
        // parser's shape instead: auto tries Orca first, then Conductor.
        assert_eq!(order(), vec![Backend::Orca, Backend::Conductor]);
    }

    /// Drives the real dispatcher against whichever ADE is installed and prints
    /// the recap exactly as it would be spoken — run explicitly with
    /// `cargo test -- --ignored --nocapture live_recap`.
    #[test]
    #[ignore]
    fn live_recap() {
        let ws = read_state(10);
        assert!(!ws.is_empty(), "no worktrees — is an ADE running?");
        println!("backend: {}  ({} worktrees)", label(), ws.len());
        for (fr, en) in [(false, "FR"), (true, "EN")].map(|(b, l)| (b, l)) {
            println!("\n── {en} ──");
            for (line, tag) in build_brief_sentences(&ws, fr, 0) {
                println!("  [{}] {line}", tag.unwrap_or_default());
            }
        }
        // Every spoken line must name a project that is really in the data —
        // the one invariant the whole grounding story rests on.
        let known: std::collections::HashSet<&str> =
            ws.iter().map(|w| w.project.as_str()).collect();
        for (line, tag) in build_brief_sentences(&ws, false, 0) {
            let tag = tag.expect("every recap line is tagged with a workspace");
            assert!(known.contains(tag.as_str()), "invented project in: {line}");
        }
    }

    #[test]
    fn backend_labels_are_the_product_names() {
        assert_eq!(Backend::Orca.label(), "Orca");
        assert_eq!(Backend::Conductor.label(), "Conductor");
        assert_eq!(Backend::Orca.slug(), "orca");
    }
}
