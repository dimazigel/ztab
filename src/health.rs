//! The health check ZTab runs when it is opened by hand (a launch from Finder, Spotlight,
//! Launchpad or `open`, opening it again while it runs, or a second copy asking it; see
//! `reopen.rs`), without AppKit: [`plan`] decides from what was checked what to fix, what to show
//! and whether to restart; `main.rs` gathers the inputs (the slow parts off the main thread) and
//! carries the plan out.
//!
//! Checked: Accessibility and Screen Recording (missing: the Set up window), the event tap
//! (installed, enabled, its thread answering: re-enabled when macOS switched it off, else ZTab
//! restarts itself once), the window model, main-thread stalls, the login agent (pointing at a
//! removed or another copy: pointed at this one, only if it was on and this copy is an installed
//! one; never turned on), the instance lock and the socket (their files re-created when the
//! temporary directory was cleaned up), secure input.

use std::path::Path;

use crate::settings::LoginState;

/// The event tap as checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tap {
    /// Accessibility is not granted: there is no tap to check.
    NoAccess,
    /// Being installed right now.
    Starting,
    /// Accessibility is granted, but the tap could not be installed.
    Missing,
    /// Installed: whether macOS had it enabled, whether it is enabled after a re-enable, and
    /// whether its thread answered.
    Installed { was_enabled: bool, enabled: bool, responsive: bool },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Model {
    NotStarted,
    /// Started `secs` ago, some apps not read yet.
    Starting { secs: f64 },
    Ready { windows: usize },
}

/// The login agent, checked (and repaired) on the settings thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Login {
    Off,
    On,
    /// It started a removed (`existed: false`) or another copy (`from`); it starts this one now.
    Repointed { from: String, existed: bool },
    RepointFailed { from: String, error: String },
    /// It starts another program, left as it is: this copy is not an installed one (`why`).
    LeftElsewhere { program: String, exists: bool, why: String },
}

/// What to do with the login agent, from its state and whether this copy is an installed one
/// (`settings::install_placement`): only an agent that exists (start at login was on) and starts
/// another program is pointed at this copy; an agent that is off stays off.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoginRepair {
    Keep,
    Repoint,
    Leave(String),
}

pub fn login_repair(state: &LoginState, placement: Result<(), &str>) -> LoginRepair {
    match (state, placement) {
        (LoginState::Off | LoginState::On, _) => LoginRepair::Keep,
        (LoginState::Elsewhere { .. }, Ok(())) => LoginRepair::Repoint,
        (LoginState::Elsewhere { .. }, Err(why)) => LoginRepair::Leave(why.to_string()),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Inputs {
    pub ax: bool,
    /// Screen Recording now (this process or a fresh child process says so).
    pub screen_now: bool,
    /// Screen Recording as this process had it at launch (a later grant needs a restart).
    pub screen_at_launch: bool,
    /// The user chose Skip Thumbnails.
    pub skip_screen: bool,
    /// This process comes from a setup restart made after Screen Recording was seen granted.
    pub restart_saw_screen: bool,
    pub tap: Tap,
    /// This process comes from a health restart (it does not restart for the health check again).
    pub restarted_by_health: bool,
    /// The path ZTab is opened by holds another binary now (an upgrade in place, or a Homebrew
    /// `opt` link to a newer keg; see `reopen::replaced_on_disk`).
    pub replaced: Option<String>,
    /// Secure input is on (the frontmost app's name, when known).
    pub secure_input: Option<Option<String>>,
    pub model: Model,
    /// The longest main-thread stall since launch (ms).
    pub longest_stall_ms: f64,
    pub login: Login,
    pub lock: crate::reopen::LockCheck,
    pub socket: crate::reopen::SocketCheck,
    /// What this launch did already (a takeover, a health restart), for the status.
    pub launch_notes: Vec<String>,
}

/// Which window shows the result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ui {
    Settings,
    /// A permission is missing: the Set up window (the reason, for the log).
    Setup(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Restart {
    /// The event tap does not work: restart ZTab (once).
    Tap(String),
    /// Another binary is installed at ZTab's path: restart into it (through that path).
    Replaced(String),
    /// Screen Recording was granted after launch: restart (the setup restart; the Set up window
    /// opens again with the result).
    ScreenRecording,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub ui: Ui,
    pub restart: Option<Restart>,
    /// Start the window model now (Accessibility is granted, it is not running).
    pub start_model: bool,
    pub fixed: Vec<String>,
    pub problems: Vec<String>,
    pub notes: Vec<String>,
}

/// A main-thread stall this long since launch is mentioned in the status.
pub const STALL_NOTE_MS: f64 = 2000.0;
/// A model that is still not ready after this long is mentioned.
pub const MODEL_NOTE_S: f64 = 30.0;

pub fn plan(i: &Inputs) -> Plan {
    let mut p = Plan { ui: Ui::Settings, restart: None, start_model: false, fixed: i.launch_notes.clone(), problems: vec![], notes: vec![] };
    if let Some(path) = &i.replaced {
        if !i.restarted_by_health {
            // The binary opened by hand is the new one: run it (it checks everything else).
            p.restart = Some(Restart::Replaced(path.clone()));
            return p;
        }
        p.problems.push(format!("another ZTab binary is installed at {path}, and restarting did not run it (quit ZTab and open it again)"));
    }
    if !i.ax {
        p.ui = Ui::Setup("Accessibility is not granted");
        p.problems.push("Accessibility is not granted: ⌥Tab is off".into());
    } else if !i.screen_now && !i.skip_screen {
        p.ui = Ui::Setup("Screen Recording is not granted");
    } else if !i.screen_now {
        p.notes.push("Thumbnails are off (no Screen Recording); Set Up ZTab… in the menu turns them on.".into());
    } else if !i.screen_at_launch {
        if i.restart_saw_screen {
            p.problems.push("Screen Recording is on, but macOS has not given it to this ZTab yet (quit ZTab and open it again)".into());
        } else {
            p.restart = Some(Restart::ScreenRecording);
        }
    }
    if i.ax {
        let broken = match &i.tap {
            Tap::NoAccess | Tap::Starting => None,
            Tap::Missing => Some("⌥Tab's keyboard hook could not be installed"),
            Tap::Installed { was_enabled, enabled, responsive } => {
                if !responsive {
                    Some("⌥Tab's keyboard hook does not respond")
                } else if !enabled {
                    Some("⌥Tab's keyboard hook stays switched off")
                } else {
                    if !was_enabled {
                        p.fixed.push("⌥Tab's keyboard hook had been switched off by macOS; it is on again".into());
                    }
                    None
                }
            }
        };
        if let Some(b) = broken {
            if i.restarted_by_health {
                p.problems.push(format!("{b}, even after a restart (quit ZTab, then turn it off and on in System Settings › Privacy & Security › Accessibility)"));
            } else if p.restart.is_none() {
                p.restart = Some(Restart::Tap(b.to_string()));
            }
        }
        match i.model {
            Model::NotStarted => {
                p.start_model = true;
                p.fixed.push("the window list was not running; it is started".into());
            }
            Model::Starting { secs } if secs >= MODEL_NOTE_S => {
                p.notes.push("Some apps have not answered yet: their windows may be missing.".into())
            }
            _ => {}
        }
    }
    if let Some(app) = &i.secure_input {
        let who = app.as_deref().map(|a| format!(" ({a} is in front)")).unwrap_or_default();
        p.notes.push(format!("Secure input is on{who}: macOS does not deliver ⌥Tab until it is off."));
    }
    if i.longest_stall_ms >= STALL_NOTE_MS {
        p.notes.push(format!(
            "ZTab's main thread was blocked for {:.1} s at some point (see ~/Library/Logs/ztab.log).",
            i.longest_stall_ms / 1000.0
        ));
    }
    match &i.login {
        Login::Off | Login::On => {}
        Login::Repointed { from, existed: false } => {
            p.fixed.push(format!("start at login pointed at a removed copy ({from}); it starts this one now"))
        }
        Login::Repointed { from, existed: true } => {
            p.fixed.push(format!("start at login pointed at another copy ({from}); it starts this one now"))
        }
        Login::RepointFailed { from, error } => {
            p.problems.push(format!("start at login starts {from}, and pointing it at this copy failed ({error})"))
        }
        Login::LeftElsewhere { program, exists, why } => p.notes.push(format!(
            "Start at login starts {}{program}; left as it is because this copy is {why}.",
            if *exists { "" } else { "a removed copy, " }
        )),
    }
    use crate::reopen::{LockCheck, SocketCheck};
    match &i.lock {
        LockCheck::Held | LockCheck::Unknown(_) => {}
        LockCheck::Retaken => p.fixed.push("the single-instance lock's file had been removed; it is back".into()),
        LockCheck::OtherHolder(pids) => p.problems.push(format!(
            "another ZTab holds the single-instance lock ({}) and does not answer (open ZTab again to replace it)",
            pids.iter().map(|p| format!("pid {p}")).collect::<Vec<_>>().join(", ")
        )),
    }
    match &i.socket {
        SocketCheck::Listening | SocketCheck::LeftToHolder | SocketCheck::Failed(_) => {}
        SocketCheck::Rebound => p.fixed.push("the file ZTab is reached through had been removed; it is back".into()),
    }
    p
}

/// The status line for the Settings window: (text, is a warning). `shortcut`: e.g. "hold ⌥ and
/// press Tab".
pub fn message(p: &Plan, shortcut: &str) -> (String, bool) {
    let mut s = if let Some(Restart::Tap(why)) = &p.restart {
        format!("{why}: restarting ZTab…")
    } else if let Some(Restart::Replaced(path)) = &p.restart {
        format!("Another ZTab is installed at {path}: restarting into it…")
    } else if let Some(Restart::ScreenRecording) = &p.restart {
        "Screen Recording was turned on: restarting ZTab so that it can use it…".to_string()
    } else if p.problems.is_empty() {
        format!("ZTab is running: {shortcut} to switch windows.")
    } else {
        format!("ZTab is running, but {}.", p.problems.join("; "))
    };
    if !p.fixed.is_empty() {
        s += &format!(" Fixed: {}.", p.fixed.join("; "));
    }
    for n in &p.notes {
        s += " ";
        s += n;
    }
    (s, !p.problems.is_empty())
}

/// What the status shows when opened, for the log: one line per finding.
pub fn describe(i: &Inputs) -> String {
    format!(
        "accessibility={} screen_recording={} (at launch {}, skip {}) tap={:?} model={:?} secure_input={} longest_stall={:.0} ms login={:?} lock={:?} socket={:?} replaced={:?}",
        i.ax,
        i.screen_now,
        i.screen_at_launch,
        i.skip_screen,
        i.tap,
        i.model,
        i.secure_input.is_some(),
        i.longest_stall_ms,
        i.login,
        i.lock,
        i.socket,
        i.replaced
    )
}

// ---------------------------------------------------------------------------------------------
// Health restart marker: the restarted ZTab shows why it restarted, and does not restart again
// for the tap.
// ---------------------------------------------------------------------------------------------

/// A health restart older than this does not count.
pub const MARKER_MAX_AGE_S: f64 = 120.0;

pub fn marker_path() -> std::path::PathBuf {
    crate::settings::user_temp_dir().join(format!("{}.health-restart", crate::settings::AGENT_LABEL))
}

/// Before a health restart (disk work: never on the main thread).
pub fn write_marker(path: &Path, now_unix: f64, reason: &str) -> std::io::Result<()> {
    std::fs::write(path, format!("{now_unix:.3} {}\n", crate::reopen::enc(reason)))
}

/// The reason of a fresh marker.
pub fn parse_marker(text: &str, now_unix: f64) -> Option<String> {
    let mut it = text.split_whitespace();
    let t: f64 = it.next()?.parse().ok()?;
    if now_unix < t - 5.0 || now_unix - t > MARKER_MAX_AGE_S {
        return None;
    }
    Some(it.next().map(crate::reopen::dec).unwrap_or_default())
}

/// At launch: the fresh marker's reason, if there is one; it is removed either way.
pub fn take_marker(path: &Path, now_unix: f64) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let _ = std::fs::remove_file(path);
    parse_marker(&text, now_unix)
}

/// Whether a check's result is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feedback {
    /// Always (opened by hand, or after a health restart).
    Show,
    /// Only when something is wrong or was fixed ([`worth_showing`]): right after login, where
    /// an argument-free launch or a reopen event may be a login item rather than the user.
    IfProblems,
    /// No check at launch (launchd, a script; or the Set up window opens, which is the feedback).
    None,
}

/// What a launch shows: opened by hand, or restarted by a health check: the status; right after
/// login without arguments: only problems; never when the Set up window opens at launch (that is
/// the feedback then).
pub fn launch_feedback(kind: crate::reopen::LaunchKind, setup_opens: bool, health_restart: bool) -> Feedback {
    use crate::reopen::LaunchKind;
    if setup_opens {
        Feedback::None
    } else if health_restart || kind == LaunchKind::ByHand {
        Feedback::Show
    } else if kind == LaunchKind::AtLogin {
        Feedback::IfProblems
    } else {
        Feedback::None
    }
}

/// The launch event says a login item (or a service item) started ZTab (`keyAELaunchedAsLogInItem`
/// in the `oapp` event, read in `applicationDidFinishLaunching`): like the login agent, no check
/// at launch, unless it comes from a health restart.
pub fn feedback_for_launch_event(f: Feedback, login_item: bool, health_restart: bool) -> Feedback {
    if login_item && !health_restart { Feedback::None } else { f }
}

/// A quiet check ([`Feedback::IfProblems`]) shows its result only when this is true: a problem,
/// a fix, a restart, or a missing permission (notes alone are not shown).
pub fn worth_showing(p: &Plan) -> bool {
    !p.problems.is_empty() || !p.fixed.is_empty() || p.restart.is_some() || p.ui != Ui::Settings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reopen::{LaunchKind, LockCheck, SocketCheck};

    fn healthy() -> Inputs {
        Inputs {
            ax: true,
            screen_now: true,
            screen_at_launch: true,
            skip_screen: false,
            restart_saw_screen: false,
            tap: Tap::Installed { was_enabled: true, enabled: true, responsive: true },
            restarted_by_health: false,
            replaced: None,
            secure_input: None,
            model: Model::Ready { windows: 12 },
            longest_stall_ms: 30.0,
            login: Login::On,
            lock: LockCheck::Held,
            socket: SocketCheck::Listening,
            launch_notes: vec![],
        }
    }

    #[test]
    fn healthy_shows_the_shortcut() {
        let p = plan(&healthy());
        assert_eq!(p, Plan { ui: Ui::Settings, restart: None, start_model: false, fixed: vec![], problems: vec![], notes: vec![] });
        assert_eq!(message(&p, "hold ⌥ and press Tab"), ("ZTab is running: hold ⌥ and press Tab to switch windows.".to_string(), false));
    }

    #[test]
    fn missing_permissions_open_the_setup_window() {
        let p = plan(&Inputs { ax: false, tap: Tap::NoAccess, model: Model::NotStarted, ..healthy() });
        assert_eq!(p.ui, Ui::Setup("Accessibility is not granted"));
        assert!(!p.start_model && p.restart.is_none(), "{p:?}");
        let p = plan(&Inputs { screen_now: false, screen_at_launch: false, ..healthy() });
        assert_eq!(p.ui, Ui::Setup("Screen Recording is not granted"));
        // Skip Thumbnails: the Settings window, with a note.
        let p = plan(&Inputs { screen_now: false, screen_at_launch: false, skip_screen: true, ..healthy() });
        assert_eq!(p.ui, Ui::Settings);
        assert_eq!(p.notes.len(), 1);
        assert!(!message(&p, "x").1);
    }

    #[test]
    fn screen_recording_granted_since_launch_restarts_once() {
        let p = plan(&Inputs { screen_at_launch: false, ..healthy() });
        assert_eq!(p.restart, Some(Restart::ScreenRecording));
        let p = plan(&Inputs { screen_at_launch: false, restart_saw_screen: true, ..healthy() });
        assert_eq!(p.restart, None);
        assert_eq!(p.problems.len(), 1);
    }

    #[test]
    fn tap_switched_off_is_fixed_in_place() {
        let p = plan(&Inputs { tap: Tap::Installed { was_enabled: false, enabled: true, responsive: true }, ..healthy() });
        assert_eq!(p.restart, None);
        assert_eq!(p.fixed.len(), 1);
        let (m, warn) = message(&p, "hold ⌥ and press Tab");
        assert!(m.starts_with("ZTab is running: hold ⌥ and press Tab") && m.contains("Fixed: ⌥Tab's keyboard hook"), "{m}");
        assert!(!warn);
    }

    #[test]
    fn a_broken_tap_restarts_once_then_reports() {
        for tap in [
            Tap::Missing,
            Tap::Installed { was_enabled: false, enabled: false, responsive: true },
            Tap::Installed { was_enabled: true, enabled: true, responsive: false },
        ] {
            let p = plan(&Inputs { tap: tap.clone(), ..healthy() });
            assert!(matches!(p.restart, Some(Restart::Tap(_))), "{tap:?}: {p:?}");
            assert!(message(&p, "x").0.contains("restarting ZTab"));
            let p = plan(&Inputs { tap: tap.clone(), restarted_by_health: true, ..healthy() });
            assert_eq!(p.restart, None, "{tap:?}: no restart loop");
            assert_eq!(p.problems.len(), 1);
            assert!(message(&p, "x").1, "a warning");
        }
        // Being installed is not broken; without Accessibility nothing is checked.
        assert_eq!(plan(&Inputs { tap: Tap::Starting, ..healthy() }).restart, None);
        assert_eq!(plan(&Inputs { ax: false, tap: Tap::Missing, ..healthy() }).restart, None);
        // The Screen Recording restart covers the tap too (one restart).
        let p = plan(&Inputs { tap: Tap::Missing, screen_at_launch: false, ..healthy() });
        assert_eq!(p.restart, Some(Restart::ScreenRecording));
    }

    #[test]
    fn a_binary_replaced_at_its_path_restarts_into_it_once() {
        let path = "/opt/homebrew/opt/ztab/ZTab.app/Contents/MacOS/ztab".to_string();
        // Whatever else is wrong: the new binary checks it.
        let p = plan(&Inputs { replaced: Some(path.clone()), ax: false, tap: Tap::Missing, ..healthy() });
        assert_eq!(p.restart, Some(Restart::Replaced(path.clone())));
        assert_eq!(p.ui, Ui::Settings);
        assert!(message(&p, "x").0.contains("restarting into it"));
        // After a health restart: no loop, a warning.
        let p = plan(&Inputs { replaced: Some(path.clone()), restarted_by_health: true, ..healthy() });
        assert_eq!(p.restart, None);
        assert!(message(&p, "x").1);
    }

    #[test]
    fn model_not_running_is_started() {
        let p = plan(&Inputs { model: Model::NotStarted, ..healthy() });
        assert!(p.start_model);
        assert_eq!(p.fixed.len(), 1);
        assert!(plan(&Inputs { model: Model::Starting { secs: 2.0 }, ..healthy() }).notes.is_empty());
        assert_eq!(plan(&Inputs { model: Model::Starting { secs: 45.0 }, ..healthy() }).notes.len(), 1);
    }

    #[test]
    fn login_item_is_repointed_only_when_it_was_on() {
        let elsewhere = LoginState::Elsewhere { program: "/Applications/Old.app/Contents/MacOS/ztab".into(), exists: false };
        assert_eq!(login_repair(&elsewhere, Ok(())), LoginRepair::Repoint);
        assert_eq!(login_repair(&elsewhere, Err("a development build")), LoginRepair::Leave("a development build".into()));
        assert_eq!(login_repair(&LoginState::Off, Ok(())), LoginRepair::Keep, "never turned on");
        assert_eq!(login_repair(&LoginState::On, Ok(())), LoginRepair::Keep);
        let p = plan(&Inputs { login: Login::Repointed { from: "/Gone/ZTab.app/Contents/MacOS/ztab".into(), existed: false }, ..healthy() });
        let (m, warn) = message(&p, "hold ⌥ and press Tab");
        assert!(m.contains("Fixed: start at login pointed at a removed copy (/Gone/ZTab.app/Contents/MacOS/ztab)"), "{m}");
        assert!(!warn);
        let p = plan(&Inputs { login: Login::Off, ..healthy() });
        assert!(p.fixed.is_empty() && p.notes.is_empty());
        let p = plan(&Inputs { login: Login::LeftElsewhere { program: "/x".into(), exists: true, why: "a development build".into() }, ..healthy() });
        assert_eq!(p.notes.len(), 1);
        let p = plan(&Inputs { login: Login::RepointFailed { from: "/x".into(), error: "denied".into() }, ..healthy() });
        assert!(message(&p, "x").1);
    }

    #[test]
    fn lock_socket_and_notes() {
        let p = plan(&Inputs { lock: LockCheck::Retaken, socket: SocketCheck::Rebound, ..healthy() });
        assert_eq!(p.fixed.len(), 2);
        // Reached only when the other one did not answer (one that answers is handed over to).
        let p = plan(&Inputs { lock: LockCheck::OtherHolder(vec![123]), socket: SocketCheck::LeftToHolder, ..healthy() });
        let (m, warn) = message(&p, "x");
        assert!(warn && m.contains("another ZTab holds the single-instance lock (pid 123) and does not answer"), "{m}");
        assert!(p.fixed.is_empty(), "the socket left to the other one is not a fix: {p:?}");
        let p = plan(&Inputs { secure_input: Some(Some("Terminal".into())), longest_stall_ms: 2500.0, ..healthy() });
        assert_eq!(p.notes.len(), 2);
        assert!(!message(&p, "x").1, "notes are not warnings");
        let p = plan(&Inputs { launch_notes: vec!["replaced a ZTab that did not respond (pid 9)".into()], ..healthy() });
        assert!(message(&p, "x").0.contains("Fixed: replaced a ZTab that did not respond (pid 9)."));
    }

    #[test]
    fn launch_feedback_rule() {
        assert_eq!(launch_feedback(LaunchKind::ByHand, false, false), Feedback::Show);
        assert_eq!(launch_feedback(LaunchKind::ByHand, true, false), Feedback::None, "the Set up window is the feedback");
        assert_eq!(launch_feedback(LaunchKind::Launchd, false, false), Feedback::None);
        assert_eq!(launch_feedback(LaunchKind::Scripted, false, false), Feedback::None);
        assert_eq!(launch_feedback(LaunchKind::Launchd, false, true), Feedback::Show, "after a health restart");
        assert_eq!(launch_feedback(LaunchKind::Scripted, false, true), Feedback::Show);
        // Right after login: checked, shown only if something is wrong.
        assert_eq!(launch_feedback(LaunchKind::AtLogin, false, false), Feedback::IfProblems);
        assert_eq!(launch_feedback(LaunchKind::AtLogin, true, false), Feedback::None);
        assert_eq!(launch_feedback(LaunchKind::AtLogin, false, true), Feedback::Show);
        // The launch event's login-item mark: like launchd.
        assert_eq!(feedback_for_launch_event(Feedback::Show, true, false), Feedback::None);
        assert_eq!(feedback_for_launch_event(Feedback::IfProblems, true, false), Feedback::None);
        assert_eq!(feedback_for_launch_event(Feedback::Show, true, true), Feedback::Show, "a health restart still shows");
        assert_eq!(feedback_for_launch_event(Feedback::Show, false, false), Feedback::Show);
    }

    #[test]
    fn quiet_checks_show_only_problems_and_fixes() {
        let fine = plan(&healthy());
        assert!(!worth_showing(&fine), "{fine:?}");
        // Notes alone (thumbnails off, secure input) are not worth a window right after login.
        let notes = plan(&Inputs { secure_input: Some(Some("Terminal".into())), longest_stall_ms: 5000.0, ..healthy() });
        assert!(!notes.notes.is_empty() && !worth_showing(&notes), "{notes:?}");
        let fixed = plan(&Inputs { login: Login::Repointed { from: "/Applications/Gone.app/Contents/MacOS/ztab".into(), existed: false }, ..healthy() });
        assert!(worth_showing(&fixed), "{fixed:?}");
        let broken = plan(&Inputs { tap: Tap::Installed { was_enabled: true, enabled: true, responsive: false }, ..healthy() });
        assert!(worth_showing(&broken), "a restart: {broken:?}");
        let no_ax = plan(&Inputs { ax: false, tap: Tap::NoAccess, ..healthy() });
        assert!(worth_showing(&no_ax), "the Set up window: {no_ax:?}");
        let other = plan(&Inputs { lock: LockCheck::OtherHolder(vec![7]), socket: SocketCheck::LeftToHolder, ..healthy() });
        assert!(worth_showing(&other) && other.problems.iter().any(|p| p.contains("pid 7")), "{other:?}");
        let noted = plan(&Inputs { launch_notes: vec!["replaced a ZTab that did not respond (pid 9)".into()], ..healthy() });
        assert!(worth_showing(&noted), "a takeover is shown: {noted:?}");
    }

    #[test]
    fn marker_round_trip() {
        let d = std::env::temp_dir().join(format!("zt-hm-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        let p = d.join("m");
        write_marker(&p, 1000.0, "⌥Tab's keyboard hook does not respond").unwrap();
        assert_eq!(take_marker(&p, 1001.0).as_deref(), Some("⌥Tab's keyboard hook does not respond"));
        assert!(!p.exists());
        write_marker(&p, 1000.0, "x").unwrap();
        assert_eq!(take_marker(&p, 1000.0 + MARKER_MAX_AGE_S + 1.0), None, "stale");
        let _ = std::fs::remove_dir_all(&d);
    }
}
