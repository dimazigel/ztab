//! Helper of the reopen live check (`live_takeover.sh`): reads a ZTab's windows through
//! Accessibility, and closes one. It never posts input.
//!
//! Usage (bundled with `scripts/bundle.sh e2e_reopen <dir>/ZTab.app`, so it has ZTab's grants):
//!   `open -W -n -g <bundle> --args <out> windows <pid>`: one line per window of `pid`,
//!     `window "<title>" main=<0|1> message="<text>"` (the Settings window's status line, or the
//!     Set up window's message), then `frontmost=<pid>`;
//!   `open -W -n -g <bundle> --args <out> close <pid> <title>`: presses that window's close button
//!     (`closed=<AXError>`).

#[path = "../windows.rs"]
#[allow(dead_code)]
mod windows;

use std::io::Write;

use objc2_app_kit::NSWorkspace;
use objc2_application_services::AXUIElement;
use objc2_core_foundation::{CFArray, CFRetained, CFString, CFType};

fn attr(el: &AXUIElement, name: &str) -> Option<CFRetained<CFType>> {
    windows::ax_attr(el, name)
}
fn str_attr(el: &AXUIElement, name: &str) -> Option<String> {
    attr(el, name).and_then(|v| v.downcast_ref::<CFString>().map(|s| s.to_string()))
}
fn children(el: &AXUIElement) -> Vec<CFRetained<AXUIElement>> {
    let Some(v) = attr(el, "AXChildren") else { return vec![] };
    let Ok(arr) = v.downcast::<CFArray>() else { return vec![] };
    let arr: CFRetained<CFArray<AXUIElement>> = unsafe { CFRetained::cast_unchecked(arr) };
    arr.to_vec()
}
fn find_id(el: &AXUIElement, id: &str, depth: usize) -> Option<CFRetained<AXUIElement>> {
    for c in children(el) {
        if str_attr(&c, "AXIdentifier").as_deref() == Some(id) {
            return Some(c);
        }
        if depth > 0
            && let Some(f) = find_id(&c, id, depth - 1)
        {
            return Some(f);
        }
    }
    None
}
fn windows_of(pid: i32) -> Vec<CFRetained<AXUIElement>> {
    let app = unsafe { AXUIElement::new_application(pid) };
    unsafe { app.set_messaging_timeout(2.0) };
    let Some(v) = attr(&app, "AXWindows") else { return vec![] };
    let Ok(arr) = v.downcast::<CFArray>() else { return vec![] };
    let arr: CFRetained<CFArray<AXUIElement>> = unsafe { CFRetained::cast_unchecked(arr) };
    arr.to_vec()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (Some(out), Some(cmd), Some(pid)) = (args.get(1), args.get(2), args.get(3).and_then(|p| p.parse::<i32>().ok())) else {
        eprintln!("usage: e2e_reopen <out> windows <pid> | e2e_reopen <out> close <pid> <title>");
        std::process::exit(2);
    };
    let mut lines = vec![format!("ax_trusted={}", windows::ax_trusted())];
    match cmd.as_str() {
        "windows" => {
            for w in windows_of(pid) {
                let title = str_attr(&w, "AXTitle").unwrap_or_default();
                let main = attr(&w, "AXMain").and_then(|v| v.downcast_ref::<objc2_core_foundation::CFBoolean>().map(|b| b.as_bool()));
                let msg = ["ztab.message", "ztab.setup.message"].iter().find_map(|id| find_id(&w, id, 4)).and_then(|e| str_attr(&e, "AXValue"));
                lines.push(format!("window {title:?} main={} message={:?}", u8::from(main == Some(true)), msg.unwrap_or_default()));
            }
        }
        "close" => {
            let title = args.get(4).cloned().unwrap_or_default();
            let w = windows_of(pid).into_iter().find(|w| str_attr(w, "AXTitle").as_deref() == Some(title.as_str()));
            let button = w.and_then(|w| attr(&w, "AXCloseButton")).and_then(|v| v.downcast::<AXUIElement>().ok());
            let r = button.map(|b| unsafe { b.perform_action(&CFString::from_str("AXPress")) });
            lines.push(format!("closed={r:?}"));
        }
        _ => lines.push(format!("unknown command {cmd}")),
    }
    let front = NSWorkspace::sharedWorkspace().frontmostApplication().map(|a| a.processIdentifier());
    lines.push(format!("frontmost={}", front.unwrap_or(0)));
    if let Ok(mut f) = std::fs::File::create(out) {
        for l in lines {
            let _ = writeln!(f, "{l}");
        }
    }
}
