//! First-run permissions, without AppKit: what the "Set up ZTab" window shows and what its
//! buttons do (`setup_window.rs` is the window, `main.rs` wires the two together).
//!
//! ZTab needs Accessibility (required: the ⌥Tab event tap, reading and raising windows) and
//! Screen Recording (optional: window thumbnails). It never asks macOS for either by itself: at
//! launch it only checks them (which shows nothing) and, when one is missing, opens the window.
//!
//! * "Grant…" ([`grant_action`]): the first click in a run calls the request API
//!   (`AXIsProcessTrustedWithOptions` with the prompt option, `CGRequestScreenCaptureAccess`),
//!   which adds ZTab to that list in System Settings (switched off) and lets macOS show its own
//!   dialog, whose "Open System Settings" button opens the right page. ZTab does not open the page
//!   as well (that would put the page and the dialog up at once). Every later click opens the page
//!   directly (ZTab is listed by then). So one click shows one thing: macOS's dialog or the page.
//!   macOS shows the Screen Recording dialog only once per app, so once ZTab has asked for it
//!   (remembered in the settings file, `screen_recording_requested`), the first click of a later
//!   run opens the page directly too. The request APIs run on a background thread.
//! * While the window is open the status is checked every second off the main thread
//!   ([`poll`]). Accessibility takes effect at once (`main.rs` then starts the tap and the window
//!   model). Screen Recording is only used by a process started after the grant, so ZTab restarts
//!   itself ([`relaunch_plan`]): a ZTab started by its login agent asks launchd
//!   (`launchctl kickstart -k`), any other one starts the same bundle again with the same
//!   arguments once it has exited. A marker file ([`write_marker`] / [`take_marker`]) makes the new
//!   ZTab open the window again, showing the result. It also records whether the old ZTab had
//!   seen Screen Recording granted: only then does the new one not restart itself for it again
//!   (that restart did not help; another could loop). A restart for any other reason (the
//!   "Restart ZTab" button after granting Accessibility) leaves the automatic one in place.
//! * Closing the window while Accessibility is granted and Screen Recording is not counts as
//!   "Skip Thumbnails" ([`skip_on_close`]): it does not open at launch for Screen Recording again
//!   (the status menu's "Set Up ZTab…" still opens it).
//! * The in-process Screen Recording check may keep its first answer until a restart, so the
//!   poll also asks a child process (`ztab --screen-status`, the same signed binary, attributed to
//!   ZTab).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::settings::{AGENT_LABEL, user_temp_dir};

/// The two permissions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Perm {
    Accessibility,
    ScreenRecording,
}

impl Perm {
    pub fn name(self) -> &'static str {
        match self {
            Perm::Accessibility => "Accessibility",
            Perm::ScreenRecording => "Screen Recording",
        }
    }

    /// The System Settings page (Privacy & Security → this permission).
    pub fn pane_url(self) -> &'static str {
        match self {
            Perm::Accessibility => "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
            Perm::ScreenRecording => "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
        }
    }
}

/// Why the window opens at launch (`None`: it does not). Accessibility missing always opens it;
/// Screen Recording missing opens it unless the user chose "Skip Thumbnails"; `flag`
/// (`--setup-window`) and `reopen` (the marker of a setup restart) open it whatever the state.
pub fn launch_reason(ax: bool, screen: bool, skip_screen: bool, flag: bool, reopen: bool) -> Option<&'static str> {
    if !ax {
        Some("Accessibility is not granted")
    } else if reopen {
        Some("reopened after a setup restart")
    } else if flag {
        Some("--setup-window")
    } else if !screen && !skip_screen {
        Some("Screen Recording is not granted")
    } else {
        None
    }
}

/// The window's main button (bottom right).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Primary {
    /// Everything granted: close.
    Done,
    /// Accessibility granted, Screen Recording not: close and do not open the window at launch
    /// again for Screen Recording (`skip_screen_recording` in the settings).
    SkipThumbnails,
    /// Accessibility missing: close for now (it opens again at the next launch).
    Later,
}

impl Primary {
    pub fn title(self) -> &'static str {
        match self {
            Primary::Done => "Done",
            Primary::SkipThumbnails => "Skip Thumbnails",
            Primary::Later => "Later",
        }
    }
}

pub fn primary(ax: bool, screen: bool) -> Primary {
    match (ax, screen) {
        (true, true) => Primary::Done,
        (true, false) => Primary::SkipThumbnails,
        (false, _) => Primary::Later,
    }
}

/// What a "Grant…" click does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantAction {
    /// Call the request API (macOS lists ZTab and shows its dialog; nothing else is opened).
    Request,
    /// Open the System Settings page directly.
    OpenPane,
}

/// `clicks_before`: how often this permission's button was clicked before in this run;
/// `screen_requested`: ZTab already asked for Screen Recording in an earlier run (macOS shows that
/// dialog only once per app, so asking again would show nothing).
pub fn grant_action(p: Perm, clicks_before: u32, screen_requested: bool) -> GrantAction {
    let asked_before = p == Perm::ScreenRecording && screen_requested;
    if clicks_before == 0 && !asked_before { GrantAction::Request } else { GrantAction::OpenPane }
}

/// The button's title after `clicks` clicks.
pub fn grant_title(clicks: u32) -> &'static str {
    if clicks == 0 { "Grant…" } else { "Open System Settings…" }
}

/// One status check (see [`poll`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Polled {
    pub ax: bool,
    /// `CGPreflightScreenCaptureAccess` in this process.
    pub screen_here: bool,
    /// The same check in a child process (asked only when this process says no; `None`: not
    /// asked or it failed).
    pub screen_child: Option<bool>,
    /// How long each check took (ms; off the main thread).
    pub ax_ms: f64,
    pub screen_ms: f64,
    pub child_ms: Option<f64>,
}

impl Polled {
    pub fn screen(&self) -> bool {
        self.screen_here || self.screen_child == Some(true)
    }

    /// Whether `other` shows the same state (timings aside, and whether the child process was
    /// asked: it is asked on the first check, then only while this process says no).
    pub fn same_state(&self, other: &Polled) -> bool {
        self.ax == other.ax && self.screen_here == other.screen_here && self.screen() == other.screen()
    }
}

/// How the Screen Recording row reads, from the current check and what this process had at launch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenRow {
    Granted,
    /// Granted after this process started: it only takes effect in a new process.
    GrantedRestartNeeded,
    NotGranted,
}

pub fn screen_row(now: bool, at_launch: bool) -> ScreenRow {
    match (now, at_launch) {
        (true, true) => ScreenRow::Granted,
        (true, false) => ScreenRow::GrantedRestartNeeded,
        (false, _) => ScreenRow::NotGranted,
    }
}

/// Restart automatically: Screen Recording was granted after launch, unless this process comes
/// from a setup restart made after Screen Recording was already granted (`restart_saw_screen`:
/// that restart did not help, a second one would not either and could loop). A setup restart made
/// for another reason (Accessibility, the button clicked before the grant) does not count.
pub fn should_auto_restart(p: &Polled, screen_at_launch: bool, restart_saw_screen: bool) -> bool {
    p.screen() && !screen_at_launch && !restart_saw_screen
}

/// Closing the window (its close button, ⌘W) after a status check of this opening found
/// Accessibility granted and Screen Recording not: the same as "Skip Thumbnails". `checked`: the
/// newest check made since the window opened (`None`: none yet; nothing is decided from a stale
/// state).
pub fn skip_on_close(checked: Option<&Polled>) -> bool {
    checked.is_some_and(|p| p.ax && !p.screen())
}

// ---------------------------------------------------------------------------------------------
// Checks and requests (never on the main thread)
// ---------------------------------------------------------------------------------------------

pub fn ax_trusted() -> bool {
    unsafe { objc2_application_services::AXIsProcessTrusted() }
}

/// `AXIsProcessTrustedWithOptions` with the prompt option: lists ZTab under Accessibility and
/// shows macOS's dialog when it is not trusted. Returns whether it is trusted.
pub fn request_accessibility() -> bool {
    use objc2_core_foundation::{CFBoolean, CFDictionary, CFString};
    let key = CFString::from_static_str("AXTrustedCheckOptionPrompt");
    let dict = CFDictionary::from_slices(&[&*key], &[&*CFBoolean::new(true)]);
    unsafe { objc2_application_services::AXIsProcessTrustedWithOptions(Some(dict.as_opaque())) }
}

/// `CGRequestScreenCaptureAccess`: lists ZTab under Screen Recording and shows macOS's dialog the
/// first time (later calls show nothing). Returns whether access is granted.
pub fn request_screen_recording() -> bool {
    objc2_core_graphics::CGRequestScreenCaptureAccess()
}

/// The child process check: `ztab --screen-status` exits 0 when Screen Recording is granted.
/// Waits at most 3 s.
pub fn child_screen_status(exe: &Path) -> Option<bool> {
    let mut child = std::process::Command::new(exe)
        .arg("--screen-status")
        .env_remove(crate::settings::ENV_SETTINGS_DIR)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let end = Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(st)) => return st.code().map(|c| c == 0),
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Check both permissions (blocking; a background thread calls it). The child process is asked
/// when this process says Screen Recording is not granted, and always when `ask_child` (the
/// first check after the window opens, so its answer is seen either way).
pub fn poll(exe: &Path, ask_child: bool) -> Polled {
    let t = Instant::now();
    let ax = ax_trusted();
    let ax_ms = ms(t);
    let t = Instant::now();
    let screen_here = crate::windows::screen_recording_granted();
    let screen_ms = ms(t);
    let (screen_child, child_ms) = if screen_here && !ask_child {
        (None, None)
    } else {
        let t = Instant::now();
        let r = child_screen_status(exe);
        (r, Some(ms(t)))
    };
    Polled { ax, screen_here, screen_child, ax_ms, screen_ms, child_ms }
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

/// Open a System Settings page (a child process; never waits on the caller's thread).
pub fn open_pane(p: Perm) {
    let url = p.pane_url();
    let _ = std::thread::Builder::new().name("ztab-open-settings".into()).spawn(move || {
        let _ = std::process::Command::new("/usr/bin/open").arg(url).status();
    });
}

// ---------------------------------------------------------------------------------------------
// Restart
// ---------------------------------------------------------------------------------------------

/// How ZTab starts itself again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Relaunch {
    /// Started by the login agent: `launchctl kickstart -k <target>` (launchd stops this process
    /// and starts the job again).
    Kickstart { target: String },
    /// Inside an app bundle: once this process has exited, `open -n -g <bundle> --args <args>`.
    Open { bundle: PathBuf, args: Vec<String> },
    /// A bare binary (development): once this process has exited, run it again with `args`.
    Exec { bin: PathBuf, args: Vec<String> },
}

/// `xpc_service_name`: `$XPC_SERVICE_NAME`, which launchd sets to the job's label for the
/// processes it starts (an app opened through LaunchServices gets `application.<id>.<n>`).
/// `args`: the arguments after the program name.
pub fn relaunch_plan(xpc_service_name: Option<&str>, uid: u32, exe: &Path, args: &[String]) -> Relaunch {
    if xpc_service_name == Some(AGENT_LABEL) {
        return Relaunch::Kickstart { target: format!("gui/{uid}/{AGENT_LABEL}") };
    }
    match bundle_of(exe) {
        Some(bundle) => Relaunch::Open { bundle, args: args.to_vec() },
        None => Relaunch::Exec { bin: exe.to_path_buf(), args: args.to_vec() },
    }
}

/// The `.app` bundle a binary at `<bundle>/Contents/MacOS/<name>` belongs to.
pub fn bundle_of(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    let is = |p: &Path, n: &str| p.file_name().is_some_and(|f| f == n);
    (is(macos, "MacOS") && is(contents, "Contents") && bundle.extension().is_some_and(|e| e == "app")).then(|| bundle.to_path_buf())
}

/// The command a detached helper runs once this process has exited (`Open` / `Exec` plans).
pub fn helper_command(plan: &Relaunch) -> Option<Vec<String>> {
    match plan {
        Relaunch::Kickstart { .. } => None,
        Relaunch::Open { bundle, args } => {
            let mut v = vec!["/usr/bin/open".into(), "-n".into(), "-g".into(), bundle.to_string_lossy().into_owned()];
            if !args.is_empty() {
                v.push("--args".into());
                v.extend(args.iter().cloned());
            }
            Some(v)
        }
        Relaunch::Exec { bin, args } => {
            let mut v = vec![bin.to_string_lossy().into_owned()];
            v.extend(args.iter().cloned());
            Some(v)
        }
    }
}

/// The helper: waits (up to 10 s) until process `$1` has exited, then runs the rest of its
/// arguments, retrying twice a second apart (LaunchServices can refuse an `open` while it still
/// holds the old instance).
const HELPER_SCRIPT: &str = r#"pid=$1; shift
i=0; while /bin/kill -0 "$pid" 2>/dev/null; do sleep 0.05; i=$((i+1)); [ "$i" -gt 200 ] && exit 1; done
sleep 0.2
for t in 1 2 3; do "$@" && exit 0; sleep 1; done
exit 1"#;

/// Start the restart. `Kickstart`: runs launchctl and waits for it (launchd then stops this
/// process); returns its outcome. `Open` / `Exec`: spawns the helper in its own process group
/// (so launchd's clean-up of this process's group does not take it along) and returns at once;
/// the caller then exits.
pub fn spawn_relaunch(plan: &Relaunch) -> Result<String, String> {
    use std::os::unix::process::CommandExt as _;
    use std::process::{Command, Stdio};
    match plan {
        Relaunch::Kickstart { target } => {
            let o = Command::new("/bin/launchctl")
                .args(["kickstart", "-k", target])
                .process_group(0)
                .stdin(Stdio::null())
                .output()
                .map_err(|e| format!("launchctl: {e}"))?;
            if o.status.success() {
                Ok(format!("launchctl kickstart -k {target}"))
            } else {
                Err(format!("launchctl kickstart -k {target}: {} {}", o.status, String::from_utf8_lossy(&o.stderr).trim()))
            }
        }
        _ => {
            let cmd = helper_command(plan).unwrap_or_default();
            Command::new("/bin/sh")
                .arg("-c")
                .arg(HELPER_SCRIPT)
                .arg("ztab-restart")
                .arg(std::process::id().to_string())
                .args(&cmd)
                .env_remove(crate::settings::ENV_SETTINGS_DIR)
                .process_group(0)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map(|c| format!("helper pid {} will run: {}", c.id(), cmd.join(" ")))
                .map_err(|e| format!("helper: {e}"))
        }
    }
}

/// A restart's work off the main thread (the restart thread of `main`'s `restart_self`, or the
/// watchdog of a stuck main thread): start the relaunch for `xpc` / `exe` / `args`, then `exit`.
/// Through launchd (`Kickstart`) launchd stops this process; if it still runs 5 s later, or
/// launchctl failed, the helper relaunches it directly. `who` prefixes the log lines; `flush`
/// writes the log out before anything can end this process. `Ok`: `exit` was called (it may
/// return, e.g. when it only asks the main thread to exit). `Err`: no restart could be started.
#[allow(clippy::too_many_arguments)]
pub fn relaunch_then_exit(
    xpc: Option<&str>,
    uid: u32,
    exe: &Path,
    args: &[String],
    who: &str,
    log: fn(&str),
    flush: &dyn Fn(),
    exit: &dyn Fn(),
) -> Result<(), String> {
    let plan = relaunch_plan(xpc, uid, exe, args);
    log(&format!("{who}: restart plan {plan:?}"));
    flush();
    let kickstart = matches!(plan, Relaunch::Kickstart { .. });
    let mut failed = String::new();
    match spawn_relaunch(&plan) {
        Ok(what) if kickstart => {
            log(&format!("{who}: {what}: launchd stops this ZTab and starts it again"));
            flush();
            std::thread::sleep(Duration::from_secs(5));
            log(&format!("{who}: still running 5 s after launchctl kickstart; restarting it directly"));
        }
        Ok(what) => {
            log(&format!("{who}: {what}; exiting"));
            flush();
            exit();
            return Ok(());
        }
        Err(e) => {
            log(&format!("{who}: restart failed: {e}"));
            failed = e;
        }
    }
    if kickstart {
        match spawn_relaunch(&relaunch_plan(None, uid, exe, args)) {
            Ok(what) => {
                log(&format!("{who}: {what}; exiting"));
                flush();
                exit();
                return Ok(());
            }
            Err(e) => {
                log(&format!("{who}: restart failed: {e}"));
                failed = e;
            }
        }
    }
    Err(failed)
}

/// The start of the health-marker reason of a restart for a stuck main thread.
pub const WEDGE_REASON: &str = "its main thread was stuck for";

pub fn wedge_reason(blocked: Duration) -> String {
    format!("{WEDGE_REASON} {:.0} s", blocked.as_secs_f64())
}

pub fn is_wedge_reason(reason: &str) -> bool {
    reason.starts_with(WEDGE_REASON)
}

/// The binary a restart for a stuck main thread starts: `exe`, or `fallback` when `exe` is gone
/// and the fallback exists.
pub fn wedge_target(exe: &Path, fallback: Option<&Path>) -> PathBuf {
    match fallback {
        Some(f) if !exe.exists() && f.exists() => f.to_path_buf(),
        _ => exe.to_path_buf(),
    }
}

/// What restarting a ZTab whose main thread is stuck needs, captured on the main thread at
/// launch (`main`'s stall watchdog; the self-test's child).
pub struct WedgeRestart {
    pub xpc: Option<String>,
    pub exe: PathBuf,
    /// Started instead when `exe` is gone by then (an upgrade removed it, e.g. `brew upgrade`
    /// deleting the old keg): the path ZTab is opened by (`settings::current_program`, a Homebrew
    /// keg by its stable `opt` link).
    pub fallback_exe: Option<PathBuf>,
    pub args: Vec<String>,
    /// The health marker: the restarted ZTab shows its status, saying why it restarted.
    pub marker: PathBuf,
    pub log: fn(&str),
    pub flush: fn(),
}

/// The main thread has been stuck in one unit of work for `blocked` (the stall watcher saw it, and
/// saw it again a moment later): from the watcher's thread, write the health marker, start the
/// relaunch (see [`relaunch_then_exit`]) and end this process with `_exit` (nothing on the main
/// thread runs any more, so neither `exit` nor the main queue can be used). Returns only when no
/// restart could be started (the reason).
pub fn wedge_restart(w: &WedgeRestart, blocked: Duration, now_unix: f64) -> String {
    (w.log)(&format!(
        "wedge: the main thread has been stuck in one unit of work for {:.1} s: restarting ZTab from the stall watcher's thread",
        blocked.as_secs_f64()
    ));
    if let Err(e) = crate::health::write_marker(&w.marker, now_unix, &wedge_reason(blocked)) {
        (w.log)(&format!("wedge: could not write the restart marker {}: {e}", w.marker.display()));
    }
    // SAFETY: ends the process at once; the log was flushed.
    let exit = || {
        (w.flush)();
        unsafe { libc::_exit(0) }
    };
    let flush = || (w.flush)();
    // SAFETY: plain system call.
    let uid = unsafe { libc::getuid() };
    let exe = wedge_target(&w.exe, w.fallback_exe.as_deref());
    if exe != w.exe {
        (w.log)(&format!("wedge: {} is gone; starting {} instead", w.exe.display(), exe.display()));
    }
    match relaunch_then_exit(w.xpc.as_deref(), uid, &exe, &w.args, "wedge", w.log, &flush, &exit) {
        Ok(()) => String::new(),
        Err(e) => {
            (w.log)(&format!("wedge: could not restart ({e}); this ZTab stays as it is"));
            let _ = std::fs::remove_file(&w.marker);
            e
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Reopen marker
// ---------------------------------------------------------------------------------------------

/// A setup restart older than this does not reopen the window.
pub const MARKER_MAX_AGE_S: f64 = 120.0;

pub fn marker_path() -> PathBuf {
    user_temp_dir().join(format!("{AGENT_LABEL}.setup-reopen"))
}

/// What a fresh marker tells the new ZTab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reopen {
    /// The old ZTab had seen Screen Recording granted when it restarted.
    pub screen_seen: bool,
}

/// The marker's content: the unix time it was written and `screen=0|1` (see [`Reopen`]). A
/// marker without the second field counts as `screen=1` (no automatic restart: the safe side).
pub fn parse_marker(text: &str, now_unix: f64) -> Option<Reopen> {
    let mut it = text.split_whitespace();
    let t: f64 = it.next()?.parse().ok()?;
    if now_unix < t - 5.0 || now_unix - t > MARKER_MAX_AGE_S {
        return None;
    }
    let screen_seen = it.find_map(|f| f.strip_prefix("screen=")).is_none_or(|v| v != "0");
    Some(Reopen { screen_seen })
}

/// Before a setup restart (disk work: never on the main thread).
pub fn write_marker(path: &Path, now_unix: f64, screen_seen: bool) -> std::io::Result<()> {
    std::fs::write(path, format!("{now_unix:.3} screen={}\n", u8::from(screen_seen)))
}

/// At launch: the fresh marker's content, if there is one; it is removed either way.
pub fn take_marker(path: &Path, now_unix: f64) -> Option<Reopen> {
    let text = std::fs::read_to_string(path).ok()?;
    let _ = std::fs::remove_file(path);
    parse_marker(&text, now_unix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wedge_restart_starts_the_opened_path_when_its_binary_is_gone() {
        let d = std::env::temp_dir().join(format!("zt-wt-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        let (old, stable) = (d.join("old-keg-ztab"), d.join("opt-ztab"));
        std::fs::write(&old, b"x").unwrap();
        std::fs::write(&stable, b"y").unwrap();
        assert_eq!(wedge_target(&old, Some(&stable)), old, "its own binary while it exists");
        assert_eq!(wedge_target(&old, None), old);
        std::fs::remove_file(&old).unwrap();
        assert_eq!(wedge_target(&old, Some(&stable)), stable, "removed by an upgrade: the opened path");
        std::fs::remove_file(&stable).unwrap();
        assert_eq!(wedge_target(&old, Some(&stable)), old, "no better choice");
        assert!(is_wedge_reason(&wedge_reason(Duration::from_millis(15400))) && wedge_reason(Duration::from_secs(15)).ends_with("15 s"));
        assert!(!is_wedge_reason("⌥Tab's keyboard hook does not respond"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn launch_rules() {
        // Accessibility missing: always, whatever was skipped.
        assert!(launch_reason(false, true, true, false, false).is_some());
        assert!(launch_reason(false, false, true, false, false).is_some());
        // Screen Recording missing: unless skipped.
        assert_eq!(launch_reason(true, false, false, false, false), Some("Screen Recording is not granted"));
        assert_eq!(launch_reason(true, false, true, false, false), None);
        // All granted: only for the flag or after a setup restart.
        assert_eq!(launch_reason(true, true, false, false, false), None);
        assert_eq!(launch_reason(true, true, false, true, false), Some("--setup-window"));
        assert_eq!(launch_reason(true, true, true, false, true), Some("reopened after a setup restart"));
    }

    #[test]
    fn primary_button() {
        assert_eq!(primary(true, true), Primary::Done);
        assert_eq!(primary(true, false), Primary::SkipThumbnails);
        assert_eq!(primary(false, true), Primary::Later);
        assert_eq!(primary(false, false), Primary::Later);
        assert_eq!(Primary::SkipThumbnails.title(), "Skip Thumbnails");
    }

    #[test]
    fn one_thing_per_click() {
        use Perm::{Accessibility as Ax, ScreenRecording as Sr};
        for p in [Ax, Sr] {
            assert_eq!(grant_action(p, 0, false), GrantAction::Request);
            assert_eq!(grant_action(p, 1, false), GrantAction::OpenPane);
            assert_eq!(grant_action(p, 7, false), GrantAction::OpenPane);
        }
        // Screen Recording asked for in an earlier run: macOS would show nothing, so the first
        // click of this run opens the page. Accessibility is asked again (its dialog shows).
        assert_eq!(grant_action(Sr, 0, true), GrantAction::OpenPane);
        assert_eq!(grant_action(Ax, 0, true), GrantAction::Request);
        assert_eq!(grant_title(0), "Grant…");
        assert_eq!(grant_title(1), "Open System Settings…");
        assert!(Perm::Accessibility.pane_url().ends_with("?Privacy_Accessibility"));
        assert!(Perm::ScreenRecording.pane_url().ends_with("?Privacy_ScreenCapture"));
    }

    fn polled(ax: bool, here: bool, child: Option<bool>) -> Polled {
        Polled { ax, screen_here: here, screen_child: child, ax_ms: 0.1, screen_ms: 4.0, child_ms: None }
    }

    #[test]
    fn screen_state_and_restart() {
        assert!(polled(true, false, Some(true)).screen());
        assert!(!polled(true, false, Some(false)).screen());
        assert!(!polled(true, false, None).screen());
        assert_eq!(screen_row(true, true), ScreenRow::Granted);
        assert_eq!(screen_row(true, false), ScreenRow::GrantedRestartNeeded);
        assert_eq!(screen_row(false, true), ScreenRow::NotGranted);
        // Granted after launch (the child sees it, this process may not): restart once.
        assert!(should_auto_restart(&polled(true, false, Some(true)), false, false));
        assert!(should_auto_restart(&polled(false, true, None), false, false));
        // Granted at launch: nothing to do. A restart made after the grant was seen: no second
        // restart.
        assert!(!should_auto_restart(&polled(true, true, None), true, false));
        assert!(!should_auto_restart(&polled(true, false, Some(true)), false, true));
        assert!(!should_auto_restart(&polled(true, false, Some(false)), false, false));
        // A setup restart for Accessibility (or Restart ZTab clicked before the Screen Recording
        // grant; marker screen=0), then Screen Recording granted: it restarts automatically.
        let reopen = parse_marker("1000.000 screen=0\n", 1001.0).unwrap();
        assert!(should_auto_restart(&polled(true, false, Some(true)), false, reopen.screen_seen));
        let reopen = parse_marker("1000.000 screen=1\n", 1001.0).unwrap();
        assert!(!should_auto_restart(&polled(true, false, Some(true)), false, reopen.screen_seen));
        // Closing the window: Skip only from a check of this opening with Accessibility granted
        // and Screen Recording not.
        assert!(skip_on_close(Some(&polled(true, false, Some(false)))));
        assert!(!skip_on_close(Some(&polled(true, false, Some(true)))));
        assert!(!skip_on_close(Some(&polled(true, true, None))));
        assert!(!skip_on_close(Some(&polled(false, false, Some(false)))));
        assert!(!skip_on_close(None));
        // Timings do not count as a change, nor whether the child was asked when both agree.
        let mut a = polled(true, true, None);
        let b = a;
        a.ax_ms = 9.0;
        assert!(a.same_state(&b));
        assert!(a.same_state(&polled(true, true, Some(true))));
        assert!(polled(true, false, None).same_state(&polled(true, false, Some(false))));
        assert!(!a.same_state(&polled(false, true, None)));
        // The child sees a grant this process does not: a change.
        assert!(!polled(true, false, None).same_state(&polled(true, false, Some(true))));
    }

    #[test]
    fn relaunch_plans() {
        let exe = Path::new("/Applications/ZTab.app/Contents/MacOS/ztab");
        let args = vec!["--settings-dir".to_string(), "/tmp/a b".to_string()];
        assert_eq!(
            relaunch_plan(Some("dev.ztab.app"), 501, exe, &args),
            Relaunch::Kickstart { target: "gui/501/dev.ztab.app".into() }
        );
        // Opened through LaunchServices (or from a terminal): the same bundle, the same arguments.
        let p = relaunch_plan(Some("application.dev.ztab.app.123.456"), 501, exe, &args);
        assert_eq!(p, Relaunch::Open { bundle: "/Applications/ZTab.app".into(), args: args.clone() });
        assert_eq!(
            helper_command(&p).unwrap(),
            vec!["/usr/bin/open", "-n", "-g", "/Applications/ZTab.app", "--args", "--settings-dir", "/tmp/a b"]
        );
        let p = relaunch_plan(None, 501, exe, &[]);
        assert_eq!(helper_command(&p).unwrap(), vec!["/usr/bin/open", "-n", "-g", "/Applications/ZTab.app"]);
        // A bare binary runs again directly.
        let p = relaunch_plan(None, 501, Path::new("/src/ztab/target/release/ztab"), &args);
        assert_eq!(helper_command(&p).unwrap(), vec!["/src/ztab/target/release/ztab", "--settings-dir", "/tmp/a b"]);
        assert_eq!(helper_command(&Relaunch::Kickstart { target: "x".into() }), None);
        assert_eq!(bundle_of(Path::new("/x/Y.app/Contents/MacOS/y")), Some(PathBuf::from("/x/Y.app")));
        assert_eq!(bundle_of(Path::new("/x/Y/Contents/MacOS/y")), None);
        assert_eq!(bundle_of(Path::new("/x/Y.app/Contents/Resources/y")), None);
        assert_eq!(bundle_of(Path::new("y")), None);
    }

    #[test]
    fn helper_waits_for_the_process_then_runs_the_command() {
        // A process that lives 0.3 s; the helper runs `touch` only after it has exited.
        let dir = std::env::temp_dir().join(format!("ztab-test-helper-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut sleeper = std::process::Command::new("/bin/sleep").arg("0.3").spawn().unwrap();
        let done = dir.join("done");
        let t = Instant::now();
        let st = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(HELPER_SCRIPT)
            .arg("ztab-restart")
            .arg(sleeper.id().to_string())
            .arg("/usr/bin/touch")
            .arg(&done)
            .spawn()
            .unwrap();
        // Reap the sleeper when it exits, so `kill -0` stops finding it.
        let _ = sleeper.wait();
        let mut st = st;
        assert!(st.wait().unwrap().success());
        assert!(done.exists());
        assert!(t.elapsed() >= Duration::from_millis(300), "{:?}", t.elapsed());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn marker() {
        let seen = |s: bool| Some(Reopen { screen_seen: s });
        assert_eq!(parse_marker("1000.0 screen=0\n", 1010.0), seen(false));
        assert_eq!(parse_marker("1000.0 screen=1\n", 1010.0), seen(true));
        // Without the field (or garbled): the safe side, no automatic restart.
        assert_eq!(parse_marker("1000.0\n", 1010.0), seen(true));
        assert_eq!(parse_marker("1000.0 screen=x", 1010.0), seen(true));
        assert_eq!(parse_marker("1000.0", 1000.0 + MARKER_MAX_AGE_S + 1.0), None);
        assert_eq!(parse_marker("1000.0", 900.0), None);
        assert_eq!(parse_marker("garbage", 1000.0), None);
        assert_eq!(parse_marker("", 1000.0), None);
        let dir = std::env::temp_dir().join(format!("ztab-test-marker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("m");
        assert_eq!(take_marker(&p, 1000.0), None);
        write_marker(&p, 1000.0, false).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "1000.000 screen=0\n");
        assert_eq!(take_marker(&p, 1001.0), seen(false));
        assert!(!p.exists(), "taken once");
        assert_eq!(take_marker(&p, 1001.0), None);
        write_marker(&p, 1000.0, true).unwrap();
        assert_eq!(take_marker(&p, 1001.0), seen(true));
        write_marker(&p, 1000.0, true).unwrap();
        assert_eq!(take_marker(&p, 2000.0), None);
        assert!(!p.exists(), "a stale marker is removed too");
        assert!(marker_path().ends_with("dev.ztab.app.setup-reopen"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
