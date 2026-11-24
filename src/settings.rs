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
}

impl Default for Settings {
    fn default() -> Self {
        Settings { show_delay_ms: DEFAULT_SHOW_DELAY_MS, hotkeys: Bindings::default() }
    }
}

impl Settings {
    pub fn describe(&self) -> String {
        format!(
            "show_delay_ms={} hotkey_all={} hotkey_same_app={}",
            self.show_delay_ms,
            self.hotkeys.all.label(),
            self.hotkeys.same_app.label()
        )
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
         hotkey_same_app = {}\n",
        s.show_delay_ms,
        s.hotkeys.all.to_conf(),
        s.hotkeys.same_app.to_conf()
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
    std::env::current_exe()
        .ok()
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `ztab --set-login on|off` / `--login-status` (command line, before any GUI): change the
/// login agent file and, outside the isolated mode, the launchd job: `on` boots the new
/// definition in (RunAtLoad starts ZTab now, like a login would; a second ZTab exits at once),
/// `off` boots it out (which stops a ZTab that launchd started). Returns the line to print.
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
        "start at login: {} ({}{})",
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
    let dir = user_temp_dir();
    let path = dir.join(format!("{AGENT_LABEL}.lock"));
    let Ok(f) = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path) else {
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

fn user_temp_dir() -> PathBuf {
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
        };
        assert_eq!(parse(&serialize(&s)), (s, vec![]));
        assert_eq!(parse(&serialize(&d)), (d, vec![]));
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
