#!/bin/bash
# Build ZTab.app, install it to ~/Applications and (optionally) start it at login.
# Usage: scripts/install.sh [--login]
set -euo pipefail
cd "$(dirname "$0")/.."
scripts/bundle.sh
DEST="$HOME/Applications/ZTab.app"
pkill -x ztab 2>/dev/null || true
mkdir -p "$HOME/Applications"
rm -rf "$DEST"
cp -R dist/ZTab.app "$DEST"
AGENT="$HOME/Library/LaunchAgents/dev.ztab.app.plist"
if [ "${1:-}" = "--login" ]; then
  mkdir -p "$HOME/Library/LaunchAgents"
  cat > "$AGENT" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>dev.ztab.app</string>
  <key>ProgramArguments</key><array><string>/usr/bin/open</string><string>-g</string><string>$DEST</string></array>
  <key>RunAtLoad</key><true/>
</dict></plist>
PLIST
  launchctl bootout "gui/$(id -u)" "$AGENT" 2>/dev/null || true
  launchctl bootstrap "gui/$(id -u)" "$AGENT"
  echo "ZTab will start at login ($AGENT)"
else
  open -g "$DEST"
fi
echo "installed $DEST"
