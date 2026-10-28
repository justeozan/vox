# Changelog

## 0.5.0

Orca becomes the workspace backend, and Vox behaves like a real Mac app.

### Added
- **Orca backend (default).** The recap, agent counts and "agent finished"
  announcements read live state from the `orca` CLI (`worktree ps`,
  `repo list`). Voice-launched agents open as a Claude terminal tab in Orca, so
  you can watch, answer and approve them there; "stop" closes the tab.
  Conductor stays selectable under **Settings → Backend** and switches without
  a restart.
- **Persistent logs** in `~/Library/Logs/Vox/vox.log` (rotated at 5 MB) when
  launched from Finder or the Dock.
- **Agent guard rails** on the headless path: `git push`, `git reset --hard`,
  `rm -rf`, `sudo`, `gh pr merge`, publishing commands and friends are passed as
  `--disallowedTools`.
- CI: `cargo clippy -D warnings` and `cargo test` on every PR.

### Fixed
- **Vox didn't act like an app.** It now has a Dock icon, a menu bar
  (⌘Q quits, ⌘W hides the pill, ⌘, opens settings), a tray menu, and clicking
  the Dock icon brings the pill back. Quitting stops the speech daemons.
- **The task prompt could drift from what you said.** The small model's
  rewrite came first and could invent work (a "deployment checklist" for a
  one-line question). Your transcribed words now lead as the reference; the
  rewrite is marked as possibly wrong, and questions don't modify files.
- Speech-to-text could start on the system Python instead of `~/.vox/venv`.

## 0.4.0

The release that closes the loop: launch an agent on any repo by voice, follow it
while it runs, and be told out loud when it answers.

### Added
- **Launch on any repo.** One `launch_agent` command targets a repo name, a
  worktree codename, or `"here"`. Exact match first, then fuzzy — and a name
  that fits two repos equally becomes a spoken question rather than a coin flip.
  Previously the model could only see worktrees Conductor marked `in-progress`:
  5 of 15 real targets.
- **Spoken agent results.** When an agent finishes, Vox says so — a one-sentence
  summary or the answer read verbatim (`agent_reply`), with an automatic
  fallback to summary past `agent_reply_max_chars`. Announcements wait for
  silence, never interrupt a turn, and survive an ⌥Space: the queue is durable,
  so nothing is lost if you cut Vox off.
- **Follow-up questions.** Finished answers stay answerable for 30 minutes
  ("what did it say?"), and running agents are in the prompt too, so "where are
  my agents at?" gets a grounded answer.
- **Conductor watcher.** Agents you start inside Conductor are announced as well,
  detected on the message watermark rather than a status edge.
- **Choose the brain.** Local Ollama, your Claude or Codex *subscription* via
  their CLI (no API key), or the Anthropic / OpenAI APIs. Unavailable providers
  stay visible with the reason.
- **Qwen3-TTS as an opt-in third voice**, in its own virtualenv, with a benchmark
  (`scripts/vox_tts_bench.py`) that produces numbers and a blind A/B page.
  Experimental — not yet measured on real hardware.
- Karaoke transcript: the spoken word is highlighted and the line scrolls to
  follow the voice.

### Fixed
- **The shipped .app had a dead microphone.** `signingIdentity` was unset, so
  Tauri ad-hoc-signed without applying `Entitlements.plist` and derived a new
  code identity on every build — a granted mic permission could never persist.
- **Conductor timestamps sorted wrong.** `updated_at` mixes two formats, and
  `'T' > ' '`, so trigger-written rows sank below every app-written one. This
  corrupted which worktree "most recent" picked.
- **The recap's "agent is done" line never fired.** It required
  `unread_count > 0`, which is 0 on every session row. Rebased on message
  recency.
- `switch_project` accepted paths that no longer exist, so Vox confirmed a
  launch that never happened.
- Speech sessions could settle the pill out from under another session that was
  still speaking.
- A dead speech daemon stayed "ready" forever, making every sentence wait the
  full 15s timeout before falling back to `say`.
- The pill now follows you onto every desktop, including another app's
  fullscreen Space (non-activating `NSPanel`), and can still be dragged.

### Changed
- Pill narrowed to 408px; the hover stretch is gone.
- Darker, more transparent shell (the vibrancy material is pinned to dark).
- `npm run dev` runs without the file watcher — it fired spurious rebuild events
  and restarted the app ~10×/minute. `npm run dev:watch` keeps it.
