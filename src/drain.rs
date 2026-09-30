use crate::board::{self};
use crate::error::{Result, SeatError};
use crate::msg;
use crate::room;
use crate::seat;
use std::io::Write;
use std::path::{Path, PathBuf};

/// What one drain delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Outcome {
    pub direct: usize,
    pub room: usize,
}

impl Outcome {
    #[cfg_attr(not(test), allow(dead_code))] // kept for callers and tests
    pub fn total(&self) -> usize {
        self.direct + self.room
    }
}

/// The digest verb. Crash-safe three stages: inbox → draining →
/// archive. An empty inbox and no unread followed-room posts prints
/// nothing and exits 0 (T012).
///
/// A message whose mark does not verify is never printed: it is
/// quarantined, the claimed `from` is named on stderr, and the command
/// exits nonzero (T013, FR-012).
pub fn drain(root: &Path, seat_name: &str, out: &mut dyn Write) -> Result<Outcome> {
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

    let mut outcome = Outcome::default();
    let mut bad_marks: Vec<String> = Vec::new();
    let mut unreadable = false;
    for p in staged {
        match deliver_direct(root, seat_name, &p, out)? {
            Direct::Delivered => outcome.direct += 1,
            Direct::Unreadable => {
                quarantine(root, seat_name, &p)?;
                unreadable = true;
            }
            Direct::BadMark(from) => {
                quarantine(root, seat_name, &p)?;
                bad_marks.push(from);
            }
        }
    }

    outcome.room = deliver_rooms(root, &dir, out, &mut bad_marks, &mut unreadable)?;

    {
        let mut meta = seat::read_meta(&dir)?;
        meta.last_drained = Some(seat::now_ts());
        seat::write_meta(&dir, &meta)?;
    }

    if !bad_marks.is_empty() {
        // The error names the first sender. Name any further ones here so
        // the process prints each claimed from once.
        for from in &bad_marks[1..] {
            eprintln!("drain: message from '{from}' failed mark verification; not delivered");
        }
        return Err(SeatError::BadMark(bad_marks[0].clone()));
    }
    if unreadable {
        return Err(SeatError::Unreadable);
    }
    Ok(outcome)
}

enum Direct {
    Delivered,
    /// The header did not parse, so there is no claimed sender to name.
    Unreadable,
    /// Mark (or envelope) does not verify; the claimed sender.
    BadMark(String),
}

/// Verify and print one direct message. Delivered messages are printed
/// and archived; everything else comes back named.
fn deliver_direct(
    root: &Path,
    seat_name: &str,
    path: &Path,
    out: &mut dyn Write,
) -> Result<Direct> {
    let (header, body) = match msg::read_msg(path) {
        Ok(x) => x,
        Err(_) => {
            // A body byte that is not UTF-8 fails before the mark is
            // checked. The header still names who claimed to send it.
            return Ok(match claimed_from(path) {
                Some(from) => Direct::BadMark(from),
                None => Direct::Unreadable,
            });
        }
    };
    // header.from is untrusted input that becomes a path component
    // (seats/<from>/token): reject anything that is not a valid name
    // before touching the filesystem with it.
    if board::valid_name(&header.from).is_err() {
        return Ok(Direct::BadMark(header.from.clone()));
    }
    let verified = match crate::verbs::sender_token(root, &header.from) {
        Ok(token) => msg::verify_msg(path, &token).unwrap_or(false),
        Err(_) => false,
    };
    if header.dest != seat_name || !verified {
        return Ok(Direct::BadMark(header.from.clone()));
    }
    writeln!(
        out,
        "[direct] id={} from={} dest={} ts={}",
        header.id, header.from, header.dest, header.ts
    )?;
    writeln!(out, "{body}")?;
    writeln!(out)?;
    let archive = seat::seat_dir(root, seat_name)
        .join("archive")
        .join(path.file_name().unwrap_or_default());
    std::fs::rename(path, archive)?;
    Ok(Direct::Delivered)
}

/// Deliver new posts from every followed room, then advance cursors
/// (a bad post must not wedge a seat on every drain). A post whose mark
/// does not verify is not printed; its claimed sender is named and the
/// command exits nonzero.
fn deliver_rooms(
    root: &Path,
    seat_dir: &Path,
    out: &mut dyn Write,
    bad_marks: &mut Vec<String>,
    unreadable: &mut bool,
) -> Result<usize> {
    let mut delivered = 0usize;
    for cursor_file in sorted_cursor_files(seat_dir)? {
        let room_name = match cursor_file.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let cursor = room::read_cursor(seat_dir, &room_name)?;
        let posts = room::posts_since(root, &room_name, &cursor)?;
        let mut max_ts: Option<String> = None;
        for p in posts {
            if let Some(ts) = msg::ts_prefix(&p) {
                max_ts = Some(match max_ts.take() {
                    Some(prev) if prev >= ts => prev,
                    _ => ts,
                });
            }
            let (header, body) = match msg::read_msg(&p) {
                Ok(x) => x,
                Err(_) => {
                    match claimed_from(&p) {
                        Some(from) => bad_marks.push(from),
                        None => *unreadable = true,
                    }
                    continue;
                }
            };
            // A post must belong to the room it sits in, and its from
            // must be a valid name before it becomes a token path.
            if header.dest != room_name || board::valid_name(&header.from).is_err() {
                bad_marks.push(header.from.clone());
                continue;
            }
            let verified = match crate::verbs::sender_token(root, &header.from) {
                Ok(token) => msg::verify_msg(&p, &token).unwrap_or(false),
                Err(_) => false,
            };
            if !verified {
                bad_marks.push(header.from.clone());
                continue;
            }
            writeln!(
                out,
                "[room:{room_name}] id={} from={} ts={}",
                header.id, header.from, header.ts
            )?;
            writeln!(out, "{body}")?;
            writeln!(out)?;
            delivered += 1;
        }
        if let Some(ts) = max_ts {
            room::write_cursor(seat_dir, &room_name, &ts)?;
        }
    }
    Ok(delivered)
}

fn sorted_cursor_files(seat_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = match std::fs::read_dir(seat_dir.join("cursors")) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(SeatError::Io(e)),
    };
    files.sort();
    Ok(files)
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
        verbs::cmd_register(&root, "alpha", "sb", None, None, None, Vec::new()).unwrap();
        verbs::cmd_register(&root, "beta", "sb", None, None, None, Vec::new()).unwrap();
        root
    }

    fn drain_str(root: &std::path::Path, seat: &str) -> (Result<Outcome>, String) {
        let mut buf = Vec::new();
        let r = drain(root, seat, &mut buf);
        (r, String::from_utf8(buf).unwrap())
    }

    #[test]
    fn drain_delivers_archives_and_empties() {
        let root = setup();
        verbs::cmd_send(&root, "alpha", "beta", "hello board").unwrap();

        let inbox = seat::seat_dir(&root, "beta").join("inbox");
        assert_eq!(msg::list_msg_dir(&inbox).unwrap().len(), 1);

        let (r, text) = drain_str(&root, "beta");
        assert_eq!(r.unwrap(), Outcome { direct: 1, room: 0 });
        assert!(text.contains("hello board"));
        assert!(text.contains("[direct]"));
        assert!(msg::list_msg_dir(&inbox).unwrap().is_empty());
        assert_eq!(
            msg::list_msg_dir(&seat::seat_dir(&root, "beta").join("archive"))
                .unwrap()
                .len(),
            1
        );

        // T012: a second, empty drain prints NOTHING and exits 0.
        let (r2, text2) = drain_str(&root, "beta");
        assert!(r2.unwrap().total() == 0);
        assert_eq!(text2, "", "empty drain prints nothing");
        let meta = seat::read_meta(&seat::seat_dir(&root, "beta")).unwrap();
        assert!(meta.last_drained.is_some());
    }

    #[test]
    fn room_posts_delivered_and_cursor_advances() {
        let root = setup();
        room::create(&root, "ops").unwrap();
        room::cmd_post(&root, "ops", "alpha", "room bulletin").unwrap();
        room::cmd_follow(&root, "ops", "beta").unwrap();

        let (r, text) = drain_str(&root, "beta");
        assert_eq!(r.unwrap(), Outcome { direct: 0, room: 1 });
        assert!(text.contains("room bulletin"));

        // Cursor advanced past the post: second drain sees nothing.
        let (r2, text2) = drain_str(&root, "beta");
        assert_eq!(r2.unwrap().total(), 0);
        assert_eq!(text2, "");

        // New posts arrive, old ones do not repeat.
        room::cmd_post(&root, "ops", "alpha", "second bulletin").unwrap();
        let (r3, text3) = drain_str(&root, "beta");
        assert_eq!(r3.unwrap().room, 1);
        assert!(text3.contains("second bulletin"));
        assert!(!text3.contains("room bulletin"));
    }

    #[test]
    fn followed_room_post_prints_even_when_inbox_is_empty() {
        let root = setup();
        room::create(&root, "ops").unwrap();
        room::cmd_post(&root, "ops", "alpha", "empty inbox bulletin").unwrap();
        room::cmd_follow(&root, "ops", "beta").unwrap();

        let (r, text) = drain_str(&root, "beta");
        assert_eq!(r.unwrap(), Outcome { direct: 0, room: 1 });
        assert!(text.contains("empty inbox bulletin"));
    }

    #[test]
    fn tampered_message_names_from_and_exits_nonzero() {
        let root = setup();
        verbs::cmd_send(&root, "alpha", "beta", "original body").unwrap();
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
        assert!(matches!(err, SeatError::BadMark(ref f) if f == "alpha"), "{err}");
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
        verbs::cmd_send(&root, "alpha", "beta", "byte flip").unwrap();
        let inbox = seat::seat_dir(&root, "beta").join("inbox");
        let p = msg::list_msg_dir(&inbox).unwrap().into_iter().next().unwrap();
        let mut raw = std::fs::read(&p).unwrap();
        let i = raw.windows(9).position(|w| w == b"byte flip").unwrap();
        raw[i] = 0xff;
        std::fs::write(&p, &raw).unwrap();

        let (r, text) = drain_str(&root, "beta");
        let err = r.unwrap_err();
        assert!(matches!(err, SeatError::BadMark(ref f) if f == "alpha"), "{err}");
        assert!(err.to_string().contains("from 'alpha'"), "{err}");
        assert!(!text.contains("byte"), "no body printed: {text}");
        assert!(msg::list_msg_dir(&inbox).unwrap().is_empty());
    }

    #[test]
    fn drain_ignores_tmp_scratch_files_in_inbox() {
        let root = setup();
        verbs::cmd_send(&root, "alpha", "beta", "real mail").unwrap();
        let inbox = seat::seat_dir(&root, "beta").join("inbox");
        // A concurrent writer's in-flight scratch file: same directory,
        // dot-prefixed, NOT a *.msg name.
        let tmp = inbox.join(".20260923T000000.000000-deadbeef.msg.tmp");
        std::fs::write(&tmp, "half-\n\nwritten").unwrap();

        let (r, text) = drain_str(&root, "beta");
        assert_eq!(r.unwrap(), Outcome { direct: 1, room: 0 });
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
        let (path, _) = msg::write_msg(&root, "beta", "../evil", &token, "forged body").unwrap();
        let inbox = seat::seat_dir(&root, "beta").join("inbox");
        std::fs::rename(&path, inbox.join(path.file_name().unwrap())).unwrap();

        let (r, text) = drain_str(&root, "beta");
        let err = r.unwrap_err();
        assert!(matches!(err, SeatError::BadMark(_)), "{err}");
        assert!(!text.contains("forged body"));
    }

    #[test]
    fn room_post_with_wrong_dest_is_a_bad_mark() {
        let root = setup();
        room::create(&root, "ops").unwrap();
        let token = verbs::sender_token(&root, "alpha").unwrap();
        // Valid MAC from a real seat, but dest is not the room it sits in.
        msg::write_msg(
            &room::room_dir(&root, "ops"),
            "not-ops",
            "alpha",
            &token,
            "misfiled",
        )
        .unwrap();
        room::cmd_follow(&root, "ops", "beta").unwrap();

        let (r, text) = drain_str(&root, "beta");
        let err = r.unwrap_err();
        assert!(matches!(err, SeatError::BadMark(ref f) if f == "alpha"), "{err}");
        assert!(!text.contains("misfiled"));
        // Cursor advanced past it: a bad post does not wedge the seat.
        let (r2, text2) = drain_str(&root, "beta");
        assert!(r2.is_ok());
        assert_eq!(text2, "");
    }

    #[test]
    fn tampered_room_post_names_from_and_exits_nonzero() {
        let root = setup();
        room::create(&root, "ops").unwrap();
        room::cmd_post(&root, "ops", "alpha", "room original").unwrap();
        room::cmd_follow(&root, "ops", "beta").unwrap();
        let p = room::posts(&root, "ops").unwrap().into_iter().next().unwrap();
        let raw = std::fs::read_to_string(&p).unwrap();
        std::fs::write(&p, raw.replace("room original", "room tampered")).unwrap();

        let (r, text) = drain_str(&root, "beta");
        let err = r.unwrap_err();
        assert!(matches!(err, SeatError::BadMark(ref f) if f == "alpha"), "{err}");
        assert!(!text.contains("room tampered"), "{text}");
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
}
