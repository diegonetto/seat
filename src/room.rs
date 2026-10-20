use crate::board::{self, valid_name};
use crate::error::{Result, SeatError};
use crate::msg;
use crate::seat;
use crate::verbs::touch_last_seen;
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn room_dir(root: &Path, name: &str) -> PathBuf {
    root.join("rooms").join(name)
}

pub fn exists(root: &Path, name: &str) -> bool {
    room_dir(root, name).is_dir()
}

/// Create `rooms/<name>/`. A duplicate room is the named RoomExists
/// error, not a silent re-init.
pub fn create(root: &Path, name: &str) -> Result<()> {
    board::ensure_board(root)?;
    valid_name(name)?;
    let dir = room_dir(root, name);
    if dir.exists() {
        return Err(SeatError::RoomExists(name.to_string()));
    }
    std::fs::create_dir_all(&dir)?;
    Ok(())
}

/// All posts in a room, oldest first.
pub fn posts(root: &Path, name: &str) -> Result<Vec<PathBuf>> {
    msg::list_msg_dir(&room_dir(root, name))
}

/// Posts strictly newer than `cursor` (lexicographic ts comparison —
/// the filename prefix is fixed-width, so this is chronological).
pub fn posts_since(root: &Path, name: &str, cursor: &str) -> Result<Vec<PathBuf>> {
    Ok(posts(root, name)?
        .into_iter()
        .filter(|p| msg::ts_prefix(p).as_deref().is_some_and(|ts| ts > cursor))
        .collect())
}

pub fn cursor_path(seat_dir: &Path, room: &str) -> PathBuf {
    seat_dir.join("cursors").join(room)
}

/// The rooms this seat follows (its cursor file names).
pub fn followed_rooms(seat_dir: &Path) -> Vec<String> {
    std::fs::read_dir(seat_dir.join("cursors"))
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_file())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The seat's last-seen ts for a room. No cursor file yet → "0", i.e.
/// everything is new. (Follow starts at "0" so the first drain delivers
/// the room's existing posts; after that the cursor advances.)
pub fn read_cursor(seat_dir: &Path, room: &str) -> Result<String> {
    let raw = std::fs::read_to_string(cursor_path(seat_dir, room)).unwrap_or_default();
    let trimmed = raw.trim().to_string();
    if trimmed.is_empty() {
        Ok("0".to_string())
    } else {
        Ok(trimmed)
    }
}

pub fn write_cursor(seat_dir: &Path, room: &str, ts: &str) -> Result<()> {
    std::fs::write(cursor_path(seat_dir, room), ts)?;
    Ok(())
}

pub fn cmd_create(root: &Path, name: &str) -> Result<()> {
    create(root, name)?;
    println!("created room '{name}'");
    Ok(())
}

/// Post into a room: same `.msg` format as direct mail, `dest` is the
/// room name, MAC keyed by the posting seat's token. The poster is
/// fixed by the identity rule (`--from` optional for bound sessions;
/// an unbound caller must give it — no implicit Diego in rooms, since
/// a room's followers include the Grok bots). `env_session` is the
/// caller's `HAPI_SESSION_ID`, read at the CLI edge. This is the
/// library half of the verb: the identity and token checks, the write,
/// the last-seen touch — and no printing, so service callers (the
/// dispatcher's alarm) post without touching stdout and pass no
/// `HAPI_SESSION_ID` into the identity rule.
pub fn post(
    root: &Path,
    room: &str,
    from: Option<&str>,
    body: &str,
    env_session: Option<&str>,
) -> Result<String> {
    board::ensure_board(root)?;
    if !exists(root, room) {
        return Err(SeatError::NoSuchRoom(room.to_string()));
    }
    let who = crate::identity::resolve_for_room(root, from, env_session)?;
    let from = who.from.as_str();
    let token = crate::verbs::sender_token(root, from)?;
    let (_, header) = msg::write_msg(
        &room_dir(root, room),
        room,
        from,
        &token,
        body,
        who.session.as_deref(),
    )?;
    touch_last_seen(root, from)?;
    Ok(header.id)
}

/// The verb: the library post plus the one printed line.
pub fn cmd_post(
    root: &Path,
    room: &str,
    from: Option<&str>,
    body: &str,
    env_session: Option<&str>,
) -> Result<()> {
    let id = post(root, room, from, body, env_session)?;
    println!("posted {id} to room '{room}'");
    Ok(())
}

pub fn cmd_read(root: &Path, room: &str, out: &mut dyn Write) -> Result<()> {
    board::ensure_board(root)?;
    if !exists(root, room) {
        return Err(SeatError::NoSuchRoom(room.to_string()));
    }
    let all = posts(root, room)?;
    if all.is_empty() {
        writeln!(out, "(room '{room}' has no posts)")?;
        return Ok(());
    }
    for p in all {
        let (header, body) = msg::read_msg(&p)?;
        writeln!(
            out,
            "[{room}] id={} from={} ts={}",
            header.id, header.from, header.ts
        )?;
        writeln!(out, "{body}")?;
    }
    Ok(())
}

/// Follow = a cursor file on the seat. Re-following an already-followed
/// room is a no-op (the cursor — and its history — is preserved).
pub fn cmd_follow(root: &Path, room: &str, seat_name: &str) -> Result<()> {
    board::ensure_board(root)?;
    if !exists(root, room) {
        return Err(SeatError::NoSuchRoom(room.to_string()));
    }
    let dir = seat::seat_dir(root, seat_name);
    seat::read_meta(&dir)?; // unknown or corrupt seat is a named error
    std::fs::create_dir_all(dir.join("cursors"))?;
    let cursor = cursor_path(&dir, room);
    if cursor.exists() {
        println!("seat '{seat_name}' already follows room '{room}'");
        return Ok(());
    }
    write_cursor(&dir, room, "0")?;
    println!("seat '{seat_name}' now follows room '{room}'");
    Ok(())
}

pub fn cmd_list(root: &Path, out: &mut dyn Write) -> Result<()> {
    board::ensure_board(root)?;
    let mut names: Vec<String> = std::fs::read_dir(board::rooms_dir(root))?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    if names.is_empty() {
        writeln!(out, "(no rooms)")?;
        return Ok(());
    }
    for name in names {
        let n = posts(root, &name)?.len();
        writeln!(out, "{name}\t{n} posts")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_root;

    fn setup() -> std::path::PathBuf {
        let root = temp_root("room");
        board::init(&root).unwrap();
        crate::verbs::cmd_register(&root, "alpha", "sb", None, None, Vec::new()).unwrap();
        root
    }

    #[test]
    fn create_is_exclusive_and_listed() {
        let root = setup();
        create(&root, "ops").unwrap();
        assert!(exists(&root, "ops"));
        assert!(matches!(
            create(&root, "ops"),
            Err(SeatError::RoomExists(_))
        ));
        let mut out = Vec::new();
        cmd_list(&root, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("ops"));
    }

    #[test]
    fn post_read_and_cursor_filtering() {
        let root = setup();
        create(&root, "ops").unwrap();
        cmd_post(&root, "ops", Some("alpha"), "first bulletin", None).unwrap();
        cmd_post(&root, "ops", Some("alpha"), "second bulletin", None).unwrap();

        let mut out = Vec::new();
        cmd_read(&root, "ops", &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("first bulletin"));
        assert!(text.contains("second bulletin"));

        // Cursor filtering: everything after "0", then nothing after the max.
        let all = posts_since(&root, "ops", "0").unwrap();
        assert_eq!(all.len(), 2);
        let max = msg::ts_prefix(all.last().unwrap()).unwrap();
        assert!(posts_since(&root, "ops", &max).unwrap().is_empty());
        assert_eq!(posts_since(&root, "ops", "9999").unwrap().len(), 0);
    }

    #[test]
    fn follow_writes_cursor_and_preserves_it() {
        let root = setup();
        create(&root, "ops").unwrap();
        let dir = seat::seat_dir(&root, "alpha");
        cmd_follow(&root, "ops", "alpha").unwrap();
        assert_eq!(read_cursor(&dir, "ops").unwrap(), "0");
        write_cursor(&dir, "ops", "20260923T070000.000000").unwrap();
        cmd_follow(&root, "ops", "alpha").unwrap(); // no-op
        assert_eq!(read_cursor(&dir, "ops").unwrap(), "20260923T070000.000000");
        // Unknown room / unknown seat refuse.
        assert!(matches!(
            cmd_follow(&root, "nope", "alpha"),
            Err(SeatError::NoSuchRoom(_))
        ));
        assert!(cmd_follow(&root, "ops", "ghost").is_err());
    }

    fn identity_setup() -> std::path::PathBuf {
        let root = temp_root("room-identity");
        board::init(&root).unwrap();
        crate::verbs::cmd_register(&root, "alpha", "sb", None, None, Vec::new()).unwrap();
        crate::verbs::cmd_register(&root, "opus-main", "sb", None, None, Vec::new()).unwrap();
        crate::testutil::write_bindings(&root, &[("opus-main", "sid-opus")]);
        create(&root, "ops").unwrap();
        root
    }

    #[test]
    fn bound_session_posts_as_itself() {
        let root = identity_setup();
        cmd_post(&root, "ops", None, "bound bulletin", Some("sid-opus")).unwrap();
        cmd_post(
            &root,
            "ops",
            Some("opus-main"),
            "explicit match",
            Some("sid-opus"),
        )
        .unwrap();
        for p in posts(&root, "ops").unwrap() {
            let (header, _) = msg::read_msg(&p).unwrap();
            assert_eq!(header.from, "opus-main");
            assert_eq!(
                header.session, None,
                "bound senders write no session header"
            );
        }
    }

    #[test]
    fn bound_session_posting_as_another_seat_is_refused() {
        let root = identity_setup();
        let err = cmd_post(&root, "ops", Some("alpha"), "nope", Some("sid-opus")).unwrap_err();
        assert!(
            err.to_string().contains("bound to opus-main")
                && err.to_string().contains("cannot send as alpha"),
            "{err}"
        );
        assert!(matches!(err, SeatError::Identity(_)));
        assert!(posts(&root, "ops").unwrap().is_empty());
    }

    #[test]
    fn unbound_explicit_from_is_unchanged_and_carries_session() {
        let root = identity_setup();
        cmd_post(&root, "ops", Some("alpha"), "plain post", None).unwrap();
        cmd_post(
            &root,
            "ops",
            Some("alpha"),
            "with session",
            Some("sid-adhoc"),
        )
        .unwrap();
        let all = posts(&root, "ops").unwrap();
        assert_eq!(all.len(), 2);
        let (h1, _) = msg::read_msg(&all[0]).unwrap();
        assert_eq!(h1.from, "alpha");
        assert_eq!(h1.session, None);
        let (h2, _) = msg::read_msg(&all[1]).unwrap();
        assert_eq!(h2.from, "alpha");
        assert_eq!(h2.session.as_deref(), Some("sid-adhoc"));
    }

    /// No implicit Diego in rooms: an unbound caller must give --from.
    #[test]
    fn unbound_omitted_from_is_refused() {
        let root = identity_setup();
        let err = cmd_post(&root, "ops", None, "who am i", None).unwrap_err();
        assert!(matches!(err, SeatError::Identity(_)), "{err}");
        assert!(err.to_string().contains("--from"), "{err}");
        let err = cmd_post(&root, "ops", None, "who am i", Some("sid-adhoc")).unwrap_err();
        assert!(matches!(err, SeatError::Identity(_)), "{err}");
        assert!(posts(&root, "ops").unwrap().is_empty());
    }
}
