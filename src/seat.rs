use crate::error::{SeatError, Result};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// `meta.lifecycle` — the durable authority for `arm`.
/// `exit-wake`: wake once, exit 0. `poller`: stay up until SIGTERM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Lifecycle {
    ExitWake,
    Poller,
}

impl Lifecycle {
    pub fn as_str(&self) -> &'static str {
        match self {
            Lifecycle::ExitWake => "exit-wake",
            Lifecycle::Poller => "poller",
        }
    }
}

/// CLI-facing parse (same kebab-case spelling as the wire format).
impl std::str::FromStr for Lifecycle {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "exit-wake" => Ok(Lifecycle::ExitWake),
            "poller" => Ok(Lifecycle::Poller),
            other => Err(format!(
                "unknown lifecycle '{other}' (expected exit-wake or poller)"
            )),
        }
    }
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
}

pub const SUBDIRS: [&str; 4] = ["inbox", "draining", "archive", "cursors"];

pub fn seat_dir(root: &Path, name: &str) -> PathBuf {
    root.join("seats").join(name)
}

pub fn meta_path(dir: &Path) -> PathBuf {
    dir.join("meta.json")
}

pub fn token_path(dir: &Path) -> PathBuf {
    dir.join("token")
}

/// rfc3339 "now" for meta timestamps (last_seen, last_drained).
pub fn now_ts() -> String {
    chrono::Utc::now().to_rfc3339()
}

pub fn wait_pid_path(dir: &Path) -> PathBuf {
    dir.join("wait.pid")
}

/// Create the seat layout:
/// `seats/<name>/{meta.json, token, inbox/, draining/, archive/, cursors/}`.
/// (meta.json and token are created by register; directories here.)
pub fn create(root: &Path, name: &str) -> Result<PathBuf> {
    let dir = seat_dir(root, name);
    for sub in SUBDIRS {
        std::fs::create_dir_all(dir.join(sub))?;
    }
    Ok(dir)
}

/// Write meta.json mode 0600.
pub fn write_meta(dir: &Path, meta: &Meta) -> Result<()> {
    let json = serde_json::to_string_pretty(meta)?;
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(meta_path(dir))?;
    f.write_all(json.as_bytes())?;
    f.write_all(b"\n")?;
    Ok(())
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

/// Read the waiter pidfile. Missing or unparseable → None (no waiter).
pub fn read_wait_pid(dir: &Path) -> Option<u32> {
    std::fs::read_to_string(wait_pid_path(dir))
        .ok()?
        .trim()
        .parse()
        .ok()
}

pub fn write_wait_pid(dir: &Path, pid: u32) -> Result<()> {
    std::fs::write(wait_pid_path(dir), pid.to_string())?;
    Ok(())
}

pub fn clear_wait_pid(dir: &Path) -> Result<()> {
    let p = wait_pid_path(dir);
    match std::fs::remove_file(&p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(SeatError::Io(e)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaiterState {
    /// No live waiter (no pidfile, or the pid is gone).
    Idle,
    /// Waiter process is alive and parented (ppid != 1).
    Live,
    /// Waiter process is alive but reparented to init (ppid == 1).
    Orphan,
}

/// Parent pid of a live process, from /proc/<pid>/stat. A zombie
/// (exited, not yet reaped) is not live: it reads as None so a dead
/// waiter's unreaped pid never blocks `arm --takeover` or reads as
/// a live waiter in the roster.
pub fn ppid_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm is in parens and may contain spaces or ')'; fields after the
    // last ')': state, ppid, ...
    let rest = stat.rfind(')')? + 1;
    let mut fields = stat[rest..].split_whitespace();
    let state = fields.next()?; // e.g. "S", or "Z" for a zombie
    if state.starts_with('Z') {
        return None;
    }
    fields.next()?.parse().ok() // ppid
}

/// Pure classification so orphan logic is unit-testable without
/// reparenting real processes.
pub fn classify_waiter(pid_alive: bool, ppid: u32) -> WaiterState {
    match (pid_alive, ppid) {
        (false, _) => WaiterState::Idle,
        (true, 1) => WaiterState::Orphan,
        (true, _) => WaiterState::Live,
    }
}

pub fn waiter_state(dir: &Path) -> WaiterState {
    match read_wait_pid(dir) {
        None => WaiterState::Idle,
        Some(pid) => match ppid_of(pid) {
            None => WaiterState::Idle, // stale pidfile
            Some(ppid) => classify_waiter(true, ppid),
        },
    }
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
    fn meta_round_trips_with_kebab_lifecycle() {
        let root = temp_root("seat-meta");
        board::init(&root).unwrap();
        let dir = create(&root, "alpha").unwrap();
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
        let back = read_meta(&dir).unwrap();
        assert_eq!(back.harness, "switchboard");
        assert_eq!(back.lifecycle, Some(Lifecycle::ExitWake));
        // Wire format is the kebab-case string.
        let raw = std::fs::read_to_string(meta_path(&dir)).unwrap();
        assert!(raw.contains("\"exit-wake\""), "raw meta: {raw}");
        // Both lifecycle spellings parse.
        let poller: Meta =
            serde_json::from_str("{\"harness\":\"h\",\"lifecycle\":\"poller\"}").unwrap();
        assert_eq!(poller.lifecycle, Some(Lifecycle::Poller));
    }

    #[test]
    fn meta_launch_fields_round_trip_and_stay_absent() {
        let root = temp_root("seat-launch-meta");
        board::init(&root).unwrap();
        let dir = create(&root, "alpha").unwrap();
        // Absent launch fields are omitted from the file, and a meta
        // written before this feature parses unchanged (T001).
        let old = Meta { harness: "h".into(), ..Default::default() };
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
        assert_eq!(back.cwd.as_deref(), Some(root.display().to_string().as_str()));
        assert_eq!(
            back.cmd.as_deref(),
            Some(&["/bin/sh".to_string(), "-c".to_string(), "sleep 30".to_string()][..])
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

    #[test]
    fn wait_pid_round_trip_and_state() {
        let root = temp_root("seat-wait");
        board::init(&root).unwrap();
        let dir = create(&root, "alpha").unwrap();
        assert_eq!(read_wait_pid(&dir), None);
        assert_eq!(waiter_state(&dir), WaiterState::Idle);

        // Our own test process is alive; the state must track whatever
        // /proc says about our parentage (the Switchboard sandbox may
        // legally reparent us to 1, which is Orphan, not Live).
        write_wait_pid(&dir, std::process::id()).unwrap();
        assert_eq!(read_wait_pid(&dir), Some(std::process::id()));
        let expected = match ppid_of(std::process::id()) {
            None => WaiterState::Idle,
            Some(ppid) => classify_waiter(true, ppid),
        };
        assert_eq!(waiter_state(&dir), expected);

        clear_wait_pid(&dir).unwrap();
        assert_eq!(read_wait_pid(&dir), None);
        clear_wait_pid(&dir).unwrap(); // idempotent

        // A pid that cannot exist → stale pidfile reads Idle.
        write_wait_pid(&dir, u32::MAX).unwrap();
        assert_eq!(waiter_state(&dir), WaiterState::Idle);
    }

    #[test]
    fn orphan_classification() {
        assert_eq!(classify_waiter(false, 1), WaiterState::Idle);
        assert_eq!(classify_waiter(false, 42), WaiterState::Idle);
        assert_eq!(classify_waiter(true, 1), WaiterState::Orphan);
        assert_eq!(classify_waiter(true, 42), WaiterState::Live);
        // /proc sanity: pid 1 exists; an impossible pid does not.
        assert!(ppid_of(1).is_some());
        assert_eq!(ppid_of(u32::MAX), None);
        // Stale pidfile (pid gone) reads Idle even though a file exists.
        assert_eq!(
            waiter_state_stale(),
            WaiterState::Idle,
            "dead pid must classify Idle"
        );
    }

    fn waiter_state_stale() -> WaiterState {
        match ppid_of(u32::MAX) {
            None => WaiterState::Idle,
            Some(p) => classify_waiter(true, p),
        }
    }

    /// A zombie (exited, not reaped) is dead: it must not hold a seat.
    /// `arm --takeover` polls ppid_of, so an unreaped waiter must clear.
    #[test]
    fn zombie_pid_reads_as_dead() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        child.kill().expect("SIGKILL child");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while ppid_of(child.id()).is_some() {
            assert!(
                std::time::Instant::now() < deadline,
                "zombie never classified dead"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = child.wait().unwrap(); // reap
        assert_eq!(ppid_of(child.id()), None);
    }
}
