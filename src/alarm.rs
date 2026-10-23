//! Alarms (item 6, the frozen contract): one line to
//! `<root>/dispatch.log` and one direct message from seat `sb-watch`
//! to the seat named in `<root>/alarm-seat`, **once per cause** (not
//! per retry). When that file is absent or empty the alarm is the log
//! line only. Causes are latched per seat (`<cause>:<seat>`) — except
//! `hub-auth`, which is board-wide — and the fired set persists
//! durably at `<root>/dispatch-alarms.json`, so a retry, a respawned
//! worker and the next scan stay silent until the condition clears
//! and returns. No deduplication beyond the listed causes.
use crate::dispatch::QUEUED_CHUNK;
use crate::dstate::{
    atomic_write_private, direct_key, key_base, log_line, store_state, DispatchState, Phase,
};
use crate::error::Result;
use crate::hub::Hub;
use crate::seat;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Mutex;

/// The seat alarms are sent as (it must be registered, with a token).
pub(crate) const SEAT: &str = "sb-watch";
/// `<root>/alarm-seat`: the seat alarms are mailed to (one line).
const ALARM_SEAT_FILE: &str = "alarm-seat";
const STATE_FILE: &str = "dispatch-alarms.json";
/// A message this old still undelivered is the `undelivered` cause.
pub(crate) const UNDELIVERED_AFTER_MIN: i64 = 15;

pub(crate) const UNDELIVERED: &str = "undelivered";
pub(crate) const SESSION_INACTIVE: &str = "session-inactive";
pub(crate) const NO_ANSWER: &str = "no-answer";
/// Board-wide (no seat): the hub's settings are the board's.
pub(crate) const HUB_AUTH: &str = "hub-auth";
pub(crate) const WEBHOOK_FAILING: &str = "webhook-failing";

/// One process-wide lock: workers fire and clear concurrently, and a
/// respawned worker must see the fired set its predecessor left (the
/// file is the record across restarts; the lock is for this process).
static LOCK: Mutex<()> = Mutex::new(());

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
struct Fired {
    #[serde(default)]
    causes: BTreeSet<String>,
}

fn key(seat: Option<&str>, cause: &str) -> String {
    format!("{cause}:{}", seat.unwrap_or("board"))
}

fn load(root: &Path) -> Fired {
    std::fs::read_to_string(root.join(STATE_FILE))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn store(root: &Path, fired: &Fired) -> Result<()> {
    let mut json = serde_json::to_string(fired)?.into_bytes();
    json.push(b'\n');
    atomic_write_private(&root.join(STATE_FILE), &json)
}

/// The seat alarms are mailed to: the first line of
/// `<root>/alarm-seat`, trimmed. An absent or empty file means the
/// alarm is the log line only.
fn alarm_seat(root: &Path) -> Option<String> {
    std::fs::read_to_string(root.join(ALARM_SEAT_FILE))
        .ok()
        .and_then(|raw| raw.lines().next().map(|l| l.trim().to_string()))
        .filter(|l| !l.is_empty())
}

/// Fire once per cause: the first occurrence writes one `alarm` log
/// line and sends `seat <seat|board>: <cause>: <detail>` as one
/// direct message from `sb-watch` to the seat named in
/// `<root>/alarm-seat` (the silent library send — no printing, no
/// `HAPI_SESSION_ID` into the identity rule); an absent or empty file
/// leaves the log line as the whole alarm. Later occurrences are
/// silent until `clear`. A failed send (the file names a seat that is
/// not on the board) is logged and does not unlatch; a persistence
/// failure is returned.
pub(crate) fn fire(root: &Path, seat: Option<&str>, cause: &str, detail: &str) -> Result<()> {
    let _guard = LOCK.lock().unwrap();
    let mut fired = load(root);
    if !fired.causes.insert(key(seat, cause)) {
        return Ok(()); // already fired: once per cause, not per retry
    }
    store(root, &fired)?;
    let who = seat.unwrap_or("board");
    log_line(root, "alarm", who, &format!("cause={cause} {detail}"));
    if let Some(to) = alarm_seat(root) {
        if let Err(e) = crate::verbs::send(
            root,
            Some(SEAT),
            &to,
            &format!("seat {who}: {cause}: {detail}"),
            None,
        ) {
            log_line(
                root,
                "alarm-post-failed",
                who,
                &format!("cause={cause} error={e}"),
            );
        }
    }
    Ok(())
}

/// The condition cleared: a future occurrence may alarm again.
pub(crate) fn clear(root: &Path, seat: Option<&str>, cause: &str) -> Result<()> {
    let _guard = LOCK.lock().unwrap();
    let mut fired = load(root);
    if fired.causes.remove(&key(seat, cause)) {
        store(root, &fired)?;
    }
    Ok(())
}

/// The `UNDELIVERED_AFTER_MIN` cutoff, back from now.
fn cutoff() -> chrono::NaiveDateTime {
    chrono::Utc::now().naive_utc()
        - chrono::TimeDelta::try_minutes(UNDELIVERED_AFTER_MIN).unwrap_or_default()
}

/// The present mail older than the cutoff and not in the delivered
/// record, oldest first (the name's `ts` prefix is fixed width, so the
/// set's order is the age order).
fn old_files<'a>(
    present: &'a BTreeSet<String>,
    delivered: &BTreeSet<String>,
    cutoff: chrono::NaiveDateTime,
) -> Vec<&'a String> {
    present
        .iter()
        .filter(|name| {
            let Some((ts, rest)) = name.split_once('-') else {
                return false;
            };
            let id = rest.strip_suffix(".msg").unwrap_or(rest);
            !delivered.contains(id)
                && chrono::NaiveDateTime::parse_from_str(ts, "%Y%m%dT%H%M%S%.6f")
                    .is_ok_and(|t| t < cutoff)
        })
        .collect()
}

/// An old file's age in whole minutes, from its file-name `ts`.
fn age_minutes(name: &str) -> i64 {
    name.split_once('-')
        .and_then(|(ts, _)| chrono::NaiveDateTime::parse_from_str(ts, "%Y%m%dT%H%M%S%.6f").ok())
        .map(|t| (chrono::Utc::now().naive_utc() - t).num_minutes().max(0))
        .unwrap_or(UNDELIVERED_AFTER_MIN)
}

/// The `undelivered` cause, by message age (the header `ts`, which is
/// also the file-name prefix): fire while the seat has an item older
/// than 15 minutes still undelivered — a mail file in `inbox/` or
/// `draining/` whose id is not in the payload-delivered record — and
/// clear once it has none. Only seats this dispatcher delivers (hapi
/// or webhook) call this; pull seats stay silent.
pub(crate) fn undelivered(
    root: &Path,
    seat: &str,
    delivered: &BTreeSet<String>,
    present: &BTreeSet<String>,
) -> Result<()> {
    if old_files(present, delivered, cutoff()).is_empty() {
        return clear(root, Some(seat), UNDELIVERED);
    }
    fire(
        root,
        Some(seat),
        UNDELIVERED,
        &format!("an item has waited over {UNDELIVERED_AFTER_MIN} minutes undelivered"),
    )
}

/// The old file's in-flight item, when the record holds one: its
/// current delivery key (a rotated generation included) and phase.
fn item_state(seat: &str, state: &DispatchState, file: &str) -> Option<(String, Phase)> {
    let base = direct_key(seat, file);
    state
        .items
        .iter()
        .find(|(key, _)| key_base(key) == base)
        .map(|(key, item)| (key.clone(), item.phase))
}

/// The hapi pass's `undelivered` (BLL-124): the same file-age rule
/// with the exceptions the session read decides, per old file. The
/// bound session is read first — asked only when something is old —
/// and an in-flight `posted` item on an active session reporting
/// `thinking` exactly `true` is `working`: the record gets that phase
/// with the time of the read, and the next pass re-reads (a read that
/// does not say thinking — a missing flag, a quiet or inactive
/// session, a GET error — returns the item to `posted`, so no page is
/// silenced by a stale fact and no new timer is needed). The hub
/// marks a queued message invoked only when the turn ends, and this
/// decision runs before the pass's poll commits that mark (the file
/// is still in `draining/`), so an old `posted` item the hub reports
/// invoked on an active session is delivered, not undelivered — the
/// page waits for a pass that reads the item still not invoked. Every
/// other old file still pages, never-posted mail included; only an
/// explicit `true` or an active session's invoked mark skips (a
/// missing or non-boolean flag never does, an inactive session's
/// invoked mark never does — the ending session made it), and a hub
/// error never skips — today's page happens with the error named
/// (a failed queued read too: the latched page cannot be amended by
/// the pass's later poll).
/// The page text names what the pass saw (`posted, session idle for
/// N min`, N the file's own age — `idle` only for an explicit
/// `false`, the words `thinking unknown` or `session not active` when
/// that is the read), so a dead delivery reads apart from a long
/// turn. When every old file is skipped the latch clears: `fire` is
/// once per cause, and a latch left set would not page when the turn
/// ends.
pub(crate) fn undelivered_hapi(
    root: &Path,
    seat: &str,
    dir: &Path,
    state: &mut DispatchState,
    present: &BTreeSet<String>,
    hub: &mut Hub,
    session: &str,
) -> Result<()> {
    let old = old_files(present, &BTreeSet::new(), cutoff());
    if old.is_empty() {
        return clear(root, Some(seat), UNDELIVERED); // nothing old is owed
    }
    // Asked only when something is old: a clean pass never pays for
    // this read.
    let session_read = hub.session_state(session).map_err(|e| e.to_string());
    let thinking = matches!(&session_read, Ok(s) if s.active && s.thinking == Some(true));
    // The stored `working` phase: set for every in-flight posted item
    // while the read says the session thinks (the session is on the
    // turn, all its queued mail included), ended by the first read
    // that does not — which is the phase the alarm below fires on.
    let now = seat::now_ts();
    let mut moved = false;
    for item in state.items.values_mut() {
        match (thinking, item.phase) {
            (true, Phase::Posted) => {
                item.phase = Phase::Working;
                item.working_at = Some(now.clone());
                moved = true;
            }
            (false, Phase::Working) => {
                item.phase = Phase::Posted;
                item.working_at = None;
                moved = true;
            }
            _ => {}
        }
    }
    if moved {
        store_state(dir, state)?;
    }
    // The turn may have ended since the POST: the invoked mark is read
    // here too, for the old posted items of an active session only.
    // A queued read that fails never skips — its error is kept for the
    // page below (the pass's own poll fails with it moments later, but
    // that cannot amend the message already latched here).
    let active = matches!(&session_read, Ok(s) if s.active);
    let mut invoked: BTreeSet<String> = BTreeSet::new();
    let mut queued_err: Option<String> = None;
    if active && !thinking {
        let keys: Vec<String> = old
            .iter()
            .filter_map(|name| item_state(seat, state, name))
            .filter(|(_, phase)| *phase == Phase::Posted)
            .map(|(key, _)| key)
            .collect();
        for chunk in keys.chunks(QUEUED_CHUNK) {
            match hub.queued_state(session, chunk) {
                Ok(qs) => {
                    invoked.extend(qs.invoked.iter().filter(|k| chunk.contains(k)).cloned());
                }
                Err(e) => {
                    queued_err.get_or_insert(e.to_string());
                }
            }
        }
    }
    let skipped = |name: &str| match item_state(seat, state, name) {
        Some((_, Phase::Working)) => true, // the session is on it
        Some((key, Phase::Posted)) => active && invoked.contains(&key), // delivered this pass
        _ => false,                        // never-posted mail, or no record
    };
    let Some(name) = old.iter().copied().find(|name| !skipped(name)) else {
        return clear(root, Some(seat), UNDELIVERED); // every old file is worked or delivered
    };
    let base = format!("an item has waited over {UNDELIVERED_AFTER_MIN} minutes undelivered");
    let posted = item_state(seat, state, name).is_some_and(|(_, p)| p == Phase::Posted);
    let age = age_minutes(name);
    let detail = match &session_read {
        Err(e) if posted => format!("{base} (posted, hub session read failed: {e})"),
        Err(e) => format!("{base} (hub session read failed: {e})"),
        Ok(s) if posted => {
            if !s.active {
                format!("{base} (posted, waited {age} min, session not active)")
            } else if let Some(e) = &queued_err {
                format!("{base} (posted, waited {age} min, hub queued read failed: {e})")
            } else if s.thinking == Some(false) {
                format!("{base} (posted, session idle for {age} min)")
            } else {
                format!("{base} (posted, waited {age} min, session thinking unknown)")
            }
        }
        Ok(_) => base,
    };
    fire(root, Some(seat), UNDELIVERED, &detail)
}
