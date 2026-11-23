// Permission probe: requests Accessibility + Screen Recording and logs the state.
use std::io::Write;
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" { fn AXIsProcessTrusted() -> bool; }
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" { fn CGPreflightScreenCaptureAccess() -> bool; fn CGRequestScreenCaptureAccess() -> bool; }
fn main() {
    let log = std::env::var("HOME").unwrap() + "/Library/Logs/ztab-probe.log";
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(log).unwrap();
    unsafe {
        let ax = AXIsProcessTrusted();
        let sc = CGPreflightScreenCaptureAccess();
        writeln!(f, "pid={} ax={} screen={}", std::process::id(), ax, sc).unwrap();
        if std::env::args().any(|a| a == "--request") {
            if !ax {
                // Opens the Accessibility prompt / adds the app to the list.
                std::process::Command::new("open").arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility").status().ok();
                ax_prompt();
            }
            if !sc { CGRequestScreenCaptureAccess(); }
        }
    }
}
fn ax_prompt() {
    use objc2_core_foundation::{CFDictionary, CFString, CFBoolean};
    let key = CFString::from_static_str("AXTrustedCheckOptionPrompt");
    let dict = CFDictionary::from_slices(&[&*key], &[&*CFBoolean::new(true)]);
    unsafe { objc2_application_services::AXIsProcessTrustedWithOptions(Some(dict.as_opaque())); }
}
