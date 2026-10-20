//! The webhook adapter's Check cases: the signal bytes against a
//! committed fixture (fixed ts, nonce and key), the payload doorbell
//! ported from `test_grok_doorbell.py` (verification, carry-once,
//! failed-post-no-retry, suppression), the re-ring that stops once
//! the bot drained, and the worker that steps aside when its seat
//! becomes hapi-bound. Nothing here touches the live board or panel:
//! every root is a throwaway and every POST targets the local
//! receiver.
use crate::dstate::load_state;
use crate::testutil::fakewebhook::FakeWebhook;
use crate::testutil::{temp_root, write_bindings};
use crate::webhook::{self, RingOutcome, RingWaits, WebhookConfig};
use crate::{alarm, board, msg, room, seat, verbs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const TS: &str = "2026-10-20T00:00:00Z";
const NONCE: &str = "00112233445566778899aabbccddeeff";
const KEY: &str = "bridge-secret";
const HOUR: Duration = Duration::from_secs(3600);
const MAIL_TS: &str = "20261015T204500.000000";

fn no_sleep(_: Duration) {}
fn hour_waits() -> RingWaits {
    RingWaits {
        first: HOUR,
        max: HOUR,
    }
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!("tests/fixtures/{name}"))
        .unwrap()
        .trim_end()
        .to_string()
}

fn wait_for(pred: impl Fn() -> bool, what: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !pred() {
        assert!(std::time::Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The signal body and headers, byte for byte the committed fixture,
/// through a live POST.
#[test]
fn signal_body_and_headers_match_the_fixture() {
    let hook = FakeWebhook::start();
    let cfg = WebhookConfig::mem(hook.url(), KEY.into(), None, false);
    let body = webhook::signal_body("eng-grokbot", TS, NONCE);
    assert_eq!(body, fixture("webhook-signal.body"));
    webhook::post(&cfg, &body, 1, &no_sleep).unwrap();
    let (headers, sent) = hook.hits()[0].clone();
    assert_eq!(sent, fixture("webhook-signal.body"));
    for line in fixture("webhook-signal.headers").lines() {
        assert!(headers.contains(line), "missing {line} in {headers}");
    }
}

fn kotek_dir(root: &Path) -> PathBuf {
    seat::seat_dir(root, "kotek-grokbot")
}

/// kotek's board: the payload seat plus the `inbox` and `other` seats
/// (`test_grok_doorbell.py`'s setUp, on a temp root).
fn kotek_board(hook_url: &str) -> PathBuf {
    let root = temp_root("kotek");
    board::init(&root).unwrap();
    for name in ["kotek-grokbot", "inbox", "other"] {
        verbs::cmd_register(&root, name, "sb", None, None, Vec::new()).unwrap();
    }
    let dir = kotek_dir(&root);
    std::fs::write(dir.join("webhook.url"), format!("{hook_url}\n")).unwrap();
    std::fs::write(dir.join("bridge.key"), "bridge-secret\n").unwrap();
    std::fs::write(dir.join("webhook.payload"), b"").unwrap();
    root
}

/// Inbox mail for kotek, the exact shape the Python test writes: the
/// MAC is always keyed by the inbox token (so `sender` need not be
/// inbox, which then fails verification, like `sender="other-seat"`).
fn hq_mail(root: &Path, text: &str, id: &str, sender: &str, tamper: bool) -> PathBuf {
    let token = msg::read_token(&seat::seat_dir(root, "inbox").join("token")).unwrap();
    let mac = msg::compute_hmac(
        &token,
        "kotek-grokbot",
        sender,
        id,
        MAIL_TS,
        text.as_bytes(),
        None,
    )
    .unwrap();
    let head = format!(
        "{{\"dest\":\"kotek-grokbot\",\"from\":\"{sender}\",\"id\":\"{id}\",\"ts\":\"{MAIL_TS}\",\"hmac\":\"{mac}\"}}"
    );
    let body = if tamper { "altered" } else { text };
    let path = kotek_dir(root)
        .join("inbox")
        .join(format!("{MAIL_TS}-{id}.msg"));
    std::fs::write(&path, format!("{head}\n\n{body}")).unwrap();
    path
}

fn payload_cfg(hook_url: &str) -> WebhookConfig {
    WebhookConfig::mem(hook_url.into(), KEY.into(), None, true)
}

/// The doorbell's cases from `test_grok_doorbell.py`: a verified
/// message is carried once with the exact payload body; a tampered
/// MAC never leaks; a failed POST is attempted exactly once, never
/// re-POSTed, and other mail cannot ring a signal over it.
#[test]
fn payload_doorbell_ports_the_python_cases() {
    let hook = FakeWebhook::start();
    let root = kotek_board(&hook.url());
    let seat_name = "kotek-grokbot";
    // An empty webhook.auth file (the Python setUp) sends no auth header.
    std::fs::write(kotek_dir(&root).join("webhook.auth"), b"").unwrap();

    // carry-once: exactly one payload POST, the exact body and signature.
    let carried = hq_mail(&root, "hello Mariola", "abcdef12", "inbox", false);
    assert_eq!(
        webhook::ring_once(&root, seat_name, &payload_cfg(&hook.url()), &no_sleep).unwrap(),
        true
    );
    let (headers, body) = hook.hits()[0].clone();
    // The exact serialization and field order of the Python doorbell's
    // `json.dumps(..., ensure_ascii=False, separators=(",", ":"))`.
    let (ts, nonce) = body
        .strip_prefix("{\"schema_version\":1,\"seat\":\"kotek-grokbot\",\"ts\":\"")
        .and_then(|r| r.split_once("\",\"nonce\":\""))
        .and_then(|(ts, r)| {
            r.split_once("\",\"from\":\"inbox\",\"id\":\"abcdef12\",\"text\":\"hello Mariola\",\"thread\":\"inbox\"}")
                .map(|(nonce, tail)| (ts.to_string(), (nonce, tail)))
        })
        .map(|(ts, (nonce, tail))| {
            assert_eq!(tail, "");
            (ts, nonce.to_string())
        })
        .unwrap_or_else(|| panic!("payload body shape: {body}"));
    assert!(ts.len() == 20 && ts.ends_with('Z'), "UTC ts: {ts}");
    assert_eq!(nonce.len(), 32);
    assert!(nonce.bytes().all(|b| b.is_ascii_hexdigit()));
    assert!(headers.contains(
        format!(
            "x-memo-signature: sha256={}",
            webhook::signature_hex(KEY, &body)
        )
        .as_str()
    ));
    assert!(headers.contains("content-type: application/json"));
    assert!(!headers.contains("authorization:"));
    assert_eq!(
        webhook::id_list(&kotek_dir(&root).join("webhook-delivered.json")),
        ["abcdef12".to_string()]
    );
    assert_eq!(
        webhook::ring_once(&root, seat_name, &payload_cfg(&hook.url()), &no_sleep).unwrap(),
        false,
        "a delivered id never rides twice"
    );
    assert_eq!(hook.hits().len(), 1);
    // The bot's own drain took the carried mail out of inbox/.
    std::fs::remove_file(&carried).unwrap();

    // verification: a tampered MAC never leaks; other mail gets the signal.
    let bad = hq_mail(&root, "secret", "feedface", "inbox", true);
    assert!(webhook::verified_hq_mail(&bad, &root, seat_name).is_none());
    assert_eq!(
        webhook::ring_once(&root, seat_name, &payload_cfg(&hook.url()), &no_sleep).unwrap(),
        true
    );
    let signal = hook.hits()[1].1.clone();
    assert!(signal.starts_with("{\"schema_version\":1,\"seat\":\"kotek-grokbot\""));
    assert!(!signal.contains("secret") && !signal.contains("altered"));

    // failed-post-no-retry: one attempt, recorded as attempted only.
    hook.ctl.lock().unwrap().status = 500;
    hq_mail(&root, "uncertain", "12345678", "inbox", false);
    assert!(webhook::ring_once(&root, seat_name, &payload_cfg(&hook.url()), &no_sleep).is_err());
    assert_eq!(hook.hits().len(), 3, "a payload POST has no retry");
    assert!(
        webhook::id_list(&kotek_dir(&root).join("webhook-attempted.json"))
            .contains(&"12345678".to_string()),
        "the attempt is recorded before the send"
    );
    assert_ne!(
        webhook::id_list(&kotek_dir(&root).join("webhook-delivered.json")),
        ["12345678".to_string()],
        "a failed POST is never marked delivered"
    );

    // suppression: no second routine for the attempted id, and other
    // mail must not make the bot drain an attempted message twice.
    hook.ctl.lock().unwrap().status = 0;
    hook.ctl.lock().unwrap().hits.clear();
    assert_eq!(
        webhook::ring_once(&root, seat_name, &payload_cfg(&hook.url()), &no_sleep).unwrap(),
        false
    );
    hq_mail(&root, "other mail", "99999999", "other", false);
    assert_eq!(
        webhook::ring_once(&root, seat_name, &payload_cfg(&hook.url()), &no_sleep).unwrap(),
        false
    );
    assert!(hook.hits().is_empty(), "{:?}", hook.hits());
}

/// Re-ring backs off while the rung mail waits, and stops once the
/// bot's own drain took every rung message (the burst closes).
#[test]
fn re_ring_stops_once_the_bot_drained() {
    let hook = FakeWebhook::start();
    let root = kotek_board(&hook.url());
    hq_mail(&root, "ring me", "abcdef12", "inbox", false);
    let cfg = payload_cfg(&hook.url());
    assert_eq!(
        webhook::ring_pass(&root, "kotek-grokbot", &cfg, hour_waits(), &no_sleep).unwrap(),
        RingOutcome::Rang
    );
    assert_eq!(
        webhook::ring_pass(&root, "kotek-grokbot", &cfg, hour_waits(), &no_sleep).unwrap(),
        RingOutcome::Waiting,
        "nothing new and the next ring is not due yet"
    );
    assert_eq!(hook.hits().len(), 1);
    // The bot drained: the file left inbox/ and draining/.
    std::fs::remove_file(
        kotek_dir(&root)
            .join("inbox")
            .join(format!("{MAIL_TS}-abcdef12.msg")),
    )
    .unwrap();
    assert_eq!(
        webhook::ring_pass(&root, "kotek-grokbot", &cfg, hour_waits(), &no_sleep).unwrap(),
        RingOutcome::Idle
    );
    assert_eq!(
        webhook::ring_pass(&root, "kotek-grokbot", &cfg, hour_waits(), &no_sleep).unwrap(),
        RingOutcome::Idle
    );
    assert_eq!(hook.hits().len(), 1, "no ring after the drain");
    assert!(load_state(&kotek_dir(&root)).ring.is_none(), "burst closed");
}

/// A webhook seat that becomes hapi-bound changes worker: the webhook
/// worker rechecks its route before each pass and during its waits,
/// exits, and the routing seam hands the seat to hapi even with
/// `webhook.url` still staged.
#[test]
fn a_webhook_seat_that_becomes_hapi_bound_changes_worker() {
    let hook = FakeWebhook::start();
    let root = kotek_board(&hook.url());
    hq_mail(&root, "ring me", "abcdef12", "inbox", false);
    let cfg = payload_cfg(&hook.url());
    let done = Arc::new(AtomicBool::new(false));
    let (r, d) = (root.clone(), Arc::clone(&done));
    let stop = Arc::new(AtomicBool::new(false));
    std::thread::spawn(move || {
        webhook::worker_loop(r, "kotek-grokbot".into(), stop, hour_waits(), Some(cfg));
        d.store(true, Ordering::SeqCst);
    });
    wait_for(|| !hook.hits().is_empty(), "the first ring never happened");
    write_bindings(&root, &[("kotek-grokbot", "s1")]);
    wait_for(
        || done.load(Ordering::SeqCst),
        "the webhook worker did not step aside",
    );
    let names = vec!["kotek-grokbot".to_string()];
    let (hapi, hooks) =
        crate::dispatch::route_seats(&root, &crate::identity::Bindings::load(&root), &names);
    assert_eq!(
        (hapi, hooks),
        (vec!["kotek-grokbot".to_string()], Vec::<String>::new()),
        "hapi wins even with webhook.url staged"
    );
}

/// Removing `webhook.url` stops this seat's worker: the route is
/// rechecked with the full routing rule — hapi binding, else
/// `webhook.url`, else pull — before each pass and during waits, so
/// once neither hapi nor webhook claims the seat the worker exits
/// during its re-ring wait instead of ringing a pull seat for an
/// hour.
#[test]
fn removing_webhook_url_stops_the_worker() {
    let hook = FakeWebhook::start();
    let root = kotek_board(&hook.url());
    hq_mail(&root, "ring me", "abcdef12", "inbox", false);
    let cfg = payload_cfg(&hook.url());
    let done = Arc::new(AtomicBool::new(false));
    let (r, d) = (root.clone(), Arc::clone(&done));
    let stop = Arc::new(AtomicBool::new(false));
    std::thread::spawn(move || {
        webhook::worker_loop(r, "kotek-grokbot".into(), stop, hour_waits(), Some(cfg));
        d.store(true, Ordering::SeqCst);
    });
    wait_for(|| !hook.hits().is_empty(), "the first ring never happened");
    std::fs::remove_file(kotek_dir(&root).join("webhook.url")).unwrap();
    wait_for(
        || done.load(Ordering::SeqCst),
        "removing webhook.url did not stop the worker",
    );
    assert_eq!(hook.hits().len(), 1, "no ring after the route left webhook");
    let names = vec!["kotek-grokbot".to_string()];
    let (hapi, hooks) =
        crate::dispatch::route_seats(&root, &crate::identity::Bindings::load(&root), &names);
    assert!(
        hapi.is_empty() && hooks.is_empty(),
        "the seat is pull now: {hapi:?} {hooks:?}"
    );
}

/// `seat dispatch --once` sets stop immediately after spawning its
/// workers (the scan's last act before it joins them), so a webhook
/// worker must not let an already-set stop flag skip its first pass:
/// the spawned worker rings once — the pass `--once` exists to run —
/// and only then honours stop and exits, so `--once` never returns
/// before every spawned worker's first pass.
#[test]
fn a_worker_spawned_with_stop_set_still_completes_its_first_pass() {
    let hook = FakeWebhook::start();
    let root = kotek_board(&hook.url());
    hq_mail(&root, "ring me", "abcdef12", "inbox", false);
    let cfg = payload_cfg(&hook.url());
    let done = Arc::new(AtomicBool::new(false));
    let (r, d) = (root.clone(), Arc::clone(&done));
    // Set before the spawn, exactly as the --once scan does.
    let stop = Arc::new(AtomicBool::new(true));
    std::thread::spawn(move || {
        webhook::worker_loop(r, "kotek-grokbot".into(), stop, hour_waits(), Some(cfg));
        d.store(true, Ordering::SeqCst);
    });
    wait_for(
        || !hook.hits().is_empty(),
        "stop was checked before the first pass: --once returned without ringing",
    );
    wait_for(|| done.load(Ordering::SeqCst), "the worker never exited");
    assert_eq!(hook.hits().len(), 1, "exactly the first pass, then stop");
}

/// A webhook seat owing only a verified followed-room post older than
/// 15 minutes still gets a worker and still raises `undelivered`:
/// worker eligibility, the ring's present set and the age check all
/// include the posts this seat's `receive` would accept, aged by their
/// verified header `ts` — the bot's own drain is what takes them,
/// exactly like inbox mail, and a post's body never rides the payload.
/// Once the bot's drain took the post (the cursor advanced past it),
/// the burst closes and the cause clears; a fresh post rings but does
/// not alarm, and a post `receive` would refuse never counts.
#[test]
fn an_old_room_post_alone_starts_a_worker_and_raises_undelivered() {
    crate::testutil::with_throwaway_home(an_old_room_post_alone_isolated);
}

fn an_old_room_post_alone_isolated() {
    let hook = FakeWebhook::start();
    let root = kotek_board(&hook.url());
    let seat_name = "kotek-grokbot";
    room::create(&root, alarm::ROOM).unwrap();
    verbs::cmd_register(&root, alarm::SEAT, "sb", None, None, Vec::new()).unwrap();
    room::create(&root, "ops").unwrap();
    // Verified posts from the inbox seat, backdated by header ts and
    // file prefix alike; the MAC is always over the honest body, so a
    // tampered post fails verification exactly as a forged one would.
    let token = msg::read_token(&seat::seat_dir(&root, "inbox").join("token")).unwrap();
    let write_post = |ts: &str, id: &str, tamper: bool| {
        let mac = msg::compute_hmac(&token, "ops", "inbox", id, ts, b"old bulletin", None).unwrap();
        let body = if tamper { "altered" } else { "old bulletin" };
        std::fs::write(
            room::room_dir(&root, "ops").join(format!("{ts}-{id}.msg")),
            format!(
                "{{\"dest\":\"ops\",\"from\":\"inbox\",\"id\":\"{id}\",\"ts\":\"{ts}\",\"hmac\":\"{mac}\"}}\n\n{body}"
            ),
        )
        .unwrap();
    };
    let old = "20260101T000000.000000";
    write_post(old, "deadbeef", false);
    room::cmd_follow(&root, "ops", seat_name).unwrap();

    // Worker eligibility: the seat owes nothing in inbox/ or draining/,
    // and the room post alone is work.
    assert!(webhook::present_files(&kotek_dir(&root)).is_empty());
    assert!(
        webhook::has_work(&root, seat_name),
        "a seat owing only a followed-room post never got a worker"
    );

    // The pass rings — a signal: the post never rides the payload —
    // and the alarm fires once and stays fired across the burst.
    let cfg = payload_cfg(&hook.url());
    assert_eq!(
        webhook::ring_pass(&root, seat_name, &cfg, hour_waits(), &no_sleep).unwrap(),
        RingOutcome::Rang
    );
    assert_eq!(hook.hits().len(), 1);
    assert!(hook.hits()[0].1.contains("\"seat\":\"kotek-grokbot\""));
    assert!(
        !hook.hits()[0].1.contains("old bulletin"),
        "a room post is never carried in the body"
    );
    assert_eq!(
        webhook::ring_pass(&root, seat_name, &cfg, hour_waits(), &no_sleep).unwrap(),
        RingOutcome::Waiting,
        "nothing new and the next ring is not due yet"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("dispatch.log"))
            .unwrap()
            .matches("alarm seat=kotek-grokbot cause=undelivered")
            .count(),
        1
    );
    assert!(std::fs::read_to_string(root.join("dispatch-alarms.json"))
        .unwrap_or_default()
        .contains("undelivered:kotek-grokbot"));
    let alarms: Vec<String> = room::posts(&root, alarm::ROOM)
        .unwrap()
        .iter()
        .filter_map(|p| msg::read_msg(p).ok().map(|(_, b)| b))
        .collect();
    assert!(alarms[0].contains("undelivered"));

    // The bot's drain took the post (the cursor advanced past it): the
    // burst closes, and with nothing old owed the cause clears.
    room::write_cursor(&kotek_dir(&root), "ops", old).unwrap();
    assert_eq!(
        webhook::ring_pass(&root, seat_name, &cfg, hour_waits(), &no_sleep).unwrap(),
        RingOutcome::Idle
    );
    assert!(load_state(&kotek_dir(&root)).ring.is_none(), "burst closed");
    assert!(!std::fs::read_to_string(root.join("dispatch-alarms.json"))
        .unwrap_or_default()
        .contains("undelivered:kotek-grokbot"));

    // A fresh post is owed — it rings — but not old: no alarm. A
    // tampered post is not owed at all: no work, no ring.
    let fresh = msg::now_ts();
    write_post(&fresh, "feedface", false);
    assert!(webhook::has_work(&root, seat_name));
    assert_eq!(
        webhook::ring_pass(&root, seat_name, &cfg, hour_waits(), &no_sleep).unwrap(),
        RingOutcome::Rang
    );
    assert!(!std::fs::read_to_string(root.join("dispatch-alarms.json"))
        .unwrap_or_default()
        .contains("undelivered:kotek-grokbot"));
    room::write_cursor(&kotek_dir(&root), "ops", &fresh).unwrap();
    write_post("99991231T235959.999999", "badbad11", true);
    assert!(
        !webhook::has_work(&root, seat_name),
        "a post receive would refuse is not owed"
    );
    assert_eq!(
        webhook::ring_pass(&root, seat_name, &cfg, hour_waits(), &no_sleep).unwrap(),
        RingOutcome::Idle
    );
    assert_eq!(hook.hits().len(), 2, "no ring for a refused post");
}
