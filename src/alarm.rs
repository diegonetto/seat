//! Alarms (item 6, the frozen contract): one line to
//! `<root>/dispatch.log` and one post to the room `blacklite-factory`
//! as seat `sb-watch`, **once per cause** (not per retry). Causes are
//! latched per seat (`<cause>:<seat>`) — except `hub-auth`, which is
//! board-wide — and the fired set persists durably at
//! `<root>/dispatch-alarms.json`, so a retry, a respawned worker and
//! the next scan stay silent until the condition clears and returns.
//! No deduplication beyond the listed causes.
use crate::dstate::{atomic_write_private, log_line};
use crate::error::Result;
use crate::room;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Mutex;

pub(crate) const ROOM: &str = "blacklite-factory";
pub(crate) const SEAT: &str = "sb-watch";
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

/// Fire once per cause: the first occurrence writes one `alarm` log
/// line and posts `seat <seat|board>: <cause>: <detail>` to the alarm
/// room as `sb-watch` (no `HAPI_SESSION_ID` into the identity rule);
/// later occurrences are silent until `clear`. A failed post is
/// logged and does not unlatch; a persistence failure is returned.
pub(crate) fn fire(root: &Path, seat: Option<&str>, cause: &str, detail: &str) -> Result<()> {
    let _guard = LOCK.lock().unwrap();
    let mut fired = load(root);
    if !fired.causes.insert(key(seat, cause)) {
        return Ok(()); // already fired: once per cause, not per retry
    }
    store(root, &fired)?;
    let who = seat.unwrap_or("board");
    log_line(root, "alarm", who, &format!("cause={cause} {detail}"));
    if let Err(e) = room::post(
        root,
        ROOM,
        Some(SEAT),
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
    let cutoff = chrono::Utc::now().naive_utc()
        - chrono::TimeDelta::try_minutes(UNDELIVERED_AFTER_MIN).unwrap_or_default();
    let old = present.iter().any(|name| {
        let Some((ts, rest)) = name.split_once('-') else {
            return false;
        };
        let id = rest.strip_suffix(".msg").unwrap_or(rest);
        !delivered.contains(id)
            && chrono::NaiveDateTime::parse_from_str(ts, "%Y%m%dT%H%M%S%.6f")
                .is_ok_and(|t| t < cutoff)
    });
    if old {
        fire(
            root,
            Some(seat),
            UNDELIVERED,
            &format!("an item has waited over {UNDELIVERED_AFTER_MIN} minutes undelivered"),
        )
    } else {
        clear(root, Some(seat), UNDELIVERED)
    }
}
