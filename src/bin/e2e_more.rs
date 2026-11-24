//! More end-to-end checks against a running ZTab (needs Accessibility + an unlocked screen):
//!  1. every display: the panel opens on the screen under the mouse, and clicking a tile there
//!     switches to that window;
//!  2. a minimized window (scratch Finder window) is restored and focused when its tile is clicked;
//!  3. a window of a hidden app (scratch TextEdit document) is unhidden and focused;
//!  4. Shift+Option+Tab quick tap goes one step back: the last window of the switcher's order
//!     (with the user's windows, where that order and the MRU order may end alike);
//!     4b. Shift+Option+` from a current window, in a scratch layout where the two orders end
//!     differently: three scratch Finder windows, the middle one minimized, must preselect and
//!     switch to the minimized one (the last of the order), not the least recently used one;
//!  5. Option+` (same app) stays inside the frontmost app and goes to its next window (two scratch
//!     Finder windows: same-2 -> same-1 -> same-2).
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
const ESC: u16 = 0x35;

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
fn open_lines() -> Vec<String> {
    ztab_log().lines().filter(|l| l.contains("] open: ")).map(str::to_string).collect()
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
/// A list field of the last `open:` line (`order=` / `mru=`).
fn open_list(line: &str, key: &str) -> Vec<u32> {
    line.split(key)
        .nth(1)
        .and_then(|r| r.split_whitespace().next())
        .map(|r| r.split(',').filter_map(|w| w.parse().ok()).collect())
        .unwrap_or_default()
}
/// Whether `order` lists each app's windows together (owner pids from CoreGraphics; windows it
/// does not know any more are ignored).
fn grouped_by_app(order: &[u32]) -> bool {
    let pids: std::collections::HashMap<u32, i32> =
        windows::cg_describe(order).iter().filter_map(|d| windows::parse_cg(d)).map(|f| (f.0, f.1)).collect();
    let mut seen = std::collections::HashSet::new();
    let mut last = None;
    for p in order.iter().filter_map(|w| pids.get(w)) {
        if last != Some(*p) && !seen.insert(*p) {
            return false;
        }
        last = Some(*p);
    }
    true
}
/// Wait (up to `secs`) for `f` to return something.
fn wait_for<T>(secs: f64, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let end = std::time::Instant::now() + Duration::from_secs_f64(secs);
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        if std::time::Instant::now() >= end {
            return None;
        }
        let _ = CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.1, false);
    }
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

    // 4. Shift+Option+Tab quick tap: one step back from the current window, i.e. the last window
    // of the switcher's order (`order=` on ZTab's `open:` line: the least recently used app's
    // last window). The order must keep each app's windows together, and focus must land there.
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
    let order = open_list(&opened, " order=");
    let mru_last = open_list(&opened, " mru=").last().copied();
    let expected = order.last().copied();
    let grouped = grouped_by_app(&order);
    let front = front_wid();
    report(
        order.len() >= 3 && opened.contains("reverse=true") && grouped && committed.is_some() && committed == expected && front == committed,
        format!(
            "shift+option+tab: {} windows, grouped by app={grouped}, expected the last one={expected:?} committed={committed:?} front={front:?} \
             (the MRU order ends with {mru_last:?}: tells the rule from the least recently used window={})",
            order.len(),
            mru_last != expected
        ),
    );

    // 4b. Shift+Option+` from a current window, where the switcher's order and the MRU order end
    // differently. Scratch Finder windows rev-a, rev-b, rev-c, each activated with `open -a` and
    // kept in front past ZTab's settle time (MRU rev-c, rev-b, rev-a), then rev-b minimized while
    // rev-c stays in front. Finder's order lists its minimized windows last (rev-c, rev-a, the
    // user's open Finder windows, rev-b), while the MRU order ends with rev-a or an older Finder
    // window of the user's: one step back must be rev-b. The session is held and released only
    // when rev-b is preselected (Esc otherwise), so it never switches to a window of the user's;
    // it is skipped when the user has a minimized Finder window (that one would come after rev-b).
    let user_min: Vec<u32> = windows::list_windows().into_iter().filter(|w| w.app_name == "Finder" && w.minimized).map(|w| w.wid).collect();
    let rev_names = ["ztab-e2e-rev-a", "ztab-e2e-rev-b", "ztab-e2e-rev-c"];
    if !user_min.is_empty() {
        report(true, format!("shift+option+` from a current window: SKIPPED, the user has minimized Finder windows {user_min:?}"));
    } else {
        let mut rev: Vec<Option<u32>> = Vec::new();
        for name in rev_names {
            let d = format!("{scratch}/{name}");
            let _ = std::fs::create_dir_all(&d);
            let _ = Command::new("open").args(["-a", "Finder", &d]).status();
            let w = wait_for(8.0, || windows::list_windows().into_iter().find(|w| w.app_name == "Finder" && w.title == name));
            if let Some(w) = &w {
                wait_for(4.0, || (front_wid() == Some(w.wid)).then_some(()));
            }
            sleep(Duration::from_millis(900));
            rev.push(w.map(|w| w.wid));
        }
        match rev[..] {
            [Some(ra), Some(rb), Some(rc)] => {
                osa("tell application \"Finder\" to set collapsed of window \"ztab-e2e-rev-b\" to true");
                let min = wait_for(4.0, || find_win(rb).filter(|w| w.minimized).map(|_| ())).is_some();
                sleep(Duration::from_millis(900));
                let before = front_wid();
                let (n_open, n0) = (open_lines().len(), commit_count());
                let shifted = CGEventFlags::MaskAlternate | CGEventFlags::MaskShift;
                option_down();
                key(SHIFT, true, shifted);
                key(GRAVE, true, shifted);
                key(GRAVE, false, shifted);
                let opened = wait_for(1.5, || {
                    let v = open_lines();
                    (v.len() > n_open).then(|| v[n_open].clone())
                })
                .unwrap_or_default();
                let sel = opened.split(" wid=").nth(1).and_then(|r| r.split(' ').next()?.parse::<u32>().ok());
                let go = sel == Some(rb);
                if !go {
                    key(ESC, true, shifted);
                    key(ESC, false, shifted);
                }
                key(SHIFT, false, CGEventFlags::MaskAlternate);
                option_up();
                let front = wait_for(3.0, || (front_wid() == Some(rb)).then_some(rb)).or_else(front_wid);
                sleep(Duration::from_millis(300));
                let committed = (commit_count() > n0).then(last_commit_wid).flatten();
                let restored = find_win(rb).map(|w| !w.minimized);
                let order = open_list(&opened, " order=");
                let mru = open_list(&opened, " mru=");
                let layout_ok = opened.contains("current=Some(0)")
                    && opened.contains("reverse=true")
                    && opened.contains("same_app=true")
                    && order.first() == Some(&rc)
                    && order.get(1) == Some(&ra)
                    && order.last() == Some(&rb);
                let tells_apart = mru.last().is_some() && mru.last() != order.last();
                report(
                    min && before == Some(rc) && layout_ok && tells_apart && go && committed == Some(rb) && front == Some(rb) && restored == Some(true),
                    format!(
                        "shift+option+` from a current window: rev-a={ra} rev-b={rb} (minimized={min}) rev-c={rc} before={before:?} \
                         order={order:?} mru={mru:?} (current first, rev-b last={layout_ok}; MRU ends elsewhere={tells_apart}) \
                         preselected={sel:?} committed={committed:?} front={front:?} rev-b restored={restored:?}"
                    ),
                );
            }
            _ => report(false, format!("shift+option+` from a current window: scratch Finder windows did not appear: {rev:?}")),
        }
        for name in rev_names {
            osa(&format!("tell application \"Finder\" to close window \"{name}\""));
            let _ = std::fs::remove_dir_all(format!("{scratch}/{name}"));
        }
        sleep(Duration::from_millis(500));
    }

    // 5. Option+` with Finder frontmost: only Finder windows, and the same app's next window. The
    // suite opens two scratch Finder windows itself (it must not depend on the user having one
    // open), each activated with `open -a` and kept in front past ZTab's settle time so both are
    // in its MRU history: from same-2, ⌥` goes to same-1, a second ⌥` back to same-2.
    let same_dirs: Vec<String> = ["ztab-e2e-same-1", "ztab-e2e-same-2"].iter().map(|n| format!("{scratch}/{n}")).collect();
    let mut same: Vec<Option<u32>> = Vec::new();
    for d in &same_dirs {
        let _ = std::fs::create_dir_all(d);
        let name = std::path::Path::new(d).file_name().unwrap().to_string_lossy().to_string();
        let _ = Command::new("open").args(["-a", "Finder", d]).status();
        let w = wait_for(8.0, || windows::list_windows().into_iter().find(|w| w.app_name == "Finder" && w.title == name));
        if let Some(w) = &w {
            wait_for(4.0, || (front_wid() == Some(w.wid)).then_some(()));
        }
        sleep(Duration::from_millis(900));
        same.push(w.map(|w| w.wid));
    }
    let finder: Vec<u32> = windows::list_windows().into_iter().filter(|w| w.app_name == "Finder").map(|w| w.wid).collect();
    let before = front_wid();
    let grave = || {
        let n0 = commit_count();
        option_down();
        key(GRAVE, true, CGEventFlags::MaskAlternate);
        key(GRAVE, false, CGEventFlags::MaskAlternate);
        option_up();
        sleep(Duration::from_millis(900));
        let committed = (commit_count() > n0).then(last_commit_wid).flatten();
        let opened = ztab_log().lines().rev().find(|l| l.contains("open:")).unwrap_or("").to_string();
        (committed, opened, front_wid())
    };
    let (c1, opened, f1) = grave();
    let count_ok = opened.contains(&format!("open: {} windows", finder.len())) && opened.contains("same_app=true");
    let (c2, _, f2) = grave();
    let (s1, s2) = (same[0], same[1]);
    let ok = s1.is_some() && s2.is_some() && before == s2 && count_ok && c1 == s1 && f1 == s1 && c2 == s2 && f2 == s2;
    report(
        ok,
        format!(
            "option+` in Finder: finder_windows={} same-1={s1:?} same-2={s2:?} before={before:?} tap1 committed={c1:?} front={f1:?}; tap2 committed={c2:?} front={f2:?} log_ok={count_ok}",
            finder.len()
        ),
    );
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
