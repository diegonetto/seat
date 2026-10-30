//! The dispatcher's per-seat state: `dispatch.json` (each in-flight
//! key, its target session and phase, the last delivery for status's
//! `awake`), the stable delivery keys, the exact text `drain` prints
//! for an item, and the one-line log.
use crate::drain::ReceivedDirect;
use crate::error::Result;
use crate::identity::Bindings;
use crate::seat;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
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
    /// The bound session is on it: recorded while the
    /// session read says `thinking` exactly `true`, returned to
    /// `Posted` by the first read that does not.
    Working,
    Done,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub(crate) struct ItemState {
    pub(crate) phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) accepted_at: Option<String>,
    /// When the read that recorded `working` ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) working_at: Option<String>,
}

/// A successful delivery, for status's `awake`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LastDelivery {
    pub session: String,
    pub invoked_at: String,
}

/// The dispatcher's sleep mark: the seat's pane was stopped
/// for idleness. Kept until a successful wake clears it; `status`
/// prints `asleep` for it while the pane is gone.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Asleep {
    pub at: String,
    pub reason: String,
}

/// A successful wake's record: the delivery key of the mail
/// that woke the seat.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Woken {
    pub at: String,
    pub key: String,
}

/// The script's `up` answer held against further wakes: a pane
/// runs with no phone session, and the script answers `up` precisely
/// so the caller stops calling — the wake is not retried until that
/// pane is verifiably gone.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WakeHold {
    pub at: String,
    pub reason: String,
}

/// The webhook adapter's ring state (inside `dispatch.json`): the
/// burst still open — files rung, waiting for the bot's own drain —
/// the doubling re-ring backoff (0 = no burst) and the failed-ring
/// count behind the `webhook-failing` alarm.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RingState {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) rung: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) next_at: Option<String>,
    #[serde(default)]
    pub(crate) backoff_ms: u64,
    #[serde(default)]
    pub(crate) failures: u32,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) ring: Option<RingState>,
    /// The idle clock's pane fact: when the timed pane was
    /// first seen, and (`pane_id`) which pane that is. A live pane with
    /// a different id — `seat up`, the gallery, or a down-and-up that
    /// landed inside one 2 s scan — starts the clock over; a gone pane
    /// clears the fact, and a seat with no fact is never slept. Beside
    /// an `asleep` mark the fact means the sleep's kill is still
    /// pending against that pane (`sleep::settle_pending_sleep`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pane_seen_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pane_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) asleep: Option<Asleep>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) woken: Option<Woken>,
    /// The held wake above: set when seat-role answers `up`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) wake_hold: Option<WakeHold>,
}

pub fn load_state(dir: &Path) -> DispatchState {
    std::fs::read_to_string(dir.join(STATE_FILE))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Atomic state write (see `seat::atomic_write_private`).
pub fn store_state(dir: &Path, state: &DispatchState) -> Result<()> {
    let mut json = serde_json::to_string_pretty(state)?.into_bytes();
    json.push(b'\n');
    seat::atomic_write_private(&dir.join(STATE_FILE), &json)
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

/// Status's `asleep` and the scan's wake rule: dispatch.json holds
/// the dispatcher's own sleep mark. A local read; the live pane is a
/// separate fact (presence checks both — the record plus no pane).
pub fn is_asleep(root: &Path, seat_name: &str) -> bool {
    load_state(&seat::seat_dir(root, seat_name))
        .asleep
        .is_some()
}

/// The scan's recovery rule: the sleep mark beside a pane fact — a
/// kill that never landed. Such a seat is scheduled whether or not it
/// is bound and whether or not mail waits; the pass settles the mark.
pub fn pending_sleep(root: &Path, seat_name: &str) -> bool {
    let state = load_state(&seat::seat_dir(root, seat_name));
    state.asleep.is_some() && state.pane_seen_at.is_some()
}

/// Delivery keys: never the 4-byte id alone.
pub(crate) fn direct_key(seat: &str, file: &str) -> String {
    format!("seat:{seat}:{file}")
}

/// Is `suffix` a generation suffix (`#` then digits, from
/// `next_delivery_key`)? Message file names never contain `#`, so a
/// key's base is unambiguous.
fn generation(key: &str) -> Option<&str> {
    key.rsplit_once('#')
        .filter(|(_, gen)| !gen.is_empty() && gen.bytes().all(|b| b.is_ascii_digit()))
        .map(|(_, gen)| gen)
}

/// A delivery key's base: the key of the item's file as received, any
/// generation suffix stripped — also that file's index in `texts`.
pub(crate) fn key_base(key: &str) -> &str {
    match key.rsplit_once('#') {
        Some((base, _)) if generation(key).is_some() => base,
        _ => key,
    }
}

/// The next generation of a delivery key: a fresh `localId` for the
/// same file. The hub dedupes on `(session, localId)` forever, so this
/// is how an in-flight item is re-sent — the invoked mark the ending
/// session left on the old key can never come back on the new one.
pub(crate) fn next_delivery_key(key: &str) -> String {
    let base = key_base(key);
    let next = generation(key)
        .and_then(|g| g.parse::<u32>().ok())
        .unwrap_or(1)
        + 1;
    format!("{base}#{next}")
}

/// Exactly the lines `seat drain` prints for the item: header line,
/// body, blank line.
pub(crate) fn direct_text(d: &ReceivedDirect) -> String {
    format!(
        "[direct] id={} from={} dest={} ts={}\n{}\n\n",
        d.header.id, d.header.from, d.header.dest, d.header.ts, d.body
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
