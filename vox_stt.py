#!/usr/bin/env python3
"""
Vox STT daemon — openai-whisper (local, no HuggingFace download needed).
Models cached in ~/.cache/whisper/ from the existing whisper CLI installation.
Protocol:
  IN:  <wav_path>\n
  OUT: <transcript>\n   on success
       ERROR:<reason>\n  on failure
Writes "ready\n" once model is loaded in memory.
"""
import sys
import os

try:
    import whisper
except ImportError:
    print("ERROR:openai-whisper not installed", flush=True)
    sys.exit(1)

MODEL_NAME = os.environ.get("VOX_STT_MODEL", "small")
LANG       = os.environ.get("VOX_STT_LANG",  "fr")

try:
    model = whisper.load_model(MODEL_NAME)
except Exception as e:
    print(f"ERROR:failed to load model: {e}", flush=True)
    sys.exit(1)

print("ready", flush=True)

for line in sys.stdin:
    wav_path = line.strip()
    if not wav_path:
        continue
    try:
        result = model.transcribe(wav_path, language=LANG, fp16=False)
        transcript = result.get("text", "").strip()
        print(transcript if transcript else "EMPTY", flush=True)
    except Exception as e:
        print(f"ERROR:{e}", flush=True)
