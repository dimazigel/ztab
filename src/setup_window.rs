//! The "Set up ZTab" window (main thread only): one row per permission (status and a "Grant…"
//! button), a message line, "Restart ZTab" and the main button (Done / Skip Thumbnails / Later).
//! It only reports what the user did ([`SetupEvent`]) and shows what it is told; `main.rs`
//! decides (with `setup.rs`). A line under the message says when ZTab turned its login item on at
//! this, its first, launch, or could not (empty otherwise).
//!
//! Accessibility identifiers for the end-to-end test: `ztab.setup.ax.status`,
//! `ztab.setup.ax.grant`, `ztab.setup.screen.status`, `ztab.setup.screen.grant`,
//! `ztab.setup.message`, `ztab.setup.login`, `ztab.setup.restart`, `ztab.setup.primary`.

use std::cell::Cell;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAccessibility, NSApplication, NSBackingStoreType, NSButton, NSColor, NSEvent, NSFont, NSScreen, NSTextAlignment, NSTextField,
    NSView, NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{NSNotification, NSPoint, NSRect, NSSize, NSString};

use crate::settings_window::MsgKind;
use crate::setup::{Perm, Primary, ScreenRow};

pub const WINDOW_TITLE: &str = "Set up ZTab";
const W: f64 = 600.0;
const H: f64 = 322.0;

/// What the user did in the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupEvent {
    Grant(Perm),
    Restart,
    /// The main button (its meaning depends on the title it shows, see `setup::Primary`).
    Primary,
    /// The window is closing (its close button, ⌘W, or `close()`); it is kept and reused.
    Closing,
    /// It became (`true`) or stopped being the key window.
    Key(bool),
    /// It was moved.
    Moved,
}

type OnEvent = Rc<dyn Fn(SetupEvent)>;

pub struct TargetIvars {
    on_event: OnEvent,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ZTabSetupTarget"]
    #[ivars = TargetIvars]
    pub struct SetupTarget;

    unsafe impl NSObjectProtocol for SetupTarget {}

    unsafe impl NSWindowDelegate for SetupTarget {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _n: &NSNotification) {
            self.emit(SetupEvent::Closing);
        }

        #[unsafe(method(windowDidBecomeKey:))]
        fn window_did_become_key(&self, _n: &NSNotification) {
            self.emit(SetupEvent::Key(true));
        }

        #[unsafe(method(windowDidResignKey:))]
        fn window_did_resign_key(&self, _n: &NSNotification) {
            self.emit(SetupEvent::Key(false));
        }

        #[unsafe(method(windowDidMove:))]
        fn window_did_move(&self, _n: &NSNotification) {
            self.emit(SetupEvent::Moved);
        }
    }

    impl SetupTarget {
        #[unsafe(method(grantAccessibility:))]
        fn grant_accessibility(&self, _sender: Option<&AnyObject>) {
            self.emit(SetupEvent::Grant(Perm::Accessibility));
        }

        #[unsafe(method(grantScreenRecording:))]
        fn grant_screen_recording(&self, _sender: Option<&AnyObject>) {
            self.emit(SetupEvent::Grant(Perm::ScreenRecording));
        }

        #[unsafe(method(restartZTab:))]
        fn restart_ztab(&self, _sender: Option<&AnyObject>) {
            self.emit(SetupEvent::Restart);
        }

        #[unsafe(method(primaryClicked:))]
        fn primary_clicked(&self, _sender: Option<&AnyObject>) {
            self.emit(SetupEvent::Primary);
        }
    }
);

impl SetupTarget {
    fn new(mtm: MainThreadMarker, on_event: OnEvent) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TargetIvars { on_event });
        unsafe { msg_send![super(this), init] }
    }

    fn emit(&self, e: SetupEvent) {
        let cb = self.ivars().on_event.clone();
        cb(e);
    }
}

struct Row {
    status: Retained<NSTextField>,
    grant: Retained<NSButton>,
}

pub struct SetupWindow {
    window: Retained<NSWindow>,
    _target: Retained<SetupTarget>,
    ax: Row,
    screen: Row,
    message: Retained<NSTextField>,
    login_note: Retained<NSTextField>,
    primary: Retained<NSButton>,
    primary_kind: Cell<Primary>,
    placed: Cell<bool>,
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

fn label(mtm: MainThreadMarker, text: &str, font: Retained<NSFont>, secondary: bool) -> Retained<NSTextField> {
    let l = NSTextField::wrappingLabelWithString(&NSString::from_str(text), mtm);
    l.setFont(Some(&font));
    if secondary {
        l.setTextColor(Some(&NSColor::secondaryLabelColor()));
    }
    l
}

fn set_id(v: &NSView, id: &str) {
    v.setAccessibilityIdentifier(Some(&NSString::from_str(id)));
}

impl SetupWindow {
    /// Build the window (hidden; its WindowServer window is created when it is first shown).
    pub fn new(mtm: MainThreadMarker, on_event: OnEvent) -> SetupWindow {
        let target = SetupTarget::new(mtm, on_event);
        // SAFETY: standard initializer; the window is owned through Retained (not released on close).
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect(0.0, 0.0, W, H),
                // Not miniaturizable, like the Settings window: minimizing runs the system's
                // animation on the main thread for about half a second.
                NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
                NSBackingStoreType::Buffered,
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
        let button = |title: &str, action, id: &str| {
            // SAFETY: target/action valid for the window's lifetime (the target is kept in `self`).
            let b = unsafe { NSButton::buttonWithTitle_target_action(&NSString::from_str(title), Some(target_obj), Some(action), mtm) };
            set_id(&b, id);
            b
        };

        let intro = label(
            mtm,
            "ZTab needs two permissions. Click Grant…, then turn on ZTab in the list that System Settings shows.",
            NSFont::systemFontOfSize(13.0),
            false,
        );
        add(&intro, x0, 18.0, W - 2.0 * x0, 36.0);

        let row = |top: f64, title: &str, detail: &str, perm: &str, action| {
            let t = label(mtm, title, NSFont::boldSystemFontOfSize(13.0), false);
            add(&t, x0, top + 6.0, 210.0, 20.0);
            // One line, right-aligned, ending left of the button (x 412).
            let status = NSTextField::labelWithString(&NSString::from_str(""), mtm);
            status.setFont(Some(&NSFont::systemFontOfSize(13.0)));
            status.setAlignment(NSTextAlignment::Right);
            set_id(&status, &format!("ztab.setup.{perm}.status"));
            add(&status, 234.0, top + 6.0, 170.0, 20.0);
            let grant = button("Grant…", action, &format!("ztab.setup.{perm}.grant"));
            add(&grant, W - x0 - 170.0 + 6.0, top, 170.0, 30.0);
            let d = label(mtm, detail, NSFont::systemFontOfSize(11.0), true);
            add(&d, x0, top + 32.0, W - 2.0 * x0, 30.0);
            Row { status, grant }
        };
        let ax = row(
            66.0,
            "Accessibility — required",
            "Lets ZTab catch ⌥Tab and switch windows. Without it ZTab cannot work.",
            "ax",
            sel!(grantAccessibility:),
        );
        let screen = row(
            136.0,
            "Screen Recording — optional",
            "For window thumbnails. Without it the switcher shows app icons. ZTab restarts itself once it is on.",
            "screen",
            sel!(grantScreenRecording:),
        );

        let message = label(mtm, "", NSFont::systemFontOfSize(12.0), true);
        set_id(&message, "ztab.setup.message");
        add(&message, x0, 206.0, W - 2.0 * x0, 36.0);
        // One line: the login item was turned on at this first launch (see `set_login_note`).
        let login_note = label(mtm, "", NSFont::systemFontOfSize(12.0), true);
        set_id(&login_note, "ztab.setup.login");
        add(&login_note, x0, 244.0, W - 2.0 * x0, 18.0);

        let restart = button("Restart ZTab", sel!(restartZTab:), "ztab.setup.restart");
        add(&restart, x0 - 6.0, 274.0, 150.0, 30.0);
        let primary = button("Done", sel!(primaryClicked:), "ztab.setup.primary");
        add(&primary, W - x0 - 170.0 + 6.0, 274.0, 170.0, 30.0);

        SetupWindow {
            window,
            _target: target,
            ax,
            screen,
            message,
            login_note,
            primary,
            primary_kind: Cell::new(Primary::Done),
            placed: Cell::new(false),
        }
    }

    pub fn window(&self) -> &NSWindow {
        &self.window
    }

    /// Order the window in and make it key (activating ZTab first, like the Settings window). The
    /// first time it is centred on the screen under the mouse.
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
        NSApplication::sharedApplication(mtm).activate();
        self.window.makeKeyAndOrderFront(None);
    }

    pub fn is_visible(&self) -> bool {
        self.window.isVisible()
    }

    /// Close it (the delegate reports `Closing`).
    pub fn close(&self) {
        self.window.close();
    }

    fn row(&self, p: Perm) -> &Row {
        match p {
            Perm::Accessibility => &self.ax,
            Perm::ScreenRecording => &self.screen,
        }
    }

    /// Accessibility's row.
    pub fn set_ax(&self, granted: bool, grant_title: &str) {
        self.set_row(Perm::Accessibility, if granted { ScreenRow::Granted } else { ScreenRow::NotGranted }, grant_title);
    }

    /// A row: its status text and colour, and the "Grant…" button (hidden once granted).
    pub fn set_row(&self, p: Perm, state: ScreenRow, grant_title: &str) {
        let r = self.row(p);
        let (text, color) = match state {
            ScreenRow::Granted => ("✓ Granted", NSColor::systemGreenColor()),
            ScreenRow::GrantedRestartNeeded => ("✓ On — restart needed", NSColor::systemOrangeColor()),
            ScreenRow::NotGranted => ("✗ Not granted", NSColor::systemRedColor()),
        };
        r.status.setStringValue(&NSString::from_str(text));
        r.status.setTextColor(Some(&color));
        r.grant.setTitle(&NSString::from_str(grant_title));
        r.grant.setHidden(state != ScreenRow::NotGranted);
    }

    /// The main button's title and meaning. Return presses it only as Done: the window can come
    /// to the front at launch while the user types elsewhere, and a stray Return must not choose
    /// "Skip Thumbnails" (remembered) or "Later" for them.
    pub fn set_primary(&self, p: Primary) {
        self.primary_kind.set(p);
        self.primary.setTitle(&NSString::from_str(p.title()));
        self.primary.setKeyEquivalent(&NSString::from_str(if p == Primary::Done { "\r" } else { "" }));
    }

    /// The main button is disabled from the moment the window opens until the first status check
    /// of this opening arrives (it would act on what an earlier opening saw).
    pub fn set_primary_enabled(&self, on: bool) {
        self.primary.setEnabled(on);
    }

    pub fn primary(&self) -> Primary {
        self.primary_kind.get()
    }

    /// The login line: ZTab turned its login item on at this launch, or could not (empty: no
    /// line).
    pub fn set_login_note(&self, text: &str, kind: MsgKind) {
        let color = match kind {
            MsgKind::Info => NSColor::secondaryLabelColor(),
            MsgKind::Warning => NSColor::systemOrangeColor(),
            MsgKind::Error => NSColor::systemRedColor(),
        };
        self.login_note.setTextColor(Some(&color));
        self.login_note.setStringValue(&NSString::from_str(text));
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
}
