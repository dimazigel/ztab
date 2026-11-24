//! Spike for src/thumbs.rs.
//! Usage: spike_thumbs <outdir> [max_w max_h] [--offscreen | --selftest | --refresh]
//! --refresh: background refresh (refresh_stale / capture_one / capture through the bounded
//! scheduler) with main-thread latency, heartbeat, in-flight, CPU and backoff checks.
//! Writes <outdir>/out.txt (report), <wid>.png (async capture), <wid>_sync.png (capture_sync),
//! <wid>_cg.png (CGWindowListCreateImage path).
#[path = "../thumbs.rs"]
mod thumbs;

use std::cell::RefCell;
use std::fmt::Write as _;
use std::io::Write as _;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, MainThreadMarker};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBitmapImageFileType, NSBitmapImageRep, NSImage,
};
use objc2_core_foundation::{CFArray, CFRunLoop, CGPoint, CGRect, CGSize, kCFRunLoopDefaultMode};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGImage, CGImageAlphaInfo, CGImageByteOrderInfo,
    CGWindowListCopyWindowInfo, CGWindowListOption,
};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};

struct Out {
    file: std::fs::File,
}
impl Out {
    fn line(&mut self, s: &str) {
        println!("{s}");
        let _ = writeln!(self.file, "{s}");
        let _ = self.file.flush();
    }
}

#[derive(Clone)]
struct WinInfo {
    wid: u32,
    owner: String,
    name: String,
    w: f64,
    h: f64,
}

fn list_windows(offscreen_only: bool) -> Vec<WinInfo> {
    let opts = if offscreen_only {
        CGWindowListOption::OptionAll | CGWindowListOption::ExcludeDesktopElements
    } else {
        CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements
    };
    let Some(arr) = CGWindowListCopyWindowInfo(opts, 0) else {
        return vec![];
    };
    // Toll-free bridged.
    let arr: &NSArray<NSDictionary<NSString, AnyObject>> =
        unsafe { &*(&*arr as *const CFArray as *const NSArray<NSDictionary<NSString, AnyObject>>) };
    let num = |d: &NSDictionary<NSString, AnyObject>, k: &str| -> Option<f64> {
        let v = d.objectForKey(&NSString::from_str(k))?;
        v.downcast_ref::<NSNumber>().map(|n| n.as_f64())
    };
    let st = |d: &NSDictionary<NSString, AnyObject>, k: &str| -> String {
        d.objectForKey(&NSString::from_str(k))
            .and_then(|v| v.downcast_ref::<NSString>().map(|s| s.to_string()))
            .unwrap_or_default()
    };
    let mut out = vec![];
    for d in arr.iter() {
        if num(&d, "kCGWindowLayer") != Some(0.0) {
            continue;
        }
        let Some(wid) = num(&d, "kCGWindowNumber") else {
            continue;
        };
        let (mut w, mut h) = (0.0, 0.0);
        if let Some(b) = d.objectForKey(&NSString::from_str("kCGWindowBounds"))
            && let Some(b) = b.downcast_ref::<NSDictionary>()
        {
            let b: &NSDictionary<NSString, AnyObject> = unsafe { &*(b as *const NSDictionary as *const _) };
            w = num(b, "Width").unwrap_or(0.0);
            h = num(b, "Height").unwrap_or(0.0);
        }
        if w < 50.0 || h < 50.0 {
            continue;
        }
        let onscreen = d
            .objectForKey(&NSString::from_str("kCGWindowIsOnscreen"))
            .and_then(|v| v.downcast_ref::<NSNumber>().map(|n| n.as_bool()))
            .unwrap_or(false);
        if offscreen_only && (onscreen || st(&d, "kCGWindowName").is_empty()) {
            continue;
        }
        out.push(WinInfo {
            wid: wid as u32,
            owner: st(&d, "kCGWindowOwnerName"),
            name: st(&d, "kCGWindowName"),
            w,
            h,
        });
    }
    out
}

fn ns_to_cg(img: &NSImage) -> Option<Retained<CGImage>> {
    unsafe { img.CGImageForProposedRect_context_hints(std::ptr::null_mut(), None, None) }
}

fn write_png(img: &NSImage, path: &str) -> Result<(usize, usize), String> {
    let cg = ns_to_cg(img).ok_or("no CGImage")?;
    let rep = NSBitmapImageRep::initWithCGImage(NSBitmapImageRep::alloc(), &cg);
    let data =
        unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }
            .ok_or("png encode failed")?;
    std::fs::write(path, data.to_vec()).map_err(|e| e.to_string())?;
    Ok((CGImage::width(Some(&cg)), CGImage::height(Some(&cg))))
}

/// Downsample to 16x16 and report (mean luma, luma stddev, mean alpha) to detect blank images.
fn stats(img: &NSImage) -> String {
    let Some(cg) = ns_to_cg(img) else {
        return "no-cg".into();
    };
    let n = 16usize;
    let mut buf = vec![0u8; n * n * 4];
    let Some(cs) = CGColorSpace::new_device_rgb() else {
        return "no-cs".into();
    };
    let info = CGImageByteOrderInfo::Order32Little.0 | CGImageAlphaInfo::PremultipliedFirst.0;
    let Some(ctx) =
        (unsafe { CGBitmapContextCreate(buf.as_mut_ptr().cast(), n, n, 8, n * 4, Some(&cs), info) })
    else {
        return "no-ctx".into();
    };
    CGContext::draw_image(
        Some(&ctx),
        CGRect::new(CGPoint::new(0., 0.), CGSize::new(n as f64, n as f64)),
        Some(&cg),
    );
    drop(ctx);
    let mut lum = vec![];
    let mut alpha = 0.0;
    for px in buf.chunks(4) {
        // BGRA little-endian premultiplied-first.
        let (b, g, r, a) = (px[0] as f64, px[1] as f64, px[2] as f64, px[3] as f64);
        lum.push(0.299 * r + 0.587 * g + 0.114 * b);
        alpha += a;
    }
    let mean = lum.iter().sum::<f64>() / lum.len() as f64;
    let sd = (lum.iter().map(|l| (l - mean).powi(2)).sum::<f64>() / lum.len() as f64).sqrt();
    let verdict = if sd < 2.0 { "BLANK?" } else { "content" };
    format!(
        "luma_mean={mean:.0} luma_sd={sd:.1} alpha_mean={:.0} {verdict}",
        alpha / (n * n) as f64
    )
}

fn ms(d: Duration) -> String {
    format!("{:.1}ms", d.as_secs_f64() * 1000.0)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let outdir = args.get(1).cloned().unwrap_or_else(|| "dist/spike_thumbs".into());
    let nums: Vec<f64> = args.iter().skip(2).filter_map(|s| s.parse().ok()).collect();
    let max_w: f64 = nums.first().copied().unwrap_or(320.0);
    let max_h: f64 = nums.get(1).copied().unwrap_or(200.0);
    std::fs::create_dir_all(&outdir).expect("outdir");
    let mut out = Out {
        file: std::fs::File::create(format!("{outdir}/out.txt")).expect("out.txt"),
    };

    let refresh = args.iter().any(|a| a == "--refresh");
    // Hard watchdog: `open -W` must never hang.
    let limit = if refresh { 120 } else { 20 };
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(limit));
        eprintln!("watchdog exit");
        std::process::exit(3);
    });

    let mtm = MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);

    if refresh {
        // Before anything touches the Screen Recording preflight on main: the first
        // refresh_stale call must also be cheap.
        refresh_mode(&outdir, &mut out);
        out.line("done");
        return;
    }

    let ax = unsafe { objc2_application_services::AXIsProcessTrusted() };
    let sc = thumbs::screen_capture_allowed();
    out.line(&format!(
        "AXIsProcessTrusted={ax} CGPreflightScreenCaptureAccess={sc} pid={}",
        std::process::id()
    ));
    out.line(&format!(
        "CGWindowListCreateImage symbol available: {}",
        thumbs::cg_window_image_available()
    ));
    out.line(&format!(
        "max {max_w}x{max_h} pt; fit_points(1600,1000)={:?} fit_points(100,50)={:?}",
        thumbs::fit_points(1600., 1000., max_w, max_h),
        thumbs::fit_points(100., 50., max_w, max_h)
    ));

    if args.iter().any(|a| a == "--offscreen") {
        // Windows on other Spaces / minimized / hidden apps (read-only: nothing is moved).
        let t = thumbs::Thumbnailer::new();
        let wins = list_windows(true);
        out.line(&format!("off-screen titled layer-0 windows: {}", wins.len()));
        for w in &wins {
            let t0 = Instant::now();
            let sck = t.capture_sync_sck(w.wid, max_w, max_h);
            let d_sck = t0.elapsed();
            let t0 = Instant::now();
            let cg = t.capture_sync_cg(w.wid, max_w, max_h);
            let d_cg = t0.elapsed();
            let desc = |i: &Option<Retained<NSImage>>| match i {
                Some(i) => format!("{:.0}x{:.0}pt {}", i.size().width, i.size().height, stats(i)),
                None => "None".into(),
            };
            out.line(&format!(
                "  wid={} owner={:?} {}x{}: SCK[{}] {} | CG[{}] {}",
                w.wid,
                w.owner,
                w.w,
                w.h,
                ms(d_sck),
                desc(&sck),
                ms(d_cg),
                desc(&cg)
            ));
            if let Some(i) = &sck {
                let _ = write_png(i, &format!("{outdir}/off_{}_sck.png", w.wid));
            }
            if let Some(i) = &cg {
                let _ = write_png(i, &format!("{outdir}/off_{}_cg.png", w.wid));
            }
        }
        out.line("done");
        return;
    }
    if args.iter().any(|a| a == "--selftest") {
        selftest(&outdir, &mut out, mtm, max_w, max_h);
        out.line("done");
        return;
    }

    let wins = list_windows(false);
    out.line(&format!("on-screen layer-0 windows: {}", wins.len()));
    for w in &wins {
        out.line(&format!(
            "  wid={} {}x{} owner={:?} name={:?}",
            w.wid, w.w, w.h, w.owner, w.name
        ));
    }
    let wids: Vec<u32> = wins.iter().map(|w| w.wid).collect();

    let t = thumbs::Thumbnailer::new();

    // ---- async capture (cold content cache) ----
    let results: Arc<Mutex<Vec<(u64, u32, Duration)>>> = Arc::new(Mutex::new(vec![]));
    let images: Rc<RefCell<Vec<(u32, Retained<NSImage>)>>> = Rc::new(RefCell::new(vec![]));
    // NSImage isn't Send; collect on main via a main-thread-only static.
    thread_local! { static IMGS: RefCell<Vec<(u32, Retained<NSImage>)>> = const { RefCell::new(Vec::new()) }; }
    let started = Instant::now();
    let r2 = results.clone();
    t.capture(wids.clone(), max_w, max_h, 7, move |gen_, wid, img| {
        assert!(MainThreadMarker::new().is_some(), "callback not on main thread");
        r2.lock().unwrap().push((gen_, wid, started.elapsed()));
        IMGS.with(|v| v.borrow_mut().push((wid, img)));
    });
    let returned_after = started.elapsed();
    out.line(&format!(
        "capture() returned after {} (must be ~0)",
        ms(returned_after)
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && results.lock().unwrap().len() < wids.len() {
        CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.02, true);
    }
    let total = started.elapsed();
    IMGS.with(|v| images.borrow_mut().extend(v.borrow_mut().drain(..)));
    {
        let res = results.lock().unwrap();
        out.line(&format!(
            "ASYNC: {} of {} windows delivered; loop ended after {}",
            res.len(),
            wids.len(),
            ms(total)
        ));
        for (g, wid, at) in res.iter() {
            out.line(&format!("  cb gen={g} wid={wid} at +{}", ms(*at)));
        }
    }
    for (wid, img) in images.borrow().iter() {
        let path = format!("{outdir}/{wid}.png");
        let sz = img.size();
        match write_png(img, &path) {
            Ok((pw, ph)) => out.line(&format!(
                "  async wid={wid} pt={:.0}x{:.0} px={pw}x{ph} {} -> {path}",
                sz.width,
                sz.height,
                stats(img)
            )),
            Err(e) => out.line(&format!("  async wid={wid} png error {e}")),
        }
        let cached = t.cached(*wid).is_some();
        out.line(&format!("  cached({wid}) present: {cached}"));
    }

    // ---- second async round (warm content cache) ----
    let results2: Arc<Mutex<Vec<(u32, Duration)>>> = Arc::new(Mutex::new(vec![]));
    let r3 = results2.clone();
    let started2 = Instant::now();
    t.capture(wids.clone(), max_w, max_h, 8, move |_, wid, _| {
        r3.lock().unwrap().push((wid, started2.elapsed()))
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let expect = results.lock().unwrap().len();
    while Instant::now() < deadline && results2.lock().unwrap().len() < expect {
        CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.02, true);
    }
    {
        let r = results2.lock().unwrap();
        let last = r.iter().map(|x| x.1).max().unwrap_or_default();
        out.line(&format!(
            "ASYNC warm: {} delivered, last at +{}",
            r.len(),
            ms(last)
        ));
    }

    // ---- sync paths ----
    for w in &wins {
        let t0 = Instant::now();
        let r = t.capture_sync_with_backend(w.wid, max_w, max_h);
        let d = t0.elapsed();
        match r {
            Some((img, b)) => {
                let path = format!("{outdir}/{}_sync.png", w.wid);
                let _ = write_png(&img, &path);
                out.line(&format!(
                    "SYNC wid={} {:?} in {} {}",
                    w.wid,
                    b,
                    ms(d),
                    stats(&img)
                ));
            }
            None => out.line(&format!("SYNC wid={} None in {}", w.wid, ms(d))),
        }
        let t0 = Instant::now();
        let r = t.capture_sync_cg(w.wid, max_w, max_h);
        let d = t0.elapsed();
        match r {
            Some(img) => {
                let path = format!("{outdir}/{}_cg.png", w.wid);
                let sz = img.size();
                let _ = write_png(&img, &path);
                out.line(&format!(
                    "CG   wid={} in {} pt={:.0}x{:.0} {}",
                    w.wid,
                    ms(d),
                    sz.width,
                    sz.height,
                    stats(&img)
                ));
            }
            None => out.line(&format!("CG   wid={} None in {}", w.wid, ms(d))),
        }
    }
    // Parallel CGWindowListCreateImage over all windows (threads), wall time.
    {
        let t0 = Instant::now();
        let handles: Vec<_> = wids
            .iter()
            .map(|&wid| {
                std::thread::spawn(move || {
                    let t = thumbs::Thumbnailer::new();
                    t.capture_sync_cg(wid, max_w, max_h).is_some()
                })
            })
            .collect();
        let ok = handles
            .into_iter()
            .filter_map(|h| h.join().ok())
            .filter(|&b| b)
            .count();
        out.line(&format!(
            "CG parallel: {ok}/{} captured, wall {}",
            wids.len(),
            ms(t0.elapsed())
        ));
    }
    // A bogus window id must not call back nor crash.
    let bogus = Arc::new(Mutex::new(0));
    let b2 = bogus.clone();
    t.capture(vec![0xFFFF_FF00], max_w, max_h, 9, move |_, _, _| {
        *b2.lock().unwrap() += 1
    });
    let d = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < d {
        CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.05, false);
    }
    out.line(&format!("bogus wid callbacks: {}", bogus.lock().unwrap()));
    out.line(&format!(
        "capture_sync(bogus) is_none: {}",
        t.capture_sync(0xFFFF_FF00, max_w, max_h).is_none()
    ));
    out.line("done");
    let _ = String::new().write_str("");
}

/// Capture this process's own window through the real SCK pipeline without the Screen
/// Recording grant (Thumbnailer::new_own_process_only).
fn selftest(outdir: &str, out: &mut Out, mtm: MainThreadMarker, max_w: f64, max_h: f64) {
    use objc2_app_kit::{NSBackingStoreType, NSColor, NSFont, NSTextField, NSWindow, NSWindowStyleMask};
    use objc2_foundation::{NSPoint, NSRect, NSSize};
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let win = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            mtm.alloc(),
            NSRect::new(NSPoint::new(200., 200.), NSSize::new(800., 500.)),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Miniaturizable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { win.setReleasedWhenClosed(false) };
    win.setTitle(&NSString::from_str("ZTab thumbs selftest"));
    win.setBackgroundColor(Some(&NSColor::colorWithRed_green_blue_alpha(0.1, 0.4, 0.9, 1.0)));
    let label = NSTextField::labelWithString(&NSString::from_str("THUMBNAIL SELFTEST 123"), mtm);
    label.setFont(Some(&NSFont::systemFontOfSize(48.0)));
    label.setFrame(NSRect::new(NSPoint::new(40., 200.), NSSize::new(720., 80.)));
    win.contentView().unwrap().addSubview(&label);
    win.makeKeyAndOrderFront(None);
    app.activate();
    let until = Instant::now() + Duration::from_millis(700);
    while Instant::now() < until {
        CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.05, false);
    }
    let wid = win.windowNumber() as u32;
    out.line(&format!("selftest window wid={wid} (800x500 pt content)"));

    let t = thumbs::Thumbnailer::new_own_process_only();
    let got: Arc<Mutex<Vec<(u64, u32, Duration, bool)>>> = Arc::new(Mutex::new(vec![]));
    thread_local! { static IMG: RefCell<Option<Retained<NSImage>>> = const { RefCell::new(None) }; }
    for round in 0..3u64 {
        let g2 = got.clone();
        let started = Instant::now();
        t.capture(
            vec![wid, 0xFFFF_FF00],
            max_w,
            max_h,
            100 + round,
            move |g, w, img| {
                let main = MainThreadMarker::new().is_some();
                g2.lock().unwrap().push((g, w, started.elapsed(), main));
                IMG.with(|c| *c.borrow_mut() = Some(img));
            },
        );
        let ret = started.elapsed();
        let want = round as usize + 1;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && got.lock().unwrap().len() < want {
            CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.01, true);
        }
        let g = got.lock().unwrap();
        out.line(&format!(
            "async round {round}: capture() returned in {}; callbacks so far {:?}",
            ms(ret),
            g.iter()
                .map(|(g, w, d, m)| format!("gen={g} wid={w} +{} main={m}", ms(*d)))
                .collect::<Vec<_>>()
        ));
    }
    // Let any stray callbacks (bogus wid) arrive.
    let until = Instant::now() + Duration::from_millis(800);
    while Instant::now() < until {
        CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.05, false);
    }
    out.line(&format!(
        "total callbacks after 3 rounds (expect 3, none for bogus wid): {}",
        got.lock().unwrap().len()
    ));
    if let Some(img) = IMG.with(|c| c.borrow().clone()) {
        let sz = img.size();
        let path = format!("{outdir}/selftest_async.png");
        match write_png(&img, &path) {
            Ok((pw, ph)) => out.line(&format!(
                "async image pt={:.0}x{:.0} px={pw}x{ph} {} -> {path}",
                sz.width,
                sz.height,
                stats(&img)
            )),
            Err(e) => out.line(&format!("png error {e}")),
        }
    }
    out.line(&format!(
        "cached(wid) present: {}; cached(bogus) present: {}",
        t.cached(wid).is_some(),
        t.cached(0xFFFF_FF00).is_some()
    ));
    for i in 0..3 {
        let t0 = Instant::now();
        let r = t.capture_sync_with_backend(wid, max_w, max_h);
        let d = t0.elapsed();
        match r {
            Some((img, b)) => {
                let path = format!("{outdir}/selftest_sync.png");
                let _ = write_png(&img, &path);
                let sz = img.size();
                out.line(&format!(
                    "capture_sync #{i}: {b:?} in {} pt={:.0}x{:.0} {}",
                    ms(d),
                    sz.width,
                    sz.height,
                    stats(&img)
                ));
            }
            None => out.line(&format!("capture_sync #{i}: None in {}", ms(d))),
        }
    }
    let t0 = Instant::now();
    let r = t.capture_sync(0xFFFF_FF00, max_w, max_h);
    out.line(&format!(
        "capture_sync(bogus) is_none={} in {}",
        r.is_none(),
        ms(t0.elapsed())
    ));
    // Concurrency: 6 parallel captures of the same window.
    let n = Arc::new(Mutex::new(0usize));
    let n2 = n.clone();
    let started = Instant::now();
    let last = Arc::new(Mutex::new(Duration::ZERO));
    let l2 = last.clone();
    for k in 0..6u64 {
        let n3 = n2.clone();
        let l3 = l2.clone();
        t.capture(vec![wid], max_w, max_h, 200 + k, move |_, _, _| {
            *n3.lock().unwrap() += 1;
            *l3.lock().unwrap() = started.elapsed();
        });
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && *n.lock().unwrap() < 6 {
        CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.01, true);
    }
    out.line(&format!(
        "6 concurrent captures: {} delivered, last at +{}",
        n.lock().unwrap(),
        ms(*last.lock().unwrap())
    ));
    // Minimized own window.
    win.miniaturize(None);
    let until = Instant::now() + Duration::from_millis(1200);
    while Instant::now() < until {
        CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.05, false);
    }
    out.line(&format!("own window minimized: {}", win.isMiniaturized()));
    let t2 = thumbs::Thumbnailer::new_own_process_only();
    let t0 = Instant::now();
    match t2.capture_sync(wid, max_w, max_h) {
        Some(img) => {
            let _ = write_png(&img, &format!("{outdir}/selftest_minimized.png"));
            out.line(&format!(
                "minimized capture_sync: Some in {} {}",
                ms(t0.elapsed()),
                stats(&img)
            ));
        }
        None => out.line(&format!("minimized capture_sync: None in {}", ms(t0.elapsed()))),
    }
    let got = Arc::new(Mutex::new(0));
    let g2 = got.clone();
    t2.capture(vec![wid], max_w, max_h, 300, move |_, _, _| {
        *g2.lock().unwrap() += 1
    });
    let until = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < until {
        CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.05, false);
    }
    out.line(&format!("minimized async callbacks: {}", got.lock().unwrap()));
    win.close();
}

// ---------------------------------------------------------------------------------------------
// --refresh

/// Main run-loop heartbeat: the longest gap between loop iterations, i.e. the longest time the
/// main thread spent away from its run loop (callbacks, our own calls).
struct Heartbeat {
    last: Instant,
    max_gap: Duration,
    over_50: usize,
    /// Highest `in_flight` seen by sampling between iterations.
    max_in_flight: usize,
    phase: &'static str,
    max_phase: &'static str,
}

impl Heartbeat {
    fn new() -> Self {
        Heartbeat {
            last: Instant::now(),
            max_gap: Duration::ZERO,
            over_50: 0,
            max_in_flight: 0,
            phase: "start",
            max_phase: "",
        }
    }
    fn spin(&mut self, t: &thumbs::Thumbnailer, dur: Duration) {
        let until = Instant::now() + dur;
        while Instant::now() < until {
            CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.005, true);
            self.beat();
            self.max_in_flight = self.max_in_flight.max(t.stats().in_flight);
        }
    }
    fn beat(&mut self) {
        let gap = self.last.elapsed();
        if gap > self.max_gap {
            self.max_gap = gap;
            self.max_phase = self.phase;
        }
        if gap > Duration::from_millis(50) {
            self.over_50 += 1;
        }
        self.last = Instant::now();
    }
}

fn cpu_time() -> Duration {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, (t.tv_usec as u32) * 1000);
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

fn ps_cpu() -> String {
    std::process::Command::new("/bin/ps")
        .args(["-o", "%cpu=,time=", "-p", &std::process::id().to_string()])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|e| e.to_string())
}

/// Timed main-thread call.
fn timed(lat: &mut Duration, f: impl FnOnce()) {
    let t0 = Instant::now();
    f();
    *lat = (*lat).max(t0.elapsed());
}

fn us(d: Duration) -> String {
    format!("{:.0}us", d.as_secs_f64() * 1e6)
}

fn refresh_mode(outdir: &str, out: &mut Out) {
    let wins = list_windows(false);
    let wids: Vec<u32> = wins.iter().map(|w| w.wid).collect();
    out.line(&format!("REFRESH: on-screen layer-0 windows: {}", wids.len()));
    let t = thumbs::Thumbnailer::new();
    let updates = Arc::new(Mutex::new(Vec::<u32>::new()));
    let off_main = Arc::new(Mutex::new(0usize));
    {
        let u = updates.clone();
        let om = off_main.clone();
        t.set_on_update(move |wid| {
            if MainThreadMarker::new().is_none() {
                *om.lock().unwrap() += 1;
            }
            u.lock().unwrap().push(wid);
        });
    }
    let mut hb = Heartbeat::new();
    let mut lat_refresh = Duration::ZERO;
    let mut lat_first = Duration::ZERO;
    let mut lat_other = Duration::ZERO;
    let max_age = Duration::from_secs(30);
    let tick = Duration::from_millis(500);
    let budget = 4;

    hb.phase = "warm";
    // ---- phase 1: warm the cache, refresh_stale(budget 4) every 500 ms ----
    let cpu0 = cpu_time();
    let t_start = Instant::now();
    let mut warm_after = None;
    let mut n = 0;
    while t_start.elapsed() < Duration::from_secs(20) {
        let t0 = Instant::now();
        t.refresh_stale(&wids, max_age, budget);
        let d = t0.elapsed();
        if n == 0 {
            lat_first = d;
        }
        lat_refresh = lat_refresh.max(d);
        hb.beat();
        n += 1;
        hb.spin(&t, tick);
        let cached = wids.iter().filter(|w| t.has_cached(**w)).count();
        let s = t.stats();
        out.line(&format!(
            "  tick {n:2} +{:6} call={:>6} cached {cached}/{} started={} ok={} failed={} fetches={} in_flight={} queued={} peak={}",
            ms(t_start.elapsed()),
            us(d),
            wids.len(),
            s.started,
            s.ok,
            s.failed,
            s.content_fetches,
            s.in_flight,
            s.queued,
            s.peak_in_flight
        ));
        if s.in_flight == 0 && s.queued == 0 && warm_after.is_none() {
            // Everything that could be captured has been (failures are backing off).
            let pending = wids
                .iter()
                .filter(|w| !t.has_cached(**w))
                .count();
            if pending == 0 || s.started >= wids.len() as u64 {
                warm_after = Some(t_start.elapsed());
                break;
            }
        }
    }
    let warm_wall = t_start.elapsed();
    let warm_cpu = cpu_time() - cpu0;
    let s1 = t.stats();
    let cached_n = wids.iter().filter(|w| t.has_cached(**w)).count();
    out.line(&format!(
        "WARM: {cached_n}/{} cached after {:?} ({n} calls); cpu {} over wall {} = {:.1}% ; stats {s1:?}",
        wids.len(),
        warm_after.map(ms),
        ms(warm_cpu),
        ms(warm_wall),
        100.0 * warm_cpu.as_secs_f64() / warm_wall.as_secs_f64()
    ));
    let missing: Vec<u32> = wids.iter().copied().filter(|w| !t.has_cached(*w)).collect();
    out.line(&format!("  not cached: {missing:?}"));
    out.line(&format!("  ps %cpu,time after warm: {}", ps_cpu()));
    hb.last = Instant::now(); // ps runs synchronously on main: not a thumbs stall

    hb.phase = "within-max-age";
    // ---- phase 2: calls within max_age must capture nothing and fetch nothing ----
    let s0 = t.stats();
    let cpu0 = cpu_time();
    let t0w = Instant::now();
    for _ in 0..10 {
        let t0 = Instant::now();
        t.refresh_stale(&wids, max_age, budget);
        lat_refresh = lat_refresh.max(t0.elapsed());
        hb.beat();
        hb.spin(&t, tick);
    }
    let s2 = t.stats();
    let idle_cpu = cpu_time() - cpu0;
    out.line(&format!(
        "WITHIN MAX_AGE: 10 calls over {}: captures started {} (expect 0), content fetches {} (expect 0); cpu {} = {:.2}%",
        ms(t0w.elapsed()),
        s2.started - s0.started,
        s2.content_fetches - s0.content_fetches,
        ms(idle_cpu),
        100.0 * idle_cpu.as_secs_f64() / t0w.elapsed().as_secs_f64()
    ));

    hb.phase = "capture_one";
    // ---- phase 3: capture_one refreshes one window now ----
    if let Some(&w0) = wids.iter().find(|w| t.has_cached(**w)) {
        let before = t.cache_age(w0);
        let s0 = t.stats();
        timed(&mut lat_other, || t.capture_one(w0));
        hb.beat();
        hb.spin(&t, Duration::from_millis(600));
        let s1 = t.stats();
        out.line(&format!(
            "CAPTURE_ONE wid={w0}: age before {:?} after {:?}; started +{} ok +{}; on_update count for it {}",
            before.map(ms),
            t.cache_age(w0).map(ms),
            s1.started - s0.started,
            s1.ok - s0.ok,
            updates.lock().unwrap().iter().filter(|w| **w == w0).count()
        ));
        // Twice in a row while the first is in flight: one capture.
        let s0 = t.stats();
        timed(&mut lat_other, || t.capture_one(w0));
        timed(&mut lat_other, || t.capture_one(w0));
        hb.beat();
        hb.spin(&t, Duration::from_millis(600));
        out.line(&format!(
            "CAPTURE_ONE x2 back-to-back: started +{} (expect 1, or 2 if the first finished before the second was queued)",
            t.stats().started - s0.started
        ));
    }

    hb.phase = "bogus";
    // ---- phase 4: a gone window is not retried in a storm ----
    let bogus = 0xFFFF_FF00u32;
    let s0 = t.stats();
    for _ in 0..8 {
        timed(&mut lat_refresh, || t.refresh_stale(&[bogus], Duration::ZERO, budget));
        hb.beat();
        hb.spin(&t, tick);
    }
    let s1 = t.stats();
    out.line(&format!(
        "BOGUS wid: 8 calls over 4 s -> started +{} failed +{} fetches +{} (expect 1 / 1 / <=2); has_cached={}",
        s1.started - s0.started,
        s1.failed - s0.failed,
        s1.content_fetches - s0.content_fetches,
        t.has_cached(bogus)
    ));

    hb.phase = "continuous";
    // ---- phase 5: everything stale every call (max_age 0) -> bounded concurrency, CPU ----
    t.reset_peak();
    hb.max_in_flight = 0;
    let s0 = t.stats();
    let cpu0 = cpu_time();
    let t0w = Instant::now();
    for _ in 0..10 {
        timed(&mut lat_refresh, || t.refresh_stale(&wids, Duration::ZERO, budget));
        hb.beat();
        hb.spin(&t, tick);
    }
    let wall = t0w.elapsed();
    let cpu = cpu_time() - cpu0;
    let s1 = t.stats();
    out.line(&format!(
        "CONTINUOUS (max_age 0, budget 4, 500 ms): started +{} ok +{} failed +{} fetches +{}; peak in flight {} (scheduler) / {} (sampled), limit 3; cpu {} over {} = {:.1}%; ps {}",
        s1.started - s0.started,
        s1.ok - s0.ok,
        s1.failed - s0.failed,
        s1.content_fetches - s0.content_fetches,
        s1.peak_in_flight,
        hb.max_in_flight,
        ms(cpu),
        ms(wall),
        100.0 * cpu.as_secs_f64() / wall.as_secs_f64(),
        ps_cpu()
    ));
    hb.last = Instant::now(); // ps runs synchronously on main: not a thumbs stall
    hb.spin(&t, Duration::from_millis(500));

    hb.phase = "capture()";
    // ---- phase 6: switcher captures (capture()) through the scheduler ----
    for (label, int_limit) in [("limit 3", 3usize), ("limit 8", 8), ("limit 3 again", 3)] {
        t.set_concurrency(3, int_limit);
        t.reset_peak();
        let got = Arc::new(Mutex::new(Vec::<(u32, Duration)>::new()));
        let g2 = got.clone();
        let started = Instant::now();
        timed(&mut lat_other, || {
            t.capture(wids.clone(), 300.0, 190.0, 1, move |_, wid, _| {
                g2.lock().unwrap().push((wid, started.elapsed()))
            })
        });
        hb.beat();
        let until = Instant::now() + Duration::from_secs(5);
        while Instant::now() < until && got.lock().unwrap().len() < cached_n {
            hb.spin(&t, Duration::from_millis(5));
        }
        let g = got.lock().unwrap();
        let first = g.first().map(|x| x.1).unwrap_or_default();
        let last = g.iter().map(|x| x.1).max().unwrap_or_default();
        out.line(&format!(
            "CAPTURE() {label}: {} of {} delivered, first +{} last +{}; peak in flight {}",
            g.len(),
            wids.len(),
            ms(first),
            ms(last),
            t.stats().peak_in_flight
        ));
        drop(g);
        hb.spin(&t, Duration::from_millis(300));
    }
    t.set_concurrency(3, 3);

    hb.phase = "priority";
    // ---- phase 7: capture() for a selected window while refresh work is queued ----
    {
        t.reset_peak();
        // Queue a full background round, then ask for the last window interactively.
        t.refresh_stale(&wids, Duration::ZERO, wids.len());
        let target = *wids.last().unwrap();
        let got = Arc::new(Mutex::new(None));
        let g2 = got.clone();
        let started = Instant::now();
        t.capture(vec![target], 300.0, 190.0, 2, move |_, _, _| {
            *g2.lock().unwrap() = Some(started.elapsed())
        });
        let until = Instant::now() + Duration::from_secs(5);
        while Instant::now() < until && got.lock().unwrap().is_none() {
            hb.spin(&t, Duration::from_millis(5));
        }
        out.line(&format!(
            "PRIORITY: interactive capture behind a {}-window background round delivered at +{:?}; peak in flight {}",
            wids.len(),
            got.lock().unwrap().map(ms),
            t.stats().peak_in_flight
        ));
        let until = Instant::now() + Duration::from_secs(5);
        while Instant::now() < until && (t.stats().in_flight > 0 || t.stats().queued > 0) {
            hb.spin(&t, Duration::from_millis(20));
        }
    }

    // ---- phase 8: main-thread cost of cache reads ----
    let mut lat_cached = Duration::ZERO;
    for w in &wids {
        timed(&mut lat_cached, || {
            let _ = t.cached(*w);
        });
        timed(&mut lat_cached, || {
            let _ = t.has_cached(*w);
        });
        timed(&mut lat_cached, || {
            let _ = t.cache_age(*w);
        });
    }

    hb.phase = "retain_only";
    // ---- phase 9: retain_only keeps listed windows ----
    let before = wids.iter().filter(|w| t.has_cached(**w)).count();
    t.retain_only(&wids);
    let after_all = wids.iter().filter(|w| t.has_cached(**w)).count();
    let w0 = wids[0];
    t.retain_only(&wids[1..]);
    let after_drop = wids[1..].iter().filter(|w| t.has_cached(**w)).count();
    out.line(&format!(
        "RETAIN_ONLY: cached {before}; after retain_only(all) {after_all}; after retain_only(all but {w0}): {w0} cached={} others {after_drop}/{}",
        t.has_cached(w0),
        wids.len() - 1
    ));
    // An in-flight capture of a window dropped meanwhile must not re-add it.
    let ok0 = t.stats().ok;
    t.capture_one(w0);
    let until = Instant::now() + Duration::from_millis(200);
    while Instant::now() < until && t.stats().in_flight == 0 {
        std::hint::spin_loop();
    }
    let was_in_flight = t.stats().in_flight > 0;
    t.retain_only(&wids[1..]);
    hb.last = Instant::now();
    hb.spin(&t, Duration::from_millis(500));
    out.line(&format!(
        "RETAIN_ONLY during in-flight capture (in flight at retain: {was_in_flight}, capture ok +{}): {w0} cached={} (expect false)",
        t.stats().ok - ok0,
        t.has_cached(w0)
    ));

    out.line(&format!(
        "MAIN THREAD: refresh_stale max {} (first call {}), capture/capture_one max {}, cached/has_cached/cache_age max {}",
        us(lat_refresh),
        us(lat_first),
        us(lat_other),
        us(lat_cached)
    ));
    out.line(&format!(
        "HEARTBEAT: max main-loop gap {} (phase {}) ; gaps > 50 ms: {} ; on_update callbacks {} ({} off main)",
        ms(hb.max_gap),
        hb.max_phase,
        hb.over_50,
        updates.lock().unwrap().len(),
        off_main.lock().unwrap()
    ));
    let allowed = std::thread::spawn(thumbs::screen_capture_allowed).join().unwrap_or(false);
    out.line(&format!("screen_capture_allowed (checked off main) = {allowed}; final stats {:?}", t.stats()));

    // PNGs of a few cached thumbnails (after measurements: encoding is slow).
    for w in wins.iter().filter(|w| t.has_cached(w.wid)).take(4) {
        if let Some(img) = t.cached(w.wid) {
            let path = format!("{outdir}/refresh_{}.png", w.wid);
            let r = write_png(&img, &path);
            out.line(&format!(
                "  png wid={} owner={:?} {:?} {} -> {path}",
                w.wid,
                w.owner,
                r,
                stats(&img)
            ));
        }
    }
}
