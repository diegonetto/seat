use crate::board;
use crate::error::{Result, SeatError};
use crate::msg;
use crate::seat;
use crate::verbs;
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The one shared session every seat runs in. Not stored on the board:
/// tmux is the record.
pub const SESSION: &str = "swarm";

/// The liveness format every session verb agrees on: tab-separated session, pane id, `@seat`,
/// `pane_dead`, `@cwd` (JSON string), `@cmd` (JSON array), `window_name`. Compact
/// JSON escapes tabs, so a tab inside a path or word cannot split a
/// field. Seat names cannot contain whitespace.
const PANE_FMT: &str =
    "#{session_name}\t#{pane_id}\t#{@seat}\t#{pane_dead}\t#{@cwd}\t#{@cmd}\t#{window_name}";

/// The window `gallery` builds so every live seat is on one screen.
const GALLERY_WINDOW: &str = "gallery";

/// Every tmux query funnels through here: SEAT_TMUX is the
/// program invoked instead of tmux, with the same arguments. `attach`
/// does not: see `tmux_attach`.
fn tmux_prog() -> String {
    std::env::var_os("SEAT_TMUX")
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tmux".to_string())
}

/// `gallery` attaches only when SEAT_TMUX is unset: with the
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

/// The one wording for a refused multiplexer call: the command line
/// as issued and tmux's own complaint from stderr. Each caller wraps
/// it in the error variant its reader expects.
fn tmux_failure_line(prog: &str, args: &[&str], stderr: &[u8]) -> String {
    format!(
        "`{} {}` failed: {}",
        prog,
        args.join(" "),
        String::from_utf8_lossy(stderr).trim()
    )
}

/// Run tmux and refuse to continue when it fails.
fn tmux_ok(prog: &str, args: &[&str]) -> Result<std::process::Output> {
    let out = tmux(prog, args)?;
    if !out.status.success() {
        return Err(SeatError::Tmux(tmux_failure_line(prog, args, &out.stderr)));
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

/// The liveness query: `list-panes -a -F` over every session.
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
/// and the stored ordered words.
struct Launch {
    cwd: String,
    cmd: Vec<String>,
}

/// Start-time checks for one target: a non-empty command, a
/// stored absolute directory, and a directory that is a directory.
fn preflight(root: &Path, name: &str) -> Result<Launch> {
    let meta = seat::read_meta_named(root, name)?;
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

/// Start one seat's stored command in its stored directory.
/// The session's first window is the `new-session` itself; every later
/// seat is a `new-window` in it. tmux execs the words directly: `-c`
/// names the directory and, after `--`, `/usr/bin/env --` plus the
/// stored words — always two or more arguments, so a one-word command
/// is never a shell line. The
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
            tmux_failure_line(prog, &args, &out.stderr),
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
    // A command that exits or cannot be run while `up` is still
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
/// given, inside the one `swarm` session. A seat that already
/// has a live pane is skipped: a second `up` opens no second session
/// and starts no second copy. Every not-running target is
/// preflighted before any start; one failure names the seat and starts
/// none. Named targets start in command-line order; a bare
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
            let meta = seat::read_meta_named(root, name)?;
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

/// `seat down [NAME...]` — kill those seats' panes. No names
/// stops every registered seat. Prints no message body and drains
/// nothing: unread mail waits for a later `drain`. A
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
        kill_seat_pane_if(n, None)?;
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
/// one window, then attach. With SEAT_TMUX set the names are
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
#[path = "session_tests.rs"]
mod tests;
