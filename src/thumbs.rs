//! Window thumbnails.
//!
//! Primary path: ScreenCaptureKit (`SCShareableContent` -> `SCContentFilter` for one
//! desktop-independent window -> `SCScreenshotManager captureImage...`). Results are stored as
//! `CGImage` (thread-safe) in a shared cache, each with the time it was captured, and handed to
//! the caller as `NSImage` on the main queue.
//!
//! Every capture goes through one small scheduler: a priority queue (lost-focus captures, then
//! switcher captures, then background refresh) deduplicated by window id, with a bounded number
//! of captures in flight so `replayd` is never flooded. A window that fails to capture (gone,
//! not in the shareable content, blank image) is remembered with exponential backoff so a
//! periodic background refresh does not retry it in a storm. A failed capture never removes or
//! overwrites an existing cached thumbnail.
//!
//! Main-thread contract: every public call except the `capture_sync*` family returns in well
//! under 1 ms on the main thread. They only copy their arguments and hop to a GCD queue; the
//! Screen Recording preflight, the `SCShareableContent` fetch, filter/config construction and
//! the screenshot submission all happen off the main thread. No lock is ever held across a
//! ScreenCaptureKit call, and the cache and scheduler locks are never nested.
//!
//! Secondary path: `CGWindowListCreateImage` (deprecated, resolved at runtime with `dlsym`
//! so a missing symbol degrades to "unavailable" instead of a link failure). Synchronous only.
//!
//! Every capture path is gated on `CGPreflightScreenCaptureAccess()`; without the Screen
//! Recording grant nothing is captured, no callback fires and no system prompt is raised.
//! This module never calls `CGRequestScreenCaptureAccess`.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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

/// How long a fetched `SCShareableContent` is reused before refetching (switcher and lost-focus
/// captures).
const CONTENT_MAX_AGE: Duration = Duration::from_millis(1500);
/// Background refresh reuses shareable content this long (window ids are stable). A window that
/// is missing from it, or whose size no longer matches its current CoreGraphics bounds, makes it
/// refetch (when older than `CONTENT_REFETCH_IF_OLDER_BG`), so a stale frame never produces a
/// letterboxed thumbnail.
const CONTENT_MAX_AGE_BG: Duration = Duration::from_secs(10);
/// Window sizes (points) differing by less than this count as unchanged.
const SIZE_TOLERANCE: f64 = 1.5;
/// Switcher captures: when a requested window is missing from content older than this,
/// refetch once (a window created after the snapshot was taken).
const CONTENT_REFETCH_IF_OLDER: Duration = Duration::from_millis(150);
/// Background captures refetch for a missing window only when the snapshot is older than
/// this, so a gone window cannot turn a periodic refresh into a stream of content fetches.
const CONTENT_REFETCH_IF_OLDER_BG: Duration = Duration::from_millis(1000);
/// Timeout for `capture_sync`'s ScreenCaptureKit path.
const SYNC_TIMEOUT: Duration = Duration::from_millis(1500);
/// An SCShareableContent fetch older than this is considered lost and is retried.
const FETCH_STUCK_AFTER: Duration = Duration::from_secs(3);
/// A capture whose completion has not arrived after this long frees its slot.
const SLOT_TIMEOUT: Duration = Duration::from_secs(3);
/// Captures in flight at once while only background refresh work is queued.
const MAX_IN_FLIGHT_BACKGROUND: usize = 3;
/// Captures in flight at once for switcher / lost-focus captures.
const MAX_IN_FLIGHT_INTERACTIVE: usize = 3;
/// Failure backoff: 5 s after the first failure, doubling, at most 60 s.
const FAIL_BACKOFF_BASE: Duration = Duration::from_secs(5);
const FAIL_BACKOFF_MAX: Duration = Duration::from_secs(60);
/// Eviction marks (which stop in-flight captures of removed windows from re-adding them).
const EVICTED_KEEP: Duration = Duration::from_secs(30);
/// Pixels per point used for rendered thumbnails (Retina).
const PIXEL_SCALE: f64 = 2.0;
/// Size used by `refresh_stale` / `capture_one` until `capture` or `set_thumb_size` sets one
/// (matches the switcher's tile size in main.rs).
const DEFAULT_MAX_W: f64 = 300.0;
const DEFAULT_MAX_H: f64 = 190.0;

/// A cached thumbnail: the image, its size in points, and when it was captured.
#[derive(Clone)]
struct Thumb {
    image: CFRetained<CGImage>,
    w_pt: f64,
    h_pt: f64,
    at: Instant,
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

/// Capture priority; lower sorts first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Prio {
    /// `capture_one`: a window that just lost focus (still on screen right now).
    Focus,
    /// `capture`: the switcher is opening and wants every tile.
    Interactive,
    /// `refresh_stale`.
    Background,
}

struct Job {
    wid: u32,
    prio: Prio,
    max_w: f64,
    max_h: f64,
    /// `capture` callers waiting for this window.
    reqs: Vec<Arc<Request>>,
    /// Fire the `set_on_update` callback when this capture lands.
    notify: bool,
}

struct Flight {
    token: u64,
    started: Instant,
    reqs: Vec<Arc<Request>>,
    notify: bool,
}

struct Failure {
    at: Instant,
    count: u32,
}

impl Failure {
    fn backing_off(&self) -> bool {
        let shift = self.count.saturating_sub(1).min(8);
        let wait = (FAIL_BACKOFF_BASE * (1u32 << shift)).min(FAIL_BACKOFF_MAX);
        self.at.elapsed() < wait
    }
}

#[derive(Default)]
struct Sched {
    queue: VecDeque<Job>,
    in_flight: HashMap<u32, Flight>,
    next_token: u64,
    failures: HashMap<u32, Failure>,
    /// Windows dropped by `retain_only` / `forget`, with when.
    evicted: HashMap<u32, Instant>,
}

#[derive(Default)]
struct Counters {
    on_main: AtomicU64,
    started: AtomicU64,
    ok: AtomicU64,
    failed: AtomicU64,
    reclaimed: AtomicU64,
    content_fetches: AtomicU64,
    peak_in_flight: AtomicUsize,
}

/// Capture counters (diagnostics and tests).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CaptureStats {
    /// Captures started (scheduler slots taken).
    pub started: u64,
    /// Captures that produced a usable image.
    pub ok: u64,
    /// Captures that failed (window not in content, no image, blank image).
    pub failed: u64,
    /// Slots reclaimed because a completion never arrived.
    pub reclaimed: u64,
    /// `SCShareableContent` fetches started.
    pub content_fetches: u64,
    /// Highest number of captures in flight at once since the last `reset_peak`.
    pub peak_in_flight: usize,
    /// Captures in flight right now.
    pub in_flight: usize,
    /// Captures queued right now.
    pub queued: usize,
    /// Capture-pipeline steps (scheduling, content fetch, capture submission, completion) that ran
    /// on the main thread. Always 0: every one of them runs on a GCD or ScreenCaptureKit queue.
    pub on_main: u64,
}

type OnImage = dyn Fn(u64, u32, Retained<NSImage>) + Send + Sync + 'static;
type OnUpdate = dyn Fn(u32) + Send + Sync + 'static;

struct Shared {
    cache: Mutex<HashMap<u32, Thumb>>,
    content: Mutex<ContentState>,
    sched: Mutex<Sched>,
    /// Size for `refresh_stale` / `capture_one` (last `capture` size).
    size: Mutex<(f64, f64)>,
    on_update: Mutex<Option<Arc<OnUpdate>>>,
    limit_bg: AtomicUsize,
    limit_interactive: AtomicUsize,
    counters: Counters,
    /// Test mode: only this process's own windows via
    /// `getCurrentProcessShareableContent` (needs no Screen Recording grant).
    own_process_only: bool,
    /// Switcher sessions below this generation ended (`cancel_generation`): their queued
    /// captures are dropped. 0 = none.
    cancelled_below: AtomicU64,
}

impl Shared {
    fn new(own_process_only: bool) -> Self {
        Shared {
            cache: Mutex::default(),
            content: Mutex::default(),
            sched: Mutex::default(),
            size: Mutex::new((DEFAULT_MAX_W, DEFAULT_MAX_H)),
            on_update: Mutex::new(None),
            limit_bg: AtomicUsize::new(MAX_IN_FLIGHT_BACKGROUND),
            limit_interactive: AtomicUsize::new(MAX_IN_FLIGHT_INTERACTIVE),
            counters: Counters::default(),
            own_process_only,
            cancelled_below: AtomicU64::new(0),
        }
    }

    fn allowed(&self) -> bool {
        self.own_process_only || screen_capture_allowed()
    }
}

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
/// negative one is re-checked at most once per second. The `Thumbnailer` only calls this off
/// the main thread.
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

fn bg_queue(qos: DispatchQoS) -> dispatch2::DispatchRetained<DispatchQueue> {
    DispatchQueue::global_queue(GlobalQueueIdentifier::QualityOfService(qos))
}

impl Thumbnailer {
    pub fn new() -> Self {
        Thumbnailer {
            shared: Arc::new(Shared::new(false)),
        }
    }

    /// Test/diagnostic constructor: captures only this process's OWN windows, using
    /// `SCShareableContent getCurrentProcessShareableContent` (macOS 14.4+), which needs no
    /// Screen Recording grant. Exercises the same filter/config/screenshot/cache/main-queue
    /// path as `new()`. The CGWindowList fallback is disabled in this mode.
    #[doc(hidden)]
    pub fn new_own_process_only() -> Self {
        Thumbnailer {
            shared: Arc::new(Shared::new(true)),
        }
    }

    fn allowed(&self) -> bool {
        self.shared.allowed()
    }

    /// Asynchronously capture each window (CGWindowID) scaled to fit `max_w` x `max_h` points,
    /// rendered at 2x pixels. For each window that captures successfully, `on_image(generation,
    /// wid, image)` is invoked on the main thread (main dispatch queue). Returns immediately.
    /// Windows that cannot be captured (gone, no permission) produce no callback.
    /// The internal cache is updated before the callback runs.
    ///
    /// These captures jump ahead of queued background refresh work, in the given order. A
    /// window whose capture is already in flight is not captured twice: the callback gets
    /// that capture's result. The size also becomes the size used by `refresh_stale` and
    /// `capture_one`.
    pub fn capture(
        &self,
        wids: Vec<u32>,
        max_w: f64,
        max_h: f64,
        generation: u64,
        on_image: impl Fn(u64, u32, Retained<NSImage>) + Send + Sync + 'static,
    ) {
        if wids.is_empty() {
            return;
        }
        let req = Arc::new(Request {
            generation,
            on_image: Arc::new(on_image),
        });
        let shared = self.shared.clone();
        bg_queue(DispatchQoS::UserInitiated).exec_async(move || {
            if let Ok(mut s) = shared.size.lock() {
                *s = (max_w, max_h);
            }
            if !shared.allowed() {
                return;
            }
            let jobs = wids
                .into_iter()
                .map(|wid| Job {
                    wid,
                    prio: Prio::Interactive,
                    max_w,
                    max_h,
                    reqs: vec![req.clone()],
                    notify: false,
                })
                .collect();
            enqueue_urgent(&shared, jobs);
            pump(&shared);
        });
    }

    /// The switcher's captures, limited to windows that need one: those without a cached
    /// thumbnail and, with `older_than`, those whose thumbnail is older than that. Windows that
    /// failed recently are skipped with the same backoff as `refresh_stale`. Otherwise like
    /// [`capture`](Self::capture): Interactive priority in the given order, `on_image` on the main
    /// thread for each capture that lands. Returns at once; the filtering runs off the caller's
    /// thread. Nothing is queued for a generation that `cancel_generation` already ended.
    pub fn capture_stale(
        &self,
        wids: Vec<u32>,
        older_than: Option<Duration>,
        max_w: f64,
        max_h: f64,
        generation: u64,
        on_image: impl Fn(u64, u32, Retained<NSImage>) + Send + Sync + 'static,
    ) {
        if wids.is_empty() {
            return;
        }
        let req = Arc::new(Request {
            generation,
            on_image: Arc::new(on_image),
        });
        let shared = self.shared.clone();
        bg_queue(DispatchQoS::UserInitiated).exec_async(move || {
            if let Ok(mut s) = shared.size.lock() {
                *s = (max_w, max_h);
            }
            if generation < shared.cancelled_below.load(Ordering::Relaxed) || !shared.allowed() {
                return;
            }
            // Cache lock and scheduler lock are taken one after the other, never nested.
            let wanted: Vec<u32> = {
                let Ok(c) = shared.cache.lock() else { return };
                wids.into_iter()
                    .filter(|w| c.get(w).is_none_or(|t| older_than.is_some_and(|a| t.at.elapsed() > a)))
                    .collect()
            };
            let wanted: Vec<u32> = {
                let Ok(s) = shared.sched.lock() else { return };
                wanted.into_iter().filter(|w| !s.failures.get(w).is_some_and(Failure::backing_off)).collect()
            };
            if wanted.is_empty() {
                return;
            }
            trace(|| format!("capture_stale gen={generation} {wanted:?}"));
            let jobs = wanted
                .into_iter()
                .map(|wid| Job {
                    wid,
                    prio: Prio::Interactive,
                    max_w,
                    max_h,
                    reqs: vec![req.clone()],
                    notify: false,
                })
                .collect();
            enqueue_urgent(&shared, jobs);
            pump(&shared);
        });
    }

    /// Switcher session `generation` (and every earlier one) ended: drop its captures that have
    /// not started, so a quick ⌥Tab tap costs no capture work. Captures already in flight finish
    /// (and update the cache); queued captures that a background refresh or a lost-focus capture
    /// also asked for are kept. Generations must increase. Cheap on the main thread.
    pub fn cancel_generation(&self, generation: u64) {
        let below = generation.saturating_add(1);
        self.shared.cancelled_below.fetch_max(below, Ordering::Relaxed);
        if let Ok(mut s) = self.shared.sched.lock() {
            drop_cancelled(&mut s, below);
        }
    }

    /// Capture in the background every window in `wids` whose cached thumbnail is missing or
    /// older than `max_age`, at most `budget` captures per call (in the given order). Never
    /// blocks the caller; results go to the cache (and to the `set_on_update` callback).
    /// Windows already queued or in flight are skipped without using budget. Windows that
    /// failed recently (gone, not capturable, blank image) are skipped with exponential
    /// backoff (5 s, 10 s, ... 60 s), so calling this periodically never causes retry storms.
    /// A call that finds nothing stale starts no capture and no content fetch.
    pub fn refresh_stale(&self, wids: &[u32], max_age: Duration, budget: usize) {
        if wids.is_empty() || budget == 0 {
            return;
        }
        let wids = wids.to_vec();
        let shared = self.shared.clone();
        bg_queue(DispatchQoS::Utility).exec_async(move || {
            if !shared.allowed() {
                return;
            }
            // Cache lock and scheduler lock are taken one after the other, never nested.
            let stale: Vec<u32> = {
                let Ok(c) = shared.cache.lock() else { return };
                let mut seen = HashSet::new();
                wids.into_iter()
                    .filter(|w| seen.insert(*w))
                    .filter(|w| c.get(w).is_none_or(|t| t.at.elapsed() > max_age))
                    .collect()
            };
            if stale.is_empty() {
                return;
            }
            let (max_w, max_h) = shared.size.lock().map(|s| *s).unwrap_or((DEFAULT_MAX_W, DEFAULT_MAX_H));
            let (added, reclaimed) = {
                let Ok(mut s) = shared.sched.lock() else { return };
                // A lost completion must not keep its window "in flight" forever.
                let reclaimed = reclaim_stuck(&shared, &mut s);
                let mut added = 0;
                for wid in stale {
                    if added >= budget {
                        break;
                    }
                    if s.in_flight.contains_key(&wid)
                        || s.queue.iter().any(|j| j.wid == wid)
                        || s.failures.get(&wid).is_some_and(Failure::backing_off)
                    {
                        continue;
                    }
                    s.queue.push_back(Job {
                        wid,
                        prio: Prio::Background,
                        max_w,
                        max_h,
                        reqs: vec![],
                        notify: true,
                    });
                    added += 1;
                }
                (added, reclaimed)
            };
            if added > 0 || reclaimed > 0 {
                pump(&shared);
            }
        });
    }

    /// Capture one window now, in the background (for example when it loses focus: it is
    /// still on screen, so this is the moment to get a real image before it is minimized or
    /// covered). Goes ahead of everything else queued, ignores failure backoff, and is skipped
    /// if a capture of that window is already in flight. Updates the cache; the
    /// `set_on_update` callback (if any) runs on the main thread when the image lands.
    pub fn capture_one(&self, wid: u32) {
        let shared = self.shared.clone();
        bg_queue(DispatchQoS::UserInitiated).exec_async(move || {
            if !shared.allowed() {
                return;
            }
            let (max_w, max_h) = shared.size.lock().map(|s| *s).unwrap_or((DEFAULT_MAX_W, DEFAULT_MAX_H));
            enqueue_urgent(
                &shared,
                vec![Job {
                    wid,
                    prio: Prio::Focus,
                    max_w,
                    max_h,
                    reqs: vec![],
                    notify: true,
                }],
            );
            pump(&shared);
        });
    }

    /// Age of the cached thumbnail for `wid` (None when there is none).
    pub fn cache_age(&self, wid: u32) -> Option<Duration> {
        self.shared.cache.lock().ok()?.get(&wid).map(|t| t.at.elapsed())
    }

    /// True when a thumbnail for `wid` is cached.
    pub fn has_cached(&self, wid: u32) -> bool {
        self.shared.cache.lock().is_ok_and(|c| c.contains_key(&wid))
    }

    /// Callback run on the main thread with the window id whenever a `refresh_stale` or
    /// `capture_one` capture lands in the cache (use it to update a visible tile from
    /// `cached`). Replaces any previous callback.
    pub fn set_on_update(&self, f: impl Fn(u32) + Send + Sync + 'static) {
        if let Ok(mut u) = self.shared.on_update.lock() {
            *u = Some(Arc::new(f));
        }
    }

    /// Size (points, fit within) used by `refresh_stale` and `capture_one`. `capture` also
    /// sets it. Default 300 x 190.
    pub fn set_thumb_size(&self, max_w: f64, max_h: f64) {
        if let Ok(mut s) = self.shared.size.lock() {
            *s = (max_w, max_h);
        }
    }

    /// Capture counters.
    #[doc(hidden)]
    pub fn stats(&self) -> CaptureStats {
        let c = &self.shared.counters;
        let (in_flight, queued) = self
            .shared
            .sched
            .lock()
            .map(|s| (s.in_flight.len(), s.queue.len()))
            .unwrap_or_default();
        CaptureStats {
            started: c.started.load(Ordering::Relaxed),
            ok: c.ok.load(Ordering::Relaxed),
            failed: c.failed.load(Ordering::Relaxed),
            reclaimed: c.reclaimed.load(Ordering::Relaxed),
            content_fetches: c.content_fetches.load(Ordering::Relaxed),
            peak_in_flight: c.peak_in_flight.load(Ordering::Relaxed),
            in_flight,
            queued,
            on_main: c.on_main.load(Ordering::Relaxed),
        }
    }

    #[doc(hidden)]
    pub fn reset_peak(&self) {
        let n = self.shared.sched.lock().map(|s| s.in_flight.len()).unwrap_or(0);
        self.shared.counters.peak_in_flight.store(n, Ordering::Relaxed);
    }

    /// Concurrency limits (captures in flight) for background refresh and for switcher /
    /// lost-focus captures. For measurement.
    #[doc(hidden)]
    pub fn set_concurrency(&self, background: usize, interactive: usize) {
        self.shared.limit_bg.store(background.max(1), Ordering::Relaxed);
        self.shared.limit_interactive.store(interactive.max(1), Ordering::Relaxed);
    }

    /// Synchronous best-effort single capture. Blocks the calling thread for up to ~1.5 s:
    /// never call it on the main thread (diagnostics and tools only).
    /// Tries ScreenCaptureKit first, then `CGWindowListCreateImage` if it is available.
    pub fn capture_sync(&self, wid: u32, max_w: f64, max_h: f64) -> Option<Retained<NSImage>> {
        let (thumb, _) = self.capture_sync_thumb(wid, max_w, max_h)?;
        Some(thumb_to_nsimage(&thumb))
    }

    /// Like `capture_sync` but also reports which backend produced the image. Blocking.
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

    /// ScreenCaptureKit-only synchronous capture (for measurement). Updates the cache. Blocking.
    pub fn capture_sync_sck(&self, wid: u32, max_w: f64, max_h: f64) -> Option<Retained<NSImage>> {
        if !self.allowed() {
            return None;
        }
        self.sck_sync(wid, max_w, max_h).map(|t| thumb_to_nsimage(&t))
    }

    /// `CGWindowListCreateImage`-only synchronous capture (for measurement). Does not touch
    /// the cache. Returns None if the symbol is unavailable or the capture fails. Blocking.
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
            CONTENT_REFETCH_IF_OLDER,
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
    /// call from the cached CGImage (cheap: the CGImage is already thumbnail-sized).
    pub fn cached(&self, wid: u32) -> Option<Retained<NSImage>> {
        let t = self.shared.cache.lock().ok()?.get(&wid).cloned()?;
        Some(thumb_to_nsimage(&t))
    }

    /// Last captured image for `wid` (main thread), as an NSImage and as the bitmap behind it.
    pub fn cached_pair(&self, wid: u32) -> Option<(Retained<NSImage>, CFRetained<CGImage>)> {
        let t = self.shared.cache.lock().ok()?.get(&wid).cloned()?;
        Some((thumb_to_nsimage(&t), t.image))
    }

    /// Last captured CGImage for `wid` (any thread), with its size in points.
    pub fn cached_cgimage(&self, wid: u32) -> Option<(CFRetained<CGImage>, f64, f64)> {
        let t = self.shared.cache.lock().ok()?.get(&wid).cloned()?;
        Some((t.image, t.w_pt, t.h_pt))
    }

    /// Drop cached images, queued captures and failure records for windows not in `live`.
    /// Pass the FULL window list (including minimized, hidden-app and other-Space windows):
    /// every listed window keeps its thumbnail. A capture of a dropped window that is still in
    /// flight is discarded when it lands.
    pub fn retain_only(&self, live: &[u32]) {
        let live: HashSet<u32> = live.iter().copied().collect();
        let now = Instant::now();
        let mut gone: Vec<u32> = Vec::new();
        if let Ok(mut c) = self.shared.cache.lock() {
            c.retain(|wid, _| {
                let keep = live.contains(wid);
                if !keep {
                    gone.push(*wid);
                }
                keep
            });
        }
        if let Ok(mut s) = self.shared.sched.lock() {
            let in_flight_gone: Vec<u32> = s.in_flight.keys().filter(|w| !live.contains(w)).copied().collect();
            for wid in gone.into_iter().chain(in_flight_gone) {
                s.evicted.insert(wid, now);
            }
            s.evicted.retain(|_, at| at.elapsed() < EVICTED_KEEP);
            s.queue.retain(|j| live.contains(&j.wid));
            s.failures.retain(|wid, _| live.contains(wid));
        }
    }

    /// Drop the cached image for one window (and discard an in-flight capture of it).
    pub fn forget(&self, wid: u32) {
        if let Ok(mut c) = self.shared.cache.lock() {
            c.remove(&wid);
        }
        if let Ok(mut s) = self.shared.sched.lock() {
            s.evicted.insert(wid, Instant::now());
            s.queue.retain(|j| j.wid != wid);
            s.failures.remove(&wid);
        }
    }

    /// Prefetch the ScreenCaptureKit shareable content so the next `capture` starts faster
    /// (e.g. call when the switcher hotkey modifier goes down). No-op without permission.
    /// Returns immediately; the permission check and the fetch run off the calling thread.
    pub fn warm_up(&self) {
        let shared = self.shared.clone();
        bg_queue(DispatchQoS::UserInitiated).exec_async(move || {
            if shared.allowed() {
                with_content(&shared, CONTENT_MAX_AGE, Box::new(|_| {}));
            }
        });
    }
}

struct Request {
    generation: u64,
    on_image: Arc<OnImage>,
}

/// Queue `jobs` (all Focus or all Interactive) ahead of every queued job of the same or lower
/// priority, keeping their order. A window already in flight gets the callbacks attached to
/// that capture instead; a window already queued is moved (its callbacks are kept).
fn enqueue_urgent(shared: &Arc<Shared>, jobs: Vec<Job>) {
    let Ok(mut s) = shared.sched.lock() else { return };
    let mut batch: Vec<Job> = Vec::with_capacity(jobs.len());
    let mut seen = HashSet::new();
    for mut job in jobs {
        if !seen.insert(job.wid) {
            continue;
        }
        if let Some(f) = s.in_flight.get_mut(&job.wid) {
            f.reqs.append(&mut job.reqs);
            f.notify |= job.notify;
            continue;
        }
        if let Some(i) = s.queue.iter().position(|j| j.wid == job.wid)
            && let Some(old) = s.queue.remove(i)
        {
            let mut reqs = old.reqs;
            reqs.append(&mut job.reqs);
            job.reqs = reqs;
            job.notify |= old.notify;
        }
        batch.push(job);
    }
    let Some(prio) = batch.first().map(|j| j.prio) else { return };
    let base = s.queue.iter().position(|j| j.prio >= prio).unwrap_or(s.queue.len());
    for (k, job) in batch.into_iter().enumerate() {
        s.queue.insert(base + k, job);
    }
}

/// Remove the requests of ended switcher sessions (generation < `below`) from queued jobs, and
/// the switcher jobs nobody else wants any more.
fn drop_cancelled(s: &mut Sched, below: u64) {
    if below == 0 {
        return;
    }
    s.queue.retain_mut(|j| {
        let had = !j.reqs.is_empty();
        j.reqs.retain(|r| r.generation >= below);
        !(j.prio == Prio::Interactive && had && j.reqs.is_empty() && !j.notify)
    });
}

/// Count a capture-pipeline step that runs on the main thread (none should; see `on_main`).
fn check_thread(shared: &Shared) {
    // SAFETY: plain libc call.
    if unsafe { libc::pthread_main_np() } != 0 {
        shared.counters.on_main.fetch_add(1, Ordering::Relaxed);
    }
}

/// Start queued captures while slots are free. Never holds the scheduler lock across a
/// ScreenCaptureKit call.
fn pump(shared: &Arc<Shared>) {
    check_thread(shared);
    loop {
        let (wid, token, max_w, max_h, prio) = {
            let Ok(mut s) = shared.sched.lock() else { return };
            reclaim_stuck(shared, &mut s);
            // A session may have ended after its jobs were queued.
            drop_cancelled(&mut s, shared.cancelled_below.load(Ordering::Relaxed));
            let Some(front) = s.queue.front() else { return };
            let limit = if front.prio == Prio::Background {
                shared.limit_bg.load(Ordering::Relaxed)
            } else {
                shared.limit_interactive.load(Ordering::Relaxed)
            };
            if s.in_flight.len() >= limit {
                return;
            }
            let Some(job) = s.queue.pop_front() else { return };
            s.next_token += 1;
            let token = s.next_token;
            s.in_flight.insert(
                job.wid,
                Flight {
                    token,
                    started: Instant::now(),
                    reqs: job.reqs,
                    notify: job.notify,
                },
            );
            shared.counters.peak_in_flight.fetch_max(s.in_flight.len(), Ordering::Relaxed);
            (job.wid, token, job.max_w, job.max_h, job.prio)
        };
        shared.counters.started.fetch_add(1, Ordering::Relaxed);
        trace(|| format!("start wid={wid} prio={prio:?} token={token}"));
        let background = prio == Prio::Background;
        let (max_age, refetch) = if background {
            (CONTENT_MAX_AGE_BG, CONTENT_REFETCH_IF_OLDER_BG)
        } else {
            (CONTENT_MAX_AGE, CONTENT_REFETCH_IF_OLDER)
        };
        let shared2 = shared.clone();
        resolve_window_checked(
            shared,
            wid,
            max_age,
            refetch,
            background,
            Box::new(move |win| {
                let Some(win) = win else {
                    return finish(&shared2, wid, token, None);
                };
                // The slot may have been reclaimed while the content fetch was pending: do not
                // submit a capture outside the concurrency cap.
                if !still_in_flight(&shared2, wid, token) {
                    return;
                }
                let shared3 = shared2.clone();
                sck_capture_raw(&win.0, max_w, max_h, Box::new(move |t| finish(&shared3, wid, token, t)));
            }),
        );
    }
}

fn still_in_flight(shared: &Shared, wid: u32, token: u64) -> bool {
    shared
        .sched
        .lock()
        .is_ok_and(|s| s.in_flight.get(&wid).is_some_and(|f| f.token == token))
}

/// Free slots whose completion never arrived (counted as failures). Returns how many.
fn reclaim_stuck(shared: &Shared, s: &mut Sched) -> usize {
    let stuck: Vec<u32> = s
        .in_flight
        .iter()
        .filter(|(_, f)| f.started.elapsed() > SLOT_TIMEOUT)
        .map(|(w, _)| *w)
        .collect();
    for &wid in &stuck {
        s.in_flight.remove(&wid);
        record_failure(s, wid);
        shared.counters.reclaimed.fetch_add(1, Ordering::Relaxed);
        trace(|| format!("reclaimed stuck slot wid={wid}"));
    }
    stuck.len()
}

fn record_failure(s: &mut Sched, wid: u32) {
    let f = s.failures.entry(wid).or_insert(Failure {
        at: Instant::now(),
        count: 0,
    });
    f.at = Instant::now();
    f.count = f.count.saturating_add(1);
}

/// A capture ended (any queue). Releases the slot, updates cache and failure memory, delivers
/// callbacks on the main queue and starts the next queued capture.
fn finish(shared: &Arc<Shared>, wid: u32, token: u64, thumb: Option<Thumb>) {
    check_thread(shared);
    // ScreenCaptureKit hands back an IOSurface-backed image: drawing one in the switcher's
    // `drawRect:` made the main thread read the surface back (measured 50-120 ms of main-thread
    // time for the first panel draw after captures). Copy it into a plain bitmap here, off the
    // main thread, once per capture.
    let thumb = thumb.map(|t| Thumb { image: plain_copy(&t.image).unwrap_or(t.image), w_pt: t.w_pt, h_pt: t.h_pt, at: t.at });
    // A fully transparent image (e.g. a window with no backing store) must not replace a
    // real thumbnail.
    let thumb = thumb.filter(|t| !is_blank(&t.image));
    let (flight, evicted) = {
        let Ok(mut s) = shared.sched.lock() else { return };
        let flight = match s.in_flight.get(&wid) {
            Some(f) if f.token == token => s.in_flight.remove(&wid),
            _ => None,
        };
        let evicted = match (s.evicted.get(&wid), &flight) {
            (Some(at), Some(f)) => *at >= f.started,
            (Some(_), None) => true,
            (None, _) => false,
        };
        if thumb.is_some() {
            s.failures.remove(&wid);
        } else if flight.is_some() {
            record_failure(&mut s, wid);
        }
        (flight, evicted)
    };
    let counters = &shared.counters;
    if thumb.is_some() {
        counters.ok.fetch_add(1, Ordering::Relaxed);
    } else {
        counters.failed.fetch_add(1, Ordering::Relaxed);
    }
    trace(|| format!("finish wid={wid} ok={} evicted={evicted}", thumb.is_some()));
    if let Some(t) = thumb
        && !evicted
    {
        if let Ok(mut c) = shared.cache.lock() {
            // Never replace a newer image with an older one (late completion).
            if c.get(&wid).is_none_or(|old| old.at <= t.at) {
                c.insert(wid, t.clone());
            }
        }
        if let Some(f) = flight {
            let on_update = if f.notify {
                shared.on_update.lock().ok().and_then(|u| u.clone())
            } else {
                None
            };
            if !f.reqs.is_empty() || on_update.is_some() {
                DispatchQueue::main().exec_async(move || {
                    if !f.reqs.is_empty() {
                        let img = thumb_to_nsimage(&t);
                        for r in &f.reqs {
                            (r.on_image)(r.generation, wid, img.clone());
                        }
                    }
                    if let Some(u) = on_update {
                        u(wid);
                    }
                });
            }
        }
    }
    let shared = shared.clone();
    bg_queue(DispatchQoS::UserInitiated).exec_async(move || pump(&shared));
}

/// True when every pixel of a downsampled copy is fully transparent.
fn is_blank(img: &CGImage) -> bool {
    const N: usize = 8;
    let mut buf = [0u8; N * N * 4];
    let Some(cs) = CGColorSpace::new_device_rgb() else {
        return false;
    };
    let info = CGImageByteOrderInfo::Order32Little.0 | CGImageAlphaInfo::PremultipliedFirst.0;
    let Some(ctx) = (unsafe { CGBitmapContextCreate(buf.as_mut_ptr().cast(), N, N, 8, N * 4, Some(&cs), info) })
    else {
        return false;
    };
    CGContext::draw_image(
        Some(&ctx),
        CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(N as f64, N as f64)),
        Some(img),
    );
    drop(ctx);
    // BGRA little-endian, premultiplied-first: alpha is the 4th byte.
    buf.chunks(4).all(|p| p[3] == 0)
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
/// Callers must not be on the main thread (the fetch submission is an XPC call).
fn with_content(shared: &Arc<Shared>, max_age: Duration, f: ContentWaiter) {
    check_thread(shared);
    let start_fetch;
    {
        let Ok(mut st) = shared.content.lock() else { return f(None) };
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
    shared.counters.content_fetches.fetch_add(1, Ordering::Relaxed);
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
/// snapshot older than `refetch_if_older`. `f` is always called exactly once (unless the
/// content fetch itself is lost, which the scheduler's slot timeout covers).
fn resolve_window(
    shared: &Arc<Shared>,
    wid: u32,
    refetch_if_older: Duration,
    f: Box<dyn FnOnce(Option<SendWindow>) + Send>,
) {
    resolve_window_checked(shared, wid, CONTENT_MAX_AGE, refetch_if_older, false, f);
}

/// `resolve_window` with content up to `max_age` old. With `check_size`, a window whose frame
/// in the (older) content no longer matches its current CoreGraphics size also counts as
/// missing, so reused content never captures at a stale aspect ratio.
fn resolve_window_checked(
    shared: &Arc<Shared>,
    wid: u32,
    max_age: Duration,
    refetch_if_older: Duration,
    check_size: bool,
    f: Box<dyn FnOnce(Option<SendWindow>) + Send>,
) {
    let shared2 = shared.clone();
    with_content(
        shared,
        max_age,
        Box::new(move |content| {
            let Some((content, fetched_at)) = content else {
                return f(None);
            };
            let found = find_window(&content.0, wid);
            let stale_frame = check_size
                && fetched_at.elapsed() > refetch_if_older
                && found.as_ref().is_some_and(|w| {
                    let fr = unsafe { w.frame() };
                    cg_window_size(wid).is_some_and(|(cw, ch)| {
                        (cw - fr.size.width).abs() > SIZE_TOLERANCE || (ch - fr.size.height).abs() > SIZE_TOLERANCE
                    })
                });
            if let Some(w) = found
                && !stale_frame
            {
                return f(Some(SendWindow(w)));
            }
            if fetched_at.elapsed() <= refetch_if_older {
                return f(None);
            }
            trace(|| format!("refetch content for wid={wid} (stale frame: {stale_frame})"));
            with_content(
                &shared2,
                refetch_if_older,
                Box::new(move |content| f(content.and_then(|(c, _)| find_window(&c.0, wid)).map(SendWindow))),
            );
        }),
    );
}

/// Capture one window via SCScreenshotManager. `done` runs on an SCK queue (or synchronously
/// when the window has no size).
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
                at: Instant::now(),
            });
            trace(|| {
                format!(
                    "complete image={} err={:?}",
                    !image.is_null(),
                    _err.as_ref().map(|e| e.localizedDescription().to_string())
                )
            });
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

/// Current size (points) of one window from CoreGraphics (`CGWindowListCreateDescriptionFromArray`,
/// a WindowServer request; never called on the main thread). `None` when the window is gone.
fn cg_window_size(wid: u32) -> Option<(f64, f64)> {
    use objc2_core_foundation::{CFArray, CFDictionary, CFString, CFType};
    let mut values = [wid as usize as *const c_void];
    // SAFETY: the array holds a raw CGWindowID with NULL callbacks, as the API expects.
    let input = unsafe { CFArray::new(None, values.as_mut_ptr(), 1, std::ptr::null()) }?;
    let arr = unsafe { objc2_core_graphics::CGWindowListCreateDescriptionFromArray(Some(&input)) }?;
    // SAFETY: the result is a CFArray of CFDictionary<CFString, CFType>.
    let arr: CFRetained<CFArray<CFDictionary<CFString, CFType>>> = unsafe { CFRetained::cast_unchecked(arr) };
    let d = arr.to_vec().into_iter().next()?;
    // SAFETY: kCGWindowBounds is a valid static key; the value is a bounds dictionary.
    let bd = unsafe { d.get_unchecked(objc2_core_graphics::kCGWindowBounds) }?.downcast_ref::<CFDictionary>()?;
    let mut rect = CGRect::default();
    if !unsafe { objc2_core_graphics::CGRectMakeWithDictionaryRepresentation(Some(bd), &mut rect) } {
        return None;
    }
    Some((rect.size.width, rect.size.height))
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
    Some(Thumb {
        image,
        w_pt,
        h_pt,
        at: Instant::now(),
    })
}

/// A plain gray bitmap image of `w` x `h` points, built exactly like a cached thumbnail (same
/// pixel format, 2x pixels), for warming up the switcher's drawing path at launch.
pub fn placeholder_image(w: f64, h: f64) -> Option<Retained<NSImage>> {
    let (pw, ph) = ((w * PIXEL_SCALE).round().max(1.0) as usize, (h * PIXEL_SCALE).round().max(1.0) as usize);
    let cs = srgb()?;
    let info = CGImageByteOrderInfo::Order32Little.0 | CGImageAlphaInfo::PremultipliedFirst.0;
    // SAFETY: null data lets CG allocate; sizes are positive.
    let ctx: CFRetained<CGContext> = unsafe { CGBitmapContextCreate(std::ptr::null_mut::<c_void>(), pw, ph, 8, 0, Some(&cs), info)? };
    CGContext::set_rgb_fill_color(Some(&ctx), 0.5, 0.5, 0.5, 1.0);
    CGContext::fill_rect(Some(&ctx), CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(pw as f64, ph as f64)));
    let image = CGBitmapContextCreateImage(Some(&ctx))?;
    Some(thumb_to_nsimage(&Thumb { image, w_pt: w, h_pt: h, at: Instant::now() }))
}

/// sRGB: the colour space of every bitmap the switcher shows (thumbnails, icons, labels) and of
/// the switcher panel itself, so Core Animation's commit on the main thread hands them to the
/// window server as they are. A bitmap in another colour space than the window's is converted
/// pixel by pixel (ColorSync via vImage) inside that commit, on every display change: tens of ms
/// for a full panel. The window server colour-matches the sRGB panel to each display on the GPU.
pub fn srgb() -> Option<CFRetained<CGColorSpace>> {
    // SAFETY: CoreGraphics-provided constant.
    CGColorSpace::with_name(Some(unsafe { objc2_core_graphics::kCGColorSpaceSRGB }))
}

/// The same image in a plain (memory-backed, sRGB) bitmap.
fn plain_copy(src: &CGImage) -> Option<CFRetained<CGImage>> {
    let (w, h) = (CGImage::width(Some(src)), CGImage::height(Some(src)));
    if w == 0 || h == 0 {
        return None;
    }
    downscale(src, w, h)
}

fn downscale(src: &CGImage, pw: usize, ph: usize) -> Option<CFRetained<CGImage>> {
    let cs: CFRetained<CGColorSpace> = srgb()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        let f = |count, ago: u64| Failure {
            at: Instant::now() - Duration::from_secs(ago),
            count,
        };
        assert!(f(1, 4).backing_off());
        assert!(!f(1, 6).backing_off());
        assert!(f(2, 9).backing_off());
        assert!(!f(2, 11).backing_off());
        assert!(f(30, 59).backing_off());
        assert!(!f(30, 61).backing_off());
    }

    #[test]
    fn stuck_slot_is_reclaimed_and_backs_off() {
        let shared = Shared::new(true);
        let mut s = Sched::default();
        let flight = |token, ago_ms: u64| Flight {
            token,
            started: Instant::now() - Duration::from_millis(ago_ms),
            reqs: vec![],
            notify: false,
        };
        s.in_flight.insert(1, flight(1, 3500));
        s.in_flight.insert(2, flight(2, 100));
        assert_eq!(reclaim_stuck(&shared, &mut s), 1);
        assert!(!s.in_flight.contains_key(&1));
        assert!(s.in_flight.contains_key(&2));
        assert!(s.failures.get(&1).is_some_and(Failure::backing_off));
        assert_eq!(shared.counters.reclaimed.load(Ordering::Relaxed), 1);
        assert_eq!(reclaim_stuck(&shared, &mut s), 0);
    }

    #[test]
    fn urgent_jobs_go_first_and_dedupe() {
        let shared = Arc::new(Shared::new(true));
        let job = |wid, prio| Job {
            wid,
            prio,
            max_w: 1.0,
            max_h: 1.0,
            reqs: vec![],
            notify: prio != Prio::Interactive,
        };
        {
            let mut s = shared.sched.lock().unwrap();
            s.queue.push_back(job(10, Prio::Background));
            s.queue.push_back(job(11, Prio::Background));
            s.in_flight.insert(
                12,
                Flight {
                    token: 1,
                    started: Instant::now(),
                    reqs: vec![],
                    notify: false,
                },
            );
        }
        enqueue_urgent(&shared, vec![job(11, Prio::Interactive), job(12, Prio::Interactive), job(13, Prio::Interactive)]);
        enqueue_urgent(&shared, vec![job(14, Prio::Focus)]);
        let s = shared.sched.lock().unwrap();
        let order: Vec<(u32, Prio)> = s.queue.iter().map(|j| (j.wid, j.prio)).collect();
        assert_eq!(
            order,
            vec![(14, Prio::Focus), (11, Prio::Interactive), (13, Prio::Interactive), (10, Prio::Background)]
        );
        // 11 kept the background job's notify flag; 12 stayed in flight (not queued twice).
        assert!(s.queue[1].notify);
    }

    #[test]
    fn fit_points_never_upscales() {
        assert_eq!(fit_points(1600.0, 1000.0, 320.0, 200.0), (320.0, 200.0));
        assert_eq!(fit_points(100.0, 50.0, 320.0, 200.0), (100.0, 50.0));
        assert_eq!(fit_points(0.0, 50.0, 320.0, 200.0), (0.0, 0.0));
    }
}
