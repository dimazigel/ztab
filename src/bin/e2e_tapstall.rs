//! End-to-end check that a busy main thread in ZTab no longer holds the keyboard: run against a
//! ZTab started with `--inject-stall-ms <n> --inject-stall-at-s <s>` (n >= 1000). When ZTab logs
//! that it is injecting the stall, this posts ⌥ down, Tab, Esc, ⌥ up (tagged synthetic HID
//! events; ZTab swallows Tab and Esc, so nothing but the two Option flag changes reaches an app)
//! and checks, with a listen-only session tap downstream of ZTab's, that:
//! - the Option flag changes went through while ZTab's main thread was still blocked (the tap is
//!   serviced on its own thread, so keyboard events are never held by it);
//! - Tab and Esc were swallowed;
//! - ZTab's tap took the Open during the stall (`open: … tap_to_main_ms=…`), and the session was
//!   cancelled afterwards (no switch).
//!
//! Usage (bundled like the other suites): `e2e_tapstall <out.txt>`.

use std::ffi::c_void;
use std::io::Write;
use std::ptr::NonNull;
use std::sync::Mutex;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use objc2_core_foundation::{CFMachPort, CFRetained, CFRunLoop, kCFRunLoopCommonModes};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType,
};

const MAGIC: i64 = 0x5A7AC;
const OPTION: u16 = 0x3A;
const TAB: u16 = 0x30;
const ESC: u16 = 0x35;

/// Tagged events that reached the sentinel: (unix time, type, keycode).
static SEEN: Mutex<Vec<(f64, u32, u16)>> = Mutex::new(Vec::new());

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

unsafe extern "C-unwind" fn sentinel(_p: CGEventTapProxy, ty: CGEventType, event: NonNull<CGEvent>, _u: *mut c_void) -> *mut CGEvent {
    // SAFETY: valid event for the duration of the callback.
    let ev = unsafe { event.as_ref() };
    if CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceUserData) == MAGIC {
        let code = CGEvent::integer_value_field(Some(ev), CGEventField::KeyboardEventKeycode) as u16;
        SEEN.lock().unwrap().push((now(), ty.0, code));
    }
    event.as_ptr()
}

fn post(src: &CGEventSource, code: u16, down: bool, flags: CGEventFlags) -> f64 {
    let ev: CFRetained<CGEvent> = CGEvent::new_keyboard_event(Some(src), code, down).unwrap();
    CGEvent::set_flags(Some(&ev), flags);
    CGEvent::set_integer_value_field(Some(&ev), CGEventField::EventSourceUserData, MAGIC);
    let t = now();
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
    sleep(Duration::from_millis(30));
    t
}

/// ZTab log lines (unix time, message) at or after `since`.
fn ztab_lines(since: f64) -> Vec<(f64, String)> {
    let path = std::env::var("HOME").unwrap_or_default() + "/Library/Logs/ztab.log";
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .filter_map(|l| {
            let rest = l.strip_prefix('[')?;
            let (ts, msg) = rest.split_once("] ")?;
            let ts: f64 = ts.parse().ok()?;
            (ts >= since).then(|| (ts, msg.to_string()))
        })
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let out_path = args.get(1).cloned().unwrap_or_else(|| "e2e_tapstall.txt".into());
    let mut out = std::fs::File::create(&out_path).unwrap();
    let mut pass = true;
    let mut check = |out: &mut std::fs::File, name: &str, ok: bool, detail: String| {
        pass &= ok;
        let _ = writeln!(out, "CHECK {name}: {ok} ({detail})");
    };
    let start = now() - 1.0;

    // Listen-only sentinel downstream of ZTab's HID tap, on its own thread.
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
        let src = CFMachPort::new_run_loop_source(None, Some(&tap), 0).unwrap();
        // SAFETY: reading an immutable CF constant.
        CFRunLoop::current().unwrap().add_source(Some(&src), unsafe { kCFRunLoopCommonModes });
        CGEvent::tap_enable(&tap, true);
        CFRunLoop::run();
    });

    // Wait for ZTab to start its injected stall.
    let deadline = Instant::now() + Duration::from_secs(90);
    let injected = loop {
        if let Some((ts, msg)) = ztab_lines(start).into_iter().find(|(_, m)| m.starts_with("injecting a ")) {
            break Some((ts, msg));
        }
        if Instant::now() > deadline {
            break None;
        }
        sleep(Duration::from_millis(5));
    };
    let Some((inject_ts, inject_msg)) = injected else {
        let _ = writeln!(out, "no 'injecting a … main-thread stall' line within 90 s (start ZTab with --inject-stall-ms)");
        let _ = writeln!(out, "RESULT FAIL");
        return;
    };
    let stall_ms: f64 = inject_msg.trim_start_matches("injecting a ").split(' ').next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    sleep(Duration::from_millis(200));

    let src = CGEventSource::new(CGEventSourceStateID::HIDSystemState).unwrap();
    let t_opt_down = post(&src, OPTION, true, CGEventFlags::MaskAlternate);
    post(&src, TAB, true, CGEventFlags::MaskAlternate);
    post(&src, TAB, false, CGEventFlags::MaskAlternate);
    post(&src, ESC, true, CGEventFlags::MaskAlternate);
    post(&src, ESC, false, CGEventFlags::MaskAlternate);
    let t_opt_up = post(&src, OPTION, false, CGEventFlags::empty());
    let _ = writeln!(out, "stall {stall_ms} ms injected at {inject_ts:.3}; posted at {t_opt_down:.3}..{t_opt_up:.3}");

    // Let the stall end and ZTab catch up.
    sleep(Duration::from_millis((stall_ms as u64).saturating_sub(200) + 1500));
    let lines = ztab_lines(inject_ts);
    let stall_end = lines.iter().find(|(_, m)| m.starts_with("main thread stall: ") && m.contains("run loop")).map(|(t, _)| *t);
    let seen = SEEN.lock().unwrap().clone();
    let _ = writeln!(out, "sentinel saw {:?}", seen.iter().map(|(t, ty, c)| format!("{:.3}:{ty}:{c:#x}", t)).collect::<Vec<_>>());
    let _ = writeln!(out, "stall ended at {stall_end:?}");
    for (t, m) in lines.iter().take(12) {
        let _ = writeln!(out, "  ztab [{t:.3}] {}", m.chars().take(200).collect::<String>());
    }

    let flags: Vec<f64> = seen.iter().filter(|(_, ty, _)| *ty == CGEventType::FlagsChanged.0).map(|(t, ..)| *t).collect();
    let end = stall_end.unwrap_or(f64::MAX);
    check(
        &mut out,
        "both Option flag changes passed during the stall, within 100 ms",
        flags.len() == 2 && flags[0] - t_opt_down < 0.1 && flags[1] - t_opt_up < 0.1 && flags[1] < end,
        format!("flags at {flags:?}, posted {t_opt_down:.3} / {t_opt_up:.3}, stall end {end:.3}"),
    );
    let keys = seen.iter().filter(|(_, ty, _)| *ty != CGEventType::FlagsChanged.0).count();
    check(&mut out, "Tab and Esc swallowed by ZTab", keys == 0, format!("{keys} key events got past ZTab"));
    let open = lines.iter().find(|(_, m)| m.starts_with("open: ") && m.contains("tap_to_main_ms="));
    let tap_at = open.and_then(|(t, m)| Some(*t - m.split("tap_to_main_ms=").nth(1)?.split(' ').next()?.parse::<f64>().ok()? / 1000.0));
    check(
        &mut out,
        "the tap took the Open during the stall",
        tap_at.is_some_and(|t| t >= inject_ts && t < end),
        format!("tap at {tap_at:?}, stall {inject_ts:.3}..{end:.3}"),
    );
    let cancelled = lines.iter().any(|(_, m)| m == "cancel");
    let committed = lines.iter().any(|(_, m)| m.starts_with("commit: "));
    check(&mut out, "session cancelled, no switch", cancelled && !committed, format!("cancel={cancelled} commit={committed}"));
    let _ = writeln!(out, "RESULT {}", if pass { "PASS" } else { "FAIL" });
}
