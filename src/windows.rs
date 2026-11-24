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
use std::sync::atomic::{AtomicU64, Ordering};

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
pub(crate) const MIN_SIZE: f64 = 50.0;
/// Per-element AX messaging timeout, so a hung app cannot freeze the caller.
pub(crate) const AX_TIMEOUT_S: f32 = 0.25;
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
    /// AX element of the window, when Accessibility is granted and the app listed it
    /// (crate-visible so the window model can attach its cached handle).
    pub(crate) ax: Option<CFRetained<AXUIElement>>,
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

pub(crate) type CfDict = CFDictionary<CFString, CFType>;

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

pub(crate) fn cg_window_list(option: CGWindowListOption) -> Vec<CFRetained<CfDict>> {
    let Some(arr) = CGWindowListCopyWindowInfo(option, 0) else { return Vec::new() };
    // SAFETY: CGWindowListCopyWindowInfo returns a CFArray of CFDictionary<CFString, CFType>.
    let arr: CFRetained<CFArray<CfDict>> = unsafe { CFRetained::cast_unchecked(arr) };
    arr.to_vec()
}

/// Window ids only (`CGWindowListCreate`), front to back. About 10x cheaper than
/// `CGWindowListCopyWindowInfo` because no dictionaries are built.
#[allow(dead_code)] // used by model.rs
pub(crate) fn cg_window_ids(option: CGWindowListOption) -> Vec<u32> {
    let Some(arr) = objc2_core_graphics::CGWindowListCreate(option, 0) else { return Vec::new() };
    // SAFETY: CGWindowListCreate returns a CFArray whose values are CGWindowIDs stored directly in
    // the pointer-sized slots (NULL callbacks), so each value is read as an integer.
    (0..arr.count()).map(|i| unsafe { arr.value_at_index(i) } as usize as u32).collect()
}

/// CG descriptions of specific windows (`CGWindowListCreateDescriptionFromArray`). Windows that no
/// longer exist are omitted.
#[allow(dead_code)] // used by model.rs
pub(crate) fn cg_describe(wids: &[u32]) -> Vec<CFRetained<CfDict>> {
    if wids.is_empty() {
        return Vec::new();
    }
    let mut values: Vec<*const c_void> = wids.iter().map(|&w| w as usize as *const c_void).collect();
    // SAFETY: the input array holds raw CGWindowIDs with NULL callbacks, as the API expects.
    let Some(input) = (unsafe { CFArray::new(None, values.as_mut_ptr(), values.len() as _, std::ptr::null()) }) else {
        return Vec::new();
    };
    let Some(arr) = (unsafe { objc2_core_graphics::CGWindowListCreateDescriptionFromArray(Some(&input)) }) else {
        return Vec::new();
    };
    // SAFETY: the result is a CFArray of CFDictionary<CFString, CFType>.
    let arr: CFRetained<CFArray<CfDict>> = unsafe { CFRetained::cast_unchecked(arr) };
    arr.to_vec()
}

// ---------------------------------------------------------------------------------------------
// Enumeration
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub(crate) struct AppMeta {
    pub(crate) name: String,
    pub(crate) bundle_id: Option<String>,
    pub(crate) hidden: bool,
}

pub(crate) struct CgWin {
    pub(crate) wid: u32,
    pub(crate) pid: i32,
    pub(crate) bounds: (f64, f64, f64, f64),
    pub(crate) name: Option<String>,
    /// Rank in the on-screen front-to-back list (among layer-0 windows of regular apps).
    pub(crate) onscreen_rank: Option<usize>,
    /// Rank in the full list (window-server order, includes off-screen windows).
    pub(crate) all_rank: usize,
}

struct AxWin {
    el: CFRetained<AXUIElement>,
    title: Option<String>,
    subrole: Option<String>,
    minimized: bool,
}

/// Regular (Dock) apps except this process and the Dock, from NSWorkspace (no IPC to the apps).
pub(crate) fn regular_apps() -> HashMap<i32, AppMeta> {
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
pub(crate) type CgFields = (u32, i32, i64, (f64, f64, f64, f64), f64, Option<String>);

pub(crate) fn parse_cg(d: &CfDict) -> Option<CgFields> {
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
pub(crate) fn cg_candidates(apps: &HashMap<i32, AppMeta>) -> Vec<CgWin> {
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

pub(crate) fn ax_attr(el: &AXUIElement, attr: &str) -> Option<CFRetained<CFType>> {
    ax_attr_res(el, attr).ok()
}

/// Like `ax_attr`, keeping the error (`CannotComplete` = timeout / app busy,
/// `InvalidUIElement` = element destroyed, `NoValue` = attribute empty).
pub(crate) fn ax_attr_res(el: &AXUIElement, attr: &str) -> Result<CFRetained<CFType>, AXError> {
    let key = CFString::from_str(attr);
    let mut value: *const CFType = std::ptr::null();
    let err = unsafe { el.copy_attribute_value(&key, NonNull::from(&mut value)) };
    if err != AXError::Success {
        return Err(err);
    }
    if value.is_null() {
        return Err(AXError::NoValue);
    }
    Ok(unsafe { CFRetained::from_raw(NonNull::new_unchecked(value as *mut CFType)) })
}

/// CGWindowID of an AX window element (`_AXUIElementGetWindow`).
pub(crate) fn ax_window_id(el: &AXUIElement) -> Option<u32> {
    let mut wid = 0u32;
    (unsafe { _AXUIElementGetWindow(el, &mut wid) } == AXError::Success && wid != 0).then_some(wid)
}

/// `AXWindows` of an application element, as (CGWindowID, element). Elements without a window id
/// are skipped. `Err` when the app did not answer (timeout, not ready, no AX support).
pub(crate) fn ax_window_list(app: &AXUIElement) -> Result<Vec<(u32, CFRetained<AXUIElement>)>, AXError> {
    let wins = ax_attr_res(app, "AXWindows")?;
    let wins = wins.downcast::<CFArray>().map_err(|_| AXError::Failure)?;
    // SAFETY: AXWindows is an array of AXUIElement.
    let wins: CFRetained<CFArray<AXUIElement>> = unsafe { CFRetained::cast_unchecked(wins) };
    Ok(wins.iter().filter_map(|el| ax_window_id(&el).map(|wid| (wid, el))).collect())
}

/// Keys for `read_ax_attrs` (title, subrole, minimized, role), built once per caller.
pub(crate) fn ax_win_keys() -> CFRetained<CFArray> {
    let keys = [
        CFString::from_str("AXTitle"),
        CFString::from_str("AXSubrole"),
        CFString::from_str("AXMinimized"),
        CFString::from_str("AXRole"),
    ];
    let key_refs: Vec<&CFString> = keys.iter().map(|k| &**k).collect();
    let arr = CFArray::from_objects(&key_refs);
    // SAFETY: erasing the element type only.
    unsafe { CFRetained::cast_unchecked(arr) }
}

/// Title / subrole / minimized of one AX window. A `None` field means that attribute failed (keep
/// any cached value); `Err` means the whole request failed (`CannotComplete`: app busy or hung,
/// `InvalidUIElement`: window gone).
#[derive(Clone, Debug, Default)]
pub(crate) struct AxAttrs {
    pub(crate) title: Option<String>,
    pub(crate) subrole: Option<String>,
    pub(crate) minimized: Option<bool>,
    #[allow(dead_code)] // used by model.rs
    pub(crate) role: Option<String>,
}

/// Read title / subrole / minimized of one AX window in a single round trip. Uses the element's
/// current messaging timeout.
pub(crate) fn read_ax_attrs(el: &AXUIElement, key_arr: &CFArray) -> Result<AxAttrs, AXError> {
    let mut values: *const CFArray = std::ptr::null();
    let err = unsafe {
        el.copy_multiple_attribute_values(
            key_arr,
            objc2_application_services::AXCopyMultipleAttributeOptions(0),
            NonNull::from(&mut values),
        )
    };
    if err != AXError::Success {
        return Err(err);
    }
    if values.is_null() {
        return Err(AXError::NoValue);
    }
    let vals: CFRetained<CFArray<CFType>> =
        unsafe { CFRetained::from_raw(NonNull::new_unchecked(values as *mut CFArray<CFType>)) };
    let v: Vec<CFRetained<CFType>> = vals.to_vec();
    // Failed attributes come back as AXValue(kAXValueAXErrorType); downcasts reject them.
    let ok = |i: usize| v.get(i).filter(|x| x.downcast_ref::<AXValue>().is_none());
    // Every attribute failing in the same way usually means the element is gone or the app is
    // stuck; report that as a whole-request failure so callers keep their cached values.
    if (0..4).all(|i| ok(i).is_none()) {
        return Err(AXError::CannotComplete);
    }
    Ok(AxAttrs {
        title: ok(0).and_then(|x| x.downcast_ref::<CFString>().map(|s| s.to_string())),
        subrole: ok(1).and_then(|x| x.downcast_ref::<CFString>().map(|s| s.to_string())),
        minimized: ok(2).and_then(|x| x.downcast_ref::<CFBoolean>().map(|b| b.as_bool())),
        role: ok(3).and_then(|x| x.downcast_ref::<CFString>().map(|s| s.to_string())),
    })
}

/// AX windows of one app keyed by CGWindowID; `None` when AX could not be queried.
fn ax_windows(pid: i32) -> Option<HashMap<u32, AxWin>> {
    ax_windows_with_extra(pid, &[])
}

/// Read title / subrole / minimized of one AX window in a single round trip.
fn read_ax_win(el: CFRetained<AXUIElement>, key_arr: &CFArray) -> AxWin {
    unsafe { el.set_messaging_timeout(AX_TIMEOUT_S) };
    let a = read_ax_attrs(&el, key_arr).unwrap_or_default();
    AxWin { el, title: a.title, subrole: a.subrole, minimized: a.minimized.unwrap_or(false) }
}

/// AX windows of one app keyed by CGWindowID; `None` when AX could not be queried.
/// `extra` are window ids (normally on other Spaces) to resolve through the remote-token sweep
/// when `AXWindows` does not list them.
fn ax_windows_with_extra(pid: i32, extra: &[u32]) -> Option<HashMap<u32, AxWin>> {
    let app = unsafe { AXUIElement::new_application(pid) };
    unsafe { app.set_messaging_timeout(AX_TIMEOUT_S) };
    let wins = ax_window_list(&app).ok()?;
    let key_arr = ax_win_keys();
    let mut out = HashMap::new();
    for (wid, el) in wins {
        out.insert(wid, read_ax_win(el, &key_arr));
    }
    let mut wanted: HashSet<u32> = extra.iter().copied().filter(|w| !out.contains_key(w)).collect();
    if !wanted.is_empty() {
        for (wid, el) in token_sweep(pid, &mut wanted, TOKEN_BUDGET) {
            out.insert(wid, read_ax_win(el, &key_arr));
        }
    }
    Some(out)
}

/// Probe remote tokens `(pid, 0, 'coco', id)` for element ids 0..1000 (AltTab's technique) and
/// return the elements whose CGWindowID is in `wanted` (found ids are removed from it).
/// This reaches windows on other Spaces, which `AXWindows` omits. Bounded by `budget`.
pub(crate) fn token_sweep(pid: i32, wanted: &mut HashSet<u32>, budget: std::time::Duration) -> Vec<(u32, CFRetained<AXUIElement>)> {
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
        if let Some(wid) = ax_window_id(&el)
            && wanted.remove(&wid)
        {
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

/// SkyLight main connection id (None if the SPI is unavailable).
pub(crate) fn main_cid() -> Option<c_int> {
    sky().main_connection_id.map(|f| unsafe { f() })
}

/// Space ids currently visible on any display (None if the SPI is unavailable).
pub(crate) fn current_space_ids(cid: c_int) -> Option<HashSet<i64>> {
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
pub(crate) fn spaces_for_window(cid: c_int, wid: u32) -> Option<Vec<i64>> {
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
pub(crate) fn plausible_offscreen(c: &CgWin, titles_readable: bool) -> bool {
    if c.bounds.2 < 100.0 || c.bounds.3 < 100.0 {
        return false;
    }
    !(titles_readable && c.name.as_deref().unwrap_or("").is_empty())
}

pub(crate) fn is_window_subrole(s: &str) -> bool {
    s == "AXStandardWindow" || s == "AXDialog"
}

/// Space membership of each candidate: (on a visible Space, on any Space). On-screen windows are
/// on a visible Space by definition, so the SkyLight query only runs for off-screen ones.
pub(crate) fn space_membership(cands: &[CgWin]) -> Vec<(bool, Option<bool>)> {
    let cid = main_cid();
    let mut current: Option<Option<HashSet<i64>>> = None;
    cands
        .iter()
        .map(|c| {
            if c.onscreen_rank.is_some() {
                return (true, Some(true));
            }
            let cur = current.get_or_insert_with(|| cid.and_then(current_space_ids));
            let spaces = cid.and_then(|cid| spaces_for_window(cid, c.wid));
            let on_current = match (&spaces, &*cur) {
                (Some(sp), Some(cur)) => sp.iter().any(|s| cur.contains(s)),
                _ => false,
            };
            (on_current, spaces.map(|s| !s.is_empty()))
        })
        .collect()
}

/// All user-facing windows of regular apps. See module docs. Call on the main thread.
pub fn list_windows() -> Vec<WinInfo> {
    let apps = regular_apps();
    let cands = cg_candidates(&apps);
    let trusted = ax_trusted();
    let titles_readable = screen_recording_granted();

    let space_info = space_membership(&cands);

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

fn ax_set_bool(el: &AXUIElement, attr: &str, v: bool) -> AXError {
    let key = CFString::from_str(attr);
    unsafe { el.set_attribute_value(&key, CFBoolean::new(v)) }
}

fn ax_raise(el: &AXUIElement) -> AXError {
    let action = CFString::from_str("AXRaise");
    unsafe { el.perform_action(&action) }
}

/// Find the AX element of `wid` even when `AXWindows` does not list it (other Space).
fn find_ax_element_by_token(pid: i32, wid: u32, budget: std::time::Duration) -> Option<CFRetained<AXUIElement>> {
    let mut wanted = HashSet::from([wid]);
    token_sweep(pid, &mut wanted, budget).pop().map(|(_, el)| el)
}

/// AX element of a window the caller has no handle for: the app's `AXWindows` list (one request,
/// plus a window-id lookup per element, no attribute reads), then the remote-token sweep for a
/// window on another Space. `current()` is checked between the two.
fn lookup_ax_element(pid: i32, wid: u32, current: &dyn Fn() -> bool) -> Option<CFRetained<AXUIElement>> {
    let app = unsafe { AXUIElement::new_application(pid) };
    unsafe { app.set_messaging_timeout(AX_TIMEOUT_S) };
    if let Ok(list) = ax_window_list(&app)
        && let Some((_, el)) = list.into_iter().find(|(w, _)| *w == wid)
    {
        unsafe { el.set_messaging_timeout(AX_TIMEOUT_S) };
        return Some(el);
    }
    if !current() {
        return None;
    }
    find_ax_element_by_token(pid, wid, std::time::Duration::from_millis(150))
}

fn mark(t0: std::time::Instant, what: &str) {
    let line = format!("{what}@{:.1}ms ", t0.elapsed().as_secs_f64() * 1000.0);
    FOCUS_TRACE.with(|t| t.borrow_mut().push_str(&line));
}

/// How `front_window` brought the app forward.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fronted {
    /// `_SLPSSetFrontProcessWithOptions` with the window id, plus the make-key-window records.
    Slps,
    /// SLPS unavailable or refused: public `activateWithOptions` (`true` = it succeeded).
    Fallback(bool),
}

/// The part of a switch that never waits on the target app: unhide its app when hidden, then
/// `_SLPSSetFrontProcessWithOptions` + make-key-window event records for `w.wid` (WindowServer
/// requests), or the public activation when the SPI is missing. `None` when the app is gone.
fn front_window(w: &WinInfo) -> Option<Fronted> {
    let t0 = std::time::Instant::now();
    let app = NSRunningApplication::runningApplicationWithProcessIdentifier(w.pid)?;
    if app.isTerminated() {
        return None;
    }
    if w.hidden_app || app.isHidden() {
        app.unhide();
    }
    let mut psn = Psn::default();
    if let Some(front) = sky().set_front_process
        && unsafe { GetProcessForPID(w.pid, &mut psn) } == 0
    {
        let r = unsafe { front(&psn, w.wid, SLPS_USER_GENERATED) };
        mark(t0, "slps");
        if r == 0 {
            make_key_window(&psn, w.wid);
            mark(t0, "key");
            return Some(Fronted::Slps);
        }
    }
    let activated = app.activateWithOptions(NSApplicationActivationOptions::empty());
    mark(t0, "fallback");
    Some(Fronted::Fallback(activated))
}

/// How a switch to one of ZTab's own windows is finished after it was fronted: `f(window id)`,
/// called on a raise thread; it must only queue the work to the main thread (which owns the
/// window), never wait for it.
static OWN_RAISE: std::sync::OnceLock<fn(u32)> = std::sync::OnceLock::new();

/// Set how switches to ZTab's own windows are finished (see `OWN_RAISE`). Once.
#[allow(dead_code)] // used by main.rs
pub fn set_own_window_raiser(f: fn(u32)) {
    let _ = OWN_RAISE.set(f);
}

/// What [`raise_window`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Raised {
    /// `raised`, `raise-failed`, `not-found`, `no-ax` or `superseded`.
    pub(crate) what: &'static str,
    /// An AX write (unminimize, `AXRaise`, `AXMain`) was sent while the request was current and
    /// returned after a newer switch had started fronting. The target app may have applied it
    /// after that switch (an AX timeout only stops the wait for the reply; the app still runs the
    /// request when it gets to it), so the newer switch may need fronting again.
    pub(crate) late: bool,
}

impl Raised {
    fn new(what: &'static str, late: bool) -> Raised {
        Raised { what, late }
    }
}

/// What the Accessibility step of a switch may ask about its own request.
pub(crate) struct ReqView<'a> {
    seq: u64,
    state: &'a FocusState,
}

impl ReqView<'_> {
    /// No newer switch has been fronted: the request may keep going. A newer request that is
    /// only queued, or whose app was gone, does not stop it.
    pub(crate) fn current(&self) -> bool {
        self.state.fronted.load(Ordering::SeqCst) == self.seq
    }

    /// A newer switch has started fronting (it may not have finished).
    pub(crate) fn overtaken(&self) -> bool {
        self.state.started.load(Ordering::SeqCst) > self.seq
    }

    /// The newest fronted switch is a newer one to app `pid` (its raise waits behind this one).
    fn newer_switch_to(&self, pid: i32) -> bool {
        self.state.latest().as_ref().is_some_and(|l| l.seq > self.seq && l.w.0.pid == pid)
    }
}

/// Late AX writes of one raise (see [`Raised::late`]).
#[derive(Default)]
struct LateWrites {
    /// A write returned after a newer switch started fronting.
    any: bool,
    /// One of them timed out: the app has not applied it yet, but may later.
    pending: bool,
}

/// One AX write of a raise: sent only while `view` is current. Returns its result (`None`: not
/// sent) and notes in `late` when it returned after a newer switch started fronting.
fn ax_write(view: &ReqView, late: &mut LateWrites, write: impl FnOnce() -> AXError) -> Option<AXError> {
    if !view.current() {
        return None;
    }
    let r = write();
    if view.overtaken() {
        late.any = true;
        late.pending |= r == AXError::CannotComplete;
    }
    Some(r)
}

/// A late write timed out, so the busy app applies it whenever it gets to it (an AX timeout only
/// stops the wait for the reply). An app handles AX requests in order: wait, probing it with a
/// cheap read, until it answers again (at most `LATE_WAIT`), so the re-front check that follows
/// runs after the write landed. Stops early when a newer switch to the same app is waiting.
fn wait_until_answering(el: &AXUIElement, view: &ReqView, pid: i32) {
    let end = std::time::Instant::now() + LATE_WAIT;
    while std::time::Instant::now() < end && !view.newer_switch_to(pid) {
        let t = std::time::Instant::now();
        match ax_attr_res(el, "AXRole") {
            Err(AXError::CannotComplete) => {
                if t.elapsed() < std::time::Duration::from_millis(100) {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
            _ => return,
        }
    }
}

/// The Accessibility part of a switch, after `front_window`: `AXRaise` (SLPS activates the app
/// but does not order the window above its siblings), unminimizing first when the model knows
/// the window is minimized, plus `AXMain` after a fallback activation. One `AXMinimized` read
/// after the raise (so it never delays it) covers an app whose minimize notification was missed:
/// then the window is unminimized and raised again. Uses the window's cached handle when it has
/// one (keeping that element's messaging timeout), otherwise looks the element up.
/// `view.current()` is checked before every AX request, so a switch that a newer, fronted one
/// replaced stops at its next step; a write already sent cannot be withdrawn, which `late`
/// reports (after waiting, when it timed out, until the busy app answers again: see
/// `wait_until_answering`). Returns what happened, for the log.
fn raise_window(w: &WinInfo, set_main: bool, view: &ReqView) -> Raised {
    let t0 = std::time::Instant::now();
    if !view.current() {
        return Raised::new("superseded", false);
    }
    // ZTab's own window: no Accessibility round trip to ourselves; the main thread orders it in.
    if w.pid == std::process::id() as i32
        && let Some(raise_own) = OWN_RAISE.get()
    {
        raise_own(w.wid);
        return Raised::new("raised (own window, main queue)", false);
    }
    let current = || view.current();
    let el = match &w.ax {
        Some(el) => el.clone(),
        None if !ax_trusted() => return Raised::new("no-ax", false),
        None => match lookup_ax_element(w.pid, w.wid, &current) {
            Some(el) => el,
            None => return Raised::new(if view.current() { "not-found" } else { "superseded" }, false),
        },
    };
    mark(t0, "element");
    let mut late = LateWrites::default();
    let raised = raise_steps(w, &el, set_main, view, &mut late, t0);
    if late.pending {
        wait_until_answering(&el, view, w.pid);
        mark(t0, "late-write-landed");
    }
    Raised::new(raised.unwrap_or("superseded"), late.any)
}

/// The AX writes of `raise_window`; `None` when it stopped because a newer switch was fronted.
fn raise_steps(w: &WinInfo, el: &AXUIElement, set_main: bool, view: &ReqView, late: &mut LateWrites, t0: std::time::Instant) -> Option<&'static str> {
    if w.minimized {
        ax_write(view, late, || ax_set_bool(el, "AXMinimized", false))?;
        mark(t0, "unminimize");
    }
    let mut r = ax_write(view, late, || ax_raise(el))?;
    mark(t0, "raise");
    // Skipped when the app did not answer the raise (busy: the read would only time out too).
    if !w.minimized && r != AXError::CannotComplete && view.current() {
        let minimized = ax_attr(el, "AXMinimized").and_then(|v| v.downcast_ref::<CFBoolean>().map(|b| b.as_bool())) == Some(true);
        mark(t0, "read_min");
        if minimized {
            ax_write(view, late, || ax_set_bool(el, "AXMinimized", false))?;
            r = ax_write(view, late, || ax_raise(el))?;
            mark(t0, "unminimize+raise");
        }
    }
    if set_main {
        ax_write(view, late, || ax_set_bool(el, "AXMain", true))?;
    }
    Some(if r == AXError::Success { "raised" } else { "raise-failed" })
}

/// Bring that specific window to the front with keyboard focus, synchronously on the calling
/// thread (diagnostics and tools; ZTab itself uses [`Focuser`]). Unhides its app and
/// unminimizes it as needed: `front_window`, then `raise_window`, which can block for the AX
/// messaging timeout while the target app is busy. Never call it on the main thread.
///
/// Returns true when SLPS accepted the request (or the fallback activation succeeded): "requested",
/// not "verified frontmost".
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
    FOCUS_TRACE.with(|t| t.borrow_mut().clear());
    let Some(f) = front_window(w) else { return false };
    let mut ok = f != Fronted::Fallback(false);
    if use_ax {
        // A private sequence in which this one request is the newest, fronted one.
        let state = FocusState::default();
        state.queued.store(1, Ordering::SeqCst);
        state.started.store(1, Ordering::SeqCst);
        state.fronted.store(1, Ordering::SeqCst);
        let r = raise_window(w, matches!(f, Fronted::Fallback(_)), &ReqView { seq: 1, state: &state });
        ok |= r.what == "raised";
    }
    ok
}

/// The topmost on-screen normal (layer 0) window of another process: (window id, pid).
fn top_window() -> Option<(u32, i32)> {
    let own = std::process::id() as i32;
    let list = cg_window_list(CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements);
    list.iter().find_map(|d| {
        let (wid, pid, layer, b, alpha, _) = parse_cg(d)?;
        (layer == 0 && alpha > 0.0 && pid != own && b.2 >= MIN_SIZE && b.3 >= MIN_SIZE).then_some((wid, pid))
    })
}

/// What happened to one [`Focuser::focus`] request (reported from the worker threads).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // used by main.rs
pub enum FocusStage {
    /// The app was fronted. `ok`: SLPS (or the fallback activation) accepted the request; it
    /// means "requested", not "verified frontmost".
    Fronted { ok: bool },
    /// Dropped before anything was fronted: a newer request replaced it.
    Superseded,
    /// The Accessibility tail ended: `raised`, `raise-failed`, `not-found`, `no-ax` or
    /// `superseded` (a newer switch was fronted before one of its AX calls).
    Raised(&'static str),
    /// An AX write of this (replaced) request returned after a newer switch started fronting;
    /// the newest switch is checked now and once more a little later.
    Late,
    /// Check of the newest switch after a late AX write of window `stale_wid`: `refronted` when
    /// that window (or its app) had come out on top and the newest window was fronted and raised
    /// again, else it was left alone.
    Refront { stale_wid: u32, refronted: bool },
}

type FocusReport = dyn Fn(&WinInfo, FocusStage, f64) + Send + Sync;
/// The fronting step (`front_window`); injectable for tests.
type FrontOp = dyn Fn(&WinInfo) -> Option<Fronted> + Send + Sync;
/// The Accessibility step (`raise_window`): window, set `AXMain`, the request's view.
type RaiseOp = dyn Fn(&WinInfo, bool, &ReqView) -> Raised + Send + Sync;
/// The topmost window (`top_window`); injectable for tests.
type TopOp = dyn Fn() -> Option<(u32, i32)> + Send + Sync;

/// A switch that a newer one replaced is checked for a late AX write this long after it (its
/// newest successor may need fronting again)...
const REFRONT_AGAIN_AFTER: std::time::Duration = std::time::Duration::from_millis(500);
/// ...but only while the newest switch is younger than this: later, the user may have moved on.
const REFRONT_WITHIN: std::time::Duration = std::time::Duration::from_secs(5);
/// A late write that timed out is waited for (the busy app probed until it answers) this long.
const LATE_WAIT: std::time::Duration = std::time::Duration::from_secs(3);
/// A per-app raise thread exits after this long without work.
const RAISE_WORKER_IDLE: std::time::Duration = std::time::Duration::from_secs(10);

struct FocusReq {
    seq: u64,
    at: std::time::Instant,
    w: SendBox<WinInfo>,
}

/// The Accessibility step of a fronted request, for its app's raise thread.
struct RaiseJob {
    seq: u64,
    at: std::time::Instant,
    w: SendBox<WinInfo>,
    set_main: bool,
}

enum FocusMsg {
    Switch(FocusReq),
    /// An AX write of request `stale_seq` (window `stale`) was late: check the newest switch.
    Refront { stale_seq: u64, stale: SendBox<WinInfo> },
}

/// The newest fronted request (for a re-front).
struct Latest {
    seq: u64,
    at: std::time::Instant,
    w: SendBox<WinInfo>,
    set_main: bool,
}

/// Sequence numbers of focus requests, shared by the caller and the worker threads. Every
/// request gets a higher number than the one before. `queued`: the newest request (the focus
/// thread skips older ones it has not fronted yet). `started` / `fronted`: the newest request
/// whose fronting started / succeeded. A raise stops once a newer request is fronted, so a
/// newer request whose app turned out to be gone leaves the previous switch to finish.
#[derive(Default)]
struct FocusState {
    queued: AtomicU64,
    started: AtomicU64,
    fronted: AtomicU64,
    latest: std::sync::Mutex<Option<Latest>>,
}

impl FocusState {
    fn latest(&self) -> std::sync::MutexGuard<'_, Option<Latest>> {
        self.latest.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Everything the focus thread and the raise threads share.
struct FocusCtx {
    state: FocusState,
    front: std::sync::Arc<FrontOp>,
    raise: std::sync::Arc<RaiseOp>,
    top: std::sync::Arc<TopOp>,
    report: std::sync::Arc<FocusReport>,
    /// The focus thread's queue (raise threads post re-front checks to it).
    tx: std::sync::mpsc::Sender<FocusMsg>,
    /// One raise thread per app with work: pid -> its queue.
    raisers: std::sync::Mutex<HashMap<i32, std::sync::mpsc::Sender<RaiseJob>>>,
}

fn ms_since(t: std::time::Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

impl FocusCtx {
    /// Focus thread: one batch of messages. The newest switch wins (older ones are reported
    /// skipped); re-front checks only matter when no newer switch is queued.
    fn handle(ctx: &FocusCtxRef, batch: Vec<FocusMsg>) {
        let mut switch: Option<FocusReq> = None;
        let mut refronts = Vec::new();
        for m in batch {
            match m {
                FocusMsg::Switch(r) => {
                    if let Some(old) = switch.replace(r) {
                        (ctx.report)(&old.w.0, FocusStage::Superseded, ms_since(old.at));
                    }
                }
                FocusMsg::Refront { stale_seq, stale } => refronts.push((stale_seq, stale)),
            }
        }
        FOCUS_TRACE.with(|t| t.borrow_mut().clear());
        match switch {
            Some(req) => Self::switch(ctx, req),
            None => {
                for (seq, stale) in refronts {
                    Self::refront(ctx, seq, &stale.0);
                }
            }
        }
    }

    fn switch(ctx: &FocusCtxRef, req: FocusReq) {
        let w = &req.w.0;
        if ctx.state.queued.load(Ordering::SeqCst) != req.seq {
            (ctx.report)(w, FocusStage::Superseded, ms_since(req.at));
            return;
        }
        ctx.state.started.store(req.seq, Ordering::SeqCst);
        match (ctx.front)(w) {
            None => (ctx.report)(w, FocusStage::Fronted { ok: false }, ms_since(req.at)),
            Some(f) => {
                let set_main = matches!(f, Fronted::Fallback(_));
                // Before the raise is handed off: from now on an older raise stops.
                ctx.state.fronted.store(req.seq, Ordering::SeqCst);
                *ctx.state.latest() = Some(Latest { seq: req.seq, at: req.at, w: SendBox(w.clone()), set_main });
                (ctx.report)(w, FocusStage::Fronted { ok: f != Fronted::Fallback(false) }, ms_since(req.at));
                Self::dispatch_raise(ctx, RaiseJob { seq: req.seq, at: req.at, w: req.w, set_main });
            }
        }
    }

    /// After a late AX write of request `stale_seq` (window `stale`): front and raise the newest
    /// switch again, but only when it is still the newest (nothing queued after it), recent, and
    /// the stale window, or its app when that is another app, is now the topmost window. So a
    /// re-front never takes the focus from anything the user chose since.
    fn refront(ctx: &FocusCtxRef, stale_seq: u64, stale: &WinInfo) {
        let Some((seq, at, w, set_main)) = ctx.state.latest().as_ref().map(|l| (l.seq, l.at, l.w.0.clone(), l.set_main)) else { return };
        if seq <= stale_seq
            || ctx.state.queued.load(Ordering::SeqCst) != seq
            || ctx.state.fronted.load(Ordering::SeqCst) != seq
            || at.elapsed() > REFRONT_WITHIN
        {
            return;
        }
        let hijacked = (ctx.top)().is_some_and(|(wid, pid)| wid == stale.wid || (pid == stale.pid && pid != w.pid));
        (ctx.report)(&w, FocusStage::Refront { stale_wid: stale.wid, refronted: hijacked }, ms_since(at));
        if hijacked && (ctx.front)(&w).is_some() {
            Self::dispatch_raise(ctx, RaiseJob { seq, at, w: SendBox(w), set_main });
        }
    }

    /// Hand `job` to its app's raise thread (started when the app has none), so a raise blocked
    /// on one busy app never holds up another app's raise; one app's raises stay in order.
    fn dispatch_raise(ctx: &FocusCtxRef, job: RaiseJob) {
        let pid = job.w.0.pid;
        let mut raisers = ctx.raisers.lock().unwrap_or_else(|e| e.into_inner());
        let job = match raisers.get(&pid) {
            Some(tx) => match tx.send(job) {
                Ok(()) => return,
                Err(e) => e.0,
            },
            None => job,
        };
        let (tx, rx) = std::sync::mpsc::channel::<RaiseJob>();
        let (w, at) = (job.w.0.clone(), job.at);
        let _ = tx.send(job);
        let me = ctx.clone();
        match std::thread::Builder::new().name(format!("ztab-raise-{pid}")).spawn(move || me.raise_thread(pid, rx)) {
            Ok(_) => {
                raisers.insert(pid, tx);
            }
            Err(_) => (ctx.report)(&w, FocusStage::Raised("no-thread"), ms_since(at)),
        }
    }

    /// Raise thread of app `pid`: one job at a time, skipping to the newest queued one; exits
    /// after `RAISE_WORKER_IDLE` without work.
    fn raise_thread(&self, pid: i32, rx: std::sync::mpsc::Receiver<RaiseJob>) {
        use std::sync::mpsc::RecvTimeoutError;
        loop {
            let mut job = match rx.recv_timeout(RAISE_WORKER_IDLE) {
                Ok(j) => j,
                Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {
                    // Jobs are only sent under this lock: nothing can arrive between the check
                    // and the removal.
                    let mut raisers = self.raisers.lock().unwrap_or_else(|e| e.into_inner());
                    match rx.try_recv() {
                        Ok(j) => j,
                        Err(_) => {
                            raisers.remove(&pid);
                            return;
                        }
                    }
                }
            };
            // A newer job of the same app was fronted after this one: this one is replaced.
            while let Ok(newer) = rx.try_recv() {
                (self.report)(&job.w.0, FocusStage::Raised("superseded"), ms_since(job.at));
                job = newer;
            }
            objc2::rc::autoreleasepool(|_| {
                FOCUS_TRACE.with(|t| t.borrow_mut().clear());
                let r = (self.raise)(&job.w.0, job.set_main, &ReqView { seq: job.seq, state: &self.state });
                (self.report)(&job.w.0, FocusStage::Raised(r.what), ms_since(job.at));
                if r.late {
                    (self.report)(&job.w.0, FocusStage::Late, ms_since(job.at));
                    let msg = || FocusMsg::Refront { stale_seq: job.seq, stale: SendBox(job.w.0.clone()) };
                    let _ = self.tx.send(msg());
                    let (tx, again) = (self.tx.clone(), msg());
                    let _ = std::thread::Builder::new().name("ztab-refront".into()).spawn(move || {
                        std::thread::sleep(REFRONT_AGAIN_AFTER);
                        let _ = tx.send(again);
                    });
                }
            });
        }
    }
}

/// `FocusCtx` behind an `Arc`, so raise threads can keep it.
type FocusCtxRef = std::sync::Arc<FocusCtx>;

/// Window switching off the main thread.
///
/// [`focus`](Self::focus) only queues the request. A `ztab-focus` thread handles requests one at
/// a time: it skips every request that a newer one has replaced, then unhides the app and fronts
/// the window through SkyLight (WindowServer requests only, so a busy app cannot delay it). It
/// then hands the Accessibility tail (`AXRaise`, unminimize) to a `ztab-raise-<pid>` thread of
/// that app, so a raise blocked on a busy app never delays another app's raise, and one app's
/// raises stay in order. A raise re-checks before each AX call and stops once a newer switch has
/// been fronted; a newer switch whose app is gone does not stop it. A raise can never be
/// withdrawn once sent, though: a busy app may apply an older switch's unminimize or `AXRaise`
/// after a newer switch. When such a write returns late (or times out, and the app answers again
/// within 3 s), the newest switch is fronted and raised again if the stale window (or its app)
/// came out on top; this is checked then and 0.5 s later, within 5 s of the newest switch. A
/// write the app applies even later than that is not covered.
#[allow(dead_code)] // used by main.rs
pub struct Focuser {
    ctx: FocusCtxRef,
}

#[allow(dead_code)] // used by main.rs
impl Focuser {
    /// Start the focus thread. `report(window, stage, ms since the request)` runs on the worker
    /// threads.
    pub fn new(report: impl Fn(&WinInfo, FocusStage, f64) + Send + Sync + 'static) -> Focuser {
        Self::with_ops(
            std::sync::Arc::new(front_window),
            std::sync::Arc::new(raise_window),
            std::sync::Arc::new(top_window),
            std::sync::Arc::new(report),
        )
    }

    fn with_ops(front: std::sync::Arc<FrontOp>, raise: std::sync::Arc<RaiseOp>, top: std::sync::Arc<TopOp>, report: std::sync::Arc<FocusReport>) -> Focuser {
        let (tx, rx) = std::sync::mpsc::channel::<FocusMsg>();
        let ctx = std::sync::Arc::new(FocusCtx {
            state: FocusState::default(),
            front,
            raise,
            top,
            report,
            tx,
            raisers: std::sync::Mutex::new(HashMap::new()),
        });
        let c2 = ctx.clone();
        let _ = std::thread::Builder::new().name("ztab-focus".into()).spawn(move || {
            while let Ok(first) = rx.recv() {
                let mut batch = vec![first];
                while let Ok(m) = rx.try_recv() {
                    batch.push(m);
                }
                objc2::rc::autoreleasepool(|_| FocusCtx::handle(&c2, batch));
            }
        });
        Focuser { ctx }
    }

    /// Queue a switch to `w` and return at once. Replaces every request not fronted yet.
    pub fn focus(&self, w: WinInfo) {
        let seq = self.ctx.state.queued.fetch_add(1, Ordering::SeqCst) + 1;
        let req = FocusReq { seq, at: std::time::Instant::now(), w: SendBox(w) };
        if let Err(std::sync::mpsc::SendError(FocusMsg::Switch(req))) = self.ctx.tx.send(FocusMsg::Switch(req)) {
            // The focus thread could not start: one-off thread (still never the caller's thread).
            let report = self.ctx.report.clone();
            let _ = std::thread::Builder::new().name("ztab-focus-1".into()).spawn(move || {
                let w = req.w.into_inner();
                let ok = focus_window(&w);
                report(&w, FocusStage::Fronted { ok }, ms_since(req.at));
            });
        }
    }
}

thread_local! {
    /// Step timings of the current request on this thread (cleared at the start of each one).
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
    let cid = main_cid();
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
    ax_window_id(&win)
}

#[cfg(test)]
mod focuser_tests {
    use super::*;
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::Duration;

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

    type Gates = Arc<Mutex<HashMap<(char, u32), (mpsc::Sender<()>, mpsc::Receiver<()>)>>>;

    /// A `Focuser` whose front / raise / topmost-window steps are fakes (never the real SkyLight,
    /// AX or CG calls). A step for a window with a gate blocks until the test opens it, like a
    /// busy app: 'f' the front, 'r' the raise before its write (an element lookup), 'w' the
    /// write itself. The fake raise behaves like `raise_window`: it checks `current()` before the
    /// write and cannot withdraw a write it sent; a write that returns after a newer switch
    /// started fronting is `late`. Windows with wid >= 100 belong to apps that are gone (front
    /// fails).
    struct Rig {
        focuser: Focuser,
        fronted: Arc<Mutex<Vec<u32>>>,
        raised: Arc<Mutex<Vec<u32>>>,
        gates: Gates,
        top: Arc<Mutex<Option<(u32, i32)>>>,
        reports: mpsc::Receiver<(u32, FocusStage)>,
    }

    impl Rig {
        fn new() -> Rig {
            let fronted = Arc::new(Mutex::new(Vec::new()));
            let raised = Arc::new(Mutex::new(Vec::new()));
            let gates: Gates = Arc::new(Mutex::new(HashMap::new()));
            let top = Arc::new(Mutex::new(None));
            let wait = |gates: &Gates, key| {
                let gate = gates.lock().unwrap().remove(&key);
                if let Some((entered, open)) = gate {
                    let _ = entered.send(());
                    let _ = open.recv_timeout(Duration::from_secs(5));
                }
            };
            let (f2, g2) = (fronted.clone(), gates.clone());
            let front: Arc<FrontOp> = Arc::new(move |w: &WinInfo| {
                wait(&g2, ('f', w.wid));
                if w.wid >= 100 {
                    return None;
                }
                f2.lock().unwrap().push(w.wid);
                Some(Fronted::Slps)
            });
            let (r2, g3) = (raised.clone(), gates.clone());
            let raise: Arc<RaiseOp> = Arc::new(move |w: &WinInfo, _set_main: bool, view: &ReqView| {
                if !view.current() {
                    return Raised::new("superseded", false);
                }
                wait(&g3, ('r', w.wid));
                if !view.current() {
                    return Raised::new("superseded", false);
                }
                wait(&g3, ('w', w.wid));
                r2.lock().unwrap().push(w.wid);
                Raised::new("raised", view.overtaken())
            });
            let t2 = top.clone();
            let top_op: Arc<TopOp> = Arc::new(move || *t2.lock().unwrap());
            let (tx, reports) = mpsc::channel();
            let report: Arc<FocusReport> = Arc::new(move |w: &WinInfo, stage: FocusStage, _ms: f64| {
                let _ = tx.send((w.wid, stage));
            });
            Rig { focuser: Focuser::with_ops(front, raise, top_op, report), fronted, raised, gates, top, reports }
        }

        /// Block `step` ('f', 'r', 'w': see above) of `wid` until the returned sender fires; the
        /// receiver fires when the step has started.
        fn gate(&self, step: char, wid: u32) -> (mpsc::Sender<()>, mpsc::Receiver<()>) {
            let (open_tx, open_rx) = mpsc::channel();
            let (entered_tx, entered_rx) = mpsc::channel();
            self.gates.lock().unwrap().insert((step, wid), (entered_tx, open_rx));
            (open_tx, entered_rx)
        }

        /// The next report, or panic after 5 s.
        fn next(&self) -> (u32, FocusStage) {
            self.reports.recv_timeout(Duration::from_secs(5)).expect("a focus report")
        }

        /// Reports until `want` arrived (inclusive).
        fn until(&self, want: (u32, FocusStage)) -> Vec<(u32, FocusStage)> {
            let mut seen = Vec::new();
            loop {
                let r = self.next();
                seen.push(r);
                if r == want {
                    return seen;
                }
            }
        }

        /// Every report that arrives within `d`.
        fn drain(&self, d: Duration) -> Vec<(u32, FocusStage)> {
            let end = std::time::Instant::now() + d;
            let mut seen = Vec::new();
            while let Ok(r) = self.reports.recv_timeout(end.saturating_duration_since(std::time::Instant::now())) {
                seen.push(r);
            }
            seen
        }
    }

    #[test]
    fn queued_older_requests_are_skipped() {
        let rig = Rig::new();
        let (open, entered) = rig.gate('f', 1);
        rig.focuser.focus(win(1, 1));
        entered.recv_timeout(Duration::from_secs(5)).unwrap(); // the worker is inside front(1)
        rig.focuser.focus(win(2, 2));
        rig.focuser.focus(win(3, 3));
        open.send(()).unwrap();
        let seen = rig.until((3, FocusStage::Raised("raised")));
        assert!(seen.contains(&(2, FocusStage::Superseded)), "{seen:?}");
        assert!(!seen.contains(&(2, FocusStage::Fronted { ok: true })), "{seen:?}");
        assert_eq!(*rig.fronted.lock().unwrap(), vec![1, 3], "window 2 was never fronted");
        // 1 was fronted before 3 existed; its raise either ran first or was stopped by 3's front.
        let seen = [seen, rig.drain(Duration::from_millis(200))].concat();
        assert!(seen.iter().any(|r| r.0 == 1 && matches!(r.1, FocusStage::Raised(_))), "{seen:?}");
    }

    #[test]
    fn a_raise_blocked_on_a_busy_app_never_delays_another_apps_switch() {
        let rig = Rig::new();
        let (open, entered) = rig.gate('w', 1);
        rig.focuser.focus(win(1, 10));
        assert_eq!(rig.next(), (1, FocusStage::Fronted { ok: true }));
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        // App 10 is busy in window 1's raise; the user switches to window 2 of app 20.
        rig.focuser.focus(win(2, 20));
        assert_eq!(rig.next(), (2, FocusStage::Fronted { ok: true }), "fronting 2 must not wait for 1's raise");
        assert_eq!(rig.next(), (2, FocusStage::Raised("raised")), "2's raise must not wait for app 10 either");
        open.send(()).unwrap();
        // 1's write was already sent: it lands (late) and is reported, nothing else of 1 runs.
        assert_eq!(rig.next(), (1, FocusStage::Raised("raised")));
        assert_eq!(rig.next(), (1, FocusStage::Late));
        assert_eq!(*rig.raised.lock().unwrap(), vec![2, 1]);
    }

    #[test]
    fn one_apps_raises_stay_in_order() {
        let rig = Rig::new();
        let (open, entered) = rig.gate('w', 1);
        rig.focuser.focus(win(1, 10));
        assert_eq!(rig.next(), (1, FocusStage::Fronted { ok: true }));
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        rig.focuser.focus(win(2, 10));
        assert_eq!(rig.next(), (2, FocusStage::Fronted { ok: true }));
        std::thread::sleep(Duration::from_millis(50));
        assert!(rig.raised.lock().unwrap().is_empty(), "2's raise waits behind 1's (same app)");
        open.send(()).unwrap();
        let seen = rig.until((2, FocusStage::Raised("raised")));
        assert_eq!(*rig.raised.lock().unwrap(), vec![1, 2], "{seen:?}");
    }

    #[test]
    fn a_late_write_refronts_the_newest_switch_only_when_the_stale_window_came_out_on_top() {
        let rig = Rig::new();
        let (open, entered) = rig.gate('w', 1);
        rig.focuser.focus(win(1, 10));
        assert_eq!(rig.next(), (1, FocusStage::Fronted { ok: true }));
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        rig.focuser.focus(win(2, 20));
        rig.until((2, FocusStage::Raised("raised")));
        // App 10 recovers and applies window 1's raise: window 1 is now on top.
        *rig.top.lock().unwrap() = Some((1, 10));
        open.send(()).unwrap();
        let seen = rig.until((2, FocusStage::Refront { stale_wid: 1, refronted: true }));
        assert!(seen.contains(&(1, FocusStage::Late)), "{seen:?}");
        rig.until((2, FocusStage::Raised("raised")));
        assert_eq!(*rig.fronted.lock().unwrap(), vec![1, 2, 2], "2 fronted again");
        // The second check, 0.5 s later, finds 2 on top and leaves it alone.
        *rig.top.lock().unwrap() = Some((2, 20));
        let later = rig.drain(Duration::from_millis(900));
        assert_eq!(later, vec![(2, FocusStage::Refront { stale_wid: 1, refronted: false })]);
        assert_eq!(rig.fronted.lock().unwrap().len(), 3);
    }

    #[test]
    fn a_late_write_never_takes_the_focus_from_something_else() {
        let rig = Rig::new();
        let (open, entered) = rig.gate('w', 1);
        rig.focuser.focus(win(1, 10));
        rig.next();
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        rig.focuser.focus(win(2, 20));
        rig.until((2, FocusStage::Raised("raised")));
        // The user has clicked into another app meanwhile.
        *rig.top.lock().unwrap() = Some((7, 70));
        open.send(()).unwrap();
        let seen = [rig.until((2, FocusStage::Refront { stale_wid: 1, refronted: false })), rig.drain(Duration::from_millis(900))].concat();
        assert!(!seen.iter().any(|r| matches!(r.1, FocusStage::Refront { refronted: true, .. })), "{seen:?}");
        assert_eq!(*rig.fronted.lock().unwrap(), vec![1, 2]);
    }

    #[test]
    fn a_newer_switch_whose_app_is_gone_does_not_stop_the_previous_one() {
        let rig = Rig::new();
        let (open, entered) = rig.gate('r', 1);
        rig.focuser.focus(win(1, 10));
        assert_eq!(rig.next(), (1, FocusStage::Fronted { ok: true }));
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        rig.focuser.focus(win(100, 99)); // its app quit
        assert_eq!(rig.next(), (100, FocusStage::Fronted { ok: false }));
        open.send(()).unwrap();
        let seen = rig.until((1, FocusStage::Raised("raised")));
        assert!(!seen.iter().any(|r| r.1 == FocusStage::Raised("superseded")), "{seen:?}");
        // The fake's late flag only means "a newer front started"; nothing newer was fronted, so
        // there is nothing to re-front.
        let later = rig.drain(Duration::from_millis(800));
        assert!(!later.iter().any(|r| matches!(r.1, FocusStage::Refront { .. })), "{later:?}");
    }

    #[test]
    fn a_raise_stops_before_its_write_once_a_newer_switch_is_fronted() {
        let rig = Rig::new();
        let (open, entered) = rig.gate('r', 1);
        rig.focuser.focus(win(1, 10));
        rig.next();
        entered.recv_timeout(Duration::from_secs(5)).unwrap(); // 1's element lookup is slow
        rig.focuser.focus(win(2, 20));
        rig.until((2, FocusStage::Raised("raised")));
        open.send(()).unwrap();
        assert_eq!(rig.next(), (1, FocusStage::Raised("superseded")));
        assert_eq!(*rig.raised.lock().unwrap(), vec![2], "1's write was never sent");
        assert!(rig.drain(Duration::from_millis(700)).is_empty(), "no write, nothing late");
    }

    #[test]
    fn sequence_views() {
        let s = FocusState::default();
        s.started.store(3, Ordering::SeqCst);
        s.fronted.store(3, Ordering::SeqCst);
        let v3 = ReqView { seq: 3, state: &s };
        assert!(v3.current() && !v3.overtaken());
        s.started.store(4, Ordering::SeqCst);
        assert!(v3.current() && v3.overtaken(), "4 is fronting: 3 keeps going");
        s.fronted.store(4, Ordering::SeqCst);
        assert!(!v3.current());
    }
}
