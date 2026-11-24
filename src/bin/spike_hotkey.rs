//! Spike for src/hotkey.rs.
//!
//! Usage: spike_hotkey <outfile> [seconds=6] [--no-post]
//!
//! Installs the Option+Tab tap, plus a downstream "sentinel" tap (session, tail-append, active) that
//! records every synthetic event that got past the hotkey tap and swallows it so nothing leaks into
//! the user's focused app or AltTab. A background thread posts tagged synthetic key sequences via
//! CGEventPost(kCGHIDEventTap). At the end the spike checks the observed events against expectations.
#[path = "../hotkey.rs"]
mod hotkey;

use std::ffi::c_void;
use std::io::Write;
use std::ptr::NonNull;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hotkey::{Hotkey, HotkeyEvent, HotkeyHandle, keycode};
use objc2_core_foundation::{CFMachPort, CFRunLoop, kCFRunLoopCommonModes, kCFRunLoopDefaultMode};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventTapProxy, CGEventType, CGPreflightScreenCaptureAccess,
};

const MAGIC: i64 = 0x5A7AB;

static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());
static HOTKEY_EVENTS: Mutex<Vec<HotkeyEvent>> = Mutex::new(Vec::new());
/// (type, keycode, flags, autorepeat) of tagged events that reached the sentinel.
static SENTINEL: Mutex<Vec<(u32, u16, u64, bool)>> = Mutex::new(Vec::new());

fn log(s: impl Into<String>) {
    let s = s.into();
    eprintln!("{s}");
    LOG.lock().unwrap().push(s);
}

fn type_name(t: u32) -> &'static str {
    match t {
        10 => "keyDown",
        11 => "keyUp",
        12 => "flagsChanged",
        _ => "other",
    }
}

unsafe extern "C-unwind" fn sentinel_cb(
    _proxy: CGEventTapProxy,
    ty: CGEventType,
    event: NonNull<CGEvent>,
    _user_info: *mut c_void,
) -> *mut CGEvent {
    if ty != CGEventType::KeyDown && ty != CGEventType::KeyUp && ty != CGEventType::FlagsChanged {
        return event.as_ptr();
    }
    let ev = unsafe { event.as_ref() };
    if CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceUserData) != MAGIC {
        return event.as_ptr(); // real user input: untouched
    }
    let code = CGEvent::integer_value_field(Some(ev), CGEventField::KeyboardEventKeycode) as u16;
    let flags = CGEvent::flags(Some(ev)).0;
    let rep = CGEvent::integer_value_field(Some(ev), CGEventField::KeyboardEventAutorepeat) != 0;
    log(format!(
        "  sentinel saw {} code=0x{code:02X} flags=0x{flags:X} autorepeat={rep}",
        type_name(ty.0)
    ));
    SENTINEL.lock().unwrap().push((ty.0, code, flags, rep));
    std::ptr::null_mut() // swallow tagged synthetic events so they never reach apps
}

fn post(code: u16, down: bool, flags: CGEventFlags, autorepeat: bool) {
    let ev = CGEvent::new_keyboard_event(None, code, down).expect("CGEventCreateKeyboardEvent");
    CGEvent::set_flags(Some(&ev), flags);
    CGEvent::set_integer_value_field(Some(&ev), CGEventField::EventSourceUserData, MAGIC);
    if autorepeat {
        CGEvent::set_integer_value_field(Some(&ev), CGEventField::KeyboardEventAutorepeat, 1);
    }
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
    std::thread::sleep(Duration::from_millis(40));
}

fn marker(name: &str) {
    if name.starts_with("S2") {
        *S2_START.lock().unwrap() =
            Some((HOTKEY_EVENTS.lock().unwrap().len(), SENTINEL.lock().unwrap().len()));
    }
    log(format!("-- poster: {name}"));
    std::thread::sleep(Duration::from_millis(150));
}

fn poster() {
    std::thread::sleep(Duration::from_millis(800));
    let alt = CGEventFlags::MaskAlternate;
    let alt_shift = CGEventFlags(alt.0 | CGEventFlags::MaskShift.0);
    let none = CGEventFlags(0);

    marker("A: Opt down, Tab, Tab, Shift+Tab, Opt up  => Open, Next, Prev, Commit");
    post(keycode::OPTION, true, alt, false);
    post(keycode::TAB, true, alt, false);
    post(keycode::TAB, false, alt, false);
    post(keycode::TAB, true, alt, false);
    post(keycode::TAB, false, alt, false);
    post(keycode::SHIFT, true, alt_shift, false);
    post(keycode::TAB, true, alt_shift, false);
    post(keycode::TAB, false, alt_shift, false);
    post(keycode::SHIFT, false, alt, false);
    post(keycode::OPTION, false, none, false);

    marker("B: plain Tab (no Option) => passes, nothing emitted");
    post(keycode::TAB, true, none, false);
    post(keycode::TAB, false, none, false);

    marker("C: Opt+Tab, Tab autorepeat x2, arrows, 'a', Esc, Opt up => Open, Next, Next, Up, Down, Prev, Next, Cancel");
    post(keycode::OPTION, true, alt, false);
    post(keycode::TAB, true, alt, false);
    post(keycode::TAB, true, alt, true);
    post(keycode::TAB, true, alt, true);
    post(keycode::TAB, false, alt, false);
    post(keycode::UP, true, alt, false);
    post(keycode::UP, false, alt, false);
    post(keycode::DOWN, true, alt, false);
    post(keycode::DOWN, false, alt, false);
    post(keycode::LEFT, true, alt, false);
    post(keycode::LEFT, false, alt, false);
    post(keycode::RIGHT, true, alt, false);
    post(keycode::RIGHT, false, alt, false);
    post(0x00, true, alt, false); // 'a' while open: swallowed, no event
    post(0x00, false, alt, false);
    post(keycode::ESCAPE, true, alt, false);
    post(keycode::ESCAPE, false, alt, false);
    post(keycode::OPTION, false, none, false);

    marker("D: Opt+` , Return, Opt up => OpenSameApp, Commit (Option up after Return: no 2nd Commit)");
    post(keycode::OPTION, true, alt, false);
    post(keycode::GRAVE, true, alt, false);
    post(keycode::GRAVE, false, alt, false);
    post(keycode::RETURN, true, alt, false);
    post(keycode::RETURN, false, alt, false);
    post(keycode::OPTION, false, none, false);

    marker("E: Cmd+Opt+Tab => passes (Command held)");
    let cmd_alt = CGEventFlags(alt.0 | CGEventFlags::MaskCommand.0);
    post(keycode::TAB, true, cmd_alt, false);
    post(keycode::TAB, false, cmd_alt, false);

    marker("F: Opt down, Shift down, Tab, Shift up, Opt up => Open{reverse:true}, Commit");
    post(keycode::OPTION, true, alt, false);
    post(keycode::SHIFT, true, alt_shift, false);
    post(keycode::TAB, true, alt_shift, false);
    post(keycode::TAB, false, alt_shift, false);
    post(keycode::SHIFT, false, alt, false);
    post(keycode::OPTION, false, none, false);
    marker("done");
}

/// Stall mode: the handler blocks ~STALL on the first Open so the OS disables the tap by timeout.
fn poster_stall() {
    std::thread::sleep(Duration::from_millis(800));
    let alt = CGEventFlags::MaskAlternate;
    let alt_shift = CGEventFlags(alt.0 | CGEventFlags::MaskShift.0);
    let none = CGEventFlags(0);
    marker("S1: Opt down, Tab down (handler stalls), Tab up, Opt up");
    post(keycode::OPTION, true, alt, false);
    post(keycode::TAB, true, alt, false);
    std::thread::sleep(Duration::from_millis(300));
    post(keycode::TAB, false, alt, false);
    post(keycode::OPTION, false, none, false);
    std::thread::sleep(Duration::from_millis(4000));
    marker("S2 (after stall): Opt down, Shift+Tab, Opt up => Open{reverse:true}, Commit; then plain Tab passes");
    post(keycode::OPTION, true, alt, false);
    post(keycode::SHIFT, true, alt_shift, false);
    post(keycode::TAB, true, alt_shift, false);
    post(keycode::TAB, false, alt_shift, false);
    post(keycode::SHIFT, false, alt, false);
    post(keycode::OPTION, false, none, false);
    post(keycode::TAB, true, none, false);
    post(keycode::TAB, false, none, false);
    marker("done");
}

static STALL_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static S2_START: Mutex<Option<(usize, usize)>> = Mutex::new(None);

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let out = args.get(1).cloned().unwrap_or_else(|| "spike_hotkey.out.txt".into());
    let secs: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(6.0);
    let no_post = args.iter().any(|a| a == "--no-post");
    let stall = args.iter().any(|a| a == "--stall");
    if stall {
        STALL_MS.store(2500, std::sync::atomic::Ordering::SeqCst);
    }
    let alttab = std::process::Command::new("/usr/bin/pgrep").args(["-x", "AltTab"]).output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().replace('\n', ","))
        .unwrap_or_default();
    log(format!("AltTab running: pid(s) [{alttab}]"));

    let ax = unsafe { objc2_application_services::AXIsProcessTrusted() };
    let sc = CGPreflightScreenCaptureAccess();
    log(format!("pid={} AXIsProcessTrusted={ax} CGPreflightScreenCaptureAccess={sc}", std::process::id()));

    static SLOT: std::sync::OnceLock<HotkeyHandle> = std::sync::OnceLock::new();
    let t0 = Instant::now();
    let handler = Box::new(move |ev: HotkeyEvent, _session: u64| {
        log(format!("EVENT {:?} at {:.3}s (open now={:?})", ev, t0.elapsed().as_secs_f64(), SLOT.get().map(|h| h.is_open())));
        HOTKEY_EVENTS.lock().unwrap().push(ev);
        let ms = STALL_MS.swap(0, std::sync::atomic::Ordering::SeqCst);
        if ms > 0 && matches!(ev, HotkeyEvent::Open { .. }) {
            log(format!("  handler stalling {ms} ms"));
            std::thread::sleep(Duration::from_millis(ms));
            log("  handler stall over");
        }
        if let Some(h) = SLOT.get() {
            match ev {
                HotkeyEvent::Open { .. } | HotkeyEvent::OpenSameApp { .. } => h.set_open(true),
                HotkeyEvent::Commit | HotkeyEvent::Cancel => h.set_open(false),
                _ => {}
            }
        }
    });

    let hk = match Hotkey::install(handler) {
        Ok(hk) => hk,
        Err(e) => {
            log(format!("install: Err({e})"));
            write_out(&out);
            std::process::exit(2);
        }
    };
    let _ = SLOT.set(hk.handle());
    let loc = hk.location();
    log(format!(
        "install: Ok, location={} enabled={} is_open={}",
        if loc == CGEventTapLocation::HIDEventTap { "kCGHIDEventTap" } else { "kCGSessionEventTap" },
        hk.is_enabled(),
        hk.is_open()
    ));

    // Pause / resume round trip; the ByUserInput notification must not undo the pause.
    hk.set_enabled(false);
    CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.3, false);
    let off = hk.is_enabled();
    hk.reenable(); // must be a no-op while paused
    let off2 = hk.is_enabled();
    hk.set_enabled(true);
    log(format!(
        "set_enabled(false)->is_enabled={off} (after 0.3s run loop, disabled_count={}); reenable() while paused->{off2}; set_enabled(true)->{}",
        hk.disabled_count(),
        hk.is_enabled()
    ));

    // Sentinel downstream of the hotkey tap.
    let mask = (1u64 << CGEventType::KeyDown.0) | (1 << CGEventType::KeyUp.0) | (1 << CGEventType::FlagsChanged.0);
    let sentinel = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::TailAppendEventTap,
            CGEventTapOptions::Default,
            mask,
            Some(sentinel_cb),
            std::ptr::null_mut(),
        )
    };
    let _sentinel_keep = match sentinel {
        Some(tap) => {
            let src = CFMachPort::new_run_loop_source(None, Some(&tap), 0).unwrap();
            CFRunLoop::current().unwrap().add_source(Some(&src), unsafe { kCFRunLoopCommonModes });
            CGEvent::tap_enable(&tap, true);
            log("sentinel: installed (session, tail-append, active)");
            Some((tap, src))
        }
        None => {
            log("sentinel: FAILED to create");
            None
        }
    };

    if stall {
        std::thread::spawn(poster_stall);
    } else if !no_post {
        std::thread::spawn(poster);
    }
    let deadline = Instant::now() + Duration::from_secs_f64(secs);
    while Instant::now() < deadline {
        CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.25, false);
    }
    log(format!(
        "end: is_open={} enabled={} disabled_count={}",
        hk.is_open(),
        hk.is_enabled(),
        hk.disabled_count()
    ));

    if stall {
        check_stall(hk.disabled_count(), hk.is_enabled());
    } else if !no_post {
        check();
    }
    drop(hk);
    log("dropped Hotkey");
    write_out(&out);
}

fn check() {
    use HotkeyEvent::*;
    let expected = vec![
        Open { reverse: false }, Next, Prev, Commit,
        Open { reverse: false }, Next, Next, Up, Down, Prev, Next, Cancel,
        OpenSameApp { reverse: false }, Commit,
        Open { reverse: true }, Commit,
    ];
    let got = HOTKEY_EVENTS.lock().unwrap().clone();
    log(format!("CHECK hotkey events match expected: {} (got {:?})", got == expected, got));

    let s = SENTINEL.lock().unwrap().clone();
    let downs_ups: Vec<(u32, u16, u64)> =
        s.iter().filter(|e| e.0 != 12).map(|e| (e.0, e.1, e.2)).collect();
    let alt = CGEventFlags::MaskAlternate.0;
    let cmd = CGEventFlags::MaskCommand.0;
    // Only the plain Tab (B) and Cmd+Opt+Tab (E) key events may get past the hotkey tap.
    let expected_pass = vec![
        (10, keycode::TAB, 0), (11, keycode::TAB, 0),
        (10, keycode::TAB, alt | cmd), (11, keycode::TAB, alt | cmd),
    ];
    let mask_mods = |f: u64| f & (alt | cmd | CGEventFlags::MaskShift.0 | CGEventFlags::MaskControl.0);
    let got_pass: Vec<(u32, u16, u64)> = downs_ups.iter().map(|&(t, c, f)| (t, c, mask_mods(f))).collect();
    log(format!(
        "CHECK only plain Tab + Cmd+Opt+Tab key events passed: {} (passed {:?})",
        got_pass == expected_pass,
        got_pass
    ));
    let flags_changed = s.iter().filter(|e| e.0 == 12).count();
    // A: 4, C: 2, D: 2, F: 4 => 12
    log(format!("CHECK all 12 flagsChanged passed through: {} (saw {flags_changed})", flags_changed == 12));
}

fn check_stall(disabled: u32, enabled: bool) {
    use HotkeyEvent::*;
    let all = HOTKEY_EVENTS.lock().unwrap().clone();
    let (ev_start, sen_start) = S2_START.lock().unwrap().unwrap_or((usize::MAX, usize::MAX));
    log(format!("stall: all hotkey events {:?}", all));
    log(format!("CHECK tap disabled at least once: {} (disabled_count={disabled})", disabled >= 1));
    log(format!("CHECK tap enabled afterwards: {enabled}"));
    let pre: Vec<_> = all.iter().take(ev_start.min(all.len())).collect();
    log(format!(
        "INFO stuck-open recovery emitted Commit before S2: {}",
        pre.iter().filter(|e| ***e == Commit).count() > 0
    ));
    let after: Vec<HotkeyEvent> = all.iter().skip(ev_start).copied().collect();
    log(format!(
        "CHECK after stall Shift+Opt+Tab => [Open{{reverse:true}}, Commit]: {} (got {:?})",
        after == vec![Open { reverse: true }, Commit],
        after
    ));
    let s = SENTINEL.lock().unwrap().clone();
    let tabs: Vec<(u32, u16)> = s.iter().skip(sen_start).filter(|e| e.0 != 12).map(|e| (e.0, e.1)).collect();
    log(format!(
        "CHECK after stall plain Tab keyDown+keyUp reached sentinel: {} (got {:?})",
        tabs == vec![(10, keycode::TAB), (11, keycode::TAB)],
        tabs
    ));
}

fn write_out(path: &str) {
    if let Ok(mut f) = std::fs::File::create(path) {
        for l in LOG.lock().unwrap().iter() {
            let _ = writeln!(f, "{l}");
        }
    }
}
