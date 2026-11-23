#!/bin/bash
# Build ZTab.app: release build, bundle, sign.
# Usage: scripts/bundle.sh [bin-name (default ztab)] [app-path (default dist/ZTab.app)]
set -euo pipefail
cd "$(dirname "$0")/.."
BIN="${1:-ztab}"
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:$PATH"
cargo build --release --bin "$BIN"
APP="${2:-dist/ZTab.app}"
rm -rf "$APP"
mkdir -p "$(dirname "$APP")" "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "target/release/$BIN" "$APP/Contents/MacOS/ztab"
cp packaging/Info.plist "$APP/Contents/Info.plist"
[ -f packaging/AppIcon.icns ] && cp packaging/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
# Prefer a stable signing identity so macOS privacy grants survive rebuilds.
IDENTITY="${ZTAB_SIGN_IDENTITY:-ztab-local-codesign}"
if security find-identity -p codesigning | grep -q "\"$IDENTITY\""; then
  codesign -f -s "$IDENTITY" --identifier dev.ztab.app "$APP"
else
  echo "warning: identity '$IDENTITY' not found; ad-hoc signing (permissions reset on every rebuild)" >&2
  codesign -f -s - --identifier dev.ztab.app "$APP"
fi
echo "built $APP"
