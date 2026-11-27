#!/usr/bin/env python3
"""
Vox STT daemon — local speech-to-text, three selectable engines (VOX_STT_MODEL):
  parakeet-v3             parakeet-mlx, mlx-community/parakeet-tdt-0.6b-v3 (default; multilingual)
  whisper-large-v3-turbo  mlx-whisper, mlx-community/whisper-large-v3-turbo
  whisper-small           openai-whisper "small" (fallback when the chosen engine is missing)
Protocol:
  IN:  <wav_path>[\t<initial_prompt>]\n
  OUT: <transcript>\n   on success
       EMPTY\n          nothing intelligible
       ERROR:<reason>\n on failure
Writes "ready\n" once the model is loaded AND warmed up in memory.
Audio is read with soundfile (no ffmpeg dependency) and handed to the engines
as a float32 mono 16 kHz array.
"""
import sys
import os
import re

import numpy as np
import soundfile as sf

SAMPLE_RATE = 16000
KNOWN_MODELS = ("whisper-small", "whisper-large-v3-turbo", "parakeet-v3")
MODEL_NAME = os.environ.get("VOX_STT_MODEL", "parakeet-v3").strip() or "parakeet-v3"
if MODEL_NAME not in KNOWN_MODELS:
    print(f"vox_stt: unknown model '{MODEL_NAME}', falling back to parakeet-v3", file=sys.stderr, flush=True)
    MODEL_NAME = "parakeet-v3"
LANG       = os.environ.get("VOX_STT_LANG",  "fr")

def load_audio(path: str) -> np.ndarray:
    """WAV/FLAC/OGG → float32 mono 16 kHz (linear resample if needed)."""
    data, sr = sf.read(path, dtype="float32", always_2d=True)
    audio = data.mean(axis=1) if data.shape[1] > 1 else data[:, 0]
    if sr != SAMPLE_RATE and len(audio) > 1:
        n = int(round(len(audio) * SAMPLE_RATE / sr))
        audio = np.interp(np.linspace(0, len(audio) - 1, n), np.arange(len(audio)), audio).astype(np.float32)
    return np.ascontiguousarray(audio, dtype=np.float32)

# Priming vocabulary: Whisper accepts a short "initial_prompt" that biases
# tokenization. Without it, French tech terms get mangled ("bosse" → "combo",
# "worktree" → "workshop", etc.). Keep this short — long prompts confuse it.
INITIAL_PROMPT_FR = (
    "Conversation avec Vox, mon assistant vocal. Tu peux me dire, tu vois ce que "
    "je veux dire, tu penses quoi. Termes techniques : agent, worktree, Orca, "
    "Vox, Claude, Codex, opencode, Ollama, refactor, deploy, bug, commit, pull "
    "request, terminal, dashboard, backend, frontend. Tu peux m'expliquer ? "
    "Il faut qu'on bosse sur quoi ?"
)
INITIAL_PROMPT_EN = (
    "Technical discussion: agent, worktree, Orca, Vox, Claude, Codex, "
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

WHISPER_KW = dict(
    language=LANG,
    temperature=0.0,
    condition_on_previous_text=False,
    no_speech_threshold=0.6,
    logprob_threshold=-1.0,
    compression_ratio_threshold=2.4,
)

def load_engine(name):
    """Load + warm one engine; returns transcribe(audio, prompt). ImportError if not installed."""
    if name == "whisper-small":
        import whisper
        model = whisper.load_model("small")

        def transcribe(audio, prompt):
            # condition_on_previous_text=False → each utterance decoded independently,
            # prevents earlier hallucinations from poisoning later ones.
            # temperature=0 → deterministic greedy decoding, no random weird outputs.
            # no_speech_threshold + logprob_threshold → aggressive silence rejection.
            r = model.transcribe(audio, fp16=False, initial_prompt=prompt, **WHISPER_KW)
            return r.get("text", "")

    elif name == "whisper-large-v3-turbo":
        import mlx_whisper
        from mlx_whisper.transcribe import ModelHolder
        import mlx.core as mx
        MLX_REPO = "mlx-community/whisper-large-v3-turbo"
        ModelHolder.get_model(MLX_REPO, mx.float16)  # load (and download on first use)

        def transcribe(audio, prompt):
            r = mlx_whisper.transcribe(audio, path_or_hf_repo=MLX_REPO,
                                       initial_prompt=prompt, **WHISPER_KW)
            return r.get("text", "")

    else:  # parakeet-v3
        import mlx.core as mx
        from parakeet_mlx import from_pretrained
        from parakeet_mlx.audio import get_logmel
        model = from_pretrained("mlx-community/parakeet-tdt-0.6b-v3")

        def transcribe(audio, prompt):
            # Language is auto-detected and there is no prompt support: ignored
            # (dictation relies on the vocabulary replacements / LLM cleanup).
            # Feed the array directly (parakeet's own loader shells out to ffmpeg).
            # The log-mel must be computed in float32 (its bf16 path breaks with
            # recent mlx); only the resulting features are cast to bf16.
            x = mx.array(audio)
            if x.shape[-1] < model.preprocessor_config.hop_length:
                return ""
            mel = get_logmel(x, model.preprocessor_config).astype(mx.bfloat16)
            return model.generate(mel)[0].text

    # Warm-up: compile kernels / allocate buffers so the first real request is fast.
    transcribe(np.zeros(SAMPLE_RATE // 2, dtype=np.float32), INITIAL_PROMPT)
    return transcribe

def engine_order(name):
    """The wanted engine first, then whisper-small as the safe fallback."""
    return [name] if name == "whisper-small" else [name, "whisper-small"]

def resolve_engine(name, loader=load_engine):
    """First engine of engine_order(name) that is installed. A missing package
    (ImportError) falls back instead of crashing, e.g. an existing user whose
    venv only has openai-whisper. Returns (engine_name, transcribe).
    Raises ImportError if none of them can be imported."""
    last = None
    for candidate in engine_order(name):
        try:
            return candidate, loader(candidate)
        except ImportError as e:
            last = e
            print(f"vox_stt: {candidate} not installed ({e})", file=sys.stderr, flush=True)
    raise last

def main():
    try:
        engine, transcribe = resolve_engine(MODEL_NAME)
        if engine != MODEL_NAME:
            print(f"vox_stt: using {engine} instead of {MODEL_NAME}", file=sys.stderr, flush=True)
    except ImportError as e:
        print(f"ERROR:{MODEL_NAME} not installed ({e})", flush=True)
        sys.exit(1)
    except Exception as e:
        print(f"ERROR:failed to load model: {e}", flush=True)
        sys.exit(1)

    serve(transcribe)

def serve(transcribe):
    print("ready", flush=True)

    for line in sys.stdin:
        # "path" or "path<TAB>initial prompt" (dictation primes Whisper with the
        # user's vocabulary instead of the assistant's default prompt).
        wav_path, _, custom_prompt = line.rstrip("\r\n").partition("\t")
        wav_path = wav_path.strip()
        if not wav_path:
            continue
        try:
            transcript = transcribe(load_audio(wav_path), custom_prompt.strip() or INITIAL_PROMPT).strip()
            if is_hallucination(transcript):
                print("EMPTY", flush=True)
            else:
                print(transcript if transcript else "EMPTY", flush=True)
        except Exception as e:
            print(f"ERROR:{e}", flush=True)

if __name__ == "__main__":
    main()
