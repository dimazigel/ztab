//! More end-to-end checks against a running ZTab (needs Accessibility + an unlocked screen):
//!  1. every display: the panel opens on the screen under the mouse, and clicking a tile there
//!     switches to that window;
//!  2. a minimized window (scratch Finder window) is restored and focused when its tile is clicked;
//!  3. a window of a hidden app (scratch TextEdit document) is unhidden and focused;
//!  4. Shift+Option+Tab quick tap goes to the least recently used window;
//!  5. Option+` (same app) stays inside the frontmost app.
//! Usage: `open -W -n -g <bundle> --args <out.txt> <scratch-dir>`.

#[path = "../windows.rs"]
#[allow(dead_code)]
mod windows;

use std::io::Write;
use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

use objc2_app_kit::NSRunningApplication;
use objc2_core_foundation::{CFRetained, CFRunLoop, CGPoint, CGRect, kCFRunLoopDefaultMode};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGEvent, CGEventFlags, CGEventSource, CGEventSourceStateID,
    CGEventTapLocation, CGEventType, CGGetActiveDisplayList, CGMouseButton,
};

const OPTION: u16 = 0x3A;
const SHIFT: u16 = 0x38;
const TAB: u16 = 0x30;
const GRAVE: u16 = 0x32;

thread_local! {
    static SRC: CFRetained<CGEventSource> = CGEventSource::new(CGEventSourceStateID::HIDSystemState).unwrap();
}

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
fn mouse(kind: CGEventType, p: CGPoint) {
    let ev = SRC.with(|s| CGEvent::new_mouse_event(Some(s), kind, p, CGMouseButton::Left)).unwrap();
    CGEvent::set_flags(Some(&ev), CGEventFlags::MaskAlternate);
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
    sleep(Duration::from_millis(40));
}
fn click(p: CGPoint) {
    mouse(CGEventType::MouseMoved, CGPoint { x: p.x + 2.0, y: p.y });
    mouse(CGEventType::MouseMoved, p);
    mouse(CGEventType::LeftMouseDown, p);
    mouse(CGEventType::LeftMouseUp, p);
}

fn ztab_log() -> String {
    std::fs::read_to_string(std::env::var("HOME").unwrap() + "/Library/Logs/ztab.log").unwrap_or_default()
}
fn commit_count() -> usize {
    ztab_log().matches("commit: wid=").count()
}
fn last_commit_wid() -> Option<u32> {
    let log = ztab_log();
    let line = log.lines().rev().find(|l| l.contains("commit: wid="))?;
    line.split("commit: wid=").nth(1)?.split(' ').next()?.parse().ok()
}
/// Panel frame and tile centers (wid -> point) of the currently shown panel, from ZTab's log.
fn panel() -> Option<((f64, f64, f64, f64), Vec<(u32, CGPoint)>)> {
    let log = ztab_log();
    let line = log.lines().rev().find(|l| l.contains("panel shown:") || l.contains("panel hidden"))?;
    let rest = line.split("cg_frame=").nth(1)?;
    let v: Vec<f64> = rest.split_whitespace().take(4).filter_map(|x| x.parse().ok()).collect();
    if v.len() != 4 {
        return None;
    }
    let tiles = rest
        .split("tiles=")
        .nth(1)
        .unwrap_or("")
        .split(';')
        .filter_map(|t| {
            // `wid:x,y:t` (t = 1 when the tile had a thumbnail when shown); older logs had no `:t`.
            let mut parts = t.trim().split(':');
            let wid = parts.next()?;
            let (x, y) = parts.next()?.split_once(',')?;
            Some((wid.parse().ok()?, CGPoint { x: x.parse().ok()?, y: y.trim().parse().ok()? }))
        })
        .collect();
    Some(((v[0], v[1], v[2], v[3]), tiles))
}
fn front_wid() -> Option<u32> {
    windows::list_windows().into_iter().min_by_key(|w| w.z_index).map(|w| w.wid)
}
fn find_win(wid: u32) -> Option<windows::WinInfo> {
    windows::list_windows().into_iter().find(|w| w.wid == wid)
}
fn osa(script: &str) -> String {
    let o = Command::new("osascript").args(["-e", script]).output().unwrap();
    String::from_utf8_lossy(&o.stdout).trim().to_string() + &String::from_utf8_lossy(&o.stderr).trim().to_string()
}
fn displays() -> Vec<CGRect> {
    let mut ids = [0 as CGDirectDisplayID; 16];
    let mut n = 0u32;
    unsafe { CGGetActiveDisplayList(16, ids.as_mut_ptr(), &mut n) };
    ids[..n as usize].iter().map(|&id| CGDisplayBounds(id)).collect()
}
fn inside(r: &CGRect, f: (f64, f64, f64, f64)) -> bool {
    f.0 >= r.origin.x - 1.0
        && f.1 >= r.origin.y - 1.0
        && f.0 + f.2 <= r.origin.x + r.size.width + 1.0
        && f.1 + f.3 <= r.origin.y + r.size.height + 1.0
}

/// Hold Option+Tab, click the tile of `wid`, release. Returns (clicked, committed wid, front wid).
fn click_tile_of(wid: u32) -> (bool, Option<u32>, Option<u32>) {
    let n0 = commit_count();
    option_down();
    tab();
    sleep(Duration::from_millis(500));
    let target = panel().and_then(|(_, tiles)| tiles.into_iter().find(|(w, _)| *w == wid).map(|(_, p)| p));
    if let Some(p) = target {
        click(p);
    }
    sleep(Duration::from_millis(900));
    option_up();
    sleep(Duration::from_millis(300));
    let committed = (commit_count() > n0).then(last_commit_wid).flatten();
    (target.is_some(), committed, front_wid())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut out = std::fs::File::create(&args[1]).unwrap();
    let scratch = args.get(2).cloned().unwrap_or_else(|| "/tmp".into());
    let mut pass = true;
    let mut report = |ok: bool, s: String| {
        pass &= ok;
        let _ = writeln!(out, "CHECK {s} -> {ok}");
    };
    let original_mouse = CGEvent::new(None).map(|e| CGEvent::location(Some(&e)));

    // 1. Each display.
    let ds = displays();
    for (i, d) in ds.iter().enumerate() {
        let c = CGPoint { x: d.origin.x + d.size.width / 2.0, y: d.origin.y + d.size.height / 2.0 };
        mouse(CGEventType::MouseMoved, c);
        sleep(Duration::from_millis(200));
        let before = front_wid();
        let n0 = commit_count();
        option_down();
        tab();
        sleep(Duration::from_millis(500));
        let p = panel();
        let frame_ok = p.as_ref().map(|(f, _)| inside(d, *f)).unwrap_or(false);
        // Click a tile that is not the current window.
        let target = p.as_ref().and_then(|(_, t)| t.iter().find(|(w, _)| Some(*w) != before).cloned());
        if let Some((_, pt)) = target {
            click(pt);
        }
        sleep(Duration::from_millis(900));
        option_up();
        sleep(Duration::from_millis(300));
        let committed = (commit_count() > n0).then(last_commit_wid).flatten();
        let after = front_wid();
        let ok = frame_ok && target.is_some() && committed == target.map(|t| t.0) && after == committed;
        report(
            ok,
            format!(
                "display {i} {:?}: panel={:?} inside={frame_ok} clicked={:?} committed={committed:?} front={after:?}",
                (d.origin.x, d.origin.y, d.size.width, d.size.height),
                p.map(|x| x.0),
                target.map(|t| t.0)
            ),
        );
    }
    if let Some(p) = original_mouse {
        mouse(CGEventType::MouseMoved, p);
    }

    // 2. Minimized window: a scratch Finder window, minimized, then picked by clicking its tile.
    let dir = format!("{scratch}/ztab-e2e-min");
    let _ = std::fs::create_dir_all(&dir);
    let before: Vec<u32> = windows::list_windows().iter().map(|w| w.wid).collect();
    osa(&format!("tell application \"Finder\" to make new Finder window to (POSIX file \"{dir}\" as alias)"));
    sleep(Duration::from_millis(800));
    let new_win = windows::list_windows().into_iter().find(|w| !before.contains(&w.wid) && w.app_name == "Finder");
    match new_win {
        None => report(false, "minimized: scratch Finder window did not appear".into()),
        Some(w) => {
            osa("tell application \"Finder\" to set collapsed of window \"ztab-e2e-min\" to true");
            sleep(Duration::from_millis(900));
            let was_min = find_win(w.wid).map(|x| x.minimized).unwrap_or(false);
            let (clicked, committed, front) = click_tile_of(w.wid);
            sleep(Duration::from_millis(600));
            let now = find_win(w.wid);
            let restored = now.as_ref().map(|x| !x.minimized).unwrap_or(false);
            let ok = was_min && clicked && committed == Some(w.wid) && restored && front_wid() == Some(w.wid);
            report(ok, format!("minimized window {}: was_min={was_min} clicked={clicked} committed={committed:?} restored={restored} front={front:?}", w.wid));
            osa("tell application \"Finder\" to close window \"ztab-e2e-min\"");
        }
    }

    // 3. Hidden app: a scratch TextEdit document, app hidden, then picked by clicking its tile.
    let textedit_was_running = osa("application \"TextEdit\" is running") == "true";
    let doc = format!("{scratch}/ztab-e2e-hidden.txt");
    let _ = std::fs::write(&doc, "ZTab e2e scratch document\n");
    let before: Vec<u32> = windows::list_windows().iter().map(|w| w.wid).collect();
    let _ = Command::new("open").args(["-a", "TextEdit", &doc]).status();
    let mut new_win = None;
    for _ in 0..40 {
        // Pump the run loop so NSWorkspace's running-app list picks up the new TextEdit.
        let _ = CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.2, false);
        new_win = windows::list_windows().into_iter().find(|w| !before.contains(&w.wid) && w.app_name == "TextEdit");
        if new_win.is_some() {
            break;
        }
    }
    sleep(Duration::from_millis(500));
    match new_win {
        None => report(false, "hidden app: scratch TextEdit window did not appear".into()),
        Some(w) => {
            if let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(w.pid) {
                app.hide();
            }
            let _ = CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.9, false);
            let hidden = NSRunningApplication::runningApplicationWithProcessIdentifier(w.pid)
                .map(|a| a.isHidden())
                .unwrap_or(false)
                && find_win(w.wid).map(|x| x.hidden_app).unwrap_or(false);
            let (clicked, committed, _) = click_tile_of(w.wid);
            let _ = CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.6, false);
            let unhidden = NSRunningApplication::runningApplicationWithProcessIdentifier(w.pid)
                .map(|a| !a.isHidden())
                .unwrap_or(false);
            let front = front_wid();
            let ok = hidden && clicked && committed == Some(w.wid) && unhidden && front == Some(w.wid);
            report(ok, format!("hidden-app window {}: hidden={hidden} clicked={clicked} committed={committed:?} unhidden={unhidden} front={front:?}", w.wid));
            osa("tell application \"TextEdit\" to close (every document whose name is \"ztab-e2e-hidden.txt\") saving no");
            if !textedit_was_running {
                osa("tell application \"TextEdit\" to quit");
            }
        }
    }
    let _ = std::fs::remove_file(&doc);
    let _ = std::fs::remove_dir_all(&dir);
    sleep(Duration::from_millis(800));

    // 4. Shift+Option+Tab quick tap: the least recently used window, i.e. the tail of the MRU
    // order ZTab logs on its `open:` line. It must not be one of the windows this suite just
    // used (the MRU head), and focus must land on it.
    let n0 = commit_count();
    option_down();
    key(SHIFT, true, CGEventFlags::MaskAlternate | CGEventFlags::MaskShift);
    key(TAB, true, CGEventFlags::MaskAlternate | CGEventFlags::MaskShift);
    key(TAB, false, CGEventFlags::MaskAlternate | CGEventFlags::MaskShift);
    key(SHIFT, false, CGEventFlags::MaskAlternate);
    option_up();
    sleep(Duration::from_millis(900));
    let committed = (commit_count() > n0).then(last_commit_wid).flatten();
    let opened = ztab_log().lines().rev().find(|l| l.contains("open:")).unwrap_or("").to_string();
    let mru: Vec<u32> = opened
        .split(" mru=")
        .nth(1)
        .and_then(|r| r.split_whitespace().next())
        .map(|r| r.split(',').filter_map(|w| w.parse().ok()).collect())
        .unwrap_or_default();
    let expected = mru.last().copied();
    let head: Vec<u32> = mru.iter().take(2).copied().collect();
    let front = front_wid();
    report(
        mru.len() >= 3 && opened.contains("reverse=true") && committed.is_some() && committed == expected && !head.contains(&expected.unwrap_or(0)) && front == committed,
        format!("shift+option+tab: mru_len={} expected least-recent={expected:?} committed={committed:?} front={front:?}", mru.len()),
    );

    // 5. Option+` with Finder frontmost: only Finder windows, switch stays in Finder. The suite
    // opens two scratch Finder windows itself (it must not depend on the user having one open).
    let same_dirs: Vec<String> = ["ztab-e2e-same-1", "ztab-e2e-same-2"].iter().map(|n| format!("{scratch}/{n}")).collect();
    for d in &same_dirs {
        let _ = std::fs::create_dir_all(d);
        osa(&format!("tell application \"Finder\" to make new Finder window to (POSIX file \"{d}\" as alias)"));
        sleep(Duration::from_millis(400));
    }
    osa("tell application \"Finder\" to activate");
    sleep(Duration::from_millis(1000));
    let finder: Vec<u32> = windows::list_windows().into_iter().filter(|w| w.app_name == "Finder").map(|w| w.wid).collect();
    let before = front_wid();
    let n0 = commit_count();
    option_down();
    key(GRAVE, true, CGEventFlags::MaskAlternate);
    key(GRAVE, false, CGEventFlags::MaskAlternate);
    option_up();
    sleep(Duration::from_millis(900));
    let committed = (commit_count() > n0).then(last_commit_wid).flatten();
    let opened = ztab_log().lines().rev().find(|l| l.contains("open:")).unwrap_or("").to_string();
    let count_ok = opened.contains(&format!("open: {} windows", finder.len())) && opened.contains("same_app=true");
    let ok = finder.len() >= 2 && count_ok && committed.map(|c| finder.contains(&c)).unwrap_or(false) && committed != before;
    report(ok, format!("option+` in Finder: finder_windows={} before={before:?} committed={committed:?} log_ok={count_ok}", finder.len()));
    for d in &same_dirs {
        let name = std::path::Path::new(d).file_name().unwrap().to_string_lossy().to_string();
        osa(&format!("tell application \"Finder\" to close window \"{name}\""));
        let _ = std::fs::remove_dir_all(d);
    }

    // Safety net: close every Finder window still showing the scratch folder or one inside it.
    osa(&format!(
        "tell application \"Finder\"\nrepeat with w in (get every Finder window)\ntry\nif POSIX path of (target of w as alias) starts with \"{scratch}\" then close w\nend try\nend repeat\nend tell"
    ));

    let _ = writeln!(out, "RESULT {}", if pass { "PASS" } else { "FAIL" });
}
