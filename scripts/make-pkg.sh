#!/bin/bash
# Build an installer package: dist/ZTab-<version>.pkg.
# It installs ZTab.app into /Applications for every user of the Mac, so Installer asks for an
# administrator's password. Its scripts run as root and act for the user logged in at the screen:
# preinstall quits their ZTab; postinstall removes an old ~/Applications/ZTab.app of theirs (and its
# receipt), turns on their login item (kept off on an upgrade if they had turned it off) and starts
# ZTab. With nobody at the screen (an install over ssh) only the app is installed. Apple silicon,
# macOS 14+. The app is signed like scripts/bundle.sh signs it; ZTAB_PKG_SIGN_IDENTITY signs the
# package itself. Neither removes the Gatekeeper prompt for a downloaded copy: that needs Developer
# ID signing plus notarization.
# Install from the command line: sudo installer -pkg dist/ZTab-<version>.pkg -target /
set -euo pipefail
cd "$(dirname "$0")/.."
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
PLIST_VERSION=$(/usr/libexec/PlistBuddy -c 'Print CFBundleShortVersionString' packaging/Info.plist)
[ "$VERSION" = "$PLIST_VERSION" ] || { echo "version mismatch: Cargo.toml $VERSION, Info.plist $PLIST_VERSION" >&2; exit 1; }
for s in packaging/pkg/scripts/*; do bash -n "$s"; done
scripts/bundle.sh ztab dist/ZTab.app
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/root/Applications"
ditto --noqtn dist/ZTab.app "$WORK/root/Applications/ZTab.app"
codesign --verify --strict "$WORK/root/Applications/ZTab.app"
# Never relocate: Installer would otherwise "upgrade" any other ZTab.app it finds (e.g. a build
# folder or an old ~/Applications copy) instead of /Applications/ZTab.app. Not version-checked: a
# rebuild with the same version number replaces the installed app.
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
echo "built $OUT ($(du -h "$OUT" | cut -f1)); install: sudo installer -pkg $OUT -target /"
