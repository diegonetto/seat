use crate::board::{self};
use crate::error::{Result, SeatError};
use crate::msg;
use crate::seat;
use std::io::Write;
use std::path::{Path, PathBuf};

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

/// The digest verb. Lock, receive, print, commit, unlock. Output, exit
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
        writeln!(
            out,
            "[direct] id={} from={} dest={} ts={}",
            d.header.id, d.header.from, d.header.dest, d.header.ts
        )?;
        writeln!(out, "{}", d.body)?;
        writeln!(out)?;
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

/// Stage and verify everything the seat owes one delivery pass. The
/// caller holds the seat's drain lock (this function takes no lock
/// itself). Inbox mail moves to `draining/` first (leftovers from a
/// crashed drain ride along), then each staged message is checked
/// exactly as `drain` always has: the sender name, the sender's token,
/// the HMAC, the destination. What fails is quarantined — never
/// delivered.
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
        assert!(r.is_err());
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
}
