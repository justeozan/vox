# Changelog

## Unreleased

Vox now drives **Orca**, which unlike Conductor has an API: a versioned CLI with
JSON output, and a journal it pushes agent state into. Conductor still works, so
nothing is lost — it just gets the smaller half of the feature set, because
reading a private SQLite file is all it ever offered.

### Added
- **Dictation (⌥⇧Space), ported from Amical.** Hold to talk, release, and the
  cleaned-up text is pasted at the cursor of the frontmost app; a tap latches
  hands-free, Esc cancels. At key press Vox reads the frontmost app and up to
  500 characters either side of the cursor through the Accessibility API (with
  `AXManualAccessibility` so Chromium/Electron apps expose a tree). Whisper gets
  the vocabulary plus the tail of the previous dictation as its initial prompt.
  The brain formats with Amical's prompt — a formatter, never a rewriter, with
  email/chat/notes rules picked from the bundle ID or URL — plus French filler
  rules; it gives up after 12 s or on a suspiciously long answer and the raw
  transcript is used. Then replacements, Amical's boundary-spacing pass, and
  clipboard + ⌘V with every pasteboard type restored after 700 ms (unless you
  copied something since). Start/stop sound cues. `dictation_format`
  (`llm`/`raw`), `~/.vox/vocabulary.json`, `~/.vox/dictations.jsonl`.
- **Dictation v2.** Fn by default (CGEventTap — no global-shortcut API sees
  a lone modifier, let alone Fn), or right ⌥/⌘/⇧ or ⌥⇧Space. Hold = push-to-talk,
  double tap = hands-free, a single tap does nothing, Fn+another key is treated
  as a shortcut and dropped silently; Esc is swallowed only while recording.
  Native capture (cpal) replaces the webview mic: started on key-down, kept warm
  for 20 s with a 400 ms pre-roll ring. Selection context via WebKit/Chromium
  text markers, the web area and text descendants, with Amical's placeholder
  checks. ⌘V keycode resolved through UCKeyTranslate for the active layout, and
  Vox's own events are tagged so the tap ignores them.
- **Speech model picker:** `stt_model` = `whisper-small` | `whisper-large-v3-turbo`
  (mlx-whisper) | `parakeet-v3` (parakeet-mlx), installed on demand; the daemon
  loads audio with soundfile (no ffmpeg) and warms the model before `ready`.
- **Settings panel regrouped** (Dictée / Voix / Cerveau / Agents) with a reset
  per row and "reset all"; `settings_defaults` backs it.

### Fixed
- The STT daemon could start under a system Python that merely had
  openai-whisper (it was tried before `~/.vox/venv`), then crash on the
  daemon's other imports and fall back to the slow CLI. The venv now comes first.

- **Orca backend.** `ade.rs` is the seam: normalized worktree/agent types, and a
  dispatcher that tries Orca, then Conductor. Selection is by data rather than a
  liveness probe — a backend that cannot answer returns nothing and the other one
  is tried, so an installed-but-closed Orca degrades to Conductor instead of
  reporting an empty world. `VOX_ADE` pins one.
- **Native launches.** A voice-launched agent can now run *inside* Orca —
  `orca terminal create --command claude` in an existing worktree, or
  `orca worktree create --agent claude --prompt …` when you target a repo, so an
  unsupervised agent never dirties a main checkout. It appears as a normal Orca
  card annotated `Vox: <task>`, and you can scroll its terminal back and take it
  over. "Stop" interrupts rather than closing the tab. Pick the mode in settings;
  headless (`claude --print`) stays the default on a machine Orca isn't on.
- **Pushed agent status.** Orca rewrites `agent-hooks/last-status.json` on every
  agent state change, so the watcher wakes on its mtime instead of discovering
  the same thing up to 4 seconds later. Only the mtime is read.
- **Worktrees answer to both their names.** Orca's `displayName` is the branch,
  which a branch convention can turn into an unsayable
  `justeozan/devis-site-avec-fonctionnalites`; the directory basename is its
  short codename ("surabaya"). Both are targetable, so whichever you say lands.
- Carousel previews are populated by the watcher again — the snapshot it already
  has carries the agent's last message, so it no longer costs a second read.

### Changed
- `AgentState` is an enum (`working | done | blocked | errored | idle`) instead of
  a status string. Every consumer used to match on literals, and the recap's
  catch-all arm silently said *nothing* for any status nobody had thought of.
- Timestamps are epoch millis everywhere. Conductor writes `updated_at` in two
  formats — ISO from the app, space-separated from its own SQL triggers — which
  sorted against each other incorrectly, sinking every trigger-written row below
  every app-written one regardless of time. Parsing at the boundary is the actual
  fix; the SQL workaround stays only to keep `ORDER BY` honest.
- The system prompt, the recap and the spoken collision line name whichever ADE
  answered instead of hardcoding one.
- Whisper's decoding hint biases toward hearing "Orca".

### Removed
- `.mcp.json` — it only held `api.conductor.build`. Orca ships a CLI and skills,
  not an MCP server.

### Fixed
- A finished agent is announced once. Orca reports `done` as a durable state
  rather than an edge, so the watermark test is what stops a settled session from
  being re-announced on every tick.
- A native launch on a repo target is attributed correctly. The new worktree's
  path differs from the repo path the job was created with, and the watcher
  matches jobs by path — without re-pointing the job, every such launch would
  have been announced anonymously.


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
