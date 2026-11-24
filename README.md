# ZTab

A Windows/Linux-style **Alt+Tab window switcher for macOS**, written in Rust (objc2 + AppKit).
It switches between *windows*, not apps.

- **⌥ Tab**: open the switcher. Every window of every app is shown with a thumbnail, grouped
  by app, in most-recently-used order.
  - A quick tap jumps straight back to the previous window; tapping again returns (MRU toggle).
- **Hold ⌥** and press **Tab / ⇧Tab** (or the arrow keys) to move, then release ⌥ to switch.
  - **⇧⌥Tab** preselects the least recently used window.
  - **Mouse**: while holding ⌥, hover a tile to select it and click it to switch immediately.
  - **Esc** cancels. **Return** also switches.
- **⌥ `** (backtick): the same switcher, limited to the current app's windows.
- Both shortcuts, the delay before the switcher appears and starting at login are set in the
  **Settings** window (menu-bar icon → *Settings…*, ⌘,); see [Settings](#settings).
- **Multiple displays**: the switcher opens on the screen under the mouse and lists windows from
  every display.
- **Minimized windows, hidden apps, other Spaces and full-screen windows** are listed and can be
  switched to (restored / unhidden / Space switched; see the limitations for Spaces). A window is
  captured when it loses focus, so a minimized window shows a real thumbnail.
- **Never freezes**: the switcher reads a window list that background threads keep up to date,
  so a busy or hung app cannot delay ⌥Tab (reading that list takes about 0.1 ms). The keyboard
  event tap runs on its own thread, so nothing ZTab's main thread does can hold up a keystroke.
  Switching fronts the window through WindowServer first and talks to the app only afterwards, on
  a background thread of that app, so a busy target app delays only its own window raise, never
  the next ⌥Tab or another app's raise.
- **Menu-bar icon** (two overlapping rectangles): shows permission status and has **Settings…** and
  **Quit ZTab**.
  While **secure input** is on, the icon gets a lock and the menu says that macOS is pausing ⌥Tab.

## Build and install

You need the Rust toolchain (`rustup`, stable) and Xcode command line tools.

```sh
scripts/install.sh           # build, copy to ~/Applications/ZTab.app, launch
scripts/install.sh --login   # same, and start ZTab at login (runs `ztab --set-login on`, see below)
scripts/uninstall.sh         # stop it, remove the login item and the app (settings are kept)
scripts/bundle.sh            # just build dist/ZTab.app
scripts/make-icon.sh         # regenerate packaging/AppIcon.icns from scripts/make-icon.swift
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

## Settings

Menu-bar icon → **Settings…** (⌘, while the menu is open) opens a normal window; ZTab activates
itself (it is an accessory app without a Dock icon) and makes the window key. The window is built,
hidden, during launch (30–60 ms of main-thread time, logged as launch work; it waits while a
switcher session is open), so opening it takes a few milliseconds.

| Setting | What it does |
|---|---|
| **Show switcher after** (0–300 ms, default 120) | How long ⌥Tab must be held before the panel appears. A quicker tap switches to the previous window without showing anything. 0 shows the panel at once. Applies to the next ⌥Tab. |
| **Start ZTab at login** | Writes or deletes `~/Library/LaunchAgents/dev.ztab.app.plist` (see below). The checkbox is ticked when that file exists and starts this copy of ZTab. A file that starts another copy (the app was moved, or the box was ticked in a development build) or a program that no longer exists shows the box unticked, with a warning naming that program; ticking it points the file at this copy. |
| **Switch between all windows** (default ⌥Tab) | Click the shortcut, then press the new one. |
| **Switch between windows of the current app** (default ⌥`) | Same. |
| **Restore defaults** | 120 ms, ⌥Tab, ⌥` (the login item is left as it is). |

**Shortcuts** are a *hold* modifier plus a key: exactly one of ⌥, ⌘ or ⌃, held while the
switcher is open (releasing it switches), and any key. ⇧ is never part of a shortcut: held as
well, it opens the switcher on the least recently used window, and ⇧ with the key moves backwards.
The recorder refuses a key without ⌥/⌘/⌃ (it would stop you typing that key), two hold modifiers,
⇧ alone, Esc, Return and the arrow keys (they cancel, switch and move inside the switcher), ⌘Tab
(macOS's app switcher; ZTab does not take it), and the other shortcut. It warns, but accepts,
standard shortcuts such as ⌘Q or ⌘` that ZTab would then take over everywhere, except inside
macOS's own ⌘Tab app switcher: after a ⌘Tab, a ⌘ shortcut does not open ZTab until ⌘ is
released, so its key keeps its meaning there (⌘` moves left, ⌘Q quits the selected app). While the switcher is open,
Tab, `, either shortcut's key and →/← move, ↑/↓ change rows, Return switches, Esc cancels. While a
recorder is capturing, ZTab's event tap lets every key through (so the recorder gets it, and the
keys cannot open the switcher); it resumes when the recorder stops, the window loses focus or
closes (the watchdog also checks this every 1.5 s). The new shortcut is in the tap at once: both
shortcuts are one atomic word that the tap thread reads without ever waiting.

**The Settings window in the switcher**: it is listed like any other window (app "ZTab"), takes
part in the most-recently-used order, and ⌥Tab switches to it (ZTab fronts it through WindowServer
and orders it in on its own main thread, without Accessibility). With it in front, a quick ⌥Tab
goes back to the window before it. Closing it while it has the focus switches to the most recent
other window, so the keyboard focus never stays with a windowless ZTab. It has no minimize button:
minimizing a window, and restoring it, runs the system's animation on the app's main thread (about
half a second each, measured), during which ZTab would not react to ⌥Tab. (Should it be minimized
some other way, the focus moves on as when it closes.)

**Storage**: `~/Library/Application Support/ZTab/settings.conf`, a few `key = value` lines
(`show_delay_ms = 120`, `hotkey_all = option+48`, `hotkey_same_app = option+50`; a shortcut is
`<option|command|control>+<virtual keycode>`). Read once at launch (a fraction of a millisecond);
written on a background thread (to a temporary file, then renamed). A missing file means the
defaults; an invalid line is ignored and logged (`settings: …` in the log).

**Login item**: ZTab alone owns `~/Library/LaunchAgents/dev.ztab.app.plist`. launchd runs the binary
inside the bundle directly (`ProgramArguments` = `…/ZTab.app/Contents/MacOS/ztab`, `RunAtLoad`,
`KeepAlive` false so quitting ZTab or killing it keeps it quit until the next login,
`ProcessType` Interactive, `LimitLoadToSessionType` Aqua, `AssociatedBundleIdentifiers` so System
Settings → General → Login Items names ZTab). The checkbox only writes or deletes the file
(launchd loads it at the next login; unloading the job now would stop a ZTab that launchd
started). The command line also loads or unloads the job:

```sh
~/Applications/ZTab.app/Contents/MacOS/ztab --set-login on    # write the agent, boot it into launchd (starts ZTab now)
~/Applications/ZTab.app/Contents/MacOS/ztab --set-login off   # boot it out (stops a launchd-started ZTab), delete it
~/Applications/ZTab.app/Contents/MacOS/ztab --login-status    # "start at login: <state> (<plist>) program <path>"
```

`--login-status` answers for the binary it is run from: `on` (exit 0) when the agent starts that
binary, `off` (exit 1), `on, but for another copy of ZTab (<path>)` or `broken: the program it starts
does not exist (<path>)` (exit 3). If `launchctl bootstrap` fails (for example when ZTab is switched
off under *Allow in the Background*), `--set-login on` says that the file was written but not
loaded, and `install.sh --login` then starts ZTab directly, so a reinstall never leaves it stopped.

Replacing the binary of a loaded agent (copying a new build over `~/Applications/ZTab.app`) and
starting the job at once fails: launchd refuses the new binary with a launch-constraint violation
(`OS_REASON_CODESIGNING` in `launchctl print gui/$UID/dev.ztab.app`) until Background Task
Management has registered it again, a few seconds later. `install.sh --login` reloads the job
(`--set-login on`), which avoids it.

Only one ZTab runs at a time: each takes an exclusive lock on `dev.ztab.app.lock` in the user's
temporary directory, and a second one (a double launch at login, or the binary started by hand
while ZTab runs) logs `another ZTab instance is running; exiting` and quits at once.

**Tests and isolation**: `--settings-dir <dir>` (or the environment variable `ZTAB_SETTINGS_DIR`)
puts the settings file *and* the login agent inside `<dir>` (`<dir>/settings.conf`,
`<dir>/LaunchAgents/dev.ztab.app.plist`), and `launchctl` is never run: every end-to-end run uses a
fresh directory, so the suites always drive the default ⌥Tab / ⌥` and never change the user's
settings or login item. The log's `settings:` line at startup names the file in use and says
`[isolated]`. `install.sh` and `uninstall.sh` run the binary without `ZTAB_SETTINGS_DIR`, so a
variable exported for tests cannot send the real login item into a test directory.

## How it works

| Module | Responsibility |
|---|---|
| `src/hotkey.rs` | Active `CGEventTap` (HID level) that swallows the two shortcuts (default ⌥Tab / ⌥`; one atomic word, replaceable from any thread) and translates keys while the switcher is open (the session ends when the shortcut's hold modifier is released). It also holds the pure shortcut rules (parsing, validation, key names). It runs on its own high-priority `ztab-tap` thread, which does nothing else: every keyboard event of the session waits for the tap's decision, so that decision never waits for the main thread. Each event is queued to the main thread in order. Re-enables itself when macOS disables it. |
| `src/model.rs` | Persistent window model. One background thread per app hosts that app's `AXObserver` (window created / destroyed / minimized / focus / title notifications) and makes every Accessibility call for it with a 0.5 s timeout, so a hung app only stalls its own thread. Notifications keep the model current; a safety-net rescan reads only `AXWindows` (plus new windows) every 60 s, every 7 s for an app whose notifications could not all be registered, and every window's attributes every 5 minutes. A second background thread keeps the CoreGraphics window list and Space membership (refreshed after window events and every 5 s). The main thread only merges that cached state (`snapshot()`, about 0.1 ms, no IPC) and keeps the most-recently-used window history. |
| `src/windows.rs` | CoreGraphics / Accessibility / SkyLight helpers (window lists, AX attributes, remote-token lookup of other-Space windows, Space membership). Focuses exactly one window with the `Focuser`: a serial `ztab-focus` thread fronts it through SkyLight (`_SLPSSetFrontProcessWithOptions` + make-key-window event records, WindowServer requests only, so a busy app cannot delay it), then a `ztab-raise-<pid>` thread of the target app sends `AXRaise` (unminimizing first when needed), so a raise stuck on one busy app never delays another app's. Each switch has a sequence number: a switch that a newer one replaced is skipped before it is fronted, and its raise stops before its next AX request once a newer switch has been fronted. An AX request already sent cannot be withdrawn, though: a busy app may apply an old raise after a newer switch. When that write returns late (or times out and the app answers again within 3 s), ZTab fronts and raises the newest window again if the old window, or its app, came out on top (checked then and 0.5 s later, within 5 s of the switch); a write applied even later is not covered. |
| `src/thumbs.rs` | Asynchronous ScreenCaptureKit thumbnails with a per-window cache and one small priority scheduler (lost-focus captures, then switcher captures, then background refresh; at most 3 in flight, failures back off). |
| `src/sysstate.rs` | Secure-input detection, session activity (screen lock, user idle) for the background-work pause, the main-thread stall detector, and app icons rasterised off the main thread. |
| `src/ui.rs` | Non-activating `NSPanel` with a vibrancy background, one level above pop-up menus (the overlay level, 102). Grid of tiles: app groups never split across rows, an accent ring marks the selection, hover/click via a tracking area. The panel's content is a tree of Core Animation layers: the tint, group backgrounds, borders and selection ring are layer properties, and thumbnails, icons and one bitmap per tile label (small icon and title) are layer contents. The labels are rendered on a `ztab-labels` thread (AppKit string drawing into a bitmap, the text colour resolved for the panel's appearance, the icon's native small bitmap), queued when a session opens, both selected and not, for the screen it will show on, so the show delay absorbs them; they are cached (least recently used dropped beyond 12 MB). So the main thread rasterises nothing, the panel has no backing store to draw, and the window server composites it on the GPU. The panel is an 8-bit sRGB window and every bitmap it shows is sRGB, so Core Animation's commit hands the images over unconverted on any display; the window server matches the panel to each display. Between sessions the panel stays ordered in, transparent and click-through, so showing it sets its frame, its layers and its opacity without ordering a window in; 50 ms after a session ends it is ordered to the front of its level again (off the show path). Tile layers beyond the current session's tiles hold no images. (`drawRect:` remains for the `--snapshot` PNG and the `draw` measurement mode.) |
| `src/settings.rs` | The settings values and their file, the isolated test location, the login agent (plist generation, `--set-login`), the `ztab-settings` writer thread and the single-instance lock. |
| `src/settings_window.rs` | The Settings window and its shortcut recorders (AppKit, main thread); it only reports what the user did. |
| `src/main.rs` | App lifecycle, menu-bar item, permission flow, session state machine, watchdog, background thumbnail refresh, applying settings, logging. |

Nothing on the main thread waits on another app: no Accessibility call, no window-list or
screen capture call, no icon loading and no disk write (the log has its own thread). The event
tap does not depend on it either: a main-thread stall delays ZTab's own reaction (the panel, the
start of the switch), not the keyboard.

Start-up (login): launchd runs the binary directly. ZTab reads its settings file, then starts the
event tap thread and the window model's background thread before it sets up AppKit, so the tap
is live about 60 ms after launchd starts the process and the first ⌥Tab works once the main run
loop runs (about 120 ms), with the window list read by then; every app's windows are known
about 20 ms later. What the first switch does not need comes later: the menu-bar item about 60 ms
into the run loop, the capture pipeline at 300 ms, an invisible panel show (warm-up) at 400 ms and
the hidden Settings window at 700 ms; each of these waits while a switcher session is open. The
stall detector runs from the first turn of the run loop and logs these steps' units too, as
`launch work on the main thread: <ms> ms`: building the Settings window takes 30–60 ms, and a ⌥Tab
pressed during it is acted on when it ends.

Thumbnails: a window is captured the moment it loses focus, and about a second after it gains
focus. Thumbnails of on-screen windows are refreshed in the background every 2 s when older than
8 s (at most 3 per tick). When the panel appears, windows whose thumbnail is missing or older
than 3 s are captured (minimized windows and hidden apps excepted); a quick ⌥Tab tap that never
shows the panel captures nothing, and captures a session queued but did not start are dropped
when it ends.

Background work pauses while nobody needs it: the thumbnail refresh, the window model's
safety-net rescans and its periodic window-list refresh stop while the screen is locked, the
displays sleep, the login session is switched out, or there has been no input for 2 minutes
(window notifications are still followed). The lock and idle state are read off the main thread
every 2 s; a read that changes the decision is applied at once. While paused, the main thread's
two timers (1.5 s watchdog, 2 s refresh) are replaced by one tick every 10 s (user idle) or 30 s
(screen off); the screen-lock and unlock notifications, display wake and session switches
trigger a read at once, and opening the switcher resumes at once. Resuming after a screen-off
pause rescans every app, spread over a couple of seconds starting 1 s later; resuming from an
idle pause does not (each app's postponed safety-net scan runs at its own time). The log says
`background work paused (<why>)` / `background work resumed` and `timers: Idle | ScreenOff |
Normal`.

### Diagnostics

| What | Where / how |
|---|---|
| Log | `~/Library/Logs/ztab.log`, rotated to `ztab.log.1` at 1 MB (at most ~2 MB kept). Each switcher session logs `open:` (window count, MRU order, `list_ms`/`total_ms` of main-thread work, `labels_queued`: tile labels sent to the `ztab-labels` thread) and `panel shown:` (frame, open-to-show latency, `show_ms`: main-thread time of the show including Core Animation's commit, with its steps `layout= frame= content= order= commit=`, and every tile as `wid:x,y:t` with `t=1` when it had a thumbnail at show time); `panel settled:` follows when the main thread next goes idle (`busy` since the show, the longest unit of work in between, and `key_to_settled_ms`, from the key press to then, also given after the show delay); `panel hidden (hide …, slowest draw …)` gives the longest `drawRect:` of the panel while it was up (draw mode), `tile_layers` / `holding_images` (tile layer sets, and how many hold an image), the label cache (`rendered`, `cached`, `pending`), `thumbs_landed=[wid@ms,…]` the captures that arrived while it was up, and the capture counters, including `on_main_thread` (capture-pipeline steps that ran on the main thread; always 0). At launch, `launch timeline (ms after process start): …`, `hotkey installed … (tap live <ms> ms after process start …)`, `first ⌥Tab usable <ms> ms after process start` and `model ready after …` time the start. A switch logs `commit: wid=…` when it is decided, then `focus: wid=… fronted ok=… after <ms> ms` and `focus: wid=… raised after <ms> ms` from the focus threads (or `skipped` / `superseded` when a newer switch replaced it; `AX write returned after a newer switch started`, then `re-fronted` / `kept` for the late-write check). `open:` and `commit:` carry `tap_to_main_ms`, how long the event waited between the tap thread and the main thread. Focus changes the model saw are logged as `model: ...`. `panel re-fronted (kept ordered in) in <ms> ms` follows each shown session. |
| Main-thread stalls | Two run-loop observers time every unit of main-thread work (one timer, event, notification or dispatched block, including nested loops such as an open menu), from the first turn of the run loop, and log `main thread stall: <ms> ms (run loop, <from> -> <to>)` for each one above 50 ms (`launch work on the main thread: …` instead for a unit in the run loop's first second or one that ran a deferred launch step), with its exact length and the run-loop points it ran between (for example `sources -> loop exit`: a source handler plus the Core Animation commit); a watcher thread logs `main thread still blocked: <ms> ms` while one lasts more than 1 s. The watcher waits on a kqueue timer that the main thread arms when it wakes up and deletes before it sleeps, so it wakes only for a unit that really lasts 1 s: nothing runs while the main thread sleeps or does short work. A summary (wake-ups, units of work, longest unit, watcher wake-ups) is logged every 10 minutes. `--stall-period-ms <n>` adds the older probe detector (a block posted to the main queue every n ms; its stalls are logged without the suffix), and `--stall-summary-s <s>` changes the summary interval: `open -g ~/Applications/ZTab.app --args --stall-period-ms 10 --stall-summary-s 30` (quit ZTab first). `--stall-threshold-ms <n>` changes the 50 ms threshold. `--inject-stall-ms <n>` blocks the main thread once, 3 s after launch (`--inject-stall-at-s <s>` changes when), to check the detectors and the event tap (`e2e_tapstall`). `--no-menu-warmup` skips laying the status menu out at launch (for comparison). |
| Background-work pause | `--idle-pause-s <s>` changes the idle time after which background work pauses (default 120). |
| Settings | `--settings-dir <dir>` / `ZTAB_SETTINGS_DIR`: isolated settings and login agent (tests); `--set-login on\|off [--program <path>]`, `--login-status`: the login item from the command line (see [Settings](#settings)). The log says `settings: <file> (loaded\|no file: defaults): …` at startup and `settings: <why>: …` / `settings: saved …` on every change. |
| Secure input | `secure input on (frontmost: …)` / `secure input off` lines, plus the menu item. macOS reports the frontmost app, which is not necessarily the app that enabled it. |
| Dump the window model | `open -W -n dist/ZTab.app --args --dump-model /tmp/m.txt` (current window, MRU order, every window) |
| Dump a one-shot window list | `dist/ZTab.app/Contents/MacOS/ztab --dump-windows /tmp/w.txt` |
| Render the switcher to a PNG | `open -W -n dist/ZTab.app --args --snapshot /tmp/s.png` |
| Panel benchmark | `open -W -n -g dist/ZTab.app --args --panel-bench /tmp/pb.txt [--bench-variants tree+keep+rgba8,draw] [--bench-rounds 3] [--bench-long-gap-s 30] [--bench-snapshot /tmp/p.png]` runs beside a running ZTab and posts no input: it shows a panel with every window on each display in turn and writes, per show, `show_ms` and how long the main thread stayed busy (`busy_ms`). Variants: `draw` (`drawRect:`) or `tree` (layer tree), plus `keep` (kept ordered in), `rgba8` (8-bit sRGB window), `perdisplay`, `nofill`. Variants in one run share the thumbnail images and so Core Animation's per-image conversion cache: compare variants in separate runs. `--bench-snapshot` captures the panel through ScreenCaptureKit as the window server shows it. |
| Label colours | `ztab --label-check <out>` (posts no input; the panel stays transparent) renders two tile labels with the process in the light appearance, switches the process to the dark one, renders them again and writes the text brightness of each (`RESULT PASS` when the text is dark, then light), plus `<out>.light.png` / `<out>.dark.png`. |
| Panel mode | `ZTAB_PANEL_MODE=<variant>` (for example `open -g --env ZTAB_PANEL_MODE=draw ~/Applications/ZTab.app`) runs ZTab with another panel mode, for measurements; the default is `tree+keep+rgba8`. The `panel warm-up:` line at launch shows the mode in use. |
| End-to-end tests | Synthetic HID key and mouse events against a running ZTab. Bundle each one with `scripts/bundle.sh <bin> dist/<bin>/ZTab.app` and run it with `open -W -n -g dist/<bin>/ZTab.app --args <out.txt> …`. Needs an unlocked screen. |
| `e2e` | Quick tap, MRU toggle, hold and navigate, Esc, and clicking a tile while holding ⌥. |
| `e2e_more <out> <scratch-dir>` | Every display (panel on the mouse's screen, click a tile there), a minimized window, a hidden app, ⇧⌥Tab (least recently used), and ⌥` (between two scratch Finder windows it opens). It opens and closes scratch Finder windows and a TextEdit document. |
| `e2e_spaces <out> <scratch-dir>` | A full-screen window and MRU across Spaces, a SIGSTOPped (hung) app (panel within 250 ms, no stall, and a fresh thumbnail of its window captured while it is stopped with no capture step on ZTab's main thread; only when TextEdit was not already running), MRU after activations from outside ZTab, the thumbnail of a minimized window, the secure-input menu warning (the status menu is opened once before secure input as a control), and a switch away from a SIGSTOPped app while its raise is blocked (the next window is fronted and raised within 200 ms and is in front after the app resumes; only when TextEdit was not already running). Scratch windows only. |
| `e2e_tapstall <out>` | Against a ZTab started with `--inject-stall-ms 1500 --inject-stall-at-s <s>`: during the stall it posts ⌥ down, Tab, Esc, ⌥ up and checks with a listen-only tap that the Option changes went through at once, that Tab and Esc were swallowed, and that the tap took the Open during the stall. |
| `e2e_settings <out> <scratch-dir> <settings-dir>` | Against a ZTab started with `--settings-dir <settings-dir>` (it refuses to run otherwise, and checks that the real settings file and login agent are unchanged): opens Settings from the menu; the window is listed, current, and switched to and from; the delay slider at 0 and 300 ms (open-to-show timings); the recorder (⌥Tab while capturing opens nothing, a bare key is refused, ⌃F18 is accepted); ⌃F18 opens the switcher without F18/Esc reaching the app in front, the old ⌥Tab passes through, releasing ⌃ switches; Restore defaults; the login checkbox writes and removes the isolated agent (valid plist, runs the binary); closing the window gives the focus back; reopened, it cannot be minimized (its minimize button is disabled and a press leaves it in front). Scratch Finder windows only. |
| `e2e_quit <out> <ztab-pid>` | Clicks the menu-bar icon, then "Quit ZTab". |
| `bench_switcher <out> holds\|cycles <n> [hold_ms]` | Not a test: holds ⌥Tab (Esc, no switch) n times and reports the `open:` / `panel shown:` timings ZTab logged (memory checks: `cycles 100`). |
| `bench_churn <out> <scratch-dir> <rounds> <cycles>` | Not a test (memory checks): per round, opens two scratch Finder windows and a scratch TextEdit document, holds ⌥Tab `cycles` times (a real switch every tenth), and closes them again. |
| Unit tests | `cargo test --bins` |

## Known limitations

- **Shortcuts macOS keeps for itself** (⌘Tab; ⌘Space while Spotlight uses it) are taken by macOS
  before any app sees them, so the recorder cannot capture them (not exercised live). ⌘Tab and the
  arrow keys are refused outright (also in the settings file); the others are accepted with a
  warning when they do reach the recorder. Key names in the window follow the US layout (the
  keycode is what counts). That a ⌘ shortcut stays out of macOS's ⌘Tab switcher is unit-tested
  only (the live suites post no ⌘ keys).
- **Login item disabled in System Settings**: General → Login Items → *Allow in the Background*
  can switch the agent off; launchd then does not start ZTab although the file exists, and the
  checkbox (which reads the file) still shows it on.

- **Secure input**: while it is on (a focused password field, Terminal's *Secure Keyboard
  Entry*), macOS does not deliver keys to event taps, so ⌥Tab does nothing. ZTab cannot change
  that; it shows the state in the menu-bar icon and menu instead.
- **Other Spaces and full-screen windows**: windows ZTab has seen keep their cached
  Accessibility handle; a window on another Space that ZTab never saw on screen is found through
  CoreGraphics plus an Accessibility remote-token sweep. Switching to a window on another Space or
  a full-screen Space relies on SkyLight switching the Space. The `e2e_spaces` full-screen check
  (to and from a full-screen window's Space) passes; switching to a window on another regular
  desktop Space is not verified (only one Space per display was available).
- **Window positions are up to 5 s old**: tile aspect ratios come from the cached window list. A
  window that just appeared may briefly show with a placeholder size, and for apps whose
  Accessibility support is broken a closed window can stay listed for up to 5 s.
- **Most-recently-used history starts empty** at launch: until windows have been used, they are
  ordered front to back. A window becomes "used" after it has had focus for a quarter second (or
  immediately when ZTab switches to it).
- **Thumbnails** need Screen Recording; without it tiles show the app icon. A window without a
  cached thumbnail shows its app icon until the capture started when the panel appears lands.
  Captured images are copied into plain sRGB bitmaps off the main thread (ScreenCaptureKit returns
  IOSurface-backed images, which made the first panel draw after a capture cost 50–120 ms of
  main-thread time; and a bitmap in another colour space than the panel's is colour-converted
  pixel by pixel inside Core Animation's commit on the main thread, again after every move to
  another display); the panel is also shown once, invisibly, at launch.
- **Remaining main-thread stalls outside ZTab's code** (seen with the run-loop detector): opening
  ZTab's status menu costs 50–80 ms inside AppKit's menu presentation. When the panel appears
  under the mouse pointer, or the pointer moves into it, AppKit resets the cursor, which costs
  5–30 ms in the system's accessibility cursor code (`AXFMouseCursorGenerator`, sampled); it runs
  right after the panel is on screen. Showing the panel itself takes 1–5 ms (median 2) of
  main-thread time, commit included, on any display, the first show on a display included (tile
  labels are rendered on the `ztab-labels` thread during the show delay; they took 9–21 ms of
  main-thread time on a display's first show when they were rendered on the main thread, and
  15–65 ms when the panel was drawn in `drawRect:` and its images were colour-converted in the
  commit). With a show delay of 0, or a title that changed, a label not rendered yet appears a few
  milliseconds after the panel (the same title's other rendering stays until then).
  These no longer hold the keyboard (the tap has its own thread), but ZTab handles the events on
  the main thread: a Tab or the ⌥ release that arrives during one is acted on (selection moved,
  switch started) when it ends.
- **The panel stays ordered in between sessions**: fully transparent, ignoring the mouse, one level
  above pop-up menus (so an open menu or another app's pop-up-level window does not cover it),
  excluded from window cycling and stationary in Mission Control, where it was last shown. Tools
  that list every on-screen window without checking its alpha see it (`kCGWindowAlpha` 0). A
  window at the same level (102) ordered in after the last session covers the panel for one
  session: the panel is ordered to the front of its level again after each session, not before a
  show.
- **A busy app can apply an old raise late**: see `src/windows.rs` above. ZTab corrects it when it
  happens within about 3 s; later than that, the old window can come out on top.
- **Screen lock / unlock** are taken from loginwindow's `com.apple.screenIsLocked` /
  `screenIsUnlocked` notifications, with the 30 s tick as the fallback; this path, display sleep
  and session switches have unit-tested decision logic but were not exercised live.
