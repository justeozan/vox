//! Ollama chat loop — port of askOllama from main.js. Attempt 1 uses the
//! native tools API (qwen2.5, llama3…); attempt 2 falls back to a JSON-object
//! prompt that any model can follow (gemma3…).

use std::sync::Arc;
use std::time::Duration;

use regex::Regex;
use serde_json::{json, Value};
use tauri::AppHandle;

use crate::{load_registry, AppState};

const OLLAMA_URL: &str = "http://localhost:11434/v1/chat/completions";

fn vox_tools() -> Value {
    json!([
        {
            "type": "function",
            "function": {
                "name": "launch_agent",
                "description": "Lance un agent Claude pour effectuer une tâche de développement en arrière-plan",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "task": { "type": "string", "description": "Description détaillée de la tâche" },
                        "text": { "type": "string", "description": "Réponse vocale courte à prononcer (1-2 phrases)" }
                    },
                    "required": ["task", "text"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "switch_project",
                "description": "Change le projet actif par son nom dans le registre ~/.vox/projects.json",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Nom du projet dans le registre" },
                        "text": { "type": "string", "description": "Réponse vocale courte" }
                    },
                    "required": ["name", "text"]
                }
            }
        }
    ])
}

fn post_chat(body: &Value, timeout_secs: u64) -> Result<Value, String> {
    ureq::post(OLLAMA_URL)
        .timeout(Duration::from_secs(timeout_secs))
        .send_json(body.clone())
        .map_err(|e| e.to_string())?
        .into_json::<Value>()
        .map_err(|e| e.to_string())
}

/// One-shot chat helper (no history, no tools) — used by the startup brief.
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
    match name {
        "launch_agent" => {
            if let Some(task) = args.get("task").and_then(|t| t.as_str()) {
                crate::agents::spawn_agent(app, state, task);
            }
            None
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
                None => Some(format!(
                    "Je ne connais pas le projet \"{pname}\". Ajoute-le dans ~/.vox/projects.json."
                )),
            }
        }
        _ => None,
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
                    format!("- {} ({}, {st}) — {label} : \"{preview}\"", w.project, s.agent)
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
             - When asked about a project, quote the agent's last message from the data below\n\
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
             - N'invente jamais un projet, une PR, ou un bug qui n'est pas dans les données\n\n\
             Projet actif : {active_name}{workspace_context}\n\n\
             RAPPEL : Réponds en français, une phrase courte."
        )
    }
}

// ── Main entry ───────────────────────────────────────────────────────────────

pub fn ask_ollama(app: &AppHandle, state: &Arc<AppState>, transcript: &str) -> String {
    state
        .history
        .lock()
        .unwrap()
        .push(json!({ "role": "user", "content": transcript }));

    let registry = load_registry();
    let base_system = build_system(state);
    let model = state.settings.lock().unwrap().model.clone();
    let history = state.history.lock().unwrap().clone();

    let mut messages = vec![json!({ "role": "system", "content": base_system })];
    messages.extend(history.iter().cloned());

    // ── Attempt 1 : native tools API ─────────────────────────────────────────
    match post_chat(
        &json!({
            "model": model,
            "messages": messages,
            "tools": vox_tools(),
            "max_tokens": 300,
            "stream": false
        }),
        90,
    ) {
        Ok(data) => {
            let msg = &data["choices"][0]["message"];

            if let Some(tool_calls) = msg.get("tool_calls").and_then(|t| t.as_array()) {
                if !tool_calls.is_empty() {
                    let mut voice = String::new();
                    for tc in tool_calls {
                        let name = tc["function"]["name"].as_str().unwrap_or("");
                        let args: Value = match &tc["function"]["arguments"] {
                            Value::String(s) => serde_json::from_str(s).unwrap_or(json!({})),
                            v => v.clone(),
                        };
                        if voice.is_empty() {
                            if let Some(t) = args.get("text").and_then(|t| t.as_str()) {
                                voice = t.to_string();
                            }
                        }
                        if let Some(override_text) = apply_action(app, state, &registry, name, &args) {
                            if voice.is_empty() {
                                voice = override_text;
                            }
                        }
                    }
                    state
                        .history
                        .lock()
                        .unwrap()
                        .push(json!({ "role": "assistant", "content": voice }));
                    println!("[vox] tools response: {voice}");
                    return voice;
                }
            }

            if let Some(text) = msg.get("content").and_then(|c| c.as_str()) {
                let text = text.trim().to_string();
                if !text.is_empty() {
                    // qwen2.5+Ollama sometimes emits: `switch_project {"name":"x","text":"y"}`
                    let re = Regex::new(r"(?s)^(\w+)\s*(\{.*\})\s*$").unwrap();
                    if let Some(caps) = re.captures(&text) {
                        if let Ok(args) = serde_json::from_str::<Value>(&caps[2]) {
                            let tool_name = caps[1].to_string();
                            let mut voice = args
                                .get("text")
                                .and_then(|t| t.as_str())
                                .unwrap_or("")
                                .to_string();
                            if let Some(override_text) =
                                apply_action(app, state, &registry, &tool_name, &args)
                            {
                                if voice.is_empty() {
                                    voice = override_text;
                                }
                            }
                            state
                                .history
                                .lock()
                                .unwrap()
                                .push(json!({ "role": "assistant", "content": voice }));
                            println!("[vox] text-tool response: {tool_name} {voice}");
                            return voice;
                        }
                    }
                    state
                        .history
                        .lock()
                        .unwrap()
                        .push(json!({ "role": "assistant", "content": text }));
                    println!("[vox] text response: {text}");
                    return text;
                }
            }
        }
        Err(e) => eprintln!("[vox] tools API failed, using JSON prompt: {e}"),
    }

    // ── Attempt 2 : JSON prompt fallback ─────────────────────────────────────
    let json_system = format!(
        "{base_system}\n\n\
         Réponds TOUJOURS avec un objet JSON valide sur une seule ligne, sans aucun texte autour.\n\
         Exemples :\n\
         {{\"action\":\"none\",\"text\":\"Oui, je t'entends bien.\"}}\n\
         {{\"action\":\"launch_agent\",\"task\":\"Corriger les tests unitaires qui échouent\",\"text\":\"Je lance l'agent sur les tests.\"}}\n\
         {{\"action\":\"switch_project\",\"name\":\"mon-app\",\"text\":\"Je passe sur mon-app.\"}}"
    );
    let mut messages2 = vec![json!({ "role": "system", "content": json_system })];
    messages2.extend(state.history.lock().unwrap().iter().cloned());

    let data = match post_chat(
        &json!({ "model": model, "messages": messages2, "max_tokens": 300, "stream": false }),
        90,
    ) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[vox] ollama JSON fallback failed: {e}");
            return String::new();
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

    let parsed = parse_json(&raw);
    let actions: Vec<Value> = match parsed {
        Value::Array(a) => a,
        v => vec![v],
    };
    let mut voice = String::new();
    for item in &actions {
        let action = item.get("action").and_then(|a| a.as_str()).unwrap_or("none");
        if action != "none" {
            if let Some(override_text) = apply_action(app, state, &registry, action, item) {
                if voice.is_empty() {
                    voice = override_text;
                }
            }
        }
        if voice.is_empty() {
            if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                voice = t.to_string();
            }
        }
    }
    voice
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
    if !s.starts_with('{') && !s.starts_with('[') {
        return json!({ "action": "none", "text": s });
    }
    eprintln!("[vox] unparseable response, suppressing TTS");
    json!({ "action": "none", "text": "" })
}
