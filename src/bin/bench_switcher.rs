//! Benchmark driver for a running ZTab (needs Accessibility: bundle it with
//! `scripts/bundle.sh bench_switcher <dir>/ZTab.app` and run it with
//! `open -W -n -g <dir>/ZTab.app --args <out.txt> holds|cycles <n> [hold_ms] [all|same]`).
//!
//! - `holds <n> [hold_ms]`: n times ⌥ down, Tab, wait `hold_ms` (default 600) with the panel up,
//!   Esc, ⌥ up (nothing is switched), then reports the `open:` / `panel shown:` timings ZTab
//!   logged for those sessions: `open_to_show_ms` (includes the show delay), `show_ms`,
//!   `total_ms` and `tap_to_main_ms`.
//! - `cycles <n> [hold_ms]`: the same with a short hold (default 250 ms), for memory checks.
//!
//! The opener is ⌥Tab (`all`, the default) or ⌥` (`same`). Only keys ZTab swallows while its
//! switcher is open are posted (plus ⌥ itself); it checks first that ZTab's tap is installed.

use std::io::Write;
use std::thread::sleep;
use std::time::Duration;

use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{CGEvent, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation};

thread_local! {
    static SRC: CFRetained<CGEventSource> = CGEventSource::new(CGEventSourceStateID::HIDSystemState).unwrap();
}

const OPTION: u16 = 0x3A;
const TAB: u16 = 0x30;
const GRAVE: u16 = 0x32;
const ESC: u16 = 0x35;

fn key(code: u16, down: bool, flags: CGEventFlags) {
    let ev = SRC.with(|s| CGEvent::new_keyboard_event(Some(s), code, down)).unwrap();
    CGEvent::set_flags(Some(&ev), flags);
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
    sleep(Duration::from_millis(20));
}

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

fn num_after(line: &str, key: &str) -> Option<f64> {
    line.split(key).nth(1)?.split(|c: char| c != '.' && !c.is_ascii_digit()).next()?.parse().ok()
}

/// The newest "hotkey installed" line comes after the newest "starting pid=" line.
fn tap_installed() -> bool {
    let lines = log_since(0.0);
    let start = lines.iter().rposition(|(_, m)| m.starts_with("starting pid="));
    let tap = lines.iter().rposition(|(_, m)| m.starts_with("hotkey installed"));
    matches!((start, tap), (Some(s), Some(t)) if t > s)
}

fn stats(v: &mut [f64]) -> String {
    if v.is_empty() {
        return "n=0".into();
    }
    v.sort_by(f64::total_cmp);
    let pct = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
    format!("n={} min={:.1} median={:.1} p90={:.1} max={:.1}", v.len(), v[0], pct(0.5), pct(0.9), v[v.len() - 1])
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut out = std::fs::File::create(&args[1]).unwrap();
    let mode = args.get(2).map(String::as_str).unwrap_or("holds");
    let n: usize = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(5);
    let hold_ms: u64 = args.get(4).and_then(|v| v.parse().ok()).unwrap_or(if mode == "cycles" { 250 } else { 600 });
    let opener = if args.get(5).map(String::as_str) == Some("same") { GRAVE } else { TAB };
    let _activity = objc2_foundation::NSProcessInfo::processInfo().beginActivityWithOptions_reason(
        objc2_foundation::NSActivityOptions::UserInitiatedAllowingIdleSystemSleep | objc2_foundation::NSActivityOptions::LatencyCritical,
        &objc2_foundation::NSString::from_str("ZTab benchmark timings"),
    );
    if !tap_installed() {
        let _ = writeln!(out, "RESULT FAIL (ZTab's tap is not installed: nothing posted)");
        return;
    }
    let start = now_secs();
    let _ = writeln!(out, "mode={mode} n={n} hold_ms={hold_ms} opener={opener:#x} start={start:.3}");
    let opt = CGEventFlags::MaskAlternate;
    for _ in 0..n {
        key(OPTION, true, opt);
        key(opener, true, opt);
        key(opener, false, opt);
        sleep(Duration::from_millis(hold_ms));
        key(ESC, true, opt);
        key(ESC, false, opt);
        key(OPTION, false, CGEventFlags::empty());
        sleep(Duration::from_millis(if mode == "cycles" { 150 } else { 400 }));
    }
    sleep(Duration::from_millis(500));
    let lines = log_since(start);
    let (mut open_total, mut tap_ms, mut open_to_show, mut show_ms) = (vec![], vec![], vec![], vec![]);
    for (_, m) in &lines {
        if m.starts_with("open: ") {
            open_total.extend(num_after(m, "total_ms="));
            tap_ms.extend(num_after(m, "tap_to_main_ms="));
        } else if m.starts_with("panel shown:") {
            open_to_show.extend(num_after(m, "open_to_show_ms="));
            show_ms.extend(num_after(m, " show_ms="));
        }
    }
    let shown = open_to_show.len();
    let opens = open_total.len();
    let stalls: Vec<&String> = lines.iter().map(|(_, m)| m).filter(|m| m.contains("main thread stall") || m.contains("still blocked")).collect();
    let _ = writeln!(out, "open: total_ms {}", stats(&mut open_total));
    let _ = writeln!(out, "open: tap_to_main_ms {}", stats(&mut tap_ms));
    let _ = writeln!(out, "panel shown: open_to_show_ms {}", stats(&mut open_to_show));
    let _ = writeln!(out, "panel shown: show_ms {}", stats(&mut show_ms));
    let _ = writeln!(out, "stalls during the run: {stalls:?}");
    let ok = opens == n && shown == n;
    let _ = writeln!(out, "RESULT {} (opens={opens} shown={shown} of {n})", if ok { "PASS" } else { "FAIL" });
}
