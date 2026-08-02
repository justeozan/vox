//! Ollama chat loop. Attempt 1 streams the native tools API (qwen2.5,
//! llama3…) and speaks sentence-by-sentence while the model generates;
//! attempt 2 falls back to a JSON-object prompt any model can follow (gemma3…).

use std::collections::BTreeMap;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use regex::Regex;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

use crate::speech::{self, SpeechItem};
use crate::{load_registry, AppState};

const OLLAMA_URL: &str = "http://localhost:11434/v1/chat/completions";

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
    if en {
        json!([
            tool(
                "launch_agent",
                "Launch a Claude agent on the ACTIVE project for a background development task",
                json!({
                    "task": { "type": "string", "description": "Detailed description of the task" },
                    "text": { "type": "string", "description": "Short spoken reply in English (1-2 sentences)" }
                }),
                json!(["task", "text"])
            ),
            tool(
                "prompt_worktree",
                "Send a prompt to the coding agent of a named Conductor worktree. YOU write the full prompt: context, precise task, constraints, done criteria.",
                json!({
                    "project": { "type": "string", "description": "Worktree/project name exactly as listed in the worktree state data" },
                    "prompt": { "type": "string", "description": "The complete detailed prompt to send to the agent (several sentences)" },
                    "text": { "type": "string", "description": "Short spoken confirmation in English" }
                }),
                json!(["project", "prompt", "text"])
            ),
            tool(
                "switch_project",
                "Change the active project by its name in the ~/.vox/projects.json registry",
                json!({
                    "name": { "type": "string", "description": "Project name in the registry" },
                    "text": { "type": "string", "description": "Short spoken reply in English" }
                }),
                json!(["name", "text"])
            ),
        ])
    } else {
        json!([
            tool(
                "launch_agent",
                "Lance un agent Claude sur le projet ACTIF pour une tâche de développement en arrière-plan",
                json!({
                    "task": { "type": "string", "description": "Description détaillée de la tâche" },
                    "text": { "type": "string", "description": "Réponse vocale courte à prononcer (1-2 phrases)" }
                }),
                json!(["task", "text"])
            ),
            tool(
                "prompt_worktree",
                "Envoie un prompt à l'agent d'un worktree Conductor nommé. C'est TOI qui rédiges le prompt complet : contexte, tâche précise, contraintes, critères de fin.",
                json!({
                    "project": { "type": "string", "description": "Nom du worktree/projet tel que listé dans l'état des worktrees" },
                    "prompt": { "type": "string", "description": "Le prompt détaillé complet à envoyer à l'agent (plusieurs phrases)" },
                    "text": { "type": "string", "description": "Confirmation vocale courte" }
                }),
                json!(["project", "prompt", "text"])
            ),
            tool(
                "switch_project",
                "Change le projet actif par son nom dans le registre ~/.vox/projects.json",
                json!({
                    "name": { "type": "string", "description": "Nom du projet dans le registre" },
                    "text": { "type": "string", "description": "Réponse vocale courte" }
                }),
                json!(["name", "text"])
            ),
        ])
    }
}

fn post_chat(body: &Value, timeout_secs: u64) -> Result<Value, String> {
    ureq::post(OLLAMA_URL)
        .timeout(Duration::from_secs(timeout_secs))
        .send_json(body.clone())
        .map_err(|e| e.to_string())?
        .into_json::<Value>()
        .map_err(|e| e.to_string())
}

/// Stream an SSE chat completion, invoking `on_delta` with each
/// `choices[0].delta` object. `cancel` is polled per line so callers can
/// abort a stream promptly (e.g. ⌥Space during the recap). Errors ONLY if
/// the endpoint or stream fails before any data arrives — a mid-stream cut
/// after data was already delivered (including ureq's total-read timeout on
/// slow generations) returns Ok with whatever was streamed, since the spoken
/// sentences cannot be unsaid.
fn stream_chat(
    body: &Value,
    timeout_secs: u64,
    cancel: &dyn Fn() -> bool,
    mut on_delta: impl FnMut(&Value),
) -> Result<(), String> {
    use std::io::{BufRead, BufReader};
    let resp = ureq::post(OLLAMA_URL)
        .timeout(Duration::from_secs(timeout_secs))
        .send_json(body.clone())
        .map_err(|e| e.to_string())?;
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
    let body = json!({
        "model": model,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user }
        ],
        "max_tokens": max_tokens,
        "stream": false
    });
    let data = post_chat(&body, 60).ok()?;
    let text = data["choices"][0]["message"]["content"].as_str()?.trim().to_string();
    Some(text.trim_matches(|c| c == '"' || c == '\'').to_string())
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
    let res = stream_chat(&body, 90, cancel, |delta| {
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
fn apply_action(
    app: &AppHandle,
    state: &Arc<AppState>,
    registry: &serde_json::Map<String, Value>,
    name: &str,
    args: &Value,
) -> Option<String> {
    let en = state.settings.lock().unwrap().language == "en";
    match name {
        "launch_agent" => {
            if let Some(task) = args.get("task").and_then(|t| t.as_str()) {
                // spawn_agent emits `delegation` itself once the agent actually
                // starts; a false return means it never launched.
                if !crate::agents::spawn_agent(app, state, task) {
                    return Some(if en {
                        "I couldn't launch the agent — is the Claude CLI installed?".into()
                    } else {
                        "Je n'ai pas pu lancer l'agent — le CLI Claude est-il installé ?".into()
                    });
                }
            }
            None
        }
        "prompt_worktree" => {
            let project = args.get("project").and_then(|n| n.as_str()).unwrap_or("");
            let prompt = args.get("prompt").and_then(|p| p.as_str()).unwrap_or("");
            if prompt.trim().is_empty() {
                return Some(if en {
                    "I need a prompt to send.".into()
                } else {
                    "Il me faut un prompt à envoyer.".into()
                });
            }
            match crate::conductor::find_workspace_path(project) {
                Some(path) => {
                    // spawn_agent_in echoes the exact prompt to the UI (the
                    // `delegation` event) only once the agent really starts, and
                    // returns false if it couldn't launch — so we never confirm
                    // a delegation that never happened.
                    if crate::agents::spawn_agent_in(app, state, project, &path, prompt) {
                        None
                    } else {
                        Some(if en {
                            "I found the worktree but couldn't launch the agent.".into()
                        } else {
                            "J'ai trouvé le worktree mais je n'ai pas pu lancer l'agent.".into()
                        })
                    }
                }
                None => Some(if en {
                    format!("I can't find the worktree \"{project}\".")
                } else {
                    format!("Je ne trouve pas le worktree \"{project}\".")
                }),
            }
        }
        "switch_project" => {
            let pname = args.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let resolved = registry
                .get(pname)
                .and_then(|v| v.as_str())
                .map(String::from)
                .or_else(|| args.get("path").and_then(|p| p.as_str()).map(String::from));
            match resolved {
                Some(p) => {
                    println!("[vox] switched project to: {p}");
                    *state.active_project.lock().unwrap() = p;
                    None
                }
                None => Some(if en {
                    format!("I don't know the project \"{pname}\". Add it to ~/.vox/projects.json.")
                } else {
                    format!("Je ne connais pas le projet \"{pname}\". Ajoute-le dans ~/.vox/projects.json.")
                }),
            }
        }
        _ => None,
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
            if let Some(ov) = apply_action(app, state, registry, action, item) {
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
    let ws = crate::conductor::read_state(10);
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
                            let a: String = a.chars().take(120).collect();
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

    if en {
        format!(
            "You are Vox, a voice AI assistant for a developer running many Conductor worktrees in parallel.\n\
             ABSOLUTE RULES:\n\
             - Reply in ENGLISH ONLY, never in French — even if the context data below is in French\n\
             - Maximum 1 short sentence (15 words max), always\n\
             - No lists, no file paths in the reply\n\
             - When asked about a project, SUMMARIZE the agent's last update in English — the data below may be in French; translate it, never quote French verbatim\n\
             - To judge progress, compare the agent's last message to the original ask in the data\n\
             - To delegate work to a specific worktree, call prompt_worktree — YOU write the full prompt (context, precise task, done criteria) from the conversation; keep 'text' to one sentence\n\
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
             - Pour déléguer du travail à un worktree précis, appelle prompt_worktree — c'est TOI qui rédiges le prompt complet (contexte, tâche précise, critères de fin) à partir de la conversation ; 'text' reste une phrase\n\
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
pub fn ask_ollama(app: &AppHandle, state: &Arc<AppState>, transcript: &str) {
    state
        .history
        .lock()
        .unwrap()
        .push(json!({ "role": "user", "content": transcript }));

    let registry = load_registry();
    let base_system = build_system(state);
    let (model, en) = {
        let s = state.settings.lock().unwrap();
        (s.model.clone(), s.language == "en")
    };
    let history = state.history.lock().unwrap().clone();

    let mut messages = vec![json!({ "role": "system", "content": base_system })];
    messages.extend(history.iter().cloned());

    // ── Attempt 1 : native tools API, streamed ──────────────────────────────
    let mut full = String::new();
    let mut sbuf = speech::SentenceBuffer::new();
    let mut session: Option<Sender<SpeechItem>> = None;
    let mut suppressed = false;
    let mut tool_acc: BTreeMap<u64, (String, String)> = BTreeMap::new();

    let stream_res = stream_chat(
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
    );

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
                    if let Some(ov) = apply_action(app, state, &registry, name, &args) {
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
                        let override_v = apply_action(app, state, &registry, &tool_name, &args);
                        println!("[vox] text-tool response: {tool_name}");
                        // Override = the action failed; it outranks the
                        // model's optimistic confirmation.
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
             {{\"action\":\"launch_agent\",\"task\":\"Fix the failing unit tests\",\"text\":\"I'm launching the agent on the tests.\"}}\n\
             {{\"action\":\"prompt_worktree\",\"project\":\"my-app\",\"prompt\":\"Fix the login redirect: after OAuth the user lands on /404. Reproduce, fix, add a test.\",\"text\":\"Sending the prompt to my-app.\"}}\n\
             {{\"action\":\"switch_project\",\"name\":\"my-app\",\"text\":\"Switching to my-app.\"}}"
        )
    } else {
        format!(
            "{base_system}\n\n\
             Réponds TOUJOURS avec un objet JSON valide sur une seule ligne, sans aucun texte autour.\n\
             Exemples :\n\
             {{\"action\":\"none\",\"text\":\"Oui, je t'entends bien.\"}}\n\
             {{\"action\":\"launch_agent\",\"task\":\"Corriger les tests unitaires qui échouent\",\"text\":\"Je lance l'agent sur les tests.\"}}\n\
             {{\"action\":\"prompt_worktree\",\"project\":\"mon-app\",\"prompt\":\"Corrige la redirection login : après OAuth on atterrit sur /404. Reproduis, corrige, ajoute un test.\",\"text\":\"J'envoie le prompt à mon-app.\"}}\n\
             {{\"action\":\"switch_project\",\"name\":\"mon-app\",\"text\":\"Je passe sur mon-app.\"}}"
        )
    };
    let mut messages2 = vec![json!({ "role": "system", "content": json_system })];
    messages2.extend(state.history.lock().unwrap().iter().cloned());

    let data = match post_chat(
        &json!({ "model": model, "messages": messages2, "max_tokens": 300, "stream": false }),
        90,
    ) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[vox] ollama JSON fallback failed: {e}");
            let _ = app.emit("speaking-done", ());
            return;
        }
    };

    let raw = data["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();
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
