//! Opening ZTab while it runs: one healthy ZTab, and something to see.
//!
//! Every ZTab that holds the single-instance lock (an exclusive `flock` on `dev.ztab.app.lock` in
//! the user's temporary directory) also listens on a Unix socket next to it
//! (`dev.ztab.app.sock`, mode 0600 in a directory only this user can enter; every connection is
//! checked with `getpeereid` on both sides). A ZTab that starts and finds the lock taken is a
//! *second process*:
//!
//! * started by launchd (the login agent: `$XPC_SERVICE_NAME` is its label) or by a script (any
//!   argument of its own, or `--background`): it exits at once and asks nothing, as before;
//! * opened by hand (Finder, Spotlight, Launchpad, `open`: no arguments): it asks the running ZTab
//!   ([`ask`]). The listener thread hands the request to the running ZTab's main thread and
//!   answers only once the main thread has taken it, so a wedged main thread does not answer.
//!   Within [`LOGIN_QUIET`] of the user's login such a launch may be a login item rather than the
//!   user ([`LaunchKind::AtLogin`]): it asks all the same, but *quietly* (the running ZTab shows a
//!   window only when its check finds something wrong or fixed something) and never makes another
//!   binary quit; an unresponsive one is replaced as below.
//!   Then [`decide_second`]:
//!   - the same binary answered: it runs its health check and shows the result; this process exits;
//!   - another binary answered (another copy, another version, or the file at its path was
//!     replaced since it started): it quits by itself, and this process takes over (continues its
//!     own launch);
//!   - no answer within about 2 s (hung, wedged main thread, or a ZTab older than this protocol):
//!     this process stops the lock holder ([`stop_holder`]: SIGTERM, then SIGKILL after a grace
//!     period; only a process of this user named `ztab` that has the lock file open) and takes
//!     over. A holder younger than [`YOUNG_HOLDER`] is still starting: the process asks again
//!     instead.
//!
//! The lock's and the socket's files live in the temporary directory, which macOS cleans up. The
//! running ZTab checks both every [`SELF_CHECK_EVERY`] (and in every health check), lock first
//! ([`ipc_action`]): it re-creates a removed file while the lock is still its own, and when another
//! ZTab took the lock on a new file meanwhile, it leaves the socket to that one and quits once that
//! one answers ([`yield_to_holder`]), so two ZTabs never keep taking the socket from each other.
//!
//! `--takeover-dry-run` (or `ZTAB_TAKEOVER=dry-run`) makes a second process report what it would
//! do and exit, stopping nothing. `ztab --reopen-check` does the same as a command (it never takes
//! the lock), and `ztab --reopen-selftest <scratch-dir>` runs the whole exchange against child
//! processes in a scratch directory (see [`selftest`]).

use std::fs::File;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::settings::{AGENT_LABEL, user_temp_dir};

/// First word of every line of the protocol.
pub const PROTO: &str = "ZTAB1";
/// How long a second process waits for the running ZTab (connecting, then its answer).
pub const ANSWER_WAIT: Duration = Duration::from_secs(2);
/// How long the listener waits for the main thread before answering `stalled`.
pub const MAIN_ACK_WAIT: Duration = Duration::from_millis(1500);
/// A lock holder younger than this that does not answer is still starting: ask again, never stop it.
pub const YOUNG_HOLDER: Duration = Duration::from_secs(5);
/// How long a second process keeps asking a young holder before it gives up.
pub const WAIT_YOUNG_MAX: Duration = Duration::from_secs(12);
/// After SIGTERM, and after SIGKILL: how long the holder gets to exit.
pub const TERM_GRACE: Duration = Duration::from_secs(2);
pub const KILL_GRACE: Duration = Duration::from_secs(2);
/// A holder that said it quits gets this long before it is stopped.
pub const QUIT_GRACE: Duration = Duration::from_secs(3);
/// The listener checks its socket and the instance lock this often (their files live in the
/// temporary directory, which macOS cleans up).
pub const SELF_CHECK_EVERY: Duration = Duration::from_secs(600);
/// Flags that never make a launch "scripted" (tests). `--just-logged-in` makes this process take
/// its own start as the user's login (see [`console_login_unix`]).
pub const TEST_FLAGS: [&str; 3] = ["--opened-by-hand", "--takeover-dry-run", "--just-logged-in"];
/// Test flags that take a value.
pub const TEST_FLAGS_WITH_VALUE: [&str; 1] = ["--inject-health"];

pub type Log = fn(&str);

// ---------------------------------------------------------------------------------------------
// Where: the lock and the socket
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ipc {
    pub lock: PathBuf,
    pub socket: PathBuf,
}

impl Ipc {
    /// The user's: in the per-user temporary directory (`settings::lock_path` is the same lock).
    pub fn user() -> Ipc {
        Ipc::in_dir(&user_temp_dir())
    }

    /// Both files inside `dir` (tests, the self-test).
    pub fn in_dir(dir: &Path) -> Ipc {
        Ipc { lock: dir.join(format!("{AGENT_LABEL}.lock")), socket: dir.join(format!("{AGENT_LABEL}.sock")) }
    }
}

// ---------------------------------------------------------------------------------------------
// Who: this process's binary, how it was started
// ---------------------------------------------------------------------------------------------

/// This process's binary as it was when the process started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub pid: i32,
    /// The canonical path of the executable.
    pub exe: String,
    /// `<dev>:<inode>:<size>:<mtime>.<ns>` of that file at start: another copy, or a file replaced
    /// at the same path (an upgrade), has another one.
    pub binid: String,
    pub version: String,
    /// The path this binary is opened by (`settings::current_program`: a Homebrew keg by its
    /// stable `opt` path): when another binary is there later, ZTab was upgraded in place.
    pub stable: String,
}

/// The identity of a binary file.
pub fn binid_of(path: &Path) -> String {
    std::fs::metadata(path)
        .map(|m| format!("{}:{}:{}:{}.{:09}", m.dev(), m.ino(), m.size(), m.mtime(), m.mtime_nsec()))
        .unwrap_or_default()
}

/// This process's identity, read once (call it early, before the file can change).
pub fn identity() -> &'static Identity {
    static ID: OnceLock<Identity> = OnceLock::new();
    ID.get_or_init(|| {
        let exe = std::env::current_exe().map(|p| std::fs::canonicalize(&p).unwrap_or(p)).unwrap_or_default();
        Identity {
            pid: std::process::id() as i32,
            binid: binid_of(&exe),
            exe: exe.to_string_lossy().into_owned(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            stable: crate::settings::current_program(),
        }
    })
}

/// Two identities name the same binary (an unknown one never matches).
pub fn same_binary(a: &str, b: &str) -> bool {
    !a.is_empty() && a == b
}

/// The path this process is opened by now holds another binary (an upgrade in place, or a
/// Homebrew `opt` link that points at a newer keg): that path. A missing file is not a new binary.
pub fn replaced_on_disk(id: &Identity) -> Option<String> {
    let now = binid_of(Path::new(&id.stable));
    (!now.is_empty() && !id.binid.is_empty() && now != id.binid).then(|| id.stable.clone())
}

/// How this process was started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchKind {
    /// By the login agent (launchd sets `$XPC_SERVICE_NAME` to its label).
    Launchd,
    /// By a script or a test: arguments of its own, or `--background`.
    Scripted,
    /// Without arguments, but within [`LOGIN_QUIET`] of the user's login: the user, or a login
    /// item (ZTab is never in macOS's own relaunch list: `main` disables relaunch on login, its
    /// login agent starts it). Treated as opened by hand, but quietly: a window only when
    /// something is wrong or was fixed, and another binary that answers is not replaced.
    AtLogin,
    /// By the user: Finder, Spotlight, Launchpad, `open` (no arguments), or `--opened-by-hand`.
    ByHand,
}

/// A launch without arguments this soon after the user logged in counts as [`LaunchKind::AtLogin`],
/// and a reopen event this soon is handled quietly. Short: it only covers login items without
/// the launch event's login-item mark (`main` reads that mark where it can, in
/// `applicationDidFinishLaunching`), and inside it a check still runs and repairs.
pub const LOGIN_QUIET: Duration = Duration::from_secs(30);

/// `secs_since_login`: since the user's console login ([`secs_since_console_login`]).
pub fn launch_kind(xpc_service_name: Option<&str>, args: &[String], secs_since_login: Option<f64>) -> LaunchKind {
    if args.iter().any(|a| a == "--opened-by-hand") {
        return LaunchKind::ByHand;
    }
    if xpc_service_name == Some(AGENT_LABEL) {
        return LaunchKind::Launchd;
    }
    let mut own = false;
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        if TEST_FLAGS_WITH_VALUE.contains(&a.as_str()) {
            it.next();
        } else if !(a.starts_with("-psn_") || TEST_FLAGS.contains(&a.as_str())) {
            own = true;
        }
    }
    if own {
        LaunchKind::Scripted
    } else if just_logged_in(secs_since_login) {
        LaunchKind::AtLogin
    } else {
        LaunchKind::ByHand
    }
}

/// Within [`LOGIN_QUIET`] of the user's login: an argument-free launch, or a reopen event, may be
/// a login item rather than the user (handled quietly).
pub fn just_logged_in(secs_since_login: Option<f64>) -> bool {
    secs_since_login.is_some_and(|s| (0.0..LOGIN_QUIET.as_secs_f64()).contains(&s))
}

/// The user's console login (unix time), read once (at launch). With `--just-logged-in` (tests),
/// this process's start counts as the login.
pub fn console_login_unix() -> Option<f64> {
    static AT: OnceLock<Option<f64>> = OnceLock::new();
    *AT.get_or_init(|| {
        if std::env::args().any(|a| a == "--just-logged-in") {
            return Some(now_unix());
        }
        secs_since_console_login().map(|s| now_unix() - s)
    })
}

/// Seconds since the console login read at launch.
pub fn secs_since_login() -> Option<f64> {
    console_login_unix().map(|t| now_unix() - t)
}

/// Seconds since this user's latest console login (`utmpx`), if there is one.
pub fn secs_since_console_login() -> Option<f64> {
    // SAFETY: getpwuid's result is read at once (launch, one thread).
    let me = unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() {
            return None;
        }
        std::ffi::CStr::from_ptr((*pw).pw_name).to_string_lossy().into_owned()
    };
    let mut latest: Option<f64> = None;
    // SAFETY: the utmpx database API, used from one thread at launch; each entry is read before
    // the next call.
    unsafe {
        libc::setutxent();
        loop {
            let e = libc::getutxent();
            if e.is_null() {
                break;
            }
            let e = &*e;
            let text = |v: &[libc::c_char]| -> String { v.iter().take_while(|&&c| c != 0).map(|&c| c as u8 as char).collect() };
            if e.ut_type == libc::USER_PROCESS && text(&e.ut_line) == "console" && text(&e.ut_user) == me {
                let t = e.ut_tv.tv_sec as f64 + e.ut_tv.tv_usec as f64 / 1e6;
                latest = Some(latest.map_or(t, |l: f64| l.max(t)));
            }
        }
        libc::endutxent();
    }
    latest.map(|t| now_unix() - t)
}

/// `--takeover-dry-run` or `ZTAB_TAKEOVER=dry-run`: report, stop nothing.
pub fn dry_run(args: &[String], env: Option<&str>) -> bool {
    args.iter().any(|a| a == "--takeover-dry-run") || env == Some("dry-run")
}

// ---------------------------------------------------------------------------------------------
// The protocol: one request line, one answer line
// ---------------------------------------------------------------------------------------------

/// Whether the running ZTab may quit for a different binary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Takeover {
    Yes,
    /// It stays, and runs its check as for the same binary: a launch right after login, or a ZTab
    /// that lost its lock file handing over to the one that holds the lock now.
    No,
    /// It stays and does nothing (tests).
    DryRun,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub pid: i32,
    pub exe: String,
    pub binid: String,
    pub version: String,
    pub takeover: Takeover,
    /// `ztab --reopen-check`: answer, but do nothing (no status window, never quit).
    pub probe: bool,
    /// Check, but show a window only when something is wrong or was fixed (a launch right after
    /// login, which may be a login item rather than the user).
    pub quiet: bool,
}

/// What the running ZTab does about a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// The same binary (or `Takeover::No`): it runs its health check and shows the result.
    Show,
    /// A different binary was opened: it quits, so that one takes over.
    Quitting,
    /// A different binary, but a dry run: it stays.
    Staying,
}

impl Status {
    fn word(self) -> &'static str {
        match self {
            Status::Show => "show",
            Status::Quitting => "quitting",
            Status::Staying => "staying",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    pub pid: i32,
    pub exe: String,
    pub binid: String,
    pub version: String,
    pub status: Status,
    /// How long the request waited for the main thread (ms).
    pub main_ms: f64,
    /// A switcher session was open: the health check waits until it ends.
    pub deferred: bool,
}

/// The running ZTab's decision about a request (its own binary: `own_binid`).
pub fn server_status(own_binid: &str, req: &Request) -> Status {
    if same_binary(own_binid, &req.binid) {
        return Status::Show;
    }
    match req.takeover {
        Takeover::Yes => Status::Quitting,
        Takeover::No => Status::Show,
        Takeover::DryRun => Status::Staying,
    }
}

/// Percent-encode everything but a few safe characters (paths have spaces).
pub fn enc(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"/._-+:,".contains(&b) {
            o.push(b as char);
        } else {
            o += &format!("%{b:02X}");
        }
    }
    if o.is_empty() { "%".into() } else { o }
}

pub fn dec(s: &str) -> String {
    if s == "%" {
        return String::new();
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn fields(line: &str) -> Vec<(&str, &str)> {
    line.split_whitespace().filter_map(|f| f.split_once('=')).collect()
}

fn field<'a>(f: &[(&'a str, &'a str)], k: &str) -> Option<&'a str> {
    f.iter().find(|(key, _)| *key == k).map(|(_, v)| *v)
}

impl Request {
    pub fn line(&self) -> String {
        format!(
            "{PROTO} reopen pid={} version={} binid={} takeover={} probe={} quiet={} exe={}\n",
            self.pid,
            enc(&self.version),
            enc(&self.binid),
            match self.takeover {
                Takeover::Yes => "yes",
                Takeover::No => "no",
                Takeover::DryRun => "dry-run",
            },
            u8::from(self.probe),
            u8::from(self.quiet),
            enc(&self.exe)
        )
    }

    pub fn parse(line: &str) -> Option<Request> {
        let rest = line.trim().strip_prefix(PROTO)?.trim_start().strip_prefix("reopen ")?;
        let f = fields(rest);
        Some(Request {
            pid: field(&f, "pid")?.parse().ok()?,
            version: dec(field(&f, "version")?),
            binid: dec(field(&f, "binid")?),
            takeover: match field(&f, "takeover")? {
                "yes" => Takeover::Yes,
                "no" => Takeover::No,
                "dry-run" => Takeover::DryRun,
                _ => return None,
            },
            probe: field(&f, "probe") == Some("1"),
            quiet: field(&f, "quiet") == Some("1"),
            exe: dec(field(&f, "exe")?),
        })
    }
}

impl Reply {
    pub fn line(&self) -> String {
        format!(
            "{PROTO} reply status={} pid={} version={} binid={} main_ms={:.1} deferred={} exe={}\n",
            self.status.word(),
            self.pid,
            enc(&self.version),
            enc(&self.binid),
            self.main_ms,
            u8::from(self.deferred),
            enc(&self.exe)
        )
    }
}

/// What a second process learnt from the running ZTab.
#[derive(Clone, Debug, PartialEq)]
pub enum Answer {
    Replied(Reply),
    /// The listener answered for a main thread that did not take the request within
    /// [`MAIN_ACK_WAIT`] (wedged).
    Stalled { pid: Option<i32>, waited_ms: u64 },
    /// Connected, but no answer in time (the whole process is stopped or hung). `pid`: the
    /// socket's owner (`LOCAL_PEERPID`).
    NoReply { pid: Option<i32> },
    /// Nobody listens: a ZTab older than this protocol, or one that is hung or still starting.
    NoListener,
    /// The socket belongs to another user (never acted on).
    Rejected { uid: u32 },
}

impl Answer {
    pub fn parse(line: &str, peer: Option<i32>) -> Option<Answer> {
        let rest = line.trim().strip_prefix(PROTO)?.trim_start();
        if let Some(r) = rest.strip_prefix("stalled") {
            let f = fields(r);
            return Some(Answer::Stalled {
                pid: field(&f, "pid").and_then(|v| v.parse().ok()).or(peer),
                waited_ms: field(&f, "waited_ms").and_then(|v| v.parse().ok()).unwrap_or(0),
            });
        }
        let f = fields(rest.strip_prefix("reply ")?);
        Some(Answer::Replied(Reply {
            status: match field(&f, "status")? {
                "show" => Status::Show,
                "quitting" => Status::Quitting,
                "staying" => Status::Staying,
                _ => return None,
            },
            pid: field(&f, "pid")?.parse().ok()?,
            version: dec(field(&f, "version")?),
            binid: dec(field(&f, "binid")?),
            main_ms: field(&f, "main_ms").and_then(|v| v.parse().ok()).unwrap_or(0.0),
            deferred: field(&f, "deferred") == Some("1"),
            exe: dec(field(&f, "exe")?),
        }))
    }

    pub fn describe(&self) -> String {
        match self {
            Answer::Replied(r) => format!(
                "pid {} answered {} (ZTab {} at {}; main thread took it in {:.1} ms{})",
                r.pid,
                r.status.word(),
                r.version,
                r.exe,
                r.main_ms,
                if r.deferred { "; a switcher session is open: its check waits" } else { "" }
            ),
            Answer::Stalled { pid, waited_ms } => {
                format!("pid {} did not answer: its main thread did not take the request within {waited_ms} ms", pid_str(*pid))
            }
            Answer::NoReply { pid } => format!("pid {} accepted the connection but did not answer within {} s", pid_str(*pid), ANSWER_WAIT.as_secs()),
            Answer::NoListener => format!("nobody answers on the socket within {} s", ANSWER_WAIT.as_secs()),
            Answer::Rejected { uid } => format!("the socket belongs to uid {uid}, not this user: ignored"),
        }
    }
}

fn pid_str(p: Option<i32>) -> String {
    p.map(|p| p.to_string()).unwrap_or_else(|| "?".into())
}

// ---------------------------------------------------------------------------------------------
// The decision of a second process (pure)
// ---------------------------------------------------------------------------------------------

/// The process holding the instance lock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holder {
    pub pid: i32,
    pub age: Duration,
    pub exe: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SecondAction {
    /// Started by launchd or a script: ask nothing, change nothing, exit.
    Quiet,
    /// The running ZTab shows its status: exit.
    Shown,
    /// The holder is still starting: ask again shortly.
    Wait,
    /// Replace the holder: it quits by itself (`graceful`), or is stopped (SIGTERM, then SIGKILL).
    TakeOver { pid: i32, graceful: bool, why: String },
    /// Exit without changing anything: what a takeover would have done (dry run), or why none is
    /// possible.
    Report(String),
}

/// What a second process does (it found the lock taken): `answer` from [`ask`], `holder` the
/// process holding the lock (from the socket's owner or the lock file's openers), `dry_run`,
/// and whether it already stopped a holder once (it never stops a second one). Started by
/// launchd or a script: nothing. Opened by hand, or right after login ([`LaunchKind::AtLogin`],
/// which asks with [`Takeover::No`], so another binary answers `show`): the answer decides, and a
/// holder that does not answer is replaced either way.
pub fn decide_second(kind: LaunchKind, answer: &Answer, holder: Option<&Holder>, dry_run: bool, stopped_once: bool) -> SecondAction {
    if !asks(kind) {
        return SecondAction::Quiet;
    }
    let unresponsive = match answer {
        Answer::Replied(r) => {
            return match r.status {
                Status::Show => SecondAction::Shown,
                Status::Staying => SecondAction::Report(format!(
                    "dry run: ZTab {} (pid {}, {}) is another binary and would quit; it stays",
                    r.version, r.pid, r.exe
                )),
                Status::Quitting if dry_run => SecondAction::Report(format!("dry run: pid {} said it quits", r.pid)),
                Status::Quitting => SecondAction::TakeOver {
                    pid: r.pid,
                    graceful: true,
                    why: format!("another binary is running (ZTab {} at {}, pid {})", r.version, r.exe, r.pid),
                },
            };
        }
        Answer::Rejected { .. } => return SecondAction::Report(answer.describe()),
        Answer::Stalled { .. } => "its main thread does not respond",
        Answer::NoReply { .. } => "it does not respond",
        Answer::NoListener => "it does not answer (a ZTab older than 0.2.4, or one that is hung)",
    };
    let Some(h) = holder else {
        return SecondAction::Report(format!("{}; could not find the process that holds the instance lock: leaving it", answer.describe()));
    };
    if h.age < YOUNG_HOLDER {
        return SecondAction::Wait;
    }
    if stopped_once {
        return SecondAction::Report(format!("pid {} {unresponsive}, and a ZTab was stopped already: giving up", h.pid));
    }
    let why = format!("pid {} ({}) {unresponsive}", h.pid, h.exe);
    if dry_run {
        return SecondAction::Report(format!(
            "dry run: would stop {why} with SIGTERM, then SIGKILL after {} s, and take over",
            TERM_GRACE.as_secs()
        ));
    }
    SecondAction::TakeOver { pid: h.pid, graceful: false, why }
}

/// Whether a second process of this kind asks the running ZTab (and may replace it).
pub fn asks(kind: LaunchKind) -> bool {
    matches!(kind, LaunchKind::ByHand | LaunchKind::AtLogin)
}

// ---------------------------------------------------------------------------------------------
// Processes
// ---------------------------------------------------------------------------------------------

/// Facts about a process (`proc_pidinfo`); `None` when it does not exist (or is a zombie).
#[derive(Clone, Debug, PartialEq)]
pub struct ProcInfo {
    pub uid: u32,
    pub name: String,
    pub start_unix: f64,
    pub exe: String,
}

pub fn proc_info(pid: i32) -> Option<ProcInfo> {
    if pid <= 0 {
        return None;
    }
    // SAFETY: proc_bsdinfo is plain old data; proc_pidinfo fills at most `size` bytes of it.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    if n != size || info.pbi_status == libc::SZOMB {
        return None;
    }
    let name: Vec<u8> = info.pbi_comm.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the buffer is as large as we say.
    let k = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    buf.truncate(k.max(0) as usize);
    Some(ProcInfo {
        uid: info.pbi_uid,
        name: String::from_utf8_lossy(&name).into_owned(),
        start_unix: info.pbi_start_tvsec as f64 + info.pbi_start_tvusec as f64 / 1e6,
        exe: String::from_utf8_lossy(&buf).into_owned(),
    })
}

fn now_unix() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn my_uid() -> u32 {
    // SAFETY: plain system call.
    unsafe { libc::getuid() }
}

unsafe extern "C" {
    /// libproc.h: the processes that have `path` open.
    fn proc_listpidspath(kind: u32, typeinfo: u32, path: *const libc::c_char, pathflags: u32, buffer: *mut libc::c_void, buffersize: libc::c_int)
    -> libc::c_int;
}
const PROC_UID_ONLY: u32 = 4;

/// This user's processes (but this one) that have `path` open.
pub fn openers(path: &Path) -> Vec<i32> {
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else { return vec![] };
    let mut buf = vec![0i32; 256];
    // SAFETY: valid C string; the buffer holds `buf.len()` ints.
    let n = unsafe {
        proc_listpidspath(PROC_UID_ONLY, my_uid(), c.as_ptr(), 0, buf.as_mut_ptr().cast(), (buf.len() * size_of::<i32>()) as libc::c_int)
    };
    let me = std::process::id() as i32;
    buf.truncate((n.max(0) as usize) / size_of::<i32>());
    buf.retain(|&p| p > 0 && p != me);
    buf.sort_unstable();
    buf.dedup();
    buf
}

/// A process that may be stopped as the lock holder: this user's, named `ztab`, not this one.
fn stoppable(pid: i32, p: &ProcInfo) -> bool {
    pid > 1 && pid != std::process::id() as i32 && p.uid == my_uid() && p.name == "ztab"
}

/// The process holding `ipc.lock`: the socket's owner when it has the lock file open, else the
/// oldest `ztab` of this user that has it open (a second process has it open briefly too).
pub fn find_holder(ipc: &Ipc, peer: Option<i32>) -> Option<Holder> {
    let open = openers(&ipc.lock);
    let now = now_unix();
    let holder = |pid: i32| -> Option<(Holder, f64)> {
        let p = proc_info(pid).filter(|p| stoppable(pid, p))?;
        let age = Duration::from_secs_f64((now - p.start_unix).max(0.0));
        Some((Holder { pid, age, exe: p.exe }, p.start_unix))
    };
    if let Some(p) = peer.filter(|p| open.contains(p))
        && let Some((h, _)) = holder(p)
    {
        return Some(h);
    }
    open.iter().filter_map(|&p| holder(p)).min_by(|a, b| a.1.total_cmp(&b.1)).map(|(h, _)| h)
}

/// How stopping a holder ended.
#[derive(Debug)]
pub enum Stopped {
    /// It exited and this process holds the instance lock now (`how`: what it took).
    Taken { file: File, how: String },
    /// It exited, but another process took the lock first.
    LockTakenByOther(String),
    /// It is still there (or could not be signalled).
    Failed(String),
    /// Refused: not a process this may stop (or a dry run).
    Refused(String),
}

/// Stop the lock holder `pid` and take the instance lock. Checks again, right before signalling,
/// that it is this user's, named `ztab`, and has `ipc.lock` open; never in a dry run. `graceful`:
/// it said it quits; wait [`QUIT_GRACE`] first. Then SIGTERM, [`TERM_GRACE`], SIGKILL,
/// [`KILL_GRACE`].
pub fn stop_holder(ipc: &Ipc, pid: i32, graceful: bool, dry_run: bool, log: Log) -> Stopped {
    if dry_run {
        return Stopped::Refused("dry run".into());
    }
    let t = Instant::now();
    let gone_and_locked = |limit: Duration| -> Option<Stopped> {
        let end = Instant::now() + limit;
        loop {
            let alive = proc_info(pid).is_some();
            if !alive || Instant::now() >= end {
                if alive {
                    return None;
                }
                // Exited: its lock is released (a lock on a file removed meanwhile does not count).
                for _ in 0..20 {
                    match try_lock(&ipc.lock) {
                        Ok(Some(file)) => return Some(Stopped::Taken { file, how: String::new() }),
                        Ok(None) => std::thread::sleep(Duration::from_millis(25)),
                        Err(e) => return Some(Stopped::Failed(format!("pid {pid} exited, but the lock: {e}"))),
                    }
                }
                return Some(Stopped::LockTakenByOther(format!("pid {pid} exited, but another process took the instance lock")));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    };
    let with_how = |s: Stopped, how: String| match s {
        Stopped::Taken { file, .. } => Stopped::Taken { file, how },
        other => other,
    };
    if graceful {
        if let Some(s) = gone_and_locked(QUIT_GRACE) {
            return with_how(s, format!("pid {pid} quit by itself after {:.0} ms", t.elapsed().as_secs_f64() * 1000.0));
        }
        log(&format!("reopen: pid {pid} said it quits but still runs after {} s; stopping it", QUIT_GRACE.as_secs()));
    }
    let check = || -> Result<ProcInfo, String> {
        let p = proc_info(pid).ok_or_else(|| format!("pid {pid} is gone"))?;
        if !stoppable(pid, &p) {
            return Err(format!("pid {pid} is not a ZTab of this user (name {:?}, uid {})", p.name, p.uid));
        }
        if !openers(&ipc.lock).contains(&pid) {
            return Err(format!("pid {pid} does not hold {}", ipc.lock.display()));
        }
        Ok(p)
    };
    for (sig, name, grace) in [(libc::SIGTERM, "SIGTERM", TERM_GRACE), (libc::SIGKILL, "SIGKILL", KILL_GRACE)] {
        match check() {
            Ok(p) => log(&format!("reopen: sending {name} to pid {pid} ({})", p.exe)),
            Err(e) if proc_info(pid).is_none() => {
                if let Some(s) = gone_and_locked(Duration::ZERO) {
                    return with_how(s, format!("pid {pid} exited ({e})"));
                }
                return Stopped::Failed(e);
            }
            Err(e) => return Stopped::Refused(e),
        }
        // SAFETY: plain system call on a pid checked just above.
        if unsafe { libc::kill(pid, sig) } != 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::ESRCH) {
                return Stopped::Failed(format!("{name} to pid {pid}: {e}"));
            }
        }
        if let Some(s) = gone_and_locked(grace) {
            return with_how(s, format!("pid {pid} exited {:.0} ms after {name}", t.elapsed().as_secs_f64() * 1000.0));
        }
        log(&format!("reopen: pid {pid} still runs {} s after {name}", grace.as_secs()));
    }
    Stopped::Failed(format!("pid {pid} still runs after SIGKILL"))
}

// ---------------------------------------------------------------------------------------------
// The instance lock
// ---------------------------------------------------------------------------------------------

/// An exclusive `flock` on `path` (created if missing): `Some(file)` holds it while the file
/// lives, `None` another process holds it.
pub fn try_lock(path: &Path) -> std::io::Result<Option<File>> {
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path)?;
    // SAFETY: plain system call on a valid descriptor.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 { Ok(Some(f)) } else { Ok(None) }
}

/// The lock this process holds (for the life of the process).
static HELD: Mutex<Option<(PathBuf, File)>> = Mutex::new(None);

fn held() -> std::sync::MutexGuard<'static, Option<(PathBuf, File)>> {
    HELD.lock().unwrap_or_else(|e| e.into_inner())
}

/// Take the instance lock and keep it. `true`: taken (or no lock file is possible: never refuse
/// to start over it, as before).
pub fn take_lock(ipc: &Ipc) -> bool {
    match try_lock(&ipc.lock) {
        Ok(Some(f)) => {
            keep_lock(ipc, f);
            true
        }
        Ok(None) => false,
        Err(_) => true,
    }
}

/// Keep `f` (a lock on `ipc.lock` just taken) for the life of the process.
pub fn keep_lock(ipc: &Ipc, f: File) {
    *held() = Some((ipc.lock.clone(), f));
}

/// Give the lock back (before asking an orphaned instance, see `main`).
pub fn release_lock() {
    *held() = None;
}

/// The state of the instance lock this process holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LockCheck {
    /// Held, and its file is still the one at the path.
    Held,
    /// Its file had been removed (or replaced): it was taken again on a new file.
    Retaken,
    /// Its file had been removed and another process holds the new one (pids: its openers).
    OtherHolder(Vec<i32>),
    /// This process holds no lock (a diagnostics mode) or it could not be checked.
    Unknown(String),
}

fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}

/// Check the lock's file is still at its path, and take it again if not (macOS cleans up the
/// temporary directory; a removed lock file would let a second ZTab start).
pub fn check_lock() -> LockCheck {
    let mut g = held();
    let Some((path, f)) = g.as_ref() else { return LockCheck::Unknown("no instance lock held".into()) };
    let path = path.clone();
    match (f.metadata(), std::fs::metadata(&path)) {
        (Ok(a), Ok(b)) if same_file(&a, &b) => LockCheck::Held,
        (Err(e), _) => LockCheck::Unknown(format!("fstat: {e}")),
        _ => match try_lock(&path) {
            Ok(Some(nf)) => {
                *g = Some((path, nf));
                LockCheck::Retaken
            }
            Ok(None) => LockCheck::OtherHolder(openers(&path)),
            Err(e) => LockCheck::Unknown(format!("{}: {e}", path.display())),
        },
    }
}

// ---------------------------------------------------------------------------------------------
// The running ZTab: the listener
// ---------------------------------------------------------------------------------------------

/// Takes a request on the main thread (or wherever it runs it) and answers through the
/// [`Responder`]; the listener waits [`MAIN_ACK_WAIT`] for that.
pub type Handler = Arc<dyn Fn(Request, Responder) + Send + Sync>;

#[derive(Default)]
struct Slot {
    reply: Option<Reply>,
    expired: bool,
}

/// Answers one request (once).
pub struct Responder {
    slot: Arc<(Mutex<Slot>, Condvar)>,
    /// When the listener received the request.
    pub received: Instant,
}

impl Responder {
    /// Answer. `false`: too late, the listener has answered `stalled` already (the second
    /// process takes over: do nothing).
    pub fn reply(self, r: Reply) -> bool {
        let (m, cv) = &*self.slot;
        let mut s = m.lock().unwrap_or_else(|e| e.into_inner());
        if s.expired {
            return false;
        }
        s.reply = Some(r);
        cv.notify_all();
        true
    }
}

/// The socket this process listens on (dev, inode), for the self-checks.
static LISTENING: Mutex<Option<(u64, u64)>> = Mutex::new(None);

/// The state of the socket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SocketCheck {
    Listening,
    /// Its file had been removed (or replaced): listening again on a new one.
    Rebound,
    /// Not rebound: another ZTab holds the instance lock now (see [`ipc_action`]); the socket at
    /// the path is that one's.
    LeftToHolder,
    Failed(String),
}

/// What the self-check does, from the lock's state and whether the socket file at the path is
/// this process's (pure).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IpcAction {
    Keep,
    /// The lock is this process's, the socket file is gone or not its own: listen on a new one.
    Rebind,
    /// Another ZTab holds the lock (on a new file, the old one was removed): never take the socket
    /// from it; this process quits once that one answers ([`yield_to_holder`]).
    Yield(Vec<i32>),
}

pub fn ipc_action(lock: &LockCheck, socket_ours: bool) -> IpcAction {
    match lock {
        LockCheck::OtherHolder(pids) => IpcAction::Yield(pids.clone()),
        _ if socket_ours => IpcAction::Keep,
        _ => IpcAction::Rebind,
    }
}

/// Ends this process (it handed over to another ZTab): `why` for the log.
pub type Quit = fn(&str);

pub struct Server {
    ipc: Ipc,
    handler: Handler,
    log: Log,
    quit: Quit,
    self_check_every: Duration,
}

static SERVER: OnceLock<Arc<Server>> = OnceLock::new();

/// Start listening (call once this process holds the instance lock). A stale socket file (a ZTab
/// that ended without removing it) is replaced. The listener thread also checks the lock and the
/// socket every [`SELF_CHECK_EVERY`] ([`ipc_action`]); `quit` ends this process when another ZTab
/// holds the lock now and answers.
pub fn serve(ipc: &Ipc, handler: Handler, log: Log, quit: Quit) -> Result<(), String> {
    serve_every(ipc, handler, log, quit, SELF_CHECK_EVERY)
}

/// [`serve`] with another self-check interval (the self-test).
pub fn serve_every(ipc: &Ipc, handler: Handler, log: Log, quit: Quit, self_check_every: Duration) -> Result<(), String> {
    let s = Arc::new(Server { ipc: ipc.clone(), handler, log, quit, self_check_every });
    s.bind()?;
    let _ = SERVER.set(s);
    Ok(())
}

impl Server {
    fn bind(self: &Arc<Self>) -> Result<(), String> {
        let path = &self.ipc.socket;
        let _ = std::fs::remove_file(path);
        let l = UnixListener::bind(path).map_err(|e| format!("bind {}: {e}", path.display()))?;
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        let m = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
        *LISTENING.lock().unwrap_or_else(|e| e.into_inner()) = Some((m.dev(), m.ino()));
        let me = self.clone();
        let ino = m.ino();
        std::thread::Builder::new()
            .name("ztab-reopen".into())
            .spawn(move || me.accept_loop(l, ino))
            .map_err(|e| format!("listener thread: {e}"))?;
        Ok(())
    }

    /// The lock first, then the socket ([`ipc_action`]): rebind only while the lock is this
    /// process's.
    fn check(self: &Arc<Self>) -> (LockCheck, SocketCheck) {
        static GUARD: Mutex<()> = Mutex::new(());
        let _g = GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let lock = check_lock();
        let ours = *LISTENING.lock().unwrap_or_else(|e| e.into_inner());
        let socket_ours = match (ours, std::fs::metadata(&self.ipc.socket)) {
            (Some((d, i)), Ok(m)) => m.dev() == d && m.ino() == i,
            _ => false,
        };
        let sock = match ipc_action(&lock, socket_ours) {
            IpcAction::Keep => SocketCheck::Listening,
            IpcAction::Yield(_) => SocketCheck::LeftToHolder,
            IpcAction::Rebind => match self.bind() {
                Ok(()) => SocketCheck::Rebound,
                Err(e) => SocketCheck::Failed(e),
            },
        };
        (lock, sock)
    }

    fn accept_loop(self: Arc<Self>, l: UnixListener, ino: u64) {
        let fd = l.as_raw_fd();
        loop {
            // Another listener replaced this one (rebound): end.
            if LISTENING.lock().unwrap_or_else(|e| e.into_inner()).is_none_or(|(_, i)| i != ino) {
                return;
            }
            let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
            // SAFETY: one valid pollfd.
            let n = unsafe { libc::poll(&mut p, 1, self.self_check_every.as_millis() as i32) };
            if n < 0 {
                // EINTR, or an error: never block in accept() on it.
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            if n == 0 {
                let (lock, sock) = self.check();
                if lock != LockCheck::Held || sock != SocketCheck::Listening {
                    (self.log)(&format!("reopen: self-check: instance lock {lock:?}, socket {sock:?}"));
                }
                if let LockCheck::OtherHolder(pids) = &lock {
                    // Nobody opened ZTab: ask as a probe (the other one shows nothing).
                    match yield_to_holder(&self.ipc, false, false, self.log) {
                        Some(pid) => (self.quit)(&format!(
                            "another ZTab (pid {pid}) holds the instance lock now and answers: quitting so that only it runs"
                        )),
                        None => (self.log)(&format!(
                            "reopen: self-check: another ZTab holds the instance lock now ({pids:?}) but does not answer: staying (opening ZTab by hand replaces it)"
                        )),
                    }
                }
                continue;
            }
            match l.accept() {
                Ok((s, _)) => self.handle(s),
                Err(e) => {
                    (self.log)(&format!("reopen: accept failed: {e}"));
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }

    fn handle(&self, mut s: UnixStream) {
        let received = Instant::now();
        let (uid, pid) = peer(&s);
        if uid != Some(my_uid()) {
            (self.log)(&format!("reopen: refused a connection from uid {uid:?} (pid {pid:?})"));
            return;
        }
        let _ = s.set_read_timeout(Some(Duration::from_secs(1)));
        let _ = s.set_write_timeout(Some(Duration::from_secs(1)));
        let mut line = String::new();
        let _ = BufReader::new((&s).take(4096)).read_line(&mut line);
        let Some(req) = Request::parse(&line) else {
            (self.log)(&format!("reopen: unreadable request from pid {pid:?}: {:?}", line.trim()));
            let _ = s.write_all(format!("{PROTO} error bad-request\n").as_bytes());
            return;
        };
        let slot = Arc::new((Mutex::new(Slot::default()), Condvar::new()));
        (self.handler)(req.clone(), Responder { slot: slot.clone(), received });
        let (m, cv) = &*slot;
        let g = m.lock().unwrap_or_else(|e| e.into_inner());
        let (mut g, _) = cv.wait_timeout_while(g, MAIN_ACK_WAIT, |s| s.reply.is_none()).unwrap_or_else(|e| e.into_inner());
        let out = match g.reply.take() {
            Some(r) => r.line(),
            None => {
                g.expired = true;
                let waited = received.elapsed().as_millis();
                (self.log)(&format!(
                    "reopen: the main thread did not take the request of pid {} within {waited} ms: answering stalled",
                    req.pid
                ));
                format!("{PROTO} stalled pid={} waited_ms={waited}\n", std::process::id())
            }
        };
        drop(g);
        let _ = s.write_all(out.as_bytes());
    }
}

/// Check the lock, then the socket (rebinding it if its file went away while the lock is still
/// this process's; see [`ipc_action`]); for the health check.
pub fn check_ipc() -> (LockCheck, SocketCheck) {
    match SERVER.get() {
        Some(s) => s.check(),
        None => (check_lock(), SocketCheck::Failed("not listening".into())),
    }
}

/// This process lost its lock file and another ZTab holds the lock now: ask that one through the
/// socket (it stays: [`Takeover::No`]). `show`: it runs its health check and shows the result
/// (`quiet`: only if something is wrong), else a probe (it shows nothing). The pid that answered,
/// if one did: then this process should quit.
pub fn yield_to_holder(ipc: &Ipc, show: bool, quiet: bool, log: Log) -> Option<i32> {
    let req = Request { takeover: Takeover::No, probe: !show, quiet, ..request_for(identity(), false) };
    let a = ask(ipc, &req, ANSWER_WAIT);
    log(&format!("reopen: this ZTab lost its instance lock file; asked the one that holds the lock now: {}", a.describe()));
    match a {
        // Its own socket (the holder does not listen: ZTab 0.2.3 or older) is no answer.
        Answer::Replied(r) if r.pid != identity().pid => Some(r.pid),
        _ => None,
    }
}

/// The peer's effective uid (`getpeereid`) and pid (`LOCAL_PEERPID`).
fn peer(s: &UnixStream) -> (Option<u32>, Option<i32>) {
    let fd = s.as_raw_fd();
    let (mut uid, mut gid) = (0u32, 0u32);
    // SAFETY: valid socket, valid out pointers.
    let uid = (unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } == 0).then_some(uid);
    let mut pid: libc::pid_t = 0;
    let mut len = size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: valid socket; the out buffer is a pid_t of `len` bytes.
    let ok = unsafe { libc::getsockopt(fd, libc::SOL_LOCAL, libc::LOCAL_PEERPID, (&raw mut pid).cast(), &mut len) } == 0;
    (uid, (ok && pid > 0).then_some(pid))
}

// ---------------------------------------------------------------------------------------------
// A second process: asking
// ---------------------------------------------------------------------------------------------

/// Whether a ZTab listens on `ipc.socket` right now (one connection attempt).
pub fn someone_listens(ipc: &Ipc) -> bool {
    UnixStream::connect(&ipc.socket).is_ok()
}

/// Ask the running ZTab (connecting retried for up to `wait`, the answer read with at least
/// [`MAIN_ACK_WAIT`] + 0.5 s left).
pub fn ask(ipc: &Ipc, req: &Request, wait: Duration) -> Answer {
    let end = Instant::now() + wait;
    let mut s = loop {
        match UnixStream::connect(&ipc.socket) {
            Ok(s) => break s,
            Err(_) if Instant::now() < end => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => return Answer::NoListener,
        }
    };
    let (uid, pid) = peer(&s);
    if uid != Some(my_uid()) {
        return Answer::Rejected { uid: uid.unwrap_or(u32::MAX) };
    }
    let left = end.saturating_duration_since(Instant::now()).max(MAIN_ACK_WAIT + Duration::from_millis(500));
    let _ = s.set_write_timeout(Some(Duration::from_secs(1)));
    if s.write_all(req.line().as_bytes()).is_err() {
        return Answer::NoReply { pid };
    }
    let _ = s.set_read_timeout(Some(left));
    let mut line = String::new();
    match BufReader::new((&s).take(4096)).read_line(&mut line) {
        Ok(n) if n > 0 => Answer::parse(&line, pid).unwrap_or(Answer::NoReply { pid }),
        _ => Answer::NoReply { pid },
    }
}

/// How a second process ends.
#[derive(Debug)]
pub enum Second {
    /// Exit now (the running ZTab shows its status, or nothing is to be done).
    Exit,
    /// This process took over: it holds the instance lock and continues its launch. The note is
    /// for the status it shows.
    Continue(String),
}

/// The request for this process (opened by hand: the running ZTab shows its status, and quits
/// when it is another binary).
pub fn request_for(id: &Identity, dry_run: bool) -> Request {
    Request {
        pid: id.pid,
        exe: id.exe.clone(),
        binid: id.binid.clone(),
        version: id.version.clone(),
        takeover: if dry_run { Takeover::DryRun } else { Takeover::Yes },
        probe: false,
        quiet: false,
    }
}

/// The request of a second process of `kind`: right after login it is quiet and never makes
/// another binary quit.
pub fn request_for_kind(id: &Identity, kind: LaunchKind, dry_run: bool) -> Request {
    let r = request_for(id, dry_run);
    if kind == LaunchKind::AtLogin {
        Request { quiet: true, takeover: if dry_run { Takeover::DryRun } else { Takeover::No }, ..r }
    } else {
        r
    }
}

/// A second process (the lock was taken): ask, decide, act. Every decision is logged.
pub fn second_instance(ipc: &Ipc, kind: LaunchKind, dry_run: bool, log: Log) -> Second {
    let id = identity();
    if !asks(kind) {
        log(&format!(
            "another ZTab instance is running; exiting (pid {}, started {}: nothing asked or changed)",
            id.pid,
            match kind {
                LaunchKind::Launchd => "by launchd",
                _ => "with arguments (a script or a test)",
            }
        ));
        return Second::Exit;
    }
    log(&format!(
        "reopen: ZTab {} (pid {}, {}) was opened while another ZTab holds the instance lock: asking it{}{}",
        id.version,
        id.pid,
        id.exe,
        if kind == LaunchKind::AtLogin {
            format!(
                " quietly (opened without arguments {:.0} s after login: maybe a login item; it shows a window only if something is wrong, and is replaced only if it does not answer)",
                secs_since_login().unwrap_or(0.0)
            )
        } else {
            String::new()
        },
        if dry_run { " (takeover dry run)" } else { "" }
    ));
    let req = request_for_kind(id, kind, dry_run);
    let started = Instant::now();
    let mut stopped_once = false;
    loop {
        let t = Instant::now();
        let answer = ask(ipc, &req, ANSWER_WAIT);
        let peer_pid = match &answer {
            Answer::Replied(r) => Some(r.pid),
            Answer::Stalled { pid, .. } | Answer::NoReply { pid } => *pid,
            _ => None,
        };
        let holder = find_holder(ipc, peer_pid);
        let action = decide_second(kind, &answer, holder.as_ref(), dry_run, stopped_once);
        log(&format!(
            "reopen: {} (after {:.0} ms; lock holder {}): {action:?}",
            answer.describe(),
            t.elapsed().as_secs_f64() * 1000.0,
            holder.as_ref().map(|h| format!("pid {} {} started {:.1} s ago", h.pid, h.exe, h.age.as_secs_f64())).unwrap_or_else(|| "not found".into())
        ));
        match action {
            SecondAction::Quiet => return Second::Exit,
            SecondAction::Shown => {
                log(if req.quiet {
                    "reopen: the running ZTab checks itself (quietly: a window only if something is wrong or was fixed); exiting"
                } else {
                    "reopen: the running ZTab shows its status; exiting"
                });
                return Second::Exit;
            }
            SecondAction::Report(why) => {
                log(&format!("reopen: {why}; exiting"));
                println!("{why}");
                return Second::Exit;
            }
            SecondAction::Wait => {
                if started.elapsed() > WAIT_YOUNG_MAX {
                    log("reopen: the lock holder is still starting after the wait; exiting");
                    return Second::Exit;
                }
                std::thread::sleep(Duration::from_millis(300));
            }
            SecondAction::TakeOver { pid, graceful, why } => {
                log(&format!("reopen: taking over: {why}"));
                match stop_holder(ipc, pid, graceful, dry_run, log) {
                    Stopped::Taken { file, how } => {
                        keep_lock(ipc, file);
                        log(&format!("reopen: {how}; this ZTab holds the instance lock now and continues its launch"));
                        let note = if graceful {
                            format!("replaced ZTab {} that was running", answer_version(&answer))
                        } else {
                            format!("replaced a ZTab that did not respond (pid {pid})")
                        };
                        return Second::Continue(note);
                    }
                    Stopped::LockTakenByOther(e) => {
                        log(&format!("reopen: {e}; asking again"));
                        stopped_once = true;
                    }
                    Stopped::Failed(e) | Stopped::Refused(e) => {
                        log(&format!("reopen: could not take over: {e}; exiting"));
                        return Second::Exit;
                    }
                }
            }
        }
    }
}

fn answer_version(a: &Answer) -> String {
    match a {
        Answer::Replied(r) => format!("{} ({})", r.version, r.exe),
        _ => "?".into(),
    }
}

/// `ztab --reopen-check`: what opening this binary by hand would do with the running ZTab, as a
/// dry run (it never takes the lock and never stops anything). Prints and returns the exit code
/// (0: answered or nothing running, 1: it would take over or cannot).
pub fn reopen_check(ipc: &Ipc) -> i32 {
    let id = identity();
    println!("this binary: ZTab {} pid {} {} (binid {}; opened as {})", id.version, id.pid, id.exe, id.binid, id.stable);
    println!(
        "console login: {} (an argument-free launch or a reopen event within {} s of it is handled quietly: checked and repaired, a window only if something is wrong or was fixed)",
        secs_since_console_login().map(|s| format!("{s:.0} s ago")).unwrap_or_else(|| "not found".into()),
        LOGIN_QUIET.as_secs()
    );
    let open = openers(&ipc.lock);
    println!("instance lock {}: open in {}", ipc.lock.display(), if open.is_empty() { "no process".to_string() } else { format!("{open:?}") });
    if open.is_empty() && !someone_listens(ipc) {
        println!("no ZTab is running: opening this binary would start it");
        return 0;
    }
    let t = Instant::now();
    let answer = ask(ipc, &Request { probe: true, ..request_for(id, true) }, ANSWER_WAIT);
    let peer_pid = match &answer {
        Answer::Replied(r) => Some(r.pid),
        Answer::Stalled { pid, .. } | Answer::NoReply { pid } => *pid,
        _ => None,
    };
    let holder = find_holder(ipc, peer_pid);
    println!("answer after {:.0} ms: {}", t.elapsed().as_secs_f64() * 1000.0, answer.describe());
    match &holder {
        Some(h) => println!("lock holder: pid {} {} (started {:.1} s ago)", h.pid, h.exe, h.age.as_secs_f64()),
        None => println!("lock holder: not found"),
    }
    let action = decide_second(LaunchKind::ByHand, &answer, holder.as_ref(), true, false);
    println!("decision (dry run): {action:?}");
    match action {
        SecondAction::Shown => 0,
        _ => 1,
    }
}

// ---------------------------------------------------------------------------------------------
// Self-test (`ztab --reopen-selftest <scratch-dir>`): two processes, real sockets and signals,
// only this process's own children in a scratch directory.
// ---------------------------------------------------------------------------------------------

/// Where a self-test child writes what it did (its "log").
static CHILD_LOG: OnceLock<PathBuf> = OnceLock::new();

fn child_log(s: &str) {
    if let Some(p) = CHILD_LOG.get()
        && let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p)
    {
        let _ = writeln!(f, "[{:.3}] {s}", now_unix());
    }
}

fn no_flush() {}

/// The stall watchdog in a self-test child (`wedge-*` modes): the run-loop detector on this
/// process's main run loop, escalating after 1 s (confirmed `confirm` later).
fn child_detector(confirm: Duration, action: Box<dyn Fn(Duration) + Send + Sync>) -> crate::sysstate::StallDetector {
    let rl = objc2_core_foundation::CFRunLoop::main().expect("main run loop");
    let d = crate::sysstate::StallDetector::observe(&rl, Duration::from_millis(200), None, child_log);
    assert!(d.escalate(Duration::from_secs(1), confirm, action));
    d
}

/// `f` on the main thread after `delay` (the main queue, which the main run loop serves).
fn on_main_after(delay: Duration, f: impl FnOnce() + Send + 'static) {
    let when = dispatch2::DispatchTime::try_from(delay).unwrap_or(dispatch2::DispatchTime::NOW);
    let _ = dispatch2::DispatchQueue::main().after(when, f);
}

/// The `wedge-stop` child's main thread: a 400 ms unit of work every 450 ms.
fn busy_units() {
    on_main_after(Duration::from_millis(50), || {
        std::thread::sleep(Duration::from_millis(400));
        busy_units();
    });
}

/// The child: `ztab --reopen-selftest-server <dir> <mode>`. Takes the lock in `dir`; `main`
/// listens and answers on its main thread (CFRunLoop, like ZTab), `main-fast` too with a 2 s
/// self-check, `wedged` listens but its main thread never runs the queue, `nolisten` only holds
/// the lock (like ZTab 0.2.x), `stubborn` also ignores SIGTERM. Without the lock, the stall
/// watchdog: `wedge-fire` blocks its main thread for 4 s (the escalation must fire once),
/// `wedge-stop` runs 400 ms units (stopped and continued by the parent: it must not fire),
/// `wedge-restart` blocks for 30 s with the real restart (`setup::wedge_restart`) as the action,
/// which starts `wedge-restarted`: it reads the health marker and says so.
pub fn selftest_server(dir: &Path, mode: &str) -> ! {
    let _ = CHILD_LOG.set(dir.join(format!("child-{mode}.log")));
    if mode.starts_with("wedge-") {
        selftest_wedge_child(dir, mode);
    }
    let ipc = Ipc::in_dir(dir);
    if !take_lock(&ipc) {
        eprintln!("selftest server: the lock in {} is taken", dir.display());
        std::process::exit(3);
    }
    fn quit_child(why: &str) {
        child_log(&format!("quit: {why}"));
        std::process::exit(0);
    }
    if mode == "stubborn" {
        // SAFETY: plain system call.
        unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
    }
    if mode == "main" || mode == "main-fast" || mode == "wedged" {
        let handler: Handler = Arc::new(|req: Request, resp: Responder| {
            dispatch2::DispatchQueue::main().exec_async(move || {
                let me = identity();
                let status = server_status(&me.binid, &req);
                let r = Reply {
                    pid: me.pid,
                    exe: me.exe.clone(),
                    binid: me.binid.clone(),
                    version: me.version.clone(),
                    status,
                    main_ms: resp.received.elapsed().as_secs_f64() * 1000.0,
                    deferred: false,
                };
                if resp.reply(r) && status == Status::Quitting {
                    std::thread::sleep(Duration::from_millis(200));
                    std::process::exit(0);
                }
            });
        });
        let every = if mode == "main-fast" { Duration::from_secs(2) } else { SELF_CHECK_EVERY };
        if let Err(e) = serve_every(&ipc, handler, child_log, quit_child, every) {
            eprintln!("selftest server: {e}");
            std::process::exit(4);
        }
    }
    if mode == "main" || mode == "main-fast" {
        objc2_core_foundation::CFRunLoop::run();
    }
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

fn selftest_wedge_child(dir: &Path, mode: &str) -> ! {
    let marker = dir.join("health-restart");
    match mode {
        "wedge-fire" => {
            let out = dir.join("wedge-fired.txt");
            let _d = child_detector(
                Duration::from_millis(500),
                Box::new(move |blocked| {
                    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&out) {
                        let _ = writeln!(f, "fired after {} ms", blocked.as_millis());
                    }
                }),
            );
            on_main_after(Duration::from_millis(300), || std::thread::sleep(Duration::from_secs(4)));
            on_main_after(Duration::from_secs(7), || std::process::exit(0));
            objc2_core_foundation::CFRunLoop::run();
        }
        "wedge-stop" => {
            let out = dir.join("wedge-stop-fired.txt");
            let _d = child_detector(
                Duration::from_secs(1),
                Box::new(move |blocked| {
                    let _ = std::fs::write(&out, format!("fired after {} ms\n", blocked.as_millis()));
                }),
            );
            busy_units();
            objc2_core_foundation::CFRunLoop::run();
        }
        "wedge-restart" => {
            let exe = std::env::current_exe().map(|p| std::fs::canonicalize(&p).unwrap_or(p)).unwrap_or_default();
            let w = crate::setup::WedgeRestart {
                xpc: None,
                exe,
                fallback_exe: None,
                args: vec!["--reopen-selftest-server".into(), dir.to_string_lossy().into_owned(), "wedge-restarted".into()],
                marker,
                log: child_log,
                flush: no_flush,
            };
            let _d = child_detector(Duration::from_millis(500), Box::new(move |blocked| {
                crate::setup::wedge_restart(&w, blocked, now_unix());
            }));
            on_main_after(Duration::from_millis(300), || std::thread::sleep(Duration::from_secs(30)));
            objc2_core_foundation::CFRunLoop::run();
        }
        "wedge-restarted" => {
            let r = crate::health::take_marker(&marker, now_unix());
            let _ = std::fs::write(dir.join("restarted.txt"), format!("pid={} reason={}\n", std::process::id(), r.unwrap_or_default()));
        }
        _ => eprintln!("selftest server: unknown mode {mode}"),
    }
    std::process::exit(0);
}

/// See the module docs. Returns the exit code (0: every check passed).
pub fn selftest(dir: &Path) -> i32 {
    let _ = std::fs::create_dir_all(dir);
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    if dir == std::fs::canonicalize(user_temp_dir()).unwrap_or_else(|_| user_temp_dir()) {
        eprintln!("refusing: {} is the real temporary directory (use a scratch directory)", dir.display());
        return 2;
    }
    let ipc = Ipc::in_dir(&dir);
    let _ = std::fs::remove_file(&ipc.socket);
    let exe = std::env::current_exe().unwrap_or_default();
    let me = identity().clone();
    let mut fails = 0;
    let mut check = |name: &str, ok: bool, detail: String| {
        println!("{} {name}: {detail}", if ok { "PASS" } else { "FAIL" });
        if !ok {
            fails += 1;
        }
    };
    fn say(s: &str) {
        println!("    log: {s}");
    }
    let spawn = |mode: &str| -> Option<(i32, std::sync::mpsc::Receiver<()>)> {
        let mut c = std::process::Command::new(&exe)
            .args(["--reopen-selftest-server", &dir.to_string_lossy(), mode])
            .env_remove(crate::settings::ENV_SETTINGS_DIR)
            .spawn()
            .ok()?;
        let pid = c.id() as i32;
        let (tx, rx) = std::sync::mpsc::channel();
        // Reap it at once (a zombie would still count as running).
        std::thread::spawn(move || {
            let _ = c.wait();
            let _ = tx.send(());
        });
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end {
            let ready = if mode == "nolisten" || mode == "stubborn" { openers(&Ipc::in_dir(&dir).lock).contains(&pid) } else { someone_listens(&Ipc::in_dir(&dir)) };
            if ready {
                return Some((pid, rx));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Some((pid, rx))
    };
    let kill = |pid: i32| unsafe {
        libc::kill(pid, libc::SIGCONT);
        libc::kill(pid, libc::SIGKILL);
    };
    let old = |h: Option<Holder>| h.map(|h| Holder { age: YOUNG_HOLDER + Duration::from_secs(1), ..h });
    let same = request_for(&me, false);
    let other_dry = Request { binid: "0:0:0:0.0".into(), version: "9.9.9".into(), ..request_for(&me, true) };
    let other = Request { takeover: Takeover::Yes, ..other_dry.clone() };

    // 1. Answered by the main thread: the same binary.
    if let Some((pid, rx)) = spawn("main") {
        let t = Instant::now();
        let a = ask(&ipc, &same, ANSWER_WAIT);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let ok = matches!(&a, Answer::Replied(r) if r.status == Status::Show && r.pid == pid);
        check("same binary answered show", ok && ms < 1000.0, format!("{a:?} in {ms:.0} ms (child pid {pid})"));
        let d = decide_second(LaunchKind::ByHand, &a, find_holder(&ipc, Some(pid)).as_ref(), false, false);
        check("decision: exit, it shows its status", d == SecondAction::Shown, format!("{d:?}"));
        let h = find_holder(&ipc, None);
        check("lock holder found through the lock file", h.as_ref().is_some_and(|h| h.pid == pid), format!("{h:?}"));
        // 2. A different binary, dry run: it stays.
        let a = ask(&ipc, &other_dry, ANSWER_WAIT);
        let alive = proc_info(pid).is_some();
        check(
            "different binary, dry run: it stays",
            matches!(&a, Answer::Replied(r) if r.status == Status::Staying) && alive,
            format!("{a:?}; child alive={alive}"),
        );
        // 3. Stopped (SIGSTOP): connected, no answer; stopped (macOS ends a stopped process on
        //    SIGTERM when it does not handle it; else SIGKILL follows).
        unsafe { libc::kill(pid, libc::SIGSTOP) };
        let t = Instant::now();
        let a = ask(&ipc, &same, ANSWER_WAIT);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        check(
            "stopped holder: no answer within ~2 s",
            matches!(a, Answer::NoReply { pid: Some(p) } if p == pid) && (1900.0..3500.0).contains(&ms),
            format!("{a:?} after {ms:.0} ms"),
        );
        let holder = old(find_holder(&ipc, Some(pid)));
        let d = decide_second(LaunchKind::ByHand, &a, holder.as_ref(), false, false);
        check("decision: take over (stop it)", matches!(&d, SecondAction::TakeOver { pid: p, graceful: false, .. } if *p == pid), format!("{d:?}"));
        let young = decide_second(LaunchKind::ByHand, &a, find_holder(&ipc, Some(pid)).as_ref(), false, false);
        check("decision for a young holder: wait", young == SecondAction::Wait, format!("{young:?}"));
        let dry = decide_second(LaunchKind::ByHand, &a, holder.as_ref(), true, false);
        check("decision in a dry run: report only", matches!(dry, SecondAction::Report(_)), format!("{dry:?}"));
        let refused = stop_holder(&ipc, pid, false, true, say);
        check("stop_holder in a dry run refuses", matches!(refused, Stopped::Refused(_)) && proc_info(pid).is_some(), format!("{refused:?}"));
        let t = Instant::now();
        let r = stop_holder(&ipc, pid, false, false, say);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let took = matches!(&r, Stopped::Taken { .. });
        check("stopped holder: ended (SIGTERM, or SIGKILL), the lock is taken", took, format!("{r:?} after {ms:.0} ms"));
        drop(r);
        let _ = rx.recv_timeout(Duration::from_secs(2));
    } else {
        check("spawn child (main)", false, "could not start".into());
    }
    // 4. A wedged main thread: the listener answers `stalled`; SIGTERM ends it.
    if let Some((pid, rx)) = spawn("wedged") {
        let t = Instant::now();
        let a = ask(&ipc, &same, ANSWER_WAIT);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        check(
            "wedged main thread: answered stalled after ~1.5 s",
            matches!(a, Answer::Stalled { pid: Some(p), waited_ms } if p == pid && waited_ms >= 1400) && ms < 2500.0,
            format!("{a:?} after {ms:.0} ms"),
        );
        let d = decide_second(LaunchKind::ByHand, &a, old(find_holder(&ipc, Some(pid))).as_ref(), false, false);
        check("decision: take over", matches!(d, SecondAction::TakeOver { graceful: false, .. }), format!("{d:?}"));
        let r = stop_holder(&ipc, pid, false, false, say);
        check("wedged holder: SIGTERM ends it, the lock is taken", matches!(&r, Stopped::Taken { how, .. } if how.contains("SIGTERM")), format!("{r:?}"));
        drop(r);
        let _ = rx.recv_timeout(Duration::from_secs(2));
    }
    // 5. Nobody listens (ZTab 0.2.x): found through the lock file; a dry run only reports.
    if let Some((pid, rx)) = spawn("nolisten") {
        let t = Instant::now();
        let a = ask(&ipc, &same, ANSWER_WAIT);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        check("no listener: NoListener after ~2 s", a == Answer::NoListener && ms >= 1900.0, format!("{a:?} after {ms:.0} ms"));
        let h = find_holder(&ipc, None);
        check("no listener: holder found through the lock file", h.as_ref().is_some_and(|h| h.pid == pid), format!("{h:?}"));
        let d = decide_second(LaunchKind::ByHand, &a, old(h).as_ref(), true, false);
        check("no listener, dry run: would stop it", matches!(&d, SecondAction::Report(s) if s.contains("would stop")), format!("{d:?}"));
        let q = decide_second(LaunchKind::Launchd, &a, None, false, false);
        check("started by launchd: quiet", q == SecondAction::Quiet, format!("{q:?}"));
        kill(pid);
        let _ = rx.recv_timeout(Duration::from_secs(2));
    }
    // 6. A different binary: it quits by itself; the lock is taken without a signal.
    if let Some((pid, rx)) = spawn("main") {
        let a = ask(&ipc, &other, ANSWER_WAIT);
        let d = decide_second(LaunchKind::ByHand, &a, find_holder(&ipc, Some(pid)).as_ref(), false, false);
        check("different binary: it quits", matches!(&d, SecondAction::TakeOver { graceful: true, .. }), format!("{a:?} -> {d:?}"));
        let r = stop_holder(&ipc, pid, true, false, say);
        check("different binary: the lock is taken without a signal", matches!(&r, Stopped::Taken { how, .. } if how.contains("quit by itself")), format!("{r:?}"));
        drop(r);
        let _ = rx.recv_timeout(Duration::from_secs(2));
    }
    // 7. A holder that ignores SIGTERM (and does not listen): SIGKILL after the grace period.
    if let Some((pid, rx)) = spawn("stubborn") {
        let a = ask(&ipc, &same, Duration::from_millis(300));
        let d = decide_second(LaunchKind::ByHand, &a, old(find_holder(&ipc, None)).as_ref(), false, false);
        check("stubborn holder: decision take over", matches!(&d, SecondAction::TakeOver { pid: p, .. } if *p == pid), format!("{a:?} -> {d:?}"));
        let t = Instant::now();
        let r = stop_holder(&ipc, pid, false, false, say);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        check(
            "stubborn holder: SIGTERM ignored, SIGKILL after the grace period, the lock is taken",
            matches!(&r, Stopped::Taken { how, .. } if how.contains("SIGKILL")) && ms >= TERM_GRACE.as_secs_f64() * 1000.0,
            format!("{r:?} after {ms:.0} ms"),
        );
        drop(r);
        let _ = rx.recv_timeout(Duration::from_secs(2));
    }
    // 8. stop_holder refuses a process that does not hold the lock (this process's own child, a
    //    `sleep`: not named ztab, no lock).
    if let Ok(mut c) = std::process::Command::new("/bin/sleep").arg("30").spawn() {
        let pid = c.id() as i32;
        let r = stop_holder(&ipc, pid, false, false, say);
        check("stop_holder refuses a process that is not a ZTab holding the lock", matches!(r, Stopped::Refused(_)) && proc_info(pid).is_some(), format!("{r:?}"));
        let _ = c.kill();
        let _ = c.wait();
    }
    // 9. Right after login (quiet, Takeover::No): the same binary checks quietly; another binary
    //    stays (and checks); a holder that does not answer is replaced all the same.
    if let Some((pid, rx)) = spawn("main") {
        let login = request_for_kind(&me, LaunchKind::AtLogin, false);
        check("login request: quiet, never makes another binary quit", login.quiet && login.takeover == Takeover::No, format!("{login:?}"));
        let a = ask(&ipc, &login, ANSWER_WAIT);
        let d = decide_second(LaunchKind::AtLogin, &a, find_holder(&ipc, Some(pid)).as_ref(), false, false);
        check("right after login, same binary: it checks (quietly); exit", matches!(&a, Answer::Replied(r) if r.status == Status::Show) && d == SecondAction::Shown, format!("{a:?} -> {d:?}"));
        let other_login = Request { binid: "0:0:0:0.0".into(), version: "9.9.9".into(), ..login.clone() };
        let a = ask(&ipc, &other_login, ANSWER_WAIT);
        std::thread::sleep(Duration::from_millis(400));
        let alive = proc_info(pid).is_some();
        let d = decide_second(LaunchKind::AtLogin, &a, find_holder(&ipc, Some(pid)).as_ref(), false, false);
        check(
            "right after login, another binary: it stays and checks; not replaced",
            matches!(&a, Answer::Replied(r) if r.status == Status::Show) && alive && d == SecondAction::Shown,
            format!("{a:?} -> {d:?}; child alive={alive}"),
        );
        unsafe { libc::kill(pid, libc::SIGSTOP) };
        let a = ask(&ipc, &login, ANSWER_WAIT);
        let d = decide_second(LaunchKind::AtLogin, &a, old(find_holder(&ipc, Some(pid))).as_ref(), false, false);
        check("right after login, a stopped holder: take over", matches!(&d, SecondAction::TakeOver { pid: p, graceful: false, .. } if *p == pid), format!("{a:?} -> {d:?}"));
        kill(pid);
        let _ = rx.recv_timeout(Duration::from_secs(2));
    }
    // 10. The temporary directory was cleaned up (lock file and socket removed) and another ZTab
    //     took the lock on a new file: the old one leaves the socket to it and quits once it
    //     answers (no flip-flop: the socket file stays the new one's).
    if let Some((old_pid, old_rx)) = spawn("main-fast") {
        let _ = std::fs::remove_file(&ipc.lock);
        let _ = std::fs::remove_file(&ipc.socket);
        if let Some((new_pid, new_rx)) = spawn("main") {
            let ino = std::fs::metadata(&ipc.socket).map(|m| m.ino()).ok();
            let t = Instant::now();
            let quit = old_rx.recv_timeout(Duration::from_secs(7)).is_ok();
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            let ino_after = std::fs::metadata(&ipc.socket).map(|m| m.ino()).ok();
            let answered = ask(&ipc, &Request { probe: true, ..same.clone() }, ANSWER_WAIT);
            let log = std::fs::read_to_string(dir.join("child-main-fast.log")).unwrap_or_default();
            check(
                "lock file taken by another ZTab: the old one quits, the socket stays the new one's",
                quit && ino.is_some() && ino == ino_after
                    && matches!(&answered, Answer::Replied(r) if r.pid == new_pid)
                    && log.contains(&format!("pid {new_pid}) holds the instance lock now and answers")),
                format!(
                    "old pid {old_pid} quit={quit} after {ms:.0} ms; socket inode {ino:?} -> {ino_after:?}; new pid {new_pid}: {}; old's log: {:?}",
                    answered.describe(),
                    log.lines().last().unwrap_or("")
                ),
            );
            kill(new_pid);
            let _ = new_rx.recv_timeout(Duration::from_secs(2));
        }
        if proc_info(old_pid).is_some() {
            kill(old_pid);
        }
        let _ = old_rx.recv_timeout(Duration::from_secs(2));
    }
    // 11-13. The stall watchdog (`sysstate::StallDetector::escalate`) in child processes with a
    //     real main run loop: it fires once while the main thread is stuck; not for a process
    //     that was stopped and continued; and the real restart (`setup::wedge_restart`) ends the
    //     stuck process from the watcher's thread and starts it again, with the health marker.
    let start = |mode: &str| -> Option<(i32, std::sync::mpsc::Receiver<()>)> {
        let mut c = std::process::Command::new(&exe)
            .args(["--reopen-selftest-server", &dir.to_string_lossy(), mode])
            .env_remove(crate::settings::ENV_SETTINGS_DIR)
            .spawn()
            .ok()?;
        let pid = c.id() as i32;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = c.wait();
            let _ = tx.send(());
        });
        Some((pid, rx))
    };
    for f in ["wedge-fired.txt", "wedge-stop-fired.txt", "restarted.txt", "health-restart", "child-wedge-stop.log", "child-wedge-restart.log"] {
        let _ = std::fs::remove_file(dir.join(f));
    }
    if let Some((pid, rx)) = start("wedge-fire") {
        let ended = rx.recv_timeout(Duration::from_secs(12)).is_ok();
        let fired = std::fs::read_to_string(dir.join("wedge-fired.txt")).unwrap_or_default();
        let ms: Vec<u64> = fired.lines().filter_map(|l| l.strip_prefix("fired after ")?.strip_suffix(" ms")?.parse().ok()).collect();
        check(
            "stall watchdog: fires once while the main thread is stuck (1 s, confirmed 0.5 s later)",
            ended && ms.len() == 1 && (1400..2600).contains(&ms[0]),
            format!("child {pid} ended={ended}; {fired:?}"),
        );
    }
    if let Some((pid, rx)) = start("wedge-stop") {
        std::thread::sleep(Duration::from_secs(1));
        for _ in 0..2 {
            unsafe { libc::kill(pid, libc::SIGSTOP) };
            std::thread::sleep(Duration::from_secs(3));
            unsafe { libc::kill(pid, libc::SIGCONT) };
            std::thread::sleep(Duration::from_millis(2500));
        }
        let fired = dir.join("wedge-stop-fired.txt").exists();
        let log = std::fs::read_to_string(dir.join("child-wedge-stop.log")).unwrap_or_default();
        let late = log.lines().filter(|l| l.contains("main thread still blocked")).count();
        check(
            "stall watchdog: no restart for a process stopped for 3 s mid-unit and continued",
            !fired,
            format!("child {pid} fired={fired}; the watcher saw a long unit after SIGCONT {late} time(s) (0: both stops fell between units)"),
        );
        kill(pid);
        let _ = rx.recv_timeout(Duration::from_secs(2));
    }
    if let Some((pid, rx)) = start("wedge-restart") {
        let ended = rx.recv_timeout(Duration::from_secs(10)).is_ok();
        let t = Instant::now();
        let mut restarted = String::new();
        while t.elapsed() < Duration::from_secs(12) {
            restarted = std::fs::read_to_string(dir.join("restarted.txt")).unwrap_or_default();
            if !restarted.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let log = std::fs::read_to_string(dir.join("child-wedge-restart.log")).unwrap_or_default();
        let new_pid: Option<i32> = restarted.strip_prefix("pid=").and_then(|r| r.split_whitespace().next()?.parse().ok());
        check(
            "stall watchdog: the real restart ends the stuck process off its main thread and starts it again with the marker",
            ended
                && new_pid.is_some_and(|p| p != pid)
                && restarted.contains(&format!("reason={}", crate::setup::WEDGE_REASON))
                && log.contains("wedge: the main thread has been stuck")
                && log.contains("helper pid"),
            format!("child {pid} ended={ended}; restarted: {:?}; child log: {:?}", restarted.trim(), log.lines().filter(|l| l.contains("wedge")).collect::<Vec<_>>()),
        );
        if proc_info(pid).is_some() {
            kill(pid);
        }
    }
    let _ = std::fs::remove_file(&ipc.socket);
    println!("RESULT {}", if fails == 0 { "PASS" } else { "FAIL" });
    i32::from(fails > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn reply(status: Status) -> Answer {
        Answer::Replied(Reply {
            pid: 42,
            exe: "/Applications/ZTab.app/Contents/MacOS/ztab".into(),
            binid: "1:2:3:4.000000005".into(),
            version: "0.2.4".into(),
            status,
            main_ms: 0.4,
            deferred: false,
        })
    }

    fn holder(age_s: u64) -> Holder {
        Holder { pid: 42, age: Duration::from_secs(age_s), exe: "/Applications/ZTab.app/Contents/MacOS/ztab".into() }
    }

    #[test]
    fn launch_kinds() {
        let day = Some(86400.0);
        assert_eq!(launch_kind(Some("dev.ztab.app"), &s(&["ztab"]), day), LaunchKind::Launchd);
        assert_eq!(launch_kind(Some("application.dev.ztab.app.123.456"), &s(&["ztab"]), day), LaunchKind::ByHand);
        assert_eq!(launch_kind(None, &s(&["ztab"]), day), LaunchKind::ByHand);
        assert_eq!(launch_kind(None, &s(&["ztab"]), None), LaunchKind::ByHand, "no login record");
        assert_eq!(launch_kind(None, &s(&["ztab", "-psn_0_12345"]), day), LaunchKind::ByHand);
        assert_eq!(launch_kind(None, &s(&["ztab", "--settings-dir", "/tmp/x"]), day), LaunchKind::Scripted);
        assert_eq!(launch_kind(None, &s(&["ztab", "--background"]), day), LaunchKind::Scripted);
        assert_eq!(launch_kind(None, &s(&["ztab", "--takeover-dry-run"]), day), LaunchKind::ByHand);
        assert_eq!(launch_kind(None, &s(&["ztab", "--inject-health", "tap"]), day), LaunchKind::ByHand);
        assert_eq!(launch_kind(None, &s(&["ztab", "--settings-dir", "/tmp/x", "--opened-by-hand"]), day), LaunchKind::ByHand);
        // Right after login, an argument-free launch may be a login item: AtLogin (it still asks,
        // quietly). The window is short.
        assert_eq!(LOGIN_QUIET, Duration::from_secs(30));
        assert_eq!(launch_kind(Some("application.dev.ztab.app.1.2"), &s(&["ztab"]), Some(10.0)), LaunchKind::AtLogin);
        assert_eq!(launch_kind(None, &s(&["ztab", "--just-logged-in"]), Some(0.5)), LaunchKind::AtLogin, "the test flag is not an argument of its own");
        assert_eq!(launch_kind(None, &s(&["ztab"]), Some(40.0)), LaunchKind::ByHand, "40 s after login it is the user");
        assert_eq!(launch_kind(None, &s(&["ztab"]), Some(LOGIN_QUIET.as_secs_f64() + 1.0)), LaunchKind::ByHand);
        assert_eq!(launch_kind(None, &s(&["ztab", "--opened-by-hand"]), Some(10.0)), LaunchKind::ByHand);
        assert_eq!(launch_kind(Some("dev.ztab.app"), &s(&["ztab"]), Some(10.0)), LaunchKind::Launchd);
        assert_eq!(launch_kind(None, &s(&["ztab", "--background"]), Some(10.0)), LaunchKind::Scripted);
        assert!(just_logged_in(Some(0.0)) && just_logged_in(Some(29.0)));
        assert!(!just_logged_in(Some(30.0)) && !just_logged_in(None) && !just_logged_in(Some(-5.0)));
        // The login agent with test flags is still launchd's, unless forced.
        assert_eq!(launch_kind(Some("dev.ztab.app"), &s(&["ztab", "--takeover-dry-run"]), day), LaunchKind::Launchd);
        assert!(dry_run(&s(&["ztab", "--takeover-dry-run"]), None));
        assert!(dry_run(&s(&["ztab"]), Some("dry-run")));
        assert!(!dry_run(&s(&["ztab"]), Some("yes")));
    }

    #[test]
    fn launchd_and_scripts_never_ask_or_take_over() {
        for kind in [LaunchKind::Launchd, LaunchKind::Scripted] {
            assert!(!asks(kind));
            for a in [reply(Status::Show), reply(Status::Quitting), Answer::NoListener, Answer::NoReply { pid: Some(42) }] {
                assert_eq!(decide_second(kind, &a, Some(&holder(60)), false, false), SecondAction::Quiet);
            }
        }
    }

    #[test]
    fn right_after_login_it_asks_quietly_and_replaces_only_an_unresponsive_holder() {
        assert!(asks(LaunchKind::AtLogin) && asks(LaunchKind::ByHand));
        let id = Identity { pid: 1, exe: "/a".into(), binid: "1:2:3:4.5".into(), version: "0.2.4".into(), stable: "/a".into() };
        let req = request_for_kind(&id, LaunchKind::AtLogin, false);
        assert!(req.quiet && req.takeover == Takeover::No && !req.probe, "{req:?}");
        assert_eq!(request_for_kind(&id, LaunchKind::AtLogin, true).takeover, Takeover::DryRun);
        let by_hand = request_for_kind(&id, LaunchKind::ByHand, false);
        assert!(!by_hand.quiet && by_hand.takeover == Takeover::Yes, "{by_hand:?}");
        // Another binary asked with Takeover::No stays and checks: `show`.
        assert_eq!(server_status("9:9:9:9.9", &req), Status::Show);
        assert_eq!(decide_second(LaunchKind::AtLogin, &reply(Status::Show), Some(&holder(60)), false, false), SecondAction::Shown);
        // A holder that does not answer is replaced, as when opened by hand (a hung ZTab right
        // after login is exactly when the user opens it).
        for a in [Answer::NoListener, Answer::NoReply { pid: Some(42) }, Answer::Stalled { pid: Some(42), waited_ms: 1500 }] {
            let d = decide_second(LaunchKind::AtLogin, &a, Some(&holder(60)), false, false);
            assert!(matches!(d, SecondAction::TakeOver { pid: 42, graceful: false, .. }), "{a:?} -> {d:?}");
            assert_eq!(decide_second(LaunchKind::AtLogin, &a, Some(&holder(1)), false, false), SecondAction::Wait);
        }
    }

    #[test]
    fn the_self_check_rebinds_only_while_the_lock_is_its_own() {
        assert_eq!(ipc_action(&LockCheck::Held, true), IpcAction::Keep);
        assert_eq!(ipc_action(&LockCheck::Held, false), IpcAction::Rebind);
        assert_eq!(ipc_action(&LockCheck::Retaken, false), IpcAction::Rebind);
        assert_eq!(ipc_action(&LockCheck::Unknown("x".into()), false), IpcAction::Rebind);
        // Another ZTab holds the lock now: never take the socket from it, whoever's file is there.
        assert_eq!(ipc_action(&LockCheck::OtherHolder(vec![7]), false), IpcAction::Yield(vec![7]));
        assert_eq!(ipc_action(&LockCheck::OtherHolder(vec![7]), true), IpcAction::Yield(vec![7]));
    }

    #[test]
    fn answered_same_binary_shows_and_exits() {
        assert_eq!(decide_second(LaunchKind::ByHand, &reply(Status::Show), Some(&holder(60)), false, false), SecondAction::Shown);
        // Whatever the holder's age.
        assert_eq!(decide_second(LaunchKind::ByHand, &reply(Status::Show), Some(&holder(0)), false, false), SecondAction::Shown);
    }

    #[test]
    fn answered_different_binary_takes_over_gracefully() {
        let d = decide_second(LaunchKind::ByHand, &reply(Status::Quitting), None, false, false);
        assert!(matches!(d, SecondAction::TakeOver { pid: 42, graceful: true, .. }), "{d:?}");
        let d = decide_second(LaunchKind::ByHand, &reply(Status::Staying), None, true, false);
        assert!(matches!(d, SecondAction::Report(_)), "{d:?}");
        let d = decide_second(LaunchKind::ByHand, &reply(Status::Quitting), None, true, false);
        assert!(matches!(d, SecondAction::Report(_)), "{d:?}");
    }

    #[test]
    fn no_answer_stops_an_old_holder_and_waits_for_a_young_one() {
        for a in [Answer::NoListener, Answer::NoReply { pid: Some(42) }, Answer::Stalled { pid: Some(42), waited_ms: 1500 }] {
            let d = decide_second(LaunchKind::ByHand, &a, Some(&holder(60)), false, false);
            assert!(matches!(d, SecondAction::TakeOver { pid: 42, graceful: false, .. }), "{a:?} -> {d:?}");
            assert_eq!(decide_second(LaunchKind::ByHand, &a, Some(&holder(1)), false, false), SecondAction::Wait, "{a:?}");
            // Dry run: report only.
            let d = decide_second(LaunchKind::ByHand, &a, Some(&holder(60)), true, false);
            assert!(matches!(&d, SecondAction::Report(s) if s.contains("would stop pid 42")), "{d:?}");
            // Never a second stop.
            let d = decide_second(LaunchKind::ByHand, &a, Some(&holder(60)), false, true);
            assert!(matches!(d, SecondAction::Report(_)), "{d:?}");
            // No holder found: leave it.
            let d = decide_second(LaunchKind::ByHand, &a, None, false, false);
            assert!(matches!(d, SecondAction::Report(_)), "{d:?}");
        }
        // Another user's socket is never acted on.
        let d = decide_second(LaunchKind::ByHand, &Answer::Rejected { uid: 0 }, Some(&holder(60)), false, false);
        assert!(matches!(d, SecondAction::Report(_)), "{d:?}");
    }

    #[test]
    fn server_decides_by_binary_identity() {
        let id = Identity { pid: 1, exe: "/a".into(), binid: "1:2:3:4.5".into(), version: "0.2.4".into(), stable: "/a".into() };
        let same = request_for(&id, false);
        assert_eq!(server_status("1:2:3:4.5", &same), Status::Show);
        assert_eq!(server_status("1:2:3:9.5", &same), Status::Quitting);
        assert_eq!(server_status("1:2:3:9.5", &request_for(&id, true)), Status::Staying);
        assert_eq!(server_status("1:2:3:9.5", &Request { takeover: Takeover::No, ..same.clone() }), Status::Show);
        // An unknown identity never matches.
        assert_eq!(server_status("", &Request { binid: String::new(), ..same.clone() }), Status::Quitting);
        assert!(!same_binary("", ""));
    }

    #[test]
    fn protocol_round_trips() {
        let req = Request {
            pid: 77,
            exe: "/Users/a b/Applications/ZTab.app/Contents/MacOS/ztab".into(),
            binid: "16777234:123:456:789.000000001".into(),
            version: "0.2.4".into(),
            takeover: Takeover::DryRun,
            probe: true,
            quiet: false,
        };
        let line = req.line();
        assert!(line.ends_with('\n') && !line.trim_end().contains('\n'));
        assert_eq!(Request::parse(&line), Some(req.clone()));
        let quiet = Request { takeover: Takeover::No, probe: false, quiet: true, ..req.clone() };
        assert_eq!(Request::parse(&quiet.line()), Some(quiet));
        let r = Reply {
            pid: 9,
            exe: "/opt/homebrew/Cellar/ztab/0.2.4/ZTab.app/Contents/MacOS/ztab".into(),
            binid: "1:2:3:4.5".into(),
            version: "0.2.4".into(),
            status: Status::Quitting,
            main_ms: 1.5,
            deferred: true,
        };
        assert_eq!(Answer::parse(&r.line(), None), Some(Answer::Replied(r)));
        assert_eq!(Answer::parse("ZTAB1 stalled pid=5 waited_ms=1500\n", None), Some(Answer::Stalled { pid: Some(5), waited_ms: 1500 }));
        assert_eq!(Answer::parse("ZTAB1 stalled waited_ms=1500\n", Some(8)), Some(Answer::Stalled { pid: Some(8), waited_ms: 1500 }));
        assert_eq!(Answer::parse("ZTAB1 error bad-request\n", None), None);
        assert_eq!(Request::parse("hello"), None);
        assert_eq!(dec(&enc("")), "");
        assert_eq!(dec(&enc("ä %/x")), "ä %/x");
    }

    fn scratch(name: &str) -> PathBuf {
        // Short: a Unix socket path is limited to 104 bytes.
        let d = std::env::temp_dir().join(format!("zt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn lock_is_exclusive_and_its_holder_is_found() {
        let d = scratch("lock");
        let ipc = Ipc::in_dir(&d);
        let a = try_lock(&ipc.lock).unwrap();
        assert!(a.is_some());
        assert!(try_lock(&ipc.lock).unwrap().is_none(), "a second flock on another descriptor fails");
        // `openers` leaves this process out; the lock file is open here only.
        assert!(openers(&ipc.lock).is_empty());
        drop(a);
        assert!(try_lock(&ipc.lock).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_binary_replaced_at_its_path_is_seen() {
        let d = scratch("rp");
        let f = d.join("ztab");
        std::fs::write(&f, b"old").unwrap();
        let id = Identity { pid: 1, exe: f.to_string_lossy().into(), binid: binid_of(&f), version: "0.2.4".into(), stable: f.to_string_lossy().into() };
        assert_eq!(replaced_on_disk(&id), None);
        // An upgrade writes a new file at the path (new inode).
        std::fs::remove_file(&f).unwrap();
        std::fs::write(&f, b"new!").unwrap();
        assert_eq!(replaced_on_disk(&id), Some(f.to_string_lossy().into_owned()));
        // A removed file is not a new binary.
        std::fs::remove_file(&f).unwrap();
        assert_eq!(replaced_on_disk(&id), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn console_login_time_is_in_the_past() {
        let s = secs_since_console_login();
        println!("console login: {s:?} s ago");
        if let Some(s) = s {
            assert!(s >= 0.0, "{s}");
        }
    }

    #[test]
    fn proc_info_of_this_process() {
        let p = proc_info(std::process::id() as i32).unwrap();
        assert_eq!(p.uid, my_uid());
        assert!(!p.exe.is_empty() && p.start_unix > 0.0);
        assert!(!stoppable(std::process::id() as i32, &p), "never this process");
        assert!(proc_info(-1).is_none());
    }

    /// The exchange in one process: a listener whose "main thread" answers, never answers, or
    /// answers too late.
    #[test]
    fn listener_answers_only_through_the_handler() {
        for (mode, expect) in [("answer", "replied"), ("never", "stalled"), ("late", "stalled")] {
            let d = scratch(&format!("l{mode}"));
            let ipc = Ipc::in_dir(&d);
            let late_ok = Arc::new(Mutex::new(None));
            let lo = late_ok.clone();
            let handler: Handler = Arc::new(move |req: Request, resp: Responder| match mode {
                "answer" => {
                    let id = identity();
                    resp.reply(Reply {
                        pid: id.pid,
                        exe: id.exe.clone(),
                        binid: id.binid.clone(),
                        version: id.version.clone(),
                        status: server_status(&id.binid, &req),
                        main_ms: 0.0,
                        deferred: false,
                    });
                }
                "late" => {
                    let lo = lo.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(MAIN_ACK_WAIT + Duration::from_millis(300));
                        let id = identity();
                        let ok = resp.reply(Reply {
                            pid: id.pid,
                            exe: String::new(),
                            binid: String::new(),
                            version: String::new(),
                            status: Status::Show,
                            main_ms: 0.0,
                            deferred: false,
                        });
                        *lo.lock().unwrap() = Some(ok);
                    });
                }
                _ => std::mem::forget(resp),
            });
            let l = UnixListener::bind(&ipc.socket).unwrap();
            let srv = Arc::new(Server { ipc: ipc.clone(), handler, log: |_| {}, quit: |_| {}, self_check_every: SELF_CHECK_EVERY });
            std::thread::spawn(move || {
                if let Ok((s, _)) = l.accept() {
                    srv.handle(s);
                }
            });
            let t = Instant::now();
            let a = ask(&ipc, &request_for(identity(), false), ANSWER_WAIT);
            let ms = t.elapsed().as_millis();
            match expect {
                "replied" => assert!(matches!(&a, Answer::Replied(r) if r.status == Status::Show && r.pid == identity().pid), "{a:?}"),
                _ => {
                    assert!(matches!(a, Answer::Stalled { waited_ms, .. } if waited_ms >= 1400), "{mode}: {a:?}");
                    assert!((1400..2600).contains(&ms), "{mode}: {ms} ms");
                }
            }
            if mode == "late" {
                std::thread::sleep(Duration::from_millis(600));
                assert_eq!(*late_ok.lock().unwrap(), Some(false), "a reply after `stalled` is refused");
            }
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn nobody_listening_is_no_listener() {
        let d = scratch("nl");
        let ipc = Ipc::in_dir(&d);
        let t = Instant::now();
        let a = ask(&ipc, &request_for(identity(), false), Duration::from_millis(300));
        assert_eq!(a, Answer::NoListener);
        assert!(t.elapsed() >= Duration::from_millis(300));
        // A stale socket file (nobody behind it) is the same.
        drop(UnixListener::bind(&ipc.socket).unwrap());
        assert!(ipc.socket.exists());
        assert!(!someone_listens(&ipc));
        assert_eq!(ask(&ipc, &request_for(identity(), false), Duration::from_millis(200)), Answer::NoListener);
        let _ = std::fs::remove_dir_all(&d);
    }
}
