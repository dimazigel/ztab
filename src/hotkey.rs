//! Global Option+Tab interception through an active `CGEventTap`.
//!
//! An active tap holds every keyboard event of the session until its callback returns, so the
//! thread that services it must never be busy. [`Hotkey::spawn`] (ZTab) installs the tap on its
//! own high-priority `ztab-tap` thread, whose run loop does nothing else; the handler runs there
//! and must only hand the event on (ZTab queues it to the main thread). [`Hotkey::install`] uses
//! the calling thread's run loop instead (tools and tests).
//!
//! The two shortcuts are [`Bindings`]: a hold modifier (⌥, ⌘ or ⌃) plus a key, by default ⌥Tab
//! (all windows) and ⌥` (current app). They live in the tap's [`TapState`] as one atomic word, so
//! another thread can replace them at any time without the tap ever waiting
//! ([`HotkeyHandle::set_bindings`]). While a shortcut recorder is capturing
//! ([`HotkeyHandle::set_recording`]), the tap neither opens nor swallows anything new, so the keys
//! reach the recorder and cannot start a switch.
//!
//! State machine (see [`decide`]):
//! - closed: hold+key of the all-windows binding -> `Open`, of the same-app binding ->
//!   `OpenSameApp` (Shift may be held too: reverse); both swallowed, and so is their matching
//!   keyUp. The binding's hold modifier becomes the session's. Everything else passes through
//!   untouched.
//! - open: every keyDown/keyUp is swallowed. Tab/Right/`/either binding's key -> `Next`, with
//!   Shift (or Left) -> `Prev`, Up/Down -> `Up`/`Down`, Return/Enter -> `Commit`, Escape ->
//!   `Cancel`. A flagsChanged without the session's hold modifier -> `Commit` (the flagsChanged
//!   itself is passed through).
//!
//! The tap itself opens a new session (numbered) right before emitting `Open`/`OpenSameApp` and
//! closes it right before emitting `Commit`/`Cancel`, so a fast Option release is never missed,
//! however late the integrator handles the events. The handler gets each event with the number of
//! the session it belongs to. The integrator may close the tap itself, for example with
//! [`HotkeyHandle::close_session`] when there is nothing to show: that closes only the given
//! session, never a newer one the tap opened meanwhile.

#![allow(dead_code)]

use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU32, AtomicU64, Ordering};

use block2::RcBlock;
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
    pub const CONTROL: u16 = 0x3B;
    pub const COMMAND: u16 = 0x37;
    pub const SPACE: u16 = 0x31;
    pub const F18: u16 = 0x4F;
    pub const F19: u16 = 0x50;
    /// Right Command (0x36) through Fn (0x3F): modifier keys, never a binding's key.
    pub fn is_modifier(code: u16) -> bool {
        (0x36..=0x3F).contains(&code)
    }
}

// ---------------------------------------------------------------------------------------------
// Bindings
// ---------------------------------------------------------------------------------------------

/// The modifier held while the switcher is open (releasing it switches).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Hold {
    Option = 1,
    Command = 2,
    Control = 3,
}

impl Hold {
    fn from_u8(v: u8) -> Option<Hold> {
        match v {
            1 => Some(Hold::Option),
            2 => Some(Hold::Command),
            3 => Some(Hold::Control),
            _ => None,
        }
    }

    pub fn symbol(self) -> &'static str {
        match self {
            Hold::Option => "⌥",
            Hold::Command => "⌘",
            Hold::Control => "⌃",
        }
    }

    /// Name in the settings file.
    pub fn name(self) -> &'static str {
        match self {
            Hold::Option => "option",
            Hold::Command => "command",
            Hold::Control => "control",
        }
    }

    pub fn from_name(s: &str) -> Option<Hold> {
        match s.trim().to_ascii_lowercase().as_str() {
            "option" | "alt" | "opt" => Some(Hold::Option),
            "command" | "cmd" => Some(Hold::Command),
            "control" | "ctrl" => Some(Hold::Control),
            _ => None,
        }
    }

    pub fn flag(self) -> CGEventFlags {
        match self {
            Hold::Option => CGEventFlags::MaskAlternate,
            Hold::Command => CGEventFlags::MaskCommand,
            Hold::Control => CGEventFlags::MaskControl,
        }
    }
}

/// One shortcut: hold modifier + key (a virtual keycode). Shift is never part of it: Shift held
/// as well opens in reverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Binding {
    pub hold: Hold,
    pub key: u16,
}

impl Binding {
    pub const DEFAULT_ALL: Binding = Binding { hold: Hold::Option, key: keycode::TAB };
    pub const DEFAULT_SAME_APP: Binding = Binding { hold: Hold::Option, key: keycode::GRAVE };

    fn encode(self) -> u32 {
        (u32::from(self.hold as u8) << 16) | u32::from(self.key)
    }

    fn decode(v: u32) -> Option<Binding> {
        let hold = Hold::from_u8((v >> 16) as u8)?;
        Some(Binding { hold, key: (v & 0xFFFF) as u16 })
    }

    /// The keyDown `code` with modifiers `mods` triggers this binding: its hold modifier is down
    /// and no other of ⌥ / ⌘ / ⌃ (Shift may be: reverse).
    pub fn matches(self, code: u16, mods: Mods) -> bool {
        code == self.key && mods.is_trigger_for(self.hold)
    }

    /// For display, e.g. "⌥Tab", "⌃F18".
    pub fn label(self) -> String {
        format!("{}{}", self.hold.symbol(), key_name(self.key))
    }

    /// Settings-file form, e.g. "option+48".
    pub fn to_conf(self) -> String {
        format!("{}+{}", self.hold.name(), self.key)
    }

    /// Parse the settings-file form ("option+48"; the keycode may be decimal or 0x hex).
    pub fn parse_conf(s: &str) -> Option<Binding> {
        let (hold, key) = s.trim().rsplit_once('+')?;
        let hold = Hold::from_name(hold)?;
        let key = key.trim();
        let key: u16 = match key.strip_prefix("0x") {
            Some(hex) => u16::from_str_radix(hex, 16).ok()?,
            None => key.parse().ok()?,
        };
        (key < 128 && !keycode::is_modifier(key)).then_some(Binding { hold, key })
    }

    /// The binding a recorder makes of a keyDown `code` with the given modifiers held: exactly one
    /// of ⌥ / ⌘ / ⌃ is the hold modifier; Shift is dropped (it reverses the direction).
    pub fn from_recorded(code: u16, mods: Mods) -> Result<Binding, String> {
        if keycode::is_modifier(code) || code >= 128 {
            return Err("Press a key together with the modifier.".into());
        }
        let holds: Vec<Hold> = [(mods.option, Hold::Option), (mods.command, Hold::Command), (mods.control, Hold::Control)]
            .into_iter()
            .filter_map(|(on, h)| on.then_some(h))
            .collect();
        match holds.as_slice() {
            [] if mods.shift => Err("⇧ can't be the hold modifier (it reverses the direction). Use ⌥, ⌘ or ⌃.".into()),
            [] => Err(format!("{} alone would stop you typing it: hold ⌥, ⌘ or ⌃ with the key.", key_name(code))),
            [h] => Ok(Binding { hold: *h, key: code }),
            _ => Err("Hold only one of ⌥, ⌘ and ⌃ (the one you keep down while switching).".into()),
        }
    }
}

/// The two shortcuts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bindings {
    /// Every window.
    pub all: Binding,
    /// Windows of the current app.
    pub same_app: Binding,
}

impl Default for Bindings {
    fn default() -> Self {
        Bindings { all: Binding::DEFAULT_ALL, same_app: Binding::DEFAULT_SAME_APP }
    }
}

impl Bindings {
    fn encode(self) -> u64 {
        (u64::from(self.all.encode()) << 32) | u64::from(self.same_app.encode())
    }

    fn decode(v: u64) -> Bindings {
        let d = Bindings::default();
        Bindings {
            all: Binding::decode((v >> 32) as u32).unwrap_or(d.all),
            same_app: Binding::decode(v as u32).unwrap_or(d.same_app),
        }
    }
}

/// Which of the two shortcuts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    All,
    SameApp,
}

impl Which {
    pub fn describe(self) -> &'static str {
        match self {
            Which::All => "switching between all windows",
            Which::SameApp => "switching between the current app's windows",
        }
    }
}

/// Check binding `b` for `which`, given the other one. `Ok(Some(warning))`: allowed, but it takes
/// over a common macOS shortcut. `Err`: not allowed (would break typing, is used inside the
/// switcher, belongs to macOS, or is already the other shortcut).
pub fn validate(which: Which, b: Binding, other: Binding) -> Result<Option<String>, String> {
    if b == other {
        let other_which = if which == Which::All { Which::SameApp } else { Which::All };
        return Err(format!("{} is already the shortcut for {}.", b.label(), other_which.describe()));
    }
    validate_alone(b)
}

/// The checks of [`validate`] that do not depend on the other shortcut.
pub fn validate_alone(b: Binding) -> Result<Option<String>, String> {
    use keycode::*;
    if keycode::is_modifier(b.key) || b.key >= 128 {
        return Err("Press a key together with the modifier.".into());
    }
    if matches!(b.key, ESCAPE | RETURN | KEYPAD_ENTER) {
        return Err(format!("{} is used inside the switcher (Esc cancels, Return switches).", key_name(b.key)));
    }
    if matches!(b.key, LEFT | RIGHT | UP | DOWN) {
        return Err(format!("{} is used inside the switcher (the arrow keys move the selection).", key_name(b.key)));
    }
    if b.hold == Hold::Command && b.key == TAB {
        return Err("⌘Tab is macOS's app switcher; ZTab can't take it over.".into());
    }
    let name = key_name(b.key);
    let warning = match (b.hold, b.key) {
        (Hold::Command, GRAVE) => Some(format!("{} replaces macOS's ⌘` (next window of the app) everywhere.", b.label())),
        (Hold::Command, SPACE) => Some("⌘Space is Spotlight's shortcut; ZTab will take it over.".to_string()),
        (Hold::Control, SPACE) => Some("⌃Space switches input sources in macOS; ZTab will take it over.".to_string()),
        (Hold::Command, _) if name.chars().count() == 1 && name.chars().all(|c| c.is_ascii_alphanumeric()) => {
            Some(format!("{} is a standard shortcut in most apps; ZTab will take it over everywhere.", b.label()))
        }
        _ => None,
    };
    // Inside macOS's own ⌘Tab app switcher the key keeps its system meaning: ZTab does not open
    // until ⌘ is released (see `decide`).
    let warning = match (b.hold, warning) {
        (Hold::Command, Some(w)) => Some(format!("{w} (Not inside macOS's ⌘Tab app switcher.)")),
        (_, w) => w,
    };
    Ok(warning)
}

/// Name of a virtual keycode on a US keyboard ("Tab", "`", "A", "F18", "←", …).
pub fn key_name(code: u16) -> String {
    const LETTERS: [(u16, &str); 50] = [
        (0x00, "A"), (0x01, "S"), (0x02, "D"), (0x03, "F"), (0x04, "H"), (0x05, "G"), (0x06, "Z"), (0x07, "X"),
        (0x08, "C"), (0x09, "V"), (0x0A, "§"), (0x0B, "B"), (0x0C, "Q"), (0x0D, "W"), (0x0E, "E"), (0x0F, "R"),
        (0x10, "Y"), (0x11, "T"), (0x12, "1"), (0x13, "2"), (0x14, "3"), (0x15, "4"), (0x16, "6"), (0x17, "5"),
        (0x18, "="), (0x19, "9"), (0x1A, "7"), (0x1B, "-"), (0x1C, "8"), (0x1D, "0"), (0x1E, "]"), (0x1F, "O"),
        (0x20, "U"), (0x21, "["), (0x22, "I"), (0x23, "P"), (0x25, "L"), (0x26, "J"), (0x27, "'"), (0x28, "K"),
        (0x29, ";"), (0x2A, "\\"), (0x2B, ","), (0x2C, "/"), (0x2D, "N"), (0x2E, "M"), (0x2F, "."), (0x32, "`"),
        (0x24, "Return"), (0x30, "Tab"),
    ];
    const OTHERS: [(u16, &str); 50] = [
        (0x31, "Space"), (0x33, "⌫"), (0x35, "Esc"), (0x75, "⌦"), (0x73, "Home"), (0x77, "End"), (0x74, "Page Up"),
        (0x79, "Page Down"), (0x7B, "←"), (0x7C, "→"), (0x7D, "↓"), (0x7E, "↑"), (0x72, "Help"), (0x7A, "F1"),
        (0x78, "F2"), (0x63, "F3"), (0x76, "F4"), (0x60, "F5"), (0x61, "F6"), (0x62, "F7"), (0x64, "F8"), (0x65, "F9"),
        (0x6D, "F10"), (0x67, "F11"), (0x6F, "F12"), (0x69, "F13"), (0x6B, "F14"), (0x71, "F15"), (0x6A, "F16"),
        (0x40, "F17"), (0x4F, "F18"), (0x50, "F19"), (0x5A, "F20"), (0x41, "Keypad ."), (0x43, "Keypad *"),
        (0x45, "Keypad +"), (0x47, "Clear"), (0x4B, "Keypad /"), (0x4C, "Enter"), (0x4E, "Keypad -"),
        (0x51, "Keypad ="), (0x52, "Keypad 0"), (0x53, "Keypad 1"), (0x54, "Keypad 2"), (0x55, "Keypad 3"),
        (0x56, "Keypad 4"), (0x57, "Keypad 5"), (0x58, "Keypad 6"), (0x59, "Keypad 7"), (0x5B, "Keypad 8"),
    ];
    if code == 0x5C {
        return "Keypad 9".into();
    }
    LETTERS
        .iter()
        .chain(OTHERS.iter())
        .find(|(c, _)| *c == code)
        .map(|(_, n)| (*n).to_string())
        .unwrap_or_else(|| format!("Key {code:#04x}"))
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

    /// `hold` is down.
    pub fn has(self, hold: Hold) -> bool {
        match hold {
            Hold::Option => self.option,
            Hold::Command => self.command,
            Hold::Control => self.control,
        }
    }

    /// `hold` held, and no other of ⌥ / ⌘ / ⌃ (Shift allowed).
    pub fn is_trigger_for(self, hold: Hold) -> bool {
        let others = [(Hold::Option, self.option), (Hold::Command, self.command), (Hold::Control, self.control)];
        self.has(hold) && others.iter().all(|&(h, on)| h == hold || !on)
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

/// Mutable state of the decision logic: whether the switcher is open (and the number of the
/// current session), which keycodes had their keyDown swallowed (so their keyUp is swallowed
/// too, whatever the open state is by then), the shortcuts, and whether a recorder is capturing.
/// Atomic: the tap thread decides, other threads may close a session, swap the shortcuts or
/// pause opening for a recorder.
#[derive(Debug)]
pub struct TapState {
    /// Bit 0: open. Bits 1..: number of the latest session (bumped by every Open).
    st: AtomicU64,
    /// Keycodes 0..63 and 64..127 whose keyDown was swallowed.
    swallowed_downs: [AtomicU64; 2],
    /// Both shortcuts, swapped as one word (`Bindings::encode`).
    bindings: AtomicU64,
    /// Hold modifier of the current session (`Hold as u8`), written by the tap thread on Open.
    hold: AtomicU8,
    /// A shortcut recorder is capturing: open nothing, swallow nothing new.
    recording: AtomicBool,
    /// macOS's ⌘Tab app switcher may be up: a ⌘Tab keyDown went through while ZTab was closed,
    /// and ⌘ has not been seen released since. ⌘-hold shortcuts do not open meanwhile, so their
    /// key reaches the system switcher (⌘` moves left there, ⌘Q quits the selected app...)
    /// instead of opening a second switcher on top of it. Cleared by any event without ⌘.
    system_switcher: AtomicBool,
}

impl Default for TapState {
    fn default() -> Self {
        TapState::with_bindings(Bindings::default())
    }
}

impl TapState {
    pub fn with_bindings(b: Bindings) -> TapState {
        TapState {
            st: AtomicU64::new(0),
            swallowed_downs: [AtomicU64::new(0), AtomicU64::new(0)],
            bindings: AtomicU64::new(b.encode()),
            hold: AtomicU8::new(Hold::Option as u8),
            recording: AtomicBool::new(false),
            system_switcher: AtomicBool::new(false),
        }
    }

    pub fn bindings(&self) -> Bindings {
        Bindings::decode(self.bindings.load(Ordering::SeqCst))
    }

    /// Replace both shortcuts (any thread). A session already open keeps its hold modifier.
    pub fn set_bindings(&self, b: Bindings) {
        self.bindings.store(b.encode(), Ordering::SeqCst);
    }

    /// Hold modifier of the current (or last) session.
    pub fn hold(&self) -> Hold {
        Hold::from_u8(self.hold.load(Ordering::SeqCst)).unwrap_or(Hold::Option)
    }

    pub fn is_recording(&self) -> bool {
        self.recording.load(Ordering::SeqCst)
    }

    pub fn set_recording(&self, on: bool) {
        self.recording.store(on, Ordering::SeqCst);
    }

    pub fn is_open(&self) -> bool {
        self.st.load(Ordering::SeqCst) & 1 == 1
    }

    /// Number of the latest session (0 before the first Open).
    pub fn session(&self) -> u64 {
        self.st.load(Ordering::SeqCst) >> 1
    }

    /// Open or close the current session (whatever its number).
    pub fn set_open(&self, open: bool) {
        if open {
            self.st.fetch_or(1, Ordering::SeqCst);
        } else {
            self.st.fetch_and(!1, Ordering::SeqCst);
        }
    }

    /// Close session `session` if it is still the current, open one. Returns whether it did.
    pub fn close_session(&self, session: u64) -> bool {
        self.st.compare_exchange(session << 1 | 1, session << 1, Ordering::SeqCst, Ordering::SeqCst).is_ok()
    }

    /// Start a new, open session; returns its number.
    fn open_new_session(&self) -> u64 {
        let mut v = self.st.load(Ordering::SeqCst);
        loop {
            let next = (((v >> 1) + 1) << 1) | 1;
            match self.st.compare_exchange_weak(v, next, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return next >> 1,
                Err(cur) => v = cur,
            }
        }
    }

    /// Whether macOS's ⌘Tab app switcher may be up (see `system_switcher`).
    pub fn in_system_switcher(&self) -> bool {
        self.system_switcher.load(Ordering::SeqCst)
    }

    /// Forget that macOS's ⌘Tab switcher may be up (⌘ is physically up: a release was missed).
    pub fn clear_system_switcher(&self) {
        self.system_switcher.store(false, Ordering::SeqCst);
    }

    /// Forget all pending keyUp swallows (after the tap was disabled, keyUps may have passed).
    pub fn clear_swallowed(&self) {
        for w in &self.swallowed_downs {
            w.store(0, Ordering::SeqCst);
        }
    }

    fn mark_down(&self, code: u16) {
        if code < 128 {
            self.swallowed_downs[usize::from(code / 64)].fetch_or(1u64 << (code % 64), Ordering::SeqCst);
        }
    }

    /// Returns whether the bit was set, clearing it.
    fn take_down(&self, code: u16) -> bool {
        if code >= 128 {
            return false;
        }
        let bit = 1u64 << (code % 64);
        self.swallowed_downs[usize::from(code / 64)].fetch_and(!bit, Ordering::SeqCst) & bit != 0
    }
}

const PASS: Decision = Decision { swallow: false, emit: None };

/// Pure decision logic for one keyboard event. Updates `state` (open flag and session, the
/// session's hold modifier, swallowed-keyDown set).
pub fn decide(state: &TapState, kind: KeyKind, code: u16, mods: Mods, autorepeat: bool) -> Decision {
    let open = state.is_open();
    if !mods.command {
        // ⌘ is up: macOS's ⌘Tab switcher (if it was up) is gone.
        state.system_switcher.store(false, Ordering::SeqCst);
    }
    match kind {
        KeyKind::FlagsChanged => {
            if open && !mods.has(state.hold()) {
                state.set_open(false);
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
            if state.is_recording() {
                // A shortcut recorder needs every key: nothing opens, nothing is swallowed.
                state.take_down(code);
                return PASS;
            }
            let b = state.bindings();
            let opener = if b.all.matches(code, mods) {
                Some((HotkeyEvent::Open { reverse: mods.shift }, b.all.hold))
            } else if b.same_app.matches(code, mods) {
                Some((HotkeyEvent::OpenSameApp { reverse: mods.shift }, b.same_app.hold))
            } else {
                None
            };
            if code == keycode::TAB && mods.command && opener.is_none() {
                // ⌘Tab (never a shortcut) goes to macOS, which shows its app switcher.
                state.system_switcher.store(true, Ordering::SeqCst);
            }
            let opener = opener.filter(|&(_, hold)| !(hold == Hold::Command && state.in_system_switcher()));
            match opener {
                Some((ev, hold)) => {
                    state.mark_down(code);
                    if autorepeat {
                        // Held hold+key after the switcher was closed: do not reopen in a burst,
                        // but do not leak the key into the focused app either.
                        Decision { swallow: true, emit: None }
                    } else {
                        state.hold.store(hold as u8, Ordering::SeqCst);
                        state.open_new_session();
                        Decision { swallow: true, emit: Some(ev) }
                    }
                }
                None => {
                    // This keyDown reaches the app, so its keyUp must too.
                    state.take_down(code);
                    PASS
                }
            }
        }
        KeyKind::Down => {
            state.mark_down(code);
            if !mods.has(state.hold()) {
                // The hold modifier is not down but we still think we are open: the flagsChanged
                // release was missed (tap disabled, secure input...). Behave as if it had been seen.
                state.set_open(false);
                return Decision { swallow: true, emit: Some(HotkeyEvent::Commit) };
            }
            let b = state.bindings();
            let emit = match code {
                keycode::RETURN | keycode::KEYPAD_ENTER if !autorepeat => Some(HotkeyEvent::Commit),
                keycode::ESCAPE if !autorepeat => Some(HotkeyEvent::Cancel),
                keycode::RETURN | keycode::KEYPAD_ENTER | keycode::ESCAPE => None,
                keycode::RIGHT => Some(HotkeyEvent::Next),
                keycode::LEFT => Some(HotkeyEvent::Prev),
                keycode::UP => Some(HotkeyEvent::Up),
                keycode::DOWN => Some(HotkeyEvent::Down),
                c if c == keycode::TAB || c == keycode::GRAVE || c == b.all.key || c == b.same_app.key => {
                    Some(if mods.shift { HotkeyEvent::Prev } else { HotkeyEvent::Next })
                }
                _ => None,
            };
            if matches!(emit, Some(HotkeyEvent::Commit | HotkeyEvent::Cancel)) {
                state.set_open(false);
            }
            Decision { swallow: true, emit }
        }
    }
}

/// The handler: an event and the number of the session it belongs to. Runs on the tap's thread.
pub type Handler = Box<dyn Fn(HotkeyEvent, u64) + Send + Sync>;

/// The run loop servicing the tap, for other threads to post work to.
struct SendRl(CFRetained<CFRunLoop>);
// SAFETY: other threads only call CFRunLoopPerformBlock and CFRunLoopWakeUp through it, which
// CoreFoundation documents as thread-safe.
unsafe impl Send for SendRl {}

/// State shared between the tap's thread, `Hotkey` / `HotkeyHandle` users and the C callback.
/// Leaked on install (one per app), so the callback's `&'static Shared` is valid for the life of
/// the process, whatever the handler does.
struct Shared {
    state: TapState,
    /// The tap port, kept retained by `Hotkey`; null once the `Hotkey` is dropped.
    tap: AtomicPtr<CFMachPort>,
    handler: Handler,
    /// Number of kCGEventTapDisabledByTimeout / ByUserInput notifications received.
    disabled_count: AtomicU32,
    /// Disabled on purpose via `Hotkey::set_enabled(false)`: don't auto re-enable.
    paused: AtomicBool,
    /// Events handed to the handler so far.
    emitted: AtomicU64,
    /// The tap's run loop and thread.
    run_loop: Mutex<Option<SendRl>>,
    thread: Mutex<Option<std::thread::ThreadId>>,
    location: Mutex<Option<CGEventTapLocation>>,
}

impl Shared {
    fn emit(&self, ev: HotkeyEvent) {
        self.emitted.fetch_add(1, Ordering::SeqCst);
        (self.handler)(ev, self.state.session());
    }

    fn tap(&self) -> Option<&CFMachPort> {
        // SAFETY: the pointer is cleared before `Hotkey` releases its retain.
        NonNull::new(self.tap.load(Ordering::SeqCst)).map(|p| unsafe { p.as_ref() })
    }

    fn on_tap_thread(&self) -> bool {
        *self.thread.lock().unwrap_or_else(|e| e.into_inner()) == Some(std::thread::current().id())
    }

    /// Re-enable the tap if macOS disabled it, and recover a missed Option release. Runs on the
    /// tap's thread (posted there from other threads), so its events go through the handler in
    /// order with the tap's own.
    fn reenable(&'static self) {
        if self.on_tap_thread() {
            self.reenable_here();
            return;
        }
        let rl = self.run_loop.lock().unwrap_or_else(|e| e.into_inner());
        let Some(rl) = rl.as_ref() else { return };
        let block = RcBlock::new(move || self.reenable_here());
        // SAFETY: kCFRunLoopCommonModes is a valid static; the block captures a 'static reference.
        unsafe {
            let mode = kCFRunLoopCommonModes.map(|m| &**m);
            rl.0.perform_block(mode, Some(&block));
        }
        rl.0.wake_up();
    }

    fn reenable_here(&self) {
        if self.paused.load(Ordering::SeqCst) {
            return;
        }
        if let Some(tap) = self.tap()
            && !CGEvent::tap_is_enabled(tap)
        {
            CGEvent::tap_enable(tap, true);
        }
        self.recover_stuck_open();
    }

    /// If we think the switcher is open but the session's hold modifier is physically up, a
    /// release was missed: commit.
    fn recover_stuck_open(&self) {
        if self.state.in_system_switcher() && !hold_down_now(Hold::Command) {
            self.state.clear_system_switcher();
        }
        if self.state.is_open() && !hold_down_now(self.state.hold()) {
            self.state.set_open(false);
            self.emit(HotkeyEvent::Commit);
        }
    }
}

/// Whether `hold` is currently held, according to the combined session state.
pub fn hold_down_now(hold: Hold) -> bool {
    CGEventSource::flags_state(CGEventSourceStateID::CombinedSessionState).contains(hold.flag())
}

/// Whether Option is currently held, according to the combined session state.
pub fn option_down_now() -> bool {
    hold_down_now(Hold::Option)
}

unsafe extern "C-unwind" fn tap_callback(
    _proxy: CGEventTapProxy,
    ty: CGEventType,
    event: NonNull<CGEvent>,
    user_info: *mut c_void,
) -> *mut CGEvent {
    // SAFETY: user_info is the leaked `&'static Shared` passed to CGEventTapCreate.
    let shared: &'static Shared = unsafe { &*(user_info as *const Shared) };
    let pass = event.as_ptr();

    if ty == CGEventType::TapDisabledByTimeout || ty == CGEventType::TapDisabledByUserInput {
        shared.disabled_count.fetch_add(1, Ordering::SeqCst);
        // keyUps may have passed while disabled; don't swallow an unrelated later keyUp.
        shared.state.clear_swallowed();
        shared.reenable_here();
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

/// An installed global hotkey tap, owning the tap port and its run-loop source. Dropping it
/// removes the tap.
pub struct Hotkey {
    shared: &'static Shared,
    tap: CFRetained<CFMachPort>,
    source: CFRetained<CFRunLoopSource>,
    run_loop: CFRetained<CFRunLoop>,
    location: CGEventTapLocation,
}

/// Cheap `Copy`, `Send` handle onto the tap state, usable from any thread. Stays valid (as a
/// no-op on the tap) after the `Hotkey` is dropped.
#[derive(Clone, Copy)]
pub struct HotkeyHandle {
    shared: &'static Shared,
}

impl HotkeyHandle {
    pub fn set_open(&self, open: bool) {
        self.shared.state.set_open(open);
    }
    pub fn is_open(&self) -> bool {
        self.shared.state.is_open()
    }
    /// Close session `session` (the number the handler got with its Open) unless the tap has
    /// opened a newer one meanwhile. Returns whether it closed it.
    pub fn close_session(&self, session: u64) -> bool {
        self.shared.state.close_session(session)
    }
    /// Re-enable the tap if the system disabled it (no-op while paused by `set_enabled(false)`)
    /// and commit if the switcher is open while Option is no longer held (a missed release).
    /// Runs on the tap's thread: from another thread it is posted there.
    pub fn reenable(&self) {
        self.shared.reenable();
    }
    /// Events handed to the handler so far.
    pub fn emitted(&self) -> u64 {
        self.shared.emitted.load(Ordering::SeqCst)
    }
    /// Replace both shortcuts (one atomic store: the tap never waits). Applies to the next
    /// keyDown; an open session keeps its hold modifier.
    pub fn set_bindings(&self, b: Bindings) {
        self.shared.state.set_bindings(b);
    }
    pub fn bindings(&self) -> Bindings {
        self.shared.state.bindings()
    }
    /// A shortcut recorder starts (`true`) or stops capturing: while it does, the tap opens
    /// nothing and swallows nothing new (a session already open finishes normally).
    pub fn set_recording(&self, on: bool) {
        self.shared.state.set_recording(on);
    }
    pub fn is_recording(&self) -> bool {
        self.shared.state.is_recording()
    }
    /// Where the tap was installed (`HIDEventTap` or `SessionEventTap`), once it is.
    pub fn location(&self) -> Option<CGEventTapLocation> {
        *self.shared.location.lock().unwrap_or_else(|e| e.into_inner())
    }
}

const ACCESSIBILITY_HINT: &str = "could not create the keyboard event tap: grant ZTab Accessibility \
     access in System Settings > Privacy & Security > Accessibility, then restart ZTab";

impl Hotkey {
    /// Install the tap on a new `ztab-tap` thread (user-interactive QoS) that runs only its run
    /// loop, and return at once. `on_result` runs on that thread once the tap exists (or could
    /// not be created; the thread then ends). `handler` runs on that thread for every event and
    /// must only hand it on: every keyboard event of the session waits for it.
    /// Errs (and `on_result` never runs) when the thread cannot be started. `bindings`: the
    /// shortcuts from the first event on.
    pub fn spawn(handler: Handler, bindings: Bindings, on_result: impl FnOnce(Result<HotkeyHandle, String>) + Send + 'static) -> Result<(), String> {
        let spawned = std::thread::Builder::new().name("ztab-tap".into()).spawn(move || {
            // SAFETY: plain libc call on the current thread.
            unsafe {
                libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
            }
            match Self::install_with(handler, bindings) {
                Ok(hk) => {
                    on_result(Ok(hk.handle()));
                    // The tap lives as long as the process: this thread only services it.
                    CFRunLoop::run();
                    drop(hk);
                }
                Err(e) => on_result(Err(e)),
            }
        });
        spawned.map(|_| ()).map_err(|e| format!("could not start the event tap thread: {e}"))
    }

    /// Install an active, head-inserted event tap for keyDown, keyUp and flagsChanged on the
    /// current thread's run loop (common modes). Tries `kCGHIDEventTap` first, then
    /// `kCGSessionEventTap`. `handler` runs synchronously on this thread inside the tap callback
    /// and must be fast (the system disables taps that take ~1 s).
    pub fn install(handler: Handler) -> Result<Hotkey, String> {
        Self::install_with(handler, Bindings::default())
    }

    /// Like [`Hotkey::install`], with the given shortcuts.
    pub fn install_with(handler: Handler, bindings: Bindings) -> Result<Hotkey, String> {
        Self::install_at(handler, bindings, &[CGEventTapLocation::HIDEventTap, CGEventTapLocation::SessionEventTap])
    }

    /// Like [`Hotkey::install_with`], trying the given tap locations in order.
    pub fn install_at(handler: Handler, bindings: Bindings, locations: &[CGEventTapLocation]) -> Result<Hotkey, String> {
        let mask: CGEventMask = (1 << CGEventType::KeyDown.0)
            | (1 << CGEventType::KeyUp.0)
            | (1 << CGEventType::FlagsChanged.0);
        let shared: &'static Shared = Box::leak(Box::new(Shared {
            state: TapState::with_bindings(bindings),
            tap: AtomicPtr::new(ptr::null_mut()),
            handler,
            disabled_count: AtomicU32::new(0),
            paused: AtomicBool::new(false),
            emitted: AtomicU64::new(0),
            run_loop: Mutex::new(None),
            thread: Mutex::new(Some(std::thread::current().id())),
            location: Mutex::new(None),
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
            // No tap references `shared`, so it can be reclaimed (avoids a leak per retry).
            // SAFETY: `shared` came from Box::leak above and nothing else holds it.
            drop(unsafe { Box::from_raw(shared as *const Shared as *mut Shared) });
            return Err(ACCESSIBILITY_HINT.to_string());
        };

        let source = CFMachPort::new_run_loop_source(None, Some(&tap), 0)
            .ok_or_else(|| "CFMachPortCreateRunLoopSource failed".to_string())?;
        let run_loop = CFRunLoop::current().ok_or_else(|| "no current run loop".to_string())?;
        // SAFETY: reading an immutable CF constant.
        let common = unsafe { kCFRunLoopCommonModes };
        run_loop.add_source(Some(&source), common);
        shared.tap.store(NonNull::from(&*tap).as_ptr(), Ordering::SeqCst);
        *shared.run_loop.lock().unwrap_or_else(|e| e.into_inner()) = Some(SendRl(run_loop.clone()));
        *shared.location.lock().unwrap_or_else(|e| e.into_inner()) = Some(location);
        CGEvent::tap_enable(&tap, true);

        let hk = Hotkey { shared, tap, source, run_loop, location };
        if !CGEvent::tap_is_enabled(&hk.tap) {
            return Err(ACCESSIBILITY_HINT.to_string());
        }
        Ok(hk)
    }

    /// Open or close the current session (decides whether keys are swallowed/translated).
    pub fn set_open(&self, open: bool) {
        self.shared.state.set_open(open);
    }

    pub fn is_open(&self) -> bool {
        self.shared.state.is_open()
    }

    /// See [`HotkeyHandle::reenable`]. Safe to call from a periodic watchdog timer.
    pub fn reenable(&self) {
        self.shared.reenable();
    }

    /// Pause (`false`) or resume (`true`) the tap. While paused, neither the disabled-notification
    /// handling in the callback nor `reenable()` turns it back on.
    pub fn set_enabled(&self, enabled: bool) {
        self.shared.paused.store(!enabled, Ordering::SeqCst);
        CGEvent::tap_enable(&self.tap, enabled);
    }

    /// Whether the tap is currently enabled.
    pub fn is_enabled(&self) -> bool {
        CGEvent::tap_is_enabled(&self.tap)
    }

    /// How many kCGEventTapDisabledByTimeout / ByUserInput notifications the callback received
    /// (`set_enabled(false)` itself produces one ByUserInput notification).
    pub fn disabled_count(&self) -> u32 {
        self.shared.disabled_count.load(Ordering::SeqCst)
    }

    /// Where the tap was installed (`HIDEventTap` or `SessionEventTap`).
    pub fn location(&self) -> CGEventTapLocation {
        self.location
    }

    /// A `Copy` handle for use inside the handler closure or on other threads.
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
        self.shared.tap.store(ptr::null_mut(), Ordering::SeqCst);
        *self.shared.run_loop.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.shared.state.set_open(false);
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
        assert!(s.is_open());
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
        assert!(!s.is_open());
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
        assert!(!s.is_open());
    }

    #[test]
    fn reverse_open_and_same_app() {
        let s = TapState::default();
        assert_eq!(decide(&s, KD, keycode::TAB, OPT_SHIFT, false), d(true, Some(Open { reverse: true })));
        s.set_open(false);
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
        assert!(!s.is_open());
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
        s.set_open(false); // integrator closed; keyUp was lost (tap disabled)
        assert_eq!(decide(&s, KD, keycode::TAB, NONE, false), d(false, None));
        assert_eq!(decide(&s, KU, keycode::TAB, NONE, false), d(false, None));
    }

    #[test]
    fn autorepeat_while_closed_does_not_reopen() {
        let s = TapState::default();
        assert_eq!(decide(&s, KD, keycode::TAB, OPT, true), d(true, None));
        assert!(!s.is_open());
    }

    #[test]
    fn sessions_are_numbered_and_only_the_given_one_is_closed() {
        let s = TapState::default();
        assert_eq!(s.session(), 0);
        decide(&s, KD, keycode::TAB, OPT, false);
        assert_eq!((s.session(), s.is_open()), (1, true));
        assert_eq!(decide(&s, FC, keycode::OPTION, NONE, false), d(false, Some(Commit)));
        decide(&s, KU, keycode::TAB, NONE, false);
        decide(&s, KD, keycode::TAB, OPT, false);
        assert_eq!((s.session(), s.is_open()), (2, true));
        // A late close of session 1 (nothing to show) must not close session 2.
        assert!(!s.close_session(1));
        assert!(s.is_open());
        assert!(s.close_session(2));
        assert!(!s.is_open());
        assert!(!s.close_session(2), "already closed");
        assert_eq!(s.session(), 2);
    }

    #[test]
    fn swallowed_keyups_for_every_keycode_range() {
        let s = TapState::default();
        decide(&s, KD, keycode::TAB, OPT, false);
        for code in [0x00u16, 0x3F, 0x40, 0x7B, 0x7F] {
            assert_eq!(decide(&s, KD, code, OPT, false).swallow, true);
        }
        s.set_open(false);
        for code in [0x00u16, 0x3F, 0x40, 0x7B, 0x7F] {
            assert_eq!(decide(&s, KU, code, OPT, false), d(true, None), "keyUp of {code:#x}");
            assert_eq!(decide(&s, KU, code, OPT, false), d(false, None), "only once: {code:#x}");
        }
    }

    const CTRL: Mods = Mods { option: false, shift: false, command: false, control: true };
    const CMD: Mods = Mods { option: false, shift: false, command: true, control: false };

    #[test]
    fn rebound_all_windows_shortcut_with_control_hold() {
        let s = TapState::default();
        s.set_bindings(Bindings { all: Binding { hold: Hold::Control, key: keycode::F18 }, ..Bindings::default() });
        // The old ⌥Tab passes through now.
        assert_eq!(decide(&s, KD, keycode::TAB, OPT, false), d(false, None));
        assert_eq!(decide(&s, KU, keycode::TAB, OPT, false), d(false, None));
        // ⌥⌃F18 is not the binding (two hold modifiers).
        assert_eq!(decide(&s, KD, keycode::F18, Mods { option: true, ..CTRL }, false), d(false, None));
        // ⌃F18 opens; F18 again is Next, Tab too; Option changes do not end a ⌃ session.
        assert_eq!(decide(&s, KD, keycode::F18, CTRL, false), d(true, Some(Open { reverse: false })));
        assert_eq!(s.hold(), Hold::Control);
        assert_eq!(decide(&s, KU, keycode::F18, CTRL, false), d(true, None));
        assert_eq!(decide(&s, KD, keycode::F18, CTRL, false), d(true, Some(Next)));
        assert_eq!(decide(&s, KD, keycode::TAB, Mods { shift: true, ..CTRL }, false), d(true, Some(Prev)));
        assert_eq!(decide(&s, FC, keycode::OPTION, Mods { option: true, ..CTRL }, false), d(false, None));
        assert!(s.is_open());
        // Releasing ⌃ commits.
        assert_eq!(decide(&s, FC, keycode::CONTROL, NONE, false), d(false, Some(Commit)));
        assert!(!s.is_open());
        // ⌥` (same app) still works, with Option as the session's hold modifier.
        assert_eq!(decide(&s, KD, keycode::GRAVE, OPT, false), d(true, Some(OpenSameApp { reverse: false })));
        assert_eq!(s.hold(), Hold::Option);
        assert_eq!(decide(&s, FC, keycode::OPTION, NONE, false), d(false, Some(Commit)));
    }

    #[test]
    fn command_hold_session_and_missed_release() {
        let s = TapState::with_bindings(Bindings { all: Binding { hold: Hold::Command, key: keycode::F19 }, ..Bindings::default() });
        assert_eq!(decide(&s, KD, keycode::F19, CMD, false), d(true, Some(Open { reverse: false })));
        // A key with ⌘ up although no flagsChanged was seen: the release was missed.
        assert_eq!(decide(&s, KD, 0x00, OPT, false), d(true, Some(Commit)));
        assert!(!s.is_open());
    }

    #[test]
    fn swapping_bindings_mid_session_keeps_the_sessions_hold() {
        let s = TapState::default();
        decide(&s, KD, keycode::TAB, OPT, false);
        s.set_bindings(Bindings { all: Binding { hold: Hold::Control, key: keycode::F18 }, ..Bindings::default() });
        // Still an ⌥ session: ⌃ up means nothing, ⌥ up commits.
        assert_eq!(decide(&s, FC, keycode::CONTROL, OPT, false), d(false, None));
        assert_eq!(decide(&s, FC, keycode::OPTION, NONE, false), d(false, Some(Commit)));
    }

    #[test]
    fn recording_passes_everything_while_closed() {
        let s = TapState::default();
        s.set_recording(true);
        assert_eq!(decide(&s, KD, keycode::TAB, OPT, false), d(false, None));
        assert_eq!(decide(&s, KU, keycode::TAB, OPT, false), d(false, None));
        assert_eq!(decide(&s, KD, keycode::GRAVE, OPT, false), d(false, None));
        assert_eq!(decide(&s, KD, keycode::ESCAPE, NONE, false), d(false, None));
        assert!(!s.is_open());
        s.set_recording(false);
        assert_eq!(decide(&s, KD, keycode::TAB, OPT, false), d(true, Some(Open { reverse: false })));
        // A session open when recording starts finishes normally.
        s.set_recording(true);
        assert_eq!(decide(&s, KD, keycode::TAB, OPT, false), d(true, Some(Next)));
        assert_eq!(decide(&s, FC, keycode::OPTION, NONE, false), d(false, Some(Commit)));
        // The keyUp of a keyDown swallowed before recording started is still swallowed.
        assert_eq!(decide(&s, KU, keycode::TAB, NONE, false), d(true, None));
    }

    #[test]
    fn binding_encoding_and_conf_round_trip() {
        let b = Bindings { all: Binding { hold: Hold::Control, key: keycode::F18 }, same_app: Binding { hold: Hold::Command, key: 0x00 } };
        assert_eq!(Bindings::decode(b.encode()), b);
        assert_eq!(Bindings::decode(Bindings::default().encode()), Bindings::default());
        // A word with no valid hold falls back to the defaults.
        assert_eq!(Bindings::decode(0), Bindings::default());
        for x in [Binding::DEFAULT_ALL, Binding::DEFAULT_SAME_APP, b.all, b.same_app] {
            assert_eq!(Binding::parse_conf(&x.to_conf()), Some(x));
        }
        assert_eq!(Binding::DEFAULT_ALL.to_conf(), "option+48");
        assert_eq!(Binding::parse_conf(" ctrl + 0x4f "), Some(Binding { hold: Hold::Control, key: 0x4F }));
        assert_eq!(Binding::parse_conf("shift+48"), None);
        assert_eq!(Binding::parse_conf("option+58"), None, "a modifier key is not a binding key");
        assert_eq!(Binding::parse_conf("option+300"), None);
        assert_eq!(Binding::parse_conf("option"), None);
        assert_eq!(Binding::DEFAULT_ALL.label(), "⌥Tab");
        assert_eq!(Binding::DEFAULT_SAME_APP.label(), "⌥`");
        assert_eq!(Binding { hold: Hold::Control, key: keycode::F18 }.label(), "⌃F18");
        assert_eq!(key_name(0x5C), "Keypad 9");
        assert_eq!(key_name(0x7F), "Key 0x7f");
    }

    #[test]
    fn recorded_combinations() {
        let ok = |code, mods| Binding::from_recorded(code, mods);
        assert_eq!(ok(keycode::TAB, OPT), Ok(Binding::DEFAULT_ALL));
        // Shift is dropped (it reverses).
        assert_eq!(ok(keycode::TAB, OPT_SHIFT), Ok(Binding::DEFAULT_ALL));
        assert_eq!(ok(keycode::F18, CTRL), Ok(Binding { hold: Hold::Control, key: keycode::F18 }));
        assert!(ok(keycode::TAB, NONE).unwrap_err().contains("stop you typing"));
        assert!(ok(keycode::TAB, Mods { shift: true, ..NONE }).unwrap_err().contains("⇧"));
        assert!(ok(keycode::TAB, Mods { option: true, ..CMD }).unwrap_err().contains("only one"));
        assert!(ok(keycode::OPTION, OPT).is_err());
    }

    #[test]
    fn validation_rules() {
        let all = Which::All;
        let def_same = Binding::DEFAULT_SAME_APP;
        assert_eq!(validate(all, Binding::DEFAULT_ALL, def_same), Ok(None));
        assert_eq!(validate(all, Binding { hold: Hold::Control, key: keycode::F18 }, def_same), Ok(None));
        assert!(validate(all, Binding { hold: Hold::Command, key: keycode::TAB }, def_same).unwrap_err().contains("app switcher"));
        assert!(validate(all, Binding { hold: Hold::Option, key: keycode::ESCAPE }, def_same).is_err());
        assert!(validate(all, Binding { hold: Hold::Option, key: keycode::RETURN }, def_same).is_err());
        assert!(validate(all, def_same, def_same).unwrap_err().contains("already"));
        assert!(validate(all, Binding { hold: Hold::Command, key: 0x0C }, def_same).unwrap().is_some(), "⌘Q warns");
        assert!(validate(all, Binding { hold: Hold::Command, key: keycode::GRAVE }, Binding::DEFAULT_ALL).unwrap().is_some());
        assert!(validate(Which::SameApp, Binding { hold: Hold::Option, key: 0x00 }, Binding::DEFAULT_ALL).unwrap().is_none());
        // The arrow keys move the selection inside the switcher, with any hold modifier.
        for key in [keycode::LEFT, keycode::RIGHT, keycode::UP, keycode::DOWN] {
            for hold in [Hold::Option, Hold::Command, Hold::Control] {
                assert!(validate(all, Binding { hold, key }, def_same).unwrap_err().contains("inside the switcher"), "{hold:?} {key:#x}");
            }
        }
        // ⌘ shortcuts say that they stay out of macOS's ⌘Tab switcher.
        assert!(validate(all, Binding { hold: Hold::Command, key: keycode::GRAVE }, def_same).unwrap().unwrap().contains("⌘Tab app switcher"));
    }

    #[test]
    fn command_shortcut_stays_out_of_the_system_app_switcher() {
        let s = TapState::with_bindings(Bindings { same_app: Binding { hold: Hold::Command, key: keycode::GRAVE }, ..Bindings::default() });
        // ⌘ down, Tab: macOS's app switcher (the tap passes it).
        assert_eq!(decide(&s, FC, keycode::COMMAND, CMD, false), d(false, None));
        assert_eq!(decide(&s, KD, keycode::TAB, CMD, false), d(false, None));
        assert_eq!(decide(&s, KU, keycode::TAB, CMD, false), d(false, None));
        assert!(s.in_system_switcher());
        // ⌘` inside it is the system switcher's key: no ZTab session, nothing swallowed.
        assert_eq!(decide(&s, KD, keycode::GRAVE, CMD, false), d(false, None));
        assert_eq!(decide(&s, KU, keycode::GRAVE, CMD, false), d(false, None));
        assert_eq!(decide(&s, KD, keycode::TAB, Mods { shift: true, ..CMD }, false), d(false, None));
        assert!(!s.is_open());
        // ⌘ released: the system switcher is gone and ⌘` opens ZTab again.
        assert_eq!(decide(&s, FC, keycode::COMMAND, NONE, false), d(false, None));
        assert!(!s.in_system_switcher());
        assert_eq!(decide(&s, FC, keycode::COMMAND, CMD, false), d(false, None));
        assert_eq!(decide(&s, KD, keycode::GRAVE, CMD, false), d(true, Some(OpenSameApp { reverse: false })));
        assert_eq!(decide(&s, FC, keycode::COMMAND, NONE, false), d(false, Some(Commit)));
        // ⌥ shortcuts are not affected, and a missed ⌘ release heals on the next event without ⌘.
        assert_eq!(decide(&s, KD, keycode::TAB, CMD, false), d(false, None));
        assert!(s.in_system_switcher());
        assert_eq!(decide(&s, KD, keycode::TAB, OPT, false), d(true, Some(Open { reverse: false })));
        assert!(!s.in_system_switcher());
    }

    #[test]
    fn missed_option_release_commits_on_next_key() {
        let s = TapState::default();
        decide(&s, KD, keycode::TAB, OPT, false);
        assert_eq!(decide(&s, KD, 0x00, NONE, false), d(true, Some(Commit)));
        assert!(!s.is_open());
        assert_eq!(decide(&s, KU, 0x00, NONE, false), d(true, None));
    }
}
