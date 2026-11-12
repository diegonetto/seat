use crate::board::{self, valid_name};
use crate::error::{Result, SeatError};
use crate::identity;
use crate::msg;
use crate::seat;
use std::path::Path;

pub fn cmd_init(root: &Path) -> Result<()> {
    board::init(root)?;
    println!("initialized seat board at {}", root.display());
    Ok(())
}

/// Register a seat: layout + one-time token mint (O_EXCL) + meta. A
/// duplicate register keeps that seat's token, inbox, and meta — only
/// launch fields actually passed change, and a refused register (a
/// relative or empty `--cwd`, or an unreadable `--idle-after`)
/// writes nothing. `--idle-after` follows the same rule as the launch
/// fields: omitted keeps the stored value, a passed value updates it
/// even when cwd and command are omitted.
pub fn cmd_register(
    root: &Path,
    name: &str,
    harness: &str,
    model: Option<String>,
    cwd: Option<String>,
    cmd: Vec<String>,
    idle_after: Option<String>,
) -> Result<()> {
    board::ensure_board(root)?;
    valid_name(name)?;
    // Record-time check: the path must
    // already be absolute. Refused here, before anything is written.
    if let Some(c) = cwd.as_deref() {
        if c.is_empty() || !Path::new(c).is_absolute() {
            return Err(SeatError::BadCwd(c.to_string()));
        }
    }
    // An unreadable duration is refused the same way: nothing written.
    if let Some(d) = idle_after.as_deref() {
        if seat::parse_idle_after(d).is_none() {
            return Err(SeatError::BadIdleAfter(d.to_string()));
        }
    }
    let dir = seat::seat_dir(root, name);
    if seat::meta_path(&dir).exists() || seat::token_path(&dir).exists() {
        // Zero words after `--` means the command was omitted, not
        // cleared (contract: register). Nothing passed changes nothing.
        if cwd.is_none() && cmd.is_empty() && idle_after.is_none() {
            println!("seat '{name}' already registered (token and mail kept)");
            return Ok(());
        }
        let launch_changed = cwd.is_some() || !cmd.is_empty();
        seat::update_meta(&dir, |m| apply_launch_fields(m, cwd, cmd, idle_after))?;
        println!("seat '{name}' already registered (token and mail kept)");
        if launch_changed {
            println!("seat '{name}' launch updated");
        }
        return Ok(());
    }
    seat::create(root, name)?;
    msg::mint_token(&seat::token_path(&dir))?;
    let mut meta = seat::Meta {
        harness: harness.to_string(),
        model,
        last_seen: Some(seat::now_ts()),
        ..Default::default()
    };
    apply_launch_fields(&mut meta, cwd, cmd, idle_after);
    seat::write_meta(&dir, &meta)?;
    println!("registered seat '{name}' (harness={harness})");
    println!("token: {}", seat::token_path(&dir).display());
    Ok(())
}

/// Register's one rule, stated once: a launch field the call omitted
/// keeps the meta's stored value; a field it passed updates it
/// (`--cwd`, the command words after `--`, `--idle-after`). Both of
/// register's paths apply it — a duplicate's read-modify-write and a
/// new seat's first meta.
fn apply_launch_fields(
    meta: &mut seat::Meta,
    cwd: Option<String>,
    cmd: Vec<String>,
    idle_after: Option<String>,
) {
    if let Some(c) = cwd {
        meta.cwd = Some(c);
    }
    if !cmd.is_empty() {
        meta.cmd = Some(cmd);
    }
    if let Some(d) = idle_after {
        meta.idle_after = Some(d);
    }
}

/// Read a seat's token; a missing token is the named GhostSeat error.
pub fn sender_token(root: &Path, seat_name: &str) -> Result<String> {
    msg::read_token(&seat::token_path(&seat::seat_dir(root, seat_name))).map_err(|e| match e {
        SeatError::Io(io) if io.kind() == std::io::ErrorKind::NotFound => {
            SeatError::GhostSeat(seat_name.to_string())
        }
        other => other,
    })
}

/// Send a direct message: MAC keyed by the *sender's* token, file lands
/// in the destination seat's inbox. The sender is fixed by the identity
/// rule (`--from` optional; see `crate::identity`): a bound session
/// sends only as its seat, an unbound caller's omitted `--from` sends
/// as `inbox` to a factory seat. `env_session` is the caller's
/// `HAPI_SESSION_ID`, read at the CLI edge. This is the library half
/// of the verb: the identity and token checks, the write, the
/// last-seen touch — and no printing, so service callers (the
/// dispatcher's alarm) send without touching stdout and pass no
/// `HAPI_SESSION_ID` into the identity rule.
pub fn send(
    root: &Path,
    from: Option<&str>,
    to: &str,
    body: &str,
    env_session: Option<&str>,
) -> Result<msg::Header> {
    board::ensure_board(root)?;
    let who = identity::resolve_for_send(root, from, env_session, to)?;
    let from = who.from.as_str();
    valid_name(from)?;
    valid_name(to)?;
    seat::read_meta_named(root, to)?;
    let token = sender_token(root, from)?;
    let (_, header) = msg::write_msg(
        &seat::seat_dir(root, to).join("inbox"),
        to,
        from,
        &token,
        body,
        who.session.as_deref(),
    )?;
    touch_last_seen(root, from)?;
    Ok(header)
}

/// The verb: the library send plus the one printed line.
pub fn cmd_send(
    root: &Path,
    from: Option<&str>,
    to: &str,
    body: &str,
    env_session: Option<&str>,
) -> Result<()> {
    let header = send(root, from, to, body, env_session)?;
    println!("sent {} from {} to {to}", header.id, header.from);
    Ok(())
}

/// Bump `meta.last_seen` for a seat that just acted on the board.
/// Atomic read-modify-write under the metadata lock (never the drain
/// lock), so a send never waits on a drain and never clobbers a
/// concurrent change to another field.
pub fn touch_last_seen(root: &Path, seat_name: &str) -> Result<()> {
    let dir = seat::seat_dir(root, seat_name);
    seat::update_meta(&dir, |meta| {
        meta.last_seen = Some(seat::now_ts());
    })?;
    Ok(())
}

/// The burn switch: remove the board under the root.
pub fn cmd_reset(root: &Path) -> Result<()> {
    board::reset(root)?;
    if root.exists() {
        println!("reset {} (root kept: not empty)", root.display());
    } else {
        println!("reset {} (root removed)", root.display());
    }
    Ok(())
}

pub fn sorted_seat_names(root: &Path) -> Result<Vec<String>> {
    let mut names: Vec<String> = std::fs::read_dir(board::seats_dir(root))?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    Ok(names)
}
