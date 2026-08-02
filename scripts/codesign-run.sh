#!/bin/sh
# Cargo `runner` for `tauri dev` on macOS.
#
# `cargo run` ad-hoc "linker-signs" the dev binary with NO entitlements, so
# WKWebView can't open the mic ("Could not create a
# 'com.apple.webkit.microphone' sandbox extension") and getUserMedia returns a
# silent stream — Vox's VAD then never hears you. This runner re-signs the exact
# binary cargo is about to launch, adding the microphone entitlement, then execs
# it. (`tauri build` signs the shipped .app separately via tauri.conf.json.)
set -e
bin="$1"
shift
script_dir=$(cd "$(dirname "$0")" && pwd)
ent="$script_dir/../src-tauri/Entitlements.plist"
# --identifier app.vox.bar keeps a STABLE code identity across rebuilds.
# Without it, ad-hoc signing derives a fresh `vox-<hash>` identifier every build,
# so macOS TCC treats each rebuild as a brand-new app and the microphone grant
# never sticks (and the prompt stops reappearing) — the mic stays dead.
codesign --force --sign - --identifier app.vox.bar --entitlements "$ent" "$bin" 2>/dev/null \
  || echo "[vox] warning: codesign failed — mic may not work" >&2
exec "$bin" "$@"
