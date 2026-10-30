//! Sleep and wake: a hapi seat idle past its
//! `--idle-after` threshold goes down through the same `down` path
//! one pane already uses and `dispatch.json` records `asleep`; mail
//! wakes it through `kit/scripts/seat-role wake <role>`. The script
//! is called, never reimplemented, and this crate never writes
//! `hapi-sessions.json` — the script is its only writer.
use crate::dstate::{self, DispatchState};
use crate::error::Result;
use crate::hub::Hub;
use crate::identity::Bindings;
use crate::seat;
use crate::webhook;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long `sb seats --json` may take (the kit's own bound is 15 s).
const MAP_TIMEOUT: Duration = Duration::from_secs(15);

/// The seat's role in the kernel seat map (`sb seats --json` — the
/// same map `seat-role` reads). `None` when the map cannot be read or
/// the seat has no role there: such a seat is never slept — the wake
/// takes a role — and a map failure is never permission to sleep.
pub(crate) fn seat_role(seat: &str) -> Option<String> {
    let mut cmd = Command::new("sb");
    cmd.args(["seats", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let output = run_bounded(&mut cmd, MAP_TIMEOUT)?;
    if !output.status.success() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    value
        .get("seats")?
        .get(seat)?
        .get("role")?
        .as_str()
        .map(str::to_string)
}

/// Run `cmd` to completion bounded by `limit`: a helper that hangs is
/// killed and the answer is `None` (never a partial one).
fn run_bounded(cmd: &mut Command, limit: Duration) -> Option<std::process::Output> {
    let mut child = cmd.spawn().ok()?;
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => return None,
        }
    }
    child.wait_with_output().ok()
}

/// Where `seat-role` is: `$SEAT_KIT/scripts/seat-role` when `SEAT_KIT`
/// is set, else the kit beside this binary, else PATH.
fn seat_role_program() -> PathBuf {
    if let Some(kit) = std::env::var_os("SEAT_KIT").filter(|v| !v.is_empty()) {
        let p = PathBuf::from(kit).join("scripts").join("seat-role");
        if p.is_file() {
            return p;
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(beside) = exe
            .parent()
            .map(|dir| dir.join("kit").join("scripts").join("seat-role"))
        {
            if beside.is_file() {
                return beside;
            }
        }
    }
    PathBuf::from("seat-role")
}

/// One wake attempt's answer.
pub(crate) enum WakeOutcome {
    /// The script answered `awake` and the re-read binding names this
    /// seat: deliver in this pass.
    Awake,
    /// Still asleep; the failure line names this reason. `busy`,
    /// `refused`, `failed`, and no-binding-after-awake — the next scan
    /// calls the script again.
    Failed(String),
    /// The script answered `up`: a pane runs with no phone session.
    /// Not a retry — the script answers `up` precisely so the caller
    /// stops calling, and a resume-style pane holds `role-wake.lock`
    /// through its idle watch — the wake waits until that pane is
    /// verifiably gone.
    Up(String),
}

/// Wake a seat through the kit's own script — the only writer of
/// `hapi-sessions.json`. Only a JSON `status: awake` answer plus a
/// binding for the seat on the re-read is a success: `busy` (the
/// cross-role `role-wake.lock`, no reason string), `refused`, and
/// `failed` all leave the seat asleep, and the next scan retries; `up`
/// leaves it asleep too but is never retried while its pane stands
/// (see `WakeOutcome::Up`). The script is waited for to the end — its
/// own deadlines bound it (a 45 s preflight budget, a 60 s session
/// wait, a 30 s exit wait), so the dispatcher never cuts a live wake
/// short; the 2-second scan promise is only that the wake starts on
/// the pass that sees the mail.
pub(crate) fn wake_seat(root: &Path, seat: &str) -> WakeOutcome {
    let Some(role) = seat_role(seat) else {
        return WakeOutcome::Failed(format!(
            "no role for seat {seat} in the kernel seat map, and seat-role wake takes a role"
        ));
    };
    let prog = seat_role_program();
    let output = match Command::new(&prog).arg("wake").arg(&role).output() {
        Ok(o) => o,
        Err(e) => {
            return WakeOutcome::Failed(format!("cannot run {}: {e}", prog.display()));
        }
    };
    let answer: serde_json::Value = match serde_json::from_slice(&output.stdout) {
        Ok(v) => v,
        Err(_) => {
            return WakeOutcome::Failed(format!(
                "seat-role wake {role} printed no readable answer (exit {})",
                output.status
            ));
        }
    };
    let status = answer.get("status").and_then(|s| s.as_str()).unwrap_or("");
    let reason = || match answer.get("reason").and_then(|r| r.as_str()) {
        Some(r) if !r.is_empty() => format!("seat-role wake {role} answered {status}: {r}"),
        _ => format!("seat-role wake {role} answered {status}"),
    };
    if status == "up" {
        // The pane is already running; it cannot grow a phone session
        // by being called again every scan.
        return WakeOutcome::Up(reason());
    }
    if status != "awake" {
        // `busy` carries no reason string; `refused` and `failed` do.
        return WakeOutcome::Failed(reason());
    }
    if Bindings::load(root).session_id(seat).is_none() {
        return WakeOutcome::Failed(format!(
            "seat-role wake {role} answered awake but hapi-sessions.json has no binding for {seat}"
        ));
    }
    WakeOutcome::Awake
}

/// The board the kit's wake script writes: seat-role fixes its binding
/// file at `<SEAT_ROLE_HOME|$HOME>/.local/share/seat` and calls
/// `seat up` with no `--root` (`kit/scripts/seat-role.py`), so a
/// dispatcher on any other `--root` could sleep a seat whose wake
/// would rewrite another board's bindings — this board would keep a
/// stale binding and never deliver. Only that one board may sleep.
fn wake_writes_root(root: &Path) -> bool {
    let Some(home) = std::env::var_os("SEAT_ROLE_HOME")
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var_os("HOME").filter(|v| !v.is_empty()))
    else {
        return false;
    };
    let script_root = PathBuf::from(&home)
        .join(".local")
        .join("share")
        .join("seat");
    match (
        std::fs::canonicalize(root),
        std::fs::canonicalize(script_root),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Is this hapi seat idle enough to sleep, and does it go down?
///
/// Invariant: a missing fact is never permission to sleep. All must
/// hold: a stored `idle_after`; an idle clock (`pane_seen_at`, or the
/// later of it and a delivery to the currently bound session) at least
/// that old; nothing in flight; nothing owed in `inbox/` or
/// `draining/`, looked for again just before the record; the bound
/// session's `thinking` explicitly `false`; a role in the kernel seat
/// map, because the wake takes one; and the dispatcher's board being
/// the board the wake script writes. The kill names the pane the clock
/// timed, and a different live pane under the same seat name cancels
/// the sleep: nothing dies.
pub(crate) fn try_sleep(
    root: &Path,
    seat: &str,
    session: &str,
    state: &mut DispatchState,
    hub: &mut Hub,
) -> Result<bool> {
    if state.asleep.is_some() {
        return Ok(false);
    }
    if !wake_writes_root(root) {
        return Ok(false);
    }
    let Some(threshold_raw) = seat::read_meta(&seat::seat_dir(root, seat))
        .ok()
        .and_then(|meta| meta.idle_after)
    else {
        return Ok(false);
    };
    let Some(threshold) = seat::parse_idle_after(&threshold_raw) else {
        return Ok(false);
    };
    let Some(seen) = state
        .pane_seen_at
        .as_deref()
        .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
    else {
        return Ok(false);
    };
    let mut clock = seen.with_timezone(&chrono::Utc);
    if let Some(last) = &state.last_delivery {
        if last.session == session {
            if let Ok(at) = chrono::DateTime::parse_from_rfc3339(&last.invoked_at) {
                clock = clock.max(at.with_timezone(&chrono::Utc));
            }
        }
    }
    if !chrono::Utc::now()
        .signed_duration_since(clock)
        .to_std()
        .is_ok_and(|age| age >= threshold)
    {
        return Ok(false);
    }
    match hub.session_state(session) {
        // Only an explicit false is quiet; anything else — missing,
        // non-boolean, 404, or an error — means the seat is not idle.
        Ok(s) if s.thinking == Some(false) => {}
        _ => return Ok(false),
    }
    if seat_role(seat).is_none() {
        return Ok(false);
    }
    // The last look before the record: mail that landed between the
    // caller's empty-branch check and here cancels the sleep — the
    // pass's owed set (`inbox/` and `draining/`, the same set receive
    // fills), so a seat holding mail is never put down.
    if !webhook::present_files(&seat::seat_dir(root, seat)).is_empty() {
        return Ok(false);
    }
    // Idle. The record is stored before the side effect — the
    // dispatcher's own rule (a crash resumes from dispatch.json) — so
    // a crash or a failed store between the two never leaves a dead
    // pane with no asleep mark: the wake path needs the mark, so mail
    // to a mark-less stopped seat could never be delivered. The pane
    // fact (timestamp and pane id) stays through the kill: it marks
    // the kill as pending against that pane, and the pass that finds
    // it beside the mark settles it (`settle_pending_sleep`).
    let reason = format!("idle {threshold_raw}");
    state.asleep = Some(dstate::Asleep {
        at: seat::now_ts(),
        reason: reason.clone(),
    });
    dstate::store_state(&seat::seat_dir(root, seat), state)?;
    // Down through the same path `cmd_down` runs for one pane, never
    // a signal by pid and never a pane the board did not start — and
    // only the pane the clock timed (a replacement that snuck in under
    // the same seat name is left standing).
    let recorded = state.pane_id.clone();
    let killed = crate::session::kill_seat_pane_if(seat, recorded.as_deref())?;
    if !killed {
        match crate::session::live_pane_id(seat) {
            // A live pane the decision never named took the seat
            // between the pass's pane check and the kill: the sleep
            // just recorded is cancelled — nothing dies, the mark
            // clears with the fact, and the next pass's pane fact
            // starts a fresh clock on the pane that stands.
            Some(Some(id)) if Some(id.as_str()) != recorded.as_deref() => {
                state.asleep = None;
                state.wake_hold = None;
                state.pane_seen_at = None;
                state.pane_id = None;
                dstate::store_state(&seat::seat_dir(root, seat), state)?;
                dstate::log_line(
                    root,
                    "sleep",
                    seat,
                    &format!("reason={reason} cancelled=1 cause=pane-replaced"),
                );
                return Ok(false);
            }
            // The multiplexer cannot be asked again: the stored mark
            // stays beside the fact — a pending kill the next pass
            // settles (`settle_pending_sleep`).
            None => {
                dstate::log_line(
                    root,
                    "sleep",
                    seat,
                    &format!("reason={reason} pane_killed=false pending=1"),
                );
                return Ok(true);
            }
            // The pane the mark named is gone (it died between the
            // pass's pane check and here): the sleep is complete —
            // the mark stands, only the fact clears, below.
            _ => {}
        }
    }
    state.pane_seen_at = None; // the pane is gone; the next pane starts a fresh clock
    state.pane_id = None;
    dstate::store_state(&seat::seat_dir(root, seat), state)?;
    dstate::log_line(
        root,
        "sleep",
        seat,
        &format!("reason={reason} pane_killed={killed}"),
    );
    Ok(true)
}

/// Settle a sleep whose kill never landed: `try_sleep` records the
/// sleep before it stops the pane, so a crash or failed kill leaves the
/// mark beside the pane fact.
///
/// Invariant: recovery verifies before it kills, with every condition
/// the decision used: the recorded pane is still the seat's live pane,
/// `thinking` reads exactly `false` again, the binding stands, and
/// nothing is owed just before the kill. A decided miss cancels: the
/// mark and the pane fact clear together and the pane stands. A GET
/// error decides nothing about the session, but owed mail still
/// cancels through it, so mail is never stranded behind a mark. With
/// nothing owed the mark stays for the next pass without restarting
/// the threshold. A pane already gone completes the sleep; a
/// multiplexer that cannot be asked settles nothing. Every outcome logs
/// the case it saw.
pub(crate) fn settle_pending_sleep(
    root: &Path,
    seat: &str,
    state: &mut DispatchState,
    hub: &mut Hub,
) -> Result<()> {
    let reason = state
        .asleep
        .as_ref()
        .map(|a| a.reason.clone())
        .unwrap_or_default();
    let recorded = state.pane_id.clone();
    match crate::session::live_pane_id(seat) {
        // Unknown is never permission to kill: the fact stands, the
        // next pass asks again.
        None => {
            dstate::log_line(
                root,
                "sleep",
                seat,
                &format!("reason={reason} pending=1 cause=pane-unknown"),
            );
        }
        // The pane died anyway (the kill half-landed, a manual down):
        // the sleep is complete, only the fact clears.
        Some(None) => {
            state.pane_seen_at = None;
            state.pane_id = None;
            dstate::store_state(&seat::seat_dir(root, seat), state)?;
            dstate::log_line(
                root,
                "sleep",
                seat,
                &format!("reason={reason} finished=1 cause=pane-gone"),
            );
        }
        Some(Some(id)) => {
            if recorded.as_deref() != Some(id.as_str()) {
                // Never kill a pane the decision did not name: a
                // replacement that snuck in under the same seat name.
                return cancel_sleep(root, seat, state, &reason, "pane-replaced");
            }
            let Some(session) = Bindings::load(root).session_id(seat).map(str::to_string)
            else {
                // No binding left to re-read: not quiet, by absence.
                return cancel_sleep(root, seat, state, &reason, "no-binding");
            };
            match hub.session_state(&session) {
                // Only an explicit false is quiet; anything else —
                // missing, non-boolean, 404 — means the session
                // worked or went away: never a kill.
                Ok(s) if s.thinking == Some(false) => {}
                Ok(_) => return cancel_sleep(root, seat, state, &reason, "session-working"),
                // A GET error decides nothing about the session, but
                // owed mail still decides: read the owed set whatever
                // the GET said — mail cancels the sleep, so the pass
                // delivers it instead of waking a pane the kill never
                // reached. An empty owed set keeps the mark, the
                // fact, and the clock untouched for a retry on the
                // next pass — an unreadable answer never restarts
                // the threshold and never kills.
                Err(e) => {
                    if !webhook::present_files(&seat::seat_dir(root, seat)).is_empty() {
                        return cancel_sleep(root, seat, state, &reason, "mail-present");
                    }
                    dstate::log_line(
                        root,
                        "sleep",
                        seat,
                        &format!("reason={reason} pending=1 cause=get-error ({e})"),
                    );
                    return Ok(());
                }
            }
            // The last look before the kill: owed mail (`inbox/` and
            // `draining/`, the pass's own set) cancels the sleep, and
            // the rest of the pass delivers it.
            if !webhook::present_files(&seat::seat_dir(root, seat)).is_empty() {
                return cancel_sleep(root, seat, state, &reason, "mail-present");
            }
            // Only the pane the decision named, verified quiet. A
            // refusal means the pane set moved between the pane query
            // above and here, and the re-query decides — the same way
            // the first sleep's refusal does: a different live pane
            // cancels, an unanswerable multiplexer settles nothing
            // this pass, and only a gone pane completes below.
            let killed = crate::session::kill_seat_pane_if(seat, Some(id.as_str()))?;
            if !killed {
                match crate::session::live_pane_id(seat) {
                    // A replacement that snuck in under the same seat
                    // name after the query that verified it: nothing
                    // dies, the same cancel as the first check's.
                    Some(Some(other)) if Some(other.as_str()) != recorded.as_deref() => {
                        return cancel_sleep(root, seat, state, &reason, "pane-replaced");
                    }
                    // The multiplexer cannot be asked again: the mark
                    // and the fact stay — the next pass settles.
                    None => {
                        dstate::log_line(
                            root,
                            "sleep",
                            seat,
                            &format!("reason={reason} pending=1 cause=pane-unknown"),
                        );
                        return Ok(());
                    }
                    // The pane the mark named is gone: the sleep is
                    // complete, below.
                    _ => {}
                }
            }
            state.pane_seen_at = None;
            state.pane_id = None;
            dstate::store_state(&seat::seat_dir(root, seat), state)?;
            dstate::log_line(
                root,
                "sleep",
                seat,
                &format!("reason={reason} pane_killed={killed} finished=1"),
            );
        }
    }
    Ok(())
}

/// Cancel a decided-miss sleep: the mark, any wake hold, and the pane
/// fact all clear together — `pass_seat` restarts the clock only when
/// the pane id moves, so the fact must go with the mark or the next
/// quiet pass would sleep immediately on the old clock.
fn cancel_sleep(
    root: &Path,
    seat: &str,
    state: &mut DispatchState,
    reason: &str,
    cause: &str,
) -> Result<()> {
    state.asleep = None;
    state.wake_hold = None;
    state.pane_seen_at = None;
    state.pane_id = None;
    dstate::store_state(&seat::seat_dir(root, seat), state)?;
    dstate::log_line(
        root,
        "sleep",
        seat,
        &format!("reason={reason} cancelled=1 cause={cause}"),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lookup that ships: `$SEAT_KIT`'s scripts directory, else
    /// the kit beside the binary (never there in a build tree), else
    /// PATH.
    #[test]
    fn seat_role_program_prefers_kit_then_binary_then_path() {
        let _guard = crate::testutil::ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let kit = crate::testutil::temp_root("sleep-kit");
        std::fs::create_dir_all(kit.join("scripts")).unwrap();
        std::fs::write(kit.join("scripts").join("seat-role"), "#!/bin/sh\n").unwrap();
        let prev = std::env::var_os("SEAT_KIT");
        std::env::set_var("SEAT_KIT", &kit);
        assert_eq!(
            seat_role_program(),
            kit.join("scripts").join("seat-role"),
            "$SEAT_KIT wins"
        );
        std::env::set_var("SEAT_KIT", "");
        assert_eq!(
            seat_role_program(),
            PathBuf::from("seat-role"),
            "empty SEAT_KIT and no kit beside the binary: PATH"
        );
        match prev {
            Some(v) => std::env::set_var("SEAT_KIT", v),
            None => std::env::remove_var("SEAT_KIT"),
        }
    }
}
