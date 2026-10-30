use crate::board::{self, valid_name};
use crate::error::{Result, SeatError};
use crate::identity;
use crate::msg;
use crate::seat;
use std::path::Path;

pub fn cmd_init(root: &Path) -> Result<()> {
    board::init(root)?;
    println!("initialized seat board at {}", root.display());
    Ok(())
}

/// Register a seat: layout + one-time token mint (O_EXCL) + meta. A
/// duplicate register keeps that seat's token, inbox, and meta — only
/// launch fields actually passed change, and a refused register (a
/// relative or empty `--cwd`, or an unreadable `--idle-after`)
/// writes nothing. `--idle-after` follows the same rule as the launch
/// fields: omitted keeps the stored value, a passed value updates it
/// even when cwd and command are omitted.
pub fn cmd_register(
    root: &Path,
    name: &str,
    harness: &str,
    model: Option<String>,
    cwd: Option<String>,
    cmd: Vec<String>,
    idle_after: Option<String>,
) -> Result<()> {
    board::ensure_board(root)?;
    valid_name(name)?;
    // Record-time check: the path must
    // already be absolute. Refused here, before anything is written.
    if let Some(c) = cwd.as_deref() {
        if c.is_empty() || !Path::new(c).is_absolute() {
            return Err(SeatError::BadCwd(c.to_string()));
        }
    }
    // An unreadable duration is refused the same way: nothing written.
    if let Some(d) = idle_after.as_deref() {
        if seat::parse_idle_after(d).is_none() {
            return Err(SeatError::BadIdleAfter(d.to_string()));
        }
    }
    let dir = seat::seat_dir(root, name);
    if seat::meta_path(&dir).exists() || seat::token_path(&dir).exists() {
        // Zero words after `--` means the command was omitted, not
        // cleared (contract: register). Nothing passed changes nothing.
        if cwd.is_none() && cmd.is_empty() && idle_after.is_none() {
            println!("seat '{name}' already registered (token and mail kept)");
            return Ok(());
        }
        let launch_changed = cwd.is_some() || !cmd.is_empty();
        seat::update_meta(&dir, |m| {
            if let Some(c) = cwd {
                m.cwd = Some(c);
            }
            if !cmd.is_empty() {
                m.cmd = Some(cmd);
            }
            if let Some(d) = idle_after {
                m.idle_after = Some(d);
            }
        })?;
        println!("seat '{name}' already registered (token and mail kept)");
        if launch_changed {
            println!("seat '{name}' launch updated");
        }
        return Ok(());
    }
    seat::create(root, name)?;
    msg::mint_token(&seat::token_path(&dir))?;
    let mut meta = seat::Meta {
        harness: harness.to_string(),
        model,
        last_seen: Some(seat::now_ts()),
        ..Default::default()
    };
    if let Some(c) = cwd {
        meta.cwd = Some(c);
    }
    if !cmd.is_empty() {
        meta.cmd = Some(cmd);
    }
    if let Some(d) = idle_after {
        meta.idle_after = Some(d);
    }
    seat::write_meta(&dir, &meta)?;
    println!("registered seat '{name}' (harness={harness})");
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
/// in the destination seat's inbox. The sender is fixed by the identity
/// rule (`--from` optional; see `crate::identity`): a bound session
/// sends only as its seat, an unbound caller's omitted `--from` sends
/// as `inbox` to a factory seat. `env_session` is the caller's
/// `HAPI_SESSION_ID`, read at the CLI edge. This is the library half
/// of the verb: the identity and token checks, the write, the
/// last-seen touch — and no printing, so service callers (the
/// dispatcher's alarm) send without touching stdout and pass no
/// `HAPI_SESSION_ID` into the identity rule.
pub fn send(
    root: &Path,
    from: Option<&str>,
    to: &str,
    body: &str,
    env_session: Option<&str>,
) -> Result<msg::Header> {
    board::ensure_board(root)?;
    let who = identity::resolve_for_send(root, from, env_session, to)?;
    let from = who.from.as_str();
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
        who.session.as_deref(),
    )?;
    touch_last_seen(root, from)?;
    Ok(header)
}

/// The verb: the library send plus the one printed line.
pub fn cmd_send(
    root: &Path,
    from: Option<&str>,
    to: &str,
    body: &str,
    env_session: Option<&str>,
) -> Result<()> {
    let header = send(root, from, to, body, env_session)?;
    println!("sent {} from {} to {to}", header.id, header.from);
    Ok(())
}

/// Bump `meta.last_seen` for a seat that just acted on the board.
/// Atomic read-modify-write under the metadata lock (never the drain
/// lock), so a send never waits on a drain and never clobbers a
/// concurrent change to another field.
pub fn touch_last_seen(root: &Path, seat_name: &str) -> Result<()> {
    let dir = seat::seat_dir(root, seat_name);
    seat::update_meta(&dir, |meta| {
        meta.last_seen = Some(seat::now_ts());
    })?;
    Ok(())
}

/// The burn switch: remove the board under the root.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drain;
    use crate::testutil::temp_root;

    fn setup() -> std::path::PathBuf {
        let root = temp_root("verbs");
        board::init(&root).unwrap();
        root
    }

    #[test]
    fn register_writes_meta_and_token() {
        let root = setup();
        cmd_register(
            &root,
            "alpha",
            "sb",
            Some("glm".into()),
            None,
            Vec::new(),
            None,
        )
        .unwrap();
        let dir = seat::seat_dir(&root, "alpha");
        let meta = seat::read_meta(&dir).unwrap();
        assert_eq!(meta.harness, "sb");
        assert_eq!(meta.model.as_deref(), Some("glm"));
        assert!(meta.last_seen.is_some());
        assert!(seat::token_path(&dir).is_file());
    }

    #[test]
    fn duplicate_register_keeps_token_and_inbox() {
        let root = setup();
        cmd_register(&root, "alpha", "sb", None, None, Vec::new(), None).unwrap();
        cmd_register(&root, "beta", "sb", None, None, Vec::new(), None).unwrap();
        cmd_send(&root, Some("beta"), "alpha", "keep my mail", None).unwrap();
        let dir = seat::seat_dir(&root, "alpha");
        let token_before = std::fs::read_to_string(seat::token_path(&dir)).unwrap();
        let inbox_before = msg::list_msg_dir(&dir.join("inbox")).unwrap();

        // Duplicate register succeeds and touches nothing.
        cmd_register(&root, "alpha", "other-harness", None, None, Vec::new(), None).unwrap();

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
    }

    #[test]
    fn register_refuses_bad_names() {
        let root = setup();
        assert!(matches!(
            cmd_register(&root, "../evil", "sb", None, None, Vec::new(), None),
            Err(SeatError::BadName(_))
        ));
        assert!(matches!(
            cmd_register(&root, "a b", "sb", None, None, Vec::new(), None),
            Err(SeatError::BadName(_))
        ));
    }

    #[test]
    fn send_requires_both_seats_and_updates_last_seen() {
        let root = setup();
        cmd_register(&root, "alpha", "sb", None, None, Vec::new(), None).unwrap();
        cmd_register(&root, "beta", "sb", None, None, Vec::new(), None).unwrap();

        assert!(matches!(
            cmd_send(&root, Some("ghost"), "beta", "hi", None),
            Err(SeatError::GhostSeat(_))
        ));
        assert!(matches!(
            cmd_send(&root, Some("alpha"), "ghost", "hi", None),
            Err(SeatError::UnknownSeat(_))
        ));

        cmd_send(&root, Some("alpha"), "beta", "hello verbs", None).unwrap();
        let inbox = seat::seat_dir(&root, "beta").join("inbox");
        assert_eq!(msg::list_msg_dir(&inbox).unwrap().len(), 1);
        let meta = seat::read_meta(&seat::seat_dir(&root, "alpha")).unwrap();
        assert!(meta.last_seen.is_some());
    }

    #[test]
    fn reset_removes_the_board() {
        let root = setup();
        cmd_register(&root, "alpha", "sb", None, None, Vec::new(), None).unwrap();
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
            Some(cwd.clone()),
            vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
            None,
        )
        .unwrap();
        let meta = seat::read_meta(&seat::seat_dir(&root, "runner")).unwrap();
        assert_eq!(meta.cwd.as_deref(), Some(cwd.as_str()));
        assert_eq!(
            meta.cmd.as_deref(),
            Some(
                &[
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "sleep 30".to_string()
                ][..]
            )
        );
        // A mail-only seat stores neither field.
        cmd_register(&root, "mailbox", "sb", None, None, Vec::new(), None).unwrap();
        let meta = seat::read_meta(&seat::seat_dir(&root, "mailbox")).unwrap();
        assert_eq!(meta.cwd, None);
        assert_eq!(meta.cmd, None);
        let raw =
            std::fs::read_to_string(seat::meta_path(&seat::seat_dir(&root, "mailbox"))).unwrap();
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
                    Some(bad.to_string()),
                    vec!["/bin/sleep".into(), "1".into()],
                    None,
                ),
                Err(SeatError::BadCwd(_))
            ));
        }
        assert!(
            !seat::seat_dir(&root, "new").exists(),
            "refused register created nothing"
        );

        // An existing seat keeps its stored launch, byte for byte.
        let cwd = root.display().to_string();
        cmd_register(
            &root,
            "alpha",
            "sb",
            None,
            Some(cwd.clone()),
            vec!["/bin/sleep".into(), "30".into()],
            None,
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
                Some("relative/path".to_string()),
                vec!["/bin/sleep".into(), "9".into()],
                None,
            ),
            Err(SeatError::BadCwd(_))
        ));
        assert_eq!(
            std::fs::read_to_string(seat::meta_path(&dir)).unwrap(),
            before,
            "refused register wrote no meta.json"
        );
        assert_eq!(
            std::fs::read_to_string(seat::token_path(&dir)).unwrap(),
            token
        );
    }

    /// Duplicate register: recording a launch on a seat that already has mail keeps
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
            Some(cwd.clone()),
            vec!["/bin/sleep".into(), "30".into()],
            None,
        )
        .unwrap();
        cmd_register(&root, "box", "sb", None, None, Vec::new(), None).unwrap();
        cmd_send(&root, Some("run"), "box", "kept", None).unwrap();
        let dir = seat::seat_dir(&root, "box");
        let token_before = std::fs::read(seat::token_path(&dir)).unwrap();

        // A launch recorded on a seat that already has mail.
        cmd_register(
            &root,
            "box",
            "sb",
            None,
            Some(cwd.clone()),
            vec!["/bin/sleep".into(), "5".into()],
            None,
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
        cmd_register(
            &root,
            "box",
            "sb",
            None,
            None,
            vec!["/bin/sleep".into(), "9".into()],
            None,
        )
        .unwrap();
        cmd_register(&root, "box", "sb", None, None, Vec::new(), None).unwrap();
        let meta = seat::read_meta(&dir).unwrap();
        assert_eq!(
            meta.cwd.as_deref(),
            Some(cwd.as_str()),
            "cwd omitted: unchanged"
        );
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
            Some(cwd.clone()),
            vec!["/bin/sleep".into(), "5".into()],
            None,
        )
        .unwrap();
        let dir = seat::seat_dir(&root, "alpha");

        // Zero words after `--` is an omission, not a clear.
        cmd_register(&root, "alpha", "sb", None, None, Vec::new(), None).unwrap();
        let meta = seat::read_meta(&dir).unwrap();
        assert_eq!(
            meta.cmd.as_deref(),
            Some(&["/bin/sleep".to_string(), "5".to_string()][..])
        );

        // Only the command passed: the directory stays.
        cmd_register(
            &root,
            "alpha",
            "sb",
            None,
            None,
            vec!["/bin/sleep".into(), "9".into()],
            None,
        )
        .unwrap();
        let meta = seat::read_meta(&dir).unwrap();
        assert_eq!(meta.cwd.as_deref(), Some(cwd.as_str()));
        assert_eq!(
            meta.cmd.as_deref(),
            Some(&["/bin/sleep".to_string(), "9".to_string()][..])
        );

        // Only the directory passed: the command stays.
        let other = root.join("elsewhere").display().to_string();
        std::fs::create_dir_all(&other).unwrap();
        cmd_register(
            &root,
            "alpha",
            "sb",
            None,
            Some(other.clone()),
            Vec::new(),
            None,
        )
        .unwrap();
        let meta = seat::read_meta(&dir).unwrap();
        assert_eq!(meta.cwd.as_deref(), Some(other.as_str()));
        assert_eq!(
            meta.cmd.as_deref(),
            Some(&["/bin/sleep".to_string(), "9".to_string()][..])
        );
    }

    /// `--idle-after`: stored on meta.json, kept when a later
    /// register omits it, updated when passed — even with cwd and
    /// command both omitted, the path that used to return early — and
    /// an unreadable duration writes nothing.
    #[test]
    fn register_stores_keeps_and_updates_idle_after() {
        let root = setup();
        cmd_register(&root, "alpha", "sb", None, None, Vec::new(), None).unwrap();
        let dir = seat::seat_dir(&root, "alpha");
        assert_eq!(
            seat::read_meta(&dir).unwrap().idle_after,
            None,
            "a first register without it means never"
        );
        let raw = std::fs::read_to_string(seat::meta_path(&dir)).unwrap();
        assert!(!raw.contains("idle_after"), "{raw}");

        // A passed flag stores it; a later register that omits it keeps it.
        cmd_register(
            &root,
            "alpha",
            "sb",
            None,
            None,
            Vec::new(),
            Some("30m".into()),
        )
        .unwrap();
        assert_eq!(
            seat::read_meta(&dir).unwrap().idle_after.as_deref(),
            Some("30m")
        );
        cmd_register(
            &root,
            "alpha",
            "sb",
            None,
            None,
            vec!["/bin/sleep".into(), "9".into()],
            None,
        )
        .unwrap();
        assert_eq!(
            seat::read_meta(&dir).unwrap().idle_after.as_deref(),
            Some("30m"),
            "an omitted flag keeps the stored value"
        );

        // A passed flag updates it even when cwd and command are omitted.
        cmd_register(
            &root,
            "alpha",
            "sb",
            None,
            None,
            Vec::new(),
            Some("2h".into()),
        )
        .unwrap();
        assert_eq!(
            seat::read_meta(&dir).unwrap().idle_after.as_deref(),
            Some("2h")
        );

        // An unreadable duration is refused and writes nothing. The
        // last one parses as u64 but its seconds overflow (9.2e18
        // minutes): refused the same way, never wrapped to a zero
        // threshold.
        let before = std::fs::read_to_string(seat::meta_path(&dir)).unwrap();
        for bad in [
            "30",
            "m",
            "30x",
            "1h30m",
            "9223372036854775808m",
            "18446744073709551616s",
        ] {
            assert!(
                matches!(
                    cmd_register(&root, "alpha", "sb", None, None, Vec::new(), Some(bad.into())),
                    Err(SeatError::BadIdleAfter(_))
                ),
                "{bad} must be refused"
            );
            assert!(
                matches!(
                    cmd_register(&root, "fresh", "sb", None, None, Vec::new(), Some(bad.into())),
                    Err(SeatError::BadIdleAfter(_))
                ),
                "{bad} refused for a new seat too"
            );
        }
        assert!(
            !seat::seat_dir(&root, "fresh").exists(),
            "a refused register created nothing"
        );
        assert_eq!(
            std::fs::read_to_string(seat::meta_path(&dir)).unwrap(),
            before,
            "a refused register wrote no meta.json"
        );
    }

    /// The identity-rule board: inbox, a bound seat, and an ordinary
    /// seat, with bindings tying the bound seat to a session id.
    fn identity_setup() -> std::path::PathBuf {
        let root = temp_root("verbs-identity");
        board::init(&root).unwrap();
        for name in ["alpha", "inbox", "opus-main", "bll-factory"] {
            cmd_register(&root, name, "sb", None, None, Vec::new(), None).unwrap();
        }
        crate::testutil::write_bindings(
            &root,
            &[("opus-main", "sid-opus"), ("bll-factory", "sid-factory")],
        );
        root
    }

    fn one_inbox_message(root: &std::path::Path, to: &str) -> std::path::PathBuf {
        let inbox = seat::seat_dir(root, to).join("inbox");
        let messages = msg::list_msg_dir(&inbox).unwrap();
        assert_eq!(messages.len(), 1);
        messages[0].clone()
    }

    #[test]
    fn bound_session_sends_as_itself() {
        let root = identity_setup();
        cmd_send(
            &root,
            None,
            "alpha",
            "from the bound seat",
            Some("sid-opus"),
        )
        .unwrap();
        let p = one_inbox_message(&root, "alpha");
        let (header, body) = msg::read_msg(&p).unwrap();
        assert_eq!(header.from, "opus-main");
        assert_eq!(
            header.session, None,
            "bound senders write no session header"
        );
        assert_eq!(body, "from the bound seat");
        let token = sender_token(&root, "opus-main").unwrap();
        assert!(msg::verify_msg(&p, &token).unwrap());
        // Explicit and matching --from is the same send.
        cmd_send(
            &root,
            Some("opus-main"),
            "alpha",
            "explicit match",
            Some("sid-opus"),
        )
        .unwrap();
        assert_eq!(
            msg::list_msg_dir(&seat::seat_dir(&root, "alpha").join("inbox"))
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn bound_session_sending_as_another_seat_is_refused() {
        let root = identity_setup();
        let err = cmd_send(
            &root,
            Some("bll-factory"),
            "alpha",
            "nope",
            Some("sid-opus"),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("bound to opus-main")
                && err.to_string().contains("cannot send as bll-factory"),
            "{err}"
        );
        assert!(matches!(err, SeatError::Identity(_)));
        // Nothing was sent.
        assert!(
            msg::list_msg_dir(&seat::seat_dir(&root, "alpha").join("inbox"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn unbound_explicit_from_is_unchanged() {
        let root = identity_setup();
        // No session id at all: exactly today's send.
        cmd_send(&root, Some("alpha"), "bll-factory", "plain send", None).unwrap();
        let p = one_inbox_message(&root, "bll-factory");
        let (header, _) = msg::read_msg(&p).unwrap();
        assert_eq!(header.from, "alpha");
        assert_eq!(header.session, None);

        // An unbound session id with an explicit --from: same sender,
        // plus the session header, covered by the HMAC.
        cmd_send(
            &root,
            Some("alpha"),
            "bll-factory",
            "with session",
            Some("sid-adhoc"),
        )
        .unwrap();
        let messages =
            msg::list_msg_dir(&seat::seat_dir(&root, "bll-factory").join("inbox")).unwrap();
        assert_eq!(messages.len(), 2);
        let (header, _) = msg::read_msg(messages.last().unwrap()).unwrap();
        assert_eq!(header.from, "alpha");
        assert_eq!(header.session.as_deref(), Some("sid-adhoc"));
        let token = sender_token(&root, "alpha").unwrap();
        assert!(msg::verify_msg(messages.last().unwrap(), &token).unwrap());
    }

    #[test]
    fn unbound_omitted_from_sends_as_inbox_to_a_factory_seat() {
        let root = identity_setup();
        cmd_send(&root, None, "bll-factory", "hq mail", None).unwrap();
        let p = one_inbox_message(&root, "bll-factory");
        let (header, body) = msg::read_msg(&p).unwrap();
        assert_eq!(header.from, "inbox");
        assert_eq!(body, "hq mail");
        assert_eq!(header.session, None);
        let token = sender_token(&root, "inbox").unwrap();
        assert!(msg::verify_msg(&p, &token).unwrap());
        // last_seen was stamped on the acting seat (inbox).
        let meta = seat::read_meta(&seat::seat_dir(&root, "inbox")).unwrap();
        assert!(meta.last_seen.is_some());
    }

    #[test]
    fn unbound_omitted_from_to_a_non_factory_seat_is_refused() {
        let root = identity_setup();
        let err = cmd_send(&root, None, "alpha", "no diego here", None).unwrap_err();
        assert!(matches!(err, SeatError::Identity(_)), "{err}");
        assert!(err.to_string().contains("inbox"), "{err}");
        assert!(err.to_string().contains("alpha"), "{err}");
        assert!(
            msg::list_msg_dir(&seat::seat_dir(&root, "alpha").join("inbox"))
                .unwrap()
                .is_empty()
        );

        // An unbound session id fares the same, and writes nothing.
        let err = cmd_send(&root, None, "alpha", "no diego here", Some("sid-adhoc")).unwrap_err();
        assert!(matches!(err, SeatError::Identity(_)), "{err}");
        // No bindings file at all: no seat is a factory seat.
        let bare = temp_root("verbs-identity-bare");
        board::init(&bare).unwrap();
        for name in ["alpha", "inbox"] {
            cmd_register(&bare, name, "sb", None, None, Vec::new(), None).unwrap();
        }
        assert!(cmd_send(&bare, None, "alpha", "no file", None).is_err());
    }
}
