//! ZTab: a Windows-style Alt+Tab (Option+Tab) window switcher for macOS.
//!
//! Hold Option, press Tab to open the switcher showing every window (grouped by app),
//! keep pressing Tab / Shift+Tab (or arrows) to move, release Option to switch.
//! Click a tile with the mouse while holding Option to switch to it. Esc cancels.
//! Option+` opens the switcher for the frontmost app's windows only.
//!
//! Main-thread rule: nothing on the main thread may wait on another app, make an Accessibility
//! call, capture the screen or touch the disk. The keyboard event tap runs on its own
//! `ztab-tap` thread and queues its events to the main queue, so a busy main thread never holds
//! the keyboard; it only delays ZTab's reaction. The window list comes from the persistent
//! [`model::WindowModel`] (fed by background threads), thumbnails and icons load on background
//! queues, focusing runs on the [`windows::Focuser`] threads and the log is written by a logger
//! thread. A [`sysstate::StallDetector`] logs every main-thread stall. Background work
//! (thumbnail refresh, the model's safety-net rescans) pauses while nobody can see the screen or
//! the user is away, and the periodic timers slow down meanwhile.

mod hotkey;
mod model;
mod settings;
mod settings_window;
mod sysstate;
mod thumbs;
mod ui;
mod windows;

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashSet;
use std::io::Write as _;
use std::rc::Rc;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::{DispatchQoS, DispatchQueue, DispatchTime, GlobalQueueIdentifier};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject, Sel};
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSImage, NSMenu, NSMenuDelegate, NSMenuItem,
    NSStatusBar, NSStatusItem, NSVariableStatusItemLength, NSWorkspace,
    NSWorkspaceScreensDidSleepNotification, NSWorkspaceScreensDidWakeNotification,
    NSWorkspaceSessionDidBecomeActiveNotification, NSWorkspaceSessionDidResignActiveNotification,
};
use objc2_core_foundation::{CFAbsoluteTimeGetCurrent, CFRunLoop, CFRunLoopTimer, kCFRunLoopCommonModes};
use objc2_foundation::{NSDistributedNotificationCenter, NSNotification, NSNotificationName, NSOperationQueue, NSString, NSTimer};

use hotkey::{Binding, Hotkey, HotkeyEvent, HotkeyHandle, Mods, Which};
use model::{ModelEvent, OwnWindow, Snapshot, WindowModel};
use settings::{Paths, Settings, Writer};
use settings_window::{MsgKind, SettingsEvent, SettingsWindow};
use sysstate::{Activity, IconPrefetcher, SecureInput, StallDetector};
use thumbs::Thumbnailer;
use ui::{Switcher, Tile};
use windows::{FocusStage, Focuser, WinInfo};

/// Thumbnails are captured to fit this many points (2x pixels) — about the largest tile size.
const THUMB_MAX_W: f64 = 300.0;
const THUMB_MAX_H: f64 = 190.0;
/// Background thumbnail refresh: period, maximum age of an on-screen window's thumbnail, and
/// captures started per tick.
const REFRESH_EVERY_S: f64 = 2.0;
const REFRESH_MAX_AGE: Duration = Duration::from_secs(8);
const REFRESH_BUDGET: usize = 3;
/// When the panel appears, windows whose thumbnail is missing or older than this are captured
/// (a quick tap that never shows the panel captures nothing).
const SHOW_CAPTURE_MAX_AGE: Duration = Duration::from_secs(3);
/// Timer slack macOS may use to coalesce ZTab's periodic wake-ups with others.
const TIMER_TOLERANCE_S: f64 = 0.5;
/// Watchdog period while ZTab runs normally.
const WATCHDOG_EVERY_S: f64 = 1.5;
/// While background work is paused, one timer does the watchdog and the activity read: every
/// `IDLE_TICK_S` while the user is idle (any ⌥Tab resumes at once), every `SCREEN_OFF_TICK_S`
/// while the screen is off (locked, displays asleep, session switched out), which notifications
/// normally end before the next tick.
const IDLE_TICK_S: f64 = 10.0;
const SCREEN_OFF_TICK_S: f64 = 30.0;
/// A window that became current is captured this long afterwards (unless a fresh thumbnail
/// exists), so a window minimized while focused (Cmd+M, no FocusLeft) still has a real image.
const FOCUSED_CAPTURE_AFTER: Duration = Duration::from_millis(1000);
const FOCUSED_CAPTURE_MAX_AGE: Duration = Duration::from_secs(3);
/// Main-thread stall detector: a unit of main-thread work longer than this is a stall. The
/// summary interval can be changed with `--stall-summary-s`; `--stall-period-ms` adds the probe
/// mode (see `sysstate::StallDetector`).
const STALL_THRESHOLD_MS: u64 = 50;
const STALL_SUMMARY_S: u64 = 600;
/// Status-bar symbols: normal, and while secure input keeps ⌥Tab from reaching ZTab.
const SYMBOL_NORMAL: &str = "rectangle.on.rectangle";
const SYMBOL_SECURE: &str = "lock.rectangle.on.rectangle";

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGRequestScreenCaptureAccess() -> bool;
}

// ---------------------------------------------------------------------------------------------
// Logging (written by a logger thread; the caller only stamps the time and queues the line)
// ---------------------------------------------------------------------------------------------

/// When the log reaches this size it is moved to `ztab.log.1` (replacing the previous one).
const LOG_MAX_BYTES: u64 = 1024 * 1024;

enum LogMsg {
    Line(f64, String),
    Flush(mpsc::SyncSender<()>),
}

fn log_path() -> String {
    std::env::var("HOME").unwrap_or_default() + "/Library/Logs/ztab.log"
}

fn now_secs() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn write_log_line(ts: f64, msg: &str) {
    let path = log_path();
    if std::fs::metadata(&path).map(|m| m.len() >= LOG_MAX_BYTES).unwrap_or(false) {
        let _ = std::fs::rename(&path, format!("{path}.1"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "[{ts:.3}] {msg}");
    }
}

fn logger() -> &'static mpsc::Sender<LogMsg> {
    static TX: OnceLock<mpsc::Sender<LogMsg>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<LogMsg>();
        let spawned = std::thread::Builder::new().name("ztab-log".into()).spawn(move || {
            while let Ok(m) = rx.recv() {
                match m {
                    LogMsg::Line(ts, s) => write_log_line(ts, &s),
                    LogMsg::Flush(done) => {
                        let _ = done.send(());
                    }
                }
            }
        });
        if spawned.is_err() {
            write_log_line(now_secs(), "logger thread failed to start; logging synchronously");
        }
        tx
    })
}

/// Queue one log line (any thread). The timestamp is taken here, at the call.
fn log(msg: &str) {
    let ts = now_secs();
    if logger().send(LogMsg::Line(ts, msg.to_string())).is_err() {
        write_log_line(ts, msg);
    }
    if std::env::var_os("ZTAB_STDERR").is_some() {
        eprintln!("{msg}");
    }
}

/// Wait (at most `timeout`) until every queued line is written. Call before exiting.
fn log_flush(timeout: Duration) {
    let (tx, rx) = mpsc::sync_channel(1);
    if logger().send(LogMsg::Flush(tx)).is_ok() {
        let _ = rx.recv_timeout(timeout);
    }
}

fn ms_since(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

/// When this process started (unix seconds, from the kernel), for the launch timeline.
fn process_start_secs() -> Option<f64> {
    static START: OnceLock<Option<f64>> = OnceLock::new();
    *START.get_or_init(|| {
        // SAFETY: proc_bsdinfo is plain old data; proc_pidinfo fills at most `size` bytes of it.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        let n = unsafe {
            libc::proc_pidinfo(std::process::id() as i32, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size)
        };
        (n == size).then(|| info.pbi_start_tvsec as f64 + info.pbi_start_tvusec as f64 / 1e6)
    })
}

/// Milliseconds since this process started ("?" when unknown), for launch log lines.
fn since_launch(at_unix: f64) -> String {
    process_start_secs().map(|s| format!("{:.0}", (at_unix - s) * 1000.0)).unwrap_or_else(|| "?".into())
}

/// Unix time of an `Instant` taken earlier (for the launch timeline).
fn unix_of(t: Instant) -> f64 {
    now_secs() - t.elapsed().as_secs_f64()
}

fn after_on_main(delay: Duration, f: impl FnOnce() + Send + 'static) {
    let when = DispatchTime::try_from(delay).unwrap_or(DispatchTime::NOW);
    let _ = DispatchQueue::main().after(when, f);
}

/// Main thread: run `f` on the main thread after `delay`, on time. `after_on_main` goes through
/// GCD, which lets a timer fire up to a tenth of its delay late to coalesce wake-ups (3–7 ms for
/// the 120 ms show delay); this is a main run-loop timer with no tolerance, in the common modes
/// (like the main queue).
fn after_on_main_exact(delay: Duration, f: impl FnOnce() + 'static) {
    let cell = RefCell::new(Some(f));
    let block = RcBlock::new(move |_t: *mut CFRunLoopTimer| {
        if let Some(f) = cell.borrow_mut().take() {
            f();
        }
    });
    let Some(rl) = CFRunLoop::main() else { return };
    // SAFETY: default allocator, a valid block (the timer copies it), a one-shot timer.
    let timer = unsafe { CFRunLoopTimer::with_handler(None, CFAbsoluteTimeGetCurrent() + delay.as_secs_f64(), 0.0, 0, 0, Some(&block)) };
    let Some(timer) = timer else { return };
    // SAFETY: zero tolerance is valid; kCFRunLoopCommonModes is a valid static.
    unsafe {
        timer.set_tolerance(0.0);
        rl.add_timer(Some(&timer), kCFRunLoopCommonModes);
    }
}

// ---------------------------------------------------------------------------------------------
// Switcher state
// ---------------------------------------------------------------------------------------------

struct State {
    mtm: MainThreadMarker,
    switcher: Switcher,
    thumbs: Thumbnailer,
    icons: IconPrefetcher,
    /// Started once Accessibility is granted (or right away for the diagnostics modes).
    model: RefCell<Option<Rc<WindowModel>>>,
    model_ready_logged: Cell<bool>,
    /// "first ⌥Tab usable" was logged.
    usable_logged: Cell<bool>,
    /// Windows currently offered by the switcher (same order as the tiles).
    wins: RefCell<Vec<WinInfo>>,
    /// Tiles built for the current session but not shown yet (show is delayed).
    pending_tiles: RefCell<Option<Vec<Tile>>>,
    /// Tile indices whose app icon was not loaded yet when the session opened.
    icon_missing: RefCell<HashSet<usize>>,
    selected: Cell<usize>,
    /// MRU order of `wins` (indices), for the capture order when the panel appears.
    mru_order: RefCell<Vec<usize>>,
    /// Bumped every time a session opens or ends; stale async work compares against it.
    generation: Cell<u64>,
    /// True between Open and Commit/Cancel.
    active: Cell<bool>,
    /// When the current session opened (for the "panel shown" latency), and how long its Open
    /// waited between the event tap and the main thread.
    opened_at: Cell<Option<Instant>>,
    open_tap_ms: Cell<Option<f64>>,
    /// Thumbnails that landed while the current session's panel was up: (wid, ms after the
    /// Open), for the `panel hidden` line.
    session_landed: RefCell<Vec<(u32, f64)>>,
    /// The event tap (on its own `ztab-tap` thread) once installed, and whether an install is
    /// under way.
    hk_handle: Cell<Option<HotkeyHandle>>,
    hk_installing: Cell<bool>,
    /// Tap events handled here so far (compared with the tap's count by the watchdog).
    hk_handled: Cell<u64>,
    /// Tap session number of the current switcher session.
    tap_session: Cell<u64>,
    menu: RefCell<Option<MenuParts>>,
    install_failed_logged: Cell<bool>,
    /// Secure input state last shown in the menu (`None` = off).
    secure: RefCell<Option<SecureInput>>,
    /// Run-loop stall detector, plus the probe one with `--stall-period-ms`.
    stall: RefCell<Vec<StallDetector>>,
    /// Window switching (off the main thread).
    focuser: Focuser,
    /// Why background work is paused (None: running). See `update_pause`.
    paused: Cell<Option<&'static str>>,
    /// The current pause had a screen-off reason at some point (resuming rescans every app).
    pause_rescan: Cell<bool>,
    /// Latest session activity applied, and the slot the background reads fill.
    activity: Cell<Activity>,
    activity_slot: Arc<Mutex<ActivitySlot>>,
    /// The periodic timers and how they are set up (see `schedule_timers`).
    timers: RefCell<Vec<Retained<NSTimer>>>,
    timer_mode: Cell<Option<TimerMode>>,
    /// From NSWorkspace notifications.
    displays_asleep: Cell<bool>,
    session_active: Cell<bool>,
    /// Idle time after which background work pauses (`--idle-pause-s`, default 120 s).
    idle_limit_s: Cell<f64>,
    /// Last time the user opened the switcher (counts as input for the idle pause).
    user_active_at: Cell<Option<Instant>>,
    ws_observers: RefCell<Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>>,
    /// The settings in effect (the show delay is read at every Open; the shortcuts live in the
    /// tap, see `apply_settings`), where they are stored, and the thread that writes them.
    settings: RefCell<Settings>,
    paths: Paths,
    writer: Writer,
    /// The Settings window, built the first time it is opened, then kept.
    settings_win: RefCell<Option<Rc<SettingsWindow>>>,
    /// Start-at-login state last read from the agent file (None: not read yet).
    login_state: RefCell<Option<settings::LoginState>>,
}

thread_local! {
    static STATE: OnceCell<Rc<State>> = const { OnceCell::new() };
}

/// Session activity read on a background queue, for the main thread.
#[derive(Default)]
struct ActivitySlot {
    /// The newest read not applied yet.
    latest: Option<Activity>,
    /// What the main thread last applied, and the idle limit (to tell whether a read changes the
    /// pause decision).
    applied: Activity,
    idle_limit_s: f64,
}

impl ActivitySlot {
    /// Whether `a` may change the pause decision against what was applied.
    fn decisive(&self, a: &Activity) -> bool {
        let idle = |x: &Activity| x.idle_s >= self.idle_limit_s;
        a.locked != self.applied.locked || a.off_console != self.applied.off_console || idle(a) != idle(&self.applied)
    }
}

/// How the periodic timers run (see `State::schedule_timers`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TimerMode {
    /// Watchdog every 1.5 s, refresh every 2 s.
    Normal,
    /// Paused, user idle: one combined tick every `IDLE_TICK_S`.
    Idle,
    /// Paused, screen off: one combined tick every `SCREEN_OFF_TICK_S`.
    ScreenOff,
}

fn state() -> Option<Rc<State>> {
    STATE.with(|s| s.get().cloned())
}

/// Keep the windows `keep` accepts; returns (windows, current, mru) re-indexed.
fn filter_snapshot(snap: Snapshot, keep: impl Fn(&WinInfo) -> bool) -> (Vec<WinInfo>, Option<usize>, Vec<usize>) {
    let mut map: Vec<Option<usize>> = vec![None; snap.windows.len()];
    let mut out = Vec::with_capacity(snap.windows.len());
    for (i, w) in snap.windows.into_iter().enumerate() {
        if keep(&w) {
            map[i] = Some(out.len());
            out.push(w);
        }
    }
    let current = snap.current.and_then(|c| map[c]);
    let mru = snap.mru.iter().filter_map(|&i| map[i]).collect();
    (out, current, mru)
}

/// Which window a session preselects: Shift => least recently used; otherwise the previous
/// window when the user is in a window right now, else the most recent one.
fn initial_selection(current: Option<usize>, mru: &[usize], reverse: bool) -> Option<usize> {
    if reverse {
        mru.last().copied()
    } else if current.is_some() {
        mru.get(1).or(mru.first()).copied()
    } else {
        mru.first().copied()
    }
}

impl State {
    fn model(&self) -> Option<Rc<WindowModel>> {
        self.model.borrow().clone()
    }

    /// Close the tap's session `session` (unless the tap has opened a newer one meanwhile).
    fn close_tap(&self, session: u64) {
        if let Some(h) = self.hk_handle.get() {
            h.close_session(session);
        }
    }

    /// Start the window model (once). Needs Accessibility to see anything but the CG list.
    fn ensure_model(&self, force: bool) {
        if self.model.borrow().is_some() || !(force || windows::ax_trusted()) {
            return;
        }
        self.adopt_model(WindowModel::start(self.mtm));
    }

    /// Take over a started window model (main() starts it before AppKit when it can).
    fn adopt_model(&self, m: Rc<WindowModel>) {
        if self.model.borrow().is_some() {
            return;
        }
        m.set_event_sink(Box::new(|e| {
            if let Some(st) = state() {
                st.on_model_event(e);
            }
        }));
        m.set_paused(self.paused.get().is_some(), false);
        *self.model.borrow_mut() = Some(m);
        log("window model started");
        self.schedule_timers();
        self.poll_model_ready(Instant::now());
    }

    /// Log once when the model has scanned every app, then prefetch every app icon.
    fn poll_model_ready(&self, started: Instant) {
        let Some(m) = self.model() else { return };
        self.check_usable();
        if m.ready() {
            self.model_ready_logged.set(true);
            let snap = m.snapshot();
            let pids: Vec<i32> = snap.windows.iter().map(|w| w.pid).collect();
            self.icons.prefetch(pids);
            log(&format!(
                "model ready after {:.0} ms ({} ms after process start): {} windows; {}",
                ms_since(started),
                since_launch(now_secs()),
                snap.windows.len(),
                m.stats()
            ));
            return;
        }
        if started.elapsed() > Duration::from_secs(30) {
            log(&format!("model not ready after 30 s: {}", m.stats()));
            self.model_ready_logged.set(true);
            return;
        }
        // Fine-grained during start-up (a cheap check under one lock), so the icons prefetch and
        // the log line are not held back by the poll.
        let every = if started.elapsed() < Duration::from_secs(2) { 5 } else { 50 };
        after_on_main(Duration::from_millis(every), move || {
            if let Some(st) = state() {
                st.poll_model_ready(started);
            }
        });
    }

    /// Log once when ⌥Tab first works: the tap is installed, the main thread runs, and the window
    /// model has a window list (with only some apps scanned so far, the switcher already lists
    /// every window from it).
    fn check_usable(&self) {
        if self.usable_logged.get() || self.hk_handle.get().is_none() || !self.model().is_some_and(|m| m.has_window_list()) {
            return;
        }
        self.usable_logged.set(true);
        log(&format!("first ⌥Tab usable {} ms after process start (tap live, main thread running, window list ready)", since_launch(now_secs())));
    }

    /// Start a switcher session. Returns false if there is nothing to show. `tap_ms`: how long the
    /// Open waited between the event tap and this call, for the log.
    fn open(&self, reverse: bool, same_app: bool, tap_ms: Option<f64>) -> bool {
        let t0 = Instant::now();
        let Some(model) = self.model() else {
            log("open: window model not running (Accessibility not granted?)");
            return false;
        };
        let gen_ = self.generation.get() + 1;
        self.generation.set(gen_);

        let snap = model.snapshot();
        let t_list = ms_since(t0);
        let active_pid = snap.active_pid.or_else(|| snap.current.map(|c| snap.windows[c].pid));
        let (list, current, mru) = if same_app {
            filter_snapshot(snap, |w| Some(w.pid) == active_pid)
        } else {
            filter_snapshot(snap, |_| true)
        };
        let Some(sel) = initial_selection(current, &mru, reverse) else {
            log(&format!("open: no windows (same_app={same_app} active_pid={active_pid:?})"));
            return false;
        };

        let mut group = 0usize;
        let mut tiles = Vec::with_capacity(list.len());
        let mut missing = HashSet::new();
        for (i, w) in list.iter().enumerate() {
            if i > 0 && list[i - 1].pid != w.pid {
                group += 1;
            }
            let tile = self.make_tile(w, group, false);
            if tile.icon.is_none() {
                missing.insert(i);
            }
            tiles.push(tile);
        }

        // No capture here: a quick tap (the common case) never shows the panel, and its switch
        // must not compete with captures. `show_now` captures what is stale. The tile labels are
        // rendered off the main thread during the show delay (only those not cached).
        let t_labels = Instant::now();
        let labels = self.switcher.prefetch_labels(&tiles);
        let labels_ms = ms_since(t_labels);
        let mru_wids: Vec<String> = mru.iter().map(|&i| list[i].wid.to_string()).collect();
        let tap = tap_ms.map(|ms| format!(" tap_to_main_ms={ms:.1}")).unwrap_or_default();
        let delay = self.settings.borrow().show_delay_ms;
        log(&format!(
            "open: {} windows, current={current:?} selected={sel} wid={} (reverse={reverse}, same_app={same_app}) \
             active_pid={active_pid:?} mru={} delay_ms={delay} list_ms={t_list:.2} labels_queued={labels} \
             labels_ms={labels_ms:.2} total_ms={:.2}{tap}",
            list.len(),
            list[sel].wid,
            mru_wids.join(","),
            ms_since(t0)
        ));
        *self.wins.borrow_mut() = list;
        *self.pending_tiles.borrow_mut() = Some(tiles);
        *self.icon_missing.borrow_mut() = missing;
        self.selected.set(sel);
        self.active.set(true);
        self.schedule_timers();
        self.opened_at.set(Some(t0));
        self.open_tap_ms.set(tap_ms);
        self.session_landed.borrow_mut().clear();
        self.mru_order.replace(mru);

        // Show after the delay (Settings: 0-300 ms, default 120) so a quick Option+Tab just
        // flips windows; 0 shows it at once.
        if delay == 0 {
            self.show_now();
        } else {
            after_on_main_exact(Duration::from_millis(delay), move || {
                if let Some(st) = state()
                    && st.generation.get() == gen_
                {
                    st.show_now();
                }
            });
        }
        true
    }

    /// Launch: draw the panel once, invisibly, with the current windows (before the stall
    /// detector starts), so the first ⌥Tab does not pay the one-time setup (measured 80 ms of
    /// main-thread time on the first show otherwise).
    fn warm_up_panel(&self) {
        let Some(m) = self.model() else { return };
        if self.active.get() {
            return;
        }
        let t = Instant::now();
        let snap = m.snapshot();
        // Windows without a thumbnail yet get a placeholder bitmap, so the image path is warmed.
        let mut group = 0usize;
        let mut tiles: Vec<Tile> = Vec::with_capacity(snap.windows.len());
        for (i, w) in snap.windows.iter().enumerate() {
            if i > 0 && snap.windows[i - 1].pid != w.pid {
                group += 1;
            }
            tiles.push(self.make_tile(w, group, true));
        }
        let n = tiles.len();
        self.switcher.warm_up(tiles);
        log(&format!(
            "panel warm-up: {n} tiles, {:.1} ms (draw {:.1} ms; {})",
            ms_since(t),
            self.switcher.take_max_draw_ms(),
            self.switcher.layer_info()
        ));
        after_on_main(Duration::from_millis(200), || {
            if let Some(st) = state() {
                st.switcher.end_warm_up();
            }
        });
    }

    fn show_now(&self) {
        if !self.active.get() {
            return;
        }
        let Some(tiles) = self.pending_tiles.borrow_mut().take() else { return };
        let has_thumb: Vec<bool> = tiles.iter().map(|t| t.thumbnail.is_some()).collect();
        let t_show = Instant::now();
        self.switcher.show(tiles, self.selected.get());
        let show_ms = ms_since(t_show);
        self.capture_for_panel();
        // Panel frame in CG (top-left origin) coordinates, for diagnostics / e2e tests.
        let f = self.switcher.panel().frame();
        let primary_h = objc2_app_kit::NSScreen::screens(self.mtm).firstObject().map(|s| s.frame().size.height).unwrap_or(0.0);
        let (px, py) = (f.origin.x, primary_h - f.origin.y - f.size.height);
        let centers: Vec<String> = {
            let wins = self.wins.borrow();
            self.switcher
                .layout()
                .tiles
                .iter()
                .zip(wins.iter())
                .zip(has_thumb.iter())
                .map(|((r, w), t)| format!("{}:{:.0},{:.0}:{}", w.wid, px + r.cx(), py + r.cy(), u8::from(*t)))
                .collect()
        };
        let after = self.opened_at.get().map(ms_since).unwrap_or(0.0);
        log(&format!(
            "panel shown: cg_frame={} {} {} {} open_to_show_ms={after:.1} show_ms={show_ms:.1} ({}) thumbs={}/{} tiles={}",
            px,
            py,
            f.size.width,
            f.size.height,
            self.switcher.last_show_breakdown(),
            has_thumb.iter().filter(|t| **t).count(),
            has_thumb.len(),
            centers.join(";")
        ));
        // How long the main thread stays busy after ordering the panel in (the rest of this unit
        // of work, Core Animation's commit with the panel's first draw, anything queued behind),
        // whatever the stall threshold: the panel is on screen at the end of it.
        let (opened, tap_ms, delay) = (self.opened_at.get(), self.open_tap_ms.get(), self.settings.borrow().show_delay_ms);
        sysstate::when_main_idle(t_show, move |busy, longest| {
            let key = opened.map(|t| ms_since(t) + tap_ms.unwrap_or(0.0));
            let key = key.map(|k| format!(" key_to_settled_ms={k:.1} (after the {delay} ms delay: {:.1})", k - delay as f64)).unwrap_or_default();
            log(&format!("panel settled: main thread busy {busy:.1} ms after ordering it in (longest unit {longest:.1} ms){key}"));
        });
    }

    /// The panel is visible: capture, in the background, the windows whose thumbnail is missing
    /// or older than `SHOW_CAPTURE_MAX_AGE`: the selected one first, then in MRU order. Minimized
    /// windows and windows of hidden apps are skipped (they cannot be captured; their image is
    /// the one taken when they lost focus); windows that failed recently back off.
    fn capture_for_panel(&self) {
        let gen_ = self.generation.get();
        let wins = self.wins.borrow();
        let sel = self.selected.get();
        let order = self.mru_order.borrow();
        let wids: Vec<u32> = std::iter::once(sel)
            .chain(order.iter().copied().filter(|&i| i != sel))
            .filter_map(|i| wins.get(i))
            .filter(|w| !w.minimized && !w.hidden_app)
            .map(|w| w.wid)
            .collect();
        self.thumbs.capture_stale(wids, Some(SHOW_CAPTURE_MAX_AGE), THUMB_MAX_W, THUMB_MAX_H, gen_, |g, wid, img| {
            if let Some(st) = state() {
                st.on_thumbnail(g, wid, img);
            }
        });
    }

    fn on_thumbnail(&self, generation: u64, wid: u32, img: Retained<NSImage>) {
        if generation != self.generation.get() || !self.active.get() {
            return;
        }
        self.note_landed(wid);
        self.put_thumbnail(wid, img);
    }

    fn note_landed(&self, wid: u32) {
        let at = self.opened_at.get().map(ms_since).unwrap_or(0.0);
        self.session_landed.borrow_mut().push((wid, at));
    }

    /// A background capture (`refresh_stale` / `capture_one`) landed in the cache.
    fn on_thumb_update(&self, wid: u32) {
        if !self.active.get() {
            return;
        }
        self.note_landed(wid);
        if let Some(img) = self.thumbs.cached(wid) {
            self.put_thumbnail(wid, img);
        }
    }

    fn put_thumbnail(&self, wid: u32, img: Retained<NSImage>) {
        let Some(idx) = self.wins.borrow().iter().position(|w| w.wid == wid) else { return };
        // The bitmap behind it (the cache already holds this capture, or a newer one).
        let (img, cg) = match self.thumbs.cached_pair(wid) {
            Some((ns, cg)) => (ns, Some(cg)),
            None => (img, None),
        };
        if let Some(tiles) = self.pending_tiles.borrow_mut().as_mut() {
            if let Some(t) = tiles.get_mut(idx) {
                t.thumbnail = Some(img);
                t.thumb_cg = cg;
            }
            return;
        }
        self.switcher.set_thumbnail(idx, img, cg);
    }

    /// The tile of window `w` (app group `group`) with what is cached for it right now: icon,
    /// thumbnail (`placeholder`: a grey one when there is none, for the launch warm-up).
    fn make_tile(&self, w: &WinInfo, group: usize, placeholder: bool) -> Tile {
        let (thumbnail, thumb_cg) = match self.thumbs.cached_pair(w.wid) {
            Some((ns, cg)) => (Some(ns), Some(cg)),
            None if placeholder => (thumbs::placeholder_image(THUMB_MAX_W, THUMB_MAX_H * 0.6), None),
            None => (None, None),
        };
        let (_, _, bw, bh) = w.bounds;
        Tile {
            app_name: w.app_name.clone(),
            title: w.title.clone(),
            icon: self.icons.get(w.pid),
            icon_cg: self.icons.get_cg(w.pid),
            icon_reps: self.icons.get_cg_all(w.pid),
            thumbnail,
            thumb_cg,
            aspect: if bh > 0.0 { bw / bh } else { 1.6 },
            minimized: w.minimized,
            group,
        }
    }

    /// Icons finished loading off the main thread: fill in the tiles that had none.
    fn on_icons_ready(&self) {
        if !self.active.get() {
            return;
        }
        let missing: Vec<usize> = self.icon_missing.borrow().iter().copied().collect();
        for idx in missing {
            let Some(pid) = self.wins.borrow().get(idx).map(|w| w.pid) else { continue };
            let Some(icon) = self.icons.get(pid) else { continue };
            let icon_cg = self.icons.get_cg(pid);
            let icon_reps = self.icons.get_cg_all(pid);
            self.icon_missing.borrow_mut().remove(&idx);
            if let Some(tiles) = self.pending_tiles.borrow_mut().as_mut() {
                if let Some(t) = tiles.get_mut(idx) {
                    t.icon = Some(icon);
                    t.icon_cg = icon_cg;
                    t.icon_reps = icon_reps;
                }
                continue;
            }
            self.switcher.set_icon(idx, icon, icon_cg, icon_reps);
        }
    }

    fn select(&self, idx: usize) {
        self.selected.set(idx);
        if self.switcher.is_visible() {
            self.switcher.set_selected(idx);
        }
    }

    fn step(&self, delta: isize) {
        let n = self.wins.borrow().len() as isize;
        if n == 0 {
            return;
        }
        // Repeated Tab presses reveal the panel right away.
        self.show_now();
        let cur = self.selected.get() as isize;
        self.select((cur + delta).rem_euclid(n) as usize);
    }

    fn vertical(&self, down: bool) {
        self.show_now();
        if self.switcher.is_visible() {
            let idx = if down { self.switcher.index_below() } else { self.switcher.index_above() };
            self.select(idx);
        }
    }

    fn end_session(&self) {
        self.active.set(false);
        self.schedule_timers();
        let ended = self.generation.get();
        self.generation.set(ended + 1);
        // Captures the session queued but did not start are dropped (in-flight ones finish).
        self.thumbs.cancel_generation(ended);
        self.pending_tiles.borrow_mut().take();
        self.icon_missing.borrow_mut().clear();
        // A panel still in its launch warm-up (transparent) is ordered out by `end_warm_up`.
        if self.switcher.is_visible() {
            let t = Instant::now();
            self.switcher.hide();
            // Captures that landed while it was up (each made off the main thread, by
            // ScreenCaptureKit from the window server: a busy or stopped app is captured too).
            let landed: Vec<String> = self.session_landed.borrow_mut().drain(..).map(|(w, ms)| format!("{w}@{ms:.0}")).collect();
            let c = self.thumbs.stats();
            let (layer_sets, holding) = self.switcher.tree_stats();
            let (rendered, cached, cached_kb, pending) = self.switcher.label_stats();
            log(&format!(
                "panel hidden (hide {:.1} ms, slowest draw {:.1} ms) tile_layers={layer_sets} holding_images={holding} \
                 labels: rendered={rendered} cached={cached} ({cached_kb} KB) pending={pending} thumbs_landed=[{}] captures: \
                 started={} ok={} failed={} reclaimed={} in_flight={} queued={} peak_in_flight={} on_main_thread={}",
                ms_since(t),
                self.switcher.take_max_draw_ms(),
                landed.join(","),
                c.started,
                c.ok,
                c.failed,
                c.reclaimed,
                c.in_flight,
                c.queued,
                c.peak_in_flight,
                c.on_main
            ));
            // Keep mode: once the switch is under way, bring the transparent panel to the front of
            // its level again (a window of that level ordered in since would cover the next show).
            after_on_main(Duration::from_millis(50), || {
                if let Some(st) = state()
                    && !st.active.get()
                    && let Some(ms) = st.switcher.refront_if_kept()
                {
                    log(&format!("panel re-fronted (kept ordered in) in {ms:.2} ms"));
                }
            });
        }
    }

    /// `tap_ms`: how long the Commit waited between the event tap and this call (main-thread
    /// latency), for the log; `None` for a mouse click.
    fn commit(&self, tap_ms: Option<f64>) {
        if !self.active.get() {
            return;
        }
        let idx = self.selected.get();
        let w = self.wins.borrow().get(idx).cloned();
        if let Some(w) = &w {
            // The switch goes first: nothing below is needed for it, and hiding the panel is a
            // synchronous WindowServer request. Focusing talks to WindowServer and then to the
            // target app over AX (which can block for the AX timeout), so the Focuser does it on
            // its own threads; a newer commit replaces an older one that has not landed yet.
            self.focuser.focus(w.clone());
            if let Some(m) = self.model() {
                m.note_focused(w.wid);
            }
        }
        self.end_session();
        if let Some(w) = w {
            let tap = tap_ms.map(|ms| format!(" tap_to_main_ms={ms:.1}")).unwrap_or_default();
            log(&format!("commit: wid={} app={} title={:?}{tap}", w.wid, w.app_name, w.title));
        }
    }

    fn cancel(&self) {
        if self.active.get() {
            log("cancel");
        }
        self.end_session();
    }

    /// A tap event, on the main thread. `session`: the tap session it belongs to; `tap_ms`: how
    /// long it waited between the tap and this call.
    fn on_hotkey(&self, ev: HotkeyEvent, session: u64, tap_ms: f64) {
        if matches!(ev, HotkeyEvent::Open { .. } | HotkeyEvent::OpenSameApp { .. }) {
            // The user is here: resume background work now, not at the next activity read.
            self.note_user_active();
        }
        match ev {
            HotkeyEvent::Open { reverse } | HotkeyEvent::OpenSameApp { reverse } => {
                self.tap_session.set(session);
                let same_app = matches!(ev, HotkeyEvent::OpenSameApp { .. });
                if !self.open(reverse, same_app, Some(tap_ms)) {
                    self.close_tap(session);
                }
            }
            HotkeyEvent::Next => self.step(1),
            HotkeyEvent::Prev => self.step(-1),
            HotkeyEvent::Down => self.vertical(true),
            HotkeyEvent::Up => self.vertical(false),
            HotkeyEvent::Commit => self.commit(Some(tap_ms)),
            HotkeyEvent::Cancel => self.cancel(),
        }
    }

    fn on_click(&self, idx: usize) {
        if !self.active.get() {
            return;
        }
        self.selected.set(idx);
        self.commit(None);
        // The tap only closes itself on Option release / Return / Esc.
        self.close_tap(self.tap_session.get());
    }

    fn on_hover(&self, idx: usize) {
        if self.active.get() {
            self.select(idx);
        }
    }

    fn on_model_event(&self, e: ModelEvent) {
        match e {
            ModelEvent::FocusLeft { wid } => {
                // Still on screen: the moment to get a real image before it is covered or
                // minimized.
                self.thumbs.capture_one(wid);
                log(&format!("model: focus left wid={wid} (capturing)"));
            }
            ModelEvent::Focused { wid } => {
                log(&format!("model: focused wid={wid}"));
                after_on_main(FOCUSED_CAPTURE_AFTER, move || {
                    if let Some(st) = state() {
                        st.thumbs.refresh_stale(&[wid], FOCUSED_CAPTURE_MAX_AGE, 1);
                    }
                });
            }
            ModelEvent::Minimized { wid } => log(&format!("model: minimized wid={wid}")),
            ModelEvent::Destroyed { wid } => self.thumbs.forget(wid),
            ModelEvent::AppLaunched { pid } => self.icons.prefetch([pid]),
            ModelEvent::AppTerminated { pid } => self.icons.forget(pid),
        }
    }

    /// Every couple of seconds: update the pause state, and while the switcher is closed and
    /// background work is not paused, refresh stale thumbnails of on-screen windows in the
    /// background and drop cached thumbnails of windows that are gone.
    fn refresh_tick(&self) {
        self.read_activity();
        if self.active.get() || self.paused.get().is_some() {
            return;
        }
        let Some(m) = self.model() else { return };
        let ready = m.ready();
        let snap = m.snapshot();
        if ready {
            // Only a complete list may evict (a partial one would drop live thumbnails).
            let all: Vec<u32> = snap.windows.iter().map(|w| w.wid).collect();
            self.thumbs.retain_only(&all);
        }
        let onscreen: Vec<u32> = snap
            .mru
            .iter()
            .map(|&i| &snap.windows[i])
            .filter(|w| w.on_current_space && !w.minimized && !w.hidden_app)
            .map(|w| w.wid)
            .collect();
        self.thumbs.refresh_stale(&onscreen, REFRESH_MAX_AGE, REFRESH_BUDGET);
    }

    /// Apply the activity read the previous tick started, and start the next read. The read
    /// (WindowServer session dictionary, input idle time) runs on a background queue. Its result
    /// waits in a slot for the next tick, so it costs the main thread no extra wake-up, unless it
    /// changes the pause decision (screen locked or unlocked, the user went idle or came back):
    /// then it is applied at once.
    fn read_activity(&self) {
        self.apply_activity();
        let slot = self.activity_slot.clone();
        DispatchQueue::global_queue(GlobalQueueIdentifier::QualityOfService(DispatchQoS::Utility)).exec_async(move || {
            let a = objc2::rc::autoreleasepool(|_| sysstate::activity_now());
            let decisive = {
                let mut s = slot.lock().unwrap_or_else(|e| e.into_inner());
                s.latest = Some(a);
                s.decisive(&a)
            };
            if decisive {
                DispatchQueue::main().exec_async(|| {
                    if let Some(st) = state() {
                        st.apply_activity();
                    }
                });
            }
        });
    }

    /// Apply the newest background activity read, if any.
    fn apply_activity(&self) {
        let Some(mut a) = self.activity_slot.lock().unwrap_or_else(|e| e.into_inner()).latest.take() else { return };
        // The read may predate a ⌥Tab that `note_user_active` already counted.
        if let Some(t) = self.user_active_at.get() {
            a.idle_s = a.idle_s.min(t.elapsed().as_secs_f64());
        }
        self.set_activity(a);
    }

    fn set_activity(&self, a: Activity) {
        self.activity.set(a);
        {
            let mut s = self.activity_slot.lock().unwrap_or_else(|e| e.into_inner());
            s.applied = a;
            s.idle_limit_s = self.idle_limit_s.get();
        }
        self.update_pause();
    }

    /// The user just used ZTab: count them as present (resumes an idle pause at once). While
    /// paused for another reason, read the session state now (the screen cannot be locked if
    /// ⌥Tab reached ZTab, but a lock read can be up to a tick old).
    fn note_user_active(&self) {
        self.user_active_at.set(Some(Instant::now()));
        let mut a = self.activity.get();
        if a.idle_s > 0.0 {
            a.idle_s = 0.0;
            self.set_activity(a);
        }
        if self.paused.get().is_some() {
            self.read_activity();
        }
    }

    /// Pause or resume background work (thumbnail refresh here, the model's safety-net rescans
    /// and periodic window-list refresh) from the latest activity read and the NSWorkspace state.
    fn update_pause(&self) {
        let reason = sysstate::pause_reason(&self.activity.get(), self.displays_asleep.get(), self.session_active.get(), self.idle_limit_s.get());
        if reason == self.paused.get() {
            return;
        }
        match reason {
            Some(r) => log(&format!("background work paused ({r})")),
            None => log("background work resumed"),
        }
        self.paused.set(reason);
        // The app threads followed notifications all along; only a pause during which the screen
        // was off (locked, displays asleep, session switched out) warrants rescanning every app.
        if reason.is_some_and(|r| r != sysstate::PAUSE_IDLE) {
            self.pause_rescan.set(true);
        }
        let rescan = reason.is_none() && self.pause_rescan.replace(false);
        if let Some(m) = self.model() {
            m.set_paused(reason.is_some(), rescan);
        }
        self.schedule_timers();
    }

    /// Set up the periodic timers for the current state: the 1.5 s watchdog and the 2 s refresh
    /// while ZTab runs, or a single slow tick doing both while background work is paused (and
    /// the tap and the model are up and no session is open). Resuming restores them at once.
    fn schedule_timers(&self) {
        let installed = self.hk_handle.get().is_some() && self.model.borrow().is_some();
        let mode = match self.paused.get() {
            Some(r) if installed && !self.active.get() => {
                if r == sysstate::PAUSE_IDLE {
                    TimerMode::Idle
                } else {
                    TimerMode::ScreenOff
                }
            }
            _ => TimerMode::Normal,
        };
        if self.timer_mode.get() == Some(mode) {
            return;
        }
        for t in self.timers.borrow_mut().drain(..) {
            t.invalidate();
        }
        let make = |every: f64, tolerance: f64, f: fn(&State)| {
            let block = RcBlock::new(move |_t: NonNull<NSTimer>| {
                if let Some(st) = state() {
                    f(&st);
                }
            });
            // SAFETY: a repeating block timer on the current (main) run loop.
            let t = unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(every, true, &block) };
            t.setTolerance(tolerance);
            t
        };
        let timers = match mode {
            TimerMode::Normal => vec![
                make(WATCHDOG_EVERY_S, TIMER_TOLERANCE_S, State::watchdog),
                make(REFRESH_EVERY_S, TIMER_TOLERANCE_S, State::refresh_tick),
            ],
            TimerMode::Idle => vec![make(IDLE_TICK_S, 1.0, State::paused_tick)],
            TimerMode::ScreenOff => vec![make(SCREEN_OFF_TICK_S, 3.0, State::paused_tick)],
        };
        *self.timers.borrow_mut() = timers;
        if self.timer_mode.replace(Some(mode)).is_some() {
            log(&format!("timers: {mode:?}"));
        }
    }

    /// The single tick while background work is paused.
    fn paused_tick(&self) {
        self.watchdog();
        self.refresh_tick();
    }

    /// Displays asleep / awake and session switched out / back (NSWorkspace notifications, main
    /// queue).
    fn install_workspace_observers(&self) {
        let nc = NSWorkspace::sharedWorkspace().notificationCenter();
        let queue = NSOperationQueue::mainQueue();
        // SAFETY: the notification names are valid static NSStrings exported by AppKit.
        let names: [(&NSNotificationName, u8); 4] = unsafe {
            [
                (NSWorkspaceScreensDidSleepNotification, 0),
                (NSWorkspaceScreensDidWakeNotification, 1),
                (NSWorkspaceSessionDidResignActiveNotification, 2),
                (NSWorkspaceSessionDidBecomeActiveNotification, 3),
            ]
        };
        let mut obs = self.ws_observers.borrow_mut();
        for (name, kind) in names {
            let block = RcBlock::new(move |_n: NonNull<NSNotification>| {
                let Some(st) = state() else { return };
                match kind {
                    0 => st.displays_asleep.set(true),
                    1 => st.displays_asleep.set(false),
                    2 => st.session_active.set(false),
                    _ => st.session_active.set(true),
                }
                st.update_pause();
                // The lock state may have changed meanwhile (a display wake shows the lock screen).
                st.read_activity();
            });
            // SAFETY: valid name, nil object, main queue (the block runs on the main thread).
            obs.push(unsafe { nc.addObserverForName_object_queue_usingBlock(Some(name), None, Some(&queue), &block) });
        }
        // Screen lock / unlock (posted by loginwindow): read the session state at once instead of
        // at the next tick, which is 30 s apart while the screen is off.
        let dnc = NSDistributedNotificationCenter::defaultCenter();
        for name in ["com.apple.screenIsLocked", "com.apple.screenIsUnlocked"] {
            let block = RcBlock::new(move |_n: NonNull<NSNotification>| {
                if let Some(st) = state() {
                    log(&format!("{name}"));
                    st.read_activity();
                }
            });
            let name = NSString::from_str(name);
            // SAFETY: valid name, nil object, main queue (the block runs on the main thread).
            obs.push(unsafe { dnc.addObserverForName_object_queue_usingBlock(Some(&name), None, Some(&queue), &block) });
        }
    }

    /// Install the event tap on its own thread (once Accessibility is granted). Its handler only
    /// queues each event to the main queue (FIFO), so a busy main thread delays ZTab's reaction
    /// but never the keyboard: the tap decides at once whether to swallow a key.
    fn try_install_hotkey(&self) {
        if self.hk_handle.get().is_some() || self.hk_installing.get() || !windows::ax_trusted() {
            return;
        }
        self.hk_installing.set(true);
        if let Err(e) = spawn_tap(self.settings.borrow().hotkeys) {
            self.hotkey_installed(Err(e), None);
        }
    }

    /// `live`: when the tap thread had the tap installed (for the launch timeline).
    fn hotkey_installed(&self, res: Result<HotkeyHandle, String>, live: Option<Instant>) {
        self.hk_installing.set(false);
        match res {
            Ok(h) => {
                // The shortcuts may have changed while the tap was being installed.
                h.set_bindings(self.settings.borrow().hotkeys);
                h.set_recording(self.settings_window().is_some_and(|w| w.is_recording()));
                let when = live
                    .map(|t| format!(" (tap live {} ms after process start, here {:.1} ms later)", since_launch(unix_of(t)), ms_since(t)))
                    .unwrap_or_default();
                log(&format!(
                    "hotkey installed ({:?}) on the ztab-tap thread: {} / {}{when}",
                    h.location(),
                    h.bindings().all.label(),
                    h.bindings().same_app.label()
                ));
                self.hk_handle.set(Some(h));
                self.refresh_menu();
                self.schedule_timers();
                self.check_usable();
            }
            Err(e) => {
                if !self.install_failed_logged.replace(true) {
                    log(&format!("hotkey install failed: {e}"));
                }
            }
        }
    }

    /// Periodic housekeeping: install the tap and start the model once Accessibility is
    /// granted, re-enable the tap if macOS disabled it, recover from a missed Option release,
    /// and follow secure input.
    fn watchdog(&self) {
        self.try_install_hotkey();
        self.ensure_model(false);
        if let Some(h) = self.hk_handle.get() {
            h.reenable();
            // The tap closed the session without an event for it (no known path does): end it,
            // but only once every event the tap emitted has been handled here, so a Commit still
            // queued behind a busy main thread is never pre-empted.
            if self.active.get() && !h.is_open() && self.hk_handled.get() == h.emitted() {
                log("watchdog: the tap is closed but a session is active; ending it");
                self.end_session();
            }
        }
        self.check_recording();
        self.check_secure_input();
    }

    /// Secure input (a password field, Terminal's Secure Keyboard Entry) hides every key from
    /// event taps, so ⌥Tab cannot work: say so in the menu and on the status icon.
    fn check_secure_input(&self) {
        let now = sysstate::secure_input();
        let prev = self.secure.borrow().clone();
        if now == prev {
            return;
        }
        match (&prev, &now) {
            (None, Some(s)) => log(&format!("secure input on (frontmost: {:?} pid {:?}); Option+Tab is paused by macOS", s.app_name, s.pid)),
            (Some(_), None) => log("secure input off"),
            (Some(a), Some(b)) if a.app_name != b.app_name && b.app_name.is_some() => {
                log(&format!("secure input still on (frontmost: {:?} pid {:?})", b.app_name, b.pid))
            }
            _ => {}
        }
        *self.secure.borrow_mut() = now;
        self.refresh_secure_ui();
    }

    fn refresh_secure_ui(&self) {
        let menu = self.menu.borrow();
        let Some(m) = menu.as_ref() else { return };
        let secure = self.secure.borrow();
        match secure.as_ref() {
            Some(s) => {
                let who = s.app_name.as_deref().map(|n| format!(" (frontmost: {n})")).unwrap_or_default();
                m.secure.setTitle(&NSString::from_str(&format!("⚠ Secure input is on{who} — ⌥Tab is paused by macOS")));
                m.secure.setHidden(false);
                m.secure_sep.setHidden(false);
            }
            None => {
                m.secure.setHidden(true);
                m.secure_sep.setHidden(true);
            }
        }
        if let Some(button) = m.item.button(self.mtm) {
            let img = if secure.is_some() { m.img_secure.as_ref().or(m.img_normal.as_ref()) } else { m.img_normal.as_ref() };
            if let Some(img) = img {
                button.setImage(Some(img));
            }
            button.setToolTip(Some(&NSString::from_str(if secure.is_some() {
                "ZTab — secure input is on: macOS is not delivering ⌥Tab"
            } else {
                "ZTab — Option+Tab window switcher"
            })));
        }
    }

    fn refresh_menu(&self) {
        let Some((ax_item, sc_item)) = self.menu.borrow().as_ref().map(|m| (m.ax.clone(), m.screen.clone())) else {
            return;
        };
        let ax = windows::ax_trusted();
        // Cached after the first positive answer (the raw preflight costs ~5 ms).
        let sc = thumbs::screen_capture_allowed();
        let hk = self.hk_handle.get().is_some();
        ax_item.setTitle(&NSString::from_str(if ax && hk {
            "Accessibility: granted"
        } else if ax {
            "Accessibility: granted (restart ZTab if Option+Tab does not work)"
        } else {
            "Grant Accessibility access…"
        }));
        ax_item.setEnabled(!ax);
        sc_item.setTitle(&NSString::from_str(if sc {
            "Screen Recording: granted"
        } else {
            "Grant Screen Recording (for thumbnails)…"
        }));
        sc_item.setEnabled(!sc);
        self.check_secure_input();
    }
}

// ---------------------------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------------------------

impl State {
    /// Make `s` the settings in effect: the shortcuts go to the tap (one atomic store), the show
    /// delay applies from the next Open, and the file is written on the settings thread.
    fn apply_settings(&self, s: Settings, why: &str) {
        let before = *self.settings.borrow();
        *self.settings.borrow_mut() = s;
        if before.hotkeys != s.hotkeys
            && let Some(h) = self.hk_handle.get()
        {
            h.set_bindings(s.hotkeys);
        }
        if before != s {
            log(&format!("settings: {why}: {}", s.describe()));
            self.writer.save(&self.paths, s, |p, _, r| match r {
                Ok(()) => log(&format!("settings: saved {}", p.settings_file.display())),
                Err(e) => log(&format!("settings: could not save {}: {e}", p.settings_file.display())),
            });
        }
    }

    fn settings_window(&self) -> Option<Rc<SettingsWindow>> {
        self.settings_win.borrow().clone()
    }

    /// The Settings window, built (hidden, without a WindowServer window yet) if it does not
    /// exist. Launch builds it during the warm-up, before the stall detector starts, so the first
    /// "Settings…" does not pay the ~35 ms of creating its controls.
    fn build_settings_window(&self, when: &str) -> Rc<SettingsWindow> {
        if let Some(w) = self.settings_window() {
            return w;
        }
        let t = Instant::now();
        let w = Rc::new(SettingsWindow::new(
            self.mtm,
            Rc::new(|e| {
                if let Some(st) = state() {
                    st.on_settings_event(e);
                }
            }),
        ));
        log(&format!("settings window built {when} in {:.1} ms", ms_since(t)));
        *self.settings_win.borrow_mut() = Some(w.clone());
        w
    }

    /// Menu "Settings…": build the window the first time, show the current values, bring it to
    /// the front (activate, then make it key).
    fn show_settings(&self) {
        let win = self.build_settings_window("on first open");
        win.set_values(&self.settings.borrow());
        win.set_message("", MsgKind::Info);
        win.set_login(self.login_state.borrow().as_ref().map(|s| s.checked()));
        self.refresh_login_state();
        let t = Instant::now();
        win.show(self.mtm);
        self.icons.prefetch([std::process::id() as i32]);
        self.update_own_window();
        log(&format!("settings window shown ({:.1} ms) wid={}", ms_since(t), win.window().windowNumber()));
        // An accessory app may be refused activation; then front the window like a switch does
        // (WindowServer request, then makeKeyAndOrderFront on this thread).
        after_on_main(Duration::from_millis(300), || {
            let Some(st) = state() else { return };
            let Some(win) = st.settings_window() else { return };
            let active = NSApplication::sharedApplication(st.mtm).isActive();
            let key = win.window().isKeyWindow();
            log(&format!("settings window: app active={active} key={key}"));
            if win.window().isVisible() && !(active && key)
                && let Some(w) = st.own_win_info()
            {
                log("settings window: not frontmost after activate; fronting it through the focuser");
                st.focuser.focus(w);
            }
        });
    }

    /// The Settings window as a switcher entry, when it is open.
    fn own_win_info(&self) -> Option<WinInfo> {
        let win = self.settings_window()?;
        let w = win.window();
        if !win.is_visible() || w.windowNumber() <= 0 {
            return None;
        }
        let f = w.frame();
        let primary_h = objc2_app_kit::NSScreen::screens(self.mtm).firstObject().map(|s| s.frame().size.height).unwrap_or(0.0);
        Some(WinInfo {
            wid: w.windowNumber() as u32,
            pid: std::process::id() as i32,
            app_name: "ZTab".into(),
            bundle_id: Some(settings::AGENT_LABEL.into()),
            title: settings_window::WINDOW_TITLE.into(),
            bounds: (f.origin.x, primary_h - f.origin.y - f.size.height, f.size.width, f.size.height),
            minimized: w.isMiniaturized(),
            on_current_space: !w.isMiniaturized(),
            hidden_app: false,
            z_index: 0,
            ax: None,
        })
    }

    /// Tell the model about the Settings window (open / moved / minimized / closed).
    fn update_own_window(&self) {
        let Some(m) = self.model() else { return };
        m.set_own_window(self.own_win_info().map(|w| OwnWindow {
            wid: w.wid,
            app_name: w.app_name,
            title: w.title,
            bounds: w.bounds,
            minimized: w.minimized,
        }));
    }

    /// Read the login agent's state on the settings thread, then show it: the box is ticked only
    /// when the agent starts this binary; an agent that starts another copy (the app was moved, a
    /// dev build) or a missing program is shown unticked, with a warning, and ticking the box
    /// points it at this binary.
    fn refresh_login_state(&self) {
        let paths = self.paths.clone();
        let program = settings::current_program();
        self.writer.run(move || {
            let ls = paths.login_state(&program);
            DispatchQueue::main().exec_async(move || {
                let Some(st) = state() else { return };
                if let Some(w) = st.settings_window() {
                    w.set_login(Some(ls.checked()));
                    if let Some(msg) = login_warning(&ls) {
                        w.set_message(&msg, MsgKind::Warning);
                    }
                }
                if st.login_state.borrow().as_ref() != Some(&ls) && matches!(ls, settings::LoginState::Elsewhere { .. }) {
                    log(&format!("settings: start at login is {}", ls.describe()));
                }
                *st.login_state.borrow_mut() = Some(ls);
            });
        });
    }

    /// The checkbox: write or delete the agent file on the settings thread, then show the state
    /// read back from the file.
    fn set_login(&self, on: bool) {
        if let Some(w) = self.settings_window() {
            w.set_login(None);
        }
        let paths = self.paths.clone();
        let program = settings::current_program();
        self.writer.run(move || {
            let r = paths.set_login_file(on, &program).map_err(|e| e.to_string());
            let now = paths.login_state(&program);
            log(&format!(
                "settings: start at login {} -> {} ({}{}){}",
                if on { "on" } else { "off" },
                now.describe(),
                paths.agent_plist.display(),
                if paths.isolated { ", isolated" } else { "" },
                r.as_ref().err().map(|e| format!(" error: {e}")).unwrap_or_default()
            ));
            DispatchQueue::main().exec_async(move || {
                let Some(st) = state() else { return };
                if let Some(w) = st.settings_window() {
                    w.set_login(Some(now.checked()));
                    match (&r, login_warning(&now)) {
                        (Err(e), _) => w.set_message(&format!("Could not change the login item: {e}"), MsgKind::Error),
                        (Ok(()), Some(msg)) => w.set_message(&msg, MsgKind::Warning),
                        (Ok(()), None) if now.checked() => w.set_message("ZTab will start when you log in.", MsgKind::Info),
                        (Ok(()), None) => w.set_message("ZTab will not start at login.", MsgKind::Info),
                    }
                }
                *st.login_state.borrow_mut() = Some(now);
            });
        });
    }

    /// The tap lets every key through while a recorder captures (and only then).
    fn set_tap_recording(&self, on: bool) {
        if let Some(h) = self.hk_handle.get()
            && h.is_recording() != on
        {
            h.set_recording(on);
            log(&format!("settings: shortcut recorder {}", if on { "capturing (tap passes every key)" } else { "stopped (tap active)" }));
        }
    }

    fn on_recorded(&self, which: Which, code: u16, mods: Mods) {
        let Some(win) = self.settings_window() else { return };
        let cur = *self.settings.borrow();
        let other = match which {
            Which::All => cur.hotkeys.same_app,
            Which::SameApp => cur.hotkeys.all,
        };
        let checked = Binding::from_recorded(code, mods).and_then(|b| hotkey::validate(which, b, other).map(|w| (b, w)));
        match checked {
            Err(msg) => {
                // Keep capturing: the user tries another combination (or presses Esc).
                log(&format!("settings: recorder refused keycode {code:#04x} {mods:?}: {msg}"));
                win.set_message(&msg, MsgKind::Error);
            }
            Ok((b, warning)) => {
                log(&format!("settings: recorder captured {} for {}", b.label(), which.describe()));
                let mut s = cur;
                match which {
                    Which::All => s.hotkeys.all = b,
                    Which::SameApp => s.hotkeys.same_app = b,
                }
                // The new shortcut is in the tap before the recorder stops and the tap resumes.
                self.apply_settings(s, &format!("recorded {} for {}", b.label(), which.describe()));
                win.set_values(&s);
                win.stop_recording();
                match warning {
                    Some(w) => win.set_message(&w, MsgKind::Warning),
                    None => win.set_message(&format!("{} is now the shortcut for {}.", b.label(), which.describe()), MsgKind::Info),
                }
            }
        }
    }

    fn on_settings_event(&self, e: SettingsEvent) {
        let Some(win) = self.settings_window() else { return };
        match e {
            SettingsEvent::Delay(ms) => {
                win.set_delay_label(ms);
                let s = Settings { show_delay_ms: ms, ..*self.settings.borrow() };
                self.apply_settings(s, "show delay");
            }
            SettingsEvent::Login(on) => self.set_login(on),
            SettingsEvent::Recorded(which, code, mods) => self.on_recorded(which, code, mods),
            SettingsEvent::Recording(_, _) => {
                win.set_message(if win.is_recording() { "Press the new shortcut (Esc cancels)." } else { "" }, MsgKind::Info);
                self.set_tap_recording(win.is_recording());
            }
            SettingsEvent::Restore => {
                win.stop_recording();
                let s = Settings::default();
                self.apply_settings(s, "restore defaults");
                win.set_values(&s);
                win.set_message(
                    &format!("Defaults restored: {} ms, {} and {}.", s.show_delay_ms, s.hotkeys.all.label(), s.hotkeys.same_app.label()),
                    MsgKind::Info,
                );
            }
            SettingsEvent::Key(key) => {
                if !key {
                    win.stop_recording();
                }
                if let Some(m) = self.model() {
                    m.note_own_key(key);
                }
            }
            SettingsEvent::Miniaturized(min) => {
                self.update_own_window();
                if min {
                    // Like closing it: ZTab would stay active with no window to type into. (The
                    // window has no minimize button; this covers a minimize from elsewhere.)
                    self.focus_away_from_settings("minimized");
                }
            }
            SettingsEvent::Moved => self.update_own_window(),
            SettingsEvent::Closing => {
                win.stop_recording();
                let wid = win.window().windowNumber();
                if let Some(m) = self.model() {
                    m.set_own_window(None);
                }
                if wid > 0 {
                    self.thumbs.forget(wid as u32);
                }
                self.focus_away_from_settings("closed");
            }
        }
    }

    /// The Settings window is closing or was minimized (`why`) while ZTab is the active app: an
    /// accessory app with no other window would keep the keyboard focus, so switch to the most
    /// recently used window (as a quick ⌥Tab would).
    fn focus_away_from_settings(&self, why: &str) {
        if !NSApplication::sharedApplication(self.mtm).isActive() {
            return;
        }
        let Some(m) = self.model() else { return };
        let snap = m.snapshot();
        let own = std::process::id() as i32;
        let target = snap.mru.iter().map(|&i| &snap.windows[i]).find(|w| w.pid != own && !w.minimized && !w.hidden_app && w.on_current_space);
        match target {
            Some(w) => {
                log(&format!("settings window {why}: focusing the most recent window wid={} app={}", w.wid, w.app_name));
                self.focuser.focus(w.clone());
                m.note_focused(w.wid);
            }
            None => log(&format!("settings window {why}: no window to focus")),
        }
    }

    /// Watchdog: a recorder must never leave the tap passing keys once it is not capturing in
    /// the key window any more.
    fn check_recording(&self) {
        let Some(h) = self.hk_handle.get() else { return };
        if !h.is_recording() {
            return;
        }
        let win = self.settings_window();
        let capturing = win.as_ref().is_some_and(|w| w.is_recording() && w.window().isKeyWindow());
        if !capturing {
            if let Some(w) = win {
                w.stop_recording();
            }
            h.set_recording(false);
            log("watchdog: no recorder is capturing in the key window; the tap is active again");
        }
    }
}

/// The Settings window's warning for a login agent that would not start this ZTab.
fn login_warning(state: &settings::LoginState) -> Option<String> {
    match state {
        settings::LoginState::Elsewhere { program, exists } => Some(format!(
            "The login item starts {}{}, not this ZTab. Tick the box to start this one at login.",
            if program.is_empty() { "an unknown program" } else { program },
            if *exists { "" } else { " (missing)" }
        )),
        _ => None,
    }
}

/// A switch to the Settings window was fronted (raise thread): order it in on the main thread.
fn raise_own_window(wid: u32) {
    DispatchQueue::main().exec_async(move || {
        let Some(st) = state() else { return };
        let Some(win) = st.settings_window() else { return };
        let w = win.window();
        if w.windowNumber() <= 0 || w.windowNumber() as u32 != wid {
            return;
        }
        if w.isMiniaturized() {
            w.deminiaturize(None);
        }
        NSApplication::sharedApplication(st.mtm).activate();
        w.makeKeyAndOrderFront(None);
    });
}

/// A tap event, queued to the main thread by the tap's handler.
/// Start the event tap on its own `ztab-tap` thread with these shortcuts. Needs nothing else of
/// ZTab (main() calls it before AppKit starts up): every event is queued to the main queue, and
/// so is the result (`State::hotkey_installed`), which the main thread handles once it runs.
fn spawn_tap(bindings: hotkey::Bindings) -> Result<(), String> {
    let handler: hotkey::Handler = Box::new(|ev: HotkeyEvent, session: u64| {
        let at = Instant::now();
        DispatchQueue::main().exec_async(move || hotkey_on_main(ev, session, at));
    });
    Hotkey::spawn(handler, bindings, |res| {
        // On the tap thread: the tap is live from here on (keys are queued to the main queue
        // until the main thread gets to them).
        let live = Instant::now();
        DispatchQueue::main().exec_async(move || {
            if let Some(st) = state() {
                st.hotkey_installed(res, Some(live));
            }
        });
    })
}

fn hotkey_on_main(ev: HotkeyEvent, session: u64, at: Instant) {
    let Some(st) = state() else { return };
    st.hk_handled.set(st.hk_handled.get() + 1);
    // A panic must never unwind into the dispatch queue (that aborts the process).
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| st.on_hotkey(ev, session, ms_since(at))));
    if r.is_err() {
        log(&format!("panic while handling {ev:?}; session closed"));
        st.close_tap(session);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| st.end_session()));
    }
}

fn icons_ready() {
    if let Some(st) = state() {
        st.on_icons_ready();
    }
}

// ---------------------------------------------------------------------------------------------
// Status bar menu
// ---------------------------------------------------------------------------------------------

struct MenuParts {
    item: Retained<NSStatusItem>,
    menu: Retained<NSMenu>,
    _target: Retained<MenuTarget>,
    ax: Retained<NSMenuItem>,
    screen: Retained<NSMenuItem>,
    secure: Retained<NSMenuItem>,
    secure_sep: Retained<NSMenuItem>,
    img_normal: Option<Retained<NSImage>>,
    img_secure: Option<Retained<NSImage>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ZTabMenuTarget"]
    #[ivars = ()]
    struct MenuTarget;

    unsafe impl NSObjectProtocol for MenuTarget {}

    unsafe impl NSMenuDelegate for MenuTarget {
        #[unsafe(method(menuWillOpen:))]
        fn menu_will_open(&self, _menu: &NSMenu) {
            if let Some(st) = state() {
                let t = Instant::now();
                st.refresh_menu();
                log(&format!("status menu opened (refresh {:.1} ms)", ms_since(t)));
            }
        }
    }

    impl MenuTarget {
        #[unsafe(method(openAccessibility:))]
        fn open_accessibility(&self, _sender: Option<&AnyObject>) {
            prompt_accessibility();
            open_settings("Privacy_Accessibility");
        }

        #[unsafe(method(openScreenRecording:))]
        fn open_screen_recording(&self, _sender: Option<&AnyObject>) {
            unsafe { CGRequestScreenCaptureAccess() };
            open_settings("Privacy_ScreenCapture");
        }

        #[unsafe(method(openSettings:))]
        fn open_settings_window(&self, _sender: Option<&AnyObject>) {
            if let Some(st) = state() {
                st.show_settings();
            }
        }

        #[unsafe(method(quit:))]
        fn quit(&self, _sender: Option<&AnyObject>) {
            log("quit from menu");
            log_flush(Duration::from_millis(300));
            let mtm = MainThreadMarker::from(self);
            NSApplication::sharedApplication(mtm).terminate(None);
        }
    }
);

impl MenuTarget {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

fn open_settings(anchor: &str) {
    let url = format!("x-apple.systempreferences:com.apple.preference.security?{anchor}");
    // Spawning a process takes a few ms: keep it off the main thread.
    let _ = std::thread::Builder::new().name("ztab-open-settings".into()).spawn(move || {
        let _ = std::process::Command::new("open").arg(url).status();
    });
}

fn prompt_accessibility() {
    use objc2_core_foundation::{CFBoolean, CFDictionary, CFString};
    let key = CFString::from_static_str("AXTrustedCheckOptionPrompt");
    let dict = CFDictionary::from_slices(&[&*key], &[&*CFBoolean::new(true)]);
    unsafe { objc2_application_services::AXIsProcessTrustedWithOptions(Some(dict.as_opaque())) };
}

fn menu_item(mtm: MainThreadMarker, title: &str, action: Option<Sel>, key: &str, target: &MenuTarget) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            action,
            &NSString::from_str(key),
        )
    };
    if action.is_some() {
        unsafe { item.setTarget(Some(target)) };
    }
    item
}

fn symbol(name: &str) -> Option<Retained<NSImage>> {
    let img = NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str(name), Some(&NSString::from_str("ZTab")));
    match &img {
        Some(i) => i.setTemplate(true),
        None => log(&format!("SF Symbol {name:?} not found")),
    }
    img
}

fn build_status_item(mtm: MainThreadMarker) -> MenuParts {
    let target = MenuTarget::new(mtm);
    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    let img_normal = symbol(SYMBOL_NORMAL);
    let img_secure = symbol(SYMBOL_SECURE);
    if let Some(button) = item.button(mtm) {
        if let Some(img) = &img_normal {
            button.setImage(Some(img));
        } else {
            button.setTitle(&NSString::from_str("⌥⇥"));
        }
        button.setToolTip(Some(&NSString::from_str("ZTab — Option+Tab window switcher")));
    }

    let menu = NSMenu::new(mtm);
    menu.setAutoenablesItems(false);
    let secure = menu_item(mtm, "⚠ Secure input is on — ⌥Tab is paused by macOS", None, "", &target);
    secure.setEnabled(false);
    secure.setHidden(true);
    menu.addItem(&secure);
    let secure_sep = NSMenuItem::separatorItem(mtm);
    secure_sep.setHidden(true);
    menu.addItem(&secure_sep);
    let header = menu_item(mtm, "ZTab — hold ⌥ and press Tab", None, "", &target);
    header.setEnabled(false);
    menu.addItem(&header);
    let hint = menu_item(mtm, "⌥` cycles windows of the current app", None, "", &target);
    hint.setEnabled(false);
    menu.addItem(&hint);
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    let ax = menu_item(mtm, "Accessibility", Some(sel!(openAccessibility:)), "", &target);
    menu.addItem(&ax);
    let screen = menu_item(mtm, "Screen Recording", Some(sel!(openScreenRecording:)), "", &target);
    menu.addItem(&screen);
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    let settings_item = menu_item(mtm, "Settings…", Some(sel!(openSettings:)), ",", &target);
    menu.addItem(&settings_item);
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    let quit = menu_item(mtm, "Quit ZTab", Some(sel!(quit:)), "q", &target);
    menu.addItem(&quit);
    menu.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(&*target)));
    item.setMenu(Some(&menu));

    MenuParts { item, menu, _target: target, ax, screen, secure, secure_sep, img_normal, img_secure }
}

/// Lay the status menu out once, with every item visible, so the text measuring and font loading
/// of its first opening happen at launch and not while the user clicks the icon.
fn warm_up_menu(parts: &MenuParts) {
    let t = Instant::now();
    let was_hidden = (parts.secure.isHidden(), parts.secure_sep.isHidden());
    parts.secure.setHidden(false);
    parts.secure_sep.setHidden(false);
    parts.menu.update();
    let size = parts.menu.size();
    parts.secure.setHidden(was_hidden.0);
    parts.secure_sep.setHidden(was_hidden.1);
    log(&format!("status menu warm-up: {:.1} ms ({:.0}x{:.0})", ms_since(t), size.width, size.height));
}

// ---------------------------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------------------------

fn make_state(mtm: MainThreadMarker, paths: Paths, settings: Settings) -> Rc<State> {
    let switcher = Switcher::new(
        mtm,
        Box::new(|idx| {
            if let Some(st) = state() {
                st.on_click(idx);
            }
        }),
        Box::new(|idx| {
            if let Some(st) = state() {
                st.on_hover(idx);
            }
        }),
    );
    // The panel is a tree of Core Animation layers (nothing rasterised on this thread but small
    // label bitmaps), in sRGB like every bitmap it shows (no colour conversion in the commit; see
    // `thumbs::srgb`), and stays ordered in, transparent, between sessions (no window ordering
    // when ⌥Tab opens it). See ui.rs. `ZTAB_PANEL_MODE` (a `--panel-bench` variant name such as
    // `draw` or `tree+rgba8`) overrides it, for measurements.
    let mode = std::env::var("ZTAB_PANEL_MODE").ok().and_then(|s| BenchVariant::parse(&s)).unwrap_or(PANEL_MODE);
    switcher.set_layer_tree(mode.layers);
    switcher.set_keep_ordered_in(mode.keep);
    switcher.set_backing_8bit(mode.rgba8);
    let thumbs = Thumbnailer::new();
    thumbs.set_thumb_size(THUMB_MAX_W, THUMB_MAX_H);
    thumbs.set_on_update(|wid| {
        if let Some(st) = state() {
            st.on_thumb_update(wid);
        }
    });
    let st = Rc::new(State {
        mtm,
        switcher,
        thumbs,
        icons: IconPrefetcher::new(mtm, Some(icons_ready)),
        model: RefCell::new(None),
        model_ready_logged: Cell::new(false),
        usable_logged: Cell::new(false),
        wins: RefCell::new(Vec::new()),
        pending_tiles: RefCell::new(None),
        icon_missing: RefCell::new(HashSet::new()),
        selected: Cell::new(0),
        generation: Cell::new(0),
        active: Cell::new(false),
        opened_at: Cell::new(None),
        open_tap_ms: Cell::new(None),
        session_landed: RefCell::new(Vec::new()),
        hk_handle: Cell::new(None),
        hk_installing: Cell::new(false),
        hk_handled: Cell::new(0),
        tap_session: Cell::new(0),
        menu: RefCell::new(None),
        install_failed_logged: Cell::new(false),
        secure: RefCell::new(None),
        stall: RefCell::new(Vec::new()),
        focuser: Focuser::new(|w, stage, ms| match stage {
            FocusStage::Fronted { ok } => log(&format!("focus: wid={} fronted ok={ok} after {ms:.1} ms", w.wid)),
            FocusStage::Superseded => log(&format!("focus: wid={} skipped: a newer switch replaced it", w.wid)),
            FocusStage::Raised(what) => log(&format!("focus: wid={} {what} after {ms:.1} ms", w.wid)),
            FocusStage::Late => log(&format!("focus: wid={} AX write returned after a newer switch started; checking the newest one", w.wid)),
            FocusStage::Refront { stale_wid, refronted: true } => {
                log(&format!("focus: wid={} re-fronted: wid={stale_wid} came out on top after its late AX write ({ms:.1} ms after the switch)", w.wid))
            }
            FocusStage::Refront { stale_wid, refronted: false } => {
                log(&format!("focus: wid={} kept: the late AX write of wid={stale_wid} did not put it on top", w.wid))
            }
        }),
        paused: Cell::new(None),
        pause_rescan: Cell::new(false),
        activity: Cell::new(Activity::default()),
        activity_slot: Arc::new(Mutex::new(ActivitySlot::default())),
        timers: RefCell::new(Vec::new()),
        timer_mode: Cell::new(None),
        displays_asleep: Cell::new(false),
        session_active: Cell::new(true),
        idle_limit_s: Cell::new(sysstate::IDLE_PAUSE_S),
        user_active_at: Cell::new(None),
        ws_observers: RefCell::new(Vec::new()),
        mru_order: RefCell::new(Vec::new()),
        settings: RefCell::new(settings),
        paths,
        writer: Writer::new(log),
        settings_win: RefCell::new(None),
        login_state: RefCell::new(None),
    });
    STATE.with(|s| {
        let _ = s.set(st.clone());
    });
    st
}

/// `ztab --dump-windows <file>`: write the one-shot window list (verification aid; blocking).
fn dump_windows(path: &str) {
    let list = windows::list_windows();
    let mut out = format!(
        "ax_trusted={} screen_recording={} count={}\n",
        windows::ax_trusted(),
        windows::screen_recording_granted(),
        list.len()
    );
    for w in &list {
        out += &format!(
            "wid={} pid={} app={:?} bundle={:?} title={:?} z={} min={} space={} hidden={} bounds={:?}\n",
            w.wid, w.pid, w.app_name, w.bundle_id, w.title, w.z_index, w.minimized, w.on_current_space, w.hidden_app, w.bounds
        );
    }
    let _ = std::fs::write(path, out);
}

/// Run `f` on the main queue once the model is ready (or after `timeout`).
fn when_model_ready(started: Instant, timeout: Duration, f: fn(&Rc<State>, bool)) {
    let Some(st) = state() else { return };
    let ready = st.model().is_some_and(|m| m.ready());
    if ready || started.elapsed() > timeout {
        f(&st, ready);
        return;
    }
    after_on_main(Duration::from_millis(25), move || when_model_ready(started, timeout, f));
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

static DIAG_PATH: OnceLock<String> = OnceLock::new();

fn exit_now(code: i32) -> ! {
    log_flush(Duration::from_millis(500));
    std::process::exit(code);
}

/// `ztab --dump-model <file>`: start the window model, wait until it is ready, write its
/// snapshot (verification aid).
fn dump_model(st: &Rc<State>, ready: bool) {
    let m = st.model().expect("model started");
    let s = m.snapshot();
    let mut out = format!(
        "ready={ready} ax_trusted={} count={} current={:?} active_pid={:?}\nmru={:?}\nstats: {}\n",
        windows::ax_trusted(),
        s.windows.len(),
        s.current,
        s.active_pid,
        s.mru,
        m.stats()
    );
    for (i, w) in s.windows.iter().enumerate() {
        out += &format!(
            "[{i}] wid={} pid={} app={:?} title={:?} z={} min={} space={} hidden={} ax={} bounds={:?}\n",
            w.wid, w.pid, w.app_name, w.title, w.z_index, w.minimized, w.on_current_space, w.hidden_app, w.has_ax(), w.bounds
        );
    }
    let _ = std::fs::write(DIAG_PATH.get().map(String::as_str).unwrap_or("/dev/stdout"), out);
    exit_now(0);
}

/// `ztab --snapshot <png>`: open the switcher with the real windows, wait for thumbnails,
/// render it to a PNG, exit (verification aid).
fn snapshot_png(st: &Rc<State>, _ready: bool) {
    let path = DIAG_PATH.get().cloned().unwrap_or_else(|| "ztab-snapshot.png".into());
    if !st.open(false, false, None) {
        let _ = std::fs::write(&path, b"");
        exit_now(2);
    }
    st.show_now();
    after_on_main(Duration::from_millis(1500), move || {
        let st = state().unwrap();
        let ok = st.switcher.snapshot_png(&path);
        log(&format!("snapshot {path}: {ok}"));
        exit_now(if ok { 0 } else { 1 });
    });
}

/// One measured show of `--panel-bench`.
struct BenchShow {
    variant: usize,
    screen: usize,
    first_on_screen: bool,
    gap_before_ms: u64,
    show_ms: f64,
    busy_ms: Option<f64>,
    longest_ms: Option<f64>,
}

/// How the real switcher panel is shown (see `make_state`).
const PANEL_MODE: BenchVariant = BenchVariant { layers: true, keep: true, per_display: false, rgba8: true, nofill: false };

/// A way of showing the panel that `--panel-bench` compares.
#[derive(Clone, Copy, Debug)]
struct BenchVariant {
    layers: bool,
    keep: bool,
    per_display: bool,
    rgba8: bool,
    nofill: bool,
}

impl BenchVariant {
    fn parse(s: &str) -> Option<BenchVariant> {
        let mut v = BenchVariant { layers: false, keep: false, per_display: false, rgba8: false, nofill: false };
        for part in s.split('+') {
            match part {
                "v0" | "draw" => {}
                "tree" | "layers" => v.layers = true,
                "keep" => v.keep = true,
                "perdisplay" => v.per_display = true,
                "rgba8" => v.rgba8 = true,
                "nofill" => v.nofill = true,
                _ => return None,
            }
        }
        Some(v)
    }
    fn name(&self) -> String {
        let mut parts = vec![if self.layers { "tree" } else { "draw" }];
        for (on, n) in [(self.keep, "keep"), (self.per_display, "perdisplay"), (self.rgba8, "rgba8"), (self.nofill, "nofill")] {
            if on {
                parts.push(n);
            }
        }
        parts.join("+")
    }
}

/// `ztab --label-check <out>`: diagnostics (posts no input; the panel stays transparent, as in
/// the launch warm-up). Renders two tile labels with this process in the light appearance, then
/// switches this process (only) to the dark one and renders them again, and writes the text
/// colour of each pass: tile labels must follow the panel's appearance (light text when dark).
fn label_check(phase: u32, mut lines: Vec<String>) {
    let Some(st) = state() else { return };
    let app = NSApplication::sharedApplication(st.mtm);
    let name = |dark: bool| {
        // SAFETY: AppKit-provided appearance name statics.
        unsafe { if dark { objc2_app_kit::NSAppearanceNameDarkAqua } else { objc2_app_kit::NSAppearanceNameAqua } }
    };
    let own = std::process::id() as i32;
    let reps = st.icons.get_cg_all(own);
    let tiles = || -> Vec<Tile> {
        (0..2)
            .map(|i| Tile {
                app_name: "Check".into(),
                title: format!("Label colour check {i} WWWW, a long title that must be truncated at the end"),
                aspect: 1.6,
                icon_cg: reps.first().cloned(),
                icon_reps: reps.clone(),
                ..Default::default()
            })
            .collect()
    };
    let next = move |phase: u32, lines: Vec<String>, ms: u64| {
        let lines = Mutex::new(lines);
        after_on_main(Duration::from_millis(ms), move || label_check(phase, lines.into_inner().unwrap_or_default()));
    };
    match phase {
        // This process's icon first (for the label's small icon), at most 1 s.
        p @ 50..=70 => {
            if st.icons.get_cg_all(own).is_empty() && p < 70 {
                next(p + 1, lines, 50);
            } else {
                next(0, lines, 0);
            }
        }
        // Light, then dark: set the app's appearance and show (transparently) once it applied.
        0 | 2 => {
            let dark = phase == 2;
            st.switcher.end_warm_up();
            app.setAppearance(objc2_app_kit::NSAppearance::appearanceNamed(name(dark)).as_deref());
            next(phase + 1, lines, 300);
        }
        1 | 3 => {
            st.switcher.warm_up(tiles());
            next(if phase == 1 { 100 } else { 200 }, lines, 0);
        }
        // Wait (up to 2 s) for the label to land, then read its text colour.
        p if p >= 100 => {
            let base = if p >= 200 { 3 } else { 1 };
            let tries = p % 100;
            let (img, dark_pal) = st.switcher.label_image(0);
            match img {
                None if tries < 40 => next(p + 1, lines, 50),
                _ => {
                    let lum = img.as_deref().and_then(text_luminance);
                    let path = DIAG_PATH.get().cloned().unwrap_or_default();
                    if let Some(i) = img.as_deref() {
                        ui::write_png(i, &format!("{path}.{}.png", if base == 1 { "light" } else { "dark" }));
                    }
                    let effective = app.effectiveAppearance().name().to_string();
                    lines.push(format!(
                        "pass={} app_appearance={effective} palette_dark={dark_pal} label_rendered={} text_luminance={}",
                        if base == 1 { "light" } else { "dark" },
                        img.is_some(),
                        lum.map(|l| format!("{l:.2}")).unwrap_or_else(|| "none".into())
                    ));
                    if base == 1 {
                        next(2, lines, 100);
                    } else {
                        st.switcher.end_warm_up();
                        let lum_of = |pass: &str| {
                            lines.iter().find(|l| l.contains(&format!("pass={pass} "))).and_then(|l| l.rsplit("text_luminance=").next()?.parse::<f64>().ok())
                        };
                        let ok = matches!((lum_of("light"), lum_of("dark")), (Some(a), Some(b)) if a < 0.35 && b > 0.65);
                        lines.push(format!("RESULT {}", if ok { "PASS" } else { "FAIL" }));
                        let path = DIAG_PATH.get().cloned().unwrap_or_else(|| "/dev/stdout".into());
                        let _ = std::fs::write(&path, lines.join("\n") + "\n");
                        exit_now(0);
                    }
                }
            }
        }
        _ => {}
    }
}

/// Mean brightness (0 dark – 1 light) of the most opaque pixels of `img`: its text.
fn text_luminance(img: &objc2_core_graphics::CGImage) -> Option<f64> {
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    use objc2_core_graphics::{CGBitmapContextCreate, CGColorSpace, CGContext, CGImage, CGImageAlphaInfo};
    let (w, h) = (CGImage::width(Some(img)), CGImage::height(Some(img)));
    // SAFETY: CoreGraphics-provided constant.
    let cs = CGColorSpace::with_name(Some(unsafe { objc2_core_graphics::kCGColorSpaceSRGB }))?;
    let mut buf = vec![0u8; w * h * 4];
    let info = CGImageAlphaInfo::PremultipliedLast.0;
    // SAFETY: `buf` is w*h*4 bytes, rows of w*4 bytes, and outlives the context.
    let ctx = unsafe { CGBitmapContextCreate(buf.as_mut_ptr().cast(), w, h, 8, w * 4, Some(&cs), info)? };
    CGContext::draw_image(Some(&ctx), CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w as f64, h as f64)), Some(img));
    drop(ctx);
    // The text only: right of the small icon (18 pt + 5 pt gap, at most 2x).
    let text_px: Vec<&[u8]> = buf.chunks_exact(4).enumerate().filter(|(i, _)| i % w >= 50).map(|(_, p)| p).collect();
    // The most opaque pixels are the text's cores (its colour may be translucent itself).
    let max_a = text_px.iter().map(|p| p[3]).max().unwrap_or(0);
    let (mut sum, mut n) = (0.0, 0usize);
    for px in text_px {
        let a = f64::from(px[3]);
        if max_a > 0 && a >= 0.8 * f64::from(max_a) {
            sum += (f64::from(px[0]) + f64::from(px[1]) + f64::from(px[2])) / 3.0 / a;
            n += 1;
        }
    }
    (n > 0).then(|| sum / n as f64)
}

struct Bench {
    out: String,
    screens: Vec<Retained<objc2_app_kit::NSScreen>>,
    variants: Vec<BenchVariant>,
    /// Per variant: one panel per screen (per-display variants) or one for all.
    panels: Vec<Vec<Rc<Switcher>>>,
    tiles: Vec<Tile>,
    /// (variant, screen, wait before the show in ms).
    plan: Vec<(usize, usize, u64)>,
    hold_ms: u64,
    shows: Vec<Rc<RefCell<BenchShow>>>,
    header: String,
}

/// `ztab --panel-bench <out> [--bench-variants v0,layers,keep,layers+keep,layers+keep+perdisplay]
/// [--bench-rounds n] [--bench-gap-ms ms] [--bench-hold-ms ms] [--bench-long-gap-s s]
/// [--bench-no-thumbs] [--bench-no-warmup] [--bench-snapshot png]`: diagnostics (runs beside a
/// running ZTab, posts no input). Compares ways of showing the switcher (`draw`: thumbnails drawn
/// in `drawRect:`; `layers`: thumbnail layers; `keep`: panel kept ordered in between sessions;
/// `perdisplay`: one panel per display), interleaved in one process so they see the same system
/// load: per round, per display, each variant shows the panel with every window (held `hold_ms`,
/// `gap_ms` between shows; then once more per variant after `long_gap_s`). Writes, per show, how
/// long ordering it in took and how long the main thread stayed busy afterwards (Core
/// Animation's commit and the first draw).
fn panel_bench(st: &Rc<State>, _ready: bool) {
    let args: Vec<String> = std::env::args().collect();
    let num = |name: &str, d: u64| arg_value(&args, name).and_then(|v| v.parse().ok()).unwrap_or(d);
    let (rounds, gap_ms, hold_ms, long_gap_s) =
        (num("--bench-rounds", 3), num("--bench-gap-ms", 700), num("--bench-hold-ms", 300), num("--bench-long-gap-s", 0));
    let variants: Vec<BenchVariant> = arg_value(&args, "--bench-variants")
        .unwrap_or_else(|| "v0,layers,keep,layers+keep,layers+keep+perdisplay".into())
        .split(',')
        .filter_map(BenchVariant::parse)
        .collect();
    let no_thumbs = args.iter().any(|a| a == "--bench-no-thumbs");
    let no_warmup = args.iter().any(|a| a == "--bench-no-warmup");
    start_stall_detector(st, None, STALL_SUMMARY_S, STALL_THRESHOLD_MS);
    let Some(m) = st.model() else { exit_now(2) };
    // Capture every window now (the real app has thumbnails from focus changes and refreshes).
    let wids: Vec<u32> = m.snapshot().windows.iter().filter(|w| !w.minimized && !w.hidden_app).map(|w| w.wid).collect();
    if !no_thumbs {
        st.thumbs.capture(wids, THUMB_MAX_W, THUMB_MAX_H, 0, |_, _, _| {});
    }
    after_on_main(Duration::from_secs(5), move || {
        let st = state().unwrap();
        let snap = st.model().unwrap().snapshot();
        let mut group = 0usize;
        let mut tiles = Vec::new();
        for (i, w) in snap.windows.iter().enumerate() {
            if i > 0 && snap.windows[i - 1].pid != w.pid {
                group += 1;
            }
            let mut t = st.make_tile(w, group, false);
            if no_thumbs {
                (t.thumbnail, t.thumb_cg) = (None, None);
            }
            tiles.push(t);
        }
        let screens: Vec<Retained<objc2_app_kit::NSScreen>> = objc2_app_kit::NSScreen::screens(st.mtm).iter().collect();
        let panels: Vec<Vec<Rc<Switcher>>> = variants
            .iter()
            .map(|v| {
                (0..if v.per_display { screens.len() } else { 1 })
                    .map(|_| {
                        let p = Switcher::new(st.mtm, Box::new(|_| {}), Box::new(|_| {}));
                        p.set_layer_tree(v.layers);
                        p.set_keep_ordered_in(v.keep);
                        p.set_backing_8bit(v.rgba8);
                        p.set_skip_fills(v.nofill);
                        Rc::new(p)
                    })
                    .collect()
            })
            .collect();
        let mut plan: Vec<(usize, usize, u64)> = Vec::new();
        for r in 0..rounds as usize {
            for k in 0..screens.len() {
                for j in 0..variants.len() {
                    // Rotate the order each round so no variant always goes first.
                    plan.push(((j + r) % variants.len(), k, gap_ms));
                }
            }
        }
        if long_gap_s > 0 {
            for j in 0..variants.len() {
                plan.push((j, 0, if j == 0 { long_gap_s * 1000 } else { gap_ms }));
            }
        }
        let header = format!(
            "panel bench: {} tiles ({} with thumbnails), {} screens {:?}, variants {:?}, warmup={} rounds={rounds} gap_ms={gap_ms} \
             hold_ms={hold_ms} long_gap_s={long_gap_s} load={}",
            tiles.len(),
            tiles.iter().filter(|t| t.thumbnail.is_some()).count(),
            screens.len(),
            screens.iter().map(|s| (s.frame().origin.x, s.backingScaleFactor())).collect::<Vec<_>>(),
            variants.iter().map(BenchVariant::name).collect::<Vec<_>>(),
            !no_warmup,
            load_average()
        );
        log(&header);
        if !no_warmup {
            // Like the real launch: warm up once (on each display's own panel when per display).
            for (v, ps) in variants.iter().zip(panels.iter()) {
                if v.per_display {
                    for (p, s) in ps.iter().zip(screens.iter()) {
                        p.warm_up_on(tiles.clone(), Some(s));
                    }
                } else {
                    ps[0].warm_up(tiles.clone());
                }
            }
        }
        let bench = Rc::new(RefCell::new(Bench {
            out: DIAG_PATH.get().cloned().unwrap_or_else(|| "/dev/stdout".into()),
            screens,
            variants,
            panels,
            tiles,
            plan,
            hold_ms,
            shows: Vec::new(),
            header,
        }));
        let b2 = bench.clone();
        after_on_main_local(Duration::from_millis(300), move || {
            for ps in &b2.borrow().panels {
                for p in ps {
                    p.end_warm_up();
                }
            }
            bench_step(b2.clone(), 0);
        });
    });
}

/// The 1-minute load average (a busy machine makes every window-server request slower).
fn load_average() -> String {
    let mut l = [0f64; 3];
    // SAFETY: valid buffer of 3 doubles.
    let n = unsafe { libc::getloadavg(l.as_mut_ptr(), 3) };
    if n >= 1 { format!("{:.1}", l[0]) } else { "?".into() }
}

fn bench_step(bench: Rc<RefCell<Bench>>, i: usize) {
    let Some(&(variant, screen, wait)) = bench.borrow().plan.get(i) else {
        // Let the last show's measurement land, then (with `--bench-snapshot <png>`) capture the
        // panel as the window server shows it (one tile marked minimized), then write the results.
        let png = arg_value(&std::env::args().collect::<Vec<_>>(), "--bench-snapshot");
        after_on_main_local(Duration::from_millis(500), move || match png {
            Some(png) => bench_snapshot(bench, png),
            None => bench_finish(&bench),
        });
        return;
    };
    let b2 = bench.clone();
    after_on_main_local(Duration::from_millis(wait), move || {
        let (panel, scr, tiles, hold) = {
            let b = b2.borrow();
            let ps = &b.panels[variant];
            let panel = ps[if ps.len() > 1 { screen } else { 0 }].clone();
            (panel, b.screens[screen].clone(), b.tiles.clone(), b.hold_ms)
        };
        let first = !b2.borrow().shows.iter().any(|s| {
            let s = s.borrow();
            s.screen == screen && s.variant == variant
        });
        let t = Instant::now();
        panel.show_on(tiles, 0, Some(&scr));
        let show = Rc::new(RefCell::new(BenchShow {
            variant,
            screen,
            first_on_screen: first,
            gap_before_ms: wait,
            show_ms: ms_since(t),
            busy_ms: None,
            longest_ms: None,
        }));
        let s2 = show.clone();
        sysstate::when_main_idle(t, move |busy, longest| {
            let mut s = s2.borrow_mut();
            s.busy_ms = Some(busy);
            s.longest_ms = Some(longest);
        });
        b2.borrow_mut().shows.push(show);
        let b3 = b2.clone();
        after_on_main_local(Duration::from_millis(hold), move || {
            panel.hide();
            bench_step(b3, i + 1);
        });
    });
}

fn bench_snapshot(bench: Rc<RefCell<Bench>>, png: String) {
    let (panel, scr, mut tiles) = {
        let b = bench.borrow();
        (b.panels[0][0].clone(), b.screens[0].clone(), b.tiles.clone())
    };
    if let Some(t) = tiles.iter_mut().find(|t| t.thumbnail.is_some()) {
        t.minimized = true;
    }
    panel.show_on(tiles, 0, Some(&scr));
    after_on_main_local(Duration::from_millis(600), move || {
        let st = state().unwrap();
        let f = panel.panel().frame();
        let wid = panel.panel().windowNumber() as u32;
        // Blocking (up to 1.5 s): acceptable here, the measurements are done.
        let ok = st.thumbs.capture_sync(wid, f.size.width, f.size.height).is_some_and(|img| {
            // SAFETY: null proposed rect, no context or hints.
            let cg = unsafe { img.CGImageForProposedRect_context_hints(std::ptr::null_mut(), None, None) };
            cg.and_then(|cg| {
                let rep = objc2_app_kit::NSBitmapImageRep::initWithCGImage(objc2_app_kit::NSBitmapImageRep::alloc(), &cg);
                // SAFETY: empty properties dictionary.
                unsafe { rep.representationUsingType_properties(objc2_app_kit::NSBitmapImageFileType::PNG, &objc2_foundation::NSDictionary::new()) }
            })
            .is_some_and(|d| d.writeToFile_atomically(&NSString::from_str(&png), true))
        });
        log(&format!("panel bench snapshot {png}: {ok}"));
        panel.hide();
        bench_finish(&bench);
    });
}

fn bench_finish(bench: &Rc<RefCell<Bench>>) {
    let b = bench.borrow();
    let mut out = format!("{} load_at_end={}\n", b.header, load_average());
    let n = b.variants.len();
    let (mut first, mut repeat): (Vec<Vec<f64>>, Vec<Vec<f64>>) = (vec![Vec::new(); n], vec![Vec::new(); n]);
    for (i, s) in b.shows.iter().enumerate() {
        let s = s.borrow();
        let x = b.screens[s.screen].frame().origin.x;
        out += &format!(
            "show {i} {} screen={} (x={x}) first_on_screen={} gap_before_ms={} show_ms={:.2} busy_ms={} longest_unit_ms={}\n",
            b.variants[s.variant].name(),
            s.screen,
            s.first_on_screen,
            s.gap_before_ms,
            s.show_ms,
            s.busy_ms.map(|v| format!("{v:.1}")).unwrap_or("?".into()),
            s.longest_ms.map(|v| format!("{v:.1}")).unwrap_or("?".into())
        );
        if let Some(v) = s.busy_ms {
            if s.first_on_screen { first[s.variant].push(v) } else { repeat[s.variant].push(v) }
        }
    }
    let med = |v: &mut Vec<f64>| {
        v.sort_by(f64::total_cmp);
        if v.is_empty() {
            "n=0".to_string()
        } else {
            format!("n={} median={:.1} p90={:.1} max={:.1}", v.len(), v[v.len() / 2], v[((v.len() - 1) as f64 * 0.9).round() as usize], v[v.len() - 1])
        }
    };
    for j in 0..n {
        out += &format!(
            "variant {}: busy_ms first show on a screen: {}; repeat shows: {} [{}]\n",
            b.variants[j].name(),
            med(&mut first[j]),
            med(&mut repeat[j]),
            b.panels[j][0].layer_info()
        );
    }
    let _ = std::fs::write(&b.out, out);
    log("panel bench done");
    exit_now(0);
}

/// `after_on_main` for closures that are not `Send` (main thread only).
fn after_on_main_local(delay: Duration, f: impl FnOnce() + 'static) {
    let cell = std::cell::RefCell::new(Some(f));
    let block = RcBlock::new(move |_t: NonNull<NSTimer>| {
        if let Some(f) = cell.borrow_mut().take() {
            f();
        }
    });
    // SAFETY: the block lives as long as the timer retains it; one-shot timer on the main run loop.
    let _ = unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(delay.as_secs_f64(), false, &block) };
}

/// The run-loop stall detector (no cost while the main thread sleeps), plus the probe detector
/// when `probe_period_ms` is given (`--stall-period-ms`, for tests).
fn start_stall_detector(st: &State, probe_period_ms: Option<u64>, summary_s: u64, threshold_ms: u64) {
    let threshold = Duration::from_millis(threshold_ms.max(1));
    let summary = Some(Duration::from_secs(summary_s.max(1)));
    let mut v = st.stall.borrow_mut();
    if let Some(rl) = CFRunLoop::main() {
        v.push(StallDetector::observe(&rl, threshold, summary, stall_log));
    }
    if let Some(p) = probe_period_ms {
        v.push(StallDetector::start_with_summary(Duration::from_millis(p.max(1)), threshold, summary, stall_log));
    }
}

/// Unix time (ms) up to which every unit of main-thread work counts as launch work: the first
/// second of the run loop (set when it starts).
static LAUNCH_UNTIL_MS: AtomicU64 = AtomicU64::new(0);
/// Unix time (ms) when the last deferred launch step (`launch_step`) finished.
static LAUNCH_STEP_END_MS: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    (now_secs() * 1000.0) as u64
}

/// The stall detectors' log. A unit of main-thread work above the threshold that is launch work
/// (it ended in the run loop's first second, or a deferred launch step ran in it) is logged as
/// `launch work on the main thread: <ms> ms (…)`, every other one as `main thread stall: …`. Both
/// are real waits for an event that arrives meanwhile (a ⌥Tab pressed during launch work is acted
/// on when it ends); the wording keeps `main thread stall` for ZTab in use.
fn stall_log(s: &str) {
    if let Some(rest) = s.strip_prefix("main thread stall: ") {
        let d = rest.split(' ').next().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        let now = now_ms();
        let step_end = LAUNCH_STEP_END_MS.load(Ordering::SeqCst);
        if now <= LAUNCH_UNTIL_MS.load(Ordering::SeqCst) || (step_end > 0 && now.saturating_sub(step_end) <= d + 20) {
            log(&format!("launch work on the main thread: {rest}"));
            return;
        }
    }
    log(s);
}

/// A deferred launch step, `delay` from now on the main thread. While a switcher session is open
/// it waits (250 ms at a time), so a ⌥Tab during launch is not held up by work that can wait.
fn launch_step<F: Fn(&State) + Send + Copy + 'static>(delay: Duration, name: &'static str, f: F) {
    after_on_main(delay, move || {
        let Some(st) = state() else { return };
        if st.active.get() {
            log(&format!("launch: {name} postponed (switcher open)"));
            launch_step(Duration::from_millis(250), name, f);
            return;
        }
        f(&st);
        LAUNCH_STEP_END_MS.store(now_ms(), Ordering::SeqCst);
    });
}

/// Launch timeline: (phase, unix time) marks, logged once the run loop is about to start.
struct Timeline(Vec<(&'static str, f64)>);

impl Timeline {
    fn mark(&mut self, what: &'static str) {
        self.0.push((what, now_secs()));
    }
    fn line(&self) -> String {
        let marks: Vec<String> = self.0.iter().map(|(w, t)| format!("{w}={}", since_launch(*t))).collect();
        format!("launch timeline (ms after process start): {}", marks.join(" "))
    }
}

fn main() {
    let mut timeline = Timeline(Vec::new());
    timeline.mark("main");
    let mtm = MainThreadMarker::new().expect("must run on the main thread");
    let args: Vec<String> = std::env::args().collect();

    if let Some(i) = args.iter().position(|a| a == "--dump-windows") {
        dump_windows(args.get(i + 1).map(String::as_str).unwrap_or("/dev/stdout"));
        return;
    }

    // Settings location: the user's, or `--settings-dir <dir>` / $ZTAB_SETTINGS_DIR (tests).
    let paths = Paths::from_args(&args);

    // Command line: the login item (scripts/install.sh --login runs `ztab --set-login on`).
    if let Some(v) = arg_value(&args, "--set-login") {
        let on = match v.as_str() {
            "on" | "true" | "1" => true,
            "off" | "false" | "0" => false,
            _ => {
                eprintln!("usage: ztab --set-login on|off [--settings-dir <dir>] [--program <path>]");
                std::process::exit(2);
            }
        };
        let program = arg_value(&args, "--program").unwrap_or_else(settings::current_program);
        match settings::cli_set_login(&paths, on, &program) {
            Ok(line) => {
                println!("{line}");
                log(&format!("--set-login {v}: {line} (program {program})"));
                exit_now(0);
            }
            Err(e) => {
                eprintln!("ztab --set-login {v}: {e}");
                log(&format!("--set-login {v} failed: {e}"));
                exit_now(1);
            }
        }
    }
    if args.iter().any(|a| a == "--login-status") {
        // Relative to this binary: "on" only when the agent starts it.
        let state = paths.login_state(&settings::current_program());
        println!(
            "start at login: {} ({}){}",
            state.describe(),
            paths.agent_plist.display(),
            paths.login_program().map(|p| format!(" program {p}")).unwrap_or_default()
        );
        std::process::exit(match state {
            settings::LoginState::On => 0,
            settings::LoginState::Off => 1,
            settings::LoginState::Elsewhere { .. } => 3,
        });
    }

    let diag = ["--snapshot", "--dump-model", "--panel-bench", "--label-check"].iter().any(|f| args.iter().any(|a| a == f));
    // One ZTab at a time, whatever settings it uses (the diagnostics modes run beside it).
    if !diag && !settings::take_instance_lock() {
        log(&format!("another ZTab instance is running; exiting (pid {})", std::process::id()));
        log_flush(Duration::from_millis(300));
        return;
    }

    // A tiny file: read here (well under a millisecond) so the tap starts with the right shortcuts.
    let t_settings = Instant::now();
    let (loaded, notes, existed) = paths.load();
    let settings_ms = ms_since(t_settings);

    timeline.mark("settings");

    // Login start: first what the first ⌥Tab needs, each on its own thread, so both work while
    // AppKit starts up below: the event tap (it needs only the shortcuts) and the window model
    // (its background thread enumerates the apps and reads the window list; the per-app threads
    // start once the main run loop runs). Everything else comes after, and what the first switch
    // does not need (status menu, icons, panel and Settings warm-ups, the capture pipeline)
    // waits until the run loop is up.
    let trusted = !diag && windows::ax_trusted();
    // The window-server connection is made here, on the main thread, before any other thread
    // talks to the window server (AppKit adopts it below).
    let _ = windows::main_cid();
    timeline.mark("connection");
    let early_tap = trusted && spawn_tap(loaded.hotkeys).is_ok();
    timeline.mark("tap_spawned");
    let early_model = trusted.then(|| WindowModel::start(mtm));
    timeline.mark("model_started");

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    timeline.mark("nsapp");

    let st = make_state(mtm, paths, loaded);
    windows::set_own_window_raiser(raise_own_window);
    st.hk_installing.set(early_tap);
    if let Some(m) = early_model {
        st.adopt_model(m);
    }
    timeline.mark("state");

    if let Some(path) = arg_value(&args, "--label-check") {
        let _ = DIAG_PATH.set(path);
        label_check(50, Vec::new());
        app.run();
        return;
    }
    for (flag, f) in [("--snapshot", snapshot_png as fn(&Rc<State>, bool)), ("--dump-model", dump_model), ("--panel-bench", panel_bench)] {
        if args.iter().any(|a| a == flag) {
            let path = arg_value(&args, flag).unwrap_or_else(|| "/dev/stdout".into());
            let _ = DIAG_PATH.set(path);
            st.ensure_model(true);
            when_model_ready(Instant::now(), Duration::from_secs(10), f);
            app.run();
            return;
        }
    }

    let probe_period: Option<u64> = arg_value(&args, "--stall-period-ms").and_then(|v| v.parse().ok());
    let stall_summary = arg_value(&args, "--stall-summary-s").and_then(|v| v.parse().ok()).unwrap_or(STALL_SUMMARY_S);
    let stall_threshold = arg_value(&args, "--stall-threshold-ms").and_then(|v| v.parse().ok()).unwrap_or(STALL_THRESHOLD_MS);
    if let Some(s) = arg_value(&args, "--idle-pause-s").and_then(|v| v.parse::<f64>().ok()) {
        st.idle_limit_s.set(s.max(1.0));
    }
    // One preflight (it costs a few ms of main-thread time), cached from here on when granted.
    let screen_granted = thumbs::screen_capture_allowed();
    log(&format!(
        "starting pid={} ({} ms after process start) ax={} screen={screen_granted} stall_probe_period_ms={probe_period:?} idle_pause_s={}",
        std::process::id(),
        since_launch(now_secs()),
        windows::ax_trusted(),
        st.idle_limit_s.get()
    ));
    log(&format!(
        "settings: {}{} ({}, read in {settings_ms:.2} ms): {}",
        st.paths.settings_file.display(),
        if st.paths.isolated { " [isolated]" } else { "" },
        if existed { "loaded" } else { "no file: defaults" },
        st.settings.borrow().describe()
    ));
    for n in notes {
        log(&format!("settings: {n}"));
    }

    if !windows::ax_trusted() {
        prompt_accessibility();
    }
    if !screen_granted {
        // Shows the system prompt only the first time; afterwards it's a no-op.
        unsafe { CGRequestScreenCaptureAccess() };
    }
    st.install_workspace_observers();
    // No-ops when they were started above (Accessibility granted).
    st.try_install_hotkey();
    st.ensure_model(false);
    st.schedule_timers();
    timeline.mark("observers");

    // The status item and its menu (a few ms of AppKit work, nothing the first switch needs):
    // right after the run loop's first turns, when the tap and the model are up.
    // Each deferred step waits while a switcher session is open (`launch_step`).
    let menu_warmup = !args.iter().any(|a| a == "--no-menu-warmup");
    launch_step(Duration::from_millis(60), "status item", move |st| {
        let t = Instant::now();
        *st.menu.borrow_mut() = Some(build_status_item(st.mtm));
        if menu_warmup && let Some(m) = st.menu.borrow().as_ref() {
            warm_up_menu(m);
        }
        st.refresh_menu();
        st.refresh_secure_ui();
        log(&format!("status item built in {:.1} ms ({} ms after process start)", ms_since(t), since_launch(now_secs())));
    });
    // The capture pipeline (a ScreenCaptureKit content fetch, off the main thread) once the
    // window model has its first window list, so the two do not compete for the window server.
    launch_step(Duration::from_millis(300), "capture warm-up", |st| st.thumbs.warm_up());
    launch_step(Duration::from_millis(400), "panel warm-up", |st| st.warm_up_panel());
    launch_step(Duration::from_millis(700), "settings window", |st| {
        st.build_settings_window("at launch");
    });
    // The detectors run from the first turn of the run loop: launch work is logged too, as such
    // (`stall_log`).
    LAUNCH_UNTIL_MS.store(now_ms() + 1000, Ordering::SeqCst);
    start_stall_detector(&st, probe_period, stall_summary, stall_threshold);
    // Verification aid: block the main thread once, so the stall detectors can be checked in the
    // real app (`--inject-stall-ms 150` must log one stall of about 150 ms), `--inject-stall-at-s`
    // (default 3) after launch.
    if let Some(ms) = arg_value(&args, "--inject-stall-ms").and_then(|v| v.parse::<u64>().ok()) {
        let at = arg_value(&args, "--inject-stall-at-s").and_then(|v| v.parse::<f64>().ok()).unwrap_or(3.0);
        after_on_main(Duration::from_secs_f64(at.max(0.0)), move || {
            log(&format!("injecting a {ms} ms main-thread stall"));
            std::thread::sleep(Duration::from_millis(ms));
        });
    }

    timeline.mark("run_loop");
    log(&timeline.line());
    app.run();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(wid: u32, pid: i32) -> WinInfo {
        WinInfo {
            wid,
            pid,
            app_name: String::new(),
            bundle_id: None,
            title: String::new(),
            bounds: (0.0, 0.0, 100.0, 100.0),
            minimized: false,
            on_current_space: true,
            hidden_app: false,
            z_index: 0,
            ax: None,
        }
    }

    #[test]
    fn selection_rules() {
        // Quick tap: previous window when in a window, else the most recent one.
        assert_eq!(initial_selection(Some(2), &[2, 0, 1], false), Some(0));
        assert_eq!(initial_selection(None, &[2, 0, 1], false), Some(2));
        // Single window: stays on it.
        assert_eq!(initial_selection(Some(0), &[0], false), Some(0));
        // Shift: least recently used.
        assert_eq!(initial_selection(Some(2), &[2, 0, 1], true), Some(1));
        assert_eq!(initial_selection(None, &[], false), None);
        assert_eq!(initial_selection(None, &[], true), None);
    }

    #[test]
    fn same_app_filter_reindexes() {
        let snap = Snapshot {
            windows: vec![win(10, 1), win(11, 1), win(20, 2), win(21, 2), win(22, 2)],
            current: Some(3),
            mru: vec![3, 0, 4, 1, 2],
            active_pid: Some(2),
        };
        let (w, cur, mru) = filter_snapshot(snap, |w| w.pid == 2);
        assert_eq!(w.iter().map(|w| w.wid).collect::<Vec<_>>(), vec![20, 21, 22]);
        assert_eq!(cur, Some(1));
        assert_eq!(mru, vec![1, 2, 0]);
        assert_eq!(initial_selection(cur, &mru, false), Some(2));
        // Filter that drops the current window.
        let snap = Snapshot { windows: vec![win(10, 1), win(20, 2)], current: Some(0), mru: vec![0, 1], active_pid: Some(2) };
        let (w, cur, mru) = filter_snapshot(snap, |w| w.pid == 2);
        assert_eq!((w.len(), cur, mru), (1, None, vec![0]));
    }
}
