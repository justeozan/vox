//! Python daemon management (whisper STT + Kokoro/Piper TTS) and external tool
//! discovery. Both daemons speak a line-based stdin/stdout protocol and print
//! "ready" once their model is loaded — see resources/vox_stt.py / vox_tts.py / vox_tts_piper.py.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::Duration;

use tauri::{AppHandle, Manager};

use crate::{home, lang_config, AppState};

pub struct DaemonHandle {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<String>,
    ready: Arc<AtomicBool>,
    /// Consecutive request timeouts; two in a row marks the daemon unhealthy.
    timeouts: u32,
}

impl DaemonHandle {
    pub fn spawn(
        python: &str,
        script: &Path,
        envs: Vec<(String, String)>,
        tag: &'static str,
        on_ready: Option<Box<dyn Fn() + Send>>,
    ) -> std::io::Result<DaemonHandle> {
        let mut cmd = Command::new(python);
        // Don't let Python drop __pycache__ next to the bundled scripts: in dev
        // that directory lives under src-tauri/resources, which Tauri's file
        // watcher treats as a source change — daemon starts, writes bytecode,
        // watcher rebuilds, daemon restarts, forever. It also keeps the shipped
        // bundle free of stale .pyc files.
        cmd.env("PYTHONDONTWRITEBYTECODE", "1");
        cmd.arg(script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let (tx, rx) = channel::<String>();
        let ready = Arc::new(AtomicBool::new(false));
        let ready2 = ready.clone();

        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                if line == "ready" {
                    ready2.store(true, Ordering::SeqCst);
                    println!("[vox] {tag} daemon ready");
                    if let Some(cb) = on_ready.as_ref() {
                        cb();
                    }
                } else {
                    let _ = tx.send(line);
                }
            }
            // EOF on stdout means the python process is gone. Clearing `ready`
            // is what makes that survivable: `synthesize_daemon` short-circuits
            // on !is_ready() in microseconds, so Vox degrades to `say`. Left
            // set, every sentence would instead write into a dead pipe and
            // block the full 15s request timeout first — a five-sentence recap
            // becomes 75 seconds of near-silence.
            ready2.store(false, Ordering::SeqCst);
            println!("[vox] {tag} daemon exited");
        });
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let trimmed: String = line.chars().take(200).collect();
                eprintln!("[{tag}] {trimmed}");
            }
        });

        Ok(DaemonHandle { child, stdin, rx, ready, timeouts: 0 })
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }

    /// One request-response round trip. Drains stale responses first so a
    /// previously timed-out request can't satisfy this one.
    pub fn request(&mut self, line: &str, timeout: Duration) -> Option<String> {
        while self.rx.try_recv().is_ok() {}
        if writeln!(self.stdin, "{line}").is_err() || self.stdin.flush().is_err() {
            // Broken pipe — the process is gone but its stdout thread may not
            // have observed EOF yet. Mark it now so the next call is instant.
            self.ready.store(false, Ordering::SeqCst);
            return None;
        }
        match self.rx.recv_timeout(timeout) {
            Ok(line) => {
                self.timeouts = 0;
                Some(line)
            }
            Err(_) => {
                // Two, not one: a single genuinely slow sentence must not
                // condemn a healthy daemon. Two in a row means it's wedged
                // below the protocol layer (model hang, swap thrash) and every
                // further request would just burn another full timeout.
                self.timeouts += 1;
                if self.timeouts >= 2 {
                    eprintln!("[vox] daemon unresponsive after 2 timeouts — marking unhealthy");
                    self.ready.store(false, Ordering::SeqCst);
                }
                None
            }
        }
    }

    pub fn kill(&mut self) {
        self.ready.store(false, Ordering::SeqCst);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for DaemonHandle {
    /// Without this, an app crash orphans the python processes — and a resident
    /// speech model holds hundreds of MB.
    fn drop(&mut self) {
        self.kill();
    }
}

// ── External tool discovery ──────────────────────────────────────────────────
// GUI apps launched from Finder get a minimal PATH, so we search common
// install locations in addition to $PATH.

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.is_file()
        && p.metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

/// EVERY executable with this name on the search path, in order.
///
/// `find_bin` returns only the first, which is wrong whenever the first match
/// is unusable: `/usr/bin/python3` is Apple's 3.9 on every Mac and always wins
/// the race, so a version check against it rejected the name outright and never
/// looked at Homebrew's modern python further down the path.
pub fn find_bins(name: &str, extra: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect();
    dirs.extend_from_slice(extra);
    let h = home();
    dirs.push(h.join(".local/bin"));
    dirs.push(PathBuf::from("/opt/homebrew/bin"));
    dirs.push(PathBuf::from("/usr/local/bin"));
    dirs.push(PathBuf::from("/usr/bin"));
    let mut seen = std::collections::HashSet::new();
    dirs.into_iter()
        .map(|d| d.join(name))
        .filter(|p| is_executable(p))
        .filter(|p| seen.insert(p.clone()))
        .collect()
}

pub fn find_bin(name: &str, extra: &[PathBuf]) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect();
    dirs.extend_from_slice(extra);
    let h = home();
    dirs.push(h.join(".local/bin"));
    dirs.push(PathBuf::from("/opt/homebrew/bin"));
    dirs.push(PathBuf::from("/usr/local/bin"));
    dirs.push(PathBuf::from("/usr/bin"));
    dirs.into_iter().map(|d| d.join(name)).find(|p| is_executable(p))
}

fn python_with_module(candidates: &[PathBuf], module: &str) -> Option<PathBuf> {
    candidates.iter().find(|p| {
        is_executable(p)
            && Command::new(p)
                .args(["-c", &format!("import {module}")])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
    }).cloned()
}

/// Locate a bundled resource script, falling back to the source tree in dev.
pub fn resource_script(app: &AppHandle, name: &str) -> Option<PathBuf> {
    if let Ok(p) = app
        .path()
        .resolve(format!("resources/{name}"), tauri::path::BaseDirectory::Resource)
    {
        if p.exists() {
            return Some(p);
        }
    }
    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources").join(name);
    if dev.exists() {
        return Some(dev);
    }
    None
}

/// Resolve every external tool once at startup (runs on a background thread —
/// `import whisper` probing takes seconds).
pub fn init_paths(state: &Arc<AppState>, app: &AppHandle) {
    let h = home();

    let whisper_cli = find_bin(
        "whisper",
        &[
            h.join("Library/Python/3.9/bin"),
            h.join("Library/Python/3.11/bin"),
        ],
    );
    match &whisper_cli {
        Some(p) => println!("[vox] whisper: {}", p.display()),
        None => eprintln!("[vox] whisper not found — STT CLI fallback unavailable"),
    }

    // STT python: prefer the interpreter next to the whisper CLI (same env).
    let mut stt_candidates: Vec<PathBuf> = Vec::new();
    if let Some(w) = &whisper_cli {
        if let Some(dir) = w.parent() {
            stt_candidates.push(dir.join("python3.9"));
            stt_candidates.push(dir.join("python3"));
        }
    }
    for name in ["python3.9", "python3.10", "python3"] {
        if let Some(p) = find_bin(name, &[]) {
            stt_candidates.push(p);
        }
    }
    stt_candidates.push(h.join(".vox/venv/bin/python"));
    let stt_python = python_with_module(&stt_candidates, "whisper");
    match &stt_python {
        Some(p) => println!("[vox] stt python: {}", p.display()),
        None => eprintln!("[vox] no Python with openai-whisper found"),
    }

    // Kokoro python: the ~/.vox/venv is the canonical install.
    let mut tts_candidates: Vec<PathBuf> = vec![h.join(".vox/venv/bin/python")];
    for name in ["python3.11", "python3"] {
        if let Some(p) = find_bin(name, &[]) {
            tts_candidates.push(p);
        }
    }
    let tts_python = tts_candidates.iter().find(|p| is_executable(p)).cloned();

    let claude_cli = find_bin("claude", &[]);
    match &claude_cli {
        Some(p) => println!("[vox] claude: {}", p.display()),
        None => eprintln!("[vox] claude CLI not found — agents will fail"),
    }

    let mut paths = state.paths.lock().unwrap();
    paths.whisper_cli = whisper_cli;
    paths.stt_python = stt_python;
    paths.tts_python = tts_python;
    paths.claude_cli = claude_cli;
    paths.stt_script = resource_script(app, "vox_stt.py");
    paths.tts_script = resource_script(app, "vox_tts.py");
    paths.tts_piper_script = resource_script(app, "vox_tts_piper.py");
    paths.tts_qwen_script = resource_script(app, "vox_tts_qwen.py");
    let qwen_py = h.join(".vox/venv-qwen/bin/python");
    paths.tts_qwen_python = if is_executable(&qwen_py) { Some(qwen_py) } else { None };
}

pub fn start_stt(state: &Arc<AppState>) {
    let (script, python) = {
        let paths = state.paths.lock().unwrap();
        match (&paths.stt_script, &paths.stt_python) {
            (Some(s), Some(p)) => (s.clone(), p.to_string_lossy().to_string()),
            _ => {
                eprintln!("[vox] STT daemon unavailable (missing script or python)");
                return;
            }
        }
    };
    let lang = {
        let s = state.settings.lock().unwrap();
        lang_config(&s.language).stt.to_string()
    };
    println!("[vox] starting STT daemon (lang: {lang})...");
    match DaemonHandle::spawn(
        &python,
        &script,
        vec![("VOX_STT_LANG".into(), lang)],
        "stt",
        None,
    ) {
        Ok(d) => *state.stt.lock().unwrap() = Some(d),
        Err(e) => eprintln!("[vox] STT daemon spawn failed: {e}"),
    }
}

pub fn start_tts(state: &Arc<AppState>, app: AppHandle) {
    let engine = {
        let s = state.settings.lock().unwrap();
        crate::resolve_engine(&s)
    };
    start_tts_engine(state, app, engine);
}

/// Spawn one specific engine. Split out so a selected-but-unavailable engine
/// can fall back to the language default instead of leaving Vox mute — the
/// early return used to skip `on_ready`, which also silently killed the recap.
fn start_tts_engine(state: &Arc<AppState>, app: AppHandle, engine: &'static str) {
    let lang = {
        let s = state.settings.lock().unwrap();
        lang_config(&s.language)
    };
    let lang_code = lang.stt.to_string();

    // Cloned before the closure takes ownership: the fallback path below still
    // needs a handle to retry with another engine.
    let app_retry = app.clone();
    let st = state.clone();
    let on_ready: Box<dyn Fn() + Send> = Box::new(move || {
        let st = st.clone();
        let app = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(800));
            crate::conductor::speak_startup_brief(&app, &st);
        });
    });

    match engine {
        // No daemon at all, so `on_ready` never fires — kick the recap
        // directly or `say` mode would silently lose it.
        "say" => {
            println!("[vox] TTS engine = say (no daemon)");
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(800));
                on_ready();
            });
        }
        "qwen3" => {
            let (script, python) = {
                let paths = state.paths.lock().unwrap();
                match (&paths.tts_qwen_script, &paths.tts_qwen_python) {
                    (Some(s), Some(p)) => (s.clone(), p.to_string_lossy().to_string()),
                    _ => {
                        let fallback = lang.tts_engine;
                        eprintln!(
                            "[vox] Qwen3 TTS not installed — falling back to {fallback} \
                             (install it from the setup panel)"
                        );
                        drop(paths);
                        start_tts_engine(state, app_retry, fallback);
                        return;
                    }
                }
            };
            let venv = home().join(".vox/venv-qwen");
            let envs = vec![
                ("VIRTUAL_ENV".into(), venv.to_string_lossy().to_string()),
                ("HF_HOME".into(), home().join(".vox/hf").to_string_lossy().to_string()),
                ("VOX_TTS_LANG".into(), lang_code.clone()),
                (
                    "VOX_QWEN_MODEL".into(),
                    std::env::var("VOX_QWEN_MODEL").unwrap_or_else(|_| crate::setup::QWEN_MODEL_ID.into()),
                ),
                (
                    "VOX_QWEN_VOICE".into(),
                    std::env::var("VOX_QWEN_VOICE").unwrap_or_else(|_| "Serena".into()),
                ),
            ];
            println!("[vox] starting Qwen3 TTS daemon...");
            match DaemonHandle::spawn(&python, &script, envs, "qwen3", Some(on_ready)) {
                Ok(d) => *state.tts.lock().unwrap() = Some(d),
                Err(e) => eprintln!("[vox] Qwen3 TTS daemon spawn failed: {e}"),
            }
        }
        "piper" => {
            let (script, python) = {
                let paths = state.paths.lock().unwrap();
                match (&paths.tts_piper_script, &paths.tts_python) {
                    (Some(s), Some(p)) => (s.clone(), p.to_string_lossy().to_string()),
                    _ => {
                        eprintln!("[vox] Piper TTS daemon unavailable (missing script or python)");
                        return;
                    }
                }
            };
            let venv = home().join(".vox/venv");
            let model_path = home().join(".vox/voices").join(format!("{}.onnx", lang.piper_model));
            let envs = vec![
                ("VIRTUAL_ENV".into(), venv.to_string_lossy().to_string()),
                ("VOX_PIPER_MODEL".into(), model_path.to_string_lossy().to_string()),
                ("VOX_TTS_LANG".into(), lang_code.clone()),
            ];
            println!("[vox] starting Piper TTS daemon ({})...", lang.piper_model);
            match DaemonHandle::spawn(&python, &script, envs, "piper", Some(on_ready)) {
                Ok(d) => *state.tts.lock().unwrap() = Some(d),
                Err(e) => eprintln!("[vox] Piper TTS daemon spawn failed: {e}"),
            }
        }
        other => {
            if other != "kokoro" {
                eprintln!("[vox] unknown TTS engine {other:?} — falling back to Kokoro");
            }
            let (script, python) = {
                let paths = state.paths.lock().unwrap();
                match (&paths.tts_script, &paths.tts_python) {
                    (Some(s), Some(p)) => (s.clone(), p.to_string_lossy().to_string()),
                    _ => {
                        eprintln!("[vox] Kokoro TTS daemon unavailable (missing script or python)");
                        return;
                    }
                }
            };
            let venv = home().join(".vox/venv");
            let envs = vec![
                ("VIRTUAL_ENV".into(), venv.to_string_lossy().to_string()),
                ("VOX_KOKORO_LANG".into(), lang.kokoro_lang.to_string()),
                (
                    "VOX_KOKORO_VOICE".into(),
                    std::env::var("VOX_KOKORO_VOICE").unwrap_or_else(|_| lang.kokoro_voice.to_string()),
                ),
                ("VOX_TTS_LANG".into(), lang_code.clone()),
            ];
            println!("[vox] starting Kokoro TTS daemon ({})...", lang.kokoro_voice);
            match DaemonHandle::spawn(&python, &script, envs, "kokoro", Some(on_ready)) {
                Ok(d) => *state.tts.lock().unwrap() = Some(d),
                Err(e) => eprintln!("[vox] Kokoro TTS daemon spawn failed: {e}"),
            }
        }
    }
}

pub fn restart_daemons(state: &Arc<AppState>, app: AppHandle) {
    if let Some(d) = state.stt.lock().unwrap().as_mut() {
        d.kill();
    }
    *state.stt.lock().unwrap() = None;
    if let Some(d) = state.tts.lock().unwrap().as_mut() {
        d.kill();
    }
    *state.tts.lock().unwrap() = None;
    start_stt(state);
    start_tts(state, app);
}
