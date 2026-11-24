#!/bin/bash
# Build ZTab.app, install it to ~/Applications and (optionally) start it at login.
# Usage: scripts/install.sh [--login]
#
# --login: ZTab owns its login item. `ztab --set-login on` writes
# ~/Library/LaunchAgents/dev.ztab.app.plist (launchd runs the binary inside the bundle directly)
# and loads it into launchd, which starts ZTab right away (RunAtLoad), exactly as at login. The
# Settings window's "Start ZTab at login" checkbox writes or deletes the same file. If launchd
# refuses the job (for example ZTab is switched off under System Settings > General > Login Items
# > Allow in the Background), ZTab is started directly instead, so it is never left stopped.
#
# The binary runs without ZTAB_SETTINGS_DIR: exported for tests, it would send the login item
# into the test directory.
set -euo pipefail
cd "$(dirname "$0")/.."
scripts/bundle.sh
DEST="$HOME/Applications/ZTab.app"
pkill -x ztab 2>/dev/null || true
for _ in $(seq 1 50); do pgrep -x ztab >/dev/null || break; sleep 0.1; done
mkdir -p "$HOME/Applications"
rm -rf "$DEST"
cp -R dist/ZTab.app "$DEST"
if [ "${1:-}" = "--login" ]; then
  if ! env -u ZTAB_SETTINGS_DIR "$DEST/Contents/MacOS/ztab" --set-login on; then
    echo "install.sh: the login item was not loaded; starting ZTab directly" >&2
    open -g "$DEST"
  fi
else
  open -g "$DEST"
fi
echo "installed $DEST"
