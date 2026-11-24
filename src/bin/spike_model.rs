//! Verification spike for src/model.rs (needs Accessibility: bundle it with scripts/bundle.sh).
//!
//!   spike_model <out.txt> <scratch-dir>            all checks (a)-(d)
//!   spike_model <out.txt> <scratch-dir> --timing   only (d): snapshot timing with the user's apps
//!
//! Only scratch windows are touched: a private TextEdit instance (`open -n`) with two documents
//! from <scratch-dir>, a Finder window on a scratch folder, and Calculator is activated if it is
//! already running. TextEdit is SIGSTOPped for check (c) and always SIGCONTed afterwards.

#[path = "../windows.rs"]
#[allow(dead_code)]
mod windows;
#[path = "../model.rs"]
#[allow(dead_code)]
mod model;

use std::cell::RefCell;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::process::Command;
use std::rc::Rc;
use std::time::{Duration, Instant};

use model::{ModelEvent, Snapshot, WindowModel};
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSRunningApplication};
use objc2_application_services::AXUIElement;
use objc2_core_foundation::{CFBoolean, CFRunLoop, CFString, kCFRunLoopDefaultMode};

struct Out {
    t0: Instant,
    text: RefCell<String>,
    path: String,
    /// Longest single main-run-loop turn seen while pumping (ms): event delivery / NSWorkspace
    /// handlers must stay cheap.
    max_turn_ms: RefCell<f64>,
    max_turn_at: RefCell<f64>,
}

impl Out {
    fn line(&self, s: impl AsRef<str>) {
        let l = format!("[{:8.1}] {}\n", self.t0.elapsed().as_secs_f64() * 1000.0, s.as_ref());
        self.text.borrow_mut().push_str(&l);
        // Keep the file current (outside of run-loop turns, so the stall measurement is unaffected).
        self.flush();
    }
    fn flush(&self) {
        let _ = std::fs::write(&self.path, &*self.text.borrow());
    }
}

fn pump(out: &Out, secs: f64) {
    let end = Instant::now() + Duration::from_secs_f64(secs);
    while Instant::now() < end {
        let t = Instant::now();
        CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.005, false);
        let ms = t.elapsed().as_secs_f64() * 1000.0 - 5.0;
        let mut m = out.max_turn_ms.borrow_mut();
        if ms > *m {
            *m = ms;
            *out.max_turn_at.borrow_mut() = out.t0.elapsed().as_secs_f64() * 1000.0;
        }
    }
}

fn stats(v: &[f64]) -> String {
    if v.is_empty() {
        return "n=0".into();
    }
    let mut v = v.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    let p = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
    format!("n={} p50={:.2}ms p95={:.2}ms max={:.2}ms", v.len(), p(0.5), p(0.95), v[v.len() - 1])
}

fn esc(s: &str) -> String {
    s.replace('"', "'")
}

fn describe(s: &Snapshot) -> String {
    let mut o = format!("current={:?} mru={:?}\n", s.current, s.mru);
    for (i, w) in s.windows.iter().enumerate() {
        let _ = writeln!(
            o,
            "    #{i} wid={} pid={} app=\"{}\" title=\"{}\" z={} min={} space={} hidden={} ax={}",
            w.wid,
            w.pid,
            esc(&w.app_name),
            esc(&w.title),
            w.z_index,
            w.minimized,
            w.on_current_space,
            w.hidden_app,
            w.has_ax()
        );
    }
    o
}

fn mru_wids(s: &Snapshot) -> Vec<u32> {
    s.mru.iter().map(|&i| s.windows[i].wid).collect()
}

fn ax_set_minimized(el: &AXUIElement, v: bool) -> bool {
    let key = CFString::from_str("AXMinimized");
    unsafe { el.set_attribute_value(&key, CFBoolean::new(v)) == objc2_application_services::AXError::Success }
}

fn ax_close(el: &AXUIElement) -> bool {
    let Some(btn) = windows::ax_attr(el, "AXCloseButton").and_then(|b| b.downcast::<AXUIElement>().ok()) else { return false };
    unsafe { btn.perform_action(&CFString::from_str("AXPress")) == objc2_application_services::AXError::Success }
}

fn pids_named(name: &str) -> Vec<i32> {
    let o = Command::new("pgrep").args(["-x", name]).output().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
    o.lines().filter_map(|l| l.trim().parse().ok()).collect()
}

thread_local! {
    static USER_WINDOW: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

/// SIGCONTs a pid when dropped (also on panic).
struct ContGuard(Option<i32>);
impl Drop for ContGuard {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            unsafe { libc::kill(p, libc::SIGCONT) };
        }
    }
}

/// Poll snapshots until `pred` finds a window; returns (index, elapsed ms, snapshot).
fn wait_for(out: &Out, m: &WindowModel, timeout_s: f64, pred: impl Fn(&windows::WinInfo) -> bool) -> Option<(f64, Snapshot)> {
    let t = Instant::now();
    while t.elapsed().as_secs_f64() < timeout_s {
        let s = m.snapshot();
        if s.windows.iter().any(&pred) {
            return Some((t.elapsed().as_secs_f64() * 1000.0, s));
        }
        pump(out, 0.005);
    }
    None
}

fn main() {
    let mtm = MainThreadMarker::new().unwrap();
    let args: Vec<String> = std::env::args().collect();
    let out = Rc::new(Out {
        t0: Instant::now(),
        text: RefCell::new(String::new()),
        path: args.get(1).cloned().unwrap_or_else(|| "/dev/stdout".into()),
        max_turn_ms: RefCell::new(0.0),
        max_turn_at: RefCell::new(0.0),
    });
    let scratch = args.get(2).cloned().unwrap_or_else(|| "/tmp".into());
    let timing_only = args.iter().any(|a| a == "--timing");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    out.line(format!("pid={} ax={} sr={}", std::process::id(), windows::ax_trusted(), windows::screen_recording_granted()));

    if args.iter().any(|a| a == "--probe-apps") {
        probe_apps(&out);
        out.flush();
        return;
    }
    let t = Instant::now();
    let model = WindowModel::start(mtm);
    out.line(format!("start took {:.2}ms", t.elapsed().as_secs_f64() * 1000.0));
    let events: Rc<RefCell<Vec<(f64, ModelEvent)>>> = Rc::new(RefCell::new(Vec::new()));
    {
        let (ev, o) = (events.clone(), out.clone());
        model.set_event_sink(Box::new(move |e| {
            let t = o.t0.elapsed().as_secs_f64() * 1000.0;
            ev.borrow_mut().push((t, e));
        }));
    }
    let t = Instant::now();
    let s0 = model.snapshot();
    out.line(format!("cold snapshot {:.2}ms windows={} | {}", t.elapsed().as_secs_f64() * 1000.0, s0.windows.len(), model.stats()));
    let t = Instant::now();
    while !model.ready() && t.elapsed().as_secs_f64() < 5.0 {
        pump(&out, 0.005);
    }
    out.line(format!("ready={} after {:.0}ms (start -> all apps scanned)", model.ready(), t.elapsed().as_secs_f64() * 1000.0));
    pump(&out, 2.0);
    let start_snap = model.snapshot();
    out.line(format!("after 2s: {}\n{}", model.stats(), describe(&start_snap)));
    let user_current = start_snap.current.map(|i| start_snap.windows[i].clone());

    let mut verdicts: Vec<(String, bool)> = Vec::new();
    let (threads0, rss0) = task_info();
    out.line(format!("process threads={threads0} rss={:.1}MB (after ready)", rss0 as f64 / 1e6));
    if args.iter().any(|a| a == "--launch-only") {
        launch_only(&out, &model, &scratch, &mut verdicts);
        out.line(format!("stats: {}", model.stats()));
        out.line(format!("max main run-loop turn while pumping: {:.2}ms at {:.0}ms", *out.max_turn_ms.borrow(), *out.max_turn_at.borrow()));
        for (v, ok) in &verdicts {
            out.line(format!("{} {v}", if *ok { "PASS" } else { "FAIL" }));
        }
        out.flush();
        return;
    }
    if !timing_only {
        run_scratch_checks(&out, &model, &events, &scratch, &mut verdicts);
    }

    // (d) timing with all the user's apps.
    pump(&out, 1.0);
    let mut times = Vec::new();
    let mut last = Snapshot::default();
    for _ in 0..200 {
        let t = Instant::now();
        last = model.snapshot();
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        pump(&out, 0.02);
    }
    out.line(format!("(d) snapshot timing, {} windows: {}", last.windows.len(), stats(&times)));
    out.line(format!("(d) final snapshot\n{}", describe(&last)));
    let t = Instant::now();
    let reference = windows::list_windows();
    let lw_ms = t.elapsed().as_secs_f64() * 1000.0;
    let a: HashSet<u32> = last.windows.iter().map(|w| w.wid).collect();
    let b: HashSet<u32> = reference.iter().map(|w| w.wid).collect();
    out.line(format!(
        "(d) vs list_windows ({lw_ms:.1}ms, {} windows): only_model={:?} only_list_windows={:?}",
        reference.len(),
        a.difference(&b).collect::<Vec<_>>(),
        b.difference(&a).collect::<Vec<_>>()
    ));
    for w in reference.iter().filter(|w| !a.contains(&w.wid)) {
        out.line(format!("    list_windows-only: wid={} app={} title={:?} min={} space={}", w.wid, w.app_name, w.title, w.minimized, w.on_current_space));
    }
    for w in last.windows.iter().filter(|w| !b.contains(&w.wid)) {
        out.line(format!("    model-only: wid={} app={} title={:?} min={} space={} ax={}", w.wid, w.app_name, w.title, w.minimized, w.on_current_space, w.has_ax()));
    }
    // The cache keeps up: after a quiet second, the snapshot's z-order of on-screen windows equals
    // a fresh CG on-screen list.
    pump(&out, 1.0);
    let s = model.snapshot();
    let fresh = windows::cg_window_ids(objc2_core_graphics::CGWindowListOption::OptionOnScreenOnly | objc2_core_graphics::CGWindowListOption::ExcludeDesktopElements);
    let listed: HashSet<u32> = s.windows.iter().map(|w| w.wid).collect();
    let fresh_order: Vec<u32> = fresh.iter().copied().filter(|w| listed.contains(w)).collect();
    let on: HashSet<u32> = fresh_order.iter().copied().collect();
    let mut by_z: Vec<&windows::WinInfo> = s.windows.iter().collect();
    by_z.sort_by_key(|w| w.z_index);
    let snap_order: Vec<u32> = by_z.iter().map(|w| w.wid).filter(|w| on.contains(w)).collect();
    let first_k: Vec<u32> = by_z.iter().take(on.len()).map(|w| w.wid).collect();
    out.line(format!("(d) z-order: fresh on-screen {fresh_order:?} snapshot {snap_order:?} first_k {first_k:?}"));
    verdicts.push(("(d) snapshot z-order == fresh CG on-screen order after 1 s quiet".into(), fresh_order == snap_order && first_k == snap_order));
    // Space membership computed on the background thread == computed on the main thread.
    let apps = windows::regular_apps();
    let cands = windows::cg_candidates(&apps);
    let main_space: std::collections::HashMap<u32, (bool, Option<bool>)> =
        cands.iter().map(|c| c.wid).zip(windows::space_membership(&cands)).collect();
    let mut diffs = Vec::new();
    let mut compared = 0;
    for (wid, pid, rank, space) in model.debug_cg_rows() {
        if rank.is_none() && apps.contains_key(&pid) && let Some(m) = main_space.get(&wid) {
            compared += 1;
            if *m != space {
                diffs.push((wid, space, *m));
            }
        }
    }
    out.line(format!("(d) Space membership bg vs main: compared {compared} off-screen windows, diffs (wid, bg, main) {diffs:?}"));
    verdicts.push((format!("(d) background SkyLight Space membership == main-thread ({compared} windows)"), diffs.is_empty() && compared > 0));
    verdicts.push((format!("(d) p95 < 10ms ({})", stats(&times)), {
        let mut v = times.clone();
        v.sort_by(|a, b| a.total_cmp(b));
        v[(v.len() - 1) * 95 / 100] < 10.0
    }));

    // Restore the user's window (the one current before the MRU test activated other apps).
    let user_current = USER_WINDOW.with(|u| u.get()).or(user_current.map(|w| w.wid));
    if !timing_only && let Some(w) = user_current {
        let s = model.snapshot();
        if let Some(w) = s.windows.iter().find(|x| x.wid == w) {
            let ok = windows::focus_window(w);
            out.line(format!("restored user's window wid={} app={} ok={ok}", w.wid, w.app_name));
        }
    }
    pump(&out, 0.3);
    // Leaks / thread count: many snapshots must not grow memory or threads.
    pump(&out, 1.0);
    let (threads1, rss1) = task_info();
    for _ in 0..5000 {
        let s = model.snapshot();
        std::hint::black_box(&s);
    }
    pump(&out, 0.5);
    let (threads2, rss2) = task_info();
    out.line(format!(
        "process threads: after ready {threads0}, after checks {threads1}, after 5000 more snapshots {threads2}; rss {:.1}MB -> {:.1}MB -> {:.1}MB",
        rss0 as f64 / 1e6,
        rss1 as f64 / 1e6,
        rss2 as f64 / 1e6
    ));
    verdicts.push((
        format!("threads bounded (one per regular app + 1): {threads2} threads, model threads={}; no growth over 5000 snapshots ({:+.2}MB)", model.stats().split("threads=").nth(1).and_then(|x| x.split(' ').next()).unwrap_or("?"), (rss2 as f64 - rss1 as f64) / 1e6),
        threads2 <= threads1 && (rss2 as f64 - rss1 as f64) < 2e6,
    ));
    out.line(format!("stats: {}", model.stats()));
    out.line(format!("max main run-loop turn while pumping: {:.2}ms", *out.max_turn_ms.borrow()));
    for (t, e) in events.borrow().iter() {
        out.line(format!("event at {t:.1}ms: {e:?}"));
    }
    for (v, ok) in &verdicts {
        out.line(format!("{} {v}", if *ok { "PASS" } else { "FAIL" }));
    }
    out.flush();
}

fn run_scratch_checks(out: &Out, model: &WindowModel, events: &Rc<RefCell<Vec<(f64, ModelEvent)>>>, scratch: &str, verdicts: &mut Vec<(String, bool)>) {
    let dir = format!("{scratch}/model-spike");
    let _ = std::fs::create_dir_all(format!("{dir}/mru-finder"));
    let (fa, fb, fc) = (format!("{dir}/model-a.txt"), format!("{dir}/model-b.txt"), format!("{dir}/model-c.txt"));
    for f in [&fa, &fb, &fc] {
        let _ = std::fs::write(f, "ztab model spike scratch\n");
    }
    let before: Vec<i32> = pids_named("TextEdit");
    out.line(format!("TextEdit pids before: {before:?}"));

    // ---- (a) appear: private TextEdit instance with two documents (initial scan path) ----
    let t_open = Instant::now();
    let st = Command::new("open").args(["-n", "-g", "-a", "TextEdit", &fa, &fb]).status();
    out.line(format!("open -n -g -a TextEdit a b: {st:?} ({:.0}ms)", t_open.elapsed().as_secs_f64() * 1000.0));
    let mut te_pid = None;
    for _ in 0..200 {
        te_pid = pids_named("TextEdit").into_iter().find(|p| !before.contains(p));
        if te_pid.is_some() {
            break;
        }
        pump(out, 0.01);
    }
    let Some(te) = te_pid else {
        out.line("FAIL: no new TextEdit instance");
        verdicts.push(("(a) TextEdit instance".into(), false));
        return;
    };
    out.line(format!("scratch TextEdit pid={te}"));
    let mut guard = ContGuard(None);

    let found = wait_for(out, model, 10.0, |w| w.pid == te && w.has_ax() && w.title.contains("model-a"))
        .and_then(|_| wait_for(out, model, 5.0, |w| w.pid == te && w.has_ax() && w.title.contains("model-b")));
    let since_open = t_open.elapsed().as_secs_f64() * 1000.0;
    let Some((_, s)) = found else {
        out.line(format!("FAIL: TextEdit windows did not appear with AX; stats {}", model.stats()));
        verdicts.push(("(a) initial windows".into(), false));
        cleanup_textedit(out, model, te, &before);
        return;
    };
    let win_a = s.windows.iter().find(|w| w.pid == te && w.title.contains("model-a")).cloned().unwrap();
    let win_b = s.windows.iter().find(|w| w.pid == te && w.title.contains("model-b")).cloned().unwrap();
    out.line(format!("(a) launch: both documents listed with AX {since_open:.0}ms after `open` started (includes app launch); a={} b={}", win_a.wid, win_b.wid));

    // Third document in the running instance: exercises AXWindowCreated.
    let others = pids_named("TextEdit").into_iter().filter(|p| *p != te).count();
    if others == 0 {
        pump(out, 0.5);
        // t = when `open` is started; poll CG (directly) and the model in the same loop.
        let t = Instant::now();
        let _ = Command::new("open").args(["-g", "-a", "TextEdit", &fc]).status();
        let open_ms = t.elapsed().as_secs_f64() * 1000.0;
        let (mut cg_ms, mut model_ms, mut wid_c) = (None, None, None);
        let cg_before: HashSet<u32> = s.windows.iter().map(|w| w.wid).collect();
        while t.elapsed().as_secs_f64() < 3.0 && (model_ms.is_none() || cg_ms.is_none()) {
            if cg_ms.is_none() {
                let on = windows::cg_window_list(objc2_core_graphics::CGWindowListOption::OptionOnScreenOnly);
                for d in &on {
                    if let Some((wid, pid, layer, b, _, _)) = windows::parse_cg(d)
                        && pid == te
                        && layer == 0
                        && b.2 >= 50.0
                        && !cg_before.contains(&wid)
                    {
                        cg_ms = Some(t.elapsed().as_secs_f64() * 1000.0);
                        wid_c = Some(wid);
                    }
                }
            }
            let snap = model.snapshot();
            if model_ms.is_none()
                && let Some(w) = snap.windows.iter().find(|w| w.pid == te && w.has_ax() && w.title.contains("model-c"))
            {
                model_ms = Some(t.elapsed().as_secs_f64() * 1000.0);
                wid_c = Some(w.wid);
            }
            pump(out, 0.002);
        }
        out.line(format!(
            "(a) AXWindowCreated, times since `open` started: open returned {open_ms:.0}ms, CG on-screen list shows it {cg_ms:?}ms, snapshot lists it with AX + title {model_ms:?}ms; wid={wid_c:?}"
        ));
        let lag = match (cg_ms, model_ms) {
            (Some(c), Some(m)) => Some(m - c),
            _ => None,
        };
        verdicts.push((
            format!("(a) new window in snapshot (AX title) within 500ms: {model_ms:?}ms after `open` started, {lag:?}ms relative to CG"),
            model_ms.is_some_and(|m| m <= 500.0),
        ));
        // Close it again (AX close button).
        let snap = model.snapshot();
        if let Some(w) = snap.windows.iter().find(|w| Some(w.wid) == wid_c)
            && let Some(el) = &w.ax
        {
            let ok = ax_close(el);
            let t = Instant::now();
            let mut gone_ms = None;
            while t.elapsed().as_secs_f64() < 3.0 {
                if !model.snapshot().windows.iter().any(|x| Some(x.wid) == wid_c) {
                    gone_ms = Some(t.elapsed().as_secs_f64() * 1000.0);
                    break;
                }
                pump(out, 0.005);
            }
            pump(out, 0.2);
            let destroyed = events.borrow().iter().any(|(_, e)| Some(*e) == wid_c.map(|wid| ModelEvent::Destroyed { wid }));
            out.line(format!("(a) close third window: press={ok} gone_from_snapshot_after={gone_ms:?}ms Destroyed_event={destroyed}"));
            verdicts.push(("(a) closed window: Destroyed event + gone within 500ms".into(), destroyed && gone_ms.is_some_and(|g| g <= 500.0)));
        }
    } else {
        out.line("(a) skipped AXWindowCreated step: another TextEdit instance is running");
    }

    // Minimize B through AX; the model must show it minimized and fire Minimized. macOS posts
    // AXWindowMiniaturized only after the minimize animation, so the window leaving CG's
    // on-screen list is measured too.
    pump(out, 0.3);
    let t = Instant::now();
    let ok = win_b.ax.as_ref().map(|el| ax_set_minimized(el, true)).unwrap_or(false);
    let set_ms = t.elapsed().as_secs_f64() * 1000.0;
    let (mut min_ms, mut cg_off_ms) = (None, None);
    while t.elapsed().as_secs_f64() < 3.0 && (min_ms.is_none() || cg_off_ms.is_none()) {
        if cg_off_ms.is_none()
            && !windows::cg_window_ids(objc2_core_graphics::CGWindowListOption::OptionOnScreenOnly).contains(&win_b.wid)
        {
            cg_off_ms = Some(t.elapsed().as_secs_f64() * 1000.0);
        }
        if min_ms.is_none() && model.snapshot().windows.iter().any(|w| w.wid == win_b.wid && w.minimized) {
            min_ms = Some(t.elapsed().as_secs_f64() * 1000.0);
        }
        pump(out, 0.002);
    }
    pump(out, 0.3);
    let min_ev = events.borrow().iter().find(|(_, e)| *e == ModelEvent::Minimized { wid: win_b.wid }).map(|(t, _)| *t);
    let min_ev_rel = min_ev.map(|e| e - (t.duration_since(out.t0).as_secs_f64() * 1000.0));
    out.line(format!(
        "(a) minimize B, times since the AX set call: AX set ok={ok}, call returned {set_ms:.0}ms, gone from CG on-screen list {cg_off_ms:?}ms, Minimized event delivered {min_ev_rel:?}ms, snapshot shows minimized {min_ms:?}ms"
    ));
    let lag = match (cg_off_ms, min_ms) {
        (Some(c), Some(m)) => Some(m - c),
        _ => None,
    };
    verdicts.push((
        format!("(a) minimized in snapshot within 500ms of the window leaving the screen (lag {lag:?}ms; {min_ms:?}ms after the AX call) + Minimized event"),
        lag.is_some_and(|l| l <= 500.0) && min_ev.is_some(),
    ));

    // ---- (c) hung app: SIGSTOP TextEdit (A normal, B minimized) ----
    let s = model.snapshot();
    let pre_a = s.windows.iter().find(|w| w.wid == win_a.wid).map(|w| (w.minimized, w.has_ax()));
    let pre_b = s.windows.iter().find(|w| w.wid == win_b.wid).map(|w| (w.minimized, w.has_ax()));
    out.line(format!("(c) before STOP: a={pre_a:?} b={pre_b:?}"));
    guard.0 = Some(te);
    let r = unsafe { libc::kill(te, libc::SIGSTOP) };
    out.line(format!("(c) SIGSTOP {te} -> {r}"));
    let t_stop = Instant::now();
    let mut times = Vec::new();
    let mut bad = Vec::new();
    // 50 snapshots over ~16 s (more than two 7 s rescan periods), pumping in between.
    for i in 0..50 {
        let t = Instant::now();
        let s = model.snapshot();
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        let a = s.windows.iter().find(|w| w.wid == win_a.wid).map(|w| (w.minimized, w.has_ax()));
        let b = s.windows.iter().find(|w| w.wid == win_b.wid).map(|w| (w.minimized, w.has_ax()));
        if a != Some((false, true)) || b != Some((true, true)) {
            bad.push(format!("#{i}: a={a:?} b={b:?}"));
        }
        if i % 10 == 0 {
            out.line(format!("(c) t+{:.1}s stats: {}", t_stop.elapsed().as_secs_f64(), model.stats()));
        }
        pump(out, 0.32);
    }
    out.line(format!("(c) stopped for {:.1}s; snapshot timing while hung: {}", t_stop.elapsed().as_secs_f64(), stats(&times)));
    out.line(format!("(c) wrong snapshots: {bad:?}"));
    out.line(format!("(c) stats while hung: {}", model.stats()));
    let max = times.iter().cloned().fold(0.0, f64::max);
    let mut sorted = times.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let p95 = sorted[(sorted.len() - 1) * 95 / 100];
    verdicts.push((format!("(c) hung app: p95={p95:.2}ms max={max:.2}ms < 10ms, both windows present with correct flags"), p95 < 10.0 && max < 10.0 && bad.is_empty()));
    drop(guard);
    out.line(format!("(c) SIGCONT {te}"));
    pump(out, 1.0);
    let t = Instant::now();
    while t.elapsed().as_secs_f64() < 10.0 && model.stats().contains(&format!("{te}:")) {
        pump(out, 0.1);
    }
    out.line(format!("(c) recovered after CONT in {:.1}s: {}", t.elapsed().as_secs_f64(), model.stats()));

    // ---- (b) MRU ----
    let s = model.snapshot();
    let user_before_b = s.current.map(|i| s.windows[i].wid);
    out.line(format!("(b) user's current window before the MRU test: {user_before_b:?}"));
    USER_WINDOW.with(|u| u.set(user_before_b));
    // Restore B so TextEdit has two normal windows; A will be TextEdit's last focused window.
    let s = model.snapshot();
    if let Some(el) = s.windows.iter().find(|w| w.wid == win_b.wid).and_then(|w| w.ax.clone()) {
        out.line(format!("(b) unminimize B: {}", ax_set_minimized(&el, false)));
    }
    pump(out, 1.0);
    let s = model.snapshot();
    let a = s.windows.iter().find(|w| w.wid == win_a.wid).cloned().unwrap_or(win_a.clone());
    out.line(format!("(b) focus_window(A) = {}", windows::focus_window(&a)));
    pump(out, 1.0);
    let st = Command::new("open").arg(format!("{dir}/mru-finder")).status();
    out.line(format!("(b) open scratch Finder folder: {st:?}"));
    let finder = wait_for(out, model, 5.0, |w| w.app_name == "Finder" && w.title == "mru-finder").map(|(_, s)| s);
    pump(out, 1.0);
    let finder_wid = finder.and_then(|s| s.windows.iter().find(|w| w.app_name == "Finder" && w.title == "mru-finder").map(|w| w.wid));
    let calc_running = !pids_named("Calculator").is_empty();
    let mut calc_wid = None;
    if calc_running {
        let st = Command::new("open").args(["-a", "Calculator"]).status();
        out.line(format!("(b) open -a Calculator: {st:?}"));
        pump(out, 1.0);
        let s = model.snapshot();
        calc_wid = s.current.map(|i| s.windows[i].clone()).filter(|w| w.app_name == "Calculator").map(|w| w.wid);
    }
    let s = model.snapshot();
    let b = s.windows.iter().find(|w| w.wid == win_b.wid).cloned().unwrap_or(win_b.clone());
    out.line(format!("(b) focus_window(B) = {} (TextEdit's last focused window was A)", windows::focus_window(&b)));
    pump(out, 1.0);
    let s = model.snapshot();
    let mut expect: Vec<u32> = vec![win_b.wid];
    expect.extend(calc_wid);
    expect.extend(finder_wid);
    expect.push(win_a.wid);
    let got: Vec<u32> = mru_wids(&s).into_iter().take(expect.len()).collect();
    let cur = s.current.map(|i| s.windows[i].wid);
    out.line(format!("(b) observed: expect mru head {expect:?} got {got:?} current={cur:?}\n{}", describe(&s)));
    verdicts.push((format!("(b) MRU follows activations (B, Calculator, Finder scratch, A), current=B; finder={finder_wid:?} calc={calc_wid:?}"), got == expect && cur == Some(win_b.wid)));
    // Explicit switch like ZTab does: note_focused + focus_window.
    model.note_focused(win_a.wid);
    let s1 = model.snapshot();
    out.line(format!("(b) right after note_focused(A): mru head {:?} current={:?}", mru_wids(&s1).into_iter().take(3).collect::<Vec<_>>(), s1.current.map(|i| s1.windows[i].wid)));
    let a = s1.windows.iter().find(|w| w.wid == win_a.wid).cloned().unwrap_or(win_a.clone());
    windows::focus_window(&a);
    pump(out, 1.0);
    let s = model.snapshot();
    let mut expect2: Vec<u32> = vec![win_a.wid, win_b.wid];
    expect2.extend(calc_wid);
    expect2.extend(finder_wid);
    let got2: Vec<u32> = mru_wids(&s).into_iter().take(expect2.len()).collect();
    let cur2 = s.current.map(|i| s.windows[i].wid);
    out.line(format!("(b) after note_focused(A)+focus_window(A): expect {expect2:?} got {got2:?} current={cur2:?}"));
    let head1: Vec<u32> = mru_wids(&s1).into_iter().take(2).collect();
    verdicts.push(("(b) note_focused: A first immediately and after the switch lands, then B".into(), got2 == expect2 && cur2 == Some(win_a.wid) && head1 == vec![win_a.wid, win_b.wid]));
    // Groups are contiguous.
    let mut seen = HashSet::new();
    let mut contiguous = true;
    for (i, w) in s.windows.iter().enumerate() {
        if i > 0 && s.windows[i - 1].pid != w.pid && !seen.insert(w.pid) {
            contiguous = false;
        }
        if i == 0 {
            seen.insert(w.pid);
        }
    }
    verdicts.push(("(b) windows grouped by app (contiguous)".into(), contiguous));

    // Close the scratch Finder window (AX close button).
    if let Some(fw) = finder_wid
        && let Some(el) = model.snapshot().windows.iter().find(|w| w.wid == fw).and_then(|w| w.ax.clone())
    {
        out.line(format!("close scratch Finder window: {}", ax_close(&el)));
    }
    cleanup_textedit(out, model, te, &before);
}

fn cleanup_textedit(out: &Out, model: &WindowModel, te: i32, before: &[i32]) {
    unsafe { libc::kill(te, libc::SIGCONT) };
    pump(out, 0.3);
    let s = model.snapshot();
    for w in s.windows.iter().filter(|w| w.pid == te) {
        if let Some(el) = &w.ax {
            if w.minimized {
                ax_set_minimized(el, false);
                pump(out, 0.6);
            }
            out.line(format!("close scratch TextEdit window {} {:?}: {}", w.wid, w.title, ax_close(el)));
            pump(out, 0.3);
        }
    }
    pump(out, 0.5);
    if !before.contains(&te)
        && let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(te)
    {
        out.line(format!("terminate scratch TextEdit instance {te}: {}", app.terminate()));
    }
    pump(out, 1.5);
    let left = pids_named("TextEdit");
    out.line(format!("TextEdit pids after cleanup: {left:?} (before: {before:?})"));
}

fn probe_apps(out: &Out) {
    use objc2_app_kit::{NSApplicationActivationPolicy, NSWorkspace};
    let mut r: Vec<(&str, Vec<f64>)> = vec![("list", vec![]), ("pid", vec![]), ("policy", vec![]), ("bundle", vec![]), ("name", vec![]), ("hidden", vec![]), ("by_pid16", vec![]), ("by_pid16_props", vec![]), ("frontmost", vec![]), ("regular_apps", vec![])];
    let mut pids16: Vec<i32> = Vec::new();
    for round in 0..40 {
        pump(out, 0.05);
        let ws = NSWorkspace::sharedWorkspace();
        let t = Instant::now();
        let apps = ws.runningApplications();
        let n = apps.count();
        r[0].1.push(t.elapsed().as_secs_f64() * 1000.0);
        let t = Instant::now();
        let pids: Vec<i32> = apps.iter().map(|a| a.processIdentifier()).collect();
        r[1].1.push(t.elapsed().as_secs_f64() * 1000.0);
        let t = Instant::now();
        let regular: Vec<bool> = apps.iter().map(|a| a.activationPolicy() == NSApplicationActivationPolicy::Regular).collect();
        r[2].1.push(t.elapsed().as_secs_f64() * 1000.0);
        let t = Instant::now();
        let _b: Vec<_> = apps.iter().map(|a| a.bundleIdentifier()).collect();
        r[3].1.push(t.elapsed().as_secs_f64() * 1000.0);
        let t = Instant::now();
        let _n: Vec<_> = apps.iter().map(|a| a.localizedName()).collect();
        r[4].1.push(t.elapsed().as_secs_f64() * 1000.0);
        let t = Instant::now();
        let _h: Vec<_> = apps.iter().map(|a| a.isHidden()).collect();
        r[5].1.push(t.elapsed().as_secs_f64() * 1000.0);
        if round == 0 {
            pids16 = pids.iter().zip(&regular).filter(|(_, r)| **r).map(|(p, _)| *p).collect();
            out.line(format!("running={n} regular={}", pids16.len()));
        }
        pump(out, 0.05);
        let t = Instant::now();
        let objs: Vec<_> = pids16.iter().filter_map(|p| objc2_app_kit::NSRunningApplication::runningApplicationWithProcessIdentifier(*p)).collect();
        r[6].1.push(t.elapsed().as_secs_f64() * 1000.0);
        let t = Instant::now();
        for a in &objs {
            let _ = (a.activationPolicy(), a.bundleIdentifier(), a.localizedName(), a.isHidden());
        }
        r[7].1.push(t.elapsed().as_secs_f64() * 1000.0);
        let t = Instant::now();
        let _ = ws.frontmostApplication().map(|a| a.processIdentifier());
        r[8].1.push(t.elapsed().as_secs_f64() * 1000.0);
        pump(out, 0.05);
        let t = Instant::now();
        let _ = windows::regular_apps();
        r[9].1.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    for (name, v) in &r {
        out.line(format!("{name}: {}", stats(v)));
    }
}

/// (thread count, resident bytes) of this process.
fn task_info() -> (i32, u64) {
    unsafe {
        let mut info: libc::proc_taskinfo = std::mem::zeroed();
        let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
        let n = libc::proc_pidinfo(std::process::id() as i32, libc::PROC_PIDTASKINFO, 0, (&mut info as *mut libc::proc_taskinfo).cast(), size);
        if n != size {
            return (-1, 0);
        }
        (info.pti_threadnum, info.pti_resident_size)
    }
}

/// Background-only checks with a private TextEdit instance (never activated): launch, hide /
/// unhide, minimize / de-minimize with snapshots polled every ~2 ms (a window must never drop out
/// of the list), quit; then idle CPU of the model over 30 s.
fn launch_only(out: &Out, model: &WindowModel, scratch: &str, verdicts: &mut Vec<(String, bool)>) {
    let dir = format!("{scratch}/model-spike");
    let _ = std::fs::create_dir_all(&dir);
    let fa = format!("{dir}/model-a.txt");
    let fb = format!("{dir}/model-b.txt");
    for f in [&fa, &fb] {
        let _ = std::fs::write(f, "ztab model spike scratch\n");
    }
    let before = pids_named("TextEdit");
    *out.max_turn_ms.borrow_mut() = 0.0;
    out.line("launch-only: open -n -g -a TextEdit a b");
    let _ = Command::new("open").args(["-n", "-g", "-a", "TextEdit", &fa, &fb]).status();
    let mut te = None;
    for _ in 0..300 {
        te = pids_named("TextEdit").into_iter().find(|p| !before.contains(p));
        if te.is_some() {
            break;
        }
        pump(out, 0.01);
    }
    let Some(te) = te else { return out.line("no TextEdit instance") };
    let mut guard = ContGuard(None);
    let found = wait_for(out, model, 10.0, |w| w.pid == te && w.has_ax() && w.title.contains("model-a"))
        .and_then(|_| wait_for(out, model, 5.0, |w| w.pid == te && w.has_ax() && w.title.contains("model-b")));
    out.line(format!("launch-only: windows listed: {}", found.is_some()));
    let Some((_, s)) = found else {
        cleanup_textedit(out, model, te, &before);
        return;
    };
    let a = s.windows.iter().find(|w| w.pid == te && w.title.contains("model-a")).cloned().unwrap();
    let b = s.windows.iter().find(|w| w.pid == te && w.title.contains("model-b")).cloned().unwrap();
    pump(out, 1.0);
    let app = NSRunningApplication::runningApplicationWithProcessIdentifier(te).unwrap();
    // Poll snapshots for `secs`: (both windows always listed, ms until `pred` holds for both).
    let watch = |secs: f64, pred: &dyn Fn(&windows::WinInfo) -> bool| -> (bool, Option<f64>, Vec<String>) {
        let t = Instant::now();
        let (mut always, mut ok_at, mut gaps) = (true, None, Vec::new());
        while t.elapsed().as_secs_f64() < secs {
            let s = model.snapshot();
            let ws: Vec<&windows::WinInfo> = s.windows.iter().filter(|w| w.wid == a.wid || w.wid == b.wid).collect();
            if ws.len() != 2 {
                always = false;
                if gaps.len() < 5 {
                    gaps.push(format!("{:.0}ms: {} listed", t.elapsed().as_secs_f64() * 1000.0, ws.len()));
                }
            }
            if ok_at.is_none() && ws.len() == 2 && ws.iter().all(|w| pred(w)) {
                ok_at = Some(t.elapsed().as_secs_f64() * 1000.0);
            }
            pump(out, 0.002);
        }
        (always, ok_at, gaps)
    };
    let onscreen = |wid: u32| windows::cg_window_ids(objc2_core_graphics::CGWindowListOption::OptionOnScreenOnly).contains(&wid);
    out.line(format!("hide: returned {} (isHidden before={})", app.hide(), app.isHidden()));
    let (always, at, gaps) = watch(1.5, &|w| w.hidden_app);
    out.line(format!("launch-only: after hide: always listed={always} hidden_app at {at:?}ms gaps={gaps:?}; isHidden={} a_onscreen={}", app.isHidden(), onscreen(a.wid)));
    verdicts.push((format!("hide: both windows stay listed, hidden_app set ({at:?}ms)"), always && at.is_some()));
    out.line(format!("unhide: returned {}", app.unhide()));
    let (always, at, gaps) = watch(1.5, &|w| !w.hidden_app && w.on_current_space);
    out.line(format!("launch-only: after unhide: always listed={always} visible at {at:?}ms gaps={gaps:?}; isHidden={} a_onscreen={}", app.isHidden(), onscreen(a.wid)));
    verdicts.push((format!("unhide: both windows stay listed, visible again ({at:?}ms)"), always && at.is_some()));
    let min_ok = b.ax.as_ref().map(|el| ax_set_minimized(el, true)).unwrap_or(false);
    let (always, at, gaps) = watch(2.0, &|w| w.wid != b.wid || w.minimized);
    out.line(format!("launch-only: minimize b ({min_ok}): always listed={always} minimized at {at:?}ms gaps={gaps:?}"));
    verdicts.push((format!("minimize: both windows stay listed, minimized at {at:?}ms after the AX call"), always && at.is_some()));
    let s = model.snapshot();
    let el = s.windows.iter().find(|w| w.wid == b.wid).and_then(|w| w.ax.clone());
    let demin_ok = el.as_ref().map(|el| ax_set_minimized(el, false)).unwrap_or(false);
    let (always, at, gaps) = watch(2.0, &|w| !w.minimized && w.on_current_space);
    out.line(format!("launch-only: de-minimize b ({demin_ok}): always listed={always} restored at {at:?}ms gaps={gaps:?}"));
    verdicts.push((format!("de-minimize: both windows stay listed, restored at {at:?}ms"), always && at.is_some()));
    out.line(format!("launch-only: max main turn so far {:.2}ms at {:.0}ms; {}", *out.max_turn_ms.borrow(), *out.max_turn_at.borrow(), model.stats().split("slowest_main_handler").nth(1).unwrap_or("")));
    guard.0 = None;
    cleanup_textedit(out, model, te, &before);
    let _ = std::fs::remove_dir_all(&dir);
    pump(out, 2.0);
    // Idle CPU of this process with the user's apps: one long run-loop wait on main (like the
    // real app, unlike `pump`), CPU per thread name.
    let th0 = thread_cpu();
    let cpu0 = cpu_ms();
    let t = Instant::now();
    CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 30.0, false);
    let cpu = cpu_ms() - cpu0;
    let wall = t.elapsed().as_secs_f64();
    let th1 = thread_cpu();
    let mut per: Vec<(String, f64)> = th1.iter().map(|(k, v)| (k.clone(), v - th0.get(k).copied().unwrap_or(0.0))).filter(|(_, d)| *d > 0.5).collect();
    per.sort_by(|a, b| b.1.total_cmp(&a.1));
    out.line(format!("idle 30s: cpu {cpu:.1}ms over {wall:.1}s = {:.3}% of one core; per thread (ms): {per:?}", cpu / (wall * 10.0)));
    out.line(format!("stats after idle: {}", model.stats()));
    verdicts.push((format!("idle CPU < 1% of a core ({:.3}%)", cpu / (wall * 10.0)), cpu / (wall * 10.0) < 1.0));
}

/// CPU ms per thread name (unnamed threads by handle) of this process.
fn thread_cpu() -> std::collections::HashMap<String, f64> {
    const PROC_PIDLISTTHREADS: libc::c_int = 6;
    let mut out = std::collections::HashMap::new();
    unsafe {
        let mut ids = vec![0u64; 512];
        let n = libc::proc_pidinfo(std::process::id() as i32, PROC_PIDLISTTHREADS, 0, ids.as_mut_ptr().cast(), (ids.len() * 8) as libc::c_int);
        if n <= 0 {
            return out;
        }
        for &id in &ids[..n as usize / 8] {
            let mut ti: libc::proc_threadinfo = std::mem::zeroed();
            let size = std::mem::size_of::<libc::proc_threadinfo>() as libc::c_int;
            if libc::proc_pidinfo(std::process::id() as i32, libc::PROC_PIDTHREADINFO, id, (&mut ti as *mut libc::proc_threadinfo).cast(), size) != size {
                continue;
            }
            let name = std::ffi::CStr::from_ptr(ti.pth_name.as_ptr()).to_string_lossy().to_string();
            let name = if name.is_empty() { format!("<unnamed {id:x}>") } else { name };
            *out.entry(name).or_insert(0.0) += (ti.pth_user_time + ti.pth_system_time) as f64 / 1e6;
        }
    }
    out
}

/// Total CPU time (user + system) of this process, ms.
fn cpu_ms() -> f64 {
    unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut ru) != 0 {
            return 0.0;
        }
        let tv = |t: libc::timeval| t.tv_sec as f64 * 1000.0 + t.tv_usec as f64 / 1000.0;
        tv(ru.ru_utime) + tv(ru.ru_stime)
    }
}
