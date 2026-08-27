//! Proactive speech: telling the user an agent finished, without being asked.
//!
//! Two halves.
//!
//! **The result store** remembers every finished agent's answer so the user can
//! ask about it later ("il a dit quoi ?"). It is written even when speaking is
//! off, and deliberately kept OUT of `state.history` — that gets replayed into
//! every Ollama call, and multi-KB agent answers would poison a 3B's context
//! within two agents.
//!
//! **The queue** is durable: announcements survive an ⌥Space interrupt and come
//! back. One drainer thread owns all speaking, so N pending announcements cost N
//! `VecDeque` entries rather than N parked OS threads.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use regex::Regex;
use serde_json::json;
use tauri::{AppHandle, Emitter};

use crate::agents::{AgentJob, AgentOutcome};
use crate::speech::{self, SpeechItem};
use crate::{now_unix, ui_at_rest, AppState};

// ── Tunables ─────────────────────────────────────────────────────────────────

/// Beyond this the oldest announcements are dropped (and counted, so the lead
/// sentence can say "and N others"). Never the newest — recency is the point.
const MAX_QUEUE: usize = 12;
/// Announcements created within this window of each other are spoken together.
const COALESCE_WINDOW_SECS: u64 = 6;
const MAX_BATCH: usize = 3;
/// Answers kept for follow-up questions.
const MAX_RESULTS: usize = 20;
/// A result older than this is no longer injected into the system prompt.
const RESULT_FRESH_SECS: u64 = 30 * 60;
/// At most this many recent results are shown to the model.
const RESULTS_IN_PROMPT: usize = 4;
/// After this many interrupted attempts, stop trying to speak an item.
const MAX_ATTEMPTS: u8 = 4;
/// Suppress a repeat of the same key for this long after speaking it.
const RECENT_TTL_SECS: u64 = 600;
/// Minimum gap between two "I could do X next" offers.
const PROPOSAL_COOLDOWN_SECS: u64 = 60;
/// Rough speaking rate, for turning a character count into seconds.
const CHARS_PER_SECOND: f32 = 14.0;

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AnnounceKind {
    /// A Conductor session produced a new answer.
    Finished,
    Errored,
    NeedsInput,
    VoxAgentDone,
    VoxAgentFailed,
}

impl AnnounceKind {
    fn as_str(self) -> &'static str {
        match self {
            AnnounceKind::Finished => "finished",
            AnnounceKind::Errored => "errored",
            AnnounceKind::NeedsInput => "needs_input",
            AnnounceKind::VoxAgentDone => "agent_done",
            AnnounceKind::VoxAgentFailed => "agent_failed",
        }
    }
    /// A blocked agent outranks a finished one — it's waiting on the user.
    fn urgent(self) -> bool {
        matches!(self, AnnounceKind::Errored | AnnounceKind::NeedsInput | AnnounceKind::VoxAgentFailed)
    }
}

#[derive(Clone)]
pub struct Announcement {
    pub id: u64,
    pub kind: AnnounceKind,
    /// Dedup identity: "conductor:<session id>" | "vox:<job id>".
    pub key: String,
    /// MUST equal Conductor's `repos.name` — it's what the renderer matches to
    /// highlight a carousel card.
    pub project: String,
    pub answer: String,
    pub original_ask: Option<String>,
    pub created_unix: u64,
    pub attempts: u8,
    /// Earliest time this may be spoken (back-off after an interrupt).
    pub not_before: u64,
}

#[derive(Clone)]
pub struct AgentResult {
    pub project: String,
    pub source: &'static str,
    pub task: Option<String>,
    pub original_ask: Option<String>,
    pub answer_full: String,
    pub outcome: String,
    pub finished_unix: u64,
}

#[derive(Default)]
pub struct Queue {
    pub items: VecDeque<Announcement>,
    pub dropped: u64,
    /// (key, spoken_at) of fully-delivered announcements.
    pub recent: VecDeque<(String, u64)>,
    pub seq: u64,
}

// ── Text preparation ─────────────────────────────────────────────────────────

/// Estimated seconds of speech.
pub fn speech_secs(text: &str) -> f32 {
    text.chars().count() as f32 / CHARS_PER_SECOND
}

/// Turn an agent's markdown answer into something that survives being read out
/// loud.
///
/// This is NOT `clean_snippet`, which extracts a one-sentence headline. Here we
/// keep the whole body and only remove what is unspeakable.
pub fn strip_for_speech(raw: &str, en: bool) -> String {
    let code_block = if en { "a code block" } else { "un bloc de code" };
    let link = if en { "a link" } else { "un lien" };

    // Fenced code → a placeholder, NOT nothing. Deleting it leaves incoherent
    // sentences ("I changed it to  and it works").
    let mut t = Regex::new(r"(?s)```.*?```")
        .unwrap()
        .replace_all(raw, format!(" {code_block}. ").as_str())
        .to_string();
    // Diff hunks read as pure noise.
    t = Regex::new(r"(?m)^(\+\+\+|---|@@).*$").unwrap().replace_all(&t, "").to_string();
    // Inline code usually holds the identifier the user needs — keep it.
    t = Regex::new(r"`+([^`\n]*)`+").unwrap().replace_all(&t, "$1").to_string();
    // Bullets and headings lose their marker but MUST gain terminal
    // punctuation: find_boundary can only split on it.
    t = Regex::new(r"(?m)^\s*[-*+]\s+").unwrap().replace_all(&t, "").to_string();
    t = Regex::new(r"(?m)^\s*#{1,6}\s+").unwrap().replace_all(&t, "").to_string();
    t = Regex::new(r"(?m)([^.!?:\n])\s*\n").unwrap().replace_all(&t, "$1. ").to_string();
    t = Regex::new(r"\*{1,3}([^*\n]+)\*{1,3}").unwrap().replace_all(&t, "$1").to_string();
    t = Regex::new(r"https?://\S+").unwrap().replace_all(&t, link).to_string();
    // Full paths are unbearable spoken; the file name carries the meaning.
    t = Regex::new(r"(?:[\w.\-]+/){1,}([\w.\-]+\.\w{1,6})").unwrap().replace_all(&t, "$1").to_string();
    t = t.chars().filter(|c| !matches!(*c as u32, 0x1F300..=0x1FAFF | 0x2600..=0x27BF)).collect();
    t = Regex::new(r"\s+").unwrap().replace_all(&t, " ").to_string();
    t = Regex::new(r"([.!?])\s*[.!?]+").unwrap().replace_all(&t, "$1").to_string();
    t.trim().to_string()
}

/// Head + tail, because an agent's verdict is almost always at the END —
/// truncating to the first N characters throws away the conclusion.
fn head_tail(text: &str, head: usize, tail: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= head + tail {
        return text.to_string();
    }
    let h: String = chars[..head].iter().collect();
    let t: String = chars[chars.len() - tail..].iter().collect();
    format!("{h}\n[…]\n{t}")
}

// ── Result store ─────────────────────────────────────────────────────────────

pub fn push_result(state: &Arc<AppState>, r: AgentResult) {
    let mut store = state.agent_results.lock().unwrap();
    store.push_back(r);
    while store.len() > MAX_RESULTS {
        store.pop_front();
    }
}

/// Recent agent answers, formatted for the system prompt.
///
/// Bounded three ways (count, age, characters) because this rides in EVERY
/// Ollama request. `chars().take()` throughout — byte slicing panics on the
/// accented text this codebase is full of.
pub fn results_block(state: &Arc<AppState>, en: bool) -> String {
    let now = now_unix();
    let store = state.agent_results.lock().unwrap();
    let recent: Vec<&AgentResult> = store
        .iter()
        .rev()
        .filter(|r| now.saturating_sub(r.finished_unix) < RESULT_FRESH_SECS)
        .take(RESULTS_IN_PROMPT)
        .collect();
    if recent.is_empty() {
        return String::new();
    }
    let mut out = String::from(if en {
        "\n\nRecent agent results (you may answer questions about these):"
    } else {
        "\n\nRésultats d'agents récents (tu peux répondre à des questions dessus) :"
    });
    for r in recent {
        let mins = now.saturating_sub(r.finished_unix) / 60;
        let ago = match (en, mins) {
            (true, 0) => "just now".to_string(),
            (true, m) => format!("{m} min ago"),
            (false, 0) => "à l'instant".to_string(),
            (false, m) => format!("il y a {m} min"),
        };
        let ask = r
            .original_ask
            .as_deref()
            .or(r.task.as_deref())
            .map(|a| a.chars().take(80).collect::<String>())
            .unwrap_or_default();
        let said: String = r.answer_full.chars().take(240).collect();
        let asked_label = if en { "asked" } else { "demandé" };
        let said_label = if en { "replied" } else { "a répondu" };
        out.push_str(&format!(
            "\n- {} ({}, {}) — {asked_label} : \"{}\" — {said_label} : \"{}\"",
            r.project, ago, r.outcome, ask, said
        ));
        if out.chars().count() > 1200 {
            break;
        }
    }
    out.push_str(if en {
        "\nWhen the user says \"and then?\", \"what did it say?\", \"details\", or asks about an agent that just finished, answer from THESE results — never from the worktree state."
    } else {
        "\nQuand l'utilisateur dit « et alors ? », « il a dit quoi ? », « détaille », ou parle d'un agent qui vient de finir, réponds à partir de CES résultats, jamais depuis l'état des worktrees."
    });
    out
}

// ── Enqueue ──────────────────────────────────────────────────────────────────

/// Called by `agents::finish` when a Vox-launched agent ends.
pub fn agent_finished(app: &AppHandle, state: &Arc<AppState>, job: &AgentJob, outcome: AgentOutcome) {
    let answer = outcome.text().unwrap_or_default().to_string();
    let kind = match outcome {
        AgentOutcome::Success { .. } => AnnounceKind::VoxAgentDone,
        _ => AnnounceKind::VoxAgentFailed,
    };
    push_result(
        state,
        AgentResult {
            project: job.label.clone(),
            source: "vox",
            task: Some(job.task.clone()),
            original_ask: Some(job.task.clone()),
            answer_full: head_tail(&answer, 6000, 2000),
            outcome: outcome.as_str().to_string(),
            finished_unix: now_unix(),
        },
    );
    enqueue(
        app,
        state,
        Announcement {
            id: 0,
            kind,
            key: format!("vox:{}", job.id),
            project: job.repo.clone(),
            answer,
            original_ask: Some(job.task.clone()),
            created_unix: now_unix(),
            attempts: 0,
            not_before: 0,
        },
    );
}

pub fn enqueue(app: &AppHandle, state: &Arc<AppState>, mut ann: Announcement) {
    if state.settings.lock().unwrap().agent_reply == "off" {
        return; // still remembered, just never spoken
    }
    let count = {
        let (lock, cv) = &*state.announce_q;
        let mut q = lock.lock().unwrap();

        // Already said this recently? Don't repeat it. Only FULL deliveries are
        // recorded, so an interrupt-requeue is never blocked by this.
        let now = now_unix();
        q.recent.retain(|(_, at)| now.saturating_sub(*at) < RECENT_TTL_SECS);
        if q.recent.iter().any(|(k, _)| *k == ann.key) {
            return;
        }

        q.seq += 1;
        ann.id = q.seq;

        // Same agent answering twice: replace in place, keeping queue position.
        if let Some(slot) = q.items.iter_mut().find(|a| a.key == ann.key) {
            let attempts = slot.attempts;
            *slot = ann;
            slot.attempts = attempts;
        } else {
            q.items.push_back(ann);
        }
        while q.items.len() > MAX_QUEUE {
            q.items.pop_front();
            q.dropped += 1;
        }
        let n = q.items.len();
        cv.notify_all();
        n
    };
    let _ = app.emit("announce-pending", json!({ "count": count }));
}

// ── Drainer ──────────────────────────────────────────────────────────────────

/// Start the single thread that speaks announcements. Called once at startup.
pub fn start_drainer(app: AppHandle, state: Arc<AppState>) {
    std::thread::spawn(move || loop {
        // Wait for work. PEEK only — nothing is removed until we can speak it.
        {
            let (lock, cv) = &*state.announce_q;
            let mut q = lock.lock().unwrap();
            while q.items.is_empty() {
                let (g, _) = cv.wait_timeout(q, Duration::from_millis(400)).unwrap();
                q = g;
            }
        }

        // Wait for silence. `ui_at_rest` covers the mic and the pill;
        // `speak_lock` covers a reply that is mid-sentence. The lock is only
        // PROBED — holding it here would deadlock against run_session, which
        // takes it for the whole session.
        loop {
            let free = state.speak_lock.try_lock().is_ok();
            if free && ui_at_rest(&state) {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }

        let batch = pop_batch(&state);
        if batch.is_empty() {
            continue;
        }
        let (dropped, remaining) = {
            let (lock, _) = &*state.announce_q;
            let mut q = lock.lock().unwrap();
            (std::mem::take(&mut q.dropped), q.items.len())
        };
        let _ = app.emit("announce-pending", json!({ "count": remaining }));

        let gen0 = state.interrupt_gen.load(Ordering::SeqCst);
        let completed = speak_batch(&app, &state, &batch, dropped, gen0);

        println!(
            "[vox] announce batch {:?} -> completed={completed}",
            batch.iter().map(|a| a.key.as_str()).collect::<Vec<_>>()
        );
        if completed {
            let now = now_unix();
            let (lock, _) = &*state.announce_q;
            let mut q = lock.lock().unwrap();
            for a in &batch {
                q.recent.push_back((a.key.clone(), now));
            }
            while q.recent.len() > 32 {
                q.recent.pop_front();
            }
        } else {
            requeue(&app, &state, batch);
        }
    });
}

/// Take up to MAX_BATCH announcements, urgent ones first, then FIFO.
fn pop_batch(state: &Arc<AppState>) -> Vec<Announcement> {
    let (lock, _) = &*state.announce_q;
    let mut q = lock.lock().unwrap();
    let now = now_unix();

    // Stable partition: urgency wins, but FIFO holds within each group.
    let ready: Vec<usize> = q
        .items
        .iter()
        .enumerate()
        .filter(|(_, a)| a.not_before <= now)
        .map(|(i, _)| i)
        .collect();
    let Some(&first) = ready
        .iter()
        .find(|&&i| q.items[i].kind.urgent())
        .or_else(|| ready.first())
    else {
        return Vec::new();
    };

    let lead = q.items.remove(first).unwrap();
    let t0 = lead.created_unix;
    let mut batch = vec![lead];
    while batch.len() < MAX_BATCH {
        let next = q.items.iter().position(|a| {
            a.not_before <= now && a.created_unix.saturating_sub(t0) <= COALESCE_WINDOW_SECS
        });
        match next {
            Some(i) => batch.push(q.items.remove(i).unwrap()),
            None => break,
        }
    }
    batch
}

/// An interrupted batch goes back to the FRONT, in order, with a growing
/// back-off. Nothing is ever lost — but if the user keeps cutting it off, the
/// voice gives up while the content stays answerable.
fn requeue(app: &AppHandle, state: &Arc<AppState>, batch: Vec<Announcement>) {
    let now = now_unix();
    let count = {
        let (lock, cv) = &*state.announce_q;
        let mut q = lock.lock().unwrap();
        for mut a in batch.into_iter().rev() {
            a.attempts = a.attempts.saturating_add(1);
            if a.attempts >= MAX_ATTEMPTS {
                println!("[vox] announcement {} interrupted {}x — memory only", a.key, a.attempts);
                continue;
            }
            a.not_before = now + (5 * a.attempts as u64).min(30);
            q.items.push_front(a);
        }
        cv.notify_all();
        q.items.len()
    };
    let _ = app.emit("announce-pending", json!({ "count": count }));
}

/// Speak one batch through a single speech session. Returns true if it landed.
fn speak_batch(
    app: &AppHandle,
    state: &Arc<AppState>,
    batch: &[Announcement],
    dropped: u64,
    gen0: u64,
) -> bool {
    let (en, mode, max_chars) = {
        let s = state.settings.lock().unwrap();
        (s.language == "en", s.agent_reply.clone(), s.agent_reply_max_chars)
    };
    // Two answers read back to back is unusable, whatever the setting says.
    let force_summary = batch.len() > 1;

    let _ = app.emit(
        "announce-start",
        json!({
            "workspace": batch[0].project,
            "kind": batch[0].kind.as_str(),
            "count": batch.len(),
        }),
    );

    let (tx, done) = speech::start_session_full(app.clone(), state.clone());
    let interrupted = || state.interrupt_gen.load(Ordering::SeqCst) != gen0;
    let send = |text: String, project: &str| {
        if !speech::is_speakable(&text) {
            return;
        }
        let _ = tx.send(SpeechItem::Sentence {
            text,
            workspace: Some(project.to_string()),
        });
    };

    if batch.len() > 1 {
        let names: Vec<String> = batch.iter().map(|a| a.project.clone()).collect();
        send(lead_sentence(en, &names, dropped), &batch[0].project);
    }

    for a in batch {
        if interrupted() {
            break;
        }
        render(state, a, en, &mode, max_chars, force_summary, gen0, &send);
    }

    if !interrupted() {
        if let Some(p) = proposal(state, batch, en) {
            send(p.clone(), &batch[0].project);
            // Give "oui, vas-y" something to refer to.
            let mut h = state.history.lock().unwrap();
            h.push(json!({ "role": "assistant", "content": p }));
            state.last_proposal_at.store(now_unix(), Ordering::SeqCst);
        }
    }

    let _ = tx.send(SpeechItem::End);
    let outcome = done.recv().ok();
    if outcome.is_none() {
        eprintln!("[vox] announce: speech session dropped its outcome");
    }
    let (started, completed) = outcome
        .as_ref()
        .map(|o| (o.started, o.completed))
        .unwrap_or((false, false));

    // The drainer owns settling here, since it took the outcome-bearing variant.
    if !started && completed {
        let _ = app.emit("speaking-done", ());
    }
    let _ = app.emit("announce-done", json!({ "spoken": started, "completed": completed }));
    completed
}

fn lead_sentence(en: bool, names: &[String], dropped: u64) -> String {
    let list = names.join(", ");
    let extra = if dropped > 0 {
        if en {
            format!(" And {dropped} more.")
        } else {
            format!(" Et {dropped} de plus.")
        }
    } else {
        String::new()
    };
    if en {
        format!("{} agents just finished: {list}.{extra}", names.len())
    } else {
        format!("{} agents viennent de finir : {list}.{extra}", names.len())
    }
}

/// Speak one announcement through `send`.
///
/// Everything goes through the same sink so ordering is guaranteed: the
/// deterministic headline is emitted FIRST — before any LLM call — so that a
/// failed, slow, or skipped summary still leaves something true spoken. (An
/// earlier shape returned the headline to the caller while streaming the
/// summary directly into the session, which silently spoke them in reverse.)
#[allow(clippy::too_many_arguments)]
fn render(
    state: &Arc<AppState>,
    a: &Announcement,
    en: bool,
    mode: &str,
    max_chars: usize,
    force_summary: bool,
    gen0: u64,
    send: &dyn Fn(String, &str),
) {
    send(headline(a, en), &a.project);

    let cleaned = strip_for_speech(&a.answer, en);
    if cleaned.is_empty() {
        return;
    }

    // A question is one sentence and paraphrasing it is actively harmful.
    if a.kind == AnnounceKind::NeedsInput {
        send(cleaned.chars().take(300).collect(), &a.project);
        return;
    }

    let verbatim_ok = mode == "verbatim"
        && !force_summary
        && cleaned.chars().count() <= max_chars
        && speech_secs(&cleaned) <= max_chars as f32 / CHARS_PER_SECOND;
    if verbatim_ok {
        // Prefix so it doesn't sound like Vox's own opinion.
        send(
            if en {
                format!("{} replied.", a.project)
            } else {
                format!("{} a répondu.", a.project)
            },
            &a.project,
        );
        for s in speech::split_sentences(&cleaned) {
            send(s, &a.project);
        }
        return;
    }

    // Summarize, streamed: the first sentence plays while the rest generates.
    let model = state.settings.lock().unwrap().model.clone();
    let sys = summary_system(en);
    let user = summary_user(en, &a.project, a.original_ask.as_deref(), &cleaned);
    let interrupted = || state.interrupt_gen.load(Ordering::SeqCst) != gen0;
    let mut sent = 0usize;
    crate::llm::chat_once_stream(&model, &sys, &user, 90, &interrupted, |s| {
        // The prompt asks for one sentence; a 3B will still sometimes keep
        // going, and every extra sentence is a chance to contradict the
        // headline. Hard-stop at one.
        if sent < 1 && !interrupted() && speech::is_speakable(&s) {
            sent += 1;
            send(s, &a.project);
        }
    });
    if sent == 0 && !interrupted() {
        // Ollama was busy or refused. Fall back to the agent's own opening
        // line — but only if it carries meaning. A snippet that is mostly the
        // "a code block" placeholder or a bare label ("Command:") is worse than
        // silence: the headline already said the true, useful thing.
        let snip = crate::conductor::clean_snippet(&cleaned);
        let placeholder = if en { "a code block" } else { "un bloc de code" };
        let meat = snip.replace(placeholder, "");
        let words = meat.split_whitespace().filter(|w| w.chars().any(char::is_alphabetic)).count();
        if words >= 4 {
            send(snip, &a.project);
        } else if !snip.is_empty() {
            println!("[vox] (dropped meaningless fallback snippet: {snip:?})");
        }
    }
}

fn headline(a: &Announcement, en: bool) -> String {
    // Rotate phrasing so repeated announcements don't sound like a machine —
    // same trick, and the same feel, as the startup brief.
    let v = (a.created_unix % 3) as usize;
    let p = &a.project;
    match (a.kind, en) {
        (AnnounceKind::VoxAgentDone | AnnounceKind::Finished, false) => match v {
            0 => format!("L'agent sur {p} a terminé."),
            1 => format!("Ça y est, {p} est fini."),
            _ => format!("{p} vient de terminer."),
        },
        (AnnounceKind::VoxAgentDone | AnnounceKind::Finished, true) => match v {
            0 => format!("The agent on {p} is done."),
            1 => format!("{p} just finished."),
            _ => format!("That's {p} wrapped up."),
        },
        (AnnounceKind::VoxAgentFailed, false) => format!("L'agent sur {p} a échoué."),
        (AnnounceKind::VoxAgentFailed, true) => format!("The agent on {p} failed."),
        (AnnounceKind::Errored, false) => format!("Il y a une erreur sur {p}."),
        (AnnounceKind::Errored, true) => format!("Something errored on {p}."),
        (AnnounceKind::NeedsInput, false) => format!("{p} attend ta réponse."),
        (AnnounceKind::NeedsInput, true) => format!("{p} is waiting on you."),
    }
}

/// One deterministic offer, on an unambiguous trigger, at most once a minute.
/// No second LLM call, and never for a question — the question IS the next step.
fn proposal(state: &Arc<AppState>, batch: &[Announcement], en: bool) -> Option<String> {
    if batch.len() != 1 {
        return None;
    }
    let a = &batch[0];
    let last = state.last_proposal_at.load(Ordering::SeqCst);
    if now_unix().saturating_sub(last) < PROPOSAL_COOLDOWN_SECS {
        return None;
    }
    let p = &a.project;
    match a.kind {
        AnnounceKind::VoxAgentDone | AnnounceKind::Finished if a.original_ask.is_some() => {
            Some(if en {
                format!("I can prompt {p} to test that, if you want.")
            } else {
                format!("Je peux prompter {p} pour tester ça, si tu veux.")
            })
        }
        AnnounceKind::VoxAgentFailed => Some(if en {
            format!("I can run the agent on {p} again.")
        } else {
            format!("Je peux relancer l'agent sur {p}.")
        }),
        _ => None,
    }
}

/// The summary prompt describes WHAT WAS DONE and nothing else.
///
/// It deliberately carries no conditional phrasings ("if the agent failed, say
/// X"). A 3B treats those as sentences to emit rather than branches to take:
/// an early version instructed « si l'agent a échoué, commence par "Ça a
/// échoué" » and the model duly said "Ça a échoué" about an agent that had just
/// succeeded — directly contradicting the headline spoken a second earlier.
/// Outcome and framing are the deterministic headline's job; this only
/// paraphrases the body.
fn summary_system(en: bool) -> String {
    if en {
        "You are Vox. In ONE spoken sentence, say what a coding agent changed.\n\
         - One sentence, 20 words maximum.\n\
         - Describe only what the message says was done. Invent nothing.\n\
         - Never say whether it succeeded or failed — the developer already knows.\n\
         - No file paths, no function names, no code, no lists, no questions.\n\
         - No preamble. Output the sentence and nothing else.\n\
         Example message: \"I added a retry to the upload helper and the tests pass.\"\n\
         Example output: It added a retry to the upload helper."
            .into()
    } else {
        "Tu es Vox. En UNE phrase orale, dis ce qu'un agent de code a changé.\n\
         - Une phrase, 20 mots maximum.\n\
         - Décris uniquement ce que le message dit avoir fait. N'invente rien.\n\
         - Ne dis jamais si ça a réussi ou échoué — le développeur le sait déjà.\n\
         - Aucun chemin de fichier, aucun nom de fonction, aucun code, aucune liste, aucune question.\n\
         - Pas de préambule. Sors la phrase et rien d'autre.\n\
         Exemple de message : « J'ai ajouté un retry au helper d'upload et les tests passent. »\n\
         Exemple de sortie : Il a ajouté un retry au helper d'upload."
            .into()
    }
}

fn summary_user(en: bool, project: &str, ask: Option<&str>, body: &str) -> String {
    let ask = ask.unwrap_or("—");
    // Head + tail: an agent's verdict is almost always at the end.
    let body = head_tail(body, 4000, 2000);
    if en {
        format!("Project: {project}\nWhat was asked: \"{ask}\"\nAgent's final message:\n\"\"\"\n{body}\n\"\"\"\nOne sentence:")
    } else {
        format!("Projet : {project}\nCe qui était demandé : \"{ask}\"\nMessage final de l'agent :\n\"\"\"\n{body}\n\"\"\"\nUne phrase :")
    }
}

// ── Commands ─────────────────────────────────────────────────────────────────

/// Number of announcements waiting to be spoken.
pub fn pending_count(state: &Arc<AppState>) -> usize {
    let (lock, _) = &*state.announce_q;
    let n = lock.lock().unwrap().items.len();
    n
}

/// Clear the back-off so the badge click speaks the backlog immediately.
pub fn speak_now(state: &Arc<AppState>) {
    let (lock, cv) = &*state.announce_q;
    let mut q = lock.lock().unwrap();
    for a in q.items.iter_mut() {
        a.not_before = 0;
        a.attempts = 0;
    }
    cv.notify_all();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_blocks_become_a_spoken_placeholder_not_a_hole() {
        let out = strip_for_speech("I changed it to ```rust\nlet x = 1;\n``` and it works.", false);
        assert!(out.contains("un bloc de code"), "got: {out}");
        assert!(!out.contains("let x"), "got: {out}");
    }

    #[test]
    fn paths_shrink_to_file_names_and_urls_are_named() {
        let out = strip_for_speech("Edited src/tauri/src/main.rs and see https://x.com/a/b", true);
        assert!(out.contains("main.rs"), "got: {out}");
        assert!(!out.contains("src/tauri"), "got: {out}");
        assert!(out.contains("a link"), "got: {out}");
    }

    /// Bullets must end up with terminal punctuation or `find_boundary` can
    /// never split them and the whole list becomes one giant "sentence".
    #[test]
    fn bullets_gain_terminal_punctuation() {
        let out = strip_for_speech("- fixed the login\n- added a test\n", false);
        assert!(out.contains("login."), "got: {out}");
        assert!(speech::split_sentences(&out).len() >= 2, "got: {out}");
    }

    #[test]
    fn head_tail_keeps_the_verdict_at_the_end() {
        let body = format!("{}VERDICT", "x".repeat(500));
        let out = head_tail(&body, 100, 20);
        assert!(out.ends_with("VERDICT"), "the conclusion must survive: {out}");
        assert!(out.contains("[…]"));
    }

    #[test]
    fn speech_secs_matches_the_verbatim_threshold() {
        // 420 chars is the default cap and should land near 30 seconds.
        let s = speech_secs(&"a".repeat(420));
        assert!((29.0..=31.0).contains(&s), "got {s}");
    }

    /// The fallback must stay silent rather than speak a bare label.
    #[test]
    fn meaningless_fallback_snippets_are_rejected() {
        let placeholder = "un bloc de code";
        for bad in ["Command:. un bloc de code.", "un bloc de code.", "Done."] {
            let meat = bad.replace(placeholder, "");
            let words = meat.split_whitespace().filter(|w| w.chars().any(char::is_alphabetic)).count();
            assert!(words < 4, "{bad:?} should be rejected, counted {words} words");
        }
        let good = "J'ai ajouté un message de bienvenue au démarrage.";
        let meat = good.replace(placeholder, "");
        let words = meat.split_whitespace().filter(|w| w.chars().any(char::is_alphabetic)).count();
        assert!(words >= 4, "a real sentence must survive");
    }

    #[test]
    fn urgent_kinds_outrank_finished() {
        assert!(AnnounceKind::Errored.urgent());
        assert!(AnnounceKind::NeedsInput.urgent());
        assert!(!AnnounceKind::Finished.urgent());
    }

    /// Every template must clear MIN_SENTENCE_CHARS (12), or the sentence
    /// splitter merges it into the next one and it loses its carousel tag.
    #[test]
    fn headlines_are_long_enough_to_stand_alone() {
        for kind in [
            AnnounceKind::VoxAgentDone,
            AnnounceKind::VoxAgentFailed,
            AnnounceKind::Errored,
            AnnounceKind::NeedsInput,
            AnnounceKind::Finished,
        ] {
            for en in [true, false] {
                for created in 0..3u64 {
                    let a = Announcement {
                        id: 1,
                        kind,
                        key: "k".into(),
                        project: "vox".into(),
                        answer: String::new(),
                        original_ask: None,
                        created_unix: created,
                        attempts: 0,
                        not_before: 0,
                    };
                    let h = headline(&a, en);
                    assert!(h.chars().count() >= 12, "too short to stand alone: {h:?}");
                    assert_eq!(speech::split_sentences(&h).len(), 1, "{h:?} must be one sentence");
                }
            }
        }
    }
}
