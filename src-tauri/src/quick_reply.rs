//! "Quick reply": right after Vox announces that an agent is blocked on the user
//! (or errored), a bare "oui vas-y" / "non" / "fais plutôt X" is routed to that
//! agent's worktree — no project name, no prompt to formulate.
//!
//! Everything here is deterministic and pure on purpose: a 3B model must not
//! arbitrate whether "oui" answers the agent or something else, and the routing
//! decision costs no LLM call. The risk is a false positive (a "oui" captured
//! while the user meant something else), so the mode is gated four ways: a short
//! window, single consumption, a strict vocabulary, and rejection of any phrase
//! that names another project.

use std::sync::Mutex;

use crate::announce::{AnnounceKind, Announcement};

/// How long after the announcement finished a bare answer still counts.
pub const QUICK_REPLY_WINDOW_SECS: u64 = 90;

/// A redirect ("fais plutôt X") shorter than this is noise, not an instruction.
const MIN_REDIRECT_CHARS: usize = 3;

/// Who a quick reply goes to: the last blocked agent Vox actually finished
/// announcing.
#[derive(Clone, Debug, PartialEq)]
pub struct QuickReply {
    /// Repo name, as announced (resolved to a worktree at routing time).
    pub project: String,
    /// What the agent said, kept for the log / future use.
    pub question: String,
    /// Unix seconds at which the announcement finished being spoken.
    pub announced_at: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Reply {
    Yes,
    No,
    /// Free sentence, passed to the agent as spoken.
    Say(String),
}

impl Reply {
    /// The prompt handed to the agent.
    pub fn prompt(&self, en: bool) -> String {
        match (self, en) {
            (Reply::Yes, true) => "Yes, go ahead.".into(),
            (Reply::Yes, false) => "Oui, vas-y.".into(),
            (Reply::No, true) => "No, don't do it.".into(),
            (Reply::No, false) => "Non, ne le fais pas.".into(),
            (Reply::Say(s), _) => s.clone(),
        }
    }
}

/// The prompt for the NEW agent session started in the blocked worktree. Vox
/// never types into an existing terminal (it could be a shell or a dev server,
/// and the text comes from speech recognition), so the answer goes through the
/// same launch path as any other agent. A fresh agent has no memory of what it
/// was asked, so the question it was blocked on travels with the answer.
pub fn context_prompt(question: &str, reply: &Reply, en: bool) -> String {
    let q = question.trim();
    let a = reply.prompt(en);
    if q.is_empty() {
        return a;
    }
    if en {
        format!("A previous agent in this worktree was blocked and asked: \"{q}\". The user's answer: {a} Continue from there.")
    } else {
        format!("Un agent précédent de ce worktree était bloqué et a demandé : « {q} ». Réponse de l'utilisateur : {a} Continue à partir de là.")
    }
}

pub fn is_valid(q: &QuickReply, now: u64) -> bool {
    now.saturating_sub(q.announced_at) <= QUICK_REPLY_WINDOW_SECS
}

/// What a finished announcement batch leaves behind. Only a batch whose urgent
/// announcements (NeedsInput / Errored) all concern ONE project qualifies: with
/// two blocked agents "oui" is ambiguous, and guessing would answer the wrong
/// one. A batch with nothing urgent (Finished…) clears any older target — a
/// "oui" after it answers the offer, not a stale block.
pub fn from_batch(batch: &[Announcement], now: u64) -> Option<QuickReply> {
    let urgent: Vec<&Announcement> = batch
        .iter()
        .filter(|a| matches!(a.kind, AnnounceKind::NeedsInput | AnnounceKind::Errored))
        .collect();
    let last = urgent.last()?;
    if urgent.iter().any(|a| a.project != last.project) {
        return None;
    }
    Some(QuickReply {
        project: last.project.clone(),
        question: last.answer.clone(),
        announced_at: now,
    })
}

/// Arm (or clear) the slot after an announcement batch. Only a batch that was
/// really spoken to the end arms it: an interrupted one is parked and
/// re-announced, and "oui" must not answer a question the user never heard.
pub fn arm(slot: &Mutex<Option<QuickReply>>, batch: &[Announcement], completed: bool, now: u64) {
    if completed {
        *slot.lock().unwrap() = from_batch(batch, now);
    }
}

/// The routing decision, pure: Some((target, reply)) when `transcript` is a
/// quick reply to a still-valid target (and consumes it), None to fall through
/// to the normal flow. `other_projects` are names whose mention means the user
/// moved on.
pub fn route(
    slot: &Mutex<Option<QuickReply>>,
    transcript: &str,
    other_projects: &[String],
    now: u64,
) -> Option<(QuickReply, Reply)> {
    peek(slot, now)?;
    let reply = classify(transcript, other_projects)?;
    // Consume before acting: whatever happens next, it was answered once.
    let target = take(slot, now)?;
    Some((target, reply))
}

/// The pending target, if still inside its window. An expired one is dropped.
pub fn peek(slot: &Mutex<Option<QuickReply>>, now: u64) -> Option<QuickReply> {
    let mut g = slot.lock().unwrap();
    match g.as_ref() {
        Some(q) if is_valid(q, now) => Some(q.clone()),
        Some(_) => {
            *g = None;
            None
        }
        None => None,
    }
}

/// Consume the target: a quick reply is answered once.
pub fn take(slot: &Mutex<Option<QuickReply>>, now: u64) -> Option<QuickReply> {
    let q = peek(slot, now)?;
    *slot.lock().unwrap() = None;
    Some(q)
}

/// Lowercase, accents kept, everything that isn't a letter/digit becomes a
/// space: "Oui, vas-y !" → "oui vas y".
fn words(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(String::from)
        .collect()
}

const YES_WORDS: &[&str] = &[
    "oui", "ouais", "yes", "yep", "yeah", "ok", "okay", "go", "vas", "y", "allez", "approuve",
    "approve", "continue", "fais", "le", "do", "it", "ahead", "sure", "d", "accord", "please", "merci", "stp", "thanks",
];
const YES_KEYS: &[&str] = &[
    "oui", "ouais", "yes", "yep", "yeah", "ok", "okay", "go", "vas", "approuve", "approve",
    "continue", "fais", "sure", "accord", "do",
];
const NO_WORDS: &[&str] = &[
    "non", "no", "nope", "nah", "nan", "refuse", "deny", "reject", "annule", "cancel", "pas", "ça",
    "ca", "le", "fais", "ne", "don", "t", "do", "it", "that", "surtout", "merci", "please", "stp",
];
const NO_KEYS: &[&str] =
    &["non", "no", "nope", "nah", "nan", "refuse", "deny", "reject", "annule", "cancel", "pas", "don"];

/// More than this and it's a sentence, not a bare answer.
const MAX_BARE_WORDS: usize = 5;

fn bare(ws: &[String], vocab: &[&str], keys: &[&str]) -> bool {
    !ws.is_empty()
        && ws.len() <= MAX_BARE_WORDS
        && ws.iter().all(|w| vocab.contains(&w.as_str()))
        && ws.iter().any(|w| keys.contains(&w.as_str()))
}

/// Does `ws` contain `name` as whole words?
fn mentions(ws: &[String], name: &str) -> bool {
    let nw = words(name);
    if nw.is_empty() || nw.iter().map(|w| w.chars().count()).sum::<usize>() < 3 {
        return false;
    }
    ws.windows(nw.len()).any(|w| w == nw.as_slice())
}

/// "fais plutôt X", "plutôt X", "instead X", "non, fais X", "no, do X".
fn redirect(text: &str) -> Option<String> {
    use std::sync::OnceLock;
    // English puts the marker last: "no, do it with a soft delete instead". The
    // sentence is kept whole ("do it with a soft delete"), not cut after "do".
    static TRAILING: OnceLock<regex::Regex> = OnceLock::new();
    let trailing = TRAILING.get_or_init(|| {
        regex::Regex::new(r"(?is)^\s*(?:no\b[\s,.!:]*)?(.+?)[\s,.!]+(?:instead|rather)\s*[.!]?\s*$").unwrap()
    });
    if let Some(c) = trailing.captures(text) {
        let x = c.get(1)?.as_str().trim();
        if x.chars().count() >= MIN_REDIRECT_CHARS {
            return Some(x.to_string());
        }
    }
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            r"(?is)^\s*(?:(?:non|no)\b[\s,.!:]*)?(?:(?:fais|fait|faites|do|make|try)\s+)?(?:plutôt|plutot|instead|rather)\b[\s,:]*(.+?)\s*$|^\s*(?:non|no)\b[\s,.!:]*(?:fais|fait|do|make)\s+(.+?)\s*$",
        )
        .unwrap()
    });
    let c = re.captures(text)?;
    let x = c.get(1).or_else(|| c.get(2))?.as_str().trim();
    (x.chars().count() >= MIN_REDIRECT_CHARS).then(|| x.to_string())
}

/// Is `transcript` a bare answer to the blocked agent? `other_projects` are the
/// names of every project EXCEPT the pending one: naming one means the user is
/// talking about something else. None → fall back to the normal flow.
pub fn classify(transcript: &str, other_projects: &[String]) -> Option<Reply> {
    let ws = words(transcript);
    if ws.is_empty() || other_projects.iter().any(|p| mentions(&ws, p)) {
        return None;
    }
    // Redirect first: "non, fais plutôt X" must not be read as a bare "non".
    if let Some(x) = redirect(transcript) {
        return Some(Reply::Say(x));
    }
    // "don't do it" contains "do it": check the negative vocabulary first.
    if bare(&ws, NO_WORDS, NO_KEYS) {
        return Some(Reply::No);
    }
    if bare(&ws, YES_WORDS, YES_KEYS) {
        return Some(Reply::Yes);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ann(kind: AnnounceKind, project: &str) -> Announcement {
        Announcement {
            id: 0,
            kind,
            key: format!("k:{project}"),
            project: project.into(),
            answer: "Should I delete the table?".into(),
            original_ask: None,
            created_unix: 0,
            attempts: 0,
            not_before: 0,
        }
    }

    fn none() -> Vec<String> {
        vec![]
    }

    #[test]
    fn yes_in_french_and_english() {
        for t in ["oui vas-y", "Oui.", "vas-y", "ok", "go", "yes", "yes go ahead", "do it", "d'accord", "oui, fais-le", "approuve", "continue"] {
            assert_eq!(classify(t, &none()), Some(Reply::Yes), "{t}");
        }
    }

    #[test]
    fn no_in_french_and_english() {
        for t in ["non", "Non !", "no", "nope", "refuse", "annule", "non, ne le fais pas", "don't do it", "no don't"] {
            assert_eq!(classify(t, &none()), Some(Reply::No), "{t}");
        }
    }

    #[test]
    fn redirect_passes_the_sentence_through() {
        assert_eq!(
            classify("fais plutôt une migration réversible", &none()),
            Some(Reply::Say("une migration réversible".into()))
        );
        assert_eq!(
            classify("Non, fais plutôt un revert", &none()),
            Some(Reply::Say("un revert".into()))
        );
        assert_eq!(
            classify("no, do it with a soft delete instead", &none()),
            Some(Reply::Say("do it with a soft delete".into()))
        );
        assert_eq!(
            classify("instead use a soft delete", &none()),
            Some(Reply::Say("use a soft delete".into()))
        );
    }

    #[test]
    fn naming_another_project_is_never_captured() {
        let others = vec!["orivo".to_string(), "tri bos".to_string()];
        assert_eq!(classify("oui vas-y sur orivo", &others), None);
        assert_eq!(classify("non tri bos", &others), None);
        assert_eq!(classify("fais plutôt ça sur orivo", &others), None);
        // …but the same phrase with no foreign name is a quick reply.
        assert_eq!(classify("oui vas-y", &others), Some(Reply::Yes));
    }

    #[test]
    fn real_commands_fall_through() {
        for t in [
            "lance un agent sur findy pour corriger les tests",
            "où en sont mes agents ?",
            "il a dit quoi ?",
            "stop",
            "launch an agent on the tests",
            "what's the status",
            "oui mais explique-moi d'abord ce qu'il veut faire",
            "",
        ] {
            assert_eq!(classify(t, &none()), None, "{t}");
        }
    }

    #[test]
    fn window_expires_and_is_consumed_once() {
        let slot = Mutex::new(from_batch(&[ann(AnnounceKind::NeedsInput, "findy")], 1000));
        assert!(peek(&slot, 1000 + QUICK_REPLY_WINDOW_SECS).is_some());
        let q = take(&slot, 1010).expect("valid inside the window");
        assert_eq!(q.project, "findy");
        assert!(take(&slot, 1011).is_none(), "already consumed");

        let slot = Mutex::new(from_batch(&[ann(AnnounceKind::NeedsInput, "findy")], 1000));
        assert!(take(&slot, 1000 + QUICK_REPLY_WINDOW_SECS + 1).is_none());
        assert!(slot.lock().unwrap().is_none(), "expired target is dropped");
    }

    #[test]
    fn only_blocked_announcements_arm_it() {
        assert!(from_batch(&[ann(AnnounceKind::NeedsInput, "findy")], 5).is_some());
        assert!(from_batch(&[ann(AnnounceKind::Errored, "findy")], 5).is_some());
        assert!(from_batch(&[ann(AnnounceKind::Finished, "findy")], 5).is_none());
        assert!(from_batch(&[ann(AnnounceKind::VoxAgentDone, "findy")], 5).is_none());
    }

    #[test]
    fn two_blocked_projects_are_ambiguous_one_is_not() {
        let two = [ann(AnnounceKind::NeedsInput, "findy"), ann(AnnounceKind::NeedsInput, "orivo")];
        assert!(from_batch(&two, 5).is_none());
        let mixed = [ann(AnnounceKind::Finished, "orivo"), ann(AnnounceKind::NeedsInput, "findy")];
        assert_eq!(from_batch(&mixed, 5).unwrap().project, "findy");
    }

    #[test]
    fn route_answers_a_blocked_agent_once_without_naming_it() {
        let slot = Mutex::new(None);
        arm(&slot, &[ann(AnnounceKind::NeedsInput, "findy")], true, 1000);
        let (t, r) = route(&slot, "oui vas-y", &none(), 1010).expect("routed");
        assert_eq!((t.project.as_str(), r), ("findy", Reply::Yes));
        assert!(route(&slot, "oui vas-y", &none(), 1011).is_none(), "consumed");
    }

    #[test]
    fn route_passes_redirect_text_as_spoken() {
        let slot = Mutex::new(None);
        arm(&slot, &[ann(AnnounceKind::Errored, "findy")], true, 0);
        let (_, r) = route(&slot, "fais plutôt une migration séparée", &none(), 5).unwrap();
        assert_eq!(r, Reply::Say("une migration séparée".into()));
    }

    #[test]
    fn route_ignores_commands_other_projects_and_expiry_without_consuming() {
        let slot = Mutex::new(None);
        arm(&slot, &[ann(AnnounceKind::NeedsInput, "findy")], true, 0);
        let others = vec!["orivo".to_string()];
        assert!(route(&slot, "où en sont mes agents", &others, 5).is_none());
        assert!(route(&slot, "oui sur orivo", &others, 5).is_none());
        assert!(peek(&slot, 5).is_some(), "still armed after a fall-through");
        assert!(route(&slot, "oui", &others, QUICK_REPLY_WINDOW_SECS + 1).is_none());
    }

    #[test]
    fn arm_needs_a_completed_blocked_announcement() {
        let slot = Mutex::new(None);
        arm(&slot, &[ann(AnnounceKind::NeedsInput, "findy")], false, 0);
        assert!(peek(&slot, 1).is_none(), "interrupted batch must not arm");
        arm(&slot, &[ann(AnnounceKind::Finished, "findy")], true, 0);
        assert!(peek(&slot, 1).is_none(), "Finished must not arm");
        arm(&slot, &[ann(AnnounceKind::NeedsInput, "findy")], true, 0);
        assert!(peek(&slot, 1).is_some());
    }

    #[test]
    fn context_prompt_carries_the_question() {
        let p = context_prompt("Should I delete the table?", &Reply::Yes, true);
        assert!(p.contains("Should I delete the table?") && p.contains("Yes, go ahead."));
        let p = context_prompt("Je supprime la table ?", &Reply::No, false);
        assert!(p.contains("Je supprime la table ?") && p.contains("Non, ne le fais pas."));
        assert_eq!(context_prompt("  ", &Reply::Yes, true), "Yes, go ahead.");
    }

    #[test]
    fn prompts_are_short_and_bilingual() {
        assert_eq!(Reply::Yes.prompt(false), "Oui, vas-y.");
        assert_eq!(Reply::No.prompt(true), "No, don't do it.");
        assert_eq!(Reply::Say("X".into()).prompt(true), "X");
    }
}
