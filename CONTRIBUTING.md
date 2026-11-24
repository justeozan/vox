# Contributing to Vox

Thanks for your interest in Vox — a local‑first voice bar for driving
[Conductor](https://conductor.build) worktrees. Contributions of all kinds are
welcome: bug reports, fixes, features, docs, and ideas.

## Ground rules

- Be kind and constructive. See the [Code of Conduct](CODE_OF_CONDUCT.md).
- Vox is **local‑first**: no cloud calls, no API keys, no telemetry. Please keep
  it that way — anything that phones home is out of scope.
- macOS‑only for now (the pill's native blur relies on `NSVisualEffectView`).

## Development setup

Prerequisites:

- macOS 12+ on Apple Silicon
- Node 20+
- A stable Rust toolchain (`rustup`)
- Xcode Command Line Tools (macOS window/blur APIs)
- [Ollama](https://ollama.com) running with a model pulled (`ollama pull qwen2.5:3b`)
- Python 3.11+ (the speech stack installs itself into `~/.vox/venv` on first run)

```bash
npm install
npm run tauri dev     # hot-reload dev build
npm run tauri build   # release .app bundle
```

### ⚠️ The microphone in dev

`tauri dev` runs a bare binary parented to your terminal. macOS refuses to vend
the WKWebView microphone sandbox extension to a terminal‑parented process, so
`getUserMedia` returns a **silent** stream and Vox can't hear you. To develop
anything mic‑related, run the app as a real signed bundle:

```bash
sh scripts/dev-bundle.sh
```

This builds a debug `.app`, re‑signs it with the microphone entitlement and a
stable identity, and launches it via LaunchServices (its own TCC "responsible
process"), where the mic works. Accept the macOS microphone prompt on first run.

## Project layout

| Path | What |
|---|---|
| `src-tauri/src/` | Rust backend: `lib.rs` (window/commands), `llm.rs` (Ollama loop + tools), `speech.rs` (STT/TTS streaming), `conductor.rs` (read‑only Conductor DB), `agents.rs` (agent subprocesses), `daemons.rs`, `setup.rs` |
| `src-tauri/resources/` | Python speech daemons (`vox_stt.py`, `vox_tts.py`, `vox_tts_piper.py`) |
| `renderer/index.html` | The pill UI, mic capture (VAD), and audio playback |
| `scripts/` | Install + dev helpers |

There's a deeper architecture write‑up in [`diagrams/`](diagrams/).

## Making a change

1. Fork and branch from `main` (e.g. `fix/mic-timeout`, `feat/thai-voice`).
2. Keep changes focused; one logical change per PR.
3. Match the surrounding code style. Rust must pass:
   ```bash
   cd src-tauri && cargo fmt && cargo clippy --all-targets -- -D warnings && cargo build
   ```
   CI runs `cargo fmt --check`, `cargo clippy`, and a build on every PR.
4. Test on a real signed bundle (`scripts/dev-bundle.sh`) — not just `tauri dev`.
5. Open a PR describing **what** changed and **why**, and how you verified it.

## Reporting bugs

Open an issue with your macOS version, whether Ollama/Conductor are running, and
the relevant lines from the app's stdout (`[vox] …`). A short repro helps a lot.

## Security

Please **do not** open public issues for security problems — see
[SECURITY.md](SECURITY.md).
