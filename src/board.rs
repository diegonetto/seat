use crate::error::{Result, SeatError};
use std::path::{Path, PathBuf};

/// The literal a board this program wrote must carry. It means "has our
/// marker," not a proof of authorship.
pub const MARKER: &str = "seat";

/// The board root (FR-013): `--root`, else `SEAT_ROOT`, else the
/// documented default `$HOME/.local/share/seat`. `MEMO_ROOT` is not
/// read. A missing root is created only by `init`.
pub fn resolve_root(explicit: Option<&str>) -> PathBuf {
    if let Some(p) = explicit {
        return PathBuf::from(p);
    }
    match std::env::var_os("SEAT_ROOT") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => default_root(),
    }
}

/// Documented default board location (see README). This is for the human
/// operator's shell, not for tests.
pub fn default_root() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".local").join("share").join("seat")
}

pub fn seats_dir(root: &Path) -> PathBuf {
    root.join("seats")
}

pub fn rooms_dir(root: &Path) -> PathBuf {
    root.join("rooms")
}

pub fn marker_path(root: &Path) -> PathBuf {
    root.join("marker")
}

/// A root is ours when its `marker` holds the literal `seat`.
pub fn marker_ok(root: &Path) -> bool {
    std::fs::read_to_string(marker_path(root))
        .map(|raw| raw.trim() == MARKER)
        .unwrap_or(false)
}

/// Seat and room names become path components: alphanumeric plus
/// `-`, `_`, `.`, no leading dot, max 64 chars. Rejected names are the
/// named BadName error, never a path traversal.
pub fn valid_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
    if ok {
        Ok(())
    } else {
        Err(SeatError::BadName(name.to_string()))
    }
}

/// A root this program did not write (or does not have) is refused by
/// every command but `reset`, with the path in the error (FR-010).
pub fn ensure_board(root: &Path) -> Result<()> {
    if !root.exists() {
        return Err(SeatError::MissingBoard(root.to_path_buf()));
    }
    if marker_ok(root) {
        Ok(())
    } else {
        Err(SeatError::ForeignRoot(root.to_path_buf()))
    }
}

fn is_empty_dir(root: &Path) -> Result<bool> {
    Ok(std::fs::read_dir(root)?.next().is_none())
}

/// Create the board. A missing root is created here and nowhere else.
/// An empty directory is not refused; a non-empty root without our
/// marker is (init included). Idempotent when the marker is `seat`.
pub fn init(root: &Path) -> Result<()> {
    if root.exists() {
        if !root.is_dir() || (!is_empty_dir(root)? && !marker_ok(root)) {
            return Err(SeatError::ForeignRoot(root.to_path_buf()));
        }
    } else {
        std::fs::create_dir_all(root)?;
    }
    if !marker_ok(root) {
        std::fs::write(marker_path(root), MARKER)?;
    }
    std::fs::create_dir_all(seats_dir(root))?;
    std::fs::create_dir_all(rooms_dir(root))?;
    Ok(())
}

/// The burn switch (FR-010): remove `marker`, `seats/`, and `rooms/`
/// under the root. The root itself is deleted only when that leaves it
/// empty. Works on a root this program does not recognize.
pub fn reset(root: &Path) -> Result<()> {
    match std::fs::remove_file(marker_path(root)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(SeatError::Io(e)),
    }
    for dir in [seats_dir(root), rooms_dir(root)] {
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(SeatError::Io(e)),
        }
    }
    match std::fs::remove_dir(root) {
        Ok(()) => Ok(()),
        Err(e)
            if e.kind() == std::io::ErrorKind::NotFound
                || e.kind() == std::io::ErrorKind::DirectoryNotEmpty =>
        {
            Ok(())
        }
        Err(e) => Err(SeatError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_root;

    #[test]
    fn init_creates_marker_and_layout() {
        let root = temp_root("board-init"); // exists and is empty
        init(&root).unwrap();
        assert_eq!(
            std::fs::read_to_string(marker_path(&root)).unwrap().trim(),
            MARKER
        );
        assert!(root.join("seats").is_dir());
        assert!(root.join("rooms").is_dir());
        assert!(marker_ok(&root));
    }

    #[test]
    fn init_is_idempotent_on_a_marked_root() {
        let root = temp_root("board-idem");
        init(&root).unwrap();
        std::fs::create_dir_all(seats_dir(&root).join("alpha")).unwrap();
        init(&root).unwrap(); // non-empty, but ours
        assert!(seats_dir(&root).join("alpha").is_dir(), "init kept the seat");
    }

    #[test]
    fn an_empty_directory_is_not_refused() {
        let root = temp_root("board-empty");
        assert!(is_empty_dir(&root).unwrap());
        init(&root).unwrap();
    }

    #[test]
    fn init_refuses_a_nonempty_unmarked_root_and_names_it() {
        let root = temp_root("board-foreign");
        std::fs::write(root.join("junk"), "not ours").unwrap();
        let err = init(&root).unwrap_err();
        assert!(matches!(err, SeatError::ForeignRoot(_)));
        assert!(err.to_string().contains(&root.display().to_string()), "{err}");
        // A wrong marker is foreign too.
        let wrong = temp_root("board-wrong-marker");
        std::fs::write(marker_path(&wrong), "memo").unwrap();
        assert!(matches!(init(&wrong), Err(SeatError::ForeignRoot(_))));
    }

    #[test]
    fn ensure_board_refuses_missing_and_foreign_roots() {
        let missing = std::env::temp_dir().join("seat-test-no-such-root-9");
        let err = ensure_board(&missing).unwrap_err();
        assert!(matches!(err, SeatError::MissingBoard(_)));
        assert!(err.to_string().contains(&missing.display().to_string()));

        let foreign = temp_root("board-ensure-foreign");
        std::fs::write(foreign.join("stray"), "x").unwrap();
        let err = ensure_board(&foreign).unwrap_err();
        assert!(matches!(err, SeatError::ForeignRoot(_)));
        assert!(err.to_string().contains(&foreign.display().to_string()));

        let ours = temp_root("board-ensure-ok");
        init(&ours).unwrap();
        ensure_board(&ours).unwrap();
    }

    #[test]
    fn reset_removes_the_board_and_deletes_an_empty_root() {
        let root = temp_root("board-reset");
        init(&root).unwrap();
        std::fs::create_dir_all(seats_dir(&root).join("alpha")).unwrap();
        reset(&root).unwrap();
        assert!(!root.exists(), "root deleted once empty");
    }

    #[test]
    fn reset_on_a_foreign_root_clears_the_way_for_init() {
        let root = temp_root("board-reset-foreign");
        std::fs::create_dir_all(root.join("seats").join("ghost")).unwrap();
        reset(&root).unwrap();
        assert!(!root.join("seats").exists());
        init(&root).unwrap(); // reset then init succeeds (quickstart step 7)
    }

    #[test]
    fn reset_keeps_the_root_when_other_files_remain() {
        let root = temp_root("board-reset-keep");
        init(&root).unwrap();
        std::fs::write(root.join("keep-me"), "operator file").unwrap();
        reset(&root).unwrap();
        assert!(root.exists(), "root kept: not empty");
        assert!(!marker_path(&root).exists());
        assert!(!seats_dir(&root).exists());
        assert!(!rooms_dir(&root).exists());
        assert!(root.join("keep-me").is_file());
    }

    #[test]
    fn reset_on_a_missing_root_is_ok() {
        let root = std::env::temp_dir().join("seat-test-gone-root-9");
        reset(&root).unwrap();
    }

    #[test]
    fn valid_name_rejects_traversal_and_odd_chars() {
        assert!(valid_name("alpha").is_ok());
        assert!(valid_name("room.v2_b").is_ok());
        for bad in [
            "",
            ".",
            "..",
            ".hidden",
            "a/b",
            "a b",
            "a\nb",
            &"x".repeat(65),
        ] {
            assert!(valid_name(bad).is_err(), "{bad:?} must be rejected");
            assert!(matches!(valid_name(bad), Err(SeatError::BadName(_))));
        }
    }

    #[test]
    fn resolve_root_prefers_explicit_then_env_over_default() {
        // One test touches the root environment; every other test
        // passes roots in. MEMO_ROOT must not be honored anymore.
        let thrown = temp_root("board-env");
        std::env::set_var("MEMO_ROOT", &thrown);
        std::env::remove_var("SEAT_ROOT");
        assert_eq!(resolve_root(None), default_root());
        assert_ne!(resolve_root(None), thrown);

        std::env::set_var("SEAT_ROOT", &thrown);
        assert_eq!(resolve_root(None), thrown);
        assert_eq!(resolve_root(Some("/elsewhere")), PathBuf::from("/elsewhere"));
        std::env::remove_var("SEAT_ROOT");
        std::env::remove_var("MEMO_ROOT");
    }
}
