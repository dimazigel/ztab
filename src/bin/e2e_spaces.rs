//! End-to-end checks of the window model against a running ZTab (needs Accessibility, an unlocked
//! screen, and ZTab's log at ~/Library/Logs/ztab.log). Uses only scratch windows it creates (Finder
//! windows on folders under the scratch dir, TextEdit documents in it) and cleans them up.
//!
//!  a) full-screen Space + MRU: scratch doc W full screen; a quick ⌥Tab goes back to the previous
//!     window P on the desktop Space, a second one returns to W (MRU across Spaces), then clicking
//!     P's tile goes back again; W leaves full screen and is closed;
//!  b) hung app: TextEdit (only if this suite launched it) with one normal and one minimized
//!     document is SIGSTOPped; holding ⌥Tab must show the panel within 250 ms with `open:` under
//!     30 ms and both documents listed; Esc; SIGCONT (always); no main-thread stall logged; and a
//!     fresh thumbnail of the normal document (its cached one is older than ZTab's 3 s
//!     show-capture age by then) must land while TextEdit is stopped (`thumbs_landed` of the
//!     `panel hidden` line);
//!  c) external MRU: windows activated with `open -a` (Finder A, TextEdit T, Finder A again); a quick
//!     ⌥Tab goes to T, another one back to A;
//!  d) minimized thumbnail: a scratch Finder window that was focused, left (FocusLeft capture) and
//!     minimized has a thumbnail in the switcher (`t=1`);
//!  e) secure input: while this process holds secure input, ZTab logs it and its menu shows the
//!     warning; both go away when it is released;
//!  f) busy app raise: TextEdit (only if this suite launched it) is SIGSTOPped; holding ⌥, Tab +
//!     Return switches to its document A, and Tab + Return again, while A's raise is blocked in
//!     the stopped app, back to a scratch Finder window B. B must be fronted and raised within 200 ms (its raise
//!     runs on Finder's own raise thread), and after SIGCONT, when TextEdit applies A's queued
//!     raise, B must end up in front (ZTab re-fronts it if A came out on top).
//!
//! Usage: `open -W -n -g <bundle> --args <out.txt> <scratch-dir> [only=abcdef]`.

#[path = "../windows.rs"]
#[allow(dead_code)]
mod windows;

use std::ffi::c_void;
use std::io::Write;
use std::process::Command;
use std::ptr::NonNull;
use std::thread::sleep;
use std::time::{Duration, Instant};

use objc2_app_kit::NSRunningApplication;
use objc2_application_services::{AXError, AXUIElement, AXValue, AXValueType};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFRetained, CFRunLoop, CFString, CFType, CGPoint, CGSize, kCFRunLoopDefaultMode,
};
use objc2_core_graphics::{
    CGDisplayBounds, CGEvent, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGEventType,
    CGMainDisplayID, CGMouseButton, CGWindowListOption,
};
use objc2_foundation::NSString;

const OPTION: u16 = 0x3A;
const TAB: u16 = 0x30;
const ESC: u16 = 0x35;
const RETURN: u16 = 0x24;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn EnableSecureEventInput() -> i32;
    fn DisableSecureEventInput() -> i32;
    fn IsSecureEventInputEnabled() -> u8;
}

thread_local! {
    static SRC: CFRetained<CGEventSource> = CGEventSource::new(CGEventSourceStateID::HIDSystemState).unwrap();
}

// ---------------------------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------------------------

fn key(code: u16, down: bool, flags: CGEventFlags) {
    let ev = SRC.with(|s| CGEvent::new_keyboard_event(Some(s), code, down)).unwrap();
    CGEvent::set_flags(Some(&ev), flags);
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
    sleep(Duration::from_millis(25));
}
fn option_down() {
    key(OPTION, true, CGEventFlags::MaskAlternate);
}
fn option_up() {
    key(OPTION, false, CGEventFlags::empty());
}
fn tab() {
    key(TAB, true, CGEventFlags::MaskAlternate);
    key(TAB, false, CGEventFlags::MaskAlternate);
}
fn esc() {
    key(ESC, true, CGEventFlags::MaskAlternate);
    key(ESC, false, CGEventFlags::MaskAlternate);
}
fn ret() {
    key(RETURN, true, CGEventFlags::MaskAlternate);
    key(RETURN, false, CGEventFlags::MaskAlternate);
}
fn quick_tap() {
    option_down();
    tab();
    option_up();
}
fn mouse(kind: CGEventType, p: CGPoint, flags: CGEventFlags) {
    let ev = SRC.with(|s| CGEvent::new_mouse_event(Some(s), kind, p, CGMouseButton::Left)).unwrap();
    CGEvent::set_flags(Some(&ev), flags);
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
    sleep(Duration::from_millis(40));
}
fn click(p: CGPoint, flags: CGEventFlags) {
    mouse(CGEventType::MouseMoved, CGPoint { x: p.x + 2.0, y: p.y }, flags);
    mouse(CGEventType::MouseMoved, p, flags);
    mouse(CGEventType::LeftMouseDown, p, flags);
    mouse(CGEventType::LeftMouseUp, p, flags);
}
fn mouse_location() -> Option<CGPoint> {
    CGEvent::new(None).map(|e| CGEvent::location(Some(&e)))
}

// ---------------------------------------------------------------------------------------------
// ZTab's log (both files, so one rotation mid-suite loses nothing)
// ---------------------------------------------------------------------------------------------

fn now_secs() -> f64 {
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

fn commits_since(t: f64) -> Vec<u32> {
    log_since(t)
        .iter()
        .filter_map(|(_, m)| m.strip_prefix("commit: wid=")?.split(' ').next()?.parse().ok())
        .collect()
}

fn first_line_since(t: f64, prefix: &str) -> Option<(f64, String)> {
    log_since(t).into_iter().find(|(_, m)| m.starts_with(prefix))
}

struct Panel {
    at: f64,
    tiles: Vec<(u32, CGPoint, bool)>,
    line: String,
}

/// The first "panel shown" line since `t`, with its tiles (`wid:x,y:t`).
fn panel_since(t: f64) -> Option<Panel> {
    let (at, line) = first_line_since(t, "panel shown:")?;
    let tiles = line
        .split("tiles=")
        .nth(1)
        .unwrap_or("")
        .split(';')
        .filter_map(|t| {
            let mut parts = t.trim().split(':');
            let wid = parts.next()?.parse().ok()?;
            let (x, y) = parts.next()?.split_once(',')?;
            let thumb = parts.next() == Some("1");
            Some((wid, CGPoint { x: x.parse().ok()?, y: y.parse().ok()? }, thumb))
        })
        .collect();
    Some(Panel { at, tiles, line })
}

fn num_after(line: &str, key: &str) -> Option<f64> {
    line.split(key).nth(1)?.split(|c: char| c != '.' && !c.is_ascii_digit()).next()?.parse().ok()
}

// ---------------------------------------------------------------------------------------------
// Windows and apps
// ---------------------------------------------------------------------------------------------

fn pump(secs: f64) {
    // Pumping keeps NSWorkspace's running-app list (used by list_windows) current.
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
        pump(0.15);
    }
}

fn front_wid() -> Option<u32> {
    windows::list_windows().into_iter().min_by_key(|w| w.z_index).map(|w| w.wid)
}
fn onscreen(wid: u32) -> bool {
    windows::cg_window_ids(CGWindowListOption::OptionOnScreenOnly).contains(&wid)
}
fn osa(script: &str) -> String {
    let o = Command::new("osascript").args(["-e", script]).output().unwrap();
    String::from_utf8_lossy(&o.stdout).trim().to_string() + String::from_utf8_lossy(&o.stderr).trim()
}
fn pids_of(name: &str) -> Vec<i32> {
    let o = Command::new("pgrep").args(["-x", name]).output().unwrap();
    String::from_utf8_lossy(&o.stdout).lines().filter_map(|l| l.trim().parse().ok()).collect()
}

fn ax_bool(el: &AXUIElement, attr: &str) -> Option<bool> {
    windows::ax_attr(el, attr).and_then(|v| v.downcast_ref::<CFBoolean>().map(|b| b.as_bool()))
}
fn ax_set_bool(el: &AXUIElement, attr: &str, v: bool) -> AXError {
    unsafe { el.set_attribute_value(&CFString::from_str(attr), CFBoolean::new(v)) }
}
fn ax_set_pos(el: &AXUIElement, x: f64, y: f64) -> AXError {
    let mut p = CGPoint { x, y };
    let Some(v) = (unsafe { AXValue::new(AXValueType::CGPoint, NonNull::from(&mut p).cast::<c_void>()) }) else {
        return AXError::Failure;
    };
    let t: &CFType = &v;
    unsafe { el.set_attribute_value(&CFString::from_str("AXPosition"), t) }
}

/// Open (or re-front) a Finder window on `dir` with `open -a Finder`; returns its window once listed.
fn open_finder(dir: &str, known: &[u32]) -> Option<windows::WinInfo> {
    let _ = std::fs::create_dir_all(dir);
    let name = std::path::Path::new(dir).file_name()?.to_string_lossy().to_string();
    let _ = Command::new("open").args(["-a", "Finder", dir]).status();
    wait_for(8.0, || {
        windows::list_windows()
            .into_iter()
            .find(|w| w.app_name == "Finder" && w.title == name && (known.is_empty() || !known.contains(&w.wid)))
    })
}

/// Open the scratch document `path` in TextEdit with `open -a TextEdit`; returns its window.
fn open_doc(path: &str) -> Option<windows::WinInfo> {
    if !std::path::Path::new(path).exists() {
        let _ = std::fs::write(path, "ZTab e2e scratch document\n");
    }
    // The title may or may not show the extension.
    let stem = std::path::Path::new(path).file_stem()?.to_string_lossy().to_string();
    let _ = Command::new("open").args(["-a", "TextEdit", path]).status();
    wait_for(10.0, || windows::list_windows().into_iter().find(|w| w.app_name == "TextEdit" && w.title.starts_with(&stem)))
}

fn close_finder(dir: &str) {
    // `close (every window whose ...)` fails on macOS 27's Finder (-10010) and left the windows
    // open (Finder then shows the parent folder once the scratch folder is deleted).
    let name = std::path::Path::new(dir).file_name().unwrap().to_string_lossy().to_string();
    osa(&format!(
        "tell application \"Finder\"\nrepeat with w in (get every Finder window whose name is \"{name}\")\nclose w\nend repeat\nend tell"
    ));
}

/// Safety net: close every Finder window showing the scratch folder or a folder inside it.
fn close_scratch_finder_windows(scratch: &str) {
    osa(&format!(
        "tell application \"Finder\"\nrepeat with w in (get every Finder window)\ntry\nif POSIX path of (target of w as alias) starts with \"{scratch}\" then close w\nend try\nend repeat\nend tell"
    ));
}

fn close_doc(path: &str) {
    // `tell application` would launch TextEdit if it is not running.
    if pids_of("TextEdit").is_empty() {
        return;
    }
    let name = std::path::Path::new(path).file_name().unwrap().to_string_lossy().to_string();
    osa(&format!("tell application \"TextEdit\" to close (every document whose name is \"{name}\") saving no"));
}

// ---------------------------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------------------------

struct Out {
    f: std::fs::File,
    pass: bool,
    skipped: Vec<String>,
}
impl Out {
    fn skip(&mut self, name: &str, why: &str) {
        self.skipped.push(name.to_string());
        let _ = writeln!(self.f, "CHECK {name}: SKIPPED: {why} -> skipped");
    }
    fn check(&mut self, name: &str, ok: bool, detail: String) {
        self.pass &= ok;
        let _ = writeln!(self.f, "CHECK {name}: {detail} -> {ok}");
    }
    fn note(&mut self, s: String) {
        let _ = writeln!(self.f, "  {s}");
    }
}

struct Ctx {
    scratch: String,
    /// TextEdit was running before the suite started (then it is never stopped or quit).
    te_was_running: bool,
    primary: (f64, f64, f64, f64),
}

impl Ctx {
    /// Quit TextEdit when this suite launched it, and wait until the process is gone (a later
    /// sub-suite must not mistake a TextEdit that is still exiting for one it may not touch).
    fn quit_textedit_if_ours(&self) {
        if !self.te_was_running && !pids_of("TextEdit").is_empty() {
            osa("tell application \"TextEdit\" to quit saving no");
            wait_for(8.0, || pids_of("TextEdit").is_empty().then_some(()));
        }
    }
}

/// Wait for `wid` to be the front window and report it.
fn wait_front(wid: u32, timeout: f64) -> Option<u32> {
    wait_for(timeout, || (front_wid() == Some(wid)).then_some(wid));
    front_wid()
}

// ---------------------------------------------------------------------------------------------
// a) full-screen Space + MRU
// ---------------------------------------------------------------------------------------------

/// P (scratch Finder window) then W (scratch document), both on the primary display; W goes
/// full screen. Returns (P, W, W's AX element, W is full screen and in front).
fn setup_a(cx: &Ctx, dir: &str, doc: &str, out: &mut Out) -> Result<(u32, u32, CFRetained<AXUIElement>, bool), String> {
    let p = open_finder(dir, &[]).ok_or("scratch Finder window P did not appear")?;
    if let Some(el) = &p.ax {
        ax_set_pos(el, cx.primary.0 + 200.0, cx.primary.1 + 150.0);
    }
    pump(1.3);
    let w = open_doc(doc).ok_or("scratch TextEdit window W did not appear")?;
    let el = w.ax.clone().ok_or("W has no AX element")?;
    ax_set_pos(&el, cx.primary.0 + 300.0, cx.primary.1 + 200.0);
    pump(1.3);
    let e = ax_set_bool(&el, "AXFullScreen", true);
    let fs = wait_for(6.0, || (ax_bool(&el, "AXFullScreen") == Some(true) && front_wid() == Some(w.wid)).then_some(()));
    pump(1.5);
    out.note(format!("(a) P={} W={} AXFullScreen set -> {e:?}, full screen + front: {}", p.wid, w.wid, fs.is_some()));
    Ok((p.wid, w.wid, el, fs.is_some()))
}

fn suite_a(cx: &Ctx, out: &mut Out) {
    let dir = format!("{}/ztab-sp-P", cx.scratch);
    let doc = format!("{}/ztab-sp-W.txt", cx.scratch);
    let mut w_el: Option<CFRetained<AXUIElement>> = None;
    match setup_a(cx, &dir, &doc, out) {
        Err(e) => out.check("a/setup", false, e),
        Ok((p, w, el, fs)) => {
            w_el = Some(el.clone());
            out.check("a/full screen", fs, format!("W={w} AXFullScreen={:?} front={:?}", ax_bool(&el, "AXFullScreen"), front_wid()));

            // 1. Quick ⌥Tab from full-screen W: the previous window P on the desktop Space.
            let t1 = now_secs();
            quick_tap();
            let front = wait_front(p, 5.0);
            pump(0.8);
            let commits = commits_since(t1);
            let w_visible = onscreen(w);
            out.check(
                "a/quick tap to desktop P",
                commits == vec![p] && front == Some(p) && !w_visible,
                format!("commits={commits:?} front={front:?} W on screen={w_visible}"),
            );

            // 2. Quick ⌥Tab again: back to W on its full-screen Space.
            let t2 = now_secs();
            quick_tap();
            let front = wait_front(w, 5.0);
            pump(0.8);
            let commits = commits_since(t2);
            let fs_now = ax_bool(&el, "AXFullScreen");
            out.check(
                "a/quick tap back to full-screen W",
                commits == vec![w] && front == Some(w) && onscreen(w) && fs_now == Some(true),
                format!("commits={commits:?} front={front:?} W on screen={} AXFullScreen={fs_now:?}", onscreen(w)),
            );

            // 3. Hold ⌥Tab and click P's tile.
            let t3 = now_secs();
            option_down();
            tab();
            let panel = wait_for(1.5, || panel_since(t3));
            let target = panel.as_ref().and_then(|pl| pl.tiles.iter().find(|(id, ..)| *id == p).map(|(_, pt, _)| *pt));
            let w_listed = panel.as_ref().is_some_and(|pl| pl.tiles.iter().any(|(id, ..)| *id == w));
            if let Some(pt) = target {
                click(pt, CGEventFlags::MaskAlternate);
            }
            pump(0.9);
            option_up();
            let front = wait_front(p, 5.0);
            pump(0.5);
            let commits = commits_since(t3);
            out.check(
                "a/click P's tile from full-screen W",
                target.is_some() && w_listed && commits == vec![p] && front == Some(p),
                format!("tile found={} W listed={w_listed} commits={commits:?} front={front:?}", target.is_some()),
            );
        }
    }
    // Cleanup: leave full screen, close W and P.
    if let Some(el) = &w_el
        && ax_bool(el, "AXFullScreen") == Some(true)
    {
        ax_set_bool(el, "AXFullScreen", false);
        let left = wait_for(6.0, || (ax_bool(el, "AXFullScreen") == Some(false)).then_some(()));
        pump(1.5);
        out.note(format!("(a) cleanup: left full screen: {}", left.is_some()));
    }
    close_doc(&doc);
    close_finder(&dir);
    pump(0.5);
    cx.quit_textedit_if_ours();
    pump(1.0);
    let _ = std::fs::remove_file(&doc);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------------------
// b) hung app
// ---------------------------------------------------------------------------------------------

/// Sends SIGCONT when dropped (also on panic).
struct ContGuard(i32);
impl Drop for ContGuard {
    fn drop(&mut self) {
        unsafe { libc::kill(self.0, libc::SIGCONT) };
    }
}

fn suite_b(cx: &Ctx, out: &mut Out) {
    if cx.te_was_running || !pids_of("TextEdit").is_empty() {
        out.skip("b/hung app", "TextEdit was already running (only a TextEdit this suite launched may be stopped)");
        return;
    }
    let d1 = format!("{}/ztab-sp-hung1.txt", cx.scratch);
    let d2 = format!("{}/ztab-sp-hung2.txt", cx.scratch);
    let w1 = open_doc(&d1);
    pump(0.5);
    let w2 = open_doc(&d2);
    let te: Vec<i32> = pids_of("TextEdit");
    match (w1, w2, te.as_slice()) {
        (Some(w1), Some(w2), [pid]) if w1.pid == *pid && w2.pid == *pid => {
            let pid = *pid;
            if let Some(el) = &w2.ax {
                ax_set_bool(el, "AXMinimized", true);
            }
            let min = wait_for(4.0, || w2.ax.as_ref().and_then(|el| ax_bool(el, "AXMinimized")).filter(|m| *m));
            pump(1.5);
            out.note(format!("(b) TextEdit pid={pid} normal={} minimized={} (min ok={})", w1.wid, w2.wid, min.is_some()));
            let t_stop = now_secs();
            let r = unsafe { libc::kill(pid, libc::SIGSTOP) };
            let guard = ContGuard(pid);
            // Let ZTab's model hit the stopped app (rescans, focus reads) before opening, and let
            // w1's thumbnail (taken when w2 opened) get older than ZTab's 3 s show-capture age,
            // so the panel must capture it again while TextEdit is stopped (ScreenCaptureKit
            // reads the window from the window server, not from the app).
            sleep(Duration::from_millis(3500));
            option_down();
            let t_key = now_secs();
            tab();
            // Only ZTab's log is read while TextEdit is stopped (AX calls from here would time out).
            // The panel stays up long enough for its captures to land.
            sleep(Duration::from_millis(1200));
            let open = first_line_since(t_key, "open:");
            let panel = panel_since(t_key);
            esc();
            option_up();
            sleep(Duration::from_millis(300));
            drop(guard);
            let t_cont = now_secs();
            sleep(Duration::from_millis(1000));
            let total = open.as_ref().and_then(|(_, l)| num_after(l, "total_ms="));
            let list = open.as_ref().and_then(|(_, l)| num_after(l, "list_ms="));
            let shown_after = panel.as_ref().map(|p| (p.at - t_key) * 1000.0);
            let both = panel.as_ref().is_some_and(|p| {
                p.tiles.iter().any(|(w, ..)| *w == w1.wid) && p.tiles.iter().any(|(w, ..)| *w == w2.wid)
            });
            let stalls: Vec<String> = log_since(t_stop)
                .into_iter()
                .filter(|(ts, m)| *ts <= t_cont + 0.5 && (m.contains("main thread stall") || m.contains("main thread still blocked")))
                .map(|(ts, m)| format!("[{ts:.3}] {m}"))
                .collect();
            let cancelled = log_since(t_key).iter().any(|(_, m)| m == "cancel");
            // `panel hidden (...) thumbs_landed=[wid@ms,...]`: captures that landed while the panel
            // was up, i.e. while TextEdit was stopped (the panel is hidden before SIGCONT).
            let hidden = log_since(t_key).into_iter().find(|(ts, m)| *ts <= t_cont && m.starts_with("panel hidden"));
            let landed: Vec<String> = hidden
                .as_ref()
                .and_then(|(_, m)| m.split("thumbs_landed=[").nth(1)?.split(']').next())
                .map(|l| l.split(',').filter(|x| !x.is_empty()).map(str::to_string).collect())
                .unwrap_or_default();
            let w1_captured = landed.iter().any(|x| x.split('@').next() == Some(&w1.wid.to_string()));
            out.check(
                "b/hung app",
                r == 0
                    && total.is_some_and(|t| t < 30.0)
                    && shown_after.is_some_and(|s| s < 250.0)
                    && both
                    && cancelled
                    && stalls.is_empty()
                    && commits_since(t_key).is_empty(),
                format!(
                    "SIGSTOP={r} open list_ms={list:?} total_ms={total:?} key->panel={shown_after:?} ms both docs listed={both} cancelled={cancelled} stalls={stalls:?}"
                ),
            );
            // ZTab counts capture-pipeline steps that ran on its main thread (must stay 0).
            let on_main = hidden.as_ref().and_then(|(_, m)| num_after(m, "on_main_thread="));
            out.check(
                "b/stopped app captured",
                r == 0 && w1_captured && stalls.is_empty() && on_main == Some(0.0),
                format!(
                    "a fresh thumbnail of the stopped app's window {} landed while it was stopped: {w1_captured}; capture steps on ZTab's main thread: {on_main:?} (landed {landed:?}; panel hidden line: {:?})",
                    w1.wid,
                    hidden.map(|(ts, m)| format!("[{ts:.3}] {}", m.chars().take(320).collect::<String>()))
                ),
            );
            if let Some(p) = &panel {
                out.note(format!("(b) {}", p.line.chars().take(220).collect::<String>()));
            }
        }
        (w1, w2, te) => out.check(
            "b/hung app",
            false,
            format!("setup failed: w1={:?} w2={:?} TextEdit pids={te:?}", w1.map(|w| w.wid), w2.map(|w| w.wid)),
        ),
    }
    // SIGCONT once more in case anything above bailed out early.
    for p in pids_of("TextEdit") {
        unsafe { libc::kill(p, libc::SIGCONT) };
    }
    pump(0.5);
    close_doc(&d1);
    close_doc(&d2);
    pump(0.5);
    cx.quit_textedit_if_ours();
    pump(1.0);
    let _ = std::fs::remove_file(&d1);
    let _ = std::fs::remove_file(&d2);
}

// ---------------------------------------------------------------------------------------------
// f) busy app raise
// ---------------------------------------------------------------------------------------------

/// `focus: wid=<wid> <what> after <ms> ms` lines since `t`: (what, ms).
fn focus_lines(t: f64, wid: u32) -> Vec<(String, f64)> {
    let prefix = format!("focus: wid={wid} ");
    log_since(t)
        .into_iter()
        .filter_map(|(_, m)| {
            let rest = m.strip_prefix(&prefix)?;
            let (what, after) = rest.split_once(" after ")?;
            Some((what.to_string(), after.trim_end_matches(" ms").parse().ok()?))
        })
        .collect()
}

fn suite_f(cx: &Ctx, out: &mut Out) {
    if cx.te_was_running || !pids_of("TextEdit").is_empty() {
        out.skip("f/busy raise", "TextEdit was already running (only a TextEdit this suite launched may be stopped)");
        return;
    }
    let doc = format!("{}/ztab-sp-busyA.txt", cx.scratch);
    let dir = format!("{}/ztab-sp-busyB", cx.scratch);
    let a = open_doc(&doc);
    pump(1.3);
    let b = open_finder(&dir, &[]);
    pump(1.3);
    let te = pids_of("TextEdit");
    match (a, b, te.as_slice()) {
        (Some(a), Some(b), [pid]) if a.pid == *pid => {
            let pid = *pid;
            let front0 = wait_front(b.wid, 4.0);
            let r = unsafe { libc::kill(pid, libc::SIGSTOP) };
            let guard = ContGuard(pid);
            sleep(Duration::from_millis(800));
            // Two switches in a row, committed with Return while ⌥ stays held (fewer events than
            // two quick taps, so the second one reliably lands while A's raise is blocked): to A,
            // whose app is stopped so its AX raise blocks, then back to B.
            let t1 = now_secs();
            let keys = Instant::now();
            option_down();
            tab();
            ret();
            let t2 = now_secs();
            tab();
            ret();
            option_up();
            let keys_ms = keys.elapsed().as_secs_f64() * 1000.0;
            sleep(Duration::from_millis(1200));
            let b_steps = focus_lines(t2, b.wid);
            drop(guard);
            pump(3.5); // TextEdit applies A's queued raise; ZTab checks the newest switch
            let front = wait_front(b.wid, 2.0);
            let commits = commits_since(t1);
            let ms_of = |what: &str| b_steps.iter().find(|(w, _)| w.starts_with(what)).map(|(_, ms)| *ms);
            let (fronted, raised) = (ms_of("fronted ok=true"), ms_of("raised"));
            let focus_log: Vec<(f64, String)> = log_since(t1).into_iter().filter(|(_, m)| m.starts_with("focus: ")).collect();
            for (ts, m) in &focus_log {
                out.note(format!("(f) [{ts:.3}] {m}"));
            }
            // The point of the check: B was switched to while A's raise was still blocked.
            let at = |wid: u32, what: &str| {
                let p = format!("focus: wid={wid} {what}");
                focus_log.iter().find(|(_, m)| m.starts_with(&p)).map(|(ts, _)| *ts)
            };
            let a_done = at(a.wid, "raise");
            let b_fronted = at(b.wid, "fronted");
            let overlap = matches!((b_fronted, a_done), (Some(b), Some(a)) if b < a);
            out.check(
                "f/busy raise",
                r == 0
                    && front0 == Some(b.wid)
                    && commits == vec![a.wid, b.wid]
                    && overlap
                    && fronted.is_some_and(|ms| ms < 200.0)
                    && raised.is_some_and(|ms| ms < 200.0)
                    && front == Some(b.wid),
                format!(
                    "SIGSTOP={r} A={} B={} B front before={} keys {keys_ms:.0} ms commits={commits:?} B fronted while A's raise was blocked={overlap} \
                     B fronted after {fronted:?} ms, raised after {raised:?} ms; front after SIGCONT={front:?}",
                    a.wid,
                    b.wid,
                    front0 == Some(b.wid)
                ),
            );
        }
        (a, b, te) => out.check(
            "f/busy raise",
            false,
            format!("setup failed: A={:?} B={:?} TextEdit pids={te:?}", a.map(|w| (w.wid, w.pid)), b.map(|w| w.wid)),
        ),
    }
    for p in pids_of("TextEdit") {
        unsafe { libc::kill(p, libc::SIGCONT) };
    }
    pump(0.5);
    close_doc(&doc);
    close_finder(&dir);
    pump(0.5);
    cx.quit_textedit_if_ours();
    pump(1.0);
    let _ = std::fs::remove_file(&doc);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------------------
// c) external MRU
// ---------------------------------------------------------------------------------------------

fn suite_c(cx: &Ctx, out: &mut Out) {
    let dir = format!("{}/ztab-sp-A", cx.scratch);
    let doc = format!("{}/ztab-sp-T.txt", cx.scratch);
    let a = open_finder(&dir, &[]);
    pump(1.3);
    let t = open_doc(&doc);
    pump(1.3);
    match (a, t) {
        (Some(a), Some(t)) => {
            // Back to A through `open -a` again: Finder re-fronts the existing window.
            let _ = Command::new("open").args(["-a", "Finder", &dir]).status();
            let front = wait_front(a.wid, 4.0);
            pump(1.3);
            out.note(format!("(c) A={} T={} after re-open front={front:?}", a.wid, t.wid));
            let t1 = now_secs();
            quick_tap();
            let f1 = wait_front(t.wid, 4.0);
            pump(0.6);
            let c1 = commits_since(t1);
            let t2 = now_secs();
            quick_tap();
            let f2 = wait_front(a.wid, 4.0);
            pump(0.6);
            let c2 = commits_since(t2);
            out.check(
                "c/external MRU",
                front == Some(a.wid) && c1 == vec![t.wid] && f1 == Some(t.wid) && c2 == vec![a.wid] && f2 == Some(a.wid),
                format!("re-fronted A={} tap1 commits={c1:?} front={f1:?}; tap2 commits={c2:?} front={f2:?}", front == Some(a.wid)),
            );
        }
        (a, t) => out.check("c/external MRU", false, format!("setup failed: A={:?} T={:?}", a.map(|w| w.wid), t.map(|w| w.wid))),
    }
    close_doc(&doc);
    close_finder(&dir);
    pump(0.5);
    cx.quit_textedit_if_ours();
    pump(1.0);
    let _ = std::fs::remove_file(&doc);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------------------
// d) minimized window thumbnail
// ---------------------------------------------------------------------------------------------

fn suite_d(cx: &Ctx, out: &mut Out) {
    let dir = format!("{}/ztab-sp-D", cx.scratch);
    let doc = format!("{}/ztab-sp-D2.txt", cx.scratch);
    let t0 = now_secs();
    let fd = open_finder(&dir, &[]);
    pump(1.2);
    let other = open_doc(&doc);
    pump(1.2);
    match (fd, other) {
        (Some(fd), Some(_)) => {
            let left = log_since(t0).iter().any(|(_, m)| m == &format!("model: focus left wid={} (capturing)", fd.wid));
            if let Some(el) = &fd.ax {
                ax_set_bool(el, "AXMinimized", true);
            }
            let min = wait_for(4.0, || fd.ax.as_ref().and_then(|el| ax_bool(el, "AXMinimized")).filter(|m| *m));
            pump(1.2);
            let t1 = now_secs();
            option_down();
            tab();
            let panel = wait_for(1.5, || panel_since(t1));
            pump(0.2);
            esc();
            option_up();
            pump(0.4);
            let tile = panel.as_ref().and_then(|p| p.tiles.iter().find(|(w, ..)| *w == fd.wid).cloned());
            out.check(
                "d/minimized thumbnail",
                left && min.is_some() && tile.as_ref().is_some_and(|t| t.2),
                format!(
                    "FD={} focus-left capture logged={left} minimized={} tile listed={} thumbnail at show={}",
                    fd.wid,
                    min.is_some(),
                    tile.is_some(),
                    tile.as_ref().is_some_and(|t| t.2)
                ),
            );
        }
        (fd, o) => out.check("d/minimized thumbnail", false, format!("setup failed: FD={:?} other={:?}", fd.map(|w| w.wid), o.map(|w| w.wid))),
    }
    close_finder(&dir);
    close_doc(&doc);
    pump(0.5);
    cx.quit_textedit_if_ours();
    pump(1.0);
    let _ = std::fs::remove_file(&doc);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------------------
// e) secure input
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
fn title(el: &AXUIElement) -> String {
    attr(el, "AXTitle").and_then(|v| v.downcast_ref::<CFString>().map(|s| s.to_string())).unwrap_or_default()
}
fn center(el: &AXUIElement) -> Option<CGPoint> {
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
    Some(CGPoint { x: p.x + s.width / 2.0, y: p.y + s.height / 2.0 })
}
fn find_title_prefix(el: &AXUIElement, prefix: &str, depth: usize) -> Option<String> {
    for c in children(el) {
        let t = title(&c);
        if t.starts_with(prefix) {
            return Some(t);
        }
        if depth > 0
            && let Some(f) = find_title_prefix(&c, prefix, depth - 1)
        {
            return Some(f);
        }
    }
    None
}

/// The running ZTab: the oldest live process with ZTab's bundle id other than this one (every e2e
/// bundle shares the bundle id, and LaunchServices may still list a suite that just exited).
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

/// Time spans (unix s) during which suite (e) had ZTab's status menu opening, open or closing.
static MENU_SPANS: std::sync::Mutex<Vec<(f64, f64)>> = std::sync::Mutex::new(Vec::new());
/// A stall while the status menu is shown fails the suite only above this.
const MENU_STALL_MAX_MS: f64 = 250.0;

/// What one look at ZTab's status menu found.
struct MenuRead {
    /// The menu opened (its window appeared on screen).
    open: bool,
    /// Title of the secure-input warning item, when shown.
    item: Option<String>,
    /// Diagnostics: status items in AXExtrasMenuBar, the icon's center, whether the menu closed.
    detail: String,
}

/// ZTab's open menu: an on-screen window of `pid` at the pop-up menu level
/// (kCGPopUpMenuWindowLevel = 101). AX is no use here: it lists a status menu's items, and
/// AppKit runs `menuWillOpen:` for AX, even while the menu is closed.
fn menu_window_open(pid: i32) -> bool {
    windows::cg_window_list(CGWindowListOption::OptionOnScreenOnly)
        .iter()
        .filter_map(|d| windows::parse_cg(d))
        .any(|(_, p, layer, _, alpha, _)| p == pid && layer == 101 && alpha > 0.0)
}

/// Open ZTab's status menu with a click on its icon, read the secure-input item, and close the
/// menu with a second click, sent only when the menu did open (a blind second click would open a
/// menu that the first one failed to open, and leave it open on the user's screen).
fn read_secure_item(pid: i32) -> MenuRead {
    let app = unsafe { AXUIElement::new_application(pid) };
    unsafe { app.set_messaging_timeout(1.0) };
    let Some(bar) = attr(&app, "AXExtrasMenuBar").and_then(|v| v.downcast::<AXUIElement>().ok()) else {
        return MenuRead { open: false, item: None, detail: "no AXExtrasMenuBar".into() };
    };
    let items = children(&bar);
    let Some(icon) = items.first().and_then(|i| center(i)) else {
        return MenuRead { open: false, item: None, detail: format!("{} status items, no frame", items.len()) };
    };
    let open_before = menu_window_open(pid);
    let span_start = now_secs();
    click(icon, CGEventFlags::empty());
    let open = wait_for(2.0, || menu_window_open(pid).then_some(())).is_some();
    let item = if open { find_title_prefix(&bar, "⚠ Secure input is on", 4) } else { None };
    let mut closed = !open;
    if open {
        click(icon, CGEventFlags::empty());
        closed = wait_for(2.0, || (!menu_window_open(pid)).then_some(())).is_some();
    }
    if let Ok(mut v) = MENU_SPANS.lock() {
        v.push((span_start, now_secs()));
    }
    MenuRead {
        open,
        item,
        detail: format!(
            "{} status items, icon at ({:.0}, {:.0}), menu window open before click={open_before}, closed again={closed}",
            items.len(),
            icon.x,
            icon.y
        ),
    }
}

fn suite_e(_cx: &Ctx, out: &mut Out) {
    let me = std::process::id() as i32;
    let listed: Vec<String> = NSRunningApplication::runningApplicationsWithBundleIdentifier(&NSString::from_str("dev.ztab.app"))
        .iter()
        .map(|a| {
            let p = a.processIdentifier();
            format!("{p}{}{}", if p == me { " (this suite)" } else { "" }, if a.isTerminated() || unsafe { libc::kill(p, 0) } != 0 { " (gone)" } else { "" })
        })
        .collect();
    out.note(format!("(e) dev.ztab.app processes: {listed:?}; ZTab = {:?}", ztab_pid()));
    let Some(pid) = ztab_pid() else {
        out.check("e/secure input", false, "ZTab is not running".into());
        return;
    };
    let original = mouse_location();
    // Control: the menu opens and shows no warning while secure input is off.
    let control = read_secure_item(pid);
    out.note(format!("(e) control: open={} item={:?} {}", control.open, control.item, control.detail));
    let t0 = now_secs();
    let r = unsafe { EnableSecureEventInput() };
    let on = unsafe { IsSecureEventInputEnabled() } != 0;
    let logged_on = wait_for(4.0, || first_line_since(t0, "secure input on"));
    let held = read_secure_item(pid);
    out.note(format!("(e) held: open={} item={:?} {}", held.open, held.item, held.detail));
    let t1 = now_secs();
    let r2 = unsafe { DisableSecureEventInput() };
    let off = unsafe { IsSecureEventInputEnabled() } == 0;
    let logged_off = wait_for(4.0, || first_line_since(t1, "secure input off"));
    let after = read_secure_item(pid);
    out.note(format!("(e) released: open={} item={:?} {}", after.open, after.item, after.detail));
    if let Some(p) = original {
        mouse(CGEventType::MouseMoved, p, CGEventFlags::empty());
    }
    out.check(
        "e/secure input",
        control.open
            && control.item.is_none()
            && r == 0
            && on
            && logged_on.is_some()
            && held.open
            && held.item.is_some()
            && r2 == 0
            && off
            && logged_off.is_some()
            && after.open
            && after.item.is_none(),
        format!(
            "control_open={} enable={r} on={on} log_on={:?} menu_open={} item={:?}; disable={r2} off={off} log_off={} menu_open={} item_after={:?}",
            control.open,
            logged_on.map(|l| l.1),
            held.open,
            held.item,
            logged_off.is_some(),
            after.open,
            after.item
        ),
    );
}

// ---------------------------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let f = std::fs::File::create(&args[1]).unwrap();
    let mut out = Out { f, pass: true, skipped: Vec::new() };
    let scratch = args.get(2).cloned().unwrap_or_else(|| "/tmp".into());
    let only = args.iter().find_map(|a| a.strip_prefix("only=")).unwrap_or("abcdef").to_string();
    // Keep App Nap from stretching this background process's sleeps (the key timings matter).
    let _activity = objc2_foundation::NSProcessInfo::processInfo().beginActivityWithOptions_reason(
        objc2_foundation::NSActivityOptions::UserInitiatedAllowingIdleSystemSleep | objc2_foundation::NSActivityOptions::LatencyCritical,
        &NSString::from_str("ZTab end-to-end timings"),
    );
    let start = now_secs();
    let b = CGDisplayBounds(CGMainDisplayID());
    let cx = Ctx {
        scratch,
        te_was_running: !pids_of("TextEdit").is_empty(),
        primary: (b.origin.x, b.origin.y, b.size.width, b.size.height),
    };
    out.note(format!(
        "ax={} screen={} TextEdit running before={} primary={:?} start={start:.3}",
        windows::ax_trusted(),
        windows::screen_recording_granted(),
        cx.te_was_running,
        cx.primary
    ));
    let suites: [(char, fn(&Ctx, &mut Out)); 6] =
        [('a', suite_a), ('b', suite_b), ('c', suite_c), ('d', suite_d), ('e', suite_e), ('f', suite_f)];
    for (c, f) in suites {
        if only.contains(c) {
            let t = now_secs();
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&cx, &mut out)));
            if r.is_err() {
                out.check(&format!("{c}/panic"), false, "suite panicked".into());
            }
            out.note(format!("({c}) {:.1} s", now_secs() - t));
            pump(0.8);
        }
    }
    // Every main-thread stall ZTab logged while the suite ran. Stalls while suite (e) had ZTab's
    // status menu on screen are AppKit presenting the menu (measured with `sample`: popup window
    // layout and Core Animation commit, 55-80 ms the first time); they are reported, and fail the
    // suite only above MENU_STALL_MAX_MS. Every other stall fails it.
    let spans = MENU_SPANS.lock().map(|v| v.clone()).unwrap_or_default();
    let in_menu = |ts: f64| spans.iter().any(|(a, b)| ts >= *a && ts <= *b + 0.2);
    let (menu, other): (Vec<(f64, String)>, Vec<(f64, String)>) = log_since(start)
        .into_iter()
        .filter(|(_, m)| m.contains("main thread stall") || m.contains("main thread still blocked"))
        .partition(|(ts, _)| in_menu(*ts));
    let ms = |m: &str| num_after(m, ": ").unwrap_or(f64::MAX);
    let menu_ok = menu.iter().all(|(_, m)| !m.contains("still blocked") && ms(m) <= MENU_STALL_MAX_MS);
    let fmt = |v: &[(f64, String)]| v.iter().map(|(ts, m)| format!("[{ts:.3}] {m}")).collect::<Vec<_>>();
    out.note(format!("stalls while ZTab's status menu was being shown (AppKit): {:?}", fmt(&menu)));
    out.check(
        "no main-thread stall during the suite",
        other.is_empty() && menu_ok,
        format!("{:?}{}", fmt(&other), if menu_ok { "" } else { " (a status-menu stall exceeded the limit)" }),
    );
    close_scratch_finder_windows(&cx.scratch);
    for p in pids_of("TextEdit") {
        if !cx.te_was_running {
            unsafe { libc::kill(p, libc::SIGCONT) };
        }
    }
    let result = match (out.pass, out.skipped.is_empty()) {
        (false, _) => "FAIL".to_string(),
        (true, true) => "PASS".to_string(),
        (true, false) => format!("INCOMPLETE (no failures; skipped: {})", out.skipped.join(", ")),
    };
    let _ = writeln!(out.f, "RESULT {result}");
}
