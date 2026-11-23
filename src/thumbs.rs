//! Window thumbnails.
//!
//! Primary path: ScreenCaptureKit (`SCShareableContent` -> `SCContentFilter` for one
//! desktop-independent window -> `SCScreenshotManager captureImage...`). Captures run
//! concurrently on ScreenCaptureKit's own queues; results are stored as `CGImage`
//! (thread-safe) in a shared cache and handed to the caller as `NSImage` on the main queue.
//!
//! Secondary path: `CGWindowListCreateImage` (deprecated, resolved at runtime with `dlsym`
//! so a missing symbol degrades to "unavailable" instead of a link failure).
//!
//! Every capture path is gated on `CGPreflightScreenCaptureAccess()`; without the Screen
//! Recording grant nothing is captured, no callback fires and no system prompt is raised.
//! This module never calls `CGRequestScreenCaptureAccess`.
#![allow(dead_code)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::{DispatchQoS, DispatchQueue, GlobalQueueIdentifier};
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_app_kit::NSImage;
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGColorSpace, CGContext, CGImage, CGImageAlphaInfo,
    CGImageByteOrderInfo, CGInterpolationQuality, CGPreflightScreenCaptureAccess, CGWindowImageOption,
    CGWindowListOption,
};
use objc2_foundation::{NSError, NSSize};
use objc2_screen_capture_kit::{
    SCContentFilter, SCScreenshotManager, SCShareableContent, SCStreamConfiguration, SCWindow,
};

/// How long a fetched `SCShareableContent` is reused before refetching.
const CONTENT_MAX_AGE: Duration = Duration::from_millis(1500);
/// When a requested window is missing from content older than this, refetch once.
const CONTENT_REFETCH_IF_OLDER: Duration = Duration::from_millis(150);
/// Timeout for `capture_sync`'s ScreenCaptureKit path.
const SYNC_TIMEOUT: Duration = Duration::from_millis(1500);
/// An SCShareableContent fetch older than this is considered lost and is retried.
const FETCH_STUCK_AFTER: Duration = Duration::from_secs(3);
/// Pixels per point used for rendered thumbnails (Retina).
const PIXEL_SCALE: f64 = 2.0;

/// A cached thumbnail: the image plus its size in points.
#[derive(Clone)]
struct Thumb {
    image: CFRetained<CGImage>,
    w_pt: f64,
    h_pt: f64,
}

/// `SCShareableContent` is an immutable snapshot; we only read from it.
#[derive(Clone)]
struct SendContent(Retained<SCShareableContent>);
// SAFETY: SCShareableContent is an immutable value object; ScreenCaptureKit itself hands it
// to arbitrary queues. We never mutate it.
unsafe impl Send for SendContent {}
unsafe impl Sync for SendContent {}

type ContentWaiter = Box<dyn FnOnce(Option<(SendContent, Instant)>) + Send>;

#[derive(Default)]
struct ContentState {
    content: Option<(SendContent, Instant)>,
    /// When the in-flight fetch started (None = no fetch in flight).
    fetching: Option<Instant>,
    waiters: Vec<ContentWaiter>,
}

#[derive(Default)]
struct Shared {
    cache: Mutex<HashMap<u32, Thumb>>,
    content: Mutex<ContentState>,
    /// Test mode: only this process's own windows via
    /// `getCurrentProcessShareableContent` (needs no Screen Recording grant).
    own_process_only: bool,
}

type OnImage = dyn Fn(u64, u32, Retained<NSImage>) + Send + Sync + 'static;

/// Window thumbnailer. Create and use on the main thread; capture work happens on
/// ScreenCaptureKit / GCD background queues and results come back on the main queue.
pub struct Thumbnailer {
    shared: Arc<Shared>,
}

impl Default for Thumbnailer {
    fn default() -> Self {
        Self::new()
    }
}

/// True when this process holds the Screen Recording grant. Never prompts.
///
/// `CGPreflightScreenCaptureAccess` costs ~4-5 ms per call, so a positive answer is cached
/// for the life of the process (a revoked grant only takes effect on relaunch anyway) and a
/// negative one is re-checked at most once per second.
pub fn screen_capture_allowed() -> bool {
    static GRANTED: AtomicBool = AtomicBool::new(false);
    static LAST_CHECK: Mutex<Option<Instant>> = Mutex::new(None);
    if GRANTED.load(Ordering::Relaxed) {
        return true;
    }
    if let Ok(mut last) = LAST_CHECK.lock() {
        if let Some(t) = *last
            && t.elapsed() < Duration::from_secs(1)
        {
            return false;
        }
        *last = Some(Instant::now());
    }
    let ok = CGPreflightScreenCaptureAccess();
    if ok {
        GRANTED.store(true, Ordering::Relaxed);
    }
    ok
}

/// Which backend produced (or would produce) an image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    ScreenCaptureKit,
    CGWindowList,
}

impl Thumbnailer {
    pub fn new() -> Self {
        Thumbnailer {
            shared: Arc::new(Shared::default()),
        }
    }

    /// Test/diagnostic constructor: captures only this process's OWN windows, using
    /// `SCShareableContent getCurrentProcessShareableContent` (macOS 14.4+), which needs no
    /// Screen Recording grant. Exercises the same filter/config/screenshot/cache/main-queue
    /// path as `new()`. The CGWindowList fallback is disabled in this mode.
    #[doc(hidden)]
    pub fn new_own_process_only() -> Self {
        Thumbnailer {
            shared: Arc::new(Shared {
                own_process_only: true,
                ..Shared::default()
            }),
        }
    }

    fn allowed(&self) -> bool {
        self.shared.own_process_only || screen_capture_allowed()
    }

    /// Asynchronously capture each window (CGWindowID) scaled to fit `max_w` x `max_h` points,
    /// rendered at 2x pixels. For each window that captures successfully, `on_image(generation,
    /// wid, image)` is invoked on the main thread (main dispatch queue). Returns immediately.
    /// Windows that cannot be captured (minimized, gone, no permission) produce no callback.
    /// The internal cache is updated before the callback runs.
    pub fn capture(
        &self,
        wids: Vec<u32>,
        max_w: f64,
        max_h: f64,
        generation: u64,
        on_image: impl Fn(u64, u32, Retained<NSImage>) + Send + Sync + 'static,
    ) {
        if wids.is_empty() || !self.allowed() {
            return;
        }
        let on_image: Arc<OnImage> = Arc::new(on_image);
        let shared = self.shared.clone();
        let req = Arc::new(Request {
            max_w,
            max_h,
            generation,
            on_image,
        });
        // Hop off the caller's (main) thread: even with a warm content cache, building the
        // filters/configs and submitting N screenshots costs several ms.
        DispatchQueue::global_queue(GlobalQueueIdentifier::QualityOfService(
            DispatchQoS::UserInitiated,
        ))
        .exec_async(move || {
            with_content(
                &shared.clone(),
                CONTENT_MAX_AGE,
                Box::new(move |content| {
                    let Some((content, fetched_at)) = content else {
                        return;
                    };
                    let mut missing = Vec::new();
                    for wid in wids {
                        match find_window(&content.0, wid) {
                            Some(win) => sck_capture_async(&shared, &win, wid, &req),
                            None => missing.push(wid),
                        }
                    }
                    // A window created after the snapshot was taken: refetch once.
                    if !missing.is_empty() && fetched_at.elapsed() > CONTENT_REFETCH_IF_OLDER {
                        let shared2 = shared.clone();
                        with_content(
                            &shared,
                            Duration::ZERO,
                            Box::new(move |content| {
                                let Some((content, _)) = content else { return };
                                for wid in missing {
                                    if let Some(win) = find_window(&content.0, wid) {
                                        sck_capture_async(&shared2, &win, wid, &req);
                                    }
                                }
                            }),
                        );
                    }
                }),
            );
        });
    }

    /// Synchronous best-effort single capture. Blocks the calling thread for at most ~1.5 s.
    /// Safe to call from the main thread (does not need the main queue to run).
    /// Tries ScreenCaptureKit first, then `CGWindowListCreateImage` if it is available.
    pub fn capture_sync(&self, wid: u32, max_w: f64, max_h: f64) -> Option<Retained<NSImage>> {
        let (thumb, _) = self.capture_sync_thumb(wid, max_w, max_h)?;
        Some(thumb_to_nsimage(&thumb))
    }

    /// Like `capture_sync` but also reports which backend produced the image.
    pub fn capture_sync_with_backend(
        &self,
        wid: u32,
        max_w: f64,
        max_h: f64,
    ) -> Option<(Retained<NSImage>, Backend)> {
        let (thumb, b) = self.capture_sync_thumb(wid, max_w, max_h)?;
        Some((thumb_to_nsimage(&thumb), b))
    }

    fn capture_sync_thumb(&self, wid: u32, max_w: f64, max_h: f64) -> Option<(Thumb, Backend)> {
        if !self.allowed() {
            return None;
        }
        if let Some(t) = self.sck_sync(wid, max_w, max_h) {
            return Some((t, Backend::ScreenCaptureKit));
        }
        if self.shared.own_process_only {
            return None;
        }
        let t = cg_capture(wid, max_w, max_h)?;
        self.shared.cache.lock().ok()?.insert(wid, t.clone());
        Some((t, Backend::CGWindowList))
    }

    /// ScreenCaptureKit-only synchronous capture (for measurement). Updates the cache.
    pub fn capture_sync_sck(&self, wid: u32, max_w: f64, max_h: f64) -> Option<Retained<NSImage>> {
        if !self.allowed() {
            return None;
        }
        self.sck_sync(wid, max_w, max_h).map(|t| thumb_to_nsimage(&t))
    }

    /// `CGWindowListCreateImage`-only synchronous capture (for measurement). Does not touch
    /// the cache. Returns None if the symbol is unavailable or the capture fails.
    pub fn capture_sync_cg(&self, wid: u32, max_w: f64, max_h: f64) -> Option<Retained<NSImage>> {
        if !screen_capture_allowed() {
            return None;
        }
        cg_capture(wid, max_w, max_h).map(|t| thumb_to_nsimage(&t))
    }

    fn sck_sync(&self, wid: u32, max_w: f64, max_h: f64) -> Option<Thumb> {
        let (tx, rx) = mpsc::channel::<Option<Thumb>>();
        let shared = self.shared.clone();
        resolve_window(
            &self.shared,
            wid,
            Box::new(move |win| {
                let Some(win) = win else {
                    let _ = tx.send(None);
                    return;
                };
                sck_capture_raw(
                    &win.0,
                    max_w,
                    max_h,
                    Box::new(move |t| {
                        if let Some(t) = &t
                            && let Ok(mut c) = shared.cache.lock()
                        {
                            c.insert(wid, t.clone());
                        }
                        let _ = tx.send(t);
                    }),
                );
            }),
        );
        rx.recv_timeout(SYNC_TIMEOUT).ok().flatten()
    }

    /// Last captured image for `wid`, if any (main thread). Built fresh as an NSImage each
    /// call from the cached CGImage.
    pub fn cached(&self, wid: u32) -> Option<Retained<NSImage>> {
        let t = self.shared.cache.lock().ok()?.get(&wid).cloned()?;
        Some(thumb_to_nsimage(&t))
    }

    /// Last captured CGImage for `wid` (any thread), with its size in points.
    pub fn cached_cgimage(&self, wid: u32) -> Option<(CFRetained<CGImage>, f64, f64)> {
        let t = self.shared.cache.lock().ok()?.get(&wid).cloned()?;
        Some((t.image, t.w_pt, t.h_pt))
    }

    /// Drop cached images for windows not in `live` (call when the window list changes).
    pub fn retain_only(&self, live: &[u32]) {
        if let Ok(mut c) = self.shared.cache.lock() {
            c.retain(|wid, _| live.contains(wid));
        }
    }

    /// Drop the cached image for one window.
    pub fn forget(&self, wid: u32) {
        if let Ok(mut c) = self.shared.cache.lock() {
            c.remove(&wid);
        }
    }

    /// Prefetch the ScreenCaptureKit shareable content so the next `capture` starts faster
    /// (e.g. call when the switcher hotkey modifier goes down). No-op without permission.
    pub fn warm_up(&self) {
        if !self.allowed() {
            return;
        }
        with_content(&self.shared, CONTENT_MAX_AGE, Box::new(|_| {}));
    }
}

struct Request {
    max_w: f64,
    max_h: f64,
    generation: u64,
    on_image: Arc<OnImage>,
}

/// Size in points of a `w` x `h` point window scaled (never up) to fit `max_w` x `max_h`.
pub fn fit_points(w: f64, h: f64, max_w: f64, max_h: f64) -> (f64, f64) {
    if !(w > 0.0 && h > 0.0 && max_w > 0.0 && max_h > 0.0) {
        return (0.0, 0.0);
    }
    let s = (max_w / w).min(max_h / h).min(1.0);
    ((w * s).max(1.0), (h * s).max(1.0))
}

fn thumb_to_nsimage(t: &Thumb) -> Retained<NSImage> {
    NSImage::initWithCGImage_size(NSImage::alloc(), &t.image, NSSize::new(t.w_pt, t.h_pt))
}

/// Run `f` with shareable content no older than `max_age`, fetching (coalesced) if needed.
/// `f` runs on the caller's thread when the cache is fresh, otherwise on an SCK queue.
fn with_content(shared: &Arc<Shared>, max_age: Duration, f: ContentWaiter) {
    let start_fetch;
    {
        let Ok(mut st) = shared.content.lock() else { return };
        if let Some((c, at)) = &st.content
            && at.elapsed() <= max_age
        {
            let v = (c.clone(), *at);
            drop(st);
            f(Some(v));
            return;
        }
        st.waiters.push(f);
        // Start a fetch unless one is in flight; a fetch whose completion never arrived
        // within FETCH_STUCK_AFTER is treated as lost so callers are not queued forever.
        start_fetch = st.fetching.is_none_or(|t| t.elapsed() > FETCH_STUCK_AFTER);
        if start_fetch {
            st.fetching = Some(Instant::now());
        }
    }
    if !start_fetch {
        return;
    }
    let shared2 = shared.clone();
    let block = RcBlock::new(move |content: *mut SCShareableContent, _err: *mut NSError| {
        // The block parameter is borrowed: retain before it escapes.
        let content = unsafe { Retained::retain(content) }.map(SendContent);
        trace(|| format!("content fetched ok={}", content.is_some()));
        let now = Instant::now();
        let waiters = match shared2.content.lock() {
            Ok(mut st) => {
                st.fetching = None;
                if let Some(c) = &content {
                    st.content = Some((c.clone(), now));
                }
                std::mem::take(&mut st.waiters)
            }
            Err(_) => return,
        };
        for w in waiters {
            w(content.clone().map(|c| (c, now)));
        }
    });
    trace(|| "content fetch start".into());
    unsafe {
        if shared.own_process_only {
            SCShareableContent::getCurrentProcessShareableContentWithCompletionHandler(&block);
        } else {
            // excludeDesktopWindows = true, onScreenWindowsOnly = false (include other spaces /
            // minimized windows where SCK lists them).
            SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
                true, false, &block,
            );
        }
    }
}

fn find_window(content: &SCShareableContent, wid: u32) -> Option<Retained<SCWindow>> {
    let windows = unsafe { content.windows() };
    windows.iter().find(|w| unsafe { w.windowID() } == wid)
}

/// `SCWindow` from an immutable content snapshot, carried to another queue.
struct SendWindow(Retained<SCWindow>);
// SAFETY: SCWindow is an immutable value object from an SCShareableContent snapshot.
unsafe impl Send for SendWindow {}

/// Look `wid` up in (cached) shareable content, refetching once if it is missing from a
/// snapshot older than `CONTENT_REFETCH_IF_OLDER`.
fn resolve_window(shared: &Arc<Shared>, wid: u32, f: Box<dyn FnOnce(Option<SendWindow>) + Send>) {
    let shared2 = shared.clone();
    with_content(
        shared,
        CONTENT_MAX_AGE,
        Box::new(move |content| {
            let Some((content, fetched_at)) = content else {
                return f(None);
            };
            if let Some(w) = find_window(&content.0, wid) {
                return f(Some(SendWindow(w)));
            }
            if fetched_at.elapsed() <= CONTENT_REFETCH_IF_OLDER {
                return f(None);
            }
            with_content(
                &shared2,
                Duration::ZERO,
                Box::new(move |content| f(content.and_then(|(c, _)| find_window(&c.0, wid)).map(SendWindow))),
            );
        }),
    );
}

fn sck_capture_async(shared: &Arc<Shared>, win: &SCWindow, wid: u32, req: &Arc<Request>) {
    let shared = shared.clone();
    let req = req.clone();
    let (max_w, max_h) = (req.max_w, req.max_h);
    sck_capture_raw(
        win,
        max_w,
        max_h,
        Box::new(move |t| {
            let Some(t) = t else { return };
            if let Ok(mut c) = shared.cache.lock() {
                c.insert(wid, t.clone());
            }
            DispatchQueue::main().exec_async(move || {
                let img = thumb_to_nsimage(&t);
                (req.on_image)(req.generation, wid, img);
            });
        }),
    );
}

/// Capture one window via SCScreenshotManager. `done` runs on an SCK queue.
fn sck_capture_raw(win: &SCWindow, max_w: f64, max_h: f64, done: Box<dyn FnOnce(Option<Thumb>) + Send>) {
    let frame = unsafe { win.frame() };
    let (w_pt, h_pt) = fit_points(frame.size.width, frame.size.height, max_w, max_h);
    if w_pt <= 0.0 {
        done(None);
        return;
    }
    let pw = (w_pt * PIXEL_SCALE).round().max(1.0) as usize;
    let ph = (h_pt * PIXEL_SCALE).round().max(1.0) as usize;
    unsafe {
        let filter = SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), win);
        let config = SCStreamConfiguration::new();
        config.setWidth(pw);
        config.setHeight(ph);
        config.setShowsCursor(false);
        config.setScalesToFit(true);
        config.setPreservesAspectRatio(true);
        config.setIgnoreShadowsSingleWindow(true);
        config.setCapturesShadowsOnly(false);
        // RcBlock requires Fn; the FnOnce is taken out of a Mutex<Option<..>> on first call.
        let done = Mutex::new(Some(done));
        let block = RcBlock::new(move |image: *mut CGImage, _err: *mut NSError| {
            let Some(done) = done.lock().ok().and_then(|mut d| d.take()) else {
                return;
            };
            let thumb = NonNull::new(image).map(|p| Thumb {
                // Borrowed parameter: retain.
                image: CFRetained::retain(p),
                w_pt,
                h_pt,
            });
            trace(|| format!("complete image={} err={:?}", !image.is_null(), _err.as_ref().map(|e| e.localizedDescription().to_string())));
            done(thumb);
        });
        trace(|| format!("submit {pw}x{ph}"));
        SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
            &filter,
            &config,
            Some(&block),
        );
    }
}

// ---------------------------------------------------------------------------------------------
// CGWindowListCreateImage (resolved at runtime).

type CGWindowListCreateImageFn =
    unsafe extern "C" fn(CGRect, CGWindowListOption, u32, CGWindowImageOption) -> *mut CGImage;

fn cg_create_image_fn() -> Option<CGWindowListCreateImageFn> {
    static F: OnceLock<usize> = OnceLock::new();
    let p = *F.get_or_init(|| unsafe {
        libc::dlsym(libc::RTLD_DEFAULT, c"CGWindowListCreateImage".as_ptr()) as usize
    });
    if p == 0 {
        None
    } else {
        Some(unsafe { std::mem::transmute::<usize, CGWindowListCreateImageFn>(p) })
    }
}

/// True if the `CGWindowListCreateImage` symbol resolves in this process.
pub fn cg_window_image_available() -> bool {
    cg_create_image_fn().is_some()
}

/// Synchronous CGWindowListCreateImage capture, downscaled to fit at 2x. Caller must have
/// checked the Screen Recording permission.
fn cg_capture(wid: u32, max_w: f64, max_h: f64) -> Option<Thumb> {
    let f = cg_create_image_fn()?;
    let null_rect = CGRect::new(CGPoint::new(f64::INFINITY, f64::INFINITY), CGSize::new(0.0, 0.0));
    let raw = unsafe {
        f(
            null_rect,
            CGWindowListOption::OptionIncludingWindow,
            wid,
            CGWindowImageOption::BoundsIgnoreFraming | CGWindowImageOption::BestResolution,
        )
    };
    // Create rule: we own the returned reference.
    let full = unsafe { CFRetained::from_raw(NonNull::new(raw)?) };
    let iw = CGImage::width(Some(&full));
    let ih = CGImage::height(Some(&full));
    if iw <= 1 || ih <= 1 {
        return None;
    }
    // Native pixels -> points: assume the backing scale is 2 when the image is "large".
    // We only need the aspect ratio and an upper bound, so treat the image as 2x.
    let (w_pt, h_pt) = fit_points(iw as f64 / PIXEL_SCALE, ih as f64 / PIXEL_SCALE, max_w, max_h);
    let pw = (w_pt * PIXEL_SCALE).round().max(1.0) as usize;
    let ph = (h_pt * PIXEL_SCALE).round().max(1.0) as usize;
    let image = if pw >= iw && ph >= ih {
        full
    } else {
        downscale(&full, pw, ph)?
    };
    Some(Thumb { image, w_pt, h_pt })
}

fn downscale(src: &CGImage, pw: usize, ph: usize) -> Option<CFRetained<CGImage>> {
    let cs: CFRetained<CGColorSpace> = CGColorSpace::new_device_rgb()?;
    let info = CGImageByteOrderInfo::Order32Little.0 | CGImageAlphaInfo::PremultipliedFirst.0;
    let ctx: CFRetained<CGContext> =
        unsafe { CGBitmapContextCreate(std::ptr::null_mut::<c_void>(), pw, ph, 8, 0, Some(&cs), info)? };
    CGContext::set_interpolation_quality(Some(&ctx), CGInterpolationQuality::High);
    CGContext::draw_image(
        Some(&ctx),
        CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(pw as f64, ph as f64)),
        Some(src),
    );
    CGBitmapContextCreateImage(Some(&ctx))
}

/// Debug tracing to stderr, enabled by setting `ZTAB_THUMBS_TRACE`.
fn trace(msg: impl FnOnce() -> String) {
    static ON: OnceLock<Option<Instant>> = OnceLock::new();
    if let Some(t0) = ON.get_or_init(|| std::env::var_os("ZTAB_THUMBS_TRACE").map(|_| Instant::now())) {
        eprintln!(
            "[thumbs +{:.1}ms {:?}] {}",
            t0.elapsed().as_secs_f64() * 1e3,
            std::thread::current().id(),
            msg()
        );
    }
}
