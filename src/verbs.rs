use crate::board::{self, valid_name};
use crate::drain;
use crate::error::{Result, SeatError};
use crate::msg;
use crate::room;
use crate::seat::{self, Lifecycle};
use std::io::Write;
use std::path::Path;

pub fn cmd_init(root: &Path) -> Result<()> {
    board::init(root)?;
    println!("initialized seat board at {}", root.display());
    Ok(())
}

/// Register a seat: layout + one-time token mint (O_EXCL) + meta with
/// the durable lifecycle (exit-wake is the default). A duplicate
/// register keeps that seat's token, inbox, and meta — only launch
/// fields actually passed change, and a refused register (a relative
/// or empty `--cwd`) writes nothing (T004, T009).
pub fn cmd_register(
    root: &Path,
    name: &str,
    harness: &str,
    model: Option<String>,
    lifecycle: Option<Lifecycle>,
    cwd: Option<String>,
    cmd: Vec<String>,
) -> Result<()> {
    board::ensure_board(root)?;
    valid_name(name)?;
    // Record-time check (research: Directory check): the path must
    // already be absolute. Refused here, before anything is written.
    if let Some(c) = cwd.as_deref() {
        if c.is_empty() || !Path::new(c).is_absolute() {
            return Err(SeatError::BadCwd(c.to_string()));
        }
    }
    let dir = seat::seat_dir(root, name);
    if seat::meta_path(&dir).exists() || seat::token_path(&dir).exists() {
        // Zero words after `--` means the command was omitted, not
        // cleared (contract: register). Nothing passed changes nothing.
        if cwd.is_none() && cmd.is_empty() {
            println!("seat '{name}' already registered (token and mail kept)");
            return Ok(());
        }
        let mut meta = seat::read_meta(&dir)?;
        if let Some(c) = cwd {
            meta.cwd = Some(c);
        }
        if !cmd.is_empty() {
            meta.cmd = Some(cmd);
        }
        seat::write_meta(&dir, &meta)?;
        println!("seat '{name}' already registered (token and mail kept)");
        println!("seat '{name}' launch updated");
        return Ok(());
    }
    seat::create(root, name)?;
    msg::mint_token(&seat::token_path(&dir))?;
    let lifecycle = lifecycle.unwrap_or(Lifecycle::ExitWake);
    let mut meta = seat::Meta {
        harness: harness.to_string(),
        model,
        lifecycle: Some(lifecycle),
        last_seen: Some(seat::now_ts()),
        ..Default::default()
    };
    if let Some(c) = cwd {
        meta.cwd = Some(c);
    }
    if !cmd.is_empty() {
        meta.cmd = Some(cmd);
    }
    seat::write_meta(&dir, &meta)?;
    println!(
        "registered seat '{name}' (harness={harness}, lifecycle={})",
        lifecycle.as_str()
    );
    println!("token: {}", seat::token_path(&dir).display());
    Ok(())
}

/// Read a seat's token; a missing token is the named GhostSeat error.
pub fn sender_token(root: &Path, seat_name: &str) -> Result<String> {
    msg::read_token(&seat::token_path(&seat::seat_dir(root, seat_name))).map_err(|e| match e {
        SeatError::Io(io) if io.kind() == std::io::ErrorKind::NotFound => {
            SeatError::GhostSeat(seat_name.to_string())
        }
        other => other,
    })
}

/// Send a direct message: MAC keyed by the *sender's* token, file lands
/// in the destination seat's inbox.
pub fn cmd_send(root: &Path, from: &str, to: &str, body: &str) -> Result<()> {
    board::ensure_board(root)?;
    valid_name(from)?;
    valid_name(to)?;
    seat::read_meta(&seat::seat_dir(root, to)).map_err(|e| match e {
        SeatError::GhostSeat(_) => SeatError::UnknownSeat(to.to_string()),
        other => other,
    })?;
    let token = sender_token(root, from)?;
    let (_, header) = msg::write_msg(
        &seat::seat_dir(root, to).join("inbox"),
        to,
        from,
        &token,
        body,
    )?;
    touch_last_seen(root, from)?;
    println!("sent {} from {from} to {to}", header.id);
    Ok(())
}

/// Bump `meta.last_seen` for a seat that just acted on the board.
pub fn touch_last_seen(root: &Path, seat_name: &str) -> Result<()> {
    let dir = seat::seat_dir(root, seat_name);
    let mut meta = seat::read_meta(&dir)?;
    meta.last_seen = Some(seat::now_ts());
    seat::write_meta(&dir, &meta)
}

/// The burn switch: remove the board under the root (FR-010).
pub fn cmd_reset(root: &Path) -> Result<()> {
    board::reset(root)?;
    if root.exists() {
        println!("reset {} (root kept: not empty)", root.display());
    } else {
        println!("reset {} (root removed)", root.display());
    }
    Ok(())
}

pub fn sorted_seat_names(root: &Path) -> Result<Vec<String>> {
    let mut names: Vec<String> = std::fs::read_dir(board::seats_dir(root))?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    Ok(names)
}

/// Inbox messages plus not-yet-drained posts from followed rooms.
pub(crate) fn unread_count(root: &Path, seat_dir: &Path) -> Result<usize> {
    let mut n = msg::list_msg_dir(&seat_dir.join("inbox"))?.len();
    for cursor in std::fs::read_dir(seat_dir.join("cursors"))
        .map(|it| {
            it.filter_map(|e| e.ok())
                .map(|e| e.path())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
    {
        if let Some(room_name) = cursor.file_name().and_then(|n| n.to_str()) {
            let c = room::read_cursor(seat_dir, room_name)?;
            n += room::posts_since(root, room_name, &c)?.len();
        }
    }
    Ok(n)
}

/// Expose drain for the CLI dispatch (kept here so main stays thin).
pub fn cmd_drain(root: &Path, seat_name: &str, out: &mut dyn Write) -> Result<()> {
    drain::drain(root, seat_name, out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_root;

    fn setup() -> std::path::PathBuf {
        let root = temp_root("verbs");
        board::init(&root).unwrap();
        root
    }

    #[test]
    fn register_persists_default_lifecycle() {
        let root = setup();
        cmd_register(&root, "alpha", "sb", Some("glm".into()), None, None, Vec::new()).unwrap();
        let dir = seat::seat_dir(&root, "alpha");
        let meta = seat::read_meta(&dir).unwrap();
        assert_eq!(meta.harness, "sb");
        assert_eq!(meta.model.as_deref(), Some("glm"));
        assert_eq!(meta.lifecycle, Some(Lifecycle::ExitWake), "default lifecycle");
        assert!(meta.last_seen.is_some());
        assert!(seat::token_path(&dir).is_file());
    }

    #[test]
    fn duplicate_register_keeps_token_and_inbox() {
        let root = setup();
        cmd_register(
            &root,
            "alpha",
            "sb",
            None,
            Some(Lifecycle::Poller),
            None,
            Vec::new(),
        )
        .unwrap();
        cmd_register(&root, "beta", "sb", None, None, None, Vec::new()).unwrap();
        cmd_send(&root, "beta", "alpha", "keep my mail").unwrap();
        let dir = seat::seat_dir(&root, "alpha");
        let token_before = std::fs::read_to_string(seat::token_path(&dir)).unwrap();
        let inbox_before = msg::list_msg_dir(&dir.join("inbox")).unwrap();

        // Duplicate register succeeds and touches nothing.
        cmd_register(&root, "alpha", "other-harness", None, None, None, Vec::new()).unwrap();

        assert_eq!(
            std::fs::read_to_string(seat::token_path(&dir)).unwrap(),
            token_before,
            "token kept"
        );
        assert_eq!(
            msg::list_msg_dir(&dir.join("inbox")).unwrap(),
            inbox_before,
            "inbox kept"
        );
        let meta = seat::read_meta(&dir).unwrap();
        assert_eq!(meta.harness, "sb", "meta kept");
        assert_eq!(meta.lifecycle, Some(Lifecycle::Poller));
    }

    #[test]
    fn register_refuses_bad_names() {
        let root = setup();
        assert!(matches!(
            cmd_register(&root, "../evil", "sb", None, None, None, Vec::new()),
            Err(SeatError::BadName(_))
        ));
        assert!(matches!(
            cmd_register(&root, "a b", "sb", None, None, None, Vec::new()),
            Err(SeatError::BadName(_))
        ));
    }

    #[test]
    fn send_requires_both_seats_and_updates_last_seen() {
        let root = setup();
        cmd_register(&root, "alpha", "sb", None, None, None, Vec::new()).unwrap();
        cmd_register(&root, "beta", "sb", None, None, None, Vec::new()).unwrap();

        assert!(matches!(
            cmd_send(&root, "ghost", "beta", "hi"),
            Err(SeatError::GhostSeat(_))
        ));
        assert!(matches!(
            cmd_send(&root, "alpha", "ghost", "hi"),
            Err(SeatError::UnknownSeat(_))
        ));

        cmd_send(&root, "alpha", "beta", "hello verbs").unwrap();
        let inbox = seat::seat_dir(&root, "beta").join("inbox");
        assert_eq!(msg::list_msg_dir(&inbox).unwrap().len(), 1);
        let meta = seat::read_meta(&seat::seat_dir(&root, "alpha")).unwrap();
        assert!(meta.last_seen.is_some());
    }

    #[test]
    fn reset_removes_the_board() {
        let root = setup();
        cmd_register(&root, "alpha", "sb", None, None, None, Vec::new()).unwrap();
        cmd_reset(&root).unwrap();
        assert!(!root.exists());
    }

    #[test]
    fn register_stores_launch_fields_new_seat() {
        let root = setup();
        let cwd = root.display().to_string();
        cmd_register(
            &root,
            "runner",
            "sb",
            None,
            None,
            Some(cwd.clone()),
            vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
        )
        .unwrap();
        let meta = seat::read_meta(&seat::seat_dir(&root, "runner")).unwrap();
        assert_eq!(meta.cwd.as_deref(), Some(cwd.as_str()));
        assert_eq!(
            meta.cmd.as_deref(),
            Some(&["/bin/sh".to_string(), "-c".to_string(), "sleep 30".to_string()][..])
        );
        // A mail-only seat stores neither field.
        cmd_register(&root, "mailbox", "sb", None, None, None, Vec::new()).unwrap();
        let meta = seat::read_meta(&seat::seat_dir(&root, "mailbox")).unwrap();
        assert_eq!(meta.cwd, None);
        assert_eq!(meta.cmd, None);
        let raw = std::fs::read_to_string(seat::meta_path(&seat::seat_dir(&root, "mailbox")))
            .unwrap();
        assert!(!raw.contains("\"cwd\""), "{raw}");
        assert!(!raw.contains("\"cmd\""), "{raw}");
    }

    #[test]
    fn relative_or_empty_cwd_writes_nothing() {
        let root = setup();
        // A new name is not created, command words in the same call
        // notwithstanding (contract: register, refused row).
        for bad in ["relative/path", ""] {
            assert!(matches!(
                cmd_register(
                    &root,
                    "new",
                    "sb",
                    None,
                    None,
                    Some(bad.to_string()),
                    vec!["/bin/sleep".into(), "1".into()],
                ),
                Err(SeatError::BadCwd(_))
            ));
        }
        assert!(!seat::seat_dir(&root, "new").exists(), "refused register created nothing");

        // An existing seat keeps its stored launch, byte for byte.
        let cwd = root.display().to_string();
        cmd_register(
            &root,
            "alpha",
            "sb",
            None,
            None,
            Some(cwd.clone()),
            vec!["/bin/sleep".into(), "30".into()],
        )
        .unwrap();
        let dir = seat::seat_dir(&root, "alpha");
        let before = std::fs::read_to_string(seat::meta_path(&dir)).unwrap();
        let token = std::fs::read_to_string(seat::token_path(&dir)).unwrap();
        assert!(matches!(
            cmd_register(
                &root,
                "alpha",
                "sb",
                None,
                None,
                Some("relative/path".to_string()),
                vec!["/bin/sleep".into(), "9".into()],
            ),
            Err(SeatError::BadCwd(_))
        ));
        assert_eq!(
            std::fs::read_to_string(seat::meta_path(&dir)).unwrap(),
            before,
            "refused register wrote no meta.json"
        );
        assert_eq!(std::fs::read_to_string(seat::token_path(&dir)).unwrap(), token);
    }

    /// The duplicate-register rows quickstart step 4 exercises (003
    /// US2): recording a launch on a seat that already has mail keeps
    /// the token byte-for-byte, leaves the inbox drainable, and never
    /// touches launch fields the call omitted.
    #[test]
    fn duplicate_register_keeps_token_mail_and_omitted_launch() {
        let root = setup();
        let cwd = root.display().to_string();
        cmd_register(
            &root,
            "run",
            "sb",
            None,
            None,
            Some(cwd.clone()),
            vec!["/bin/sleep".into(), "30".into()],
        )
        .unwrap();
        cmd_register(&root, "box", "sb", None, None, None, Vec::new()).unwrap();
        cmd_send(&root, "run", "box", "kept").unwrap();
        let dir = seat::seat_dir(&root, "box");
        let token_before = std::fs::read(seat::token_path(&dir)).unwrap();

        // A launch recorded on a seat that already has mail.
        cmd_register(
            &root,
            "box",
            "sb",
            None,
            None,
            Some(cwd.clone()),
            vec!["/bin/sleep".into(), "5".into()],
        )
        .unwrap();
        assert_eq!(
            std::fs::read(seat::token_path(&dir)).unwrap(),
            token_before,
            "token bytes unchanged"
        );
        let meta = seat::read_meta(&dir).unwrap();
        assert_eq!(meta.cwd.as_deref(), Some(cwd.as_str()));
        assert_eq!(
            meta.cmd.as_deref(),
            Some(&["/bin/sleep".to_string(), "5".to_string()][..])
        );

        // The inbox still drains after the re-register.
        let mut buf = Vec::new();
        drain::drain(&root, "box", &mut buf).unwrap();
        assert!(String::from_utf8(buf).unwrap().contains("kept"));

        // Omitted launch fields stay as stored: a cmd-only pass keeps
        // the directory; a pass with neither keeps both.
        cmd_register(&root, "box", "sb", None, None, None, vec!["/bin/sleep".into(), "9".into()])
            .unwrap();
        cmd_register(&root, "box", "sb", None, None, None, Vec::new()).unwrap();
        let meta = seat::read_meta(&dir).unwrap();
        assert_eq!(meta.cwd.as_deref(), Some(cwd.as_str()), "cwd omitted: unchanged");
        assert_eq!(
            meta.cmd.as_deref(),
            Some(&["/bin/sleep".to_string(), "9".to_string()][..]),
            "no fields passed: launch unchanged"
        );

        // The mail drained exactly once: a second drain finds nothing.
        let mut buf = Vec::new();
        drain::drain(&root, "box", &mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "", "mail not duplicated");
    }

    #[test]
    fn reregister_changes_only_passed_launch_fields() {
        let root = setup();
        let cwd = root.display().to_string();
        cmd_register(
            &root,
            "alpha",
            "sb",
            None,
            None,
            Some(cwd.clone()),
            vec!["/bin/sleep".into(), "5".into()],
        )
        .unwrap();
        let dir = seat::seat_dir(&root, "alpha");

        // Zero words after `--` is an omission, not a clear.
        cmd_register(&root, "alpha", "sb", None, None, None, Vec::new()).unwrap();
        let meta = seat::read_meta(&dir).unwrap();
        assert_eq!(meta.cmd.as_deref(), Some(&["/bin/sleep".to_string(), "5".to_string()][..]));

        // Only the command passed: the directory stays.
        cmd_register(&root, "alpha", "sb", None, None, None, vec!["/bin/sleep".into(), "9".into()])
            .unwrap();
        let meta = seat::read_meta(&dir).unwrap();
        assert_eq!(meta.cwd.as_deref(), Some(cwd.as_str()));
        assert_eq!(meta.cmd.as_deref(), Some(&["/bin/sleep".to_string(), "9".to_string()][..]));

        // Only the directory passed: the command stays.
        let other = root.join("elsewhere").display().to_string();
        std::fs::create_dir_all(&other).unwrap();
        cmd_register(&root, "alpha", "sb", None, None, Some(other.clone()), Vec::new()).unwrap();
        let meta = seat::read_meta(&dir).unwrap();
        assert_eq!(meta.cwd.as_deref(), Some(other.as_str()));
        assert_eq!(meta.cmd.as_deref(), Some(&["/bin/sleep".to_string(), "9".to_string()][..]));
    }
}
