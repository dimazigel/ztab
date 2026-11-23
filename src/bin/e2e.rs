//! End-to-end check against a running ZTab: posts real (synthetic HID) key/mouse events and
//! verifies which window ends up focused. Must run with Accessibility (bundle it with
//! `scripts/bundle.sh e2e dist/e2e/ZTab.app` and `open -W -n dist/e2e/ZTab.app --args <out>`).
//! Requires an unlocked screen and a running ZTab (dist/ZTab.app or ~/Applications/ZTab.app).

#[path = "../windows.rs"]
#[allow(dead_code)]
mod windows;

use std::io::Write;
use std::thread::sleep;
use std::time::Duration;

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

/// Frame (x, y, w, h) of ZTab's switcher panel if it is currently shown (from ZTab's log).
fn panel_frame() -> Option<(f64, f64, f64, f64)> {
    let log = std::fs::read_to_string(std::env::var("HOME").unwrap() + "/Library/Logs/ztab.log").ok()?;
    let line = log.lines().rev().find(|l| l.contains("panel shown:") || l.contains("panel hidden"))?;
    let v: Vec<f64> = line.split("cg_frame=").nth(1)?.split_whitespace().filter_map(|x| x.parse().ok()).collect();
    (v.len() == 4).then(|| (v[0], v[1], v[2], v[3]))
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

    // 1. Quick Option+Tab: switches to another window without showing the panel.
    let before = front_wid();
    let n0 = commit_count();
    option_down();
    tab();
    option_up();
    sleep(Duration::from_millis(700));
    let after = front_wid();
    let committed = last_commit_wid();
    let ok = commit_count() == n0 + 1 && after.is_some() && after == committed && after != before;
    pass &= ok;
    report(format!("CHECK quick-tap: before={before:?} after={after:?} committed={committed:?} -> {ok}"));

    // 2. Two more quick taps: MRU toggling must alternate between the last two windows.
    let first = after;
    option_down();
    tab();
    option_up();
    sleep(Duration::from_millis(700));
    let second = front_wid();
    let second_commit = last_commit_wid();
    option_down();
    tab();
    option_up();
    sleep(Duration::from_millis(700));
    let third = front_wid();
    let ok = first.is_some() && second.is_some() && second == second_commit && second != first && third == first;
    pass &= ok;
    report(format!("CHECK MRU toggle: tap1={first:?} tap2={second:?} tap3={third:?} -> {ok}"));

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

    // 5. Mouse: hold Option+Tab, click the first tile while Option is held.
    let n0 = commit_count();
    option_down();
    tab();
    sleep(Duration::from_millis(500));
    let frame = panel_frame();
    let mut clicked = false;
    if let Some((x, y, _, _)) = frame {
        // First tile: panel padding + group padding + a bit into the thumbnail.
        let p = CGPoint { x: x + 60.0, y: y + 60.0 };
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
    let ok = clicked && commit_count() == n0 + 1 && after_click == committed && panel_gone && commit_count() == n0 + 1;
    pass &= ok;
    report(format!(
        "CHECK click-while-holding: panel={frame:?} focused={after_click:?} committed={committed:?} panel_gone_before_release={panel_gone} -> {ok}"
    ));

    report(format!("RESULT {}", if pass { "PASS" } else { "FAIL" }));
}
