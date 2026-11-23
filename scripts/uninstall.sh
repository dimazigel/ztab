#!/bin/bash
# Stop ZTab, remove the login agent and the installed app.
AGENT="$HOME/Library/LaunchAgents/dev.ztab.app.plist"
launchctl bootout "gui/$(id -u)" "$AGENT" 2>/dev/null || true
rm -f "$AGENT"
pkill -x ztab 2>/dev/null || true
rm -rf "$HOME/Applications/ZTab.app"
echo "ZTab removed (privacy grants can be removed in System Settings > Privacy & Security)"
