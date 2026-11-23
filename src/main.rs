//! ZTab: a Windows-style Alt+Tab (Option+Tab) window switcher for macOS.
//!
//! Hold Option, press Tab to open the switcher showing every window (grouped by app),
//! keep pressing Tab / Shift+Tab (or arrows) to move, release Option to switch.
//! Click a tile with the mouse while holding Option to switch to it. Esc cancels.
//! Option+` opens the switcher for the frontmost app's windows only.

mod hotkey;
mod thumbs;
mod ui;
mod windows;

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::io::Write as _;
use std::rc::Rc;
use std::time::Duration;

use dispatch2::{DispatchQueue, DispatchTime};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, Sel};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSImage, NSMenu, NSMenuDelegate, NSMenuItem,
    NSRunningApplication, NSStatusBar, NSWorkspace, NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{NSString, NSTimer};

use hotkey::{Hotkey, HotkeyEvent, HotkeyHandle};
use thumbs::Thumbnailer;
use ui::{Switcher, Tile};
use windows::WinInfo;

/// Thumbnails are captured to fit this many points (2x pixels) — about the largest tile size.
const THUMB_MAX_W: f64 = 300.0;
const THUMB_MAX_H: f64 = 190.0;
/// A quick Option+Tab tap switches without flashing the panel (like Windows).
const SHOW_DELAY_MS: u64 = 120;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGRequestScreenCaptureAccess() -> bool;
}

// ---------------------------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------------------------

fn log(msg: &str) {
    let path = std::env::var("HOME").unwrap_or_default() + "/Library/Logs/ztab.log";
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let _ = writeln!(f, "[{ts:.3}] {msg}");
    }
    if std::env::var_os("ZTAB_STDERR").is_some() {
        eprintln!("{msg}");
    }
}

// ---------------------------------------------------------------------------------------------
// Switcher state
// ---------------------------------------------------------------------------------------------

struct State {
    mtm: MainThreadMarker,
    switcher: Switcher,
    thumbs: Thumbnailer,
    /// Windows currently offered by the switcher (same order as the tiles).
    wins: RefCell<Vec<WinInfo>>,
    /// Tiles built for the current session but not shown yet (show is delayed).
    pending_tiles: RefCell<Option<Vec<Tile>>>,
    selected: Cell<usize>,
    /// Bumped every time a session opens or ends; stale async work compares against it.
    generation: Cell<u64>,
    /// True between Open and Commit/Cancel.
    active: Cell<bool>,
    hotkey: RefCell<Option<Hotkey>>,
    hk_handle: Cell<Option<HotkeyHandle>>,
    icons: RefCell<HashMap<i32, Retained<NSImage>>>,
    menu: RefCell<Option<MenuParts>>,
    install_failed_logged: Cell<bool>,
}

/// `WinInfo` holds an `AXUIElement`, which is not marked `Send` by the bindings; AX elements
/// are CF objects that may be used from any thread (the AX API is thread-safe).
struct SendWin(WinInfo);
// SAFETY: see above; the value is moved to exactly one other thread and used only there.
unsafe impl Send for SendWin {}
impl SendWin {
    fn into_inner(self) -> WinInfo {
        self.0
    }
}

thread_local! {
    static STATE: OnceCell<Rc<State>> = const { OnceCell::new() };
}

fn state() -> Option<Rc<State>> {
    STATE.with(|s| s.get().cloned())
}

impl State {
    fn app_icon(&self, pid: i32) -> Option<Retained<NSImage>> {
        if let Some(i) = self.icons.borrow().get(&pid) {
            return Some(i.clone());
        }
        let icon = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?.icon()?;
        self.icons.borrow_mut().insert(pid, icon.clone());
        Some(icon)
    }

    fn set_tap_open(&self, open: bool) {
        if let Some(h) = self.hk_handle.get() {
            h.set_open(open);
        }
    }

    /// Start a switcher session. Returns false if there is nothing to show.
    fn open(&self, reverse: bool, same_app: bool) -> bool {
        let gen_ = self.generation.get() + 1;
        self.generation.set(gen_);

        let mut list = windows::list_windows();
        // Evict thumbnails / icons of windows and apps that are gone (computed before the
        // same-app filter so Option+` doesn't drop other apps' cached thumbnails).
        let live: Vec<u32> = list.iter().map(|w| w.wid).collect();
        self.thumbs.retain_only(&live);
        self.icons.borrow_mut().retain(|pid, _| list.iter().any(|w| w.pid == *pid));

        let front_pid = NSWorkspace::sharedWorkspace().frontmostApplication().map(|a| a.processIdentifier());
        if same_app {
            list.retain(|w| Some(w.pid) == front_pid);
        }
        if list.is_empty() {
            log("open: no windows");
            return false;
        }

        // The window the user is in right now: the focused window if AX reports one in the
        // list, else the frontmost app's topmost window when that is also the topmost overall.
        // If the active app has no window on top (last window closed / minimized, desktop
        // clicked), there is no current window and the topmost listed window is the target.
        let mut by_z: Vec<usize> = (0..list.len()).collect();
        by_z.sort_by_key(|&i| list[i].z_index);
        let focused = windows::focused_window_id();
        let current = focused
            .and_then(|wid| list.iter().position(|w| w.wid == wid))
            .or_else(|| (Some(list[by_z[0]].pid) == front_pid && list[by_z[0]].on_current_space).then_some(by_z[0]));
        // Most-recently-used order: current window first, then z order.
        if let Some(c) = current {
            by_z.retain(|&i| i != c);
            by_z.insert(0, c);
        }
        let sel = if reverse {
            *by_z.last().unwrap()
        } else if current.is_some() {
            by_z.get(1).copied().unwrap_or(by_z[0])
        } else {
            by_z[0]
        };

        let mut group = 0usize;
        let mut tiles = Vec::with_capacity(list.len());
        for (i, w) in list.iter().enumerate() {
            if i > 0 && list[i - 1].pid != w.pid {
                group += 1;
            }
            let (_, _, bw, bh) = w.bounds;
            tiles.push(Tile {
                app_name: w.app_name.clone(),
                title: w.title.clone(),
                icon: self.app_icon(w.pid),
                thumbnail: self.thumbs.cached(w.wid),
                aspect: if bh > 0.0 { bw / bh } else { 1.6 },
                minimized: w.minimized,
                group,
            });
        }

        // Fresh thumbnails: selected first, then the rest in display order.
        let mut wids: Vec<u32> = vec![list[sel].wid];
        wids.extend(list.iter().filter(|w| w.wid != list[sel].wid).map(|w| w.wid));
        self.thumbs.capture(wids, THUMB_MAX_W, THUMB_MAX_H, gen_, |g, wid, img| {
            if let Some(st) = state() {
                st.on_thumbnail(g, wid, img);
            }
        });

        log(&format!(
            "open: {} windows, current={current:?} selected={sel} (reverse={reverse}, same_app={same_app})",
            list.len()
        ));
        *self.wins.borrow_mut() = list;
        *self.pending_tiles.borrow_mut() = Some(tiles);
        self.selected.set(sel);
        self.active.set(true);

        // Show after a short delay so a quick Option+Tab just flips windows.
        let when = DispatchTime::try_from(Duration::from_millis(SHOW_DELAY_MS)).unwrap_or(DispatchTime::NOW);
        let _ = DispatchQueue::main().after(when, move || {
            if let Some(st) = state() {
                if st.generation.get() == gen_ {
                    st.show_now();
                }
            }
        });
        true
    }

    fn show_now(&self) {
        if !self.active.get() {
            return;
        }
        if let Some(tiles) = self.pending_tiles.borrow_mut().take() {
            self.switcher.show(tiles, self.selected.get());
            // Panel frame in CG (top-left origin) coordinates, for diagnostics / e2e tests.
            let f = self.switcher.panel().frame();
            let primary_h = objc2_app_kit::NSScreen::screens(self.mtm)
                .firstObject()
                .map(|s| s.frame().size.height)
                .unwrap_or(0.0);
            log(&format!(
                "panel shown: cg_frame={} {} {} {}",
                f.origin.x,
                primary_h - f.origin.y - f.size.height,
                f.size.width,
                f.size.height
            ));
        }
    }

    fn on_thumbnail(&self, generation: u64, wid: u32, img: Retained<NSImage>) {
        if std::env::var_os("ZTAB_THUMBS_TRACE").is_some() {
            log(&format!("thumb gen={generation} cur={} wid={wid} active={}", self.generation.get(), self.active.get()));
        }
        if generation != self.generation.get() || !self.active.get() {
            return;
        }
        let Some(idx) = self.wins.borrow().iter().position(|w| w.wid == wid) else { return };
        if let Some(tiles) = self.pending_tiles.borrow_mut().as_mut() {
            if let Some(t) = tiles.get_mut(idx) {
                t.thumbnail = Some(img);
            }
            return;
        }
        self.switcher.set_thumbnail(idx, img);
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
        self.generation.set(self.generation.get() + 1);
        self.pending_tiles.borrow_mut().take();
        if self.switcher.is_visible() {
            log("panel hidden");
        }
        self.switcher.hide();
    }

    fn commit(&self) {
        if !self.active.get() {
            return;
        }
        let idx = self.selected.get();
        self.end_session();
        let gen_ = self.generation.get();
        let _ = gen_;
        let Some(w) = self.wins.borrow().get(idx).cloned() else { return };
        // Focusing talks to the target app over AX (it can block up to the AX timeout), so it
        // runs off the main thread: the main run loop also services the keyboard event tap.
        let w = SendWin(w);
        let _ = std::thread::Builder::new().name("ztab-focus".into()).spawn(move || {
            let w = w.into_inner();
            let ok = windows::focus_window(&w);
            log(&format!("commit: wid={} app={} title={:?} ok={ok}", w.wid, w.app_name, w.title));
        });
    }

    fn cancel(&self) {
        if self.active.get() {
            log("cancel");
        }
        self.end_session();
    }

    fn on_hotkey(&self, ev: HotkeyEvent) {
        match ev {
            HotkeyEvent::Open { reverse } => {
                if !self.open(reverse, false) {
                    self.set_tap_open(false);
                }
            }
            HotkeyEvent::OpenSameApp { reverse } => {
                if !self.open(reverse, true) {
                    self.set_tap_open(false);
                }
            }
            HotkeyEvent::Next => self.step(1),
            HotkeyEvent::Prev => self.step(-1),
            HotkeyEvent::Down => self.vertical(true),
            HotkeyEvent::Up => self.vertical(false),
            HotkeyEvent::Commit => self.commit(),
            HotkeyEvent::Cancel => self.cancel(),
        }
    }

    fn on_click(&self, idx: usize) {
        if !self.active.get() {
            return;
        }
        self.selected.set(idx);
        self.commit();
        // The tap only closes itself on Option release / Return / Esc.
        self.set_tap_open(false);
    }

    fn on_hover(&self, idx: usize) {
        if self.active.get() {
            self.select(idx);
        }
    }

    fn try_install_hotkey(&self) {
        if self.hotkey.borrow().is_some() || !windows::ax_trusted() {
            return;
        }
        let handler = Box::new(|ev: HotkeyEvent| {
            if let Some(st) = state() {
                st.on_hotkey(ev);
            }
        });
        match Hotkey::install(handler) {
            Ok(hk) => {
                log(&format!("hotkey installed ({:?})", hk.location()));
                self.hk_handle.set(Some(hk.handle()));
                *self.hotkey.borrow_mut() = Some(hk);
                self.refresh_menu();
            }
            Err(e) => {
                if !self.install_failed_logged.replace(true) {
                    log(&format!("hotkey install failed: {e}"));
                }
            }
        }
    }

    /// Periodic housekeeping: install the tap once Accessibility is granted, re-enable it if
    /// macOS disabled it, recover from a missed Option release.
    fn watchdog(&self) {
        self.try_install_hotkey();
        if let Some(h) = self.hk_handle.get() {
            h.reenable();
            if self.active.get() && !h.is_open() {
                // Tap closed itself (e.g. recovery commit) without telling us.
                self.end_session();
            }
        }
    }

    fn refresh_menu(&self) {
        let Some(m) = self.menu.borrow().as_ref().map(|m| (m.ax.clone(), m.screen.clone())) else {
            return;
        };
        let (ax_item, sc_item) = m;
        let ax = windows::ax_trusted();
        let sc = windows::screen_recording_granted();
        let hk = self.hotkey.borrow().is_some();
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
    }
}

// ---------------------------------------------------------------------------------------------
// Status bar menu
// ---------------------------------------------------------------------------------------------

struct MenuParts {
    _item: Retained<NSStatusItem>,
    _target: Retained<MenuTarget>,
    ax: Retained<NSMenuItem>,
    screen: Retained<NSMenuItem>,
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
                st.refresh_menu();
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

        #[unsafe(method(quit:))]
        fn quit(&self, _sender: Option<&AnyObject>) {
            log("quit from menu");
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
    let _ = std::process::Command::new("open").arg(url).spawn();
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

fn build_status_item(mtm: MainThreadMarker) -> MenuParts {
    let target = MenuTarget::new(mtm);
    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    if let Some(button) = item.button(mtm) {
        let img = NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &NSString::from_str("rectangle.on.rectangle"),
            Some(&NSString::from_str("ZTab")),
        );
        if let Some(img) = img {
            img.setTemplate(true);
            button.setImage(Some(&img));
        } else {
            button.setTitle(&NSString::from_str("⌥⇥"));
        }
        button.setToolTip(Some(&NSString::from_str("ZTab — Option+Tab window switcher")));
    }

    let menu = NSMenu::new(mtm);
    menu.setAutoenablesItems(false);
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
    let quit = menu_item(mtm, "Quit ZTab", Some(sel!(quit:)), "q", &target);
    menu.addItem(&quit);
    menu.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(&*target)));
    item.setMenu(Some(&menu));

    MenuParts { _item: item, _target: target, ax, screen }
}

// ---------------------------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------------------------

fn make_state(mtm: MainThreadMarker) -> Rc<State> {
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
    let st = Rc::new(State {
        mtm,
        switcher,
        thumbs: Thumbnailer::new(),
        wins: RefCell::new(Vec::new()),
        pending_tiles: RefCell::new(None),
        selected: Cell::new(0),
        generation: Cell::new(0),
        active: Cell::new(false),
        hotkey: RefCell::new(None),
        hk_handle: Cell::new(None),
        icons: RefCell::new(HashMap::new()),
        menu: RefCell::new(None),
        install_failed_logged: Cell::new(false),
    });
    STATE.with(|s| {
        let _ = s.set(st.clone());
    });
    st
}

/// `ztab --dump-windows <file>`: write the switcher's window list (verification aid).
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

/// `ztab --snapshot <png>`: open the switcher with the real windows, wait for thumbnails,
/// render it to a PNG, exit (verification aid).
fn snapshot(st: &Rc<State>, path: String) {
    if !st.open(false, false) {
        let _ = std::fs::write(&path, b"");
        std::process::exit(2);
    }
    st.show_now();
    let when = DispatchTime::try_from(Duration::from_millis(1500)).unwrap();
    let _ = DispatchQueue::main().after(when, move || {
        let st = state().unwrap();
        let ok = st.switcher.snapshot_png(&path);
        log(&format!("snapshot {path}: {ok}"));
        std::process::exit(if ok { 0 } else { 1 });
    });
}

fn already_running() -> bool {
    let me = std::process::id() as i32;
    let apps = NSRunningApplication::runningApplicationsWithBundleIdentifier(&NSString::from_str("dev.ztab.app"));
    apps.iter().any(|a| a.processIdentifier() != me)
}

fn main() {
    let mtm = MainThreadMarker::new().expect("must run on the main thread");
    let args: Vec<String> = std::env::args().collect();

    if let Some(i) = args.iter().position(|a| a == "--dump-windows") {
        dump_windows(args.get(i + 1).map(String::as_str).unwrap_or("/dev/stdout"));
        return;
    }

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let st = make_state(mtm);

    if let Some(i) = args.iter().position(|a| a == "--snapshot") {
        let path = args.get(i + 1).cloned().unwrap_or_else(|| "ztab-snapshot.png".into());
        snapshot(&st, path);
        app.run();
        return;
    }

    if already_running() {
        log("another ZTab instance is running; exiting");
        return;
    }

    log(&format!(
        "starting pid={} ax={} screen={}",
        std::process::id(),
        windows::ax_trusted(),
        windows::screen_recording_granted()
    ));

    *st.menu.borrow_mut() = Some(build_status_item(mtm));

    if !windows::ax_trusted() {
        prompt_accessibility();
    }
    if !windows::screen_recording_granted() {
        // Shows the system prompt only the first time; afterwards it's a no-op.
        unsafe { CGRequestScreenCaptureAccess() };
    }
    st.try_install_hotkey();
    st.refresh_menu();

    // Warm up AX connections and the capture pipeline so the first Option+Tab is fast.
    let _ = windows::list_windows();
    st.thumbs.warm_up();

    let watchdog = block2::RcBlock::new(|_t: std::ptr::NonNull<NSTimer>| {
        if let Some(st) = state() {
            st.watchdog();
        }
    });
    let _timer = unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(1.5, true, &watchdog) };

    app.run();
}
