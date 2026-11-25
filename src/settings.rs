//! ZTab's settings: the values, their file, where it lives (with an isolated location for
//! tests), the start-at-login LaunchAgent, a writer thread and the single-instance lock.
//!
//! * Values ([`Settings`]): the show delay and the two shortcuts. Stored as `key = value` lines
//!   in `~/Library/Application Support/ZTab/settings.conf`; a missing file or line means the
//!   default, a bad line is ignored (and reported).
//! * Isolation ([`Paths::from_args`]): `--settings-dir <dir>` (or the `ZTAB_SETTINGS_DIR`
//!   environment variable) moves the settings file AND the login agent into `<dir>`
//!   (`<dir>/settings.conf`, `<dir>/LaunchAgents/dev.ztab.app.plist`), and `launchctl` is never
//!   run: tests can change everything without touching the user's ZTab.
//! * Start at login: ZTab alone owns `~/Library/LaunchAgents/dev.ztab.app.plist`. Its presence is
//!   the setting (read from the file, never cached); [`Paths::login_state`] also checks that it
//!   starts the running binary. The agent runs the binary inside the bundle
//!   directly ([`agent_plist`]). The app only writes or deletes the file (launchd loads it at the
//!   next login; unloading it now would kill a ZTab that launchd started). The command line
//!   (`ztab --set-login on|off`, used by `scripts/install.sh --login`) also boots the job in or
//!   out of launchd.
//! * First launch ([`first_launch`]): ZTab turns its login item on by itself only on the very
//!   first launch of an installed copy for this user, and records in the settings
//!   (`login_item_offered`) that the question is settled, so it never does it again (see the rule
//!   there). When the agent cannot be written it records `login_item_retry` instead and the next
//!   launch tries again. These records edit their line of the settings file ([`Paths::record`]);
//!   the rest of the file stays as it was.
//! * Disk writes happen on the `ztab-settings` thread ([`Writer`]); saves are coalesced.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use crate::hotkey::{Binding, Bindings, Which, validate, validate_alone};

pub const DEFAULT_SHOW_DELAY_MS: u64 = 120;
pub const MAX_SHOW_DELAY_MS: u64 = 300;
/// launchd label and bundle id.
pub const AGENT_LABEL: &str = "dev.ztab.app";
pub const ENV_SETTINGS_DIR: &str = "ZTAB_SETTINGS_DIR";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// How long ⌥Tab must be held before the switcher appears (0: at once).
    pub show_delay_ms: u64,
    pub hotkeys: Bindings,
    /// The user chose "Skip Thumbnails" in the Set up ZTab window: a missing Screen Recording
    /// grant alone no longer opens it at launch. "Restore defaults" keeps it.
    pub skip_screen_recording: bool,
    /// ZTab has asked macOS for Screen Recording (`CGRequestScreenCaptureAccess`), whose dialog
    /// shows only once per app: a later "Grant…" opens System Settings directly. "Restore
    /// defaults" keeps it.
    pub screen_recording_requested: bool,
    /// The start-at-login question is settled for this user: ZTab turned the login item on at its
    /// first launch, found that it had run before, or the user (checkbox) or an installer
    /// (`--set-login`) chose. From then on ZTab never turns the login item on by itself (see
    /// [`first_launch`]). "Restore defaults" keeps it.
    pub login_item_offered: bool,
    /// The first launch tried to turn the login item on and could not write the agent: the
    /// question stays open and the next launch tries again (the settings file and the log that
    /// this launch left do not count as an earlier run then; see [`first_launch`]). Written only
    /// while it is true; the checkbox and `--set-login` clear it. "Restore defaults" keeps it.
    pub login_item_retry: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            show_delay_ms: DEFAULT_SHOW_DELAY_MS,
            hotkeys: Bindings::default(),
            skip_screen_recording: false,
            screen_recording_requested: false,
            login_item_offered: false,
            login_item_retry: false,
        }
    }
}

impl Settings {
    pub fn describe(&self) -> String {
        format!(
            "show_delay_ms={} hotkey_all={} hotkey_same_app={} skip_screen_recording={} screen_recording_requested={} login_item_offered={}{}",
            self.show_delay_ms,
            self.hotkeys.all.label(),
            self.hotkeys.same_app.label(),
            self.skip_screen_recording,
            self.screen_recording_requested,
            self.login_item_offered,
            if self.login_item_retry { " login_item_retry=true" } else { "" }
        )
    }

    /// "Restore defaults": the default delay and shortcuts; the Set up window's choice, what ZTab
    /// asked macOS for and the settled login-item question stay.
    pub fn restored(&self) -> Settings {
        Settings {
            skip_screen_recording: self.skip_screen_recording,
            screen_recording_requested: self.screen_recording_requested,
            login_item_offered: self.login_item_offered,
            login_item_retry: self.login_item_retry,
            ..Settings::default()
        }
    }
}

/// Parse a settings file. Unknown keys, malformed or invalid values are skipped (the default
/// stays) and reported in the returned notes.
pub fn parse(text: &str) -> (Settings, Vec<String>) {
    let mut s = Settings::default();
    let mut notes = Vec::new();
    let mut all: Option<Binding> = None;
    let mut same: Option<Binding> = None;
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            notes.push(format!("line {}: no '=': {line:?}", n + 1));
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        match k {
            "show_delay_ms" => match v.parse::<u64>() {
                Ok(ms) => {
                    if ms > MAX_SHOW_DELAY_MS {
                        notes.push(format!("show_delay_ms {ms} is above {MAX_SHOW_DELAY_MS}; using {MAX_SHOW_DELAY_MS}"));
                    }
                    s.show_delay_ms = ms.min(MAX_SHOW_DELAY_MS);
                }
                Err(_) => notes.push(format!("show_delay_ms: not a number: {v:?}")),
            },
            "skip_screen_recording" | "screen_recording_requested" | "login_item_offered" | "login_item_retry" => {
                let b = match v {
                    "true" | "1" => true,
                    "false" | "0" => false,
                    _ => {
                        notes.push(format!("{k}: not true or false: {v:?}"));
                        continue;
                    }
                };
                match k {
                    "skip_screen_recording" => s.skip_screen_recording = b,
                    "screen_recording_requested" => s.screen_recording_requested = b,
                    "login_item_offered" => s.login_item_offered = b,
                    _ => s.login_item_retry = b,
                }
            }
            "hotkey_all" | "hotkey_same_app" => match Binding::parse_conf(v) {
                Some(b) if k == "hotkey_all" => all = Some(b),
                Some(b) => same = Some(b),
                None => notes.push(format!("{k}: not a shortcut: {v:?}")),
            },
            _ => notes.push(format!("unknown setting {k:?}")),
        }
    }
    // Each shortcut must be valid on its own (else it is the default), and the same-app one must
    // differ from the all-windows one (else it is its default, or ⌥Tab when that collides).
    let d = Bindings::default();
    let mut hk = Bindings { all: all.unwrap_or(d.all), same_app: same.unwrap_or(d.same_app) };
    if let Err(e) = validate_alone(hk.all) {
        notes.push(format!("hotkey_all {}: {e} Using {}.", hk.all.label(), d.all.label()));
        hk.all = d.all;
    }
    if let Err(e) = validate_alone(hk.same_app) {
        notes.push(format!("hotkey_same_app {}: {e} Using {}.", hk.same_app.label(), d.same_app.label()));
        hk.same_app = d.same_app;
    }
    if let Err(e) = validate(Which::SameApp, hk.same_app, hk.all) {
        let fallback = if d.same_app != hk.all { d.same_app } else { d.all };
        notes.push(format!("hotkey_same_app {}: {e} Using {}.", hk.same_app.label(), fallback.label()));
        hk.same_app = fallback;
    }
    s.hotkeys = hk;
    (s, notes)
}

pub fn serialize(s: &Settings) -> String {
    format!(
        "# ZTab settings (written by ZTab; edit while ZTab is not running)\n\
         # Shortcuts: <hold modifier>+<virtual keycode>, hold = option | command | control.\n\
         show_delay_ms = {}\n\
         hotkey_all = {}\n\
         hotkey_same_app = {}\n\
         skip_screen_recording = {}\n\
         screen_recording_requested = {}\n\
         login_item_offered = {}\n{}",
        s.show_delay_ms,
        s.hotkeys.all.to_conf(),
        s.hotkeys.same_app.to_conf(),
        s.skip_screen_recording,
        s.screen_recording_requested,
        s.login_item_offered,
        if s.login_item_retry { "login_item_retry = true\n" } else { "" }
    )
}

/// Where the settings file and the login agent live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub settings_file: PathBuf,
    pub agent_plist: PathBuf,
    /// Test location: never run `launchctl`.
    pub isolated: bool,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

impl Paths {
    /// The user's real locations.
    pub fn user() -> Paths {
        let h = home();
        Paths {
            settings_file: h.join("Library/Application Support/ZTab/settings.conf"),
            agent_plist: h.join("Library/LaunchAgents").join(format!("{AGENT_LABEL}.plist")),
            isolated: false,
        }
    }

    /// Everything inside `dir` (tests).
    pub fn isolated(dir: &Path) -> Paths {
        Paths {
            settings_file: dir.join("settings.conf"),
            agent_plist: dir.join("LaunchAgents").join(format!("{AGENT_LABEL}.plist")),
            isolated: true,
        }
    }

    /// `--settings-dir <dir>`, else `$ZTAB_SETTINGS_DIR`, else the user's locations.
    pub fn from_args(args: &[String]) -> Paths {
        let flag = args.iter().position(|a| a == "--settings-dir").and_then(|i| args.get(i + 1)).map(PathBuf::from);
        let env = std::env::var_os(ENV_SETTINGS_DIR).filter(|v| !v.is_empty()).map(PathBuf::from);
        match flag.or(env) {
            Some(d) => Paths::isolated(&d),
            None => Paths::user(),
        }
    }

    /// Read the settings (missing file: defaults). Returns the notes of `parse` plus whether the
    /// file existed.
    pub fn load(&self) -> (Settings, Vec<String>, bool) {
        match std::fs::read_to_string(&self.settings_file) {
            Ok(text) => {
                let (s, notes) = parse(&text);
                (s, notes, true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Settings::default(), Vec::new(), false),
            Err(e) => (Settings::default(), vec![format!("cannot read {}: {e}", self.settings_file.display())], false),
        }
    }

    /// Write the settings atomically (temporary file + rename). Disk work: never on the main thread.
    pub fn save(&self, s: &Settings) -> std::io::Result<()> {
        write_atomic(&self.settings_file, serialize(s).as_bytes())
    }

    /// The login agent is installed: its plist exists with ZTab's label.
    pub fn login_enabled(&self) -> bool {
        std::fs::read_to_string(&self.agent_plist).is_ok_and(|t| plist_label_is_ours(&t))
    }

    /// The login agent compared with `current` (this process's binary, see [`current_program`]):
    /// off, on (it starts `current`), or installed but starting another binary (moved app, a
    /// deleted dev build): then it would not start this ZTab at the next login. Reads the file
    /// (disk work: never on the main thread).
    pub fn login_state(&self, current: &str) -> LoginState {
        if !self.login_enabled() {
            return LoginState::Off;
        }
        let Some(program) = self.login_program() else {
            return LoginState::Elsewhere { program: String::new(), exists: false };
        };
        let canon = |p: &str| std::fs::canonicalize(p).ok();
        match (canon(&program), canon(current)) {
            (Some(a), Some(b)) if a == b => LoginState::On,
            (a, _) => LoginState::Elsewhere { exists: a.is_some(), program },
        }
    }

    /// The program the login agent starts, when installed.
    pub fn login_program(&self) -> Option<String> {
        let t = std::fs::read_to_string(&self.agent_plist).ok()?;
        let args = t.split("<key>ProgramArguments</key>").nth(1)?;
        let first = args.split("<string>").nth(1)?.split("</string>").next()?;
        Some(xml_unescape(first))
    }

    /// Install (`on`) or remove the login agent file. Disk work: never on the main thread.
    pub fn set_login_file(&self, on: bool, program: &str) -> std::io::Result<()> {
        if on {
            write_atomic(&self.agent_plist, agent_plist(program).as_bytes())
        } else {
            match std::fs::remove_file(&self.agent_plist) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        }
    }
}

/// The login item as seen from a running ZTab (see [`Paths::login_state`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoginState {
    Off,
    /// The agent starts this binary.
    On,
    /// The agent exists but starts `program` (`exists`: whether that file exists).
    Elsewhere { program: String, exists: bool },
}

impl LoginState {
    /// The checkbox: ticked only when the agent starts this ZTab.
    pub fn checked(&self) -> bool {
        *self == LoginState::On
    }

    /// For `--login-status` and the log: `on`, `off`, or why the agent would not start this ZTab.
    pub fn describe(&self) -> String {
        match self {
            LoginState::Off => "off".into(),
            LoginState::On => "on".into(),
            LoginState::Elsewhere { program, exists: true } => format!("on, but for another copy of ZTab ({program})"),
            LoginState::Elsewhere { program, exists: false } => format!("broken: the program it starts does not exist ({program})"),
        }
    }
}

fn plist_label_is_ours(t: &str) -> bool {
    t.split("<key>Label</key>").nth(1).is_some_and(|rest| rest.trim_start().starts_with(&format!("<string>{AGENT_LABEL}</string>")))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&amp;", "&")
}

/// The login agent: launchd runs ZTab's binary directly (no `open`, no LaunchServices round
/// trip), once per login session, in the GUI (Aqua) session only, never restarts it (quitting
/// ZTab from its menu, or killing it, keeps it quit until the next login), and does not throttle
/// it as a background job. `AssociatedBundleIdentifiers` names ZTab in System Settings > Login
/// Items.
pub fn agent_plist(program: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{AGENT_LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<false/>
	<key>ProcessType</key>
	<string>Interactive</string>
	<key>LimitLoadToSessionType</key>
	<string>Aqua</string>
	<key>AssociatedBundleIdentifiers</key>
	<array>
		<string>{AGENT_LABEL}</string>
	</array>
</dict>
</plist>
"#,
        xml_escape(program)
    )
}

/// This process's binary (inside the app bundle when run from one).
pub fn current_program() -> String {
    let path = std::env::current_exe()
        .ok()
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    stable_program(&path, |p| std::fs::canonicalize(p).ok().map(|c| c.to_string_lossy().into_owned()))
}

/// A Homebrew install runs from a versioned keg (`<prefix>/Cellar/ztab/<version>/...`) that
/// `brew upgrade` deletes; the login agent must name the stable `<prefix>/opt/ztab/...` link
/// instead. `canon` resolves a path (symlinks), so the opt path is used only when it leads to
/// this very binary. Any other path is returned as it is.
pub fn stable_program(path: &str, canon: impl Fn(&str) -> Option<String>) -> String {
    let Some(i) = path.find("/Cellar/ztab/") else {
        return path.to_string();
    };
    let rest = &path[i + "/Cellar/ztab/".len()..];
    let Some(j) = rest.find('/') else {
        return path.to_string();
    };
    let opt = format!("{}/opt/ztab{}", &path[..i], &rest[j..]);
    if canon(&opt).as_deref() == Some(path) { opt } else { path.to_string() }
}

// ---------------------------------------------------------------------------------------------
// First launch: ZTab turns its login item on by itself, once
// ---------------------------------------------------------------------------------------------

/// Traces that an earlier run of ZTab leaves for this user outside the settings folder, and that
/// this run creates itself: the log (`~/Library/Logs/ztab.log`, or `ztab.log.1` after a rotation;
/// the first log line creates it) and the single-instance lock file in the user's temporary folder
/// (taking the lock creates it). So they are read at launch before the first log line and before
/// the lock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PriorRun {
    pub log: bool,
    pub lock_file: bool,
}

impl PriorRun {
    /// Three `stat`s (microseconds; on the main thread at launch).
    pub fn capture(log: &Path) -> PriorRun {
        let rotated = PathBuf::from(format!("{}.1", log.display()));
        PriorRun { log: exists(log) || exists(&rotated), lock_file: exists(&lock_path()) }
    }
}

/// A file (or a symbolic link, even a broken one) is at `p`.
fn exists(p: &Path) -> bool {
    p.symlink_metadata().is_ok()
}

/// What the first-launch rule ([`first_launch`]) looks at.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Evidence {
    /// `login_item_offered = true` in the settings.
    pub recorded: bool,
    /// `login_item_retry = true` in the settings: an earlier first launch could not write the
    /// agent, so the traces it left (settings file and folder, log, lock file) do not count.
    pub retry: bool,
    /// A file is at the login agent's path (whatever it contains).
    pub agent: bool,
    /// The settings file exists.
    pub settings_file: bool,
    /// The settings folder exists (`~/Library/Application Support/ZTab`; only ZTab creates it).
    pub settings_folder: bool,
    /// Traces outside the settings folder (see [`PriorRun`]).
    pub prior: PriorRun,
    /// The isolated test mode: the settings folder is the test's own directory, and the log and
    /// the lock file belong to the real user, so only the record, the settings file and the agent
    /// file count.
    pub isolated: bool,
}

/// The first-launch rule's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FirstLaunch {
    /// The question is settled (`login_item_offered`): nothing to do.
    Settled,
    /// This copy is not an installed app (why): nothing is changed or recorded.
    NotInstalled(&'static str),
    /// ZTab ran before for this user (the signs): the login item stays as it is, on or off (a
    /// user who turned it off keeps it off), and the question is recorded as settled.
    RanBefore(Vec<&'static str>),
    /// The very first launch for this user (or a retry, see [`Settings::login_item_retry`]): turn
    /// the login item on and record that; if the agent cannot be written, record a retry instead.
    TurnOn,
}

/// The first-launch rule. ZTab turns its login item on by itself only on the very first launch of
/// an installed copy for this user, so opening the app once (a Homebrew install, which cannot do
/// it while installing) is enough:
///
/// 1. `login_item_offered = true` in the settings: settled, nothing happens. Outcomes 3 and 4
///    record it (4 only once the agent is written), and so do the Settings checkbox and
///    `ztab --set-login on|off`.
/// 2. Not an installed copy ([`install_placement`]): nothing happens and nothing is recorded.
/// 3. Any sign that ZTab ran before for this user: a file at the login agent's path, the settings
///    file, the settings folder, the log (`ztab.log` or `ztab.log.1`) or the single-instance lock
///    file as found before this run made them. Then the login item stays as it is and the
///    question is recorded: a user who had turned it off (unticking deletes the agent and may
///    leave no settings file) never gets it back. In the isolated test mode only the settings
///    file and the agent file inside the test directory count (see [`Evidence::isolated`]). After
///    a failed first launch (`login_item_retry = true`) only an agent file counts: the other
///    traces are that launch's own.
/// 4. Otherwise: the first launch. Turn the login item on (write the agent for this binary) and
///    record it. When the agent cannot be written (an unwritable `~/Library/LaunchAgents`, a full
///    disk), record `login_item_retry = true` instead, so the next launch tries again.
///
/// The records are line edits of the settings file ([`Paths::record`]), so the rest of the file
/// stays exactly as it was.
pub fn first_launch(ev: &Evidence, placement: Result<(), &'static str>) -> FirstLaunch {
    if ev.recorded {
        return FirstLaunch::Settled;
    }
    if let Err(why) = placement {
        return FirstLaunch::NotInstalled(why);
    }
    let mut signs = Vec::new();
    if ev.agent {
        signs.push("a login agent file");
    }
    if ev.settings_file && !ev.retry {
        signs.push("the settings file");
    }
    if !ev.isolated && !ev.retry {
        if ev.settings_folder {
            signs.push("the settings folder");
        }
        if ev.prior.log {
            signs.push("the log");
        }
        if ev.prior.lock_file {
            signs.push("the instance lock file");
        }
    }
    if signs.is_empty() { FirstLaunch::TurnOn } else { FirstLaunch::RanBefore(signs) }
}

/// Whether `program` (this binary, as [`current_program`] names it) is an installed copy, the only
/// kind that turns the login item on by itself: inside an app bundle; not run through App
/// Translocation (a downloaded copy opened where it lies, from a randomised read-only path); not on
/// a mounted volume (`/Volumes/…`, such as a disk image) or a read-only file system
/// (`read_only`); not a build's output (`target/debug|release/…`, or a bundle directly in a `dist`
/// or `target` folder, as `scripts/bundle.sh` makes it); not in a temporary folder.
pub fn install_placement(program: &str, read_only: bool) -> Result<(), &'static str> {
    let Some(i) = program.find(".app/Contents/MacOS/") else {
        return Err("not inside an app bundle");
    };
    if program.contains("/AppTranslocation/") {
        return Err("run through App Translocation (a downloaded copy opened where it lies)");
    }
    if program.starts_with("/Volumes/") {
        return Err("on a mounted volume (/Volumes)");
    }
    if read_only {
        return Err("on a read-only volume");
    }
    if program.contains("/target/debug/") || program.contains("/target/release/") {
        return Err("a cargo build (target/)");
    }
    let bundle_dir = Path::new(&program[..i + ".app".len()]).parent().and_then(|d| d.file_name()).and_then(|n| n.to_str());
    if matches!(bundle_dir, Some("dist" | "target")) {
        return Err("a build's bundle (dist/ or target/)");
    }
    if ["/tmp/", "/private/tmp/", "/var/folders/", "/private/var/folders/"].iter().any(|t| program.starts_with(t)) {
        return Err("in a temporary folder");
    }
    Ok(())
}

/// The file system holding `path` is mounted read-only (one `statfs`).
pub fn read_only_volume(path: &str) -> bool {
    let Ok(c) = std::ffi::CString::new(path) else { return false };
    // SAFETY: statfs is plain old data; the call fills it for a valid C string.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statfs(c.as_ptr(), &mut st) }) == 0 && (st.f_flags & libc::MNT_RDONLY as u32) != 0
}

/// What the settings record about the start-at-login question ([`Paths::record`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Record {
    /// Settled: `login_item_offered = true`, and no `login_item_retry` line.
    Settled,
    /// Still open, tried again at the next launch: `login_item_retry = true`.
    Retry,
}

impl Record {
    /// `s` with this record (the in-memory copy of what [`Paths::record`] writes).
    pub fn apply(self, s: &Settings) -> Settings {
        match self {
            Record::Settled => Settings { login_item_offered: true, login_item_retry: false, ..*s },
            Record::Retry => Settings { login_item_retry: true, ..*s },
        }
    }

    /// The line edits: (key, `Some(value)`: set or append, `None`: remove).
    fn edits(self) -> &'static [(&'static str, Option<&'static str>)] {
        match self {
            Record::Settled => &[("login_item_offered", Some("true")), ("login_item_retry", None)],
            Record::Retry => &[("login_item_retry", Some("true"))],
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Record::Settled => "login_item_offered = true",
            Record::Retry => "login_item_retry = true (the next launch tries again)",
        }
    }
}

/// `text` (a settings file, any bytes, even invalid UTF-8) with every `key = …` line replaced by
/// `key = value` (or removed, `None`); `key = value` is appended when there is no such line.
/// Every other byte stays as it was: comments, unknown keys, values ZTab could not parse.
pub fn edit_conf(text: &[u8], key: &str, value: Option<&str>) -> Vec<u8> {
    let is_key = |line: &[u8]| {
        line.trim_ascii_start().strip_prefix(key.as_bytes()).is_some_and(|rest| rest.trim_ascii_start().first() == Some(&b'='))
    };
    let mut out = Vec::with_capacity(text.len() + key.len() + 16);
    let mut found = false;
    for line in text.split_inclusive(|&b| b == b'\n') {
        if is_key(line) {
            found = true;
            if let Some(v) = value {
                out.extend_from_slice(format!("{key} = {v}\n").as_bytes());
            }
        } else {
            out.extend_from_slice(line);
        }
    }
    if !found && let Some(v) = value {
        if !out.is_empty() && !out.ends_with(b"\n") {
            out.push(b'\n');
        }
        out.extend_from_slice(format!("{key} = {v}\n").as_bytes());
    }
    out
}

/// What [`Paths::apply_first_launch`] did with the login item.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum LoginApplied {
    /// Not touched (nothing to do, or a file was already at the agent's path).
    #[default]
    Untouched,
    /// The agent was written: ZTab starts at the next login.
    TurnedOn,
    /// Writing the agent failed (the error, short, for the windows): start at login stays off and
    /// the question stays open (`login_item_retry`).
    Failed(String),
}

/// The result of [`Paths::apply_first_launch`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    pub login: LoginApplied,
    /// The record meant for the settings (the in-memory copy takes it with [`Record::apply`],
    /// even when writing it failed: that is in `errors`). None: nothing to record.
    pub record: Option<Record>,
    /// What was done, and what failed (for the log).
    pub done: Vec<String>,
    pub errors: Vec<String>,
}

impl Paths {
    /// The first-launch evidence for these paths (`settings`: as loaded; `prior`: see
    /// [`PriorRun`]). A few `stat`s.
    pub fn first_launch_evidence(&self, settings: &Settings, prior: PriorRun) -> Evidence {
        Evidence {
            recorded: settings.login_item_offered,
            retry: settings.login_item_retry,
            agent: exists(&self.agent_plist),
            settings_file: exists(&self.settings_file),
            settings_folder: self.settings_file.parent().is_some_and(exists),
            prior,
            isolated: self.isolated,
        }
    }

    /// Write `r` into the settings file as line edits ([`edit_conf`]): the rest of the file stays
    /// byte for byte, whatever ZTab could parse of it (a missing file is written with the
    /// defaults). A file that cannot be read (permissions, an I/O error) is left alone and not
    /// recorded: it cannot be rewritten without losing it. Returns whether the file changed.
    /// Disk work: never on the main thread.
    pub fn record(&self, r: Record) -> Result<bool, String> {
        let file = self.settings_file.display();
        let text = match std::fs::read(&self.settings_file) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return write_atomic(&self.settings_file, serialize(&r.apply(&Settings::default())).as_bytes())
                    .map(|()| true)
                    .map_err(|e| format!("{file}: {e}"));
            }
            Err(e) => return Err(format!("cannot read {file} ({e}): {} not recorded, the file is left as it is", r.describe())),
        };
        let new = r.edits().iter().fold(text.clone(), |t, (k, v)| edit_conf(&t, k, *v));
        if new == text {
            return Ok(false);
        }
        write_atomic(&self.settings_file, &new).map(|()| true).map_err(|e| format!("{file}: {e}"))
    }

    /// Carry out a [`first_launch`] answer: `TurnOn` writes the login agent for `program` (unless
    /// a file appeared at its path meanwhile: never overwrite one) and records `Settled`, or
    /// `Retry` when the agent could not be written; `RanBefore` records `Settled`. It does not run
    /// `launchctl`: launchd loads the agent at the next login, as when the Settings checkbox is
    /// ticked (this ZTab runs already; loading the job now would start a second one, which would
    /// exit at once). Disk work: never on the main thread.
    pub fn apply_first_launch(&self, d: &FirstLaunch, program: &str) -> Applied {
        let mut a = Applied::default();
        let record = match d {
            FirstLaunch::Settled | FirstLaunch::NotInstalled(_) => return a,
            FirstLaunch::RanBefore(_) => Record::Settled,
            FirstLaunch::TurnOn if exists(&self.agent_plist) => {
                a.done.push(format!("a file appeared at {} meanwhile: left as it is", self.agent_plist.display()));
                Record::Settled
            }
            FirstLaunch::TurnOn => match self.set_login_file(true, program) {
                Ok(()) => {
                    a.login = LoginApplied::TurnedOn;
                    a.done.push(format!("wrote {} (program {program})", self.agent_plist.display()));
                    Record::Settled
                }
                Err(e) => {
                    a.errors.push(format!("could not write {}: {e}", self.agent_plist.display()));
                    a.login = LoginApplied::Failed(e.to_string());
                    Record::Retry
                }
            },
        };
        a.record = Some(record);
        match self.record(record) {
            Ok(changed) => a.done.push(format!(
                "{} {} in {}",
                if changed { "recorded" } else { "already recorded:" },
                record.describe(),
                self.settings_file.display()
            )),
            Err(e) => a.errors.push(e),
        }
        a
    }
}

/// `ztab --set-login on|off` / `--login-status` (command line, before any GUI): change the
/// login agent file and, outside the isolated mode, the launchd job: `on` boots the new
/// definition in (RunAtLoad starts ZTab now, like a login would; a second ZTab exits at once),
/// `off` boots it out (which stops a ZTab that launchd started). Either records the choice in the
/// settings ([`Record::Settled`], a line edit), so ZTab's first launch does not decide again (an
/// installer that leaves the login item off runs `--set-login off`). Returns the line to print.
pub fn cli_set_login(paths: &Paths, on: bool, program: &str) -> Result<String, String> {
    let domain = format!("gui/{}", unsafe { libc::getuid() });
    let launchctl = |args: &[&str]| -> Result<(), String> {
        let o = std::process::Command::new("/bin/launchctl").args(args).output().map_err(|e| e.to_string())?;
        if o.status.success() {
            Ok(())
        } else {
            Err(format!("launchctl {}: {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim()))
        }
    };
    let agent = paths.agent_plist.to_string_lossy().into_owned();
    if !paths.isolated {
        // Unload whatever definition launchd holds (an older `open -g` one, or this one).
        let _ = launchctl(&["bootout", &format!("{domain}/{AGENT_LABEL}")]);
    }
    paths.set_login_file(on, program).map_err(|e| format!("{}: {e}", paths.agent_plist.display()))?;
    // The choice is made: record it as a line edit (the rest of the settings file stays as it
    // is). A failure only costs the record; the evidence rule of `first_launch` still sees the
    // settings file, or the agent and the log this command writes.
    let record = match paths.record(Record::Settled) {
        Ok(_) => String::new(),
        Err(e) => format!("; could not record the choice: {e}"),
    };
    if on && !paths.isolated {
        launchctl(&["bootstrap", &domain, &agent]).map_err(|e| {
            format!(
                "{} written, but launchd did not load it ({e}); is ZTab switched off in System Settings > General > \
                 Login Items & Extensions (Allow in the Background)?",
                paths.agent_plist.display()
            )
        })?;
    }
    Ok(format!(
        "start at login: {} ({}{}){record}",
        paths.login_state(program).describe(),
        paths.agent_plist.display(),
        if paths.isolated { ", isolated: launchctl not used" } else { "" }
    ))
}

// ---------------------------------------------------------------------------------------------
// Writer thread
// ---------------------------------------------------------------------------------------------

type Job = Box<dyn FnOnce() + Send>;

/// Serial background thread for settings disk work. Saves are coalesced: only the newest
/// pending settings are written.
pub struct Writer {
    tx: mpsc::Sender<Job>,
    pending: Arc<Mutex<Option<(Paths, Settings)>>>,
}

impl Writer {
    pub fn new(report: fn(&str)) -> Writer {
        let (tx, rx) = mpsc::channel::<Job>();
        let spawned = std::thread::Builder::new().name("ztab-settings".into()).spawn(move || {
            while let Ok(job) = rx.recv() {
                job();
            }
        });
        if let Err(e) = spawned {
            report(&format!("settings: writer thread failed to start: {e}"));
        }
        Writer { tx, pending: Arc::new(Mutex::new(None)) }
    }

    /// Run `job` on the writer thread (in order with the other jobs).
    pub fn run(&self, job: impl FnOnce() + Send + 'static) {
        let _ = self.tx.send(Box::new(job));
    }

    /// Save `s` to `paths` on the writer thread; `done(result)` runs there afterwards.
    pub fn save(&self, paths: &Paths, s: Settings, done: fn(&Paths, &Settings, std::io::Result<()>)) {
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some((paths.clone(), s));
        let pending = self.pending.clone();
        self.run(move || {
            let job = pending.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some((p, s)) = job {
                let r = p.save(&s);
                done(&p, &s, r);
            }
        });
    }
}

// ---------------------------------------------------------------------------------------------
// Single instance
// ---------------------------------------------------------------------------------------------

/// Take the per-user single-instance lock (an exclusive `flock` on a file in the user's temporary
/// directory, held until the process exits). `false`: another ZTab holds it. Independent of the
/// settings location: two ZTabs must never run at once, whatever settings they use.
pub fn take_instance_lock() -> bool {
    let Ok(f) = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(lock_path()) else {
        // No lock file possible: do not refuse to start over it.
        return true;
    };
    use std::os::fd::AsRawFd as _;
    // SAFETY: plain syscall on a valid descriptor.
    let ok = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    if ok {
        // Held for the life of the process.
        std::mem::forget(f);
    }
    ok
}

/// The single-instance lock file (it stays after ZTab exits).
pub fn lock_path() -> PathBuf {
    user_temp_dir().join(format!("{AGENT_LABEL}.lock"))
}

pub fn user_temp_dir() -> PathBuf {
    let mut buf = vec![0u8; 1024];
    // SAFETY: buffer and length match.
    let n = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, buf.as_mut_ptr().cast(), buf.len()) };
    if n > 1 && n <= buf.len() {
        buf.truncate(n - 1);
        if let Ok(s) = String::from_utf8(buf) {
            return PathBuf::from(s);
        }
    }
    std::env::temp_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::{Hold, keycode};

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ztab-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn defaults_and_round_trip() {
        let d = Settings::default();
        assert_eq!(d.show_delay_ms, 120);
        assert_eq!(d.hotkeys, Bindings::default());
        assert_eq!(parse(""), (d, vec![]));
        let s = Settings {
            show_delay_ms: 0,
            hotkeys: Bindings { all: Binding { hold: Hold::Control, key: keycode::F18 }, same_app: Binding { hold: Hold::Command, key: 0x00 } },
            skip_screen_recording: true,
            screen_recording_requested: true,
            login_item_offered: true,
            login_item_retry: true,
        };
        assert_eq!(parse(&serialize(&s)), (s, vec![]));
        assert_eq!(parse(&serialize(&d)), (d, vec![]));
        assert!(serialize(&s).contains("\nskip_screen_recording = true\nscreen_recording_requested = true\nlogin_item_offered = true\nlogin_item_retry = true\n"));
        // The retry marker is written only while it is set.
        assert!(serialize(&d).ends_with("\nlogin_item_offered = false\n"));
        assert!(!serialize(&d).contains("login_item_retry"));
        // An older file without the keys: not skipped, not asked, the login question not settled.
        let old = parse("show_delay_ms = 50\n").0;
        assert!(!old.skip_screen_recording && !old.screen_recording_requested && !old.login_item_offered);
        assert!(parse("login_item_offered = 1\n").0.login_item_offered);
        assert_eq!(parse("login_item_offered = yes\n").1.len(), 1);
        assert!(parse("login_item_retry = true\n").0.login_item_retry && !old.login_item_retry);
        assert_eq!(parse("login_item_retry = later\n").1.len(), 1);
        assert_eq!(parse("skip_screen_recording = maybe\n").1.len(), 1);
        assert_eq!(parse("screen_recording_requested = maybe\n").1, vec!["screen_recording_requested: not true or false: \"maybe\"".to_string()]);
        assert!(parse("screen_recording_requested = 1\n").0.screen_recording_requested);
        // Restore defaults keeps the choice made in the Set up window and what was asked.
        let r = s.restored();
        assert_eq!(
            (r.show_delay_ms, r.hotkeys, r.skip_screen_recording, r.screen_recording_requested, r.login_item_offered, r.login_item_retry),
            (d.show_delay_ms, d.hotkeys, true, true, true, true)
        );
        // The records in memory.
        let open = Settings { show_delay_ms: 7, login_item_retry: true, ..d };
        assert_eq!(Record::Settled.apply(&open), Settings { show_delay_ms: 7, login_item_offered: true, ..d });
        assert_eq!(Record::Retry.apply(&Settings { show_delay_ms: 7, ..d }), open);
    }

    #[test]
    fn bad_lines_keep_defaults() {
        let (s, notes) = parse("show_delay_ms = abc\nhotkey_all = shift+48\nfoo = 1\nnonsense\n# comment\n");
        assert_eq!(s, Settings::default());
        assert_eq!(notes.len(), 4, "{notes:?}");
        let (s, notes) = parse("show_delay_ms = 900\n");
        assert_eq!(s.show_delay_ms, MAX_SHOW_DELAY_MS);
        assert_eq!(notes.len(), 1);
    }

    #[test]
    fn invalid_or_conflicting_shortcuts_fall_back() {
        // ⌘Tab is refused; the default comes back.
        let (s, notes) = parse("hotkey_all = command+48\n");
        assert_eq!(s.hotkeys.all, Binding::DEFAULT_ALL);
        assert_eq!(notes.len(), 1);
        // Same shortcut twice: the same-app one falls back to its default.
        let (s, notes) = parse("hotkey_all = control+79\nhotkey_same_app = control+79\n");
        assert_eq!(s.hotkeys.all, Binding { hold: Hold::Control, key: 79 });
        assert_eq!(s.hotkeys.same_app, Binding::DEFAULT_SAME_APP);
        assert_eq!(notes.len(), 1);
        // The all-windows shortcut moved to ⌥`: the same-app default would collide, so it takes ⌥Tab.
        let (s, _) = parse("hotkey_all = option+50\nhotkey_same_app = option+50\n");
        assert_eq!(s.hotkeys.all, Binding::DEFAULT_SAME_APP);
        assert_eq!(s.hotkeys.same_app, Binding::DEFAULT_ALL);
    }

    #[test]
    fn load_save_in_isolated_dir() {
        let dir = tmpdir("settings");
        let p = Paths::isolated(&dir);
        assert!(p.isolated);
        assert_eq!(p.settings_file, dir.join("settings.conf"));
        assert_eq!(p.agent_plist, dir.join("LaunchAgents/dev.ztab.app.plist"));
        let (s, notes, existed) = p.load();
        assert_eq!((s, notes.len(), existed), (Settings::default(), 0, false));
        let s2 = Settings { show_delay_ms: 300, ..Settings::default() };
        p.save(&s2).unwrap();
        assert_eq!(p.load(), (s2, vec![], true));
        // No temporary file is left behind.
        let names: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["settings.conf".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn paths_from_args() {
        let args: Vec<String> = ["ztab", "--settings-dir", "/tmp/x"].iter().map(|s| s.to_string()).collect();
        assert_eq!(Paths::from_args(&args), Paths::isolated(Path::new("/tmp/x")));
        if std::env::var_os(ENV_SETTINGS_DIR).is_none() {
            assert_eq!(Paths::from_args(&["ztab".to_string()]), Paths::user());
            assert!(Paths::user().settings_file.ends_with("Library/Application Support/ZTab/settings.conf"));
            assert!(Paths::user().agent_plist.ends_with("Library/LaunchAgents/dev.ztab.app.plist"));
        }
    }

    #[test]
    fn agent_plist_is_valid_and_execs_the_binary() {
        let prog = "/Users/someone/Applications/Z & T <ab>.app/Contents/MacOS/ztab";
        let xml = agent_plist(prog);
        for needle in [
            "<key>Label</key>\n\t<string>dev.ztab.app</string>",
            "<key>RunAtLoad</key>\n\t<true/>",
            "<key>KeepAlive</key>\n\t<false/>",
            "<key>ProcessType</key>\n\t<string>Interactive</string>",
            "<key>LimitLoadToSessionType</key>\n\t<string>Aqua</string>",
            "<string>/Users/someone/Applications/Z &amp; T &lt;ab&gt;.app/Contents/MacOS/ztab</string>",
        ] {
            assert!(xml.contains(needle), "missing {needle:?} in\n{xml}");
        }
        assert!(!xml.contains("/usr/bin/open"));
        // plutil agrees it is a valid property list with the expected values.
        let dir = tmpdir("plist");
        let p = Paths::isolated(&dir);
        assert!(!p.login_enabled());
        p.set_login_file(true, prog).unwrap();
        assert!(p.login_enabled());
        assert_eq!(p.login_program().as_deref(), Some(prog));
        let lint = std::process::Command::new("/usr/bin/plutil").arg("-lint").arg(&p.agent_plist).output().unwrap();
        assert!(lint.status.success(), "{}", String::from_utf8_lossy(&lint.stdout));
        let json = std::process::Command::new("/usr/bin/plutil")
            .args(["-convert", "json", "-o", "-"])
            .arg(&p.agent_plist)
            .output()
            .unwrap();
        let json = String::from_utf8_lossy(&json.stdout);
        assert!(json.contains(r#""ProgramArguments":["\/Users\/someone\/Applications\/Z & T <ab>.app\/Contents\/MacOS\/ztab"]"#), "{json}");
        assert!(json.contains(r#""KeepAlive":false"#) && json.contains(r#""RunAtLoad":true"#), "{json}");
        p.set_login_file(false, prog).unwrap();
        assert!(!p.login_enabled());
        // Removing it twice is fine.
        p.set_login_file(false, prog).unwrap();
        // A plist with another label is not ours.
        std::fs::write(&p.agent_plist, agent_plist(prog).replace("<string>dev.ztab.app</string>\n\t<key>Prog", "<string>other</string>\n\t<key>Prog")).unwrap();
        assert!(!p.login_enabled());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn homebrew_keg_path_becomes_the_opt_link() {
        let keg = "/opt/homebrew/Cellar/ztab/0.2.2/ZTab.app/Contents/MacOS/ztab";
        let opt = "/opt/homebrew/opt/ztab/ZTab.app/Contents/MacOS/ztab";
        // The opt link leads to this binary: use it.
        assert_eq!(stable_program(keg, |p| (p == opt).then(|| keg.to_string())), opt);
        // The opt link leads elsewhere (another version linked) or is missing: keep the keg path.
        assert_eq!(stable_program(keg, |_| Some("/somewhere/else".into())), keg);
        assert_eq!(stable_program(keg, |_| None), keg);
        // Not a Homebrew keg: unchanged.
        let app = "/Applications/ZTab.app/Contents/MacOS/ztab";
        assert_eq!(stable_program(app, |_| panic!("not consulted")), app);
        assert_eq!(stable_program("/x/Cellar/ztab/1.0", |_| None), "/x/Cellar/ztab/1.0");
    }

    #[test]
    fn login_state_compares_the_program_with_this_binary() {
        let dir = tmpdir("state");
        let p = Paths::isolated(&dir);
        let bin = dir.join("bin/ztab");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, b"x").unwrap();
        let other = dir.join("bin/other");
        std::fs::write(&other, b"x").unwrap();
        let bin_s = bin.to_string_lossy().into_owned();
        assert_eq!(p.login_state(&bin_s), LoginState::Off);
        p.set_login_file(true, &bin_s).unwrap();
        assert_eq!(p.login_state(&bin_s), LoginState::On);
        assert!(p.login_state(&bin_s).checked());
        // The same file through another spelling of its path is still this binary.
        let dotted = format!("{}/bin/../bin/ztab", dir.display());
        assert_eq!(p.login_state(&dotted), LoginState::On);
        // Another copy (the app was moved, or the box was ticked in a dev build).
        let st = p.login_state(&other.to_string_lossy());
        assert_eq!(st, LoginState::Elsewhere { program: bin_s.clone(), exists: true });
        assert!(!st.checked() && st.describe().contains("another copy"));
        // The program it starts was deleted.
        p.set_login_file(true, "/Applications/Gone/ZTab.app/Contents/MacOS/ztab").unwrap();
        let st = p.login_state(&bin_s);
        assert!(matches!(st, LoginState::Elsewhere { exists: false, .. }) && st.describe().starts_with("broken"), "{st:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cli_in_isolated_mode_never_needs_launchctl() {
        let dir = tmpdir("cli");
        let p = Paths::isolated(&dir);
        let bin = dir.join("ztab");
        std::fs::write(&bin, b"x").unwrap();
        let bin = bin.to_string_lossy().into_owned();
        let line = cli_set_login(&p, true, &bin).unwrap();
        assert!(line.starts_with("start at login: on (") && line.contains("isolated"), "{line}");
        assert!(p.login_enabled());
        // A program that does not exist is reported, not called "on".
        let line = cli_set_login(&p, true, "/x/ztab").unwrap();
        assert!(line.starts_with("start at login: broken"), "{line}");
        let line = cli_set_login(&p, false, &bin).unwrap();
        assert!(line.starts_with("start at login: off"), "{line}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cli_records_the_choice() {
        let dir = tmpdir("cli-record");
        let p = Paths::isolated(&dir);
        // Off on a fresh directory: nothing to remove, but the choice is recorded.
        cli_set_login(&p, false, "/x/ztab").unwrap();
        assert!(!p.login_enabled());
        assert!(p.load().0.login_item_offered);
        // Other settings are kept, byte for byte (a line edit), and a retry marker is cleared.
        let text = "# mine\nshow_delay_ms = 42\nfuture_key = 7\nlogin_item_retry = true\nlogin_item_offered = false\n";
        std::fs::write(&p.settings_file, text).unwrap();
        cli_set_login(&p, true, "/x/ztab").unwrap();
        assert_eq!(std::fs::read_to_string(&p.settings_file).unwrap(), "# mine\nshow_delay_ms = 42\nfuture_key = 7\nlogin_item_offered = true\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn edit_conf_changes_only_its_line() {
        let k = "login_item_offered";
        // Appended (a newline first when the file has none at its end), replaced, removed.
        assert_eq!(edit_conf(b"", k, Some("true")), b"login_item_offered = true\n");
        assert_eq!(edit_conf(b"a = 1", k, Some("true")), b"a = 1\nlogin_item_offered = true\n");
        assert_eq!(edit_conf(b"a = 1\n  login_item_offered\t=false\nb = 2\n", k, Some("true")), b"a = 1\nlogin_item_offered = true\nb = 2\n");
        assert_eq!(edit_conf(b"a = 1\nlogin_item_offered = false", k, Some("true")), b"a = 1\nlogin_item_offered = true\n");
        assert_eq!(edit_conf(b"login_item_offered = 1\na\nlogin_item_offered = 0\n", k, None), b"a\n");
        assert_eq!(edit_conf(b"a = 1\n", k, None), b"a = 1\n");
        // Comments, longer keys and other bytes are not its line.
        let other = b"# login_item_offered = true\nlogin_item_offered_x = 1\nx = \xe9\r\n";
        let mut want = other.to_vec();
        want.extend_from_slice(b"login_item_offered = true\n");
        assert_eq!(edit_conf(other, k, Some("true")), want);
    }

    #[test]
    fn record_keeps_the_rest_of_the_file() {
        let dir = tmpdir("record");
        let p = Paths::isolated(&dir);
        // The reviewer's case: a Latin-1 byte in a comment (the file is not UTF-8, so loading it
        // gives the defaults), shortcuts, a delay and a key of a newer build. Byte for byte, plus
        // one line.
        let text: &[u8] = b"# caf\xe9 settings\nshow_delay_ms = 300\nhotkey_all = control+48\nfuture_key = 7\n";
        std::fs::write(&p.settings_file, text).unwrap();
        assert!(p.load().0 == Settings::default(), "not UTF-8: the defaults");
        assert_eq!(p.record(Record::Settled), Ok(true));
        let mut want = text.to_vec();
        want.extend_from_slice(b"login_item_offered = true\n");
        assert_eq!(std::fs::read(&p.settings_file).unwrap(), want);
        // Already recorded: the file is not written again.
        assert_eq!(p.record(Record::Settled), Ok(false));
        // A retry marker is a line too, and settling removes it.
        std::fs::write(&p.settings_file, "show_delay_ms = 300\n").unwrap();
        assert_eq!(p.record(Record::Retry), Ok(true));
        assert_eq!(std::fs::read_to_string(&p.settings_file).unwrap(), "show_delay_ms = 300\nlogin_item_retry = true\n");
        assert_eq!(p.record(Record::Settled), Ok(true));
        assert_eq!(std::fs::read_to_string(&p.settings_file).unwrap(), "show_delay_ms = 300\nlogin_item_offered = true\n");
        // No file: the defaults with the record.
        std::fs::remove_file(&p.settings_file).unwrap();
        assert_eq!(p.record(Record::Retry), Ok(true));
        assert_eq!(p.load(), (Settings { login_item_retry: true, ..Settings::default() }, vec![], true));
        // A file that cannot be read is left alone: not recorded, not replaced.
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::write(&p.settings_file, "show_delay_ms = 300\n").unwrap();
        std::fs::set_permissions(&p.settings_file, std::fs::Permissions::from_mode(0o000)).unwrap();
        let e = p.record(Record::Settled).unwrap_err();
        assert!(e.contains("cannot read") && e.contains("left as it is"), "{e}");
        std::fs::set_permissions(&p.settings_file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(std::fs::read_to_string(&p.settings_file).unwrap(), "show_delay_ms = 300\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    const APP: &str = "/Applications/ZTab.app/Contents/MacOS/ztab";

    fn fresh() -> Evidence {
        Evidence::default()
    }

    #[test]
    fn first_launch_rule() {
        use FirstLaunch::*;
        // A fresh user: no record, no agent, no settings, no log, no lock file.
        assert_eq!(first_launch(&fresh(), Ok(())), TurnOn);
        // Each sign of an earlier run alone keeps the login item as it is (and is recorded).
        let one = |f: fn(&mut Evidence)| {
            let mut e = fresh();
            f(&mut e);
            first_launch(&e, Ok(()))
        };
        assert_eq!(one(|e| e.prior.log = true), RanBefore(vec!["the log"]));
        assert_eq!(one(|e| e.prior.lock_file = true), RanBefore(vec!["the instance lock file"]));
        assert_eq!(one(|e| e.settings_file = true), RanBefore(vec!["the settings file"]));
        assert_eq!(one(|e| e.settings_folder = true), RanBefore(vec!["the settings folder"]));
        // The login item is on already (or a file of someone else's is there): not touched.
        assert_eq!(one(|e| e.agent = true), RanBefore(vec!["a login agent file"]));
        // An existing user who turned it off: unticking deleted the agent and wrote no settings
        // before this version, but the log is there.
        let mut off_before = fresh();
        off_before.prior.log = true;
        off_before.settings_folder = false;
        assert!(matches!(first_launch(&off_before, Ok(())), RanBefore(_)));
        // Recorded (the user toggled the box, an installer chose, or a launch decided): settled,
        // whatever else is (or is no longer) there, even with every trace deleted.
        let mut recorded = fresh();
        recorded.recorded = true;
        assert_eq!(first_launch(&recorded, Ok(())), Settled);
        assert_eq!(first_launch(&recorded, Err("x")), Settled);
        // Not an installed copy: nothing happens, nothing is recorded.
        assert_eq!(first_launch(&fresh(), Err("on a mounted volume (/Volumes)")), NotInstalled("on a mounted volume (/Volumes)"));
        // Isolated: the folder is the test directory, the log and the lock file are the real
        // user's; only the settings file and the agent inside the directory count.
        let mut iso = Evidence { isolated: true, settings_folder: true, prior: PriorRun { log: true, lock_file: true }, ..fresh() };
        assert_eq!(first_launch(&iso, Ok(())), TurnOn);
        iso.settings_file = true;
        assert_eq!(first_launch(&iso, Ok(())), RanBefore(vec!["the settings file"]));
        iso.settings_file = false;
        iso.agent = true;
        assert_eq!(first_launch(&iso, Ok(())), RanBefore(vec!["a login agent file"]));
        iso.recorded = true;
        assert_eq!(first_launch(&iso, Ok(())), Settled);
        // Several signs are all named.
        let all = Evidence { agent: true, settings_file: true, settings_folder: true, prior: PriorRun { log: true, lock_file: true }, ..fresh() };
        assert_eq!(first_launch(&all, Ok(())), RanBefore(vec!["a login agent file", "the settings file", "the settings folder", "the log", "the instance lock file"]));
        // A retry (the last first launch could not write the agent): its own traces do not count,
        // an agent file still does, a record still settles it, and only an installed copy retries.
        let retry = Evidence { retry: true, settings_file: true, settings_folder: true, prior: PriorRun { log: true, lock_file: true }, ..fresh() };
        assert_eq!(first_launch(&retry, Ok(())), TurnOn);
        assert_eq!(first_launch(&Evidence { isolated: true, ..retry }, Ok(())), TurnOn);
        assert_eq!(first_launch(&Evidence { agent: true, ..retry }, Ok(())), RanBefore(vec!["a login agent file"]));
        assert_eq!(first_launch(&Evidence { recorded: true, ..retry }, Ok(())), Settled);
        assert_eq!(first_launch(&retry, Err("x")), NotInstalled("x"));
    }

    #[test]
    fn only_an_installed_copy_turns_it_on() {
        let ok = |p: &str| install_placement(p, false);
        assert_eq!(ok(APP), Ok(()));
        assert_eq!(ok("/Users/a/Applications/ZTab.app/Contents/MacOS/ztab"), Ok(()));
        assert_eq!(ok("/opt/homebrew/opt/ztab/ZTab.app/Contents/MacOS/ztab"), Ok(()));
        assert_eq!(ok("/opt/homebrew/Cellar/ztab/0.2.3/ZTab.app/Contents/MacOS/ztab"), Ok(()));
        assert_eq!(ok("/Users/a/Apps/dist-tools/ZTab.app/Contents/MacOS/ztab"), Ok(()));
        assert_eq!(ok("/Users/a/.claude/progress/x/first-launch/ZTab.app/Contents/MacOS/ztab"), Ok(()));
        let no = |p: &str| install_placement(p, false).unwrap_err();
        assert!(no("/Users/a/src/ztab/target/release/ztab").contains("app bundle"));
        assert!(no("/usr/local/bin/ztab").contains("app bundle"));
        assert!(no("/private/var/folders/x1/abc/T/AppTranslocation/0F2A/d/ZTab.app/Contents/MacOS/ztab").contains("Translocation"));
        assert!(no("/Volumes/ZTab/ZTab.app/Contents/MacOS/ztab").contains("/Volumes"));
        assert!(install_placement(APP, true).unwrap_err().contains("read-only"));
        assert!(no("/Users/a/src/ztab/target/release/bundle/osx/ZTab.app/Contents/MacOS/ztab").contains("cargo"));
        assert!(no("/Users/a/src/ztab/target/debug/bundle/osx/ZTab.app/Contents/MacOS/ztab").contains("cargo"));
        assert!(no("/Users/a/src/ztab/dist/ZTab.app/Contents/MacOS/ztab").contains("dist/"));
        assert!(no("/Users/a/src/ztab/target/ZTab.app/Contents/MacOS/ztab").contains("dist/"));
        assert!(no("/private/var/folders/x1/abc/T/ZTab.app/Contents/MacOS/ztab").contains("temporary"));
        assert!(no("/private/tmp/ZTab.app/Contents/MacOS/ztab").contains("temporary"));
        assert!(no("/tmp/ZTab.app/Contents/MacOS/ztab").contains("temporary"));
        // The real file systems: the data volume (where /Applications and the home folder live)
        // is writable, the sealed system volume is not.
        assert!(!read_only_volume(&std::env::temp_dir().to_string_lossy()));
        assert!(!read_only_volume("/Applications"));
        assert!(read_only_volume("/System/Library/CoreServices"));
        assert!(!read_only_volume("/does/not/exist"));
    }

    #[test]
    fn first_launch_in_an_isolated_dir() {
        let dir = tmpdir("first");
        let p = Paths::isolated(&dir);
        let prog = dir.join("ZTab.app/Contents/MacOS/ztab");
        std::fs::create_dir_all(prog.parent().unwrap()).unwrap();
        std::fs::write(&prog, b"x").unwrap();
        let prog = prog.to_string_lossy().into_owned();
        // The real user's traces exist (this user runs ZTab): ignored in the isolated mode.
        let prior = PriorRun { log: true, lock_file: true };
        let (s, _, _) = p.load();
        let ev = p.first_launch_evidence(&s, prior);
        assert_eq!(ev, Evidence { settings_folder: true, prior, isolated: true, ..Evidence::default() });
        let d = first_launch(&ev, Ok(()));
        assert_eq!(d, FirstLaunch::TurnOn);
        let a = p.apply_first_launch(&d, &prog);
        assert_eq!((&a.login, a.record, a.errors.len()), (&LoginApplied::TurnedOn, Some(Record::Settled), 0), "{a:?}");
        assert!(a.done[0].starts_with("wrote") && a.done[1].starts_with("recorded"), "{a:?}");
        assert_eq!(p.login_state(&prog), LoginState::On);
        assert_eq!(p.login_program().as_deref(), Some(prog.as_str()));
        let (s2, notes, existed) = p.load();
        assert_eq!((s2, notes.len(), existed), (Settings { login_item_offered: true, ..s }, 0, true));
        // The next launch: settled, nothing changes.
        assert_eq!(first_launch(&p.first_launch_evidence(&s2, prior), Ok(())), FirstLaunch::Settled);

        // Turned off afterwards (the box, or --set-login off): stays off at every later launch.
        cli_set_login(&p, false, &prog).unwrap();
        let (s3, _, _) = p.load();
        assert_eq!(first_launch(&p.first_launch_evidence(&s3, prior), Ok(())), FirstLaunch::Settled);
        assert_eq!(p.login_state(&prog), LoginState::Off);

        // A directory with settings from an earlier version (no record) and no agent: ran before;
        // the agent is not written, the record is, as one more line.
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&p.settings_file, "show_delay_ms = 80\n").unwrap();
        let (s4, _, _) = p.load();
        let d = first_launch(&p.first_launch_evidence(&s4, prior), Ok(()));
        assert_eq!(d, FirstLaunch::RanBefore(vec!["the settings file"]));
        let a = p.apply_first_launch(&d, &prog);
        assert_eq!((&a.login, a.record, a.errors.len()), (&LoginApplied::Untouched, Some(Record::Settled), 0), "{a:?}");
        assert!(!p.agent_plist.exists());
        assert_eq!(std::fs::read_to_string(&p.settings_file).unwrap(), "show_delay_ms = 80\nlogin_item_offered = true\n");

        // A file that appears at the agent's path between the decision and the write is kept.
        std::fs::remove_dir_all(&dir).unwrap();
        let d = first_launch(&p.first_launch_evidence(&Settings::default(), prior), Ok(()));
        assert_eq!(d, FirstLaunch::TurnOn);
        std::fs::create_dir_all(p.agent_plist.parent().unwrap()).unwrap();
        std::fs::write(&p.agent_plist, "not ours").unwrap();
        let a = p.apply_first_launch(&d, &prog);
        assert!(a.done[0].contains("left as it is") && a.login == LoginApplied::Untouched && a.record == Some(Record::Settled), "{a:?}");
        assert_eq!(std::fs::read_to_string(&p.agent_plist).unwrap(), "not ours");

        // Settled and not-installed answers change nothing.
        std::fs::remove_dir_all(&dir).unwrap();
        for d in [FirstLaunch::Settled, FirstLaunch::NotInstalled("x")] {
            assert_eq!(p.apply_first_launch(&d, &prog), Applied::default());
            assert!(!dir.exists());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_first_launch_is_retried() {
        let dir = tmpdir("retry");
        let p = Paths::isolated(&dir);
        let prog = "/Applications/ZTab.app/Contents/MacOS/ztab";
        let prior = PriorRun::default();
        // The agent cannot be written: a regular file where the LaunchAgents folder should be.
        let agents = p.agent_plist.parent().unwrap().to_path_buf();
        std::fs::write(&agents, "not a folder").unwrap();
        let d = first_launch(&p.first_launch_evidence(&p.load().0, prior), Ok(()));
        assert_eq!(d, FirstLaunch::TurnOn);
        let a = p.apply_first_launch(&d, prog);
        let LoginApplied::Failed(e) = &a.login else { panic!("{a:?}") };
        assert!(!e.is_empty() && !e.contains('/'), "short, for the windows: {e}");
        assert_eq!(a.record, Some(Record::Retry));
        assert!(a.errors.len() == 1 && a.errors[0].starts_with("could not write"), "{a:?}");
        // Not settled: the retry marker is recorded instead, and start at login is off.
        let (s, _, _) = p.load();
        assert_eq!((s.login_item_offered, s.login_item_retry), (false, true));
        assert_eq!(p.login_state(prog), LoginState::Off);
        // The next launch tries again, although the settings file (the marker's) exists now.
        let ev = p.first_launch_evidence(&s, prior);
        assert!(ev.settings_file && ev.retry);
        assert_eq!(first_launch(&ev, Ok(())), FirstLaunch::TurnOn);
        // Still failing: still open.
        assert_eq!(p.apply_first_launch(&FirstLaunch::TurnOn, prog).record, Some(Record::Retry));
        assert_eq!(first_launch(&p.first_launch_evidence(&p.load().0, prior), Ok(())), FirstLaunch::TurnOn);
        // Fixed (the folder can be created): turned on, settled, the marker gone.
        std::fs::remove_file(&agents).unwrap();
        let a = p.apply_first_launch(&FirstLaunch::TurnOn, prog);
        assert_eq!((&a.login, a.record, a.errors.len()), (&LoginApplied::TurnedOn, Some(Record::Settled), 0), "{a:?}");
        let text = std::fs::read_to_string(&p.settings_file).unwrap();
        assert!(text.contains("\nlogin_item_offered = true\n") && !text.contains("login_item_retry"), "{text}");
        assert_eq!(first_launch(&p.first_launch_evidence(&p.load().0, prior), Ok(())), FirstLaunch::Settled);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prior_run_traces() {
        let dir = tmpdir("prior");
        let log = dir.join("ztab.log");
        let lock = exists(&lock_path());
        assert_eq!(PriorRun::capture(&log), PriorRun { log: false, lock_file: lock });
        std::fs::write(dir.join("ztab.log.1"), b"x").unwrap();
        assert!(PriorRun::capture(&log).log);
        std::fs::remove_file(dir.join("ztab.log.1")).unwrap();
        std::fs::write(&log, b"x").unwrap();
        assert!(PriorRun::capture(&log).log);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn writer_coalesces_saves() {
        static SAVES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = tmpdir("writer");
        let p = Paths::isolated(&dir);
        let w = Writer::new(|_| {});
        // Block the writer so the saves pile up.
        let (go_tx, go_rx) = mpsc::channel::<()>();
        w.run(move || {
            let _ = go_rx.recv();
        });
        for ms in [10, 20, 30] {
            w.save(&p, Settings { show_delay_ms: ms, ..Settings::default() }, |_, _, r| {
                r.unwrap();
                SAVES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            });
        }
        go_tx.send(()).unwrap();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        w.run(move || {
            let _ = done_tx.send(());
        });
        done_rx.recv().unwrap();
        assert_eq!(SAVES.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(p.load().0.show_delay_ms, 30);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn instance_lock_temp_dir_is_per_user() {
        let d = user_temp_dir();
        assert!(d.is_dir(), "{}", d.display());
    }
}
