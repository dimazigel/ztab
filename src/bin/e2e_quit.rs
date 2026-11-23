//! End-to-end check of the menu-bar item: clicks ZTab's status icon with the mouse, then clicks
//! the "Quit ZTab" menu item, and verifies the ZTab process exits. On macOS 27 status items are
//! drawn by the Window Server, so their frames come from Accessibility (AXExtrasMenuBar).
//! Usage (needs Accessibility): bundle as an app, `open -W -n -g <app> --args <out> <ztab-pid>`.

use std::ffi::c_void;
use std::io::Write;
use std::ptr::NonNull;
use std::thread::sleep;
use std::time::Duration;

use objc2_application_services::{AXError, AXUIElement, AXValue, AXValueType};
use objc2_core_foundation::{CFArray, CFRetained, CFString, CFType, CGPoint, CGSize};
use objc2_core_graphics::{CGEvent, CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGEventType, CGMouseButton};

fn attr(el: &AXUIElement, name: &str) -> Option<CFRetained<CFType>> {
    let key = CFString::from_str(name);
    let mut value: *const CFType = std::ptr::null();
    let err = unsafe { el.copy_attribute_value(&key, NonNull::from(&mut value)) };
    if err != AXError::Success || value.is_null() {
        return None;
    }
    Some(unsafe { CFRetained::from_raw(NonNull::new_unchecked(value as *mut CFType)) })
}

fn children(el: &AXUIElement) -> Vec<CFRetained<AXUIElement>> {
    let Some(v) = attr(el, "AXChildren") else { return vec![] };
    let Ok(arr) = v.downcast::<CFArray>() else { return vec![] };
    let arr: CFRetained<CFArray<AXUIElement>> = unsafe { CFRetained::cast_unchecked(arr) };
    arr.to_vec()
}

fn title(el: &AXUIElement) -> String {
    attr(el, "AXTitle").and_then(|v| v.downcast_ref::<CFString>().map(|s| s.to_string())).unwrap_or_default()
}

/// Center of an element's frame in global (top-left origin) coordinates.
fn center(el: &AXUIElement) -> Option<CGPoint> {
    let pos = attr(el, "AXPosition")?;
    let size = attr(el, "AXSize")?;
    let (pos, size) = (pos.downcast_ref::<AXValue>()?, size.downcast_ref::<AXValue>()?);
    let mut p = CGPoint::default();
    let mut s = CGSize::default();
    unsafe {
        if !pos.value(AXValueType::CGPoint, NonNull::from(&mut p).cast::<c_void>()) {
            return None;
        }
        if !size.value(AXValueType::CGSize, NonNull::from(&mut s).cast::<c_void>()) {
            return None;
        }
    }
    Some(CGPoint { x: p.x + s.width / 2.0, y: p.y + s.height / 2.0 })
}

/// Depth-first search for a menu item with the given title.
fn find_titled(el: &AXUIElement, wanted: &str, depth: usize) -> Option<CFRetained<AXUIElement>> {
    for c in children(el) {
        if title(&c) == wanted {
            return Some(c);
        }
        if depth > 0 {
            if let Some(f) = find_titled(&c, wanted, depth - 1) {
                return Some(f);
            }
        }
    }
    None
}

fn click(p: CGPoint) {
    let src = CGEventSource::new(CGEventSourceStateID::HIDSystemState).unwrap();
    for kind in [CGEventType::MouseMoved, CGEventType::LeftMouseDown, CGEventType::LeftMouseUp] {
        let ev = CGEvent::new_mouse_event(Some(&src), kind, p, CGMouseButton::Left).unwrap();
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
        sleep(Duration::from_millis(60));
    }
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut out = std::fs::File::create(&args[1]).unwrap();
    let pid: i32 = args[2].parse().unwrap();
    let app = unsafe { AXUIElement::new_application(pid) };
    let Some(bar) = attr(&app, "AXExtrasMenuBar").and_then(|v| v.downcast::<AXUIElement>().ok()) else {
        let _ = writeln!(out, "RESULT FAIL (no AXExtrasMenuBar: status item missing)");
        return;
    };
    let items = children(&bar);
    let _ = writeln!(out, "status items: {}", items.len());
    let Some(icon) = items.first().and_then(|i| center(i)) else {
        let _ = writeln!(out, "RESULT FAIL (status item has no frame)");
        return;
    };
    let _ = writeln!(out, "status icon at {icon:?}");
    let original = CGEvent::new(None).map(|e| CGEvent::location(Some(&e)));
    click(icon);
    sleep(Duration::from_millis(600));
    let quit = find_titled(&bar, "Quit ZTab", 4);
    let quit_at = quit.as_deref().and_then(center);
    let _ = writeln!(out, "menu item 'Quit ZTab' found={} at {quit_at:?}", quit.is_some());
    let Some(q) = quit_at else {
        let _ = writeln!(out, "RESULT FAIL (menu did not open / no Quit item)");
        return;
    };
    click(q);
    let mut gone = false;
    for _ in 0..30 {
        sleep(Duration::from_millis(100));
        if !alive(pid) {
            gone = true;
            break;
        }
    }
    if let Some(p) = original {
        let src = CGEventSource::new(CGEventSourceStateID::HIDSystemState).unwrap();
        let ev = CGEvent::new_mouse_event(Some(&src), CGEventType::MouseMoved, p, CGMouseButton::Left).unwrap();
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
    }
    let _ = writeln!(out, "CHECK quit via menu: process exited={gone}");
    let _ = writeln!(out, "RESULT {}", if gone { "PASS" } else { "FAIL" });
}
