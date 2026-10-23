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
const PANE_FMT: &str =
    "#{session_name}\t#{pane_id}\t#{@seat}\t#{pane_dead}\t#{@cwd}\t#{@cmd}\t#{window_name}";

/// The window `gallery` builds so every live seat is on one screen.
const GALLERY_WINDOW: &str = "gallery";

/// Every tmux query funnels through here (T019): SEAT_TMUX is the
/// program invoked instead of tmux, with the same arguments. `attach`
/// does not: see `tmux_attach`.
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

/// Attach to `session` on the caller's terminal.
///
/// `tmux()` uses `.output()`, which pipes stdout. tmux then exits with
/// `open terminal failed: not a terminal` after `gallery` has already
/// printed the pane names. Attach must inherit stdin, stdout, and
/// stderr. Do not route it through `tmux()`.
fn tmux_attach(prog: &str, target: &str) -> Result<()> {
    let status = std::process::Command::new(prog)
        .args(["attach", "-t", target])
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|e| SeatError::Tmux(format!("cannot run {prog:?}: {e}")))?;
    if status.success() {
        return Ok(());
    }
    Err(SeatError::Tmux(format!(
        "`{prog} attach -t {target}` failed: {status}"
    )))
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
    pub window: String,
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
                    window: fields.next().unwrap_or("").to_string(),
                });
            }
            _ => continue,
        }
    }
    Ok(found)
}

/// Live panes in the `swarm` session. `gallery` prints these names.
/// `status` calls this set `pane`. A dead pane is not in the set.
pub fn running_seats(prog: &str) -> Result<BTreeSet<String>> {
    Ok(panes(prog)?
        .into_iter()
        .filter(|p| p.session == SESSION && !p.dead)
        .map(|p| p.seat)
        .collect())
}

fn has_session(prog: &str) -> Result<bool> {
    Ok(tmux(prog, &["has-session", "-t", SESSION])?
        .status
        .success())
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
fn start_seat(prog: &str, seat_name: &str, launch: &Launch, have_session: &mut bool) -> Result<()> {
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
        SeatError::StartFailed(
            seat_name.to_string(),
            format!("cannot run multiplexer: {e}"),
        )
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
    for (option, value) in [
        ("@seat", seat_name),
        ("@cwd", &cwd_json),
        ("@cmd", &cmd_json),
    ] {
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
    // One kill per seat: a name twice on the command line is one pane.
    let mut seen = BTreeSet::new();
    for n in targets {
        if !seen.insert(n) {
            continue;
        }
        kill_seat_pane(n)?;
    }
    Ok(())
}

/// The `down` path for one pane: kill this seat's own pane in the
/// `swarm` session (dead-but-listed counts; a seat with no pane is a
/// no-op). The dispatcher's idle sleep stops a pane through here too —
/// the same command and the same target, never a signal by pid, never
/// a pane the board did not start. `only_if` names the pane id the
/// caller's decision was about: the search then skips dead-but-listed
/// panes, so a dead listing never shields the live pane the decision
/// named, and a live pane that is another pane — a replacement under
/// the same seat name — is left standing. Without `only_if`
/// (`cmd_down`) the first pane listed for the seat dies, dead-but-
/// listed included. True when a pane was killed.
pub fn kill_seat_pane_if(name: &str, only_if: Option<&str>) -> Result<bool> {
    let prog = tmux_prog();
    let swarm: Vec<Pane> = panes(&prog)?
        .into_iter()
        .filter(|p| p.session == SESSION)
        .collect();
    if let Some(pane) = swarm
        .iter()
        .find(|p| p.seat == name && (only_if.is_none() || !p.dead))
    {
        if only_if.is_some_and(|id| pane.id != id) {
            return Ok(false); // not the pane the decision named
        }
        tmux_ok(&prog, &["kill-pane", "-t", &pane.id])?;
        return Ok(true);
    }
    Ok(false)
}

/// `cmd_down`'s one-pane kill, by seat name alone.
pub fn kill_seat_pane(name: &str) -> Result<bool> {
    kill_seat_pane_if(name, None)
}

/// Does this seat have a live pane in the `swarm` session right now
/// (the same query `status` uses; a dead pane is not up)? `None` when
/// the multiplexer cannot be asked at all — the caller leaves any
/// idle-clock fact untouched, because unknown is never permission to
/// sleep.
pub fn pane_is_up(seat_name: &str) -> Option<bool> {
    running_seats(&tmux_prog())
        .ok()
        .map(|running| running.contains(seat_name))
}

/// The id of this seat's live pane in the `swarm` session — the pane
/// the idle clock times. `Some(None)`: asked, and the seat has no live
/// pane. `None`: the multiplexer cannot be asked, which changes
/// nothing (unknown is never permission to sleep and never a clock
/// reset).
pub fn live_pane_id(seat_name: &str) -> Option<Option<String>> {
    panes(&tmux_prog()).ok().map(|all| {
        all.into_iter()
            .find(|p| p.session == SESSION && p.seat == seat_name && !p.dead)
            .map(|p| p.id)
    })
}

/// `seat gallery` — print the live seat names, tile those panes into
/// one window, then attach (T018). With SEAT_TMUX set the names are
/// printed and there is no tile and no attach. With no pane it reports
/// `gallery: empty`, exits 0, and registers no seat. Awake and
/// unattended seats are not listed and their panes are not moved.
pub fn cmd_gallery(root: &Path) -> Result<()> {
    board::ensure_board(root)?;
    let registered: BTreeSet<String> = verbs::sorted_seat_names(root)?.into_iter().collect();
    let mut out = std::io::stdout().lock();
    gallery(&tmux_prog(), attach_allowed(), &registered, &mut out)
}

/// The gallery body. `attach` is false whenever SEAT_TMUX is set.
/// Only registered seats count, so the printed set is the seats whose
/// presence is `pane`.
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
        out.flush()?;
        tile_seats(prog, &running)?;
        tmux_attach(prog, &format!("{SESSION}:{GALLERY_WINDOW}"))?;
    }
    Ok(())
}

/// Join every live registered seat pane into one window and tile it.
///
/// The join target is a pane id, the same way the previous gallery did
/// it. A pane that is not in `seats` stays where it is. The resize hook
/// re-tiles after a size change and leaves a zoomed pane alone, so one
/// seat can still fill the screen. Mouse is on so a click focuses a pane.
fn tile_seats(prog: &str, seats: &BTreeSet<String>) -> Result<()> {
    let mut mine: Vec<Pane> = panes(prog)?
        .into_iter()
        .filter(|p| p.session == SESSION && !p.dead && seats.contains(&p.seat))
        .collect();
    mine.sort_by(|a, b| a.seat.cmp(&b.seat));
    if mine.is_empty() {
        return Ok(());
    }
    let dest_id = mine
        .iter()
        .find(|p| p.window == GALLERY_WINDOW)
        .map(|p| p.id.clone())
        .unwrap_or_else(|| mine[0].id.clone());
    let dest_window = mine
        .iter()
        .find(|p| p.id == dest_id)
        .map(|p| p.window.clone())
        .unwrap_or_default();
    for pane in &mine {
        if pane.id == dest_id || pane.window == dest_window {
            continue;
        }
        tmux_ok(prog, &["join-pane", "-s", &pane.id, "-t", &dest_id])?;
    }
    tmux_ok(prog, &["rename-window", "-t", &dest_id, GALLERY_WINDOW])?;
    let target = format!("{SESSION}:{GALLERY_WINDOW}");
    // A window has one name. The seat name stays on the pane as @seat,
    // and the border shows that. Programs rewrite pane titles.
    tmux_ok(
        prog,
        &[
            "set-option",
            "-w",
            "-t",
            &target,
            "pane-border-status",
            "top",
        ],
    )?;
    tmux_ok(
        prog,
        &[
            "set-option",
            "-w",
            "-t",
            &target,
            "pane-border-format",
            "#{?pane_active,#[reverse],}#{@seat}",
        ],
    )?;
    tmux_ok(prog, &["select-layout", "-t", &target, "tiled"])?;
    let hook =
        format!("if-shell -F '#{{window_zoomed_flag}}' '' 'select-layout -t {target} tiled'");
    tmux_ok(
        prog,
        &["set-hook", "-w", "-t", &target, "window-resized", &hook],
    )?;
    tmux_ok(prog, &["set-option", "-t", SESSION, "mouse", "on"])?;
    tmux_ok(prog, &["select-window", "-t", &target])?;
    Ok(())
}

/// One `seat status` row. Presence and the mail count are separate.
#[derive(Debug, PartialEq, Eq)]
struct StatusRow {
    name: String,
    presence: &'static str,
    mail: usize,
}

/// Presence, separate from the mail count: `pane` (a live swarm
/// pane), `asleep` (the dispatcher's own sleep mark and no pane),
/// `awake` (a dispatcher delivery to the bound session within 10
/// minutes), else `unattended` (specs/status-presence.md). The
/// dispatcher's words outrank the delivery's: a threshold under ten
/// minutes still prints `asleep`. Mail never changes the word.
fn presence(in_pane: bool, asleep: bool, awake: bool) -> &'static str {
    if in_pane {
        "pane"
    } else if asleep {
        "asleep"
    } else if awake {
        "awake"
    } else {
        "unattended"
    }
}

/// One row of `status` per registered seat, sorted by name.
/// `mail` is the inbox file count.
fn status_rows(root: &Path, prog: &str) -> Result<Vec<StatusRow>> {
    board::ensure_board(root)?;
    let running = running_seats(prog)?;
    let mut rows = Vec::new();
    for name in verbs::sorted_seat_names(root)? {
        let dir = seat::seat_dir(root, &name);
        let mail = msg::list_msg_dir(&dir.join("inbox"))?.len();
        let in_pane = running.contains(&name);
        let asleep = crate::dstate::is_asleep(root, &name); // a local read
        let awake = crate::dstate::awake(root, &name); // a local read
        rows.push(StatusRow {
            name,
            presence: presence(in_pane, asleep, awake),
            mail,
        });
    }
    Ok(rows)
}

/// `seat status` — one row per registered seat, sorted by name, as
/// `<name> <presence> mail=<N>`. The pane set is the same query
/// `gallery` prints.
pub fn cmd_status(root: &Path) -> Result<()> {
    let rows = status_rows(root, &tmux_prog())?;
    let mut out = std::io::stdout().lock();
    for row in rows {
        writeln!(out, "{} {} mail={}", row.name, row.presence, row.mail)?;
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
        let _guard = crate::testutil::ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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

    /// Serializes the tests that own SEAT_TMUX and SEAT_FAKE_STATE
    /// (the process-wide env lock in testutil, shared with the
    /// dispatcher tests that stub PATH): process-wide env is safe
    /// under the parallel test threads only one such test runs at a
    /// time.

    /// Presence and the mail count are separate: a live pane is
    /// `pane`, everything else — mail or not — is `unattended` (this
    /// board has no bindings, so no seat is awake). Mail is the inbox
    /// file count and does not change presence.
    #[test]
    fn status_presence_and_mail_are_separate() {
        let _guard = crate::testutil::ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
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

    /// The whole session story against the fake (T015's consumer).
    #[test]
    fn session_verbs_drive_the_fake_multiplexer() {
        let _guard = crate::testutil::ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
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

    /// The launch story (003 US1): `up` starts the stored command in
    /// the stored directory, once; a mail-only seat is not a bare-`up`
    /// target; one bad named target starts nothing; a failure mid-`up`
    /// leaves earlier seats running and later seats unstarted.
    #[test]
    fn up_starts_the_stored_command() {
        let _guard = crate::testutil::ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
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

        // Bare up targets only seats that have a command (SC from
        // spec 003 US1 scenario 5): mailbox and nocmd are not started
        // and are not errors.
        cmd_up(&root, &[]).unwrap();
        let live: Vec<String> = running_seats(&fake_tmux()).unwrap().into_iter().collect();
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
}
