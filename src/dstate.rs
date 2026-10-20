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
}

pub fn load_state(dir: &Path) -> DispatchState {
    std::fs::read_to_string(dir.join(STATE_FILE))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

static STATE_TMP: AtomicU64 = AtomicU64::new(0);

/// Atomic private write, the board's standard shape (`seat.rs`
/// `write_meta`): a fresh temp file (mode 0600, O_CREAT|O_EXCL) in the
/// same directory, fsync, rename over the target, then fsync the
/// directory so the rename itself is durable. Errors are returned,
/// never ignored, and no scratch file is left behind.
pub(crate) fn atomic_write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = path.parent().unwrap_or(Path::new("."));
    let n = STATE_TMP.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(
        ".{}.{}-{n}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let write = (|| -> Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    std::fs::File::open(dir)?.sync_all()?; // make the rename durable
    Ok(())
}

/// Atomic state write (see `atomic_write_private`).
pub fn store_state(dir: &Path, state: &DispatchState) -> Result<()> {
    let mut json = serde_json::to_string_pretty(state)?.into_bytes();
    json.push(b'\n');
    atomic_write_private(&dir.join(STATE_FILE), &json)
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
