#!/bin/bash
# Build an installer package: dist/ZTab-<version>.pkg.
# It installs ZTab.app into ~/Applications of the user who runs it (no administrator password),
# quits a running ZTab first, then turns on the login item (kept off on an upgrade if the user had
# turned it off) and starts ZTab (postinstall). Apple silicon, macOS 14+. The app is signed like
# scripts/bundle.sh signs it; ZTAB_PKG_SIGN_IDENTITY signs the package itself. Neither removes the
# Gatekeeper prompt for a downloaded copy: that needs Developer ID signing plus notarization.
# Install from the command line: installer -pkg dist/ZTab-<version>.pkg -target CurrentUserHomeDirectory
set -euo pipefail
cd "$(dirname "$0")/.."
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
PLIST_VERSION=$(/usr/libexec/PlistBuddy -c 'Print CFBundleShortVersionString' packaging/Info.plist)
[ "$VERSION" = "$PLIST_VERSION" ] || { echo "version mismatch: Cargo.toml $VERSION, Info.plist $PLIST_VERSION" >&2; exit 1; }
scripts/bundle.sh ztab dist/ZTab.app
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/root/Applications"
ditto dist/ZTab.app "$WORK/root/Applications/ZTab.app"
# Never relocate: Installer would otherwise "upgrade" any other ZTab.app it finds (e.g. a build
# folder) instead of ~/Applications/ZTab.app. Not version-checked: a rebuild with the same
# version number replaces the installed app.
pkgbuild --analyze --root "$WORK/root" "$WORK/component.plist" >/dev/null 2>&1
plutil -replace 0.BundleIsRelocatable -bool NO "$WORK/component.plist"
plutil -replace 0.BundleIsVersionChecked -bool NO "$WORK/component.plist"
pkgbuild --root "$WORK/root" --component-plist "$WORK/component.plist" --install-location / \
  --scripts packaging/pkg/scripts --identifier dev.ztab.app.pkg --version "$VERSION" \
  "$WORK/ZTab-component.pkg" >/dev/null
sed "s/__VERSION__/$VERSION/" packaging/pkg/distribution.xml > "$WORK/distribution.xml"
OUT="dist/ZTab-$VERSION.pkg"
SIGN=()
[ -n "${ZTAB_PKG_SIGN_IDENTITY:-}" ] && SIGN=(--sign "$ZTAB_PKG_SIGN_IDENTITY")
productbuild --distribution "$WORK/distribution.xml" --resources packaging/pkg/resources \
  --package-path "$WORK" ${SIGN[@]+"${SIGN[@]}"} "$OUT" >/dev/null
echo "built $OUT ($(du -h "$OUT" | cut -f1))"
