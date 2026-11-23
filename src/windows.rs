//! Window enumeration and focusing.
//!
//! Enumeration merges three sources:
//! * CoreGraphics window lists (`CGWindowListCopyWindowInfo`) for the front-to-back order,
//!   bounds, layer and owner of every window (no permission needed; titles need Screen Recording),
//! * Accessibility (`AXWindows` per app, mapped to CGWindowIDs with `_AXUIElementGetWindow`)
//!   for titles, subroles and the minimized flag (needs Accessibility),
//! * SkyLight Space SPI (`SLSCopySpacesForWindows`, `SLSCopyManagedDisplaySpaces`) to tell which
//!   windows live on a currently visible Space (no permission needed).
//!
//! Focusing uses the SkyLight process-front SPI plus a synthetic "make key window" event record,
//! then `AXRaise`, which works from a never-activated accessory (LSUIElement) app.

use std::collections::{HashMap, HashSet};
use std::ffi::{c_char, c_int, c_void};
use std::ptr::NonNull;
use std::sync::OnceLock;

use objc2_app_kit::{NSApplicationActivationOptions, NSApplicationActivationPolicy, NSRunningApplication, NSWorkspace};
use objc2_application_services::{AXError, AXUIElement, AXValue};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType, CGRect,
};
use objc2_core_graphics::{
    CGRectMakeWithDictionaryRepresentation, CGWindowListCopyWindowInfo, CGWindowListOption,
    kCGWindowAlpha, kCGWindowBounds, kCGWindowLayer, kCGWindowName, kCGWindowNumber,
    kCGWindowOwnerPID,
};

/// Minimum width and height (points) for a window to count as user-facing.
const MIN_SIZE: f64 = 50.0;
/// Per-element AX messaging timeout, so a hung app cannot freeze the caller.
const AX_TIMEOUT_S: f32 = 0.25;
/// Time budget of one remote-token sweep (per app) during `list_windows`.
const TOKEN_BUDGET: std::time::Duration = std::time::Duration::from_millis(60);
/// Overall budget for the parallel AX pass of `list_windows`.
const AX_DEADLINE: std::time::Duration = std::time::Duration::from_millis(150);
/// `kCPSUserGenerated` mode for `_SLPSSetFrontProcessWithOptions`.
const SLPS_USER_GENERATED: u32 = 0x200;

#[derive(Clone, Debug)]
pub struct WinInfo {
    /// CGWindowID.
    pub wid: u32,
    pub pid: i32,
    pub app_name: String,
    pub bundle_id: Option<String>,
    /// AX title (or CG window name when AX is unavailable and Screen Recording is granted). May be empty.
    pub title: String,
    /// x, y, w, h in CG global coordinates (top-left origin of the primary display).
    pub bounds: (f64, f64, f64, f64),
    pub minimized: bool,
    /// Window is on a Space that is currently visible on some display.
    pub on_current_space: bool,
    /// The owning app is hidden (Cmd+H).
    pub hidden_app: bool,
    /// Dense front-to-back rank over the returned windows: 0 = frontmost on-screen window.
    /// Off-screen windows (minimized, hidden app, other Space) rank after all on-screen ones.
    pub z_index: usize,
    /// AX element of the window, when Accessibility is granted and the app listed it.
    ax: Option<CFRetained<AXUIElement>>,
}

impl WinInfo {
    /// Whether an AX element is attached (needed for AXRaise / unminimize).
    #[allow(dead_code)]
    pub fn has_ax(&self) -> bool {
        self.ax.is_some()
    }
}

/// `AXIsProcessTrusted()`.
pub fn ax_trusted() -> bool {
    unsafe { objc2_application_services::AXIsProcessTrusted() }
}

/// `CGPreflightScreenCaptureAccess()`.
pub fn screen_recording_granted() -> bool {
    objc2_core_graphics::CGPreflightScreenCaptureAccess()
}

// ---------------------------------------------------------------------------------------------
// Private SPI
// ---------------------------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
struct Psn {
    hi: u32,
    lo: u32,
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn _AXUIElementGetWindow(element: &AXUIElement, wid: *mut u32) -> AXError;
    fn GetProcessForPID(pid: libc::pid_t, psn: *mut Psn) -> i32;
    /// Returns a +1 AXUIElement for a remote token (pid, 0, 'coco', element id), or NULL.
    fn _AXUIElementCreateWithRemoteToken(token: &objc2_core_foundation::CFData) -> *mut AXUIElement;
}

type SlsMainConnectionId = unsafe extern "C" fn() -> c_int;
type SlsCopySpacesForWindows = unsafe extern "C" fn(c_int, c_int, *const CFArray) -> *const CFArray;
type SlsCopyManagedDisplaySpaces = unsafe extern "C" fn(c_int) -> *const CFArray;
type SlpsSetFrontProcessWithOptions = unsafe extern "C" fn(*const Psn, u32, u32) -> i32;
type SlpsPostEventRecordTo = unsafe extern "C" fn(*const Psn, *const u8) -> i32;

struct Sky {
    main_connection_id: Option<SlsMainConnectionId>,
    copy_spaces_for_windows: Option<SlsCopySpacesForWindows>,
    copy_managed_display_spaces: Option<SlsCopyManagedDisplaySpaces>,
    set_front_process: Option<SlpsSetFrontProcessWithOptions>,
    post_event_record: Option<SlpsPostEventRecordTo>,
}

fn sky() -> &'static Sky {
    static SKY: OnceLock<Sky> = OnceLock::new();
    SKY.get_or_init(|| unsafe {
        let path = c"/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight";
        let handle = libc::dlopen(path.as_ptr(), libc::RTLD_LAZY);
        let sym = |name: &std::ffi::CStr| -> *mut c_void {
            let mut p = std::ptr::null_mut();
            if !handle.is_null() {
                p = libc::dlsym(handle, name.as_ptr() as *const c_char);
            }
            if p.is_null() {
                p = libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr() as *const c_char);
            }
            p
        };
        macro_rules! load {
            ($name:literal, $ty:ty) => {{
                let p = sym($name);
                if p.is_null() { None } else { Some(std::mem::transmute::<*mut c_void, $ty>(p)) }
            }};
        }
        Sky {
            main_connection_id: load!(c"SLSMainConnectionID", SlsMainConnectionId),
            copy_spaces_for_windows: load!(c"SLSCopySpacesForWindows", SlsCopySpacesForWindows),
            copy_managed_display_spaces: load!(c"SLSCopyManagedDisplaySpaces", SlsCopyManagedDisplaySpaces),
            set_front_process: load!(c"_SLPSSetFrontProcessWithOptions", SlpsSetFrontProcessWithOptions),
            post_event_record: load!(c"SLPSPostEventRecordTo", SlpsPostEventRecordTo),
        }
    })
}

// ---------------------------------------------------------------------------------------------
// CF helpers
// ---------------------------------------------------------------------------------------------

type CfDict = CFDictionary<CFString, CFType>;

fn dict_get<'a>(d: &'a CfDict, key: &CFString) -> Option<&'a CFType> {
    unsafe { d.get_unchecked(key) }
}

fn num_i64(v: Option<&CFType>) -> Option<i64> {
    v?.downcast_ref::<CFNumber>()?.as_i64()
}

fn num_f64(v: Option<&CFType>) -> Option<f64> {
    v?.downcast_ref::<CFNumber>()?.as_f64()
}

fn cf_string(v: Option<&CFType>) -> Option<String> {
    Some(v?.downcast_ref::<CFString>()?.to_string())
}

fn cg_window_list(option: CGWindowListOption) -> Vec<CFRetained<CfDict>> {
    let Some(arr) = CGWindowListCopyWindowInfo(option, 0) else { return Vec::new() };
    // SAFETY: CGWindowListCopyWindowInfo returns a CFArray of CFDictionary<CFString, CFType>.
    let arr: CFRetained<CFArray<CfDict>> = unsafe { CFRetained::cast_unchecked(arr) };
    arr.to_vec()
}

// ---------------------------------------------------------------------------------------------
// Enumeration
// ---------------------------------------------------------------------------------------------

struct AppMeta {
    name: String,
    bundle_id: Option<String>,
    hidden: bool,
}

struct CgWin {
    wid: u32,
    pid: i32,
    bounds: (f64, f64, f64, f64),
    name: Option<String>,
    /// Rank in the on-screen front-to-back list (among layer-0 windows of regular apps).
    onscreen_rank: Option<usize>,
    /// Rank in the full list (window-server order, includes off-screen windows).
    all_rank: usize,
}

struct AxWin {
    el: CFRetained<AXUIElement>,
    title: Option<String>,
    subrole: Option<String>,
    minimized: bool,
}

fn regular_apps() -> HashMap<i32, AppMeta> {
    let me = std::process::id() as i32;
    let mut out = HashMap::new();
    let ws = NSWorkspace::sharedWorkspace();
    for app in ws.runningApplications().iter() {
        if app.activationPolicy() != NSApplicationActivationPolicy::Regular {
            continue;
        }
        let pid = app.processIdentifier();
        if pid == me || pid <= 0 {
            continue;
        }
        let bundle_id = app.bundleIdentifier().map(|s| s.to_string());
        if bundle_id.as_deref() == Some("com.apple.dock") {
            continue;
        }
        let name = app.localizedName().map(|s| s.to_string()).unwrap_or_default();
        out.insert(pid, AppMeta { name, bundle_id, hidden: app.isHidden() });
    }
    out
}

/// (wid, pid, layer, bounds, alpha, name) of one CG window dictionary.
type CgFields = (u32, i32, i64, (f64, f64, f64, f64), f64, Option<String>);

fn parse_cg(d: &CfDict) -> Option<CgFields> {
    unsafe {
        let wid = num_i64(dict_get(d, kCGWindowNumber))? as u32;
        let pid = num_i64(dict_get(d, kCGWindowOwnerPID))? as i32;
        let layer = num_i64(dict_get(d, kCGWindowLayer)).unwrap_or(-1);
        let alpha = num_f64(dict_get(d, kCGWindowAlpha)).unwrap_or(1.0);
        let mut rect = CGRect::default();
        let bd = dict_get(d, kCGWindowBounds)?.downcast_ref::<CFDictionary>()?;
        if !CGRectMakeWithDictionaryRepresentation(Some(bd), &mut rect) {
            return None;
        }
        let bounds = (rect.origin.x, rect.origin.y, rect.size.width, rect.size.height);
        let name = cf_string(dict_get(d, kCGWindowName));
        Some((wid, pid, layer, bounds, alpha, name))
    }
}

/// Collect layer-0, big-enough windows of regular apps, with on-screen and global ranks.
fn cg_candidates(apps: &HashMap<i32, AppMeta>) -> Vec<CgWin> {
    let mut onscreen_rank: HashMap<u32, usize> = HashMap::new();
    let on = cg_window_list(CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements);
    for d in &on {
        if let Some((wid, pid, layer, b, alpha, _)) = parse_cg(d)
            && layer == 0
            && alpha > 0.0
            && apps.contains_key(&pid)
            && b.2 >= MIN_SIZE
            && b.3 >= MIN_SIZE
        {
            let r = onscreen_rank.len();
            onscreen_rank.entry(wid).or_insert(r);
        }
    }
    let all = cg_window_list(CGWindowListOption::OptionAll | CGWindowListOption::ExcludeDesktopElements);
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for (i, d) in all.iter().enumerate() {
        let Some((wid, pid, layer, b, _alpha, name)) = parse_cg(d) else { continue };
        if layer != 0 || !apps.contains_key(&pid) || b.2 < MIN_SIZE || b.3 < MIN_SIZE {
            continue;
        }
        if !seen.insert(wid) {
            continue;
        }
        out.push(CgWin { wid, pid, bounds: b, name, onscreen_rank: onscreen_rank.get(&wid).copied(), all_rank: i });
    }
    out
}

fn ax_attr(el: &AXUIElement, attr: &str) -> Option<CFRetained<CFType>> {
    let key = CFString::from_str(attr);
    let mut value: *const CFType = std::ptr::null();
    let err = unsafe { el.copy_attribute_value(&key, NonNull::from(&mut value)) };
    if err != AXError::Success || value.is_null() {
        return None;
    }
    Some(unsafe { CFRetained::from_raw(NonNull::new_unchecked(value as *mut CFType)) })
}

/// AX windows of one app keyed by CGWindowID; `None` when AX could not be queried.
fn ax_windows(pid: i32) -> Option<HashMap<u32, AxWin>> {
    ax_windows_with_extra(pid, &[])
}

/// Read title / subrole / minimized of one AX window in a single round trip.
fn read_ax_win(el: CFRetained<AXUIElement>, key_arr: &CFArray) -> AxWin {
    unsafe { el.set_messaging_timeout(AX_TIMEOUT_S) };
    let mut values: *const CFArray = std::ptr::null();
    let err = unsafe {
        el.copy_multiple_attribute_values(
            key_arr,
            objc2_application_services::AXCopyMultipleAttributeOptions(0),
            NonNull::from(&mut values),
        )
    };
    let (mut title, mut subrole, mut minimized) = (None, None, false);
    if err == AXError::Success && !values.is_null() {
        let vals: CFRetained<CFArray<CFType>> =
            unsafe { CFRetained::from_raw(NonNull::new_unchecked(values as *mut CFArray<CFType>)) };
        let v: Vec<CFRetained<CFType>> = vals.to_vec();
        // Failed attributes come back as AXValue(kAXValueAXErrorType); downcasts reject them.
        let is_err = |x: &CFType| x.downcast_ref::<AXValue>().is_some();
        if let Some(x) = v.first().filter(|x| !is_err(x)) {
            title = x.downcast_ref::<CFString>().map(|s| s.to_string());
        }
        if let Some(x) = v.get(1).filter(|x| !is_err(x)) {
            subrole = x.downcast_ref::<CFString>().map(|s| s.to_string());
        }
        if let Some(x) = v.get(2).filter(|x| !is_err(x)) {
            minimized = x.downcast_ref::<CFBoolean>().map(|b| b.as_bool()).unwrap_or(false);
        }
    }
    AxWin { el, title, subrole, minimized }
}

/// AX windows of one app keyed by CGWindowID; `None` when AX could not be queried.
/// `extra` are window ids (normally on other Spaces) to resolve through the remote-token sweep
/// when `AXWindows` does not list them.
fn ax_windows_with_extra(pid: i32, extra: &[u32]) -> Option<HashMap<u32, AxWin>> {
    let app = unsafe { AXUIElement::new_application(pid) };
    unsafe { app.set_messaging_timeout(AX_TIMEOUT_S) };
    let wins = ax_attr(&app, "AXWindows")?;
    let wins = wins.downcast::<CFArray>().ok()?;
    // SAFETY: AXWindows is an array of AXUIElement.
    let wins: CFRetained<CFArray<AXUIElement>> = unsafe { CFRetained::cast_unchecked(wins) };
    let keys = [CFString::from_str("AXTitle"), CFString::from_str("AXSubrole"), CFString::from_str("AXMinimized")];
    let key_refs: Vec<&CFString> = keys.iter().map(|k| &**k).collect();
    let key_arr = CFArray::from_objects(&key_refs);
    let mut out = HashMap::new();
    for el in wins.iter() {
        let mut wid = 0u32;
        if unsafe { _AXUIElementGetWindow(&el, &mut wid) } != AXError::Success || wid == 0 {
            continue;
        }
        out.insert(wid, read_ax_win(el, key_arr.as_opaque()));
    }
    let mut wanted: HashSet<u32> = extra.iter().copied().filter(|w| !out.contains_key(w)).collect();
    if !wanted.is_empty() {
        for (wid, el) in token_sweep(pid, &mut wanted, TOKEN_BUDGET) {
            out.insert(wid, read_ax_win(el, key_arr.as_opaque()));
        }
    }
    Some(out)
}

/// Probe remote tokens `(pid, 0, 'coco', id)` for element ids 0..1000 (AltTab's technique) and
/// return the elements whose CGWindowID is in `wanted` (found ids are removed from it).
/// This reaches windows on other Spaces, which `AXWindows` omits. Bounded by `budget`.
fn token_sweep(pid: i32, wanted: &mut HashSet<u32>, budget: std::time::Duration) -> Vec<(u32, CFRetained<AXUIElement>)> {
    let start = std::time::Instant::now();
    let mut found = Vec::new();
    let mut token = [0u8; 20];
    token[0..4].copy_from_slice(&pid.to_ne_bytes());
    token[8..12].copy_from_slice(&0x636f_636fu32.to_ne_bytes());
    for id in 0u64..1000 {
        if wanted.is_empty() || start.elapsed() > budget {
            break;
        }
        token[12..20].copy_from_slice(&id.to_ne_bytes());
        let data = objc2_core_foundation::CFData::from_bytes(&token);
        let raw = unsafe { _AXUIElementCreateWithRemoteToken(&data) };
        let Some(raw) = NonNull::new(raw) else { continue };
        let el: CFRetained<AXUIElement> = unsafe { CFRetained::from_raw(raw) };
        unsafe { el.set_messaging_timeout(AX_TIMEOUT_S) };
        let mut wid = 0u32;
        if unsafe { _AXUIElementGetWindow(&el, &mut wid) } == AXError::Success && wanted.remove(&wid) {
            found.push((wid, el));
        }
    }
    found
}

/// Wrapper to move AX results between threads. AXUIElementRef is an immutable CF object and the
/// AX client API may be called from any thread.
struct SendBox<T>(T);
unsafe impl<T> Send for SendBox<T> {}
impl<T> SendBox<T> {
    fn into_inner(self) -> T {
        self.0
    }
}

/// Query `ax_windows` for every pid concurrently. Apps that have not answered within
/// `AX_DEADLINE` map to `None` (callers fall back to CG-only data); their worker threads finish on
/// their own, bounded by the per-element messaging timeout.
fn ax_windows_parallel(jobs: HashMap<i32, Vec<u32>>) -> HashMap<i32, Option<HashMap<u32, AxWin>>> {
    let pids: Vec<i32> = jobs.keys().copied().collect();
    let (tx, rx) = std::sync::mpsc::channel::<(i32, SendBox<Option<HashMap<u32, AxWin>>>)>();
    let mut spawned = 0;
    let mut out = HashMap::new();
    for (pid, extra) in jobs {
        let tx = tx.clone();
        let extra2 = extra.clone();
        let r = std::thread::Builder::new().name("ztab-ax".into()).spawn(move || {
            let _ = tx.send((pid, SendBox(ax_windows_with_extra(pid, &extra2))));
        });
        match r {
            Ok(_) => spawned += 1,
            Err(_) => {
                out.insert(pid, ax_windows_with_extra(pid, &extra));
            }
        }
    }
    drop(tx);
    let deadline = std::time::Instant::now() + AX_DEADLINE;
    for _ in 0..spawned {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok((pid, SendBox(res))) => {
                out.insert(pid, res);
            }
            Err(_) => break,
        }
    }
    for pid in pids {
        out.entry(pid).or_insert(None);
    }
    out
}

/// Space ids currently visible on any display (None if the SPI is unavailable).
fn current_space_ids(cid: c_int) -> Option<HashSet<i64>> {
    let f = sky().copy_managed_display_spaces?;
    let raw = unsafe { f(cid) };
    let raw = NonNull::new(raw as *mut CFArray)?;
    let arr: CFRetained<CFArray> = unsafe { CFRetained::from_raw(raw) };
    let arr: CFRetained<CFArray<CfDict>> = unsafe { CFRetained::cast_unchecked(arr) };
    let key_cur = CFString::from_str("Current Space");
    let key_id = CFString::from_str("ManagedSpaceID");
    let key_id64 = CFString::from_str("id64");
    let mut out = HashSet::new();
    for disp in arr.iter() {
        let Some(cur) = dict_get(&disp, &key_cur).and_then(|c| c.downcast_ref::<CFDictionary>()) else { continue };
        let cur: &CfDict = unsafe { &*(cur as *const CFDictionary as *const CfDict) };
        if let Some(id) = num_i64(dict_get(cur, &key_id64)).or_else(|| num_i64(dict_get(cur, &key_id))) {
            out.insert(id);
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Spaces a window belongs to (empty when the window is on no Space, e.g. ordered out).
fn spaces_for_window(cid: c_int, wid: u32) -> Option<Vec<i64>> {
    let f = sky().copy_spaces_for_windows?;
    let n = CFNumber::new_i64(wid as i64);
    let arr = CFArray::from_objects(&[&*n]);
    let raw = unsafe { f(cid, 0x7, arr.as_opaque()) };
    let raw = NonNull::new(raw as *mut CFArray)?;
    let res: CFRetained<CFArray> = unsafe { CFRetained::from_raw(raw) };
    let res: CFRetained<CFArray<CFNumber>> = unsafe { CFRetained::cast_unchecked(res) };
    Some(res.iter().filter_map(|n| n.as_i64()).collect())
}

/// Extra checks for windows that only CG knows about and that are not on screen.
fn plausible_offscreen(c: &CgWin, titles_readable: bool) -> bool {
    if c.bounds.2 < 100.0 || c.bounds.3 < 100.0 {
        return false;
    }
    !(titles_readable && c.name.as_deref().unwrap_or("").is_empty())
}

fn is_window_subrole(s: &str) -> bool {
    s == "AXStandardWindow" || s == "AXDialog"
}

/// All user-facing windows of regular apps. See module docs. Call on the main thread.
pub fn list_windows() -> Vec<WinInfo> {
    let apps = regular_apps();
    let cands = cg_candidates(&apps);
    let trusted = ax_trusted();
    let titles_readable = screen_recording_granted();

    let cid = sky().main_connection_id.map(|f| unsafe { f() });
    let current = cid.and_then(current_space_ids);

    // Space membership: (on a visible Space, on any Space).
    let space_info: Vec<(bool, Option<bool>)> = cands
        .iter()
        .map(|c| {
            let spaces = cid.and_then(|cid| spaces_for_window(cid, c.wid));
            let on_current = match (&spaces, &current) {
                (Some(sp), Some(cur)) => sp.iter().any(|s| cur.contains(s)),
                _ => c.onscreen_rank.is_some(),
            };
            (on_current, spaces.map(|s| !s.is_empty()))
        })
        .collect();

    // AX windows per pid that owns at least one candidate (queried in parallel, with a deadline).
    // Off-screen windows on another Space are resolved through the remote-token sweep.
    let mut ax: HashMap<i32, Option<HashMap<u32, AxWin>>> = HashMap::new();
    if trusted {
        let mut jobs: HashMap<i32, Vec<u32>> = HashMap::new();
        for (c, &(on_current, on_some)) in cands.iter().zip(&space_info) {
            let extra = jobs.entry(c.pid).or_default();
            if c.onscreen_rank.is_none() && !on_current && on_some == Some(true) {
                extra.push(c.wid);
            }
        }
        ax = ax_windows_parallel(jobs);
    }

    // (sort key, WinInfo); sort key: on-screen rank, then OFF + global rank.
    const OFF: usize = 1 << 32;
    let mut out: Vec<(usize, WinInfo)> = Vec::new();
    for (c, (on_current, on_some_space)) in cands.into_iter().zip(space_info) {
        let meta = &apps[&c.pid];
        // An empty AXWindows answer while CG shows windows means AX is broken for this app
        // (some Java / game / Electron builds): use the CG-only heuristics instead.
        let app_ax = ax.get(&c.pid).and_then(|o| o.as_ref()).filter(|m| !m.is_empty());

        let (title, minimized, el) = match app_ax {
            Some(map) => match map.get(&c.wid) {
                Some(w) => {
                    // AX knows the window: trust its subrole when it has one.
                    match w.subrole.as_deref() {
                        // Missing subrole (attribute error) = no AX info: keep, decided by CG filters.
                        Some(s) if !is_window_subrole(s) => continue,
                        _ => {}
                    }
                    (w.title.clone().or(c.name).unwrap_or_default(), w.minimized, Some(w.el.clone()))
                }
                None => {
                    // AX lists current-Space, minimized and hidden-app windows; anything else that
                    // is real must live on another Space.
                    if c.onscreen_rank.is_some() || on_current || on_some_space != Some(true) {
                        continue;
                    }
                    if !plausible_offscreen(&c, titles_readable) {
                        continue;
                    }
                    (c.name.unwrap_or_default(), false, None)
                }
            },
            None => {
                // No AX for this app: CG-only heuristics. Off-screen windows on a visible Space are
                // ambiguous without AX (minimized vs. ordered-out helper windows look identical in
                // CG), so only keep them for hidden apps; keep windows on other Spaces.
                if c.onscreen_rank.is_none() {
                    if on_some_space != Some(true) || (on_current && !meta.hidden) {
                        continue;
                    }
                    if !plausible_offscreen(&c, titles_readable) {
                        continue;
                    }
                }
                (c.name.unwrap_or_default(), false, None)
            }
        };

        let key = match c.onscreen_rank {
            Some(r) => r,
            None => OFF + c.all_rank,
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
                ax: el,
            },
        ));
    }

    // Dense z ranks.
    out.sort_by_key(|(k, _)| *k);
    let mut wins: Vec<WinInfo> = out
        .into_iter()
        .enumerate()
        .map(|(i, (_, mut w))| {
            w.z_index = i;
            w
        })
        .collect();

    // Group by app, apps ordered by their frontmost window, windows by z.
    let mut app_rank: HashMap<i32, usize> = HashMap::new();
    for w in &wins {
        app_rank.entry(w.pid).or_insert(w.z_index);
    }
    wins.sort_by_key(|w| (app_rank[&w.pid], w.z_index));
    wins
}

// ---------------------------------------------------------------------------------------------
// Focusing
// ---------------------------------------------------------------------------------------------

/// Two 0xf8-byte event records that make `wid` the key window of the process `psn`.
fn make_key_window(psn: &Psn, wid: u32) -> bool {
    let Some(post) = sky().post_event_record else { return false };
    let mut bytes = [0u8; 0xf8];
    bytes[0x04] = 0xf8;
    bytes[0x08] = 0x01;
    bytes[0x3a] = 0x10;
    bytes[0x3c..0x40].copy_from_slice(&wid.to_ne_bytes());
    bytes[0x20..0x30].fill(0xff);
    let r1 = unsafe { post(psn, bytes.as_ptr()) };
    bytes[0x08] = 0x02;
    let r2 = unsafe { post(psn, bytes.as_ptr()) };
    r1 == 0 && r2 == 0
}

fn ax_set_bool(el: &AXUIElement, attr: &str, v: bool) -> bool {
    let key = CFString::from_str(attr);
    unsafe { el.set_attribute_value(&key, CFBoolean::new(v)) == AXError::Success }
}

fn ax_raise(el: &AXUIElement) -> bool {
    let action = CFString::from_str("AXRaise");
    unsafe { el.perform_action(&action) == AXError::Success }
}

/// Re-resolve the AX element of `wid` through the app's `AXWindows` list.
fn find_ax_element(pid: i32, wid: u32) -> Option<CFRetained<AXUIElement>> {
    ax_windows(pid)?.remove(&wid).map(|w| w.el)
}

/// Find the AX element of `wid` even when `AXWindows` does not list it (other Space).
fn find_ax_element_by_token(pid: i32, wid: u32, budget: std::time::Duration) -> Option<CFRetained<AXUIElement>> {
    let mut wanted = HashSet::from([wid]);
    token_sweep(pid, &mut wanted, budget).pop().map(|(_, el)| el)
}

/// Bring that specific window to the front with keyboard focus. Unhides its app and unminimizes
/// it as needed.
///
/// Synchronously: unhide, unminimize (AX), `_SLPSSetFrontProcessWithOptions` + make-key-window
/// event records. Then `AXRaise` runs on a detached thread (it can block up to the 0.25 s AX
/// timeout while the target app animates). SLPS alone only activates the app; the window is
/// actually ordered front by AXRaise, so without Accessibility (or when no AX element can be
/// found) the app activates but the window may stay behind.
///
/// Returns true when SLPS accepted the request (or the fallback activation succeeded). `true`
/// therefore means "requested", not "verified frontmost"; the raise lands asynchronously.
pub fn focus_window(w: &WinInfo) -> bool {
    focus_impl(w, true)
}

/// Diagnostics: focus using only the SkyLight SPI (no AX unminimize / raise).
#[doc(hidden)]
#[allow(dead_code)]
pub fn focus_window_without_ax(w: &WinInfo) -> bool {
    focus_impl(w, false)
}

fn focus_impl(w: &WinInfo, use_ax: bool) -> bool {
    let t0 = std::time::Instant::now();
    let mark = |what: &str| {
        let line = format!("{what}@{:.1}ms ", t0.elapsed().as_secs_f64() * 1000.0);
        FOCUS_TRACE.with(|t| t.borrow_mut().push_str(&line));
    };
    FOCUS_TRACE.with(|t| t.borrow_mut().clear());
    let app = NSRunningApplication::runningApplicationWithProcessIdentifier(w.pid);
    let Some(app) = app else { return false };
    if app.isTerminated() {
        return false;
    }
    if w.hidden_app || app.isHidden() {
        app.unhide();
    }

    let trusted = use_ax && ax_trusted();
    let el = if !use_ax { None } else { match (&w.ax, trusted) {
        (Some(el), _) => Some(el.clone()),
        (None, true) => find_ax_element(w.pid, w.wid),
        (None, false) => None,
    } };
    if let Some(el) = &el {
        unsafe { el.set_messaging_timeout(AX_TIMEOUT_S) };
        let minimized = ax_attr(el, "AXMinimized")
            .and_then(|v| v.downcast_ref::<CFBoolean>().map(|b| b.as_bool()))
            .unwrap_or(w.minimized);
        mark("read_min");
        if minimized {
            ax_set_bool(el, "AXMinimized", false);
            mark("unminimize");
        }
    }

    let mut ok = false;
    let mut psn = Psn::default();
    if let Some(front) = sky().set_front_process
        && unsafe { GetProcessForPID(w.pid, &mut psn) } == 0
    {
        let r = unsafe { front(&psn, w.wid, SLPS_USER_GENERATED) };
        ok = r == 0;
        mark("slps");
        if ok {
            make_key_window(&psn, w.wid);
            mark("key");
        }
    }
    if ok {
        // AXRaise orders the window above its siblings. It blocks until the target app answers,
        // which during activation / de-minimize animations is often the full messaging timeout,
        // so it runs on a detached thread instead of stalling the caller (main thread).
        // Without an element (window on another Space, not in AXWindows) the thread probes
        // remote tokens for it first; SLPS alone activates the app but does not raise the window.
        if use_ax && (el.is_some() || trusted) {
            let boxed = SendBox(el);
            let (pid, wid) = (w.pid, w.wid);
            // A spawn failure is ignored: SLPS already fronted the app.
            let _ = std::thread::Builder::new().name("ztab-raise".into()).spawn(move || {
                let el = boxed.into_inner().or_else(|| {
                    find_ax_element_by_token(pid, wid, std::time::Duration::from_millis(150))
                });
                if let Some(el) = el {
                    ax_raise(&el);
                }
            });
        }
        mark("raise_dispatched");
        return true;
    }

    // Fallback: public activation + AX raise/main (synchronous).
    let activated = app.activateWithOptions(NSApplicationActivationOptions::empty());
    let mut raised = false;
    if let Some(el) = &el {
        raised = ax_raise(el);
        ax_set_bool(el, "AXMain", true);
    }
    mark("fallback");
    activated || raised
}

thread_local! {
    static FOCUS_TRACE: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// Diagnostics: run the remote-token lookup for `wid` and report (found, elapsed ms).
#[doc(hidden)]
#[allow(dead_code)]
pub fn debug_token_lookup(pid: i32, wid: u32) -> (bool, f64) {
    let t = std::time::Instant::now();
    let found = find_ax_element_by_token(pid, wid, std::time::Duration::from_millis(500)).is_some();
    (found, t.elapsed().as_secs_f64() * 1000.0)
}

/// Diagnostics: step timings of the last `focus_window` call on this thread.
#[doc(hidden)]
#[allow(dead_code)]
pub fn last_focus_trace() -> String {
    FOCUS_TRACE.with(|t| t.borrow().clone())
}

/// Diagnostics: every layer-0 CG window of regular apps with raw rank / Space info.
#[doc(hidden)]
#[allow(dead_code)]
pub fn debug_candidates() -> Vec<String> {
    let apps = regular_apps();
    let cid = sky().main_connection_id.map(|f| unsafe { f() });
    let current = cid.and_then(current_space_ids);
    let mut v = vec![format!("cid={cid:?} current_spaces={current:?}")];
    for c in cg_candidates(&apps) {
        let spaces = cid.and_then(|cid| spaces_for_window(cid, c.wid));
        v.push(format!(
            "wid={} pid={} app={} onscreen_rank={:?} all_rank={} spaces={:?} bounds={:?} name={:?}",
            c.wid, c.pid, apps[&c.pid].name, c.onscreen_rank, c.all_rank, spaces, c.bounds, c.name
        ));
    }
    v
}

/// Diagnostics: time each AX per-app query (ms).
#[doc(hidden)]
#[allow(dead_code)]
pub fn debug_ax_timing() -> Vec<String> {
    let apps = regular_apps();
    let mut pids: Vec<i32> = cg_candidates(&apps).iter().map(|c| c.pid).collect();
    pids.dedup();
    let mut seen = HashSet::new();
    pids.retain(|p| seen.insert(*p));
    pids.iter()
        .map(|&pid| {
            let t = std::time::Instant::now();
            let r = ax_windows(pid);
            format!("ax pid={} app={} n={:?} ms={:.2}", pid, apps[&pid].name, r.map(|m| m.len()), t.elapsed().as_secs_f64() * 1000.0)
        })
        .collect()
}

/// CGWindowID of the window that has keyboard focus system-wide (AX `AXFocusedApplication` ->
/// `AXFocusedWindow`). Needs Accessibility; `None` otherwise.
#[allow(dead_code)]
pub fn focused_window_id() -> Option<u32> {
    let sys = unsafe { AXUIElement::new_system_wide() };
    unsafe { sys.set_messaging_timeout(AX_TIMEOUT_S) };
    let app = ax_attr(&sys, "AXFocusedApplication")?.downcast::<AXUIElement>().ok()?;
    unsafe { app.set_messaging_timeout(AX_TIMEOUT_S) };
    let win = ax_attr(&app, "AXFocusedWindow")?.downcast::<AXUIElement>().ok()?;
    let mut wid = 0u32;
    (unsafe { _AXUIElementGetWindow(&win, &mut wid) } == AXError::Success && wid != 0).then_some(wid)
}
