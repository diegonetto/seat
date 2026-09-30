// arm — the only waiter (constitution V, US1), implemented in
// src/serve.rs. Exactly one process watches a seat's mail. When new
// mail arrives it emits a wake notice naming the seat and the count of
// new messages (within the 5s budget). It never drains: the digest is
// `drain`'s job, so mail armed awake is still there for a later drain.
// `exit-wake` wakes once and exits 0; `poller` stays up until SIGTERM.
// `--exec` additionally runs a command on each wake.
//
// None of this touches tmux: arm works on a seat that was never
// brought `up` (T010).
use crate::board;
use crate::error::{Result, SeatError};
use crate::msg;
use crate::room;
use crate::seat::{self, Lifecycle};
use crate::verbs;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Board polling cadence. Filesystem polling only — no inotify, the
/// board is the protocol. Well inside the 5-second wake budget.
const POLL_INTERVAL: Duration = Duration::from_millis(200);
/// How long --takeover waits after SIGTERM before refusing.
const TAKEOVER_TIMEOUT: Duration = Duration::from_secs(10);

/// Set by the SIGTERM handler (poller shutdown is graceful: release the
/// pidfile, exit 0). Unit tests pass their own stop flag to `run`.
static GLOBAL_STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term(_sig: libc::c_int) {
    GLOBAL_STOP.store(true, Ordering::SeqCst);
}

fn install_sigterm_handler() {
    unsafe { libc::signal(libc::SIGTERM, on_term as *const () as libc::sighandler_t) };
}

pub struct Options {
    pub seat: String,
    /// Ephemeral lifecycle override (never written back to meta.json).
    pub lifecycle: Option<Lifecycle>,
    /// Wake command run in addition to the notice.
    pub exec: Option<String>,
    pub takeover: bool,
}

/// CLI entry: installs the real SIGTERM handler and serves until
/// wake/stop. Exit 0 covers exit-wake completion, a reported live
/// waiter, and graceful SIGTERM.
pub fn cmd_arm(
    root: &Path,
    seat_name: &str,
    lifecycle: Option<Lifecycle>,
    exec: Option<&str>,
    takeover: bool,
    out: &mut dyn Write,
) -> Result<()> {
    install_sigterm_handler();
    let opts = Options {
        seat: seat_name.to_string(),
        lifecycle,
        exec: exec.map(str::to_string),
        takeover,
    };
    run(root, &opts, &GLOBAL_STOP, out)
}

/// The waiter loop. `stop` is the shutdown flag — the SIGTERM handler
/// in production, a test-owned AtomicBool in unit tests.
fn run(root: &Path, opts: &Options, stop: &AtomicBool, out: &mut dyn Write) -> Result<()> {
    board::ensure_board(root)?;
    let dir = seat::seat_dir(root, &opts.seat);
    let meta = seat::read_meta(&dir).map_err(|e| match e {
        SeatError::GhostSeat(_) => SeatError::UnknownSeat(opts.seat.clone()),
        other => other,
    })?;

    // FR-004/T010: a live wait.pid means a waiter already holds the
    // seat. Report it, start nothing, exit 0.
    if !opts.takeover {
        if let Some(pid) = seat::read_wait_pid(&dir) {
            if seat::ppid_of(pid).is_some() {
                writeln!(
                    out,
                    "arm: seat '{}' already has a live waiter (pid {pid}); not starting another",
                    opts.seat
                )?;
                return Ok(());
            }
        }
    }

    // meta.lifecycle is the durable authority; a --lifecycle flag on
    // arm is an ephemeral override and is never persisted.
    let lifecycle = match opts.lifecycle {
        Some(flag) => flag,
        None => meta.lifecycle.ok_or_else(|| {
            SeatError::Arm(format!(
                "seat '{}' has no lifecycle; register with --lifecycle or pass --lifecycle exit-wake|poller",
                opts.seat
            ))
        })?,
    };

    claim_waiter(&opts.seat, &dir, opts.takeover)?;
    verbs::touch_last_seen(root, &opts.seat)?;
    eprintln!(
        "arm: seat '{}' waiting for mail (lifecycle={})",
        opts.seat,
        lifecycle.as_str()
    );

    // Lock the high-water baseline BEFORE the initial wake so mail
    // arriving during it still reads as a rise (never swallowed into
    // the baseline by a preemption between the two).
    let mut marks = mail_marks(root, &dir)?;

    // Mail already waiting when armed wakes immediately (both
    // lifecycles — the notice fires without draining anything). All of
    // it is new to this waiter, so it counts against an empty baseline.
    if verbs::unread_count(root, &dir)? > 0 {
        let empty: Vec<(String, String)> = Vec::new();
        wake(root, opts, &dir, &empty, out)?;
        if lifecycle == Lifecycle::ExitWake {
            release_waiter(&dir);
            return Ok(());
        }
        marks = mail_marks(root, &dir)?;
    }

    loop {
        if stop.load(Ordering::SeqCst) {
            release_waiter(&dir);
            eprintln!("arm: stopped");
            return Ok(());
        }
        let cur = mail_marks(root, &dir)?;
        if marks_rose(&marks, &cur) {
            wake(root, opts, &dir, &marks, out)?;
            if lifecycle == Lifecycle::ExitWake {
                release_waiter(&dir);
                return Ok(());
            }
        }
        marks = cur;
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// One wake action (T011): the notice names the seat and the count of
/// new messages, then --exec (if given) runs. The mail is left in place.
fn wake(
    root: &Path,
    opts: &Options,
    dir: &Path,
    prev: &[(String, String)],
    out: &mut dyn Write,
) -> Result<()> {
    let n = count_new(root, dir, prev)?;
    writeln!(out, "arm: seat '{}' has {n} new message(s)", opts.seat)?;
    if let Some(cmd) = &opts.exec {
        exec_signal(cmd)?;
    }
    Ok(())
}

/// Run the --exec command through the shell. Its stdout/stderr and exit
/// status are its own; the waiter keeps watching afterwards either way.
fn exec_signal(cmd: &str) -> Result<()> {
    match Command::new("sh").arg("-c").arg(cmd).status() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => {
            eprintln!("arm: --exec exited with {status}");
            Ok(())
        }
        Err(e) => Err(SeatError::Arm(format!("--exec failed to run: {e}"))),
    }
}

/// High-water marks per mail source: the newest `.msg` filename in the
/// inbox stages and in every followed room. The waiter wakes only when
/// a mark rises (new mail) — never merely because mail was drained
/// away, and never repeatedly while the same mail sits unread.
fn mail_marks(root: &Path, dir: &Path) -> Result<Vec<(String, String)>> {
    let mut marks = Vec::new();
    for stage in ["inbox", "draining"] {
        if let Some(max) = max_msg_name(&dir.join(stage))? {
            marks.push((stage.to_string(), max));
        }
    }
    for name in followed_rooms(dir)? {
        if let Some(max) = max_msg_name(&room::room_dir(root, &name))? {
            marks.push((format!("room:{name}"), max));
        }
    }
    Ok(marks)
}

fn max_msg_name(dir: &Path) -> Result<Option<String>> {
    Ok(msg::list_msg_dir(dir)?
        .into_iter()
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_string))
        .max())
}

fn followed_rooms(dir: &Path) -> Result<Vec<String>> {
    let mut names: Vec<String> = std::fs::read_dir(dir.join("cursors"))
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_file())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    Ok(names)
}

/// Messages newer than the previous high-water marks: the inbox stages
/// plus every followed room. Without a baseline mark, inbox mail counts
/// as all-present and room posts count from the seat's cursor. This is
/// the count the wake notice reports.
fn count_new(root: &Path, dir: &Path, prev: &[(String, String)]) -> Result<usize> {
    let mut n = 0usize;
    for stage in ["inbox", "draining"] {
        let files = msg::list_msg_dir(&dir.join(stage))?;
        n += match prev.iter().find(|(src, _)| *src == stage) {
            Some((_, mark)) => count_after(&files, mark),
            None => files.len(),
        };
    }
    for name in followed_rooms(dir)? {
        let key = format!("room:{name}");
        n += match prev.iter().find(|(src, _)| *src == key) {
            Some((_, mark)) => count_after(&room::posts(root, &name)?, mark),
            None => {
                let cursor = room::read_cursor(dir, &name)?;
                room::posts_since(root, &name, &cursor)?.len()
            }
        };
    }
    Ok(n)
}

fn count_after(files: &[std::path::PathBuf], cutoff: &str) -> usize {
    files
        .iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n > cutoff)
        })
        .count()
}

fn marks_rose(prev: &[(String, String)], cur: &[(String, String)]) -> bool {
    cur.iter().any(|(src, file)| {
        match prev.iter().find(|(psrc, _)| psrc == src) {
            None => true, // a source now has mail it did not have before
            Some((_, pfile)) => file > pfile,
        }
    })
}

/// One waiter per seat: a live holder blocks the claim unless
/// --takeover SIGTERMs it and waits for the pid to clear. (The
/// no-takeover live case is handled in `run`, which exits 0.)
fn claim_waiter(seat_name: &str, dir: &Path, takeover: bool) -> Result<()> {
    if let Some(pid) = seat::read_wait_pid(dir) {
        if seat::ppid_of(pid).is_some() {
            if !takeover {
                return Err(SeatError::WaiterBusy(seat_name.to_string(), pid));
            }
            term_and_wait(pid)?;
        }
    }
    seat::write_wait_pid(dir, std::process::id())
}

fn term_and_wait(pid: u32) -> Result<()> {
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    let deadline = Instant::now() + TAKEOVER_TIMEOUT;
    while Instant::now() < deadline {
        if seat::ppid_of(pid).is_none() {
            return Ok(());
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    Err(SeatError::Arm(format!(
        "waiter pid {pid} did not exit after SIGTERM within {TAKEOVER_TIMEOUT:?}; refusing takeover"
    )))
}

/// Remove our pidfile — only if it still names us (a --takeover
/// successor may already have replaced it).
fn release_waiter(dir: &Path) {
    if seat::read_wait_pid(dir) == Some(std::process::id()) {
        let _ = seat::clear_wait_pid(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_root;
    use crate::verbs;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};

    /// A Write that locks a shared buffer per call, so tests can read a
    /// running poller's output while it keeps looping.
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);
    impl Write for SharedBuf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(b)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn setup() -> PathBuf {
        let root = temp_root("serve");
        board::init(&root).unwrap();
        verbs::cmd_register(&root, "alpha", "sb", None, None, None, Vec::new()).unwrap();
        root
    }

    fn opts(
        seat: &str,
        lifecycle: Option<Lifecycle>,
        exec: Option<String>,
        takeover: bool,
    ) -> Options {
        Options {
            seat: seat.to_string(),
            lifecycle,
            exec,
            takeover,
        }
    }

    /// Arm on a thread: stop flag, shared output, result channel.
    struct Spawned {
        stop: Arc<AtomicBool>,
        buf: Arc<Mutex<Vec<u8>>>,
        rx: mpsc::Receiver<Result<()>>,
    }

    fn spawn(root: &Path, o: Options) -> Spawned {
        let stop = Arc::new(AtomicBool::new(false));
        let buf = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::channel();
        let (stop2, buf2, root2) = (Arc::clone(&stop), Arc::clone(&buf), root.to_path_buf());
        std::thread::spawn(move || {
            let mut out = SharedBuf(buf2);
            let _ = tx.send(run(&root2, &o, &stop2, &mut out));
        });
        Spawned { stop, buf, rx }
    }

    fn buf_str(buf: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8_lossy(&buf.lock().unwrap()).to_string()
    }

    fn wait_for(buf: &Arc<Mutex<Vec<u8>>>, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !buf_str(buf).contains(needle) {
            assert!(Instant::now() < deadline, "arm never printed {needle:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    // Every test below arms a seat that was never passed to `up` (or
    // any session verb): arm needs no tmux pane (T010).

    #[test]
    fn exit_wake_with_waiting_mail_notices_and_exits_zero() {
        let root = setup();
        verbs::cmd_register(&root, "beta", "sb", None, Some(Lifecycle::ExitWake), None, Vec::new()).unwrap();
        verbs::cmd_send(&root, "alpha", "beta", "wake now").unwrap();

        let Spawned { stop, buf, rx } = spawn(&root, opts("beta", None, None, false));
        rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        let text = buf_str(&buf);
        assert!(text.contains("has 1 new message"), "notice: {text}");
        assert!(text.contains("beta"), "notice names the seat: {text}");
        // The waiter never drains: the mail is still there for `drain`.
        assert!(!text.contains("wake now"), "no body printed: {text}");
        let dir = seat::seat_dir(&root, "beta");
        assert_eq!(msg::list_msg_dir(&dir.join("inbox")).unwrap().len(), 1);
        // Pidfile released on the way out.
        assert_eq!(seat::read_wait_pid(&dir), None);
        assert!(
            !stop.load(Ordering::SeqCst),
            "exit-wake exits on mail, not stop"
        );
    }

    #[test]
    fn exit_wake_blocks_then_wakes_within_5s_of_arriving_mail() {
        let root = setup();
        verbs::cmd_register(&root, "beta", "sb", None, Some(Lifecycle::ExitWake), None, Vec::new()).unwrap();

        let Spawned { buf, rx, .. } = spawn(&root, opts("beta", None, None, false));
        std::thread::sleep(Duration::from_millis(300));
        // Still blocked, still healthy (Empty = running; Ok would be an early exit).
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));

        let sent_at = Instant::now();
        verbs::cmd_send(&root, "alpha", "beta", "arriving mail").unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        let elapsed = sent_at.elapsed();
        assert!(elapsed < Duration::from_secs(5), "wake took {elapsed:?}");
        wait_for(&buf, &format!("has 1 new message"));
        // Mail untouched: a later drain still returns it.
        let inbox = seat::seat_dir(&root, "beta").join("inbox");
        assert_eq!(msg::list_msg_dir(&inbox).unwrap().len(), 1);
    }

    #[test]
    fn lifecycle_flag_overrides_meta_ephemerally() {
        let root = setup();
        verbs::cmd_register(&root, "beta", "sb", None, Some(Lifecycle::Poller), None, Vec::new()).unwrap();
        verbs::cmd_send(&root, "alpha", "beta", "override wake").unwrap();

        // Poller meta would stay up; the exit-wake override wakes and returns.
        let Spawned { stop, buf, rx } =
            spawn(&root, opts("beta", Some(Lifecycle::ExitWake), None, false));
        rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        wait_for(&buf, "1 new message");
        assert!(!stop.load(Ordering::SeqCst));

        // meta.json was not touched by the override.
        let meta = seat::read_meta(&seat::seat_dir(&root, "beta")).unwrap();
        assert_eq!(meta.lifecycle, Some(Lifecycle::Poller));
    }

    #[test]
    fn missing_lifecycle_refuses_before_claiming() {
        let root = setup();
        let dir = seat::seat_dir(&root, "beta");
        seat::create(&root, "beta").unwrap();
        // A meta with no lifecycle (register always writes one; hand-write).
        seat::write_meta(
            &dir,
            &seat::Meta {
                harness: "sb".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let err = run(
            &root,
            &opts("beta", None, None, false),
            &AtomicBool::new(false),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("lifecycle"), "{err}");
        assert!(matches!(err, SeatError::Arm(_)));
        assert_eq!(seat::read_wait_pid(&dir), None);
    }

    #[test]
    fn second_arm_reports_live_waiter_and_exits_zero() {
        let root = setup();
        verbs::cmd_register(&root, "beta", "sb", None, Some(Lifecycle::Poller), None, Vec::new()).unwrap();
        // A live holder: this test process itself.
        let dir = seat::seat_dir(&root, "beta");
        seat::write_wait_pid(&dir, std::process::id()).unwrap();

        let mut out = Vec::new();
        run(
            &root,
            &opts("beta", None, None, false),
            &AtomicBool::new(false),
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("already has a live waiter"),
            "reports the waiter: {text}"
        );
        assert!(text.contains(&std::process::id().to_string()), "{text}");
        assert!(!text.contains("waiting for mail"), "started nothing: {text}");
        // Refused arm did not steal the pidfile.
        assert_eq!(seat::read_wait_pid(&dir), Some(std::process::id()));
    }

    #[test]
    fn stale_pidfile_is_reclaimed_without_takeover() {
        let root = setup();
        verbs::cmd_register(&root, "beta", "sb", None, Some(Lifecycle::ExitWake), None, Vec::new()).unwrap();
        verbs::cmd_send(&root, "alpha", "beta", "reclaim me").unwrap();
        // A pid that cannot exist: stale pidfile must not block arming.
        seat::write_wait_pid(&seat::seat_dir(&root, "beta"), u32::MAX).unwrap();

        let mut out = Vec::new();
        run(
            &root,
            &opts("beta", None, None, false),
            &AtomicBool::new(false),
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("1 new message"), "{text}");
        assert_eq!(seat::read_wait_pid(&seat::seat_dir(&root, "beta")), None);
    }

    #[test]
    fn takeover_terms_live_waiter_and_takes_the_seat() {
        let root = setup();
        verbs::cmd_register(&root, "beta", "sb", None, Some(Lifecycle::ExitWake), None, Vec::new()).unwrap();
        verbs::cmd_send(&root, "alpha", "beta", "steal me").unwrap();

        // A real live waiter that is not this process.
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let dir = seat::seat_dir(&root, "beta");
        seat::write_wait_pid(&dir, child.id()).unwrap();

        let mut out = Vec::new();
        run(
            &root,
            &opts("beta", Some(Lifecycle::ExitWake), None, true),
            &AtomicBool::new(false),
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("1 new message"), "{text}");
        // The old waiter got SIGTERM (15), the new one woke and released.
        let status = child.wait().unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(15), "old waiter: {status}");
        assert_eq!(seat::read_wait_pid(&dir), None);
    }

    #[test]
    fn poller_wakes_on_each_arrival_and_stays_up_until_stopped() {
        let root = setup();
        verbs::cmd_register(&root, "beta", "sb", None, Some(Lifecycle::Poller), None, Vec::new()).unwrap();

        let Spawned { stop, buf, rx } = spawn(&root, opts("beta", None, None, false));
        std::thread::sleep(Duration::from_millis(300));
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));

        let sent_at = Instant::now();
        verbs::cmd_send(&root, "alpha", "beta", "poller ping").unwrap();
        wait_for(&buf, "has 1 new message");
        assert!(sent_at.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(300));
        // Notice delivered, waiter still up, mail never drained.
        let inbox = seat::seat_dir(&root, "beta").join("inbox");
        assert_eq!(msg::list_msg_dir(&inbox).unwrap().len(), 1);
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));

        // A second arrival wakes it again (two notices total).
        verbs::cmd_send(&root, "alpha", "beta", "poller pong").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while buf_str(&buf).matches("new message").count() < 2 {
            assert!(Instant::now() < deadline, "poller never woke twice");
            std::thread::sleep(Duration::from_millis(50));
        }

        stop.store(true, Ordering::SeqCst);
        rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        assert_eq!(seat::read_wait_pid(&seat::seat_dir(&root, "beta")), None);
    }

    #[test]
    fn poller_wakes_on_waiting_mail_at_arm_time() {
        let root = setup();
        verbs::cmd_register(&root, "beta", "sb", None, Some(Lifecycle::Poller), None, Vec::new()).unwrap();
        verbs::cmd_send(&root, "alpha", "beta", "already here").unwrap();

        let Spawned { stop, buf, rx } = spawn(&root, opts("beta", None, None, false));
        wait_for(&buf, "has 1 new message");
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));

        stop.store(true, Ordering::SeqCst);
        rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
    }

    #[test]
    fn exec_runs_on_each_wake_and_is_notice_plus_signal() {
        let root = setup();
        verbs::cmd_register(&root, "beta", "sb", None, Some(Lifecycle::Poller), None, Vec::new()).unwrap();
        verbs::cmd_send(&root, "alpha", "beta", "signal only").unwrap();

        let marker_dir = temp_root("serve-exec");
        let marker = marker_dir.join("marker");
        let cmd = format!("echo woken >> {}", marker.display());
        let Spawned { stop, buf, rx } = spawn(&root, opts("beta", None, Some(cmd), false));

        let deadline = Instant::now() + Duration::from_secs(5);
        while !marker.exists() {
            assert!(Instant::now() < deadline, "--exec never fired");
            std::thread::sleep(Duration::from_millis(50));
        }
        // The notice is printed too, and the mail is never drained.
        wait_for(&buf, "1 new message");
        let inbox = seat::seat_dir(&root, "beta").join("inbox");
        assert_eq!(msg::list_msg_dir(&inbox).unwrap().len(), 1);

        // Re-arm: a second send fires the command again (append proves 2 runs).
        verbs::cmd_send(&root, "alpha", "beta", "more signal").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::fs::read_to_string(&marker).unwrap().lines().count() < 2 {
            assert!(Instant::now() < deadline, "--exec did not re-arm");
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(msg::list_msg_dir(&inbox).unwrap().len(), 2, "never drained");

        stop.store(true, Ordering::SeqCst);
        rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        assert_eq!(seat::read_wait_pid(&seat::seat_dir(&root, "beta")), None);
    }

    #[test]
    fn exec_with_exit_wake_runs_once_and_exits_zero() {
        let root = setup();
        verbs::cmd_register(&root, "beta", "sb", None, Some(Lifecycle::ExitWake), None, Vec::new()).unwrap();
        verbs::cmd_send(&root, "alpha", "beta", "one shot").unwrap();

        let marker_dir = temp_root("serve-exec-ew");
        let marker = marker_dir.join("marker");
        let cmd = format!("echo woken >> {}", marker.display());
        let mut out = Vec::new();
        run(
            &root,
            &opts("beta", None, Some(cmd), false),
            &AtomicBool::new(false),
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("1 new message"), "{text}");
        assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 1);
        assert_eq!(seat::read_wait_pid(&seat::seat_dir(&root, "beta")), None);
    }

    #[test]
    fn unknown_seat_refuses() {
        let root = setup();
        let err = run(
            &root,
            &opts("ghost", Some(Lifecycle::ExitWake), None, false),
            &AtomicBool::new(false),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(matches!(err, SeatError::UnknownSeat(_)));
    }
}
