//! The Settings window (main thread only): show delay slider, start-at-login checkbox, the two
//! shortcut recorders and "Restore defaults". It only reports what the user did
//! ([`SettingsEvent`]) and shows what it is told; `main.rs` applies, validates and saves.
//!
//! A recorder is a push button: clicking it starts capturing (title "Type shortcut…"), the next
//! key with ⌥ / ⌘ / ⌃ is reported as [`SettingsEvent::Recorded`], Esc (or clicking it again,
//! or the window losing key status) stops. While one captures, `main.rs` tells the event tap to
//! let every key through, so the keys reach this window.
//!
//! Every control has an accessibility identifier (`ztab.delay`, `ztab.delay.value`,
//! `ztab.login`, `ztab.hotkey.all`, `ztab.hotkey.same`, `ztab.restore`, `ztab.message`) for the
//! end-to-end test.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAccessibility, NSApplication, NSBackingStoreType, NSBezelStyle, NSButton, NSColor, NSControl, NSControlStateValueOff,
    NSControlStateValueOn, NSEvent, NSEventModifierFlags, NSFont, NSResponder, NSScreen, NSSlider, NSTextField, NSView,
    NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{NSNotification, NSPoint, NSRect, NSSize, NSString};

use crate::hotkey::{Binding, Mods, Which, keycode};
use crate::settings::{MAX_SHOW_DELAY_MS, Settings};

pub const WINDOW_TITLE: &str = "ZTab Settings";
const RECORDING_TITLE: &str = "Type shortcut…";
const W: f64 = 500.0;
const H: f64 = 388.0;

/// What the user did in the window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SettingsEvent {
    /// The slider moved (ms, rounded to 10).
    Delay(u64),
    /// The checkbox was clicked (the new state).
    Login(bool),
    /// A recorder captured a key with these modifiers.
    Recorded(Which, u16, Mods),
    /// A recorder started (`true`) or stopped capturing.
    Recording(Which, bool),
    Restore,
    /// The window became (`true`) or stopped being the key window.
    Key(bool),
    Miniaturized(bool),
    Moved,
    /// The window is closing (it is kept and reused).
    Closing,
}

type OnEvent = Rc<dyn Fn(SettingsEvent)>;

pub struct TargetIvars {
    on_event: OnEvent,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ZTabSettingsTarget"]
    #[ivars = TargetIvars]
    pub struct SettingsTarget;

    unsafe impl NSObjectProtocol for SettingsTarget {}

    unsafe impl NSWindowDelegate for SettingsTarget {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _n: &NSNotification) {
            self.emit(SettingsEvent::Closing);
        }

        #[unsafe(method(windowDidBecomeKey:))]
        fn window_did_become_key(&self, _n: &NSNotification) {
            self.emit(SettingsEvent::Key(true));
        }

        #[unsafe(method(windowDidResignKey:))]
        fn window_did_resign_key(&self, _n: &NSNotification) {
            self.emit(SettingsEvent::Key(false));
        }

        #[unsafe(method(windowDidMiniaturize:))]
        fn window_did_miniaturize(&self, _n: &NSNotification) {
            self.emit(SettingsEvent::Miniaturized(true));
        }

        #[unsafe(method(windowDidDeminiaturize:))]
        fn window_did_deminiaturize(&self, _n: &NSNotification) {
            self.emit(SettingsEvent::Miniaturized(false));
        }

        #[unsafe(method(windowDidMove:))]
        fn window_did_move(&self, _n: &NSNotification) {
            self.emit(SettingsEvent::Moved);
        }
    }

    impl SettingsTarget {
        #[unsafe(method(delayChanged:))]
        fn delay_changed(&self, sender: Option<&AnyObject>) {
            let Some(slider) = sender.and_then(|s| s.downcast_ref::<NSSlider>()) else { return };
            let ms = round_delay(slider.doubleValue());
            self.emit(SettingsEvent::Delay(ms));
        }

        #[unsafe(method(loginToggled:))]
        fn login_toggled(&self, sender: Option<&AnyObject>) {
            let Some(b) = sender.and_then(|s| s.downcast_ref::<NSButton>()) else { return };
            self.emit(SettingsEvent::Login(b.state() == NSControlStateValueOn));
        }

        #[unsafe(method(restoreDefaults:))]
        fn restore_defaults(&self, _sender: Option<&AnyObject>) {
            self.emit(SettingsEvent::Restore);
        }
    }
);

impl SettingsTarget {
    fn new(mtm: MainThreadMarker, on_event: OnEvent) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TargetIvars { on_event });
        unsafe { msg_send![super(this), init] }
    }

    fn emit(&self, e: SettingsEvent) {
        let cb = self.ivars().on_event.clone();
        cb(e);
    }
}

/// Slider value to ms: rounded to 10, clamped.
pub fn round_delay(v: f64) -> u64 {
    ((v.max(0.0) / 10.0).round() as u64 * 10).min(MAX_SHOW_DELAY_MS)
}

pub struct RecorderIvars {
    which: Which,
    recording: Cell<bool>,
    /// The binding shown while not recording.
    label: RefCell<String>,
    on_event: OnEvent,
}

define_class!(
    // SAFETY: NSButton has no subclassing requirements beyond main-thread use; no Drop.
    #[unsafe(super(NSButton, NSControl, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ZTabShortcutRecorder"]
    #[ivars = RecorderIvars]
    pub struct Recorder;

    unsafe impl NSObjectProtocol for Recorder {}

    impl Recorder {
        #[unsafe(method(recorderClicked:))]
        fn recorder_clicked(&self, _sender: Option<&AnyObject>) {
            if self.ivars().recording.get() {
                self.stop();
            } else {
                self.start();
            }
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(resignFirstResponder))]
        fn resign_first_responder(&self) -> bool {
            if self.ivars().recording.get() {
                self.stop();
            }
            unsafe { msg_send![super(self), resignFirstResponder] }
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            if self.ivars().recording.get() {
                self.captured(event);
            } else {
                unsafe { msg_send![super(self), keyDown: event] }
            }
        }

        // ⌘ and ⌃ combinations reach the window as key equivalents first.
        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, event: &NSEvent) -> bool {
            if self.ivars().recording.get() && self.is_first_responder() {
                self.captured(event);
                true
            } else {
                unsafe { msg_send![super(self), performKeyEquivalent: event] }
            }
        }

        #[unsafe(method(flagsChanged:))]
        fn flags_changed(&self, event: &NSEvent) {
            if self.ivars().recording.get() {
                let m = mods_of(event);
                let held: String = [(m.control, "⌃"), (m.option, "⌥"), (m.shift, "⇧"), (m.command, "⌘")]
                    .iter()
                    .filter_map(|(on, s)| on.then_some(*s))
                    .collect();
                let t = if held.is_empty() { RECORDING_TITLE.to_string() } else { format!("{held}…") };
                self.setTitle(&NSString::from_str(&t));
            }
        }
    }
);

fn mods_of(event: &NSEvent) -> Mods {
    let f = event.modifierFlags();
    Mods {
        option: f.contains(NSEventModifierFlags::Option),
        shift: f.contains(NSEventModifierFlags::Shift),
        command: f.contains(NSEventModifierFlags::Command),
        control: f.contains(NSEventModifierFlags::Control),
    }
}

impl Recorder {
    fn new(mtm: MainThreadMarker, which: Which, frame: NSRect, on_event: OnEvent) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(RecorderIvars {
            which,
            recording: Cell::new(false),
            label: RefCell::new(String::new()),
            on_event,
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        this.setBezelStyle(NSBezelStyle::Push);
        // SAFETY: the target is this button, which outlives its own action.
        unsafe {
            this.setTarget(Some(&this));
            this.setAction(Some(sel!(recorderClicked:)));
        }
        this
    }

    fn is_first_responder(&self) -> bool {
        let me: &NSResponder = self;
        self.window().and_then(|w| w.firstResponder()).is_some_and(|r| std::ptr::eq(&*r, me))
    }

    fn emit(&self, e: SettingsEvent) {
        let cb = self.ivars().on_event.clone();
        cb(e);
    }

    pub fn is_recording(&self) -> bool {
        self.ivars().recording.get()
    }

    /// Start capturing: take the keyboard focus, then tell `main.rs` (which pauses the tap).
    fn start(&self) {
        let Some(w) = self.window() else { return };
        // Another recorder that was capturing stops when it loses the focus.
        w.makeFirstResponder(Some(self));
        self.ivars().recording.set(true);
        self.setTitle(&NSString::from_str(RECORDING_TITLE));
        self.emit(SettingsEvent::Recording(self.ivars().which, true));
    }

    /// Stop capturing (no-op when not capturing); shows the current binding again.
    pub fn stop(&self) {
        if !self.ivars().recording.replace(false) {
            return;
        }
        let label = self.ivars().label.borrow().clone();
        self.setTitle(&NSString::from_str(&label));
        self.emit(SettingsEvent::Recording(self.ivars().which, false));
    }

    fn captured(&self, event: &NSEvent) {
        let code = event.keyCode();
        let m = mods_of(event);
        if code == keycode::ESCAPE && !(m.option || m.command || m.control || m.shift) {
            self.stop();
            return;
        }
        if event.isARepeat() {
            return;
        }
        self.emit(SettingsEvent::Recorded(self.ivars().which, code, m));
    }

    fn set_binding(&self, b: Binding) {
        let label = b.label();
        *self.ivars().label.borrow_mut() = label.clone();
        if !self.ivars().recording.get() {
            self.setTitle(&NSString::from_str(&label));
        }
    }
}

/// How a message is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgKind {
    Info,
    Warning,
    Error,
}

pub struct SettingsWindow {
    window: Retained<NSWindow>,
    _target: Retained<SettingsTarget>,
    slider: Retained<NSSlider>,
    delay_value: Retained<NSTextField>,
    login: Retained<NSButton>,
    rec_all: Retained<Recorder>,
    rec_same: Retained<Recorder>,
    message: Retained<NSTextField>,
    placed: Cell<bool>,
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

fn label(mtm: MainThreadMarker, text: &str, size: f64, secondary: bool) -> Retained<NSTextField> {
    let l = NSTextField::wrappingLabelWithString(&NSString::from_str(text), mtm);
    l.setFont(Some(&NSFont::systemFontOfSize(size)));
    if secondary {
        l.setTextColor(Some(&NSColor::secondaryLabelColor()));
    }
    l
}

fn set_id(v: &NSView, id: &str) {
    v.setAccessibilityIdentifier(Some(&NSString::from_str(id)));
}

impl SettingsWindow {
    /// Build the window (hidden; its WindowServer window is created when it is first shown).
    /// `on_event` gets every user action.
    pub fn new(mtm: MainThreadMarker, on_event: Rc<dyn Fn(SettingsEvent)>) -> SettingsWindow {
        let target = SettingsTarget::new(mtm, on_event.clone());
        // SAFETY: standard initializer; the window is owned through Retained (not released on close).
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect(0.0, 0.0, W, H),
                // Not miniaturizable: minimizing (and restoring) a window runs the system's
                // animation on the main thread (measured 530-570 ms each), during which ⌥Tab is
                // not handled.
                NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
                NSBackingStoreType::Buffered,
                // No WindowServer window until it is first shown.
                true,
            )
        };
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str(WINDOW_TITLE));
        window.setDelegate(Some(ProtocolObject::from_ref(&*target)));
        let content = window.contentView().expect("a titled window has a content view");
        let add = |v: &NSView, x: f64, top: f64, w: f64, h: f64| {
            v.setFrame(rect(x, H - top - h, w, h));
            content.addSubview(v);
        };
        let target_obj: &AnyObject = &target;
        let x0 = 24.0;

        // Show delay.
        let title = label(mtm, "Show switcher after", 13.0, false);
        add(&title, x0, 24.0, 150.0, 20.0);
        // SAFETY: target/action valid for the window's lifetime (the target is kept in `self`).
        let slider = unsafe {
            NSSlider::sliderWithValue_minValue_maxValue_target_action(
                crate::settings::DEFAULT_SHOW_DELAY_MS as f64,
                0.0,
                MAX_SHOW_DELAY_MS as f64,
                Some(target_obj),
                Some(sel!(delayChanged:)),
                mtm,
            )
        };
        slider.setContinuous(true);
        set_id(&slider, "ztab.delay");
        add(&slider, 180.0, 22.0, 220.0, 24.0);
        let delay_value = label(mtm, "120 ms", 13.0, false);
        set_id(&delay_value, "ztab.delay.value");
        add(&delay_value, 410.0, 24.0, 70.0, 20.0);
        let delay_note = label(
            mtm,
            "A quick ⌥Tab shorter than this switches without showing the switcher. 0 ms shows it at once.",
            11.0,
            true,
        );
        add(&delay_note, x0, 52.0, W - 2.0 * x0, 30.0);

        // Start at login.
        // SAFETY: as above.
        let login = unsafe {
            NSButton::checkboxWithTitle_target_action(&NSString::from_str("Start ZTab at login"), Some(target_obj), Some(sel!(loginToggled:)), mtm)
        };
        set_id(&login, "ztab.login");
        login.setEnabled(false);
        add(&login, x0, 96.0, 300.0, 20.0);

        // Shortcuts.
        let all_title = label(mtm, "Switch between all windows", 13.0, false);
        add(&all_title, x0, 140.0, 290.0, 20.0);
        let rec_all = Recorder::new(mtm, Which::All, rect(0.0, 0.0, 150.0, 30.0), on_event.clone());
        set_id(&rec_all, "ztab.hotkey.all");
        add(&rec_all, W - x0 - 150.0, 135.0, 150.0, 30.0);
        let same_title = label(mtm, "Switch between windows of the current app", 13.0, false);
        add(&same_title, x0, 180.0, 290.0, 20.0);
        let rec_same = Recorder::new(mtm, Which::SameApp, rect(0.0, 0.0, 150.0, 30.0), on_event.clone());
        set_id(&rec_same, "ztab.hotkey.same");
        add(&rec_same, W - x0 - 150.0, 175.0, 150.0, 30.0);
        let hk_note = label(
            mtm,
            "Click a shortcut, then press the new one: hold ⌥, ⌘ or ⌃ and press a key. Releasing that \
             modifier switches; ⇧ goes backwards. ⌘Tab belongs to macOS's app switcher.",
            11.0,
            true,
        );
        add(&hk_note, x0, 214.0, W - 2.0 * x0, 44.0);

        let message = label(mtm, "", 12.0, false);
        set_id(&message, "ztab.message");
        // Up to four lines: the health status of a launch by hand says what it fixed.
        add(&message, x0, 262.0, W - 2.0 * x0, 64.0);

        // SAFETY: as above.
        let restore = unsafe {
            NSButton::buttonWithTitle_target_action(&NSString::from_str("Restore defaults"), Some(target_obj), Some(sel!(restoreDefaults:)), mtm)
        };
        set_id(&restore, "ztab.restore");
        add(&restore, x0 - 6.0, 338.0, 160.0, 30.0);

        SettingsWindow {
            window,
            _target: target,
            slider,
            delay_value,
            login,
            rec_all,
            rec_same,
            message,
            placed: Cell::new(false),
        }
    }

    pub fn window(&self) -> &NSWindow {
        &self.window
    }

    /// Order the window in and make it key. ZTab is an accessory app: activate it first, then
    /// `makeKeyAndOrderFront` (the activation policy stays as it is). The first time, the window
    /// is centered on the screen under the mouse.
    pub fn show(&self, mtm: MainThreadMarker) {
        if !self.placed.replace(true) {
            let m = NSEvent::mouseLocation();
            let screen = NSScreen::screens(mtm)
                .iter()
                .find(|s| {
                    let f = s.frame();
                    m.x >= f.origin.x && m.x <= f.origin.x + f.size.width && m.y >= f.origin.y && m.y <= f.origin.y + f.size.height
                })
                .or_else(|| NSScreen::mainScreen(mtm));
            if let Some(s) = screen {
                let v = s.visibleFrame();
                let f = self.window.frame();
                let x = v.origin.x + (v.size.width - f.size.width) / 2.0;
                let y = v.origin.y + (v.size.height - f.size.height) * 0.6;
                self.window.setFrameOrigin(NSPoint::new(x.round(), y.round()));
            }
        }
        if self.window.isMiniaturized() {
            self.window.deminiaturize(None);
        }
        NSApplication::sharedApplication(mtm).activate();
        self.window.makeKeyAndOrderFront(None);
    }

    pub fn is_visible(&self) -> bool {
        self.window.isVisible() || self.window.isMiniaturized()
    }

    /// Show these values (slider, label, shortcut titles).
    pub fn set_values(&self, s: &Settings) {
        self.slider.setDoubleValue(s.show_delay_ms as f64);
        self.set_delay_label(s.show_delay_ms);
        self.rec_all.set_binding(s.hotkeys.all);
        self.rec_same.set_binding(s.hotkeys.same_app);
    }

    pub fn set_delay_label(&self, ms: u64) {
        self.delay_value.setStringValue(&NSString::from_str(&format!("{ms} ms")));
    }

    /// The login checkbox: `None` while the state is being read or written (disabled).
    pub fn set_login(&self, on: Option<bool>) {
        match on {
            Some(on) => {
                self.login.setState(if on { NSControlStateValueOn } else { NSControlStateValueOff });
                self.login.setEnabled(true);
            }
            None => self.login.setEnabled(false),
        }
    }

    pub fn set_message(&self, text: &str, kind: MsgKind) {
        let color = match kind {
            MsgKind::Info => NSColor::secondaryLabelColor(),
            MsgKind::Warning => NSColor::systemOrangeColor(),
            MsgKind::Error => NSColor::systemRedColor(),
        };
        self.message.setTextColor(Some(&color));
        self.message.setStringValue(&NSString::from_str(text));
    }

    /// Stop whichever recorder is capturing.
    pub fn stop_recording(&self) {
        self.rec_all.stop();
        self.rec_same.stop();
    }

    pub fn is_recording(&self) -> bool {
        self.rec_all.is_recording() || self.rec_same.is_recording()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slider_values_round_to_ten_ms() {
        assert_eq!(round_delay(0.0), 0);
        assert_eq!(round_delay(4.9), 0);
        assert_eq!(round_delay(5.0), 10);
        assert_eq!(round_delay(123.0), 120);
        assert_eq!(round_delay(299.0), 300);
        assert_eq!(round_delay(400.0), 300);
        assert_eq!(round_delay(-3.0), 0);
    }
}
