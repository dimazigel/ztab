//! End-to-end check against a running ZTab: posts real (synthetic HID) key/mouse events and
//! verifies which window ends up focused. Must run with Accessibility (bundle it with
//! `scripts/bundle.sh e2e dist/e2e/ZTab.app` and `open -W -n dist/e2e/ZTab.app --args <out>`).
//! Requires an unlocked screen and a running ZTab (dist/ZTab.app or ~/Applications/ZTab.app).

#[path = "../windows.rs"]
#[allow(dead_code)]
mod windows;

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::thread::sleep;
use std::time::Duration;

use objc2_app_kit::NSWorkspace;
use objc2_core_foundation::{CGPoint, CFRetained};
use objc2_core_graphics::{
    CGEvent, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGEventType, CGMouseButton,
};

thread_local! {
    // Events from a HID-state source update the system modifier state like a real keyboard.
    static SRC: CFRetained<CGEventSource> = CGEventSource::new(CGEventSourceStateID::HIDSystemState).unwrap();
}

const OPTION: u16 = 0x3A;
const TAB: u16 = 0x30;
const ESC: u16 = 0x35;

fn key(code: u16, down: bool, flags: CGEventFlags) {
    let ev: CFRetained<CGEvent> = SRC.with(|src| CGEvent::new_keyboard_event(Some(src), code, down)).unwrap();
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

fn mouse(kind: CGEventType, p: CGPoint) {
    let ev = SRC.with(|src| CGEvent::new_mouse_event(Some(src), kind, p, CGMouseButton::Left)).unwrap();
    CGEvent::set_flags(Some(&ev), CGEventFlags::MaskAlternate);
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
    sleep(Duration::from_millis(40));
}

/// The window in front: topmost on-screen layer-0 window of a regular app. (AX "focused window"
/// is unreliable as a probe: Chrome often doesn't answer it in time.)
fn front_wid() -> Option<u32> {
    windows::list_windows().into_iter().min_by_key(|w| w.z_index).map(|w| w.wid)
}

fn last_commit_wid() -> Option<u32> {
    let log = std::fs::read_to_string(std::env::var("HOME").unwrap() + "/Library/Logs/ztab.log").ok()?;
    let line = log.lines().rev().find(|l| l.contains("commit: wid="))?;
    line.split("commit: wid=").nth(1)?.split(' ').next()?.parse().ok()
}

fn commit_count() -> usize {
    std::fs::read_to_string(std::env::var("HOME").unwrap() + "/Library/Logs/ztab.log")
        .map(|s| s.matches("commit: wid=").count())
        .unwrap_or(0)
}

/// The last `open:` line ZTab logged: (current index, selected index, the switcher's window order).
fn last_open() -> Option<(Option<usize>, usize, Vec<u32>)> {
    let log = std::fs::read_to_string(std::env::var("HOME").unwrap() + "/Library/Logs/ztab.log").ok()?;
    let line = log.lines().rev().find(|l| l.contains("] open: "))?;
    let cur = line.split("current=Some(").nth(1).and_then(|r| r.split(')').next()?.parse().ok());
    let sel = line.split(" selected=").nth(1)?.split(' ').next()?.parse().ok()?;
    let order = line.split(" order=").nth(1)?.split_whitespace().next()?.split(',').filter_map(|w| w.parse().ok()).collect();
    Some((cur, sel, order))
}

/// Owner pids of windows (CoreGraphics).
fn owner_pids(wids: &[u32]) -> HashMap<u32, i32> {
    windows::cg_describe(wids).iter().filter_map(|d| windows::parse_cg(d)).map(|f| (f.0, f.1)).collect()
}

/// Whether `order` keeps each app's windows together (windows CoreGraphics no longer knows are
/// ignored).
fn grouped(order: &[u32], pids: &HashMap<u32, i32>) -> bool {
    let mut seen = HashSet::new();
    let mut last = None;
    for p in order.iter().filter_map(|w| pids.get(w)) {
        if last != Some(*p) && !seen.insert(*p) {
            return false;
        }
        last = Some(*p);
    }
    true
}

/// A quick tap's session follows the rule: the order is grouped by app, the preselected window
/// is the one after the current window (the first one without a current window), and it is
/// what got committed. Returns (ok, detail, current wid, current app has another window).
fn quick_tap_rule(committed: Option<u32>) -> (bool, String, Option<u32>, bool) {
    let Some((cur, sel, order)) = last_open() else { return (false, "no open: line with order=".into(), None, false) };
    let pids = owner_pids(&order);
    let n = order.len();
    let want = match cur {
        Some(c) if n > 0 => (c + 1) % n,
        _ => 0,
    };
    let cur_wid = cur.and_then(|c| order.get(c).copied());
    let cur_pid = cur_wid.and_then(|w| pids.get(&w).copied());
    let siblings = cur_pid.is_some_and(|p| order.iter().filter(|w| pids.get(w) == Some(&p)).count() >= 2);
    let same_app_next = !siblings || order.get(want).and_then(|w| pids.get(w)) == cur_pid.as_ref();
    let g = grouped(&order, &pids);
    let ok = g && sel == want && committed == order.get(sel).copied() && same_app_next;
    let detail = format!(
        "current={cur:?} ({cur_wid:?}) selected={sel} (want {want}) wid={:?} grouped={g} app has another window={siblings} -> same app={same_app_next}",
        order.get(sel)
    );
    (ok, detail, cur_wid, siblings)
}

/// Frame (x, y, w, h) of ZTab's switcher panel if it is currently shown (from ZTab's log).
fn panel_frame() -> Option<(f64, f64, f64, f64)> {
    let log = std::fs::read_to_string(std::env::var("HOME").unwrap() + "/Library/Logs/ztab.log").ok()?;
    let line = log.lines().rev().find(|l| l.contains("panel shown:") || l.contains("panel hidden"))?;
    let v: Vec<f64> = line.split("cg_frame=").nth(1)?.split_whitespace().filter_map(|x| x.parse().ok()).collect();
    (v.len() == 4).then(|| (v[0], v[1], v[2], v[3]))
}

/// Tile centers (wid, point) of the panel currently shown, from ZTab's `panel shown:` line.
fn panel_tiles() -> Vec<(u32, CGPoint)> {
    let Some(log) = std::fs::read_to_string(std::env::var("HOME").unwrap() + "/Library/Logs/ztab.log").ok() else { return vec![] };
    let Some(line) = log.lines().rev().find(|l| l.contains("panel shown:") || l.contains("panel hidden")) else { return vec![] };
    line.split("tiles=")
        .nth(1)
        .unwrap_or("")
        .split(';')
        .filter_map(|t| {
            let mut parts = t.trim().split(':');
            let wid = parts.next()?.parse().ok()?;
            let (x, y) = parts.next()?.split_once(',')?;
            Some((wid, CGPoint { x: x.parse().ok()?, y: y.trim().parse().ok()? }))
        })
        .collect()
}

fn main() {
    let out_path = std::env::args().nth(1).unwrap_or_else(|| "/dev/stdout".into());
    let mut out = std::fs::File::create(&out_path).unwrap();
    let mut report = |s: String| {
        let _ = writeln!(out, "{s}");
    };
    report(format!("ax={} screen={}", windows::ax_trusted(), windows::screen_recording_granted()));
    let mut pass = true;

    option_down();
    let held = CGEventSource::flags_state(CGEventSourceStateID::CombinedSessionState).contains(CGEventFlags::MaskAlternate);
    option_up();
    report(format!("synthetic option registers in session state: {held}"));
    sleep(Duration::from_millis(300));

    // 1. Quick Option+Tab: switches without showing the panel, to the window after the current
    // one in the switcher's order (`order=` on ZTab's `open:` line: the current app's other
    // windows first, then the other apps by their most recent open window; checked grouped by app
    // with CoreGraphics' owner pids). When the frontmost app has no window in front (e.g. Finder
    // with only the desktop, as a previous suite's cleanup can leave it), the user is in no
    // window: the quick tap goes to the first window of the order, which may be the one already
    // on top.
    let top = windows::list_windows().into_iter().min_by_key(|w| w.z_index);
    let before = top.as_ref().map(|w| w.wid);
    let front_app = NSWorkspace::sharedWorkspace().frontmostApplication().map(|a| a.processIdentifier());
    let in_window = top.as_ref().is_some_and(|w| Some(w.pid) == front_app);
    let n0 = commit_count();
    option_down();
    tab();
    option_up();
    sleep(Duration::from_millis(700));
    let after = front_wid();
    let committed = last_commit_wid();
    let (rule, detail, _, _) = quick_tap_rule(committed);
    let ok = commit_count() == n0 + 1 && after.is_some() && after == committed && (after != before || !in_window) && rule;
    pass &= ok;
    report(format!("CHECK quick-tap: before={before:?} in_window={in_window} after={after:?} committed={committed:?} {detail} -> {ok}"));

    // 2. Two more quick taps, each to the window after the current one. From a window whose app
    // has another window, the next one is that app's (same app first), and a second tap returns
    // (the two most recent windows of the app toggle). This suite runs on the user's windows: the
    // toggle is only checked when the first tap lands in an app with another window, and the line
    // says when it was not (e2e_spaces a and c check it on scratch windows).
    let first = after;
    option_down();
    tab();
    option_up();
    sleep(Duration::from_millis(700));
    let second = front_wid();
    let second_commit = last_commit_wid();
    let (rule2, detail2, cur2, siblings2) = quick_tap_rule(second_commit);
    option_down();
    tab();
    option_up();
    sleep(Duration::from_millis(700));
    let third = front_wid();
    let third_commit = last_commit_wid();
    let (rule3, detail3, _, _) = quick_tap_rule(third_commit);
    let toggle = !siblings2 || third == first;
    let toggle_note = if siblings2 {
        (third == first).to_string()
    } else {
        "not checked (tap1's app has no other window; e2e_spaces a and c check the toggle)".to_string()
    };
    let ok = first.is_some()
        && cur2 == first
        && second.is_some()
        && second == second_commit
        && second != first
        && rule2
        && third == third_commit
        && rule3
        && toggle;
    pass &= ok;
    report(format!(
        "CHECK next window, same app first: tap1={first:?} tap2={second:?} [{detail2}] tap3={third:?} [{detail3}] toggle within the app={toggle_note} -> {ok}"
    ));

    // 3. Hold: Option+Tab, wait for panel, Tab twice, screenshot, release.
    let n0 = commit_count();
    option_down();
    tab();
    sleep(Duration::from_millis(500));
    let frame = panel_frame();
    tab();
    tab();
    sleep(Duration::from_millis(300));
    if let Some(dir) = std::path::Path::new(&out_path).parent() {
        let _ = std::process::Command::new("screencapture")
            .args(["-x", "-m", dir.join("e2e-panel.png").to_str().unwrap()])
            .status();
    }
    option_up();
    sleep(Duration::from_millis(700));
    let after = front_wid();
    let committed = last_commit_wid();
    let ok = frame.is_some() && commit_count() == n0 + 1 && after == committed;
    pass &= ok;
    report(format!("CHECK hold+tab tab: panel={frame:?} focused={after:?} committed={committed:?} -> {ok}"));
    sleep(Duration::from_millis(300));
    let gone = panel_frame().is_none();
    pass &= gone;
    report(format!("CHECK panel hidden after release: {gone}"));

    // 4. Esc cancels: focus unchanged, no commit.
    let before = front_wid();
    let n0 = commit_count();
    option_down();
    tab();
    sleep(Duration::from_millis(400));
    key(ESC, true, CGEventFlags::MaskAlternate);
    key(ESC, false, CGEventFlags::MaskAlternate);
    sleep(Duration::from_millis(200));
    option_up();
    sleep(Duration::from_millis(500));
    let after = front_wid();
    let ok = commit_count() == n0 && after == before && panel_frame().is_none();
    pass &= ok;
    report(format!("CHECK esc-cancel: before={before:?} after={after:?} commits+{} -> {ok}", commit_count() - n0));

    // 5. Mouse: hold Option+Tab, click the first tile (its center, from ZTab's log) while Option is
    // held.
    let n0 = commit_count();
    option_down();
    tab();
    sleep(Duration::from_millis(500));
    let frame = panel_frame();
    let target = panel_tiles().into_iter().next();
    let mut clicked = false;
    if let (Some(_), Some((_, p))) = (frame, target) {
        mouse(CGEventType::MouseMoved, CGPoint { x: p.x + 3.0, y: p.y });
        mouse(CGEventType::MouseMoved, p);
        mouse(CGEventType::LeftMouseDown, p);
        mouse(CGEventType::LeftMouseUp, p);
        clicked = true;
    }
    sleep(Duration::from_millis(700));
    let after_click = front_wid();
    let committed = last_commit_wid();
    let panel_gone = panel_frame().is_none();
    option_up();
    sleep(Duration::from_millis(400));
    let ok = clicked && commit_count() == n0 + 1 && committed == target.map(|t| t.0) && after_click == committed && panel_gone;
    pass &= ok;
    report(format!(
        "CHECK click-while-holding: panel={frame:?} clicked={:?} focused={after_click:?} committed={committed:?} panel_gone_before_release={panel_gone} -> {ok}",
        target.map(|t| t.0)
    ));

    report(format!("RESULT {}", if pass { "PASS" } else { "FAIL" }));
}
