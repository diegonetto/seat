//! The dispatcher check cases against the one small fake hub on
//! 127.0.0.1 (never the live hub): the brief's hapi contract plus the
//! local-state cases (room cursors, invalid posts, `awake`, the
//! fresh-work wake).
use crate::dispatch::{self, Pass, Waits};
use crate::testutil::dispatchkit::{kit, log_text, mark, n_files, n_posts, only_key, send};
use crate::testutil::write_bindings;
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
