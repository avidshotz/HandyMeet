#!/bin/sh
# Gracefully stop any running HandyMeet (local dev build) instance.
#
# Always use this instead of `pkill`/`kill -9` when testing rebuilds.
# Two failure modes this exists to prevent, both hit during actual
# development of this fork:
#
#   1. An abrupt kill mid-meeting orphans the meeting in "recording" status
#      in the database (no matching stop). The app's own RunEvent::Exit
#      handler now stops an in-progress meeting on a clean --quit/Cmd+Q/tray
#      Quit, but that only runs if the process actually gets a chance to
#      shut down — a SIGKILL skips it entirely.
#   2. Repeatedly rebuilding and relaunching without ever fully quitting the
#      *previous* instance can leave a days-old process running invisibly
#      (a tray app's window-close does not quit it). That stale process
#      keeps whatever macOS permission state it had cached at launch, so a
#      later `tccutil reset` or permission grant has no visible effect until
#      the process is actually restarted — this looked exactly like a
#      permissions bug and cost real time to track down.
#
# Tries `--quit` first (routed through tauri_plugin_single_instance — only
# works between two instances macOS itself recognizes as "the same app" via
# LaunchServices, i.e. both launched through `open`/Finder/Dock; it's a
# no-op against a directly-run binary like the ones this repo's own dev
# scripts tend to launch). Falls back to SIGTERM, which the app now also
# handles gracefully via its own signal handler (same RunEvent::Exit cleanup
# either way — stops an in-progress meeting, unloads the model) regardless
# of how the process was started. SIGKILL is only a last resort for a
# genuinely hung process.
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP_BIN="$ROOT/src-tauri/target/debug/bundle/macos/HandyMeet.app/Contents/MacOS/handy"
PATTERN="MacOS/handy"

wait_for_exit() {
    attempts="$1"
    i=0
    while [ "$i" -lt "$attempts" ]; do
        if ! pgrep -f "$PATTERN" >/dev/null 2>&1; then
            return 0
        fi
        i=$((i + 1))
        sleep 0.5
    done
    return 1
}

if ! pgrep -f "$PATTERN" >/dev/null 2>&1; then
    echo "No running HandyMeet instance found."
    exit 0
fi

if [ -x "$APP_BIN" ]; then
    echo "Asking the running instance to quit cleanly (--quit)..."
    "$APP_BIN" --quit >/dev/null 2>&1 || true
    if wait_for_exit 3; then
        echo "Stopped cleanly via --quit."
        exit 0
    fi
fi

echo "Sending SIGTERM (also handled gracefully by the app itself)..."
pkill -TERM -f "$PATTERN" 2>/dev/null || true
if wait_for_exit 10; then
    echo "Stopped via SIGTERM."
    exit 0
fi

echo "Still running after SIGTERM; sending SIGKILL (last resort — this is exactly the situation this script exists to avoid needing)..."
pkill -KILL -f "$PATTERN" 2>/dev/null || true
sleep 0.5
if pgrep -f "$PATTERN" >/dev/null 2>&1; then
    echo "WARNING: a HandyMeet process is still alive after SIGKILL." >&2
    exit 1
fi
echo "Stopped via SIGKILL."
