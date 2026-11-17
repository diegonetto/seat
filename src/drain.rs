use crate::board::{self};
use crate::dstate;
use crate::error::{Result, SeatError};
use crate::msg;
use crate::seat;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The `--hold` lease: how long held mail stays leased before it is
/// owed again. One constant, 30 minutes — the Matter bot's longest
/// run waits up to 20 minutes on an approval, so the lease outlives
/// the run instead of being renewed.
pub const LEASE: Duration = Duration::from_secs(30 * 60);

/// A staged file is leased while its mtime — the deadline a `--hold`
/// wrote — is later than now. Nothing else ever sets a future mtime
/// (`rename` keeps the send-time mtime), so a crashed drain's
/// leftovers and the hapi dispatcher's staged items are never leased.
/// Used in exactly two places: `receive` skips a leased file (not
/// printed, not quarantined, not committed) and `present_files`
/// leaves it out, so held mail is owed by nobody — no ring, no scan
/// worker, no alarm — until its deadline passes, and owed again the
/// moment after.
pub(crate) fn leased(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|at| at.duration_since(SystemTime::now()).ok())
        .is_some()
}

/// Set a file's mtime, leaving its atime alone. The lease deadline
/// lives in the mtime and nowhere else.
pub(crate) fn set_mtime(path: &Path, at: SystemTime) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let since_epoch = at
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
    let times = [
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
        libc::timespec {
            tv_sec: since_epoch.as_secs() as libc::time_t,
            tv_nsec: since_epoch.subsec_nanos() as libc::c_long,
        },
    ];
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    if unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// One verified direct message, staged in `draining/` and awaiting
/// commit. `file_name` (timestamp plus id) is the delivery key the
/// dispatcher will use.
#[derive(Debug, Clone)]
pub struct ReceivedDirect {
    pub file_name: String,
    pub header: msg::Header,
    pub body: String,
}

/// What one `receive` found: the verified mail worth delivering and
/// the failures it recorded instead.
///
/// `receive` never archives: a receive with no commit leaves every
/// message recoverable by the next drain (good direct mail stays in
/// `draining/`).
#[derive(Debug, Clone, Default)]
pub struct Received {
    pub directs: Vec<ReceivedDirect>,
    /// Failed verifications, in the order they were met (the first is
    /// the named error; the rest are printed on stderr).
    pub bad_marks: Vec<String>,
    /// Something unreadable was quarantined (no claimed sender to name).
    pub unreadable: bool,
}

/// The drain verb. Lock, receive, print, commit, unlock. Output, exit
/// codes, quarantine behaviour and stderr wording are unchanged: good
/// mail is archived and `last_drained` is stamped exactly as before
/// even when `receive` recorded failures — then the command exits
/// nonzero, so a bad message never wedges the next drain.
pub fn drain(root: &Path, seat_name: &str, out: &mut dyn Write) -> Result<()> {
    board::ensure_board(root)?;
    let dir = seat::seat_dir(root, seat_name);
    seat::read_meta(&dir)?; // unknown/corrupt seat is a named error

    // One drain of this seat at a time: a second drain waits (bounded),
    // never interleaves; other seats have their own locks.
    let _lock = seat::Flock::seat(&dir, seat_name)?;

    let received = receive(root, seat_name)?;

    for d in &received.directs {
        write!(out, "{}", dstate::direct_text(d))?;
    }

    commit(root, seat_name, &received)?;

    if !received.bad_marks.is_empty() {
        // The error names the first sender. Name any further ones here so
        // the process prints each claimed from once.
        for from in &received.bad_marks[1..] {
            eprintln!("drain: message from '{from}' failed mark verification; not delivered");
        }
        return Err(SeatError::BadMark(received.bad_marks[0].clone()));
    }
    if received.unreadable {
        return Err(SeatError::Unreadable);
    }
    Ok(())
}

/// The `drain --hold` verb: the same lock and the same receive as
/// `drain`, then for each good message set the staged file's mtime to
/// the lease deadline (`now + LEASE`), print it exactly as `drain`
/// prints, and commit nothing — the mail stays in `draining/` until
/// the bot that held it names its ids done (`done`) or the deadline
/// passes. Order per file: staged, deadline set, printed; a file
/// whose deadline cannot be set is not printed and the command fails
/// naming it. Mail leased by an earlier hold is left alone by
/// `receive` and printed by nobody. `last_drained` is not stamped.
pub fn hold(root: &Path, seat_name: &str, out: &mut dyn Write) -> Result<()> {
    board::ensure_board(root)?;
    let dir = seat::seat_dir(root, seat_name);
    seat::read_meta(&dir)?; // unknown/corrupt seat is a named error

    // One drain of this seat at a time, exactly as `drain`.
    let _lock = seat::Flock::seat(&dir, seat_name)?;

    let received = receive(root, seat_name)?;

    let deadline = SystemTime::now() + LEASE;
    for d in &received.directs {
        let staged = dir.join("draining").join(&d.file_name);
        set_mtime(&staged, deadline).map_err(|e| {
            SeatError::Drain(format!(
                "cannot set the lease deadline on {}: {e}",
                staged.display()
            ))
        })?;
        write!(out, "{}", dstate::direct_text(d))?;
    }

    if !received.bad_marks.is_empty() {
        for from in &received.bad_marks[1..] {
            eprintln!("drain: message from '{from}' failed mark verification; not delivered");
        }
        return Err(SeatError::BadMark(received.bad_marks[0].clone()));
    }
    if received.unreadable {
        return Err(SeatError::Unreadable);
    }
    Ok(())
}

/// The `drain --done` verb: under the seat lock, archive exactly the
/// staged files named by id — `*-<id>.msg` in `draining/`, leased or
/// not — and stamp `last_drained`. `inbox/` is never touched. Every
/// id is checked (8 lowercase hex) and every id's file count decided
/// before the first rename, so a malformed or ambiguous id refuses
/// the whole command and archives nothing. An id with no file in
/// `draining/` is success (already archived, or taken by a later
/// drain). Two files matching one id is refused, naming both. Prints
/// nothing.
pub fn done(root: &Path, seat_name: &str, ids: &[String]) -> Result<()> {
    board::ensure_board(root)?;
    let dir = seat::seat_dir(root, seat_name);
    seat::read_meta(&dir)?; // unknown/corrupt seat is a named error

    // Refused before the lock, before the listing, before anything
    // moves: one malformed id must not archive the others.
    for id in ids {
        if !msg::valid_msg_id(id) {
            return Err(SeatError::Drain(format!(
                "--done id {id:?} is not 8 lowercase hex characters"
            )));
        }
    }

    // One drain of this seat at a time, exactly as `drain`.
    let _lock = seat::Flock::seat(&dir, seat_name)?;

    let staged = msg::list_msg_dir(&dir.join("draining"))?;
    let matching = |id: &str| -> Vec<PathBuf> {
        let suffix = format!("-{id}.msg");
        staged
            .iter()
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(&suffix))
            })
            .cloned()
            .collect()
    };
    // The whole command is decided before the first rename: an
    // ambiguous id archives nothing.
    for id in ids {
        let matches = matching(id);
        if matches.len() > 1 {
            let both = matches
                .iter()
                .map(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or_default()
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join(" and ");
            return Err(SeatError::Drain(format!(
                "--done id {id} matches two files: {both}"
            )));
        }
    }
    for id in ids {
        if let Some(p) = matching(id).first() {
            let to = dir.join("archive").join(p.file_name().unwrap_or_default());
            std::fs::rename(p, to)?;
        }
    }
    seat::update_meta(&dir, |meta| {
        meta.last_drained = Some(seat::now_ts());
    })?;
    Ok(())
}

/// Stage and verify everything the seat owes one delivery pass. The
/// caller holds the seat's drain lock (this function takes no lock
/// itself). Inbox mail moves to `draining/` first (leftovers from a
/// crashed drain ride along), then each staged message is checked
/// exactly as `drain` always has: the sender name, the sender's token,
/// the HMAC, the destination. What fails is quarantined — never
/// delivered. A leased file (`--hold` still inside its lease) is
/// skipped whole: not printed, not quarantined, not committed.
pub fn receive(root: &Path, seat_name: &str) -> Result<Received> {
    board::ensure_board(root)?;
    let dir = seat::seat_dir(root, seat_name);
    seat::read_meta(&dir)?; // unknown/corrupt seat is a named error

    let draining = dir.join("draining");
    std::fs::create_dir_all(&draining)?;
    for p in msg::list_msg_dir(&dir.join("inbox"))? {
        let dest = draining.join(p.file_name().unwrap_or_default());
        std::fs::rename(&p, dest)?;
    }
    // Leftovers from a crashed drain ride this one too.
    let staged = msg::list_msg_dir(&draining)?;

    let mut received = Received::default();
    for p in staged {
        // Held mail is the holding bot's until its deadline passes.
        if leased(&p) {
            continue;
        }
        match check_direct(root, seat_name, &p) {
            Ok(item) => received.directs.push(item),
            Err(failure) => {
                quarantine(root, seat_name, &p)?;
                match failure {
                    DirectFailure::BadMark(from) => received.bad_marks.push(from),
                    DirectFailure::Unreadable => received.unreadable = true,
                }
            }
        }
    }

    Ok(received)
}

/// Archive the delivered direct messages, stamp `last_drained`. The
/// caller holds the seat's drain lock. Runs even when `receive`
/// recorded failures, so good mail is archived exactly as it always
/// did.
pub fn commit(root: &Path, seat_name: &str, received: &Received) -> Result<()> {
    let dir = seat::seat_dir(root, seat_name);
    for d in &received.directs {
        let archive = dir.join("archive").join(&d.file_name);
        std::fs::rename(dir.join("draining").join(&d.file_name), archive)?;
    }
    seat::update_meta(&dir, |meta| {
        meta.last_drained = Some(seat::now_ts());
    })?;
    Ok(())
}

enum DirectFailure {
    /// Mark (or envelope) does not verify; the claimed sender.
    BadMark(String),
    /// The header did not parse, so there is no claimed sender to name.
    Unreadable,
}

/// Verify one staged direct message against its claimed sender, the
/// same checks `drain` has always run. Nothing is printed and nothing
/// is archived here.
fn check_direct(
    root: &Path,
    seat_name: &str,
    path: &Path,
) -> std::result::Result<ReceivedDirect, DirectFailure> {
    let (header, body) = match msg::read_msg(path) {
        Ok(x) => x,
        Err(_) => {
            // A body byte that is not UTF-8 fails before the mark is
            // checked. The header still names who claimed to send it.
            return Err(match claimed_from(path) {
                Some(from) => DirectFailure::BadMark(from),
                None => DirectFailure::Unreadable,
            });
        }
    };
    // header.from is untrusted input that becomes a path component
    // (seats/<from>/token): reject anything that is not a valid name
    // before touching the filesystem with it.
    if board::valid_name(&header.from).is_err() {
        return Err(DirectFailure::BadMark(header.from.clone()));
    }
    let verified = match crate::verbs::sender_token(root, &header.from) {
        Ok(token) => msg::verify_msg(path, &token).unwrap_or(false),
        Err(_) => false,
    };
    if header.dest != seat_name || !verified {
        return Err(DirectFailure::BadMark(header.from.clone()));
    }
    Ok(ReceivedDirect {
        file_name: file_name_of(path),
        header,
        body,
    })
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string()
}

/// `from` when the header parsed, including when the body is not UTF-8.
fn claimed_from(path: &Path) -> Option<String> {
    let raw = std::fs::read(path).ok()?;
    let split = raw.windows(2).position(|w| w == b"\n\n")?;
    let value: serde_json::Value = serde_json::from_slice(&raw[..split]).ok()?;
    let from = value.get("from")?.as_str()?.to_string();
    if from.is_empty() {
        None
    } else {
        Some(from)
    }
}

/// Quarantine a direct message: move it out of the receive path.
fn quarantine(root: &Path, seat_name: &str, path: &Path) -> Result<PathBuf> {
    let qdir = seat::seat_dir(root, seat_name).join("quarantine");
    std::fs::create_dir_all(&qdir)?;
    let dest = qdir.join(path.file_name().unwrap_or_default());
    std::fs::rename(path, &dest)?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_root;
    use crate::verbs;

    fn setup() -> std::path::PathBuf {
        let root = temp_root("drain");
        board::init(&root).unwrap();
        verbs::cmd_register(&root, "alpha", "sb", None, None, Vec::new(), None).unwrap();
        verbs::cmd_register(&root, "beta", "sb", None, None, Vec::new(), None).unwrap();
        root
    }

    fn drain_str(root: &std::path::Path, seat: &str) -> (Result<()>, String) {
        let mut buf = Vec::new();
        let r = drain(root, seat, &mut buf);
        (r, String::from_utf8(buf).unwrap())
    }

    fn hold_str(root: &std::path::Path, seat: &str) -> (Result<()>, String) {
        let mut buf = Vec::new();
        let r = hold(root, seat, &mut buf);
        (r, String::from_utf8(buf).unwrap())
    }

    /// The 8-hex id a staged file's name carries.
    fn id_of(p: &Path) -> String {
        let name = p.file_name().unwrap().to_str().unwrap();
        name.strip_suffix(".msg")
            .unwrap()
            .rsplit_once('-')
            .unwrap()
            .1
            .to_string()
    }

    #[test]
    fn drain_delivers_archives_and_empties() {
        let root = setup();
        verbs::cmd_send(&root, Some("alpha"), "beta", "hello board", None).unwrap();

        let inbox = seat::seat_dir(&root, "beta").join("inbox");
        assert_eq!(msg::list_msg_dir(&inbox).unwrap().len(), 1);

        let (r, text) = drain_str(&root, "beta");
        r.unwrap();
        assert!(text.contains("hello board"));
        assert!(text.contains("[direct]"));
        assert!(msg::list_msg_dir(&inbox).unwrap().is_empty());
        assert_eq!(
            msg::list_msg_dir(&seat::seat_dir(&root, "beta").join("archive"))
                .unwrap()
                .len(),
            1
        );

        // A second, empty drain prints NOTHING and exits 0.
        let (r2, text2) = drain_str(&root, "beta");
        r2.unwrap();
        assert_eq!(text2, "", "empty drain prints nothing");
        let meta = seat::read_meta(&seat::seat_dir(&root, "beta")).unwrap();
        assert!(meta.last_drained.is_some());
    }

    /// drain's output for a delivered direct message is exactly the
    /// header line, the body, a blank line — pinned byte for byte
    /// against the archived message's own header fields, built here
    /// independently of the printing code under test.
    #[test]
    fn drain_prints_the_full_direct_text() {
        let root = setup();
        verbs::cmd_send(&root, Some("alpha"), "beta", "full body line", None).unwrap();

        let (r, text) = drain_str(&root, "beta");
        r.unwrap();

        let archived = msg::list_msg_dir(&seat::seat_dir(&root, "beta").join("archive"))
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        let (header, body) = msg::read_msg(&archived).unwrap();
        let expected = format!(
            "[direct] id={} from={} dest={} ts={}\n{}\n\n",
            header.id, header.from, header.dest, header.ts, body
        );
        assert_eq!(text, expected, "drain's output is the direct text");
    }

    #[test]
    fn tampered_message_names_from_and_exits_nonzero() {
        let root = setup();
        verbs::cmd_send(&root, Some("alpha"), "beta", "original body", None).unwrap();
        let inbox = seat::seat_dir(&root, "beta").join("inbox");
        let p = msg::list_msg_dir(&inbox)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        let raw = std::fs::read_to_string(&p).unwrap();
        std::fs::write(&p, raw.replace("original body", "tampered body")).unwrap();

        let (r, text) = drain_str(&root, "beta");
        let err = r.unwrap_err();
        assert!(
            matches!(err, SeatError::BadMark(ref f) if f == "alpha"),
            "{err}"
        );
        assert!(err.to_string().contains("from 'alpha'"), "{err}");
        assert!(!text.contains("tampered body"), "no body printed: {text}");
        let qdir = seat::seat_dir(&root, "beta").join("quarantine");
        assert_eq!(msg::list_msg_dir(&qdir).unwrap().len(), 1);
        assert!(msg::list_msg_dir(&inbox).unwrap().is_empty());
        // Board not wedged: the next drain is clean.
        let (r2, text2) = drain_str(&root, "beta");
        assert!(r2.is_ok());
        assert_eq!(text2, "");
    }

    #[test]
    fn invalid_utf8_body_names_from_and_exits_nonzero() {
        let root = setup();
        verbs::cmd_send(&root, Some("alpha"), "beta", "byte flip", None).unwrap();
        let inbox = seat::seat_dir(&root, "beta").join("inbox");
        let p = msg::list_msg_dir(&inbox)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        let mut raw = std::fs::read(&p).unwrap();
        let i = raw.windows(9).position(|w| w == b"byte flip").unwrap();
        raw[i] = 0xff;
        std::fs::write(&p, &raw).unwrap();

        let (r, text) = drain_str(&root, "beta");
        let err = r.unwrap_err();
        assert!(
            matches!(err, SeatError::BadMark(ref f) if f == "alpha"),
            "{err}"
        );
        assert!(err.to_string().contains("from 'alpha'"), "{err}");
        assert!(!text.contains("byte"), "no body printed: {text}");
        assert!(msg::list_msg_dir(&inbox).unwrap().is_empty());
    }

    #[test]
    fn drain_ignores_tmp_scratch_files_in_inbox() {
        let root = setup();
        verbs::cmd_send(&root, Some("alpha"), "beta", "real mail", None).unwrap();
        let inbox = seat::seat_dir(&root, "beta").join("inbox");
        // A concurrent writer's in-flight scratch file: same directory,
        // dot-prefixed, NOT a *.msg name.
        let tmp = inbox.join(".20260923T000000.000000-deadbeef.msg.tmp");
        std::fs::write(&tmp, "half-\n\nwritten").unwrap();

        let (r, text) = drain_str(&root, "beta");
        r.unwrap();
        assert!(text.contains("real mail"));
        assert!(!text.contains("half-"));
        // Not delivered, not quarantined, not touched: the writer owns it.
        assert!(tmp.exists());
        assert!(msg::list_msg_dir(&inbox).unwrap().is_empty());
    }

    #[test]
    fn forged_from_traversal_is_a_bad_mark() {
        let root = setup();
        // A token planted where a "../evil" from would resolve if used
        // naively as a path (seats/../evil/token). Without valid_name the
        // MAC would verify against it and the message would deliver.
        let evil = root.join("evil");
        std::fs::create_dir_all(&evil).unwrap();
        let token = msg::mint_token(&evil.join("token")).unwrap();
        let (path, _) =
            msg::write_msg(&root, "beta", "../evil", &token, "forged body", None).unwrap();
        let inbox = seat::seat_dir(&root, "beta").join("inbox");
        std::fs::rename(&path, inbox.join(path.file_name().unwrap())).unwrap();

        let (r, text) = drain_str(&root, "beta");
        let err = r.unwrap_err();
        assert!(matches!(err, SeatError::BadMark(_)), "{err}");
        assert!(!text.contains("forged body"));
    }

    #[test]
    fn drain_of_unknown_or_corrupt_seat_refuses() {
        let root = setup();
        let (r, _) = drain_str(&root, "ghost");
        // Drain reads meta at the storage level, so it keeps the
        // GhostSeat wording; read_meta_named callers get UnknownSeat.
        assert_eq!(
            r.unwrap_err().to_string(),
            "seat ghost has no token on this board"
        );
        std::fs::create_dir_all(root.join("seats").join("broken")).unwrap();
        std::fs::write(
            root.join("seats").join("broken").join("meta.json"),
            "{ nope",
        )
        .unwrap();
        let (r2, _) = drain_str(&root, "broken");
        assert!(matches!(r2, Err(SeatError::CorruptMeta { .. })));
    }

    /// Two concurrent drains of one seat never deliver a message twice
    /// and never error on a rename race: the seat lock serializes them.
    #[test]
    fn concurrent_drains_deliver_each_message_exactly_once() {
        let root = setup();
        const N: usize = 12;
        for i in 0..N {
            verbs::cmd_send(&root, Some("alpha"), "beta", &format!("mail {i}"), None).unwrap();
        }
        let mut handles = Vec::new();
        for t in 0..2u8 {
            let root2 = root.clone();
            handles.push(std::thread::spawn(move || {
                let mut buf = Vec::new();
                let r = drain(&root2, "beta", &mut buf);
                (r.is_ok(), String::from_utf8(buf).unwrap(), t)
            }));
        }
        let mut bodies: Vec<String> = Vec::new();
        for h in handles {
            let (ok, text, _t) = h.join().unwrap();
            assert!(ok, "a concurrent drain failed");
            for line in text.lines() {
                if !line.starts_with('[') && !line.is_empty() && line.starts_with("mail ") {
                    bodies.push(line.to_string());
                }
            }
        }
        bodies.sort();
        bodies.dedup();
        assert_eq!(
            bodies.len(),
            N,
            "each message delivered exactly once: {bodies:?}"
        );
        // Nothing stranded mid-flight, everything archived exactly once.
        let dir = seat::seat_dir(&root, "beta");
        assert!(msg::list_msg_dir(&dir.join("inbox")).unwrap().is_empty());
        assert!(msg::list_msg_dir(&dir.join("draining")).unwrap().is_empty());
        assert_eq!(msg::list_msg_dir(&dir.join("archive")).unwrap().len(), N);
    }

    /// A drain waits for a held lock (it does not error and does not
    /// interleave), then proceeds once the holder releases.
    #[test]
    fn drain_waits_for_a_held_lock_then_proceeds() {
        let root = setup();
        verbs::cmd_send(&root, Some("alpha"), "beta", "held seat", None).unwrap();
        let dir = seat::seat_dir(&root, "beta");
        let held = seat::Flock::seat(&dir, "beta").unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<(Result<()>, String)>();
        let root2 = root.clone();
        let waiter = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let r = drain(&root2, "beta", &mut buf);
            let _ = tx.send((r, String::from_utf8(buf).unwrap()));
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        // Still blocked on the lock, nothing delivered yet.
        assert!(rx.try_recv().is_err(), "drain ran while the lock was held");
        drop(held);
        let (r, text) = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        waiter.join().unwrap();
        r.unwrap();
        assert!(text.contains("held seat"));
    }

    /// receive with no commit leaves every message recoverable by the
    /// next drain: good direct mail stays in draining/, and
    /// last_drained is not stamped.
    #[test]
    fn receive_without_commit_is_recoverable() {
        let root = setup();
        verbs::cmd_send(&root, Some("alpha"), "beta", "direct recover", None).unwrap();

        let received = receive(&root, "beta").unwrap();
        assert_eq!(received.directs.len(), 1);
        assert!(received.bad_marks.is_empty() && !received.unreadable);

        // The delivered item carries its file name: timestamp plus id.
        for name in received.directs.iter().map(|d| d.file_name.as_str()) {
            assert!(
                name.ends_with(".msg")
                    && name.split_once('-').is_some_and(|(ts, _)| {
                        ts.len() == 22
                            && ts.bytes().nth(8) == Some(b'T')
                            && ts
                                .bytes()
                                .all(|b| b.is_ascii_digit() || b == b'T' || b == b'.')
                    }),
                "file name is timestamp plus id: {name}"
            );
        }

        // Nothing archived, no last_drained stamped.
        let dir = seat::seat_dir(&root, "beta");
        assert!(msg::list_msg_dir(&dir.join("archive")).unwrap().is_empty());
        let meta = seat::read_meta(&dir).unwrap();
        assert!(meta.last_drained.is_none());

        // The next drain recovers everything exactly once.
        let (r, text) = drain_str(&root, "beta");
        r.unwrap();
        assert!(text.contains("direct recover"));
        assert_eq!(msg::list_msg_dir(&dir.join("draining")).unwrap().len(), 0);
        assert_eq!(msg::list_msg_dir(&dir.join("archive")).unwrap().len(), 1);
    }

    /// One quarantined message does not stop the good mail: commit
    /// runs, the good messages are archived, and drain still exits
    /// nonzero with today's stderr.
    #[test]
    fn quarantined_message_still_archives_the_good_ones() {
        let root = setup();
        verbs::cmd_send(&root, Some("alpha"), "beta", "good one", None).unwrap();
        verbs::cmd_send(&root, Some("alpha"), "beta", "bad one", None).unwrap();
        verbs::cmd_send(&root, Some("alpha"), "beta", "good two", None).unwrap();
        let inbox = seat::seat_dir(&root, "beta").join("inbox");
        let files = msg::list_msg_dir(&inbox).unwrap();
        assert_eq!(files.len(), 3);
        // Tamper the middle message.
        let raw = std::fs::read_to_string(&files[1]).unwrap();
        std::fs::write(&files[1], raw.replace("bad one", "tampered one")).unwrap();

        let (r, text) = drain_str(&root, "beta");
        let err = r.unwrap_err();
        assert!(
            matches!(err, SeatError::BadMark(ref f) if f == "alpha"),
            "{err}"
        );
        assert!(err.to_string().contains("from 'alpha'"), "{err}");
        // The good ones were delivered and archived.
        assert!(
            text.contains("good one") && text.contains("good two"),
            "{text}"
        );
        assert!(!text.contains("tampered one"), "{text}");
        let dir = seat::seat_dir(&root, "beta");
        assert_eq!(msg::list_msg_dir(&dir.join("archive")).unwrap().len(), 2);
        assert_eq!(msg::list_msg_dir(&dir.join("quarantine")).unwrap().len(), 1);
        // last_drained stamped exactly as today, board not wedged.
        assert!(seat::read_meta(&dir).unwrap().last_drained.is_some());
        let (r2, text2) = drain_str(&root, "beta");
        assert!(r2.is_ok());
        assert_eq!(text2, "");
    }

    /// A drain holding another seat's lock is never blocked: locks are
    /// per seat directory.
    #[test]
    fn drain_of_one_seat_is_not_blocked_by_anothers_lock() {
        let root = setup();
        verbs::cmd_register(&root, "gamma", "sb", None, None, Vec::new(), None).unwrap();
        verbs::cmd_send(&root, Some("alpha"), "beta", "other seat", None).unwrap();
        let _gamma_lock = seat::Flock::seat(&seat::seat_dir(&root, "gamma"), "gamma").unwrap();
        let started = std::time::Instant::now();
        let (r, text) = drain_str(&root, "beta");
        r.unwrap();
        assert!(text.contains("other seat"));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "a foreign seat's lock blocked this drain"
        );
    }

    /// `--hold` prints the mail exactly as `drain` prints it, sets
    /// each staged file's mtime to the lease deadline (a future
    /// mtime), archives nothing and stamps nothing. Inside the lease a
    /// second `--hold` and a plain `drain` print nothing and move
    /// nothing.
    #[test]
    fn hold_prints_leases_and_moves_nothing() {
        let root = setup();
        verbs::cmd_send(&root, Some("alpha"), "beta", "held body", None).unwrap();
        let dir = seat::seat_dir(&root, "beta");

        let (r, text) = hold_str(&root, "beta");
        r.unwrap();
        assert!(text.contains("held body"));
        assert!(text.contains("[direct]"));

        // Leased: one file in draining/ with a future mtime, an empty
        // archive, no last_drained stamped.
        let staged = msg::list_msg_dir(&dir.join("draining")).unwrap();
        assert_eq!(staged.len(), 1);
        assert!(leased(&staged[0]), "the deadline is the file's mtime");
        assert!(msg::list_msg_dir(&dir.join("archive")).unwrap().is_empty());
        assert!(seat::read_meta(&dir).unwrap().last_drained.is_none());

        // Inside the lease nobody takes it: not a second hold...
        let (r2, text2) = hold_str(&root, "beta");
        r2.unwrap();
        assert_eq!(text2, "", "a second --hold inside the lease prints nothing");
        // ...and not a plain drain.
        let (r3, text3) = drain_str(&root, "beta");
        r3.unwrap();
        assert_eq!(text3, "", "a plain drain inside the lease prints nothing");
        assert_eq!(msg::list_msg_dir(&dir.join("draining")).unwrap().len(), 1);
        assert!(msg::list_msg_dir(&dir.join("archive")).unwrap().is_empty());
    }

    /// The deadline is set at the hold, not inherited from the send:
    /// mail that sat in `inbox/` longer than the lease before the hold
    /// is still leased for the whole 30 minutes after it.
    #[test]
    fn a_hold_leases_even_mail_older_than_the_lease() {
        let root = setup();
        verbs::cmd_send(&root, Some("alpha"), "beta", "old mail", None).unwrap();
        let dir = seat::seat_dir(&root, "beta");
        // The send-time mtime, a full lease plus a minute ago.
        let sent = SystemTime::now() - LEASE - Duration::from_secs(60);
        let inbox = msg::list_msg_dir(&dir.join("inbox")).unwrap();
        assert_eq!(inbox.len(), 1);
        set_mtime(&inbox[0], sent).unwrap();
        assert!(!leased(&inbox[0]));

        let (r, text) = hold_str(&root, "beta");
        r.unwrap();
        assert!(text.contains("old mail"));
        let staged = msg::list_msg_dir(&dir.join("draining")).unwrap();
        assert_eq!(staged.len(), 1);
        assert!(leased(&staged[0]), "the deadline was set at the hold");
    }

    /// A leftover from a receive without commit (no hold) is not
    /// leased — nothing ever set its mtime — so the next plain
    /// `drain` prints and archives it exactly as before the hold
    /// existed.
    #[test]
    fn a_receive_leftover_is_not_leased() {
        let root = setup();
        verbs::cmd_send(&root, Some("alpha"), "beta", "leftover", None).unwrap();
        let dir = seat::seat_dir(&root, "beta");
        let received = receive(&root, "beta").unwrap();
        assert_eq!(received.directs.len(), 1);

        let staged = msg::list_msg_dir(&dir.join("draining")).unwrap();
        assert_eq!(staged.len(), 1);
        assert!(
            !leased(&staged[0]),
            "a crashed drain's leftover is not held"
        );

        let (r, text) = drain_str(&root, "beta");
        r.unwrap();
        assert!(text.contains("leftover"));
        assert_eq!(msg::list_msg_dir(&dir.join("archive")).unwrap().len(), 1);
        assert!(msg::list_msg_dir(&dir.join("draining")).unwrap().is_empty());
    }

    /// `--done` archives exactly the named ids (leased or not), never
    /// touches `inbox/`, stamps `last_drained`, and prints nothing. A
    /// repeat exits 0 and moves nothing; an id found only in `inbox/`
    /// is success and the inbox file stays there; a malformed id
    /// refuses the whole command before anything moves.
    #[test]
    fn done_archives_exactly_the_named_ids() {
        let root = setup();
        verbs::cmd_send(&root, Some("alpha"), "beta", "one", None).unwrap();
        verbs::cmd_send(&root, Some("alpha"), "beta", "two", None).unwrap();
        verbs::cmd_send(&root, Some("alpha"), "beta", "three", None).unwrap();
        let dir = seat::seat_dir(&root, "beta");

        let (r, text) = hold_str(&root, "beta");
        r.unwrap();
        assert!(text.contains("one") && text.contains("two") && text.contains("three"));
        let staged = msg::list_msg_dir(&dir.join("draining")).unwrap();
        assert_eq!(staged.len(), 3);
        let ids: Vec<String> = staged.iter().map(|p| id_of(p)).collect();

        // Mail that is only in inbox/ names an id `--done` must not
        // touch, even when its id matches.
        verbs::cmd_send(&root, Some("alpha"), "beta", "still inbox", None).unwrap();
        let inbox = msg::list_msg_dir(&dir.join("inbox")).unwrap();
        assert_eq!(inbox.len(), 1);

        done(&root, "beta", &ids[0..2]).unwrap();
        assert_eq!(msg::list_msg_dir(&dir.join("archive")).unwrap().len(), 2);
        assert_eq!(msg::list_msg_dir(&dir.join("draining")).unwrap().len(), 1);
        assert_eq!(msg::list_msg_dir(&dir.join("inbox")).unwrap().len(), 1);
        assert!(seat::read_meta(&dir).unwrap().last_drained.is_some());

        // A repeat of the same command exits 0 and moves nothing.
        done(&root, "beta", &ids[0..2]).unwrap();
        assert_eq!(msg::list_msg_dir(&dir.join("archive")).unwrap().len(), 2);
        assert_eq!(msg::list_msg_dir(&dir.join("draining")).unwrap().len(), 1);
        assert_eq!(msg::list_msg_dir(&dir.join("inbox")).unwrap().len(), 1);

        // An id only in inbox/ is success — and inbox/ is left alone.
        done(&root, "beta", &[id_of(&inbox[0])]).unwrap();
        assert_eq!(msg::list_msg_dir(&dir.join("inbox")).unwrap().len(), 1);
        assert_eq!(msg::list_msg_dir(&dir.join("archive")).unwrap().len(), 2);
        assert_eq!(msg::list_msg_dir(&dir.join("draining")).unwrap().len(), 1);

        // A malformed id is refused before anything moves.
        let err = done(&root, "beta", &[ids[2].clone(), "NOPE".into()]).unwrap_err();
        assert!(err.to_string().contains("8 lowercase hex"), "{err}");
        assert_eq!(msg::list_msg_dir(&dir.join("archive")).unwrap().len(), 2);
        assert_eq!(msg::list_msg_dir(&dir.join("draining")).unwrap().len(), 1);
    }

    /// Two staged files matching one id is refused naming both, and
    /// the refusal is decided before the first rename: a command
    /// naming one clean id and the ambiguous one archives neither.
    #[test]
    fn two_files_matching_one_done_id_is_refused_naming_both() {
        let root = setup();
        verbs::cmd_send(&root, Some("alpha"), "beta", "first", None).unwrap();
        verbs::cmd_send(&root, Some("alpha"), "beta", "second", None).unwrap();
        let dir = seat::seat_dir(&root, "beta");
        let (r, _) = hold_str(&root, "beta");
        r.unwrap();

        // A second staged file with the same id, a different
        // timestamp: two files now match that id.
        let staged = msg::list_msg_dir(&dir.join("draining")).unwrap();
        let dupe_name = format!("20200101T000000.000000-{}.msg", id_of(&staged[0]));
        std::fs::copy(&staged[0], dir.join("draining").join(&dupe_name)).unwrap();

        let err = done(&root, "beta", &[id_of(&staged[1]), id_of(&staged[0])]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("matches two files"), "{msg}");
        assert!(
            msg.contains(&dupe_name)
                && msg.contains(staged[0].file_name().unwrap().to_str().unwrap()),
            "the refusal names both files: {msg}"
        );
        // Decided before the first rename: neither id archived.
        assert!(msg::list_msg_dir(&dir.join("archive")).unwrap().is_empty());
        assert_eq!(msg::list_msg_dir(&dir.join("draining")).unwrap().len(), 3);
    }

    /// `--done` takes a leased file as happily as an expired one: the
    /// lease holds a `drain` off held mail, never the bot's own done.
    #[test]
    fn done_archives_a_leased_file() {
        let root = setup();
        verbs::cmd_send(&root, Some("alpha"), "beta", "leased done", None).unwrap();
        let dir = seat::seat_dir(&root, "beta");
        let (r, _) = hold_str(&root, "beta");
        r.unwrap();
        let staged = msg::list_msg_dir(&dir.join("draining")).unwrap();
        assert_eq!(staged.len(), 1);
        assert!(leased(&staged[0]));

        done(&root, "beta", &[id_of(&staged[0])]).unwrap();
        assert!(msg::list_msg_dir(&dir.join("draining")).unwrap().is_empty());
        assert_eq!(msg::list_msg_dir(&dir.join("archive")).unwrap().len(), 1);
    }
}
