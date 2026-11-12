use super::*;
use crate::board;
use crate::testutil::{fake_tmux, temp_root};
use crate::verbs;

#[test]
fn presence_is_pane_asleep_awake_or_unattended() {
    assert_eq!(presence(true, true, true), "pane");
    assert_eq!(presence(true, false, false), "pane");
    assert_eq!(presence(false, true, false), "asleep");
    // The sleep mark outranks a delivery inside the awake window:
    // a threshold under ten minutes still prints asleep.
    assert_eq!(presence(false, true, true), "asleep");
    assert_eq!(presence(false, false, true), "awake");
    assert_eq!(presence(false, false, false), "unattended");
}

/// `status` prints the fourth word from the dispatcher's own
/// record: a seat whose dispatch.json says `asleep` and whose pane
/// is gone prints `asleep`; a pane that came back outranks the
/// record (the wake or a manual `up` happened first).
#[test]
fn status_prints_asleep_from_the_record() {
    let _guard = crate::testutil::env_lock();
    let root = temp_root("status-asleep");
    board::init(&root).unwrap();
    let cwd = root.display().to_string();
    verbs::cmd_register(
        &root,
        "snorlax",
        "sb",
        None,
        Some(cwd),
        vec!["/bin/sleep".into(), "30".into()],
        Some("30m".into()),
    )
    .unwrap();
    let state = root.join("tmux.state");
    std::env::set_var("SEAT_TMUX", fake_tmux());
    std::env::set_var("SEAT_FAKE_STATE", &state);

    std::fs::write(
        seat::seat_dir(&root, "snorlax").join("dispatch.json"),
        "{\"asleep\":{\"at\":\"2026-10-23T00:00:00Z\",\"reason\":\"idle 30m\"}}",
    )
    .unwrap();
    let rows = status_rows(&root, &fake_tmux()).unwrap();
    assert_eq!(
        rows,
        vec![StatusRow {
            name: "snorlax".into(),
            presence: "asleep",
            mail: 0
        }]
    );

    // A live pane outranks the record.
    cmd_up(&root, &["snorlax".to_string()]).unwrap();
    assert_eq!(
        status_rows(&root, &fake_tmux()).unwrap()[0].presence,
        "pane",
        "a pane that came back outranks the sleep mark"
    );
    cmd_down(&root, &["snorlax".to_string()]).unwrap();
    assert_eq!(status_rows(&root, &fake_tmux()).unwrap()[0].presence, "asleep");

    std::env::remove_var("SEAT_TMUX");
    std::env::remove_var("SEAT_FAKE_STATE");
}

/// Presence and the mail count are separate: a live pane is
/// `pane`, everything else — mail or not — is `unattended` (this
/// board has no bindings, so no seat is awake). Mail is the inbox
/// file count and does not change presence.
#[test]
fn status_presence_and_mail_are_separate() {
    let _guard = crate::testutil::env_lock();
    let root = temp_root("status-presence");
    board::init(&root).unwrap();
    let cwd = root.display().to_string();
    verbs::cmd_register(
        &root,
        "paned",
        "sb",
        None,
        Some(cwd),
        vec!["/bin/sleep".into(), "30".into()],
        None,
    )
    .unwrap();
    verbs::cmd_register(&root, "quiet", "sb", None, None, vec![], None).unwrap();
    verbs::cmd_register(&root, "stocked", "sb", None, None, vec![], None).unwrap();
    let state = root.join("tmux.state");
    std::env::set_var("SEAT_TMUX", fake_tmux());
    std::env::set_var("SEAT_FAKE_STATE", &state);

    cmd_up(&root, &["paned".to_string()]).unwrap();
    verbs::cmd_send(&root, Some("quiet"), "paned", "while up", None).unwrap();
    verbs::cmd_send(&root, Some("quiet"), "stocked", "one", None).unwrap();
    verbs::cmd_send(&root, Some("paned"), "stocked", "two", None).unwrap();

    let rows = status_rows(&root, &fake_tmux()).unwrap();
    assert_eq!(
        rows,
        vec![
            StatusRow {
                name: "paned".into(),
                presence: "pane",
                mail: 1
            },
            StatusRow {
                name: "quiet".into(),
                presence: "unattended",
                mail: 0
            },
            StatusRow {
                name: "stocked".into(),
                presence: "unattended",
                mail: 2
            },
        ]
    );
}

/// The whole session story against the fake.
#[test]
fn session_verbs_drive_the_fake_multiplexer() {
    let _guard = crate::testutil::env_lock();
    let root = temp_root("session");
    board::init(&root).unwrap();
    let cwd = root.display().to_string();
    for name in ["alpha", "beta", "gamma"] {
        verbs::cmd_register(
            &root,
            name,
            "sb",
            None,
            Some(cwd.clone()),
            vec!["/bin/sleep".into(), "30".into()],
            None,
        )
        .unwrap();
    }
    let state = root.join("tmux.state");
    std::env::set_var("SEAT_TMUX", fake_tmux());
    std::env::set_var("SEAT_FAKE_STATE", &state);

    // up with no names brings every registered seat.
    cmd_up(&root, &[]).unwrap();
    let expected: BTreeSet<String> = ["alpha", "beta", "gamma"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(running_seats(&fake_tmux()).unwrap(), expected);

    // A second up reuses the session: no duplicate panes.
    cmd_up(&root, &[]).unwrap();
    assert_eq!(panes(&fake_tmux()).unwrap().len(), 3, "one pane per seat");

    // down kills the pane and prints nothing on stdout;
    // already-stopped is success.
    cmd_down(&root, &["beta".to_string()]).unwrap();
    let running: Vec<String> = running_seats(&fake_tmux()).unwrap().into_iter().collect();
    assert_eq!(running, vec!["alpha".to_string(), "gamma".to_string()]);
    cmd_down(&root, &["beta".to_string()]).unwrap();

    // Unknown names exit nonzero and name the seat.
    assert!(matches!(
        cmd_up(&root, &["ghost".to_string()]),
        Err(SeatError::UnknownSeat(ref s)) if s == "ghost"
    ));
    assert!(matches!(
        cmd_down(&root, &["ghost".to_string()]),
        Err(SeatError::UnknownSeat(ref s)) if s == "ghost"
    ));

    // status: three seats, sorted. beta has mail and no pane;
    // gamma has an empty inbox and no pane. Mail does not change
    // presence, so both are unattended.
    verbs::cmd_send(&root, Some("alpha"), "beta", "unread while stopped", None).unwrap();
    cmd_down(&root, &["gamma".to_string()]).unwrap();
    let rows = status_rows(&root, &fake_tmux()).unwrap();
    assert_eq!(
        rows,
        vec![
            StatusRow {
                name: "alpha".into(),
                presence: "pane",
                mail: 0
            },
            StatusRow {
                name: "beta".into(),
                presence: "unattended",
                mail: 1
            },
            StatusRow {
                name: "gamma".into(),
                presence: "unattended",
                mail: 0
            },
        ]
    );

    // The pane set matches what gallery prints, and down drained
    // nothing: beta's mail is still in the inbox.
    let running_names: Vec<String> = rows
        .iter()
        .filter(|row| row.presence == "pane")
        .map(|row| row.name.clone())
        .collect();
    assert_eq!(running_names, vec!["alpha".to_string()]);
    assert_eq!(
        msg::list_msg_dir(&seat::seat_dir(&root, "beta").join("inbox"))
            .unwrap()
            .len(),
        1,
        "down kept beta's mail for a later drain"
    );

    // SEAT_TMUX is set: gallery must not have attached.
    let recorded = std::fs::read_to_string(&state).unwrap();
    assert!(!recorded.contains("X|attach"), "{recorded}");

    // Empty gallery reports, exits 0, registers nothing.
    cmd_down(&root, &["alpha".to_string()]).unwrap();
    let registered: BTreeSet<String> = verbs::sorted_seat_names(&root)
        .unwrap()
        .into_iter()
        .collect();
    let mut buf = Vec::new();
    gallery(&fake_tmux(), false, &registered, &mut buf).unwrap();
    assert_eq!(String::from_utf8(buf).unwrap(), "gallery: empty\n");
    assert_eq!(verbs::sorted_seat_names(&root).unwrap().len(), 3);

    // The attach path itself: the fake's attach exits 0 at once
    // and records the call, so this cannot hang.
    cmd_up(&root, &["alpha".to_string()]).unwrap();
    // A pane tagged with a name that is not registered must not
    // appear: gallery's set is status's running set.
    let stray = std::process::Command::new(fake_tmux())
        .args([
            "new-window",
            "-d",
            "-t",
            "swarm",
            "-n",
            "stray",
            "-P",
            "-F",
            "#{pane_id}",
        ])
        .output()
        .unwrap();
    let stray_id = String::from_utf8_lossy(&stray.stdout).trim().to_string();
    assert!(stray.status.success(), "{stray_id}");
    let tagged = std::process::Command::new(fake_tmux())
        .args(["set-option", "-p", "-t", &stray_id, "@seat", "stray"])
        .status()
        .unwrap();
    assert!(tagged.success());
    let mut shown = Vec::new();
    gallery(&fake_tmux(), false, &registered, &mut shown).unwrap();
    assert_eq!(String::from_utf8(shown).unwrap(), "alpha\n");
    cmd_up(&root, &["beta".to_string()]).unwrap();
    gallery(&fake_tmux(), true, &registered, &mut Vec::new()).unwrap();
    let recorded = std::fs::read_to_string(&state).unwrap();
    assert!(recorded.contains("X|attach|swarm:gallery\n"), "{recorded}");
    assert!(
        recorded.contains("X|layout|tiled|swarm:gallery"),
        "{recorded}"
    );
    assert!(recorded.contains("window_zoomed_flag"), "{recorded}");
    assert!(
        recorded.contains("X|option|swarm:gallery|pane-border-status|top"),
        "{recorded}"
    );
    assert!(
        recorded.contains("pane-border-format|#{?pane_active,#[reverse],}#{@seat}"),
        "{recorded}"
    );
    let after = panes(&fake_tmux()).unwrap();
    for name in ["alpha", "beta"] {
        let pane = after.iter().find(|p| p.seat == name).unwrap();
        assert_eq!(pane.window, "gallery", "{name} was left in its own window");
    }
    let stray_pane = after.iter().find(|p| p.id == stray_id).unwrap();
    assert_eq!(stray_pane.window, "stray", "unregistered pane was moved");

    // Drop the unregistered pane so the next check is about seats.
    let killed = std::process::Command::new(fake_tmux())
        .args(["kill-pane", "-t", &stray_id])
        .status()
        .unwrap();
    assert!(killed.success());

    // No names stops every running seat.
    cmd_up(&root, &[]).unwrap();
    cmd_down(&root, &[]).unwrap();
    assert!(running_seats(&fake_tmux()).unwrap().is_empty());

    std::env::remove_var("SEAT_TMUX");
    std::env::remove_var("SEAT_FAKE_STATE");
}

/// `gallery` used to capture tmux's stdout, so attach failed with
/// "not a terminal" on a real shell. The probe is re-exec'd with a
/// pty as its terminal and a script that exits 0 only when both
/// stdin and stdout are ttys.
#[test]
fn attach_inherits_the_callers_terminal() {
    if std::env::var_os("SEAT_TTY_PROBE").is_some() {
        let prog = std::env::var("SEAT_TTY_PROBE_PROG").expect("probe prog");
        tmux_attach(&prog, SESSION).expect("attach must see a terminal");
        return;
    }

    let dir = temp_root("attach-tty");
    let prog = dir.join("tty-check.sh");
    std::fs::write(
        &prog,
        "#!/bin/sh\nif [ -t 0 ] && [ -t 1 ]; then exit 0; fi\nexit 1\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&prog, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, 0, "openpty");

    let bin = std::env::current_exe().unwrap();
    let mut cmd = std::process::Command::new(&bin);
    cmd.arg("--exact")
        .arg("session::tests::attach_inherits_the_callers_terminal")
        .arg("--test-threads=1")
        .env("SEAT_TTY_PROBE", "1")
        .env("SEAT_TTY_PROBE_PROG", &prog);
    unsafe {
        use std::os::unix::io::FromRawFd;
        use std::os::unix::process::CommandExt;
        cmd.stdin(std::process::Stdio::from_raw_fd(libc::dup(slave)))
            .stdout(std::process::Stdio::from_raw_fd(libc::dup(slave)))
            .stderr(std::process::Stdio::from_raw_fd(libc::dup(slave)))
            .pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        libc::close(slave);
    }

    let mut child = cmd.spawn().expect("spawn tty probe");
    // Command keeps its slave fds until dropped. The master read
    // only sees EOF once those fds close.
    drop(cmd);
    let drain = std::thread::spawn(move || {
        let mut buf = [0u8; 512];
        let mut acc = Vec::new();
        loop {
            let n =
                unsafe { libc::read(master, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n <= 0 {
                break;
            }
            acc.extend_from_slice(&buf[..n as usize]);
        }
        unsafe { libc::close(master) };
        acc
    });
    let status = child.wait().unwrap();
    let acc = drain.join().unwrap();
    let text = String::from_utf8_lossy(&acc);
    assert!(
        status.success(),
        "attach child failed: {status}, output:\n{text}"
    );
    assert!(
        text.contains("attach_inherits_the_callers_terminal"),
        "probe did not run the attach test:\n{text}"
    );
}

/// The launch story: `up` starts the stored command in
/// the stored directory, once; a mail-only seat is not a bare-`up`
/// target; one bad named target starts nothing; a failure mid-`up`
/// leaves earlier seats running and later seats unstarted.
#[test]
fn up_starts_the_stored_command() {
    let _guard = crate::testutil::env_lock();
    let root = temp_root("session-launch");
    board::init(&root).unwrap();
    let cwd = root.display().to_string();
    let reg = |name: &str, cwd: Option<String>, cmd: Vec<String>| {
        verbs::cmd_register(&root, name, "sb", None, cwd, cmd, None).unwrap();
    };
    let sleep30 = || vec!["/bin/sleep".to_string(), "30".to_string()];
    reg("runner", Some(cwd.clone()), sleep30());
    reg("mailbox", None, Vec::new());
    reg("nocmd", Some(cwd.clone()), Vec::new());
    let state = root.join("tmux.state");
    std::env::set_var("SEAT_TMUX", fake_tmux());
    std::env::set_var("SEAT_FAKE_STATE", &state);

    // Bare up targets only seats that have a command: mailbox and
    // nocmd are not started and are not errors.
    cmd_up(&root, &[]).unwrap();
    let live: Vec<String> = running_seats(&fake_tmux()).unwrap().into_iter().collect();
    assert_eq!(live, vec!["runner".to_string()]);

    // The record shows the command words and the directory, once
    // each, as JSON.
    let listed = panes(&fake_tmux()).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].seat, "runner");
    assert!(!listed[0].dead);
    assert_eq!(listed[0].cwd, serde_json::to_string(&cwd).unwrap());
    assert_eq!(listed[0].cmd, "[\"/bin/sleep\",\"30\"]");

    // A second up does not start a second copy.
    cmd_up(&root, &[]).unwrap();
    assert_eq!(panes(&fake_tmux()).unwrap().len(), 1);

    // Naming a seat with no command fails, names it, starts nothing.
    assert!(matches!(
        cmd_up(&root, &["nocmd".to_string()]),
        Err(SeatError::NoCmd(ref s)) if s == "nocmd"
    ));
    assert_eq!(panes(&fake_tmux()).unwrap().len(), 1);

    // One bad named target starts none of them: zzz is
    // startable, nodir's directory is not a directory.
    reg("zzz", Some(cwd.clone()), sleep30());
    reg(
        "nodir",
        Some(root.join("no-such-dir").display().to_string()),
        sleep30(),
    );
    assert!(matches!(
        cmd_up(&root, &["nodir".to_string(), "zzz".to_string()]),
        Err(SeatError::NotADirectory { ref seat, .. }) if seat == "nodir"
    ));
    assert_eq!(panes(&fake_tmux()).unwrap().len(), 1, "nothing started");

    // Named targets start in command-line order, not sorted.
    cmd_down(&root, &[]).unwrap();
    cmd_up(&root, &["zzz".to_string(), "runner".to_string()]).unwrap();
    let order: Vec<String> = panes(&fake_tmux())
        .unwrap()
        .into_iter()
        .map(|p| p.seat)
        .collect();
    assert_eq!(order, vec!["zzz".to_string(), "runner".to_string()]);

    // A target that is already running is not checked and not
    // started: runner's launch is now unusable, but the
    // named up of a running seat is a no-op, and the pane keeps
    // the launch it was started with.
    reg(
        "runner",
        Some(root.join("gone-dir").display().to_string()),
        vec!["/other".to_string()],
    );
    cmd_up(&root, &["runner".to_string()]).unwrap();
    let pane = panes(&fake_tmux())
        .unwrap()
        .into_iter()
        .find(|p| p.seat == "runner")
        .unwrap();
    assert_eq!(pane.cwd, serde_json::to_string(&cwd).unwrap());
    assert_eq!(pane.cmd, "[\"/bin/sleep\",\"30\"]");
    reg("runner", Some(cwd.clone()), sleep30());

    // A failure mid-up: die's pane is written dead, so the
    // follow-up list has no live pane for it. Seats already started
    // stay running; later targets are never started.
    reg("die", Some(cwd.clone()), vec!["seat-die".to_string()]);
    cmd_down(&root, &[]).unwrap();
    assert!(matches!(
        cmd_up(
            &root,
            &[
                "runner".to_string(),
                "die".to_string(),
                "zzz".to_string()
            ]
        ),
        Err(SeatError::StartFailed(ref s, _)) if s == "die"
    ));
    let listed = panes(&fake_tmux()).unwrap();
    assert_eq!(
        running_seats(&fake_tmux()).unwrap(),
        BTreeSet::from(["runner".to_string()]),
        "die is not running, earlier seats stay"
    );
    assert!(listed.iter().any(|p| p.seat == "die" && p.dead));
    assert!(
        !listed.iter().any(|p| p.seat == "zzz"),
        "later target unstarted"
    );

    // The multiplexer refusing the spawn (seat-fail) is the same
    // failure: named, nonzero, no pane.
    reg("nope", Some(cwd.clone()), vec!["seat-fail".to_string()]);
    assert!(matches!(
        cmd_up(&root, &["nope".to_string()]),
        Err(SeatError::StartFailed(ref s, _)) if s == "nope"
    ));
    assert!(!panes(&fake_tmux())
        .unwrap()
        .iter()
        .any(|p| p.seat == "nope"));

    std::env::remove_var("SEAT_TMUX");
    std::env::remove_var("SEAT_FAKE_STATE");
}

/// A refused multiplexer call inside `up` names the seat and the
/// full command line it tried: the complete `StartFailed` message,
/// byte for byte (the spawn refusal in start_seat).
#[test]
fn up_failure_message_pins_the_start_seat_refusal() {
    let _guard = crate::testutil::ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = temp_root("session-fail-line");
    board::init(&root).unwrap();
    let cwd = root.display().to_string();
    verbs::cmd_register(
        &root,
        "nope",
        "sb",
        None,
        Some(cwd.clone()),
        vec!["seat-fail".to_string()],
        None,
    )
    .unwrap();
    let state = root.join("tmux.state");
    std::env::set_var("SEAT_TMUX", fake_tmux());
    std::env::set_var("SEAT_FAKE_STATE", &state);

    let prog = fake_tmux();
    let err = cmd_up(&root, &["nope".to_string()]).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "up: seat nope did not start: `{prog} new-session -d -s swarm -n nope \
             -c {cwd} -P -F #{{pane_id}} -- /usr/bin/env -- seat-fail` failed: \
             fake-tmux: launch refused: seat-fail"
        )
    );

    std::env::remove_var("SEAT_TMUX");
    std::env::remove_var("SEAT_FAKE_STATE");
}

/// The same refusal wording, in its other wrapper: a tmux call
/// that fails outside a seat start is `Tmux`, complete message
/// byte for byte (the check in tmux_ok).
#[test]
fn up_failure_message_pins_the_tmux_ok_refusal() {
    let _guard = crate::testutil::ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = temp_root("session-fail-line-ok");
    let state = root.join("tmux.state");
    std::env::set_var("SEAT_TMUX", fake_tmux());
    std::env::set_var("SEAT_FAKE_STATE", &state);

    let prog = fake_tmux();
    let err = tmux_ok(&prog, &["kill-pane", "-t", "%999"]).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("tmux: `{prog} kill-pane -t %999` failed: fake-tmux: no such pane: %999")
    );

    std::env::remove_var("SEAT_TMUX");
    std::env::remove_var("SEAT_FAKE_STATE");
}
