# ZTab

A Windows/Linux-style **Alt+Tab window switcher for macOS**, written in Rust (objc2 + AppKit).
It switches between *windows*, not apps.

- **⌥ Tab**: open the switcher. Every window of every app is shown with a live thumbnail, and
  each app's windows are grouped together.
  - A quick tap jumps straight back to the previous window.
- **Hold ⌥** and press **Tab / ⇧Tab** (or the arrow keys) to move, then release ⌥ to switch.
  - **Mouse**: while holding ⌥, hover a tile to select it and click it to switch immediately.
  - **Esc** cancels. **Return** also switches.
- **⌥ `** (backtick): the same switcher, limited to the current app's windows.
- **Multiple displays**: the switcher opens on the screen under the mouse and lists windows from
  every display.
- **Menu-bar icon** (two overlapping rectangles): shows permission status and has **Quit ZTab**.
- **What gets switched to**: minimized windows are restored and hidden apps are unhidden.
  Windows on other Spaces are listed on a best-effort basis.

## Build and install

You need the Rust toolchain (`rustup`, stable) and Xcode command line tools.

```sh
scripts/install.sh           # build, copy to ~/Applications/ZTab.app, launch
scripts/install.sh --login   # same, and also start ZTab at login (LaunchAgent)
scripts/uninstall.sh         # stop and remove it
scripts/bundle.sh            # just build dist/ZTab.app
```

**First launch: grant ZTab two permissions** in *System Settings → Privacy & Security*:

| Permission | Needed for |
|---|---|
| **Accessibility** | Catching ⌥Tab and focusing the chosen window. Without it nothing works. |
| **Screen & System Audio Recording** | Window thumbnails. Without it, tiles show app icons. |

The menu-bar menu shows each permission's state and links straight to its settings page.

### Code signing and why it matters

macOS ties privacy grants to the app's code signature. `bundle.sh` signs with the keychain
identity `ztab-local-codesign` if it exists, so the grants survive rebuilds. Otherwise it signs
ad hoc, and you have to grant the permissions again after every rebuild. To create that
self-signed identity (once):

```sh
cat > /tmp/cs.cnf <<'EOF'
[req]
distinguished_name=dn
x509_extensions=ext
prompt=no
[dn]
CN=ztab-local-codesign
[ext]
basicConstraints=critical,CA:false
keyUsage=critical,digitalSignature
extendedKeyUsage=critical,codeSigning
EOF
openssl req -x509 -newkey rsa:2048 -nodes -keyout /tmp/k.pem -out /tmp/c.pem -days 3650 -config /tmp/cs.cnf
openssl pkcs12 -export -legacy -inkey /tmp/k.pem -in /tmp/c.pem -out /tmp/id.p12 -passout pass:ztab
security import /tmp/id.p12 -k ~/Library/Keychains/login.keychain-db -P ztab -T /usr/bin/codesign
rm /tmp/k.pem /tmp/c.pem /tmp/id.p12 /tmp/cs.cnf
```

### Other Option+Tab tools

Quit any other switcher bound to ⌥Tab (for example AltTab) or change its shortcut. ZTab's
event tap sits at the HID level and normally gets the keys first, but two switchers at once is
confusing.

## How it works

| Module | Responsibility |
|---|---|
| `src/hotkey.rs` | Active `CGEventTap` (HID level) that swallows ⌥Tab and translates keys while the switcher is open. Re-enables itself when macOS disables it. |
| `src/windows.rs` | Builds the window list from CoreGraphics (IDs, z-order) and Accessibility (titles, minimized state, subrole filtering), grouped by app in most-recently-used order. Focuses exactly one window via SkyLight `_SLPSSetFrontProcessWithOptions` + make-key-window event records + `AXRaise`. |
| `src/thumbs.rs` | Asynchronous thumbnails from ScreenCaptureKit (`SCScreenshotManager`) with a per-window cache, falling back to `CGWindowListCreateImage`. |
| `src/ui.rs` | Non-activating `NSPanel` with a vibrancy background. Grid of tiles: app groups never split across rows, an accent ring marks the selection, hover/click via a tracking area. |
| `src/main.rs` | App lifecycle, menu-bar item, permission flow, session state machine, watchdog. |

### Diagnostics

| What | Where / how |
|---|---|
| Log | `~/Library/Logs/ztab.log`, rotated to `ztab.log.1` at 1 MB (at most ~2 MB kept) |
| Dump the window list | `dist/ZTab.app/Contents/MacOS/ztab --dump-windows /tmp/w.txt` |
| Render the switcher to a PNG | `open -W -n dist/ZTab.app --args --snapshot /tmp/s.png` |
| End-to-end tests | Synthetic HID key and mouse events against a running ZTab. Bundle each one with `scripts/bundle.sh <bin> dist/<bin>/ZTab.app` and run it with `open -W -n -g dist/<bin>/ZTab.app --args <out.txt> …`. Needs an unlocked screen. |
| `e2e` | Quick tap, MRU toggle, hold and navigate, Esc, and clicking a tile while holding ⌥. |
| `e2e_more <out> <scratch-dir>` | Every display (panel on the mouse's screen, click a tile there), a minimized window, a hidden app, Shift+⌥Tab, and ⌥`. It opens and closes a scratch Finder window and a TextEdit document. |
| `e2e_quit <out> <ztab-pid>` | Clicks the menu-bar icon, then "Quit ZTab". |
| Unit tests | `cargo test` |

## Known limitations

- **Other Spaces and full-screen apps**: these windows come from CoreGraphics heuristics plus an
  Accessibility token sweep. Focusing them switches Space, but this was not tested on a
  multi-Space setup.
- **Secure input**: while it is on (password fields, Terminal's *Secure Keyboard Entry*), macOS
  does not deliver keys to event taps, so ⌥Tab does nothing.
- **Minimized windows**: they have no live backing store. They show their last captured
  thumbnail, or the app icon.
