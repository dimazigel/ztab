//! System-state helpers that never block the main thread.
//!
//! - [`secure_input`]: is Secure Event Input on (and which app WindowServer attributes it to).
//!   While it is on, macOS hides keystrokes from event taps, so ⌥Tab cannot work; the menu
//!   should say so.
//! - [`activity_now`] / [`pause_reason`]: screen lock, console and user-idle state, and whether
//!   background work should pause.
//! - [`StallDetector`]: logs every main-thread stall, from run-loop observers (default) or by
//!   probing the main dispatch queue (`--stall-period-ms`).
//! - [`IconPrefetcher`]: loads and rasterises app icons on a worker thread, so the main thread
//!   only ever wraps a finished bitmap.
#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, c_void};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::{DispatchQoS, DispatchQueue, GlobalQueueIdentifier};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2::{AnyThread, MainThreadMarker};
use objc2_app_kit::{NSBitmapImageRep, NSGraphicsContext, NSImage, NSRunningApplication};
use objc2_core_foundation::{
    CFDictionary, CFIndex, CFRetained, CFRunLoop, CFRunLoopActivity, CFRunLoopObserver, CGPoint, CGRect, CGSize,
    kCFRunLoopCommonModes,
};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGColorSpace, CGContext, CGEventSource, CGEventSourceStateID,
    CGEventType, CGImage, CGImageAlphaInfo, CGImageByteOrderInfo, CGInterpolationQuality, CGSessionCopyCurrentDictionary,
};
use objc2_foundation::{NSDictionary, NSNumber, NSPoint, NSRect, NSSize, NSString};

// ---------------------------------------------------------------------------------------------
// Secure event input
// ---------------------------------------------------------------------------------------------

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    /// HIToolbox. Global: true while any process has secure event input enabled.
    fn IsSecureEventInputEnabled() -> u8;
}

/// Secure Event Input state. `pid`/`app_name` are what WindowServer reports in the session
/// dictionary (`kCGSSessionSecureInputPID`); either can be `None` when unknown.
///
/// Caution (measured on macOS 27): when a *background* process enables secure input, the
/// reported pid is the FRONTMOST app, not the holder. So word it in the UI as
/// "Secure input is on (frontmost: X)", never "X is blocking". The common legitimate case, a
/// focused password field, is the frontmost app anyway.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecureInput {
    pub pid: Option<i32>,
    pub app_name: Option<String>,
}

/// Session dictionary key carrying the pid of the process WindowServer attributes secure input to.
const SECURE_INPUT_PID_KEY: &str = "kCGSSessionSecureInputPID";
/// How old a pid/name snapshot may be before `secure_input` schedules a refresh.
const SECURE_SNAPSHOT_TTL: Duration = Duration::from_millis(500);
/// Older snapshots are not shown at all (they may be from an earlier secure-input episode).
/// Comfortably above main.rs's 1.5 s watchdog period, so polling from it keeps one usable.
const SECURE_SNAPSHOT_MAX_AGE: Duration = Duration::from_secs(4);

struct SecureSnapshot {
    at: Instant,
    who: Option<SecureInput>,
}
static SECURE_SNAPSHOT: Mutex<Option<SecureSnapshot>> = Mutex::new(None);
static SECURE_REFRESHING: AtomicBool = AtomicBool::new(false);

/// `None` when Secure Event Input is off; otherwise who WindowServer attributes it to.
///
/// Main-thread safe, never blocks: the on/off flag is a local HIToolbox read (measured < 2 µs).
/// The pid/name need a WindowServer round trip (`CGSessionCopyCurrentDictionary`, measured p50
/// 0.06 ms but tails of 3–15 ms), so they come from a snapshot refreshed on a background queue
/// when older than 500 ms, and a refresh only starts when this is called. A call that finds no
/// snapshot younger than 4 s (e.g. the first call after secure input turns on) returns
/// `pid: None, app_name: None` and schedules the refresh, which lands within a few ms
/// (measured in the spike). So poll it from a periodic timer (main.rs's 1.5 s watchdog;
/// ~free while secure input is off) to keep the snapshot warm, then a menu-open call has the
/// name; a one-off caller must call again a few ms later. Use [`secure_input_now`] off
/// the main thread when a fresh answer is required.
pub fn secure_input() -> Option<SecureInput> {
    // SAFETY: no arguments, no preconditions.
    if unsafe { IsSecureEventInputEnabled() } == 0 {
        return None;
    }
    let (who, fresh) = match SECURE_SNAPSHOT.lock() {
        Ok(g) => match &*g {
            Some(s) => {
                let age = s.at.elapsed();
                (s.who.clone().filter(|_| age < SECURE_SNAPSHOT_MAX_AGE), age < SECURE_SNAPSHOT_TTL)
            }
            None => (None, false),
        },
        Err(_) => (None, false),
    };
    if !fresh && !SECURE_REFRESHING.swap(true, Ordering::AcqRel) {
        DispatchQueue::global_queue(GlobalQueueIdentifier::QualityOfService(DispatchQoS::Utility))
            .exec_async(|| {
                let who = autoreleasepool(|_| secure_input_now());
                if let Ok(mut g) = SECURE_SNAPSHOT.lock() {
                    *g = Some(SecureSnapshot { at: Instant::now(), who });
                }
                SECURE_REFRESHING.store(false, Ordering::Release);
            });
    }
    Some(who.unwrap_or(SecureInput { pid: None, app_name: None }))
}

/// Synchronous version of [`secure_input`]: queries WindowServer and resolves the name now.
/// Can take several ms; do not call on the main thread.
pub fn secure_input_now() -> Option<SecureInput> {
    // SAFETY: no arguments, no preconditions.
    if unsafe { IsSecureEventInputEnabled() } == 0 {
        return None;
    }
    let pid = secure_input_pid();
    Some(SecureInput { pid, app_name: pid.and_then(process_name_cached) })
}

fn secure_input_pid() -> Option<i32> {
    let dict: CFRetained<CFDictionary> = CGSessionCopyCurrentDictionary()?;
    // Toll-free bridged.
    let dict: &NSDictionary<NSString, AnyObject> =
        unsafe { &*(&*dict as *const CFDictionary as *const NSDictionary<NSString, AnyObject>) };
    let v = dict.objectForKey(&NSString::from_str(SECURE_INPUT_PID_KEY))?;
    let pid = v.downcast_ref::<NSNumber>()?.intValue();
    (pid > 0).then_some(pid)
}

/// Name for `pid`, remembering the last answer (the attributed pid rarely changes).
fn process_name_cached(pid: i32) -> Option<String> {
    static LAST: Mutex<Option<(i32, Option<String>)>> = Mutex::new(None);
    if let Ok(g) = LAST.lock()
        && let Some((p, name)) = &*g
        && *p == pid
    {
        return name.clone();
    }
    let name = process_name(pid);
    if let Ok(mut g) = LAST.lock() {
        *g = Some((pid, name.clone()));
    }
    name
}

/// Localized app name from LaunchServices, else the BSD process name (`proc_name`, which also
/// covers plain command-line processes that are not `NSRunningApplication`s).
pub fn process_name(pid: i32) -> Option<String> {
    let ls = autoreleasepool(|_| {
        NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
            .and_then(|a| a.localizedName())
            .map(|s| s.to_string())
    });
    if let Some(n) = ls.filter(|n| !n.is_empty()) {
        return Some(n);
    }
    let mut buf = [0u8; 256];
    // SAFETY: buffer and its length are valid; proc_name NUL-terminates on success.
    let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr() as *mut c_void, buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    CStr::from_bytes_until_nul(&buf).ok().map(|c| c.to_string_lossy().into_owned())
}

// ---------------------------------------------------------------------------------------------
// Session activity: screen lock, console, user idle
// ---------------------------------------------------------------------------------------------

/// What [`activity_now`] reads.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Activity {
    /// The screen is locked (`CGSSessionScreenIsLocked` in the session dictionary).
    pub locked: bool,
    /// This login session is not the one on the console (fast user switching).
    pub off_console: bool,
    /// Seconds since the last keyboard, mouse or trackpad event of the session.
    pub idle_s: f64,
}

/// Background work pauses after this much time without user input.
pub const IDLE_PAUSE_S: f64 = 120.0;
/// [`pause_reason`] when the only reason is the user being idle.
pub const PAUSE_IDLE: &str = "user idle";

/// Session state now. Makes WindowServer requests (`CGSessionCopyCurrentDictionary`, measured p50
/// 0.06 ms with tails of 3–15 ms): never call it on the main thread.
pub fn activity_now() -> Activity {
    let (locked, off_console) = match CGSessionCopyCurrentDictionary() {
        Some(dict) => {
            // Toll-free bridged.
            let dict: &NSDictionary<NSString, AnyObject> =
                unsafe { &*(&*dict as *const CFDictionary as *const NSDictionary<NSString, AnyObject>) };
            let flag = |key: &str| dict.objectForKey(&NSString::from_str(key)).and_then(|v| v.downcast_ref::<NSNumber>().map(|n| n.boolValue()));
            (flag("CGSSessionScreenIsLocked").unwrap_or(false), flag("kCGSSessionOnConsoleKey") == Some(false))
        }
        None => (false, false),
    };
    // kCGAnyInputEventType is `(CGEventType)(~0)` (CGEventTypes.h).
    let idle_s = CGEventSource::seconds_since_last_event_type(CGEventSourceStateID::CombinedSessionState, CGEventType(!0));
    Activity { locked, off_console, idle_s }
}

/// Why background work (thumbnail refresh, the window model's safety-net rescans and periodic
/// window-list refresh) should pause now, or `None` to run it. Nobody looks at the screen while
/// it is locked, the displays sleep or the session is switched out, and thumbnails do not change
/// while the user is away. `displays_asleep` / `session_active` come from NSWorkspace
/// notifications.
pub fn pause_reason(a: &Activity, displays_asleep: bool, session_active: bool, idle_limit_s: f64) -> Option<&'static str> {
    if a.locked {
        Some("screen locked")
    } else if displays_asleep {
        Some("displays asleep")
    } else if !session_active || a.off_console {
        Some("session inactive")
    } else if a.idle_s >= idle_limit_s {
        Some(PAUSE_IDLE)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------------------------
// Main-thread stall detector
// ---------------------------------------------------------------------------------------------

struct StallShared {
    stop: AtomicBool,
    count: AtomicU64,
    /// Longest stall, in microseconds.
    max_us: AtomicU64,
    /// Probe mode: probes answered. Run-loop mode: work units measured.
    probes: AtomicU64,
    /// Longest probe wait / work unit (stall or not), in microseconds.
    max_wait_us: AtomicU64,
}

impl StallShared {
    fn new() -> Arc<StallShared> {
        Arc::new(StallShared {
            stop: AtomicBool::new(false),
            count: AtomicU64::new(0),
            max_us: AtomicU64::new(0),
            probes: AtomicU64::new(0),
            max_wait_us: AtomicU64::new(0),
        })
    }

    /// Record one measured wait / work unit; returns true when it is a stall.
    fn record(&self, d: Duration, threshold: Duration) -> bool {
        let us = d.as_micros() as u64;
        self.probes.fetch_add(1, Ordering::Relaxed);
        self.max_wait_us.fetch_max(us, Ordering::Relaxed);
        if d > threshold {
            self.count.fetch_add(1, Ordering::Relaxed);
            self.max_us.fetch_max(us, Ordering::Relaxed);
            return true;
        }
        false
    }
}

/// A main-thread wait of this long is reported while it is still going on (then at 2x, 4x...).
const STILL_BLOCKED_FIRST: Duration = Duration::from_secs(1);

/// Watchdog for the main thread. Two modes:
///
/// **Run-loop mode** ([`observe`](Self::observe), ZTab's default). Two `CFRunLoopObserver`s on
/// the run loop (all common modes, so menu tracking and other nested loops are covered) stamp
/// every pass of the loop: waking up, before timers, before sources, entering / leaving a
/// (nested) loop and going to sleep. The time between two stamps is one unit of work (one timer,
/// one event, one dispatched block...); a unit above `threshold` is logged by the main thread
/// itself, when it ends, as `main thread stall: <ms> ms (run loop)`. Every stall is caught, with
/// its exact length. A watcher thread logs `main thread still blocked: <ms> ms` for a unit that
/// is still running after 1 s (and 2 s, 4 s...), so a hang shows up while it lasts. It waits on a
/// kqueue one-shot timer that the main thread arms when it wakes up and deletes before it sleeps
/// (two system calls per wake-up, no thread switch), so the watcher wakes up only when the main
/// thread really has been busy for 1 s: nothing runs while the main thread sleeps, and nothing
/// extra runs when it wakes up briefly.
///
/// **Probe mode** ([`start`](Self::start), `--stall-period-ms`). Every `period` a thread posts a
/// tiny block to the main dispatch queue and measures how long it waits; a wait above
/// `threshold` is logged as `main thread stall: <ms> ms`. It wakes the main thread every period
/// (an idle cost) and catches a stall for sure only when it lasts `threshold + period`, but it
/// measures the latency a queued request actually sees.
///
/// Start it once the main run loop is running, otherwise launch work counts as a stall.
/// Dropping it stops it (the threads are never joined, which could deadlock against the main
/// thread).
pub struct StallDetector {
    shared: Arc<StallShared>,
    /// Run-loop mode: the observers (invalidated on drop) and the watcher's state.
    observers: Vec<CFRetained<CFRunLoopObserver>>,
    watch: Option<Arc<Watch>>,
}

/// State shared by the run-loop observers (main thread) and the watcher thread.
struct Watch {
    m: Mutex<WatchState>,
    /// The kqueue the watcher waits on. The timer is armed, re-armed and deleted only under `m`,
    /// and the watcher closes the queue under `m` once `stop` is set.
    kq: i32,
}

#[derive(Default)]
struct WatchState {
    /// Start of the unit of work in progress; `None` while the run loop sleeps.
    unit: Option<Instant>,
    /// The kqueue timer is armed.
    armed: bool,
    stop: bool,
    /// Times the watcher thread woke up (diagnostics and tests).
    wakeups: u64,
}

/// Ident of the watcher's kqueue timer.
const WATCH_TIMER: usize = 1;

impl Watch {
    fn new() -> Option<Watch> {
        // SAFETY: plain system call.
        let kq = unsafe { libc::kqueue() };
        (kq >= 0).then(|| Watch { m: Mutex::new(WatchState::default()), kq })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, WatchState> {
        self.m.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Arm (`Some(after)`, one-shot, replacing any armed timer) or delete (`None`) the timer.
    /// Call with the lock held and `!stop`.
    fn set_timer(&self, st: &mut WatchState, after: Option<Duration>) {
        let (flags, ms) = match after {
            Some(d) => (libc::EV_ADD | libc::EV_ENABLE | libc::EV_ONESHOT, d.as_millis().max(1) as libc::intptr_t),
            None if !st.armed => return,
            None => (libc::EV_DELETE, 0),
        };
        let change = libc::kevent { ident: WATCH_TIMER, filter: libc::EVFILT_TIMER, flags, fflags: 0, data: ms, udata: std::ptr::null_mut() };
        // SAFETY: valid kqueue and change list; no event list. Milliseconds are the default unit.
        unsafe { libc::kevent(self.kq, &change, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
        st.armed = after.is_some();
    }
}

/// Main-thread side of run-loop mode.
struct Meter {
    shared: Arc<StallShared>,
    watch: Arc<Watch>,
    threshold: Duration,
    log: fn(&str),
    /// Start of the unit of work in progress (and the activity that started it); `None` while
    /// the run loop sleeps.
    last: Option<(Instant, CFRunLoopActivity)>,
    summary_every: Option<Duration>,
    window_start: Instant,
    window_wakeups: u64,
    window_units: u64,
    window_max: Duration,
}

/// A callback waiting for the main thread to go idle (see [`when_main_idle`]): when it was
/// registered, the longest unit of work seen since, and the callback.
type IdleWaiter = (Instant, Duration, Box<dyn FnOnce(f64, f64)>);

thread_local! {
    /// Callbacks for the next time the main run loop goes to sleep (main thread only).
    static IDLE_WAITERS: RefCell<Vec<IdleWaiter>> = const { RefCell::new(Vec::new()) };
}

/// Main thread, with the run-loop stall detector running: call `f(busy_ms, longest_unit_ms)` the
/// next time the main run loop goes to sleep. `busy_ms` is the time from `since` until then (so
/// it covers the rest of the current unit of work, Core Animation's commit and any work queued
/// behind it), `longest_unit_ms` the longest single unit of work in between. Without a running
/// detector `f` is never called. Measurement only (e.g. how long the main thread stays busy after
/// the switcher panel is ordered in, whatever the stall threshold).
pub fn when_main_idle(since: Instant, f: impl FnOnce(f64, f64) + 'static) {
    IDLE_WAITERS.with(|w| w.borrow_mut().push((since, Duration::ZERO, Box::new(f))));
}

impl Meter {
    fn on_activity(&mut self, act: CFRunLoopActivity) {
        let now = Instant::now();
        if act == CFRunLoopActivity::AfterWaiting {
            self.last = Some((now, act));
            self.window_wakeups += 1;
            self.set_unit(Some(now));
            self.maybe_summary(now);
            return;
        }
        if let Some((start, from)) = self.last {
            let d = now.saturating_duration_since(start);
            self.window_units += 1;
            self.window_max = self.window_max.max(d);
            if self.shared.record(d, self.threshold) {
                (self.log)(&format!("main thread stall: {} ms (run loop, {} -> {})", d.as_millis(), activity_name(from), activity_name(act)));
            }
            IDLE_WAITERS.with(|w| {
                for (since, max, _) in w.borrow_mut().iter_mut() {
                    // Only the part of the unit after the waiter was registered counts.
                    *max = (*max).max(now.saturating_duration_since(start.max(*since)));
                }
            });
        }
        if act == CFRunLoopActivity::BeforeWaiting {
            let due = IDLE_WAITERS.with(|w| std::mem::take(&mut *w.borrow_mut()));
            for (since, max, f) in due {
                f(now.saturating_duration_since(since).as_secs_f64() * 1000.0, max.as_secs_f64() * 1000.0);
            }
        }
        let unit = (act != CFRunLoopActivity::BeforeWaiting).then_some(now);
        self.last = unit.map(|t| (t, act));
        self.set_unit(unit);
    }

    /// A unit of work started (`Some`) or the run loop goes to sleep (`None`): arm the watcher's
    /// timer for 1 s if it is not armed, or delete it.
    fn set_unit(&self, unit: Option<Instant>) {
        let mut st = self.watch.lock();
        st.unit = unit;
        if st.stop {
            return;
        }
        match unit {
            Some(_) if !st.armed => self.watch.set_timer(&mut st, Some(STILL_BLOCKED_FIRST)),
            Some(_) => {}
            None => self.watch.set_timer(&mut st, None),
        }
    }

    fn maybe_summary(&mut self, now: Instant) {
        let Some(every) = self.summary_every else { return };
        let span = now.saturating_duration_since(self.window_start);
        if span < every {
            return;
        }
        let watcher = self.watch.lock().wakeups;
        (self.log)(&format!(
            "stall detector (run loop): {} wake-ups, {} work units in the last {:.0} s, longest unit {:.1} ms; \
             watcher wake-ups so far {watcher}; stalls so far {}",
            self.window_wakeups,
            self.window_units,
            span.as_secs_f64(),
            self.window_max.as_secs_f64() * 1000.0,
            self.shared.count.load(Ordering::Relaxed)
        ));
        self.window_start = now;
        self.window_wakeups = 0;
        self.window_units = 0;
        self.window_max = Duration::ZERO;
    }
}

/// Short name of a run-loop activity, for stall lines: the unit ran between these two points.
fn activity_name(a: CFRunLoopActivity) -> &'static str {
    match a {
        CFRunLoopActivity::Entry => "loop entry",
        CFRunLoopActivity::BeforeTimers => "timers",
        CFRunLoopActivity::BeforeSources => "sources",
        CFRunLoopActivity::BeforeWaiting => "sleep",
        CFRunLoopActivity::AfterWaiting => "wake-up",
        CFRunLoopActivity::Exit => "loop exit",
        _ => "?",
    }
}

/// The run-loop watcher thread: waits on the kqueue timer the main thread arms, which fires only
/// when a unit of work has run for 1 s (then 2 s, 4 s...), and logs that it is still running.
fn watch_thread(watch: Arc<Watch>, log: fn(&str)) {
    let mut noted: Option<(Instant, Duration)> = None;
    loop {
        // SAFETY: zeroed kevent is a valid out value.
        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        // SAFETY: valid kqueue; one-slot event list; no timeout (block until the timer fires).
        let n = unsafe { libc::kevent(watch.kq, std::ptr::null(), 0, &mut ev, 1, std::ptr::null()) };
        let mut st = watch.lock();
        st.wakeups += 1;
        if st.stop || (n < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR)) {
            // SAFETY: the queue is ours; nobody uses it once `stop` is set (checked under the lock).
            unsafe { libc::close(watch.kq) };
            st.stop = true;
            return;
        }
        if n == 1 {
            st.armed = false; // one-shot
        }
        let Some(start) = st.unit else { continue };
        let next = match noted {
            Some((s, n)) if s == start => n,
            _ => STILL_BLOCKED_FIRST,
        };
        let elapsed = start.elapsed();
        if elapsed >= next {
            noted = Some((start, next * 2));
            watch.set_timer(&mut st, Some(next * 2 - elapsed));
            drop(st);
            log(&format!("main thread still blocked: {} ms", elapsed.as_millis()));
        } else {
            // A later unit of the same pass: wait for the rest of its first second.
            watch.set_timer(&mut st, Some(next - elapsed));
        }
    }
}

impl StallDetector {
    /// Probe mode (see the type docs).
    pub fn start(period: Duration, threshold: Duration, log: fn(&str)) -> StallDetector {
        Self::start_with_summary(period, threshold, None, log)
    }

    /// Probe mode, also logging `stall detector: ...` with the probe count, the longest wait of
    /// any probe and the stall count every `summary_every` (only when probes ran since the last
    /// summary). Logs one `stall detector started` line.
    pub fn start_with_summary(
        period: Duration,
        threshold: Duration,
        summary_every: Option<Duration>,
        log: fn(&str),
    ) -> StallDetector {
        let shared = StallShared::new();
        let s = shared.clone();
        let spawned = std::thread::Builder::new().name("ztab-stall-probe".into()).spawn(move || {
            log(&format!(
                "stall detector started (probe): period={} ms threshold={} ms (stalls of >= {} ms are always caught)",
                period.as_millis(),
                threshold.as_millis(),
                (period + threshold).as_millis()
            ));
            let mut window_start = Instant::now();
            let mut window_probes = 0u64;
            let mut window_max = Duration::ZERO;
            while !s.stop.load(Ordering::Relaxed) {
                std::thread::sleep(period);
                if s.stop.load(Ordering::Relaxed) {
                    break;
                }
                let (tx, rx) = mpsc::sync_channel::<Instant>(1);
                let posted = Instant::now();
                DispatchQueue::main().exec_async(move || {
                    let _ = tx.send(Instant::now());
                });
                let mut next_note = STILL_BLOCKED_FIRST;
                let ran = loop {
                    match rx.recv_timeout(next_note.saturating_sub(posted.elapsed())) {
                        Ok(t) => break Some(t),
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            log(&format!("main thread still blocked: {} ms", posted.elapsed().as_millis()));
                            next_note *= 2;
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break None,
                    }
                };
                let Some(ran) = ran else { break };
                let waited = ran.saturating_duration_since(posted);
                window_probes += 1;
                window_max = window_max.max(waited);
                if s.record(waited, threshold) {
                    log(&format!("main thread stall: {} ms", waited.as_millis()));
                }
                if let Some(every) = summary_every
                    && window_start.elapsed() >= every
                {
                    if window_probes > 0 {
                        log(&format!(
                            "stall detector (probe): {} probes in the last {:.0} s, longest main-queue wait {:.1} ms; stalls so far {}",
                            window_probes,
                            window_start.elapsed().as_secs_f64(),
                            window_max.as_secs_f64() * 1000.0,
                            s.count.load(Ordering::Relaxed)
                        ));
                    }
                    window_start = Instant::now();
                    window_probes = 0;
                    window_max = Duration::ZERO;
                }
            }
        });
        if let Err(e) = spawned {
            log(&format!("stall detector: thread spawn failed: {e}"));
        }
        StallDetector { shared, observers: Vec::new(), watch: None }
    }

    /// Run-loop mode (see the type docs) on `rl`, which must be the calling thread's run loop
    /// (normally `CFRunLoop::main()` from the main thread): the observers run on that thread.
    /// With `summary_every`, logs `stall detector (run loop): ...` (wake-ups, units of work, the
    /// longest unit, stalls so far) at the first wake-up after each interval. Logs one
    /// `stall detector started` line.
    pub fn observe(rl: &CFRunLoop, threshold: Duration, summary_every: Option<Duration>, log: fn(&str)) -> StallDetector {
        let shared = StallShared::new();
        let Some(watch) = Watch::new().map(Arc::new) else {
            log("stall detector: kqueue() failed; run-loop mode not started");
            return StallDetector { shared, observers: Vec::new(), watch: None };
        };
        let meter = Rc::new(RefCell::new(Meter {
            shared: shared.clone(),
            watch: watch.clone(),
            threshold,
            log,
            last: Some((Instant::now(), CFRunLoopActivity::Entry)),
            summary_every,
            window_start: Instant::now(),
            window_wakeups: 0,
            window_units: 0,
            window_max: Duration::ZERO,
        }));
        // Early observer: the start of each pass (before AppKit's own observers run). Late
        // observer: going to sleep / leaving a loop, after Core Animation's commit and the
        // autorelease pool pop, so those count as work too.
        let specs = [
            (
                CFRunLoopActivity::Entry | CFRunLoopActivity::AfterWaiting | CFRunLoopActivity::BeforeTimers | CFRunLoopActivity::BeforeSources,
                CFIndex::MIN,
            ),
            (CFRunLoopActivity::BeforeWaiting | CFRunLoopActivity::Exit, CFIndex::MAX),
        ];
        let mut observers = Vec::new();
        for (acts, order) in specs {
            let m = meter.clone();
            let block = RcBlock::new(move |_obs: *mut CFRunLoopObserver, act: CFRunLoopActivity| {
                // A re-entrant callback (a log call cannot cause one) would only skip a stamp.
                if let Ok(mut m) = m.try_borrow_mut() {
                    m.on_activity(act);
                }
            });
            // SAFETY: default allocator; the block lives as long as the observer retains it.
            if let Some(obs) = unsafe { CFRunLoopObserver::with_handler(None, acts.0, true, order, Some(&block)) } {
                // SAFETY: kCFRunLoopCommonModes is a valid static.
                rl.add_observer(Some(&obs), unsafe { kCFRunLoopCommonModes });
                observers.push(obs);
            }
        }
        let w = watch.clone();
        if let Err(e) = std::thread::Builder::new().name("ztab-stall-watch".into()).spawn(move || watch_thread(w, log)) {
            log(&format!("stall detector: watcher thread spawn failed: {e}"));
        }
        // The unit in progress (this call) is covered too.
        meter.borrow().set_unit(Some(Instant::now()));
        log(&format!(
            "stall detector started (run loop): threshold={} ms, every stall is caught; {} observers",
            threshold.as_millis(),
            observers.len()
        ));
        StallDetector { shared, observers, watch: Some(watch) }
    }

    /// (number of stalls above the threshold, longest stall in ms).
    pub fn stalls(&self) -> (u64, f64) {
        (self.shared.count.load(Ordering::Relaxed), self.shared.max_us.load(Ordering::Relaxed) as f64 / 1000.0)
    }

    /// Probe mode: (probes answered, longest wait of any probe in ms). Run-loop mode: (units of
    /// work measured, longest unit in ms).
    pub fn probes(&self) -> (u64, f64) {
        (self.shared.probes.load(Ordering::Relaxed), self.shared.max_wait_us.load(Ordering::Relaxed) as f64 / 1000.0)
    }

    /// Run-loop mode: how often the watcher thread has woken up (0 in probe mode).
    pub fn watcher_wakeups(&self) -> u64 {
        self.watch.as_ref().map_or(0, |w| w.lock().wakeups)
    }
}

impl Drop for StallDetector {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        for o in self.observers.drain(..) {
            o.invalidate();
        }
        if let Some(w) = &self.watch {
            let mut st = w.lock();
            if !st.stop {
                // Wake the watcher at once; it closes the queue and ends.
                w.set_timer(&mut st, Some(Duration::from_millis(1)));
                st.stop = true;
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// App icon prefetcher
// ---------------------------------------------------------------------------------------------

/// Pixel sizes each icon is rasterised at, each a native IconServices rendering (IconServices
/// has size-specific designs below 128 px, so a downscaled big raster looks different):
/// 192 = the largest ui.rs draw (96 pt placeholder at 2x; >= 128 px renderings are exact
/// scales of each other), 64 = mid sizes, 36 / 18 = the 18 pt label icon at 2x / 1x.
pub const ICON_PX: [usize; 4] = [192, 64, 36, 18];
/// Point size of the returned `NSImage`. All reps share it; AppKit picks the rep whose pixel
/// size fits each draw (measured: draws at 18/36/64/192 px are pixel-identical to drawing
/// `NSRunningApplication.icon` directly).
pub const ICON_PT: f64 = 96.0;

#[derive(Default)]
struct IconShared {
    /// Finished rasters (one per `ICON_PX`); `None` = the pid has no icon (gone, no bundle).
    ready: HashMap<i32, Option<Vec<CFRetained<CGImage>>>>,
    /// Queued or being loaded.
    pending: HashSet<i32>,
}

/// App icons without main-thread loading.
///
/// A worker thread looks the app up, takes `NSRunningApplication.icon` and rasterises it into
/// bitmaps at each of `ICON_PX` (so no lazy IconServices decode is left for the first draw). The
/// main thread only wraps those in an `NSImage` on first [`get`](Self::get) (tens of µs).
/// `get` never loads synchronously: an icon that is not ready yet returns `None` (draw a
/// placeholder) and is queued; `on_ready`, if given, runs on the main queue once per drained
/// batch so the UI can redraw.
///
/// Main thread only (holds `NSImage`s). Entries are keyed by pid: call
/// [`retain_pids`](Self::retain_pids) with the live pids so a reused pid never shows a stale icon.
pub struct IconPrefetcher {
    shared: Arc<Mutex<IconShared>>,
    tx: mpsc::Sender<i32>,
    images: RefCell<HashMap<i32, Retained<NSImage>>>,
}

impl IconPrefetcher {
    pub fn new(_mtm: MainThreadMarker, on_ready: Option<fn()>) -> IconPrefetcher {
        let shared = Arc::new(Mutex::new(IconShared::default()));
        let (tx, rx) = mpsc::channel::<i32>();
        let s = shared.clone();
        let _ = std::thread::Builder::new().name("ztab-icons".into()).spawn(move || {
            while let Ok(first) = rx.recv() {
                let mut next = Some(first);
                while let Some(pid) = next {
                    let img = autoreleasepool(|_| render_icon(pid));
                    if let Ok(mut g) = s.lock() {
                        // Skip if it was forgotten (retain_pids) while loading.
                        if g.pending.remove(&pid) {
                            g.ready.insert(pid, img);
                        }
                    }
                    next = rx.try_recv().ok();
                }
                if let Some(f) = on_ready {
                    DispatchQueue::main().exec_async(f);
                }
            }
        });
        IconPrefetcher { shared, tx, images: RefCell::new(HashMap::new()) }
    }

    /// Queue icon loads for pids that are neither loaded nor queued. Cheap on the main thread.
    pub fn prefetch(&self, pids: impl IntoIterator<Item = i32>) {
        let mut g = match self.shared.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let images = self.images.borrow();
        for pid in pids {
            if images.contains_key(&pid) || g.ready.contains_key(&pid) || g.pending.contains(&pid) {
                continue;
            }
            g.pending.insert(pid);
            let _ = self.tx.send(pid);
        }
    }

    /// The icon if loaded; otherwise `None` and the pid is queued (see [`prefetch`]).
    pub fn get(&self, pid: i32) -> Option<Retained<NSImage>> {
        if let Some(i) = self.images.borrow().get(&pid) {
            return Some(i.clone());
        }
        let cg = {
            let g = self.shared.lock().ok()?;
            match g.ready.get(&pid) {
                Some(Some(cg)) => cg.clone(),
                Some(None) => return None, // known: no icon
                None => {
                    let queued = g.pending.contains(&pid);
                    drop(g);
                    if !queued {
                        self.prefetch([pid]);
                    }
                    return None;
                }
            }
        };
        let size = NSSize::new(ICON_PT, ICON_PT);
        let img = NSImage::initWithSize(NSImage::alloc(), size);
        for c in &cg {
            let rep = NSBitmapImageRep::initWithCGImage(NSBitmapImageRep::alloc(), c);
            rep.setSize(size);
            img.addRepresentation(&rep);
        }
        self.images.borrow_mut().insert(pid, img.clone());
        Some(img)
    }

    /// The largest raster of the icon (`ICON_PX[0]` pixels), if loaded; queues it like `get`.
    pub fn get_cg(&self, pid: i32) -> Option<CFRetained<CGImage>> {
        let g = self.shared.lock().ok()?;
        match g.ready.get(&pid) {
            Some(Some(cg)) => cg.first().cloned(),
            Some(None) => None,
            None => {
                let queued = g.pending.contains(&pid);
                drop(g);
                if !queued {
                    self.prefetch([pid]);
                }
                None
            }
        }
    }

    /// Every raster of the icon (`ICON_PX`), if loaded (empty otherwise; queues it like `get`).
    pub fn get_cg_all(&self, pid: i32) -> Vec<CFRetained<CGImage>> {
        let Ok(g) = self.shared.lock() else { return Vec::new() };
        match g.ready.get(&pid) {
            Some(Some(cg)) => cg.clone(),
            Some(None) => Vec::new(),
            None => {
                let queued = g.pending.contains(&pid);
                drop(g);
                if !queued {
                    self.prefetch([pid]);
                }
                Vec::new()
            }
        }
    }

    /// True if `pid` has been loaded (with or without an icon) and `get` will not queue it.
    pub fn is_loaded(&self, pid: i32) -> bool {
        self.images.borrow().contains_key(&pid)
            || self.shared.lock().map(|g| g.ready.contains_key(&pid)).unwrap_or(false)
    }

    /// Drop everything cached or queued for one pid (the app quit; its pid may be reused).
    pub fn forget(&self, pid: i32) {
        self.images.borrow_mut().remove(&pid);
        if let Ok(mut g) = self.shared.lock() {
            g.ready.remove(&pid);
            g.pending.remove(&pid);
        }
    }

    /// Drop everything cached or queued for pids not in `live`.
    pub fn retain_pids(&self, live: &[i32]) {
        self.images.borrow_mut().retain(|p, _| live.contains(p));
        if let Ok(mut g) = self.shared.lock() {
            g.ready.retain(|p, _| live.contains(p));
            g.pending.retain(|p| live.contains(p));
        }
    }
}

/// Worker-thread icon load + rasterisation. `None` if the app is gone or has no icon.
fn render_icon(pid: i32) -> Option<Vec<CFRetained<CGImage>>> {
    let icon = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?.icon()?;
    ICON_PX.iter().map(|&px| rasterise(&icon, px)).collect()
}

fn rasterise(icon: &NSImage, px: usize) -> Option<CFRetained<CGImage>> {
    let side = px as f64;
    // sRGB, like the panel (see `thumbs::srgb`).
    // SAFETY: CoreGraphics-provided constant.
    let cs = CGColorSpace::with_name(Some(unsafe { objc2_core_graphics::kCGColorSpaceSRGB }))?;
    let info = CGImageByteOrderInfo::Order32Little.0 | CGImageAlphaInfo::PremultipliedFirst.0;
    // SAFETY: null data lets CG allocate; sizes are positive.
    let ctx: CFRetained<CGContext> =
        unsafe { CGBitmapContextCreate(std::ptr::null_mut::<c_void>(), px, px, 8, 0, Some(&cs), info)? };
    // The reference context matters: with none, IconServices hands back a 128 px rendering for
    // small requests (wrong size-specific design); with our bitmap it renders exactly px.
    let gc = NSGraphicsContext::graphicsContextWithCGContext_flipped(&ctx, false);
    let mut rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(side, side));
    // SAFETY: rect is a valid pointer, no hints.
    let src = unsafe { icon.CGImageForProposedRect_context_hints(&mut rect, Some(&gc), None) }?;
    // Draw into our own bitmap so the result is fully decoded, whatever `src` is backed by.
    CGContext::set_interpolation_quality(Some(&ctx), CGInterpolationQuality::High);
    CGContext::draw_image(
        Some(&ctx),
        CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(side, side)),
        Some(&src),
    );
    CGBitmapContextCreateImage(Some(&ctx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_core_foundation::{CFAbsoluteTimeGetCurrent, CFRunLoopTimer, kCFRunLoopDefaultMode};

    #[test]
    fn pause_gating() {
        let active = Activity { locked: false, off_console: false, idle_s: 3.0 };
        assert_eq!(pause_reason(&active, false, true, IDLE_PAUSE_S), None);
        assert_eq!(pause_reason(&Activity { locked: true, ..active }, false, true, IDLE_PAUSE_S), Some("screen locked"));
        assert_eq!(pause_reason(&active, true, true, IDLE_PAUSE_S), Some("displays asleep"));
        assert_eq!(pause_reason(&active, false, false, IDLE_PAUSE_S), Some("session inactive"));
        assert_eq!(pause_reason(&Activity { off_console: true, ..active }, false, true, IDLE_PAUSE_S), Some("session inactive"));
        assert_eq!(pause_reason(&Activity { idle_s: 119.0, ..active }, false, true, IDLE_PAUSE_S), None);
        assert_eq!(pause_reason(&Activity { idle_s: 120.0, ..active }, false, true, IDLE_PAUSE_S), Some("user idle"));
        // Locked wins over the other reasons (it is the most specific one for the log).
        assert_eq!(pause_reason(&Activity { locked: true, idle_s: 900.0, ..active }, true, false, IDLE_PAUSE_S), Some("screen locked"));
    }

    static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());
    fn capture(s: &str) {
        LOG.lock().unwrap().push(s.to_string());
    }

    /// A one-shot timer on the current run loop that blocks for `ms` when it fires.
    fn blocking_timer(rl: &CFRunLoop, ms: u64) {
        let block = RcBlock::new(move |_t: *mut CFRunLoopTimer| std::thread::sleep(Duration::from_millis(ms)));
        // SAFETY: default allocator, valid block, one-shot.
        let t = unsafe { CFRunLoopTimer::with_handler(None, CFAbsoluteTimeGetCurrent() + 0.05, 0.0, 0, 0, Some(&block)) }.unwrap();
        // SAFETY: kCFRunLoopDefaultMode is a valid static.
        rl.add_timer(Some(&t), unsafe { kCFRunLoopDefaultMode });
    }

    fn run_for(secs: f64) {
        // SAFETY: kCFRunLoopDefaultMode is a valid static.
        CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, secs, false);
    }

    fn stall_ms(lines: &[String]) -> Vec<u64> {
        lines
            .iter()
            .filter_map(|l| l.strip_prefix("main thread stall: ")?.split(' ').next()?.parse().ok())
            .collect()
    }

    #[test]
    fn run_loop_mode_catches_each_stall_and_sleeps_while_idle() {
        // A private thread's run loop stands in for the main one.
        let r = std::thread::spawn(|| {
            let rl = CFRunLoop::current().unwrap();
            // Keeps the mode non-empty (fires in an hour).
            let keep = RcBlock::new(|_t: *mut CFRunLoopTimer| {});
            // SAFETY: as in `blocking_timer`.
            let t = unsafe { CFRunLoopTimer::with_handler(None, CFAbsoluteTimeGetCurrent() + 3600.0, 3600.0, 0, 0, Some(&keep)) }.unwrap();
            rl.add_timer(Some(&t), unsafe { kCFRunLoopDefaultMode });
            let det = StallDetector::observe(&rl, Duration::from_millis(50), None, capture);
            std::thread::sleep(Duration::from_millis(20));
            let w0 = det.watcher_wakeups();
            run_for(0.4); // idle: the run loop sleeps
            let idle_wakeups = det.watcher_wakeups() - w0;
            // Busy-ish but never stalled: a repeating 50 ms timer wakes the run loop ~20 times.
            let tick = RcBlock::new(|_t: *mut CFRunLoopTimer| std::thread::sleep(Duration::from_millis(1)));
            // SAFETY: as in `blocking_timer`.
            let rep = unsafe { CFRunLoopTimer::with_handler(None, CFAbsoluteTimeGetCurrent() + 0.05, 0.05, 0, 0, Some(&tick)) }.unwrap();
            rl.add_timer(Some(&rep), unsafe { kCFRunLoopDefaultMode });
            let (w1, u1) = (det.watcher_wakeups(), det.probes().0);
            run_for(1.2);
            rep.invalidate();
            let ticking = (det.watcher_wakeups() - w1, det.probes().0 - u1);
            let idle_stalls = det.stalls().0;
            blocking_timer(&rl, 150);
            run_for(0.4);
            let after_short = det.stalls();
            blocking_timer(&rl, 1300);
            run_for(1.6);
            (idle_wakeups, ticking, idle_stalls, after_short, det.stalls(), det.probes())
        })
        .join()
        .unwrap();
        let (idle_wakeups, (tick_wakeups, tick_units), idle_stalls, after_short, after_long, (units, _)) = r;
        let lines = LOG.lock().unwrap().clone();
        assert_eq!(idle_wakeups, 0, "the watcher sleeps while the run loop sleeps; {lines:?}");
        assert!(tick_units >= 20, "the repeating timer ran: {tick_units} units");
        assert_eq!(tick_wakeups, 0, "short units of work never wake the watcher ({tick_units} units); {lines:?}");
        assert_eq!(idle_stalls, 0, "{lines:?}");
        assert_eq!(after_short.0, 1, "{lines:?}");
        assert!((145.0..400.0).contains(&after_short.1), "150 ms unit measured as {} ms", after_short.1);
        assert_eq!(after_long.0, 2, "{lines:?}");
        assert!((1295.0..1700.0).contains(&after_long.1), "1300 ms unit measured as {} ms", after_long.1);
        let ms = stall_ms(&lines);
        assert_eq!(ms.len(), 2, "one log line per stall: {lines:?}");
        assert!(lines.iter().any(|l| l.starts_with("main thread still blocked: ")), "a long stall is reported while it lasts: {lines:?}");
        assert!(units > 5, "{units} units");
    }
}
