//! The dispatcher check cases against the one small fake hub on
//! 127.0.0.1 (never the live hub): the brief's hapi contract plus the
//! local-state cases (room cursors, invalid posts, `awake`, the
//! fresh-work wake) and the 0.6.0 additions (the alarm matrix, the
//! room-post wake, the reload).
use crate::alarm;
use crate::dispatch::{self, Pass, Waits};
use crate::error::SeatError;
use crate::testutil::dispatchkit::{kit, log_text, mark, n_files, n_posts, only_key, send};
use crate::testutil::fakewebhook::FakeWebhook;
use crate::testutil::write_bindings;
use crate::webhook::{self, RingWaits, WebhookConfig};
use crate::{drain, dstate, msg, room, seat, verbs};
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

/// Room posts deliver under room keys; no cursor advances past a post
/// still in flight — and posts invoked in different passes still
/// advance it once the last one lands, so the seat goes Idle.
#[test]
fn room_cursor_waits_then_advances() {
    let (root, hub, mut client) = kit(&[("opus", "s1")], &["s1"]);
    room::create(&root, "ops").unwrap();
    room::cmd_post(&root, "ops", Some("opus"), "post one", None).unwrap();
    room::cmd_post(&root, "ops", Some("opus"), "post two", None).unwrap();
    room::cmd_follow(&root, "ops", "opus").unwrap();
    let key = |i: usize| {
        dstate::room_key(
            "opus",
            "ops",
            room::posts(&root, "ops").unwrap()[i]
                .file_name()
                .unwrap()
                .to_str()
                .unwrap(),
        )
    };
    mark(&hub, &key(0), "invoked"); // post two does not invoke yet
    let dir = seat::seat_dir(&root, "opus");
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Waiting
    );
    assert_eq!(
        room::read_cursor(&dir, "ops").unwrap(),
        "0",
        "post two is still in flight"
    );
    assert!(
        log_text(&root).contains(&key(0)),
        "room posts log their key"
    );
    mark(&hub, &key(1), "invoked");
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Delivered
    );
    assert_eq!(
        room::read_cursor(&dir, "ops").unwrap(),
        msg::ts_prefix(&room::posts(&root, "ops").unwrap()[1]).unwrap(),
        "both posts done: the cursor advances past post one too"
    );
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Idle
    );
}

/// A pass whose only followed-room posts are invalid advances the
/// cursors past them under the lock (as drain's commit does): one
/// failure line, not one per 2-second scan, and nothing is POSTed.
#[test]
fn invalid_room_posts_do_not_repeat_every_scan() {
    let (root, hub, mut client) = kit(&[("opus", "s1")], &["s1"]);
    room::create(&root, "ops").unwrap();
    room::cmd_post(&root, "ops", Some("opus"), "tampered post", None).unwrap();
    let post = room::posts(&root, "ops").unwrap().remove(0);
    let raw = std::fs::read_to_string(&post).unwrap();
    std::fs::write(&post, raw.replace("tampered post", "broken post")).unwrap();
    room::cmd_follow(&root, "ops", "opus").unwrap();
    let dir = seat::seat_dir(&root, "opus");
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Idle
    );
    assert_eq!(
        room::read_cursor(&dir, "ops").unwrap(),
        msg::ts_prefix(&post).unwrap(),
        "the cursor moved past the rejected post"
    );
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Idle
    );
    assert_eq!(
        log_text(&root).matches("failure").count(),
        1,
        "logged once, not per scan"
    );
    assert_eq!(n_posts(&hub.journal()), 0);
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

/// A followed-room post wakes a worker waiting on a queued item; a
/// post the receive path would refuse (tampered) never wakes it.
#[test]
fn a_followed_room_post_wakes_a_backing_off_worker() {
    let (root, hub, _client) = kit(&[("opus", "s1")], &["s1"]);
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
    // A good post in a followed room wakes the worker mid-backoff.
    room::create(&root, "ops").unwrap();
    room::cmd_post(&root, "ops", Some("opus"), "wake up", None).unwrap();
    room::cmd_follow(&root, "ops", "opus").unwrap();
    let room_key = dstate::room_key(
        "opus",
        "ops",
        room::posts(&root, "ops").unwrap()[0]
            .file_name()
            .unwrap()
            .to_str()
            .unwrap(),
    );
    wait_for(
        || posted(&format!("POST-MSG s1 {room_key}")),
        "the room post waited behind the queued backoff",
    );
    std::thread::sleep(Duration::from_millis(200)); // let the pass finish
                                                    // A tampered post would be refused by receive: it must not wake.
    let (before, log_before) = (hub.journal().len(), log_text(&root).len());
    room::cmd_post(&root, "ops", Some("opus"), "do not wake", None).unwrap();
    let last = room::posts(&root, "ops").unwrap().pop().unwrap();
    let raw = std::fs::read_to_string(&last).unwrap();
    std::fs::write(&last, raw.replace("do not wake", "broken post")).unwrap();
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        hub.journal().len(),
        before,
        "a refused post woke the worker"
    );
    assert!(
        !log_text(&root)[log_before..].contains("rejected"),
        "a refused post was received"
    );
    stop.store(true, Ordering::SeqCst);
}

fn alarm_posts(root: &std::path::Path) -> Vec<String> {
    room::posts(root, alarm::ROOM)
        .unwrap()
        .iter()
        .filter_map(|p| msg::read_msg(p).ok().map(|(_, b)| b))
        .collect()
}

/// The frozen alarm contract, one matrix: each cause fires once (one
/// log line, one room post as `sb-watch`), retries stay silent,
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
    room::create(&root, alarm::ROOM).unwrap();
    verbs::cmd_register(&root, alarm::SEAT, "sb", None, None, Vec::new()).unwrap();
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
    assert_eq!(alarm_posts(&root).len(), 1);
    assert!(alarm_posts(&root)[0].starts_with("seat opus: session-inactive: session s1"));

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
    assert_eq!(alarm_posts(&root).len(), 2);

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
    assert_eq!(alarm_posts(&root).len(), 4);
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
    assert_eq!(alarm_posts(&root).len(), 6);

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
    assert_eq!(alarm_posts(&root).len(), 7);
    dispatch::pass_seat(&root, "opus", &mut client).unwrap(); // quarantined: clears
    std::fs::write(&old, "stale\n\nstale").unwrap();
    dispatch::pass_seat(&root, "opus", &mut client).unwrap();
    assert_eq!(log_text(&root).matches("cause=undelivered").count(), 2);
    assert_eq!(alarm_posts(&root).len(), 8);

    // webhook-failing: strikes 1 and 2 log one failure line each,
    // strike 3 alarms once, further failures stay silent, and a
    // successful ring clears.
    let hook = FakeWebhook::start();
    verbs::cmd_register(&root, "grok", "sb", None, None, Vec::new()).unwrap();
    let gdir = seat::seat_dir(&root, "grok");
    std::fs::write(gdir.join("webhook.url"), format!("{}\n", hook.url())).unwrap();
    std::fs::write(gdir.join("bridge.key"), "k\n").unwrap();
    send(&root, "grok", "ring grok");
    let cfg = WebhookConfig::mem(hook.url(), "k".into(), None, false);
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
    assert_eq!(alarm_posts(&root).len(), 9);
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
    verbs::cmd_register(&root, "alpha", "sb", None, None, Vec::new()).unwrap();
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
    room::create(&root, alarm::ROOM).unwrap();
    verbs::cmd_register(&root, alarm::SEAT, "sb", None, None, Vec::new()).unwrap();
    alarm::fire(&root, Some("alpha"), alarm::SESSION_INACTIVE, "s1 gone").unwrap();
    alarm::fire(&root, Some("alpha"), alarm::SESSION_INACTIVE, "again").unwrap();
    assert_eq!(alarm_posts(&root).len(), 1, "latched across the reload");
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

/// A pending followed-room post raises `undelivered`: the age check
/// includes, beside the seat's own `inbox/` and `draining/` files,
/// the posts this seat's `receive` would accept, aged by their
/// verified header `ts` — a seat whose only owed item is an old room
/// post is undelivered too. Delivering the post clears the cause; a
/// tampered post never counts, exactly as `receive` refuses it.
#[test]
fn an_old_pending_room_post_raises_undelivered() {
    let (root, hub, mut client) = kit(&[("opus", "s1")], &["s1"]);
    room::create(&root, alarm::ROOM).unwrap();
    verbs::cmd_register(&root, alarm::SEAT, "sb", None, None, Vec::new()).unwrap();
    room::create(&root, "ops").unwrap();
    // Verified posts, backdated by header ts and file prefix alike.
    let token = verbs::sender_token(&root, "opus").unwrap();
    let write_post = |ts: &str, id: &str, tamper: bool| {
        // The MAC is always over the honest body; a tampered post
        // writes something else, so verification fails exactly as a
        // forged post would.
        let mac = msg::compute_hmac(&token, "ops", "opus", id, ts, b"old bulletin", None).unwrap();
        let head = format!(
            "{{\"dest\":\"ops\",\"from\":\"opus\",\"id\":\"{id}\",\"ts\":\"{ts}\",\"hmac\":\"{mac}\"}}"
        );
        let body = if tamper { "altered" } else { "old bulletin" };
        std::fs::write(
            room::room_dir(&root, "ops").join(format!("{ts}-{id}.msg")),
            format!("{head}\n\n{body}"),
        )
        .unwrap();
    };
    let old = "20260101T000000.000000";
    write_post(old, "deadbeef", false);
    room::cmd_follow(&root, "ops", "opus").unwrap();

    // The seat owes nothing in inbox/ or draining/ — only the old
    // room post — and that alone raises the alarm.
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Waiting
    );
    assert_eq!(
        log_text(&root)
            .matches("alarm seat=opus cause=undelivered")
            .count(),
        1
    );
    assert!(alarm_posts(&root)[0].contains("undelivered"));

    // Delivered: the cursor advances past the post, the seat owes
    // nothing old any more, and the cause clears.
    let key = dstate::room_key("opus", "ops", &format!("{old}-deadbeef.msg"));
    mark(&hub, &key, "invoked");
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Delivered
    );
    dispatch::pass_seat(&root, "opus", &mut client).unwrap(); // the idle pass: the clear
    assert!(!std::fs::read_to_string(root.join("dispatch-alarms.json"))
        .unwrap_or_default()
        .contains("undelivered:opus"));

    // A tampered old post is not owed (receive refuses it): no new
    // alarm, and the pass advances the cursor past it.
    write_post("20260101T000001.000000", "feedface", true);
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Idle
    );
    assert_eq!(
        log_text(&root).matches("cause=undelivered").count(),
        1,
        "a post receive would refuse is not undelivered"
    );
}

/// Partial room delivery: an old post delivered in an earlier round
/// (`Phase::Done`) with a younger post from the same room still in
/// flight — the cursor lags behind both, but the delivered post is
/// not owed mail, so once only the young post remains the
/// `undelivered` cause clears instead of staying latched on the done
/// key behind the cursor.
#[test]
fn a_delivered_room_post_behind_the_cursor_is_not_owed() {
    let (root, hub, mut client) = kit(&[("opus", "s1")], &["s1"]);
    room::create(&root, alarm::ROOM).unwrap();
    verbs::cmd_register(&root, alarm::SEAT, "sb", None, None, Vec::new()).unwrap();
    room::create(&root, "ops").unwrap();
    // Verified posts: an old one and a fresh one, same room.
    let token = verbs::sender_token(&root, "opus").unwrap();
    let write_post = |ts: &str, id: &str| {
        let mac = msg::compute_hmac(&token, "ops", "opus", id, ts, b"bulletin", None).unwrap();
        std::fs::write(
            room::room_dir(&root, "ops").join(format!("{ts}-{id}.msg")),
            format!(
                "{{\"dest\":\"ops\",\"from\":\"opus\",\"id\":\"{id}\",\"ts\":\"{ts}\",\"hmac\":\"{mac}\"}}\n\nbulletin"
            ),
        )
        .unwrap();
    };
    let old = "20260101T000000.000000";
    let young = msg::now_ts();
    write_post(old, "deadbeef");
    write_post(&young, "feedface");
    room::cmd_follow(&root, "ops", "opus").unwrap();
    let old_key = dstate::room_key("opus", "ops", &format!("{old}-deadbeef.msg"));
    let young_key = dstate::room_key("opus", "ops", &format!("{young}-feedface.msg"));

    // The old post is genuinely undelivered: the alarm fires once.
    // Delivering it while the young one stays queued leaves the room
    // cursor behind both (the young post holds it back).
    mark(&hub, &old_key, "invoked");
    mark(&hub, &young_key, "queued");
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Waiting
    );
    assert_eq!(
        room::read_cursor(&seat::seat_dir(&root, "opus"), "ops").unwrap(),
        "0"
    );
    assert!(std::fs::read_to_string(root.join("dispatch-alarms.json"))
        .unwrap_or_default()
        .contains("undelivered:opus"));

    // The next pass owes only the young post: the delivered one is
    // `Phase::Done` behind the cursor and must not keep the cause
    // latched, and the young one is not old — so the cause clears.
    assert_eq!(
        dispatch::pass_seat(&root, "opus", &mut client).unwrap(),
        Pass::Waiting
    );
    assert_eq!(
        log_text(&root).matches("cause=undelivered").count(),
        1,
        "the delivered post behind the cursor is not owed"
    );
    assert!(!std::fs::read_to_string(root.join("dispatch-alarms.json"))
        .unwrap_or_default()
        .contains("undelivered:opus"));
}

/// A pending followed-room post is aged by its verified header `ts`,
/// never by its file-name prefix: nothing requires the two to match.
/// An accepted post whose header is old but whose file name is recent
/// must fire; the reverse — a fresh header under an old file name —
/// must not.
#[test]
fn a_room_post_is_aged_by_its_header_ts_not_its_file_name() {
    let write = |root: &std::path::Path, token: &str, header_ts: &str, file_ts: &str, id: &str| {
        let mac =
            msg::compute_hmac(token, "ops", "opus", id, header_ts, b"bulletin", None).unwrap();
        std::fs::write(
            room::room_dir(root, "ops").join(format!("{file_ts}-{id}.msg")),
            format!(
                "{{\"dest\":\"ops\",\"from\":\"opus\",\"id\":\"{id}\",\"ts\":\"{header_ts}\",\"hmac\":\"{mac}\"}}\n\nbulletin"
            ),
        )
        .unwrap();
    };
    // Old header, recent file name: owed and old — the alarm fires.
    {
        let (root, _hub, mut client) = kit(&[("opus", "s1")], &["s1"]);
        room::create(&root, alarm::ROOM).unwrap();
        verbs::cmd_register(&root, alarm::SEAT, "sb", None, None, Vec::new()).unwrap();
        room::create(&root, "ops").unwrap();
        let token = verbs::sender_token(&root, "opus").unwrap();
        write(
            &root,
            &token,
            "20260101T000000.000000",
            &msg::now_ts(),
            "deadbeef",
        );
        room::cmd_follow(&root, "ops", "opus").unwrap();
        dispatch::pass_seat(&root, "opus", &mut client).unwrap();
        assert!(
            log_text(&root).contains("alarm seat=opus cause=undelivered"),
            "an accepted old post is owed however its file is named"
        );
        assert!(alarm_posts(&root)[0].contains("undelivered"));
    }
    // Fresh header, old file name: not owed-old — no alarm.
    {
        let (root, _hub, mut client) = kit(&[("opus", "s1")], &["s1"]);
        room::create(&root, alarm::ROOM).unwrap();
        verbs::cmd_register(&root, alarm::SEAT, "sb", None, None, Vec::new()).unwrap();
        room::create(&root, "ops").unwrap();
        let token = verbs::sender_token(&root, "opus").unwrap();
        write(
            &root,
            &token,
            &msg::now_ts(),
            "20260101T000000.000000",
            "deadbeef",
        );
        room::cmd_follow(&root, "ops", "opus").unwrap();
        dispatch::pass_seat(&root, "opus", &mut client).unwrap();
        assert_eq!(
            log_text(&root).matches("cause=undelivered").count(),
            0,
            "a fresh post under an old file name is not undelivered"
        );
    }
}
