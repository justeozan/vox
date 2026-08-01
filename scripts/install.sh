#!/usr/bin/env bash
# Vox setup script — creates ~/.vox/venv and installs the local STT/TTS deps
# (Whisper, Kokoro, Piper). No cloud calls, everything runs on your machine.
#
# Safe to re-run: it only creates the venv if missing and pip install is
# idempotent.
set -euo pipefail

VOX_HOME="$HOME/.vox"
VENV_DIR="$VOX_HOME/venv"

log() { printf '\033[1;32m==>\033[0m %s\n' "$1"; }
warn() { printf '\033[1;33m!!\033[0m %s\n' "$1"; }
die() { printf '\033[1;31mERROR\033[0m %s\n' "$1" >&2; exit 1; }

log "Setting up Vox in ${VOX_HOME}"

# ── macOS check ──────────────────────────────────────────────────────────────
if [[ "$(uname -s)" != "Darwin" ]]; then
    die "Vox is macOS-only (found $(uname -s)). See github.com/justeozan/vox for details."
fi
log "macOS detected: $(sw_vers -productVersion 2>/dev/null || echo unknown)"

# ── Python 3.11+ check ───────────────────────────────────────────────────────
find_python() {
    local candidate version major minor
    for candidate in python3.13 python3.12 python3.11 python3; do
        if command -v "$candidate" >/dev/null 2>&1; then
            version="$("$candidate" -c 'import sys; print("%d.%d" % sys.version_info[:2])' 2>/dev/null || true)"
            major="${version%%.*}"
            minor="${version##*.}"
            if [[ -n "$major" && -n "$minor" ]] && (( major == 3 && minor >= 11 )); then
                echo "$candidate"
                return 0
            fi
        fi
    done
    return 1
}

if ! PYTHON_BIN="$(find_python)"; then
    warn "Python 3.11+ not found."
    warn "Install it with: brew install python@3.11"
    die "cannot continue without Python 3.11+"
fi
log "Using $PYTHON_BIN ($($PYTHON_BIN --version))"

# ── ffmpeg check (Whisper decodes/resamples audio through it) ──────────────
if ! command -v ffmpeg >/dev/null 2>&1; then
    warn "ffmpeg not found — Whisper STT needs it to read audio."
    warn "Install it with: brew install ffmpeg"
fi

# ── espeak-ng check (Kokoro's French voice phonemizes through it) ──────────
if ! command -v espeak-ng >/dev/null 2>&1; then
    warn "espeak-ng not found — the Kokoro French voice needs it."
    warn "Install it with: brew install espeak-ng"
fi

# ── Ollama check (the brain) ─────────────────────────────────────────────────
if ! command -v ollama >/dev/null 2>&1; then
    warn "Ollama not found — Vox needs it to run the LLM locally."
    warn "Install it with: brew install ollama"
    warn "Then pull a model with: ollama pull qwen2.5:3b"
else
    log "Ollama found: $(command -v ollama)"
fi

# ── Create the venv ──────────────────────────────────────────────────────────
if [[ -d "$VENV_DIR" ]]; then
    log "Reusing existing venv at ${VENV_DIR}"
else
    log "Creating venv at ${VENV_DIR}"
    mkdir -p "$VOX_HOME"
    "$PYTHON_BIN" -m venv "$VENV_DIR"
fi

VENV_PY="$VENV_DIR/bin/python"
VENV_PIP="$VENV_DIR/bin/pip"

log "Upgrading pip"
"$VENV_PY" -m pip install --upgrade pip --quiet

# ── Install the exact deps the resource scripts import ──────────────────────
# vox_stt.py:        whisper                -> openai-whisper
# vox_tts.py:         numpy, soundfile, kokoro, (num2words)
# vox_tts_piper.py:   piper, piper.config    -> piper-tts, (num2words)
log "Installing Python dependencies (this can take a few minutes on first run)"
"$VENV_PIP" install --quiet \
    openai-whisper \
    numpy \
    soundfile \
    kokoro \
    piper-tts \
    num2words

log "Done."
echo
echo "Next steps:"
echo "  1. Open Vox.app"
echo "  2. Press Option+Space to talk to it"
echo
echo "If Ollama isn't set up yet:"
echo "  brew install ollama && ollama pull qwen2.5:3b"
