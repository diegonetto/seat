//! The dispatcher check cases against the one small fake hub on
//! 127.0.0.1 (never the live hub): the brief's hapi contract plus the
//! local-state cases (`awake`, the fresh-work wake), the alarm matrix
//! and the reload, the undelivered-versus-thinking cases, and
//! the sleep/wake cases.
use crate::alarm;
use crate::dispatch::{self, Pass, Waits};
use crate::error::SeatError;
use crate::hub::Hub;
use crate::testutil::dispatchkit::{board_with, kit, log_text, mark, n_files, n_posts, only_key, send};
use crate::testutil::fakehub::FakeHub;
use crate::testutil::fakewebhook::FakeWebhook;
use crate::testutil::write_bindings;
use crate::webhook::{self, RingWaits, WebhookConfig};
use crate::{drain, dstate, msg, seat, verbs};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn wait_for(pred: impl Fn() -> bool, msg: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !pred() {
        assert!(std::time::Instant::now() < deadline, "{msg}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Auth happens once; after the hub retires that JWT, the next pass
/// meets a 401, re-authenticates, and still delivers.
#[test]
fn auth_and_reauth_on_401() {
    let (root, hub, mut client) = kit(&[("opus", "s1")], &["s1"]);
    send(&root, "opus", "re-auth me");
    mark(&hub, &only_key(&root, "opus"), "invoked");
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Delivered
    );
    hub.ctl.lock().unwrap().banned_jwts.insert("jwt-1".into());
    send(&root, "opus", "second mail");
    mark(&hub, &only_key(&root, "opus"), "invoked");
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Delivered
    );
    assert_eq!(hub.auth_count(), 2, "a 401 must not stop delivery");
    assert_eq!(n_files(&root, "opus", "archive"), 2);
}

/// An inactive or missing session is never POSTed: the mail stays
/// staged in draining/, one failure line per seat, nothing archived.
#[test]
fn inactive_or_404_session_is_never_posted() {
    let (root, hub, mut client) = kit(&[("opus", "s1"), ("bll", "s2")], &[]);
    hub.ctl.lock().unwrap().sessions.insert("s1".into(), false); // s2: 404
    send(&root, "opus", "held for opus");
    send(&root, "bll", "held for bll");
    for seat in ["opus", "bll"] {
        assert_eq!(
            dispatch::pass_seat(&root, seat, &mut client).unwrap(),
            Pass::Blocked
        );
        assert_eq!(
            (
                n_files(&root, seat, "draining"),
                n_files(&root, seat, "archive")
            ),
            (1, 0),
            "{seat}'s mail stays staged"
        );
    }
    let journal = hub.journal();
    assert_eq!(
        n_posts(&journal),
        0,
        "no POST to an inactive or missing session"
    );
    assert!(journal.contains(&"GET /api/sessions/s1".to_string()));
    let log = log_text(&root);
    assert!(
        log.contains("failure seat=opus") && log.contains("failure seat=bll"),
        "{log}"
    );
}

/// Queued then invoked commits exactly the invoked item; the next
/// pass delivers the queued one.
#[test]
fn queued_then_invoked_commits_exactly_that_item() {
    let (root, hub, mut client) = kit(&[("opus", "s1")], &["s1"]);
    send(&root, "opus", "first");
    send(&root, "opus", "second");
    let keys: Vec<String> = msg::list_msg_dir(&seat::seat_dir(&root, "opus").join("inbox"))
        .unwrap()
        .iter()
        .map(|p| dstate::direct_key("opus", p.file_name().unwrap().to_str().unwrap()))
        .collect();
    mark(&hub, &keys[0], "invoked");
    mark(&hub, &keys[1], "queued");
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Waiting
    );
    assert_eq!(
        (
            n_files(&root, "opus", "archive"),
            n_files(&root, "opus", "draining")
        ),
        (1, 1),
        "exactly the invoked item; the queued one stays staged"
    );
    let log = log_text(&root);
    assert!(log.contains(&keys[0]) && !log.contains(&keys[1]), "{log}");
    mark(&hub, &keys[1], "invoked");
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Delivered
    );
    assert_eq!(n_files(&root, "opus", "archive"), 2);
}

/// Invoked while the session is inactive does not commit.
#[test]
fn invoked_while_inactive_does_not_commit() {
    let (root, hub, mut client) = kit(&[("opus", "s1")], &["s1"]);
    hub.ctl.lock().unwrap().flip_inactive.insert("s1".into()); // flips when polled
    send(&root, "opus", "invoked then gone");
    mark(&hub, &only_key(&root, "opus"), "invoked");
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Blocked
    );
    assert_eq!(
        (n_posts(&hub.journal()), n_files(&root, "opus", "archive")),
        (1, 0)
    );
    assert_eq!(n_files(&root, "opus", "draining"), 1, "still in flight");
    let log = log_text(&root);
    assert!(
        !log.contains("delivered") && log.contains("failure seat=opus"),
        "{log}"
    );
}

/// The resume case of the same rule: the invoked mark the ending
/// session left must not commit even once the session is active again
/// (a resume can keep the same session id) — the item is re-sent under
/// a new delivery key, and only the new key's invocation archives it.
#[test]
fn invoked_while_inactive_rotates_the_key_and_resends_once_active() {
    let (root, hub, mut client) = kit(&[("opus", "s1")], &["s1"]);
    hub.ctl.lock().unwrap().flip_inactive.insert("s1".into()); // flips when polled
    send(&root, "opus", "invoked then gone");
    let key = only_key(&root, "opus");
    mark(&hub, &key, "invoked"); // the ending session's mark
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Blocked
    );
    assert_eq!(n_files(&root, "opus", "archive"), 0, "nothing archived");
    assert_eq!(n_files(&root, "opus", "draining"), 1, "still in flight");
    // The in-flight item rotated to a fresh key, Staged.
    let fresh = format!("{key}#2");
    let state = dstate::load_state(&seat::seat_dir(&root, "opus"));
    assert_eq!(
        state.items.keys().collect::<Vec<_>>(),
        vec![&fresh],
        "the old key is gone from the state"
    );
    assert_eq!(
        state.items.get(&fresh).unwrap().phase,
        dstate::Phase::Staged
    );
    // The session is active again under the same id; the old invoked
    // mark is never answered, the mail is re-sent under the new key,
    // and only that invocation commits.
    {
        let mut ctl = hub.ctl.lock().unwrap();
        ctl.flip_inactive.clear();
        ctl.sessions.insert("s1".into(), true);
    }
    mark(&hub, &fresh, "invoked");
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Delivered
    );
    assert_eq!(
        n_files(&root, "opus", "archive"),
        1,
        "archived on the new key"
    );
    let journal = hub.journal();
    assert!(
        journal.contains(&format!("POST-MSG s1 {key}"))
            && journal.contains(&format!("POST-MSG s1 {fresh}")),
        "{journal:?}"
    );
    let log = log_text(&root);
    assert!(log.contains(&format!("key={fresh}")), "{log}");
}

/// The partial-POST-failure case of the same rule: the POST loop
/// persists only after it succeeds, so a loop whose second POST fails
/// leaves the first message accepted by the hub but recorded Staged
/// on disk. The ending session marks that key invoked anyway, so the
/// inactive pass must rotate every in-flight item — Staged on disk
/// included — and a resume that keeps the session id must not commit
/// on the old mark; only the fresh keys' invocation archives.
#[test]
fn a_partly_failed_post_loop_still_rotates_the_accepted_item() {
    let (root, hub, mut client) = kit(&[("opus", "s1")], &["s1"]);
    send(&root, "opus", "accepted by the hub");
    send(&root, "opus", "never accepted");
    let keys: Vec<String> = msg::list_msg_dir(&seat::seat_dir(&root, "opus").join("inbox"))
        .unwrap()
        .iter()
        .map(|p| dstate::direct_key("opus", p.file_name().unwrap().to_str().unwrap()))
        .collect();
    // The loop POSTs in key order; the second POST fails, so the
    // first item is accepted while dispatch.json still says Staged.
    hub.ctl.lock().unwrap().fail_keys.insert(keys[1].clone());
    assert!(dispatch::pass_seat(&root, "opus", &mut client).is_err());
    let state = dstate::load_state(&seat::seat_dir(&root, "opus"));
    assert!(
        state
            .items
            .values()
            .all(|i| i.phase == dstate::Phase::Staged),
        "the store after the loop never ran: {state:?}"
    );
    // The session ends — the hub marks the accepted key invoked — and
    // the next pass finds it inactive.
    mark(&hub, &keys[0], "invoked");
    hub.ctl.lock().unwrap().sessions.insert("s1".into(), false);
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Blocked
    );
    let state = dstate::load_state(&seat::seat_dir(&root, "opus"));
    let fresh: Vec<String> = state.items.keys().cloned().collect();
    assert_eq!(
        fresh,
        vec![format!("{}#2", keys[0]), format!("{}#2", keys[1])],
        "every in-flight item rotated, Staged on disk included"
    );
    // The resume keeps the session id: the old invoked mark must not
    // commit — accepted-but-unread mail is not archived.
    {
        let mut ctl = hub.ctl.lock().unwrap();
        ctl.fail_keys.clear();
        ctl.sessions.insert("s1".into(), true);
    }
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Waiting
    );
    assert_eq!(
        n_files(&root, "opus", "archive"),
        0,
        "unread mail is never archived on the old mark"
    );
    assert_eq!(
        hub.journal()
            .iter()
            .filter(|l| **l == format!("POST-MSG s1 {}", keys[0]))
            .count(),
        1,
        "the rotated key is never re-POSTed"
    );
    // Only the fresh keys' invocation commits.
    for key in &fresh {
        mark(&hub, key, "invoked");
    }
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Delivered
    );
    assert_eq!(n_files(&root, "opus", "archive"), 2);
}

/// Indeterminate means wait: nothing committed, nothing re-POSTed, and
/// waiting is not a failure.
#[test]
fn indeterminate_waits() {
    let (root, hub, mut client) = kit(&[("opus", "s1")], &["s1"]);
    send(&root, "opus", "indeterminate mail");
    mark(&hub, &only_key(&root, "opus"), "indeterminate");
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Waiting
    );
    assert_eq!(
        (n_files(&root, "opus", "archive"), n_posts(&hub.journal())),
        (0, 1)
    );
    assert!(!log_text(&root).contains("failure"));
}

/// An id absent from all three lists is re-POSTed with the same key.
#[test]
fn absent_reposts_with_the_same_key() {
    let (root, hub, mut client) = kit(&[("opus", "s1")], &["s1"]);
    send(&root, "opus", "lost by the hub");
    let key = only_key(&root, "opus"); // no mark: the hub lost it
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Waiting
    );
    let journal = hub.journal();
    let posts: Vec<&String> = journal
        .iter()
        .filter(|l| l.starts_with("POST-MSG"))
        .collect();
    assert!(
        posts.len() >= 2 && posts.iter().all(|l| l.ends_with(&key)),
        "{posts:?}"
    );
}

/// A crash between POST and commit resumes from dispatch.json without
/// a second distinct POST: the state says posted, so the next pass
/// polls instead of POSTing, then commits.
#[test]
fn crash_between_post_and_commit_resumes() {
    let (root, hub, mut client) = kit(&[("opus", "s1")], &["s1"]);
    send(&root, "opus", "survive the crash");
    let key = only_key(&root, "opus");
    mark(&hub, &key, "queued");
    dispatch::pass_seat(&root, "opus", &mut client).unwrap(); // POST, no commit
    mark(&hub, &key, "invoked");
    let mut client = crate::hub::Hub::new(hub.config()); // a "restart"
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Delivered
    );
    assert_eq!(
        (n_posts(&hub.journal()), n_files(&root, "opus", "archive")),
        (1, 1)
    );
}

/// A binding that changed while items were in flight re-POSTs them to
/// the new session under the same keys.
#[test]
fn a_changed_binding_reposts_in_flight_items() {
    let (root, hub, mut client) = kit(&[("opus", "s1")], &["s1", "s2"]);
    send(&root, "opus", "moved mid-flight");
    let key = only_key(&root, "opus");
    mark(&hub, &key, "queued");
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Waiting
    );
    write_bindings(&root, &[("opus", "s2")]);
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Waiting
    );
    let journal = hub.journal();
    let posts = |s: &str| journal.contains(&format!("POST-MSG {s} {key}"));
    assert!(posts("s1") && posts("s2"), "{journal:?}");
}

/// A manual drain that took the file first counts as done: the next
/// pass prunes the key quietly — no new POST, no failure line.
#[test]
fn manual_drain_took_the_file_counts_as_done() {
    let (root, hub, mut client) = kit(&[("opus", "s1")], &["s1"]);
    send(&root, "opus", "taken by a manual drain");
    mark(&hub, &only_key(&root, "opus"), "queued");
    dispatch::pass_seat(&root, "opus", &mut client).unwrap(); // POST, waiting
    let mut out = Vec::new();
    drain::drain(&root, "opus", &mut out).unwrap(); // the manual drain
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Idle
    );
    assert_eq!(n_posts(&hub.journal()), 1);
    assert!(
        !log_text(&root).contains("failure"),
        "done elsewhere is not failure"
    );
}

/// One seat's hung hub does not delay another seat's delivery; the
/// blocked worker's own 10 s bound ends it.
#[test]
fn one_seats_hung_hub_does_not_delay_another() {
    let (root, hub, _client) = kit(&[("opus", "s1"), ("bll", "s2")], &["s1", "s2"]);
    {
        let mut ctl = hub.ctl.lock().unwrap();
        ctl.hang.insert("s1".into()); // opus's session never answers
    }
    send(&root, "opus", "stuck");
    send(&root, "bll", "flows");
    mark(&hub, &only_key(&root, "bll"), "invoked");
    let fast = Waits {
        first: Duration::from_millis(10),
        max: Duration::from_millis(10),
    };
    let cfg = hub.config();
    let stop = Arc::new(AtomicBool::new(false));
    let worker = |name: &'static str| {
        let (r, c, s) = (root.clone(), cfg.clone(), stop.clone());
        std::thread::spawn(move || dispatch::worker_loop(r, name.into(), c, s, fast))
    };
    let a = worker("opus");
    let b = worker("bll");
    wait_for(
        || n_files(&root, "bll", "archive") >= 1,
        "bll was delayed by opus's hung hub",
    );
    stop.store(true, Ordering::SeqCst);
    a.join().unwrap(); // opus times out at its own 10 s bound
    b.join().unwrap();
    let log = log_text(&root);
    assert!(
        log.contains("delivered seat=bll") && log.contains("failure seat=opus"),
        "{log}"
    );
}

/// The POSTed text is exactly the lines `seat drain` prints for the
/// item — compared against drain's own stdout, not a rebuilt copy of
/// the helper.
#[test]
fn posted_text_is_exactly_the_drain_lines() {
    let (root, hub, mut client) = kit(&[("opus", "s1")], &["s1"]);
    verbs::cmd_send(&root, Some("opus"), "opus", "line one\nline two", None).unwrap();
    let key = only_key(&root, "opus");
    mark(&hub, &key, "queued"); // stays staged, so drain prints it
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Waiting
    );
    let mut out = Vec::new();
    drain::drain(&root, "opus", &mut out).unwrap();
    assert_eq!(
        hub.ctl.lock().unwrap().texts.get(&key).map(String::as_str),
        Some(String::from_utf8_lossy(&out).as_ref()),
        "the POST must be byte for byte what drain prints"
    );
}

/// Status's `awake` reads dispatch.json alone: true after a delivery,
/// false when older than 10 minutes or the binding moved.
#[test]
fn awake_reads_the_last_delivery_locally() {
    let (root, hub, mut client) = kit(&[("opus", "s1")], &["s1"]);
    send(&root, "opus", "wake up");
    mark(&hub, &only_key(&root, "opus"), "invoked");
    dispatch::pass_seat(&root, "opus", &mut client).unwrap();
    assert!(dstate::awake(&root, "opus"));
    let dir = seat::seat_dir(&root, "opus");
    let mut state = dstate::load_state(&dir);
    state.last_delivery.as_mut().unwrap().invoked_at =
        (chrono::Utc::now() - chrono::TimeDelta::try_minutes(11).unwrap()).to_rfc3339();
    dstate::store_state(&dir, &state).unwrap();
    assert!(!dstate::awake(&root, "opus"), "11 minutes old is not awake");
    let ld = state.last_delivery.as_mut().unwrap();
    ld.invoked_at = seat::now_ts();
    ld.session = "s9".into();
    dstate::store_state(&dir, &state).unwrap();
    assert!(
        !dstate::awake(&root, "opus"),
        "a moved binding is not awake"
    );
}

/// A waiting worker sleeps up to its 5-minute queued-state backoff
/// between passes, but fresh work never waits behind it: a moved
/// binding and new inbox mail each wake the worker at once (the
/// 2-second scan's promise, held mid-backoff).
#[test]
fn fresh_work_wakes_a_backing_off_worker_at_once() {
    let (root, hub, _client) = kit(&[("opus", "s1")], &["s1", "s2"]);
    send(&root, "opus", "stuck queued");
    let key = only_key(&root, "opus");
    mark(&hub, &key, "queued");
    let stop = Arc::new(AtomicBool::new(false));
    let (r, cfg, s) = (root.clone(), hub.config(), Arc::clone(&stop));
    std::thread::spawn(move || {
        dispatch::worker_loop(
            r,
            "opus".into(),
            cfg,
            s,
            Waits {
                first: Duration::from_secs(30),
                max: Duration::from_secs(30),
            },
        )
    });
    let posted = |line: &str| hub.journal().iter().any(|l| l.contains(line));
    wait_for(
        || posted(&format!("POST-MSG s1 {key}")),
        "the first POST never happened",
    );
    write_bindings(&root, &[("opus", "s2")]);
    wait_for(
        || posted(&format!("POST-MSG s2 {key}")),
        "a moved binding waited behind the queued backoff",
    );
    send(&root, "opus", "fresh mail");
    wait_for(
        || {
            hub.journal()
                .iter()
                .filter(|l| l.starts_with("POST-MSG s2"))
                .count()
                >= 2
        },
        "new mail waited behind the queued backoff",
    );
    stop.store(true, Ordering::SeqCst);
}

/// The alarm bodies mailed to the seat named in `<root>/alarm-seat`
/// (the watcher's unread inbox, oldest first).
fn alarm_mail(root: &std::path::Path) -> Vec<String> {
    let to = std::fs::read_to_string(root.join("alarm-seat"))
        .unwrap()
        .trim()
        .to_string();
    msg::list_msg_dir(&seat::seat_dir(root, &to).join("inbox"))
        .unwrap()
        .iter()
        .filter_map(|p| msg::read_msg(p).ok().map(|(_, b)| b))
        .collect()
}

/// The alarm sink's board: `sb-watch` registered with a token, plus a
/// watcher seat named in `<root>/alarm-seat`.
fn watch_board(root: &std::path::Path, watcher: &str) {
    verbs::cmd_register(root, alarm::SEAT, "sb", None, None, Vec::new(), None).unwrap();
    verbs::cmd_register(root, watcher, "sb", None, None, Vec::new(), None).unwrap();
    std::fs::write(root.join("alarm-seat"), format!("{watcher}\n")).unwrap();
}

/// The frozen alarm contract, one matrix: each cause fires once (one
/// log line, one direct message from `sb-watch` to the seat named in
/// `<root>/alarm-seat`), retries stay silent,
/// recovery clears — a recovered hub clears even while the session
/// reads inactive — and a recurrence alarms again. HOME points at a
/// throwaway directory for the whole matrix, so nothing the test runs
/// can read the operator's `~/.hapi/settings.json`.
#[test]
fn alarm_matrix_once_per_cause_and_recovery() {
    crate::testutil::with_throwaway_home(alarm_matrix_once_per_cause_and_recovery_isolated);
}

fn alarm_matrix_once_per_cause_and_recovery_isolated() {
    let (root, hub, mut client) = kit(&[("opus", "s1")], &[]);
    watch_board(&root, "diego");
    hub.ctl.lock().unwrap().sessions.insert("s1".into(), false);

    // session-inactive: fires once across retries; failure lines stay.
    send(&root, "opus", "held for opus");
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Blocked
    );
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Blocked
    );
    let log = log_text(&root);
    assert_eq!(
        log.matches("alarm seat=opus cause=session-inactive")
            .count(),
        1
    );
    assert_eq!(log.matches("failure seat=opus").count(), 2, "{log}");
    assert_eq!(alarm_mail(&root).len(), 1);
    assert!(alarm_mail(&root)[0].starts_with("seat opus: session-inactive: session s1"));

    // Recovery clears (the session reads active again)…
    hub.ctl.lock().unwrap().sessions.insert("s1".into(), true);
    dispatch::pass_seat(&root, "opus", &mut client).unwrap();
    // …so a recurrence alarms again.
    hub.ctl.lock().unwrap().sessions.insert("s1".into(), false);
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Blocked
    );
    assert_eq!(log_text(&root).matches("cause=session-inactive").count(), 2);
    assert_eq!(alarm_mail(&root).len(), 2);

    // no-answer (per seat) and hub-auth (board-wide): once each,
    // then cleared by any successful hub response — this one reports
    // the session inactive, and the clear still holds.
    dispatch::log_failure(
        &root,
        "opus",
        &SeatError::Hub("GET session s1: timed out".into()),
    );
    dispatch::log_failure(
        &root,
        "opus",
        &SeatError::Hub("GET session s1: timed out".into()),
    );
    dispatch::log_failure(&root, "opus", &SeatError::Hub("auth refused twice".into()));
    assert_eq!(
        log_text(&root)
            .matches("alarm seat=opus cause=no-answer")
            .count(),
        1
    );
    assert_eq!(
        log_text(&root)
            .matches("alarm seat=board cause=hub-auth")
            .count(),
        1
    );
    assert_eq!(alarm_mail(&root).len(), 4);
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Blocked
    ); // the hub answered (active:false): the causes cleared…
    dispatch::log_failure(
        &root,
        "opus",
        &SeatError::Hub("GET session s1: timed out".into()),
    );
    dispatch::log_failure(&root, "opus", &SeatError::Hub("auth refused twice".into()));
    assert_eq!(log_text(&root).matches("cause=no-answer").count(), 2);
    assert_eq!(log_text(&root).matches("cause=hub-auth").count(), 2);
    assert_eq!(alarm_mail(&root).len(), 6);

    // undelivered: an item older than 15 minutes (the ts in its file
    // name) fires once; receive quarantines it, so the next pass
    // clears; a recurrence alarms again.
    let old = seat::seat_dir(&root, "opus")
        .join("inbox")
        .join("20260101T000000.000000-deadbeef.msg");
    std::fs::write(&old, "stale\n\nstale").unwrap();
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Blocked
    );
    assert_eq!(
        log_text(&root)
            .matches("alarm seat=opus cause=undelivered")
            .count(),
        1
    );
    assert_eq!(alarm_mail(&root).len(), 7);
    dispatch::pass_seat(&root, "opus", &mut client).unwrap(); // quarantined: clears
    std::fs::write(&old, "stale\n\nstale").unwrap();
    dispatch::pass_seat(&root, "opus", &mut client).unwrap();
    assert_eq!(log_text(&root).matches("cause=undelivered").count(), 2);
    assert_eq!(alarm_mail(&root).len(), 8);

    // webhook-failing: strikes 1 and 2 log one failure line each,
    // strike 3 alarms once, further failures stay silent, and a
    // successful ring clears.
    let hook = FakeWebhook::start();
    verbs::cmd_register(&root, "grok", "sb", None, None, Vec::new(), None).unwrap();
    let gdir = seat::seat_dir(&root, "grok");
    std::fs::write(gdir.join("webhook.url"), format!("{}\n", hook.url())).unwrap();
    std::fs::write(gdir.join("bridge.key"), "k\n").unwrap();
    send(&root, "grok", "ring grok");
    let cfg = WebhookConfig::mem(hook.url(), "k".into(), None, false, None);
    // A 1ns backoff makes every pass's re-ring due at once.
    let waits = RingWaits {
        first: Duration::from_nanos(1),
        max: Duration::from_nanos(1),
    };
    hook.ctl.lock().unwrap().status = 500;
    for i in 1..=4u32 {
        webhook::ring_pass(&root, "grok", &cfg, waits, &|_| {}).unwrap();
        assert_eq!(
            log_text(&root)
                .matches(&format!("ring attempt {i} failed"))
                .count(),
            usize::from(i <= 2)
        );
    }
    assert_eq!(
        log_text(&root)
            .matches("alarm seat=grok cause=webhook-failing")
            .count(),
        1
    );
    assert_eq!(alarm_mail(&root).len(), 9);
    hook.ctl.lock().unwrap().status = 0;
    send(&root, "grok", "ring again");
    assert!(matches!(
        webhook::ring_pass(&root, "grok", &cfg, waits, &|_| {}).unwrap(),
        webhook::RingOutcome::Rang
    ));
    send(&root, "grok", "and again");
    webhook::ring_pass(&root, "grok", &cfg, waits, &|_| {}).unwrap();
    assert_eq!(
        log_text(&root).matches("cause=webhook-failing").count(),
        1,
        "cleared by the successful ring"
    );
}

// ---- undelivered against a thinking session -------------------------

/// The fixture: one real message, renamed old (the file name
/// is what the alarm ages), already staged in `draining/`, and
/// recorded in `dispatch.json` as in flight `Posted` under its
/// delivery key — exactly the record a pass that POSTed it leaves
/// behind when the turn outlives the wait. Returns the delivery key.
fn old_posted_item(root: &std::path::Path, seat_name: &str, session: &str) -> String {
    send(root, seat_name, "a long turn");
    let dir = seat::seat_dir(root, seat_name);
    let inbox = dir.join("inbox");
    let fresh = msg::list_msg_dir(&inbox).unwrap()[0].clone();
    let old = "20260101T000000.000000-deadbeef.msg";
    std::fs::create_dir_all(dir.join("draining")).unwrap();
    std::fs::rename(&fresh, dir.join("draining").join(old)).unwrap();
    let key = dstate::direct_key(seat_name, old);
    let mut state = dstate::DispatchState::default();
    state.session = Some(session.into());
    state.items.insert(
        key.clone(),
        dstate::ItemState {
            phase: dstate::Phase::Posted,
            accepted_at: Some(seat::now_ts()),
            working_at: None,
        },
    );
    dstate::store_state(&dir, &state).unwrap();
    key
}

/// The hub marks a queued message invoked only when the turn ends, so
/// a posted item on a thinking session is being worked — however long
/// the turn — and is not `undelivered`: no page while the session
/// thinks, and the record gets phase `working` with the time of the
/// read (the next pass re-reads, and the first read that does not say
/// thinking returns the item to `posted`). A second old file the
/// record does not hold as in-flight `Posted` still pages — the skip
/// is per file — and once every old file is worked again the latch
/// clears. The turn ends with the message still not invoked: the item
/// is `posted` again and one page names what the pass saw. An invoked
/// message clears as today. HOME points at a throwaway directory for
/// the whole case, like the alarm matrix.
#[test]
fn a_thinking_session_is_working_not_undelivered() {
    crate::testutil::with_throwaway_home(a_thinking_session_is_working_not_undelivered_isolated);
}

fn a_thinking_session_is_working_not_undelivered_isolated() {
    let (root, hub, mut client) = kit(&[("opus", "s1")], &[]);
    watch_board(&root, "diego");
    hub.ctl.lock().unwrap().get_bodies.insert(
        "s1".into(),
        r#"{"session":{"active":true,"thinking":true,"thinkingAt":60000}}"#.into(),
    );
    let key = old_posted_item(&root, "opus", "s1");
    mark(&hub, &key, "queued"); // posted, not invoked: the turn goes on

    // Thinking past the threshold: no page, nothing re-POSTed, and the
    // record holds `working` with the time of the read.
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Waiting
    );
    let log = log_text(&root);
    assert!(!log.contains("cause=undelivered"), "{log}");
    assert_eq!(n_posts(&hub.journal()), 0, "the item is already posted");
    let dir = seat::seat_dir(&root, "opus");
    let item = dstate::load_state(&dir).items.get(&key).unwrap().clone();
    assert_eq!(
        item.phase,
        dstate::Phase::Working,
        "the record gets the working phase"
    );
    assert!(item.working_at.is_some(), "with the time of the read");
    assert_eq!(n_files(&root, "opus", "draining"), 1);

    // A second old file the record does not hold as in-flight Posted —
    // never-posted mail — still pages: the skip is per file.
    std::fs::write(
        seat::seat_dir(&root, "opus")
            .join("inbox")
            .join("20260101T000000.000000-feedface.msg"),
        "stale\n\nstale",
    )
    .unwrap();
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Waiting
    );
    let log = log_text(&root);
    assert_eq!(
        log.matches("alarm seat=opus cause=undelivered").count(),
        1,
        "{log}"
    );
    assert_eq!(alarm_mail(&root).len(), 1);

    // The stale file is quarantined away; every old file is worked
    // again, and the latch clears (the next beat is the proof).
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Waiting
    );
    assert_eq!(
        log_text(&root).matches("cause=undelivered").count(),
        1,
        "no re-fire while the session works"
    );

    // The turn ends and the message is still not invoked: the item is
    // `posted` again and one page names the read, so a dead delivery
    // reads apart from a long turn.
    hub.ctl.lock().unwrap().get_bodies.insert(
        "s1".into(),
        r#"{"session":{"active":true,"thinking":false,"thinkingAt":0}}"#.into(),
    );
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Waiting
    );
    let item = dstate::load_state(&dir).items.get(&key).unwrap().clone();
    assert_eq!(
        item.phase,
        dstate::Phase::Posted,
        "the read that does not say thinking ends the working phase"
    );
    assert!(item.working_at.is_none());
    let log = log_text(&root);
    assert_eq!(
        log.matches("alarm seat=opus cause=undelivered").count(),
        2,
        "{log}"
    );
    assert!(
        log.contains("posted, session idle for"),
        "the page names what the pass saw: {log}"
    );
    assert_eq!(alarm_mail(&root).len(), 2);

    // Invoked clears as today: archived, delivered, and the next pass
    // clears the latch.
    mark(&hub, &key, "invoked");
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Delivered
    );
    dispatch::pass_seat(&root, "opus", &mut client).unwrap();
    assert_eq!(n_files(&root, "opus", "archive"), 1);
    assert!(
        !std::fs::read_to_string(root.join("dispatch-alarms.json"))
            .unwrap_or_default()
            .contains("undelivered:opus"),
        "the condition cleared"
    );
}

/// A hub error is never a skip: today's page still happens, the page
/// names the error it saw, and the delivery gate fails the pass as it
/// already did.
#[test]
fn a_hub_error_pages_undelivered_with_the_error_named() {
    crate::testutil::with_throwaway_home(a_hub_error_pages_undelivered_with_the_error_named_isolated);
}

fn a_hub_error_pages_undelivered_with_the_error_named_isolated() {
    let (root, hub, mut client) = kit(&[("opus", "s1")], &[]);
    watch_board(&root, "diego");
    hub.ctl.lock().unwrap().get_bodies.insert(
        "s1".into(),
        r#"{"session":{"active":true,"thinking":true,"thinkingAt":60000}}"#.into(),
    );
    let key = old_posted_item(&root, "opus", "s1");
    mark(&hub, &key, "queued");
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Waiting
    );
    assert!(!log_text(&root).contains("cause=undelivered"));

    // The hub stops answering the session read: the page happens with
    // the error named — never a silent skip.
    hub.ctl.lock().unwrap().fail_get.insert("s1".into());
    assert!(
        dispatch::pass_seat(&root, "opus", &mut client).is_err(),
        "the delivery gate fails as today"
    );
    let log = log_text(&root);
    assert_eq!(
        log.matches("alarm seat=opus cause=undelivered").count(),
        1,
        "{log}"
    );
    assert!(
        log.contains("posted, hub session read failed") && log.contains("status 500"),
        "the page names the error: {log}"
    );
    let mail = alarm_mail(&root);
    assert_eq!(mail.len(), 1);
    assert!(mail[0].contains("hub session read failed"));
}

/// The other hub error path: the session read
/// succeeds and says idle, but the queued-state read fails. That
/// failure is never a skip and never a bare idle page — the item's
/// invoked fact is unknown, so the page names the queued error (the
/// pass's later poll fails with the same error, but it cannot amend
/// the message already latched here).
#[test]
fn a_queued_read_failure_pages_with_the_error_named() {
    crate::testutil::with_throwaway_home(a_queued_read_failure_pages_with_the_error_named_isolated);
}

fn a_queued_read_failure_pages_with_the_error_named_isolated() {
    let (root, hub, mut client) = kit(&[("opus", "s1")], &[]);
    watch_board(&root, "diego");
    hub.ctl.lock().unwrap().get_bodies.insert(
        "s1".into(),
        r#"{"session":{"active":true,"thinking":false,"thinkingAt":0}}"#.into(),
    );
    let key = old_posted_item(&root, "opus", "s1");
    mark(&hub, &key, "queued"); // not invoked: the fact must come from the read
    hub.ctl.lock().unwrap().fail_queued_state.insert("s1".into());
    assert!(
        dispatch::pass_seat(&root, "opus", &mut client).is_err(),
        "the delivery gate fails as today"
    );
    let log = log_text(&root);
    assert_eq!(
        log.matches("alarm seat=opus cause=undelivered").count(),
        1,
        "{log}"
    );
    assert!(
        log.contains("posted, waited")
            && log.contains("hub queued read failed")
            && log.contains("status 500"),
        "the page names the queued error: {log}"
    );
    assert!(!log.contains("session idle"), "{log}");
    let mail = alarm_mail(&root);
    assert_eq!(mail.len(), 1);
    assert!(mail[0].contains("hub queued read failed"));
}

/// Only an explicit `thinking: true` skips. The fake hub's default
/// body for an active session omits the flag, and a missing fact is
/// never a working session — the missing flag pages, it does not hush,
/// and the page says the flag was not reported (idle means an
/// explicit `false`, as ever).
#[test]
fn a_missing_thinking_flag_does_not_skip() {
    crate::testutil::with_throwaway_home(a_missing_thinking_flag_does_not_skip_isolated);
}

fn a_missing_thinking_flag_does_not_skip_isolated() {
    let (root, hub, mut client) = kit(&[("opus", "s1")], &[]);
    watch_board(&root, "diego");
    hub.ctl.lock().unwrap().sessions.insert("s1".into(), true);
    let key = old_posted_item(&root, "opus", "s1");
    mark(&hub, &key, "queued");
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Waiting
    );
    let log = log_text(&root);
    assert_eq!(
        log.matches("alarm seat=opus cause=undelivered").count(),
        1,
        "a missing flag is not a working session: {log}"
    );
    assert!(
        log.contains("posted, waited") && log.contains("session thinking unknown"),
        "the page says the read did not report the flag: {log}"
    );
    assert!(!log.contains("session idle"), "{log}");
}

/// The pass that observes the turn end: the hub marks
/// a queued message invoked when the turn ends — the session is no
/// longer thinking — while the file still sits in `draining/`, because
/// the decision runs before this pass's poll commits and archives the
/// invoked mark. That pass must not page: an active session's invoked
/// item is delivered. Only a pass that reads the item still not
/// invoked, session not thinking, past the threshold, pages.
#[test]
fn the_pass_that_observes_invoked_does_not_page() {
    crate::testutil::with_throwaway_home(the_pass_that_observes_invoked_does_not_page_isolated);
}

fn the_pass_that_observes_invoked_does_not_page_isolated() {
    let (root, hub, mut client) = kit(&[("opus", "s1")], &[]);
    watch_board(&root, "diego");
    hub.ctl.lock().unwrap().get_bodies.insert(
        "s1".into(),
        r#"{"session":{"active":true,"thinking":true,"thinkingAt":60000}}"#.into(),
    );
    let key = old_posted_item(&root, "opus", "s1");
    mark(&hub, &key, "queued");

    // The turn runs past the threshold: working, no page.
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Waiting
    );
    assert!(!log_text(&root).contains("cause=undelivered"));

    // The turn ends and the hub has marked the message invoked: one
    // pass observes both facts together — not thinking, invoked, file
    // still in draining/ — and delivers without a page.
    hub.ctl.lock().unwrap().get_bodies.insert(
        "s1".into(),
        r#"{"session":{"active":true,"thinking":false,"thinkingAt":0}}"#.into(),
    );
    mark(&hub, &key, "invoked");
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Delivered
    );
    let log = log_text(&root);
    assert!(!log.contains("cause=undelivered"), "{log}");
    assert_eq!(n_files(&root, "opus", "archive"), 1);
    assert!(
        !std::fs::read_to_string(root.join("dispatch-alarms.json"))
            .unwrap_or_default()
            .contains("undelivered:opus"),
        "the condition never arose"
    );
}

/// The page words match the read: `session idle`
/// only for an explicit `thinking: false` on an active session — a
/// missing flag pages as `thinking unknown`, a session that is not
/// active (a 404 reads `Ok` with the flag unset) as `session not
/// active` — and both still page; the inactive pass keeps today's
/// file-age page beside its `session-inactive`.
#[test]
fn the_page_names_the_session_fact_it_actually_read() {
    crate::testutil::with_throwaway_home(the_page_names_the_session_fact_it_actually_read_isolated);
}

fn the_page_names_the_session_fact_it_actually_read_isolated() {
    let (root, hub, mut client) = kit(&[("opus", "s1"), ("bll", "s2")], &[]);
    watch_board(&root, "diego");
    // opus: active, the thinking flag missing from the answer.
    hub.ctl.lock().unwrap().sessions.insert("s1".into(), true);
    // bll: no session at all — the 404 reads Ok with the flag unset.
    let opus_key = old_posted_item(&root, "opus", "s1");
    let bll_key = old_posted_item(&root, "bll", "s2");
    mark(&hub, &opus_key, "queued");
    mark(&hub, &bll_key, "queued");
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Waiting
    );
    assert_eq!(
        dispatch::pass_seat(&root, "bll", &mut client).unwrap(),
        Pass::Blocked
    );
    let log = log_text(&root);
    assert!(
        log.contains("seat=opus cause=undelivered") && log.contains("seat=bll cause=undelivered"),
        "both still page: {log}"
    );
    assert!(
        log.matches("posted, waited").count() == 2
            && log.contains("session thinking unknown")
            && log.contains("session not active"),
        "the words match the read: {log}"
    );
    assert!(!log.contains("session idle"), "{log}");
}

/// dispatch.json and the alarm state survive a reload: both are
/// written with the board's durable shape (0600, no scratch left) and
/// reload equal; a re-fired cause stays latched across the reload.
/// HOME points at a throwaway directory, so nothing the test runs can
/// read the operator's `~/.hapi/settings.json`.
#[test]
fn dispatch_state_and_alarms_survive_a_reload() {
    crate::testutil::with_throwaway_home(dispatch_state_and_alarms_survive_a_reload_isolated);
}

fn dispatch_state_and_alarms_survive_a_reload_isolated() {
    use std::os::unix::fs::PermissionsExt;
    let root = crate::testutil::dispatchkit::board_with(&[]);
    verbs::cmd_register(&root, "alpha", "sb", None, None, Vec::new(), None).unwrap();
    let dir = seat::seat_dir(&root, "alpha");
    let mut state = dstate::DispatchState::default();
    state.session = Some("s1".into());
    state
        .items
        .insert("seat:alpha:x.msg".into(), Default::default());
    dstate::store_state(&dir, &state).unwrap();
    assert_eq!(
        std::fs::metadata(dir.join("dispatch.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(dstate::load_state(&dir).session.as_deref(), Some("s1"));
    assert_eq!(dstate::load_state(&dir).items.len(), 1);
    watch_board(&root, "diego");
    alarm::fire(&root, Some("alpha"), alarm::SESSION_INACTIVE, "s1 gone").unwrap();
    alarm::fire(&root, Some("alpha"), alarm::SESSION_INACTIVE, "again").unwrap();
    assert_eq!(alarm_mail(&root).len(), 1, "latched across the reload");
    let raw = std::fs::read_to_string(root.join("dispatch-alarms.json")).unwrap();
    assert!(raw.contains("session-inactive:alpha"), "{raw}");
    assert_eq!(
        std::fs::metadata(root.join("dispatch-alarms.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let scratch: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with(".dispatch.json"))
        .collect();
    assert!(scratch.is_empty(), "scratch left: {scratch:?}");
}

/// Without `<root>/alarm-seat` the alarm is the log line only: the
/// cause still latches, and no message is sent anywhere.
#[test]
fn without_alarm_seat_the_log_line_is_the_whole_alarm() {
    let root = crate::testutil::dispatchkit::board_with(&[]);
    verbs::cmd_register(&root, "alpha", "sb", None, None, Vec::new(), None).unwrap();
    verbs::cmd_register(&root, alarm::SEAT, "sb", None, None, Vec::new(), None).unwrap();
    alarm::fire(&root, Some("alpha"), alarm::SESSION_INACTIVE, "s1 gone").unwrap();
    alarm::fire(&root, Some("alpha"), alarm::SESSION_INACTIVE, "again").unwrap();
    assert_eq!(
        log_text(&root)
            .matches("alarm seat=alpha cause=session-inactive")
            .count(),
        1,
        "once per cause, and the log line is written"
    );
    // No seat on this board received anything.
    for name in ["alpha", alarm::SEAT] {
        assert!(
            msg::list_msg_dir(&seat::seat_dir(&root, name).join("inbox"))
                .unwrap()
                .is_empty(),
            "{name} was mailed without an alarm-seat file"
        );
    }
    assert!(std::fs::read_to_string(root.join("dispatch-alarms.json"))
        .unwrap_or_default()
        .contains("session-inactive:alpha"));
}

/// A present `<root>/alarm-seat` naming a seat that is not on the
/// board is a failed sink, not the log-only case: the send fails, one
/// `alarm-post-failed` line is logged, and the cause stays latched
/// (a corrected file still receives nothing until the condition
/// clears and returns).
#[test]
fn an_alarm_seat_naming_a_missing_seat_is_a_failed_sink() {
    let root = crate::testutil::dispatchkit::board_with(&[]);
    verbs::cmd_register(&root, "alpha", "sb", None, None, Vec::new(), None).unwrap();
    verbs::cmd_register(&root, alarm::SEAT, "sb", None, None, Vec::new(), None).unwrap();
    std::fs::write(root.join("alarm-seat"), "ghost\n").unwrap();
    alarm::fire(&root, Some("alpha"), alarm::SESSION_INACTIVE, "s1 gone").unwrap();
    let log = log_text(&root);
    assert_eq!(
        log.matches("alarm seat=alpha cause=session-inactive")
            .count(),
        1
    );
    assert!(
        log.contains("alarm-post-failed"),
        "the failed sink is logged: {log}"
    );
    assert!(std::fs::read_to_string(root.join("dispatch-alarms.json"))
        .unwrap_or_default()
        .contains("session-inactive:alpha"));
    // Latched: a second fire of the same cause stays silent (the
    // failed-sink line is not another alarm).
    alarm::fire(&root, Some("alpha"), alarm::SESSION_INACTIVE, "again").unwrap();
    assert_eq!(
        log_text(&root)
            .matches("alarm seat=alpha cause=session-inactive")
            .count(),
        1
    );
    assert_eq!(
        log_text(&root).matches("alarm-post-failed").count(),
        1,
        "the retry did not re-attempt the sink"
    );
}

/// One dispatcher per board: `cmd_dispatch` holds an exclusive,
/// non-blocking lock on `<root>/dispatch.lock` for its whole life
/// (`--once` included), so a second dispatcher fails at once — never
/// waits — with one line naming the lock, and succeeds again once the
/// first lets go.
#[test]
fn a_second_dispatcher_exits_nonzero_while_the_first_holds_the_lock() {
    let root = crate::testutil::dispatchkit::board_with(&[]);
    let held = seat::Flock::dispatch(&root).unwrap();
    assert!(seat::dispatch_lock_path(&root).is_file());
    let err = dispatch::cmd_dispatch(&root, true).unwrap_err();
    let line = err.to_string();
    assert!(line.lines().count() == 1, "one line: {line}");
    assert!(
        line.contains(
            seat::dispatch_lock_path(&root)
                .display()
                .to_string()
                .as_str()
        ),
        "the lock is named: {line}"
    );
    // Released when the first holder drops: the next dispatcher runs.
    drop(held);
    dispatch::cmd_dispatch(&root, true).unwrap();
}

// ---- sleep and wake -------------------------------------------------

/// The fake multiplexer the sleep cases bring a pane up under (the
/// same one the session tests use).
fn fake_tmux() -> String {
    let p = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake-tmux.sh");
    assert!(Path::new(p).is_file(), "fake-tmux.sh must exist at {p}");
    p.to_string()
}

/// Hold the process-wide env lock and set variables for the body,
/// restoring the previous values on drop (even on panic). The stubs
/// these tests install are visible to every subprocess this process
/// spawns, so they never run beside a test that reads them.
struct Env {
    saved: Vec<(&'static str, Option<OsString>)>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

fn lock_env(vars: &[(&'static str, OsString)]) -> Env {
    let lock = crate::testutil::ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut saved = Vec::new();
    for (name, value) in vars {
        saved.push((*name, std::env::var_os(name)));
        std::env::set_var(name, value);
    }
    Env { saved, _lock: lock }
}

impl Drop for Env {
    fn drop(&mut self) {
        for (name, value) in &self.saved {
            match value {
                Some(v) => std::env::set_var(name, v),
                None => std::env::remove_var(name),
            }
        }
    }
}

/// `dir` first on PATH (the rest kept: the stubs shadow `sb` and
/// `seat-role`, everything else still resolves).
fn path_with(dir: &Path) -> OsString {
    let prev = std::env::var_os("PATH").unwrap_or_default();
    OsString::from(format!("{}:{}", dir.display(), prev.to_string_lossy()))
}

/// Write `name` into `dir` as an executable `#!/bin/sh` script.
fn stub(dir: &Path, name: &str, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A stub directory for the sleep/wake cases: an `sb` answering
/// `sb seats --json` (`Some(json)` prints it, `None` fails the map
/// read) and, when wanted, a `seat-role` whose wake is the given
/// body. The wake-success bodies are built by `wake_success`.
fn stub_kit(tag: &str, sb_map: Option<&str>, seat_role_body: Option<&str>) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "seat-test-kit-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    match sb_map {
        Some(json) => stub(&dir, "sb", &format!("printf '%s' '{json}'")),
        None => stub(&dir, "sb", "exit 1"),
    }
    if let Some(body) = seat_role_body {
        stub(&dir, "seat-role", body);
    }
    dir
}

/// The seat-role stub body for a successful wake: it refuses any call
/// but `wake <role>`, writes the binding for `seat` → `session` at
/// `root` (the script is the only writer of that file), and answers
/// the kit's awake JSON.
fn wake_success(root: &Path, seat: &str, role: &str, session: &str) -> String {
    r#"
[ "$1" = "wake" ] && [ "$2" = "ROLE" ] || { echo "stub: expected wake ROLE, got $*" >&2; exit 9; }
printf '%s\n' '{"schema_version":1,"seats":{"SEAT":{"url":"https://hapi.bll.dev/sessions/SESSION","moved_at":"2026-10-23T12:00:00Z"}}}' > 'ROOT/hapi-sessions.json'
printf '%s' '{"status":"awake","url":"https://hapi.bll.dev/sessions/SESSION"}'
"#
    .replace("ROLE", role)
    .replace("SEAT", seat)
    .replace("SESSION", session)
    .replace("ROOT", &root.display().to_string())
}

/// One sleep case's board: `seat` hapi-bound to `session`, up under
/// the fake multiplexer (state at `<root>/tmux.state`), registered
/// with `--idle-after 30m`, its pane first seen `minutes_ago`. The
/// board sits behind a fake `SEAT_ROLE_HOME` whose
/// `.local/share/seat` resolves to it — the supported configuration,
/// since the real wake script writes exactly that path. The caller
/// holds ENV_LOCK, PATH holds the stub kit, SEAT_TMUX is set.
fn up_and_seen(
    seat: &str,
    session: &str,
    minutes_ago: i64,
    thinking_body: &str,
) -> (PathBuf, FakeHub, Hub) {
    let (root, hub, client) = kit(&[(seat, session)], &[]);
    let home = root.join("role-home");
    std::fs::create_dir_all(home.join(".local").join("share")).unwrap();
    std::os::unix::fs::symlink(&root, home.join(".local").join("share").join("seat")).unwrap();
    std::env::set_var("SEAT_ROLE_HOME", &home);
    hub.ctl
        .lock()
        .unwrap()
        .get_bodies
        .insert(session.into(), thinking_body.into());
    verbs::cmd_register(
        &root,
        seat,
        "sb",
        None,
        Some(root.display().to_string()),
        vec!["/bin/sleep".into(), "30".into()],
        Some("30m".into()),
    )
    .unwrap();
    std::env::set_var("SEAT_FAKE_STATE", root.join("tmux.state"));
    crate::session::cmd_up(&root, &[seat.to_string()]).unwrap();
    let live = crate::session::live_pane_id(seat)
        .expect("the fake multiplexer answers")
        .expect("the pane is up");
    let dir = seat::seat_dir(&root, seat);
    let mut state = dstate::load_state(&dir);
    state.pane_seen_at = Some(
        (chrono::Utc::now() - chrono::TimeDelta::try_minutes(minutes_ago).unwrap()).to_rfc3339(),
    );
    state.pane_id = Some(live);
    dstate::store_state(&dir, &state).unwrap();
    (root, hub, client)
}

/// A quiet session body for the thinking gate: explicitly not
/// thinking.
const QUIET: &str = r#"{"session":{"active":true,"thinking":false,"thinkingAt":0}}"#;

/// The seat is past its threshold with an empty inbox and a
/// non-thinking session: it sleeps — the pane dies through the `down`
/// path, dispatch.json records why — once and only once.
#[test]
fn idle_seat_sleeps_once_and_only_once() {
    let kit_dir = stub_kit(
        "sleep-once",
        Some(r#"{"seats":{"opus":{"role":"opus"}}}"#),
        None,
    );
    let _env = lock_env(&[
        ("PATH", path_with(&kit_dir)),
        ("SEAT_KIT", OsString::from("")),
        ("SEAT_TMUX", OsString::from(fake_tmux())),
    ]);
    let (root, _hub, mut client) = up_and_seen("opus", "s1", 31, QUIET);
    assert!(
        crate::session::running_seats(&fake_tmux())
            .unwrap()
            .contains("opus")
    );

    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Idle
    );
    let dir = seat::seat_dir(&root, "opus");
    let state = dstate::load_state(&dir);
    assert_eq!(
        state.asleep,
        Some(dstate::Asleep {
            at: state.asleep.as_ref().unwrap().at.clone(),
            reason: "idle 30m".into(),
        }),
        "the record names the cause"
    );
    assert_eq!(state.pane_seen_at, None, "the clock died with the pane");
    assert!(
        !crate::session::running_seats(&fake_tmux())
            .unwrap()
            .contains("opus"),
        "the pane was killed"
    );
    let log = log_text(&root);
    assert!(log.contains("sleep seat=opus reason=idle 30m"), "{log}");

    // Only once: further passes are quiet, the record never moves,
    // and no error is raised for the seat that is already down.
    let at = dstate::load_state(&dir).asleep.unwrap().at;
    for _ in 0..2 {
        assert_eq!(
            dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
            Pass::Idle
        );
    }
    let state = dstate::load_state(&dir);
    assert_eq!(state.asleep.as_ref().unwrap().at, at, "slept once");
    assert_eq!(log_text(&root).matches("sleep seat=opus").count(), 1);
    assert!(state.woken.is_none());
}

/// The idle clock is the pane fact plus a delivery to the currently
/// bound session: a delivery inside the window blocks sleep, a stale
/// one does not, and a delivery to a session the seat is no longer
/// bound to never counted.
#[test]
fn the_idle_clock_is_the_pane_plus_the_bound_delivery() {
    let kit_dir = stub_kit(
        "sleep-clock",
        Some(r#"{"seats":{"opus":{"role":"opus"}}}"#),
        None,
    );
    let _env = lock_env(&[
        ("PATH", path_with(&kit_dir)),
        ("SEAT_KIT", OsString::from("")),
        ("SEAT_TMUX", OsString::from(fake_tmux())),
    ]);

    // A delivery to the bound session just now: the clock is recent.
    let (root, _hub, mut client) = up_and_seen("opus", "s1", 31, QUIET);
    let dir = seat::seat_dir(&root, "opus");
    let mut state = dstate::load_state(&dir);
    state.last_delivery = Some(dstate::LastDelivery {
        session: "s1".into(),
        invoked_at: seat::now_ts(),
    });
    dstate::store_state(&dir, &state).unwrap();
    dispatch::pass_seat(&root, "opus", &mut client).unwrap();
    assert!(dstate::load_state(&dir).asleep.is_none(), "still awake");
    assert!(
        crate::session::running_seats(&fake_tmux())
            .unwrap()
            .contains("opus")
    );

    // The same delivery 31 minutes old: past the threshold.
    let mut state = dstate::load_state(&dir);
    state.last_delivery.as_mut().unwrap().invoked_at =
        (chrono::Utc::now() - chrono::TimeDelta::try_minutes(31).unwrap()).to_rfc3339();
    dstate::store_state(&dir, &state).unwrap();
    dispatch::pass_seat(&root, "opus", &mut client).unwrap();
    assert!(dstate::load_state(&dir).asleep.is_some(), "it sleeps");

    // A just-now delivery to a session that is not the binding does
    // not count: the clock is the pane fact alone.
    let (root, _hub, mut client) = up_and_seen("opus", "s1", 31, QUIET);
    let dir = seat::seat_dir(&root, "opus");
    let mut state = dstate::load_state(&dir);
    state.last_delivery = Some(dstate::LastDelivery {
        session: "s9".into(),
        invoked_at: seat::now_ts(),
    });
    dstate::store_state(&dir, &state).unwrap();
    dispatch::pass_seat(&root, "opus", &mut client).unwrap();
    assert!(
        dstate::load_state(&dir).asleep.is_some(),
        "a moved binding's delivery never counted"
    );
}

/// A thinking session is working, whatever the dispatch record says:
/// it never sleeps.
#[test]
fn a_thinking_session_never_sleeps() {
    let kit_dir = stub_kit(
        "sleep-thinking",
        Some(r#"{"seats":{"opus":{"role":"opus"}}}"#),
        None,
    );
    let _env = lock_env(&[
        ("PATH", path_with(&kit_dir)),
        ("SEAT_KIT", OsString::from("")),
        ("SEAT_TMUX", OsString::from(fake_tmux())),
    ]);
    let (root, hub, mut client) = up_and_seen(
        "opus",
        "s1",
        31,
        r#"{"session":{"active":true,"thinking":true,"thinkingAt":60000}}"#,
    );
    dispatch::pass_seat(&root, "opus", &mut client).unwrap();
    let dir = seat::seat_dir(&root, "opus");
    assert!(dstate::load_state(&dir).asleep.is_none(), "working");
    assert!(
        crate::session::running_seats(&fake_tmux())
            .unwrap()
            .contains("opus"),
        "the pane stands"
    );
    assert!(
        hub.journal().contains(&"GET /api/sessions/s1".to_string()),
        "the hub was asked"
    );
    assert!(!log_text(&root).contains("sleep seat=opus"));
}

/// A missing `thinking` key, a non-boolean one, a 404, and a GET
/// error all mean not idle — a missing flag is never permission to
/// sleep.
#[test]
fn a_bad_thinking_answer_never_sleeps() {
    let kit_dir = stub_kit(
        "sleep-bad-answer",
        Some(r#"{"seats":{"opus":{"role":"opus"}}}"#),
        None,
    );
    let _env = lock_env(&[
        ("PATH", path_with(&kit_dir)),
        ("SEAT_KIT", OsString::from("")),
        ("SEAT_TMUX", OsString::from(fake_tmux())),
    ]);
    let cases: [(&str, Option<&str>); 4] = [
        ("missing key", Some(r#"{"session":{"active":true}}"#)),
        (
            "non-boolean",
            Some(r#"{"session":{"active":true,"thinking":"yes"}}"#),
        ),
        ("404", None), // no sessions entry, no body: the fake answers 404
        ("GET error", None), // fail_get below answers 500
    ];
    for (name, body) in cases {
        let (root, hub, mut client) = up_and_seen("opus", "s1", 31, "");
        let mut ctl = hub.ctl.lock().unwrap();
        ctl.get_bodies.remove("s1"); // up_and_seen stored the empty body
        if let Some(body) = body {
            ctl.get_bodies.insert("s1".into(), body.into());
        }
        if name == "GET error" {
            ctl.fail_get.insert("s1".into());
        }
        drop(ctl);
        dispatch::pass_seat(&root, "opus", &mut client).unwrap();
        let dir = seat::seat_dir(&root, "opus");
        assert!(dstate::load_state(&dir).asleep.is_none(), "{name}: slept");
        assert!(
            crate::session::running_seats(&fake_tmux())
                .unwrap()
                .contains("opus"),
            "{name}: the pane stands"
        );
        assert!(
            hub.journal().contains(&"GET /api/sessions/s1".to_string()),
            "{name}: the hub was asked"
        );
    }
}

/// An in-flight item never sleeps, even past the threshold with a
/// quiet session: the mail is delivered first.
#[test]
fn an_in_flight_item_never_sleeps() {
    let kit_dir = stub_kit(
        "sleep-inflight",
        Some(r#"{"seats":{"opus":{"role":"opus"}}}"#),
        None,
    );
    let _env = lock_env(&[
        ("PATH", path_with(&kit_dir)),
        ("SEAT_KIT", OsString::from("")),
        ("SEAT_TMUX", OsString::from(fake_tmux())),
    ]);
    let (root, hub, mut client) = up_and_seen("opus", "s1", 31, QUIET);
    hub.ctl.lock().unwrap().sessions.insert("s1".into(), true);
    send(&root, "opus", "still working");
    let key = only_key(&root, "opus");
    mark(&hub, &key, "queued");
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Waiting
    );
    let dir = seat::seat_dir(&root, "opus");
    assert!(dstate::load_state(&dir).asleep.is_none(), "mail first");
    assert!(
        crate::session::running_seats(&fake_tmux())
            .unwrap()
            .contains("opus")
    );
}

/// A seat without `idle_after`, and a seat the dispatcher has never
/// seen a pane for, are never touched. The threshold-less seat runs
/// on the wake script's own board with its pane up, its clock old,
/// and everything else the sleep rule asks already holding — so the
/// missing `idle_after` is the only thing keeping it awake (remove
/// that gate and this case sleeps).
#[test]
fn a_seat_without_a_threshold_or_seen_pane_never_sleeps() {
    let kit_dir = stub_kit(
        "sleep-unarmed",
        Some(r#"{"seats":{"opus":{"role":"opus"},"drowsy":{"role":"drowsy"}}}"#),
        None,
    );
    let _env = lock_env(&[
        ("PATH", path_with(&kit_dir)),
        ("SEAT_KIT", OsString::from("")),
        ("SEAT_TMUX", OsString::from(fake_tmux())),
    ]);

    let (root, hub, mut client) = kit(&[("opus", "s1"), ("drowsy", "s2")], &[]);
    // The board the wake script writes — `up_and_seen`'s symlink: on
    // this board every other sleep condition can hold.
    let home = root.join("role-home");
    std::fs::create_dir_all(home.join(".local").join("share")).unwrap();
    std::os::unix::fs::symlink(&root, home.join(".local").join("share").join("seat")).unwrap();
    std::env::set_var("SEAT_ROLE_HOME", &home);
    hub.ctl
        .lock()
        .unwrap()
        .get_bodies
        .insert("s1".into(), QUIET.into());
    hub.ctl
        .lock()
        .unwrap()
        .get_bodies
        .insert("s2".into(), QUIET.into());
    // opus has no threshold; drowsy has the threshold but never a
    // seen pane (it is never up when a pass looks).
    verbs::cmd_register(
        &root,
        "opus",
        "sb",
        None,
        Some(root.display().to_string()),
        vec!["/bin/sleep".into(), "30".into()],
        None,
    )
    .unwrap();
    verbs::cmd_register(
        &root,
        "drowsy",
        "sb",
        None,
        Some(root.display().to_string()),
        vec!["/bin/sleep".into(), "30".into()],
        Some("30m".into()),
    )
    .unwrap();
    std::env::set_var("SEAT_FAKE_STATE", root.join("tmux.state"));
    // opus's pane is up and its clock is 31 minutes old: without the
    // stored threshold this seat would sleep on this pass.
    crate::session::cmd_up(&root, &["opus".to_string()]).unwrap();
    let live = crate::session::live_pane_id("opus")
        .expect("the fake multiplexer answers")
        .expect("the pane is up");
    let dir = seat::seat_dir(&root, "opus");
    let mut state = dstate::load_state(&dir);
    state.pane_seen_at = Some(
        (chrono::Utc::now() - chrono::TimeDelta::try_minutes(31).unwrap()).to_rfc3339(),
    );
    state.pane_id = Some(live);
    dstate::store_state(&dir, &state).unwrap();
    // drowsy goes down before any pass sees its pane: with no seen
    // pane it never sleeps.
    crate::session::cmd_up(&root, &["drowsy".to_string()]).unwrap();
    crate::session::cmd_down(&root, &["drowsy".to_string()]).unwrap();

    dispatch::pass_seat(&root, "opus", &mut client).unwrap();
    assert!(
        dstate::load_state(&dir).asleep.is_none(),
        "no threshold: never"
    );
    assert!(
        crate::session::running_seats(&fake_tmux())
            .unwrap()
            .contains("opus"),
        "the pane was never touched"
    );
    dispatch::pass_seat(&root, "drowsy", &mut client).unwrap();
    assert!(
        dstate::load_state(&seat::seat_dir(&root, "drowsy"))
            .asleep
            .is_none(),
        "no seen pane: never"
    );
    assert!(!log_text(&root).contains("sleep seat="));
}

/// `idle_after` without a role in the kernel seat map never sleeps —
/// the wake takes a role — and neither does a map the kernel refuses
/// to answer.
#[test]
fn idle_after_without_a_role_never_sleeps() {
    let no_role = stub_kit("sleep-norole", Some(r#"{"seats":{"other":{"role":"x"}}}"#), None);
    let map_fails = stub_kit("sleep-nomap", None, None);
    for (name, kit_dir) in [("no role in the map", no_role), ("map read fails", map_fails)] {
        let _env = lock_env(&[
            ("PATH", path_with(&kit_dir)),
            ("SEAT_KIT", OsString::from("")),
            ("SEAT_TMUX", OsString::from(fake_tmux())),
        ]);
        let (root, _hub, mut client) = up_and_seen("opus", "s1", 31, QUIET);
        dispatch::pass_seat(&root, "opus", &mut client).unwrap();
        assert!(
            dstate::load_state(&seat::seat_dir(&root, "opus"))
                .asleep
                .is_none(),
            "{name}: slept"
        );
        assert!(
            crate::session::running_seats(&fake_tmux())
                .unwrap()
                .contains("opus"),
            "{name}: the pane stands"
        );
    }
}

/// Mail to an asleep seat runs the wake — the worker's first act, a
/// stubbed `seat-role` on PATH that writes the binding and prints the
/// kit's awake JSON — and delivers in the same pass, clearing
/// `asleep` and recording the delivery key that woke it.
#[test]
fn mail_to_an_asleep_seat_wakes_and_delivers_in_the_same_pass() {
    let root = board_with(&[("opus", "s1")]); // s1: the binding sleep left behind
    let kit_dir = stub_kit(
        "wake-deliver",
        Some(r#"{"seats":{"opus":{"role":"opus"}}}"#),
        Some(&wake_success(&root, "opus", "opus", "s2")),
    );
    let hub = FakeHub::start();
    hub.ctl.lock().unwrap().sessions.insert("s2".into(), true);
    let mut client = Hub::new(hub.config());
    let _env = lock_env(&[
        ("PATH", path_with(&kit_dir)),
        ("SEAT_KIT", OsString::from("")),
        ("SEAT_TMUX", OsString::from(fake_tmux())),
        ("SEAT_FAKE_STATE", OsString::from(root.join("tmux.state").display().to_string())),
    ]);
    let dir = seat::seat_dir(&root, "opus");
    dstate::store_state(
        &dir,
        &dstate::DispatchState {
            asleep: Some(dstate::Asleep {
                at: "2026-10-23T00:00:00Z".into(),
                reason: "idle 30m".into(),
            }),
            ..Default::default()
        },
    )
    .unwrap();
    send(&root, "opus", "wake up");
    let key = only_key(&root, "opus");
    mark(&hub, &key, "invoked");

    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Delivered,
        "the wake and the delivery share one pass"
    );
    let state = dstate::load_state(&dir);
    assert_eq!(state.asleep, None, "the mark is cleared");
    assert_eq!(state.woken.map(|w| w.key), Some(key.clone()));
    assert_eq!(n_files(&root, "opus", "archive"), 1);
    let journal = hub.journal();
    assert!(journal.contains(&format!("POST-MSG s2 {key}")), "{journal:?}");
    assert!(
        !journal.iter().any(|l| l.starts_with("POST-MSG s1")),
        "the dead binding is never POSTed to"
    );
    assert!(log_text(&root).contains("wake seat=opus"), "the wake is logged");
}

/// A failed wake leaves the seat asleep with the item in the inbox:
/// `busy` (no reason string), `refused` (with the script's reason),
/// and an `awake` answer with no binding after it all name themselves
/// in the failure line, and nothing is ever POSTed.
#[test]
fn a_failed_wake_leaves_the_seat_asleep() {
    struct Case {
        name: &'static str,
        body: &'static str,
        log_expects: &'static str,
    }
    let cases = [
        Case {
            name: "busy",
            body: r#"printf '%s' '{"status":"busy"}'"#,
            log_expects: "answered busy",
        },
        Case {
            name: "refused",
            body: r#"printf '%s' '{"status":"refused","reason":"Windows has 512 MB free; a wake needs 3000 MB"}'"#,
            log_expects: "Windows has 512 MB free",
        },
        Case {
            name: "awake but no binding",
            body: r#"printf '%s' '{"status":"awake","url":"https://hapi.bll.dev/sessions/s2"}'"#,
            log_expects: "no binding",
        },
    ];
    for case in cases {
        let root = board_with(&[]);
        verbs::cmd_register(&root, "opus", "sb", None, None, Vec::new(), None).unwrap();
        let kit_dir = stub_kit(
            "wake-fail",
            Some(r#"{"seats":{"opus":{"role":"opus"}}}"#),
            Some(case.body),
        );
        let hub = FakeHub::start();
        hub.ctl.lock().unwrap().sessions.insert("s2".into(), true);
        let mut client = Hub::new(hub.config());
        let _env = lock_env(&[
            ("PATH", path_with(&kit_dir)),
            ("SEAT_KIT", OsString::from("")),
            ("SEAT_TMUX", OsString::from(fake_tmux())),
            (
                "SEAT_FAKE_STATE",
                OsString::from(root.join("tmux.state").display().to_string()),
            ),
        ]);
        let dir = seat::seat_dir(&root, "opus");
        let asleep = dstate::Asleep {
            at: "2026-10-23T00:00:00Z".into(),
            reason: "idle 30m".into(),
        };
        dstate::store_state(
            &dir,
            &dstate::DispatchState {
                asleep: Some(asleep.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        send(&root, "opus", "still sleeping");

        assert_eq!(
            dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
            Pass::Blocked,
            "{}: blocked, not an error",
            case.name
        );
        assert_eq!(
            dstate::load_state(&dir).asleep,
            Some(asleep),
            "{}: still asleep",
            case.name
        );
        assert_eq!(
            (n_files(&root, "opus", "inbox"), n_files(&root, "opus", "draining")),
            (1, 0),
            "{}: the item stays in the inbox",
            case.name
        );
        assert_eq!(n_files(&root, "opus", "archive"), 0);
        let log = log_text(&root);
        assert!(
            log.contains("wake failed") && log.contains(case.log_expects),
            "{}: {log}",
            case.name
        );
        assert!(
            !hub.journal().iter().any(|l| l.starts_with("POST-MSG")),
            "{}: nothing was POSTed",
            case.name
        );
    }
}

/// The scan spawns a worker for an asleep seat whose inbox holds mail
/// even though hapi-sessions.json no longer names the seat
/// (seat-role's start_down pops the binding before `seat up`): one
/// `--once` scan wakes it and delivers.
#[test]
fn the_scan_wakes_an_asleep_seat_the_binding_no_longer_names() {
    let root = board_with(&[]); // no bindings file at all
    verbs::cmd_register(&root, "opus", "sb", None, None, Vec::new(), None).unwrap();
    let kit_dir = stub_kit(
        "scan-wake",
        Some(r#"{"seats":{"opus":{"role":"opus"}}}"#),
        Some(&wake_success(&root, "opus", "opus", "s9")),
    );
    let hub = FakeHub::start();
    hub.ctl.lock().unwrap().sessions.insert("s9".into(), true);
    let mut client = Hub::new(hub.config());
    let _env = lock_env(&[
        ("PATH", path_with(&kit_dir)),
        ("SEAT_KIT", OsString::from("")),
        ("SEAT_TMUX", OsString::from(fake_tmux())),
        (
            "SEAT_FAKE_STATE",
            OsString::from(root.join("tmux.state").display().to_string()),
        ),
        ("HAPI_API_URL", OsString::from(hub.config().base)),
        ("CLI_API_TOKEN", OsString::from("test-token")),
    ]);
    let dir = seat::seat_dir(&root, "opus");
    dstate::store_state(
        &dir,
        &dstate::DispatchState {
            asleep: Some(dstate::Asleep {
                at: "2026-10-23T00:00:00Z".into(),
                reason: "idle 30m".into(),
            }),
            ..Default::default()
        },
    )
    .unwrap();
    send(&root, "opus", "find your seat");
    let key = only_key(&root, "opus");
    mark(&hub, &key, "invoked");

    let stop = Arc::new(AtomicBool::new(false));
    dispatch::run(
        &root,
        true,
        &stop,
        Waits {
            first: Duration::from_millis(5),
            max: Duration::from_millis(5),
        },
    )
    .unwrap();
    let state = dstate::load_state(&dir);
    assert_eq!(state.asleep, None, "woken by the scan");
    assert_eq!(state.woken.map(|w| w.key), Some(key.clone()));
    assert_eq!(n_files(&root, "opus", "archive"), 1);
    assert!(hub.journal().contains(&format!("POST-MSG s9 {key}")));
    // The one client here only proves the fake hub answers everyone;
    // the worker owned its own.
    let _ = &mut client;
}

/// The wake retry outranks the webhook route. A hapi seat that also
/// holds `webhook.url` (hapi wins while bound) and was slept loses its
/// binding to `start_down`'s pop; the popped seat would classify as
/// webhook, but mail must take the wake, not the hook: one `--once`
/// scan wakes it and delivers, and the hook is never rung.
#[test]
fn the_scan_wakes_an_asleep_seat_that_also_holds_a_webhook_url() {
    let root = board_with(&[]); // no bindings file at all
    verbs::cmd_register(&root, "opus", "sb", None, None, Vec::new(), None).unwrap();
    // A leftover hook file must not capture the wake retry.
    std::fs::write(
        seat::seat_dir(&root, "opus").join("webhook.url"),
        "https://127.0.0.1:9/hook\n",
    )
    .unwrap();
    let kit_dir = stub_kit(
        "scan-wake-hook",
        Some(r#"{"seats":{"opus":{"role":"opus"}}}"#),
        Some(&wake_success(&root, "opus", "opus", "s9")),
    );
    let hub = FakeHub::start();
    hub.ctl.lock().unwrap().sessions.insert("s9".into(), true);
    let mut client = Hub::new(hub.config());
    let _env = lock_env(&[
        ("PATH", path_with(&kit_dir)),
        ("SEAT_KIT", OsString::from("")),
        ("SEAT_TMUX", OsString::from(fake_tmux())),
        (
            "SEAT_FAKE_STATE",
            OsString::from(root.join("tmux.state").display().to_string()),
        ),
        ("HAPI_API_URL", OsString::from(hub.config().base)),
        ("CLI_API_TOKEN", OsString::from("test-token")),
    ]);
    let dir = seat::seat_dir(&root, "opus");
    dstate::store_state(
        &dir,
        &dstate::DispatchState {
            asleep: Some(dstate::Asleep {
                at: "2026-10-23T00:00:00Z".into(),
                reason: "idle 30m".into(),
            }),
            ..Default::default()
        },
    )
    .unwrap();
    send(&root, "opus", "find your seat");
    let key = only_key(&root, "opus");
    mark(&hub, &key, "invoked");

    let stop = Arc::new(AtomicBool::new(false));
    dispatch::run(
        &root,
        true,
        &stop,
        Waits {
            first: Duration::from_millis(5),
            max: Duration::from_millis(5),
        },
    )
    .unwrap();
    let state = dstate::load_state(&dir);
    assert_eq!(state.asleep, None, "woken by the scan, not rung");
    assert_eq!(state.woken.map(|w| w.key), Some(key.clone()));
    assert_eq!(n_files(&root, "opus", "archive"), 1);
    assert!(hub.journal().contains(&format!("POST-MSG s9 {key}")));
    let log = log_text(&root);
    assert!(
        !log.contains("webhook failed"),
        "the hook was never rung: {log}"
    );
    assert!(
        !dir.join("webhook-attempted.json").exists(),
        "no ring record was written"
    );
    let _ = &mut client;
}

/// A sleep whose kill never landed — `try_sleep` records the sleep
/// before it stops the pane, and the process died between the two —
/// is finished by the next pass: the pane goes down, the record
/// stands, so mail still takes the wake path. A pane that came back
/// after a completed sleep (no pane fact beside the mark) ends the
/// sleep: the mark clears and that pane starts a fresh clock.
#[test]
fn a_pass_finishes_a_sleep_whose_kill_never_landed() {
    let kit_dir = stub_kit(
        "sleep-finish",
        Some(r#"{"seats":{"opus":{"role":"opus"}}}"#),
        None,
    );
    let _env = lock_env(&[
        ("PATH", path_with(&kit_dir)),
        ("SEAT_KIT", OsString::from("")),
        ("SEAT_TMUX", OsString::from(fake_tmux())),
    ]);
    let (root, _hub, mut client) = up_and_seen("opus", "s1", 31, QUIET);
    let dir = seat::seat_dir(&root, "opus");
    // Exactly what the record store leaves: the mark beside a standing
    // pane fact, the pane still up, the inbox empty.
    let mut state = dstate::load_state(&dir);
    state.asleep = Some(dstate::Asleep {
        at: seat::now_ts(),
        reason: "idle 30m".into(),
    });
    dstate::store_state(&dir, &state).unwrap();
    assert!(
        crate::session::running_seats(&fake_tmux())
            .unwrap()
            .contains("opus"),
        "the pane never went down"
    );

    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Idle
    );
    let state = dstate::load_state(&dir);
    assert!(state.asleep.is_some(), "the record stands");
    assert_eq!(state.pane_seen_at, None, "the pane fact is cleared");
    assert!(
        !crate::session::running_seats(&fake_tmux())
            .unwrap()
            .contains("opus"),
        "the kill was finished"
    );
    assert!(
        log_text(&root).contains("sleep seat=opus") && log_text(&root).contains("finished=1"),
        "the finish is logged"
    );

    // A pane that came back after a completed sleep has no pane fact:
    // the early return ends the sleep — the mark clears and the
    // returned pane starts a fresh clock (presence prints `pane`).
    crate::session::cmd_up(&root, &["opus".to_string()]).unwrap();
    let back = crate::session::live_pane_id("opus")
        .expect("the fake multiplexer answers")
        .expect("the pane is up");
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Idle
    );
    let state = dstate::load_state(&dir);
    assert_eq!(state.asleep, None, "the returned pane ends the sleep");
    assert_eq!(
        state.pane_id,
        Some(back.clone()),
        "the clock times the returned pane"
    );
    let seen = chrono::DateTime::parse_from_rfc3339(state.pane_seen_at.as_deref().unwrap())
        .expect("the fresh timestamp parses");
    assert!(
        chrono::Utc::now().signed_duration_since(seen) < chrono::TimeDelta::try_minutes(1).unwrap(),
        "the returned pane's clock started over"
    );
    assert!(
        crate::session::running_seats(&fake_tmux())
            .unwrap()
            .contains("opus"),
        "a pane that came back is never killed"
    );
    assert_eq!(log_text(&root).matches("sleep seat=opus").count(), 1);
}

/// Mail to a seat whose sleep was recorded but never killed cancels
/// the pending kill — a last look at the owed set immediately before
/// the kill — and delivers in the same pass: the pane the mark named
/// stands, no wake runs, and the binding never moves.
#[test]
fn mail_to_an_unfinished_sleep_cancels_the_kill_and_delivers() {
    let root = board_with(&[("opus", "s1")]); // s1: the binding sleep left behind
    let kit_dir = stub_kit(
        "wake-finish",
        Some(r#"{"seats":{"opus":{"role":"opus"}}}"#),
        // Installed to prove it is never called: a wake would move the
        // binding to s2 and the delivery with it.
        Some(&wake_success(&root, "opus", "opus", "s2")),
    );
    let hub = FakeHub::start();
    hub.ctl
        .lock()
        .unwrap()
        .get_bodies
        .insert("s1".into(), QUIET.into());
    hub.ctl.lock().unwrap().sessions.insert("s1".into(), true);
    let mut client = Hub::new(hub.config());
    let _env = lock_env(&[
        ("PATH", path_with(&kit_dir)),
        ("SEAT_KIT", OsString::from("")),
        ("SEAT_TMUX", OsString::from(fake_tmux())),
        (
            "SEAT_FAKE_STATE",
            OsString::from(root.join("tmux.state").display().to_string()),
        ),
    ]);
    verbs::cmd_register(
        &root,
        "opus",
        "sb",
        None,
        Some(root.display().to_string()),
        vec!["/bin/sleep".into(), "30".into()],
        Some("30m".into()),
    )
    .unwrap();
    crate::session::cmd_up(&root, &["opus".to_string()]).unwrap();
    let pane_id = crate::session::live_pane_id("opus")
        .expect("the fake multiplexer answers")
        .expect("the pane is up");
    let dir = seat::seat_dir(&root, "opus");
    // The record store's state: the mark beside the standing pane fact
    // naming the pane the clock timed.
    dstate::store_state(
        &dir,
        &dstate::DispatchState {
            asleep: Some(dstate::Asleep {
                at: "2026-10-23T00:00:00Z".into(),
                reason: "idle 30m".into(),
            }),
            pane_seen_at: Some("2026-10-23T00:00:00Z".into()),
            pane_id: Some(pane_id),
            ..Default::default()
        },
    )
    .unwrap();
    send(&root, "opus", "wake up");
    let key = only_key(&root, "opus");
    mark(&hub, &key, "invoked");

    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Delivered,
        "cancel and delivery share one pass"
    );
    let state = dstate::load_state(&dir);
    assert_eq!(state.asleep, None, "the pending sleep is cancelled");
    assert_eq!(state.woken, None, "no wake ever ran");
    assert!(
        crate::session::running_seats(&fake_tmux())
            .unwrap()
            .contains("opus"),
        "the pane the mark named was never killed"
    );
    let log = log_text(&root);
    assert!(
        log.contains("cancelled=1 cause=mail-present")
            && !log.contains("finished=1")
            && !log.contains("wake seat=opus"),
        "the kill was cancelled before it landed: {log}"
    );
    assert_eq!(n_files(&root, "opus", "archive"), 1);
    let journal = hub.journal();
    assert!(
        journal.contains(&format!("POST-MSG s1 {key}")),
        "delivered on the standing binding: {journal:?}"
    );
    assert!(
        !journal.iter().any(|l| l.starts_with("POST-MSG s2")),
        "the wake stub never rewrote the binding: {journal:?}"
    );
}

/// The idle clock belongs to one pane: when the live pane is not the
/// pane the fact timed — the boot race, `seat up`, the gallery, or a
/// down-and-up that landed inside one 2 s scan — the clock starts
/// over, so a pane that just started is never killed on the previous
/// pane's old timestamp.
#[test]
fn a_new_pane_resets_the_idle_clock() {
    let kit_dir = stub_kit(
        "sleep-new-pane",
        Some(r#"{"seats":{"opus":{"role":"opus"}}}"#),
        None,
    );
    let _env = lock_env(&[
        ("PATH", path_with(&kit_dir)),
        ("SEAT_KIT", OsString::from("")),
        ("SEAT_TMUX", OsString::from(fake_tmux())),
    ]);
    let (root, _hub, mut client) = up_and_seen("opus", "s1", 31, QUIET);
    let dir = seat::seat_dir(&root, "opus");
    // The fact names a pane that is no longer the live one, with an
    // old timestamp: before the fix this pass would sleep the seat.
    let mut state = dstate::load_state(&dir);
    state.pane_id = Some("%0".into());
    dstate::store_state(&dir, &state).unwrap();
    let live = crate::session::live_pane_id("opus")
        .expect("the fake multiplexer answers")
        .expect("the pane is up");
    assert_ne!(state.pane_id, Some(live.clone()));

    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Idle
    );
    let state = dstate::load_state(&dir);
    assert!(
        state.asleep.is_none(),
        "the new pane is never slept on the old clock"
    );
    assert_eq!(
        state.pane_id,
        Some(live),
        "the fact times the live pane now"
    );
    let seen = chrono::DateTime::parse_from_rfc3339(state.pane_seen_at.as_deref().unwrap())
        .expect("the fresh timestamp parses");
    assert!(
        chrono::Utc::now().signed_duration_since(seen) < chrono::TimeDelta::try_minutes(1).unwrap(),
        "the clock started over"
    );
    assert!(
        crate::session::running_seats(&fake_tmux())
            .unwrap()
            .contains("opus"),
        "the pane stands"
    );
    assert!(!log_text(&root).contains("sleep seat="));
}

/// Recovery of a pending sleep never kills a pane the decision did
/// not name: the dispatcher died between recording the sleep and the
/// kill, the timed pane is gone, and a replacement (`seat up`, the
/// gallery) stands in its place — the sleep is cancelled, the
/// replacement keeps a fresh clock, and its pane stands.
#[test]
fn a_pending_sleep_never_kills_a_replacement_pane() {
    let kit_dir = stub_kit(
        "sleep-replaced",
        Some(r#"{"seats":{"opus":{"role":"opus"}}}"#),
        None,
    );
    let _env = lock_env(&[
        ("PATH", path_with(&kit_dir)),
        ("SEAT_KIT", OsString::from("")),
        ("SEAT_TMUX", OsString::from(fake_tmux())),
    ]);
    let (root, _hub, mut client) = up_and_seen("opus", "s1", 31, QUIET);
    // A second seat's pane keeps the id counter above opus's, so the
    // replacement really is a different pane id.
    verbs::cmd_register(
        &root,
        "stay",
        "sb",
        None,
        Some(root.display().to_string()),
        vec!["/bin/sleep".into(), "30".into()],
        None,
    )
    .unwrap();
    crate::session::cmd_up(&root, &["stay".to_string()]).unwrap();
    let dir = seat::seat_dir(&root, "opus");
    let mut state = dstate::load_state(&dir);
    let old_pane = state.pane_id.clone().unwrap();
    state.asleep = Some(dstate::Asleep {
        at: seat::now_ts(),
        reason: "idle 30m".into(),
    });
    dstate::store_state(&dir, &state).unwrap();
    // Down and up land between scans: the next pass finds a new pane
    // beside the mark.
    crate::session::cmd_down(&root, &["opus".to_string()]).unwrap();
    crate::session::cmd_up(&root, &["opus".to_string()]).unwrap();
    let new_pane = crate::session::live_pane_id("opus")
        .expect("the fake multiplexer answers")
        .expect("the pane is up");
    assert_ne!(old_pane, new_pane, "the replacement is a different pane");

    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Idle
    );
    let state = dstate::load_state(&dir);
    assert!(
        state.asleep.is_none(),
        "the sleep is cancelled, not finished"
    );
    assert_eq!(
        state.pane_id,
        Some(new_pane),
        "the replacement starts a fresh clock"
    );
    let seen = chrono::DateTime::parse_from_rfc3339(state.pane_seen_at.as_deref().unwrap())
        .expect("the fresh timestamp parses");
    assert!(
        chrono::Utc::now().signed_duration_since(seen) < chrono::TimeDelta::try_minutes(1).unwrap(),
        "the replacement's clock starts now"
    );
    assert!(
        crate::session::running_seats(&fake_tmux())
            .unwrap()
            .contains("opus"),
        "the replacement pane was never killed"
    );
    let log = log_text(&root);
    assert!(
        log.contains("cancelled=1 cause=pane-replaced")
            && !log.contains("finished=1")
            && !log.contains("pane_killed=true"),
        "{log}"
    );
}

/// A replacement that lands between recovery's own pane query and its
/// kill — the query verified the pane the mark names, the kill finds
/// another live pane under the seat — refuses the kill, and the
/// re-query cancels the sleep exactly as the first check would: the
/// mark and the fact clear, nothing dies, and mail that lands beside
/// the replacement delivers on the standing binding instead of
/// running the wake path against a live pane. The swap is driven by a
/// wrapper around the fake multiplexer: right after the first
/// `list-panes` it starts a second pane for the seat and kills the
/// one the mark named.
#[test]
fn a_replacement_between_recovery_pane_queries_cancels_the_sleep() {
    let kit_dir = stub_kit(
        "sleep-mid-swap",
        Some(r#"{"seats":{"opus":{"role":"opus"}}}"#),
        None,
    );
    let _env = lock_env(&[
        ("PATH", path_with(&kit_dir)),
        ("SEAT_KIT", OsString::from("")),
        ("SEAT_TMUX", OsString::from(fake_tmux())),
    ]);
    let (root, hub, mut client) = up_and_seen("opus", "s1", 31, QUIET);
    let dir = seat::seat_dir(&root, "opus");
    let mut state = dstate::load_state(&dir);
    let old_pane = state.pane_id.clone().unwrap();
    state.asleep = Some(dstate::Asleep {
        at: seat::now_ts(),
        reason: "idle 30m".into(),
    });
    dstate::store_state(&dir, &state).unwrap();

    // The wrapper delegates every call to the fake multiplexer and,
    // right after the first `list-panes` (recovery's pane query),
    // swaps the pane under the seat: a replacement starts and the
    // pane the mark named dies, so the kill's own query — after the
    // binding, session, and mail checks — finds the replacement.
    let fake = fake_tmux();
    let script = r#"
n=0
if [ "$1" = "list-panes" ]; then
    n=$(( $(cat 'COUNT' 2>/dev/null || echo 0) + 1 ))
    echo "$n" > 'COUNT'
fi
'FAKE' "$@"
status=$?
if [ "$1" = "list-panes" ] && [ "$n" -eq 1 ]; then
    new=$('FAKE' new-window -d -t swarm -n opus -P -F '#{pane_id}' -- /usr/bin/env -- /bin/sleep 30)
    'FAKE' set-option -p -t "$new" @seat opus
    'FAKE' kill-pane -t 'OLD'
fi
exit $status
"#
    .replace("FAKE", &fake)
    .replace("COUNT", &root.join("swap.count").display().to_string())
    .replace("OLD", &old_pane);
    stub(&root, "swap-tmux.sh", &script);
    std::env::set_var("SEAT_TMUX", root.join("swap-tmux.sh"));

    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Idle
    );
    let state = dstate::load_state(&dir);
    assert!(state.asleep.is_none(), "the refused kill cancels the sleep");
    let new_pane = crate::session::live_pane_id("opus")
        .expect("the fake multiplexer answers")
        .expect("the pane is up");
    assert_ne!(old_pane, new_pane, "the replacement is a different pane");
    assert_eq!(
        state.pane_id,
        Some(new_pane),
        "the replacement starts a fresh clock"
    );
    assert!(
        crate::session::running_seats(&fake)
            .unwrap()
            .contains("opus"),
        "the replacement pane was never killed"
    );
    let log = log_text(&root);
    assert!(
        log.contains("cancelled=1 cause=pane-replaced")
            && !log.contains("finished=1")
            && !log.contains("pane_killed=true"),
        "{log}"
    );

    // Mail beside the replacement delivers on the standing binding:
    // the cancelled sleep never runs the wake path.
    send(&root, "opus", "deliver me directly");
    let key = only_key(&root, "opus");
    mark(&hub, &key, "invoked");
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Delivered,
        "the mail delivers without a wake"
    );
    assert_eq!(n_files(&root, "opus", "archive"), 1);
    assert!(
        !log_text(&root).contains("wake"),
        "no wake ran against the live replacement: {}",
        log_text(&root)
    );
    assert!(
        hub.journal().contains(&format!("POST-MSG s1 {key}")),
        "delivered on the standing binding: {:?}",
        hub.journal()
    );
}

/// A pending sleep whose pane died on its own — a manual `down`, a
/// kill that half-landed — is completed by the pass that asks and
/// finds no live pane: only the fact clears, the mark stands, and the
/// settlement names the case it saw.
#[test]
fn a_pending_sleep_whose_pane_already_died_completes() {
    let kit_dir = stub_kit(
        "sleep-already-gone",
        Some(r#"{"seats":{"opus":{"role":"opus"}}}"#),
        None,
    );
    let _env = lock_env(&[
        ("PATH", path_with(&kit_dir)),
        ("SEAT_KIT", OsString::from("")),
        ("SEAT_TMUX", OsString::from(fake_tmux())),
    ]);
    let (root, _hub, mut client) = up_and_seen("opus", "s1", 31, QUIET);
    let dir = seat::seat_dir(&root, "opus");
    let mut state = dstate::load_state(&dir);
    state.asleep = Some(dstate::Asleep {
        at: seat::now_ts(),
        reason: "idle 30m".into(),
    });
    dstate::store_state(&dir, &state).unwrap();
    // The pane dies without the dispatcher (a manual `seat down`).
    crate::session::cmd_down(&root, &["opus".to_string()]).unwrap();

    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Idle
    );
    let state = dstate::load_state(&dir);
    assert!(state.asleep.is_some(), "the record stands");
    assert_eq!(state.pane_seen_at, None, "the pane fact is cleared");
    let log = log_text(&root);
    assert!(
        log.contains("finished=1 cause=pane-gone") && !log.contains("pane_killed="),
        "the already-gone completion is logged: {log}"
    );
}

/// A multiplexer that cannot be asked settles nothing: the mark, the
/// pane fact, and the clock all stand untouched for a retry on the
/// next pass, and the settlement names the case it saw.
#[test]
fn a_pending_sleep_survives_an_unanswerable_multiplexer() {
    let kit_dir = stub_kit(
        "sleep-unreadable-tmux",
        Some(r#"{"seats":{"opus":{"role":"opus"}}}"#),
        None,
    );
    let _env = lock_env(&[
        ("PATH", path_with(&kit_dir)),
        ("SEAT_KIT", OsString::from("")),
        ("SEAT_TMUX", OsString::from(fake_tmux())),
    ]);
    let (root, _hub, mut client) = up_and_seen("opus", "s1", 31, QUIET);
    let dir = seat::seat_dir(&root, "opus");
    let before = {
        let mut state = dstate::load_state(&dir);
        state.asleep = Some(dstate::Asleep {
            at: "2026-10-23T00:00:00Z".into(),
            reason: "idle 30m".into(),
        });
        dstate::store_state(&dir, &state).unwrap();
        state
    };
    // The multiplexer goes away between passes (the socket died).
    std::env::set_var("SEAT_TMUX", root.join("no-such-multiplexer"));

    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Idle
    );
    let state = dstate::load_state(&dir);
    assert_eq!(state.asleep, before.asleep, "the mark stands");
    assert_eq!(state.pane_seen_at, before.pane_seen_at, "the clock stands");
    assert_eq!(state.pane_id, before.pane_id, "the pane fact stands");
    assert!(
        crate::session::running_seats(&fake_tmux())
            .unwrap()
            .contains("opus"),
        "the pane was never asked to die"
    );
    let log = log_text(&root);
    assert!(
        log.contains("pending=1 cause=pane-unknown")
            && !log.contains("finished=1")
            && !log.contains("cancelled=1"),
        "the unanswerable multiplexer is named: {log}"
    );
}

/// Recovery of a pending sleep re-reads the one gate that can have
/// flipped since the record: the same pane on a session that resumed
/// work is never killed — the sleep is cancelled and the pane stands.
#[test]
fn a_pending_sleep_on_a_session_that_resumed_work_is_cancelled() {
    let kit_dir = stub_kit(
        "sleep-resumed",
        Some(r#"{"seats":{"opus":{"role":"opus"}}}"#),
        None,
    );
    let _env = lock_env(&[
        ("PATH", path_with(&kit_dir)),
        ("SEAT_KIT", OsString::from("")),
        ("SEAT_TMUX", OsString::from(fake_tmux())),
    ]);
    let (root, hub, mut client) = up_and_seen(
        "opus",
        "s1",
        31,
        r#"{"session":{"active":true,"thinking":true,"thinkingAt":60000}}"#,
    );
    let dir = seat::seat_dir(&root, "opus");
    let mut state = dstate::load_state(&dir);
    state.asleep = Some(dstate::Asleep {
        at: seat::now_ts(),
        reason: "idle 30m".into(),
    });
    dstate::store_state(&dir, &state).unwrap();
    assert!(
        crate::session::running_seats(&fake_tmux())
            .unwrap()
            .contains("opus"),
        "the pane never went down"
    );

    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Idle
    );
    let state = dstate::load_state(&dir);
    assert!(state.asleep.is_none(), "working: the sleep is cancelled");
    assert!(
        crate::session::running_seats(&fake_tmux())
            .unwrap()
            .contains("opus"),
        "the resumed pane was never killed"
    );
    assert!(
        hub.journal().contains(&"GET /api/sessions/s1".to_string()),
        "recovery re-read the session before deciding"
    );
    let log = log_text(&root);
    assert!(
        log.contains("cancelled=1 cause=session-working") && !log.contains("finished=1"),
        "{log}"
    );
}

/// The kill names the pane the clock timed, not the first swarm pane
/// listed for the seat: a dead-but-listed pane never shields the live
/// one — the sleep kills the timed pane, and `cmd_down` (no `only_if`)
/// still kills by seat name, dead-but-listed included.
#[test]
fn the_sleep_kills_the_pane_the_mark_names_not_the_first_listed() {
    let kit_dir = stub_kit(
        "sleep-named-pane",
        Some(r#"{"seats":{"opus":{"role":"opus"}}}"#),
        None,
    );
    let _env = lock_env(&[
        ("PATH", path_with(&kit_dir)),
        ("SEAT_KIT", OsString::from("")),
        ("SEAT_TMUX", OsString::from(fake_tmux())),
    ]);
    let (root, hub, mut client) = kit(&[("opus", "s1")], &[]);
    let home = root.join("role-home");
    std::fs::create_dir_all(home.join(".local").join("share")).unwrap();
    std::os::unix::fs::symlink(&root, home.join(".local").join("share").join("seat")).unwrap();
    std::env::set_var("SEAT_ROLE_HOME", &home);
    hub.ctl
        .lock()
        .unwrap()
        .get_bodies
        .insert("s1".into(), QUIET.into());
    // A launch whose command dies at once leaves a dead-but-listed
    // pane tagged for the seat (`up` reports the failure; the pane
    // stays listed).
    verbs::cmd_register(
        &root,
        "opus",
        "sb",
        None,
        Some(root.display().to_string()),
        vec!["seat-die".into()],
        Some("30m".into()),
    )
    .unwrap();
    std::env::set_var("SEAT_FAKE_STATE", root.join("tmux.state"));
    assert!(crate::session::cmd_up(&root, &["opus".to_string()]).is_err());
    // The real launch joins it under the same seat name, live.
    verbs::cmd_register(
        &root,
        "opus",
        "sb",
        None,
        Some(root.display().to_string()),
        vec!["/bin/sleep".into(), "30".into()],
        Some("30m".into()),
    )
    .unwrap();
    crate::session::cmd_up(&root, &["opus".to_string()]).unwrap();
    let live = crate::session::live_pane_id("opus")
        .expect("the fake multiplexer answers")
        .expect("the pane is up");
    let listed = crate::session::panes(&fake_tmux()).unwrap();
    assert!(
        listed.iter().any(|p| p.seat == "opus" && p.dead),
        "a dead pane is listed first for the seat"
    );
    let dir = seat::seat_dir(&root, "opus");
    let mut state = dstate::load_state(&dir);
    state.pane_seen_at = Some(
        (chrono::Utc::now() - chrono::TimeDelta::try_minutes(31).unwrap()).to_rfc3339(),
    );
    state.pane_id = Some(live);
    dstate::store_state(&dir, &state).unwrap();

    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Idle
    );
    let state = dstate::load_state(&dir);
    assert!(state.asleep.is_some(), "the seat slept");
    assert_eq!(state.pane_seen_at, None, "the pane fact is cleared");
    assert!(
        !crate::session::running_seats(&fake_tmux())
            .unwrap()
            .contains("opus"),
        "the live pane the clock timed was killed, not shielded"
    );
    let log = log_text(&root);
    assert!(
        log.contains("sleep seat=opus") && log.contains("pane_killed=true"),
        "the kill landed: {log}"
    );

    // `cmd_down` keeps killing by seat name, dead-but-listed included:
    // the leftover dead listing dies with it.
    crate::session::cmd_down(&root, &["opus".to_string()]).unwrap();
    let after: Vec<String> = crate::session::panes(&fake_tmux())
        .unwrap()
        .into_iter()
        .filter(|p| p.seat == "opus")
        .map(|p| p.id)
        .collect();
    assert!(after.is_empty(), "down took the dead listing too: {after:?}");
}

/// When a different live pane has taken the seat at kill time — the
/// pane the clock timed is gone and another runs under the same name —
/// the sleep `try_sleep` just recorded is cancelled: the mark it
/// stored clears, nothing is killed, and the pane stands. The pass's
/// own pane-fact check would have reset the clock; this is the swap
/// that lands inside one pass, so the case is driven directly.
#[test]
fn a_sleep_is_cancelled_when_a_different_live_pane_remains() {
    let kit_dir = stub_kit(
        "sleep-other-live-pane",
        Some(r#"{"seats":{"opus":{"role":"opus"}}}"#),
        None,
    );
    let _env = lock_env(&[
        ("PATH", path_with(&kit_dir)),
        ("SEAT_KIT", OsString::from("")),
        ("SEAT_TMUX", OsString::from(fake_tmux())),
    ]);
    let (root, _hub, mut client) = up_and_seen("opus", "s1", 31, QUIET);
    let dir = seat::seat_dir(&root, "opus");
    // The fact names a pane that is no longer live; another live pane
    // holds the seat.
    let mut state = dstate::load_state(&dir);
    state.pane_id = Some("%0".into());
    dstate::store_state(&dir, &state).unwrap();
    let live = crate::session::live_pane_id("opus")
        .expect("the fake multiplexer answers")
        .expect("the pane is up");
    assert_ne!(state.pane_id, Some(live));

    let mut st = dstate::load_state(&dir);
    crate::sleep::try_sleep(&root, "opus", "s1", &mut st, &mut client).unwrap();
    assert_eq!(st.asleep, None, "the mark it just stored is cleared");
    assert_eq!(st.pane_seen_at, None, "the fact clears with it");
    assert!(
        crate::session::running_seats(&fake_tmux())
            .unwrap()
            .contains("opus"),
        "nothing was killed"
    );
    assert!(
        log_text(&root).contains("cancelled=1 cause=pane-replaced"),
        "the cancel is logged: {}",
        log_text(&root)
    );
}

/// The mail re-check guards the initial sleep's record too, and it
/// reads the pass's whole owed set: mail in `inbox/` or already in
/// `draining/` stops the store — no mark, no kill, the pane stands.
#[test]
fn a_sleep_re_checks_owed_mail_before_storing_the_mark() {
    let kit_dir = stub_kit(
        "sleep-mail-recheck",
        Some(r#"{"seats":{"opus":{"role":"opus"}}}"#),
        None,
    );
    let _env = lock_env(&[
        ("PATH", path_with(&kit_dir)),
        ("SEAT_KIT", OsString::from("")),
        ("SEAT_TMUX", OsString::from(fake_tmux())),
    ]);
    let (root, _hub, mut client) = up_and_seen("opus", "s1", 31, QUIET);
    let dir = seat::seat_dir(&root, "opus");

    // Mail in the inbox: no sleep.
    send(&root, "opus", "landed during the pass");
    let mut st = dstate::load_state(&dir);
    crate::sleep::try_sleep(&root, "opus", "s1", &mut st, &mut client).unwrap();
    assert_eq!(st.asleep, None, "inbox mail stops the sleep");
    assert!(
        crate::session::running_seats(&fake_tmux())
            .unwrap()
            .contains("opus"),
        "the pane stands"
    );

    // The same file already in draining: still owed, still no sleep.
    let inbox = dir.join("inbox");
    let file = msg::list_msg_dir(&inbox).unwrap()[0].clone();
    let draining = dir.join("draining");
    std::fs::create_dir_all(&draining).unwrap();
    std::fs::rename(&file, draining.join(file.file_name().unwrap())).unwrap();
    let mut st = dstate::load_state(&dir);
    crate::sleep::try_sleep(&root, "opus", "s1", &mut st, &mut client).unwrap();
    assert_eq!(st.asleep, None, "draining mail stops the sleep too");
    assert!(
        crate::session::running_seats(&fake_tmux())
            .unwrap()
            .contains("opus"),
        "the pane still stands"
    );
    assert!(
        !log_text(&root).contains("sleep seat="),
        "no sleep was recorded: {}",
        log_text(&root)
    );
}

/// A GET error while settling a pending sleep is no decision about
/// the session — but owed mail still is: mail that arrives while the
/// GET still errors cancels the mark without waiting for the hub (no
/// wake runs, the pane the kill never reached stands), and only the
/// delivery itself waits for the hub to answer. With nothing owed the
/// mark, the pane fact, and the clock all stay untouched for a retry
/// on the next pass (the threshold never restarts), and the log names
/// the case; the recovered pass then delivers the staged mail.
#[test]
fn a_pending_sleep_survives_a_get_error_and_cancels_on_mail() {
    let kit_dir = stub_kit(
        "sleep-get-error",
        Some(r#"{"seats":{"opus":{"role":"opus"}}}"#),
        None,
    );
    let _env = lock_env(&[
        ("PATH", path_with(&kit_dir)),
        ("SEAT_KIT", OsString::from("")),
        ("SEAT_TMUX", OsString::from(fake_tmux())),
    ]);
    let (root, hub, mut client) = up_and_seen("opus", "s1", 31, QUIET);
    let dir = seat::seat_dir(&root, "opus");
    let before = dstate::load_state(&dir);
    let mut pending = before.clone();
    pending.asleep = Some(dstate::Asleep {
        at: "2026-10-23T00:00:00Z".into(),
        reason: "idle 30m".into(),
    });
    dstate::store_state(&dir, &pending).unwrap();
    hub.ctl.lock().unwrap().fail_get.insert("s1".into());

    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Idle
    );
    let state = dstate::load_state(&dir);
    assert!(
        state.asleep.is_some(),
        "a GET error never cancels the sleep"
    );
    assert_eq!(state.asleep.unwrap().at, "2026-10-23T00:00:00Z");
    assert_eq!(
        state.pane_seen_at, before.pane_seen_at,
        "the clock never restarted"
    );
    assert_eq!(state.pane_id, before.pane_id, "the pane fact stands");
    assert!(
        crate::session::running_seats(&fake_tmux())
            .unwrap()
            .contains("opus"),
        "nothing was killed"
    );
    let log = log_text(&root);
    assert!(
        log.contains("pending=1 cause=get-error")
            && !log.contains("cancelled=1")
            && !log.contains("finished=1"),
        "the unreadable answer is named: {log}"
    );

    // Mail while the GET still errors: owed mail is a decided miss
    // whatever the unreadable session said, so the mark cancels now —
    // no wake runs and the pane the kill never reached stands. The
    // delivery gate's own GET is the one the hub is failing, so the
    // pass surfaces that error with the mail staged.
    send(&root, "opus", "while the hub is unreadable");
    let key = only_key(&root, "opus");
    mark(&hub, &key, "invoked");
    assert!(
        dispatch::pass_seat(&root, "opus", &mut client).is_err(),
        "the delivery gate's own GET fails while the hub errors"
    );
    let state = dstate::load_state(&dir);
    assert_eq!(
        state.asleep, None,
        "owed mail cancels the sleep even through a GET error"
    );
    assert!(
        crate::session::running_seats(&fake_tmux())
            .unwrap()
            .contains("opus"),
        "the pane the mark named was never killed"
    );
    let log = log_text(&root);
    assert!(
        log.contains("cancelled=1 cause=mail-present")
            && !log.contains("wake seat=opus")
            && !log.contains("wake failed"),
        "mail cancels through the unreadable session without a wake: {log}"
    );

    // The hub answers again: the staged mail delivers on the pass
    // that recovers.
    hub.ctl.lock().unwrap().fail_get.remove("s1");
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Delivered,
        "delivery shares the recovered pass"
    );
    let state = dstate::load_state(&dir);
    assert_eq!(state.asleep, None, "the mail cancelled the sleep");
    assert_eq!(n_files(&root, "opus", "archive"), 1);
    assert!(
        crate::session::running_seats(&fake_tmux())
            .unwrap()
            .contains("opus"),
        "the pane was never killed"
    );
    assert!(
        log_text(&root).contains("cancelled=1 cause=mail-present"),
        "{}",
        log_text(&root)
    );
}

/// Recovery's decided misses: a session that no longer reads quiet —
/// a 404, the flag gone with the session — and a binding that
/// disappeared each cancel the pending sleep, clear the pane fact
/// with the mark (so the clock restarts), and leave the pane standing.
#[test]
fn a_pending_sleep_on_a_missing_session_or_binding_cancels() {
    // A 404 on the session GET: `thinking` cannot read exactly false.
    {
        let kit_dir = stub_kit(
            "sleep-404",
            Some(r#"{"seats":{"opus":{"role":"opus"}}}"#),
            None,
        );
        let _env = lock_env(&[
            ("PATH", path_with(&kit_dir)),
            ("SEAT_KIT", OsString::from("")),
            ("SEAT_TMUX", OsString::from(fake_tmux())),
        ]);
        let (root, hub, mut client) = up_and_seen("opus", "s1", 31, QUIET);
        hub.ctl.lock().unwrap().get_bodies.remove("s1"); // no body, no session: 404
        let dir = seat::seat_dir(&root, "opus");
        let mut state = dstate::load_state(&dir);
        state.asleep = Some(dstate::Asleep {
            at: "2026-10-23T00:00:00Z".into(),
            reason: "idle 30m".into(),
        });
        dstate::store_state(&dir, &state).unwrap();
        dispatch::pass_seat(&root, "opus", &mut client).unwrap();
        let state = dstate::load_state(&dir);
        assert_eq!(state.asleep, None, "a 404 is a decided miss");
        let seen = chrono::DateTime::parse_from_rfc3339(state.pane_seen_at.as_deref().unwrap())
            .expect("the fresh timestamp parses");
        assert!(
            chrono::Utc::now().signed_duration_since(seen)
                < chrono::TimeDelta::try_minutes(31).unwrap(),
            "the standing pane's clock restarted"
        );
        assert!(
            crate::session::running_seats(&fake_tmux())
                .unwrap()
                .contains("opus"),
            "the pane stands"
        );
        assert!(
            log_text(&root).contains("cancelled=1 cause=session-working"),
            "{}",
            log_text(&root)
        );
    }
    // No binding at all: there is nothing quiet to re-read.
    {
        let kit_dir = stub_kit(
            "sleep-no-binding",
            Some(r#"{"seats":{"opus":{"role":"opus"}}}"#),
            None,
        );
        let _env = lock_env(&[
            ("PATH", path_with(&kit_dir)),
            ("SEAT_KIT", OsString::from("")),
            ("SEAT_TMUX", OsString::from(fake_tmux())),
        ]);
        let (root, _hub, mut client) = up_and_seen("opus", "s1", 31, QUIET);
        crate::testutil::write_bindings(&root, &[]);
        let dir = seat::seat_dir(&root, "opus");
        let mut state = dstate::load_state(&dir);
        state.asleep = Some(dstate::Asleep {
            at: "2026-10-23T00:00:00Z".into(),
            reason: "idle 30m".into(),
        });
        dstate::store_state(&dir, &state).unwrap();
        dispatch::pass_seat(&root, "opus", &mut client).unwrap();
        let state = dstate::load_state(&dir);
        assert_eq!(state.asleep, None, "no binding is a decided miss");
        assert_eq!(state.pane_seen_at, None, "the fact clears with the mark");
        assert!(
            crate::session::running_seats(&fake_tmux())
                .unwrap()
                .contains("opus"),
            "the pane stands"
        );
        assert!(
            log_text(&root).contains("cancelled=1 cause=no-binding"),
            "{}",
            log_text(&root)
        );
    }
}

/// The script's `up` answer is a hold, not a retry: a pane runs with
/// no phone session, so the script is not called again while that
/// pane stands (it answers `up` precisely so the caller stops); once
/// the pane is verifiably gone the wake runs again and delivers.
#[test]
fn an_up_answer_is_held_not_retried_while_the_pane_stands() {
    let root = board_with(&[]); // the wake stub writes the binding
    verbs::cmd_register(
        &root,
        "opus",
        "sb",
        None,
        Some(root.display().to_string()),
        vec!["/bin/sleep".into(), "30".into()],
        None,
    )
    .unwrap();
    let counter = root.join("wake-calls");
    let body = r#"
[ "$1" = "wake" ] && [ "$2" = "opus" ] || { echo "stub: expected wake opus, got $*" >&2; exit 9; }
n=$(cat 'COUNT' 2>/dev/null || echo 0)
n=$((n+1))
echo "$n" > 'COUNT'
if [ "$n" -ge 2 ]; then
  printf '%s\n' '{"schema_version":1,"seats":{"opus":{"url":"https://hapi.bll.dev/sessions/s2","moved_at":"2026-10-23T12:00:00Z"}}}' > 'ROOT/hapi-sessions.json'
  printf '%s' '{"status":"awake","url":"https://hapi.bll.dev/sessions/s2"}'
else
  printf '%s' '{"status":"up","reason":"opus is running in its terminal, but its command resumes an old session, so it has no phone session yet"}'
fi
"#
    .replace("COUNT", &counter.display().to_string())
    .replace("ROOT", &root.display().to_string());
    let kit_dir = stub_kit(
        "wake-up-hold",
        Some(r#"{"seats":{"opus":{"role":"opus"}}}"#),
        Some(&body),
    );
    let hub = FakeHub::start();
    hub.ctl.lock().unwrap().sessions.insert("s2".into(), true);
    let mut client = Hub::new(hub.config());
    let _env = lock_env(&[
        ("PATH", path_with(&kit_dir)),
        ("SEAT_KIT", OsString::from("")),
        ("SEAT_TMUX", OsString::from(fake_tmux())),
        (
            "SEAT_FAKE_STATE",
            OsString::from(root.join("tmux.state").display().to_string()),
        ),
    ]);
    let dir = seat::seat_dir(&root, "opus");
    let calls = || {
        std::fs::read_to_string(&counter)
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    dstate::store_state(
        &dir,
        &dstate::DispatchState {
            asleep: Some(dstate::Asleep {
                at: "2026-10-23T00:00:00Z".into(),
                reason: "idle 30m".into(),
            }),
            ..Default::default()
        },
    )
    .unwrap();
    send(&root, "opus", "wake up");
    let key = only_key(&root, "opus");
    mark(&hub, &key, "invoked");

    // The first wake answers up: one failure line, the hold recorded,
    // the mail kept.
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Blocked
    );
    assert_eq!(calls(), "1");
    assert!(
        dstate::load_state(&dir).wake_hold.is_some(),
        "the up answer is held"
    );
    assert_eq!(n_files(&root, "opus", "inbox"), 1);
    assert!(
        log_text(&root).contains("answered up"),
        "the reason is reported"
    );

    // The pane the script started stands: the next pass never calls
    // the script and never re-reports.
    crate::session::cmd_up(&root, &["opus".to_string()]).unwrap();
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Blocked
    );
    assert_eq!(calls(), "1", "the script was not called again");
    assert_eq!(
        log_text(&root).matches("wake failed").count(),
        1,
        "the hold is not re-reported every pass"
    );
    assert!(
        !hub.journal().iter().any(|l| l.starts_with("POST-MSG")),
        "nothing was POSTed"
    );

    // The pane gone: the wake runs again and now delivers.
    crate::session::cmd_down(&root, &["opus".to_string()]).unwrap();
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Delivered
    );
    assert_eq!(calls(), "2", "the pane gone re-armed the wake");
    let state = dstate::load_state(&dir);
    assert_eq!(state.asleep, None, "woken");
    assert_eq!(state.wake_hold, None, "the hold is spent");
    assert_eq!(state.woken.map(|w| w.key), Some(key.clone()));
    assert_eq!(n_files(&root, "opus", "archive"), 1);
}

/// seat-role fixes its board at `<SEAT_ROLE_HOME>/.local/share/seat`
/// and calls `seat up` with no `--root`, so a dispatcher on any other
/// board could sleep a seat whose wake would rewrite another board's
/// bindings. Such a board never sleeps; the board the script writes
/// still does.
#[test]
fn a_board_the_wake_script_cannot_write_never_sleeps() {
    let kit_dir = stub_kit(
        "sleep-other-board",
        Some(r#"{"seats":{"opus":{"role":"opus"}}}"#),
        None,
    );
    let _env = lock_env(&[
        ("PATH", path_with(&kit_dir)),
        ("SEAT_KIT", OsString::from("")),
        ("SEAT_TMUX", OsString::from(fake_tmux())),
    ]);
    let (root, _hub, mut client) = up_and_seen("opus", "s1", 31, QUIET);
    // Point the script's home at a different board.
    let other = crate::testutil::temp_root("other-board-home");
    std::fs::create_dir_all(other.join(".local").join("share").join("seat")).unwrap();
    std::env::set_var("SEAT_ROLE_HOME", &other);
    let dir = seat::seat_dir(&root, "opus");

    dispatch::pass_seat(&root, "opus", &mut client).unwrap();
    assert!(
        dstate::load_state(&dir).asleep.is_none(),
        "a board the wake cannot write never sleeps"
    );
    assert!(
        crate::session::running_seats(&fake_tmux())
            .unwrap()
            .contains("opus"),
        "the pane stands"
    );
    assert!(!log_text(&root).contains("sleep seat="));

    // The same seat on the board the script writes still sleeps.
    std::env::set_var("SEAT_ROLE_HOME", root.join("role-home"));
    dispatch::pass_seat(&root, "opus", &mut client).unwrap();
    assert!(
        dstate::load_state(&dir).asleep.is_some(),
        "the supported board sleeps"
    );
    assert!(
        !crate::session::running_seats(&fake_tmux())
            .unwrap()
            .contains("opus"),
        "the pane went down"
    );
}

// ---- pending recovery and the working phase -------------------------

/// One `--once` scan: a worker per scheduled seat, exactly one pass
/// each, then the scan returns.
fn one_pass_scan(root: &Path) {
    let stop = Arc::new(AtomicBool::new(false));
    dispatch::run(
        root,
        true,
        &stop,
        Waits {
            first: Duration::from_millis(5),
            max: Duration::from_millis(5),
        },
    )
    .unwrap();
}

/// A dispatcher restart leaves an unbound asleep seat carrying a
/// pending sleep mark with an empty inbox: the scan schedules it —
/// no binding, no mail — and the first pass settles the mark through
/// the no-binding cancel, logging one settlement line and no failure.
#[test]
fn the_scan_settles_a_pending_sleep_with_no_binding_and_no_mail() {
    let root = board_with(&[]); // no bindings at all
    verbs::cmd_register(
        &root,
        "opus",
        "sb",
        None,
        Some(root.display().to_string()),
        vec!["/bin/sleep".into(), "30".into()],
        None,
    )
    .unwrap();
    // No kit is stubbed and PATH is left alone: the cancel path never
    // wakes, so nothing in this pass consults the kit.
    let hub = FakeHub::start();
    let _env = lock_env(&[
        ("SEAT_TMUX", OsString::from(fake_tmux())),
        (
            "SEAT_FAKE_STATE",
            OsString::from(root.join("tmux.state").display().to_string()),
        ),
        ("HAPI_API_URL", OsString::from(hub.config().base)),
        ("CLI_API_TOKEN", OsString::from("test-token")),
    ]);
    // The restart's leftover: the sleep mark beside a pane fact naming
    // the live pane — a kill that never landed — and an empty inbox.
    crate::session::cmd_up(&root, &["opus".to_string()]).unwrap();
    let pane = crate::session::live_pane_id("opus")
        .expect("the fake multiplexer answers")
        .expect("the pane is up");
    let dir = seat::seat_dir(&root, "opus");
    dstate::store_state(
        &dir,
        &dstate::DispatchState {
            asleep: Some(dstate::Asleep {
                at: "2026-10-23T00:00:00Z".into(),
                reason: "idle 30m".into(),
            }),
            pane_seen_at: Some("2026-10-23T00:00:00Z".into()),
            pane_id: Some(pane),
            ..Default::default()
        },
    )
    .unwrap();

    one_pass_scan(&root);

    let state = dstate::load_state(&dir);
    assert_eq!(
        state.asleep, None,
        "the pending sleep is settled on the first pass"
    );
    assert_eq!(state.pane_seen_at, None, "the pane fact cleared with it");
    assert!(
        crate::session::running_seats(&fake_tmux())
            .unwrap()
            .contains("opus"),
        "the pane was never killed"
    );
    let log = log_text(&root);
    assert!(
        log.contains("cancelled=1 cause=no-binding"),
        "the settlement logs its line: {log}"
    );
    assert!(
        !log.contains("failure") && !log.contains("wake"),
        "no binding and no mail is not a delivery failure: {log}"
    );
}

/// The mixed-age case: a turn marks every in-flight posted item
/// `working`, younger mail included; the turn ends between one pass's
/// session read and its poll, so the pass archives the older item and
/// leaves the younger one stored `working`. Once the session reads
/// idle, the next pass — nothing old owed, the younger file fresh —
/// must still re-read the session and return the item to `posted`,
/// and the alarm stays silent until the item's own file crosses the
/// threshold (a late alarm, never a false one).
#[test]
fn a_stored_working_item_returns_to_posted_when_the_session_stops_thinking() {
    let (root, hub, _client) = kit(&[("opus", "s1")], &[]);
    let thinking = r#"{"session":{"active":true,"thinking":true,"thinkingAt":60000}}"#;
    hub.ctl
        .lock()
        .unwrap()
        .get_bodies
        .insert("s1".into(), thinking.into());
    let _env = lock_env(&[
        ("SEAT_TMUX", OsString::from(fake_tmux())),
        (
            "SEAT_FAKE_STATE",
            OsString::from(root.join("tmux.state").display().to_string()),
        ),
        ("HAPI_API_URL", OsString::from(hub.config().base)),
        ("CLI_API_TOKEN", OsString::from("test-token")),
    ]);
    // The older item: posted in an earlier turn, its file old, still
    // queued. The younger one arrives mid-turn and stays fresh.
    let old_key = old_posted_item(&root, "opus", "s1");
    send(&root, "opus", "the younger one");
    let young_key = only_key(&root, "opus");
    mark(&hub, &old_key, "queued");
    mark(&hub, &young_key, "queued");
    let dir = seat::seat_dir(&root, "opus");

    // Pass one: the older item is worked (no page), the younger is
    // posted.
    one_pass_scan(&root);
    let item = dstate::load_state(&dir)
        .items
        .get(&young_key)
        .unwrap()
        .clone();
    assert_eq!(item.phase, dstate::Phase::Posted);

    // Pass two: the session is on the turn, so both in-flight items
    // are stored `working`, younger mail included.
    one_pass_scan(&root);
    for key in [&old_key, &young_key] {
        let item = dstate::load_state(&dir).items.get(key).unwrap().clone();
        assert_eq!(item.phase, dstate::Phase::Working, "{key}");
    }

    // The turn ends between pass three's session read and its poll:
    // the hub has already marked the older item invoked while the
    // session still reads thinking. The pass archives the older item
    // and leaves the younger one stored `working` — the stale fact.
    mark(&hub, &old_key, "invoked");
    one_pass_scan(&root);
    let state = dstate::load_state(&dir);
    assert_eq!(
        state.items.get(&old_key).unwrap().phase,
        dstate::Phase::Done
    );
    let item = state.items.get(&young_key).unwrap().clone();
    assert_eq!(item.phase, dstate::Phase::Working, "the stale fact is stored");
    assert_eq!(n_files(&root, "opus", "archive"), 1);

    // The session goes idle. The next pass owes nothing old, but the
    // stored `working` item is re-read and returned to `posted`.
    hub.ctl
        .lock()
        .unwrap()
        .get_bodies
        .insert("s1".into(), QUIET.into());
    one_pass_scan(&root);
    let item = dstate::load_state(&dir)
        .items
        .get(&young_key)
        .unwrap()
        .clone();
    assert_eq!(
        item.phase,
        dstate::Phase::Posted,
        "the stale working phase is gone on the next pass"
    );
    assert_eq!(item.working_at, None);
    assert_eq!(
        n_files(&root, "opus", "draining"),
        1,
        "the younger item is still in flight"
    );
    assert_eq!(
        n_posts(&hub.journal()),
        1,
        "the younger item was posted exactly once"
    );
    let log = log_text(&root);
    assert!(
        !log.contains("cause=undelivered"),
        "no page before the item's own threshold: {log}"
    );
}
