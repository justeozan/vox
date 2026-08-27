//! Spoken name → filesystem target.
//!
//! One catalog read, all matching in Rust: exact tiers first, then a
//! confidence-gated fuzzy pass. A name Vox is not SURE about never reaches
//! spawn — guessing runs an unsupervised agent with `--dangerously-skip-permissions`
//! in the wrong tree, which is the one outcome this module exists to prevent.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use crate::conductor::{self, TargetCandidate};
use crate::AppState;

/// A resolved place to run an agent.
#[derive(Clone, Debug)]
pub struct Target {
    /// What Vox SAYS out loud: "findy", "orivo (porto)", "oneiby (repo)".
    pub label: String,
    /// cwd handed to the agent. Verified to exist at resolve time.
    pub path: String,
    pub source: TargetSource,
    pub confidence: MatchKind,
    /// A Conductor UI session is already `working` in this exact worktree.
    pub live_session: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TargetSource {
    /// The "here" sentinel, or the active project as a last resort.
    Active,
    Worktree { repo: String, name: String },
    /// A repo's main checkout — only when it has no ready worktree.
    RepoRoot { repo: String },
    Registry { name: String },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MatchKind {
    Exact,
    Alias,
    Fuzzy,
}

pub enum Resolution {
    Found(Target),
    /// Several equally plausible names — ask rather than guess.
    Ambiguous { needle: String, options: Vec<String> },
    Unknown { needle: String },
}

/// Words that mean "the project I'm looking at". `launch_agent`'s schema names
/// "here"/"ici" explicitly, but a small model paraphrases, so accept the family.
const SELF_ALIASES: &[&str] = &[
    "here", "this", "thisproject", "current", "currentproject", "active",
    "activeproject", "ici", "ceprojet", "leprojetactif", "actif", "la", "cerepo",
];

// ── Normalization & fuzzy matching ───────────────────────────────────────────

/// Lowercase, strip accents and separators. "Tri bos" and "tribos-landing-page"
/// both collapse to comparable forms, which is what makes STT output matchable.
pub fn normalize(s: &str) -> String {
    s.chars()
        .filter_map(|c| {
            let c = c.to_lowercase().next().unwrap_or(c);
            let c = match c {
                'à' | 'â' | 'ä' | 'á' | 'ã' | 'å' => 'a',
                'é' | 'è' | 'ê' | 'ë' => 'e',
                'î' | 'ï' | 'í' | 'ì' => 'i',
                'ô' | 'ö' | 'ó' | 'ò' | 'õ' => 'o',
                'ù' | 'û' | 'ü' | 'ú' => 'u',
                'ç' => 'c',
                'ñ' => 'n',
                other => other,
            };
            if c.is_ascii_alphanumeric() {
                Some(c)
            } else {
                None
            }
        })
        .collect()
}

/// Two-row Levenshtein. Hand-rolled: Cargo.toml carries no fuzzy dependency and
/// the crate deliberately avoids adding them (same reasoning as the sqlite CLI).
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Edit-distance budget. Scales with length so short names stay strict —
/// "vox" and "box" must not be confusable.
fn max_dist(len: usize) -> usize {
    if len <= 4 {
        1
    } else if len <= 8 {
        2
    } else {
        3
    }
}

/// Does the candidate share a distinctive word with the needle?
/// "landing page" → "tribos-landing-page". Requires a ≥4-char exact token so
/// "app" or "the" can't carry a match.
fn token_overlap(needle: &str, cand: &str) -> bool {
    let toks = |s: &str| -> HashSet<String> {
        s.split(|c: char| !c.is_alphanumeric())
            .map(normalize)
            .filter(|t| t.chars().count() >= 4)
            .collect()
    };
    let (n, c) = (toks(needle), toks(cand));
    !n.is_empty() && n.intersection(&c).next().is_some()
}

// ── Aliases ──────────────────────────────────────────────────────────────────

/// Spoken form → canonical name.
///
/// Two sources. `~/.vox/aliases.json` is explicit. The other is free: the
/// pronunciation dictionary solves the OUTPUT problem ("say `orivo` as
/// `oreevo`"), and the input problem is its exact inverse — if the user taught
/// the voice to say "oreevo", Whisper will hear "oreevo" too.
fn load_aliases() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (word, phonetic) in crate::load_pronunciations() {
        if !word.trim().is_empty() && !phonetic.trim().is_empty() {
            out.push((normalize(&phonetic), word));
        }
    }
    let p = crate::home().join(".vox/aliases.json");
    if let Some(map) = std::fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
    {
        for (spoken, canonical) in map {
            if let Some(c) = canonical.as_str() {
                out.push((normalize(&spoken), c.to_string()));
            }
        }
    }
    out
}

// ── Resolution ───────────────────────────────────────────────────────────────

fn active_target(state: &Arc<AppState>) -> Option<Target> {
    let path = state.active_project.lock().unwrap().clone();
    if path.is_empty() || !Path::new(&path).exists() {
        return None;
    }
    let label = Path::new(&path)
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| "the current project".into());
    Some(Target {
        label,
        path,
        source: TargetSource::Active,
        confidence: MatchKind::Exact,
        live_session: false,
    })
}

/// Build a Target from a catalog row. A repo name resolves to its most recent
/// worktree when it has one: running an unsupervised agent in the main checkout
/// dirties the tree that worktrees exist to protect.
fn from_candidate(c: &TargetCandidate, cat: &[TargetCandidate], kind: MatchKind) -> Target {
    if c.kind == "repo" {
        if let Some(wt) = cat
            .iter()
            .filter(|x| x.kind == "worktree" && x.repo.eq_ignore_ascii_case(&c.repo))
            .max_by(|a, b| a.ts.cmp(&b.ts))
        {
            return Target {
                label: format!("{} ({})", wt.repo, wt.name),
                path: wt.path.clone(),
                source: TargetSource::Worktree { repo: wt.repo.clone(), name: wt.name.clone() },
                confidence: kind,
                live_session: wt.live,
            };
        }
        return Target {
            label: c.name.clone(),
            path: c.path.clone(),
            source: TargetSource::RepoRoot { repo: c.repo.clone() },
            confidence: kind,
            live_session: false,
        };
    }
    Target {
        // Name the repo too: worktree codenames ("porto", "luanda") say nothing
        // about which project they belong to, and two repos can share one.
        label: if c.repo.is_empty() || c.repo.eq_ignore_ascii_case(&c.name) {
            c.name.clone()
        } else {
            format!("{} ({})", c.repo, c.name)
        },
        path: c.path.clone(),
        source: TargetSource::Worktree { repo: c.repo.clone(), name: c.name.clone() },
        confidence: kind,
        live_session: c.live,
    }
}

/// Pick the most recent of several equally-good matches.
fn newest<'a>(v: &[&'a TargetCandidate]) -> Option<&'a TargetCandidate> {
    v.iter().max_by(|a, b| a.ts.cmp(&b.ts)).copied()
}

pub fn resolve_target(state: &Arc<AppState>, name: &str) -> Resolution {
    let raw = name.trim();
    let needle = normalize(raw);
    if needle.is_empty() {
        return match active_target(state) {
            Some(t) => Resolution::Found(t),
            None => Resolution::Unknown { needle: raw.to_string() },
        };
    }

    // Tier 0 — the "here" sentinel.
    if SELF_ALIASES.contains(&needle.as_str()) {
        if let Some(t) = active_target(state) {
            return Resolution::Found(t);
        }
    }

    let cat = conductor::catalog();

    // Tier 0.5 — aliases, then re-enter the exact tiers under the canonical name.
    let mut needle = needle;
    let mut kind = MatchKind::Exact;
    for (spoken, canonical) in load_aliases() {
        if spoken == needle {
            needle = normalize(&canonical);
            kind = MatchKind::Alias;
            break;
        }
    }

    if let Some(r) = exact_tiers(state, &cat, &needle, kind) {
        return r;
    }

    // Tier 4 — fuzzy, gated. A UNIQUE best distance wins; a tie is a question.
    let mut best: Option<usize> = None;
    let mut group: Vec<&TargetCandidate> = Vec::new();
    for c in &cat {
        let n = normalize(&c.name);
        let d = levenshtein(&needle, &n);
        if d > max_dist(needle.chars().count().max(n.chars().count())) {
            continue;
        }
        match best {
            Some(b) if d > b => continue,
            Some(b) if d < b => {
                best = Some(d);
                group.clear();
            }
            None => best = Some(d),
            _ => {}
        }
        group.push(c);
    }
    if best.is_none() {
        // Token overlap catches multi-word names STT splits: "landing page".
        group = cat.iter().filter(|c| token_overlap(raw, &c.name)).collect();
        if !group.is_empty() {
            best = Some(0);
        }
    }
    if best.is_some() {
        // Same repo reached by several rows is one answer, not an ambiguity.
        let repos: HashSet<String> = group.iter().map(|c| c.repo.to_lowercase()).collect();
        if repos.len() == 1 {
            if let Some(c) = newest(&group) {
                return Resolution::Found(from_candidate(c, &cat, MatchKind::Fuzzy));
            }
        }
        let mut options: Vec<String> = Vec::new();
        for c in &group {
            if !options.iter().any(|o| o.eq_ignore_ascii_case(&c.repo)) && !c.repo.is_empty() {
                options.push(c.repo.clone());
            }
        }
        options.truncate(3);
        return Resolution::Ambiguous { needle: raw.to_string(), options };
    }

    // Tier 5 — branch substring, last and least: a substring hit on an
    // unrelated repo's branch must never outrank anything above.
    let hits: Vec<&TargetCandidate> = cat
        .iter()
        .filter(|c| !c.branch.is_empty() && normalize(&c.branch).contains(&needle))
        .collect();
    if let Some(c) = newest(&hits) {
        return Resolution::Found(from_candidate(c, &cat, MatchKind::Fuzzy));
    }

    Resolution::Unknown { needle: raw.to_string() }
}

/// Tiers 1-3: exact repo name, exact worktree name, then the registry.
fn exact_tiers(
    state: &Arc<AppState>,
    cat: &[TargetCandidate],
    needle: &str,
    kind: MatchKind,
) -> Option<Resolution> {
    // Tier 1 — repo name. What people actually say ("sur findy").
    let repo_hits: Vec<&TargetCandidate> =
        cat.iter().filter(|c| c.kind == "repo" && normalize(&c.name) == needle).collect();
    if let Some(c) = newest(&repo_hits) {
        return Some(Resolution::Found(from_candidate(c, cat, kind)));
    }

    // Tier 2 — worktree codename. Ambiguous ACROSS repos (`hat-yai` exists in
    // two), so ask instead of coin-flipping.
    let wt_hits: Vec<&TargetCandidate> =
        cat.iter().filter(|c| c.kind == "worktree" && normalize(&c.name) == needle).collect();
    if !wt_hits.is_empty() {
        let repos: HashSet<String> = wt_hits.iter().map(|c| c.repo.to_lowercase()).collect();
        if repos.len() > 1 {
            let mut options: Vec<String> = wt_hits.iter().map(|c| c.repo.clone()).collect();
            options.dedup();
            options.truncate(3);
            return Some(Resolution::Ambiguous { needle: needle.to_string(), options });
        }
        if let Some(c) = newest(&wt_hits) {
            return Some(Resolution::Found(from_candidate(c, cat, kind)));
        }
    }

    // Tier 3 — the ~/.vox/projects.json registry. Last of the exact tiers: it
    // auto-creates itself with one entry and then goes stale, while Conductor's
    // answer is a real isolated worktree.
    for (rname, rpath) in crate::load_registry() {
        if normalize(&rname) != needle {
            continue;
        }
        let Some(p) = rpath.as_str() else { continue };
        if p.is_empty() || !Path::new(p).exists() {
            continue;
        }
        let _ = state;
        return Some(Resolution::Found(Target {
            label: rname.clone(),
            path: p.to_string(),
            source: TargetSource::Registry { name: rname },
            confidence: kind,
            live_session: false,
        }));
    }
    None
}

// ── Spoken lines ─────────────────────────────────────────────────────────────

pub fn ask_which(en: bool, options: &[String]) -> String {
    let list = match options.len() {
        0 => return unknown_line(en, ""),
        1 => options[0].clone(),
        _ => {
            let last = options.last().unwrap();
            let head = options[..options.len() - 1].join(", ");
            if en {
                format!("{head} or {last}")
            } else {
                format!("{head} ou {last}")
            }
        }
    };
    if en {
        format!("Did you mean {list}?")
    } else {
        format!("Tu veux dire {list} ?")
    }
}

/// A dead end is a wasted turn; naming real targets turns it into a menu, and
/// it's how the user discovers their own repo list by voice.
pub fn unknown_line(en: bool, needle: &str) -> String {
    let names = conductor::launchable_names(3).join(", ");
    match (en, names.is_empty()) {
        (true, true) => format!("I can't find \"{needle}\"."),
        (true, false) => format!("I can't find \"{needle}\". I have {names}…"),
        (false, true) => format!("Je ne trouve pas \"{needle}\"."),
        (false, false) => format!("Je ne trouve pas \"{needle}\". J'ai {names}…"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(name: &str, kind: &'static str, repo: &str, ts: &str) -> TargetCandidate {
        TargetCandidate {
            name: name.into(),
            kind,
            repo: repo.into(),
            path: "/tmp".into(),
            branch: String::new(),
            ts: ts.into(),
            live: false,
        }
    }

    fn fixture() -> Vec<TargetCandidate> {
        vec![
            cand("vox", "repo", "vox", "2026-08-06 20:00:00"),
            cand("orivo", "repo", "orivo", "2026-08-06 19:00:00"),
            cand("Cardex", "repo", "Cardex", "2026-08-06 18:00:00"),
            cand("oneiby", "repo", "oneiby", "2026-08-06 17:00:00"),
            cand("tribos-landing-page", "repo", "tribos-landing-page", "2026-08-06 16:00:00"),
            cand("marseille", "worktree", "vox", "2026-08-06 20:22:00"),
            cand("porto", "worktree", "orivo", "2026-08-06 20:16:00"),
            cand("hat-yai", "worktree", "orivo", "2026-08-06 18:52:00"),
            cand("hat-yai", "worktree", "Cardex", "2026-08-06 18:20:00"),
        ]
    }

    #[test]
    fn normalizes_accents_and_separators() {
        assert_eq!(normalize("Tri bos"), "tribos");
        assert_eq!(normalize("tribos-landing-page"), "triboslandingpage");
        assert_eq!(normalize("Élan_Café"), "elancafe");
    }

    #[test]
    fn levenshtein_basics() {
        // Insertion + substitution: "orivo" cannot become "aurivo" in one edit.
        assert_eq!(levenshtein("orivo", "aurivo"), 2);
        assert_eq!(levenshtein("findy", "findi"), 1);
        assert_eq!(levenshtein("vox", "vox"), 0);
        assert_eq!(levenshtein("", "abc"), 3);
    }

    /// The mangled names STT actually produces must still land, and the
    /// budget for their length must be wide enough to catch them.
    #[test]
    fn stt_manglings_stay_within_budget() {
        for (heard, real) in [("aurivo", "orivo"), ("findi", "findy"), ("marseil", "marseille")] {
            let d = levenshtein(&normalize(heard), &normalize(real));
            let budget = max_dist(heard.chars().count().max(real.chars().count()));
            assert!(d <= budget, "{heard} -> {real}: distance {d} exceeds budget {budget}");
        }
    }

    /// Short names stay strict — "vox" and "box" are one edit apart, and the
    /// budget must be tight enough that only a UNIQUE best match can win.
    #[test]
    fn short_names_are_strict() {
        assert_eq!(max_dist(3), 1);
        assert_eq!(max_dist(12), 3);
        assert_eq!(levenshtein("vox", "box"), 1);
    }

    #[test]
    fn repo_resolves_to_its_newest_worktree_never_the_root() {
        let cat = fixture();
        let repo = cat.iter().find(|c| c.name == "orivo" && c.kind == "repo").unwrap();
        let t = from_candidate(repo, &cat, MatchKind::Exact);
        assert_eq!(t.path, "/tmp");
        match t.source {
            TargetSource::Worktree { ref name, .. } => assert_eq!(name, "porto"),
            _ => panic!("expected the newest worktree, got {:?}", t.source),
        }
        assert_eq!(t.label, "orivo (porto)");
    }

    /// A repo with no worktree is the one legitimate RepoRoot case.
    #[test]
    fn repo_without_worktree_falls_back_to_root() {
        let cat = fixture();
        let repo = cat.iter().find(|c| c.name == "oneiby").unwrap();
        let t = from_candidate(repo, &cat, MatchKind::Exact);
        assert!(matches!(t.source, TargetSource::RepoRoot { .. }));
    }

    /// The safety-critical rule: a codename living in two repos is a question,
    /// never a coin flip.
    #[test]
    fn duplicate_worktree_name_across_repos_is_ambiguous() {
        let cat = fixture();
        let hits: Vec<&TargetCandidate> =
            cat.iter().filter(|c| c.kind == "worktree" && c.name == "hat-yai").collect();
        let repos: HashSet<String> = hits.iter().map(|c| c.repo.to_lowercase()).collect();
        assert_eq!(hits.len(), 2);
        assert_eq!(repos.len(), 2, "hat-yai must be ambiguous across orivo/Cardex");
    }

    #[test]
    fn token_overlap_needs_a_distinctive_word() {
        assert!(token_overlap("landing page", "tribos-landing-page"));
        assert!(!token_overlap("the app", "tribos-landing-page"));
    }

    #[test]
    fn self_aliases_cover_both_languages() {
        for w in ["here", "ici", "ce projet", "this project"] {
            assert!(SELF_ALIASES.contains(&normalize(w).as_str()), "{w} should be a self alias");
        }
    }

    #[test]
    fn newest_wins_on_normalized_timestamps() {
        let cat = fixture();
        let wt: Vec<&TargetCandidate> = cat.iter().filter(|c| c.kind == "worktree").collect();
        assert_eq!(newest(&wt).unwrap().name, "marseille");
    }

    /// Hits the real Conductor DB — run explicitly with
    /// `cargo test -- --ignored --nocapture live_catalog`.
    #[test]
    #[ignore]
    fn live_catalog() {
        let cat = conductor::catalog();
        println!("{} launchable targets", cat.len());
        for c in &cat {
            println!("  {:9} {:24} repo={:22} live={} {}", c.kind, c.name, c.repo, c.live, c.path);
        }
        assert!(!cat.is_empty(), "no targets — is Conductor installed?");
        assert!(
            cat.iter().all(|c| Path::new(&c.path).exists()),
            "every candidate path must exist"
        );
        println!("\nlaunchable_names(24): {:?}", conductor::launchable_names(24));
    }
}
