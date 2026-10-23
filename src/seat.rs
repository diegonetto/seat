use crate::error::{Result, SeatError};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// `meta.lifecycle` — the retired `arm` waiter's durable authority.
/// Still accepted when reading old `meta.json` files, never written:
/// 0.4.0 deleted `seat arm`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Lifecycle {
    ExitWake,
    Poller,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Meta {
    pub harness: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_drained: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<Lifecycle>,
    /// Launch state (003): the absolute working directory `up` starts
    /// the command in. Omitted means none stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Launch state (003): the ordered argv `up` starts. Omitted (or
    /// empty) means no command: the seat is mail-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmd: Option<Vec<String>>,
    /// The idle threshold (0.8.0): how long the dispatcher lets this
    /// seat's pane stand with nothing to do before sleeping it
    /// (`--idle-after`, e.g. `30m`, `2h`). Omitted means never.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_after: Option<String>,
}

/// Parse an `--idle-after` duration: one integer followed by exactly
/// one unit — `s`, `m`, or `h` (e.g. `30m`, `2h`). Anything else,
/// including a value whose seconds do not fit, is refused by
/// `register` and means "never sleep" wherever it is read.
pub fn parse_idle_after(raw: &str) -> Option<Duration> {
    let unit = raw.chars().last()?;
    let per_sec: u64 = match unit {
        's' => 1,
        'm' => 60,
        'h' => 60 * 60,
        _ => return None,
    };
    let n: u64 = raw[..raw.len() - 1].parse().ok()?;
    Some(Duration::from_secs(n.checked_mul(per_sec)?))
}

/// 0.7.0 removed rooms: the room-cursor directory is no longer
/// created (a leftover `cursors/` from before is never read or
/// written).
pub const SUBDIRS: [&str; 3] = ["inbox", "draining", "archive"];

pub fn seat_dir(root: &Path, name: &str) -> PathBuf {
    root.join("seats").join(name)
}

pub fn meta_path(dir: &Path) -> PathBuf {
    dir.join("meta.json")
}

pub fn token_path(dir: &Path) -> PathBuf {
    dir.join("token")
}

/// The drain lock file: `<seat>/lock`. One exclusive holder per seat for
/// the whole of a drain (or, later, a dispatcher's message batch). Other
/// seats have their own files and are never blocked.
pub fn lock_path(dir: &Path) -> PathBuf {
    dir.join("lock")
}

/// The metadata lock file: `<seat>/meta.lock`. A short-hold lock around
/// each read-modify-write of meta.json. Never the drain lock: a holder
/// of the drain lock may take the metadata lock, never the reverse, so
/// `send` and `register` never wait on a drain.
pub fn meta_lock_path(dir: &Path) -> PathBuf {
    dir.join("meta.lock")
}

/// How long a second drain waits for the seat lock before giving up
/// with the named SeatBusy error.
pub const LOCK_WAIT: Duration = Duration::from_secs(30);
/// Poll cadence between non-blocking flock attempts.
const LOCK_POLL: Duration = Duration::from_millis(20);

/// An exclusive `flock` on `<seat>/lock`, held for the guard's lifetime.
/// A second drain of the same seat waits (bounded) and never
/// interleaves; the lock is per seat directory, so other seats never
/// block. The lock is released when the guard drops (or the process
/// dies).
#[derive(Debug)]
pub struct SeatLock {
    file: File,
}

fn flock_exclusive_nb(fd: std::os::unix::io::RawFd) -> std::io::Result<()> {
    let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

impl SeatLock {
    /// Acquire the drain lock, waiting up to LOCK_WAIT (30s).
    pub fn acquire(dir: &Path, seat_name: &str) -> Result<Self> {
        Self::acquire_with_timeout(dir, seat_name, LOCK_WAIT)
    }

    /// Acquire with a caller-supplied bound (tests use a short one; the
    /// error wording always names the production 30s).
    pub fn acquire_with_timeout(dir: &Path, seat_name: &str, timeout: Duration) -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(lock_path(dir))?;
        let deadline = Instant::now() + timeout;
        loop {
            match flock_exclusive_nb(file.as_raw_fd()) {
                Ok(()) => return Ok(SeatLock { file }),
                Err(e) => match e.raw_os_error() {
                    // On Linux EWOULDBLOCK == EAGAIN; EINTR retries.
                    Some(libc::EWOULDBLOCK) | Some(libc::EINTR) => {}
                    _ => return Err(SeatError::Io(e)),
                },
            }
            if Instant::now() >= deadline {
                return Err(SeatError::SeatBusy(seat_name.to_string()));
            }
            std::thread::sleep(LOCK_POLL);
        }
    }

    /// A second, non-blocking probe: does some other holder hold the
    /// seat right now? (Useful for tests; takes no lock.)
    #[cfg(test)]
    pub fn is_held(dir: &Path) -> bool {
        let Ok(file) = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(lock_path(dir))
        else {
            return false;
        };
        match flock_exclusive_nb(file.as_raw_fd()) {
            Ok(()) => false, // we got it, so nobody held it
            Err(_) => true,
        }
    }
}

impl Drop for SeatLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// The dispatcher's board lock file: `<root>/dispatch.lock`. One
/// dispatcher per board, held for the whole life of `seat dispatch`
/// (`--once` included), so two scanners never interleave over one
/// board. Unlike the seat lock this one never waits.
pub fn dispatch_lock_path(root: &Path) -> PathBuf {
    root.join("dispatch.lock")
}

/// An exclusive, non-blocking `flock` on `<root>/dispatch.lock`, held
/// for the guard's lifetime. A second dispatcher fails at once with
/// the lock named; the lock is released when the guard drops (or the
/// process dies).
#[derive(Debug)]
pub struct DispatchLock {
    file: File,
}

impl DispatchLock {
    /// Acquire without waiting: any other holder means another
    /// dispatcher is running this board.
    pub fn acquire(root: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(dispatch_lock_path(root))?;
        loop {
            match flock_exclusive_nb(file.as_raw_fd()) {
                Ok(()) => return Ok(DispatchLock { file }),
                Err(e) => match e.raw_os_error() {
                    // On Linux EWOULDBLOCK == EAGAIN; EINTR retries.
                    Some(libc::EWOULDBLOCK) => {
                        return Err(SeatError::DispatchBusy(dispatch_lock_path(root)))
                    }
                    Some(libc::EINTR) => {}
                    _ => return Err(SeatError::Io(e)),
                },
            }
        }
    }
}

impl Drop for DispatchLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// The short metadata lock (a blocking `flock` on `<seat>/meta.lock`;
/// holders only ever hold it for one read-modify-write).
struct MetaLock {
    file: File,
}

impl MetaLock {
    fn acquire(dir: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(meta_lock_path(dir))?;
        loop {
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if rc == 0 {
                return Ok(MetaLock { file });
            }
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(SeatError::Io(e));
        }
    }
}

impl Drop for MetaLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// rfc3339 "now" for meta timestamps (last_seen, last_drained).
pub fn now_ts() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Create the seat layout:
/// `seats/<name>/{meta.json, token, inbox/, draining/, archive/}`.
/// (meta.json and token are created by register; directories here.)
pub fn create(root: &Path, name: &str) -> Result<PathBuf> {
    let dir = seat_dir(root, name);
    for sub in SUBDIRS {
        std::fs::create_dir_all(dir.join(sub))?;
    }
    Ok(dir)
}

static META_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Write meta.json atomically: temp file in the same directory, fsync,
/// rename over meta.json, fsync the directory. Mode 0600. A reader
/// never sees a half-written file. (This takes no lock; the locked
/// read-modify-write is `update_meta`. First-time seat creation may use
/// it directly.) The retired `lifecycle` field is accepted on read and
/// never written back.
pub fn write_meta(dir: &Path, meta: &Meta) -> Result<()> {
    let mut meta = meta.clone();
    meta.lifecycle = None;
    let mut json = serde_json::to_string_pretty(&meta)?.into_bytes();
    json.push(b'\n');
    let n = META_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".meta.json.{}-{n}.tmp", std::process::id()));
    let write = (|| -> Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, meta_path(dir)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(SeatError::Io(e));
    }
    // Make the rename durable.
    File::open(dir)?.sync_all()?;
    Ok(())
}

/// Read-modify-write meta.json under the short per-seat metadata lock
/// (`<seat>/meta.lock`, never the drain lock). Every verb that changes
/// an existing meta.json — register's launch update, send's
/// `last_seen`, drain's `last_drained` — goes through here, so
/// overlapping changes to different fields both survive and a reader
/// never sees a partial file.
pub fn update_meta<F>(dir: &Path, change: F) -> Result<Meta>
where
    F: FnOnce(&mut Meta),
{
    let _guard = MetaLock::acquire(dir)?;
    let mut meta = read_meta(dir)?;
    change(&mut meta);
    write_meta(dir, &meta)?;
    Ok(meta)
}

/// Read meta.json. Corrupt JSON (including an unknown lifecycle value) is
/// the named CorruptMeta error — never a panic, never a ghost seat.
pub fn read_meta(dir: &Path) -> Result<Meta> {
    let raw = std::fs::read_to_string(meta_path(dir)).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            SeatError::GhostSeat(
                dir.file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default(),
            )
        } else {
            SeatError::Io(e)
        }
    })?;
    serde_json::from_str(&raw).map_err(|source| SeatError::CorruptMeta {
        path: meta_path(dir),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board;
    use crate::testutil::temp_root;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn create_builds_layout() {
        let root = temp_root("seat-create");
        board::init(&root).unwrap();
        let dir = create(&root, "alpha").unwrap();
        assert_eq!(dir, root.join("seats").join("alpha"));
        for sub in SUBDIRS {
            assert!(dir.join(sub).is_dir(), "missing {sub}");
        }
    }

    #[test]
    fn lifecycle_is_accepted_on_read_and_never_written() {
        let root = temp_root("seat-meta");
        board::init(&root).unwrap();
        let dir = create(&root, "alpha").unwrap();
        // A meta written by 0.3.0 (lifecycle in the file) parses.
        std::fs::write(
            meta_path(&dir),
            "{\"harness\":\"h\",\"lifecycle\":\"poller\"}",
        )
        .unwrap();
        let back = read_meta(&dir).unwrap();
        assert_eq!(back.lifecycle, Some(Lifecycle::Poller));
        assert_eq!(back.harness, "h");

        // Any later write drops the field: write_meta strips it even
        // when the caller's struct still carries one.
        let meta = Meta {
            harness: "switchboard".into(),
            model: Some("glm".into()),
            lifecycle: Some(Lifecycle::ExitWake),
            ..Default::default()
        };
        write_meta(&dir, &meta).unwrap();
        let mode = std::fs::metadata(meta_path(&dir))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let raw = std::fs::read_to_string(meta_path(&dir)).unwrap();
        assert!(!raw.contains("lifecycle"), "raw meta: {raw}");
        assert_eq!(read_meta(&dir).unwrap().lifecycle, None);
        // Wire format of the enum is still the kebab-case string (old
        // files keep parsing after future schema changes).
        let old = serde_json::to_string(&Lifecycle::ExitWake).unwrap();
        assert_eq!(old, "\"exit-wake\"");
    }

    #[test]
    fn meta_launch_fields_round_trip_and_stay_absent() {
        let root = temp_root("seat-launch-meta");
        board::init(&root).unwrap();
        let dir = create(&root, "alpha").unwrap();
        // Absent launch fields are omitted from the file, and a meta
        // written before this feature parses unchanged (T001).
        let old = Meta {
            harness: "h".into(),
            ..Default::default()
        };
        write_meta(&dir, &old).unwrap();
        let raw = std::fs::read_to_string(meta_path(&dir)).unwrap();
        assert!(!raw.contains("\"cwd\""), "cwd omitted: {raw}");
        assert!(!raw.contains("\"cmd\""), "cmd omitted: {raw}");
        let back = read_meta(&dir).unwrap();
        assert_eq!(back.cwd, None);
        assert_eq!(back.cmd, None);

        let launched = Meta {
            cwd: Some(root.display().to_string()),
            cmd: Some(vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()]),
            ..old
        };
        write_meta(&dir, &launched).unwrap();
        let back = read_meta(&dir).unwrap();
        assert_eq!(
            back.cwd.as_deref(),
            Some(root.display().to_string().as_str())
        );
        assert_eq!(
            back.cmd.as_deref(),
            Some(
                &[
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "sleep 30".to_string()
                ][..]
            )
        );
    }

    #[test]
    fn corrupt_meta_is_named_error_no_ghost() {
        let root = temp_root("seat-corrupt");
        board::init(&root).unwrap();
        let dir = create(&root, "alpha").unwrap();
        std::fs::write(meta_path(&dir), "{ nope").unwrap();
        assert!(matches!(
            read_meta(&dir),
            Err(SeatError::CorruptMeta { .. })
        ));
        // Unknown lifecycle string is corrupt too.
        std::fs::write(
            meta_path(&dir),
            "{\"harness\":\"h\",\"lifecycle\":\"ghost\"}",
        )
        .unwrap();
        assert!(matches!(
            read_meta(&dir),
            Err(SeatError::CorruptMeta { .. })
        ));
        // Missing meta → ghost seat.
        let dir2 = create(&root, "beta").unwrap();
        assert!(matches!(read_meta(&dir2), Err(SeatError::GhostSeat(_))));
    }

    fn locked_root(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let root = temp_root(tag);
        board::init(&root).unwrap();
        let dir = create(&root, "alpha").unwrap();
        write_meta(
            &dir,
            &Meta {
                harness: "sb".into(),
                ..Default::default()
            },
        )
        .unwrap();
        (root, dir)
    }

    #[test]
    fn second_lock_holder_gives_up_with_the_named_error() {
        let (_root, dir) = locked_root("seat-lock-busy");
        let _held = SeatLock::acquire(&dir, "alpha").unwrap();
        assert!(SeatLock::is_held(&dir));
        let started = std::time::Instant::now();
        let err =
            SeatLock::acquire_with_timeout(&dir, "alpha", std::time::Duration::from_millis(120))
                .unwrap_err();
        assert!(started.elapsed() >= std::time::Duration::from_millis(100));
        assert_eq!(
            err.to_string(),
            "seat 'alpha' is busy: another drain holds its lock (gave up after 30s)",
            "{err}"
        );
        assert!(matches!(err, SeatError::SeatBusy(_)));
    }

    #[test]
    fn update_meta_applies_changes_and_survives_overlap() {
        let (_root, dir) = locked_root("seat-update-meta");
        let dir2 = dir.clone();
        let dir3 = dir.clone();
        let a = std::thread::spawn(move || {
            for i in 0..100usize {
                update_meta(&dir2, |m| m.model = Some(format!("m{i}"))).unwrap();
            }
        });
        let b = std::thread::spawn(move || {
            for _ in 0..100usize {
                update_meta(&dir3, |m| m.last_seen = Some(now_ts())).unwrap();
            }
        });
        a.join().unwrap();
        b.join().unwrap();
        let meta = read_meta(&dir).unwrap();
        assert!(meta.model.is_some(), "model updates survived");
        assert!(meta.last_seen.is_some(), "last_seen updates survived");
    }

    /// Concurrent readers never see a truncated or partial meta.json
    /// while update_meta writes; and the lock order holds — a metadata
    /// update never waits on the drain lock.
    #[test]
    fn readers_never_see_partial_meta_and_meta_never_waits_on_drain_lock() {
        let (_root, dir) = locked_root("seat-meta-readers");
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut handles = Vec::new();
        for _ in 0..2u8 {
            let dir2 = dir.clone();
            let stop2 = std::sync::Arc::clone(&stop);
            handles.push(std::thread::spawn(move || {
                let mut reads = 0usize;
                while !stop2.load(std::sync::atomic::Ordering::SeqCst) {
                    let meta = read_meta(&dir2).expect("parse");
                    assert_eq!(meta.harness, "sb");
                    reads += 1;
                }
                assert!(reads > 0, "reader made no reads");
            }));
        }
        // A held drain lock must not block metadata updates: send and
        // register never wait on a drain. The bound only guards against
        // a deadlock (each update fsyncs; slow is fine, stuck is not).
        let _drain_lock = SeatLock::acquire(&dir, "alpha").unwrap();
        let started = std::time::Instant::now();
        for i in 0..50usize {
            update_meta(&dir, |m| m.last_seen = Some(format!("ts{i}"))).unwrap();
            update_meta(&dir, |m| m.model = Some(format!("m{i}"))).unwrap();
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "update_meta blocked on the drain lock"
        );
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        for h in handles {
            h.join().unwrap();
        }
        // No tmp scratch lingers in the seat directory.
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with(".meta.json"))
            .collect();
        assert!(leftovers.is_empty(), "tmp leftovers: {leftovers:?}");
    }
}
