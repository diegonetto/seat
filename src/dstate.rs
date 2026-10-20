//! The dispatcher's per-seat state: `dispatch.json` (each in-flight
//! key, its target session and phase, the last delivery for status's
//! `awake`), the stable delivery keys, the exact text `drain` prints
//! for an item, and the one-line log.
use crate::drain::{ReceivedDirect, ReceivedRoom};
use crate::error::Result;
use crate::identity::Bindings;
use crate::seat;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const STATE_FILE: &str = "dispatch.json";
const LOG_FILE: &str = "dispatch.log";
/// A delivery keeps a seat `awake` this long.
pub const AWAKE_WINDOW: Duration = Duration::from_secs(10 * 60);

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Phase {
    #[default]
    Staged,
    Posted,
    Done,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub(crate) struct ItemState {
    pub(crate) phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) accepted_at: Option<String>,
}

/// A successful delivery, for status's `awake`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LastDelivery {
    pub session: String,
    pub invoked_at: String,
}

/// Per-seat dispatcher state (`<seat>/dispatch.json`), written
/// atomically before any POST so a restart resumes exactly.
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct DispatchState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_delivery: Option<LastDelivery>,
    #[serde(default)]
    pub(crate) items: BTreeMap<String, ItemState>,
}

pub fn load_state(dir: &Path) -> DispatchState {
    std::fs::read_to_string(dir.join(STATE_FILE))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

static STATE_TMP: AtomicU64 = AtomicU64::new(0);

/// Atomic state write: temp file, fsync, rename.
pub fn store_state(dir: &Path, state: &DispatchState) -> Result<()> {
    use std::io::Write;
    let mut json = serde_json::to_string_pretty(state)?.into_bytes();
    json.push(b'\n');
    let n = STATE_TMP.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".dispatch.json.{}-{n}.tmp", std::process::id()));
    let write = (|| -> Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(std::fs::rename(&tmp, dir.join(STATE_FILE))?)
}

/// Status's `awake`: dispatch.json records a delivery to this seat's
/// currently bound session within AWAKE_WINDOW (a local read; status
/// never asks the hub).
pub fn awake(root: &Path, seat_name: &str) -> bool {
    let Some(last) = load_state(&seat::seat_dir(root, seat_name)).last_delivery else {
        return false;
    };
    if Bindings::load(root).session_id(seat_name) != Some(last.session.as_str()) {
        return false;
    }
    chrono::DateTime::parse_from_rfc3339(&last.invoked_at)
        .map(|t| {
            chrono::Utc::now()
                .signed_duration_since(t.with_timezone(&chrono::Utc))
                .to_std()
                .is_ok_and(|a| a <= AWAKE_WINDOW)
        })
        .unwrap_or(false)
}

/// Delivery keys: never the 4-byte id alone.
pub(crate) fn direct_key(seat: &str, file: &str) -> String {
    format!("seat:{seat}:{file}")
}

pub(crate) fn room_key(seat: &str, room: &str, file: &str) -> String {
    format!("room:{room}:{file}:{seat}")
}

/// Exactly the lines `seat drain` prints for the item: header line,
/// body, blank line.
pub(crate) fn direct_text(d: &ReceivedDirect) -> String {
    format!(
        "[direct] id={} from={} dest={} ts={}\n{}\n\n",
        d.header.id, d.header.from, d.header.dest, d.header.ts, d.body
    )
}

pub(crate) fn room_text(r: &ReceivedRoom) -> String {
    format!(
        "[room:{}] id={} from={} ts={}\n{}\n\n",
        r.room, r.header.id, r.header.from, r.header.ts, r.body
    )
}

/// One line to `<root>/dispatch.log`.
pub(crate) fn log_line(root: &Path, kind: &str, seat: &str, rest: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join(LOG_FILE))
    {
        let _ = writeln!(f, "{} {kind} seat={seat} {rest}", seat::now_ts());
    }
}
