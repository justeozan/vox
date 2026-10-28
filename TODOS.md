# TODOS — Vox Voice Bar

## Post-migration Tauri (2026-07-18)

### Overlay fullscreen Spaces
**What:** La barre n'apparaît pas au-dessus des apps fullscreen d'AUTRES apps sur certains Spaces (macOS 26 / Darwin 25).
**Context:** CanJoinAllSpaces + FullScreenAuxiliary + NSStatusWindowLevel sont posés (lib.rs setup) mais WindowServer refuse la composition sur les Spaces fullscreen d'apps tierces. Les bureaux normaux fonctionnent.
**Piste:** convertir la fenêtre en NSPanel non-activant (tauri-nspanel) — approche standard des overlays type Spotlight.

### Code signing / notarization
**What:** Le bundle Vox.app est non signé (ad-hoc). Gatekeeper acceptera en local, pas en distribution.
**Where:** `tauri.conf.json > bundle > macOS` + certificat Developer ID.

## Later (2026-10-29)
- **Stable code signature for local builds.** Ad-hoc signing changes the code identity on every build, so macOS re-asks for the mic. A self-signed "Vox Dev" certificate in the keychain + `signingIdentity` would keep the permission.
- **Orca agents have no deny list.** Voice-launched agents run as normal interactive Claude sessions in Orca (they ask permission), unlike the headless path's `--disallowedTools`.
- **Agent count includes manually started Orca agents** (including the session you're coding in).
- **Spoken "stop".** Only the `kill_agents` tool exists; no dedicated voice phrase.
- **Tray icon as template image** so it adapts to light/dark menu bars.
