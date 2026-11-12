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
use crate::testutil::{temp_root, wait_for, write_bindings};
use crate::webhook::{self, RingOutcome, RingWaits, WebhookConfig};
use crate::{board, msg, seat, verbs};
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

/// The signal body and headers, byte for byte the committed fixture,
/// through a live POST.
#[test]
fn signal_body_and_headers_match_the_fixture() {
    let hook = FakeWebhook::start();
    let cfg = WebhookConfig::mem(hook.url(), KEY.into(), None, false, None);
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

/// Pin one signal POST whole — the complete body and the complete
/// header block the receiver records — comparing complete outputs,
/// not substrings. The only nondeterministic bytes are handled
/// explicitly: the UTC `ts` and the 32-lowercase-hex `nonce` are read
/// out of the captured body, validated for shape, and substituted
/// back so every other byte stays pinned; the signature is recomputed
/// over the exact captured body; the receiver's ephemeral `host:port`
/// comes from the hook itself.
fn pin_signal_post(hook: &FakeWebhook, hit: usize) {
    let (headers, body) = hook.hits()[hit].clone();
    let (ts, after_ts) = body
        .strip_prefix("{\"schema_version\":1,\"seat\":\"kotek-grokbot\",\"ts\":\"")
        .and_then(|r| r.split_once("\",\"nonce\":\""))
        .unwrap_or_else(|| panic!("signal body does not parse: {body}"));
    let (nonce, tail) = after_ts
        .split_once("\"}")
        .unwrap_or_else(|| panic!("signal body does not parse: {body}"));
    assert_eq!(tail, "", "bytes after the closing brace: {tail:?}");
    assert_eq!(ts.len(), 20, "second-resolution UTC ts: {ts}");
    chrono::DateTime::parse_from_rfc3339(ts).expect("RFC 3339 ts");
    assert_eq!(nonce.len(), 32, "16 bytes as 32 hex chars: {nonce}");
    assert!(
        nonce
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "lowercase hex nonce: {nonce}"
    );
    assert_eq!(
        body,
        format!(
            "{{\"schema_version\":1,\"seat\":\"kotek-grokbot\",\"ts\":\"{ts}\",\"nonce\":\"{nonce}\"}}"
        ),
        "the complete body, byte for byte"
    );
    let sig = webhook::signature_hex(KEY, &body);
    let url = hook.url();
    let host = url.strip_prefix("http://").unwrap_or_default();
    assert_eq!(
        headers,
        format!(
            "accept-encoding: gzip\ncontent-length: {}\nuser-agent: ureq/3.4.2\naccept: */*\nhost: {host}\ncontent-type: application/json\nx-memo-signature: sha256={sig}\n",
            body.len()
        ),
        "the complete header block, line for line"
    );
}

/// Pin one payload POST whole — the complete body and the complete
/// header block the receiver records — the payload twin of
/// `pin_signal_post`, normalizing the same nondeterministic bytes: the
/// UTC `ts` and the 32-lowercase-hex `nonce` are read out of the
/// captured body, validated for shape, and substituted back so every
/// other byte stays pinned; the signature is recomputed over the exact
/// captured body; the receiver's ephemeral `host:port` comes from the
/// hook itself.
fn pin_payload_post(hook: &FakeWebhook, hit: usize, id: &str, text: &str) {
    let (headers, body) = hook.hits()[hit].clone();
    let quoted_text = serde_json::to_string(text).unwrap();
    let (ts, rest) = body
        .strip_prefix("{\"schema_version\":1,\"seat\":\"kotek-grokbot\",\"ts\":\"")
        .and_then(|r| r.split_once("\",\"nonce\":\""))
        .unwrap_or_else(|| panic!("payload body does not parse: {body}"));
    let tail = format!(
        "\",\"from\":\"inbox\",\"id\":\"{id}\",\"text\":{quoted_text},\"thread\":\"inbox\"}}"
    );
    let nonce = rest
        .strip_suffix(&tail)
        .unwrap_or_else(|| panic!("payload body does not parse: {body}"));
    assert_eq!(ts.len(), 20, "second-resolution UTC ts: {ts}");
    chrono::DateTime::parse_from_rfc3339(ts).expect("RFC 3339 ts");
    assert_eq!(nonce.len(), 32, "16 bytes as 32 hex chars: {nonce}");
    assert!(
        nonce
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "lowercase hex nonce: {nonce}"
    );
    assert_eq!(
        body,
        format!(
            "{{\"schema_version\":1,\"seat\":\"kotek-grokbot\",\"ts\":\"{ts}\",\"nonce\":\"{nonce}\",\"from\":\"inbox\",\"id\":\"{id}\",\"text\":{quoted_text},\"thread\":\"inbox\"}}"
        ),
        "the complete body, byte for byte"
    );
    let sig = webhook::signature_hex(KEY, &body);
    let url = hook.url();
    let host = url.strip_prefix("http://").unwrap_or_default();
    assert_eq!(
        headers,
        format!(
            "accept-encoding: gzip\ncontent-length: {}\nuser-agent: ureq/3.4.2\naccept: */*\nhost: {host}\ncontent-type: application/json\nx-memo-signature: sha256={sig}\n",
            body.len()
        ),
        "the complete header block, line for line"
    );
}

/// Pin the complete persisted ring state — every byte of
/// `dispatch.json` after a pass — with its one nondeterministic field,
/// `next_at`, read out, parsed as rfc3339, and bounded by the pass's
/// own window (`before` through `after`, taken around the pass) shifted
/// by the backoff, then substituted back so every other byte stays
/// pinned.
fn pin_ring_state(
    dir: &Path,
    rung: &[String],
    backoff_ms: u64,
    before: chrono::DateTime<chrono::Utc>,
    after: chrono::DateTime<chrono::Utc>,
) {
    let delay = chrono::TimeDelta::from_std(Duration::from_millis(backoff_ms)).unwrap();
    let next = load_state(dir).ring.unwrap().next_at.unwrap();
    let at = chrono::DateTime::parse_from_rfc3339(&next)
        .unwrap()
        .with_timezone(&chrono::Utc);
    assert!(
        at >= before + delay && at <= after + delay,
        "next_at {next} is not {backoff_ms} ms out inside the pass's window"
    );
    let entries = rung.iter().map(|f| format!("\"{f}\"")).collect::<Vec<_>>();
    assert_eq!(
        std::fs::read_to_string(dir.join("dispatch.json")).unwrap(),
        format!(
            "{{\n  \"items\": {{}},\n  \"ring\": {{\n    \"rung\": [\n      {}\n    ],\n    \"next_at\": \"{next}\",\n    \"backoff_ms\": {backoff_ms},\n    \"failures\": 0\n  }}\n}}\n",
            entries.join(",\n      ")
        ),
        "the complete dispatch.json, byte for byte"
    );
}

/// Pin the complete `dispatch.log` — exactly this many lines — with
/// each line's one nondeterministic field, the rfc3339 stamp `log_line`
/// writes, read out, validated, and substituted back so every other
/// byte stays pinned. An absent log is the empty list.
fn pin_log(root: &Path, expected: &[&str]) {
    let raw = std::fs::read_to_string(root.join("dispatch.log")).unwrap_or_default();
    let lines: Vec<&str> = if raw.is_empty() {
        Vec::new()
    } else {
        assert!(
            raw.ends_with('\n'),
            "a log line without its newline: {raw:?}"
        );
        raw.strip_suffix('\n').unwrap().split('\n').collect()
    };
    assert_eq!(lines.len(), expected.len(), "the complete log: {raw:?}");
    for (line, rest) in lines.iter().zip(expected) {
        let (stamp, tail) = line
            .split_once(' ')
            .unwrap_or_else(|| panic!("a log line without its stamp: {line}"));
        chrono::DateTime::parse_from_rfc3339(stamp)
            .unwrap_or_else(|e| panic!("the stamp is not rfc3339 ({stamp}): {e}"));
        assert_eq!(tail, *rest, "the complete line minus its stamp");
    }
}

/// The signal ring `ring_once` sends in signal mode (no payload
/// marker): exactly one POST, its complete body and complete header
/// block pinned whole by `pin_signal_post`.
#[test]
fn signal_mode_ring_once_pins_the_whole_post() {
    let hook = FakeWebhook::start();
    let cfg = WebhookConfig::mem(hook.url(), KEY.into(), None, false, None);
    let root = temp_root("signal-pin");
    assert_eq!(
        webhook::ring_once(&root, "kotek-grokbot", &cfg, &no_sleep).unwrap(),
        true
    );
    assert_eq!(hook.hits().len(), 1);
    pin_signal_post(&hook, 0);
}

/// Payload mode's fallback ring — unverified other mail waits, so no
/// payload is due — sends the very same complete POST, pinned whole
/// by the same check: both signal sends are byte-identical.
#[test]
fn payload_fallback_ring_once_pins_the_whole_post() {
    let hook = FakeWebhook::start();
    let root = kotek_board(&hook.url());
    hq_mail(&root, "plain note", "aaaa1111", "other", false);
    assert_eq!(
        webhook::ring_once(&root, "kotek-grokbot", &payload_cfg(&hook.url()), &no_sleep).unwrap(),
        true
    );
    assert_eq!(hook.hits().len(), 1);
    pin_signal_post(&hook, 0);
}

/// kotek's board: the payload seat plus the `inbox` and `other` seats
/// (`test_grok_doorbell.py`'s setUp, on a temp root).
fn kotek_board(hook_url: &str) -> PathBuf {
    let root = temp_root("kotek");
    board::init(&root).unwrap();
    for name in ["kotek-grokbot", "inbox", "other"] {
        verbs::cmd_register(&root, name, "sb", None, None, Vec::new(), None).unwrap();
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
    WebhookConfig::mem(hook_url.into(), KEY.into(), None, true, None)
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

#[test]
fn backoff_characterization_production_ring_waits() {
    assert_eq!(
        (webhook::PROD_RING_WAITS.first, webhook::PROD_RING_WAITS.max),
        (Duration::from_secs(60), Duration::from_secs(3600))
    );
}

/// An already delivered payload suppresses POSTs while its burst
/// keeps backing off. Pin every state byte, the initial cap, doubling,
/// the repeated cap, and the reset when the bot drains the burst.
#[test]
fn backoff_characterization_ring_state() {
    for (first, max, expected) in [(3, 10, [3000, 6000, 10000, 10000]), (20, 7, [7000; 4])] {
        let hook = FakeWebhook::start();
        let root = kotek_board(&hook.url());
        let dir = kotek_dir(&root);
        let file = hq_mail(&root, "already delivered", "abcdef12", "inbox", false);
        let mail = std::fs::read(&file).unwrap();
        let delivered = "[\"abcdef12\"]\n";
        std::fs::write(dir.join("webhook-delivered.json"), delivered).unwrap();
        let cfg = payload_cfg(&hook.url());
        let waits = RingWaits {
            first: Duration::from_secs(first),
            max: Duration::from_secs(max),
        };
        // The fifth pass is a new burst, after the fourth was drained.
        for (pass, backoff_ms) in expected.into_iter().chain([expected[0]]).enumerate() {
            let before = chrono::Utc::now();
            assert_eq!(
                webhook::ring_pass(&root, "kotek-grokbot", &cfg, waits, &|_| {
                    panic!("a suppressed payload must not retry HTTP")
                })
                .unwrap(),
                RingOutcome::Waiting
            );
            let after = chrono::Utc::now();
            let mut state = load_state(&dir);
            let next = state.ring.as_ref().unwrap().next_at.as_deref().unwrap();
            let at = chrono::DateTime::parse_from_rfc3339(next).unwrap();
            let delay = chrono::TimeDelta::milliseconds(backoff_ms);
            assert!(at >= before + delay && at <= after + delay);
            assert_eq!(
                std::fs::read_to_string(dir.join("dispatch.json")).unwrap(),
                format!(
                    "{{\n  \"items\": {{}},\n  \"ring\": {{\n    \"rung\": [\n      \"{MAIL_TS}-abcdef12.msg\"\n    ],\n    \"next_at\": \"{next}\",\n    \"backoff_ms\": {backoff_ms},\n    \"failures\": 0\n  }}\n}}\n"
                )
            );
            assert_eq!(std::fs::read(&file).unwrap(), mail);
            if pass == 3 {
                std::fs::remove_file(&file).unwrap();
                assert_eq!(
                    webhook::ring_pass(&root, "kotek-grokbot", &cfg, waits, &no_sleep).unwrap(),
                    RingOutcome::Idle
                );
                assert_eq!(
                    std::fs::read_to_string(dir.join("dispatch.json")).unwrap(),
                    "{\n  \"items\": {}\n}\n"
                );
                hq_mail(&root, "already delivered", "abcdef12", "inbox", false);
            } else {
                state.ring.as_mut().unwrap().next_at = Some("2000-01-01T00:00:00Z".into());
                crate::dstate::store_state(&dir, &state).unwrap();
            }
        }
        assert_eq!(hook.hits(), Vec::<(String, String)>::new());
        assert_eq!(
            std::fs::read_to_string(dir.join("webhook-delivered.json")).unwrap(),
            delivered
        );
        assert!(!dir.join("webhook-attempted.json").exists());
        assert!(!root.join("dispatch.log").exists());
        assert!(!root.join("dispatch-alarms.json").exists());
        assert!(msg::list_msg_dir(&dir.join("draining")).unwrap().is_empty());
        assert!(msg::list_msg_dir(&dir.join("archive")).unwrap().is_empty());
    }
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

/// The re-ring wait's new-mail arm: a file the burst has not rung that
/// appears while the worker sleeps out an hour-long backoff wakes it
/// at once, and the next pass rings the new mail. Every output of both
/// passes is pinned whole: both POSTs by `pin_payload_post`, the
/// persisted state after each pass by `pin_ring_state` (the hour cap
/// doubling holds at the hour), the id records' exact
/// bytes, and the complete log: the undelivered alarm the first pass
/// fires (the month-old fixture mail was undelivered when that pass
/// started), one `delivered` line per POST. The alarm latch stays set,
/// because the second pass's new mail was still undelivered when it
/// started.
#[test]
fn new_mail_wakes_the_re_ring_wait() {
    let hook = FakeWebhook::start();
    let root = kotek_board(&hook.url());
    hq_mail(&root, "ring me", "abcdef12", "inbox", false);
    let dir = kotek_dir(&root);
    let cfg = payload_cfg(&hook.url());
    let done = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let (r, d, s) = (root.clone(), Arc::clone(&done), Arc::clone(&stop));
    let start = chrono::Utc::now();
    std::thread::spawn(move || {
        webhook::worker_loop(r, "kotek-grokbot".into(), s, hour_waits(), Some(cfg));
        d.store(true, Ordering::SeqCst);
    });
    wait_for(|| hook.hits().len() == 1, "the first ring never happened");
    wait_for(
        || load_state(&dir).ring.is_some(),
        "the first pass never recorded its burst",
    );
    pin_payload_post(&hook, 0, "abcdef12", "ring me");
    pin_ring_state(
        &dir,
        &[format!("{MAIL_TS}-abcdef12.msg")],
        3_600_000,
        start,
        chrono::Utc::now(),
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("webhook-delivered.json")).unwrap(),
        "[\"abcdef12\"]\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("webhook-attempted.json")).unwrap(),
        "[\"abcdef12\"]\n"
    );
    let second = chrono::Utc::now();
    hq_mail(&root, "second note", "99999999", "inbox", false);
    wait_for(|| hook.hits().len() == 2, "new mail did not wake the wait");
    stop.store(true, Ordering::SeqCst);
    wait_for(|| done.load(Ordering::SeqCst), "the worker never exited");
    pin_payload_post(&hook, 1, "99999999", "second note");
    pin_ring_state(
        &dir,
        &[
            format!("{MAIL_TS}-99999999.msg"),
            format!("{MAIL_TS}-abcdef12.msg"),
        ],
        3_600_000,
        second,
        chrono::Utc::now(),
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("webhook-delivered.json")).unwrap(),
        "[\"abcdef12\",\"99999999\"]\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("webhook-attempted.json")).unwrap(),
        "[\"abcdef12\",\"99999999\"]\n"
    );
    pin_log(
        &root,
        &[
            "alarm seat=kotek-grokbot cause=undelivered an item has waited over 15 minutes undelivered",
            "delivered seat=kotek-grokbot id=abcdef12 mode=payload",
            "delivered seat=kotek-grokbot id=99999999 mode=payload",
        ],
    );
    assert_eq!(
        std::fs::read_to_string(root.join("dispatch-alarms.json")).unwrap(),
        "{\"causes\":[\"undelivered:kotek-grokbot\"]}\n"
    );
}

/// The re-ring wait's empty-inbox arm: when the bot's drain takes every
/// file while the worker sleeps, nothing is present and the wait ends
/// at once — the next pass prunes the burst closed and the worker
/// returns Idle instead of sleeping the backoff out. The first pass's
/// POST is pinned whole, its persisted state by `pin_ring_state`, and
/// after the close `dispatch.json` is pinned to its exact no-ring
/// bytes. The alarm the first pass fired (the month-old fixture mail,
/// undelivered when that pass started) is pinned cleared by the
/// closing pass, which found nothing old and undelivered.
#[test]
fn an_emptied_inbox_ends_the_re_ring_wait() {
    let hook = FakeWebhook::start();
    let root = kotek_board(&hook.url());
    hq_mail(&root, "ring me", "abcdef12", "inbox", false);
    let dir = kotek_dir(&root);
    let cfg = payload_cfg(&hook.url());
    let done = Arc::new(AtomicBool::new(false));
    let (r, d) = (root.clone(), Arc::clone(&done));
    let stop = Arc::new(AtomicBool::new(false));
    let start = chrono::Utc::now();
    std::thread::spawn(move || {
        webhook::worker_loop(r, "kotek-grokbot".into(), stop, hour_waits(), Some(cfg));
        d.store(true, Ordering::SeqCst);
    });
    wait_for(|| hook.hits().len() == 1, "the first ring never happened");
    wait_for(
        || load_state(&dir).ring.is_some(),
        "the first pass never recorded its burst",
    );
    pin_payload_post(&hook, 0, "abcdef12", "ring me");
    pin_ring_state(
        &dir,
        &[format!("{MAIL_TS}-abcdef12.msg")],
        3_600_000,
        start,
        chrono::Utc::now(),
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("webhook-delivered.json")).unwrap(),
        "[\"abcdef12\"]\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("webhook-attempted.json")).unwrap(),
        "[\"abcdef12\"]\n"
    );
    std::fs::remove_file(dir.join("inbox").join(format!("{MAIL_TS}-abcdef12.msg"))).unwrap();
    wait_for(
        || done.load(Ordering::SeqCst),
        "an emptied inbox did not end the wait",
    );
    assert_eq!(hook.hits().len(), 1);
    assert_eq!(
        std::fs::read_to_string(dir.join("dispatch.json")).unwrap(),
        "{\n  \"items\": {}\n}\n",
        "the burst closed: no ring at all in the state"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("webhook-delivered.json")).unwrap(),
        "[\"abcdef12\"]\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("webhook-attempted.json")).unwrap(),
        "[\"abcdef12\"]\n"
    );
    pin_log(
        &root,
        &[
            "alarm seat=kotek-grokbot cause=undelivered an item has waited over 15 minutes undelivered",
            "delivered seat=kotek-grokbot id=abcdef12 mode=payload",
        ],
    );
    assert_eq!(
        std::fs::read_to_string(root.join("dispatch-alarms.json")).unwrap(),
        "{\"causes\":[]}\n",
        "the closing pass cleared the latch"
    );
}

/// The re-ring wait re-reads the persisted state every slice: a burst
/// record cleared on disk while the worker sleeps makes the
/// still-present mail new again and wakes the wait. The woken pass
/// sends no POST (the id is already delivered, so suppression holds)
/// — the wake is observable only in the burst record coming back, a
/// fresh burst at the first backoff again. The first pass's POST is
/// pinned whole, the state after each pass by `pin_ring_state`, the id
/// records' exact bytes, and the alarm the first pass fired pinned
/// cleared by the woken pass (its mail is delivered, so nothing old is
/// owed).
#[test]
fn a_persisted_state_change_wakes_the_re_ring_wait() {
    let hook = FakeWebhook::start();
    let root = kotek_board(&hook.url());
    hq_mail(&root, "ring me", "abcdef12", "inbox", false);
    let dir = kotek_dir(&root);
    let cfg = payload_cfg(&hook.url());
    let done = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let (r, d, s) = (root.clone(), Arc::clone(&done), Arc::clone(&stop));
    let start = chrono::Utc::now();
    std::thread::spawn(move || {
        webhook::worker_loop(r, "kotek-grokbot".into(), s, hour_waits(), Some(cfg));
        d.store(true, Ordering::SeqCst);
    });
    wait_for(|| hook.hits().len() == 1, "the first ring never happened");
    wait_for(
        || load_state(&dir).ring.is_some(),
        "the first pass never recorded its burst",
    );
    pin_payload_post(&hook, 0, "abcdef12", "ring me");
    pin_ring_state(
        &dir,
        &[format!("{MAIL_TS}-abcdef12.msg")],
        3_600_000,
        start,
        chrono::Utc::now(),
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("webhook-delivered.json")).unwrap(),
        "[\"abcdef12\"]\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("webhook-attempted.json")).unwrap(),
        "[\"abcdef12\"]\n"
    );
    let cleared = chrono::Utc::now();
    let mut state = load_state(&dir);
    state.ring = None;
    crate::dstate::store_state(&dir, &state).unwrap();
    wait_for(
        || load_state(&dir).ring.is_some(),
        "a persisted state change did not wake the wait",
    );
    stop.store(true, Ordering::SeqCst);
    wait_for(|| done.load(Ordering::SeqCst), "the worker never exited");
    assert_eq!(hook.hits().len(), 1, "the delivered id never rides twice");
    pin_ring_state(
        &dir,
        &[format!("{MAIL_TS}-abcdef12.msg")],
        3_600_000,
        cleared,
        chrono::Utc::now(),
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("webhook-delivered.json")).unwrap(),
        "[\"abcdef12\"]\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("webhook-attempted.json")).unwrap(),
        "[\"abcdef12\"]\n"
    );
    pin_log(
        &root,
        &[
            "alarm seat=kotek-grokbot cause=undelivered an item has waited over 15 minutes undelivered",
            "delivered seat=kotek-grokbot id=abcdef12 mode=payload",
        ],
    );
    assert_eq!(
        std::fs::read_to_string(root.join("dispatch-alarms.json")).unwrap(),
        "{\"causes\":[]}\n",
        "the woken pass cleared the latch"
    );
}

/// The re-ring wait's deadline arm: with nothing new on disk at all,
/// the deadline the worker cached after its pass (50 ms out) alone
/// wakes the wait, and the woken pass doubles the backoff into the
/// 100 ms cap — where every suppressed pass after it stays, so the
/// final state is stable however late the stop lands. (The first
/// pass's own complete outputs are the sibling cases' to pin: same
/// setup, and this one's second pass follows in ~100 ms.) The one
/// POST is pinned whole, the final state by `pin_ring_state`, the id
/// records' exact bytes, and the alarm the first pass fired pinned
/// cleared by every later pass (the mail is delivered, nothing old is
/// owed).
#[test]
fn the_cached_deadline_wakes_the_re_ring_wait() {
    let hook = FakeWebhook::start();
    let root = kotek_board(&hook.url());
    hq_mail(&root, "ring me", "abcdef12", "inbox", false);
    let dir = kotek_dir(&root);
    let cfg = payload_cfg(&hook.url());
    let waits = RingWaits {
        first: Duration::from_millis(50),
        max: Duration::from_millis(100),
    };
    let done = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let (r, d, s) = (root.clone(), Arc::clone(&done), Arc::clone(&stop));
    let start = chrono::Utc::now();
    std::thread::spawn(move || {
        webhook::worker_loop(r, "kotek-grokbot".into(), s, waits, Some(cfg));
        d.store(true, Ordering::SeqCst);
    });
    wait_for(|| hook.hits().len() == 1, "the first ring never happened");
    wait_for(
        || {
            load_state(&dir)
                .ring
                .as_ref()
                .is_some_and(|r| r.backoff_ms == 100)
        },
        "the cached deadline did not wake the wait",
    );
    stop.store(true, Ordering::SeqCst);
    wait_for(|| done.load(Ordering::SeqCst), "the worker never exited");
    assert_eq!(hook.hits().len(), 1, "the delivered id never rides twice");
    pin_payload_post(&hook, 0, "abcdef12", "ring me");
    pin_ring_state(
        &dir,
        &[format!("{MAIL_TS}-abcdef12.msg")],
        100,
        start + chrono::TimeDelta::from_std(Duration::from_millis(50)).unwrap(),
        chrono::Utc::now(),
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("webhook-delivered.json")).unwrap(),
        "[\"abcdef12\"]\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("webhook-attempted.json")).unwrap(),
        "[\"abcdef12\"]\n"
    );
    pin_log(
        &root,
        &[
            "alarm seat=kotek-grokbot cause=undelivered an item has waited over 15 minutes undelivered",
            "delivered seat=kotek-grokbot id=abcdef12 mode=payload",
        ],
    );
    assert_eq!(
        std::fs::read_to_string(root.join("dispatch-alarms.json")).unwrap(),
        "{\"causes\":[]}\n",
        "every pass after the first cleared the latch"
    );
}

/// The wait reads an unreadable deadline as not yet due — the caller
/// difference `due_at` does not share, where an unreadable deadline in
/// an open burst is due: a malformed `next_at` rewritten into
/// `dispatch.json` while the worker sleeps never wakes it, awake or
/// asleep, cached or reloaded (none of those read an instant out of
/// it). Whether the wait caches its deadline at all is the sibling
/// readable case below; this one pins that the rewritten bytes stand
/// exactly, with the first pass's POST, state, id records, the complete
/// log (the undelivered alarm and the first pass's `delivered` line)
/// and its still-latched cause (no second pass runs to clear it) all
/// pinned whole.
#[test]
fn an_unreadable_deadline_rewritten_on_disk_never_wakes_the_wait() {
    let hook = FakeWebhook::start();
    let root = kotek_board(&hook.url());
    hq_mail(&root, "ring me", "abcdef12", "inbox", false);
    let dir = kotek_dir(&root);
    let cfg = payload_cfg(&hook.url());
    let done = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let (r, d, s) = (root.clone(), Arc::clone(&done), Arc::clone(&stop));
    let start = chrono::Utc::now();
    std::thread::spawn(move || {
        webhook::worker_loop(r, "kotek-grokbot".into(), s, hour_waits(), Some(cfg));
        d.store(true, Ordering::SeqCst);
    });
    wait_for(|| hook.hits().len() == 1, "the first ring never happened");
    wait_for(
        || load_state(&dir).ring.is_some(),
        "the first pass never recorded its burst",
    );
    pin_payload_post(&hook, 0, "abcdef12", "ring me");
    pin_ring_state(
        &dir,
        &[format!("{MAIL_TS}-abcdef12.msg")],
        3_600_000,
        start,
        chrono::Utc::now(),
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("webhook-delivered.json")).unwrap(),
        "[\"abcdef12\"]\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("webhook-attempted.json")).unwrap(),
        "[\"abcdef12\"]\n"
    );
    // Readable JSON, unreadable timestamp.
    let mut state = load_state(&dir);
    state.ring.as_mut().unwrap().next_at = Some("not-a-timestamp".into());
    crate::dstate::store_state(&dir, &state).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        std::fs::read_to_string(dir.join("dispatch.json")).unwrap(),
        format!(
            "{{\n  \"items\": {{}},\n  \"ring\": {{\n    \"rung\": [\n      \"{MAIL_TS}-abcdef12.msg\"\n    ],\n    \"next_at\": \"not-a-timestamp\",\n    \"backoff_ms\": 3600000,\n    \"failures\": 0\n  }}\n}}\n"
        ),
        "no pass ran over the unreadable rewrite: the bytes stand exactly"
    );
    stop.store(true, Ordering::SeqCst);
    wait_for(|| done.load(Ordering::SeqCst), "the worker never exited");
    assert_eq!(hook.hits().len(), 1);
    assert_eq!(
        std::fs::read_to_string(dir.join("webhook-delivered.json")).unwrap(),
        "[\"abcdef12\"]\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("webhook-attempted.json")).unwrap(),
        "[\"abcdef12\"]\n"
    );
    pin_log(
        &root,
        &[
            "alarm seat=kotek-grokbot cause=undelivered an item has waited over 15 minutes undelivered",
            "delivered seat=kotek-grokbot id=abcdef12 mode=payload",
        ],
    );
    assert_eq!(
        std::fs::read_to_string(root.join("dispatch-alarms.json")).unwrap(),
        "{\"causes\":[\"undelivered:kotek-grokbot\"]}\n",
        "no second pass ran: the first ring's latch stands"
    );
}

/// The wait caches its deadline from the pass that opened it: a
/// readable, long-expired `next_at` rewritten into `dispatch.json`
/// while the worker sleeps never wakes it — the cached hour is the
/// only instant that counts. A worker that reloaded its deadline every
/// slice would read the expired instant, wake within a slice or two
/// and run a pass (rewriting `next_at` to the future), so the expired
/// bytes surviving is exactly what distinguishes caching from
/// reloading. One startup race exists — the rewrite can land between
/// the pass's `store_state` and the worker's deadline read, which also
/// runs a single pass — so on finding the rewrite overwritten the test
/// lets the worker settle into its new hour and rewrites once more:
/// only a real reload wakes twice. Every output is
/// pinned whole (the POST, the state after the first pass, the id
/// records, the complete log — the undelivered alarm and the first
/// pass's `delivered` line); the alarm latch is the one bit the race
/// pass moves (it clears it), so both its values are accepted.
#[test]
fn a_readable_expired_deadline_rewritten_on_disk_never_wakes_the_wait() {
    const EXPIRED: &str = "2000-01-01T00:00:00Z";
    let hook = FakeWebhook::start();
    let root = kotek_board(&hook.url());
    hq_mail(&root, "ring me", "abcdef12", "inbox", false);
    let dir = kotek_dir(&root);
    let cfg = payload_cfg(&hook.url());
    let done = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let (r, d, s) = (root.clone(), Arc::clone(&done), Arc::clone(&stop));
    let start = chrono::Utc::now();
    std::thread::spawn(move || {
        webhook::worker_loop(r, "kotek-grokbot".into(), s, hour_waits(), Some(cfg));
        d.store(true, Ordering::SeqCst);
    });
    wait_for(|| hook.hits().len() == 1, "the first ring never happened");
    wait_for(
        || load_state(&dir).ring.is_some(),
        "the first pass never recorded its burst",
    );
    pin_payload_post(&hook, 0, "abcdef12", "ring me");
    pin_ring_state(
        &dir,
        &[format!("{MAIL_TS}-abcdef12.msg")],
        3_600_000,
        start,
        chrono::Utc::now(),
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("webhook-delivered.json")).unwrap(),
        "[\"abcdef12\"]\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("webhook-attempted.json")).unwrap(),
        "[\"abcdef12\"]\n"
    );
    for round in 0..2 {
        let mut state = load_state(&dir);
        state.ring.as_mut().unwrap().next_at = Some(EXPIRED.into());
        crate::dstate::store_state(&dir, &state).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        if load_state(&dir).ring.and_then(|r| r.next_at) == Some(EXPIRED.to_string()) {
            break; // no pass ran over the expired rewrite
        }
        assert!(
            round == 0,
            "passes ran over the rewritten expired deadline twice: \
             the wait reloads its deadline instead of caching it"
        );
        // The pass was the startup race: the rewrite landed between
        // the pass's store and the worker's deadline read. The worker
        // has long since cached a fresh hour — rewrite into that wait.
        std::thread::sleep(Duration::from_millis(100));
    }
    stop.store(true, Ordering::SeqCst);
    wait_for(|| done.load(Ordering::SeqCst), "the worker never exited");
    assert_eq!(hook.hits().len(), 1, "the delivered id never rides twice");
    assert_eq!(
        std::fs::read_to_string(dir.join("dispatch.json")).unwrap(),
        format!(
            "{{\n  \"items\": {{}},\n  \"ring\": {{\n    \"rung\": [\n      \"{MAIL_TS}-abcdef12.msg\"\n    ],\n    \"next_at\": \"{EXPIRED}\",\n    \"backoff_ms\": 3600000,\n    \"failures\": 0\n  }}\n}}\n"
        ),
        "no pass ran over the expired rewrite: the bytes stand exactly"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("webhook-delivered.json")).unwrap(),
        "[\"abcdef12\"]\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("webhook-attempted.json")).unwrap(),
        "[\"abcdef12\"]\n"
    );
    pin_log(
        &root,
        &[
            "alarm seat=kotek-grokbot cause=undelivered an item has waited over 15 minutes undelivered",
            "delivered seat=kotek-grokbot id=abcdef12 mode=payload",
        ],
    );
    let alarms = std::fs::read_to_string(root.join("dispatch-alarms.json")).unwrap();
    assert!(
        alarms == "{\"causes\":[\"undelivered:kotek-grokbot\"]}\n" || alarms == "{\"causes\":[]}\n",
        "the first ring's latch, or cleared by the race pass: {alarms}"
    );
}

/// Write the ingest env file (`KEY=VALUE` lines) into the current
/// (throwaway) HOME.
fn ingest_env_file() {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .expect("HOME set");
    let dir = home.join(".config").join("bll-ingest");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("env"),
        "# bll ingest credentials\nACCESS_CLIENT_ID=cf-client-123\nACCESS_CLIENT_SECRET=cf-secret-456\nOTHER=ignored\n",
    )
    .unwrap();
}

fn ack_cfg(hook_url: &str, ack_url: &str) -> WebhookConfig {
    WebhookConfig::mem(
        hook_url.into(),
        KEY.into(),
        None,
        true,
        Some(ack_url.into()),
    )
}

/// The hq delivery receipt, the doorbell's `report_delivery` ported
/// unchanged (the seat marker `webhook.hq-ack` in place of
/// `KOTEK_HQ_ACK=1`): exactly one receipt POST after each successful
/// payload delivery — `{"seat", "message_id"}` with the two Access
/// headers from `~/.config/bll-ingest/env`, User-Agent
/// `bll-seat-mail/1 (Slix)` — and none without the marker. The
/// marker-to-URL wiring goes through `WebhookConfig::load` itself:
/// HOME is a throwaway so the operator's real env file is never read,
/// and since the local receivers cannot speak https (load refuses a
/// `webhook.url` that does not start `https://`), the url stages a
/// placeholder and both POST targets are pointed at the receivers
/// after the load — load's on-disk decisions (payload marker,
/// hq-ack marker) are the production ones. The secrets never reach
/// the log.
#[test]
fn hq_receipt_follows_each_payload_delivery_when_marked() {
    crate::testutil::with_throwaway_home(
        hq_receipt_follows_each_payload_delivery_when_marked_isolated,
    );
}

fn hq_receipt_follows_each_payload_delivery_when_marked_isolated() {
    let hook = FakeWebhook::start(); // the payload POSTs
    let hq = FakeWebhook::start(); // the receipt POSTs
    let root = kotek_board(&hook.url());
    ingest_env_file();
    std::fs::write(kotek_dir(&root).join("webhook.hq-ack"), b"").unwrap();
    std::fs::write(
        kotek_dir(&root).join("webhook.url"),
        "https://panel.invalid/\n",
    )
    .unwrap();
    hq_mail(&root, "hello Mariola", "abcdef12", "inbox", false);
    let mut cfg = WebhookConfig::load(&kotek_dir(&root)).unwrap();
    assert!(cfg.payload_mode, "the payload marker loaded");
    assert_eq!(
        cfg.hq_ack_url.as_deref(),
        Some("https://hq.bll.dev/ingest/seat-delivery"),
        "the hq-ack marker stages the ingest URL"
    );
    cfg.url = hook.url();
    cfg.hq_ack_url = Some(hq.url()); // the receiver stands in for the ingest
    assert_eq!(
        webhook::ring_once(&root, "kotek-grokbot", &cfg, &no_sleep).unwrap(),
        true
    );
    assert_eq!(hook.hits().len(), 1, "the payload POST");
    assert_eq!(hq.hits().len(), 1, "exactly one receipt");
    let (headers, body) = hq.hits()[0].clone();
    // The doorbell's json.dumps default separators, field for field.
    assert_eq!(
        body, "{\"seat\": \"kotek-grokbot\", \"message_id\": \"abcdef12\"}",
        "the exact receipt body"
    );
    assert!(
        headers.contains("user-agent: bll-seat-mail/1 (Slix)\n"),
        "{headers}"
    );
    assert!(
        headers.contains("cf-access-client-id: cf-client-123\n"),
        "{headers}"
    );
    assert!(
        headers.contains("cf-access-client-secret: cf-secret-456\n"),
        "{headers}"
    );
    assert!(
        headers.contains("content-type: application/json\n"),
        "{headers}"
    );
    // A delivered id never rides twice, and no stray receipt follows.
    assert_eq!(
        webhook::ring_once(&root, "kotek-grokbot", &cfg, &no_sleep).unwrap(),
        false
    );
    assert_eq!(hq.hits().len(), 1);
    let log = std::fs::read_to_string(root.join("dispatch.log")).unwrap_or_default();
    assert!(
        !log.contains("cf-client-123") && !log.contains("cf-secret-456"),
        "secrets in the log: {log}"
    );

    // Without the marker load stages no receipt URL at all, and the
    // delivery sends none.
    std::fs::remove_file(kotek_dir(&root).join("webhook.hq-ack")).unwrap();
    hq_mail(&root, "second mail", "999999ab", "inbox", false);
    let mut plain = WebhookConfig::load(&kotek_dir(&root)).unwrap();
    assert!(plain.hq_ack_url.is_none(), "no marker: no receipt URL");
    plain.url = hook.url();
    assert_eq!(
        webhook::ring_once(&root, "kotek-grokbot", &plain, &no_sleep).unwrap(),
        true
    );
    assert_eq!(hook.hits().len(), 2, "the second payload POST");
    assert_eq!(hq.hits().len(), 1, "no marker: no receipt");
}

/// The receipt is best effort, exactly as in the doorbell: a refused
/// (or unsendable) receipt is one attempt, never resent, and the
/// payload delivery stands — the id is delivered and the ring
/// succeeded. HOME is a throwaway; the receipt URL is the local
/// receiver.
#[test]
fn a_failed_hq_receipt_is_best_effort_and_never_resent() {
    crate::testutil::with_throwaway_home(
        a_failed_hq_receipt_is_best_effort_and_never_resent_isolated,
    );
}

fn a_failed_hq_receipt_is_best_effort_and_never_resent_isolated() {
    let hook = FakeWebhook::start();
    let hq = FakeWebhook::start();
    hq.ctl.lock().unwrap().status = 500; // the ingest refuses
    let root = kotek_board(&hook.url());
    ingest_env_file();
    hq_mail(&root, "uncertain receipt", "abcdef12", "inbox", false);
    let cfg = ack_cfg(&hook.url(), &hq.url());
    assert_eq!(
        webhook::ring_once(&root, "kotek-grokbot", &cfg, &no_sleep).unwrap(),
        true,
        "the delivery stands: a failed receipt is not a failed ring"
    );
    assert_eq!(
        webhook::id_list(&kotek_dir(&root).join("webhook-delivered.json")),
        ["abcdef12".to_string()],
        "the id is delivered"
    );
    assert_eq!(hq.hits().len(), 1, "one receipt attempt");
    // Never resent: nothing at all is sent for a delivered id.
    assert_eq!(
        webhook::ring_once(&root, "kotek-grokbot", &cfg, &no_sleep).unwrap(),
        false
    );
    assert_eq!(hq.hits().len(), 1, "the failed receipt is not retried");
    // One failure line, no detail, no secrets.
    let log = std::fs::read_to_string(root.join("dispatch.log")).unwrap_or_default();
    assert!(log.contains("hq receipt abcdef12 failed"), "{log}");
    assert!(
        !log.contains("cf-secret-456") && !log.contains("cf-client-123"),
        "{log}"
    );

    // Missing credentials: the receipt is skipped, the delivery still
    // stands.
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .expect("HOME set");
    std::fs::remove_file(home.join(".config").join("bll-ingest").join("env")).unwrap();
    hq_mail(&root, "no credentials", "87654321", "inbox", false);
    assert_eq!(
        webhook::ring_once(&root, "kotek-grokbot", &cfg, &no_sleep).unwrap(),
        true
    );
    assert!(
        webhook::id_list(&kotek_dir(&root).join("webhook-delivered.json"))
            .contains(&"87654321".to_string()),
        "delivered without a receipt"
    );
    assert_eq!(hq.hits().len(), 1, "no receipt without credentials");
}

/// A missing ingest env names the exact path it could not read:
/// `$HOME/.config/bll-ingest/env`, HOME read with the `.` fallback,
/// no other spelling.
#[test]
fn a_missing_ingest_env_names_the_home_path() {
    crate::testutil::with_throwaway_home(|| {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .expect("HOME set");
        let err =
            webhook::hq_receipt("https://hq.invalid/", "kotek-grokbot", "abcdef12").unwrap_err();
        assert_eq!(
            err,
            format!(
                "no readable {}: No such file or directory (os error 2)",
                home.join(".config")
                    .join("bll-ingest")
                    .join("env")
                    .display()
            ),
            "the error names the HOME-derived path"
        );
    });
}
