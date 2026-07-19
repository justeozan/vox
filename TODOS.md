# TODOS — Vox Voice Bar

## Post-demo / v2

### Project registry for switch_project()
**What:** Configurable JSON file mapping project names to filesystem paths.
**Why:** `switch_project("vox")` currently has no way to resolve a name to a cwd. Hardcoded for demo. A `~/.vox/projects.json` registry would make this real.
**Context:** `activeProjectPath` in main.js is hardcoded for MVP. switch_project() should read from registry.
**Where to start:** `main.js` switch_project tool handler + `~/.vox/projects.json` schema.
**Depends on:** Nothing — independent of MVP scope.

### Configurable agent timeout
**What:** Make the 30s subprocess timeout configurable per-project or per-task type.
**Why:** 30s is too short for real coding work (Codex finding). Demo uses it theatrically but real use needs 3-10 min.
**Context:** `setTimeout(() => proc.kill(), 30000)` in main.js. Extract to a constant or config.
**Depends on:** Project registry (above).

### macOS packaging & entitlements
**What:** Proper Electron packaging with NSMicrophoneUsageDescription, code signing, notarization.
**Why:** Microphone access in a packaged .app requires entitlements — dev mode works without them.
**Context:** Add to electron-builder config post-demo.
**Depends on:** Working MVP.

## Post-migration Tauri (2026-07-18)

### Overlay fullscreen Spaces
**What:** La barre n'apparaît pas au-dessus des apps fullscreen d'AUTRES apps sur certains Spaces (macOS 26 / Darwin 25).
**Context:** CanJoinAllSpaces + FullScreenAuxiliary + NSStatusWindowLevel sont posés (lib.rs setup) mais WindowServer refuse la composition sur les Spaces fullscreen d'apps tierces. Les bureaux normaux fonctionnent.
**Piste:** convertir la fenêtre en NSPanel non-activant (tauri-nspanel) — approche standard des overlays type Spotlight.

### Code signing / notarization
**What:** Le bundle Vox.app est non signé (ad-hoc). Gatekeeper acceptera en local, pas en distribution.
**Where:** `tauri.conf.json > bundle > macOS` + certificat Developer ID.
