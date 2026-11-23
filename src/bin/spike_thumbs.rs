//! Spike for src/thumbs.rs.
//! Usage: spike_thumbs <outdir> [max_w max_h]
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

    // Hard watchdog: `open -W` must never hang.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(20));
        eprintln!("watchdog exit");
        std::process::exit(3);
    });

    let mtm = MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);

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
