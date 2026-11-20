#!/usr/bin/env python3
"""
Vox TTS daemon — keeps Kokoro model in memory for low-latency synthesis.
Protocol (stdin/stdout, line-based):
  IN:  <output_wav_path>\t<text>\n
  OUT: ok:<output_wav_path>\n   on success
       error:<reason>\n          on failure
Writes "ready\n" to stdout once model is loaded.
"""
import sys
import os

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
try:
    from vox_tts_common import protocol_stdout, log, serve
except ImportError as e:
    print(f"error:missing dependency: vox_tts_common ({e})", flush=True)
    sys.exit(1)

# Claim stdout for the protocol BEFORE importing anything heavy: torch, kokoro
# and the HF hub all print warnings there, and Rust reads every non-"ready"
# stdout line as a response to a pending request.
respond = protocol_stdout()

try:
    import numpy as np
    import soundfile as sf
    from kokoro import KPipeline
except ImportError as e:
    respond(f"error:missing dependency: {e}")
    sys.exit(1)


VOICE = os.environ.get("VOX_KOKORO_VOICE", "ff_siwis")
LANG  = os.environ.get("VOX_KOKORO_LANG",  "f")
SPEED = float(os.environ.get("VOX_KOKORO_SPEED", "0.93"))


try:
    pipe = KPipeline(lang_code=LANG)
    log(f"kokoro ready voice={VOICE} lang={LANG} speed={SPEED}")
except Exception as e:
    respond(f"error:failed to load model: {e}")
    sys.exit(1)


def synth(out_path, text):
    # Short texts synthesize as one unit (better global prosody); longer ones
    # split on sentence boundaries so each sentence gets its own intonation.
    split_pat = None if len(text) < 120 else r'(?<=[.!?])\s+'
    chunks = [a for _, _, a in pipe(text, voice=VOICE, speed=SPEED, split_pattern=split_pat)]
    if not chunks:
        raise RuntimeError("empty audio")
    sf.write(out_path, np.concatenate(chunks), 24000)


respond("ready")
serve(synth, respond)
