#!/usr/bin/env python3
"""
Vox TTS daemon — Qwen3-TTS via MLX (Apple Silicon). THIRD engine, opt-in.

Protocol (stdin/stdout, line-based):
  IN:  <output_wav_path>\t<text>\n
  OUT: ok:<output_wav_path>\n   on success
       error:<reason>\n          on failure
Writes "ready\n" to stdout once the model is loaded AND warmed.

Lives in its own virtualenv (~/.vox/venv-qwen) so a failed experiment cannot
touch the Kokoro/Piper install it is being compared against.
"""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
try:
    from vox_tts_common import protocol_stdout, log, serve, LANG
except ImportError as e:
    print(f"error:missing dependency: vox_tts_common ({e})", flush=True)
    sys.exit(1)

# Claim stdout BEFORE importing mlx_audio: it prints loader banners and the HF
# hub prints download progress, and any of it on stdout desynchronises the
# protocol for the rest of the session.
respond = protocol_stdout()

os.environ.setdefault("HF_HUB_DISABLE_PROGRESS_BARS", "1")
os.environ.setdefault("HF_HUB_DISABLE_TELEMETRY", "1")
os.environ.setdefault("TOKENIZERS_PARALLELISM", "false")

try:
    import numpy as np
    import soundfile as sf
    from mlx_audio.tts.utils import load_model
except ImportError as e:
    respond(f"error:missing dependency: {e}")
    sys.exit(1)

MODEL = os.environ.get("VOX_QWEN_MODEL", "mlx-community/Qwen3-TTS-12Hz-0.6B-CustomVoice-4bit")
VOICE = os.environ.get("VOX_QWEN_VOICE", "Serena")
QLANG = os.environ.get("VOX_QWEN_LANG", {"fr": "french", "en": "english"}.get(LANG, "auto"))
INSTRUCT = os.environ.get("VOX_QWEN_INSTRUCT", "").strip() or None
SPEED = float(os.environ.get("VOX_QWEN_SPEED", "1.0"))
# Below the usual 0.9: short technical lines want stability, not variety. A
# recap that sounds different every launch is worse than a flat one.
TEMP = float(os.environ.get("VOX_QWEN_TEMPERATURE", "0.7"))
MAX_TOK = int(os.environ.get("VOX_QWEN_MAX_TOKENS", "2048"))
STREAM = os.environ.get("VOX_QWEN_STREAM", "1") != "0"
INTERVAL = float(os.environ.get("VOX_QWEN_STREAM_INTERVAL", "0.64"))
# Deliberately under the Rust side's 15s request timeout: a runaway generation
# returns a clean error and falls back to `say` for that one sentence, instead
# of 15 seconds of silence.
BUDGET_S = float(os.environ.get("VOX_QWEN_BUDGET_S", "10"))
WARMUP = os.environ.get("VOX_QWEN_WARMUP", "1") != "0"
BENCH = os.environ.get("VOX_TTS_BENCH", "") == "1"

try:
    t0 = time.perf_counter()
    model = load_model(MODEL)
    load_ms = (time.perf_counter() - t0) * 1000

    # Validate against what THIS checkpoint actually offers, so a typo degrades
    # to a working default instead of failing every request forever.
    try:
        speakers = list(model.get_supported_speakers() or [])
        if speakers and VOICE.lower() not in [s.lower() for s in speakers]:
            log(f"voice {VOICE!r} unsupported; speakers={speakers} -> using {speakers[0]!r}")
            VOICE = speakers[0]
        langs = list(model.get_supported_languages() or [])
        if langs and QLANG.lower() not in [l.lower() for l in langs]:
            log(f"language {QLANG!r} unsupported; langs={langs} -> using 'auto'")
            QLANG = "auto"
    except Exception as e:
        log(f"capability probe failed ({e}) — proceeding with {VOICE}/{QLANG}")

    SR = int(getattr(model, "sample_rate", 24000) or 24000)
    log(f"loaded {MODEL} in {load_ms:.0f}ms voice={VOICE} lang={QLANG} sr={SR}")
except Exception as e:
    respond(f"error:failed to load model: {e}")
    sys.exit(1)


def _render(text, budget_s):
    """Return (float32 mono array, sample_rate, time-to-first-audio ms, chunks)."""
    t0 = time.perf_counter()
    kw = dict(text=text, voice=VOICE, lang_code=QLANG, speed=SPEED,
              temperature=TEMP, max_tokens=MAX_TOK, verbose=False)
    if INSTRUCT:
        kw["instruct"] = INSTRUCT
    if STREAM:
        kw.update(stream=True, streaming_interval=INTERVAL)

    chunks, ttfa, sr = [], None, SR
    for r in model.generate(**kw):
        if ttfa is None:
            ttfa = (time.perf_counter() - t0) * 1000
        sr = int(getattr(r, "sample_rate", sr) or sr)
        chunks.append(np.asarray(r.audio, dtype=np.float32).reshape(-1))
        if time.perf_counter() - t0 > budget_s:
            raise TimeoutError(f"synthesis budget {budget_s:.0f}s exceeded")
    if not chunks:
        raise RuntimeError("empty audio")
    return np.concatenate(chunks), sr, (ttfa or 0.0), len(chunks)


def synth(out_path, text):
    global STREAM
    t0 = time.perf_counter()
    try:
        audio, sr, ttfa, n = _render(text, BUDGET_S)
    except TypeError as e:
        # Some checkpoints reject stream=True — degrade once, loudly.
        if not STREAM:
            raise
        log(f"streaming rejected ({e}) — falling back to non-streaming")
        STREAM = False
        audio, sr, ttfa, n = _render(text, BUDGET_S)
    sf.write(out_path, audio, sr)
    if BENCH:
        total = (time.perf_counter() - t0) * 1000
        dur = len(audio) / float(sr)
        log(f"bench engine=qwen3 chars={len(text)} synth_ms={total:.0f} "
            f"ttfa_ms={ttfa:.0f} audio_s={dur:.2f} "
            f"rtf={total / 1000.0 / max(dur, 1e-6):.3f} chunks={n}")


# Warm up BEFORE announcing ready: the first generate() pays MLX graph
# compilation, and paying it here means the recap's first sentence isn't the one
# that stutters. It does delay "ready", and run_session only waits 8s — hence
# the log line, which is exactly what the benchmark's cold-start gate measures.
if WARMUP:
    try:
        t0 = time.perf_counter()
        _render("Bonjour." if LANG == "fr" else "Hello.", max(BUDGET_S, 30))
        log(f"warmup {(time.perf_counter() - t0) * 1000:.0f}ms")
    except Exception as e:
        log(f"warmup failed (non-fatal): {e}")

respond("ready")
serve(synth, respond)
