//! First-run setup: probe the local speech stack (Python, venv, Whisper,
//! Kokoro, Piper, voices) and auto-install what's missing into `~/.vox/venv`
//! with progress streamed to the renderer. No separate install script needed.
//!
//! System tools (ffmpeg, espeak-ng, Ollama) can't be bundled — they're probed
//! and surfaced in the setup panel with a brew hint.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

use crate::{home, AppState};

pub const VENV_DIR: &str = ".vox/venv";
/// Qwen3-TTS gets its OWN virtualenv. The point is isolation: this is an engine
/// under evaluation, and a failed experiment must not be able to disturb the
/// Kokoro/Piper install it is being compared against. (mlx-audio also pins
/// `transformers>=5.14` and `huggingface_hub>=1.0`, which pip would happily
/// resolve underneath Kokoro.)
pub const QWEN_VENV_DIR: &str = ".vox/venv-qwen";
const QWEN_PIP_DEPS: &[&str] = &["mlx-audio>=0.4.7", "soundfile", "num2words"];
pub const QWEN_MODEL_ID: &str = "mlx-community/Qwen3-TTS-12Hz-0.6B-CustomVoice-4bit";

pub fn qwen_python() -> PathBuf {
    home().join(QWEN_VENV_DIR).join("bin/python")
}
const PIP_DEPS: &[&str] = &["openai-whisper", "numpy", "soundfile", "kokoro", "piper-tts", "num2words"];
const PIPER_BASE: &str = "fr_FR-siwis-medium";
const PIPER_URL: &str = "https://huggingface.co/rhasspy/piper-voices/resolve/main/fr/fr_FR/siwis/medium/fr_FR-siwis-medium.onnx";

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.is_file()
        && p.metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

pub fn venv_python() -> PathBuf {
    home().join(".vox/venv/bin/python")
}

fn comp(ok: bool, detail: impl Into<String>) -> Value {
    json!({ "ok": ok, "detail": detail.into() })
}

/// Locate a system Python 3.11+ (needed to create the venv).
/// Lowest supported Python for the speech stack.
const PY_MIN: (u32, u32) = (3, 11);
/// First version that BREAKS it. `kokoro` requires `<3.13`, and it pulls in
/// spacy -> thinc -> blis, none of which publish wheels for 3.13+ — pip then
/// tries to compile Cython and the whole install dies. Measured, not assumed.
const PY_BREAKS_AT: (u32, u32) = (3, 13);

fn py_version(p: &std::path::Path) -> Option<(u32, u32)> {
    let out = Command::new(p)
        .args(["-c", "import sys; v=sys.version_info; print('%d.%d' % (v[0], v[1]))"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let mut parts = s.trim().split('.');
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ))
}

/// A Python the speech stack can actually install into.
///
/// Two passes, and the order matters more than it looks. Searching newest-first
/// picked 3.14 on a stock Homebrew Mac and the install failed outright; the
/// known-good window is preferred instead, and a too-new interpreter is only
/// used as a last resort, with a warning, so the user gets *something* rather
/// than a hard stop.
pub fn find_python() -> Option<PathBuf> {
    let names = ["python3.12", "python3.11", "python3.13", "python3.14", "python3"];
    let mut fallback: Option<(PathBuf, (u32, u32))> = None;

    for name in names {
        for p in crate::daemons::find_bins(name, &[]) {
            let Some(v) = py_version(&p) else { continue };
            if v < PY_MIN {
                continue;
            }
            if v < PY_BREAKS_AT {
                return Some(p);
            }
            if fallback.is_none() {
                fallback = Some((p, v));
            }
        }
    }

    if let Some((p, v)) = fallback {
        eprintln!(
            "[vox] only Python {}.{} found; the speech stack wants {}.{}-{}.{} \
             and may fail to build",
            v.0, v.1, PY_MIN.0, PY_MIN.1, PY_BREAKS_AT.0, PY_BREAKS_AT.1 - 1
        );
        return Some(p);
    }
    None
}

/// Check a module is importable in the given interpreter WITHOUT importing it
/// (find_spec is fast — `import whisper` itself takes seconds).
fn venv_has(py: &Path, module: &str) -> bool {
    let code = format!(
        "import importlib.util,sys; sys.exit(0 if importlib.util.find_spec('{module}') else 1)"
    );
    Command::new(py)
        .args(["-c", &code])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ── Probe ────────────────────────────────────────────────────────────────────

/// Report the health of every component Vox needs. The renderer renders this
/// as a chip grid; `needsSetup` drives whether the install button is relevant.
pub fn probe() -> Value {
    let h = home();
    let venv_py = venv_python();
    let has_venv = is_executable(&venv_py);
    let whisper = has_venv && venv_has(&venv_py, "whisper");
    let kokoro = has_venv && venv_has(&venv_py, "kokoro");
    let piper = has_venv && venv_has(&venv_py, "piper");
    let py = find_python();
    let voice = h.join(".vox/voices").join(format!("{PIPER_BASE}.onnx"));

    let need = |s: &str| s.to_string();
    let ffmpeg = crate::daemons::find_bin("ffmpeg", &[]).is_some();
    let espeak = crate::daemons::find_bin("espeak-ng", &[]).is_some();
    let ollama = crate::daemons::find_bin("ollama", &[]).is_some();
    let qwen_py = qwen_python();
    let qwen = is_executable(&qwen_py) && venv_has(&qwen_py, "mlx_audio");
    let qwen_weights = h
        .join(".vox/hf/hub")
        .join(format!("models--{}", QWEN_MODEL_ID.replace('/', "--")))
        .exists();

    json!({
        "components": {
            "python": comp(py.is_some(), &py.map(|p| p.display().to_string()).unwrap_or_else(|| need("brew install python@3.11"))),
            "ffmpeg": comp(ffmpeg, if ffmpeg { need("ok") } else { need("brew install ffmpeg") }),
            "espeakNg": comp(espeak, if espeak { need("ok") } else { need("brew install espeak-ng") }),
            "ollama": comp(ollama, if ollama { need("ok") } else { need("brew install ollama") }),
            "venv": comp(has_venv, venv_py.display().to_string()),
            "whisper": comp(whisper, need("openai-whisper")),
            "kokoro": comp(kokoro, need("kokoro")),
            "piper": comp(piper, need("piper-tts")),
            "piperVoice": comp(voice.exists(), voice.display().to_string()),
            "qwen3": comp(qwen, if qwen { need("mlx-audio") } else { need("optionnel — moteur en test") }),
            "qwen3Model": comp(qwen_weights, if qwen_weights { need("ok") } else { need("~1,7 Go") }),
        },
        // Qwen deliberately does NOT gate this. Anything ANDed here becomes
        // mandatory and re-triggers the installer at every launch; an optional
        // engine under evaluation must never do that.
        "needsSetup": !(whisper && kokoro && piper),
        "qwenReady": qwen && qwen_weights,
    })
}

// ── Progress streaming ───────────────────────────────────────────────────────

static ANSI_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]").unwrap());

fn clean_line(line: &str) -> Option<String> {
    let s = ANSI_RE.replace_all(line, "").to_string().replace('\r', "");
    let s = s.trim().to_string();
    if s.is_empty() {
        return None;
    }
    Some(s.chars().take(200).collect())
}

/// Progress goes to the setup panel AND to stdout.
///
/// It used to be renderer-only, which made first-run install the one flow in
/// the app with no trace in the logs: if it failed — or never started — there
/// was nothing to look at. That is exactly the flow a brand-new user hits.
pub fn emit_log(app: &AppHandle, tag: &str, line: &str) {
    if let Some(s) = clean_line(line) {
        println!("[setup:{tag}] {s}");
    let _ = app.emit("setup-log", json!({ "tag": tag, "line": s }));
    }
}

fn stream_lines(child: &mut std::process::Child, app: &AppHandle, tag: &'static str) {
    if let Some(out) = child.stdout.take() {
        let handle = app.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                emit_log(&handle, tag, &line);
            }
        });
    }
    if let Some(err) = child.stderr.take() {
        let handle = app.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                emit_log(&handle, tag, &line);
            }
        });
    }
}

fn run_quiet(py: &Path, args: &[&str], tag: &'static str, app: &AppHandle) -> Result<(), String> {
    let mut child = Command::new(py)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    stream_lines(&mut child, app, tag);
    let status = child.wait().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{tag} exited with {status}"))
    }
}

fn download(app: &AppHandle, url: &str, dest: &Path) -> Result<(), String> {
    let resp = ureq::get(url)
        .set("User-Agent", "vox-setup")
        .timeout(Duration::from_secs(600))
        .call()
        .map_err(|e| format!("{url}: {e}"))?;
    let total: u64 = resp
        .header("Content-Length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let part = dest.with_extension("part");
    let mut out = std::fs::File::create(&part).map_err(|e| e.to_string())?;
    let mut buf = [0u8; 32768];
    let mut done: u64 = 0;
    let mut last_pct: u32 = 0;
    let mut reader = resp.into_reader();
    loop {
        let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        done += n as u64;
        if total > 0 {
            let pct = ((done as f64 / total as f64) * 100.0) as u32;
            if pct - last_pct >= 5 {
                last_pct = pct;
                emit_log(app, "voice", &format!("… {pct}%"));
            }
        }
    }
    out.flush().map_err(|e| e.to_string())?;
    std::fs::rename(&part, dest).map_err(|e| e.to_string())?;
    emit_log(app, "voice", "Voix téléchargée ✓");
    Ok(())
}

// ── Run setup ────────────────────────────────────────────────────────────────

/// Create `~/.vox/venv`, pip-install Whisper/Kokoro/Piper, and fetch the Piper
/// French voice model. Idempotent — safe to re-run; missing pieces only.
/// Streams progress to the renderer through `setup-log` events.
pub fn run_setup(app: &AppHandle, _state: &Arc<AppState>) -> Result<Value, String> {
    println!("[vox] run_setup invoked");
    let h = home();
    let vox = h.join(".vox");
    let venv = vox.join("venv");
    let venv_py = venv.join("bin/python");

    emit_log(app, "setup", "Démarrage de l'installation…");

    let py = find_python().ok_or_else(|| {
        emit_log(
            app,
            "setup",
            "Python 3.12 introuvable — installe-le puis relance : brew install python@3.12",
        );
        "Python 3.11+ not found".to_string()
    })?;
    emit_log(app, "setup", &format!("Python : {}", py.display()));

    std::fs::create_dir_all(&vox).map_err(|e| e.to_string())?;

    if !is_executable(&venv_py) {
        emit_log(app, "setup", "Création de l'environnement virtuel…");
        run_quiet(&py, &["-m", "venv", venv.to_str().unwrap_or_default()], "venv", app)
            .map_err(|e| {
                emit_log(app, "setup", &format!("Échec de création du venv : {e}"));
                e
            })?;
    }

    emit_log(app, "setup", "Mise à jour de pip…");
    run_quiet(
        &venv_py,
        &[
            "-m",
            "pip",
            "install",
            "--upgrade",
            "pip",
            "--disable-pip-version-check",
            "--no-input",
            "--progress-bar",
            "off",
        ],
        "pip",
        app,
    )
    .map_err(|e| {
        emit_log(app, "setup", &format!("Échec de pip : {e}"));
        e
    })?;

    emit_log(app, "setup", "Installation de Whisper, Kokoro et Piper (quelques minutes)…");
    run_quiet(
        &venv_py,
        &[
            "-m",
            "pip",
            "install",
            "--disable-pip-version-check",
            "--no-input",
            "--progress-bar",
            "off",
        ]
        .into_iter()
        .chain(PIP_DEPS.iter().copied())
        .collect::<Vec<_>>(),
        "pip",
        app,
    )
    .map_err(|e| {
        emit_log(app, "setup", &format!("Échec d'installation des dépendances : {e}"));
        e
    })?;

    // Piper French voice (the .onnx + sidecar config). Kokoro pulls its own
    // voices on first load; Whisper downloads its model on first daemon start.
    let voices = vox.join("voices");
    std::fs::create_dir_all(&voices).map_err(|e| e.to_string())?;
    let onnx = voices.join(format!("{PIPER_BASE}.onnx"));
    let js = voices.join(format!("{PIPER_BASE}.onnx.json"));
    if !onnx.exists() {
        emit_log(app, "voice", "Téléchargement de la voix Piper (63 Mo)…");
        download(app, PIPER_URL, &onnx).map_err(|e| {
            emit_log(app, "voice", &format!("Échec du téléchargement : {e}"));
            e
        })?;
    }
    if !js.exists() {
        download(app, &format!("{PIPER_URL}.json"), &js).map_err(|e| {
            emit_log(app, "voice", &format!("Échec du téléchargement : {e}"));
            e
        })?;
    }

    emit_log(app, "setup", "Installation terminée ✓");
    Ok(probe())
}

// ── Optional engines ─────────────────────────────────────────────────────────

/// Install Qwen3-TTS into its own virtualenv, then pull the weights.
///
/// Kept entirely separate from `run_setup`: this is opt-in, it must never gate
/// launch, and it must not be able to disturb the working speech stack.
pub fn install_qwen(app: &AppHandle) -> Result<Value, String> {
    let venv = home().join(QWEN_VENV_DIR);
    let venv_py = qwen_python();
    emit_log(app, "qwen", "Installation du moteur Qwen3-TTS (environnement séparé)…");

    let py = find_python().ok_or_else(|| {
        "Python 3.11+ introuvable — installe-le (brew install python@3.11)".to_string()
    })?;
    if !is_executable(&venv_py) {
        emit_log(app, "qwen", "Création de ~/.vox/venv-qwen…");
        run_quiet(&py, &["-m", "venv", &venv.to_string_lossy()], "qwen", app)?;
    }
    run_quiet(
        &venv_py,
        &["-m", "pip", "install", "--upgrade", "pip", "--disable-pip-version-check",
          "--no-input", "--progress-bar", "off"],
        "qwen",
        app,
    )?;

    emit_log(app, "qwen", "Installation de mlx-audio (quelques minutes)…");
    let args: Vec<&str> = ["-m", "pip", "install", "--disable-pip-version-check",
                           "--no-input", "--progress-bar", "off"]
        .into_iter()
        .chain(QWEN_PIP_DEPS.iter().copied())
        .collect();
    run_quiet(&venv_py, &args, "qwen", app)?;

    // Loading the model IS the download — which also proves the checkpoint
    // actually loads on this machine before it can ever be selected. Let
    // huggingface_hub handle the sharded, resumable transfer; `download()`
    // stays the right tool for the single-file Piper voice.
    emit_log(app, "qwen", "Téléchargement du modèle Qwen3-TTS (~1,7 Go)…");
    let mut child = Command::new(&venv_py)
        .args([
            "-c",
            "import sys; from mlx_audio.tts.utils import load_model; \
             load_model(sys.argv[1]); print('loaded', file=sys.stderr)",
            QWEN_MODEL_ID,
        ])
        .env("HF_HOME", home().join(".vox/hf"))
        .env("HF_HUB_DISABLE_TELEMETRY", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    stream_lines(&mut child, app, "qwen");
    if !child.wait().map_err(|e| e.to_string())?.success() {
        return Err("le téléchargement du modèle Qwen3-TTS a échoué".into());
    }

    emit_log(app, "qwen", "Qwen3-TTS prêt ✓");
    Ok(probe())
}
