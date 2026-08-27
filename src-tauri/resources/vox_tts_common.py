#!/usr/bin/env python3
"""Shared protocol + text normalisation for every Vox TTS daemon.

Protocol (stdin/stdout, line-based):
  IN:  <output_wav_path>\t<text>\n
  OUT: ok:<output_wav_path>\n   on success
       error:<reason>\n          on failure
"ready\n" once the model is loaded. NOTHING else may ever reach stdout.

Imported as a sibling: `python /path/vox_tts_x.py` puts the script's own
directory on sys.path[0], and tauri.conf.json bundles `resources/*` by glob.
"""
import os
import re
import sys


def protocol_stdout():
    """Reserve the real stdout for the protocol and point fd 1 at stderr.

    Third-party libraries (mlx_audio, kokoro, the HF hub) print banners,
    warnings and progress bars to stdout at import and load time. Rust reads
    every stdout line that isn't "ready" as a response to a pending request, so
    ONE stray print desynchronises the protocol permanently — every later
    request gets the previous one's answer.

    Duplicating fd 1 and then `dup2(2, 1)` catches all of it, including writes
    from C extensions that never touch sys.stdout.

    Returns a `respond(line)` function: the only sanctioned way to reach stdout.
    """
    fd = os.dup(1)
    os.dup2(2, 1)
    try:
        sys.stdout.reconfigure(line_buffering=True)
    except Exception:
        pass
    out = os.fdopen(fd, "w", buffering=1, encoding="utf-8")

    def respond(line):
        out.write(line + "\n")
        out.flush()

    return respond


def log(msg):
    """Human-readable output. Always stderr — Rust tails it into the app log."""
    print(f"[vox-tts] {msg}", file=sys.stderr, flush=True)


LANG = os.environ.get("VOX_TTS_LANG", "fr").lower()[:2]

try:
    from num2words import num2words
    HAS_NUM2WORDS = True
except ImportError:
    HAS_NUM2WORDS = False


# Acronyms a phonemizer would otherwise read as words, in BOTH languages.
# The dots force letter-by-letter reading.
_ABBREVS_COMMON = [
    (r'\bPRs\b',    'pull requests'),
    (r'\bPR\b',     'pull request'),
    (r'\bMRs\b',    'merge requests'),
    (r'\bMR\b',     'merge request'),
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
    (r'\bMVPs?\b',  lambda m: 'M.V.P.s' if m.group().endswith('s') else 'M.V.P.'),
    (r'\bLLMs?\b',  lambda m: 'L.L.M.s' if m.group().endswith('s') else 'L.L.M.'),
    (r'\bAI\b',     'A.I.'),
    (r'\bSTT\b',    'S.T.T.'),
    (r'\bTTS\b',    'T.T.S.'),
    (r'\bSSH\b',    'S.S.H.'),
    (r'\bSEO\b',    'S.E.O.'),
]

# French-only phonetic hacks for espeak-fr. Applying these in English mode is a
# REGRESSION — "jason", "yamel" and "type script" are simply wrong for an
# English voice, and both daemons used to do exactly that regardless of language.
_ABBREVS_FR = [
    (r'\bJSON\b',       'jason'),
    (r'\bYAML\b',       'yamel'),
    (r'\bTypeScript\b', 'type script'),
    (r'\bJavaScript\b', 'java script'),
    (r'\bNextJS\b',     'next java script'),
    (r'\bReactJS\b',    'react java script'),
]

_ABBREVS_EN = [
    (r'\bNextJS\b',  'Next J S'),
    (r'\bReactJS\b', 'React J S'),
]

_LOC = {
    'fr': {'link': 'le lien', 'issue': r'numéro \1', 'n2w': 'fr',
           'abbrevs': _ABBREVS_COMMON + _ABBREVS_FR},
    'en': {'link': 'the link', 'issue': r'number \1', 'n2w': 'en',
           'abbrevs': _ABBREVS_COMMON + _ABBREVS_EN},
}


def clean_text(text, lang=None):
    loc = _LOC.get((lang or LANG), _LOC['fr'])
    for pattern, repl in loc['abbrevs']:
        text = re.sub(pattern, repl, text)
    text = re.sub(r'\*{1,3}([^*\n]+)\*{1,3}', r'\1', text)
    text = re.sub(r'_{1,2}([^_\n]+)_{1,2}', r'\1', text)
    text = re.sub(r'`+([^`\n]*)`+', r'\1', text)
    text = re.sub(r'^#+\s+', '', text, flags=re.MULTILINE)
    text = re.sub(r'https?://\S+', loc['link'], text)
    text = re.sub(r'(?:/[\w.\-]+){2,}/([\w.\-]+)', r'\1', text)
    text = re.sub(r'#(\d+)', loc['issue'], text)
    if HAS_NUM2WORDS:
        def _num(m):
            try:
                return num2words(int(m.group(0)), lang=loc['n2w'])
            except Exception:
                return m.group(0)
        text = re.sub(r'\b\d+\b', _num, text)
    text = re.sub(r'\n+', '. ', text)
    text = re.sub(r'([.!?])\s*([.!?])', r'\1', text)
    text = re.sub(r'\s+', ' ', text)
    return text.strip()


def serve(synth, respond, lang=None):
    """Run the request loop. `synth(out_path, cleaned_text)` raises on failure."""
    for raw_line in sys.stdin:
        line = raw_line.strip()
        if not line:
            continue
        if "\t" not in line:
            respond("error:invalid input (expected path\\ttext)")
            continue

        out_path, text = line.split("\t", 1)
        text = clean_text(text, lang)
        if not text:
            respond("error:empty text after cleaning")
            continue

        try:
            synth(out_path, text)
            respond(f"ok:{out_path}")
        except Exception as e:
            respond(f"error:{e}")
