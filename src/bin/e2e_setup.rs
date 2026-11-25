//! End-to-end check of ZTab's "Set up ZTab" window against a running ZTab. It never grants or
//! revokes anything: it only reads the window through Accessibility and presses ZTab's own buttons
//! ("Restart ZTab", then the main button), and clicks ZTab's status menu.
//!
//!  A) the window is open (started with `--setup-window`, or reopened after a setup restart), or
//!     it opens from the status menu ("Set Up ZTab…") with a scratch Finder window in front; ZTab is
//!     active and the window is in front;
//!  B) the rows show the expected state (`--expect <ax><screen>`, 1 = granted; default 11): "✓
//!     Granted" without a "Grant…" button, or "✗ Not granted" with one; the main button reads Done /
//!     Skip Thumbnails / Later accordingly; the status checks ran off the main thread (their log
//!     line, with timings) and agree; showing the window took under 100 ms of main-thread time;
//!     with Accessibility (`--expect 1x`), ⌥Tab held with the window in front lists it (its id in
//!     the session's `order=`) as the current window, then Esc cancels (after C's restart: listed;
//!     whether it is current there is only noted);
//!  C) with `--restart`: "Restart ZTab" restarts ZTab: a new process with the same arguments (and,
//!     with `--expect-launchd`, it is the login agent's launchd job), which opens the window again
//!     on its own (B is checked again there);
//!  D) the main button closes the window; with the scratch window opened in A it is in front
//!     again, and (with Accessibility) ⌥Tab held there no longer lists the closed window; the
//!     settings file is unchanged when everything was granted (Done remembers nothing);
//!  E) `--dialogs <before-file>`: no window of another process at a raised level (a system
//!     dialog) appeared since `e2e_setup --snapshot <before-file>` was written (the first-launch
//!     check of a build without the permissions: ZTab must not prompt by itself).
//!
//! Usage: `open -W -n -g <bundle> --args <out.txt> <scratch-dir> [--bundle-id <id>] [--expect 11|10|00|01]
//!        [--restart] [--expect-launchd] [--settings-dir <dir>] [--dialogs <file>]`, or
//!        `... --args --snapshot <file>` (writes the on-screen windows and exits).

#[path = "../windows.rs"]
#[allow(dead_code)]
mod windows;

use std::ffi::c_void;
use std::io::Write;
use std::process::Command;
use std::ptr::NonNull;
use std::thread::sleep;
use std::time::{Duration, Instant};

use objc2_app_kit::{NSRunningApplication, NSWorkspace};
use objc2_application_services::{AXError, AXUIElement, AXValue, AXValueType};
use objc2_core_foundation::{CFArray, CFBoolean, CFRetained, CFRunLoop, CFString, CFType, CGPoint, CGSize, kCFRunLoopDefaultMode};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGEventType, CGMouseButton, CGWindowListOption,
};
use objc2_foundation::NSString;

const TITLE: &str = "Set up ZTab";
const MENU_ITEM: &str = "Set Up ZTab…";
const MAGIC: i64 = 0x5E772;
const OPTION: u16 = 0x3A;
const TAB: u16 = 0x30;
const ESC: u16 = 0x35;

// ---------------------------------------------------------------------------------------------
// Log, waiting, windows
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

fn pump(secs: f64) {
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

/// The Set up window's id while it is on screen (CG, by owner and title).
fn setup_wid(pid: i32) -> Option<u32> {
    windows::cg_window_list(CGWindowListOption::OptionOnScreenOnly)
        .iter()
        .filter_map(|d| windows::parse_cg(d))
        .find(|(_, p, layer, _, _, name)| *p == pid && *layer == 0 && name.as_deref() == Some(TITLE))
        .map(|(wid, ..)| wid)
}

fn menu_window_open(pid: i32) -> bool {
    windows::cg_window_list(CGWindowListOption::OptionOnScreenOnly)
        .iter()
        .filter_map(|d| windows::parse_cg(d))
        .any(|(_, p, layer, _, alpha, _)| p == pid && layer == 101 && alpha > 0.0)
}

/// The running app with this bundle id (the oldest live one other than this process).
fn app_pid(bundle_id: &str) -> Option<i32> {
    let me = std::process::id() as i32;
    let apps = NSRunningApplication::runningApplicationsWithBundleIdentifier(&NSString::from_str(bundle_id));
    apps.iter()
        .filter(|a| !a.isTerminated())
        .map(|a| (a.processIdentifier(), a.launchDate().map(|d| d.timeIntervalSince1970()).unwrap_or(f64::MAX)))
        .filter(|(p, _)| *p != me && unsafe { libc::kill(*p, 0) } == 0)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(p, _)| p)
}

fn command_of(pid: i32) -> String {
    let o = Command::new("ps").args(["-o", "command=", "-p", &pid.to_string()]).output().unwrap();
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

fn proc_name(pid: i32) -> String {
    let mut buf = [0u8; 256];
    let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast::<c_void>(), buf.len() as u32) };
    String::from_utf8_lossy(&buf[..n.max(0) as usize]).into_owned()
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

/// On-screen windows of every process: (wid, pid, layer, owner).
fn screen_windows() -> Vec<(u32, i32, i64, String)> {
    windows::cg_window_list(CGWindowListOption::OptionOnScreenOnly)
        .iter()
        .filter_map(|d| windows::parse_cg(d))
        .filter(|(_, _, _, _, alpha, _)| *alpha > 0.0)
        .map(|(wid, pid, layer, ..)| (wid, pid, layer, proc_name(pid)))
        .collect()
}

thread_local! {
    static SRC: CFRetained<CGEventSource> = CGEventSource::new(CGEventSourceStateID::HIDSystemState).unwrap();
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

fn key(code: u16, down: bool, flags: CGEventFlags) {
    let ev = SRC.with(|s| CGEvent::new_keyboard_event(Some(s), code, down)).unwrap();
    CGEvent::set_flags(Some(&ev), flags);
    CGEvent::set_integer_value_field(Some(&ev), CGEventField::EventSourceUserData, MAGIC);
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
    sleep(Duration::from_millis(25));
}
fn press_key(code: u16, flags: CGEventFlags) {
    key(code, true, flags);
    key(code, false, flags);
}

/// Hold ⌥, press Tab, wait for the session's `open:` line, then Esc and release ⌥ (cancelled:
/// nothing is switched to). Returns the `open:` line and any commit since.
fn hold_and_cancel() -> (Option<String>, Vec<String>) {
    const OPT: CGEventFlags = CGEventFlags::MaskAlternate;
    let t = now();
    key(OPTION, true, OPT);
    press_key(TAB, OPT);
    let open = wait_for(1.5, || lines_with(t, "open: ").into_iter().next());
    sleep(Duration::from_millis(300));
    press_key(ESC, OPT);
    key(OPTION, false, CGEventFlags::empty());
    sleep(Duration::from_millis(400));
    (open, lines_with(t, "commit: wid="))
}

/// An `open:` line's `order=` (window ids).
fn open_order(line: &str) -> Vec<u32> {
    line.split(" order=")
        .nth(1)
        .and_then(|r| r.split_whitespace().next())
        .map(|r| r.split(',').filter_map(|w| w.parse().ok()).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// Accessibility of ZTab's window
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
fn bool_attr(el: &AXUIElement, name: &str) -> Option<bool> {
    attr(el, name).and_then(|v| v.downcast_ref::<CFBoolean>().map(|b| b.as_bool()))
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
fn press(el: &AXUIElement) -> AXError {
    unsafe { el.perform_action(&CFString::from_str("AXPress")) }
}
fn find_by(el: &AXUIElement, key: &str, wanted: &str, depth: usize) -> Option<CFRetained<AXUIElement>> {
    for c in children(el) {
        if str_attr(&c, key).as_deref() == Some(wanted) {
            return Some(c);
        }
        if depth > 0
            && let Some(f) = find_by(&c, key, wanted, depth - 1)
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
        find_by(&w, "AXIdentifier", id, 4)
    }
    fn value_of(&self, id: &str) -> Option<String> {
        str_attr(&*self.el(id)?, "AXValue")
    }
    fn title_of(&self, id: &str) -> Option<String> {
        str_attr(&*self.el(id)?, "AXTitle")
    }
    fn press(&self, id: &str) -> Option<AXError> {
        Some(press(&*self.el(id)?))
    }
    /// Open the status menu and click "Set Up ZTab…". Returns (menu opened, item clicked).
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
        let item = if open { find_by(&bar, "AXTitle", MENU_ITEM, 4) } else { None };
        // The permission status lines above it.
        if let Some(menu) = item.as_ref().and_then(|i| attr(i, "AXParent")).and_then(|v| v.downcast::<AXUIElement>().ok()) {
            let titles: Vec<String> = children(&menu).iter().filter_map(|c| str_attr(c, "AXTitle")).filter(|t| !t.is_empty()).collect();
            out.note(format!("status menu items: {titles:?}"));
        }
        let at = item.as_deref().and_then(center);
        out.note(format!("status icon at ({:.0}, {:.0}); menu open={open}; {MENU_ITEM} item at {at:?}", icon.x, icon.y));
        match at {
            Some(p) => {
                click(p);
                (true, true)
            }
            None => {
                if open {
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

struct Opts {
    scratch: String,
    bundle_id: String,
    expect_ax: bool,
    expect_screen: bool,
    restart: bool,
    expect_launchd: bool,
    settings_dir: Option<String>,
    dialogs: Option<String>,
}

fn opt(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

// ---------------------------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(path) = opt(&args, "--snapshot") {
        let lines: Vec<String> = screen_windows().iter().map(|(w, p, l, o)| format!("{w} {p} {l} {o}")).collect();
        let _ = std::fs::write(path, lines.join("\n") + "\n");
        return;
    }
    let f = std::fs::File::create(&args[1]).unwrap();
    let mut out = Out { f, pass: true };
    let expect = opt(&args, "--expect").unwrap_or_else(|| "11".into());
    let o = Opts {
        scratch: args.get(2).filter(|a| !a.starts_with("--")).cloned().unwrap_or_else(|| "/tmp".into()),
        bundle_id: opt(&args, "--bundle-id").unwrap_or_else(|| "dev.ztab.app".into()),
        expect_ax: expect.starts_with('1'),
        expect_screen: expect.ends_with('1'),
        restart: args.iter().any(|a| a == "--restart"),
        expect_launchd: args.iter().any(|a| a == "--expect-launchd"),
        settings_dir: opt(&args, "--settings-dir"),
        dialogs: opt(&args, "--dialogs"),
    };
    let start = now();
    let Some(pid) = app_pid(&o.bundle_id) else {
        let _ = writeln!(out.f, "{} is not running\nRESULT FAIL", o.bundle_id);
        return;
    };
    out.note(format!("ZTab ({}) pid {pid}: {}", o.bundle_id, command_of(pid)));
    let settings_file = o.settings_dir.as_ref().map(|d| format!("{d}/settings.conf"));
    let settings_before = settings_file.as_ref().map(|f| std::fs::read_to_string(f).unwrap_or_else(|_| "<missing>".into()));
    let original_mouse = mouse_location();
    let dir_a = format!("{}/setup-A", o.scratch);
    let mut used_finder = false;
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&mut out, pid, &o, &dir_a, &mut used_finder)));
    if r.is_err() {
        out.check("panic", false, "suite panicked".into());
    }
    // Cleanup, whatever happened: the window closed (main button), the scratch window closed.
    if let Some(p) = app_pid(&o.bundle_id) {
        let ui = Ui::new(p);
        if ui.window().is_some() {
            out.note("cleanup: closing the Set up window".into());
            if let Some(close) = ui.window().and_then(|w| attr(&w, "AXCloseButton")).and_then(|v| v.downcast::<AXUIElement>().ok()) {
                press(&close);
            }
        }
    }
    if used_finder {
        close_finder(&dir_a);
    }
    if let Some(p) = original_mouse {
        mouse(CGEventType::MouseMoved, p);
    }
    if let (Some(f), Some(before)) = (&settings_file, &settings_before) {
        let after = std::fs::read_to_string(f).unwrap_or_else(|_| "<missing>".into());
        if o.expect_screen {
            out.check("D/the settings file is unchanged (Done remembers nothing)", after == *before, format!("{after:?}"));
        }
    }
    let stalls: Vec<String> = log_since(start)
        .into_iter()
        .filter(|(_, m)| m.contains("main thread stall") || m.contains("still blocked"))
        .map(|(t, m)| format!("[{t:.3}] {m}"))
        .collect();
    out.note(format!("main-thread stalls during the suite (opening the status menu costs 50-80 ms in AppKit): {stalls:?}"));
    out.check("no main thread blocked for a second", !stalls.iter().any(|s| s.contains("still blocked")), format!("{} stall lines", stalls.len()));
    let _ = writeln!(out.f, "RESULT {}", if out.pass { "PASS" } else { "FAIL" });
}

fn run(out: &mut Out, pid: i32, o: &Opts, dir_a: &str, used_finder: &mut bool) {
    // E) No system dialog since the snapshot (taken before this ZTab started).
    if let Some(path) = &o.dialogs {
        let before: std::collections::HashSet<u32> = std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.split(' ').next()?.parse().ok())
            .collect();
        let me = std::process::id() as i32;
        let new: Vec<_> = screen_windows().into_iter().filter(|(w, p, _, _)| !before.contains(w) && *p != pid && *p != me).collect();
        let raised: Vec<_> = new.iter().filter(|(_, _, layer, _)| *layer != 0).collect();
        out.note(format!("windows of other processes that appeared since the snapshot: {new:?}"));
        out.check(
            "E/no dialog window of another process appeared since ZTab started",
            raised.is_empty() && !new.iter().any(|(_, _, _, n)| n.contains("UserNotificationCenter") || n.contains("universalAccess") || n == "tccd"),
            format!("raised-level windows {raised:?}"),
        );
    }

    // A) The window: open already, or from the status menu with a scratch window in front.
    let ui = Ui::new(pid);
    let t = now();
    let mut a: Option<u32> = None;
    let already = wait_for(1.0, || setup_wid(pid)).is_some();
    if !already {
        a = front_finder(dir_a);
        *used_finder = true;
        out.note(format!("A = scratch Finder window {a:?}"));
        let (open, clicked) = ui.open_from_menu(pid, out);
        out.check("A/opens from the status menu", open && clicked, format!("menu open={open} item clicked={clicked}"));
    }
    let wid = wait_for(3.0, || setup_wid(pid));
    let front = wait_for(3.0, || (frontmost_pid() == Some(pid) && top_any().map(|x| x.0) == wid && wid.is_some()).then_some(()));
    let shown = lines_with(if already { t - 600.0 } else { t }, "setup window shown");
    out.check(
        "A/the window is open, in front, ZTab active",
        wid.is_some() && front.is_some(),
        format!("already open={already} wid={wid:?} top={:?} frontmost={:?} log={:?}", top_any(), frontmost_pid(), shown.last()),
    );
    if wid.is_none() {
        return;
    }
    let show_ms = shown.last().and_then(|l| num_after(l, "shown in "));
    let build_ms = shown.last().and_then(|l| num_after(l, " in "));
    out.check(
        "A/showing it took under 100 ms of main-thread time",
        show_ms.is_some_and(|v| v < 100.0) && build_ms.is_some_and(|v| v < 100.0),
        format!("build {build_ms:?} ms, show {show_ms:?} ms"),
    );

    // B) The rows; the window in the switcher.
    check_rows(out, &ui, o, "B");
    check_switcher(out, pid, o, "B", true);

    // C) Restart.
    let mut pid = pid;
    if o.restart {
        let old_cmd = command_of(pid);
        let t = now();
        let pressed = ui.press("ztab.setup.restart");
        let new_pid = wait_for(20.0, || app_pid(&o.bundle_id).filter(|p| *p != pid && unsafe { libc::kill(pid, 0) } != 0));
        let plan = lines_with(t, "setup: restart plan").into_iter().next();
        out.note(format!("pressed={pressed:?} new pid {new_pid:?}; {plan:?}"));
        let Some(np) = new_pid else {
            out.check("C/Restart ZTab starts a new ZTab", false, format!("old pid {pid} alive={}", unsafe { libc::kill(pid, 0) } == 0));
            return;
        };
        let new_cmd = command_of(np);
        let reopened = wait_for(10.0, || setup_wid(np));
        let reopen_line = wait_for(3.0, || lines_with(t, "setup window shown (reopened after a setup restart)").into_iter().next());
        out.check(
            "C/Restart ZTab: a new ZTab with the same arguments, which opens the window again",
            new_cmd == old_cmd && reopened.is_some() && reopen_line.is_some(),
            format!("old {pid} {old_cmd:?} -> new {np} {new_cmd:?}; window {reopened:?}; {reopen_line:?}"),
        );
        if o.expect_launchd {
            let uid = unsafe { libc::getuid() };
            let p = Command::new("launchctl").args(["print", &format!("gui/{uid}/dev.ztab.app")]).output().unwrap();
            let text = String::from_utf8_lossy(&p.stdout).to_string();
            let job_pid: Option<i32> = text.lines().find_map(|l| l.trim().strip_prefix("pid = ")?.parse().ok());
            out.check(
                "C/the new ZTab is the login agent's launchd job",
                job_pid == Some(np) && plan.as_deref().is_some_and(|l| l.contains("Kickstart")),
                format!("launchctl job pid {job_pid:?}, new pid {np}"),
            );
        }
        pid = np;
        let ui2 = Ui::new(pid);
        wait_for(3.0, || ui2.window());
        check_rows(out, &ui2, o, "C");
        check_switcher(out, pid, o, "C", false);
    }

    // D) The main button closes it; the focus goes back.
    let ui = Ui::new(pid);
    let closed_wid = setup_wid(pid);
    let title = ui.title_of("ztab.setup.primary");
    let t = now();
    let pressed = ui.press("ztab.setup.primary");
    let gone = wait_for(3.0, || setup_wid(pid).is_none().then_some(()));
    let closed = wait_for(2.0, || lines_with(t, "setup window closed").into_iter().next());
    let focus_line = lines_with(t, "setup window closed:").into_iter().next();
    out.check(
        &format!("D/{} closes the window", title.as_deref().unwrap_or("?")),
        pressed == Some(AXError::Success) && gone.is_some() && closed.is_some(),
        format!("pressed={pressed:?} gone={} {focus_line:?}", gone.is_some()),
    );
    if let Some(a) = a {
        let back = wait_for(3.0, || (top_any().map(|x| x.0) == Some(a)).then_some(()));
        out.check("D/the scratch window is in front again", back.is_some(), format!("top={:?} frontmost={:?}", top_any(), frontmost_pid()));
        // The closed window left the switcher (keys only with the scratch window in front).
        if o.expect_ax && back.is_some() && top_any().map(|x| x.0) == Some(a) {
            let (open, commits) = hold_and_cancel();
            let order = open.as_deref().map(open_order).unwrap_or_default();
            out.check(
                "D/⌥Tab from the scratch window no longer lists the closed window (and cancels)",
                open.is_some() && closed_wid.is_some_and(|w| !order.contains(&w)) && commits.is_empty(),
                format!("closed wid {closed_wid:?}; order {order:?}; commits {commits:?}"),
            );
        }
    }
}

/// The Set up window is listed by the switcher (with Accessibility, so ⌥Tab works): ⌥Tab held with
/// it in front opens a session whose order holds its window id; Esc cancels. Keys are only posted
/// while ZTab is the active app with this window in front.
fn check_switcher(out: &mut Out, pid: i32, o: &Opts, part: &str, strict_current: bool) {
    if !o.expect_ax {
        return;
    }
    let Some(wid) = wait_for(3.0, || setup_wid(pid)) else {
        out.check(&format!("{part}/the switcher lists the window"), false, "window not on screen".into());
        return;
    };
    let front = wait_for(3.0, || (frontmost_pid() == Some(pid) && top_any().map(|x| x.0) == Some(wid)).then_some(()));
    if front.is_none() {
        out.check(
            &format!("{part}/the switcher lists the window"),
            false,
            format!("not in front before ⌥Tab (top {:?}, frontmost {:?}): no keys posted", top_any(), frontmost_pid()),
        );
        return;
    }
    // After the restart (C) the tap and the window model may still be starting.
    if part == "C" {
        wait_for(5.0, || lines_with(now() - 30.0, "first ⌥Tab usable").into_iter().next());
    }
    let (open, commits) = hold_and_cancel();
    let order = open.as_deref().map(open_order).unwrap_or_default();
    let cur_idx: Option<usize> = open.as_deref().and_then(|l| l.split("current=Some(").nth(1)?.split(')').next()?.parse().ok());
    let current = cur_idx.and_then(|i| order.get(i)) == Some(&wid);
    let what = if strict_current { "lists it as the current window" } else { "lists it" };
    out.check(
        &format!("{part}/⌥Tab with the window in front {what} (and cancels)"),
        order.contains(&wid) && (current || !strict_current) && commits.is_empty(),
        format!(
            "wid {wid} in order: {}; current index {cur_idx:?} is it: {current}; commits {commits:?}; open {:?}",
            order.contains(&wid),
            open.as_deref().map(|l| l.chars().take(260).collect::<String>())
        ),
    );
}

/// B: the two rows, the main button and the status checks' log line.
fn check_rows(out: &mut Out, ui: &Ui, o: &Opts, part: &str) {
    // The first status check lands a few milliseconds after the window shows.
    let want = |g: bool| if g { "✓ Granted" } else { "✗ Not granted" };
    let read = || (ui.value_of("ztab.setup.ax.status"), ui.value_of("ztab.setup.screen.status"));
    let ok = wait_for(3.0, || {
        let (a, s) = read();
        (a.as_deref() == Some(want(o.expect_ax)) && s.as_deref() == Some(want(o.expect_screen))).then_some(())
    });
    let (ax_s, sc_s) = read();
    let grant_ax = ui.el("ztab.setup.ax.grant").map(|e| (str_attr(&e, "AXTitle"), bool_attr(&e, "AXEnabled")));
    let grant_sc = ui.el("ztab.setup.screen.grant").map(|e| (str_attr(&e, "AXTitle"), bool_attr(&e, "AXEnabled")));
    let grant_ok = |g: &Option<(Option<String>, Option<bool>)>, granted: bool| {
        if granted { g.is_none() } else { g.as_ref().is_some_and(|(t, e)| t.as_deref() == Some("Grant…") && *e == Some(true)) }
    };
    out.check(
        &format!("{part}/rows: Accessibility {:?}, Screen Recording {:?}; Grant… only where missing", want(o.expect_ax), want(o.expect_screen)),
        ok.is_some() && grant_ok(&grant_ax, o.expect_ax) && grant_ok(&grant_sc, o.expect_screen),
        format!("ax {ax_s:?} grant {grant_ax:?}; screen {sc_s:?} grant {grant_sc:?}"),
    );
    let primary = ui.title_of("ztab.setup.primary");
    let want_primary = match (o.expect_ax, o.expect_screen) {
        (true, true) => "Done",
        (true, false) => "Skip Thumbnails",
        _ => "Later",
    };
    let restart = ui.title_of("ztab.setup.restart");
    let message = ui.value_of("ztab.setup.message");
    out.check(
        &format!("{part}/main button {want_primary:?}, Restart ZTab present"),
        primary.as_deref() == Some(want_primary) && restart.as_deref() == Some("Restart ZTab"),
        format!("primary {primary:?} restart {restart:?} message {message:?}"),
    );
    let poll = wait_for(3.0, || {
        log_since(now() - 30.0).into_iter().rev().find(|(_, m)| m.starts_with("setup: accessibility=")).map(|(_, m)| m)
    });
    // The first check after opening also asks the helper process (the same binary, as ZTab), so
    // its answer is seen whether or not Screen Recording is granted.
    let agrees = poll.as_deref().is_some_and(|l| {
        l.contains(&format!("accessibility={} screen_recording={}", o.expect_ax, o.expect_screen))
            && l.contains(&format!("helper process {}", o.expect_screen))
            && l.contains("off the main thread")
    });
    out.check(&format!("{part}/the status checks (this process and the helper process) ran off the main thread and agree"), agrees, format!("{poll:?}"));
    // Layout: each row's status text ends left of its button (the buttons show where not granted).
    let overlap = |status: &str, grant: &str| -> Option<String> {
        let s = frame(&*ui.el(status)?)?;
        let g = frame(&*ui.el(grant)?)?;
        let hit = s.0 < g.0 + g.2 && g.0 < s.0 + s.2 && s.1 < g.1 + g.3 && g.1 < s.1 + s.3;
        hit.then(|| format!("{status} {s:?} overlaps {grant} {g:?}"))
    };
    let overlaps: Vec<String> =
        [("ztab.setup.ax.status", "ztab.setup.ax.grant"), ("ztab.setup.screen.status", "ztab.setup.screen.grant")]
            .iter()
            .filter_map(|(a, b)| overlap(a, b))
            .collect();
    out.check(&format!("{part}/no status text under a Grant… button"), overlaps.is_empty(), format!("{overlaps:?}"));
}
