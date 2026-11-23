//! Spike for src/windows.rs.
//!   spike_windows <outfile> [--dump]        list windows + timing
//!   spike_windows <outfile> --focus <wid>   focus a window, then report the frontmost window/app
//!   spike_windows <outfile> --focus-nth <n> focus the n-th window in z order (0 = frontmost)
#[path = "../windows.rs"]
mod windows;

use std::fmt::Write as _;
use std::time::Instant;

use objc2_app_kit::NSWorkspace;
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFArray, CFString, CFType, kCFRunLoopDefaultMode, CFRunLoop};
use objc2_core_graphics::{CGWindowListCopyWindowInfo, CGWindowListOption, kCGWindowLayer, kCGWindowNumber, kCGWindowOwnerPID, kCGWindowOwnerName};

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn line(w: &windows::WinInfo) -> String {
    format!(
        "{{\"wid\":{},\"pid\":{},\"app\":\"{}\",\"bundle\":\"{}\",\"title\":\"{}\",\"min\":{},\"space\":{},\"hidden\":{},\"z\":{},\"ax\":{},\"bounds\":[{:.0},{:.0},{:.0},{:.0}]}}",
        w.wid, w.pid, esc(&w.app_name), esc(w.bundle_id.as_deref().unwrap_or("")), esc(&w.title),
        w.minimized, w.on_current_space, w.hidden_app, w.z_index, w.has_ax(),
        w.bounds.0, w.bounds.1, w.bounds.2, w.bounds.3
    )
}

/// First layer-0 on-screen window (wid, pid, owner).
fn frontmost_cg() -> Option<(u32, i32, String)> {
    let arr = CGWindowListCopyWindowInfo(CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements, 0)?;
    let arr: CFRetained<CFArray<CFDictionary<CFString, CFType>>> = unsafe { CFRetained::cast_unchecked(arr) };
    for d in arr.iter() {
        let g = |k: &CFString| unsafe { d.get_unchecked(k) };
        let layer = unsafe { g(kCGWindowLayer) }.and_then(|v| v.downcast_ref::<CFNumber>()?.as_i64()).unwrap_or(-1);
        if layer != 0 { continue; }
        let wid = unsafe { g(kCGWindowNumber) }.and_then(|v| v.downcast_ref::<CFNumber>()?.as_i64())? as u32;
        let pid = unsafe { g(kCGWindowOwnerPID) }.and_then(|v| v.downcast_ref::<CFNumber>()?.as_i64())? as i32;
        let owner = unsafe { g(kCGWindowOwnerName) }.and_then(|v| v.downcast_ref::<CFString>().map(|s| s.to_string())).unwrap_or_default();
        return Some((wid, pid, owner));
    }
    None
}

fn pump(seconds: f64) {
    CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, seconds, false);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let out_path = args.get(1).cloned().unwrap_or_else(|| "/dev/stdout".into());
    let mut out = String::new();
    writeln!(out, "pid={} AXIsProcessTrusted={} CGPreflightScreenCaptureAccess={}", std::process::id(), windows::ax_trusted(), windows::screen_recording_granted()).unwrap();

    let mode = args.get(2).map(String::as_str).unwrap_or("--dump");
    match mode {
        "--dump" => {
            let mut times = Vec::new();
            let mut list = Vec::new();
            for _ in 0..10 {
                let t = Instant::now();
                list = windows::list_windows();
                times.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            writeln!(out, "count={} timings_ms={:?}", list.len(), times.iter().map(|t| format!("{t:.2}")).collect::<Vec<_>>()).unwrap();
            for w in &list {
                writeln!(out, "{}", line(w)).unwrap();
            }
            writeln!(out, "frontmost_cg={:?}", frontmost_cg()).unwrap();
        }
        "--focus" | "--focus-nth" | "--focus-noax" => {
            let arg: usize = args.get(3).and_then(|s| s.parse().ok()).expect("need number");
            let list = windows::list_windows();
            let target = if mode != "--focus-nth" {
                list.iter().find(|w| w.wid as usize == arg)
            } else {
                list.iter().find(|w| w.z_index == arg)
            };
            match target {
                None => writeln!(out, "target not found in list ({} windows)", list.len()).unwrap(),
                Some(w) => {
                    writeln!(out, "before_front_cg={:?}", frontmost_cg()).unwrap();
                    writeln!(out, "target={}", line(w)).unwrap();
                    let t = Instant::now();
                    let ok = if mode == "--focus-noax" { windows::focus_window_without_ax(w) } else { windows::focus_window(w) };
                    writeln!(out, "focus_window={} took_ms={:.2}", ok, t.elapsed().as_secs_f64() * 1000.0).unwrap();
                    writeln!(out, "trace: {}", windows::last_focus_trace()).unwrap();
                    pump(0.3);
                    let front = frontmost_cg();
                    let app = NSWorkspace::sharedWorkspace().frontmostApplication();
                    let (fpid, fname) = app.map(|a| (a.processIdentifier(), a.localizedName().map(|s| s.to_string()).unwrap_or_default())).unwrap_or((0, String::new()));
                    writeln!(out, "after_front_cg={:?} frontmost_app=({}, {})", front, fpid, fname).unwrap();
                    let wid_ok = front.as_ref().map(|f| f.0) == Some(w.wid);
                    let focused = windows::focused_window_id();
                    writeln!(out, "ax_focused_wid={:?}", focused).unwrap();
                    writeln!(out, "RESULT front_wid_matches={} front_app_matches={} key_wid_matches={}", wid_ok, fpid == w.pid, focused == Some(w.wid)).unwrap();
                    // Re-list to show new z order head.
                    let l2 = windows::list_windows();
                    if let Some(first) = l2.iter().find(|x| x.z_index == 0) {
                        writeln!(out, "new_z0={}", line(first)).unwrap();
                    }
                }
            }
        }
        "--raw" => {
            let wids: Vec<i64> = args[3..].iter().filter_map(|s| s.parse().ok()).collect();
            let arr = CGWindowListCopyWindowInfo(CGWindowListOption::OptionAll, 0).unwrap();
            let arr: CFRetained<CFArray<CFDictionary<CFString, CFType>>> = unsafe { CFRetained::cast_unchecked(arr) };
            for d in arr.iter() {
                let wid = unsafe { d.get_unchecked(kCGWindowNumber) }.and_then(|v| v.downcast_ref::<CFNumber>()?.as_i64()).unwrap_or(0);
                if wids.contains(&wid) {
                    writeln!(out, "{d:?}").unwrap();
                }
            }
        }
        "--hide" => {
            let pid: i32 = args[3].parse().unwrap();
            let app = objc2_app_kit::NSRunningApplication::runningApplicationWithProcessIdentifier(pid).unwrap();
            writeln!(out, "hide={}", app.hide()).unwrap();
            pump(0.5);
            for w in windows::list_windows().iter().filter(|w| w.pid == pid) {
                writeln!(out, "{}", line(w)).unwrap();
            }
        }
        "--token" => {
            for w in windows::list_windows() {
                let (found, ms) = windows::debug_token_lookup(w.pid, w.wid);
                writeln!(out, "token wid={} app={} found={} ms={:.2}", w.wid, w.app_name, found, ms).unwrap();
            }
        }
        "--debug" => {
            for _ in 0..3 { for l in windows::debug_ax_timing() { writeln!(out, "{l}").unwrap(); } }
            for l in windows::debug_candidates() {
                writeln!(out, "{l}").unwrap();
            }
        }
        _ => writeln!(out, "unknown mode {mode}").unwrap(),
    }
    std::fs::write(&out_path, out).expect("write out");
}
