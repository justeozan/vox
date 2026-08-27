//! The chat loop. Attempt 1 streams the native tools API (qwen2.5, llama3, GPT…)
//! and speaks sentence-by-sentence while the model generates; attempt 2 falls
//! back to a JSON-object prompt any model can follow (gemma3…).
//!
//! Which LLM answers is chosen in `brain.rs`. Subscription CLIs (`claude`,
//! `codex`) have no tools API at all, so they SKIP attempt 1 and go straight to
//! the JSON path — the same machinery weak local models already needed.

use std::collections::BTreeMap;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use regex::Regex;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

use crate::brain::{self, Brain, Transport};
use crate::speech::{self, SpeechItem};
use crate::{load_registry, AppState};


fn vox_tools(en: bool) -> Value {
    // Descriptions follow the UI language so the model isn't nudged toward
    // French `text` replies in English mode.
    let tool = |name: &str, desc: &str, props: Value, req: Value| {
        json!({
            "type": "function",
            "function": {
                "name": name,
                "description": desc,
                "parameters": { "type": "object", "properties": props, "required": req }
            }
        })
    };
    // ONE launch tool, not two. `launch_agent` (active project) and
    // `prompt_worktree` (named worktree) were the same act differing by a
    // prepositional phrase, and a 3B asked to "launch an agent on findy" would
    // routinely pick the active-project one — wrong cwd, silent failure,
    // confident lie. A single tool makes that whole class impossible.
    //
    // `target` is REQUIRED with an explicit sentinel rather than optional: a
    // small model handles absent-means-default badly (it either always omits
    // the key or hallucinates a value), but handles a required string whose
    // description names the fallback token very well.
    if en {
        json!([
            tool(
                "launch_agent",
                "Launch a Claude coding agent in a repository to do a development task in the background. Use this whenever the user asks to launch, start, run, or send work to an agent — on the current project or on any other repo.",
                json!({
                    "target": { "type": "string", "description": "Name of the repo or worktree to work in, exactly as listed in LAUNCHABLE TARGETS. Use \"here\" for the active project." },
                    "task": { "type": "string", "description": "What the user wants done, in one or two sentences, in their own words" },
                    "text": { "type": "string", "description": "Short spoken reply in English (one sentence)" }
                }),
                json!(["target", "task", "text"])
            ),
            tool(
                "switch_project",
                "Change which project is ACTIVE (does not launch anything). Accepts any name from LAUNCHABLE TARGETS or the ~/.vox/projects.json registry.",
                json!({
                    "name": { "type": "string", "description": "Repo, worktree, or registry name" },
                    "text": { "type": "string", "description": "Short spoken reply in English" }
                }),
                json!(["name", "text"])
            ),
        ])
    } else {
        json!([
            tool(
                "launch_agent",
                "Lance un agent Claude dans un dépôt pour une tâche de développement en arrière-plan. À utiliser dès que l'utilisateur demande de lancer, démarrer, ou envoyer du travail à un agent — sur le projet courant ou sur n'importe quel autre dépôt.",
                json!({
                    "target": { "type": "string", "description": "Nom du dépôt ou du worktree où travailler, exactement comme listé dans CIBLES DISPONIBLES. Mets \"ici\" pour le projet actif." },
                    "task": { "type": "string", "description": "Ce que l'utilisateur veut faire, en une ou deux phrases, avec ses mots" },
                    "text": { "type": "string", "description": "Réponse vocale courte en français (une phrase)" }
                }),
                json!(["target", "task", "text"])
            ),
            tool(
                "switch_project",
                "Change le projet ACTIF (ne lance rien). Accepte n'importe quel nom des CIBLES DISPONIBLES ou du registre ~/.vox/projects.json.",
                json!({
                    "name": { "type": "string", "description": "Nom de dépôt, de worktree, ou du registre" },
                    "text": { "type": "string", "description": "Réponse vocale courte" }
                }),
                json!(["name", "text"])
            ),
        ])
    }
}

fn post_chat(brain: &Brain, body: &Value, timeout_secs: u64) -> Result<Value, String> {
    brain.post(body, timeout_secs)
}

/// Stream an SSE chat completion, invoking `on_delta` with each
/// `choices[0].delta` object. `cancel` is polled per line so callers can
/// abort a stream promptly (e.g. ⌥Space during the recap). Errors ONLY if
/// the endpoint or stream fails before any data arrives — a mid-stream cut
/// after data was already delivered (including ureq's total-read timeout on
/// slow generations) returns Ok with whatever was streamed, since the spoken
/// sentences cannot be unsaid.
fn stream_chat(
    brain: &Brain,
    body: &Value,
    timeout_secs: u64,
    cancel: &dyn Fn() -> bool,
    mut on_delta: impl FnMut(&Value),
) -> Result<(), String> {
    use std::io::{BufRead, BufReader};
    let mut req = ureq::post(brain.url).timeout(Duration::from_secs(timeout_secs));
    if let Some(k) = brain.api_key() {
        req = req.set("Authorization", &format!("Bearer {k}"));
    }
    let resp = req.send_json(body.clone()).map_err(|e| e.to_string())?;
    let mut saw_data = false;
    for line in BufReader::new(resp.into_reader()).lines() {
        if cancel() {
            break;
        }
        let line = match line {
            Ok(l) => l,
            Err(_) if saw_data => break,
            Err(e) => return Err(e.to_string()),
        };
        let line = line.trim();
        let Some(data) = line.strip_prefix("data:") else { continue };
        let data = data.trim();
        if data == "[DONE]" {
            break;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
        saw_data = true;
        on_delta(&v["choices"][0]["delta"]);
    }
    if saw_data {
        Ok(())
    } else {
        Err("no stream data".into())
    }
}

/// One-shot chat helper (no history, no tools).
pub fn chat_once(model: &str, system: &str, user: &str, max_tokens: u32) -> Option<String> {
    // `model` is kept as the parameter for the existing call sites; the
    // provider comes from settings via brain_for().
    let b = brain_from_model(model);
    let text = b.complete(system, user, max_tokens)?;
    Some(text.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
}

/// Resolve a Brain when only a model id is at hand. The provider is global, so
/// this reads it from the persisted settings file rather than threading state
/// through every call site.
fn brain_from_model(model: &str) -> Brain {
    brain::resolve(&crate::persisted_provider(), model)
}

/// Streamed one-shot chat: `on_sentence` fires as each sentence completes,
/// so TTS can start before generation finishes. `cancel` aborts the stream
/// (and skips the fallback). Falls back to the blocking call ONLY when
/// nothing was emitted — a partial stream must not be replayed from the top.
/// Returns the full text.
pub fn chat_once_stream(
    model: &str,
    system: &str,
    user: &str,
    max_tokens: u32,
    cancel: &dyn Fn() -> bool,
    mut on_sentence: impl FnMut(String),
) -> Option<String> {
    let body = json!({
        "model": model,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user }
        ],
        "max_tokens": max_tokens,
        "stream": true
    });
    let mut sbuf = speech::SentenceBuffer::new();
    let mut full = String::new();
    let mut emitted = 0usize;
    let b = brain_from_model(model);
    // A subscription CLI can't stream — one blocking call, split afterwards.
    if b.transport == Transport::Cli {
        let text = b.complete(system, user, max_tokens)?;
        for sentence in speech::split_sentences(&text) {
            if cancel() {
                break;
            }
            on_sentence(sentence);
        }
        return Some(text);
    }
    let res = stream_chat(&b, &body, 90, cancel, |delta| {
        if let Some(c) = delta.get("content").and_then(|v| v.as_str()) {
            full.push_str(c);
            for s in sbuf.push(c) {
                emitted += 1;
                on_sentence(s);
            }
        }
    });
    if res.is_err() {
        if cancel() || emitted > 0 {
            // Already spoke part of it (or was cancelled) — never replay.
            return if full.trim().is_empty() { None } else { Some(full) };
        }
        let text = chat_once(model, system, user, max_tokens)?;
        for s in speech::split_sentences(&text) {
            on_sentence(s);
        }
        return Some(text);
    }
    if !cancel() {
        if let Some(rest) = sbuf.flush() {
            on_sentence(rest);
        }
    }
    let full = full.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
    if full.is_empty() {
        None
    } else {
        Some(full)
    }
}

// ── Action execution ─────────────────────────────────────────────────────────

/// Execute one tool action; returns an override voice line (e.g. unknown
/// project) or None to keep the model-provided text.
/// What an action did, from the point of view of what should be SAID.
///
/// Three states, not two: resolution can succeed while the model's spoken line
/// is still wrong (it named one repo, we resolved another; there's already an
/// agent there). That's neither "say the model's line" nor "the action failed".
pub enum ActionOutcome {
    /// Nothing to add — speak the model's `text`.
    Ok,
    /// Succeeded, but the model's line is imprecise or incomplete. Outranks it.
    Amend(String),
    /// FAILED. Speaking the model's confirmation would be a lie.
    Failed(String),
}

impl ActionOutcome {
    /// The line that must replace the model's `text`, if any.
    fn line(self) -> Option<String> {
        match self {
            ActionOutcome::Ok => None,
            ActionOutcome::Amend(s) | ActionOutcome::Failed(s) => Some(s),
        }
    }
}

fn apply_action(
    app: &AppHandle,
    state: &Arc<AppState>,
    registry: &serde_json::Map<String, Value>,
    name: &str,
    args: &Value,
) -> ActionOutcome {
    use crate::targets::{self, Resolution};
    let en = state.settings.lock().unwrap().language == "en";
    match name {
        // `prompt_worktree` is the pre-merge name: unadvertised, still accepted,
        // because a model can echo it back out of mid-session history.
        "launch_agent" | "prompt_worktree" => {
            let raw_target = args
                .get("target")
                .and_then(|v| v.as_str())
                .or_else(|| args.get("project").and_then(|v| v.as_str()))
                .unwrap_or("here");
            let task = args
                .get("task")
                .and_then(|v| v.as_str())
                .or_else(|| args.get("prompt").and_then(|v| v.as_str()))
                .unwrap_or("")
                .trim();
            if task.is_empty() {
                return ActionOutcome::Failed(if en {
                    "I need to know what the agent should do.".into()
                } else {
                    "Il me faut savoir quoi demander à l'agent.".into()
                });
            }
            let target = match targets::resolve_target(state, raw_target) {
                Resolution::Found(t) => t,
                Resolution::Ambiguous { options, .. } => {
                    return ActionOutcome::Failed(targets::ask_which(en, &options))
                }
                Resolution::Unknown { needle } => {
                    return ActionOutcome::Failed(targets::unknown_line(en, &needle))
                }
            };
            let already = crate::agents::count_in(state, &target.path);
            match crate::agents::launch(app, state, &target, task) {
                Ok(_) => {
                    println!("[vox] agent → {} ({})", target.label, target.path);
                    match amend_line(en, &target, already, args) {
                        Some(l) => ActionOutcome::Amend(l),
                        None => ActionOutcome::Ok,
                    }
                }
                Err(e) => ActionOutcome::Failed(launch_error_line(en, &e, &target.label)),
            }
        }
        "switch_project" => {
            let pname = args.get("name").and_then(|n| n.as_str()).unwrap_or("");
            // Registry first (that's what this tool has always meant), then the
            // full resolver — so "switch to findy" works even for a repo that
            // was never added to projects.json.
            let resolved = registry
                .get(pname)
                .and_then(|v| v.as_str())
                .map(String::from)
                .or_else(|| args.get("path").and_then(|p| p.as_str()).map(String::from))
                .filter(|p| std::path::Path::new(p).exists())
                .or_else(|| match targets::resolve_target(state, pname) {
                    Resolution::Found(t) => Some(t.path),
                    _ => None,
                });
            match resolved {
                Some(p) => {
                    println!("[vox] switched project to: {p}");
                    *state.active_project.lock().unwrap() = p;
                    ActionOutcome::Ok
                }
                // The registry goes stale (a worktree is archived, a folder
                // moves) and nothing used to catch it: spawn() failed later,
                // inside the worker thread, long after this returned — so the
                // user heard a confident "launching" and nothing happened.
                None => ActionOutcome::Failed(targets::unknown_line(en, pname)),
            }
        }
        _ => ActionOutcome::Ok,
    }
}

/// Say the RESOLVED target whenever it isn't already what the model said.
/// The user hears the mistake about a second into a ten-minute task — which is
/// worth more than a confirmation prompt, and costs no extra turn.
fn amend_line(en: bool, target: &crate::targets::Target, already: usize, args: &Value) -> Option<String> {
    use crate::targets::{MatchKind, TargetSource};
    let said = args.get("text").and_then(|t| t.as_str()).unwrap_or("").to_lowercase();
    let label = &target.label;

    if already > 0 {
        let n = already + 1;
        return Some(if en {
            format!("On it — that's {n} agents on {label} now.")
        } else {
            format!("C'est parti, ça fait {n} agents sur {label}.")
        });
    }
    // A headless agent editing files under a live Conductor session is the one
    // genuinely surprising collision. Don't block it — just say it.
    if target.live_session {
        return Some(if en {
            format!("A Conductor agent is already working on {label} — launching anyway.")
        } else {
            format!("L'agent Conductor bosse déjà sur {label}, je lance quand même.")
        });
    }
    if let TargetSource::RepoRoot { .. } = target.source {
        return Some(if en {
            format!("On {label}, straight on the repo — no worktree.")
        } else {
            format!("Sur {label}, direct sur le repo — pas de worktree.")
        });
    }
    // Fuzzy hit, or a repo that resolved to a worktree: name where it actually
    // went if the spoken line doesn't already contain it.
    let bare = label.split(" (").next().unwrap_or(label).to_lowercase();
    if target.confidence != MatchKind::Exact || !said.contains(&bare) {
        return Some(if en {
            format!("Launching on {label}.")
        } else {
            format!("Je lance sur {label}.")
        });
    }
    None
}

fn launch_error_line(en: bool, e: &crate::agents::LaunchError, label: &str) -> String {
    use crate::agents::LaunchError as E;
    match (e, en) {
        (E::NoCli, true) => "Claude Code isn't installed — I can't launch agents.".into(),
        (E::NoCli, false) => "Claude Code n'est pas installé, je ne peux pas lancer d'agent.".into(),
        (E::BadCwd(l), true) => format!("{l}'s folder is gone."),
        (E::BadCwd(l), false) => format!("Le dossier de {l} n'existe plus."),
        // Refuse, never queue: a voice "ok" followed by ten minutes of nothing
        // is worse than an honest no, and the announcements make "wait for one
        // to finish" a zero-effort instruction.
        (E::TooMany(n), true) => format!("{n} agents already running — wait for one to finish."),
        (E::TooMany(n), false) => format!("Déjà {n} agents en route, attends qu'il y en ait un qui finisse."),
        (E::Cancelled, true) => "Cancelled.".into(),
        (E::Cancelled, false) => "Annulé.".into(),
        (E::SpawnFailed(_), true) => format!("The agent didn't start on {label}."),
        (E::SpawnFailed(_), false) => format!("L'agent n'a pas démarré sur {label}."),
    }
}

/// Run every action in a parsed JSON reply (attempt-2 format); returns the
/// voice line to speak.
fn execute_parsed(
    app: &AppHandle,
    state: &Arc<AppState>,
    registry: &serde_json::Map<String, Value>,
    parsed: Value,
) -> String {
    let actions: Vec<Value> = match parsed {
        Value::Array(a) => a,
        v => vec![v],
    };
    let mut text_voice = String::new();
    let mut override_voice = String::new();
    for item in &actions {
        let action = item.get("action").and_then(|a| a.as_str()).unwrap_or("none");
        if action != "none" {
            if let Some(ov) = apply_action(app, state, registry, action, item).line() {
                if override_voice.is_empty() {
                    override_voice = ov;
                }
            }
        }
        if text_voice.is_empty() {
            if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                text_voice = t.to_string();
            }
        }
    }
    // An override means the action FAILED — speaking the model's optimistic
    // confirmation instead would be a lie.
    if !override_voice.is_empty() {
        override_voice
    } else {
        text_voice
    }
}

// ── Prompt drafting ──────────────────────────────────────────────────────────

/// Expand the short spoken task into a real agent prompt.
///
/// Runs on the agent's worker thread, AFTER the spoken confirmation has already
/// gone out, so its latency is invisible. That's also why the tool schema now
/// asks for a one-or-two-sentence `task` instead of a full prompt: a long field
/// inside a tool call under `max_tokens: 300` truncates the JSON, and
/// `parse_json` then suppresses the reply entirely — the user hears nothing at
/// all. Drafting here is unbounded and can use a bigger local model.
pub fn draft_prompt(state: &Arc<AppState>, target_label: &str, task: &str) -> String {
    let (model, en) = {
        let s = state.settings.lock().unwrap();
        (
            std::env::var("VOX_DRAFT_MODEL").unwrap_or_else(|_| s.model.clone()),
            s.language == "en",
        )
    };

    // The user's own words beat a 3B's paraphrase, and they're free.
    let recent: Vec<String> = state
        .history
        .lock()
        .unwrap()
        .iter()
        .rev()
        .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
        .filter_map(|m| m.get("content").and_then(|c| c.as_str()).map(String::from))
        .take(3)
        .collect();
    let quoted = recent
        .iter()
        .rev()
        .map(|l| format!("  \"{}\"", l.chars().take(200).collect::<String>()))
        .collect::<Vec<_>>()
        .join("\n");

    let sys = if en {
        "You turn a developer's spoken request into a precise prompt for a coding agent. Write the prompt itself and nothing else: no preamble, no explanation, no markdown fences. 3 to 6 sentences."
    } else {
        "Tu transformes la demande orale d'un développeur en prompt précis pour un agent de code. Écris le prompt lui-même et rien d'autre : pas de préambule, pas d'explication, pas de balises markdown. 3 à 6 phrases."
    };
    let user = if en {
        format!("Repository: {target_label}\nWhat they asked for: {task}\n\nWrite the prompt.")
    } else {
        format!("Dépôt : {target_label}\nCe qu'ils demandent : {task}\n\nÉcris le prompt.")
    };
    let drafted = chat_once(&model, sys, &user, 500)
        .map(|d| d.trim().to_string())
        .filter(|d| d.chars().count() >= 40)
        .unwrap_or_else(|| task.to_string());

    // The scaffolding is assembled in Rust and is ALWAYS present, whatever the
    // model does — including the closing instruction, which is what makes the
    // agent produce an answer worth reading aloud.
    let ctx = if quoted.is_empty() {
        String::new()
    } else if en {
        format!("\n\nContext from a voice conversation (may be partial — verify against the repo):\n{quoted}")
    } else {
        format!("\n\nContexte issu d'une conversation vocale (possiblement partiel — vérifie dans le repo) :\n{quoted}")
    };

    if en {
        format!(
            "{drafted}{ctx}\n\n\
             Constraints: work only in this repository. Make the smallest change that satisfies \
             the request. Run the project's existing tests if there are any. Do not commit or push \
             unless explicitly asked.\n\n\
             Done when: the change is implemented and the repo builds / tests pass.\n\
             End your final message with a one-sentence summary of what you changed, written to be \
             read aloud."
        )
    } else {
        format!(
            "{drafted}{ctx}\n\n\
             Contraintes : travaille uniquement dans ce dépôt. Fais le plus petit changement qui \
             satisfait la demande. Lance les tests existants s'il y en a. Ne commit ni ne push sans \
             demande explicite.\n\n\
             Terminé quand : le changement est implémenté et le repo build / les tests passent.\n\
             Termine ton message final par une phrase unique résumant ce que tu as changé, écrite \
             pour être lue à voix haute."
        )
    }
}

// ── System prompt ────────────────────────────────────────────────────────────

fn build_system(state: &Arc<AppState>) -> String {
    let (model_lang, active) = {
        let s = state.settings.lock().unwrap();
        (s.language.clone(), state.active_project.lock().unwrap().clone())
    };
    let en = model_lang == "en";
    let active_name = std::path::Path::new(&active)
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or(active);

    // Rich per-workspace state from the Conductor DB. Language-aware
    // descriptors so the model doesn't drift into French in English mode.
    // Six, not ten: this block is the expensive part of the prompt and it now
    // shares room with the launchable-target list. The recap path keeps 10.
    let ws = crate::conductor::read_state(6);
    let mut workspace_context = String::new();
    if !ws.is_empty() {
        let lines = ws
            .iter()
            .map(|w| match &w.session {
                None => format!("- {} : {}", w.project, if en { "no agent" } else { "aucun agent" }),
                Some(s) => {
                    let preview = s
                        .preview
                        .as_deref()
                        .map(crate::conductor::clean_snippet)
                        .unwrap_or_else(|| (if en { "(nothing new)" } else { "(rien de neuf)" }).into());
                    let st = match s.status.as_str() {
                        "working" => if en { "agent working".into() } else { "agent en cours".into() },
                        "error" => if en { "agent ERRORED".into() } else { "agent EN ERREUR".into() },
                        "idle" => if en { "agent idle".into() } else { "agent au repos".into() },
                        other => if en { format!("status {other}") } else { format!("statut {other}") },
                    };
                    let label = if en { "last message" } else { "dernier message" };
                    let ask = s
                        .original_ask
                        .as_deref()
                        .map(|a| {
                            let a: String = a.chars().take(80).collect();
                            if en {
                                format!(" — original ask: \"{a}\"")
                            } else {
                                format!(" — demande initiale : \"{a}\"")
                            }
                        })
                        .unwrap_or_default();
                    format!("- {} ({}, {st}){ask} — {label} : \"{preview}\"", w.project, s.agent)
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let heading = if en {
            "\n\nCurrent Conductor worktree state (use ONLY this data to answer project questions):"
        } else {
            "\n\nÉtat de tes worktrees Conductor (utilise UNIQUEMENT ces données pour répondre aux questions sur un projet) :"
        };
        workspace_context = format!("{heading}\n{lines}");
    }

    // Names the model may TARGET. The state block above only carries
    // in-progress worktrees, so without this the model is forbidden (by the
    // "never invent a project" rule) from aiming at most of the user's repos.
    let names = crate::conductor::launchable_names(24);
    let catalog_block = if names.is_empty() {
        String::new()
    } else {
        let list = names.join(", ");
        if en {
            format!("\n\nLAUNCHABLE TARGETS (you may send an agent to ANY of these):\n{list}")
        } else {
            format!("\n\nCIBLES DISPONIBLES (tu peux envoyer un agent sur N'IMPORTE LAQUELLE) :\n{list}")
        }
    };
    let results_block = crate::announce::results_block(state, en);
    // Running agents FIRST: "where are we at?" is most often about the thing
    // that hasn't finished yet.
    let live_block = crate::agents::status_block(state, en);
    let workspace_context =
        format!("{workspace_context}{catalog_block}{live_block}{results_block}");

    if en {
        format!(
            "You are Vox, a voice AI assistant for a developer running many Conductor worktrees in parallel.\n\
             ABSOLUTE RULES:\n\
             - Reply in ENGLISH ONLY, never in French — even if the context data below is in French\n\
             - Maximum 1 short sentence (15 words max), always\n\
             - No lists, no file paths in the reply\n\
             - When asked about a project, SUMMARIZE the agent's last update in English — the data below may be in French; translate it, never quote French verbatim\n\
             - To judge progress, compare the agent's last message to the original ask in the data\n\
             - To start work anywhere, call launch_agent with the repo or worktree name as 'target' (or \"here\" for the active project); 'task' is one or two sentences in the user's own words\n\
             - The state data answers STATUS questions. To LAUNCH you may target any name in LAUNCHABLE TARGETS, even one with no state shown\n\
             - Never invent a project name, PR, or bug not in the data\n\n\
             Active project: {active_name}{workspace_context}\n\n\
             REMINDER: Answer in English, one short sentence."
        )
    } else {
        format!(
            "Tu es Vox, assistant vocal d'un développeur qui gère plusieurs worktrees Conductor en parallèle.\n\
             RÈGLES ABSOLUES :\n\
             - Toujours en français, jamais en anglais\n\
             - Maximum 1 phrase courte (15 mots max)\n\
             - Pas de liste, pas de chemin de fichier dans la réponse\n\
             - Quand on te demande où en est un projet, cite ce que l'agent a dit en dernier dans les données ci-dessous\n\
             - Pour juger l'avancement, compare le dernier message de l'agent à la demande initiale dans les données\n\
             - Pour lancer du travail où que ce soit, appelle launch_agent avec le nom du dépôt ou du worktree dans 'target' (ou \"ici\" pour le projet actif) ; 'task' fait une ou deux phrases, avec les mots de l'utilisateur\n\
             - Les données d'état servent aux questions de STATUT. Pour LANCER, tu peux viser n'importe quel nom des CIBLES DISPONIBLES, même sans état affiché\n\
             - N'invente jamais un projet, une PR, ou un bug qui n'est pas dans les données\n\n\
             Projet actif : {active_name}{workspace_context}\n\n\
             RAPPEL : Réponds en français, une phrase courte."
        )
    }
}

// ── Main entry ───────────────────────────────────────────────────────────────

/// True when the streamed content is shaping up to be an inline tool call
/// (`switch_project {"name":…}`), bare JSON, or a prose sentence followed by
/// a JSON action blob (the classic weak-model shape) — stop feeding speech
/// from this point on and parse the whole thing at stream end.
fn looks_like_inline_tool(s: &str) -> bool {
    let t = s.trim_start();
    if t.starts_with('{') || t.starts_with("```") || t.starts_with('[') {
        return true;
    }
    // A `{"` anywhere means a JSON object is starting mid-content; real
    // spoken prose essentially never contains one.
    if t.contains("{\"") {
        return true;
    }
    if let Some(pos) = t.find('{') {
        return t[..pos].trim_end().chars().all(|c| c.is_alphanumeric() || c == '_');
    }
    false
}

/// Handle one user utterance end-to-end: query Ollama (streaming when
/// possible), execute tool actions, and SPEAK the reply. Speech starts on the
/// first complete sentence while the model is still generating. Every path
/// ends with a speaking-done (directly or via the speech session).
/// Conversation turns replayed into every Ollama call. Bounded because the
/// whole history is re-sent each turn: unbounded growth silently degrades a 3B
/// model's answers over a long session, then starts costing real latency.
const MAX_HISTORY: usize = 24;

pub fn ask_ollama(app: &AppHandle, state: &Arc<AppState>, transcript: &str) {
    {
        let mut h = state.history.lock().unwrap();
        h.push(json!({ "role": "user", "content": transcript }));
        if h.len() > MAX_HISTORY {
            let drop = h.len() - MAX_HISTORY;
            h.drain(..drop);
        }
    }

    let registry = load_registry();
    let (provider, model_id) = {
        let st = state.settings.lock().unwrap();
        (st.provider.clone(), st.model.clone())
    };
    let brain = brain::resolve(&provider, &model_id);
    // Refuse out loud rather than time out silently: a missing key or an
    // uninstalled CLI is a setup problem the user can fix in one action.
    if let Some(why) = brain.unavailable() {
        let en = state.settings.lock().unwrap().language == "en";
        eprintln!("[vox] provider {provider} unavailable: {why}");
        speech::speak(
            app,
            state,
            &if en {
                format!("I can't reach {provider} — {why}.")
            } else {
                format!("Je ne peux pas utiliser {provider} — {why}.")
            },
        );
        return;
    }
    let base_system = build_system(state);
    // The default model is a 3B: prompt size is the first thing to look at when
    // tool-calling gets flaky. ~4 chars/token is close enough for French.
    println!(
        "[vox] system prompt ~{} tok ({} chars)",
        base_system.chars().count() / 4,
        base_system.chars().count()
    );
    let (model, en) = {
        let s = state.settings.lock().unwrap();
        (s.model.clone(), s.language == "en")
    };
    let history = state.history.lock().unwrap().clone();

    let mut messages = vec![json!({ "role": "system", "content": base_system })];
    messages.extend(history.iter().cloned());

    // A subscription CLI exposes no tools API and cannot stream, so attempt 1
    // is meaningless for it — jump straight to the JSON-object path.
    let skip_tools = brain.transport == Transport::Cli;

    // ── Attempt 1 : native tools API, streamed ──────────────────────────────
    let mut full = String::new();
    let mut sbuf = speech::SentenceBuffer::new();
    let mut session: Option<Sender<SpeechItem>> = None;
    let mut suppressed = false;
    let mut tool_acc: BTreeMap<u64, (String, String)> = BTreeMap::new();

    let stream_res = if skip_tools {
        Err("cli provider: no tools api".to_string())
    } else {
        stream_chat(
        &brain,
        &json!({
            "model": model,
            "messages": messages,
            "tools": vox_tools(en),
            "max_tokens": 300,
            "stream": true
        }),
        120,
        &|| false,
        |delta| {
            if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                for tc in tcs {
                    // Ollama's OpenAI-compat layer has emitted `index` as the
                    // position within EACH chunk (0 for every one-call chunk),
                    // so we can't trust it as identity. If the slot already
                    // holds a complete JSON args object and a new object
                    // starts, that's a NEW call — allocate a fresh slot.
                    let mut idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                    if let Some(a) = tc.pointer("/function/arguments").and_then(|v| v.as_str()) {
                        let complete = tool_acc
                            .get(&idx)
                            .is_some_and(|(_, args)| {
                                !args.is_empty() && serde_json::from_str::<Value>(args).is_ok()
                            });
                        if complete && a.trim_start().starts_with('{') {
                            idx = tool_acc.keys().max().copied().unwrap_or(0) + 1;
                        }
                    }
                    let entry = tool_acc.entry(idx).or_default();
                    if let Some(n) = tc.pointer("/function/name").and_then(|v| v.as_str()) {
                        // Names arrive whole; never concatenate two of them.
                        if entry.0.is_empty() {
                            entry.0.push_str(n);
                        }
                    }
                    if let Some(a) = tc.pointer("/function/arguments").and_then(|v| v.as_str()) {
                        entry.1.push_str(a);
                    }
                }
            }
            if let Some(c) = delta.get("content").and_then(|v| v.as_str()) {
                full.push_str(c);
                if !suppressed && looks_like_inline_tool(&full) {
                    suppressed = true;
                }
                // Speak prose as it streams — but never while a tool call is
                // forming (its `text` arg is the voice line, spoken at the end).
                if !suppressed && tool_acc.is_empty() {
                    for s in sbuf.push(c) {
                        let tx = session.get_or_insert_with(|| {
                            speech::start_session(app.clone(), state.clone())
                        });
                        let _ = tx.send(SpeechItem::Sentence { text: s, workspace: None });
                    }
                }
            }
        },
        )
    };

    match stream_res {
        Ok(()) => {
            // Native tool calls, accumulated from deltas.
            if !tool_acc.is_empty() {
                let mut text_voice = String::new();
                let mut override_voice = String::new();
                for (name, args_raw) in tool_acc.values() {
                    let args: Value = serde_json::from_str(args_raw).unwrap_or(json!({}));
                    if text_voice.is_empty() {
                        if let Some(t) = args.get("text").and_then(|t| t.as_str()) {
                            text_voice = t.to_string();
                        }
                    }
                    if let Some(ov) = apply_action(app, state, &registry, name, &args).line() {
                        if override_voice.is_empty() {
                            override_voice = ov;
                        }
                    }
                }
                if let Some(tx) = session.take() {
                    // Prose already streamed to the voice — close it out, but
                    // an action override (e.g. "unknown worktree") is NEW
                    // information the prose can't have covered: speak it.
                    if let Some(rest) = sbuf.flush() {
                        let _ = tx.send(SpeechItem::Sentence { text: rest, workspace: None });
                    }
                    if !override_voice.is_empty() {
                        let _ = tx.send(SpeechItem::Sentence { text: override_voice.clone(), workspace: None });
                    }
                    let _ = tx.send(SpeechItem::End);
                    state.history.lock().unwrap().push(json!({ "role": "assistant", "content": full }));
                } else {
                    let voice = if !override_voice.is_empty() { override_voice } else { text_voice };
                    state.history.lock().unwrap().push(json!({ "role": "assistant", "content": voice }));
                    if voice.is_empty() {
                        let _ = app.emit("speaking-done", ());
                    } else {
                        speech::speak(app, state, &voice);
                    }
                }
                println!("[vox] tools response executed");
                return;
            }

            let text = full.trim().to_string();

            // Inline text-tool (`switch_project {…}`), bare JSON, or
            // prose-then-JSON content. If some prose already streamed to a
            // session before suppression kicked in, close that session first —
            // the action confirmation will be spoken separately after it.
            if suppressed && !text.is_empty() {
                if let Some(tx) = session.take() {
                    let _ = tx.send(SpeechItem::End);
                }
                let re = Regex::new(r"(?s)^(\w+)\s*(\{.*\})\s*$").unwrap();
                let voice = if let Some(caps) = re.captures(&text) {
                    if let Ok(args) = serde_json::from_str::<Value>(&caps[2]) {
                        let tool_name = caps[1].to_string();
                        let text_v = args
                            .get("text")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string();
                        let override_v = apply_action(app, state, &registry, &tool_name, &args).line();
                        println!("[vox] text-tool response: {tool_name}");
                        // An override outranks the model's optimistic line:
                        // either the action failed, or it landed somewhere the
                        // model didn't name.
                        override_v.unwrap_or(text_v)
                    } else {
                        execute_parsed(app, state, &registry, parse_json(&text))
                    }
                } else {
                    execute_parsed(app, state, &registry, parse_json(&text))
                };
                state
                    .history
                    .lock()
                    .unwrap()
                    .push(json!({ "role": "assistant", "content": voice }));
                if voice.is_empty() {
                    let _ = app.emit("speaking-done", ());
                } else {
                    speech::speak(app, state, &voice);
                }
                return;
            }

            // Plain streamed prose.
            if !text.is_empty() {
                if let Some(rest) = sbuf.flush() {
                    let tx = session.get_or_insert_with(|| {
                        speech::start_session(app.clone(), state.clone())
                    });
                    let _ = tx.send(SpeechItem::Sentence { text: rest, workspace: None });
                }
                if let Some(tx) = session.take() {
                    let _ = tx.send(SpeechItem::End);
                } else {
                    let _ = app.emit("speaking-done", ());
                }
                state.history.lock().unwrap().push(json!({ "role": "assistant", "content": text }));
                println!("[vox] streamed response: {text}");
                return;
            }
            // Empty response — fall through to attempt 2.
        }
        Err(e) => {
            eprintln!("[vox] tools stream failed: {e}");
            // If part of the reply was already spoken, close it out as a
            // truncated answer — running attempt 2 on top would speak a
            // SECOND, differently-worded reply after the first one.
            if session.is_some() || !full.trim().is_empty() {
                if let Some(rest) = sbuf.flush() {
                    let tx = session.get_or_insert_with(|| {
                        speech::start_session(app.clone(), state.clone())
                    });
                    let _ = tx.send(SpeechItem::Sentence { text: rest, workspace: None });
                }
                if let Some(tx) = session.take() {
                    let _ = tx.send(SpeechItem::End);
                }
                state
                    .history
                    .lock()
                    .unwrap()
                    .push(json!({ "role": "assistant", "content": full }));
                return;
            }
            eprintln!("[vox] no data streamed — trying JSON prompt fallback");
        }
    }

    // ── Attempt 2 : JSON prompt fallback ─────────────────────────────────────
    // Instructions AND few-shot examples follow the UI language — otherwise the
    // weak models that need this fallback mirror the French exemplars.
    let json_system = if en {
        format!(
            "{base_system}\n\n\
             ALWAYS reply with a single valid JSON object on one line, with no surrounding text.\n\
             The \"text\" field must be in ENGLISH.\n\
             Examples:\n\
             {{\"action\":\"none\",\"text\":\"Yes, I hear you.\"}}\n\
             {{\"action\":\"launch_agent\",\"target\":\"here\",\"task\":\"Fix the failing unit tests\",\"text\":\"Launching an agent on the tests.\"}}\n\
             {{\"action\":\"launch_agent\",\"target\":\"my-app\",\"task\":\"Fix the login redirect that lands on /404 after OAuth\",\"text\":\"Agent heading out on my-app.\"}}\n\
             {{\"action\":\"switch_project\",\"name\":\"my-app\",\"text\":\"Switching to my-app.\"}}"
        )
    } else {
        format!(
            "{base_system}\n\n\
             Réponds TOUJOURS avec un objet JSON valide sur une seule ligne, sans aucun texte autour.\n\
             Exemples :\n\
             {{\"action\":\"none\",\"text\":\"Oui, je t'entends bien.\"}}\n\
             {{\"action\":\"launch_agent\",\"target\":\"ici\",\"task\":\"Corriger les tests unitaires qui échouent\",\"text\":\"Je lance un agent sur les tests.\"}}\n\
             {{\"action\":\"launch_agent\",\"target\":\"mon-app\",\"task\":\"Corriger la redirection login qui tombe sur /404 après OAuth\",\"text\":\"L'agent part sur mon-app.\"}}\n\
             {{\"action\":\"switch_project\",\"name\":\"mon-app\",\"text\":\"Je passe sur mon-app.\"}}"
        )
    };
    let mut messages2 = vec![json!({ "role": "system", "content": json_system })];
    messages2.extend(state.history.lock().unwrap().iter().cloned());

    // A CLI provider has no HTTP endpoint at all — `post_chat` would POST to an
    // empty URL and fail. Route it through the transport-aware helper instead;
    // this is the ONLY path a subscription CLI ever takes.
    let raw = if brain.transport == Transport::Cli {
        // History is replayed as plain text: the CLI takes one prompt, not a
        // messages array.
        let convo = state
            .history
            .lock()
            .unwrap()
            .iter()
            .rev()
            .take(6)
            .filter_map(|m| {
                let role = m.get("role")?.as_str()?;
                let content = m.get("content")?.as_str()?;
                Some(format!("{role}: {content}"))
            })
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        match brain.complete(&json_system, &convo, 300) {
            Some(t) => t,
            None => {
                eprintln!("[vox] {} produced no reply", brain.id);
                let _ = app.emit("speaking-done", ());
                return;
            }
        }
    } else {
        let data = match post_chat(
            &brain,
            &json!({ "model": model, "messages": messages2, "max_tokens": 300, "stream": false }),
            90,
        ) {
            Ok(d) => d,
            Err(e) => {
                // Both attempts failed. Saying nothing is the worst outcome:
                // the user pressed a key, spoke, and got silence with no idea
                // why. The common cause by far is a model that is configured
                // but not pulled, so name it.
                eprintln!("[vox] {} request failed: {e}", brain.id);
                let missing = e.contains("404") || e.to_lowercase().contains("not found");
                let line = match (en, missing) {
                    (true, true) => format!("The model {model} isn't installed. Pull it, or pick another one in settings."),
                    (false, true) => format!("Le modèle {model} n'est pas installé. Télécharge-le, ou choisis-en un autre dans les réglages."),
                    (true, false) => format!("I couldn't reach {}.", brain.id),
                    (false, false) => format!("Je n'arrive pas à joindre {}.", brain.id),
                };
                speech::speak(app, state, &line);
                return;
            }
        };
        data["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_string()
    };
    state
        .history
        .lock()
        .unwrap()
        .push(json!({ "role": "assistant", "content": raw }));
    println!("[vox] raw ollama (json): {raw}");

    let voice = execute_parsed(app, state, &registry, parse_json(&raw));
    if voice.is_empty() {
        let _ = app.emit("speaking-done", ());
    } else {
        speech::speak(app, state, &voice);
    }
}

// Robust JSON parser: handles code fences, literal newlines in strings,
// partial JSON, and raw prose (port of parseJSON).
fn parse_json(raw: &str) -> Value {
    let mut s = Regex::new(r"(?i)^```(?:json)?\s*")
        .unwrap()
        .replace(raw, "")
        .to_string();
    s = Regex::new(r"\s*```$").unwrap().replace(&s, "").trim().to_string();

    if let Ok(v) = serde_json::from_str::<Value>(&s) {
        return v;
    }
    let escaped = s.replace('\n', "\\n");
    if let Ok(v) = serde_json::from_str::<Value>(&escaped) {
        return v;
    }
    if let Some(m) = Regex::new(r"(?s)(\{.*\}|\[.*\])").unwrap().find(&s) {
        let block = m.as_str();
        if let Ok(v) = serde_json::from_str::<Value>(block) {
            return v;
        }
        if let Ok(v) = serde_json::from_str::<Value>(&block.replace('\n', "\\n")) {
            return v;
        }
    }
    if let Some(caps) = Regex::new(r#""text"\s*:\s*"((?:[^"\\]|\\.)*)""#).unwrap().captures(&s) {
        eprintln!("[vox] JSON malformed, extracted text field via regex");
        return json!({ "action": "none", "text": caps[1].replace("\\n", " ") });
    }
    // Anything still containing a '{' at this point is a broken JSON/tool
    // attempt (e.g. truncated by max_tokens) — never read that aloud.
    if !s.contains('{') && !s.starts_with('[') {
        return json!({ "action": "none", "text": s });
    }
    eprintln!("[vox] unparseable response, suppressing TTS");
    json!({ "action": "none", "text": "" })
}
