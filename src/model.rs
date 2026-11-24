//! Persistent window model.
//!
//! Background threads keep a cache of every window, so the main thread (which also services the
//! keyboard event tap) never makes an Accessibility call or waits on another app:
//!
//! * one `ztab-ax-<pid>` thread per regular app runs its own CFRunLoop hosting that app's
//!   `AXObserver`. Every AX call for the app happens on that thread with a 0.5 s messaging
//!   timeout, so a busy or hung app stalls only its own thread. The thread scans `AXWindows` once,
//!   then follows notifications (window created / destroyed / (de)miniaturized / title, focused and
//!   main window, app activated / hidden / shown). A safety-net rescan reads only `AXWindows` (plus
//!   new windows and windows whose notifications could not be registered) every 60 s, every 7 s
//!   for an app with a failed registration, and every window's attributes every 5 minutes; it is
//!   paused while the display sleeps, the screen is locked or the user is idle. The thread also finds windows on
//!   other Spaces through the remote-token sweep and caches every AX element.
//! * one `ztab-model-bg` thread keeps a parsed copy of the full CoreGraphics window list (z-order,
//!   bounds, Space membership) and the Screen Recording flag, and re-enumerates NSWorkspace's
//!   running apps when needed. It refreshes on every AX window / focus event, every NSWorkspace
//!   event and at least every 5 s. Reading `NSRunningApplication` properties costs ~0.16 ms per
//!   app after every main run-loop turn (LaunchServices refresh), and CG list calls can block for
//!   100+ ms while WindowServer is busy, so neither ever happens in `snapshot()`.
//! * the main thread observes NSWorkspace notifications (launch / terminate / activate / hide /
//!   unhide / Space change), starts and stops the app threads, keeps the most-recently-used
//!   history and delivers [`ModelEvent`]s.
//!
//! `snapshot()` makes no IPC: it merges the cached CG list with the cached AX state in memory.
//! Apps whose thread has not finished its first scan (or whose AX is broken) fall back to the
//! CG-only rules of `windows::list_windows`.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::ptr::NonNull;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2_app_kit::{
    NSApplicationActivationPolicy, NSRunningApplication, NSWorkspace, NSWorkspaceActiveSpaceDidChangeNotification, NSWorkspaceApplicationKey,
    NSWorkspaceDidActivateApplicationNotification, NSWorkspaceDidHideApplicationNotification,
    NSWorkspaceDidLaunchApplicationNotification, NSWorkspaceDidTerminateApplicationNotification,
    NSWorkspaceDidUnhideApplicationNotification,
};
use objc2_application_services::{AXError, AXObserver, AXUIElement};
use objc2_core_foundation::{CFArray, CFRetained, CFRunLoop, CFString, kCFRunLoopDefaultMode};
use objc2_core_graphics::CGWindowListOption;
use objc2_foundation::{NSNotification, NSNotificationName, NSOperationQueue};

use crate::windows::{self, AppMeta, AxAttrs, CgWin, WinInfo};

/// AX messaging timeout for every element the app threads use.
const AX_TIMEOUT_S: f32 = 0.5;
/// Retry cap (failed scans and observer creation back off up to this) and the rescan period of
/// an app whose notifications could not all be registered.
const RESCAN_EVERY: Duration = Duration::from_secs(7);
/// Safety-net rescan period of an app whose notifications are all registered: notifications
/// cover window creation / destruction, minimizing, titles and focus, so this only catches a
/// missed one. Also how long a paused safety-net scan is postponed.
const RESCAN_SAFETY: Duration = Duration::from_secs(60);
/// A full scan (every window's attributes) happens at most this often; other scans read
/// `AXWindows` plus the windows that are new or have no notifications.
const FULL_SCAN_EVERY: Duration = Duration::from_secs(300);
/// First retry delay after a failed scan (doubles up to `RESCAN_EVERY`).
const RETRY_MIN: Duration = Duration::from_millis(250);
/// A window the model learned about less than this before a CG list started may be missing
/// from it (it may have been created during the list call).
const CG_SLACK: Duration = Duration::from_millis(50);
/// App enumerations requested by launches of non-regular apps run at most this often (the last
/// request is never dropped: it runs when the gap has passed).
const ENUM_MIN_GAP: Duration = Duration::from_secs(1);
/// While Screen Recording is not granted, the flag is re-read at most this often (a granted flag
/// is kept: the preflight costs ~4 ms and a revoked grant only applies after a relaunch).
const SCREEN_REC_RECHECK: Duration = Duration::from_secs(2);
/// Periodic CG refresh while the model is paused (display asleep / screen locked).
const CG_REFRESH_PAUSED: Duration = Duration::from_secs(60);
/// A window must stay current this long to enter the MRU history on its own. Shorter spells are
/// transient (e.g. an app is activated with its old focused window, then the clicked window
/// takes focus a few ms later) and must not push the window the user came from down the list.
const SETTLE: Duration = Duration::from_millis(250);
/// After `note_focused`, polled focus reads of that app that disagree are ignored this long (the
/// switch is still landing), and NSWorkspace's frontmost app is not used to correct the model.
const NOTE_GRACE: Duration = Duration::from_millis(1000);
/// Time budget of one remote-token sweep on an app thread.
const TOKEN_BUDGET: Duration = Duration::from_millis(300);
/// A window id the token sweep did not find is not searched again for this long.
const TOKEN_NEG_TTL: Duration = Duration::from_secs(600);
/// The full CG list is refreshed at least this often (and right after window events).
const CG_REFRESH_EVERY: Duration = Duration::from_secs(5);
/// A pid that owns windows but is not a regular app is re-checked (full app enumeration on the
/// background thread) at most this often, to catch apps that turn regular after launch.
const NON_REGULAR_RECHECK: Duration = Duration::from_secs(60);
/// Bursts of window events are coalesced into one CG refresh.
const CG_COALESCE: Duration = Duration::from_millis(30);
/// Space membership of an off-screen window is cached this long (window ids are never reused).
const WIN_SPACES_TTL: Duration = Duration::from_secs(60);
/// The set of visible Spaces is cached this long (and dropped on every Space change).
const CURRENT_SPACES_TTL: Duration = Duration::from_secs(10);
/// NSWorkspace's frontmost app must disagree with the model this long before it is corrected.
const FRONT_MISMATCH: Duration = Duration::from_millis(1500);
/// After a focus change inside an app, its windows are rescanned this soon (tab switches).
const FOCUS_RESCAN_AFTER: Duration = Duration::from_millis(400);
/// An app thread is asked for an extra rescan at most this often by `snapshot()`.
const POKE_EVERY: Duration = Duration::from_secs(2);
/// Resuming after a lock, display sleep or session switch rescans every app: the first this
/// long after the resume, then one more app every `RESUME_RESCAN_SPREAD`.
const RESUME_RESCAN_AFTER: Duration = Duration::from_millis(1000);
const RESUME_RESCAN_SPREAD: Duration = Duration::from_millis(150);
/// `snapshot()` asks again about the same window (on screen but unknown to AX, or known but
/// missing from the window list) at most this often once a rescan it asked for has finished
/// without settling it (after one quick retry): a window AX never reports must not turn the
/// periodic snapshots into a rescan of its app every `POKE_EVERY`. See [`Asks`].
const REPOKE_SAME_WINDOW: Duration = Duration::from_secs(30);

const APP_NOTIFICATIONS: [&str; 6] = [
    "AXWindowCreated",
    "AXFocusedWindowChanged",
    "AXMainWindowChanged",
    "AXApplicationHidden",
    "AXApplicationShown",
    "AXApplicationActivated",
];
const WINDOW_NOTIFICATIONS: [&str; 4] =
    ["AXUIElementDestroyed", "AXWindowMiniaturized", "AXWindowDeminiaturized", "AXTitleChanged"];

// ---------------------------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------------------------

/// Something the model noticed. Delivered on the main thread through the event sink.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelEvent {
    /// `wid` stopped being the current window (the focused window of the active app). It is still
    /// on screen (not minimized, app not hidden), so it can be captured now.
    FocusLeft { wid: u32 },
    /// `wid` became the current window.
    Focused { wid: u32 },
    Minimized { wid: u32 },
    Destroyed { wid: u32 },
    /// A regular app appeared after the model started (not sent for apps running at start).
    AppLaunched { pid: i32 },
    AppTerminated { pid: i32 },
}

/// ZTab's own window that the switcher lists like any other (the Settings window), as the main
/// thread knows it. ZTab is an accessory app, so the model would otherwise never see it; it is
/// never asked about over Accessibility.
#[derive(Clone, Debug, PartialEq)]
pub struct OwnWindow {
    pub wid: u32,
    pub app_name: String,
    pub title: String,
    /// CG (top-left origin) frame; used until the background CG list has the window.
    pub bounds: (f64, f64, f64, f64),
    pub minimized: bool,
}

/// The switcher's window list.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// Grouped by app (contiguous). Apps are ordered by their most recently used window, windows
    /// within an app most recent first; never-focused windows come after, by z-order.
    pub windows: Vec<WinInfo>,
    /// Index of the window the user is in right now (the focused window of the frontmost app),
    /// when it is listed. `None` e.g. when the active app has no window or the desktop is focused.
    pub current: Option<usize>,
    /// All indices, most recent first (`current` first when it is `Some`).
    pub mru: Vec<usize>,
    /// The active (frontmost) app as the model knows it, even when it has no listed window.
    pub active_pid: Option<i32>,
}

/// Main-thread handle of the window model. See the module docs.
pub struct WindowModel {
    shared: Arc<Shared>,
    threads: RefCell<HashMap<i32, AppThread>>,
    sink: RefCell<Option<Rc<dyn Fn(ModelEvent)>>>,
    observers: RefCell<Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>>,
    poked: RefCell<HashMap<i32, Instant>>,
    /// Windows `snapshot()` asked an app thread to rescan for (see [`Asks`]).
    asked: RefCell<Asks>,
    /// Owner pid of every window in the last snapshot (for `note_focused` on CG-only windows).
    pids_by_wid: RefCell<HashMap<u32, i32>>,
    /// Durations (ms) of recent `snapshot()` calls, for `stats()`.
    snap_ms: RefCell<Vec<f64>>,
    /// Slowest snapshot so far: (ms, per-phase cumulative timings).
    slowest: RefCell<(f64, String)>,
    /// Slowest main-thread notification handler / event drain so far: (ms, what).
    slowest_handler: RefCell<(f64, String)>,
    next_gen: Cell<u64>,
    /// AppLaunched events are only sent after the initial reconcile.
    started: Cell<bool>,
    own_pid: i32,
}

impl WindowModel {
    /// Start the model: the background thread, the NSWorkspace observers and (once the
    /// background thread has enumerated the running apps, a few tens of ms later) one thread per
    /// regular app. Returns at once; `ready()` tells when the first scans are done. Call once, on
    /// the main thread, with a running (or regularly pumped) main run loop.
    pub fn start(mtm: MainThreadMarker) -> Rc<WindowModel> {
        let _ = mtm;
        let shared = Arc::new(Shared {
            st: Mutex::new(State::default()),
            cg: Mutex::new(Arc::new(CgList::default())),
            // First background pass: enumerate apps, then scan CG.
            bg: Mutex::new(BgFlags { cg: true, apps: true, ..BgFlags::default() }),
            bg_cv: Condvar::new(),
            bg_stop: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            pending_apps: Mutex::new(None),
        });
        let s2 = shared.clone();
        let _ = std::thread::Builder::new().name("ztab-model-bg".into()).stack_size(512 * 1024).spawn(move || bg_thread(s2));
        let model = Rc::new(WindowModel {
            shared,
            threads: RefCell::new(HashMap::new()),
            sink: RefCell::new(None),
            observers: RefCell::new(Vec::new()),
            poked: RefCell::new(HashMap::new()),
            asked: RefCell::new(Asks::default()),
            pids_by_wid: RefCell::new(HashMap::new()),
            snap_ms: RefCell::new(Vec::new()),
            slowest: RefCell::new((0.0, String::new())),
            slowest_handler: RefCell::new((0.0, String::new())),
            next_gen: Cell::new(1),
            started: Cell::new(false),
            own_pid: std::process::id() as i32,
        });
        MODEL.with(|m| *m.borrow_mut() = Rc::downgrade(&model));
        model.install_observers();
        model
    }

    /// The background thread has the CoreGraphics window list (from then on `snapshot()` lists
    /// every window, apps not scanned yet by the CG-only rules).
    pub fn has_window_list(&self) -> bool {
        self.shared.cg().at.is_some()
    }

    /// The first app enumeration has been applied, the CG list exists and every app thread has
    /// finished (or failed) its first scan. Before that, `snapshot()` may miss windows.
    pub fn ready(&self) -> bool {
        if !self.started.get() || self.shared.cg().at.is_none() {
            return false;
        }
        let st = self.shared.lock();
        st.active_pid.is_some() && st.apps.values().all(|a| a.ready || a.busy_since.is_some())
    }

    /// Current window list. Main thread. Makes no IPC at all (no AX, no WindowServer, no
    /// LaunchServices): it merges the model's state with the background CG list under two short
    /// locks. The CG list is refreshed on every window / focus / app event and every 5 s.
    pub fn snapshot(&self) -> Snapshot {
        let t0 = Instant::now();
        let mut marks: Vec<(&'static str, f64)> = Vec::new();
        let mut mark = |what: &'static str| marks.push((what, t0.elapsed().as_secs_f64() * 1000.0));
        let cg = self.shared.cg();
        // Copy what we need from the model.
        let (apps, known, ready, used, current_cur, active_pid, active_focus_known, last_scan, own_win) = {
            let st = self.shared.lock();
            let apps: HashMap<i32, AppMeta> = st
                .apps
                .iter()
                .map(|(p, a)| (*p, AppMeta { name: a.name.clone(), bundle_id: a.bundle_id.clone(), hidden: a.hidden }))
                .collect();
            let newer = |t: Option<Instant>| t.is_some_and(|t| cg.at.is_none_or(|at| t > at));
            let known: HashMap<u32, KnownWin> = st
                .wins
                .iter()
                .map(|(wid, w)| {
                    let app_changed = st.apps.get(&w.pid).is_some_and(|a| newer(a.hidden_changed_at));
                    (*wid, KnownWin {
                        pid: w.pid,
                        el: w.el.0.clone(),
                        title: w.title.clone(),
                        subrole: w.subrole.clone(),
                        minimized: w.minimized,
                        cg_stale: newer(w.changed_at) || app_changed,
                        added_at: w.added_at,
                    })
                })
                .collect();
            let ready: HashSet<i32> = st.apps.iter().filter(|(_, a)| a.ready).map(|(p, _)| *p).collect();
            let active = st.active_pid;
            let afk = active.and_then(|p| st.apps.get(&p)).is_some_and(|a| a.ready && a.focus_known);
            let last_scan: HashMap<i32, Instant> = st.apps.iter().filter_map(|(p, a)| a.last_scan.map(|t| (*p, t))).collect();
            (apps, known, ready, st.used.clone(), st.current.map(|c| (c.wid, c.pid)), active, afk, last_scan, st.own_win.clone())
        };
        mark("copy");

        // Candidates: windows AX reported after the CG list started (just created; frontmost),
        // then the CG list in z-order. A known window that is missing from a CG list started
        // after the model learned about it is closed (its destroy notification was missed): it
        // is left out, and its app is rescanned.
        let mut cg_dirty = false;
        let now = Instant::now();
        let mut asks = self.asked.borrow_mut();
        asks.prune(now);
        // Windows to ask their app thread about (rescan), by pid; recorded in `asks` only for
        // the pids the rescan was actually sent to (see below).
        let mut to_poke: HashMap<i32, Vec<u32>> = HashMap::new();
        let mut ask = |wid: u32, pid: i32| -> bool {
            if !asks.due(wid, last_scan.get(&pid).copied(), now) {
                return false;
            }
            to_poke.entry(pid).or_default().push(wid);
            true
        };
        let mut cands: Vec<(CgWin, (bool, Option<bool>))> = Vec::new();
        for (wid, w) in &known {
            if apps.contains_key(&w.pid) && !cg.by_wid.contains_key(wid) && !cg.others.contains(wid) {
                if cg.started.is_some_and(|s| w.added_at + CG_SLACK <= s) {
                    if ask(*wid, w.pid) {
                        cg_dirty = true;
                    }
                    continue;
                }
                let space = (!w.minimized, Some(true));
                cands.push((CgWin { wid: *wid, pid: w.pid, bounds: (0.0, 0.0, 800.0, 600.0), name: None, onscreen_rank: None, all_rank: 0 }, space));
            }
        }
        let pending = cands.len();
        for r in &cg.rows {
            if apps.contains_key(&r.pid) {
                cands.push((CgWin { wid: r.wid, pid: r.pid, bounds: r.bounds, name: r.name.clone(), onscreen_rank: r.on_rank, all_rank: r.all_rank }, r.space));
            }
        }
        // Apps the model can answer for: first scan done and at least one window known (an empty
        // AXWindows while CG shows windows means AX is broken for the app).
        let model_apps: HashSet<i32> = known.values().filter(|w| ready.contains(&w.pid)).map(|w| w.pid).collect();
        let titles_readable = cg.screen_recording;

        const OFF: usize = 1 << 40;
        let mut out: Vec<(usize, WinInfo)> = Vec::with_capacity(cands.len());
        for (i, (c, (on_cur, on_some))) in cands.into_iter().enumerate() {
            let meta: &AppMeta = &apps[&c.pid];
            let onscreen = c.onscreen_rank.is_some() || i < pending;
            let (title, minimized, on_current, ax) = if model_apps.contains(&c.pid) {
                match known.get(&c.wid) {
                    Some(w) => {
                        if let Some(s) = w.subrole.as_deref()
                            && !windows::is_window_subrole(s)
                        {
                            continue;
                        }
                        // Off screen without being minimized or hidden: only a window on another
                        // Space counts (not e.g. the background tabs of a native tab group).
                        // Unless the CG row predates a de-minimize / unhide: then it is stale.
                        if !onscreen && !w.minimized && !meta.hidden && !w.cg_stale && (on_cur || on_some != Some(true)) {
                            continue;
                        }
                        let title = w.title.clone().or_else(|| c.name.clone()).unwrap_or_default();
                        // The minimized flag comes from AX notifications, fresher than the CG list.
                        let on_current = !w.minimized && (onscreen || on_cur || w.cg_stale && !meta.hidden);
                        (title, w.minimized, on_current, Some(w.el.clone()))
                    }
                    None => {
                        if onscreen {
                            // A visible window AX has not reported (yet): ask for a rescan.
                            ask(c.wid, c.pid);
                            continue;
                        }
                        // AX lists current-Space, minimized and hidden-app windows; anything else
                        // that is real lives on another Space.
                        if on_cur || on_some != Some(true) || !windows::plausible_offscreen(&c, titles_readable) {
                            continue;
                        }
                        (c.name.clone().unwrap_or_default(), false, false, None)
                    }
                }
            } else {
                // CG-only rules (same as `windows::list_windows` without AX).
                if !onscreen && (on_some != Some(true) || (on_cur && !meta.hidden) || !windows::plausible_offscreen(&c, titles_readable)) {
                    continue;
                }
                (c.name.clone().unwrap_or_default(), false, onscreen || on_cur, None)
            };
            let key = if i < pending {
                0
            } else {
                match c.onscreen_rank {
                    Some(r) => r + 1,
                    None => OFF + c.all_rank,
                }
            };
            out.push((
                key,
                WinInfo {
                    wid: c.wid,
                    pid: c.pid,
                    app_name: meta.name.clone(),
                    bundle_id: meta.bundle_id.clone(),
                    title,
                    bounds: c.bounds,
                    minimized,
                    on_current_space: on_current,
                    hidden_app: meta.hidden,
                    z_index: 0,
                    ax,
                },
            ));
        }
        // ZTab's own Settings window: by its window id only (never the switcher panel), in the
        // CG list's z-order once the list has it, in front until then (it was just opened).
        if let Some(ow) = own_win {
            let row = cg.by_wid.get(&ow.wid).map(|&i| &cg.rows[i]);
            let (bounds, key, onscreen) = match row {
                Some(r) => (r.bounds, r.on_rank.map_or(OFF + r.all_rank, |k| k + 1), r.on_rank.is_some()),
                None => (ow.bounds, 0, !ow.minimized),
            };
            out.push((
                key,
                WinInfo {
                    wid: ow.wid,
                    pid: self.own_pid,
                    app_name: ow.app_name.clone(),
                    bundle_id: Some(OWN_BUNDLE_ID.to_string()),
                    title: ow.title.clone(),
                    bounds,
                    minimized: ow.minimized,
                    on_current_space: onscreen && !ow.minimized,
                    hidden_app: false,
                    z_index: 0,
                    ax: None,
                },
            ));
        }
        mark("merge");

        // Dense z ranks.
        out.sort_by_key(|(k, _)| *k);
        let wins: Vec<WinInfo> = out
            .into_iter()
            .enumerate()
            .map(|(i, (_, mut w))| {
                w.z_index = i;
                w
            })
            .collect();

        let current = pick_current(&wins, current_cur, &used, active_pid, active_focus_known);
        let (windows_out, current, mru) = order_mru(wins, current, &used);

        // Bookkeeping (all cheap, no waiting).
        {
            let mut p = self.pids_by_wid.borrow_mut();
            p.clear();
            p.extend(windows_out.iter().map(|w| (w.wid, w.pid)));
        }
        for (pid, wids) in to_poke {
            // A throttled poke sends nothing: those windows are asked about again next time.
            if self.poke(pid, Cmd::Rescan) {
                for wid in wids {
                    asks.sent(wid, now);
                }
            }
        }
        drop(asks);
        if cg_dirty {
            self.shared.mark_cg_dirty();
        }
        // Windows of pids the app table does not know: let the background thread re-check.
        if cg.rows.iter().any(|r| r.on_rank.is_some() && !apps.contains_key(&r.pid) && r.pid != self.own_pid) {
            self.shared.wake_bg(false, false);
        }
        mark("order");
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        if ms > self.slowest.borrow().0 || self.slowest.borrow().1.is_empty() {
            let detail = marks.iter().map(|(w, t)| format!("{w}@{t:.2}")).collect::<Vec<_>>().join(" ");
            *self.slowest.borrow_mut() = (ms, detail);
        }
        let mut v = self.snap_ms.borrow_mut();
        if v.len() >= 64 {
            v.remove(0);
        }
        v.push(ms);
        Snapshot { windows: windows_out, current, mru, active_pid }
    }

    /// ZTab just switched to `wid`: make it the current window and the top of the MRU history.
    pub fn note_focused(&self, wid: u32) {
        let mut st = self.shared.lock();
        let pid = st.wins.get(&wid).map(|w| w.pid).or_else(|| self.pids_by_wid.borrow().get(&wid).copied());
        match pid {
            Some(pid) => {
                if let Some(a) = st.apps.get_mut(&pid) {
                    a.focused = Some(wid);
                }
                st.active_pid = Some(pid);
                st.note = Some(Note { pid, wid, at: Instant::now() });
                st.set_current(Some((wid, pid)), true);
            }
            None => {
                let s = st.next_seq();
                st.used.insert(wid, s);
            }
        }
        self.shared.flush(st);
    }

    /// ZTab's own listed window appeared, changed (title, frame, minimized) or went away (`None`).
    /// A window that goes away leaves the MRU history.
    pub fn set_own_window(&self, w: Option<OwnWindow>) {
        let mut st = self.shared.lock();
        let old = st.own_win.as_ref().map(|o| o.wid);
        if let Some(old) = old
            && w.as_ref().is_none_or(|n| n.wid != old)
        {
            st.used.remove(&old);
            if st.current.is_some_and(|c| c.wid == old) {
                st.current = None;
            }
            st.own_key = false;
        }
        st.own_win = w;
        self.shared.flush(st);
    }

    /// ZTab's own window became (`true`) or stopped being the key window. While ZTab is the
    /// active app, that makes it the current window (or leaves none current).
    pub fn note_own_key(&self, key: bool) {
        let mut st = self.shared.lock();
        st.own_key = key;
        if st.active_pid == Some(self.own_pid) {
            let cur = st.own_focus().map(|w| (w, self.own_pid));
            st.set_current(cur, false);
        }
        self.shared.flush(st);
    }

    /// Pause (display asleep, screen locked, session switched away, user idle) or resume the
    /// background work nobody needs then: the app threads' safety-net rescans and the periodic
    /// CG refresh. Notifications are still followed while paused. Resuming refreshes the CG list
    /// (no AX). With `rescan` (after a lock, display sleep or session switch) it also asks every
    /// app thread for a rescan, spread over a couple of seconds and starting
    /// `RESUME_RESCAN_AFTER` later, so the burst never lands on the switch that ended the pause.
    /// Without it (an idle pause, which the user usually ends with ⌥Tab) each app's postponed
    /// safety-net scan simply runs at its own time.
    pub fn set_paused(&self, paused: bool, rescan: bool) {
        let was = self.shared.paused.swap(paused, Ordering::Relaxed);
        if was && !paused {
            if rescan {
                let ctls: Vec<Arc<AppCtl>> = self.threads.borrow().values().map(|t| t.ctl.clone()).collect();
                for (i, c) in ctls.iter().enumerate() {
                    c.send(Cmd::RescanAfter(RESUME_RESCAN_AFTER + RESUME_RESCAN_SPREAD * i as u32));
                }
            }
            self.shared.wake_bg(true, false);
        }
    }

    /// Where events go. They are delivered on the main thread, outside any model lock.
    pub fn set_event_sink(&self, sink: Box<dyn Fn(ModelEvent)>) {
        *self.sink.borrow_mut() = Some(Rc::from(sink));
    }

    /// One-line diagnostics.
    pub fn stats(&self) -> String {
        let threads = self.threads.borrow().len();
        let st = self.shared.lock();
        let now = Instant::now();
        let ready = st.apps.values().filter(|a| a.ready).count();
        let token = st.wins.values().filter(|w| !w.in_ax_list).count();
        let minimized = st.wins.values().filter(|w| w.minimized).count();
        let oldest_scan = st.apps.values().filter_map(|a| a.last_scan.map(|t| now.duration_since(t).as_secs_f64())).fold(0.0, f64::max);
        let busy: Vec<String> = st
            .apps
            .iter()
            .filter_map(|(p, a)| a.busy_since.map(|t| format!("{p}:{:.1}s/err{}", now.duration_since(t).as_secs_f64(), a.last_err.unwrap_or(0))))
            .collect();
        let not_ready: Vec<i32> = st.apps.iter().filter(|(_, a)| !a.ready).map(|(p, _)| *p).collect();
        let cur = st.current.map(|c| c.wid);
        let active = st.active_pid;
        let used = st.used.len();
        let wins = st.wins.len();
        let apps = st.apps.len();
        drop(st);
        let cg = self.shared.cg();
        let cg_age = cg.at.map(|t| now.duration_since(t).as_secs_f64()).unwrap_or(-1.0);
        let mut v = self.snap_ms.borrow().clone();
        v.sort_by(|a, b| a.total_cmp(b));
        let pct = |q: f64| if v.is_empty() { 0.0 } else { v[((v.len() - 1) as f64 * q).round() as usize] };
        format!(
            "apps={apps} ready={ready} threads={threads} not_ready={not_ready:?} windows={wins} minimized={minimized} \
             not_in_axwindows={token} active={active:?} current={cur:?} mru_entries={used} oldest_scan={oldest_scan:.1}s \
             busy={busy:?} callback_panics={} front_corrections={} cg_rows={} cg_age={cg_age:.1}s cg_ms={:.1} snapshot_ms(n={}) p50={:.2} p95={:.2} max={:.2} slowest=[{}] \
             slowest_main_handler={:.2}ms ({})",
            CALLBACK_PANICS.load(Ordering::Relaxed),
            FRONT_CORRECTIONS.load(Ordering::Relaxed),
            cg.rows.len(),
            cg.ms,
            v.len(),
            pct(0.5),
            pct(0.95),
            v.last().copied().unwrap_or(0.0),
            self.slowest.borrow().1,
            self.slowest_handler.borrow().0,
            self.slowest_handler.borrow().1
        )
    }

    /// Diagnostics: the background CG list as (wid, pid, on-screen rank, (on a visible Space, on
    /// any Space)).
    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn debug_cg_rows(&self) -> Vec<(u32, i32, Option<usize>, (bool, Option<bool>))> {
        self.shared.cg().rows.iter().map(|r| (r.wid, r.pid, r.on_rank, r.space)).collect()
    }

    // -----------------------------------------------------------------------------------------

    fn note_handler(&self, t: Instant, what: &str) {
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        if ms > self.slowest_handler.borrow().0 {
            *self.slowest_handler.borrow_mut() = (ms, what.to_string());
        }
    }

    /// Send `cmd` to the app thread of `pid`. A `Rescan` is sent at most every `POKE_EVERY`
    /// per app. Returns whether it was sent.
    fn poke(&self, pid: i32, cmd: Cmd) -> bool {
        let threads = self.threads.borrow();
        let Some(t) = threads.get(&pid) else { return false };
        if cmd == Cmd::Rescan {
            let mut p = self.poked.borrow_mut();
            if p.get(&pid).is_some_and(|t| t.elapsed() < POKE_EVERY) {
                return false;
            }
            p.insert(pid, Instant::now());
        }
        t.ctl.send(cmd);
        true
    }

    /// Apply a full list of regular apps: start threads for new ones, stop those of apps that are
    /// gone (or whose pid now belongs to another process), refresh names / hidden flags.
    /// `scanned_at`: when the list was taken (apps tracked after that are left alone).
    fn reconcile(&self, apps: &HashMap<i32, AppMeta>, scanned_at: Option<Instant>) {
        let gone: Vec<i32> = self
            .threads
            .borrow()
            .iter()
            .filter(|(pid, t)| {
                let missing = !apps.contains_key(pid) && scanned_at.is_none_or(|s| t.tracked_at < s);
                missing || proc_start(**pid).is_some_and(|s| s != t.start)
            })
            .map(|(p, _)| *p)
            .collect();
        for pid in gone {
            self.untrack(pid);
        }
        for (&pid, meta) in apps {
            self.track(pid, meta);
        }
    }

    /// Start following a regular app (no-op when it is already followed); refreshes its name.
    /// The hidden flag of a followed app is left to the hide / unhide notifications: a background
    /// enumeration may predate the latest one.
    fn track(&self, pid: i32, meta: &AppMeta) {
        if pid == self.own_pid || pid <= 0 {
            return;
        }
        if self.threads.borrow().contains_key(&pid) {
            let mut st = self.shared.lock();
            if let Some(a) = st.apps.get_mut(&pid) {
                a.name.clone_from(&meta.name);
                a.bundle_id.clone_from(&meta.bundle_id);
            }
            return;
        }
        let Some(start) = proc_start(pid) else { return };
        let epoch_ = self.next_gen.get();
        self.next_gen.set(epoch_ + 1);
        // The record must exist before the thread can report anything.
        {
            let mut st = self.shared.lock();
            if let Some(old) = st.apps.get(&pid).map(|a| a.epoch) {
                st.remove_app(pid, old);
            }
            st.apps.insert(pid, AppRec::new(epoch_, meta));
        }
        match spawn_app_thread(&self.shared, pid, epoch_) {
            Some(ctl) => {
                self.threads.borrow_mut().insert(pid, AppThread { start, ctl, tracked_at: Instant::now() });
                let mut st = self.shared.lock();
                if self.started.get() {
                    st.events.push(ModelEvent::AppLaunched { pid });
                }
                self.shared.flush(st);
            }
            None => {
                let mut st = self.shared.lock();
                st.apps.remove(&pid);
            }
        }
    }

    /// Stop following an app: stop its thread, forget its windows (Destroyed + AppTerminated).
    fn untrack(&self, pid: i32) {
        let t = self.threads.borrow_mut().remove(&pid);
        if let Some(t) = t {
            t.ctl.request_stop();
            let mut st = self.shared.lock();
            st.remove_app(pid, t.ctl.epoch);
            self.shared.flush(st);
        }
    }

    /// A full app list from the background thread is waiting (main thread).
    fn apply_pending_apps(&self) {
        let pending = self.shared.pending_apps.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some((at, apps)) = pending {
            let before = self.threads.borrow().len();
            self.reconcile(&apps, if self.started.get() { Some(at) } else { None });
            self.started.set(true);
            if self.threads.borrow().len() != before {
                // Space membership of the new apps' off-screen windows.
                self.shared.wake_bg(true, false);
            }
        }
    }

    fn install_observers(self: &Rc<Self>) {
        let ws = NSWorkspace::sharedWorkspace();
        let nc = ws.notificationCenter();
        let queue = NSOperationQueue::mainQueue();
        // SAFETY: the notification names are valid static NSStrings exported by AppKit.
        let names: [(&NSNotificationName, Ws); 6] = unsafe {
            [
                (NSWorkspaceDidLaunchApplicationNotification, Ws::Launch),
                (NSWorkspaceDidTerminateApplicationNotification, Ws::Terminate),
                (NSWorkspaceDidActivateApplicationNotification, Ws::Activate),
                (NSWorkspaceDidHideApplicationNotification, Ws::Hide),
                (NSWorkspaceDidUnhideApplicationNotification, Ws::Unhide),
                (NSWorkspaceActiveSpaceDidChangeNotification, Ws::Space),
            ]
        };
        let mut obs = self.observers.borrow_mut();
        for (name, kind) in names {
            let weak: Weak<WindowModel> = Rc::downgrade(self);
            let block = RcBlock::new(move |n: NonNull<NSNotification>| {
                // SAFETY: NSNotificationCenter passes a valid notification for the call.
                let n = unsafe { n.as_ref() };
                if let Some(m) = weak.upgrade() {
                    let t = Instant::now();
                    m.on_workspace(kind, notification_app(n));
                    m.note_handler(t, kind.name());
                }
            });
            // SAFETY: valid name, nil object, main queue (the block runs on the main thread).
            let token = unsafe { nc.addObserverForName_object_queue_usingBlock(Some(name), None, Some(&queue), &block) };
            obs.push(token);
        }
    }

    fn on_workspace(&self, kind: Ws, app: Option<Retained<NSRunningApplication>>) {
        // Reading this one app's properties costs well under a millisecond.
        let pid = app.as_ref().map(|a| a.processIdentifier());
        match (kind, pid) {
            (Ws::Launch, Some(pid)) | (Ws::Activate, Some(pid)) if !self.threads.borrow().contains_key(&pid) => {
                if let Some(meta) = app.as_deref().and_then(app_meta) {
                    self.track(pid, &meta);
                } else if kind == Ws::Launch {
                    // Not regular (yet): Electron-style apps switch policy after launch.
                    self.shared.wake_bg(false, true);
                }
            }
            _ => {}
        }
        match (kind, pid) {
            (Ws::Terminate, Some(pid)) => self.untrack(pid),
            (Ws::Activate, Some(pid)) => {
                let mut st = self.shared.lock();
                st.activate(pid);
                let grace = st.in_grace(pid).is_some();
                self.shared.flush(st);
                if !grace {
                    // The cached focused window may be stale; the app thread re-reads it.
                    self.poke(pid, Cmd::RefreshFocus);
                }
            }
            (Ws::Hide | Ws::Unhide, Some(pid)) => {
                let mut st = self.shared.lock();
                if let Some(a) = st.apps.get_mut(&pid) {
                    a.set_hidden(kind == Ws::Hide);
                }
                self.shared.flush(st);
            }
            (Ws::Space, _) => {
                self.shared.bg.lock().unwrap_or_else(|e| e.into_inner()).spaces = true;
                let ctls: Vec<Arc<AppCtl>> = self.threads.borrow().values().map(|t| t.ctl.clone()).collect();
                for c in ctls {
                    c.send(Cmd::Rescan);
                }
            }
            _ => {}
        }
        self.shared.wake_bg(true, false);
    }

    /// Deliver queued events (main thread).
    fn drain(&self) {
        let events = {
            let mut st = self.shared.lock();
            st.drain_scheduled = false;
            let evs = std::mem::take(&mut st.events);
            // A window that got minimized / hidden / closed meanwhile is no longer capturable.
            evs.into_iter()
                .filter(|e| match e {
                    ModelEvent::FocusLeft { wid } => st.on_screen_known(*wid),
                    _ => true,
                })
                .collect::<Vec<_>>()
        };
        let sink = self.sink.borrow().clone();
        if let Some(sink) = sink {
            for e in events {
                sink(e);
            }
        }
    }
}

impl Drop for WindowModel {
    fn drop(&mut self) {
        let nc = NSWorkspace::sharedWorkspace().notificationCenter();
        for t in self.observers.borrow_mut().drain(..) {
            // SAFETY: `t` is a token returned by addObserverForName on this center.
            let obj: &objc2::runtime::AnyObject = (*t).as_ref();
            unsafe { nc.removeObserver(obj) };
        }
        for (_, t) in self.threads.borrow_mut().drain() {
            t.ctl.request_stop();
        }
        self.shared.bg_stop.store(true, Ordering::Relaxed);
        self.shared.wake_bg(true, false);
    }
}

/// Windows `snapshot()` asked an app thread to rescan for: an on-screen window AX has not
/// reported, or a known window missing from the CG list. A window is recorded only when the
/// rescan was actually sent (a poke can be throttled). Once a rescan that finished after the ask
/// still did not settle it, the window is asked about once more after `POKE_EVERY` (CG can list a
/// new window before `AXWindows` does), then every `REPOKE_SAME_WINDOW`; while the rescan has not
/// finished (a busy app), it is not asked about again for `REPOKE_SAME_WINDOW`.
#[derive(Default)]
struct Asks(HashMap<u32, Ask>);

struct Ask {
    at: Instant,
    tries: u32,
}

impl Asks {
    /// Whether to ask about `wid` now. `last_scan`: when its app's last successful scan ended.
    fn due(&self, wid: u32, last_scan: Option<Instant>, now: Instant) -> bool {
        let Some(a) = self.0.get(&wid) else { return true };
        match last_scan.filter(|s| *s >= a.at) {
            None => now.saturating_duration_since(a.at) >= REPOKE_SAME_WINDOW,
            Some(scanned) => {
                let wait = if a.tries < 2 { POKE_EVERY } else { REPOKE_SAME_WINDOW };
                now.saturating_duration_since(scanned) >= wait
            }
        }
    }

    /// A rescan was sent for `wid` at `now`.
    fn sent(&mut self, wid: u32, now: Instant) {
        let tries = self.0.get(&wid).map_or(0, |a| a.tries) + 1;
        self.0.insert(wid, Ask { at: now, tries });
    }

    /// Forget windows not asked about for a while (AX reported them, or they are gone).
    fn prune(&mut self, now: Instant) {
        self.0.retain(|_, a| now.saturating_duration_since(a.at) < REPOKE_SAME_WINDOW * 2);
    }
}

/// Index of the window the user is in, among z-sorted windows (`z_index` == position).
///
/// `focused` is the model's current window: the active app's focused window, with its pid. When
/// that window is not listed (a floating panel, palette or inspector has focus), the app's most
/// recently used on-screen window stands in for it (else its topmost one): the user is visually
/// in that window, so a quick tap must go to the window before it. When the model has no focus
/// information for the active app (`active_focus_known` false), the topmost window stands in if
/// it belongs to the active app.
fn pick_current(
    wins: &[WinInfo],
    focused: Option<(u32, i32)>,
    used: &HashMap<u32, u64>,
    active_pid: Option<i32>,
    active_focus_known: bool,
) -> Option<usize> {
    if let Some((wid, pid)) = focused {
        if let Some(i) = wins.iter().position(|w| w.wid == wid) {
            return Some(i);
        }
        let stand_in = wins
            .iter()
            .enumerate()
            .filter(|(_, w)| w.pid == pid && w.on_current_space && !w.minimized)
            .max_by_key(|(_, w)| (used.get(&w.wid).copied(), std::cmp::Reverse(w.z_index)))
            .map(|(i, _)| i);
        if stand_in.is_some() {
            return stand_in;
        }
    }
    let top = wins.first()?;
    (!active_focus_known && Some(top.pid) == active_pid && top.on_current_space && !top.minimized).then_some(0)
}

/// Order z-sorted windows (`z_index` == position) for the switcher. MRU: `current` first, then by
/// last use (`used`: higher = more recent), then never-used windows by z. Windows are grouped by
/// app, apps ordered by their best MRU position. Returns (windows, current, mru) with indices
/// into the returned windows.
fn order_mru(mut wins: Vec<WinInfo>, current: Option<usize>, used: &HashMap<u32, u64>) -> (Vec<WinInfo>, Option<usize>, Vec<usize>) {
    let rank_key = |i: usize, w: &WinInfo| -> (u8, u64) {
        if Some(i) == current {
            (0, 0)
        } else if let Some(s) = used.get(&w.wid) {
            (1, u64::MAX - s)
        } else {
            (2, w.z_index as u64)
        }
    };
    let mut mru: Vec<usize> = (0..wins.len()).collect();
    mru.sort_by_key(|&i| rank_key(i, &wins[i]));
    let mut pos = vec![0usize; wins.len()];
    for (p, &i) in mru.iter().enumerate() {
        pos[i] = p;
    }
    let mut app_rank: HashMap<i32, usize> = HashMap::new();
    for (i, w) in wins.iter().enumerate() {
        let r = app_rank.entry(w.pid).or_insert(usize::MAX);
        *r = (*r).min(pos[i]);
    }
    let mut order: Vec<usize> = (0..wins.len()).collect();
    order.sort_by_key(|&i| (app_rank[&wins[i].pid], pos[i]));
    let mut new_idx = vec![0usize; wins.len()];
    for (n, &o) in order.iter().enumerate() {
        new_idx[o] = n;
    }
    let mut slots: Vec<Option<WinInfo>> = wins.drain(..).map(Some).collect();
    let out: Vec<WinInfo> = order.iter().map(|&o| slots[o].take().expect("each index once")).collect();
    let current = current.map(|c| new_idx[c]);
    let mru: Vec<usize> = mru.iter().map(|&o| new_idx[o]).collect();
    (out, current, mru)
}

thread_local! {
    /// The model, for the main-thread event drain (dispatched from background threads).
    static MODEL: RefCell<Weak<WindowModel>> = const { RefCell::new(Weak::new()) };
}

fn drain_on_main() {
    if let Some(m) = MODEL.with(|m| m.borrow().upgrade()) {
        let t = Instant::now();
        m.drain();
        m.note_handler(t, "event drain (incl. sink)");
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ws {
    Launch,
    Terminate,
    Activate,
    Hide,
    Unhide,
    Space,
}

impl Ws {
    fn name(self) -> &'static str {
        match self {
            Ws::Launch => "didLaunch",
            Ws::Terminate => "didTerminate",
            Ws::Activate => "didActivate",
            Ws::Hide => "didHide",
            Ws::Unhide => "didUnhide",
            Ws::Space => "activeSpaceDidChange",
        }
    }
}

fn notification_app(n: &NSNotification) -> Option<Retained<NSRunningApplication>> {
    // SAFETY: NSWorkspaceApplicationKey is a valid static NSString.
    let key = unsafe { NSWorkspaceApplicationKey };
    let app = n.userInfo()?.objectForKey(key)?;
    app.downcast::<NSRunningApplication>().ok()
}

/// Metadata of a regular app (None for accessory / background apps and the Dock).
fn app_meta(app: &NSRunningApplication) -> Option<AppMeta> {
    if app.activationPolicy() != NSApplicationActivationPolicy::Regular {
        return None;
    }
    let bundle_id = app.bundleIdentifier().map(|s| s.to_string());
    if bundle_id.as_deref() == Some("com.apple.dock") {
        return None;
    }
    Some(AppMeta { name: app.localizedName().map(|s| s.to_string()).unwrap_or_default(), bundle_id, hidden: app.isHidden() })
}

fn apply_apps_on_main() {
    if let Some(m) = MODEL.with(|m| m.borrow().upgrade()) {
        let t = Instant::now();
        m.apply_pending_apps();
        m.note_handler(t, "apply app list");
    }
}

fn frontmost_pid() -> Option<i32> {
    NSWorkspace::sharedWorkspace().frontmostApplication().map(|a| a.processIdentifier())
}

/// Process start time (s, us): tells a reused pid apart from the original process.
fn proc_start(pid: i32) -> Option<(u64, u64)> {
    // SAFETY: plain syscall into a properly sized, zeroed buffer.
    unsafe {
        let mut info: libc::proc_bsdinfo = std::mem::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let n = libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&mut info as *mut libc::proc_bsdinfo).cast(), size);
        (n == size).then_some((info.pbi_start_tvsec, info.pbi_start_tvusec))
    }
}

// ---------------------------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------------------------

/// An AX element that crosses threads.
struct SendEl(CFRetained<AXUIElement>);
// SAFETY: an AXUIElementRef is an immutable CF object whose retain count is atomic, and the AX
// client API may be called from any thread. The model only stores and clones the handle under
// its mutex; AX calls on it happen on the owning app thread (and in `focus_window`).
unsafe impl Send for SendEl {}
// SAFETY: see above; shared references only clone the handle.
unsafe impl Sync for SendEl {}

struct Shared {
    st: Mutex<State>,
    cg: Mutex<Arc<CgList>>,
    bg: Mutex<BgFlags>,
    bg_cv: Condvar,
    bg_stop: AtomicBool,
    /// See `WindowModel::set_paused`.
    paused: AtomicBool,
    /// Full regular-app list from the background thread, for the main thread: (taken at, apps).
    pending_apps: Mutex<Option<(Instant, HashMap<i32, AppMeta>)>>,
}

/// Work requested from the background thread.
#[derive(Default)]
struct BgFlags {
    /// The Space configuration changed: drop cached Space membership (implies a CG refresh).
    spaces: bool,
    /// Refresh the CG window list.
    cg: bool,
    /// Re-enumerate the running apps.
    apps: bool,
    /// Check the CG list for windows of unknown pids.
    check: bool,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn cg(&self) -> Arc<CgList> {
        self.cg.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn mark_cg_dirty(&self) {
        self.wake_bg(true, false);
    }

    /// Ask the background thread for a CG refresh (`cg`), an app enumeration (`apps`), or (both
    /// false) a check of the CG list for windows of untracked pids.
    fn wake_bg(&self, cg: bool, apps: bool) {
        let mut f = self.bg.lock().unwrap_or_else(|e| e.into_inner());
        f.cg |= cg;
        f.apps |= apps;
        f.check |= !cg && !apps;
        drop(f);
        self.bg_cv.notify_one();
    }

    /// Unlock and make sure queued events get delivered on the main thread.
    fn flush(&self, mut st: MutexGuard<'_, State>) {
        let schedule = !st.events.is_empty() && !st.drain_scheduled;
        if schedule {
            st.drain_scheduled = true;
        }
        drop(st);
        if schedule {
            DispatchQueue::main().exec_async(drain_on_main);
        }
    }
}

#[derive(Default)]
struct State {
    apps: HashMap<i32, AppRec>,
    wins: HashMap<u32, WinRec>,
    /// MRU sequence number of every window that has been current (settled or explicit).
    used: HashMap<u32, u64>,
    seq: u64,
    active_pid: Option<i32>,
    current: Option<Cur>,
    note: Option<Note>,
    events: Vec<ModelEvent>,
    drain_scheduled: bool,
    /// ZTab's own listed window (see [`OwnWindow`]) and whether it is the key window.
    own_win: Option<OwnWindow>,
    own_key: bool,
}

/// Bundle id reported for ZTab's own window.
const OWN_BUNDLE_ID: &str = "dev.ztab.app";

struct AppRec {
    /// Instance of the app thread that owns this record; stale threads' writes are ignored.
    epoch: u64,
    name: String,
    bundle_id: Option<String>,
    /// The first AX scan finished.
    ready: bool,
    /// Focused window as last reported (None: no focused window).
    focused: Option<u32>,
    focus_known: bool,
    hidden: bool,
    scans: u32,
    full_scans: u32,
    last_scan: Option<Instant>,
    last_err: Option<i32>,
    /// First failed AX request since the last successful scan.
    busy_since: Option<Instant>,
    /// When `hidden` last changed (see `WinRec::changed_at`).
    hidden_changed_at: Option<Instant>,
}

impl AppRec {
    fn new(epoch_: u64, meta: &AppMeta) -> Self {
        AppRec {
            epoch: epoch_,
            name: meta.name.clone(),
            bundle_id: meta.bundle_id.clone(),
            ready: false,
            focused: None,
            focus_known: false,
            hidden: meta.hidden,
            scans: 0,
            full_scans: 0,
            last_scan: None,
            last_err: None,
            busy_since: None,
            hidden_changed_at: None,
        }
    }

    fn set_hidden(&mut self, hidden: bool) {
        if self.hidden != hidden {
            self.hidden = hidden;
            self.hidden_changed_at = Some(Instant::now());
        }
    }
}

struct WinRec {
    pid: i32,
    epoch: u64,
    el: SendEl,
    title: Option<String>,
    subrole: Option<String>,
    minimized: bool,
    /// When the minimized flag last changed: until the CG list is newer than this, its on-screen
    /// / Space data for the window may be stale.
    changed_at: Option<Instant>,
    /// Listed by the app's `AXWindows` at the last scan. Windows that are not (found through the
    /// remote-token sweep) are either on another Space or ordered out, e.g. the background tabs of
    /// a native tab group; `snapshot()` keeps them only when SkyLight puts them on another Space.
    in_ax_list: bool,
    /// When the model first learned about the window (a CG list started later must contain it).
    added_at: Instant,
}

#[derive(Clone, Copy)]
struct Cur {
    wid: u32,
    pid: i32,
    since: Instant,
    seq: u64,
    /// Already in `used` (explicit `note_focused`).
    committed: bool,
}

#[derive(Clone, Copy)]
struct Note {
    pid: i32,
    wid: u32,
    at: Instant,
}

struct KnownWin {
    pid: i32,
    el: CFRetained<AXUIElement>,
    title: Option<String>,
    subrole: Option<String>,
    minimized: bool,
    /// The window's minimized flag or its app's hidden flag changed after the CG list was taken.
    cg_stale: bool,
    added_at: Instant,
}

impl State {
    fn app(&mut self, pid: i32, epoch_: u64) -> Option<&mut AppRec> {
        self.apps.get_mut(&pid).filter(|a| a.epoch == epoch_)
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn in_grace(&self, pid: i32) -> Option<u32> {
        self.note.filter(|n| n.pid == pid && n.at.elapsed() < NOTE_GRACE).map(|n| n.wid)
    }

    fn on_screen_known(&self, wid: u32) -> bool {
        self.wins.get(&wid).is_some_and(|w| !w.minimized && !self.apps.get(&w.pid).is_some_and(|a| a.hidden))
    }

    /// Make `new` (wid, pid) the current window. `explicit` (ZTab switched to it) enters it into
    /// the MRU history right away; otherwise it enters once it has been current for `SETTLE`.
    fn set_current(&mut self, new: Option<(u32, i32)>, explicit: bool) {
        self.set_current_at(new, explicit, Instant::now());
    }

    fn set_current_at(&mut self, new: Option<(u32, i32)>, explicit: bool, now: Instant) {
        if let (Some(c), Some((wid, _))) = (self.current, new)
            && c.wid == wid
        {
            if explicit {
                let seq = self.next_seq();
                self.used.insert(wid, seq);
                self.current = Some(Cur { seq, since: now, committed: true, ..c });
            }
            return;
        }
        if self.current.is_none() && new.is_none() {
            return;
        }
        if let Some(old) = self.current.take() {
            if old.committed || now.duration_since(old.since) >= SETTLE {
                self.used.insert(old.wid, old.seq);
            }
            if self.on_screen_known(old.wid) {
                self.events.push(ModelEvent::FocusLeft { wid: old.wid });
            }
        }
        if let Some((wid, pid)) = new {
            let seq = self.next_seq();
            if explicit {
                self.used.insert(wid, seq);
            }
            self.current = Some(Cur { wid, pid, since: now, seq, committed: explicit });
            if self.wins.contains_key(&wid) {
                self.events.push(ModelEvent::Focused { wid });
            }
        }
    }

    /// The app `pid` became the active app (main thread). For ZTab itself, its own window is the
    /// focused one while it is key.
    fn activate(&mut self, pid: i32) {
        self.active_pid = Some(pid);
        // ZTab itself: its key window, or the one ZTab just switched to (its key status may be
        // reported after the activation).
        let f = if pid == std::process::id() as i32 {
            self.own_focus().or_else(|| self.in_grace(pid))
        } else {
            self.apps.get(&pid).and_then(|a| a.focused)
        };
        self.set_current(f.map(|w| (w, pid)), false);
    }

    /// ZTab's own window, when it is the key window.
    fn own_focus(&self) -> Option<u32> {
        self.own_win.as_ref().filter(|_| self.own_key).map(|w| w.wid)
    }

    /// An app thread reports its focused window. `polled`: read from `AXFocusedWindow` rather
    /// than delivered by a focus notification.
    fn apply_focus(&mut self, pid: i32, epoch_: u64, focus: Option<u32>, polled: bool) {
        if polled
            && let Some(w) = self.in_grace(pid)
            && focus != Some(w)
        {
            return;
        }
        let Some(app) = self.app(pid, epoch_) else { return };
        app.focused = focus;
        app.focus_known = true;
        if self.active_pid == Some(pid) {
            self.set_current(focus.map(|w| (w, pid)), false);
        }
    }

    fn remove_win(&mut self, wid: u32) {
        if let Some(w) = self.wins.remove(&wid) {
            self.events.push(ModelEvent::Destroyed { wid });
            if let Some(a) = self.apps.get_mut(&w.pid)
                && a.focused == Some(wid)
            {
                a.focused = None;
            }
        }
        self.used.remove(&wid);
        if self.current.is_some_and(|c| c.wid == wid) {
            self.current = None;
        }
    }

    /// Forget an app instance and its windows (it terminated, or its pid was reused).
    fn remove_app(&mut self, pid: i32, epoch_: u64) {
        if self.apps.get(&pid).is_none_or(|a| a.epoch != epoch_) {
            return;
        }
        self.apps.remove(&pid);
        let wids: Vec<u32> = self.wins.iter().filter(|(_, w)| w.pid == pid && w.epoch == epoch_).map(|(w, _)| *w).collect();
        for w in wids {
            self.remove_win(w);
        }
        if self.current.is_some_and(|c| c.pid == pid) {
            self.current = None;
        }
        self.events.push(ModelEvent::AppTerminated { pid });
    }
}

// ---------------------------------------------------------------------------------------------
// Background CG list
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct CgList {
    /// When the list call started / when the list was complete.
    started: Option<Instant>,
    at: Option<Instant>,
    ms: f64,
    /// Layer-0, big-enough windows: on-screen ones front to back, then the off-screen ones in
    /// window-server order.
    rows: Vec<CgRow>,
    by_wid: HashMap<u32, usize>,
    /// Every other window id of the list (other layers, too small).
    others: HashSet<u32>,
    screen_recording: bool,
}

#[derive(Clone)]
struct CgRow {
    wid: u32,
    pid: i32,
    bounds: (f64, f64, f64, f64),
    name: Option<String>,
    /// Rank among the on-screen (alpha > 0) rows, front to back.
    on_rank: Option<usize>,
    all_rank: usize,
    /// (on a visible Space, on any Space); on-screen rows are (true, Some(true)).
    space: (bool, Option<bool>),
}

/// The background CG snapshot: the full window list, the on-screen order and, for off-screen
/// windows of `tracked` apps, Space membership (SkyLight, cached per window in `spaces`).
fn cg_scan_all(spaces: &mut SpaceCache, tracked: Option<&HashSet<i32>>, screen_recording: bool) -> CgList {
    let t = Instant::now();
    let all = windows::cg_window_list(CGWindowListOption::OptionAll | CGWindowListOption::ExcludeDesktopElements);
    let on_ids = windows::cg_window_ids(CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements);
    let mut parsed: HashMap<u32, (i32, i64, (f64, f64, f64, f64), f64, Option<String>, usize)> = HashMap::new();
    for (i, d) in all.iter().enumerate() {
        if let Some((wid, pid, layer, b, alpha, name)) = windows::parse_cg(d) {
            parsed.entry(wid).or_insert((pid, layer, b, alpha, name, i));
        }
    }
    // On-screen windows that appeared between the two calls.
    let late: Vec<u32> = on_ids.iter().copied().filter(|w| !parsed.contains_key(w)).collect();
    for d in windows::cg_describe(&late) {
        if let Some((wid, pid, layer, b, alpha, name)) = windows::parse_cg(&d) {
            parsed.entry(wid).or_insert((pid, layer, b, alpha, name, usize::MAX / 4));
        }
    }
    let candidate = |p: &(i32, i64, (f64, f64, f64, f64), f64, Option<String>, usize)| {
        p.1 == 0 && p.2.2 >= windows::MIN_SIZE && p.2.3 >= windows::MIN_SIZE
    };
    let mut rows = Vec::new();
    let mut by_wid = HashMap::new();
    for wid in &on_ids {
        if let Some(p) = parsed.get(wid)
            && candidate(p)
            && p.3 > 0.0
            && !by_wid.contains_key(wid)
        {
            by_wid.insert(*wid, rows.len());
            let rank = rows.len();
            rows.push(CgRow { wid: *wid, pid: p.0, bounds: p.2, name: p.4.clone(), on_rank: Some(rank), all_rank: p.5, space: (true, Some(true)) });
        }
    }
    let mut off: Vec<(u32, &(i32, i64, (f64, f64, f64, f64), f64, Option<String>, usize))> =
        parsed.iter().filter(|(w, p)| candidate(p) && !by_wid.contains_key(*w)).map(|(w, p)| (*w, p)).collect();
    off.sort_by_key(|(_, p)| p.5);
    spaces.begin(&parsed);
    for (wid, p) in off {
        let space = if tracked.is_none_or(|t| t.contains(&p.0)) { spaces.info(wid) } else { (false, None) };
        by_wid.insert(wid, rows.len());
        rows.push(CgRow { wid, pid: p.0, bounds: p.2, name: p.4.clone(), on_rank: None, all_rank: p.5, space });
    }
    let others: HashSet<u32> = parsed.keys().copied().filter(|w| !by_wid.contains_key(w)).collect();
    let at = Instant::now();
    CgList { started: Some(t), at: Some(at), ms: (at - t).as_secs_f64() * 1000.0, rows, by_wid, others, screen_recording }
}

/// State of the background thread between iterations.
struct Bg {
    /// Pids that own windows but were not regular at the last enumeration, and when.
    non_regular: HashMap<i32, Instant>,
    spaces: SpaceCache,
    /// Frontmost app per NSWorkspace differing from the model's active app: (pid, since).
    front_mismatch: Option<(i32, Instant)>,
    own: i32,
    /// Last app enumeration, and whether a launch asked for one that is waiting for `ENUM_MIN_GAP`.
    last_enum: Option<Instant>,
    enum_deferred: bool,
    /// Screen Recording flag and when it was read.
    screen_rec: Option<(bool, Instant)>,
}

impl Bg {
    fn screen_recording(&mut self) -> bool {
        match self.screen_rec {
            Some((true, _)) => true,
            Some((false, t)) if t.elapsed() < SCREEN_REC_RECHECK => false,
            _ => {
                let v = windows::screen_recording_granted();
                self.screen_rec = Some((v, Instant::now()));
                v
            }
        }
    }

    /// How long until a deferred enumeration may run (None: none deferred).
    fn enum_wait(&self) -> Option<Duration> {
        self.enum_deferred.then(|| self.last_enum.map_or(Duration::ZERO, |t| ENUM_MIN_GAP.saturating_sub(t.elapsed())))
    }

    /// Enumerate the running apps and hand the list to the main thread.
    fn enumerate(&mut self, shared: &Shared) -> HashMap<i32, AppMeta> {
        let at = Instant::now();
        self.last_enum = Some(at);
        self.enum_deferred = false;
        let apps = windows::regular_apps();
        *shared.pending_apps.lock().unwrap_or_else(|e| e.into_inner()) = Some((at, apps.clone()));
        DispatchQueue::main().exec_async(apply_apps_on_main);
        apps
    }

    /// One pass: app enumeration, CG refresh, untracked-pid check, frontmost-app correction.
    fn iteration(&mut self, shared: &Shared, flags: BgFlags, period: Duration) {
        if flags.spaces {
            self.spaces.invalidate();
        }
        // Periodic refresh, also when frequent other wake-ups keep the wait from timing out.
        let periodic = shared.cg().at.is_none_or(|t| t.elapsed() >= period);
        let mut enumerated: Option<HashSet<i32>> = None;
        if flags.apps || self.enum_deferred {
            if self.last_enum.is_some_and(|t| t.elapsed() < ENUM_MIN_GAP) {
                self.enum_deferred = true;
            } else {
                enumerated = Some(self.enumerate(shared).keys().copied().collect());
            }
        }
        let mut list = None;
        if flags.cg || flags.spaces || periodic {
            if enumerated.is_none() {
                std::thread::sleep(CG_COALESCE);
            }
            let mut tracked: HashSet<i32> = shared.lock().apps.keys().copied().collect();
            tracked.extend(enumerated.iter().flatten());
            let rec = self.screen_recording();
            let l = Arc::new(cg_scan_all(&mut self.spaces, Some(&tracked), rec));
            *shared.cg.lock().unwrap_or_else(|e| e.into_inner()) = l.clone();
            list = Some(l);
        }
        if let (Some(apps), Some(list)) = (&enumerated, &list) {
            let now = Instant::now();
            for r in &list.rows {
                if !apps.contains(&r.pid) {
                    self.non_regular.insert(r.pid, now);
                }
            }
        }
        let mut enumerate = false;
        if enumerated.is_none() && !self.enum_deferred {
            // On-screen windows of pids the app table does not know: an app may have turned
            // regular after launch (no notification for that).
            let list = list.unwrap_or_else(|| shared.cg());
            let tracked: HashSet<i32> = shared.lock().apps.keys().copied().collect();
            let now = Instant::now();
            enumerate = list.rows.iter().any(|r| {
                r.on_rank.is_some()
                    && r.pid != self.own
                    && !tracked.contains(&r.pid)
                    && self.non_regular.get(&r.pid).is_none_or(|t| now.duration_since(*t) > NON_REGULAR_RECHECK)
            });
        }
        if enumerate {
            let apps = self.enumerate(shared);
            let at = self.last_enum.unwrap_or_else(Instant::now);
            for r in shared.cg().rows.iter() {
                if !apps.contains_key(&r.pid) {
                    self.non_regular.insert(r.pid, at);
                }
            }
            self.non_regular.retain(|_, t| at.duration_since(*t) < NON_REGULAR_RECHECK * 2);
        }
        // Safety net for a missed activation (or a ZTab switch that did not land): correct the
        // active app when NSWorkspace disagrees for a while and no switch is in progress.
        if let Some(front) = frontmost_pid() {
            let (active, grace) = {
                let st = shared.lock();
                (st.active_pid, st.note.is_some_and(|n| n.at.elapsed() < NOTE_GRACE))
            };
            if active.is_none() {
                // Start-up: take NSWorkspace's frontmost app as the active one.
                let mut st = shared.lock();
                if st.active_pid.is_none() {
                    st.activate(front);
                }
                shared.flush(st);
            } else if active == Some(front) || grace {
                self.front_mismatch = None;
            } else {
                match self.front_mismatch {
                    Some((p, since)) if p == front && since.elapsed() >= FRONT_MISMATCH => {
                        self.front_mismatch = None;
                        let mut st = shared.lock();
                        if st.active_pid != Some(front) {
                            st.activate(front);
                            FRONT_CORRECTIONS.fetch_add(1, Ordering::Relaxed);
                        }
                        shared.flush(st);
                    }
                    Some((p, _)) if p == front => {}
                    _ => self.front_mismatch = Some((front, Instant::now())),
                }
            }
        }
    }
}

fn bg_thread(shared: Arc<Shared>) {
    let mut bg = Bg {
        non_regular: HashMap::new(),
        spaces: SpaceCache::default(),
        front_mismatch: None,
        own: std::process::id() as i32,
        last_enum: None,
        enum_deferred: false,
        screen_rec: None,
    };
    loop {
        let period = if shared.paused.load(Ordering::Relaxed) { CG_REFRESH_PAUSED } else { CG_REFRESH_EVERY };
        let timeout = bg.enum_wait().map_or(period, |w| w.min(period));
        let flags = {
            let mut f = shared.bg.lock().unwrap_or_else(|e| e.into_inner());
            if !(f.cg || f.apps || f.check || f.spaces) && !timeout.is_zero() {
                f = match shared.bg_cv.wait_timeout(f, timeout) {
                    Ok((g, _)) => g,
                    Err(e) => e.into_inner().0,
                };
            }
            std::mem::take(&mut *f)
        };
        if shared.bg_stop.load(Ordering::Relaxed) {
            return;
        }
        // NSWorkspace / NSRunningApplication calls autorelease: drain once per pass (this thread
        // never ends, so its implicit pool would never be drained).
        objc2::rc::autoreleasepool(|_| bg.iteration(&shared, flags, period));
    }
}

// ---------------------------------------------------------------------------------------------
// Space membership cache (background thread)
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct SpaceCache {
    current: Option<(Option<HashSet<i64>>, Instant)>,
    per_win: HashMap<u32, (Option<Vec<i64>>, Instant)>,
}

impl SpaceCache {
    fn invalidate(&mut self) {
        self.current = None;
        self.per_win.clear();
    }

    /// Drop entries of windows that no longer exist; refresh the visible-Space set if old.
    fn begin<T>(&mut self, exists: &HashMap<u32, T>) {
        self.per_win.retain(|w, _| exists.contains_key(w));
        let now = Instant::now();
        if self.current.as_ref().is_none_or(|(_, t)| now.duration_since(*t) > CURRENT_SPACES_TTL) {
            self.current = Some((windows::main_cid().and_then(windows::current_space_ids), now));
        }
    }

    /// (on a visible Space, on any Space) of an off-screen window.
    fn info(&mut self, wid: u32) -> (bool, Option<bool>) {
        let now = Instant::now();
        let spaces = match self.per_win.get(&wid) {
            Some((s, t)) if now.duration_since(*t) < WIN_SPACES_TTL => s.clone(),
            _ => {
                let s = windows::main_cid().and_then(|cid| windows::spaces_for_window(cid, wid));
                self.per_win.insert(wid, (s.clone(), now));
                s
            }
        };
        let cur = self.current.as_ref().and_then(|(c, _)| c.as_ref());
        let on_current = match (&spaces, cur) {
            (Some(sp), Some(cur)) => sp.iter().any(|s| cur.contains(s)),
            _ => false,
        };
        (on_current, spaces.map(|s| !s.is_empty()))
    }
}

// ---------------------------------------------------------------------------------------------
// Per-app threads
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Cmd {
    Rescan,
    /// Rescan after this delay (unless one is due sooner).
    RescanAfter(Duration),
    RefreshFocus,
}

struct AppThread {
    start: (u64, u64),
    ctl: Arc<AppCtl>,
    tracked_at: Instant,
}

/// The run loop of an app thread, used from the main thread to wake it.
struct SendRl(CFRetained<CFRunLoop>);
// SAFETY: other threads only call CFRunLoopPerformBlock and CFRunLoopWakeUp through it, which
// CoreFoundation documents as thread-safe; the run loop itself stays owned by its thread.
unsafe impl Send for SendRl {}

struct AppCtl {
    pid: i32,
    epoch: u64,
    stop: AtomicBool,
    cmds: Mutex<Vec<Cmd>>,
    rl: Mutex<Option<SendRl>>,
}

impl AppCtl {
    fn send(&self, cmd: Cmd) {
        {
            let mut c = self.cmds.lock().unwrap_or_else(|e| e.into_inner());
            if !c.contains(&cmd) {
                c.push(cmd);
            }
        }
        self.wake();
    }

    fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.wake();
    }

    /// Make the thread leave its current `CFRunLoopRunInMode` and look at its flags. A block is
    /// queued (rather than calling CFRunLoopStop directly) so a wake-up that arrives while the
    /// thread is between two runs is not lost.
    fn wake(&self) {
        let rl = self.rl.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(rl) = rl.as_ref() {
            let block = RcBlock::new(|| {
                if let Some(rl) = CFRunLoop::current() {
                    rl.stop();
                }
            });
            // SAFETY: kCFRunLoopDefaultMode is a valid mode; the block captures nothing.
            unsafe {
                let mode = kCFRunLoopDefaultMode.map(|m| &**m);
                rl.0.perform_block(mode, Some(&block));
            }
            rl.0.wake_up();
        }
    }
}

fn spawn_app_thread(shared: &Arc<Shared>, pid: i32, epoch_: u64) -> Option<Arc<AppCtl>> {
    let ctl = Arc::new(AppCtl { pid, epoch: epoch_, stop: AtomicBool::new(false), cmds: Mutex::new(Vec::new()), rl: Mutex::new(None) });
    let (s2, c2) = (shared.clone(), ctl.clone());
    std::thread::Builder::new()
        .name(format!("ztab-ax-{pid}"))
        .stack_size(512 * 1024)
        .spawn(move || app_thread(s2, c2))
        .ok()?;
    Some(ctl)
}

thread_local! {
    /// The app context of the current app thread (for the AX observer callback).
    static CTX: RefCell<Option<Rc<AppCtx>>> = const { RefCell::new(None) };
}

fn app_thread(shared: Arc<Shared>, ctl: Arc<AppCtl>) {
    let (pid, epoch_) = (ctl.pid, ctl.epoch);
    let Some(rl) = CFRunLoop::current() else { return };
    // SAFETY: kCFRunLoopDefaultMode is a valid static.
    let mode = unsafe { kCFRunLoopDefaultMode };
    *ctl.rl.lock().unwrap_or_else(|e| e.into_inner()) = Some(SendRl(rl.clone()));
    // SAFETY: plain AX constructor.
    let app = unsafe { AXUIElement::new_application(pid) };
    unsafe { app.set_messaging_timeout(AX_TIMEOUT_S) };

    let mut backoff = RETRY_MIN;
    let observer = loop {
        if ctl.stop.load(Ordering::Relaxed) {
            *ctl.rl.lock().unwrap_or_else(|e| e.into_inner()) = None;
            return;
        }
        let mut raw: *mut AXObserver = std::ptr::null_mut();
        // SAFETY: valid out pointer; the callback has the AXObserverCallback signature.
        let err = unsafe { AXObserver::create(pid, Some(ax_callback), NonNull::from(&mut raw)) };
        if err == AXError::Success
            && let Some(p) = NonNull::new(raw)
        {
            // SAFETY: AXObserverCreate returns a +1 reference.
            break unsafe { CFRetained::from_raw(p) };
        }
        {
            let mut st = shared.lock();
            if let Some(a) = st.app(pid, epoch_) {
                a.last_err = Some(err.0);
                a.busy_since.get_or_insert_with(Instant::now);
            }
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(RESCAN_EVERY);
    };
    // SAFETY: the observer is valid; its source is added to this thread's run loop only.
    let src = unsafe { observer.run_loop_source() };
    rl.add_source(Some(&src), mode);

    let ctx = Rc::new(AppCtx {
        pid,
        epoch: epoch_,
        shared,
        ctl: ctl.clone(),
        app,
        observer,
        keys: windows::ax_win_keys(),
        local: RefCell::new(Local::default()),
    });
    CTX.with(|c| *c.borrow_mut() = Some(ctx.clone()));
    ctx.run();
    CTX.with(|c| c.borrow_mut().take());
    rl.remove_source(Some(&src), mode);
    *ctl.rl.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// AX observer callback; runs on the app thread that owns the observer.
unsafe extern "C-unwind" fn ax_callback(
    _observer: NonNull<AXObserver>,
    element: NonNull<AXUIElement>,
    name: NonNull<CFString>,
    refcon: *mut c_void,
) {
    // A panic must not unwind into the AX run-loop source; it is counted (see `stats()`).
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Some(ctx) = CTX.with(|c| c.borrow().clone()) else { return };
        // SAFETY: AX passes a valid element and name for the duration of the callback.
        let el = unsafe { CFRetained::retain(element) };
        let name = unsafe { name.as_ref() }.to_string();
        ctx.on_notification(&el, &name, refcon as usize as u32);
    }));
    if r.is_err() {
        CALLBACK_PANICS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Times the background thread corrected the active app from NSWorkspace (diagnostics).
static FRONT_CORRECTIONS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Panics caught in AX callbacks (diagnostics).
static CALLBACK_PANICS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[derive(Default)]
struct Local {
    /// Cached AX element of every known window of the app.
    els: HashMap<u32, CFRetained<AXUIElement>>,
    /// Windows whose notification registration failed (retried on the next scan).
    unsubscribed: HashSet<u32>,
    /// Windows that do not support some notification: their attributes are read at every scan.
    poll_only: HashSet<u32>,
    app_subscribed: bool,
    /// Some app-level notification is unsupported (window creation may go unnoticed).
    app_poll: bool,
    /// Window ids the token sweep did not find, and when.
    neg: HashMap<u32, Instant>,
    next_scan: Option<Instant>,
    /// `next_scan` is the periodic safety net (postponed while the model is paused), not a scan
    /// something asked for.
    next_is_safety: bool,
    /// A scan has succeeded.
    scanned: bool,
    last_full: Option<Instant>,
    fails: u32,
}

impl Local {
    /// Every notification is registered: the periodic rescan is only a safety net.
    fn healthy(&self) -> bool {
        self.app_subscribed && !self.app_poll && self.unsubscribed.is_empty() && self.poll_only.is_empty()
    }
}

struct AppCtx {
    pid: i32,
    epoch: u64,
    shared: Arc<Shared>,
    ctl: Arc<AppCtl>,
    app: CFRetained<AXUIElement>,
    observer: CFRetained<AXObserver>,
    keys: CFRetained<CFArray>,
    local: RefCell<Local>,
}

/// One window read during a scan: (wid, element, attributes, newly found, listed by AXWindows).
type Found = (u32, CFRetained<AXUIElement>, AxAttrs, bool, bool);

impl AppCtx {
    fn stopping(&self) -> bool {
        self.ctl.stop.load(Ordering::Relaxed)
    }

    fn run(&self) {
        // One autorelease pool per turn: this thread lives as long as the app, so its implicit
        // pool would never be drained.
        while objc2::rc::autoreleasepool(|_| self.turn()) {}
    }

    /// Commands, a due scan, then the run loop (AX notifications) until the next scan is due or
    /// a command arrives. Returns false when the thread must stop.
    fn turn(&self) -> bool {
        // SAFETY: kCFRunLoopDefaultMode is a valid static.
        let mode = unsafe { kCFRunLoopDefaultMode };
        if self.stopping() {
            return false;
        }
        let cmds = std::mem::take(&mut *self.ctl.cmds.lock().unwrap_or_else(|e| e.into_inner()));
        for c in cmds {
            match c {
                Cmd::Rescan => {
                    let mut l = self.local.borrow_mut();
                    l.next_scan = Some(Instant::now());
                    l.next_is_safety = false;
                }
                Cmd::RescanAfter(d) => self.schedule_scan(d),
                Cmd::RefreshFocus => self.refresh_focus(),
            }
        }
        if !self.local.borrow().app_subscribed {
            self.subscribe_app();
        }
        let now = Instant::now();
        let (due, deferrable) = {
            let l = self.local.borrow();
            (l.next_scan.is_none_or(|t| now >= t), l.next_is_safety && l.scanned)
        };
        if due && !self.stopping() {
            if deferrable && self.shared.paused.load(Ordering::Relaxed) {
                // Display asleep / screen locked: nobody needs the safety net now (resuming asks
                // every app for a rescan).
                self.local.borrow_mut().next_scan = Some(now + RESCAN_SAFETY);
            } else {
                self.scan();
            }
        }
        if self.stopping() {
            return false;
        }
        let wait = self
            .local
            .borrow()
            .next_scan
            .map(|t| t.saturating_duration_since(Instant::now()))
            .unwrap_or(RESCAN_EVERY)
            .clamp(Duration::from_millis(10), RESCAN_SAFETY);
        CFRunLoop::run_in_mode(mode, wait.as_secs_f64(), false);
        true
    }

    fn subscribe_app(&self) {
        let (mut ok, mut unsupported) = (true, false);
        for n in APP_NOTIFICATIONS {
            // SAFETY: valid observer / element / name; refcon 0 marks app-level notifications.
            let e = unsafe { self.observer.add_notification(&self.app, &CFString::from_static_str(n), std::ptr::null_mut()) };
            match e {
                AXError::Success | AXError::NotificationAlreadyRegistered => {}
                AXError::NotificationUnsupported => unsupported = true,
                _ => ok = false,
            }
        }
        let mut l = self.local.borrow_mut();
        l.app_subscribed = ok;
        l.app_poll = unsupported;
    }

    fn subscribe_window(&self, el: &AXUIElement, wid: u32) {
        let (mut ok, mut unsupported) = (true, false);
        for n in WINDOW_NOTIFICATIONS {
            // SAFETY: valid observer / element / name; the refcon carries the window id.
            let e = unsafe { self.observer.add_notification(el, &CFString::from_static_str(n), wid as usize as *mut c_void) };
            match e {
                AXError::Success | AXError::NotificationAlreadyRegistered => {}
                AXError::NotificationUnsupported => unsupported = true,
                _ => ok = false,
            }
        }
        let mut l = self.local.borrow_mut();
        if ok {
            l.unsubscribed.remove(&wid);
        } else {
            l.unsubscribed.insert(wid);
        }
        if unsupported {
            l.poll_only.insert(wid);
        } else {
            l.poll_only.remove(&wid);
        }
    }

    fn schedule_scan(&self, after: Duration) {
        let t = Instant::now() + after;
        let mut l = self.local.borrow_mut();
        if l.next_scan.is_none_or(|n| n > t) {
            l.next_scan = Some(t);
            l.next_is_safety = false;
        }
    }

    fn on_notification(&self, el: &CFRetained<AXUIElement>, name: &str, wid_ref: u32) {
        match name {
            "AXWindowCreated" => {
                self.add_window(el.clone(), false);
                self.shared.mark_cg_dirty();
            }
            "AXFocusedWindowChanged" => match windows::ax_window_id(el) {
                Some(wid) => {
                    if !self.local.borrow().els.contains_key(&wid) {
                        self.add_window(el.clone(), false);
                    }
                    self.report_focus(Some(wid), false);
                }
                None => self.refresh_focus(),
            },
            "AXMainWindowChanged" | "AXApplicationActivated" => self.refresh_focus(),
            "AXApplicationHidden" | "AXApplicationShown" => {
                let mut st = self.shared.lock();
                if let Some(a) = st.app(self.pid, self.epoch) {
                    a.set_hidden(name == "AXApplicationHidden");
                }
                drop(st);
                self.shared.mark_cg_dirty();
            }
            "AXUIElementDestroyed" => {
                let wid = if wid_ref != 0 {
                    Some(wid_ref)
                } else {
                    self.local.borrow().els.iter().find(|(_, e)| ***e == **el).map(|(w, _)| *w)
                };
                if let Some(wid) = wid {
                    {
                        let mut l = self.local.borrow_mut();
                        l.els.remove(&wid);
                        l.unsubscribed.remove(&wid);
                        l.poll_only.remove(&wid);
                    }
                    let mut st = self.shared.lock();
                    if st.wins.get(&wid).is_some_and(|w| w.epoch == self.epoch) {
                        st.remove_win(wid);
                    }
                    self.shared.flush(st);
                    self.shared.mark_cg_dirty();
                }
            }
            "AXWindowMiniaturized" | "AXWindowDeminiaturized" => {
                let min = name == "AXWindowMiniaturized";
                let mut st = self.shared.lock();
                let s = &mut *st;
                if let Some(w) = s.wins.get_mut(&wid_ref)
                    && w.epoch == self.epoch
                    && w.minimized != min
                {
                    w.minimized = min;
                    w.changed_at = Some(Instant::now());
                    if min {
                        s.events.push(ModelEvent::Minimized { wid: wid_ref });
                    }
                }
                self.shared.flush(st);
                self.shared.mark_cg_dirty();
            }
            "AXTitleChanged" => {
                let el = self.local.borrow().els.get(&wid_ref).cloned();
                if let Some(el) = el
                    && let Ok(v) = windows::ax_attr_res(&el, "AXTitle")
                    && let Ok(s) = v.downcast::<CFString>()
                {
                    let mut st = self.shared.lock();
                    if let Some(w) = st.wins.get_mut(&wid_ref)
                        && w.epoch == self.epoch
                    {
                        w.title = Some(s.to_string());
                    }
                }
            }
            _ => {}
        }
    }

    /// Track a window the app reported (created / focused / found by token). Returns its id.
    fn add_window(&self, el: CFRetained<AXUIElement>, via_token: bool) -> Option<u32> {
        let wid = windows::ax_window_id(&el)?;
        if self.local.borrow().els.contains_key(&wid) {
            return Some(wid);
        }
        // SAFETY: plain AX setter on a valid element.
        unsafe { el.set_messaging_timeout(AX_TIMEOUT_S) };
        let attrs = match windows::read_ax_attrs(&el, &self.keys) {
            Ok(a) => a,
            Err(e) => {
                if e == AXError::CannotComplete {
                    self.schedule_scan(Duration::from_millis(500));
                }
                return None;
            }
        };
        if attrs.role.as_deref().is_some_and(|r| r != "AXWindow") {
            return None;
        }
        self.subscribe_window(&el, wid);
        self.local.borrow_mut().els.insert(wid, el.clone());
        let mut st = self.shared.lock();
        st.app(self.pid, self.epoch)?;
        st.wins.entry(wid).or_insert(WinRec {
            pid: self.pid,
            epoch: self.epoch,
            el: SendEl(el),
            title: attrs.title,
            subrole: attrs.subrole,
            minimized: attrs.minimized.unwrap_or(false),
            changed_at: None,
            in_ax_list: !via_token,
            added_at: Instant::now(),
        });
        self.shared.flush(st);
        Some(wid)
    }

    /// `AXFocusedWindow` of the app: Some(Some(wid, el)), Some(None) for "no focused window",
    /// None when the app did not answer.
    fn read_focus(&self) -> Option<Option<(u32, CFRetained<AXUIElement>)>> {
        match windows::ax_attr_res(&self.app, "AXFocusedWindow") {
            Ok(v) => Some(v.downcast::<AXUIElement>().ok().and_then(|w| windows::ax_window_id(&w).map(|wid| (wid, w)))),
            Err(AXError::NoValue) | Err(AXError::AttributeUnsupported) => Some(None),
            Err(_) => None,
        }
    }

    fn refresh_focus(&self) {
        let Some(f) = self.read_focus() else { return };
        if let Some((wid, el)) = &f
            && !self.local.borrow().els.contains_key(wid)
        {
            self.add_window(el.clone(), false);
        }
        self.report_focus(f.map(|(w, _)| w), true);
    }

    fn report_focus(&self, focus: Option<u32>, polled: bool) {
        let mut st = self.shared.lock();
        st.apply_focus(self.pid, self.epoch, focus, polled);
        self.shared.flush(st);
        // Focus changes reorder windows (CG z-order) and can swap tabs of a tab group.
        self.shared.mark_cg_dirty();
        self.schedule_scan(FOCUS_RESCAN_AFTER);
    }

    fn scan_failed(&self, e: AXError) {
        {
            let mut l = self.local.borrow_mut();
            l.fails = (l.fails + 1).min(8);
            let b = (RETRY_MIN * (1u32 << (l.fails - 1))).min(RESCAN_EVERY);
            l.next_scan = Some(Instant::now() + b);
            // Retries of an app that was scanned before wait while the model is paused.
            l.next_is_safety = l.scanned;
        }
        let mut st = self.shared.lock();
        if let Some(a) = st.app(self.pid, self.epoch) {
            a.last_err = Some(e.0);
            a.busy_since.get_or_insert_with(Instant::now);
        }
    }

    /// AX scan. Always: `AXWindows`, the focused window, and the remote-token sweep for this
    /// app's off-screen CG windows AX did not list (other Spaces). A full scan (the first one,
    /// retries after a failure, and every `FULL_SCAN_EVERY`) also reads the attributes of every
    /// known window; other scans read only new windows and windows without notifications, since
    /// notifications keep titles and the minimized flag current. A known window that `AXWindows`
    /// does not list and that is missing from a CG list started after the model learned about
    /// it is gone, without an AX request. Aborts on the first timeout and keeps the previous
    /// state.
    fn scan(&self) {
        let full = {
            let l = self.local.borrow();
            !l.scanned || l.fails > 0 || l.last_full.is_none_or(|t| t.elapsed() >= FULL_SCAN_EVERY)
        };
        let list = match windows::ax_window_list(&self.app) {
            Ok(l) => l,
            Err(e) => {
                self.scan_failed(e);
                return;
            }
        };
        let (known, poll): (HashMap<u32, CFRetained<AXUIElement>>, HashSet<u32>) = {
            let l = self.local.borrow();
            (l.els.clone(), l.unsubscribed.union(&l.poll_only).copied().collect())
        };
        let added: HashMap<u32, Instant> = {
            let st = self.shared.lock();
            known.keys().filter_map(|w| st.wins.get(w).filter(|r| r.epoch == self.epoch).map(|r| (*w, r.added_at))).collect()
        };
        let cg = self.shared.cg();
        let mut found: Vec<Found> = Vec::new();
        // Listed / unlisted known windows whose attributes were not read this time.
        let mut kept_listed: Vec<u32> = Vec::new();
        let mut kept_unlisted: Vec<u32> = Vec::new();
        let mut listed: HashSet<u32> = HashSet::new();
        for (wid, el) in list {
            listed.insert(wid);
            let (el, new) = match known.get(&wid) {
                Some(k) => (k.clone(), false),
                None => {
                    // SAFETY: plain AX setter on a valid element.
                    unsafe { el.set_messaging_timeout(AX_TIMEOUT_S) };
                    (el, true)
                }
            };
            if !(full || new || poll.contains(&wid)) {
                kept_listed.push(wid);
                continue;
            }
            match windows::read_ax_attrs(&el, &self.keys) {
                Ok(a) => {
                    if a.role.as_deref().is_none_or(|r| r == "AXWindow") {
                        found.push((wid, el, a, new, true));
                    }
                }
                Err(AXError::CannotComplete) => return self.scan_failed(AXError::CannotComplete),
                Err(_) => {}
            }
            if self.stopping() {
                return;
            }
        }
        // Known windows AX did not list: alive on another Space (or an inactive tab), or gone.
        let mut gone: Vec<u32> = Vec::new();
        for (wid, el) in &known {
            if listed.contains(wid) {
                continue;
            }
            let in_cg = cg.by_wid.contains_key(wid) || cg.others.contains(wid);
            let cg_newer = match (cg.started, added.get(wid)) {
                (Some(s), Some(a)) => *a + CG_SLACK <= s,
                _ => false,
            };
            if !in_cg && cg_newer {
                gone.push(*wid);
                continue;
            }
            if in_cg && !full && !poll.contains(wid) {
                kept_unlisted.push(*wid);
                continue;
            }
            match windows::read_ax_attrs(el, &self.keys) {
                Ok(a) => found.push((*wid, el.clone(), a, false, false)),
                Err(AXError::InvalidUIElement) => gone.push(*wid),
                Err(AXError::CannotComplete) => return self.scan_failed(AXError::CannotComplete),
                Err(_) => {}
            }
        }
        // Remote-token sweep for this app's other-Space CG windows nobody knows yet.
        let now = Instant::now();
        let mut wanted: HashSet<u32> = {
            let l = self.local.borrow();
            cg.rows
                .iter()
                .filter(|r| {
                    // Only windows SkyLight puts on another Space (AXWindows already lists
                    // current-Space, minimized and hidden windows).
                    r.pid == self.pid
                        && r.on_rank.is_none()
                        && r.space == (false, Some(true))
                        && !listed.contains(&r.wid)
                        && !known.contains_key(&r.wid)
                        && l.neg.get(&r.wid).is_none_or(|t| now.duration_since(*t) > TOKEN_NEG_TTL)
                })
                .map(|r| r.wid)
                .collect()
        };
        if !wanted.is_empty() {
            for (wid, el) in windows::token_sweep(self.pid, &mut wanted, TOKEN_BUDGET) {
                // SAFETY: plain AX setter on a valid element.
                unsafe { el.set_messaging_timeout(AX_TIMEOUT_S) };
                match windows::read_ax_attrs(&el, &self.keys) {
                    Ok(a) if a.role.as_deref().is_none_or(|r| r == "AXWindow") => found.push((wid, el, a, true, false)),
                    _ => {
                        wanted.insert(wid);
                    }
                }
            }
            let mut l = self.local.borrow_mut();
            for w in wanted {
                l.neg.insert(w, now);
            }
            l.neg.retain(|_, t| now.duration_since(*t) < TOKEN_NEG_TTL * 2);
        }
        let focus = self.read_focus();
        // Register notifications for new windows (and retry failed registrations).
        let retry: HashSet<u32> = self.local.borrow().unsubscribed.clone();
        for (wid, el, _, new, _) in &found {
            if *new || retry.contains(wid) {
                self.subscribe_window(el, *wid);
            }
        }
        {
            let mut l = self.local.borrow_mut();
            for (wid, el, ..) in &found {
                l.els.insert(*wid, el.clone());
            }
            for w in &gone {
                l.els.remove(w);
                l.unsubscribed.remove(w);
                l.poll_only.remove(w);
            }
            l.fails = 0;
            l.scanned = true;
            if full {
                l.last_full = Some(Instant::now());
            }
            let every = if l.healthy() { RESCAN_SAFETY } else { RESCAN_EVERY };
            l.next_scan = Some(Instant::now() + every);
            l.next_is_safety = true;
        }

        let mut st = self.shared.lock();
        if st.app(self.pid, self.epoch).is_none() {
            return;
        }
        let s = &mut *st;
        for (wid, in_list) in kept_listed.iter().map(|w| (w, true)).chain(kept_unlisted.iter().map(|w| (w, false))) {
            if let Some(w) = s.wins.get_mut(wid)
                && w.epoch == self.epoch
            {
                w.in_ax_list = in_list;
            }
        }
        for (wid, el, a, _new, in_list) in found {
            match s.wins.get_mut(&wid) {
                Some(w) if w.epoch == self.epoch => {
                    w.in_ax_list = in_list;
                    if a.title.is_some() {
                        w.title = a.title;
                    }
                    if a.subrole.is_some() {
                        w.subrole = a.subrole;
                    }
                    if let Some(m) = a.minimized {
                        if m && !w.minimized {
                            s.events.push(ModelEvent::Minimized { wid });
                        }
                        if m != w.minimized {
                            w.changed_at = Some(Instant::now());
                        }
                        w.minimized = m;
                    }
                }
                _ => {
                    s.wins.insert(wid, WinRec {
                        pid: self.pid,
                        epoch: self.epoch,
                        el: SendEl(el),
                        title: a.title,
                        subrole: a.subrole,
                        minimized: a.minimized.unwrap_or(false),
                        changed_at: None,
                        in_ax_list: in_list,
                        added_at: Instant::now(),
                    });
                }
            }
        }
        for w in gone {
            s.remove_win(w);
        }
        if let Some(app) = s.app(self.pid, self.epoch) {
            app.ready = true;
            app.scans += 1;
            if full {
                app.full_scans += 1;
            }
            app.last_scan = Some(Instant::now());
            app.last_err = None;
            app.busy_since = None;
        }
        if let Some(f) = focus {
            s.apply_focus(self.pid, self.epoch, f.map(|(w, _)| w), true);
        }
        self.shared.flush(st);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(wid: u32, pid: i32, z: usize) -> WinInfo {
        WinInfo {
            wid,
            pid,
            app_name: format!("app{pid}"),
            bundle_id: None,
            title: String::new(),
            bounds: (0.0, 0.0, 100.0, 100.0),
            minimized: false,
            on_current_space: true,
            hidden_app: false,
            z_index: z,
            ax: None,
        }
    }

    fn el() -> SendEl {
        // SAFETY: plain constructor; the element is never messaged in these tests.
        SendEl(unsafe { AXUIElement::new_application(1) })
    }

    fn state_with(wins: &[(u32, i32)]) -> State {
        let mut st = State::default();
        for &(wid, pid) in wins {
            st.apps.entry(pid).or_insert_with(|| AppRec::new(1, &AppMeta { name: String::new(), bundle_id: None, hidden: false }));
            st.wins.insert(wid, WinRec { pid, epoch: 1, el: el(), title: None, subrole: None, minimized: false, changed_at: None, in_ax_list: true, added_at: Instant::now() });
        }
        st
    }

    #[test]
    fn asks_wait_for_a_sent_rescan_retry_once_then_back_off() {
        let t0 = Instant::now();
        let s = |secs: f64| t0 + Duration::from_secs_f64(secs);
        let mut a = Asks::default();
        // Never asked: due. A throttled poke records nothing, so it stays due.
        assert!(a.due(7, None, s(0.0)));
        assert!(a.due(7, None, s(0.5)));
        // Rescan sent at 1 s: not due while it has not finished (a busy app), except after 30 s.
        a.sent(7, s(1.0));
        assert!(!a.due(7, None, s(5.0)));
        assert!(!a.due(7, Some(s(0.9)), s(5.0)), "a scan that ended before the ask does not count");
        assert!(a.due(7, None, s(31.0)));
        // The rescan finished at 1.2 s without the window: one quick retry after POKE_EVERY.
        assert!(!a.due(7, Some(s(1.2)), s(2.0)));
        assert!(a.due(7, Some(s(1.2)), s(3.2)));
        // The retry did not settle it either: back off REPOKE_SAME_WINDOW from its scan.
        a.sent(7, s(3.2));
        assert!(!a.due(7, Some(s(3.4)), s(6.0)));
        assert!(!a.due(7, Some(s(3.4)), s(33.0)));
        assert!(a.due(7, Some(s(3.4)), s(33.4)));
        // Windows not asked about for 60 s are forgotten (asked again from scratch).
        a.sent(8, s(40.0));
        a.prune(s(64.0));
        assert!(a.0.contains_key(&8) && !a.0.contains_key(&7));
    }

    #[test]
    fn order_groups_apps_by_most_recent_window() {
        // z order: 10(p1) 20(p2) 11(p1) 30(p3) 21(p2)
        let wins = vec![win(10, 1, 0), win(20, 2, 1), win(11, 1, 2), win(30, 3, 3), win(21, 2, 4)];
        // Used: 21 most recent, then 11. Current = 20 (index 1).
        let used = HashMap::from([(21, 9), (11, 5)]);
        let (out, cur, mru) = order_mru(wins, Some(1), &used);
        let wids: Vec<u32> = out.iter().map(|w| w.wid).collect();
        // p2 first (current 20, then 21), p1 (11 used, then 10 by z), p3.
        assert_eq!(wids, vec![20, 21, 11, 10, 30]);
        assert_eq!(cur, Some(0));
        let mru_wids: Vec<u32> = mru.iter().map(|&i| out[i].wid).collect();
        assert_eq!(mru_wids, vec![20, 21, 11, 10, 30]);
    }

    #[test]
    fn floating_panel_focus_stands_in_for_the_apps_last_window() {
        // z order: D (p1, the document the user is in), E (p2), F (p1, older document).
        let wins = vec![win(10, 1, 0), win(20, 2, 1), win(11, 1, 2)];
        // The Fonts panel 99 of p1 has focus; D was the window before it (most recent of p1).
        let used = HashMap::from([(20, 3), (11, 4), (10, 5)]);
        let cur = pick_current(&wins, Some((99, 1)), &used, Some(1), true);
        assert_eq!(cur, Some(0), "D stands in for the unlisted panel");
        let (out, cur, mru) = order_mru(wins.clone(), cur, &used);
        assert_eq!(out[cur.unwrap()].wid, 10);
        // D heads the MRU order, so a quick tap (main.rs `initial_selection`: the second MRU entry
        // when there is a current window) goes to the window before D, not to D itself.
        assert_eq!(mru.iter().map(|&i| out[i].wid).collect::<Vec<_>>(), vec![10, 11, 20]);
        // Without history the app's topmost on-screen window stands in.
        assert_eq!(pick_current(&wins, Some((99, 1)), &HashMap::new(), Some(1), true), Some(0));
        // Minimized and other-Space windows never stand in.
        let mut hidden = wins.clone();
        hidden[0].minimized = true;
        hidden[2].on_current_space = false;
        assert_eq!(pick_current(&hidden, Some((99, 1)), &used, Some(1), true), None);
    }

    #[test]
    fn listed_focus_wins_and_unknown_focus_uses_the_topmost_window() {
        let wins = vec![win(10, 1, 0), win(20, 2, 1)];
        assert_eq!(pick_current(&wins, Some((20, 2)), &HashMap::new(), Some(2), true), Some(1));
        // No focus information for the active app: the topmost window, if it is that app's.
        assert_eq!(pick_current(&wins, None, &HashMap::new(), Some(1), false), Some(0));
        assert_eq!(pick_current(&wins, None, &HashMap::new(), Some(2), false), None);
        // Focus known to be on nothing (desktop): no current window.
        assert_eq!(pick_current(&wins, None, &HashMap::new(), Some(1), true), None);
    }

    #[test]
    fn order_without_history_is_z_order() {
        let wins = vec![win(1, 1, 0), win(2, 2, 1), win(3, 1, 2)];
        let (out, cur, mru) = order_mru(wins, None, &HashMap::new());
        assert_eq!(out.iter().map(|w| w.wid).collect::<Vec<_>>(), vec![1, 3, 2]);
        assert_eq!(cur, None);
        assert_eq!(mru.iter().map(|&i| out[i].wid).collect::<Vec<_>>(), vec![1, 2, 3]);
    }

    #[test]
    fn transient_current_does_not_enter_history() {
        let mut st = state_with(&[(1, 10), (2, 20), (3, 20)]);
        let t0 = Instant::now();
        st.active_pid = Some(10);
        st.set_current_at(Some((1, 10)), false, t0);
        // App 20 activates with its stale focused window 2, then window 3 takes focus 20 ms later.
        st.set_current_at(Some((2, 20)), false, t0 + Duration::from_secs(5));
        st.set_current_at(Some((3, 20)), false, t0 + Duration::from_secs(5) + Duration::from_millis(20));
        assert!(st.used.contains_key(&1), "window 1 was current for 5 s");
        assert!(!st.used.contains_key(&2), "window 2 was current for 20 ms only");
        assert_eq!(st.current.map(|c| c.wid), Some(3));
        let lefts: Vec<u32> = st.events.iter().filter_map(|e| if let ModelEvent::FocusLeft { wid } = e { Some(*wid) } else { None }).collect();
        assert_eq!(lefts, vec![1, 2]);
    }

    #[test]
    fn own_window_is_current_while_ztab_is_active_and_it_is_key() {
        let own = std::process::id() as i32;
        let mut st = state_with(&[(1, 10)]);
        st.apps.get_mut(&10).unwrap().focused = Some(1);
        let t0 = Instant::now() - Duration::from_secs(10);
        st.active_pid = Some(10);
        st.set_current_at(Some((1, 10)), false, t0);
        st.own_win = Some(OwnWindow { wid: 99, app_name: "ZTab".into(), title: "ZTab Settings".into(), bounds: (0.0, 0.0, 500.0, 360.0), minimized: false });
        // ZTab activates while its window is not key (yet): no current window.
        st.activate(own);
        assert_eq!((st.active_pid, st.current.map(|c| c.wid)), (Some(own), None));
        assert!(st.used.contains_key(&1), "window 1 was current for 10 s");
        // Its window became key: it is the current window.
        st.own_key = true;
        st.activate(own);
        assert_eq!(st.current.map(|c| (c.wid, c.pid)), Some((99, own)));
        // Another app activates: back to its focused window.
        st.activate(10);
        assert_eq!(st.current.map(|c| c.wid), Some(1));
        // ZTab switched to its own window (noted) and is activated before the window reports
        // that it is key: the activation keeps it current.
        st.own_key = false;
        st.note = Some(Note { pid: own, wid: 99, at: Instant::now() });
        st.activate(own);
        assert_eq!(st.current.map(|c| c.wid), Some(99));
        st.activate(10);
        // The switcher's view: with ZTab's window current, the quick tap goes to window 1.
        let wins = vec![
            WinInfo { pid: own, ..win(99, own, 0) },
            win(1, 10, 1),
        ];
        let used: HashMap<u32, u64> = [(1, 5)].into_iter().collect();
        let cur = pick_current(&wins, Some((99, own)), &used, Some(own), false);
        assert_eq!(cur, Some(0));
        let (out, cur, mru) = order_mru(wins, cur, &used);
        assert_eq!(out[cur.unwrap()].wid, 99);
        assert_eq!(out[mru[1]].wid, 1);
    }

    #[test]
    fn explicit_switch_is_recorded_at_once() {
        let mut st = state_with(&[(1, 10), (2, 20)]);
        let t0 = Instant::now();
        st.set_current_at(Some((1, 10)), false, t0);
        st.set_current_at(Some((2, 20)), true, t0 + Duration::from_millis(1));
        assert!(st.used.contains_key(&2));
        assert!(!st.used.contains_key(&1), "1 was current for 1 ms");
        // Same window again explicitly: fresh, higher sequence number.
        let before = st.used[&2];
        st.set_current_at(Some((2, 20)), true, t0 + Duration::from_millis(2));
        assert!(st.used[&2] > before);
    }

    #[test]
    fn polled_focus_is_ignored_during_switch_grace() {
        let mut st = state_with(&[(1, 10), (2, 10)]);
        st.active_pid = Some(10);
        st.note = Some(Note { pid: 10, wid: 2, at: Instant::now() });
        st.set_current(Some((2, 10)), true);
        st.apply_focus(10, 1, Some(1), true);
        assert_eq!(st.current.map(|c| c.wid), Some(2), "a stale poll must not undo the switch");
        st.apply_focus(10, 1, Some(1), false);
        assert_eq!(st.current.map(|c| c.wid), Some(1), "a focus notification is authoritative");
    }

    #[test]
    fn removing_app_forgets_windows_and_stale_epochs_are_ignored() {
        let mut st = state_with(&[(1, 10), (2, 10), (3, 20)]);
        st.active_pid = Some(10);
        st.set_current(Some((1, 10)), true);
        st.apply_focus(10, 99, Some(2), false);
        assert_eq!(st.current.map(|c| c.wid), Some(1), "report from a stale thread epoch");
        st.remove_app(10, 1);
        assert!(st.wins.keys().all(|w| *w == 3));
        assert!(st.current.is_none());
        assert!(!st.used.contains_key(&1));
        assert!(st.events.contains(&ModelEvent::AppTerminated { pid: 10 }));
        assert!(st.events.contains(&ModelEvent::Destroyed { wid: 1 }));
    }

    #[test]
    fn focus_left_only_for_capturable_windows() {
        let mut st = state_with(&[(1, 10), (2, 10)]);
        st.active_pid = Some(10);
        st.set_current(Some((1, 10)), false);
        st.wins.get_mut(&1).unwrap().minimized = true;
        st.set_current(Some((2, 10)), false);
        assert!(!st.events.iter().any(|e| matches!(e, ModelEvent::FocusLeft { .. })));
    }
}
