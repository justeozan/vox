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
import re

try:
    import whisper
except ImportError:
    print("ERROR:openai-whisper not installed", flush=True)
    sys.exit(1)

MODEL_NAME = os.environ.get("VOX_STT_MODEL", "small")
LANG       = os.environ.get("VOX_STT_LANG",  "fr")

# Priming vocabulary: Whisper accepts a short "initial_prompt" that biases
# tokenization. Without it, French tech terms get mangled ("bosse" → "combo",
# "worktree" → "workshop", etc.). Keep this short — long prompts confuse it.
INITIAL_PROMPT_FR = (
    "Conversation avec Vox, mon assistant vocal. Tu peux me dire, tu vois ce que "
    "je veux dire, tu penses quoi. Termes techniques : agent, worktree, Conductor, "
    "Vox, Claude, Codex, opencode, Ollama, refactor, deploy, bug, commit, pull "
    "request, terminal, dashboard, backend, frontend. Tu peux m'expliquer ? "
    "Il faut qu'on bosse sur quoi ?"
)
INITIAL_PROMPT_EN = (
    "Technical discussion: agent, worktree, Conductor, Vox, Claude, Codex, "
    "opencode, Ollama, refactor, deploy, bug, commit, pull request."
)
INITIAL_PROMPT = INITIAL_PROMPT_FR if LANG.startswith("fr") else INITIAL_PROMPT_EN

# Known Whisper hallucination phrases on silence/noise — leaked from its
# YouTube training set. Filter them out post-transcription.
HALLUCINATION_PATTERNS = [
    r"^merci d'avoir regardé.*$",
    r"^merci de.*regard.*$",
    r"^abonnez-vous.*$",
    r"^sous-titr.*(amara|société|française|par).*$",
    r"^à bientôt.*!?$",
    r"^n'oubliez pas de.*$",
    r"^thanks? for watching.*$",
    r"^please subscribe.*$",
]
HALLUCINATION_RE = re.compile("|".join(HALLUCINATION_PATTERNS), re.IGNORECASE)

def is_hallucination(text: str) -> bool:
    stripped = text.strip().rstrip(".!?").lower()
    if not stripped:
        return True
    return bool(HALLUCINATION_RE.match(stripped))

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
        # condition_on_previous_text=False → each utterance decoded independently,
        # prevents earlier hallucinations from poisoning later ones.
        # temperature=0 → deterministic greedy decoding, no random weird outputs.
        # no_speech_threshold + logprob_threshold → aggressive silence rejection.
        result = model.transcribe(
            wav_path,
            language=LANG,
            fp16=False,
            temperature=0.0,
            initial_prompt=INITIAL_PROMPT,
            condition_on_previous_text=False,
            no_speech_threshold=0.6,
            logprob_threshold=-1.0,
            compression_ratio_threshold=2.4,
        )
        transcript = result.get("text", "").strip()
        if is_hallucination(transcript):
            print("EMPTY", flush=True)
        else:
            print(transcript if transcript else "EMPTY", flush=True)
    except Exception as e:
        print(f"ERROR:{e}", flush=True)
