#!/usr/bin/env python3
"""
Vox TTS daemon — Piper voice synthesis.
Protocol (stdin/stdout, line-based):
  IN:  <output_wav_path>\t<text>\n
  OUT: ok:<output_wav_path>\n   on success
       error:<reason>\n          on failure
Writes "ready\n" to stdout once model is loaded.
"""
import sys
import os
import wave

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
try:
    from vox_tts_common import protocol_stdout, log, serve
except ImportError as e:
    print(f"error:missing dependency: vox_tts_common ({e})", flush=True)
    sys.exit(1)

# Claim stdout for the protocol before importing anything that might print.
respond = protocol_stdout()

try:
    from piper import PiperVoice
    from piper.config import SynthesisConfig
except ImportError as e:
    respond(f"error:missing dependency: {e}")
    sys.exit(1)

MODEL = os.environ.get("VOX_PIPER_MODEL", os.path.expanduser("~/.vox/voices/fr_FR-siwis-medium.onnx"))
SPEED = float(os.environ.get("VOX_PIPER_SPEED", "1.0"))

try:
    voice = PiperVoice.load(MODEL)
    log(f"piper ready model={MODEL} speed={SPEED} sr={voice.config.sample_rate}")
except Exception as e:
    respond(f"error:failed to load model: {e}")
    sys.exit(1)


def synth(out_path, text):
    cfg = SynthesisConfig()
    # `length_scale` is a DURATION multiplier, so it is inverted relative to
    # Kokoro's `speed` — same env name, opposite sense. Left as-is to avoid
    # changing the voice people are used to.
    if SPEED != 1.0:
        cfg.length_scale = SPEED
    with wave.open(out_path, "wb") as wf:
        voice.synthesize_wav(text, wf, syn_config=cfg)


respond("ready")
serve(synth, respond)
