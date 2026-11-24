//! Spike for src/sysstate.rs.
//!
//! Usage:
//!   spike_sysstate <outdir>                       full run; writes <outdir>/out.txt, icons.png
//!   spike_sysstate --hold-secure <ms>             (child) hold secure event input for <ms>
//!   spike_sysstate --icon-timing <file> <pids>    (child) synchronous main-thread icon costs
#[path = "../sysstate.rs"]
mod sysstate;

use std::ffi::c_void;
use std::io::{BufRead, BufReader, Write as _};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use objc2::AnyThread;
use objc2::MainThreadMarker;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBitmapImageFileType, NSBitmapImageRep,
    NSCompositingOperation, NSGraphicsContext, NSImage, NSRunningApplication, NSWorkspace,
};
use objc2_core_foundation::{
    CFDictionary, CFRetained, CFRunLoop, CGPoint, CGRect, CGSize, kCFRunLoopDefaultMode,
};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGBitmapContextGetBytesPerRow,
    CGBitmapContextGetData, CGColorSpace, CGContext, CGImage, CGImageAlphaInfo,
    CGImageByteOrderInfo, CGSessionCopyCurrentDictionary,
};
use objc2_foundation::{NSDictionary, NSPoint, NSRect, NSSize, NSString};

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn EnableSecureEventInput() -> i32;
    fn IsSecureEventInputEnabled() -> u8;
    fn DisableSecureEventInput() -> i32;
}

/// Disables secure input on drop, so every exit path releases it.
struct SecureGuard;
impl SecureGuard {
    fn enable() -> SecureGuard {
        let st = unsafe { EnableSecureEventInput() };
        assert_eq!(st, 0, "EnableSecureEventInput failed: {st}");
        SecureGuard
    }
}
impl Drop for SecureGuard {
    fn drop(&mut self) {
        unsafe { DisableSecureEventInput() };
    }
}

static OUT: Mutex<Option<std::fs::File>> = Mutex::new(None);
fn line(s: &str) {
    println!("{s}");
    if let Some(f) = OUT.lock().unwrap().as_mut() {
        let _ = writeln!(f, "{s}");
        let _ = f.flush();
    }
}

static T0: Mutex<Option<Instant>> = Mutex::new(None);
fn since0() -> f64 {
    T0.lock().unwrap().map(|t| t.elapsed().as_secs_f64() * 1000.0).unwrap_or(0.0)
}

static STALL_LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());
fn stall_log(s: &str) {
    let msg = format!("[+{:.0} ms, main={}] {s}", since0(), MainThreadMarker::new().is_some());
    STALL_LOG.lock().unwrap().push(msg.clone());
    line(&format!("  DETECTOR: {msg}"));
}

static MAIN_PATH_MAX: Mutex<Duration> = Mutex::new(Duration::ZERO);
static READY_CALLS: AtomicU64 = AtomicU64::new(0);
fn on_ready() {
    READY_CALLS.fetch_add(1, Ordering::Relaxed);
}

fn ms(d: Duration) -> String {
    format!("{:.3} ms", d.as_secs_f64() * 1000.0)
}

fn spin(secs: f64) {
    let end = Instant::now() + Duration::from_secs_f64(secs);
    while Instant::now() < end {
        CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.01, false);
    }
}

fn bitmap(px: usize) -> CFRetained<CGContext> {
    let cs = CGColorSpace::new_device_rgb().unwrap();
    let info = CGImageByteOrderInfo::Order32Little.0 | CGImageAlphaInfo::PremultipliedFirst.0;
    unsafe { CGBitmapContextCreate(std::ptr::null_mut::<c_void>(), px, px, 8, 0, Some(&cs), info) }
        .unwrap()
}

/// Draw `img` into `ctx` at (x, y, px, px) through AppKit, as ui.rs does.
fn draw_nsimage(ctx: &CGContext, img: &NSImage, x: f64, y: f64, px: f64) {
    let gc = NSGraphicsContext::graphicsContextWithCGContext_flipped(ctx, false);
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&gc));
    unsafe {
        img.drawInRect_fromRect_operation_fraction_respectFlipped_hints(
            NSRect::new(NSPoint::new(x, y), NSSize::new(px, px)),
            NSRect::ZERO,
            NSCompositingOperation::SourceOver,
            1.0,
            true,
            None,
        )
    };
    NSGraphicsContext::restoreGraphicsState_class();
}

/// BGRA premultiplied bytes of a square bitmap context.
fn pixels(ctx: &CGContext, px: usize) -> Vec<u8> {
    let data = CGBitmapContextGetData(Some(ctx)) as *const u8;
    let bpr = CGBitmapContextGetBytesPerRow(Some(ctx));
    let mut v = Vec::with_capacity(px * px * 4);
    for y in 0..px {
        let row = unsafe { std::slice::from_raw_parts(data.add(y * bpr), px * 4) };
        v.extend_from_slice(row);
    }
    v
}

fn opaque_fraction(p: &[u8]) -> f64 {
    let n = p.len() / 4;
    p.chunks(4).filter(|c| c[3] > 16).count() as f64 / n as f64
}

fn regular_app_pids(max: usize) -> Vec<(i32, String)> {
    autoreleasepool(|_| {
        let apps = NSWorkspace::sharedWorkspace().runningApplications();
        let mut v = vec![];
        for a in apps.iter() {
            if a.activationPolicy() == NSApplicationActivationPolicy::Regular {
                let name = a.localizedName().map(|s| s.to_string()).unwrap_or_default();
                v.push((a.processIdentifier(), name));
            }
        }
        v.truncate(max);
        v
    })
}

fn dump_session_dict(tag: &str) {
    let Some(dict) = CGSessionCopyCurrentDictionary() else {
        line(&format!("  [{tag}] CGSessionCopyCurrentDictionary -> NULL"));
        return;
    };
    let dict: &NSDictionary<NSString, AnyObject> =
        unsafe { &*(&*dict as *const CFDictionary as *const NSDictionary<NSString, AnyObject>) };
    let mut keys: Vec<String> = dict.allKeys().iter().map(|k| k.to_string()).collect();
    keys.sort();
    let mut parts = vec![];
    for k in &keys {
        let v = dict.objectForKey(&NSString::from_str(k)).unwrap();
        let cls = v.class().name().to_string_lossy().into_owned();
        if k.contains("Secure") || k.contains("Locked") || k.contains("OnConsole") {
            parts.push(format!("{k}={:?}({cls})", { let d: Retained<NSString> = unsafe { objc2::msg_send![&*v, description] }; d.to_string() }));
        } else {
            parts.push(format!("{k}({cls})"));
        }
    }
    line(&format!("  [{tag}] session dict: {}", parts.join(", ")));
}

fn front_pid() -> Option<i32> {
    autoreleasepool(|_| NSWorkspace::sharedWorkspace().frontmostApplication().map(|a| a.processIdentifier()))
}

fn stats(v: &mut [Duration]) -> String {
    v.sort();
    let q = |f: f64| v[((v.len() - 1) as f64 * f) as usize];
    let over = v.iter().filter(|d| **d > Duration::from_millis(2)).count();
    format!("p50 {} p99 {} max {} >2ms {}/{}", ms(q(0.5)), ms(q(0.99)), ms(q(1.0)), over, v.len())
}

/// Times secure_input() (main-thread path) and its parts. Returns (secure_input_now() result,
/// frontmost pid at that moment).
fn time_secure_input(tag: &str, n: usize) -> (Option<sysstate::SecureInput>, Option<i32>) {
    let front = front_pid();
    let t = Instant::now();
    let main_first = sysstate::secure_input();
    let main_first_t = t.elapsed();
    let t = Instant::now();
    let now = sysstate::secure_input_now();
    let now_t = t.elapsed();
    let (mut whole, mut flag, mut dict, mut name, mut nowv) = (vec![], vec![], vec![], vec![], vec![]);
    let mut unnamed = 0;
    for _ in 0..n {
        let t = Instant::now();
        let r = sysstate::secure_input();
        whole.push(t.elapsed());
        {
            let mut m = MAIN_PATH_MAX.lock().unwrap();
            *m = (*m).max(t.elapsed());
        }
        if r.as_ref().is_some_and(|s| s.pid.is_none()) {
            unnamed += 1;
        }
        let t = Instant::now();
        let _ = unsafe { IsSecureEventInputEnabled() };
        flag.push(t.elapsed());
        let t = Instant::now();
        let _ = CGSessionCopyCurrentDictionary();
        dict.push(t.elapsed());
        let t = Instant::now();
        let _ = sysstate::secure_input_now();
        nowv.push(t.elapsed());
        if let Some(p) = now.as_ref().and_then(|s| s.pid) {
            let t = Instant::now();
            let _ = sysstate::process_name(p);
            name.push(t.elapsed());
        }
    }
    std::thread::sleep(Duration::from_millis(30));
    let settled = sysstate::secure_input();
    line(&format!("  [{tag}] frontmost pid {front:?}"));
    line(&format!("      secure_input()     first {main_first:?} in {}; after loop+30ms {settled:?}", ms(main_first_t)));
    line(&format!("      secure_input_now() {now:?} in {}", ms(now_t)));
    line(&format!("      x{n} secure_input() [main path]: {}; pid unknown in {unnamed}", stats(&mut whole)));
    line(&format!("      x{n} secure_input_now():         {}", stats(&mut nowv)));
    line(&format!("      IsSecureEventInputEnabled:       {}", stats(&mut flag)));
    line(&format!("      CGSessionCopyCurrentDictionary:  {}", stats(&mut dict)));
    if !name.is_empty() {
        line(&format!("      process_name (uncached):         {}", stats(&mut name)));
    }
    if settled != now {
        line(&format!("      NOTE settled main-path result differs from _now ({settled:?} vs {now:?})"));
    }
    (now, front)
}

/// An ON sample passes when the reported pid is the expected holder or the frontmost app
/// (observed: with a background holder WindowServer reports the frontmost app).
fn check_on(failures: &mut Vec<String>, what: &str, r: &Option<sysstate::SecureInput>, front: Option<i32>, holder: i32) {
    match r {
        Some(s) if s.pid == Some(holder) => line(&format!("  OK {what}: reports the holder {s:?}")),
        Some(s) if s.pid.is_some() && s.pid == front => {
            line(&format!("  OK {what}: on, reports the FRONTMOST app {s:?} (holder pid {holder})"))
        }
        other => failures.push(format!("{what}: holder {holder}, frontmost {front:?}, got {other:?}")),
    }
}

fn child_hold_secure(ms_: u64, ready_file: Option<&String>) {
    let _g = SecureGuard::enable();
    let msg = format!("held {}", std::process::id());
    match ready_file {
        Some(f) => std::fs::write(f, &msg).unwrap(),
        None => {
            println!("{msg}");
            let _ = std::io::stdout().flush();
        }
    }
    std::thread::sleep(Duration::from_millis(ms_));
}

fn child_icon_timing(path: &str, pids: &str) {
    *OUT.lock().unwrap() = Some(std::fs::File::create(path).unwrap());
    let _mtm = MainThreadMarker::new().unwrap();
    let pids: Vec<i32> = pids.split(',').filter_map(|s| s.parse().ok()).collect();
    line("SYNC icon costs on the main thread in a fresh process (cold = first time in-process):");
    line("  pid      lookup     icon()   draw36#1  draw192#1  draw36#2 | warm: lookup+icon+draw36  app");
    let (mut cold_tot, mut cold_max, mut warm_max) = (Duration::ZERO, Duration::ZERO, Duration::ZERO);
    for pid in &pids {
        autoreleasepool(|_| {
            let t = Instant::now();
            let app = NSRunningApplication::runningApplicationWithProcessIdentifier(*pid);
            let t_lookup = t.elapsed();
            let Some(app) = app else {
                line(&format!("  {pid:<6} gone"));
                return;
            };
            let t = Instant::now();
            let icon = app.icon();
            let t_icon = t.elapsed();
            let Some(icon) = icon else {
                line(&format!("  {pid:<6} no icon"));
                return;
            };
            let c36 = bitmap(36);
            let c192 = bitmap(192);
            let t = Instant::now();
            draw_nsimage(&c36, &icon, 0.0, 0.0, 36.0);
            let d36 = t.elapsed();
            let t = Instant::now();
            draw_nsimage(&c192, &icon, 0.0, 0.0, 192.0);
            let d192 = t.elapsed();
            let c36b = bitmap(36);
            let t = Instant::now();
            draw_nsimage(&c36b, &icon, 0.0, 0.0, 36.0);
            let d36b = t.elapsed();
            let t = Instant::now();
            let app2 = NSRunningApplication::runningApplicationWithProcessIdentifier(*pid).unwrap();
            let icon2 = app2.icon().unwrap();
            let c = bitmap(36);
            draw_nsimage(&c, &icon2, 0.0, 0.0, 36.0);
            let warm = t.elapsed();
            let cold = t_lookup + t_icon + d36 + d192;
            cold_tot += cold;
            cold_max = cold_max.max(cold);
            warm_max = warm_max.max(warm);
            let name = app.localizedName().map(|s| s.to_string()).unwrap_or_default();
            line(&format!(
                "  {pid:<6} {:>9} {:>9} {:>9} {:>10} {:>9} | {:>9}  {name}",
                ms(t_lookup),
                ms(t_icon),
                ms(d36),
                ms(d192),
                ms(d36b),
                ms(warm)
            ));
        });
    }
    line(&format!(
        "  cold total {} for {} apps (max per app {}); warm max {}",
        ms(cold_tot),
        pids.len(),
        ms(cold_max),
        ms(warm_max)
    ));
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("--hold-secure") => return child_hold_secure(args[2].parse().unwrap(), args.get(3)),
        Some("--icon-timing") => return child_icon_timing(&args[2], &args[3]),
        _ => {}
    }
    let outdir = args.get(1).cloned().unwrap_or_else(|| ".".into());
    std::fs::create_dir_all(&outdir).unwrap();
    *OUT.lock().unwrap() = Some(std::fs::File::create(format!("{outdir}/out.txt")).unwrap());
    let mtm = MainThreadMarker::new().unwrap();
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let me = std::process::id() as i32;
    line(&format!("spike_sysstate pid {me}"));
    let mut failures: Vec<String> = vec![];

    // ------------------------------------------------------------------ secure input
    line("== secure input");
    dump_session_dict("baseline");
    let (base, _) = time_secure_input("baseline", 500);
    if base.is_some() {
        line("  baseline already ON (another holder) -> off/on checks inconclusive");
    }
    // An ON sample passes when the reported pid is the expected holder or the frontmost app
    // (observed: with a background holder WindowServer reports the frontmost app).
    {
        let g = SecureGuard::enable();
        let t = Instant::now();
        // First ON episode in this process, so no snapshot exists yet: time until a pid shows.
        let first = sysstate::secure_input();
        let t_ready = Instant::now();
        let mut polls = 0;
        while sysstate::secure_input().is_some_and(|s| s.pid.is_none()) && t_ready.elapsed() < Duration::from_secs(1) {
            std::thread::sleep(Duration::from_micros(250));
            polls += 1;
        }
        line(&format!(
            "  cold main-path call -> {first:?}; pid known after {} ({polls} polls): {:?}",
            ms(t_ready.elapsed()),
            sysstate::secure_input()
        ));
        let (r, front) = time_secure_input("in-process ON", 300);
        dump_session_dict("in-process ON");
        drop(g);
        line(&format!("  held in-process for {}", ms(t.elapsed())));
        check_on(&mut failures, "in-process", &r, front, me);
    }
    let (after, _) = time_secure_input("after Disable", 50);
    if base.is_none() && after.is_some() {
        failures.push(format!("after Disable still on: {after:?}"));
    }
    {
        let exe = std::env::current_exe().unwrap();
        let mut child = Command::new(exe)
            .args(["--hold-secure", "400"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let cpid = child.id() as i32;
        let mut l = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut l).unwrap();
        let (r, front) = time_secure_input("child ON", 100);
        check_on(&mut failures, "spawned child", &r, front, cpid);
        child.wait().unwrap();
    }
    let (after2, _) = time_secure_input("after child exit", 50);
    if base.is_none() && after2.is_some() {
        failures.push(format!("after child exit still on: {after2:?}"));
    }
    // A separately launched app (its own responsible process): pass its bundle path.
    if let Ok(holder) = std::env::var("SPIKE_HOLDER_APP") {
        let ready = format!("{outdir}/holder_ready.txt");
        let _ = std::fs::remove_file(&ready);
        let st = Command::new("/usr/bin/open")
            .args(["-n", "-g", &holder, "--args", "--hold-secure", "600", &ready])
            .status()
            .unwrap();
        let t = Instant::now();
        let mut hpid = None;
        while t.elapsed() < Duration::from_secs(10) && hpid.is_none() {
            hpid = std::fs::read_to_string(&ready)
                .ok()
                .and_then(|s| s.strip_prefix("held ").and_then(|p| p.trim().parse::<i32>().ok()));
            std::thread::sleep(Duration::from_millis(5));
        }
        line(&format!("  holder app: open {st}, ready pid {hpid:?} after {}", ms(t.elapsed())));
        let (r, front) = time_secure_input("holder app ON", 100);
        check_on(&mut failures, "separate holder app", &r, front, hpid.unwrap_or(-1));
        std::thread::sleep(Duration::from_millis(900));
        let (after3, _) = time_secure_input("after holder app exit", 50);
        if base.is_none() && after3.is_some() {
            failures.push(format!("after holder app exit still on: {after3:?}"));
        }
    }
    // Make ourselves frontmost while holding, then hide again (our own app only).
    if std::env::var("SPIKE_ACTIVATE").is_ok() {
        let front0 = front_pid();
        let g = SecureGuard::enable();
        let t_hold = Instant::now();
        let (a, fa) = (sysstate::secure_input_now(), front_pid());
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
        app.activate();
        let t = Instant::now();
        while front_pid() != Some(me) && t.elapsed() < Duration::from_secs(1) {
            CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.01, false);
        }
        let (b, fb) = (sysstate::secure_input_now(), front_pid());
        let activated = fb == Some(me);
        app.hide(None);
        let t = Instant::now();
        while front_pid() == Some(me) && t.elapsed() < Duration::from_secs(1) {
            CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.01, false);
        }
        let (c, fc) = (sysstate::secure_input_now(), front_pid());
        drop(g);
        let held = t_hold.elapsed();
        spin(0.3);
        let front_end = front_pid();
        line(&format!("  activation test (held {}): frontmost before {front0:?}", ms(held)));
        line(&format!("    background holder: {a:?} (front {fa:?})"));
        line(&format!("    spike frontmost:   {b:?} (front {fb:?}, activated {activated})"));
        line(&format!("    after hide:        {c:?} (front {fc:?})"));
        line(&format!("    frontmost restored: {front_end:?} == {front0:?}: {}", front_end == front0));
        if activated {
            check_on(&mut failures, "activated spike", &b, None, me);
        } else {
            line("    activation refused -> holder-while-frontmost case unverified");
        }
    }

    // ------------------------------------------------------------------ sync icon costs (child)
    let apps = regular_app_pids(15);
    line(&format!("== icons: {} regular apps: {:?}", apps.len(), apps));
    let pids: Vec<i32> = apps.iter().map(|a| a.0).collect();
    let pid_list = pids.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(",");
    let timing_file = format!("{outdir}/icon_timing.txt");
    let st = Command::new(std::env::current_exe().unwrap())
        .args(["--icon-timing", &timing_file, &pid_list])
        .stdout(Stdio::null())
        .status()
        .unwrap();
    line(&format!("  icon-timing child exit {st}"));
    for l in std::fs::read_to_string(&timing_file).unwrap_or_default().lines() {
        line(l);
    }

    // ------------------------------------------------------------------ stall detector + prefetch
    line("== stall detector (period 10 ms, threshold 50 ms) + IconPrefetcher, 5.5 s window");
    spin(0.3); // let the run loop settle before measuring
    *T0.lock().unwrap() = Some(Instant::now());
    let det = sysstate::StallDetector::start(
        Duration::from_millis(10),
        Duration::from_millis(50),
        stall_log,
    );
    spin(0.5);
    line(&format!("  [+{:.0}] idle 0.5 s: stalls {:?}", since0(), det.stalls()));

    let icons = sysstate::IconPrefetcher::new(mtm, Some(on_ready));
    let bogus = 999_999;
    let mut all = pids.clone();
    all.push(bogus);
    let t = Instant::now();
    icons.prefetch(all.clone());
    line(&format!("  prefetch({} pids) returned in {}", all.len(), ms(t.elapsed())));
    let t = Instant::now();
    let early = icons.get(pids[0]);
    line(&format!("  get() before ready -> {} in {}", if early.is_some() { "Some" } else { "None" }, ms(t.elapsed())));
    let t_pf = Instant::now();
    while !all.iter().all(|p| icons.is_loaded(*p)) && t_pf.elapsed() < Duration::from_secs(5) {
        CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.005, true);
    }
    line(&format!(
        "  all loaded off-main after {} ({} on_ready calls); stalls so far {:?}",
        ms(t_pf.elapsed()),
        READY_CALLS.load(Ordering::Relaxed),
        det.stalls()
    ));
    // Main-thread costs of using the prefetched icons.
    let (mut get_max, mut draw_max, mut get2_max) = (Duration::ZERO, Duration::ZERO, Duration::ZERO);
    let mut got: Vec<(i32, Retained<NSImage>)> = vec![];
    for p in &all {
        let t = Instant::now();
        let g = icons.get(*p);
        get_max = get_max.max(t.elapsed());
        let t = Instant::now();
        let _ = icons.get(*p);
        get2_max = get2_max.max(t.elapsed());
        match g {
            Some(img) => {
                let c = bitmap(192);
                let t = Instant::now();
                draw_nsimage(&c, &img, 0.0, 0.0, 192.0);
                let c2 = bitmap(36);
                draw_nsimage(&c2, &img, 0.0, 0.0, 36.0);
                draw_max = draw_max.max(t.elapsed());
                let f = opaque_fraction(&pixels(&c, 192));
                if f < 0.2 {
                    failures.push(format!("pid {p}: prefetched icon nearly transparent ({f:.2})"));
                }
                got.push((*p, img));
            }
            None if *p == bogus => {}
            None => failures.push(format!("pid {p}: no icon after prefetch")),
        }
    }
    line(&format!(
        "  main-thread: first get() max {}, cached get() max {}, first draw 192+36 px max {}",
        ms(get_max),
        ms(get2_max),
        ms(draw_max)
    ));
    line(&format!("  bogus pid {bogus}: get -> {:?}, is_loaded {}", icons.get(bogus).is_some(), icons.is_loaded(bogus)));

    // Block the main thread once, at about +2.5 s.
    while since0() < 2500.0 {
        CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.01, false);
    }
    let before_block = det.stalls();
    let t = Instant::now();
    std::thread::sleep(Duration::from_millis(120));
    let blocked = t.elapsed();
    line(&format!("  [+{:.0}] blocked main for {} (stalls before: {before_block:?})", since0(), ms(blocked)));
    while since0() < 5500.0 {
        CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.01, false);
    }
    let (count, max) = det.stalls();
    line(&format!("  [+{:.0}] final stalls: count {count}, max {max:.1} ms", since0()));
    let logged: Vec<String> = STALL_LOG.lock().unwrap().clone();
    let stall_lines = logged.iter().filter(|l| l.contains("main thread stall: ")).count();
    if count != 1 || stall_lines != 1 {
        failures.push(format!("expected exactly 1 stall, got count {count}, log lines {stall_lines}: {logged:?}"));
    } else if !(90.0..=130.0).contains(&max) {
        failures.push(format!("stall {max:.1} ms not ~120 ms"));
    } else {
        line(&format!("  OK exactly one stall, {max:.1} ms (actual block {})", ms(blocked)));
    }
    drop(det);

    // ------------------------------------------------------------------ correctness of rasters
    line("== prefetched vs direct main-thread rendering (64 px), contact sheet icons.png");
    let n = got.len();
    let sheet = bitmap(64 * n.max(1));
    for (i, (p, img)) in got.iter().enumerate() {
        let direct = NSRunningApplication::runningApplicationWithProcessIdentifier(*p).and_then(|a| a.icon());
        let cmp = |px: usize| -> (f64, [f64; 4], f64) {
            let a = bitmap(px);
            draw_nsimage(&a, img, 0.0, 0.0, px as f64);
            let b = bitmap(px);
            if let Some(d) = &direct {
                draw_nsimage(&b, d, 0.0, 0.0, px as f64);
            }
            let (pa, pb) = (pixels(&a, px), pixels(&b, px));
            let n = pa.len() as f64;
            let abs = pa.iter().zip(&pb).map(|(x, y)| (*x as i32 - *y as i32).abs() as f64).sum::<f64>() / n;
            let mut signed = [0.0; 4];
            for (i, (x, y)) in pa.iter().zip(&pb).enumerate() {
                signed[i % 4] += (*x as f64 - *y as f64) / (n / 4.0);
            }
            (abs, signed, opaque_fraction(&pa))
        };
        let (diff, s64, op) = cmp(64);
        let (d256, s256, _) = cmp(256);
        let (d36, _, _) = cmp(36);
        let others: Vec<String> = [18usize, 48, 96, 140, 192].iter().map(|&x| format!("{x}:{:.1}", cmp(x).0)).collect();
        line(&format!(
            "  pid {p:<6} 64px mean |diff| {diff:5.2}/255 signed BGRA {:?}; 256px {d256:5.2} signed {:?}; 36px {d36:5.2}; other sizes {}; opaque {op:.2}",
            s64.map(|v| (v * 10.0).round() / 10.0),
            s256.map(|v| (v * 10.0).round() / 10.0),
            others.join(" ")
        ));
        if diff > 12.0 {
            failures.push(format!("pid {p}: prefetched differs from direct render by {diff:.1}"));
        }
        // Row 0 (top in PNG): prefetched; we draw prefetched at top half and direct at bottom.
        draw_nsimage(&sheet, img, (i * 64) as f64, (64 * n.max(1) - 64) as f64, 64.0);
        if let Some(d) = NSRunningApplication::runningApplicationWithProcessIdentifier(*p).and_then(|a| a.icon()) {
            draw_nsimage(&sheet, &d, (i * 64) as f64, (64 * n.max(1) - 128) as f64, 64.0);
        }
    }
    if let Some(cg) = CGBitmapContextCreateImage(Some(&sheet)) {
        save_png(&cg, &format!("{outdir}/icons.png"));
    }
    // Same at 36 px (the 2x label icon size): top row prefetched, second row direct.
    let sheet36 = bitmap(40 * n.max(1));
    for (i, (p, img)) in got.iter().enumerate() {
        let top = (40 * n.max(1) - 40) as f64;
        draw_nsimage(&sheet36, img, (i * 40) as f64, top, 36.0);
        if let Some(d) = NSRunningApplication::runningApplicationWithProcessIdentifier(*p).and_then(|a| a.icon()) {
            draw_nsimage(&sheet36, &d, (i * 40) as f64, top - 40.0, 36.0);
        }
    }
    if let Some(cg) = CGBitmapContextCreateImage(Some(&sheet36)) {
        save_png(&cg, &format!("{outdir}/icons36.png"));
    }
    for (p, _) in got.iter().take(3) {
        diag_36(*p);
    }
    icons.retain_pids(&pids[1..]);
    line(&format!("  retain_pids(drop {}) -> is_loaded {}", pids[0], icons.is_loaded(pids[0])));

    let mp = *MAIN_PATH_MAX.lock().unwrap();
    if mp > Duration::from_millis(2) {
        failures.push(format!("secure_input() main path max {} > 2 ms", ms(mp)));
    } else {
        line(&format!("  secure_input() main path max over all samples: {}", ms(mp)));
    }
    line("== RESULT");
    if failures.is_empty() {
        line("PASS");
    } else {
        for f in &failures {
            line(&format!("FAIL: {f}"));
        }
    }
}


fn diag_36(pid: i32) {
    let Some(icon) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid).and_then(|a| a.icon()) else { return };
    let px = 36usize;
    let a = bitmap(px);
    draw_nsimage(&a, &icon, 0.0, 0.0, px as f64);
    let pa = pixels(&a, px);
    let d = |p: &[u8]| pa.iter().zip(p).map(|(x, y)| (*x as i32 - *y as i32).abs() as f64).sum::<f64>() / pa.len() as f64;
    let draw_cg = |cg: &CGImage| {
        let c = bitmap(px);
        CGContext::draw_image(Some(&c), CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(px as f64, px as f64)), Some(cg));
        pixels(&c, px)
    };
    let mut r = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(px as f64, px as f64));
    let b = unsafe { icon.CGImageForProposedRect_context_hints(&mut r, None, None) }.unwrap();
    let bw = CGImage::width(Some(&b));
    let pb = draw_cg(&b);
    let ctxc = bitmap(px);
    let gc = NSGraphicsContext::graphicsContextWithCGContext_flipped(&ctxc, false);
    let mut r2 = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(px as f64, px as f64));
    let c = unsafe { icon.CGImageForProposedRect_context_hints(&mut r2, Some(&gc), None) }.unwrap();
    let cw = CGImage::width(Some(&c));
    let pc = draw_cg(&c);
    let single = NSImage::initWithSize(NSImage::alloc(), NSSize::new(128.0, 128.0));
    let rep = NSBitmapImageRep::initWithCGImage(NSBitmapImageRep::alloc(), &b);
    rep.setSize(NSSize::new(128.0, 128.0));
    single.addRepresentation(&rep);
    let dd = bitmap(px);
    draw_nsimage(&dd, &single, 0.0, 0.0, px as f64);
    let pd = pixels(&dd, px);
    line(&format!("  diag36 pid {pid}: CGImageForProposedRect(nil ctx) {bw}px diff {:.2}; (bitmap ctx) {cw}px diff {:.2}; single-rep NSImage diff {:.2}", d(&pb), d(&pc), d(&pd)));
}

fn save_png(cg: &CGImage, path: &str) {
    let rep = NSBitmapImageRep::initWithCGImage(NSBitmapImageRep::alloc(), cg);
    let data = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) };
    if let Some(d) = data {
        let _ = std::fs::write(path, d.to_vec());
    }
    let _ = (CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(0.0, 0.0)),);
}
