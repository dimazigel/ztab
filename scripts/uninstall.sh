#!/bin/bash
# Stop ZTab, remove its login item and the installed app. Settings stay in
# ~/Library/Application Support/ZTab (delete that folder to reset them).
APP="$HOME/Applications/ZTab.app"
AGENT="$HOME/Library/LaunchAgents/dev.ztab.app.plist"
if [ -x "$APP/Contents/MacOS/ztab" ]; then
  # Without ZTAB_SETTINGS_DIR (exported for tests, it would point at the test directory).
  env -u ZTAB_SETTINGS_DIR "$APP/Contents/MacOS/ztab" --set-login off || true
fi
launchctl bootout "gui/$(id -u)/dev.ztab.app" 2>/dev/null || true
rm -f "$AGENT"
pkill -x ztab 2>/dev/null || true
rm -rf "$APP"
echo "ZTab removed (privacy grants can be removed in System Settings > Privacy & Security)"
