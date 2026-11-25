//! End-to-end checks of ZTab's Settings window against a running ZTab that uses an ISOLATED
//! settings location (`--settings-dir <dir>`): the suite refuses to touch anything otherwise, and
//! checks at the end that the user's real settings file and login agent are unchanged.
//!
//!  A) the window opens from the status menu ("Settings…"): ZTab becomes the active app and the
//!     window is in front;
//!  B) the window behaves like any window in the switcher: with it key, ⌥Tab lists it as the
//!     current window and preselects the next app's most recent window (scratch Finder window A;
//!     the Settings window is ZTab's only window); a quick ⌥Tab goes to A; from A, the Settings
//!     window comes right after all of Finder's windows, and stepping there switches back to it;
//!  C) show delay: the slider set to 0 ms (mouse click at its left end) shows the panel at once,
//!     300 ms (right end) shows it after 300 ms and not for a 200 ms hold;
//!  D) recorder: while it captures, ⌥Tab does not open the switcher (it is recorded instead); a
//!     key without ⌥/⌘/⌃ is refused; ⌃F18 is accepted (title, settings file);
//!  E) the new shortcut: with A in front, holding ⌃F18 opens the switcher and F18 / Esc never
//!     reach A (a listen-only session tap downstream of ZTab's sees no F18 or Esc); the old ⌥Tab
//!     no longer opens it (it passes through to A, a scratch Finder window, where it is inert);
//!     stepping with ⌃F18 switches to the Settings window, and a quick ⌃F18 (⌃ released) from it
//!     back to A;
//!  F) "Restore defaults": ⌥Tab / ⌥` / 120 ms again (titles, label, file); ⌥Tab opens the
//!     switcher, ⌃F18 does not;
//!  G) start at login in the isolated mode. First what ZTab's first launch did with the
//!     directory, against an expectation this suite derives for itself (or takes from
//!     `--expect-first-launch on|off|kept`, see `FirstLaunchExpect`): `on` (an installed ZTab, in
//!     /Applications or ~/Applications, started without a settings file or an agent): the box is
//!     ticked, the agent was written after ZTab started and runs its binary, and
//!     `login_item_offered = true` is in the settings; `kept` (an agent older than ZTab's start,
//!     such as `run_round.sh`'s STALE_AGENT one, or a settings file at its start): the agent is as
//!     it was, the box shows whether it runs this binary, and the question is recorded; `off`
//!     (not an installed copy: no bundle, target/ or dist/): no agent, unticked, nothing recorded.
//!     Then the checkbox (unticked first when it is ticked) writes `<dir>/LaunchAgents/
//!     dev.ztab.app.plist` (valid, runs ZTab's binary directly, replacing a stale agent) and
//!     deletes it again; the real `~/Library/LaunchAgents/dev.ztab.app.plist` is not touched;
//!  H) closing the window (its close button, through AX) while it is key gives the focus back
//!     to the most recent window (A);
//!  I) it cannot be minimized (minimizing runs the system's animation on ZTab's main thread for
//!     about half a second): reopened from the menu, its minimize button is disabled, and a press
//!     on it leaves the window in front, not minimized.
//!
//! Only keys ZTab swallows, or that are inert in a scratch Finder window (⌥Tab, ⌃F18, F18), are
//! posted, and only while that scratch window or ZTab's own window is in front.
//!
//! Usage: `open -W -n -g <bundle> --args <out.txt> <scratch-dir> <settings-dir>
//! [--expect-first-launch on|off|kept]`.

#[path = "../windows.rs"]
#[allow(dead_code)]
mod windows;

use std::ffi::c_void;
use std::io::Write;
use std::process::Command;
use std::ptr::NonNull;
use std::sync::Mutex;
use std::thread::sleep;
use std::time::{Duration, Instant};

use objc2_app_kit::{NSRunningApplication, NSWorkspace};
use objc2_application_services::{AXError, AXUIElement, AXValue, AXValueType};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFMachPort, CFNumber, CFRetained, CFRunLoop, CFString, CFType, CGPoint, CGSize, kCFRunLoopCommonModes,
    kCFRunLoopDefaultMode,
};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType, CGMouseButton, CGWindowListOption,
};
use objc2_foundation::NSString;

const MAGIC: i64 = 0x5E771;
const OPTION: u16 = 0x3A;
const CONTROL: u16 = 0x3B;
const TAB: u16 = 0x30;
const ESC: u16 = 0x35;
const F18: u16 = 0x4F;
const TITLE: &str = "ZTab Settings";
const RECORDING: &str = "Type shortcut…";

// ---------------------------------------------------------------------------------------------
// Keys and the downstream sentinel
// ---------------------------------------------------------------------------------------------

/// Tagged key events that reached the session level (after ZTab's HID tap): (time, type, code).
static SEEN: Mutex<Vec<(f64, u32, u16)>> = Mutex::new(Vec::new());
/// Untagged keyboard events at the session level: someone else typing during the suite (they
/// can end a held switcher session, so a failure next to one is not trustworthy).
static FOREIGN: Mutex<Vec<(f64, u32, u16)>> = Mutex::new(Vec::new());

unsafe extern "C-unwind" fn sentinel(_p: CGEventTapProxy, ty: CGEventType, event: NonNull<CGEvent>, _u: *mut c_void) -> *mut CGEvent {
    // SAFETY: valid event for the duration of the callback.
    let ev = unsafe { event.as_ref() };
    let code = CGEvent::integer_value_field(Some(ev), CGEventField::KeyboardEventKeycode) as u16;
    let store = if CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceUserData) == MAGIC { &SEEN } else { &FOREIGN };
    if let Ok(mut v) = store.lock() {
        v.push((now(), ty.0, code));
    }
    event.as_ptr()
}

fn start_sentinel() {
    std::thread::spawn(|| {
        let mask = (1u64 << CGEventType::KeyDown.0) | (1 << CGEventType::KeyUp.0) | (1 << CGEventType::FlagsChanged.0);
        // SAFETY: callback matches CGEventTapCallBack; no user info.
        let tap = unsafe {
            CGEvent::tap_create(
                CGEventTapLocation::SessionEventTap,
                CGEventTapPlacement::TailAppendEventTap,
                CGEventTapOptions::ListenOnly,
                mask,
                Some(sentinel),
                std::ptr::null_mut(),
            )
        };
        let Some(tap) = tap else { return };
        let Some(src) = CFMachPort::new_run_loop_source(None, Some(&tap), 0) else { return };
        // SAFETY: reading an immutable CF constant.
        if let Some(rl) = CFRunLoop::current() {
            rl.add_source(Some(&src), unsafe { kCFRunLoopCommonModes });
        }
        CGEvent::tap_enable(&tap, true);
        CFRunLoop::run();
    });
}

/// Key downs/ups (not flag changes) with keycode `code` the sentinel saw since `t`.
fn seen_keys_since(t: f64, code: u16) -> usize {
    SEEN.lock()
        .map(|v| v.iter().filter(|(ts, ty, c)| *ts >= t && *c == code && *ty != CGEventType::FlagsChanged.0).count())
        .unwrap_or(0)
}

thread_local! {
    static SRC: CFRetained<CGEventSource> = CGEventSource::new(CGEventSourceStateID::HIDSystemState).unwrap();
}

fn key(code: u16, down: bool, flags: CGEventFlags) {
    let ev = SRC.with(|s| CGEvent::new_keyboard_event(Some(s), code, down)).unwrap();
    CGEvent::set_flags(Some(&ev), flags);
    CGEvent::set_integer_value_field(Some(&ev), CGEventField::EventSourceUserData, MAGIC);
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
    sleep(Duration::from_millis(25));
}
const OPT: CGEventFlags = CGEventFlags::MaskAlternate;
const CTRL: CGEventFlags = CGEventFlags::MaskControl;
fn press_key(code: u16, flags: CGEventFlags) {
    key(code, true, flags);
    key(code, false, flags);
}
fn opt_down() {
    key(OPTION, true, OPT);
}
fn opt_up() {
    key(OPTION, false, CGEventFlags::empty());
}
fn ctrl_down() {
    key(CONTROL, true, CTRL);
}
fn ctrl_up() {
    key(CONTROL, false, CGEventFlags::empty());
}
fn release_modifiers() {
    opt_up();
    ctrl_up();
}

fn mouse(kind: CGEventType, p: CGPoint) {
    let ev = SRC.with(|s| CGEvent::new_mouse_event(Some(s), kind, p, CGMouseButton::Left)).unwrap();
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
    sleep(Duration::from_millis(50));
}
fn click(p: CGPoint) {
    mouse(CGEventType::MouseMoved, CGPoint { x: p.x + 2.0, y: p.y });
    mouse(CGEventType::MouseMoved, p);
    mouse(CGEventType::LeftMouseDown, p);
    mouse(CGEventType::LeftMouseUp, p);
}
fn mouse_location() -> Option<CGPoint> {
    CGEvent::new(None).map(|e| CGEvent::location(Some(&e)))
}

// ---------------------------------------------------------------------------------------------
// ZTab's log
// ---------------------------------------------------------------------------------------------

fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64()
}

fn log_since(t: f64) -> Vec<(f64, String)> {
    let base = std::env::var("HOME").unwrap() + "/Library/Logs/ztab.log";
    let mut text = std::fs::read_to_string(format!("{base}.1")).unwrap_or_default();
    text += &std::fs::read_to_string(&base).unwrap_or_default();
    text.lines()
        .filter_map(|l| {
            let rest = l.strip_prefix('[')?;
            let (ts, msg) = rest.split_once("] ")?;
            let ts: f64 = ts.parse().ok()?;
            (ts >= t).then(|| (ts, msg.to_string()))
        })
        .collect()
}

fn lines_with(t: f64, prefix: &str) -> Vec<String> {
    log_since(t).into_iter().filter(|(_, m)| m.starts_with(prefix)).map(|(_, m)| m).collect()
}

fn num_after(line: &str, key: &str) -> Option<f64> {
    line.split(key).nth(1)?.split(|c: char| c != '.' && !c.is_ascii_digit()).next()?.parse().ok()
}

fn commits_since(t: f64) -> Vec<u32> {
    lines_with(t, "commit: wid=").iter().filter_map(|m| m.strip_prefix("commit: wid=")?.split(' ').next()?.parse().ok()).collect()
}

// ---------------------------------------------------------------------------------------------
// Waiting, windows, apps
// ---------------------------------------------------------------------------------------------

fn pump(secs: f64) {
    // Pumping keeps NSWorkspace's frontmost app and running-app list current.
    let end = Instant::now() + Duration::from_secs_f64(secs);
    while Instant::now() < end {
        let _ = CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.05, false);
    }
}

fn wait_for<T>(timeout_s: f64, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let end = Instant::now() + Duration::from_secs_f64(timeout_s);
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        if Instant::now() >= end {
            return None;
        }
        pump(0.1);
    }
}

fn frontmost_pid() -> Option<i32> {
    NSWorkspace::sharedWorkspace().frontmostApplication().map(|a| a.processIdentifier())
}

/// The topmost on-screen normal window of any app but this one: (wid, pid).
fn top_any() -> Option<(u32, i32)> {
    let me = std::process::id() as i32;
    windows::cg_window_list(CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements)
        .iter()
        .filter_map(|d| windows::parse_cg(d))
        .find(|(_, pid, layer, b, alpha, _)| *layer == 0 && *alpha > 0.0 && *pid != me && b.2 >= 50.0 && b.3 >= 50.0)
        .map(|(wid, pid, ..)| (wid, pid))
}

/// ZTab's Settings window id, while it is on screen (CG, by owner and title).
fn settings_wid(pid: i32) -> Option<u32> {
    windows::cg_window_list(CGWindowListOption::OptionOnScreenOnly)
        .iter()
        .filter_map(|d| windows::parse_cg(d))
        .find(|(_, p, layer, _, _, name)| *p == pid && *layer == 0 && name.as_deref() == Some(TITLE))
        .map(|(wid, ..)| wid)
}

/// ZTab's open status menu: an on-screen window of `pid` at the pop-up menu level (101).
fn menu_window_open(pid: i32) -> bool {
    windows::cg_window_list(CGWindowListOption::OptionOnScreenOnly)
        .iter()
        .filter_map(|d| windows::parse_cg(d))
        .any(|(_, p, layer, _, alpha, _)| p == pid && layer == 101 && alpha > 0.0)
}

/// The running ZTab: the oldest live process with ZTab's bundle id other than this one.
fn ztab_pid() -> Option<i32> {
    let me = std::process::id() as i32;
    let apps = NSRunningApplication::runningApplicationsWithBundleIdentifier(&NSString::from_str("dev.ztab.app"));
    apps.iter()
        .filter(|a| !a.isTerminated())
        .map(|a| (a.processIdentifier(), a.launchDate().map(|d| d.timeIntervalSince1970()).unwrap_or(f64::MAX)))
        .filter(|(p, _)| *p != me && unsafe { libc::kill(*p, 0) } == 0)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(p, _)| p)
}

fn osa(script: &str) -> String {
    let o = Command::new("osascript").args(["-e", script]).output().unwrap();
    String::from_utf8_lossy(&o.stdout).trim().to_string() + String::from_utf8_lossy(&o.stderr).trim()
}

/// Open (or re-front) a scratch Finder window on `dir`; returns its window id once it is in front.
fn front_finder(dir: &str) -> Option<u32> {
    let _ = std::fs::create_dir_all(dir);
    let name = std::path::Path::new(dir).file_name()?.to_string_lossy().to_string();
    let _ = Command::new("open").args(["-a", "Finder", dir]).status();
    let w = wait_for(8.0, || windows::list_windows().into_iter().find(|w| w.app_name == "Finder" && w.title == name))?;
    wait_for(4.0, || (top_any().map(|t| t.0) == Some(w.wid)).then_some(w.wid))
}

fn close_finder(dir: &str) {
    let name = std::path::Path::new(dir).file_name().unwrap().to_string_lossy().to_string();
    osa(&format!(
        "tell application \"Finder\"\nrepeat with w in (get every Finder window whose name is \"{name}\")\nclose w\nend repeat\nend tell"
    ));
}

// ---------------------------------------------------------------------------------------------
// Accessibility of ZTab's own UI
// ---------------------------------------------------------------------------------------------

fn attr(el: &AXUIElement, name: &str) -> Option<CFRetained<CFType>> {
    windows::ax_attr(el, name)
}
fn children(el: &AXUIElement) -> Vec<CFRetained<AXUIElement>> {
    let Some(v) = attr(el, "AXChildren") else { return vec![] };
    let Ok(arr) = v.downcast::<CFArray>() else { return vec![] };
    let arr: CFRetained<CFArray<AXUIElement>> = unsafe { CFRetained::cast_unchecked(arr) };
    arr.to_vec()
}
fn str_attr(el: &AXUIElement, name: &str) -> Option<String> {
    attr(el, name).and_then(|v| v.downcast_ref::<CFString>().map(|s| s.to_string()))
}
fn num_attr(el: &AXUIElement, name: &str) -> Option<f64> {
    let v = attr(el, name)?;
    if let Some(n) = v.downcast_ref::<CFNumber>() {
        return n.as_f64();
    }
    v.downcast_ref::<CFBoolean>().map(|b| if b.as_bool() { 1.0 } else { 0.0 })
}
fn frame(el: &AXUIElement) -> Option<(f64, f64, f64, f64)> {
    let pos = attr(el, "AXPosition")?;
    let size = attr(el, "AXSize")?;
    let (pos, size) = (pos.downcast_ref::<AXValue>()?, size.downcast_ref::<AXValue>()?);
    let mut p = CGPoint::default();
    let mut s = CGSize::default();
    unsafe {
        if !pos.value(AXValueType::CGPoint, NonNull::from(&mut p).cast::<c_void>()) {
            return None;
        }
        if !size.value(AXValueType::CGSize, NonNull::from(&mut s).cast::<c_void>()) {
            return None;
        }
    }
    Some((p.x, p.y, s.width, s.height))
}
fn center(el: &AXUIElement) -> Option<CGPoint> {
    frame(el).map(|(x, y, w, h)| CGPoint { x: x + w / 2.0, y: y + h / 2.0 })
}
fn set_bool(el: &AXUIElement, name: &str, v: bool) -> AXError {
    unsafe { el.set_attribute_value(&CFString::from_str(name), CFBoolean::new(v)) }
}

fn press(el: &AXUIElement) -> AXError {
    unsafe { el.perform_action(&CFString::from_str("AXPress")) }
}
fn find_titled(el: &AXUIElement, wanted: &str, depth: usize) -> Option<CFRetained<AXUIElement>> {
    for c in children(el) {
        if str_attr(&c, "AXTitle").as_deref() == Some(wanted) {
            return Some(c);
        }
        if depth > 0
            && let Some(f) = find_titled(&c, wanted, depth - 1)
        {
            return Some(f);
        }
    }
    None
}
fn find_id(el: &AXUIElement, id: &str, depth: usize) -> Option<CFRetained<AXUIElement>> {
    for c in children(el) {
        if str_attr(&c, "AXIdentifier").as_deref() == Some(id) {
            return Some(c);
        }
        if depth > 0
            && let Some(f) = find_id(&c, id, depth - 1)
        {
            return Some(f);
        }
    }
    None
}

struct Ui {
    app: CFRetained<AXUIElement>,
}

impl Ui {
    fn new(pid: i32) -> Ui {
        let app = unsafe { AXUIElement::new_application(pid) };
        unsafe { app.set_messaging_timeout(2.0) };
        Ui { app }
    }
    fn window(&self) -> Option<CFRetained<AXUIElement>> {
        let v = attr(&self.app, "AXWindows")?;
        let arr = v.downcast::<CFArray>().ok()?;
        let arr: CFRetained<CFArray<AXUIElement>> = unsafe { CFRetained::cast_unchecked(arr) };
        arr.to_vec().into_iter().find(|w| str_attr(w, "AXTitle").as_deref() == Some(TITLE))
    }
    fn el(&self, id: &str) -> Option<CFRetained<AXUIElement>> {
        let w = self.window()?;
        find_id(&w, id, 4)
    }
    fn title_of(&self, id: &str) -> Option<String> {
        let el = self.el(id)?;
        str_attr(&el, "AXTitle")
    }
    fn value_of(&self, id: &str) -> Option<String> {
        let el = self.el(id)?;
        str_attr(&el, "AXValue").or_else(|| num_attr(&el, "AXValue").map(|n| n.to_string()))
    }
    fn press(&self, id: &str) -> Option<AXError> {
        let el = self.el(id)?;
        Some(press(&el))
    }
    /// Open the status menu and click "Settings…". Returns (menu opened, item clicked).
    fn open_from_menu(&self, pid: i32, out: &mut Out) -> (bool, bool) {
        let Some(bar) = attr(&self.app, "AXExtrasMenuBar").and_then(|v| v.downcast::<AXUIElement>().ok()) else {
            out.note("no AXExtrasMenuBar".into());
            return (false, false);
        };
        let Some(icon) = children(&bar).first().and_then(|i| center(i)) else {
            out.note("status item has no frame".into());
            return (false, false);
        };
        click(icon);
        let open = wait_for(2.0, || menu_window_open(pid).then_some(())).is_some();
        let item = if open { find_titled(&bar, "Settings…", 4).as_deref().and_then(center) } else { None };
        out.note(format!("status icon at ({:.0}, {:.0}); menu open={open}; Settings… item at {item:?}", icon.x, icon.y));
        match item {
            Some(p) => {
                click(p);
                (true, true)
            }
            None => {
                if open {
                    // Close the menu again (never leave it open on the user's screen).
                    click(icon);
                }
                (open, false)
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------------------------

struct Out {
    f: std::fs::File,
    pass: bool,
}
impl Out {
    fn check(&mut self, name: &str, ok: bool, detail: String) {
        self.pass &= ok;
        let _ = writeln!(self.f, "CHECK {name}: {detail} -> {ok}");
    }
    fn note(&mut self, s: String) {
        let _ = writeln!(self.f, "  {s}");
    }
}

fn file_or_missing(p: &std::path::Path) -> String {
    std::fs::read_to_string(p).unwrap_or_else(|_| "<missing>".into())
}

fn settings_text(dir: &str) -> String {
    std::fs::read_to_string(format!("{dir}/settings.conf")).unwrap_or_default()
}

/// Wait until the settings file contains every one of `lines`.
fn wait_settings(dir: &str, lines: &[&str]) -> bool {
    wait_for(3.0, || {
        let t = settings_text(dir);
        lines.iter().all(|l| t.lines().any(|x| x.trim() == *l)).then_some(())
    })
    .is_some()
}

/// What ZTab's first launch should have done with the isolated login item (check G).
#[derive(Clone, Debug, PartialEq, Eq)]
enum FirstLaunchExpect {
    /// Turned on: ticked, an agent written after ZTab started that runs its binary, recorded.
    On,
    /// Left as it was (ZTab ran before with this directory): the agent unchanged since before
    /// ZTab started (or none), the box ticked only if it runs this binary, recorded.
    Kept,
    /// Not an installed copy: no agent, unticked, nothing recorded.
    Off,
    /// This suite cannot tell (why): pass `--expect-first-launch`.
    Unknown(String),
}

/// The expectation for G, independent of ZTab's own decision (not read from its log):
/// `--expect-first-launch` when given; else from the binary's place (in /Applications or
/// ~/Applications: installed; outside an app bundle, or under target/ or dist/: not installed;
/// anything else: unknown) and, for an installed copy, from what was there when ZTab started: an
/// agent file older than ZTab's start (`pre_seeded`) or a settings file (`no file: defaults`
/// missing from ZTab's settings line) means it ran before (`Kept`), nothing means `On`.
fn first_launch_expect(flag: Option<&str>, bin: &str, home: &str, pre_seeded: bool, settings_existed: bool) -> FirstLaunchExpect {
    match flag {
        Some("on") => return FirstLaunchExpect::On,
        Some("off") => return FirstLaunchExpect::Off,
        Some("kept") => return FirstLaunchExpect::Kept,
        Some(v) => return FirstLaunchExpect::Unknown(format!("--expect-first-launch {v:?}: not on, off or kept")),
        None => {}
    }
    let installed = bin.starts_with("/Applications/") || bin.starts_with(&format!("{home}/Applications/"));
    let not_installed = !bin.contains(".app/Contents/MacOS/") || bin.contains("/target/") || bin.contains("/dist/");
    if not_installed {
        FirstLaunchExpect::Off
    } else if !installed {
        FirstLaunchExpect::Unknown(format!("{bin} is neither in /Applications nor clearly a build; pass --expect-first-launch on|off|kept"))
    } else if pre_seeded || settings_existed {
        FirstLaunchExpect::Kept
    } else {
        FirstLaunchExpect::On
    }
}

fn mtime_secs(p: &std::path::Path) -> Option<f64> {
    let t = std::fs::metadata(p).ok()?.modified().ok()?;
    Some(t.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs_f64())
}

fn binary_path(pid: i32) -> String {
    let o = Command::new("ps").args(["-o", "comm=", "-p", &pid.to_string()]).output().unwrap();
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// Hold the opener (`hold_flag` + `code`) for `hold_ms`, then Esc and release: returns the
/// (open line, panel line) logged for that session, if any. A session that switched instead of
/// being cancelled is reported (with any foreign key events): the checks after it would run
/// against the wrong window.
fn hold_and_cancel(hold_key: u16, hold_flag: CGEventFlags, code: u16, hold_ms: u64) -> (Option<String>, Option<String>, f64) {
    let t = now();
    key(hold_key, true, hold_flag);
    press_key(code, hold_flag);
    sleep(Duration::from_millis(hold_ms));
    press_key(ESC, hold_flag);
    key(hold_key, false, CGEventFlags::empty());
    sleep(Duration::from_millis(400));
    let open = lines_with(t, "open: ").into_iter().next();
    let shown = lines_with(t, "panel shown:").into_iter().next();
    let commits = commits_since(t);
    if !commits.is_empty() {
        UNEXPECTED.lock().unwrap().push(format!("a held session at {t:.3} switched to {commits:?} instead of being cancelled; foreign keys {:?}", foreign_since(t)));
    }
    (open, shown, t)
}

/// Problems the checks would otherwise only see indirectly.
static UNEXPECTED: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn foreign_since(t: f64) -> Vec<String> {
    FOREIGN.lock().map(|v| v.iter().filter(|(ts, ..)| *ts >= t).map(|(ts, ty, c)| format!("{ts:.3}:{ty}:{c:#x}")).collect()).unwrap_or_default()
}

/// An `open:` line's preselected index (`selected=`) and the switcher's window order (`order=`).
fn open_order(line: &str) -> (Option<usize>, Vec<u32>) {
    let sel = line.split(" selected=").nth(1).and_then(|r| r.split(' ').next()?.parse().ok());
    let order = line
        .split(" order=")
        .nth(1)
        .and_then(|r| r.split_whitespace().next())
        .map(|r| r.split(',').filter_map(|w| w.parse().ok()).collect())
        .unwrap_or_default();
    (sel, order)
}

/// Owner pids of windows (CoreGraphics), for checking the switcher's app grouping.
fn owner_pids(wids: &[u32]) -> std::collections::HashMap<u32, i32> {
    windows::cg_describe(wids).iter().filter_map(|d| windows::parse_cg(d)).map(|f| (f.0, f.1)).collect()
}

/// Switch to `target` by stepping: hold the opener, then press its key until the selection is on
/// `target`'s tile (by the order ZTab logged for the session), and release. Other windows are only
/// selected on the way, never switched to: if `target` is not after the preselected tile, the
/// session is cancelled. Returns (commits, the session's window order, `target`'s index in it).
fn step_to(hold_key: u16, hold_flag: CGEventFlags, code: u16, target: u32) -> (Vec<u32>, Vec<u32>, Option<usize>) {
    let t = now();
    key(hold_key, true, hold_flag);
    press_key(code, hold_flag);
    let open = wait_for(1.5, || lines_with(t, "open: ").into_iter().next());
    let (sel, order) = open.as_deref().map(open_order).unwrap_or((None, Vec::new()));
    let idx = order.iter().position(|w| *w == target);
    match (sel, idx) {
        (Some(s), Some(i)) if i >= s => (s..i).for_each(|_| press_key(code, hold_flag)),
        _ => press_key(ESC, hold_flag),
    }
    sleep(Duration::from_millis(150));
    key(hold_key, false, CGEventFlags::empty());
    sleep(Duration::from_millis(700));
    (commits_since(t), order, idx)
}

/// A quick tap of the opener (hold modifier released right away): returns the commits logged.
fn quick_tap(hold_key: u16, hold_flag: CGEventFlags, code: u16) -> Vec<u32> {
    let t = now();
    key(hold_key, true, hold_flag);
    press_key(code, hold_flag);
    key(hold_key, false, CGEventFlags::empty());
    sleep(Duration::from_millis(700));
    commits_since(t)
}

// ---------------------------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let f = std::fs::File::create(&args[1]).unwrap();
    let mut out = Out { f, pass: true };
    let scratch = args.get(2).cloned().unwrap_or_else(|| "/tmp".into());
    let Some(sdir) = args.get(3).cloned() else {
        let _ = writeln!(out.f, "usage: e2e_settings <out> <scratch-dir> <settings-dir> [--expect-first-launch on|off|kept]\nRESULT FAIL");
        return;
    };
    let expect_flag = args.iter().position(|a| a == "--expect-first-launch").and_then(|i| args.get(i + 1)).cloned();
    let _activity = objc2_foundation::NSProcessInfo::processInfo().beginActivityWithOptions_reason(
        objc2_foundation::NSActivityOptions::UserInitiatedAllowingIdleSystemSleep | objc2_foundation::NSActivityOptions::LatencyCritical,
        &NSString::from_str("ZTab end-to-end timings"),
    );
    let start = now();
    let Some(pid) = ztab_pid() else {
        let _ = writeln!(out.f, "ZTab is not running\nRESULT FAIL");
        return;
    };
    // Guard: ZTab must be using exactly this isolated settings directory.
    let lines = log_since(0.0);
    let started = lines.iter().rposition(|(_, m)| m.starts_with(&format!("starting pid={pid} ")));
    let started_at = started.map(|i| lines[i].0).unwrap_or(0.0);
    let settings_line = started.and_then(|i| lines[i..].iter().find(|(_, m)| m.starts_with("settings: /")).map(|(_, m)| m.clone()));
    let want = format!("settings: {sdir}/settings.conf [isolated]");
    let isolated = settings_line.as_deref().is_some_and(|l| l.starts_with(&want));
    out.note(format!("ZTab pid {pid}; its settings line: {settings_line:?}"));
    if !isolated {
        out.check("ZTab runs with the isolated settings dir", false, format!("expected a line starting with {want:?}; nothing was touched"));
        let _ = writeln!(out.f, "RESULT FAIL");
        return;
    }
    let home = std::path::PathBuf::from(std::env::var("HOME").unwrap());
    let real_settings = home.join("Library/Application Support/ZTab/settings.conf");
    let real_agent = home.join("Library/LaunchAgents/dev.ztab.app.plist");
    let real_before = (file_or_missing(&real_settings), file_or_missing(&real_agent));
    let original_mouse = mouse_location();
    let dir_a = format!("{scratch}/settings-A");
    start_sentinel();
    let ui = Ui::new(pid);
    // For G: what was there when ZTab started (its settings line says whether a settings file was).
    // Positive evidence only: an unrecognised line counts as "no file", so G expects `on` (strict).
    let settings_existed = settings_line.as_deref().is_some_and(|l| l.contains(" (loaded, "));
    let fl = FirstLaunchCtx { started_at, settings_existed, flag: expect_flag, home: home.to_string_lossy().into_owned() };
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&mut out, pid, &ui, &sdir, &dir_a, &fl)));
    if r.is_err() {
        out.check("panic", false, "suite panicked".into());
    }

    // Cleanup, whatever happened: modifiers up, the Settings window closed, the scratch window
    // closed, the mouse back.
    release_modifiers();
    if let Some(w) = ui.window() {
        if num_attr(&w, "AXMinimized") == Some(1.0) {
            set_bool(&w, "AXMinimized", false);
            pump(1.0);
        }
        if let Some(close) = attr(&w, "AXCloseButton").and_then(|v| v.downcast::<AXUIElement>().ok()) {
            press(&close);
        }
    }
    close_finder(&dir_a);
    if let Some(p) = original_mouse {
        mouse(CGEventType::MouseMoved, p);
    }
    let unexpected = UNEXPECTED.lock().map(|v| v.clone()).unwrap_or_default();
    let foreign = foreign_since(start);
    out.note(format!("keyboard events from outside this suite (user input) during it: {}: {foreign:?}", foreign.len()));
    out.check("no held session switched unexpectedly", unexpected.is_empty(), format!("{unexpected:?}"));
    let real_after = (file_or_missing(&real_settings), file_or_missing(&real_agent));
    out.check(
        "the user's real settings file and login agent are untouched",
        real_before == real_after,
        format!("settings.conf same={} agent plist same={}", real_before.0 == real_after.0, real_before.1 == real_after.1),
    );
    let stalls: Vec<String> = log_since(start)
        .into_iter()
        .filter(|(_, m)| m.contains("main thread stall") || m.contains("still blocked"))
        .map(|(t, m)| format!("[{t:.3}] {m}"))
        .collect();
    let worst = stalls.iter().filter_map(|s| num_after(s, "stall: ")).fold(0.0f64, f64::max);
    out.note(format!("main-thread stalls during the suite: {stalls:?}"));
    out.check(
        "no main-thread stall above 100 ms",
        worst <= 100.0 && !stalls.iter().any(|s| s.contains("still blocked")),
        format!("worst {worst:.0} ms"),
    );
    let _ = writeln!(out.f, "RESULT {}", if out.pass { "PASS" } else { "FAIL" });
}

/// What G needs to know about ZTab's launch (see `first_launch_expect`).
struct FirstLaunchCtx {
    /// ZTab's `starting pid=` log line time (epoch seconds).
    started_at: f64,
    /// A settings file was there when ZTab started.
    settings_existed: bool,
    flag: Option<String>,
    home: String,
}

fn run(out: &mut Out, pid: i32, ui: &Ui, sdir: &str, dir_a: &str, fl: &FirstLaunchCtx) {
    // Scratch window A in front, so the Settings window has a "previous" window.
    let Some(a) = front_finder(dir_a) else {
        out.check("setup: scratch Finder window A in front", false, "not found".into());
        return;
    };
    out.note(format!("A = Finder wid {a}"));

    // A) Open from the status menu.
    let t = now();
    let (menu_open, clicked) = ui.open_from_menu(pid, out);
    let sw = wait_for(3.0, || settings_wid(pid));
    let front = wait_for(3.0, || (frontmost_pid() == Some(pid) && top_any().map(|x| x.0) == sw && sw.is_some()).then_some(()));
    let shown = lines_with(t, "settings window shown");
    let active = lines_with(t, "settings window: app active=");
    out.check(
        "A/opens from the menu, in front, ZTab active",
        menu_open && clicked && sw.is_some() && front.is_some(),
        format!("menu_open={menu_open} clicked={clicked} wid={sw:?} front={:?} frontmost_pid={:?} log={shown:?} {active:?}", top_any(), frontmost_pid()),
    );
    let Some(sw) = sw else { return };
    if front.is_none() {
        return;
    }
    let (rec_all, rec_same) = (ui.title_of("ztab.hotkey.all"), ui.title_of("ztab.hotkey.same"));
    let delay_label = ui.value_of("ztab.delay.value");
    out.check(
        "A/shows the defaults",
        rec_all.as_deref() == Some("⌥Tab") && rec_same.as_deref() == Some("⌥`") && delay_label.as_deref() == Some("120 ms"),
        format!("all={rec_all:?} same={rec_same:?} delay={delay_label:?}"),
    );

    // B) Own window in the switcher.
    let (open, shown, _) = hold_and_cancel(OPTION, OPT, TAB, 500);
    let open_s = open.clone().unwrap_or_default();
    let mru_first: Option<u32> = open_s.split("mru=").nth(1).and_then(|m| m.split([',', ' ']).next()?.parse().ok());
    let selected: Option<u32> = open_s.split(" wid=").nth(1).and_then(|m| m.split(' ').next()?.parse().ok());
    let listed = shown.as_deref().is_some_and(|l| l.contains(&format!("tiles={sw}:")) || l.contains(&format!(";{sw}:")));
    // The Settings window is ZTab's only window: the window after it is the next app's most
    // recent one, A.
    out.check(
        "B/listed as the current window, the next app's most recent window (A) preselected",
        open_s.contains("current=Some(") && mru_first == Some(sw) && selected == Some(a) && listed,
        format!("mru_first={mru_first:?} selected={selected:?} listed={listed} open={:?}", open_s.chars().take(220).collect::<String>()),
    );
    let c1 = quick_tap(OPTION, OPT, TAB);
    let f1 = wait_for(2.0, || (top_any().map(|x| x.0) == Some(a)).then_some(()));
    // Back from A: Finder's other windows (the user's own, if any) come first, then ZTab's
    // Settings window, the next app's most recent window. Step to it.
    let (c2, order, idx) = step_to(OPTION, OPT, TAB, sw);
    let f2 = wait_for(2.0, || (top_any().map(|x| x.0) == Some(sw) && frontmost_pid() == Some(pid)).then_some(()));
    let pids = owner_pids(&order);
    let finder_first = pids.contains_key(&a) && idx.is_some_and(|i| i >= 1 && order[..i].iter().all(|w| pids.get(w) == pids.get(&a)));
    out.check(
        "B/quick ⌥Tab goes to A; the Settings window comes right after all of Finder's windows",
        c1 == vec![a] && f1.is_some() && order.first() == Some(&a) && finder_first && c2 == vec![sw] && f2.is_some(),
        format!(
            "commits {c1:?} then {c2:?}; Settings window at index {idx:?} of {order:?} (all before it Finder's: {finder_first}); front after: {:?}, frontmost pid {:?}",
            top_any(),
            frontmost_pid()
        ),
    );
    if f2.is_none() {
        return;
    }

    // C) Show delay: 0 ms, then 300 ms, with mouse clicks at the slider's ends.
    let Some((sx, sy, sw_, sh)) = ui.el("ztab.delay").and_then(|e| frame(&e)) else {
        out.check("C/slider found", false, "no ztab.delay element".into());
        return;
    };
    click(CGPoint { x: sx + 2.0, y: sy + sh / 2.0 });
    let label0 = wait_for(2.0, || ui.value_of("ztab.delay.value").filter(|v| v == "0 ms"));
    let saved0 = wait_settings(sdir, &["show_delay_ms = 0"]);
    let (open, shown, _) = hold_and_cancel(OPTION, OPT, TAB, 300);
    let o2s0 = shown.as_deref().and_then(|l| num_after(l, "open_to_show_ms="));
    out.check(
        "C/0 ms: label, file, panel at once",
        label0.is_some() && saved0 && open.as_deref().is_some_and(|l| l.contains("delay_ms=0")) && o2s0.is_some_and(|v| v < 30.0),
        format!("label={label0:?} saved={saved0} open_to_show_ms={o2s0:?}"),
    );
    click(CGPoint { x: sx + sw_ - 2.0, y: sy + sh / 2.0 });
    let label300 = wait_for(2.0, || ui.value_of("ztab.delay.value").filter(|v| v == "300 ms"));
    let saved300 = wait_settings(sdir, &["show_delay_ms = 300"]);
    let (open_short, shown_short, _) = hold_and_cancel(OPTION, OPT, TAB, 180);
    let (open, shown, _) = hold_and_cancel(OPTION, OPT, TAB, 600);
    let o2s300 = shown.as_deref().and_then(|l| num_after(l, "open_to_show_ms="));
    out.check(
        "C/300 ms: label, file, no panel for a 200 ms hold, panel after 300 ms",
        label300.is_some()
            && saved300
            && open_short.is_some()
            && shown_short.is_none()
            && open.as_deref().is_some_and(|l| l.contains("delay_ms=300"))
            && o2s300.is_some_and(|v| (299.0..360.0).contains(&v)),
        format!("label={label300:?} saved={saved300} short: open={} shown={}; open_to_show_ms={o2s300:?}", open_short.is_some(), shown_short.is_some()),
    );

    // D) Recorder.
    let t = now();
    let pressed = ui.press("ztab.hotkey.all");
    let rec = wait_for(2.0, || ui.title_of("ztab.hotkey.all").filter(|x| x == RECORDING));
    let capturing = wait_for(2.0, || (!lines_with(t, "settings: shortcut recorder capturing").is_empty()).then_some(()));
    let in_front = frontmost_pid() == Some(pid);
    out.check(
        "D/recorder captures (tap passes keys)",
        pressed == Some(AXError::Success) && rec.is_some() && capturing.is_some() && in_front,
        format!("press={pressed:?} title={rec:?} tap_log={} ztab_front={in_front}", capturing.is_some()),
    );
    if rec.is_none() || !in_front {
        return;
    }
    // ⌥Tab while capturing: recorded, never opens the switcher.
    let t = now();
    opt_down();
    press_key(TAB, OPT);
    opt_up();
    sleep(Duration::from_millis(500));
    let opened = !lines_with(t, "open: ").is_empty();
    let captured = lines_with(t, "settings: recorder captured ⌥Tab");
    let title = ui.title_of("ztab.hotkey.all");
    out.check(
        "D/⌥Tab while capturing is recorded and opens nothing",
        !opened && !captured.is_empty() && title.as_deref() == Some("⌥Tab") && seen_keys_since(t, TAB) >= 1,
        format!("opened={opened} captured={captured:?} title={title:?} tab_reached_session={}", seen_keys_since(t, TAB)),
    );
    // A key without a hold modifier is refused (and capturing goes on), then ⌃F18 is accepted.
    let _ = ui.press("ztab.hotkey.all");
    let rec = wait_for(2.0, || ui.title_of("ztab.hotkey.all").filter(|x| x == RECORDING));
    if rec.is_none() || frontmost_pid() != Some(pid) {
        out.check("D/recorder captures again", false, format!("title={:?}", ui.title_of("ztab.hotkey.all")));
        return;
    }
    press_key(F18, CGEventFlags::empty());
    let refused = wait_for(2.0, || ui.value_of("ztab.message").filter(|m| m.contains("alone")));
    let still = ui.title_of("ztab.hotkey.all");
    ctrl_down();
    press_key(F18, CTRL);
    ctrl_up();
    let accepted = wait_for(2.0, || ui.title_of("ztab.hotkey.all").filter(|x| x == "⌃F18"));
    let saved = wait_settings(sdir, &["hotkey_all = control+79"]);
    out.check(
        "D/F18 alone refused; ⌃F18 recorded and saved",
        refused.is_some() && still.as_deref() == Some(RECORDING) && accepted.is_some() && saved,
        format!("message={refused:?} title_after_refusal={still:?} title={accepted:?} saved={saved}"),
    );
    if accepted.is_none() {
        return;
    }

    // E) The new shortcut, with A in front.
    if front_finder(dir_a) != Some(a) {
        out.check("E/A in front", false, format!("front={:?}", top_any()));
        return;
    }
    let (open, shown, t) = hold_and_cancel(CONTROL, CTRL, F18, 600);
    let leaked = seen_keys_since(t, F18) + seen_keys_since(t, ESC);
    out.check(
        "E/⌃F18 opens the switcher; F18 and Esc never reach A",
        open.is_some() && shown.is_some() && leaked == 0,
        format!("open={} shown={} leaked_key_events={leaked}", open.is_some(), shown.is_some()),
    );
    let t = now();
    opt_down();
    press_key(TAB, OPT);
    opt_up();
    sleep(Duration::from_millis(600));
    let opened = !lines_with(t, "open: ").is_empty();
    let passed = seen_keys_since(t, TAB);
    out.check(
        "E/the old ⌥Tab no longer opens it (passes through to A)",
        !opened && passed >= 2 && top_any().map(|x| x.0) == Some(a),
        format!("opened={opened} tab_events_at_session={passed} front={:?}", top_any()),
    );
    // To the Settings window by stepping with F18 (Finder's other windows come first), then a
    // quick ⌃F18 from it (ZTab's only window) goes to the next app's most recent window, A.
    let (c1, _, idx) = step_to(CONTROL, CTRL, F18, sw);
    let f1 = wait_for(2.0, || (top_any().map(|x| x.0) == Some(sw) && frontmost_pid() == Some(pid)).then_some(()));
    let c2 = quick_tap(CONTROL, CTRL, F18);
    let f2 = wait_for(2.0, || (top_any().map(|x| x.0) == Some(a)).then_some(()));
    out.check(
        "E/⌃F18 steps to the Settings window; a quick ⌃F18 (⌃ released) goes back to A",
        c1 == vec![sw] && f1.is_some() && c2 == vec![a] && f2.is_some(),
        format!("commits {c1:?} (Settings window at index {idx:?}) then {c2:?}; front {:?}", top_any()),
    );

    // F) Restore defaults (AXPress works with the window in the background).
    let pressed = ui.press("ztab.restore");
    let titles = wait_for(2.0, || {
        let (x, y) = (ui.title_of("ztab.hotkey.all"), ui.title_of("ztab.hotkey.same"));
        (x.as_deref() == Some("⌥Tab") && y.as_deref() == Some("⌥`")).then_some(())
    });
    let label = ui.value_of("ztab.delay.value");
    let saved = wait_settings(sdir, &["show_delay_ms = 120", "hotkey_all = option+48", "hotkey_same_app = option+50"]);
    out.check(
        "F/restore defaults: titles, label, file",
        pressed == Some(AXError::Success) && titles.is_some() && label.as_deref() == Some("120 ms") && saved,
        format!("press={pressed:?} titles_ok={} label={label:?} saved={saved}", titles.is_some()),
    );
    if top_any().map(|x| x.0) != Some(a) && front_finder(dir_a) != Some(a) {
        out.check("F/A in front", false, format!("front={:?}", top_any()));
        return;
    }
    let (open, shown, _) = hold_and_cancel(OPTION, OPT, TAB, 400);
    let t = now();
    ctrl_down();
    press_key(F18, CTRL);
    ctrl_up();
    sleep(Duration::from_millis(500));
    let opened_f18 = !lines_with(t, "open: ").is_empty();
    out.check(
        "F/⌥Tab opens the switcher again, ⌃F18 does not",
        open.is_some() && shown.is_some() && !opened_f18 && seen_keys_since(t, F18) >= 2,
        format!("⌥Tab open={} shown={}; ⌃F18 opened={opened_f18} f18_at_session={}", open.is_some(), shown.is_some(), seen_keys_since(t, F18)),
    );

    // G) Start at login, isolated: first what ZTab's first launch did with the directory (see
    // `first_launch_expect`), then the checkbox.
    let agent = format!("{sdir}/LaunchAgents/dev.ztab.app.plist");
    let agent_path = std::path::Path::new(&agent);
    let bin = binary_path(pid);
    let execs_bin = |text: &str| !bin.is_empty() && text.contains(&format!("<string>{bin}</string>")) && !text.contains("/usr/bin/open");
    let agent_text = std::fs::read_to_string(&agent).unwrap_or_default();
    let agent_mtime = mtime_secs(agent_path);
    // The log stamps are rounded to the millisecond: 2 ms of margin (a pre-seeded agent is written
    // well before ZTab even starts; ZTab's own write comes after its `starting` line).
    let pre_seeded = agent_mtime.is_some_and(|t| t < fl.started_at - 0.002);
    let expect = first_launch_expect(fl.flag.as_deref(), &bin, &fl.home, pre_seeded, fl.settings_existed);
    let first = wait_for(3.0, || ui.el("ztab.login").and_then(|e| num_attr(&e, "AXValue")).filter(|v| *v == 0.0 || *v == 1.0));
    let recorded = settings_text(sdir).contains("\nlogin_item_offered = true\n");
    let runs_bin = execs_bin(&agent_text);
    let first_launch_ok = match &expect {
        FirstLaunchExpect::On => first == Some(1.0) && runs_bin && agent_mtime.is_some() && !pre_seeded && recorded,
        FirstLaunchExpect::Kept => first == Some(if runs_bin { 1.0 } else { 0.0 }) && (agent_mtime.is_none() || pre_seeded) && recorded,
        FirstLaunchExpect::Off => first == Some(0.0) && agent_mtime.is_none() && !recorded,
        FirstLaunchExpect::Unknown(_) => false,
    };
    out.note(format!(
        "G: first launch expected {expect:?} (flag {:?}, ZTab started at {:.3}, settings file then: {}): box={first:?} agent mtime={agent_mtime:?} \
         pre_seeded={pre_seeded} runs the binary={runs_bin} recorded={recorded} -> {first_launch_ok}",
        fl.flag, fl.started_at, fl.settings_existed
    ));
    // A ticked box is unticked first, so the checkbox sequence below starts from off.
    let untick_ok = if first == Some(1.0) {
        let p0 = ui.press("ztab.login");
        let removed0 = wait_for(3.0, || (!agent_path.exists()).then_some(()));
        let off0 = wait_for(2.0, || ui.el("ztab.login").and_then(|e| num_attr(&e, "AXValue")).filter(|v| *v == 0.0));
        out.note(format!("G: unticked first: removed={} unchecked={}", removed0.is_some(), off0.is_some()));
        p0 == Some(AXError::Success) && removed0.is_some() && off0.is_some()
    } else {
        true
    };
    let before = num_attr(&ui.el("ztab.login").unwrap_or_else(|| ui.app.clone()), "AXValue");
    let enabled = ui.el("ztab.login").and_then(|e| num_attr(&e, "AXEnabled"));
    let pressed = ui.press("ztab.login");
    // Written for this binary (a stale agent from before is already there: wait for the content).
    let written = wait_for(3.0, || execs_bin(&std::fs::read_to_string(&agent).unwrap_or_default()).then_some(()));
    let lint = Command::new("/usr/bin/plutil").args(["-lint", &agent]).output().map(|o| o.status.success()).unwrap_or(false);
    let text = std::fs::read_to_string(&agent).unwrap_or_default();
    let execs = execs_bin(&text);
    let on = wait_for(2.0, || ui.el("ztab.login").and_then(|e| num_attr(&e, "AXValue")).filter(|v| *v == 1.0));
    let pressed2 = ui.press("ztab.login");
    let removed = wait_for(3.0, || (!std::path::Path::new(&agent).exists()).then_some(()));
    let off = wait_for(2.0, || ui.el("ztab.login").and_then(|e| num_attr(&e, "AXValue")).filter(|v| *v == 0.0));
    out.check(
        "G/start at login writes and removes the isolated agent (valid plist, runs the binary)",
        first_launch_ok
            && untick_ok
            && settings_text(sdir).contains("\nlogin_item_offered = true\n")
            && before == Some(0.0)
            && enabled == Some(1.0)
            && pressed == Some(AXError::Success)
            && written.is_some()
            && lint
            && execs
            && on.is_some()
            && pressed2 == Some(AXError::Success)
            && removed.is_some()
            && off.is_some(),
        format!(
            "first launch {expect:?}: box={first:?} ok={first_launch_ok} untick_ok={untick_ok}; before={before:?} enabled={enabled:?} written={} lint={lint} execs_binary={execs} ({bin}) checked={} removed={} unchecked={}",
            written.is_some(),
            on.is_some(),
            removed.is_some(),
            off.is_some()
        ),
    );

    // H) Close the window while it is key: the focus goes back to A.
    let (c, _, _) = step_to(OPTION, OPT, TAB, sw);
    let key = wait_for(2.0, || (top_any().map(|x| x.0) == Some(sw) && frontmost_pid() == Some(pid)).then_some(()));
    if key.is_none() {
        out.check("H/Settings window in front before closing", false, format!("commits {c:?} front {:?}", top_any()));
        return;
    }
    let t = now();
    let close = ui.window().and_then(|w| attr(&w, "AXCloseButton")).and_then(|v| v.downcast::<AXUIElement>().ok());
    let pressed = close.as_deref().map(press);
    let gone = wait_for(2.0, || settings_wid(pid).is_none().then_some(()));
    let back = wait_for(2.0, || (top_any().map(|x| x.0) == Some(a) && frontmost_pid() != Some(pid)).then_some(()));
    let logged = lines_with(t, "settings window closed: focusing");
    out.check(
        "H/closing it gives the focus back to the most recent window (A)",
        pressed == Some(AXError::Success) && gone.is_some() && back.is_some() && logged.iter().any(|l| l.contains(&format!("wid={a} "))),
        format!("press={pressed:?} gone={} front={:?} frontmost_pid={:?} log={logged:?}", gone.is_some(), top_any(), frontmost_pid()),
    );
    if back.is_none() {
        return;
    }

    // I) It cannot be minimized (last: nothing depends on it).
    let (menu_open, clicked) = ui.open_from_menu(pid, out);
    let sw2 = wait_for(3.0, || settings_wid(pid));
    let front = wait_for(3.0, || (frontmost_pid() == Some(pid) && top_any().map(|x| x.0) == sw2 && sw2.is_some()).then_some(()));
    if front.is_none() {
        out.check("I/Settings window reopened in front", false, format!("menu_open={menu_open} clicked={clicked} wid={sw2:?} front={:?}", top_any()));
        return;
    }
    let t = now();
    let min_button = ui.window().and_then(|w| attr(&w, "AXMinimizeButton")).and_then(|v| v.downcast::<AXUIElement>().ok());
    let enabled = min_button.as_deref().and_then(|b| num_attr(b, "AXEnabled"));
    let pressed = min_button.as_deref().map(press);
    pump(1.0);
    let minimized = ui.window().and_then(|w| num_attr(&w, "AXMinimized"));
    let still_front = frontmost_pid() == Some(pid) && top_any().map(|x| x.0) == sw2;
    let logged = lines_with(t, "settings window minimized");
    out.check(
        "I/it cannot be minimized (minimize button disabled, a press leaves it in front)",
        min_button.as_ref().is_none_or(|_| enabled == Some(0.0)) && minimized == Some(0.0) && still_front && logged.is_empty(),
        format!(
            "button={} enabled={enabled:?} press={pressed:?} minimized={minimized:?} front={:?} frontmost_pid={:?} log={logged:?}",
            min_button.is_some(),
            top_any(),
            frontmost_pid()
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::{FirstLaunchExpect as E, first_launch_expect as expect};

    #[test]
    fn first_launch_expectation() {
        let app = "/Applications/ZTab.app/Contents/MacOS/ztab";
        let h = "/Users/a";
        // run_round.sh: a fresh, empty directory and the installed ZTab.
        assert_eq!(expect(None, app, h, false, false), E::On);
        assert_eq!(expect(None, "/Users/a/Applications/ZTab.app/Contents/MacOS/ztab", h, false, false), E::On);
        // Its STALE_AGENT mode (an agent from before ZTab started), or a settings file then.
        assert_eq!(expect(None, app, h, true, false), E::Kept);
        assert_eq!(expect(None, app, h, true, true), E::Kept);
        assert_eq!(expect(None, app, h, false, true), E::Kept);
        // Not an installed copy, whatever was there.
        assert_eq!(expect(None, "/Users/a/src/ztab/target/release/ztab", h, false, false), E::Off);
        assert_eq!(expect(None, "/Users/a/src/ztab/dist/ZTab.app/Contents/MacOS/ztab", h, true, true), E::Off);
        // Anything else needs the flag, which always wins.
        assert!(matches!(expect(None, "/Users/a/x/ZTab.app/Contents/MacOS/ztab", h, false, false), E::Unknown(_)));
        assert_eq!(expect(Some("on"), "/Users/a/x/ZTab.app/Contents/MacOS/ztab", h, false, false), E::On);
        assert_eq!(expect(Some("kept"), app, h, false, false), E::Kept);
        assert_eq!(expect(Some("off"), app, h, false, false), E::Off);
        assert!(matches!(expect(Some("yes"), app, h, false, false), E::Unknown(_)));
    }
}
