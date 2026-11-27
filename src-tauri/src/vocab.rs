//! Effective vocabulary: the user's own words plus the names Orca knows
//! (repos, worktree codenames, branches). Injected into the Whisper prompt and
//! used for a strict post-STT correction pass (Parakeet ignores the prompt).
//! Pure functions, no I/O — the caller fetches the catalog off the UI thread.
//! Auto-injected names are never written to `vocabulary.json`.

use crate::targets::{levenshtein, normalize};
use crate::workspace::TargetCandidate;

/// Byte budget for the whole vocabulary part of a Whisper prompt (the prompt
/// itself is capped at 800 bytes in `dictation_text`; long prompts confuse
/// Whisper).
const MAX_VOCAB_BYTES: usize = 500;
const MAX_BRANCH_LEN: usize = 24;
const MIN_FUZZY_LEN: usize = 5;

/// Default technical terms of the assistant path. `transcribe_with_prompt`
/// replaces the daemon's default prompt, so these are re-injected (mirrors
/// `INITIAL_PROMPT_*` in vox_stt.py; a test guards against drift).
const ASSISTANT_PROMPT_FR: &str = "Conversation avec Vox, mon assistant vocal. Tu peux me dire, tu vois ce que je veux dire, tu penses quoi. Termes techniques : agent, worktree, Orca, Vox, Claude, Codex, opencode, Ollama, refactor, deploy, bug, commit, pull request, terminal, dashboard, backend, frontend. Tu peux m'expliquer ? Il faut qu'on bosse sur quoi ?";
const ASSISTANT_PROMPT_EN: &str = "Technical discussion: agent, worktree, Orca, Vox, Claude, Codex, opencode, Ollama, refactor, deploy, bug, commit, pull request.";

/// Common words that must never be "corrected" into a project name.
const COMMON: &[&str] = &[
    "find", "finds", "fine", "fund", "main", "make", "made", "mode", "more", "most", "test",
    "tests", "text", "that", "this", "then", "than", "them", "they", "with", "work", "word", "what",
    "when", "where", "will", "from", "have", "here", "into", "just", "like", "look", "need", "next",
    "open", "show", "stop", "tell", "time", "unit", "want", "agent", "bien", "bon", "dans", "fait",
    "faire", "pour", "plus", "tout", "tous", "avec", "sans", "mais", "donc", "sur", "sous", "vers",
    "page", "code", "base", "data", "docs", "site", "tool", "tools", "demo", "lance",
    "lancer", "stoppe", "arrete", "projet", "ici", "cela", "cette", "elle", "nous", "vous", "ils",
];

/// Repo names, then worktree codenames, then cleaned branches — deduplicated
/// case-insensitively, in that priority order.
pub fn catalog_terms(cands: &[TargetCandidate]) -> Vec<String> {
    let mut repos = Vec::new();
    let mut names = Vec::new();
    let mut branches = Vec::new();
    for c in cands {
        repos.push(c.repo.trim().to_string());
        if c.kind == "repo" {
            repos.push(c.name.trim().to_string());
        } else {
            names.push(c.name.trim().to_string());
        }
        if let Some(b) = clean_branch(&c.branch) {
            branches.push(b);
        }
    }
    let mut seen = std::collections::HashSet::new();
    repos
        .into_iter()
        .chain(names)
        .chain(branches)
        .filter(|t| !t.is_empty() && seen.insert(t.to_lowercase()))
        .collect()
}

/// Names safe to use for the fuzzy post-STT correction: repos and worktree
/// codenames only. Branches ("auth", "feat"…) and the user's own words are
/// ordinary vocabulary that would rewrite free prose ("auto" → "auth").
pub fn fuzzy_terms(cands: &[TargetCandidate]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for c in cands {
        for n in [c.repo.trim(), c.name.trim()] {
            if n.chars().count() >= MIN_FUZZY_LEN && seen.insert(n.to_lowercase()) {
                out.push(n.to_string());
            }
        }
    }
    out
}

/// "orb/issue-25-…" → None (long slug); "feature/auth" → "auth".
fn clean_branch(b: &str) -> Option<String> {
    let tail = b.trim().rsplit('/').next().unwrap_or("").trim();
    let lower = tail.to_lowercase();
    if tail.is_empty()
        || tail.len() > MAX_BRANCH_LEN
        || tail.chars().any(char::is_whitespace)
        || matches!(lower.as_str(), "main" | "master" | "develop" | "head")
    {
        return None;
    }
    Some(tail.to_string())
}

/// User words first (always kept), then catalog terms while the total stays
/// under the byte budget. Case-insensitive dedupe.
pub fn effective_vocab(user: &[String], catalog: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut bytes = 0usize;
    for w in user.iter().map(|w| w.trim()).filter(|w| !w.is_empty()) {
        if seen.insert(w.to_lowercase()) {
            bytes += w.len() + 2;
            out.push(w.to_string());
        }
    }
    for t in catalog.iter().map(|t| t.trim()).filter(|t| !t.is_empty()) {
        if bytes + t.len() + 2 > MAX_VOCAB_BYTES {
            break;
        }
        if seen.insert(t.to_lowercase()) {
            bytes += t.len() + 2;
            out.push(t.to_string());
        }
    }
    out
}

/// Whisper prompt for the assistant voice path: default technical terms plus
/// the catalog names. `None` when the catalog is empty, so the daemon keeps its
/// own default prompt (behaviour identical to before).
pub fn assistant_prompt(fr: bool, catalog: &[String]) -> Option<String> {
    let base = if fr { ASSISTANT_PROMPT_FR } else { ASSISTANT_PROMPT_EN };
    let names = effective_vocab(&[], catalog);
    if names.is_empty() {
        return None;
    }
    Some(format!("{base} {} : {}.", if fr { "Projets" } else { "Projects" }, names.join(", ")))
}

fn words(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        if c.is_alphanumeric() {
            start.get_or_insert(i);
        } else if let Some(s) = start.take() {
            spans.push((s, i));
        }
    }
    if let Some(s) = start {
        spans.push((s, text.len()));
    }
    spans
}

fn threshold(len: usize) -> usize {
    if len >= 8 { 2 } else { 1 }
}

/// The unique single-token name within the edit budget of `w`, if any.
fn best_match<'a>(w: &str, names: &'a [(String, &'a str)]) -> Option<&'a str> {
    if w.chars().count() < MIN_FUZZY_LEN || COMMON.contains(&w) {
        return None;
    }
    if names.iter().any(|(n, _)| n == w) {
        return None; // already a known name
    }
    let mut hit: Option<&str> = None;
    for (n, raw) in names {
        let d = levenshtein(w, n);
        if d >= 1 && d <= threshold(n.chars().count()) && w.chars().next() == n.chars().next() {
            if hit.is_some() {
                return None; // ambiguous: leave it, like resolve_target asks
            }
            hit = Some(raw);
        }
    }
    hit
}

/// Replace a word (or two adjacent words) that is a near-miss of exactly one
/// known name by that name. Strict: ≥ 4 letters, same first letter, edit
/// distance ≤ 1 (≤ 2 for long names), never a common word, never ambiguous.
/// Idempotent and word-boundary safe.
pub fn correct_transcript(text: &str, terms: &[String]) -> String {
    correct_with_changes(text, terms).0
}

/// Like [`correct_transcript`], also returning each `(heard, name)` rewrite so
/// the app can *suggest* turning it into a permanent replacement.
pub fn correct_with_changes(text: &str, terms: &[String]) -> (String, Vec<(String, String)>) {
    let mut changes: Vec<(String, String)> = Vec::new();
    let names: Vec<(String, &str)> = terms
        .iter()
        .map(|t| (normalize(t), t.as_str()))
        .filter(|(n, _)| n.chars().count() >= MIN_FUZZY_LEN)
        .collect();
    if names.is_empty() {
        return (text.to_string(), changes);
    }
    let spans = words(text);
    let mut out = String::with_capacity(text.len());
    let mut pos = 0;
    let mut i = 0;
    while i < spans.len() {
        let (s, e) = spans[i];
        let one = normalize(&text[s..e]);
        // Prefer the two-word reading only when the single word is not a match
        // on its own ("fin di" → "findy"); only across plain whitespace.
        let mut done = false;
        if let Some(&(s2, e2)) = spans.get(i + 1) {
            if text[e..s2].chars().all(char::is_whitespace) && best_match(&one, &names).is_none() {
                let two = format!("{one}{}", normalize(&text[s2..e2]));
                if let Some(m) = best_match(&two, &names) {
                    out.push_str(&text[pos..s]);
                    out.push_str(m);
                    changes.push((text[s..e2].to_string(), m.to_string()));
                    pos = e2;
                    i += 2;
                    done = true;
                }
            }
        }
        if done {
            continue;
        }
        if let Some(m) = best_match(&one, &names) {
            out.push_str(&text[pos..s]);
            out.push_str(m);
            if !text[s..e].eq_ignore_ascii_case(m) {
                changes.push((text[s..e].to_string(), m.to_string()));
            }
            pos = e;
        }
        i += 1;
    }
    out.push_str(&text[pos..]);
    (out, changes)
}

const MAX_SUGGESTIONS: usize = 5;
static SUGGESTIONS: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

/// Queue `(heard, name)` pairs for the user to validate in Settings. Pure
/// bookkeeping: skips pairs already queued or already a known replacement.
/// Nothing is ever written to `vocabulary.json` from here.
pub fn queue_suggestions(changes: &[(String, String)], known: &[(String, String)]) {
    let mut q = SUGGESTIONS.lock().unwrap();
    merge_suggestions(&mut q, changes, known);
}

fn merge_suggestions(q: &mut Vec<(String, String)>, changes: &[(String, String)], known: &[(String, String)]) {
    for (from, to) in changes {
        let dup = |(f, _): &(String, String)| f.eq_ignore_ascii_case(from);
        if from.trim().is_empty() || q.iter().any(dup) || known.iter().any(dup) {
            continue;
        }
        if q.len() >= MAX_SUGGESTIONS {
            q.remove(0);
        }
        q.push((from.clone(), to.clone()));
    }
}

pub fn pending_suggestions() -> Vec<(String, String)> {
    SUGGESTIONS.lock().unwrap().clone()
}

/// Remove a suggestion (accepted or dismissed); returns it if it was queued.
pub fn take_suggestion(from: &str) -> Option<(String, String)> {
    let mut q = SUGGESTIONS.lock().unwrap();
    let i = q.iter().position(|(f, _)| f.eq_ignore_ascii_case(from))?;
    Some(q.remove(i))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(name: &str, kind: &'static str, repo: &str, branch: &str) -> TargetCandidate {
        TargetCandidate {
            name: name.into(),
            kind,
            repo: repo.into(),
            path: String::new(),
            branch: branch.into(),
            ts: String::new(),
            live: false,
        }
    }
    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn catalog_priority_dedupe_and_branch_cleaning() {
        let c = [
            cand("findy", "repo", "findy", "main"),
            cand("surabaya", "worktree", "findy", "feature/auth"),
            cand("tyler", "worktree", "Findy", "orb/issue-25-ajouter-un-dictionnaire-personnel"),
        ];
        assert_eq!(catalog_terms(&c), s(&["findy", "surabaya", "tyler", "auth"]));
        assert!(catalog_terms(&[]).is_empty());
    }

    #[test]
    fn effective_vocab_user_first_dedupe_and_cap() {
        let v = effective_vocab(&s(&["Tauri", "tauri", " "]), &s(&["TAURI", "findy"]));
        assert_eq!(v, s(&["Tauri", "findy"]));
        assert_eq!(effective_vocab(&s(&["x"]), &[]), s(&["x"]));
        let big: Vec<String> = (0..200).map(|i| format!("project{i}")).collect();
        let v = effective_vocab(&s(&["mine"]), &big);
        assert_eq!(v[0], "mine");
        assert!(v.join(", ").len() <= MAX_VOCAB_BYTES);
        // user words are never dropped by the cap
        let users: Vec<String> = (0..100).map(|i| format!("userword{i}")).collect();
        assert_eq!(effective_vocab(&users, &s(&["findy"])).len(), 100);
    }

    #[test]
    fn assistant_prompt_has_defaults_and_names() {
        let p = assistant_prompt(true, &s(&["findy", "surabaya"])).unwrap();
        assert!(p.contains("worktree") && p.contains("findy, surabaya"));
        assert!(assistant_prompt(false, &s(&["findy"])).unwrap().contains("Orca"));
        assert!(p.len() <= 800);
    }

    #[test]
    fn assistant_prompt_is_none_without_catalog() {
        // None → the daemon keeps its default prompt: no behaviour change.
        assert_eq!(assistant_prompt(true, &[]), None);
        assert_eq!(assistant_prompt(false, &[]), None);
    }

    /// The Rust defaults must equal `INITIAL_PROMPT_*` in vox_stt.py.
    #[test]
    fn default_prompts_match_daemon() {
        let py = include_str!("../resources/vox_stt.py");
        let grab = |name: &str| -> String {
            let start = py.find(&format!("{name} = (")).expect("prompt in vox_stt.py");
            let block = &py[start..start + py[start..].find("\n)").unwrap()];
            block.split('"').skip(1).step_by(2).collect::<String>()
        };
        assert_eq!(grab("INITIAL_PROMPT_FR"), ASSISTANT_PROMPT_FR);
        assert_eq!(grab("INITIAL_PROMPT_EN"), ASSISTANT_PROMPT_EN);
    }

    #[test]
    fn fuzzy_terms_exclude_branches_and_short_names() {
        let c = [
            cand("findy", "repo", "findy", "main"),
            cand("tyler", "worktree", "findy", "feature/auth"),
            cand("vox", "repo", "vox", "docs/feat"),
        ];
        let t = fuzzy_terms(&c);
        assert_eq!(t, s(&["findy", "tyler"]));
        // free prose is never rewritten into a branch or a short name
        assert_eq!(correct_transcript("c'est auto, la feat est ok", &t), "c'est auto, la feat est ok");
        assert_eq!(correct_transcript("c'est auto", &s(&["auth"])), "c'est auto");
    }

    #[test]
    fn fuzzy_corrects_near_misses() {
        let t = s(&["findy", "surabaya"]);
        assert_eq!(correct_transcript("lance un agent sur findi", &t), "lance un agent sur findy");
        assert_eq!(correct_transcript("lance sur fin di pour les tests", &t), "lance sur findy pour les tests");
        assert_eq!(correct_transcript("va sur Surabaia.", &t), "va sur surabaya.");
    }

    #[test]
    fn fuzzy_is_strict_and_idempotent() {
        let t = s(&["findy", "vox", "surabaya"]);
        // short names, common words, unrelated words: untouched
        assert_eq!(correct_transcript("la box est fine, find it", &t), "la box est fine, find it");
        assert_eq!(correct_transcript("vox", &t), "vox");
        // word boundaries: a longer word containing the name is untouched
        assert_eq!(correct_transcript("findings and finder", &t), "findings and finder");
        // already correct (any case) is untouched
        assert_eq!(correct_transcript("sur Findy", &t), "sur Findy");
        let once = correct_transcript("sur findi", &t);
        assert_eq!(correct_transcript(&once, &t), once);
        // ambiguous: two names equally close → no guess
        assert_eq!(correct_transcript("sur tylar", &s(&["tyler", "tylor"])), "sur tylar");
        assert_eq!(correct_transcript("findi", &[]), "findi");
    }

    #[test]
    fn changes_report_heard_and_name() {
        let (out, ch) = correct_with_changes("lance sur findi", &s(&["findy"]));
        assert_eq!(out, "lance sur findy");
        assert_eq!(ch, vec![("findi".to_string(), "findy".to_string())]);
        let (_, none) = correct_with_changes("lance sur findy", &s(&["findy"]));
        assert!(none.is_empty());
    }

    #[test]
    fn suggestions_dedupe_cap_and_skip_known() {
        let mut q = Vec::new();
        let pair = |a: &str, b: &str| (a.to_string(), b.to_string());
        merge_suggestions(&mut q, &[pair("findi", "findy"), pair("FINDI", "findy")], &[]);
        assert_eq!(q.len(), 1);
        merge_suggestions(&mut q, &[pair("tauri", "Tauri")], &[pair("tauri", "Tauri")]);
        assert_eq!(q.len(), 1);
        for i in 0..8 {
            merge_suggestions(&mut q, &[pair(&format!("w{i}"), "x")], &[]);
        }
        assert_eq!(q.len(), MAX_SUGGESTIONS);
        assert_eq!(q.last().unwrap().0, "w7");
    }
}
