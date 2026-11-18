use crate::board::{self};
use crate::dstate;
use crate::error::{Result, SeatError};
use crate::msg;
use crate::seat;
use std::collections::{BTreeMap, BTreeSet};
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
/// leaves it out.
pub(crate) fn leased(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|at| at.duration_since(SystemTime::now()).ok())
        .is_some()
}

/// Set a file's mtime, leaving its atime alone.
pub(crate) fn set_mtime(path: &Path, at: SystemTime) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .set_times(std::fs::FileTimes::new().set_modified(at))
}

/// One verified direct message, staged in `draining/` and awaiting
/// commit. `file_name` (timestamp plus id) is the delivery key the
/// dispatcher will use.
#[derive(Debug, Clone)]
pub struct ReceivedDirect {
    pub file_name: String,
    pub header: msg::Header,
    pub body: String,
    /// The file was already in `draining/` before this receive staged
    /// the inbox (an expired hold, a crashed drain, or mail the hapi
    /// dispatcher staged and pushed), so it may have been shown
    /// before. `check_direct` leaves it false; `receive` sets it.
    pub redelivery: bool,
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
        write_direct(d, out)?;
    }

    commit(root, seat_name, &received)?;

    report_failures(&received)
}

/// Print one delivered message the way `drain` and `hold` do: mail
/// that may have been shown before says so first — the reader checks
/// whether it already did this — then the direct text.
fn write_direct(d: &ReceivedDirect, out: &mut dyn Write) -> std::io::Result<()> {
    if d.redelivery {
        write!(
            out,
            "[redelivery] check whether you already did this; if so, only run: seat done {}\n",
            d.header.id
        )?;
    }
    write!(out, "{}", dstate::direct_text(d))
}

/// Report `receive`'s failures the way every drain verb does: the
/// first bad mark is the named error, an unreadable quarantine is its
/// own, and good mail has already been handled.
fn report_failures(received: &Received) -> Result<()> {
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
/// prints it (redelivery line included), and commit nothing — the
/// mail stays in `draining/` until the bot that held it names its ids
/// done (`done`) or the deadline passes. Order per file: staged,
/// deadline set, printed; a file whose deadline cannot be set is not
/// printed and the command fails naming it. Mail leased by an earlier
/// hold is left alone by `receive` and printed by nobody. A hold
/// stamps `last_drained` on every call — after printing, including a
/// call that printed nothing: a reader that only holds still reads.
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
        write_direct(d, out)?;
    }

    // The hold itself is a read: stamp the reader alive (as `commit`
    // does), even when nothing was printed.
    seat::update_meta(&dir, |meta| {
        meta.last_drained = Some(seat::now_ts());
    })?;

    report_failures(&received)
}

/// The `done` verb (`seat done <ID>...`, and `drain --seat X --done
/// ID...`): find each id anywhere on the board and archive the mail
/// it names. Every id is checked (8 lowercase hex) and every id is
/// decided before the first rename, so a malformed, ambiguous or
/// unaccounted-for id refuses the whole command and archives nothing.
/// Decision order per id: two or more `*-<id>.msg` files in any
/// seat's `draining/` is refused, naming both as
/// `<seat>/draining/<file>`; exactly one is the pick, leased or not;
/// else a file already in some `archive/` is success (already done,
/// nothing moves); else a file in `inbox/` is refused (`drain it
/// first`); else one in `quarantine/` is refused (never delivered);
/// else the id is not on this board. The renames then run per seat in
/// sorted order, each under that seat's lock alone (never two seats'
/// locks at once), stamping `last_drained` on each seat a file was
/// archived for. NotFound on a rename is success: a racing done took
/// it. Prints nothing.
pub fn done(root: &Path, ids: &[String]) -> Result<()> {
    board::ensure_board(root)?;

    // Refused before the listing, before anything moves: one
    // malformed id must not archive the others.
    for id in ids {
        if !msg::valid_msg_id(id) {
            return Err(SeatError::Done(format!(
                "id {id:?} is not 8 lowercase hex characters"
            )));
        }
    }

    let seats = crate::verbs::sorted_seat_names(root)?;
    // Every seat's `*-<id>.msg` in one directory, as (seat, file)
    // pairs — a missing directory is an empty list.
    let find = |id: &str, sub: &str| -> Result<Vec<(String, String)>> {
        let suffix = format!("-{id}.msg");
        let mut found = Vec::new();
        for seat_name in &seats {
            for p in msg::list_msg_dir(&seat::seat_dir(root, seat_name).join(sub))? {
                if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                    if name.ends_with(&suffix) {
                        found.push((seat_name.clone(), name.to_string()));
                    }
                }
            }
        }
        Ok(found)
    };
    // The whole command is decided before the first rename: any
    // refusal archives nothing.
    let mut picks: Vec<(String, String)> = Vec::new();
    for id in ids {
        let staged = find(id, "draining")?;
        if staged.len() > 1 {
            let both = staged
                .iter()
                .map(|(s, f)| format!("{s}/draining/{f}"))
                .collect::<Vec<_>>()
                .join(" and ");
            return Err(SeatError::Done(format!(
                "id {id} matches two files: {both}"
            )));
        }
        if let Some((seat_name, file)) = staged.into_iter().next() {
            picks.push((seat_name, file));
            continue;
        }
        if !find(id, "archive")?.is_empty() {
            continue; // already done: success, nothing moves
        }
        if let Some((seat_name, _)) = find(id, "inbox")?.into_iter().next() {
            return Err(SeatError::Done(format!(
                "id {id} is unread in {seat_name}/inbox; drain it first"
            )));
        }
        if let Some((seat_name, _)) = find(id, "quarantine")?.into_iter().next() {
            return Err(SeatError::Done(format!(
                "id {id} is in {seat_name}/quarantine; it was never delivered"
            )));
        }
        return Err(SeatError::Done(format!("id {id} is not on this board")));
    }
    // Per seat in sorted order, under that seat's lock alone.
    let mut by_seat: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (seat_name, file) in picks {
        by_seat.entry(seat_name).or_default().push(file);
    }
    for (seat_name, files) in by_seat {
        let dir = seat::seat_dir(root, &seat_name);
        let _lock = seat::Flock::seat(&dir, &seat_name)?;
        let mut archived_any = false;
        for file in files {
            let to = dir.join("archive").join(&file);
            // The picks are a snapshot, so an id repeated in one
            // command renames its file once and then, like a file
            // taken by a racing done, finds it gone: success, not an
            // error (commit_invoked tolerates the same race).
            if let Err(e) = std::fs::rename(dir.join("draining").join(&file), &to) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    return Err(e.into());
                }
            } else {
                archived_any = true;
            }
        }
        if archived_any {
            seat::update_meta(&dir, |meta| {
                meta.last_drained = Some(seat::now_ts());
            })?;
        }
    }
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
    // What was already staged before this receive: a leftover from a
    // crashed drain, an expired hold, or mail the hapi dispatcher
    // staged and pushed — any of it may have been shown before, so
    // its file name is remembered for the redelivery line.
    let redeliveries: BTreeSet<String> = msg::list_msg_dir(&draining)?
        .iter()
        .map(|p| file_name_of(p))
        .collect();
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
            Ok(mut item) => {
                item.redelivery = redeliveries.contains(&item.file_name);
                received.directs.push(item);
            }
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
        // Set by `receive` when the file was staged before this pass.
        redelivery: false,
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
        verbs::cmd_register(&root, "alpha", "sb", None, None, Vec::new(), None, None).unwrap();
        verbs::cmd_register(&root, "beta", "sb", None, None, Vec::new(), None, None).unwrap();
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
        verbs::cmd_register(&root, "gamma", "sb", None, None, Vec::new(), None, None).unwrap();
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
    /// mtime), archives nothing, and stamps `last_drained` (the
    /// reader is alive). Inside the lease a second `--hold` and a
    /// plain `drain` print nothing and move nothing.
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
        // archive — and last_drained stamped: the reader is alive.
        let staged = msg::list_msg_dir(&dir.join("draining")).unwrap();
        assert_eq!(staged.len(), 1);
        assert!(leased(&staged[0]), "the deadline is the file's mtime");
        assert!(msg::list_msg_dir(&dir.join("archive")).unwrap().is_empty());
        assert!(seat::read_meta(&dir).unwrap().last_drained.is_some());

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
    /// existed, under the redelivery line first: it may have been
    /// shown before.
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
        // The leftover was in draining/ before this drain's receive:
        // the redelivery line precedes its direct text.
        let line = format!(
            "[redelivery] check whether you already did this; if so, only run: seat done {}\n",
            id_of(&staged[0])
        );
        assert!(text.starts_with(&line), "the line comes first: {text}");
        assert!(text.contains("leftover"));
        assert_eq!(msg::list_msg_dir(&dir.join("archive")).unwrap().len(), 1);
        assert!(msg::list_msg_dir(&dir.join("draining")).unwrap().is_empty());
    }

    /// `done` archives exactly the named ids (leased or not), never
    /// touches `inbox/`, stamps `last_drained`, and prints nothing. A
    /// repeat exits 0 and moves nothing; an id found only in `inbox/`
    /// is refused (`drain it first`) and nothing moves; a malformed
    /// id refuses the whole command before anything moves.
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

        // Mail that is only in inbox/ names an id `done` must not
        // touch, even when its id matches.
        verbs::cmd_send(&root, Some("alpha"), "beta", "still inbox", None).unwrap();
        let inbox = msg::list_msg_dir(&dir.join("inbox")).unwrap();
        assert_eq!(inbox.len(), 1);

        done(&root, &ids[0..2]).unwrap();
        assert_eq!(msg::list_msg_dir(&dir.join("archive")).unwrap().len(), 2);
        assert_eq!(msg::list_msg_dir(&dir.join("draining")).unwrap().len(), 1);
        assert_eq!(msg::list_msg_dir(&dir.join("inbox")).unwrap().len(), 1);
        assert!(seat::read_meta(&dir).unwrap().last_drained.is_some());

        // A repeat of the same command exits 0 and moves nothing.
        done(&root, &ids[0..2]).unwrap();
        assert_eq!(msg::list_msg_dir(&dir.join("archive")).unwrap().len(), 2);
        assert_eq!(msg::list_msg_dir(&dir.join("draining")).unwrap().len(), 1);
        assert_eq!(msg::list_msg_dir(&dir.join("inbox")).unwrap().len(), 1);

        // An id only in inbox/ is refused — and inbox/ is left alone.
        let err = done(&root, &[id_of(&inbox[0])]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unread in beta/inbox"), "{msg}");
        assert!(msg.contains("drain it first"), "{msg}");
        assert_eq!(msg::list_msg_dir(&dir.join("inbox")).unwrap().len(), 1);
        assert_eq!(msg::list_msg_dir(&dir.join("archive")).unwrap().len(), 2);
        assert_eq!(msg::list_msg_dir(&dir.join("draining")).unwrap().len(), 1);

        // A malformed id is refused before anything moves.
        let err = done(&root, &[ids[2].clone(), "NOPE".into()]).unwrap_err();
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

        let err = done(&root, &[id_of(&staged[1]), id_of(&staged[0])]).unwrap_err();
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

        done(&root, &[id_of(&staged[0])]).unwrap();
        assert!(msg::list_msg_dir(&dir.join("draining")).unwrap().is_empty());
        assert_eq!(msg::list_msg_dir(&dir.join("archive")).unwrap().len(), 1);
    }

    /// An id repeated in one `--done` is success, like an id with no
    /// file: the file archives once and the command still stamps
    /// `last_drained` and exits 0.
    #[test]
    fn done_with_a_repeated_id_archives_once_and_succeeds() {
        let root = setup();
        verbs::cmd_send(&root, Some("alpha"), "beta", "repeat me", None).unwrap();
        let dir = seat::seat_dir(&root, "beta");
        let (r, _) = hold_str(&root, "beta");
        r.unwrap();
        let staged = msg::list_msg_dir(&dir.join("draining")).unwrap();
        assert_eq!(staged.len(), 1);
        let id = id_of(&staged[0]);

        done(&root, &[id.clone(), id.clone()]).unwrap();
        assert!(msg::list_msg_dir(&dir.join("draining")).unwrap().is_empty());
        assert_eq!(msg::list_msg_dir(&dir.join("archive")).unwrap().len(), 1);
        assert!(seat::read_meta(&dir).unwrap().last_drained.is_some());
    }

    /// `seat done` names no seat: a held id is archived wherever the
    /// board holds it, a repeat of an archived id succeeds, and an
    /// already-archived id succeeds with nothing to move.
    #[test]
    fn done_without_a_seat_archives_a_held_id_board_wide() {
        let root = setup();
        verbs::cmd_send(&root, Some("alpha"), "beta", "board wide", None).unwrap();
        let dir = seat::seat_dir(&root, "beta");
        let (r, _) = hold_str(&root, "beta");
        r.unwrap();
        let staged = msg::list_msg_dir(&dir.join("draining")).unwrap();
        assert_eq!(staged.len(), 1);
        let id = id_of(&staged[0]);

        done(&root, &[id.clone()]).unwrap();
        assert!(msg::list_msg_dir(&dir.join("draining")).unwrap().is_empty());
        assert_eq!(msg::list_msg_dir(&dir.join("archive")).unwrap().len(), 1);

        // A repeat (the file is now archived) succeeds and moves
        // nothing.
        done(&root, &[id]).unwrap();
        assert_eq!(msg::list_msg_dir(&dir.join("archive")).unwrap().len(), 1);
    }

    /// An unknown id, an inbox id and a quarantined id are each
    /// refused with their words, and nothing moves.
    #[test]
    fn done_refuses_unknown_inbox_and_quarantined_ids() {
        let root = setup();
        verbs::cmd_send(&root, Some("alpha"), "beta", "unread one", None).unwrap();
        let dir = seat::seat_dir(&root, "beta");
        let inbox = msg::list_msg_dir(&dir.join("inbox")).unwrap();
        assert_eq!(inbox.len(), 1);

        // Unknown: not on this board.
        let err = done(&root, &["0000000f".into()]).unwrap_err();
        assert!(
            err.to_string().contains("0000000f is not on this board"),
            "{err}"
        );

        // Unread: drain it first.
        let err = done(&root, &[id_of(&inbox[0])]).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("unread in beta/inbox"), "{text}");
        assert!(text.contains("drain it first"), "{text}");
        assert_eq!(msg::list_msg_dir(&dir.join("inbox")).unwrap().len(), 1);

        // Quarantined: it was never delivered.
        verbs::cmd_send(&root, Some("alpha"), "beta", "quarantine me", None).unwrap();
        let files = msg::list_msg_dir(&dir.join("inbox")).unwrap();
        let raw = std::fs::read_to_string(&files[1]).unwrap();
        std::fs::write(&files[1], raw.replace("quarantine me", "tampered me")).unwrap();
        let (r, _) = drain_str(&root, "beta");
        r.unwrap_err(); // the bad mark is the named error
        let qdir = dir.join("quarantine");
        let quarantined = msg::list_msg_dir(&qdir).unwrap();
        assert_eq!(quarantined.len(), 1);
        let err = done(&root, &[id_of(&quarantined[0])]).unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("is in beta/quarantine; it was never delivered"),
            "{text}"
        );
        assert_eq!(msg::list_msg_dir(&qdir).unwrap().len(), 1);
    }

    /// Two `draining/` matches in two seats are refused naming both
    /// as `<seat>/draining/<file>`; one `draining/` match beside an
    /// archived one (an id collision) archives the `draining/` file.
    #[test]
    fn done_two_seats_matches_refuse_and_collisions_archive_the_draining_one() {
        let root = setup();
        verbs::cmd_send(&root, Some("alpha"), "beta", "first", None).unwrap();
        verbs::cmd_send(&root, Some("alpha"), "beta", "second", None).unwrap();
        let beta_dir = seat::seat_dir(&root, "beta");
        let (r, _) = hold_str(&root, "beta");
        r.unwrap();
        let staged = msg::list_msg_dir(&beta_dir.join("draining")).unwrap();
        assert_eq!(staged.len(), 2);
        let name0 = staged[0].file_name().unwrap().to_str().unwrap().to_string();
        let name1 = staged[1].file_name().unwrap().to_str().unwrap().to_string();

        // The same file staged in another seat: two matches on the
        // board.
        let alpha_dir = seat::seat_dir(&root, "alpha");
        std::fs::copy(&staged[0], alpha_dir.join("draining").join(&name0)).unwrap();
        let err = done(&root, &[id_of(&staged[0])]).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("matches two files"), "{text}");
        assert!(
            text.contains(&format!("alpha/draining/{name0}"))
                && text.contains(&format!("beta/draining/{name0}")),
            "the refusal names both seats' files: {text}"
        );
        // Nothing moved.
        assert_eq!(msg::list_msg_dir(&beta_dir.join("draining")).unwrap().len(), 2);
        assert_eq!(
            msg::list_msg_dir(&alpha_dir.join("draining")).unwrap().len(),
            1
        );

        // An id collision — the other staged file also exists,
        // archived, under alpha — archives the draining one and
        // leaves the archived file alone.
        std::fs::copy(&staged[1], alpha_dir.join("archive").join(&name1)).unwrap();
        done(&root, &[id_of(&staged[1])]).unwrap();
        assert_eq!(msg::list_msg_dir(&beta_dir.join("draining")).unwrap().len(), 1);
        assert_eq!(msg::list_msg_dir(&beta_dir.join("archive")).unwrap().len(), 1);
        assert_eq!(
            msg::list_msg_dir(&alpha_dir.join("archive")).unwrap().len(),
            1
        );
    }

    /// One good id and one unknown id in one command: every id is
    /// decided before the first rename, so nothing archives.
    #[test]
    fn done_with_one_good_and_one_unknown_id_archives_nothing() {
        let root = setup();
        verbs::cmd_send(&root, Some("alpha"), "beta", "the good id", None).unwrap();
        let dir = seat::seat_dir(&root, "beta");
        let (r, _) = hold_str(&root, "beta");
        r.unwrap();
        let staged = msg::list_msg_dir(&dir.join("draining")).unwrap();
        assert_eq!(staged.len(), 1);

        let err = done(&root, &[id_of(&staged[0]), "0000000f".into()]).unwrap_err();
        assert!(
            err.to_string().contains("0000000f is not on this board"),
            "{err}"
        );
        assert!(msg::list_msg_dir(&dir.join("archive")).unwrap().is_empty());
        assert_eq!(msg::list_msg_dir(&dir.join("draining")).unwrap().len(), 1);
    }

    /// `last_drained` is stamped on the seat whose file was archived,
    /// only there; and a hold of an empty seat stamps it too — the
    /// reader is alive even when it printed nothing.
    #[test]
    fn done_stamps_last_drained_on_the_archived_seat_and_a_hold_stamps_too() {
        let root = setup();
        verbs::cmd_send(&root, Some("alpha"), "beta", "stamp me", None).unwrap();
        let beta_dir = seat::seat_dir(&root, "beta");
        let alpha_dir = seat::seat_dir(&root, "alpha");
        let received = receive(&root, "beta").unwrap();
        assert_eq!(received.directs.len(), 1);
        // receive alone stamps nothing.
        assert!(seat::read_meta(&beta_dir).unwrap().last_drained.is_none());
        let staged = msg::list_msg_dir(&beta_dir.join("draining")).unwrap();
        assert_eq!(staged.len(), 1);

        done(&root, &[id_of(&staged[0])]).unwrap();
        assert!(seat::read_meta(&beta_dir).unwrap().last_drained.is_some());
        assert!(seat::read_meta(&alpha_dir).unwrap().last_drained.is_none());

        // A hold of an empty seat prints nothing and still stamps.
        let (r, text) = hold_str(&root, "alpha");
        r.unwrap();
        assert_eq!(text, "");
        assert!(seat::read_meta(&alpha_dir).unwrap().last_drained.is_some());
    }

    /// A redelivery is exactly the redelivery line plus the direct
    /// text: hold, expire the lease, hold again — byte for byte.
    #[test]
    fn an_expired_hold_reprints_under_the_redelivery_line() {
        let root = setup();
        verbs::cmd_send(&root, Some("alpha"), "beta", "expired body", None).unwrap();
        let dir = seat::seat_dir(&root, "beta");
        let (r, text) = hold_str(&root, "beta");
        r.unwrap();
        assert!(text.contains("expired body"));
        assert!(!text.contains("[redelivery]"), "fresh mail: {text}");

        let staged = msg::list_msg_dir(&dir.join("draining")).unwrap();
        assert_eq!(staged.len(), 1);
        let id = id_of(&staged[0]);
        set_mtime(&staged[0], SystemTime::now() - Duration::from_secs(1)).unwrap();

        let (r2, text2) = hold_str(&root, "beta");
        r2.unwrap();
        // The first hold's output was exactly the direct text (pinned
        // byte for byte by drain_prints_the_full_direct_text), so the
        // reprint is the line plus that, byte for byte.
        assert_eq!(
            text2,
            format!(
                "[redelivery] check whether you already did this; if so, only run: seat done {id}\n{text}"
            ),
            "the reprint is the line plus the direct text"
        );
    }

    /// A hold printing one fresh and one expired message marks only
    /// the expired one: exactly one redelivery line, naming its id.
    #[test]
    fn a_hold_marks_only_the_expired_message_redelivery() {
        let root = setup();
        verbs::cmd_send(&root, Some("alpha"), "beta", "old news", None).unwrap();
        let dir = seat::seat_dir(&root, "beta");
        let (r, _) = hold_str(&root, "beta");
        r.unwrap();
        let staged = msg::list_msg_dir(&dir.join("draining")).unwrap();
        assert_eq!(staged.len(), 1);
        let old_id = id_of(&staged[0]);
        set_mtime(&staged[0], SystemTime::now() - Duration::from_secs(1)).unwrap();

        verbs::cmd_send(&root, Some("alpha"), "beta", "fresh news", None).unwrap();
        let (r2, text) = hold_str(&root, "beta");
        r2.unwrap();
        assert!(text.contains("old news") && text.contains("fresh news"));
        let lines: Vec<&str> = text.lines().collect();
        let redeliveries: Vec<&&str> = lines
            .iter()
            .filter(|l| l.starts_with("[redelivery]"))
            .collect();
        assert_eq!(redeliveries.len(), 1, "one line only: {text}");
        assert_eq!(
            *redeliveries[0],
            format!("[redelivery] check whether you already did this; if so, only run: seat done {old_id}"),
            "the line names the expired message's id: {text}"
        );
    }
}
