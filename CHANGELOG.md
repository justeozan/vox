# Changelog

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
