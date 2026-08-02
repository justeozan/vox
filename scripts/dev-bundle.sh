#!/bin/sh
# Build a signed Vox.app and launch it via LaunchServices, so the microphone
# actually works during development.
#
# Why not `tauri dev`? It runs a bare binary whose macOS "responsible process"
# is the launching terminal. macOS refuses to vend the WKWebView microphone
# sandbox extension to a terminal-parented process, so getUserMedia returns a
# LIVE-but-silent track (peak RMS 0) and Vox never hears you. A real .app
# launched with `open` is its own responsible process, macOS shows the mic
# prompt, and capture works. tauri.conf.json wires the entitlement for release
# builds; here we re-sign the debug bundle with a STABLE identity so the TCC
# grant persists across rebuilds.
set -e
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ENT="$ROOT/src-tauri/Entitlements.plist"
TARGET="${VOX_BUNDLE_TARGET:-/tmp/voxbundle}"
APP="$TARGET/debug/bundle/macos/Vox.app"

echo "[dev-bundle] building debug .app (target: $TARGET)…"
cd "$ROOT"
CARGO_TARGET_DIR="$TARGET" npm run tauri build -- --debug --bundles app

echo "[dev-bundle] re-signing with microphone entitlement + stable identity…"
codesign --force --sign - --identifier app.vox.bar --entitlements "$ENT" "$APP/Contents/MacOS/vox"
codesign --force --sign - --identifier app.vox.bar --entitlements "$ENT" "$APP"

echo "[dev-bundle] relaunching…"
# Quit any running Vox (bundle or `tauri dev`) so it doesn't fight for ⌥Space.
pkill -f "Vox.app/Contents/MacOS/vox" 2>/dev/null || true
pkill -f "target/debug/vox" 2>/dev/null || true
sleep 1
open "$APP"
echo "[dev-bundle] launched. On first run, accept the macOS microphone prompt."
