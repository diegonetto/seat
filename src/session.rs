use crate::board;
use crate::error::{Result, SeatError};
use crate::msg;
use crate::seat;
use crate::verbs;
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The one shared session every seat runs in. Not stored on the board:
/// tmux is the record (data model: Session).
pub const SESSION: &str = "swarm";

/// The liveness format every session verb agrees on (data model:
/// Session record): tab-separated session, pane id, `@seat`,
/// `pane_dead`, `@cwd` (JSON string), `@cmd` (JSON array). Compact
/// JSON escapes tabs, so a tab inside a path or word cannot split a
/// field. Seat names cannot contain whitespace.
const PANE_FMT: &str = "#{session_name}\t#{pane_id}\t#{@seat}\t#{pane_dead}\t#{@cwd}\t#{@cmd}";

/// Every tmux invocation funnels through here (T019): SEAT_TMUX is the
/// program invoked instead of tmux, with the same arguments.
fn tmux_prog() -> String {
    std::env::var_os("SEAT_TMUX")
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tmux".to_string())
}

/// `gallery` attaches only when SEAT_TMUX is unset (T018): with the
/// fake in place it prints the names and stops.
fn attach_allowed() -> bool {
    std::env::var_os("SEAT_TMUX")
        .map(|v| v.is_empty())
        .unwrap_or(true)
}

fn tmux(prog: &str, args: &[&str]) -> Result<std::process::Output> {
    std::process::Command::new(prog)
        .args(args)
        .output()
        .map_err(|e| SeatError::Tmux(format!("cannot run {prog:?}: {e}")))
}

/// Run tmux and refuse to continue when it fails.
fn tmux_ok(prog: &str, args: &[&str]) -> Result<std::process::Output> {
    let out = tmux(prog, args)?;
    if !out.status.success() {
        return Err(SeatError::Tmux(format!(
            "`{} {}` failed: {}",
            prog,
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(out)
}

/// One pane as the liveness query reports it. `dead` is `pane_dead`;
/// `cwd` and `cmd` are the launch copy `up` stamped on the pane (JSON
/// text, empty when never set).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    pub session: String,
    pub id: String,
    pub seat: String,
    pub dead: bool,
    pub cwd: String,
    pub cmd: String,
}

/// The liveness query (T021): `list-panes -a -F` over every session.
/// A pane with no `@seat` (a window this program did not open) yields
/// an empty third field and is skipped. A dead server is not an error:
/// nothing is running.
pub fn panes(prog: &str) -> Result<Vec<Pane>> {
    let out = tmux(prog, &["list-panes", "-a", "-F", PANE_FMT])?;
    if !out.status.success() {
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut fields = line.split('\t');
        match (fields.next(), fields.next(), fields.next()) {
            (Some(session), Some(id), Some(seat_name)) if !seat_name.is_empty() => {
                found.push(Pane {
                    session: session.to_string(),
                    id: id.to_string(),
                    seat: seat_name.to_string(),
                    dead: fields.next() == Some("1"),
                    cwd: fields.next().unwrap_or("").to_string(),
                    cmd: fields.next().unwrap_or("").to_string(),
                });
            }
            _ => continue,
        }
    }
    Ok(found)
}

/// The running seats: the names `gallery` prints and `status` labels
/// running. Only live panes in the `swarm` session count — a dead pane
/// is a stopped seat (data model: Running).
pub fn running_seats(prog: &str) -> Result<BTreeSet<String>> {
    Ok(panes(prog)?
        .into_iter()
        .filter(|p| p.session == SESSION && !p.dead)
        .map(|p| p.seat)
        .collect())
}

fn has_session(prog: &str) -> Result<bool> {
    Ok(tmux(prog, &["has-session", "-t", SESSION])?.status.success())
}

/// The launch `up` starts a seat with: the stored absolute directory
/// and the stored ordered words (data model: Launch).
struct Launch {
    cwd: String,
    cmd: Vec<String>,
}

/// Start-time checks for one target (FR-005): a non-empty command, a
/// stored absolute directory, and a directory that is a directory.
fn preflight(root: &Path, name: &str) -> Result<Launch> {
    let meta = seat::read_meta(&seat::seat_dir(root, name)).map_err(|e| match e {
        SeatError::GhostSeat(_) => SeatError::UnknownSeat(name.to_string()),
        other => other,
    })?;
    let cmd = meta
        .cmd
        .filter(|c| !c.is_empty())
        .ok_or_else(|| SeatError::NoCmd(name.to_string()))?;
    let cwd = meta
        .cwd
        .filter(|c| !c.is_empty())
        .ok_or_else(|| SeatError::NoCwd(name.to_string()))?;
    let path = PathBuf::from(&cwd);
    if !path.is_dir() {
        return Err(SeatError::NotADirectory {
            seat: name.to_string(),
            path,
        });
    }
    Ok(Launch { cwd, cmd })
}

/// Start one seat's stored command in its stored directory (T005).
/// The session's first window is the `new-session` itself; every later
/// seat is a `new-window` in it. tmux execs the words directly: `-c`
/// names the directory and, after `--`, `/usr/bin/env --` plus the
/// stored words — always two or more arguments, so a one-word command
/// is never a shell line (research: Passing the command to tmux). The
/// pane is stamped `@seat`, `@cwd` (JSON string), and `@cmd` (JSON
/// array of the stored words only): the launch as started, not as
/// stored now. No pin file is read; no shell is used.
fn start_seat(
    prog: &str,
    seat_name: &str,
    launch: &Launch,
    have_session: &mut bool,
) -> Result<()> {
    let mut args: Vec<&str> = if *have_session {
        vec!["new-window", "-d", "-t", SESSION, "-n", seat_name]
    } else {
        vec!["new-session", "-d", "-s", SESSION, "-n", seat_name]
    };
    args.push("-c");
    args.push(launch.cwd.as_str());
    args.extend(["-P", "-F", "#{pane_id}", "--", "/usr/bin/env", "--"]);
    args.extend(launch.cmd.iter().map(|s| s.as_str()));
    let out = tmux(prog, &args).map_err(|e| {
        SeatError::StartFailed(seat_name.to_string(), format!("cannot run multiplexer: {e}"))
    })?;
    if !out.status.success() {
        return Err(SeatError::StartFailed(
            seat_name.to_string(),
            format!(
                "`{} {}` failed: {}",
                prog,
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ));
    }
    *have_session = true;
    let pane_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if pane_id.is_empty() {
        return Err(SeatError::StartFailed(
            seat_name.to_string(),
            format!("{prog} printed no pane id"),
        ));
    }
    let cwd_json = serde_json::to_string(&launch.cwd)?;
    let cmd_json = serde_json::to_string(&launch.cmd)?;
    for (option, value) in [("@seat", seat_name), ("@cwd", &cwd_json), ("@cmd", &cmd_json)] {
        tmux_ok(prog, &["set-option", "-p", "-t", &pane_id, option, value])
            .map_err(|e| SeatError::StartFailed(seat_name.to_string(), e.to_string()))?;
    }
    // FR-011: a command that exits or cannot be run while `up` is still
    // in progress leaves that seat not running. Seats already started
    // by this `up` stay running: the caller stops at this error.
    let live = panes(prog)?
        .into_iter()
        .any(|p| p.session == SESSION && p.seat == seat_name && !p.dead);
    if !live {
        return Err(SeatError::StartFailed(
            seat_name.to_string(),
            "no live pane after start (the command exited or could not be run)".to_string(),
        ));
    }
    Ok(())
}

/// `seat up [NAME...]` — start the stored command of those registered
/// seats, or of every registered seat that has one when no name is
/// given, inside the one `swarm` session (T005). A seat that already
/// has a live pane is skipped: a second `up` opens no second session
/// and starts no second copy (FR-006). Every not-running target is
/// preflighted before any start; one failure names the seat and starts
/// none (FR-005). Named targets start in command-line order; a bare
/// `up` starts them sorted by name.
pub fn cmd_up(root: &Path, names: &[String]) -> Result<()> {
    board::ensure_board(root)?;
    let prog = tmux_prog();
    let registered = verbs::sorted_seat_names(root)?;
    let targets: Vec<String> = if names.is_empty() {
        // Bare up: only seats whose stored command is non-empty are
        // targets; mail-only seats are not mentioned and not errors.
        let mut with_cmd = Vec::new();
        for name in &registered {
            let meta = seat::read_meta(&seat::seat_dir(root, name)).map_err(|e| match e {
                SeatError::GhostSeat(_) => SeatError::UnknownSeat(name.clone()),
                other => other,
            })?;
            if meta.cmd.as_ref().is_some_and(|c| !c.is_empty()) {
                with_cmd.push(name.clone());
            }
        }
        with_cmd
    } else {
        let mut ordered: Vec<String> = Vec::new();
        for n in names {
            if !registered.contains(n) {
                return Err(SeatError::UnknownSeat(n.clone()));
            }
            if !ordered.contains(n) {
                ordered.push(n.clone());
            }
        }
        ordered
    };
    let running = running_seats(&prog)?;
    let mut starts: Vec<(String, Launch)> = Vec::new();
    for name in &targets {
        if running.contains(name) {
            continue; // already running: not checked, not started
        }
        starts.push((name.clone(), preflight(root, name)?));
    }
    let mut have_session = has_session(&prog)?;
    for (name, launch) in &starts {
        start_seat(&prog, name, launch, &mut have_session)?;
    }
    Ok(())
}

/// `seat down [NAME...]` — kill those seats' panes (T017). No names
/// stops every registered seat. Prints no message body and drains
/// nothing (FR-007): unread mail waits for a later `drain`. A
/// registered seat that is already stopped is success.
pub fn cmd_down(root: &Path, names: &[String]) -> Result<()> {
    board::ensure_board(root)?;
    let prog = tmux_prog();
    let registered = verbs::sorted_seat_names(root)?;
    // No names means every registered seat, the same shape as `up`.
    let targets: Vec<&str> = if names.is_empty() {
        registered.iter().map(|s| s.as_str()).collect()
    } else {
        for n in names {
            if !registered.contains(n) {
                return Err(SeatError::UnknownSeat(n.clone()));
            }
        }
        names.iter().map(|s| s.as_str()).collect()
    };
    let swarm: Vec<Pane> = panes(&prog)?
        .into_iter()
        .filter(|p| p.session == SESSION)
        .collect();
    // One kill per seat: a name twice on the command line is one pane.
    let mut seen = BTreeSet::new();
    for n in targets {
        if !seen.insert(n) {
            continue;
        }
        if let Some(pane) = swarm.iter().find(|p| p.seat == n) {
            tmux_ok(&prog, &["kill-pane", "-t", &pane.id])?;
        }
    }
    Ok(())
}

/// `seat gallery` — print the running seat names, one per line, then
/// attach to the session (T018). With SEAT_TMUX set the names are
/// printed and there is no attach. With nothing running it reports
/// `gallery: empty`, exits 0, and registers no seat.
pub fn cmd_gallery(root: &Path) -> Result<()> {
    board::ensure_board(root)?;
    let registered: BTreeSet<String> = verbs::sorted_seat_names(root)?.into_iter().collect();
    let mut out = std::io::stdout().lock();
    gallery(&tmux_prog(), attach_allowed(), &registered, &mut out)
}

/// The gallery body. `attach` is false whenever SEAT_TMUX is set.
/// Only registered seats count, so the printed set matches `status`.
fn gallery(
    prog: &str,
    attach: bool,
    registered: &BTreeSet<String>,
    out: &mut dyn Write,
) -> Result<()> {
    let running: BTreeSet<String> = running_seats(prog)?
        .into_iter()
        .filter(|name| registered.contains(name))
        .collect();
    if running.is_empty() {
        writeln!(out, "gallery: empty")?;
        return Ok(());
    }
    for name in &running {
        writeln!(out, "{name}")?;
    }
    if attach {
        let res = tmux(prog, &["attach", "-t", SESSION])?;
        if !res.status.success() {
            return Err(SeatError::Tmux(format!(
                "`{} attach -t {SESSION}` failed: {}",
                prog,
                String::from_utf8_lossy(&res.stderr).trim()
            )));
        }
    }
    Ok(())
}

/// The three states of FR-009, in priority order: running beats
/// stopped-with-unread, which beats needs-operator. A stopped seat
/// with an empty inbox is needs-operator. Unread means a non-empty
/// inbox (T022).
fn classify(running: bool, unread: usize) -> &'static str {
    match (running, unread) {
        (true, _) => "running",
        (false, n) if n > 0 => "stopped-with-unread",
        (false, _) => "needs-operator",
    }
}

/// One row of `status`: the seat name and its one state.
fn status_rows(root: &Path, prog: &str) -> Result<Vec<(String, &'static str)>> {
    board::ensure_board(root)?;
    let running = running_seats(prog)?;
    let mut rows = Vec::new();
    for name in verbs::sorted_seat_names(root)? {
        let is_running = running.contains(&name);
        let unread = msg::list_msg_dir(&seat::seat_dir(root, &name).join("inbox"))?.len();
        rows.push((name, classify(is_running, unread)));
    }
    Ok(rows)
}

/// `seat status` — one row per registered seat, sorted by name, as
/// `<name> <state>`. The running set is the same query `gallery`
/// prints (T021).
pub fn cmd_status(root: &Path) -> Result<()> {
    let rows = status_rows(root, &tmux_prog())?;
    let mut out = std::io::stdout().lock();
    for (name, state) in rows {
        writeln!(out, "{name} {state}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board;
    use crate::testutil::temp_root;
    use crate::verbs;

    /// The fake multiplexer the quickstart pairs with SEAT_TMUX. Its
    /// state file doubles as the record of every call it answered.
    fn fake_tmux() -> String {
        let p = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake-tmux.sh");
        assert!(Path::new(p).is_file(), "fake-tmux.sh must exist at {p}");
        p.to_string()
    }

    #[test]
    fn state_priority_is_running_then_unread_then_needs_operator() {
        assert_eq!(classify(true, 0), "running");
        assert_eq!(classify(true, 7), "running", "running beats unread");
        assert_eq!(classify(false, 1), "stopped-with-unread");
        assert_eq!(classify(false, 0), "needs-operator");
    }

    /// Serializes the tests that own SEAT_TMUX and SEAT_FAKE_STATE:
    /// process-wide env is safe under the parallel test threads only
    /// one such test runs at a time.
    static FAKE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The whole session story against the fake (T015's consumer).
    #[test]
    fn session_verbs_drive_the_fake_multiplexer() {
        let _guard = FAKE_LOCK.lock().unwrap();
        let root = temp_root("session");
        board::init(&root).unwrap();
        let cwd = root.display().to_string();
        for name in ["alpha", "beta", "gamma"] {
            verbs::cmd_register(
                &root,
                name,
                "sb",
                None,
                None,
                Some(cwd.clone()),
                vec!["/bin/sleep".into(), "30".into()],
            )
            .unwrap();
        }
        let state = root.join("tmux.state");
        std::env::set_var("SEAT_TMUX", fake_tmux());
        std::env::set_var("SEAT_FAKE_STATE", &state);

        // up with no names brings every registered seat (T016).
        cmd_up(&root, &[]).unwrap();
        let expected: BTreeSet<String> = ["alpha", "beta", "gamma"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(running_seats(&fake_tmux()).unwrap(), expected);

        // A second up reuses the session: no duplicate panes.
        cmd_up(&root, &[]).unwrap();
        assert_eq!(panes(&fake_tmux()).unwrap().len(), 3, "one pane per seat");

        // down kills the pane and prints nothing on stdout (T017);
        // already-stopped is success.
        cmd_down(&root, &["beta".to_string()]).unwrap();
        let running: Vec<String> = running_seats(&fake_tmux())
            .unwrap()
            .into_iter()
            .collect();
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

        // status: three seats, three states, sorted (T021/T022). beta
        // gets mail while stopped; gamma stopped with an empty inbox.
        verbs::cmd_send(&root, "alpha", "beta", "unread while stopped").unwrap();
        cmd_down(&root, &["gamma".to_string()]).unwrap();
        let rows = status_rows(&root, &fake_tmux()).unwrap();
        assert_eq!(
            rows,
            vec![
                ("alpha".to_string(), "running"),
                ("beta".to_string(), "stopped-with-unread"),
                ("gamma".to_string(), "needs-operator"),
            ]
        );

        // The running set matches what gallery prints (SC-005/FR-008),
        // and down drained nothing: beta's mail is still unread.
        let running_names: Vec<String> = rows
            .iter()
            .filter(|(_, s)| *s == "running")
            .map(|(n, _)| n.clone())
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

        // Empty gallery reports, exits 0, registers nothing (T018).
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
        // appear: gallery's set is status's running set (FR-008).
        let stray = std::process::Command::new(fake_tmux())
            .args([
                "new-window", "-d", "-t", "swarm", "-n", "stray", "-P", "-F", "#{pane_id}",
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
        gallery(&fake_tmux(), true, &registered, &mut Vec::new()).unwrap();
        let recorded = std::fs::read_to_string(&state).unwrap();
        assert!(recorded.contains("X|attach|swarm"), "{recorded}");

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

    /// The launch story (003 US1): `up` starts the stored command in
    /// the stored directory, once; a mail-only seat is not a bare-`up`
    /// target; one bad named target starts nothing; a failure mid-`up`
    /// leaves earlier seats running and later seats unstarted.
    #[test]
    fn up_starts_the_stored_command() {
        let _guard = FAKE_LOCK.lock().unwrap();
        let root = temp_root("session-launch");
        board::init(&root).unwrap();
        let cwd = root.display().to_string();
        let reg = |name: &str, cwd: Option<String>, cmd: Vec<String>| {
            verbs::cmd_register(&root, name, "sb", None, None, cwd, cmd).unwrap();
        };
        let sleep30 = || vec!["/bin/sleep".to_string(), "30".to_string()];
        reg("runner", Some(cwd.clone()), sleep30());
        reg("mailbox", None, Vec::new());
        reg("nocmd", Some(cwd.clone()), Vec::new());
        let state = root.join("tmux.state");
        std::env::set_var("SEAT_TMUX", fake_tmux());
        std::env::set_var("SEAT_FAKE_STATE", &state);

        // Bare up targets only seats that have a command (SC from
        // spec 003 US1 scenario 5): mailbox and nocmd are not started
        // and are not errors.
        cmd_up(&root, &[]).unwrap();
        let live: Vec<String> = running_seats(&fake_tmux())
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(live, vec!["runner".to_string()]);

        // The record shows the command words and the directory, once
        // each, as the JSON the data model promises.
        let listed = panes(&fake_tmux()).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].seat, "runner");
        assert!(!listed[0].dead);
        assert_eq!(listed[0].cwd, serde_json::to_string(&cwd).unwrap());
        assert_eq!(listed[0].cmd, "[\"/bin/sleep\",\"30\"]");

        // A second up does not start a second copy (FR-006).
        cmd_up(&root, &[]).unwrap();
        assert_eq!(panes(&fake_tmux()).unwrap().len(), 1);

        // Naming a seat with no command fails, names it, starts nothing.
        assert!(matches!(
            cmd_up(&root, &["nocmd".to_string()]),
            Err(SeatError::NoCmd(ref s)) if s == "nocmd"
        ));
        assert_eq!(panes(&fake_tmux()).unwrap().len(), 1);

        // One bad named target starts none of them (FR-005): zzz is
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

        // Named targets start in command-line order (T005), not sorted.
        cmd_down(&root, &[]).unwrap();
        cmd_up(&root, &["zzz".to_string(), "runner".to_string()]).unwrap();
        let order: Vec<String> = panes(&fake_tmux())
            .unwrap()
            .into_iter()
            .map(|p| p.seat)
            .collect();
        assert_eq!(order, vec!["zzz".to_string(), "runner".to_string()]);

        // A target that is already running is not checked and not
        // started (FR-005): runner's launch is now unusable, but the
        // named up of a running seat is a no-op, and the pane keeps
        // the launch it was started with (FR-007).
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

        // A failure mid-up (FR-011): die's pane is written dead, so the
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
        assert!(!listed.iter().any(|p| p.seat == "zzz"), "later target unstarted");

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
}
