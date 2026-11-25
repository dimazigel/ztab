#!/bin/bash
# Build ZTab.app, install it to /Applications (for every user of this Mac) and start it at login,
# like the installer package. An administrator can write to /Applications without sudo; anyone else
# gets a clear error (use the package: it asks for an administrator's password).
# Usage: scripts/install.sh [--no-login]   (--login, the default, is still accepted)
#
# Migration: a per-user copy in ~/Applications (bundle id dev.ztab.app; from 0.1.0 or 0.2.1–0.2.4)
# is
# removed, with its package receipt in the home folder, so there is exactly one ZTab.
#
# Login item: ZTab owns it, per user. `ztab --set-login on` writes
# ~/Library/LaunchAgents/dev.ztab.app.plist (launchd runs /Applications/ZTab.app/Contents/MacOS/ztab
# directly) and loads it into launchd, which starts ZTab right away (RunAtLoad), exactly as at
# login. With --no-login it is left off, unless it was on (then it is pointed at the new copy, so
# it does not start a removed one); left off, that choice is recorded (`ztab --set-login off`), so
# ZTab's first launch does not turn it on by itself. If launchd refuses the job (for example ZTab is switched off
# under System Settings > General > Login Items > Allow in the Background), ZTab is started
# directly instead, so it is never left stopped.
#
# The binary runs without ZTAB_SETTINGS_DIR: exported for tests, it would send the login item
# into the test directory.
set -euo pipefail
cd "$(dirname "$0")/.."
DEST="/Applications/ZTab.app"
OLD="$HOME/Applications/ZTab.app"
AGENT="$HOME/Library/LaunchAgents/dev.ztab.app.plist"
LOGIN=1
for a in "$@"; do
  case "$a" in
    --no-login) LOGIN=0 ;;
    --login) ;;
    *) echo "usage: scripts/install.sh [--no-login]" >&2; exit 2 ;;
  esac
done

# Before anything is stopped: can this user replace /Applications/ZTab.app?
if [ ! -w /Applications ]; then
  echo "install.sh: $(id -un) cannot write to /Applications (not an administrator)." >&2
  echo "  Build the installer package (scripts/make-pkg.sh) and open it: it asks for an administrator's password." >&2
  exit 1
elif [ -e "$DEST" ] && [ -n "$(find "$DEST" ! -user "$(id -u)" -print -quit 2>/dev/null)" ]; then
  echo "install.sh: $DEST was installed by the package (it belongs to $(stat -f %Su "$DEST"))." >&2
  echo "  Install the new version with the package, or remove that copy first:" >&2
  echo "  sudo rm -rf $DEST && sudo pkgutil --forget dev.ztab.app.pkg" >&2
  exit 1
fi

scripts/bundle.sh
pkill -x ztab 2>/dev/null || true
for _ in $(seq 1 50); do pgrep -x ztab >/dev/null || break; sleep 0.1; done
rm -rf "$DEST"
cp -R dist/ZTab.app "$DEST"

# A per-user copy of earlier versions (never the copy just installed, should ~/Applications
# lead to /Applications).
if [ -d "$OLD" ] && ! [ "$OLD" -ef "$DEST" ] \
  && [ "$(/usr/libexec/PlistBuddy -c 'Print CFBundleIdentifier' "$OLD/Contents/Info.plist" 2>/dev/null)" = dev.ztab.app ]; then
  rm -rf "$OLD"
  echo "removed the old copy $OLD"
fi
if pkgutil --volume "$HOME" --pkgs 2>/dev/null | grep -qx dev.ztab.app.pkg; then
  pkgutil --volume "$HOME" --forget dev.ztab.app.pkg >/dev/null && echo "forgot the old package receipt in $HOME"
fi

if [ "$LOGIN" = 1 ] || [ -f "$AGENT" ]; then
  if ! env -u ZTAB_SETTINGS_DIR "$DEST/Contents/MacOS/ztab" --set-login on; then
    echo "install.sh: the login item was not loaded; starting ZTab directly" >&2
    open -g "$DEST" --args --background
  fi
else
  # Recorded in the settings (login_item_offered), so ZTab's first launch leaves it off.
  env -u ZTAB_SETTINGS_DIR "$DEST/Contents/MacOS/ztab" --set-login off >/dev/null || true
  open -g "$DEST" --args --background
fi
echo "installed $DEST"
