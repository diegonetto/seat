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
/// room name, MAC keyed by the posting seat's token.
pub fn cmd_post(root: &Path, room: &str, from: &str, body: &str) -> Result<()> {
    board::ensure_board(root)?;
    if !exists(root, room) {
        return Err(SeatError::NoSuchRoom(room.to_string()));
    }
    let token = crate::verbs::sender_token(root, from)?;
    let (_, header) = msg::write_msg(&room_dir(root, room), room, from, &token, body)?;
    touch_last_seen(root, from)?;
    println!("posted {} to room '{room}'", header.id);
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
        crate::verbs::cmd_register(&root, "alpha", "sb", None, None, None, Vec::new()).unwrap();
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
        cmd_post(&root, "ops", "alpha", "first bulletin").unwrap();
        cmd_post(&root, "ops", "alpha", "second bulletin").unwrap();

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
}
