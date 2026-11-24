//! Memory-check driver for a running ZTab: switcher open/close cycles plus window churn (needs
//! Accessibility; bundle it with `scripts/bundle.sh bench_churn <dir>/ZTab.app` and run it with
//! `open -W -n -g <dir>/ZTab.app --args <out.txt> <scratch-dir> <rounds> <cycles-per-round>`).
//!
//! Each round opens two scratch Finder windows (folders under `<scratch-dir>`) and one scratch
//! TextEdit document, lets ZTab see them, then holds ⌥Tab `cycles-per-round` times (250 ms with
//! the panel up, Esc), with a quick ⌥Tab tap every tenth cycle (a real switch, between the scratch
//! windows: the newest scratch window is in front), and closes the scratch windows again (the
//! document without saving). TextEdit is quit at the end only if this driver launched it. Only
//! scratch windows it created are opened or closed; only ⌥Tab-family keys are posted.

#[path = "../windows.rs"]
#[allow(dead_code)]
mod windows;

use std::io::Write;
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

use objc2_core_foundation::{CFRetained, CFRunLoop, kCFRunLoopDefaultMode};
use objc2_core_graphics::{CGEvent, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation};

thread_local! {
    static SRC: CFRetained<CGEventSource> = CGEventSource::new(CGEventSourceStateID::HIDSystemState).unwrap();
}

const OPTION: u16 = 0x3A;
const TAB: u16 = 0x30;
const ESC: u16 = 0x35;

fn key(code: u16, down: bool, flags: CGEventFlags) {
    let ev = SRC.with(|s| CGEvent::new_keyboard_event(Some(s), code, down)).unwrap();
    CGEvent::set_flags(Some(&ev), flags);
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
    sleep(Duration::from_millis(20));
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
        pump(0.15);
    }
}

fn osa(script: &str) -> String {
    let o = Command::new("osascript").args(["-e", script]).output().unwrap();
    String::from_utf8_lossy(&o.stdout).trim().to_string() + String::from_utf8_lossy(&o.stderr).trim()
}

fn pids_of(name: &str) -> Vec<i32> {
    let o = Command::new("pgrep").args(["-x", name]).output().unwrap();
    String::from_utf8_lossy(&o.stdout).lines().filter_map(|l| l.trim().parse().ok()).collect()
}

fn open_finder(dir: &str) -> Option<u32> {
    let _ = std::fs::create_dir_all(dir);
    let name = std::path::Path::new(dir).file_name()?.to_string_lossy().to_string();
    let _ = Command::new("open").args(["-a", "Finder", dir]).status();
    wait_for(8.0, || windows::list_windows().into_iter().find(|w| w.app_name == "Finder" && w.title == name).map(|w| w.wid))
}

fn open_doc(path: &str) -> Option<u32> {
    if !std::path::Path::new(path).exists() {
        let _ = std::fs::write(path, "ZTab memory-check scratch document\n");
    }
    let stem = std::path::Path::new(path).file_stem()?.to_string_lossy().to_string();
    let _ = Command::new("open").args(["-a", "TextEdit", path]).status();
    wait_for(10.0, || windows::list_windows().into_iter().find(|w| w.app_name == "TextEdit" && w.title.starts_with(&stem)).map(|w| w.wid))
}

fn close_finder(dir: &str) {
    let name = std::path::Path::new(dir).file_name().unwrap().to_string_lossy().to_string();
    osa(&format!(
        "tell application \"Finder\"\nrepeat with w in (get every Finder window whose name is \"{name}\")\nclose w\nend repeat\nend tell"
    ));
}

fn close_scratch_finder_windows(scratch: &str) {
    osa(&format!(
        "tell application \"Finder\"\nrepeat with w in (get every Finder window)\ntry\nif POSIX path of (target of w as alias) starts with \"{scratch}\" then close w\nend try\nend repeat\nend tell"
    ));
}

fn close_doc(path: &str) {
    if pids_of("TextEdit").is_empty() {
        return;
    }
    let name = std::path::Path::new(path).file_name().unwrap().to_string_lossy().to_string();
    osa(&format!("tell application \"TextEdit\" to close (every document whose name is \"{name}\") saving no"));
}

fn hold_cycle() {
    let opt = CGEventFlags::MaskAlternate;
    key(OPTION, true, opt);
    key(TAB, true, opt);
    key(TAB, false, opt);
    sleep(Duration::from_millis(250));
    key(ESC, true, opt);
    key(ESC, false, opt);
    key(OPTION, false, CGEventFlags::empty());
    sleep(Duration::from_millis(150));
}

fn quick_tap() {
    let opt = CGEventFlags::MaskAlternate;
    key(OPTION, true, opt);
    key(TAB, true, opt);
    key(TAB, false, opt);
    key(OPTION, false, CGEventFlags::empty());
    sleep(Duration::from_millis(300));
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut out = std::fs::File::create(&args[1]).unwrap();
    let scratch = args[2].clone();
    let rounds: usize = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(2);
    let per_round: usize = args.get(4).and_then(|v| v.parse().ok()).unwrap_or(25);
    let _activity = objc2_foundation::NSProcessInfo::processInfo().beginActivityWithOptions_reason(
        objc2_foundation::NSActivityOptions::UserInitiatedAllowingIdleSystemSleep | objc2_foundation::NSActivityOptions::LatencyCritical,
        &objc2_foundation::NSString::from_str("ZTab memory check"),
    );
    let te_was_running = !pids_of("TextEdit").is_empty();
    let t0 = Instant::now();
    let (mut cycles, mut taps, mut opened, mut failed) = (0usize, 0usize, 0usize, 0usize);
    for r in 0..rounds {
        let a = format!("{scratch}/churn-{r}-a");
        let b = format!("{scratch}/churn-{r}-b");
        let doc = format!("{scratch}/churn-{r}.txt");
        let wa = open_finder(&a);
        pump(0.4);
        let wd = open_doc(&doc);
        pump(0.4);
        let wb = open_finder(&b);
        pump(1.2);
        for w in [wa, wd, wb] {
            if w.is_some() {
                opened += 1
            } else {
                failed += 1
            }
        }
        for c in 0..per_round {
            if c % 10 == 9 {
                quick_tap();
                taps += 1;
            } else {
                hold_cycle();
                cycles += 1;
            }
        }
        close_finder(&a);
        close_finder(&b);
        close_doc(&doc);
        pump(1.0);
        let _ = std::fs::remove_file(&doc);
    }
    close_scratch_finder_windows(&scratch);
    if !te_was_running {
        osa("tell application \"TextEdit\" to quit saving no");
    }
    for r in 0..rounds {
        let _ = std::fs::remove_dir(format!("{scratch}/churn-{r}-a"));
        let _ = std::fs::remove_dir(format!("{scratch}/churn-{r}-b"));
        let _ = std::fs::remove_file(format!("{scratch}/churn-{r}.txt"));
    }
    let _ = writeln!(
        out,
        "rounds={rounds} hold_cycles={cycles} quick_taps={taps} scratch_windows_opened={opened} failed_to_open={failed} took {:.0} s",
        t0.elapsed().as_secs_f64()
    );
    let _ = writeln!(out, "RESULT {}", if failed == 0 { "PASS" } else { "FAIL" });
}
