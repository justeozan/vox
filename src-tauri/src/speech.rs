//! STT + TTS pipeline: whisper daemon transcription (with CLI fallback) and
//! Kokoro/Piper synthesis played by the renderer (echo cancellation → barge-in).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::channel;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use regex::Regex;
use tauri::{AppHandle, Emitter};

use crate::{lang_config, AppState};

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()
}

/// Rewrite tricky words with the user's phonetic respellings before the text
/// hits the voice engine. Whole-word, case-insensitive.
fn apply_pronunciations(text: &str) -> String {
    let pairs = crate::load_pronunciations();
    if pairs.is_empty() {
        return text.to_string();
    }
    let mut out = text.to_string();
    for (word, phonetic) in pairs {
        let pat = format!(r"(?i)\b{}\b", regex::escape(&word));
        if let Ok(re) = Regex::new(&pat) {
            out = re.replace_all(&out, phonetic.as_str()).to_string();
        }
    }
    out
}

pub fn transcribe(state: &Arc<AppState>, wav: &Path) -> String {
    // Prefer daemon (in-memory model, no CLI cold start)
    {
        let mut guard = state.stt.lock().unwrap();
        if let Some(d) = guard.as_mut() {
            if d.is_ready() {
                if let Some(line) = d.request(&wav.to_string_lossy(), Duration::from_secs(60)) {
                    if line == "EMPTY" || line.starts_with("ERROR:") {
                        return String::new();
                    }
                    println!("[vox] transcript (daemon): {line}");
                    return line;
                }
            }
        }
    }

    // Fallback: whisper CLI
    let whisper = state.paths.lock().unwrap().whisper_cli.clone();
    let Some(whisper) = whisper else {
        eprintln!("[vox] no STT available");
        return String::new();
    };
    let lang = lang_config(&state.settings.lock().unwrap().language).stt.to_string();
    let tmpdir = std::env::temp_dir();
    let status = Command::new(&whisper)
        .args([
            wav.to_string_lossy().as_ref(),
            "--model", "small",
            "--language", &lang,
            "--output_format", "txt",
            "--output_dir", tmpdir.to_string_lossy().as_ref(),
        ])
        .status();
    if !status.map(|s| s.success()).unwrap_or(false) {
        return String::new();
    }
    let txt = wav.with_extension("txt");
    let transcript = std::fs::read_to_string(&txt).unwrap_or_default().trim().to_string();
    let _ = std::fs::remove_file(&txt);
    println!("[vox] transcript (cli): {transcript}");
    transcript
}

fn synthesize_kokoro(state: &Arc<AppState>, text: &str) -> Option<PathBuf> {
    let mut guard = state.tts.lock().unwrap();
    let d = guard.as_mut()?;
    if !d.is_ready() {
        return None;
    }
    let out = std::env::temp_dir().join(format!("vox_tts_{}.wav", now_ms()));
    // Protocol is line-based (path\ttext\n). Embedded newlines would be read
    // as separate lines by the daemon, truncating synthesis.
    let flat = text.replace(['\r', '\n'], ". ");
    let flat = Regex::new(r"\.\s*\.+").unwrap().replace_all(&flat, ".").to_string();
    let resp = d.request(&format!("{}\t{}", out.display(), flat), Duration::from_secs(15))?;
    if resp.starts_with("ok:") {
        Some(out)
    } else {
        eprintln!("[vox] kokoro error: {resp}");
        None
    }
}

fn say_fallback(state: &Arc<AppState>, text: &str) {
    let voice = std::env::var("VOX_SAY_VOICE").unwrap_or_else(|_| {
        lang_config(&state.settings.lock().unwrap().language).say_voice.to_string()
    });
    let _ = Command::new("say").args(["-v", &voice, text]).status();
}

pub fn speak(app: &AppHandle, state: &Arc<AppState>, text: &str) {
    let _guard = state.speak_lock.lock().unwrap();
    println!("[vox] 🔊 {text}");

    // What the voice actually pronounces (user's phonetic overrides applied).
    let spoken = apply_pronunciations(text);
    let text = spoken.as_str();

    let engine = std::env::var("VOX_TTS").unwrap_or_else(|_| {
        lang_config(&state.settings.lock().unwrap().language).tts_engine.to_string()
    }).to_lowercase();
    if engine == "say" {
        let _ = app.emit("speaking-start", ());
        say_fallback(state, text);
        let _ = app.emit("speaking-done", ());
        return;
    }

    // Wait up to 8s for Kokoro to finish loading on first call
    for _ in 0..80 {
        let ready = state
            .tts
            .lock()
            .unwrap()
            .as_ref()
            .map(|d| d.is_ready())
            .unwrap_or(false);
        if ready {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    if let Some(wav) = synthesize_kokoro(state, text) {
        let (tx, rx) = channel::<()>();
        *state.audio_done.lock().unwrap() = Some(tx);
        let _ = app.emit("speaking-start", ());
        // Renderer plays the wav (echo cancellation works there) and calls
        // audio_done when finished or barged-in.
        let _ = app.emit("play-wav", wav.to_string_lossy().to_string());
        let _ = rx.recv_timeout(Duration::from_secs(180));
        *state.audio_done.lock().unwrap() = None;
        let _ = std::fs::remove_file(&wav);
        let _ = app.emit("speaking-done", ());
        return;
    }

    // Fallback: say (no barge-in — bypasses browser echo cancellation)
    let _ = app.emit("speaking-start", ());
    say_fallback(state, text);
    let _ = app.emit("speaking-done", ());
}
