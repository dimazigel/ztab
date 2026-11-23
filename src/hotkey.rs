//! Global Option+Tab interception through an active `CGEventTap`.
//!
//! The tap is installed on the run loop of the thread that calls [`Hotkey::install`] (the main
//! thread in ZTab). The handler is invoked synchronously from the tap callback on that thread.
//!
//! State machine (see [`decide`]):
//! - closed: Option+Tab keyDown -> `Open`, Option+` keyDown -> `OpenSameApp`; both swallowed, and so
//!   is their matching keyUp. Everything else passes through untouched.
//! - open: every keyDown/keyUp is swallowed. Tab/Right/` -> `Next`, Shift+Tab/Left/Shift+` -> `Prev`,
//!   Up/Down -> `Up`/`Down`, Return/Enter -> `Commit`, Escape -> `Cancel`. A flagsChanged without
//!   Option -> `Commit` (the flagsChanged itself is passed through).
//!
//! The tap itself flips `open` to true right before emitting `Open`/`OpenSameApp` and to false right
//! before emitting `Commit`/`Cancel`, so a fast Option release can never be missed even if the
//! integrator updates its UI asynchronously. The integrator may still call `set_open` at any time
//! (for example `set_open(false)` from the handler when there is nothing to show).

#![allow(dead_code)]

use std::cell::Cell;
use std::ffi::c_void;
use std::ptr::{self, NonNull};

use objc2_core_foundation::{CFMachPort, CFRetained, CFRunLoop, CFRunLoopSource, kCFRunLoopCommonModes};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventMask, CGEventSource, CGEventSourceStateID,
    CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventTapProxy, CGEventType,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    /// Option+Tab (reverse=true with Shift) pressed while the switcher is closed.
    Open { reverse: bool },
    /// Tab / Right arrow / ` while open.
    Next,
    /// Shift+Tab / Left arrow / Shift+` while open.
    Prev,
    /// Up arrow while open.
    Up,
    /// Down arrow while open.
    Down,
    /// Option released while open, or Return / keypad Enter while open.
    Commit,
    /// Escape while open.
    Cancel,
    /// Option+` (kVK_ANSI_Grave): switcher limited to the frontmost app's windows.
    OpenSameApp { reverse: bool },
}

pub mod keycode {
    pub const TAB: u16 = 0x30;
    pub const GRAVE: u16 = 0x32;
    pub const ESCAPE: u16 = 0x35;
    pub const RETURN: u16 = 0x24;
    pub const KEYPAD_ENTER: u16 = 0x4C;
    pub const LEFT: u16 = 0x7B;
    pub const RIGHT: u16 = 0x7C;
    pub const DOWN: u16 = 0x7D;
    pub const UP: u16 = 0x7E;
    pub const OPTION: u16 = 0x3A;
    pub const RIGHT_OPTION: u16 = 0x3D;
    pub const SHIFT: u16 = 0x38;
}

/// Keyboard event kinds the decision logic distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    Down,
    Up,
    FlagsChanged,
}

/// Modifier state relevant to the switcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods {
    pub option: bool,
    pub shift: bool,
    pub command: bool,
    pub control: bool,
}

impl Mods {
    pub fn from_flags(flags: CGEventFlags) -> Mods {
        Mods {
            option: flags.contains(CGEventFlags::MaskAlternate),
            shift: flags.contains(CGEventFlags::MaskShift),
            command: flags.contains(CGEventFlags::MaskCommand),
            control: flags.contains(CGEventFlags::MaskControl),
        }
    }

    /// Option held, Command/Control not held (Shift allowed).
    fn is_trigger(self) -> bool {
        self.option && !self.command && !self.control
    }
}

/// What the tap does with one event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    /// Return NULL from the tap (the event never reaches other taps or apps).
    pub swallow: bool,
    /// Event to hand to the integrator, if any.
    pub emit: Option<HotkeyEvent>,
}

/// Mutable state of the decision logic: whether the switcher is open and which keycodes had their
/// keyDown swallowed (so their keyUp is swallowed too, whatever the open state is by then).
#[derive(Debug, Default)]
pub struct TapState {
    pub open: Cell<bool>,
    swallowed_downs: Cell<u128>,
}

impl TapState {
    /// Forget all pending keyUp swallows (after the tap was disabled, keyUps may have passed).
    pub fn clear_swallowed(&self) {
        self.swallowed_downs.set(0);
    }
}

impl TapState {
    fn mark_down(&self, code: u16) {
        if code < 128 {
            self.swallowed_downs.set(self.swallowed_downs.get() | (1u128 << code));
        }
    }

    /// Returns whether the bit was set, clearing it.
    fn take_down(&self, code: u16) -> bool {
        if code >= 128 {
            return false;
        }
        let bits = self.swallowed_downs.get();
        let bit = 1u128 << code;
        self.swallowed_downs.set(bits & !bit);
        bits & bit != 0
    }
}

const PASS: Decision = Decision { swallow: false, emit: None };

/// Pure decision logic for one keyboard event. Updates `state` (open flag, swallowed-keyDown set).
pub fn decide(state: &TapState, kind: KeyKind, code: u16, mods: Mods, autorepeat: bool) -> Decision {
    let open = state.open.get();
    match kind {
        KeyKind::FlagsChanged => {
            if open && !mods.option {
                state.open.set(false);
                Decision { swallow: false, emit: Some(HotkeyEvent::Commit) }
            } else {
                PASS
            }
        }
        KeyKind::Up => {
            let was_swallowed = state.take_down(code);
            Decision { swallow: was_swallowed || open, emit: None }
        }
        KeyKind::Down if !open => {
            let opener = match code {
                keycode::TAB => Some(HotkeyEvent::Open { reverse: mods.shift }),
                keycode::GRAVE => Some(HotkeyEvent::OpenSameApp { reverse: mods.shift }),
                _ => None,
            };
            match opener {
                Some(ev) if mods.is_trigger() => {
                    state.mark_down(code);
                    if autorepeat {
                        // Held Option+Tab after the switcher was closed: do not reopen in a burst,
                        // but do not leak a Tab into the focused app either.
                        Decision { swallow: true, emit: None }
                    } else {
                        state.open.set(true);
                        Decision { swallow: true, emit: Some(ev) }
                    }
                }
                _ => {
                    // This keyDown reaches the app, so its keyUp must too.
                    state.take_down(code);
                    PASS
                }
            }
        }
        KeyKind::Down => {
            state.mark_down(code);
            if !mods.option {
                // Option is not down but we still think we are open: the flagsChanged release was
                // missed (tap disabled, secure input...). Behave as if it had been seen.
                state.open.set(false);
                return Decision { swallow: true, emit: Some(HotkeyEvent::Commit) };
            }
            let emit = match code {
                keycode::TAB | keycode::GRAVE => {
                    Some(if mods.shift { HotkeyEvent::Prev } else { HotkeyEvent::Next })
                }
                keycode::RIGHT => Some(HotkeyEvent::Next),
                keycode::LEFT => Some(HotkeyEvent::Prev),
                keycode::UP => Some(HotkeyEvent::Up),
                keycode::DOWN => Some(HotkeyEvent::Down),
                keycode::RETURN | keycode::KEYPAD_ENTER if !autorepeat => Some(HotkeyEvent::Commit),
                keycode::ESCAPE if !autorepeat => Some(HotkeyEvent::Cancel),
                _ => None,
            };
            if matches!(emit, Some(HotkeyEvent::Commit | HotkeyEvent::Cancel)) {
                state.open.set(false);
            }
            Decision { swallow: true, emit }
        }
    }
}

/// State shared between `Hotkey` and the C callback. Leaked on install (one per app), so the
/// callback's `&'static Shared` is valid for the life of the process, whatever the handler does.
struct Shared {
    state: TapState,
    /// The tap port, kept retained by `Hotkey`; `None` once the `Hotkey` is dropped.
    tap: Cell<Option<NonNull<CFMachPort>>>,
    handler: Box<dyn Fn(HotkeyEvent)>,
    /// Number of kCGEventTapDisabledByTimeout / ByUserInput notifications received.
    disabled_count: Cell<u32>,
    /// Disabled on purpose via `Hotkey::set_enabled(false)`: don't auto re-enable.
    paused: Cell<bool>,
}

impl Shared {
    fn emit(&self, ev: HotkeyEvent) {
        (self.handler)(ev);
    }

    fn reenable(&self) {
        if self.paused.get() {
            return;
        }
        if let Some(tap) = self.tap.get() {
            // SAFETY: the pointer is cleared before `Hotkey` releases its retain.
            let tap = unsafe { tap.as_ref() };
            if !CGEvent::tap_is_enabled(tap) {
                CGEvent::tap_enable(tap, true);
            }
        }
        self.recover_stuck_open();
    }

    /// If we think the switcher is open but Option is physically up, a release was missed: commit.
    fn recover_stuck_open(&self) {
        if self.state.open.get() && !option_down_now() {
            self.state.open.set(false);
            self.emit(HotkeyEvent::Commit);
        }
    }
}

/// Whether Option is currently held, according to the combined session state.
pub fn option_down_now() -> bool {
    CGEventSource::flags_state(CGEventSourceStateID::CombinedSessionState)
        .contains(CGEventFlags::MaskAlternate)
}

unsafe extern "C-unwind" fn tap_callback(
    _proxy: CGEventTapProxy,
    ty: CGEventType,
    event: NonNull<CGEvent>,
    user_info: *mut c_void,
) -> *mut CGEvent {
    // SAFETY: user_info is the leaked `&'static Shared` passed to CGEventTapCreate.
    let shared = unsafe { &*(user_info as *const Shared) };
    let pass = event.as_ptr();

    if ty == CGEventType::TapDisabledByTimeout || ty == CGEventType::TapDisabledByUserInput {
        shared.disabled_count.set(shared.disabled_count.get() + 1);
        // keyUps may have passed while disabled; don't swallow an unrelated later keyUp.
        shared.state.clear_swallowed();
        shared.reenable();
        return pass;
    }
    let kind = match ty {
        CGEventType::KeyDown => KeyKind::Down,
        CGEventType::KeyUp => KeyKind::Up,
        CGEventType::FlagsChanged => KeyKind::FlagsChanged,
        _ => return pass,
    };
    // SAFETY: for keyboard event types the event pointer is a valid CGEvent for this call.
    let ev = unsafe { event.as_ref() };
    let mods = Mods::from_flags(CGEvent::flags(Some(ev)));
    let code = CGEvent::integer_value_field(Some(ev), CGEventField::KeyboardEventKeycode) as u16;
    let autorepeat = CGEvent::integer_value_field(Some(ev), CGEventField::KeyboardEventAutorepeat) != 0;

    let d = decide(&shared.state, kind, code, mods, autorepeat);
    if let Some(e) = d.emit {
        shared.emit(e);
    }
    if d.swallow { ptr::null_mut() } else { pass }
}

/// An installed global hotkey tap. `!Send`/`!Sync`: use it on the thread that installed it.
pub struct Hotkey {
    shared: &'static Shared,
    tap: CFRetained<CFMachPort>,
    source: CFRetained<CFRunLoopSource>,
    run_loop: CFRetained<CFRunLoop>,
    location: CGEventTapLocation,
}

/// Cheap `Copy` handle onto the tap state, usable from inside the handler (which is created before
/// the `Hotkey` exists). Stays valid (as a no-op on the tap) after the `Hotkey` is dropped.
#[derive(Clone, Copy)]
pub struct HotkeyHandle {
    shared: &'static Shared,
}

impl HotkeyHandle {
    pub fn set_open(&self, open: bool) {
        self.shared.state.open.set(open);
    }
    pub fn is_open(&self) -> bool {
        self.shared.state.open.get()
    }
    pub fn reenable(&self) {
        self.shared.reenable();
    }
}

const ACCESSIBILITY_HINT: &str = "could not create the keyboard event tap: grant ZTab Accessibility \
     access in System Settings > Privacy & Security > Accessibility, then restart ZTab";

impl Hotkey {
    /// Install an active, head-inserted event tap for keyDown, keyUp and flagsChanged on the
    /// current thread's run loop (common modes). Tries `kCGHIDEventTap` first, then
    /// `kCGSessionEventTap`. `handler` runs synchronously on this thread inside the tap callback
    /// and must be fast (the system disables taps that take ~1 s).
    pub fn install(handler: Box<dyn Fn(HotkeyEvent)>) -> Result<Hotkey, String> {
        Self::install_at(handler, &[CGEventTapLocation::HIDEventTap, CGEventTapLocation::SessionEventTap])
    }

    /// Like [`Hotkey::install`], trying the given tap locations in order.
    pub fn install_at(
        handler: Box<dyn Fn(HotkeyEvent)>,
        locations: &[CGEventTapLocation],
    ) -> Result<Hotkey, String> {
        let mask: CGEventMask = (1 << CGEventType::KeyDown.0)
            | (1 << CGEventType::KeyUp.0)
            | (1 << CGEventType::FlagsChanged.0);
        let shared: &'static Shared = Box::leak(Box::new(Shared {
            state: TapState::default(),
            tap: Cell::new(None),
            handler,
            disabled_count: Cell::new(0),
            paused: Cell::new(false),
        }));
        let user_info = shared as *const Shared as *mut c_void;

        let mut created = None;
        for &loc in locations {
            // SAFETY: callback matches CGEventTapCallBack; user_info is 'static.
            let tap = unsafe {
                CGEvent::tap_create(
                    loc,
                    CGEventTapPlacement::HeadInsertEventTap,
                    CGEventTapOptions::Default,
                    mask,
                    Some(tap_callback),
                    user_info,
                )
            };
            if let Some(tap) = tap {
                created = Some((tap, loc));
                break;
            }
        }
        let Some((tap, location)) = created else {
            return Err(ACCESSIBILITY_HINT.to_string());
        };

        let source = CFMachPort::new_run_loop_source(None, Some(&tap), 0)
            .ok_or_else(|| "CFMachPortCreateRunLoopSource failed".to_string())?;
        let run_loop = CFRunLoop::current().ok_or_else(|| "no current run loop".to_string())?;
        // SAFETY: reading an immutable CF constant.
        let common = unsafe { kCFRunLoopCommonModes };
        run_loop.add_source(Some(&source), common);
        shared.tap.set(Some(NonNull::from(&*tap)));
        CGEvent::tap_enable(&tap, true);

        let hk = Hotkey { shared, tap, source, run_loop, location };
        if !CGEvent::tap_is_enabled(&hk.tap) {
            return Err(ACCESSIBILITY_HINT.to_string());
        }
        Ok(hk)
    }

    /// Tell the tap whether the switcher UI is open (decides whether keys are swallowed/translated).
    pub fn set_open(&self, open: bool) {
        self.shared.state.open.set(open);
    }

    pub fn is_open(&self) -> bool {
        self.shared.state.open.get()
    }

    /// Re-enable the tap if the system disabled it (no-op while paused by `set_enabled(false)`),
    /// and commit if the switcher is open while Option
    /// is no longer held (a missed release). Safe to call from a periodic watchdog timer.
    pub fn reenable(&self) {
        self.shared.reenable();
    }

    /// Pause (`false`) or resume (`true`) the tap. While paused, neither the disabled-notification
    /// handling in the callback nor `reenable()` turns it back on.
    pub fn set_enabled(&self, enabled: bool) {
        self.shared.paused.set(!enabled);
        CGEvent::tap_enable(&self.tap, enabled);
    }

    /// Whether the tap is currently enabled.
    pub fn is_enabled(&self) -> bool {
        CGEvent::tap_is_enabled(&self.tap)
    }

    /// How many kCGEventTapDisabledByTimeout / ByUserInput notifications the callback received
    /// (`set_enabled(false)` itself produces one ByUserInput notification).
    pub fn disabled_count(&self) -> u32 {
        self.shared.disabled_count.get()
    }

    /// Where the tap was installed (`HIDEventTap` or `SessionEventTap`).
    pub fn location(&self) -> CGEventTapLocation {
        self.location
    }

    /// A `Copy` handle for use inside the handler closure.
    pub fn handle(&self) -> HotkeyHandle {
        HotkeyHandle { shared: self.shared }
    }
}

impl Drop for Hotkey {
    fn drop(&mut self) {
        CGEvent::tap_enable(&self.tap, false);
        // SAFETY: reading an immutable CF constant.
        let common = unsafe { kCFRunLoopCommonModes };
        self.run_loop.remove_source(Some(&self.source), common);
        CFMachPort::invalidate(&self.tap);
        self.shared.tap.set(None);
        self.shared.state.open.set(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use HotkeyEvent::*;
    use KeyKind::{Down as KD, FlagsChanged as FC, Up as KU};

    const OPT: Mods = Mods { option: true, shift: false, command: false, control: false };
    const OPT_SHIFT: Mods = Mods { option: true, shift: true, command: false, control: false };
    const NONE: Mods = Mods { option: false, shift: false, command: false, control: false };

    fn d(swallow: bool, emit: Option<HotkeyEvent>) -> Decision {
        Decision { swallow, emit }
    }

    #[test]
    fn full_cycle() {
        let s = TapState::default();
        assert_eq!(decide(&s, FC, keycode::OPTION, OPT, false), d(false, None));
        assert_eq!(decide(&s, KD, keycode::TAB, OPT, false), d(true, Some(Open { reverse: false })));
        assert!(s.open.get());
        assert_eq!(decide(&s, KU, keycode::TAB, OPT, false), d(true, None));
        assert_eq!(decide(&s, KD, keycode::TAB, OPT, false), d(true, Some(Next)));
        assert_eq!(decide(&s, KD, keycode::TAB, OPT, true), d(true, Some(Next)));
        assert_eq!(decide(&s, KU, keycode::TAB, OPT, false), d(true, None));
        assert_eq!(decide(&s, FC, keycode::SHIFT, OPT_SHIFT, false), d(false, None));
        assert_eq!(decide(&s, KD, keycode::TAB, OPT_SHIFT, false), d(true, Some(Prev)));
        assert_eq!(decide(&s, KU, keycode::TAB, OPT_SHIFT, false), d(true, None));
        assert_eq!(decide(&s, KD, keycode::LEFT, OPT, false), d(true, Some(Prev)));
        assert_eq!(decide(&s, KD, keycode::RIGHT, OPT, false), d(true, Some(Next)));
        assert_eq!(decide(&s, KD, keycode::UP, OPT, false), d(true, Some(Up)));
        assert_eq!(decide(&s, KD, keycode::DOWN, OPT, false), d(true, Some(Down)));
        assert_eq!(decide(&s, KD, 0x00, OPT, false), d(true, None)); // 'a' swallowed
        assert_eq!(decide(&s, FC, keycode::OPTION, NONE, false), d(false, Some(Commit)));
        assert!(!s.open.get());
        // Closed again: plain keys pass.
        assert_eq!(decide(&s, KD, keycode::TAB, NONE, false), d(false, None));
        assert_eq!(decide(&s, KU, keycode::TAB, NONE, false), d(false, None));
    }

    #[test]
    fn closed_passthrough_and_modifier_guards() {
        let s = TapState::default();
        assert_eq!(decide(&s, KD, keycode::TAB, NONE, false), d(false, None));
        let cmd = Mods { command: true, ..OPT };
        let ctrl = Mods { control: true, ..OPT };
        assert_eq!(decide(&s, KD, keycode::TAB, cmd, false), d(false, None));
        assert_eq!(decide(&s, KD, keycode::TAB, ctrl, false), d(false, None));
        assert_eq!(decide(&s, KD, keycode::ESCAPE, OPT, false), d(false, None));
        assert_eq!(decide(&s, KD, keycode::RETURN, OPT, false), d(false, None));
        assert_eq!(decide(&s, FC, keycode::OPTION, NONE, false), d(false, None));
        assert!(!s.open.get());
    }

    #[test]
    fn reverse_open_and_same_app() {
        let s = TapState::default();
        assert_eq!(decide(&s, KD, keycode::TAB, OPT_SHIFT, false), d(true, Some(Open { reverse: true })));
        s.open.set(false);
        assert_eq!(decide(&s, KU, keycode::TAB, OPT_SHIFT, false), d(true, None)); // matching keyUp
        assert_eq!(decide(&s, KD, keycode::GRAVE, OPT, false), d(true, Some(OpenSameApp { reverse: false })));
        assert_eq!(decide(&s, KD, keycode::GRAVE, OPT, false), d(true, Some(Next)));
        assert_eq!(decide(&s, KD, keycode::GRAVE, OPT_SHIFT, false), d(true, Some(Prev)));
    }

    #[test]
    fn escape_and_return_keyups_swallowed_after_close() {
        let s = TapState::default();
        decide(&s, KD, keycode::TAB, OPT, false);
        decide(&s, KU, keycode::TAB, OPT, false);
        assert_eq!(decide(&s, KD, keycode::ESCAPE, OPT, false), d(true, Some(Cancel)));
        assert!(!s.open.get());
        assert_eq!(decide(&s, KU, keycode::ESCAPE, OPT, false), d(true, None));
        assert_eq!(decide(&s, KU, keycode::ESCAPE, OPT, false), d(false, None)); // only once
        decide(&s, KD, keycode::TAB, OPT, false);
        assert_eq!(decide(&s, KD, keycode::KEYPAD_ENTER, OPT, false), d(true, Some(Commit)));
        assert_eq!(decide(&s, KU, keycode::KEYPAD_ENTER, OPT, false), d(true, None));
        // Option released after Commit: flagsChanged passes, no second Commit.
        assert_eq!(decide(&s, FC, keycode::OPTION, NONE, false), d(false, None));
    }

    #[test]
    fn passed_keydown_clears_stale_keyup_swallow() {
        let s = TapState::default();
        decide(&s, KD, keycode::TAB, OPT, false); // Open, bit set
        s.open.set(false); // integrator closed; keyUp was lost (tap disabled)
        assert_eq!(decide(&s, KD, keycode::TAB, NONE, false), d(false, None));
        assert_eq!(decide(&s, KU, keycode::TAB, NONE, false), d(false, None));
    }

    #[test]
    fn autorepeat_while_closed_does_not_reopen() {
        let s = TapState::default();
        assert_eq!(decide(&s, KD, keycode::TAB, OPT, true), d(true, None));
        assert!(!s.open.get());
    }

    #[test]
    fn missed_option_release_commits_on_next_key() {
        let s = TapState::default();
        decide(&s, KD, keycode::TAB, OPT, false);
        assert_eq!(decide(&s, KD, 0x00, NONE, false), d(true, Some(Commit)));
        assert!(!s.open.get());
        assert_eq!(decide(&s, KU, 0x00, NONE, false), d(true, None));
    }
}
