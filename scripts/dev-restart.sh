#!/bin/sh
# Stop any running HandyMeet instance (see dev-stop.sh for why this matters),
# re-sign the already-built debug .app, and launch it fresh.
#
# Doesn't build — run `bun run tauri build --debug` first. Re-signing is
# needed on every rebuild: `codesign --force --sign -` (ad-hoc) silently
# drops the mic/audio entitlements unless given --entitlements explicitly,
# and hardened runtime + ad-hoc signing (Tauri's default bundle config) is a
# broken combination for a non-Developer-ID build — macOS hard-blocks the
# launch instead of offering the usual "unidentified developer" override.
#
# Uses `open` (not a direct binary exec) so macOS attributes permission
# prompts to HandyMeet itself, not to whatever launched this script.
# First launch of a freshly-rebuilt binary still needs one manual
# right-click > Open in Finder (ad-hoc signing has no Developer ID for
# Gatekeeper to trust automatically) — this script can't do that click for
# you, only the person at the keyboard can.
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP="$ROOT/src-tauri/target/debug/bundle/macos/HandyMeet.app"
ENTITLEMENTS="$ROOT/src-tauri/Entitlements.plist"

if [ ! -d "$APP" ]; then
    echo "Error: $APP not found. Build it first:" >&2
    echo "  CMAKE_POLICY_VERSION_MINIMUM=3.5 bun run tauri build --debug" >&2
    exit 1
fi

"$ROOT/scripts/dev-stop.sh"

echo "Re-signing (ad-hoc, entitlements preserved, hardened runtime off)..."
codesign --deep --force --sign - --entitlements "$ENTITLEMENTS" "$APP"

echo "Launching..."
open "$APP"
