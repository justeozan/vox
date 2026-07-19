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
import re

try:
    import numpy as np
    import soundfile as sf
    from kokoro import KPipeline
except ImportError as e:
    print(f"error:missing dependency: {e}", flush=True)
    sys.exit(1)

try:
    from num2words import num2words
    HAS_NUM2WORDS = True
except ImportError:
    HAS_NUM2WORDS = False

VOICE = os.environ.get("VOX_KOKORO_VOICE", "ff_siwis")
LANG  = os.environ.get("VOX_KOKORO_LANG",  "f")
SPEED = float(os.environ.get("VOX_KOKORO_SPEED", "0.93"))


# Tech abbreviations that espeak-fr mispronounces (e.g. PR → "Professeur").
# Applied before any other transformation so espeak never sees the raw acronym.
_ABBREVS = [
    # Git/dev workflow — most critical
    (r'\bPRs\b',    'pull requests'),
    (r'\bPR\b',     'pull request'),
    (r'\bMRs\b',    'merge requests'),
    (r'\bMR\b',     'merge request'),
    # Pure acronyms — dots force espeak to read letter-by-letter
    (r'\bAPIs?\b',  lambda m: 'A.P.I.s' if m.group().endswith('s') else 'A.P.I.'),
    (r'\bURLs?\b',  lambda m: 'U.R.L.s' if m.group().endswith('s') else 'U.R.L.'),
    (r'\bUIs?\b',   lambda m: 'U.I.s'   if m.group().endswith('s') else 'U.I.'),
    (r'\bUX\b',     'U.X.'),
    (r'\bCLI\b',    'C.L.I.'),
    (r'\bCI/CD\b',  'C.I. C.D.'),
    (r'\bCI\b',     'C.I.'),
    (r'\bCD\b',     'C.D.'),
    (r'\bSQL\b',    'S.Q.L.'),
    (r'\bCSS\b',    'C.S.S.'),
    (r'\bHTML\b',   'H.T.M.L.'),
    (r'\bJSON\b',   'jason'),
    (r'\bYAML\b',   'yamel'),
    (r'\bMVPs?\b',  lambda m: 'M.V.P.s' if m.group().endswith('s') else 'M.V.P.'),
    (r'\bLLMs?\b',  lambda m: 'L.L.M.s' if m.group().endswith('s') else 'L.L.M.'),
    (r'\bAI\b',     'A.I.'),
    (r'\bSTT\b',    'S.T.T.'),
    (r'\bTTS\b',    'T.T.S.'),
    (r'\bSSH\b',    'S.S.H.'),
    (r'\bSEO\b',    'S.E.O.'),
    # JS/TS kept as full names — espeak français prononce mal
    (r'\bTypeScript\b', 'type script'),
    (r'\bJavaScript\b', 'java script'),
    (r'\bNextJS\b',  'next java script'),
    (r'\bReactJS\b', 'react java script'),
]

def _expand_abbrevs(text):
    for pattern, repl in _ABBREVS:
        text = re.sub(pattern, repl if isinstance(repl, str) else repl, text)
    return text


def clean_text(text):
    # Expand tech abbreviations before espeak sees the text
    text = _expand_abbrevs(text)
    # Strip markdown bold / italic / code
    text = re.sub(r'\*{1,3}([^*\n]+)\*{1,3}', r'\1', text)
    text = re.sub(r'_{1,2}([^_\n]+)_{1,2}', r'\1', text)
    text = re.sub(r'`+([^`\n]*)`+', r'\1', text)
    # Strip markdown headers
    text = re.sub(r'^#+\s+', '', text, flags=re.MULTILINE)
    # Replace URLs
    text = re.sub(r'https?://\S+', 'le lien', text)
    # Replace file paths — keep only the last component
    text = re.sub(r'(?:/[\w.\-]+){2,}/([\w.\-]+)', r'\1', text)
    # Issue/PR numbers → "numéro X"
    text = re.sub(r'#(\d+)', r'numéro \1', text)
    # Expand standalone numbers to French words
    if HAS_NUM2WORDS:
        def _num(m):
            try: return num2words(int(m.group(0)), lang='fr')
            except: return m.group(0)
        text = re.sub(r'\b\d+\b', _num, text)
    # Collapse newlines into sentence breaks
    text = re.sub(r'\n+', '. ', text)
    # Remove double punctuation artifacts (but preserve "A.P.I." style dots)
    text = re.sub(r'([.!?])\s*([.!?])', r'\1', text)
    # Collapse whitespace
    text = re.sub(r'\s+', ' ', text)
    return text.strip()


try:
    pipe = KPipeline(lang_code=LANG)
    print("ready", flush=True)
except Exception as e:
    print(f"error:failed to load model: {e}", flush=True)
    sys.exit(1)

for raw_line in sys.stdin:
    line = raw_line.strip()
    if not line:
        continue
    if "\t" not in line:
        print("error:invalid input (expected path\\ttext)", flush=True)
        continue

    out_path, text = line.split("\t", 1)
    text = clean_text(text)
    if not text:
        print("error:empty text after cleaning", flush=True)
        continue

    try:
        chunks = []
        # For short texts: synthesize as one unit (better global prosody).
        # For longer texts: split on sentence boundaries so each sentence
        # gets independent intonation.
        split_pat = None if len(text) < 120 else r'(?<=[.!?])\s+'
        for _, _, audio in pipe(text, voice=VOICE, speed=SPEED, split_pattern=split_pat):
            chunks.append(audio)

        if not chunks:
            print("error:empty audio", flush=True)
            continue

        audio_data = np.concatenate(chunks)
        sf.write(out_path, audio_data, 24000)
        print(f"ok:{out_path}", flush=True)
    except Exception as e:
        print(f"error:{e}", flush=True)
