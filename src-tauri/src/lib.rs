//! Vox — voice AI orchestration bar. Tauri backend.
//!
//! Why Tauri: the bar needs real-time desktop blur *clipped to the pill's
//! rounded corners*. window-vibrancy can round the native NSVisualEffectView
//! (radius param), which Electron cannot do without native modules — that was
//! the source of the "grey frame" overflow bug.

pub mod agents;
pub mod conductor;
pub mod daemons;
pub mod llm;
pub mod speech;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, WebviewWindow};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

pub const BAR_W: f64 = 480.0;
pub const BAR_H: f64 = 56.0;
const PILL_RADIUS: f64 = 28.0;

// ── Settings ─────────────────────────────────────────────────────────────────

#[derive(Clone, Serialize, Deserialize)]
pub struct Settings {
    pub model: String,
    pub language: String,
}

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
    Settings { model, language }
}

fn save_settings(s: &Settings) {
    let p = settings_path();
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    let _ = std::fs::write(p, serde_json::to_string_pretty(&json!({
        "model": s.model,
        "language": s.language,
    })).unwrap());
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
    pub agent_count: AtomicI64,
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

    const STEPS: u32 = 20;
    for i in 1..=STEPS {
        if state.anim_gen.load(Ordering::SeqCst) != gen {
            return; // superseded by a newer animation
        }
        let t = i as f64 / STEPS as f64;
        let e = 1.0 - (1.0 - t).powi(3);
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
async fn list_models() -> Vec<String> {
    tauri::async_runtime::spawn_blocking(|| {
        let resp = ureq::get("http://localhost:11434/api/tags")
            .timeout(Duration::from_secs(3))
            .call();
        resp.ok()
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
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default()
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
            save_settings(&settings);
        }
        if restart {
            // Reset conversation so the model doesn't carry wrong-language turns.
            st.history.lock().unwrap().clear();
            daemons::restart_daemons(&st, app);
        }
        let s = st.settings.lock().unwrap().clone();
        Ok(json!({ "model": s.model, "language": s.language, "restarted": restart }))
    })
    .await
    .map_err(|e| e.to_string())?
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
    let active_project = std::env::var("VOX_PROJECT")
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| home()).to_string_lossy().to_string());

    let state = Arc::new(AppState {
        settings: Mutex::new(settings),
        history: Mutex::new(Vec::new()),
        stt: Mutex::new(None),
        tts: Mutex::new(None),
        audio_done: Mutex::new(None),
        play_id: AtomicU64::new(0),
        agent_count: AtomicI64::new(0),
        active_project: Mutex::new(active_project),
        startup_brief_done: AtomicBool::new(false),
        interrupt_gen: AtomicU64::new(0),
        anim_gen: AtomicU64::new(0),
        paths: Mutex::new(Paths {
            stt_script: None,
            tts_script: None,
            tts_piper_script: None,
            stt_python: None,
            tts_python: None,
            whisper_cli: None,
            claude_cli: None,
        }),
        speak_lock: Mutex::new(()),
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
            voice_input,
            audio_done,
            resize_window,
            interrupt,
            replay_recap,
            get_version,
            check_update,
            open_url,
        ])
        .setup(move |app| {
            println!("[vox] starting pid={}", std::process::id());

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
                    // canJoinAllSpaces (1<<0) | fullScreenAuxiliary (1<<8)
                    let behavior: usize = (1 << 0) | (1 << 8);
                    let _: () = msg_send![ns_win, setCollectionBehavior: behavior];
                    let level: isize = 25; // NSStatusWindowLevel
                    let _: () = msg_send![ns_win, setLevel: level];
                    // Accessory-policy apps don't get their windows ordered
                    // front automatically — force it.
                    let _: () = msg_send![ns_win, orderFrontRegardless];
                    // Make the whole pill draggable. `data-tauri-drag-region`'s
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
                });
            }

            // Heavy init (python probing, model loads) off the main thread.
            let st = state.clone();
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                ensure_registry(&st.active_project.lock().unwrap().clone());
                daemons::init_paths(&st, &handle);
                daemons::start_stt(&st);
                daemons::start_tts(&st, handle.clone());
                println!("[vox] ready — Option+Space to activate, Cmd+, for settings");
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running vox");
}
