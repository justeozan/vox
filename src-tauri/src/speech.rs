//! STT + TTS pipeline: whisper daemon transcription (with CLI fallback) and
//! Kokoro/Piper synthesis played by the renderer (echo cancellation → barge-in).
//!
//! Speech is QUEUED and STREAMED: a session receives sentences one by one
//! (from a streaming LLM or a pre-built brief) and synthesizes sentence N+1
//! while sentence N plays. The voice starts on the first complete sentence
//! instead of waiting for the full text.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{channel, Receiver, Sender};
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
            // NoExpand: insert the phonetic value literally — a '$' in it must
            // not be read as a capture-group reference.
            out = re.replace_all(&out, regex::NoExpand(phonetic.as_str())).to_string();
        }
    }
    out
}

// ── Sentence streaming ───────────────────────────────────────────────────────

/// One unit of a queued speech session.
pub enum SpeechItem {
    /// A sentence to synthesize and play, optionally tagged with the
    /// Conductor project it talks about (drives the renderer carousel).
    Sentence { text: String, workspace: Option<String> },
    /// No more sentences will come; the session ends after the queue drains.
    End,
}

/// Don't cut fragments shorter than this — per-sentence synthesis has a fixed
/// cost, and "Oui." alone sounds choppier than "Oui. Je m'en occupe."
const MIN_SENTENCE_CHARS: usize = 12;

/// Byte index just after a sentence boundary (end punctuation followed by
/// whitespace) that yields a fragment of at least `min` chars. Decimals like
/// "2.5" survive because the '.' isn't followed by whitespace.
fn find_boundary(s: &str, min: usize) -> Option<usize> {
    let mut prev_end = false;
    for (i, c) in s.char_indices() {
        if prev_end && c.is_whitespace() && s[..i].trim().chars().count() >= min {
            return Some(i);
        }
        prev_end = matches!(c, '.' | '!' | '?' | '…' | ':');
    }
    None
}

/// Incremental sentence splitter for streamed LLM output: feed deltas, get
/// back completed sentences as soon as their end punctuation arrives.
pub struct SentenceBuffer {
    buf: String,
}

impl Default for SentenceBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl SentenceBuffer {
    pub fn new() -> Self {
        SentenceBuffer { buf: String::new() }
    }

    pub fn push(&mut self, delta: &str) -> Vec<String> {
        self.buf.push_str(delta);
        let mut out = Vec::new();
        while let Some(cut) = find_boundary(&self.buf, MIN_SENTENCE_CHARS) {
            let sentence: String = self.buf.drain(..cut).collect();
            let s = sentence.trim().to_string();
            if !s.is_empty() {
                out.push(s);
            }
        }
        out
    }

    pub fn flush(&mut self) -> Option<String> {
        let s = self.buf.trim().to_string();
        self.buf.clear();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }
}

/// Split a complete text into speakable sentences.
pub fn split_sentences(text: &str) -> Vec<String> {
    let mut b = SentenceBuffer::new();
    let mut out = b.push(text);
    if let Some(rest) = b.flush() {
        out.push(rest);
    }
    out
}

// ── STT ──────────────────────────────────────────────────────────────────────

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

// ── TTS ──────────────────────────────────────────────────────────────────────

/// Synthesize one sentence through the active TTS daemon (Kokoro or Piper —
/// both speak the same line protocol).
fn synthesize_daemon(state: &Arc<AppState>, text: &str) -> Option<PathBuf> {
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
        eprintln!("[vox] tts daemon error: {resp}");
        None
    }
}

fn say_fallback(state: &Arc<AppState>, text: &str) {
    let voice = std::env::var("VOX_SAY_VOICE").unwrap_or_else(|_| {
        lang_config(&state.settings.lock().unwrap().language).say_voice.to_string()
    });
    let _ = Command::new("say").args(["-v", &voice, text]).status();
}

// ── Queued speech session ────────────────────────────────────────────────────

enum PlayMsg {
    Wav { path: PathBuf, workspace: Option<String> },
    Say { text: String },
    End,
}

/// Begin a queued speech session. Push sentences on the returned channel as
/// they become available and finish with `SpeechItem::End`. The session emits
/// speaking-start before the first sound and exactly one speaking-done at the
/// end, honors `state.interrupt` at every step, and synthesizes one sentence
/// ahead of playback.
pub fn start_session(app: AppHandle, state: Arc<AppState>) -> Sender<SpeechItem> {
    let (tx, rx) = channel::<SpeechItem>();
    std::thread::spawn(move || run_session(app, state, rx));
    tx
}

fn run_session(app: AppHandle, state: Arc<AppState>, rx: Receiver<SpeechItem>) {
    // Serialize sessions — a second speak while one is running waits its turn.
    let _guard = state.speak_lock.lock().unwrap();

    let engine = std::env::var("VOX_TTS")
        .unwrap_or_else(|_| {
            lang_config(&state.settings.lock().unwrap().language).tts_engine.to_string()
        })
        .to_lowercase();
    let use_daemon = engine != "say";

    // Wait for the daemon once per session (first call after boot loads the
    // model). Interrupt aborts the wait.
    if use_daemon {
        for _ in 0..80 {
            if state.interrupt.load(Ordering::SeqCst) {
                break;
            }
            let ready = state.tts.lock().unwrap().as_ref().map(|d| d.is_ready()).unwrap_or(false);
            if ready {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    // Synth stage: at most 2 sentences ahead of playback.
    let (ptx, prx) = std::sync::mpsc::sync_channel::<PlayMsg>(2);
    let synth_state = state.clone();
    let synth = std::thread::spawn(move || {
        for item in rx {
            if synth_state.interrupt.load(Ordering::SeqCst) {
                break;
            }
            match item {
                SpeechItem::End => break,
                SpeechItem::Sentence { text, workspace } => {
                    println!("[vox] 🔊 {text}");
                    let spoken = apply_pronunciations(&text);
                    let msg = if use_daemon {
                        match synthesize_daemon(&synth_state, &spoken) {
                            Some(path) => PlayMsg::Wav { path, workspace },
                            None => PlayMsg::Say { text: spoken },
                        }
                    } else {
                        PlayMsg::Say { text: spoken }
                    };
                    if ptx.send(msg).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = ptx.send(PlayMsg::End);
    });

    // Play stage.
    let mut started = false;
    for msg in prx {
        let interrupted = state.interrupt.load(Ordering::SeqCst);
        match msg {
            PlayMsg::End => break,
            PlayMsg::Wav { path, workspace } => {
                if interrupted {
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
                if !started {
                    let _ = app.emit("speaking-start", ());
                    started = true;
                }
                let (tx_done, rx_done) = channel::<()>();
                *state.audio_done.lock().unwrap() = Some(tx_done);
                let _ = app.emit(
                    "play-wav",
                    serde_json::json!({ "path": path.to_string_lossy(), "workspace": workspace }),
                );
                let _ = rx_done.recv_timeout(Duration::from_secs(120));
                *state.audio_done.lock().unwrap() = None;
                let _ = std::fs::remove_file(&path);
            }
            PlayMsg::Say { text } => {
                if interrupted {
                    continue;
                }
                if !started {
                    let _ = app.emit("speaking-start", ());
                    started = true;
                }
                say_fallback(&state, &text);
            }
        }
    }
    let _ = synth.join();
    let _ = app.emit("speaking-done", ());
}

/// Speak a complete text (split into sentences, queued, non-blocking). The
/// renderer is driven by speaking-start / play-wav / speaking-done events.
pub fn speak(app: &AppHandle, state: &Arc<AppState>, text: &str) {
    if state.interrupt.load(Ordering::SeqCst) {
        let _ = app.emit("speaking-done", ());
        return;
    }
    let tx = start_session(app.clone(), state.clone());
    for s in split_sentences(text) {
        let _ = tx.send(SpeechItem::Sentence { text: s, workspace: None });
    }
    let _ = tx.send(SpeechItem::End);
}
