#!/usr/bin/env python3
"""Benchmark Vox's TTS engines against each other, on the sentences Vox says.

Drives the daemons exactly as Rust does — same interpreters, same env vars, same
`path\\ttext\\n` protocol — so the numbers transfer to the real app. Runs OUTSIDE
the app so a hanging model can't take Vox down with it.

    python3 scripts/vox_tts_bench.py --engines kokoro,piper --langs fr
    python3 scripts/vox_tts_bench.py --engines kokoro,piper,qwen3 --langs fr,en --repeat 3

Outputs into --out:
    report.md   the table, also printed
    bench.json  one row per synthesis + a summary per (engine, lang)
    <engine>/<lang>_<NN>.wav   kept, not deleted — this is the point
    ab.html     blind side-by-side listening test

Decide with the gates in report.md. Any single hard-gate failure means "do not
replace"; the quality call is made by ear, in ab.html, blind.
"""
import argparse
import json
import os
import shutil
import statistics
import subprocess
import sys
import threading
import time
import wave

HERE = os.path.dirname(os.path.abspath(__file__))
RESOURCES = os.path.join(HERE, "..", "src-tauri", "resources")
sys.path.insert(0, HERE)
from vox_tts_corpus import CORPUS  # noqa: E402

HOME = os.path.expanduser("~")
VENV = os.path.join(HOME, ".vox/venv/bin/python")
QWEN_VENV = os.path.join(HOME, ".vox/venv-qwen/bin/python")

ENGINES = {
    "kokoro": dict(
        py=VENV, script="vox_tts.py",
        env=lambda l: {"VOX_TTS_LANG": l,
                       "VOX_KOKORO_LANG": "a" if l == "en" else "f",
                       "VOX_KOKORO_VOICE": "af_heart" if l == "en" else "ff_siwis"},
    ),
    "piper": dict(
        py=VENV, script="vox_tts_piper.py",
        env=lambda l: {"VOX_TTS_LANG": l,
                       "VOX_PIPER_MODEL": os.path.join(HOME, ".vox/voices/fr_FR-siwis-medium.onnx")},
    ),
    "qwen3": dict(
        py=QWEN_VENV, script="vox_tts_qwen.py",
        env=lambda l: {"VOX_TTS_LANG": l, "VOX_TTS_BENCH": "1",
                       "HF_HOME": os.path.join(HOME, ".vox/hf")},
    ),
}

READY_TIMEOUT = 90.0
SYNTH_TIMEOUT = 60.0


class Daemon:
    """One TTS daemon, spoken to exactly the way daemons.rs does."""

    def __init__(self, name, lang, log_path):
        cfg = ENGINES[name]
        self.name, self.lang = name, lang
        self.log = open(log_path, "w")
        env = dict(os.environ)
        env.update(cfg["env"](lang))
        env["VIRTUAL_ENV"] = os.path.dirname(os.path.dirname(cfg["py"]))
        self.proc = subprocess.Popen(
            [cfg["py"], os.path.join(RESOURCES, cfg["script"])],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.log,
            text=True, bufsize=1, env=env,
        )
        self.rss_peak = 0
        self._stop = threading.Event()
        self._sampler = threading.Thread(target=self._sample_rss, daemon=True)
        self._sampler.start()

    def _sample_rss(self):
        while not self._stop.is_set():
            try:
                out = subprocess.run(["ps", "-o", "rss=", "-p", str(self.proc.pid)],
                                     capture_output=True, text=True, timeout=2)
                kb = int(out.stdout.strip() or 0)
                self.rss_peak = max(self.rss_peak, kb // 1024)
            except Exception:
                pass
            self._stop.wait(0.2)

    def wait_ready(self):
        t0 = time.perf_counter()
        while time.perf_counter() - t0 < READY_TIMEOUT:
            line = self.proc.stdout.readline()
            if not line:
                raise RuntimeError(f"{self.name} died before ready (see its log)")
            line = line.strip()
            if line == "ready":
                return (time.perf_counter() - t0) * 1000
            if line.startswith("error:"):
                raise RuntimeError(f"{self.name}: {line}")
        raise TimeoutError(f"{self.name} never became ready within {READY_TIMEOUT:.0f}s")

    def synth(self, out_path, text):
        t0 = time.perf_counter()
        self.proc.stdin.write(f"{out_path}\t{text}\n")
        self.proc.stdin.flush()
        line = (self.proc.stdout.readline() or "").strip()
        ms = (time.perf_counter() - t0) * 1000
        if not line.startswith("ok:"):
            return ms, line or "error:no response"
        return ms, None

    def close(self):
        self._stop.set()
        try:
            self.proc.stdin.close()
        except Exception:
            pass
        try:
            self.proc.wait(timeout=5)
        except Exception:
            self.proc.kill()
        self.log.close()


def wav_info(path):
    with wave.open(path, "rb") as w:
        return w.getnframes() / float(w.getframerate()), w.getframerate()


def pct(values, p):
    if not values:
        return 0.0
    values = sorted(values)
    k = min(len(values) - 1, int(round((p / 100.0) * (len(values) - 1))))
    return values[k]


def run(engine, lang, outdir, repeat):
    wavdir = os.path.join(outdir, engine)
    os.makedirs(wavdir, exist_ok=True)
    log_path = os.path.join(outdir, f"{engine}_{lang}.stderr.log")

    if not os.path.exists(ENGINES[engine]["py"]):
        print(f"  {engine}: interpreter not found ({ENGINES[engine]['py']}) — skipped")
        return None

    print(f"  {engine}/{lang}: starting…", flush=True)
    try:
        d = Daemon(engine, lang, log_path)
        cold_ms = d.wait_ready()
    except Exception as e:
        print(f"  {engine}/{lang}: FAILED to start — {e}")
        return {"engine": engine, "lang": lang, "cold_ms": None, "errors": 1,
                "rows": [], "note": str(e)}

    rows, errors = [], 0
    first_sentence_ms = None
    for r in range(repeat):
        for i, text in enumerate(CORPUS[lang]):
            wav = os.path.join(wavdir, f"{lang}_{i:02d}.wav")
            ms, err = d.synth(wav, text)
            if err:
                errors += 1
                print(f"    ! {engine}/{lang}[{i}] {err}")
                continue
            if first_sentence_ms is None:
                # The one the user actually waits for: nothing hides it.
                first_sentence_ms = ms
            try:
                dur, sr = wav_info(wav)
            except Exception:
                dur, sr = 0.0, 0
            rows.append({
                "engine": engine, "lang": lang, "idx": i, "repeat": r,
                "chars": len(text), "synth_ms": round(ms, 1),
                "audio_s": round(dur, 3), "sample_rate": sr,
                "rtf": round((ms / 1000.0) / dur, 4) if dur > 0 else None,
                "wav": os.path.relpath(wav, outdir), "text": text,
            })
    d.close()

    synth = [x["synth_ms"] for x in rows]
    rtf = [x["rtf"] for x in rows if x["rtf"] is not None]
    summary = {
        "engine": engine, "lang": lang,
        "cold_ms": round(cold_ms),
        "first_sentence_ms": round(first_sentence_ms) if first_sentence_ms else None,
        "synth_ms_p50": round(pct(synth, 50)), "synth_ms_p95": round(pct(synth, 95)),
        "rtf_p50": round(pct(rtf, 50), 3), "rtf_p95": round(pct(rtf, 95), 3),
        "rss_peak_mb": d.rss_peak, "n": len(rows), "errors": errors,
        "rows": rows,
    }
    print(f"  {engine}/{lang}: cold {summary['cold_ms']}ms, "
          f"1st {summary['first_sentence_ms']}ms, p95 {summary['synth_ms_p95']}ms, "
          f"RTF p95 {summary['rtf_p95']}, RSS {summary['rss_peak_mb']}MB, "
          f"{errors} errors")
    return summary


GATES = """
## Hard gates — a single failure means "do not replace"

| # | Metric | Threshold | Why exactly this number |
|---|---|---|---|
| G1 | `cold_ms` | **< 8000**, aim ≤ 5000 | `run_session` waits 80 × 100ms for the daemon (speech.rs). Past 8s the ENTIRE first utterance silently falls back to `say`. |
| G2 | `rtf_p95` | **< 1.0**, aim ≤ 0.5 | Synthesis runs 2 sentences ahead of playback. RTF ≥ 1 means the queue can never catch up and every gap grows. |
| G3 | `first_sentence_ms` | **< 1200**, aim ≤ 700 | Nothing hides the first sentence; past ~1.2s the pill looks frozen after ⌥Space. |
| G4 | `synth_ms_p95` | **< 3000** | Must stay far from the 15s request timeout and under the Qwen daemon's own 10s budget, or sentences drop to `say` mid-recap. |
| G5 | `rss_peak_mb` | **< 4000**, aim ≤ 2000 | Vox shares the machine with Ollama (2-4GB), whisper (~1GB) and the agents. |
| G6 | `errors` | **== 0** | One stray stdout line desynchronises the protocol permanently — a structural reject, not a tuning issue. |
| G7 | plays in the pill | yes | Check one WAV through the real app, not just QuickTime: output must live in /tmp (assetProtocol scope). |

## Quality gate — blind, in ab.html

- **Q1** on the 14 FR sentences, pick the better of {incumbent, qwen3}. Qwen must win **≥ 10/14** to justify replacing. 8-9 → keep both, leave the default alone. ≤ 7 → drop it.
- **Q2** on the acronym / URL / PR-number sentences (5, 7, 12, 14), Qwen must not be *worse* on any. A prettier voice that mispronounces "PR" is a regression for this product.
- **Q3** with `--repeat 3`, the three takes of a sentence must be perceptually interchangeable. A recap that sounds different every launch is worse than a flat one. Audible drift → lower `VOX_QWEN_TEMPERATURE` to 0.5 and re-run.
"""


def write_report(summaries, outdir, repeat):
    lines = ["# Vox TTS benchmark\n"]
    by_lang = {}
    for s in summaries:
        if s:
            by_lang.setdefault(s["lang"], []).append(s)
    for lang, group in by_lang.items():
        n = len(CORPUS[lang])
        lines.append(f"\n## {lang} — {n} sentences × {repeat}\n")
        lines.append("| engine | cold ms | 1st sent ms | synth p50 | synth p95 | RTF p50 | RTF p95 | RSS MB | err |")
        lines.append("|--------|--------:|------------:|----------:|----------:|--------:|--------:|-------:|----:|")
        for s in group:
            if s.get("cold_ms") is None:
                lines.append(f"| {s['engine']} | FAILED | | | | | | | {s['errors']} |")
                continue
            lines.append(
                f"| {s['engine']} | {s['cold_ms']} | {s['first_sentence_ms']} | "
                f"{s['synth_ms_p50']} | {s['synth_ms_p95']} | {s['rtf_p50']} | "
                f"{s['rtf_p95']} | {s['rss_peak_mb']} | {s['errors']} |"
            )
    lines.append(GATES)
    text = "\n".join(lines)
    with open(os.path.join(outdir, "report.md"), "w") as f:
        f.write(text)
    print("\n" + text)


def write_ab(summaries, outdir):
    """Blind by default: engine columns are shuffled and labelled A/B/C, with the
    mapping folded away at the bottom. Knowing which is which biases the ear."""
    engines = sorted({s["engine"] for s in summaries if s and s["rows"]})
    if len(engines) < 2:
        return
    # Deterministic but non-alphabetical, so column order isn't a giveaway.
    order = sorted(engines, key=lambda e: (sum(ord(c) for c in e) % 7, e))
    letters = dict(zip(order, "ABCDEFG"))

    by_lang = {}
    for s in summaries:
        if s:
            by_lang.setdefault(s["lang"], {})[s["engine"]] = s

    html = ["<!DOCTYPE html><meta charset='utf-8'><title>Vox TTS A/B</title>",
            "<style>body{font:14px -apple-system,system-ui;margin:2rem;max-width:1100px}",
            "td,th{padding:6px 10px;border-bottom:1px solid #ddd;vertical-align:top}",
            "audio{height:32px}.s{max-width:420px}</style>",
            "<h1>Vox TTS — blind A/B</h1>",
            "<p>Pick the better rendering of each sentence <em>before</em> opening the key at the bottom.</p>"]
    for lang, per_engine in by_lang.items():
        html.append(f"<h2>{lang}</h2><table><tr><th class='s'>sentence</th>"
                    + "".join(f"<th>{letters[e]}</th>" for e in order if e in per_engine)
                    + "</tr>")
        n = len(CORPUS[lang])
        for i in range(n):
            html.append(f"<tr><td class='s'>{CORPUS[lang][i]}</td>")
            for e in order:
                if e not in per_engine:
                    continue
                rel = f"{e}/{lang}_{i:02d}.wav"
                cell = "—"
                if os.path.exists(os.path.join(outdir, rel)):
                    cell = "<audio controls src='" + rel + "'></audio>"
                html.append("<td>" + cell + "</td>")
            html.append("</tr>")
        html.append("</table>")
    html.append("<details><summary>Key (open only after listening)</summary><ul>")
    for e in order:
        html.append(f"<li><b>{letters[e]}</b> = {e}</li>")
    html.append("</ul></details>")
    with open(os.path.join(outdir, "ab.html"), "w") as f:
        f.write("\n".join(html))
    print(f"\nBlind listening test: {os.path.join(outdir, 'ab.html')}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--engines", default="kokoro,piper")
    ap.add_argument("--langs", default="fr")
    ap.add_argument("--out", default="/tmp/vox_bench")
    ap.add_argument("--repeat", type=int, default=1)
    a = ap.parse_args()

    if os.path.isdir(a.out):
        shutil.rmtree(a.out)
    os.makedirs(a.out, exist_ok=True)

    summaries = []
    for lang in a.langs.split(","):
        print(f"\n{lang}:")
        for engine in a.engines.split(","):
            if engine not in ENGINES:
                print(f"  unknown engine {engine!r}")
                continue
            summaries.append(run(engine, lang, a.out, a.repeat))

    with open(os.path.join(a.out, "bench.json"), "w") as f:
        json.dump([s for s in summaries if s], f, indent=2)
    write_report(summaries, a.out, a.repeat)
    write_ab(summaries, a.out)


if __name__ == "__main__":
    main()
