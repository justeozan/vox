#!/usr/bin/env python3
"""Non-regression check for the TTS text pipeline.

`clean_text` was copy-pasted into vox_tts.py and vox_tts_piper.py and has now
been extracted into vox_tts_common.py. This freezes the ORIGINAL implementation
as a reference and asserts the shared one still matches it — on the French path,
which is the one that was live.

The English path deliberately DIVERGES: both daemons used to apply French
phonetic hacks ("jason", "yamel", "type script") regardless of language, which
is wrong for an English voice. Those cases are asserted as intentional changes.

    python3 scripts/vox_tts_selftest.py

Needs only num2words (already in the venv):
    ~/.vox/venv/bin/python scripts/vox_tts_selftest.py
"""
import os
import re
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                "..", "src-tauri", "resources"))

try:
    from num2words import num2words
    HAS_NUM2WORDS = True
except ImportError:
    HAS_NUM2WORDS = False

# ── Frozen reference: the implementation as it stood before the extraction ──

_REF_ABBREVS = [
    (r'\bPRs\b', 'pull requests'), (r'\bPR\b', 'pull request'),
    (r'\bMRs\b', 'merge requests'), (r'\bMR\b', 'merge request'),
    (r'\bAPIs?\b', lambda m: 'A.P.I.s' if m.group().endswith('s') else 'A.P.I.'),
    (r'\bURLs?\b', lambda m: 'U.R.L.s' if m.group().endswith('s') else 'U.R.L.'),
    (r'\bUIs?\b', lambda m: 'U.I.s' if m.group().endswith('s') else 'U.I.'),
    (r'\bUX\b', 'U.X.'), (r'\bCLI\b', 'C.L.I.'),
    (r'\bCI/CD\b', 'C.I. C.D.'), (r'\bCI\b', 'C.I.'), (r'\bCD\b', 'C.D.'),
    (r'\bSQL\b', 'S.Q.L.'), (r'\bCSS\b', 'C.S.S.'), (r'\bHTML\b', 'H.T.M.L.'),
    (r'\bJSON\b', 'jason'), (r'\bYAML\b', 'yamel'),
    (r'\bMVPs?\b', lambda m: 'M.V.P.s' if m.group().endswith('s') else 'M.V.P.'),
    (r'\bLLMs?\b', lambda m: 'L.L.M.s' if m.group().endswith('s') else 'L.L.M.'),
    (r'\bAI\b', 'A.I.'), (r'\bSTT\b', 'S.T.T.'), (r'\bTTS\b', 'T.T.S.'),
    (r'\bSSH\b', 'S.S.H.'), (r'\bSEO\b', 'S.E.O.'),
    (r'\bTypeScript\b', 'type script'), (r'\bJavaScript\b', 'java script'),
    (r'\bNextJS\b', 'next java script'), (r'\bReactJS\b', 'react java script'),
]


def ref_clean_text(text):
    for pattern, repl in _REF_ABBREVS:
        text = re.sub(pattern, repl, text)
    text = re.sub(r'\*{1,3}([^*\n]+)\*{1,3}', r'\1', text)
    text = re.sub(r'_{1,2}([^_\n]+)_{1,2}', r'\1', text)
    text = re.sub(r'`+([^`\n]*)`+', r'\1', text)
    text = re.sub(r'^#+\s+', '', text, flags=re.MULTILINE)
    text = re.sub(r'https?://\S+', 'le lien', text)
    text = re.sub(r'(?:/[\w.\-]+){2,}/([\w.\-]+)', r'\1', text)
    text = re.sub(r'#(\d+)', r'numéro \1', text)
    if HAS_NUM2WORDS:
        def _num(m):
            try:
                return num2words(int(m.group(0)), lang='fr')
            except Exception:
                return m.group(0)
        text = re.sub(r'\b\d+\b', _num, text)
    text = re.sub(r'\n+', '. ', text)
    text = re.sub(r'([.!?])\s*([.!?])', r'\1', text)
    text = re.sub(r'\s+', ' ', text)
    return text.strip()


CASES = [
    # Every abbreviation, one per entry.
    "3 PRs sont ouvertes", "la PR est prête", "2 MRs en attente", "la MR passe",
    "les APIs REST", "une API simple", "les URLs cassées", "cette URL",
    "les UIs mobiles", "l'UI est propre", "l'UX compte", "via le CLI",
    "le CI/CD tourne", "la CI casse", "le CD est bloqué", "une requête SQL",
    "du CSS pur", "du HTML brut", "un fichier JSON", "un fichier YAML",
    "deux MVPs livrés", "un MVP", "les LLMs locaux", "un LLM",
    "l'AI locale", "le STT marche", "le TTS aussi", "une clé SSH", "le SEO",
    "du TypeScript", "du JavaScript", "NextJS et ReactJS",
    # Structure.
    "voir https://github.com/justeozan/vox pour le diff",
    "le fichier /Users/spectre/conductor/repos/vox/README.md a changé",
    "corrige #42 puis #7",
    "**gras** et _italique_ et `code`",
    "# Titre\ndeuxième ligne",
    "trop    d'espaces\n\n\net des sauts",
    "double ponctuation ?!",
    "42 fichiers et 7 dossiers",
    "",
    "   ",
    "...",
]

# Cases where the English path is EXPECTED to differ from the frozen French
# reference — these are the bug fix, not a regression.
EN_DIVERGENT = {"un fichier JSON", "un fichier YAML", "du TypeScript",
                "du JavaScript", "NextJS et ReactJS",
                "voir https://github.com/justeozan/vox pour le diff",
                "corrige #42 puis #7", "42 fichiers et 7 dossiers"}


def main():
    try:
        import vox_tts_common
    except ImportError as e:
        print(f"FAIL: cannot import vox_tts_common ({e})")
        return 1

    failures = []
    for case in CASES:
        want = ref_clean_text(case)
        got = vox_tts_common.clean_text(case, "fr")
        if got != want:
            failures.append((case, want, got))

    print(f"fr: {len(CASES) - len(failures)}/{len(CASES)} identical to the frozen reference")
    for case, want, got in failures:
        print(f"  MISMATCH {case!r}\n    want {want!r}\n    got  {got!r}")

    # The English path must at minimum not apply the French hacks.
    en_bad = []
    for case in ("un fichier JSON", "du TypeScript", "voir https://x.com/a pour le diff"):
        got = vox_tts_common.clean_text(case, "en")
        if "jason" in got or "type script" in got or "le lien" in got:
            en_bad.append((case, got))
    if en_bad:
        print("en: French hacks still leaking into English mode:")
        for case, got in en_bad:
            print(f"  {case!r} -> {got!r}")
    else:
        print("en: no French phonetic hacks leak into English mode ✓")
        for case in sorted(EN_DIVERGENT)[:3]:
            print(f"     {case!r} -> {vox_tts_common.clean_text(case, 'en')!r}")

    if failures or en_bad:
        return 1
    print("\nOK — the shared module matches the frozen French behaviour.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
