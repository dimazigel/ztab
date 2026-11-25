#!/bin/bash
# Stop ZTab, remove this user's login item, the app in /Applications (and an old copy in
# ~/Applications) and the package receipts. Settings stay in ~/Library/Application Support/ZTab
# (delete that folder to reset them). Other users of this Mac keep their login item file
# (~/Library/LaunchAgents/dev.ztab.app.plist), which then starts nothing.
#
# A copy installed by the package belongs to root: removing it (and its receipt) runs sudo, which
# asks for your password in a terminal.
#
# Usage: scripts/uninstall.sh [--user]
# --user: remove only your own copy (~/Applications/ZTab.app, a "me only" install), its receipt in
# your home folder and your login item; a shared /Applications/ZTab.app is left for everyone else,
# and nothing runs sudo.
USER_ONLY=0
case "${1:-}" in
  --user) USER_ONLY=1 ;;
  "") ;;
  *) echo "usage: scripts/uninstall.sh [--user]" >&2; exit 2 ;;
esac
APP="/Applications/ZTab.app"
OLD="$HOME/Applications/ZTab.app"
AGENT="$HOME/Library/LaunchAgents/dev.ztab.app.plist"
for a in "$APP" "$OLD"; do
  if [ -x "$a/Contents/MacOS/ztab" ]; then
    # Without ZTAB_SETTINGS_DIR (exported for tests, it would point at the test directory).
    env -u ZTAB_SETTINGS_DIR "$a/Contents/MacOS/ztab" --set-login off || true
    break
  fi
done
launchctl bootout "gui/$(id -u)/dev.ztab.app" 2>/dev/null || true
rm -f "$AGENT"
pkill -x ztab 2>/dev/null || true
for _ in $(seq 1 50); do pgrep -x ztab >/dev/null || break; sleep 0.1; done

if [ -d "$OLD" ] && ! [ "$OLD" -ef "$APP" ]; then rm -rf "$OLD" && echo "removed $OLD"; fi
if pkgutil --volume "$HOME" --pkgs 2>/dev/null | grep -qx dev.ztab.app.pkg; then
  pkgutil --volume "$HOME" --forget dev.ztab.app.pkg >/dev/null && echo "forgot the package receipt in $HOME"
fi
if [ "$USER_ONLY" = 1 ]; then
  [ -e "$APP" ] && ! [ "$APP" -ef "$OLD" ] && echo "left $APP (installed for all users) in place"
  echo "ZTab removed for $(id -un) (privacy grants can be removed in System Settings > Privacy & Security)"
  exit 0
fi
if [ -e "$APP" ]; then
  if [ -w /Applications ] && [ -z "$(find "$APP" ! -user "$(id -u)" -print -quit 2>/dev/null)" ]; then
    rm -rf "$APP" && echo "removed $APP"
  else
    echo "$APP was installed by the package: removing it needs an administrator (sudo)."
    sudo rm -rf "$APP" && echo "removed $APP" || echo "not removed; run: sudo rm -rf $APP" >&2
  fi
fi
if pkgutil --pkgs 2>/dev/null | grep -qx dev.ztab.app.pkg; then
  sudo pkgutil --forget dev.ztab.app.pkg >/dev/null && echo "forgot the package receipt" \
    || echo "receipt kept; run: sudo pkgutil --forget dev.ztab.app.pkg" >&2
fi
echo "ZTab removed (privacy grants can be removed in System Settings > Privacy & Security)"
