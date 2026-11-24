//! Spike for src/ui.rs: builds fake tiles, shows the switcher, writes PNG snapshots,
//! drives synthetic mouse events through the panel and checks focus is not stolen.
//! Usage: spike_ui <outdir>
#[path = "../ui.rs"]
mod ui;

use std::cell::RefCell;
use std::fmt::Write as _;
use std::rc::Rc;
use std::time::Instant;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::Bool;
use objc2::{AllocAnyThread, MainThreadMarker};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationOptions, NSApplicationActivationPolicy, NSRunningApplication, NSBezierPath, NSColor, NSEvent,
    NSEventMask, NSEventModifierFlags, NSEventType, NSScreen, NSWindow, NSFont, NSFontAttributeName, NSForegroundColorAttributeName,
    NSGraphicsContext, NSImage, NSStringDrawing, NSWorkspace,
};
use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGColorSpace, CGEvent, CGEventTapLocation,
    CGEventType, CGMouseButton, CGWarpMouseCursorPosition,
};
use objc2_foundation::{NSDefaultRunLoopMode, NSDate, NSDictionary, NSPoint, NSRect, NSRunLoop, NSSize, NSString};
use objc2::runtime::AnyObject;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
}
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
}

fn spin(secs: f64) {
    NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(secs));
}

/// Synthetic window thumbnail: title bar with traffic lights + big label at the TOP,
/// so a vertical flip would be obvious in the PNG.
fn fake_thumb(w: f64, h: f64, hue: (f64, f64, f64), label: &str, dark: bool) -> Retained<NSImage> {
    let label = label.to_string();
    let block = RcBlock::new(move |rect: NSRect| -> Bool {
        let (bg, fg) = if dark { (0.13, 0.9) } else { (0.97, 0.15) };
        NSColor::colorWithWhite_alpha(bg, 1.0).setFill();
        NSBezierPath::fillRect(rect);
        let bar = 0.09 * h;
        NSColor::colorWithSRGBRed_green_blue_alpha(hue.0, hue.1, hue.2, 1.0).setFill();
        NSBezierPath::fillRect(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, bar)));
        for (k, c) in [(1.0, 0.37, 0.34), (1.0, 0.74, 0.18), (0.16, 0.79, 0.25)].iter().enumerate() {
            NSColor::colorWithSRGBRed_green_blue_alpha(c.0, c.1, c.2, 1.0).setFill();
            let d = bar * 0.42;
            NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
                NSPoint::new(bar * 0.4 + k as f64 * d * 1.6, (bar - d) / 2.0),
                NSSize::new(d, d),
            ))
            .fill();
        }
        // Sidebar + content lines.
        NSColor::colorWithSRGBRed_green_blue_alpha(hue.0, hue.1, hue.2, 0.12).setFill();
        NSBezierPath::fillRect(NSRect::new(NSPoint::new(0.0, bar), NSSize::new(w * 0.22, h - bar)));
        NSColor::colorWithWhite_alpha(fg, 0.18).setFill();
        let mut y = bar + 0.22 * h;
        while y < h - 0.08 * h {
            NSBezierPath::fillRect(NSRect::new(NSPoint::new(w * 0.27, y), NSSize::new(w * 0.6, 0.025 * h)));
            y += 0.07 * h;
        }
        let font = NSFont::boldSystemFontOfSize(0.11 * h);
        let color = NSColor::colorWithWhite_alpha(fg, 1.0);
        let f: &AnyObject = &font;
        let c: &AnyObject = &color;
        let keys = unsafe { [NSFontAttributeName, NSForegroundColorAttributeName] };
        let attrs = NSDictionary::from_slices(&keys, &[f, c]);
        unsafe { NSString::from_str(&label).drawAtPoint_withAttributes(NSPoint::new(w * 0.27, bar + 0.04 * h), Some(&attrs)) };
        Bool::YES
    });
    let vector = NSImage::imageWithSize_flipped_drawingHandler(NSSize::new(w, h), true, &block);
    // Rasterize into a real BGRA bitmap (like a ScreenCaptureKit capture), so the
    // thumbnail is not re-rendered lazily on first draw.
    let (pw, ph) = (w as usize, h as usize);
    let cs = CGColorSpace::with_name(Some(unsafe { objc2_core_graphics::kCGColorSpaceSRGB }));
    let info = 2 /* PremultipliedFirst */ | (2 << 12) /* ByteOrder32Little */;
    let ctx = unsafe { CGBitmapContextCreate(std::ptr::null_mut(), pw, ph, 8, 0, cs.as_deref(), info) }
        .expect("bitmap ctx");
    let ns = NSGraphicsContext::graphicsContextWithCGContext_flipped(&ctx, false);
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&ns));
    vector.drawInRect(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h)));
    NSGraphicsContext::restoreGraphicsState_class();
    let cg = CGBitmapContextCreateImage(Some(&ctx)).expect("cgimage");
    NSImage::initWithCGImage_size(NSImage::alloc(), &cg, NSSize::new(w, h))
}

fn main() {
    let outdir = std::env::args().nth(1).unwrap_or_else(|| "dist/spike_ui".into());
    std::fs::create_dir_all(&outdir).ok();
    let mut log = String::new();
    macro_rules! out {
        ($($t:tt)*) => {{ let s = format!($($t)*); println!("{s}"); let _ = writeln!(log, "{s}"); }};
    }
    out!("AXIsProcessTrusted={} CGPreflightScreenCaptureAccess={}", unsafe { AXIsProcessTrusted() }, unsafe {
        CGPreflightScreenCaptureAccess()
    });

    let mtm = MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();

    let ws = NSWorkspace::sharedWorkspace();
    let icon = |p: &str| Some(ws.iconForFile(&NSString::from_str(p)));
    let mail = icon("/System/Applications/Mail.app");
    let notes = icon("/System/Applications/Notes.app");
    let cal = icon("/System/Applications/Calendar.app");
    let textedit = icon("/System/Applications/TextEdit.app");
    let music = icon("/System/Applications/Music.app");
    let settings = icon("/System/Applications/System Settings.app");

    let blue = (0.20, 0.47, 0.96);
    let yellow = (0.98, 0.78, 0.18);
    let red = (0.93, 0.27, 0.27);
    let gray = (0.55, 0.55, 0.58);
    let tile = |app: &str, title: &str, icon: &Option<Retained<NSImage>>, thumb: Option<Retained<NSImage>>, aspect: f64, min: bool, group: usize| ui::Tile {
        app_name: app.into(),
        title: title.into(),
        icon: icon.clone(),
        thumbnail: thumb,
        aspect,
        minimized: min,
        group,
        ..Default::default()
    };
    let tiles = vec![
        tile("Mail", "Inbox — 1,204 messages", &mail, Some(fake_thumb(1600.0, 1000.0, blue, "Inbox", false)), 1.6, false, 0),
        tile("Mail", "Re: Quarterly planning review and the very long subject line that must truncate", &mail, Some(fake_thumb(900.0, 700.0, blue, "Re: Q3", false)), 900.0 / 700.0, false, 0),
        tile("Mail", "Drafts", &mail, Some(fake_thumb(1400.0, 900.0, blue, "Drafts", true)), 1400.0 / 900.0, false, 0),
        tile("Mail", "Sent", &mail, None, 1.5, false, 0),
        tile("Notes", "Shopping list", &notes, Some(fake_thumb(800.0, 1000.0, yellow, "Shopping", false)), 0.8, false, 1),
        tile("Notes", "Ideas", &notes, Some(fake_thumb(1200.0, 800.0, yellow, "Ideas", true)), 1.5, false, 1),
        tile("Notes", "Old note", &notes, Some(fake_thumb(1200.0, 800.0, yellow, "Old", false)), 1.5, true, 1),
        tile("Calendar", "Calendar — November 2026", &cal, Some(fake_thumb(1800.0, 1100.0, red, "Nov 2026", false)), 1800.0 / 1100.0, false, 2),
        tile("Calendar", "", &cal, None, 1.6, true, 2),
        tile("TextEdit", "README.txt", &textedit, Some(fake_thumb(700.0, 900.0, gray, "README", false)), 700.0 / 900.0, false, 3),
        tile("TextEdit", "Untitled 2", &textedit, None, 1.3, false, 3),
        tile("TextEdit", "notes-from-meeting-2026-11-23-final-FINAL.txt", &textedit, Some(fake_thumb(2400.0, 900.0, gray, "Wide", true)), 2400.0 / 900.0, false, 3),
    ];
    let n = tiles.len();

    let clicks: Rc<RefCell<Vec<usize>>> = Rc::default();
    let hovers: Rc<RefCell<Vec<usize>>> = Rc::default();
    let (c2, h2) = (clicks.clone(), hovers.clone());
    let sw: Rc<RefCell<Option<Rc<ui::Switcher>>>> = Rc::default();
    let sw_for_hover = sw.clone();
    let switcher = Rc::new(ui::Switcher::new(
        mtm,
        Box::new(move |i| c2.borrow_mut().push(i)),
        // Like the integrator will: hover -> set_selected (re-entrant call into the switcher).
        Box::new(move |i| {
            h2.borrow_mut().push(i);
            if let Some(s) = sw_for_hover.borrow().as_ref() {
                s.set_selected(i);
            }
        }),
    ));
    *sw.borrow_mut() = Some(switcher.clone());
    let s = &*switcher;

    let front = |ws: &NSWorkspace| {
        ws.frontmostApplication()
            .map(|a| format!("{} pid={}", a.localizedName().map(|n| n.to_string()).unwrap_or_default(), a.processIdentifier()))
            .unwrap_or_else(|| "none".into())
    };
    let front_before = front(&ws);
    PREV_FRONT.with(|p| p.set(ws.frontmostApplication().map(|a| a.processIdentifier())));

    let t0 = Instant::now();
    s.show(tiles, 1);
    let dt_show12 = t0.elapsed();
    out!("show(12 tiles) took {:.2} ms (drawing happens at the following CA commit)", dt_show12.as_secs_f64() * 1e3);
    spin(0.5);
    let front_after = front(&ws);
    out!("frontmost before={front_before} after={front_after} unchanged={}", front_before == front_after);
    out!(
        "panel isKeyWindow={} is_visible={} app.isActive={} len={} selected={}",
        s.panel().isKeyWindow(),
        s.is_visible(),
        app.isActive(),
        s.len(),
        s.selected()
    );
    let l = s.layout();
    out!("layout {}x{} thumb_h={} rows={:?}", l.width, l.height, l.thumb_h, l.rows);

    s.set_dark(Some(false));
    let ok = s.snapshot_png(&format!("{outdir}/ui-light.png"));
    out!("snapshot ui-light.png ok={ok}");

    // Fill a missing thumbnail (idx 3 "Sent") before the dark snapshot.
    s.set_thumbnail(3, fake_thumb(1500.0, 1000.0, blue, "Sent", true), None);
    s.set_thumbnail(99, fake_thumb(10.0, 10.0, blue, "x", true), None); // out of range: ignored
    s.set_selected(7);
    s.set_dark(Some(true));
    let ok = s.snapshot_png(&format!("{outdir}/ui-dark.png"));
    out!("snapshot ui-dark.png ok={ok} (thumbnail 3 filled, selected=7)");
    s.set_dark(None);
    spin(0.3);

    // ---- Synthetic mouse events through the real window -> view path ----
    let l = s.layout();
    let wn = s.panel().windowNumber();
    let mk = |t: NSEventType, x: f64, y_flipped: f64| {
        NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            t,
            NSPoint::new(x, l.height - y_flipped), // window coords are bottom-left origin
            NSEventModifierFlags::empty(),
            0.0,
            wn,
            None,
            0,
            1,
            if t == NSEventType::MouseMoved { 0.0 } else { 1.0 },
        )
        .expect("event")
    };
    for (i, r) in l.tiles.iter().enumerate() {
        let mv = mk(NSEventType::MouseMoved, r.cx(), r.cy());
        s.view().mouseMoved(&mv);
        // Second move within the same tile must not fire again.
        let mv2 = mk(NSEventType::MouseMoved, r.cx() + 3.0, r.cy() + 3.0);
        s.view().mouseMoved(&mv2);
        assert_eq!(s.selected(), i, "hover->set_selected re-entrancy");
        s.panel().sendEvent(&mk(NSEventType::LeftMouseDown, r.cx(), r.cy()));
        s.panel().sendEvent(&mk(NSEventType::LeftMouseUp, r.cx(), r.cy()));
    }
    // Press on tile 0, release on tile 1: no click. Press/release in a gap: no click.
    let (a, b) = (l.tiles[0], l.tiles[1]);
    s.panel().sendEvent(&mk(NSEventType::LeftMouseDown, a.cx(), a.cy()));
    s.panel().sendEvent(&mk(NSEventType::LeftMouseUp, b.cx(), b.cy()));
    s.panel().sendEvent(&mk(NSEventType::LeftMouseDown, 3.0, 3.0));
    s.panel().sendEvent(&mk(NSEventType::LeftMouseUp, 3.0, 3.0));
    let expected: Vec<usize> = (0..n).collect();
    out!("clicks={:?} expected={:?} ok={}", clicks.borrow(), expected, *clicks.borrow() == expected);
    out!("hovers={:?} ok={}", hovers.borrow(), *hovers.borrow() == expected);
    out!("after events: isKeyWindow={} frontmost={}", s.panel().isKeyWindow(), front(&ws));

    // ---- Real window-server input (needs Accessibility; opt-in with --real-input) ----
    if std::env::args().any(|a| a == "--real-input") {
        real_input_test(&app, s, &clicks, &hovers, &ws, &mut log, mtm);
        let _ = front;
    }

    // ---- Up/Down navigation ----
    let mut nav = String::new();
    for i in 0..n {
        s.set_selected(i);
        let _ = write!(nav, "{i}:^{} v{}  ", s.index_above(), s.index_below());
    }
    out!("nav {nav}");
    s.set_selected(1000);
    out!("set_selected(1000) clamps -> {}", s.selected());

    // ---- On-screen capture (may need Screen Recording permission) ----
    s.set_selected(1);
    spin(0.4);
    let screen_png = format!("{outdir}/screen.png");
    let _ = std::fs::remove_file(&screen_png);
    let st = std::process::Command::new("screencapture")
        .args(["-x", "-o", "-l", &wn.to_string(), &screen_png])
        .output();
    let size = std::fs::metadata(&screen_png).map(|m| m.len()).unwrap_or(0);
    match st {
        Ok(o) => out!(
            "screencapture -l {wn}: status={} bytes={size} stderr={}",
            o.status,
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => out!("screencapture failed to run: {e}"),
    }

    let shadow_png = format!("{outdir}/screen-shadow.png");
    let ok = std::process::Command::new("screencapture")
        .args(["-x", "-l", &wn.to_string(), &shadow_png])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    out!("screencapture with shadow ok={ok}");

    // On-screen capture with the real HUD material in dark appearance.
    s.set_dark(Some(true));
    spin(0.3);
    let ok = std::process::Command::new("screencapture")
        .args(["-x", "-o", "-l", &wn.to_string(), &format!("{outdir}/screen-dark.png")])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    out!("screencapture dark ok={ok}");
    s.set_dark(None);

    // ---- Stress: 30 and 40 tiles ----
    let apps = [&mail, &notes, &cal, &textedit, &music, &settings];
    let names = ["Mail", "Notes", "Calendar", "TextEdit", "Music", "System Settings"];
    let hues = [blue, yellow, red, gray, (0.95, 0.25, 0.45), gray];
    let counts = [10, 8, 6, 5, 4, 3, 2, 1, 1];
    let make = |total: usize| {
        let mut v = Vec::new();
        let mut g = 0;
        'outer: for (gi, &c) in counts.iter().enumerate() {
            for k in 0..c {
                if v.len() == total {
                    break 'outer;
                }
                let a = gi % apps.len();
                let aspect = [1.6, 1.33, 0.75, 1.78, 2.4][(v.len()) % 5];
                let thumb = if v.len() % 7 == 3 {
                    None
                } else {
                    Some(fake_thumb(480.0 * aspect, 480.0, hues[a], &format!("W{}", v.len()), v.len() % 2 == 0))
                };
                v.push(ui::Tile {
                    app_name: names[a].into(),
                    title: format!("{} window {}", names[a], k + 1),
                    icon: apps[a].clone(),
                    thumbnail: thumb,
                    aspect,
                    minimized: v.len() % 11 == 5,
                    group: g,
                    ..Default::default()
                });
            }
            g += 1;
        }
        v
    };
    for total in [30, 40] {
        let t = make(total);
        let t0 = Instant::now();
        s.show(t, 0);
        let dt = t0.elapsed();
        let l = s.layout();
        out!(
            "show({total} tiles) took {:.2} ms; layout {}x{} thumb_h={} rows={}",
            dt.as_secs_f64() * 1e3,
            l.width,
            l.height,
            l.thumb_h,
            l.rows.len()
        );
        spin(0.1);
    }
    // Warm re-show timing (typical repeated Alt+Tab).
    let t = make(30);
    let t0 = Instant::now();
    s.show(t, 3);
    out!("re-show(30 tiles) took {:.2} ms", t0.elapsed().as_secs_f64() * 1e3);
    // Repeated timing (machine may be loaded): show() alone, and show() + one run-loop
    // pass (which includes the CA commit where drawRect runs).
    for total in [30, 40] {
        let mut a = Vec::new();
        let mut b = Vec::new();
        let warm = make(total);
        for k in 0..20 {
            s.hide();
            spin(0.02);
            // Even iterations: fresh images; odd: the same (already drawn) images.
            let t = if k % 2 == 0 { make(total) } else { warm.clone() };
            let t0 = Instant::now();
            s.show(t, 1);
            a.push(t0.elapsed().as_secs_f64() * 1e3);
            spin(0.0);
            b.push(t0.elapsed().as_secs_f64() * 1e3);
        }
        let cold: Vec<f64> = b.iter().step_by(2).copied().collect();
        let warm_b: Vec<f64> = b.iter().skip(1).step_by(2).copied().collect();
        out!("  show+commit cold-images {:?}", cold.iter().map(|v| v.round()).collect::<Vec<_>>());
        out!("  show+commit warm-images {:?}", warm_b.iter().map(|v| v.round()).collect::<Vec<_>>());
        a.sort_by(|x, y| x.partial_cmp(y).unwrap());
        b.sort_by(|x, y| x.partial_cmp(y).unwrap());
        out!(
            "x20 show({total}): min {:.2} med {:.2} max {:.2} ms | show+commit: min {:.2} med {:.2} max {:.2} ms",
            a[0], a[10], a[19], b[0], b[10], b[19]
        );
    }
    let t = make(40);
    s.show(t, 5);
    s.set_dark(Some(true));
    spin(0.3);
    let ok = std::process::Command::new("screencapture")
        .args(["-x", "-o", "-l", &wn.to_string(), &format!("{outdir}/screen-stress-dark.png")])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    out!("screencapture stress dark ok={ok}");
    out!("snapshot ui-stress.png ok={}", s.snapshot_png(&format!("{outdir}/ui-stress.png")));
    s.set_dark(Some(false));
    out!("snapshot ui-stress-light.png ok={}", s.snapshot_png(&format!("{outdir}/ui-stress-light.png")));

    // Cold first draw with large (Retina-window-sized) thumbnails: 30 tiles x 1920x1200 px.
    for (pw, ph) in [(1920.0, 1200.0), (640.0, 400.0)] {
        let mut v = make(30);
        for t in v.iter_mut() {
            if t.thumbnail.is_some() {
                t.thumbnail = Some(fake_thumb(pw, ph, blue, "Big", false));
            }
            t.aspect = pw / ph;
        }
        s.hide();
        spin(0.05);
        let t0 = Instant::now();
        s.show(v, 0);
        let a = t0.elapsed().as_secs_f64() * 1e3;
        spin(0.0);
        let b = t0.elapsed().as_secs_f64() * 1e3;
        out!("cold 30 tiles @{pw}x{ph}px thumbs: show {a:.2} ms, show+commit {b:.2} ms");
    }

    // Empty show hides.
    s.show(Vec::new(), 0);
    out!("after show([]): visible={} len={}", s.is_visible(), s.len());
    s.show(make(3), 0);
    s.hide();
    out!("after hide: visible={} selected={} len={}", s.is_visible(), s.selected(), s.len());

    std::fs::write(format!("{outdir}/out.txt"), &log).ok();
}

/// Dispatch real NSEvents for `secs` (runUntilDate does not dispatch input events).
fn pump(app: &NSApplication, secs: f64) {
    let until = NSDate::dateWithTimeIntervalSinceNow(secs);
    loop {
        let ev = app.nextEventMatchingMask_untilDate_inMode_dequeue(
            NSEventMask::Any,
            Some(&until),
            unsafe { NSDefaultRunLoopMode },
            true,
        );
        match ev {
            Some(e) => app.sendEvent(&e),
            None => break,
        }
    }
}

thread_local! {
    static PREV_FRONT: std::cell::Cell<Option<i32>> = const { std::cell::Cell::new(None) };
}

fn real_input_test(
    app: &NSApplication,
    s: &ui::Switcher,
    clicks: &Rc<RefCell<Vec<usize>>>,
    hovers: &Rc<RefCell<Vec<usize>>>,
    ws: &NSWorkspace,
    log: &mut String,
    mtm: MainThreadMarker,
) {
    macro_rules! out {
        ($($t:tt)*) => {{ let s = format!($($t)*); println!("{s}"); let _ = writeln!(log, "{s}"); }};
    }
    if !unsafe { AXIsProcessTrusted() } {
        out!("real-input: skipped, AXIsProcessTrusted=false");
        return;
    }
    s.set_selected(1);
    let active0 = app.isActive();
    pump(app, 0.3);
    // A LaunchServices launch activates the app on its first event pump; undo that so
    // we can check that real hover/click on the panel do not activate us.
    if let Some(prev) = PREV_FRONT.with(|p| p.get()).and_then(NSRunningApplication::runningApplicationWithProcessIdentifier) {
        app.yieldActivationToApplication(&prev);
        prev.activateWithOptions(NSApplicationActivationOptions::empty());
    }
    pump(app, 0.5);
    let active1 = app.isActive();
    clicks.borrow_mut().clear();
    hovers.borrow_mut().clear();
    let l = s.layout();
    let pf = s.panel().frame();
    let wn = s.panel().windowNumber();
    let main_h = NSScreen::screens(mtm).objectAtIndex(0).frame().size.height;
    // Tile centre -> Cocoa global (bottom-left) and CG global (top-left) coordinates.
    let at = |i: usize| {
        let r = l.tiles[i];
        let cocoa = NSPoint::new(pf.origin.x + r.cx(), pf.origin.y + pf.size.height - r.cy());
        (cocoa, CGPoint::new(cocoa.x, main_h - cocoa.y))
    };
    let (c3, g3) = at(3);
    let (c4, g4) = at(4);
    let top3 = NSWindow::windowNumberAtPoint_belowWindowWithWindowNumber(c3, 0, mtm);
    let top4 = NSWindow::windowNumberAtPoint_belowWindowWithWindowNumber(c4, 0, mtm);
    if top3 != wn || top4 != wn {
        out!("real-input: skipped, panel not topmost at tile centres ({top3}, {top4} vs {wn})");
        return;
    }
    let saved = CGEvent::location(CGEvent::new(None).as_deref());
    let front_before = ws.frontmostApplication().map(|a| a.processIdentifier());
    let post = |t: CGEventType, p: CGPoint| {
        let e = CGEvent::new_mouse_event(None, t, p, CGMouseButton::Left);
        CGEvent::post(CGEventTapLocation::HIDEventTap, e.as_deref());
    };
    post(CGEventType::MouseMoved, g3);
    pump(app, 0.25);
    post(CGEventType::MouseMoved, CGPoint::new(g3.x + 4.0, g3.y + 2.0)); // same tile: no new hover
    pump(app, 0.15);
    post(CGEventType::MouseMoved, g4);
    pump(app, 0.25);
    let sel_after_hover = s.selected();
    let active2 = app.isActive();
    post(CGEventType::LeftMouseDown, g4);
    pump(app, 0.1);
    post(CGEventType::LeftMouseUp, g4);
    pump(app, 0.3);
    let front_after = ws.frontmostApplication().map(|a| a.processIdentifier());
    out!(
        "real-input: isActive before-pump={active0} after-handback={active1} after-hover={active2} after-click={} frontmost={:?} self_pid={}",
        app.isActive(),
        ws.frontmostApplication().and_then(|a| a.localizedName()).map(|n| n.to_string()),
        std::process::id()
    );
    let _ = CGWarpMouseCursorPosition(saved);
    out!(
        "real-input: hovers={:?} (expect [3, 4]) selected_after_hover={} clicks={:?} (expect [4]) \
         isKeyWindow={} app.isActive={} frontmost_unchanged={} cursor restored to ({:.0},{:.0})",
        hovers.borrow(),
        sel_after_hover,
        clicks.borrow(),
        s.panel().isKeyWindow(),
        app.isActive(),
        front_before == front_after,
        saved.x,
        saved.y
    );
}
