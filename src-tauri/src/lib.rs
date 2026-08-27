//! Vox — voice AI orchestration bar. Tauri backend.
//!
//! Why Tauri: the bar needs real-time desktop blur *clipped to the pill's
//! rounded corners*. window-vibrancy can round the native NSVisualEffectView
//! (radius param), which Electron cannot do without native modules — that was
//! the source of the "grey frame" overflow bug.

pub mod agents;
pub mod announce;
pub mod brain;
pub mod conductor;
pub mod daemons;
pub mod llm;
pub mod setup;
pub mod speech;
pub mod targets;
pub mod watch;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, WebviewWindow};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

pub const BAR_W: f64 = 408.0;
pub const BAR_H: f64 = 56.0;
const PILL_RADIUS: f64 = 28.0;
/// NSPopUpMenuWindowLevel. See the setLevel call in `setup` for why not 25.
#[cfg(target_os = "macos")]
const PILL_WINDOW_LEVEL: isize = 101;

// ── Settings ─────────────────────────────────────────────────────────────────

#[derive(Clone, Serialize, Deserialize)]
pub struct Settings {
    pub model: String,
    pub language: String,
    /// How a finished agent's answer is spoken:
    /// "summary" (LLM-condensed, default) | "verbatim" (its own words) | "off".
    /// "off" is intentionally not in the UI — set it in settings.json or via
    /// VOX_AGENT_REPLY. Results are still remembered and still answerable.
    #[serde(default = "default_agent_reply")]
    pub agent_reply: String,
    /// Verbatim above this many (cleaned) characters falls back to a summary.
    /// ~14 chars/second of speech, so 420 is about half a minute.
    #[serde(default = "default_agent_reply_max")]
    pub agent_reply_max_chars: usize,
    /// "auto" (language-derived, the historical behaviour) | "kokoro" |
    /// "piper" | "qwen3" | "say".
    #[serde(default = "default_tts_engine")]
    pub tts_engine: String,
    /// Which LLM answers: "ollama" (local) | "claude-cli" / "codex-cli"
    /// (your subscription, via the CLI) | "anthropic" / "openai" (API key).
    #[serde(default = "default_provider")]
    pub provider: String,
}

fn default_provider() -> String {
    "ollama".into()
}

/// The provider, read straight from disk.
///
/// `chat_once` and friends are called from places that hold a model id but no
/// AppState (the recap, announcement summaries). Reading the small settings
/// file is cheaper than threading state through every one of them.
pub fn persisted_provider() -> String {
    std::env::var("VOX_PROVIDER")
        .ok()
        .or_else(|| {
            std::fs::read_to_string(settings_path())
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .and_then(|v| v.get("provider").and_then(|p| p.as_str()).map(String::from))
        })
        .filter(|p| brain::PROVIDERS.contains(&p.as_str()))
        .unwrap_or_else(default_provider)
}

fn default_tts_engine() -> String {
    "auto".into()
}
pub const TTS_ENGINES: &[&str] = &["auto", "kokoro", "piper", "qwen3", "say"];

/// Collapse the user's setting into one concrete engine.
///
/// Returning `&'static str` keeps `start_tts`'s match exhaustive over literals
/// and makes an unknown value impossible downstream — the old code silently
/// fell through to Kokoro, which is exactly how a "qwen3" run could have
/// produced Kokoro audio and invalidated a whole benchmark.
pub fn resolve_engine(s: &Settings) -> &'static str {
    match s.tts_engine.as_str() {
        "kokoro" => "kokoro",
        "piper" => "piper",
        "qwen3" => "qwen3",
        "say" => "say",
        _ => lang_config(&s.language).tts_engine,
    }
}

fn default_agent_reply() -> String {
    "summary".into()
}
fn default_agent_reply_max() -> usize {
    420
}
pub const AGENT_REPLY_MODES: &[&str] = &["summary", "verbatim", "off"];

pub struct LangConfig {
    pub stt: &'static str,
    pub tts_engine: &'static str,
    pub kokoro_lang: &'static str,
    pub kokoro_voice: &'static str,
    pub piper_model: &'static str,
    pub say_voice: &'static str,
}

pub fn lang_config(lang: &str) -> LangConfig {
    match lang {
        "en" => LangConfig { stt: "en", tts_engine: "kokoro", kokoro_lang: "a", kokoro_voice: "af_heart", piper_model: "", say_voice: "Samantha" },
        _ => LangConfig { stt: "fr", tts_engine: "piper", kokoro_lang: "f", kokoro_voice: "ff_siwis", piper_model: "fr_FR-siwis-medium", say_voice: "Thomas" },
    }
}

pub fn home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/"))
}

fn settings_path() -> PathBuf {
    home().join(".vox/settings.json")
}

fn load_settings() -> Settings {
    let persisted: Value = std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}));
    let model = std::env::var("VOX_MODEL")
        .ok()
        .or_else(|| persisted.get("model").and_then(|v| v.as_str()).map(String::from))
        .unwrap_or_else(|| "qwen2.5:3b".into());
    let language = std::env::var("VOX_LANG")
        .ok()
        .or_else(|| persisted.get("language").and_then(|v| v.as_str()).map(String::from))
        .unwrap_or_else(|| "fr".into())
        .to_lowercase();
    let agent_reply = std::env::var("VOX_AGENT_REPLY")
        .ok()
        .or_else(|| persisted.get("agent_reply").and_then(|v| v.as_str()).map(String::from))
        .unwrap_or_else(default_agent_reply)
        .to_lowercase();
    let agent_reply = if AGENT_REPLY_MODES.contains(&agent_reply.as_str()) {
        agent_reply
    } else {
        default_agent_reply()
    };
    let agent_reply_max_chars = persisted
        .get("agent_reply_max_chars")
        .and_then(|v| v.as_u64())
        .map(|n| (n as usize).clamp(120, 2000))
        .unwrap_or_else(default_agent_reply_max);
    // VOX_TTS stops being a per-session runtime bypass and becomes a
    // launch-time SEED for the persisted setting: leaving it as a live
    // override would let a stale shell export silently beat the settings
    // picker, which is the exact class of bug the picker exists to remove.
    let tts_engine = std::env::var("VOX_TTS")
        .ok()
        .or_else(|| persisted.get("tts_engine").and_then(|v| v.as_str()).map(String::from))
        .unwrap_or_else(default_tts_engine)
        .to_lowercase();
    let tts_engine = if TTS_ENGINES.contains(&tts_engine.as_str()) {
        tts_engine
    } else {
        default_tts_engine()
    };
    let provider = std::env::var("VOX_PROVIDER")
        .ok()
        .or_else(|| persisted.get("provider").and_then(|v| v.as_str()).map(String::from))
        .unwrap_or_else(default_provider);
    let provider = if brain::PROVIDERS.contains(&provider.as_str()) {
        provider
    } else {
        default_provider()
    };
    // Keep the two in step at LOAD time, not just when the dropdown changes:
    // VOX_PROVIDER (or a hand-edited settings.json) can set the provider
    // without touching the model, leaving "Claude (subscription)" sitting next
    // to "qwen3:8b" — a pairing that fails on every request.
    let model = if brain::model_fits(&provider, &model) {
        model
    } else {
        let d = brain::default_model(&provider).to_string();
        if !model.trim().is_empty() {
            println!("[vox] model {model:?} doesn't fit provider {provider:?} — using {d:?}");
        }
        d
    };
    Settings { model, language, agent_reply, agent_reply_max_chars, tts_engine, provider }
}

fn save_settings(s: &Settings) {
    let p = settings_path();
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    // Serialize the struct rather than hand-building the object: the old
    // key-by-key version silently dropped any field someone forgot to add.
    if let Ok(v) = serde_json::to_string_pretty(s) {
        let _ = std::fs::write(p, v);
    }
}

// ── Project registry ─────────────────────────────────────────────────────────

fn registry_path() -> PathBuf {
    home().join(".vox/projects.json")
}

pub fn load_registry() -> serde_json::Map<String, Value> {
    std::fs::read_to_string(registry_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn ensure_registry(active_project: &str) {
    let p = registry_path();
    if p.exists() {
        return;
    }
    let name = std::path::Path::new(active_project)
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".into());
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    let mut map = serde_json::Map::new();
    map.insert(name.clone(), Value::String(active_project.to_string()));
    let _ = std::fs::write(&p, serde_json::to_string_pretty(&Value::Object(map)).unwrap());
    println!("[vox] created ~/.vox/projects.json with \"{name}\"");
}

// ── Shared state ─────────────────────────────────────────────────────────────

pub struct Paths {
    pub stt_script: Option<PathBuf>,
    pub tts_script: Option<PathBuf>,
    pub tts_piper_script: Option<PathBuf>,
    pub tts_qwen_script: Option<PathBuf>,
    /// The dedicated ~/.vox/venv-qwen interpreter. No system-python fallback:
    /// a python without mlx_audio would just report a missing dependency at
    /// every launch.
    pub tts_qwen_python: Option<PathBuf>,
    pub stt_python: Option<PathBuf>,
    pub tts_python: Option<PathBuf>,
    pub whisper_cli: Option<PathBuf>,
    pub claude_cli: Option<PathBuf>,
}

pub struct AppState {
    pub settings: Mutex<Settings>,
    pub history: Mutex<Vec<Value>>,
    pub stt: Mutex<Option<daemons::DaemonHandle>>,
    pub tts: Mutex<Option<daemons::DaemonHandle>>,
    /// (play id, done sender) for the sentence currently playing. The id lets
    /// audio_done ignore stale acks — a failed Audio load can fire twice
    /// (onerror + play().catch), and a duplicate ack must not consume the
    /// NEXT sentence's sender.
    pub audio_done: Mutex<Option<(u64, std::sync::mpsc::Sender<()>)>>,
    /// Monotonic id source for play-wav events.
    pub play_id: AtomicU64,
    /// Agents Vox has running, in launch order. Replaces the old bare counter:
    /// an announcement has to name WHICH agent finished, and the concurrency
    /// cap has to count slots reserved before a process even exists.
    pub agents: Mutex<Vec<agents::AgentJob>>,
    pub agent_seq: AtomicU64,
    pub active_project: Mutex<String>,
    pub startup_brief_done: AtomicBool,
    /// Interrupt GENERATION — bumped by the `interrupt` command (⌥Space /
    /// barge-in). Sessions capture the value at creation and treat any later
    /// mismatch as "cancelled". A counter (not a bool) so an old interrupt can
    /// never be un-armed by a later turn and resurrect a killed session.
    pub interrupt_gen: AtomicU64,
    pub anim_gen: AtomicU64,
    pub paths: Mutex<Paths>,
    pub speak_lock: Mutex<()>,
    /// Renderer pill state, mirrored from `setState` (see UI_* below). The
    /// backend otherwise has NO way to know the mic is open — that lives
    /// entirely in the webview — and speaking into an open mic feeds Vox's own
    /// voice back to its VAD. Proactive speech only starts from a resting code.
    pub ui_busy: AtomicU8,
    /// Unix seconds of the last `ui_state` call. A webview that dies mid-turn
    /// would otherwise pin `ui_busy` to a busy code forever.
    pub ui_busy_at: AtomicU64,
    /// Finished agent answers, newest last. Kept OUT of `history` on purpose —
    /// history is replayed into every model call.
    pub agent_results: Mutex<VecDeque<announce::AgentResult>>,
    /// Announcements waiting to be spoken, plus the condvar the drainer parks
    /// on. Durable: an interrupted batch comes back here.
    pub announce_q: Arc<(Mutex<announce::Queue>, Condvar)>,
    /// Unix seconds of the last spoken "I could do X next" offer.
    pub last_proposal_at: AtomicU64,
    /// (cwd, unix) for agents Vox launched recently. Guards against announcing
    /// the same work twice should a Conductor bridge ever exist.
    pub vox_cwds: Mutex<Vec<(String, u64)>>,
}

// Renderer pill states, mirrored into `AppState::ui_busy`. Kept in sync with
// UI_CODES in renderer/index.html.
pub const UI_IDLE: u8 = 0;
pub const UI_RUNNING: u8 = 5;
/// After this long without a `ui_state` update, treat a busy code as stale.
const UI_BUSY_STALE_SECS: u64 = 90;

/// True when the renderer is at rest — nothing is being said, heard, or thought
/// about — so Vox may speak on its own initiative.
pub fn ui_at_rest(state: &Arc<AppState>) -> bool {
    let code = state.ui_busy.load(Ordering::SeqCst);
    if code == UI_IDLE || code == UI_RUNNING {
        return true;
    }
    // Fail open on a stale reading: a wedged webview must not mute Vox forever.
    let at = state.ui_busy_at.load(Ordering::SeqCst);
    now_unix().saturating_sub(at) > UI_BUSY_STALE_SECS
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ── Microphone permission (macOS TCC) ────────────────────────────────────────
// WKWebView's getUserMedia returns a "live" track but delivers SILENCE until the
// host app holds a TCC microphone grant — wry auto-grants at the WebKit layer
// yet never fires the OS prompt, so the mic sandbox extension can't be created
// ("Could not create a 'com.apple.webkit.microphone' sandbox extension"). We
// trigger the real prompt ourselves via AVFoundation; once the user allows it
// (persisted against our stable app.vox.bar signature), the webview mic works.
#[cfg(target_os = "macos")]
fn request_microphone_access() {
    use block2::RcBlock;
    use objc2::runtime::Bool;
    use objc2::{class, msg_send};
    use objc2_foundation::NSString;

    unsafe {
        // AVMediaTypeAudio is the string constant "soun".
        let audio_type = NSString::from_str("soun");
        let cls = class!(AVCaptureDevice);
        // AVAuthorizationStatus: 0=notDetermined 1=restricted 2=denied 3=authorized
        let status: isize = msg_send![cls, authorizationStatusForMediaType: &*audio_type];
        println!("[vox] microphone TCC status = {status} (0=undetermined 1=restricted 2=denied 3=authorized)");
        if status == 3 {
            return;
        }
        if status == 2 {
            eprintln!("[vox] microphone DENIED — enable it in System Settings > Privacy & Security > Microphone");
            return;
        }
        // notDetermined -> fire the prompt. Heap block: the callback runs later,
        // after this fn returns, so a stack block would be freed too early.
        let handler: RcBlock<dyn Fn(Bool)> = RcBlock::new(|granted: Bool| {
            println!("[vox] microphone access granted = {}", granted.as_bool());
        });
        let _: () = msg_send![cls, requestAccessForMediaType: &*audio_type, completionHandler: &*handler];
    }
}


// ── Window frame animation ───────────────────────────────────────────────────
// Bottom-anchored, horizontally centered grow/shrink. Stepped resize with
// ease-out cubic — runs off the main thread, each step dispatches to AppKit.

fn animate_frame(window: WebviewWindow, state: Arc<AppState>, gen: u64, tw: f64, th: f64) {
    let scale = window.scale_factor().unwrap_or(2.0);
    let (Ok(size), Ok(pos)) = (window.inner_size(), window.outer_position()) else { return };
    let cw = size.width as f64 / scale;
    let ch = size.height as f64 / scale;
    let cx = pos.x as f64 / scale;
    let cy = pos.y as f64 / scale;
    if (cw - tw).abs() < 0.5 && (ch - th).abs() < 0.5 {
        return;
    }
    let tx = cx - (tw - cw) / 2.0;
    let ty = cy - (th - ch); // tauri y grows downward; keep bottom edge fixed

    // When growing, expand the size BEFORE recentering the origin — otherwise
    // set_position moves the window left while it's still narrow, so the right
    // edge briefly recedes and clips before the size step widens it again.
    let growing = tw > cw || th > ch;

    // ~240ms with a quintic ease-out. Two reasons for the longer, softer curve:
    //  - it moves the native frame in step with the in-page CSS reveal (the gear
    //    width + bar gap transition over 0.35s var(--smooth)), so the pill edge
    //    and its contents expand as ONE gesture instead of the edge snapping wide
    //    in 120ms and the content trailing behind it;
    //  - a quintic tail (vs cubic) settles the last few pixels more gently, so
    //    the growing edge eases into place rather than arriving with a hard stop.
    // 40 steps keeps ≥1 position update per display refresh even on 120Hz
    // ProMotion, so the motion never reads as stepped.
    const STEPS: u32 = 40;
    for i in 1..=STEPS {
        if state.anim_gen.load(Ordering::SeqCst) != gen {
            return; // superseded by a newer animation
        }
        let t = i as f64 / STEPS as f64;
        let e = 1.0 - (1.0 - t).powi(5);
        let pos = tauri::LogicalPosition::new(cx + (tx - cx) * e, cy + (ty - cy) * e);
        let size = tauri::LogicalSize::new(cw + (tw - cw) * e, ch + (th - ch) * e);
        if growing {
            let _ = window.set_size(size);
            let _ = window.set_position(pos);
        } else {
            let _ = window.set_position(pos);
            let _ = window.set_size(size);
        }
        std::thread::sleep(Duration::from_millis(6));
    }
}

fn position_bottom_center(window: &WebviewWindow) {
    if let Ok(Some(mon)) = window.primary_monitor() {
        let scale = mon.scale_factor();
        let ms = mon.size().to_logical::<f64>(scale);
        let x = (ms.width - BAR_W) / 2.0;
        let y = ms.height - BAR_H - 96.0; // clear the dock
        let _ = window.set_position(tauri::LogicalPosition::new(x, y));
    }
}

// ── Commands ─────────────────────────────────────────────────────────────────

#[tauri::command]
fn get_settings(state: tauri::State<'_, Arc<AppState>>) -> Settings {
    state.settings.lock().unwrap().clone()
}

#[tauri::command]
async fn list_models(provider: Option<String>) -> Vec<String> {
    let provider = provider.unwrap_or_else(persisted_provider);
    tauri::async_runtime::spawn_blocking(move || match provider.as_str() {
        // Ollama is the only provider that can be enumerated live; everything
        // else gets a curated shortlist, because these run on EVERY voice turn
        // and the big models are the wrong tool for a one-sentence reply.
        "ollama" => ureq::get("http://localhost:11434/api/tags")
            .timeout(Duration::from_secs(3))
            .call()
            .ok()
            .and_then(|r| r.into_json::<Value>().ok())
            .and_then(|v| v.get("models").and_then(|m| m.as_array()).cloned())
            .map(|arr| {
                let mut names: Vec<String> = arr
                    .iter()
                    .filter_map(|m| m.get("name").and_then(|n| n.as_str()).map(String::from))
                    .collect();
                names.sort();
                names
            })
            .unwrap_or_default(),
        "anthropic" => ["claude-haiku-4-5", "claude-sonnet-5", "claude-opus-5"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        "openai" => ["gpt-4o-mini", "gpt-4o"].iter().map(|s| s.to_string()).collect(),
        // The CLI decides its own model (whatever you're signed in with), so
        // there is nothing to choose here.
        _ => vec!["subscription".to_string()],
    })
    .await
    .unwrap_or_default()
}

/// Whether each provider can actually be used right now, so the picker can grey
/// out what would fail and say why.
#[tauri::command]
fn provider_status() -> Value {
    let mut out = serde_json::Map::new();
    for p in brain::PROVIDERS {
        let b = brain::resolve(p, "");
        let why = b.unavailable();
        out.insert(
            p.to_string(),
            json!({ "ok": why.is_none(), "detail": why.unwrap_or_default() }),
        );
    }
    Value::Object(out)
}

/// First-run setup: probe the local stack (Python, venv, Whisper/Kokoro/Piper,
/// voices, system tools). The renderer renders this as status chips.
#[tauri::command]
fn probe_setup() -> Value {
    setup::probe()
}

/// Auto-install the missing speech stack into `~/.vox/venv`, streaming progress
/// via `setup-log` events. On success, re-resolves paths and starts the
/// daemons so the recap plays right after install.
#[tauri::command]
async fn run_setup(
    app: AppHandle,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<Value, String> {
    let st = state.inner().clone();
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let res = setup::run_setup(&handle, &st);
        if res.is_ok() {
            daemons::init_paths(&st, &handle);
            daemons::start_stt(&st);
            daemons::start_tts(&st, handle.clone());
        }
        res
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn set_settings(
    app: AppHandle,
    state: tauri::State<'_, Arc<AppState>>,
    next: Value,
) -> Result<Value, String> {
    let st = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut restart = false;
        let mut engine_changed = false;
        {
            let mut settings = st.settings.lock().unwrap();
            if let Some(m) = next.get("model").and_then(|v| v.as_str()) {
                if !m.is_empty() && m != settings.model {
                    settings.model = m.to_string();
                }
            }
            if let Some(l) = next.get("language").and_then(|v| v.as_str()) {
                if (l == "fr" || l == "en") && l != settings.language {
                    settings.language = l.to_string();
                    restart = true;
                }
            }
            // No daemon restart: this is read per announcement, not at spawn.
            if let Some(r) = next.get("agent_reply").and_then(|v| v.as_str()) {
                if AGENT_REPLY_MODES.contains(&r) {
                    settings.agent_reply = r.to_string();
                }
            }
            if let Some(n) = next.get("agent_reply_max_chars").and_then(|v| v.as_u64()) {
                settings.agent_reply_max_chars = (n as usize).clamp(120, 2000);
            }
            if let Some(pv) = next.get("provider").and_then(|v| v.as_str()) {
                if brain::PROVIDERS.contains(&pv) {
                    settings.provider = pv.to_string();
                }
                // Re-check unconditionally, NOT only when the provider changed:
                // the two can already be out of step (env var, edited file), and
                // picking the provider that is displayed must repair that rather
                // than be a no-op.
                if !brain::model_fits(&settings.provider, &settings.model) {
                    settings.model = brain::default_model(&settings.provider).to_string();
                }
            }
            if let Some(e) = next.get("tts_engine").and_then(|v| v.as_str()) {
                let e = e.to_lowercase();
                if TTS_ENGINES.contains(&e.as_str()) && e != settings.tts_engine {
                    settings.tts_engine = e;
                    restart = true;
                    engine_changed = true;
                }
            }
            save_settings(&settings);
        }
        if restart {
            if !engine_changed {
                // Language change: reset the conversation so the model doesn't
                // carry wrong-language turns. An engine change is not a
                // conversation change.
                st.history.lock().unwrap().clear();
            }
            daemons::restart_daemons(&st, app.clone());
        }
        // The whole point of an A/B is hearing the new voice NOW.
        // `startup_brief_done` stays true, so nothing else would speak until
        // the next turn and the change would feel like it did nothing.
        if engine_changed {
            let (st2, app2) = (st.clone(), app.clone());
            std::thread::spawn(move || {
                for _ in 0..200 {
                    let ready = st2.tts.lock().unwrap().as_ref().map(|d| d.is_ready()).unwrap_or(false);
                    if ready {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                let en = st2.settings.lock().unwrap().language == "en";
                speech::speak(&app2, &st2, tts_sample_line(en));
            });
        }
        let s = st.settings.lock().unwrap().clone();
        Ok(json!({
            "provider": s.provider,
            "tts_engine": s.tts_engine,
            "model": s.model,
            "language": s.language,
            "agent_reply": s.agent_reply,
            "agent_reply_max_chars": s.agent_reply_max_chars,
            "restarted": restart,
        }))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Not "hello": the sample has to exercise what the A/B is actually about —
/// a project name, accents, an acronym, and digits.
fn tts_sample_line(en: bool) -> &'static str {
    if en {
        "The agent on marseille is done — pull request 42 is ready for review."
    } else {
        "L'agent sur marseille a terminé, la pull request numéro 42 est prête à relire."
    }
}

#[tauri::command]
async fn voice_input(
    app: AppHandle,
    state: tauri::State<'_, Arc<AppState>>,
    wav: String,
) -> Result<(), String> {
    let st = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        // NOTE: no interrupt reset here. Interruption is generation-based —
        // new sessions capture the current generation and are unaffected by
        // past interrupts, while a killed recap stays killed even if the user
        // immediately asks something (clearing a global flag here used to
        // resurrect the aborted recap's advice stream).
        use base64::Engine;
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&wav) else {
            let _ = app.emit("speaking-done", ());
            return;
        };
        let tmp = std::env::temp_dir().join(format!(
            "vox_{}.wav",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        ));
        if std::fs::write(&tmp, &bytes).is_err() {
            let _ = app.emit("speaking-done", ());
            return;
        }

        let transcript = speech::transcribe(&st, &tmp);
        let _ = std::fs::remove_file(&tmp);

        if transcript.is_empty() {
            let _ = app.emit("speaking-done", ());
            return;
        }
        // Renderer reveals this letter by letter.
        let _ = app.emit("transcript", &transcript);

        // Streams the reply to TTS as it generates and guarantees a
        // speaking-done on every path.
        llm::ask_ollama(&app, &st, &transcript);
    })
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
fn audio_done(state: tauri::State<'_, Arc<AppState>>, id: u64) {
    let mut guard = state.audio_done.lock().unwrap();
    // Only honor the ack for the sentence that's actually playing.
    if guard.as_ref().is_some_and(|(cur, _)| *cur == id) {
        if let Some((_, tx)) = guard.take() {
            let _ = tx.send(());
        }
    }
}

#[tauri::command]
fn resize_window(
    window: WebviewWindow,
    state: tauri::State<'_, Arc<AppState>>,
    width: f64,
    height: f64,
) {
    let gen = state.anim_gen.fetch_add(1, Ordering::SeqCst) + 1;
    let st = state.inner().clone();
    std::thread::spawn(move || animate_frame(window, st, gen, width, height));
}

/// Interrupt whatever Vox is currently saying — ⌥Space or barge-in. Bumps the
/// interrupt generation (cancelling every session/brief created before this
/// moment, permanently) and unblocks the current playback wait.
#[tauri::command]
fn interrupt(state: tauri::State<'_, Arc<AppState>>) {
    state.interrupt_gen.fetch_add(1, Ordering::SeqCst);
    if let Some((_, tx)) = state.audio_done.lock().unwrap().take() {
        let _ = tx.send(());
    }
}

/// Move the pill by a logical-pixel delta. The renderer owns dragging: see the
/// setMovableByWindowBackground call in `setup` for why AppKit can't.
#[tauri::command]
fn move_pill(window: WebviewWindow, dx: f64, dy: f64) {
    let scale = window.scale_factor().unwrap_or(2.0);
    if let Ok(pos) = window.outer_position() {
        let _ = window.set_position(tauri::LogicalPosition::new(
            pos.x as f64 / scale + dx,
            pos.y as f64 / scale + dy,
        ));
    }
}

/// Mirror the renderer's pill state into the backend. Called from `setState`
/// on every transition. Proactive speech (agent-completion announcements) waits
/// for a resting code — see `ui_at_rest`.
#[tauri::command]
fn ui_state(state: tauri::State<'_, Arc<AppState>>, code: u8) {
    state.ui_busy.store(code, Ordering::SeqCst);
    state.ui_busy_at.store(now_unix(), Ordering::SeqCst);
}

/// Install an optional speech engine (Qwen3-TTS). Separate from `run_setup`
/// on purpose: it must never gate launch.
#[tauri::command]
async fn install_engine(
    app: AppHandle,
    state: tauri::State<'_, Arc<AppState>>,
    name: String,
) -> Result<Value, String> {
    if name != "qwen3" {
        return Err(format!("unknown engine \"{name}\""));
    }
    let st = state.inner().clone();
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let res = setup::install_qwen(&handle);
        if res.is_ok() {
            // Re-probe paths so the engine becomes selectable without a restart.
            daemons::init_paths(&st, &handle);
        }
        res
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Stop every running agent — voice ("stop", "arrête les agents") or a click on
/// the badge. A one-second remedy for ANY launch mistake, including ones a
/// confirmation prompt would never have caught (right repo, wrong task).
#[tauri::command]
fn kill_agents(app: AppHandle, state: tauri::State<'_, Arc<AppState>>) -> usize {
    agents::kill_all(&app, &state.inner().clone())
}

/// Finished agent answers, newest first — for the UI and for debugging.
#[tauri::command]
fn recent_results(state: tauri::State<'_, Arc<AppState>>) -> Vec<Value> {
    state
        .agent_results
        .lock()
        .unwrap()
        .iter()
        .rev()
        .map(|r| {
            json!({
                "project": r.project,
                "source": r.source,
                "outcome": r.outcome,
                "task": r.task,
                "answer": r.answer_full,
                "finished_unix": r.finished_unix,
            })
        })
        .collect()
}

/// Speak the announcement backlog now (badge click): clears the interrupt
/// back-off so held items go out on the drainer's next pass.
#[tauri::command]
fn speak_pending_announcements(state: tauri::State<'_, Arc<AppState>>) {
    announce::speak_now(&state.inner().clone());
}

/// Re-speak the most recent agent answer ("redis-moi ça").
#[tauri::command]
fn replay_last_result(app: AppHandle, state: tauri::State<'_, Arc<AppState>>) {
    let st = state.inner().clone();
    std::thread::spawn(move || {
        let (en, text) = {
            let s = st.settings.lock().unwrap();
            let en = s.language == "en";
            drop(s);
            let last = st.agent_results.lock().unwrap().back().cloned();
            let text = match last {
                Some(r) => announce::strip_for_speech(&r.answer_full, en),
                None => (if en { "Nothing to replay yet." } else { "Rien à redire pour l'instant." })
                    .to_string(),
            };
            (en, text)
        };
        let _ = en;
        speech::speak(&app, &st, &text);
    });
}

/// Re-run the worktree recap on demand (recap button next to the gear).
#[tauri::command]
fn replay_recap(app: AppHandle, state: tauri::State<'_, Arc<AppState>>) {
    let st = state.inner().clone();
    std::thread::spawn(move || {
        st.startup_brief_done.store(false, Ordering::SeqCst);
        if !conductor::speak_startup_brief(&app, &st) {
            // A button press deserves a reply even when there's nothing.
            let en = st.settings.lock().unwrap().language == "en";
            speech::speak(
                &app,
                &st,
                if en { "No active worktree to report." } else { "Aucun worktree actif à signaler." },
            );
        }
    });
}

// ── Version & updates ─────────────────────────────────────────────────────────

#[tauri::command]
fn get_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Compare dotted numeric versions ("0.10.0" > "0.9.9"). Non-numeric parts
/// compare as 0 — good enough for our tag scheme.
fn version_gt(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.split(['.', '-', '+'])
            .map(|p| p.chars().take_while(|c| c.is_ascii_digit()).collect::<String>())
            .map(|s| s.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let (pa, pb) = (parse(a), parse(b));
    for i in 0..pa.len().max(pb.len()) {
        let (x, y) = (pa.get(i).copied().unwrap_or(0), pb.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

/// Ask GitHub for the latest release and report whether it's newer. Full
/// silent auto-install needs tauri-plugin-updater + a signed feed + release CI;
/// for now this surfaces the update and one-click opens the download.
#[tauri::command]
async fn check_update() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let current = env!("CARGO_PKG_VERSION").to_string();
        let latest_tag = ureq::get("https://api.github.com/repos/justeozan/vox/releases/latest")
            .set("User-Agent", "vox-updater")
            .set("Accept", "application/vnd.github+json")
            .timeout(Duration::from_secs(8))
            .call()
            .ok()
            .and_then(|r| r.into_json::<Value>().ok())
            .and_then(|v| v.get("tag_name").and_then(|t| t.as_str()).map(String::from));

        match latest_tag {
            Some(tag) => {
                let latest = tag.trim_start_matches('v').to_string();
                json!({
                    "current": current,
                    "latest": latest,
                    "hasUpdate": version_gt(&latest, &current),
                    "url": "https://github.com/justeozan/vox/releases/latest",
                })
            }
            // No releases yet / offline — report "up to date" rather than error.
            None => json!({ "current": current, "latest": current, "hasUpdate": false, "url": "" }),
        }
    })
    .await
    .map_err(|e| e.to_string())
}

/// Open a URL in the default browser (release download page).
#[tauri::command]
fn open_url(url: String) {
    if url.starts_with("https://") {
        let _ = std::process::Command::new("open").arg(&url).spawn();
    }
}

// ── Pronunciation dictionary ──────────────────────────────────────────────────

fn pronunciations_path() -> PathBuf {
    home().join(".vox/pronunciations.json")
}

/// Word → phonetic respelling, applied to TTS input so the voice says tricky
/// names the way you want (e.g. {"Conductor": "conedeuctor"}). Edited via
/// ~/.vox/pronunciations.json. Voice-cloning is out of scope for Kokoro.
pub fn load_pronunciations() -> Vec<(String, String)> {
    std::fs::read_to_string(pronunciations_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .map(|m| {
            m.into_iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
                .filter(|(k, _)| !k.trim().is_empty())
                .collect()
        })
        .unwrap_or_default()
}

// ── App entry ────────────────────────────────────────────────────────────────

pub fn run() {
    let settings = load_settings();
    // Write back immediately. load_settings repairs an incoherent
    // provider/model pair and fills in fields absent from an older file; without
    // this the file keeps contradicting the running app until some unrelated
    // setting happens to change.
    save_settings(&settings);
    let active_project = std::env::var("VOX_PROJECT")
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| home()).to_string_lossy().to_string());

    let state = Arc::new(AppState {
        settings: Mutex::new(settings),
        history: Mutex::new(Vec::new()),
        stt: Mutex::new(None),
        tts: Mutex::new(None),
        audio_done: Mutex::new(None),
        play_id: AtomicU64::new(0),
        agents: Mutex::new(Vec::new()),
        agent_seq: AtomicU64::new(0),
        active_project: Mutex::new(active_project),
        startup_brief_done: AtomicBool::new(false),
        interrupt_gen: AtomicU64::new(0),
        anim_gen: AtomicU64::new(0),
        paths: Mutex::new(Paths {
            stt_script: None,
            tts_script: None,
            tts_piper_script: None,
            tts_qwen_script: None,
            tts_qwen_python: None,
            stt_python: None,
            tts_python: None,
            whisper_cli: None,
            claude_cli: None,
        }),
        speak_lock: Mutex::new(()),
        ui_busy: AtomicU8::new(UI_IDLE),
        ui_busy_at: AtomicU64::new(0),
        agent_results: Mutex::new(VecDeque::new()),
        announce_q: Arc::new((Mutex::new(announce::Queue::default()), Condvar::new())),
        last_proposal_at: AtomicU64::new(0),
        vox_cwds: Mutex::new(Vec::new()),
    });

    let alt_space = Shortcut::new(Some(Modifiers::ALT), Code::Space);
    let cmd_comma = Shortcut::new(Some(Modifiers::SUPER), Code::Comma);

    tauri::Builder::default()
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(move |app, shortcut, event| {
                    if event.state() != ShortcutState::Pressed {
                        return;
                    }
                    if shortcut == &alt_space {
                        let _ = app.emit("toggle-listening", ());
                    } else if shortcut == &cmd_comma {
                        let _ = app.emit("toggle-settings", ());
                    }
                })
                .build(),
        )
        .manage(state.clone())
        .invoke_handler(tauri::generate_handler![
            get_settings,
            set_settings,
            list_models,
            provider_status,
            probe_setup,
            run_setup,
            voice_input,
            audio_done,
            resize_window,
            interrupt,
            ui_state,
            move_pill,
            install_engine,
            kill_agents,
            recent_results,
            speak_pending_announcements,
            replay_last_result,
            replay_recap,
            get_version,
            check_update,
            open_url,
        ])
        .setup(move |app| {
            println!("[vox] starting pid={}", std::process::id());

            // Prompt for microphone access up front so the webview mic isn't a
            // silent stream (see request_microphone_access).
            #[cfg(target_os = "macos")]
            request_microphone_access();

            // Floating accessory — no dock icon, follows every Space.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let window = app.get_webview_window("main").expect("main window");
            let _ = window.set_visible_on_all_workspaces(true);

            // THE migration payoff: native blur clipped to the pill radius.
            #[cfg(target_os = "macos")]
            window_vibrancy::apply_vibrancy(
                &window,
                window_vibrancy::NSVisualEffectMaterial::HudWindow,
                Some(window_vibrancy::NSVisualEffectState::Active),
                Some(PILL_RADIUS),
            )
            .expect("apply_vibrancy failed");

            // Native window tuning, all in one main-thread block:
            //  - FullScreenAuxiliary so the bar shows over fullscreen Spaces
            //    (tauri's visible_on_all_workspaces only sets CanJoinAllSpaces)
            //  - NSStatusWindowLevel to float above app windows
            //  - re-raise the webview above the vibrancy view: apply_vibrancy
            //    inserts the NSVisualEffectView "below new siblings", but the
            //    webview is already attached, so the blur lands ON TOP of the
            //    page — content invisible and mouse events dead (the drag bug)
            #[cfg(target_os = "macos")]
            if let Ok(ptr) = window.ns_window() {
                use objc2::msg_send;
                use objc2::runtime::AnyObject;
                let ns_win = ptr as *mut AnyObject;
                unsafe {
                    // Turn the window into a non-activating NSPanel.
                    //
                    // This is what makes the pill follow you onto EVERY desktop,
                    // including another app's fullscreen Space. A plain NSWindow
                    // with canJoinAllSpaces is honoured on ordinary desktops but
                    // WindowServer refuses to composite it over a third-party
                    // fullscreen Space — the long-standing bug in TODOS.md. A
                    // non-activating panel is the shape overlay bars (Spotlight,
                    // Raycast) use, and it is allowed there.
                    //
                    // Swizzling the class of a live window is how tauri-nspanel
                    // does it too. Guarded: if NSPanel can't be found we simply
                    // keep the plain window rather than crash.
                    if let Some(panel_cls) = objc2::runtime::AnyClass::get(c"NSPanel") {
                        objc2::ffi::object_setClass(
                            ns_win as *mut objc2::runtime::AnyObject as *mut _,
                            panel_cls as *const _ as *mut _,
                        );
                        // NSWindowStyleMaskNonactivatingPanel (1<<7) is only
                        // honoured by NSPanel — hence the class change first.
                        let mask: usize = msg_send![ns_win, styleMask];
                        let _: () = msg_send![ns_win, setStyleMask: mask | (1 << 7)];
                        let _: () = msg_send![ns_win, setFloatingPanel: true];
                        // Accessory apps "deactivate" constantly; without this
                        // the panel would vanish every time you click elsewhere.
                        let _: () = msg_send![ns_win, setHidesOnDeactivate: false];
                        println!("[vox] pill is a non-activating NSPanel");
                    }

                    // canJoinAllSpaces (1<<0) — present on every desktop
                    // ignoresCycle (1<<6) — never a ⌘-Tab / ⌘-` destination
                    // fullScreenAuxiliary (1<<8) — allowed over fullscreen Spaces
                    //
                    // NOT stationary (1<<4): that pins the window like the
                    // desktop picture, which is the opposite of following you.
                    let behavior: usize = (1 << 0) | (1 << 6) | (1 << 8);
                    let _: () = msg_send![ns_win, setCollectionBehavior: behavior];
                    // NSPopUpMenuWindowLevel, not NSStatusWindowLevel (25):
                    // 25 sits at the same height as other apps' status windows,
                    // so activating one of those could bury the pill. 101 keeps
                    // it above them while staying below the screen saver and
                    // the lock screen.
                    let _: () = msg_send![ns_win, setLevel: PILL_WINDOW_LEVEL];
                    // Accessory-policy apps don't get their windows ordered
                    // front automatically — force it.
                    let _: () = msg_send![ns_win, orderFrontRegardless];
                    // Re-assert AFTER the class swizzle: setStyleMask on the
                    // freshly-converted panel can clear it, and losing it makes
                    // the pill undraggable.
                    let _: () = msg_send![ns_win, setMovable: true];

                    // Force the vibrancy material to render DARK.
                    //
                    // The grey cast wasn't the CSS tint — it was the
                    // NSVisualEffectView following the system appearance, so on
                    // a light desktop the blur itself came back light and no
                    // amount of lowering the CSS alpha could fix it (less alpha
                    // just showed MORE of the light blur). Pinning the window to
                    // the vibrant-dark appearance makes the material dark at the
                    // source, which is what lets the tint be thin AND the pill
                    // stay dark.
                    // Built through the runtime rather than pulling in
                    // objc2-foundation for one string.
                    if let (Some(str_cls), Some(app_cls)) = (
                        objc2::runtime::AnyClass::get(c"NSString"),
                        objc2::runtime::AnyClass::get(c"NSAppearance"),
                    ) {
                        let name: *mut objc2::runtime::AnyObject = msg_send![
                            str_cls,
                            stringWithUTF8String: c"NSAppearanceNameVibrantDark".as_ptr()
                        ];
                        let appearance: *mut objc2::runtime::AnyObject =
                            msg_send![app_cls, appearanceNamed: name];
                        if !appearance.is_null() {
                            let _: () = msg_send![ns_win, setAppearance: appearance];
                            println!("[vox] pill appearance pinned to vibrant dark");
                        }
                    }
                    // Dragging is driven from the renderer (the `move_pill`
                    // command), NOT by AppKit. A non-activating NSPanel never
                    // becomes key on click, so AppKit's background-drag loop
                    // doesn't engage — the pill became immovable the moment it
                    // was converted. Leaving this true as well would double
                    // every movement if AppKit ever did engage.
                    let _: () = msg_send![ns_win, setMovableByWindowBackground: false];
                    // (kept for reference) `data-tauri-drag-region`'s
                    // startDragging path is unreliable on a borderless status-
                    // level accessory window; AppKit's own background-drag is
                    // rock-solid and still lets clicks reach the webview
                    // controls (gear / selects).
                    let _: () = msg_send![ns_win, setMovableByWindowBackground: true];

                    let content: *mut AnyObject = msg_send![ns_win, contentView];
                    let subviews: *mut AnyObject = msg_send![content, subviews];
                    let count: usize = msg_send![subviews, count];
                    let mut views: Vec<(*mut AnyObject, String)> = Vec::with_capacity(count);
                    for i in 0..count {
                        let v: *mut AnyObject = msg_send![subviews, objectAtIndex: i];
                        let name = (*v).class().name().to_string_lossy().to_string();
                        views.push((v, name));
                    }
                    println!(
                        "[vox] view stack (bottom→top): {:?}",
                        views.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>()
                    );
                    for (v, name) in &views {
                        if name.contains("WebView") {
                            // Re-adding an attached subview moves it to the top
                            // of the sibling stack — above the vibrancy view.
                            let _: () = msg_send![content, addSubview: *v];
                            println!("[vox] raised {name} above vibrancy view");

                            // Round the webview's OWN backing layer to the pill
                            // radius so the GPU compositor clips its content at
                            // all times — including mid-resize. Without this the
                            // page's CSS `border-radius` + `overflow:hidden` is a
                            // paint-time clip that lags one frame behind the
                            // native frame during the hover grow, so the growing
                            // edge flashes square corners before the CSS repaints
                            // them round. A layer-level mask (same 28px circular
                            // curve window-vibrancy gives the blur view) removes
                            // that desync entirely — the corners are simply never
                            // square. (The mask bounds track the frame without a
                            // Core Animation tween: a layer-backed AppKit view
                            // resizes its layer synchronously outside an animation
                            // context, so nothing smears mid-step.)
                            let _: () = msg_send![*v, setWantsLayer: true];
                            let layer: *mut AnyObject = msg_send![*v, layer];
                            if !layer.is_null() {
                                let _: () = msg_send![layer, setCornerRadius: PILL_RADIUS];
                                let _: () = msg_send![layer, setMasksToBounds: true];
                            }
                        }
                    }
                }
            }

            position_bottom_center(&window);

            if let Err(e) = app.global_shortcut().register(alt_space) {
                eprintln!("[vox] failed to register Alt+Space: {e}");
            }
            if let Err(e) = app.global_shortcut().register(cmd_comma) {
                eprintln!("[vox] failed to register Cmd+,: {e}");
            }

            #[cfg(debug_assertions)]
            if std::env::var("VOX_DEVTOOLS").is_ok() {
                window.open_devtools();
            }

            // Accessory-policy apps sometimes fail to order their windows in
            // at launch — re-show shortly after startup.
            {
                let w = window.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(400));
                    let _ = w.show();
                    // Fire the entrance at the exact moment the window becomes
                    // visible. Running it at page load instead would play it
                    // while the window is still hidden — i.e. not at all.
                    let _ = w.emit("pill-shown", ());
                });
            }

            // Keep the pill on top across app switches. Activating another app
            // can re-order the window stack, and an accessory-policy app gets
            // no activation callback to react to — so re-assert on a slow tick.
            // orderFrontRegardless does NOT activate the app or steal focus, so
            // this is invisible unless it's actually needed.
            #[cfg(target_os = "macos")]
            {
                let w = window.clone();
                let h = app.handle().clone();
                // Two msg_sends per tick, so this can be prompt without being
                // wasteful. Fast enough that a re-appearance is animated while
                // it still reads as an entrance.
                std::thread::spawn(move || {
                    let seen = Arc::new(std::sync::atomic::AtomicBool::new(true));
                    loop {
                    std::thread::sleep(Duration::from_millis(400));
                    let w2 = w.clone();
                    let h2 = h.clone();
                    let seen2 = seen.clone();
                    let _ = w.run_on_main_thread(move || {
                        if let Ok(ptr) = w2.ns_window() {
                            use objc2::msg_send;
                            use objc2::runtime::AnyObject;
                            let ns_win = ptr as *mut AnyObject;
                            unsafe {
                                // IDEMPOTENT: only touch the window when it has
                                // actually drifted. setLevel re-orders the
                                // window, and re-ordering mid-drag aborts
                                // AppKit's background-drag loop — so an
                                // unconditional re-assert every 2s made the pill
                                // impossible to move with the mouse.
                                let want: usize = (1 << 0) | (1 << 6) | (1 << 8);
                                let behavior: usize = msg_send![ns_win, collectionBehavior];
                                if behavior != want {
                                    let _: () = msg_send![ns_win, setCollectionBehavior: want];
                                }
                                let level: isize = msg_send![ns_win, level];
                                if level != PILL_WINDOW_LEVEL {
                                    let _: () = msg_send![ns_win, setLevel: PILL_WINDOW_LEVEL];
                                    let _: () = msg_send![ns_win, orderFrontRegardless];
                                }

                                // Entrance animation trigger.
                                //
                                // A true Space-change hook is
                                // NSWorkspaceActiveSpaceDidChangeNotification,
                                // which needs a block or a custom observer
                                // class. These two PUBLIC properties are what
                                // can be read cheaply by polling instead:
                                // occlusionState drops its visible bit while the
                                // pill's Space is off-screen, and isOnActiveSpace
                                // reports the same thing from the other side.
                                // Either edge back to "on screen" is the moment
                                // to play the entrance.
                                let occl: usize = msg_send![ns_win, occlusionState];
                                let on_active: bool = msg_send![ns_win, isOnActiveSpace];
                                let visible = (occl & (1 << 1)) != 0 && on_active;
                                let was = seen2.swap(visible, Ordering::SeqCst);
                                if visible && !was {
                                    let _ = h2.emit("pill-appear", ());
                                }
                            }
                        }
                    });
                    }
                });
            }

            // Heavy init (python probing, model loads) off the main thread.
            let st = state.clone();
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                ensure_registry(&st.active_project.lock().unwrap().clone());
                // The announcement drainer runs regardless of whether the
                // speech stack ever comes up: events still fire and the queue
                // just holds until there is a voice to say them with.
                announce::start_drainer(handle.clone(), st.clone());
                watch::start(handle.clone(), st.clone());

                // Dev affordance: drive a full turn (LLM → tool → agent →
                // announcement) without a microphone. The mic needs a signed
                // bundle, so this is the only way to exercise the loop from a
                // `tauri dev` run.
                if let Ok(script) = std::env::var("VOX_DEBUG_SAY") {
                    let (st2, h2) = (st.clone(), handle.clone());
                    std::thread::spawn(move || {
                        // "first turn || 30 || second turn" — the number is a
                        // pause in seconds, so a launch can be followed by a
                        // real follow-up question while the agent is still
                        // running. One-shot couldn't test that at all.
                        for part in script.split("||") {
                            let part = part.trim();
                            if part.is_empty() {
                                continue;
                            }
                            if let Ok(secs) = part.parse::<u64>() {
                                std::thread::sleep(Duration::from_secs(secs));
                                continue;
                            }
                            std::thread::sleep(Duration::from_secs(3));
                            println!("[vox] VOX_DEBUG_SAY: {part}");
                            let _ = h2.emit("transcript", part);
                            llm::ask_ollama(&h2, &st2, part);
                        }
                    });
                }
                daemons::init_paths(&st, &handle);
                if setup::probe().get("needsSetup").and_then(|v| v.as_bool()).unwrap_or(false) {
                    // Speech stack not installed yet — hold the daemons and ask
                    // the renderer to auto-install from the setup panel.
                    println!("[vox] local speech stack incomplete — awaiting in-app setup");
                    let _ = handle.emit("setup-needed", setup::probe());
                } else {
                    daemons::start_stt(&st);
                    daemons::start_tts(&st, handle.clone());
                    println!("[vox] ready — Option+Space to activate, Cmd+, for settings");
                }
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running vox");
}
