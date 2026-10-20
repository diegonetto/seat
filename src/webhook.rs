//! The `webhook` adapter (item 4): the Grok bots drain the board
//! themselves, so it never runs `receive` and never archives — it
//! rings, and re-rings (1 min doubling to 1 h) until every rung
//! message has left `inbox/` and `draining/`. Two modes, byte for
//! byte against today's scripts: **signal** (eng-grokbot,
//! sales-grokbot; `wake-grokbot`) POSTs `{"schema_version":1,
//! "seat":..,"ts":..,"nonce":..}` HMAC'd with `bridge.key`, never a
//! message body; **payload** (kotek-grokbot; `grok-doorbell.py`;
//! enabled by `<seat>/webhook.payload`) carries verified direct mail
//! from the `inbox` seat in the body at most once per message id,
//! with its attempted/delivered records; everything else gets the
//! signal.
use crate::alarm;
use crate::dstate::{atomic_write_private, load_state, log_line, store_state, RingState};
use crate::error::{Result, SeatError};
use crate::identity::Bindings;
use crate::msg;
use crate::seat;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const SLICE: Duration = Duration::from_millis(100);
const MAX_BODY: usize = 8192; // the doorbell's cap on a message it carries
const SUPPRESSED: &str = "no POST sent: an attempted hq message is still unread";

/// Re-ring backoff (1 min doubling to 1 h; tests pass something tiny).
#[derive(Debug, Clone, Copy)]
pub(crate) struct RingWaits {
    pub first: Duration,
    pub max: Duration,
}

pub(crate) const PROD_RING_WAITS: RingWaits = RingWaits {
    first: Duration::from_secs(60),
    max: Duration::from_secs(60 * 60),
};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RingOutcome {
    Rang,    // a POST went out; the burst is open
    Waiting, // burst open, next ring not due yet
    Idle,    // nothing to ring: the bot's drain took everything
}

pub(crate) struct WebhookConfig {
    pub(crate) url: String,
    pub(crate) key: String,
    pub(crate) authorization: Option<String>,
    pub(crate) payload_mode: bool,
}

/// The first line of a config file, trimmed (None when absent/empty).
fn private_line(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(raw) => Ok(raw
            .lines()
            .next()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(SeatError::Io(e)),
    }
}

impl WebhookConfig {
    /// Fail closed like the scripts: no URL/key staged, or a URL not
    /// starting `https://`, is an error and nothing is sent.
    pub(crate) fn load(dir: &Path) -> Result<WebhookConfig> {
        let url = private_line(&dir.join("webhook.url"))?
            .ok_or_else(|| SeatError::Webhook("no webhook.url staged".into()))?;
        if !url.starts_with("https://") {
            return Err(SeatError::Webhook(
                "webhook.url does not start https://".into(),
            ));
        }
        let key = private_line(&dir.join("bridge.key"))?
            .ok_or_else(|| SeatError::Webhook("no bridge.key staged".into()))?;
        let authorization = private_line(&dir.join("webhook.auth"))?.map(|auth| {
            auth.strip_prefix("Authorization:")
                .map(|rest| rest.trim().to_string())
                .unwrap_or(auth)
        });
        Ok(WebhookConfig {
            url,
            key,
            authorization,
            payload_mode: dir.join("webhook.payload").exists(),
        })
    }

    /// In memory, for tests pointing the POST at a local receiver.
    #[cfg(test)]
    pub(crate) fn mem(
        url: String,
        key: String,
        authorization: Option<String>,
        payload_mode: bool,
    ) -> WebhookConfig {
        WebhookConfig {
            url,
            key,
            authorization,
            payload_mode,
        }
    }
}

/// The signal body, byte for byte the shell script's string.
pub(crate) fn signal_body(seat: &str, ts: &str, nonce: &str) -> String {
    format!("{{\"schema_version\":1,\"seat\":\"{seat}\",\"ts\":\"{ts}\",\"nonce\":\"{nonce}\"}}")
}

/// `X-Memo-Signature`: the full HMAC-SHA256 hex digest of the exact
/// body with `bridge.key`.
pub(crate) fn signature_hex(key: &str, body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("hmac takes any key");
    mac.update(body.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// One POST with the shared headers. A signal retries 3x (the
/// script's `2*attempt` backoff between); a payload POST is attempted
/// exactly once (the doorbell's at-most-once rule).
pub(crate) fn post(
    cfg: &WebhookConfig,
    body: &str,
    attempts: usize,
    sleeper: &dyn Fn(Duration),
) -> std::result::Result<(), String> {
    let agent = ureq::Agent::new_with_config(
        ureq::config::Config::builder()
            .http_status_as_error(false)
            .timeout_global(Some(HTTP_TIMEOUT))
            .build(),
    );
    let sig = signature_hex(&cfg.key, body);
    let mut last = String::new();
    for attempt in 1..=attempts {
        let mut req = agent
            .post(&cfg.url)
            .header("Content-Type", "application/json")
            .header("X-Memo-Signature", format!("sha256={sig}").as_str());
        if let Some(auth) = &cfg.authorization {
            req = req.header("Authorization", auth);
        }
        let failed = match req.send(body) {
            Ok(resp) if (200..300).contains(&resp.status().as_u16()) => return Ok(()),
            Ok(resp) => format!("HTTP {}", resp.status().as_u16()),
            Err(e) => e.to_string(),
        };
        last = failed;
        if attempt < attempts {
            sleeper(Duration::from_secs(2 * attempt as u64));
        }
    }
    Err(format!(
        "webhook failed after {attempts} attempt(s) ({last})"
    ))
}

/// File names in `inbox/` and `draining/`: work the bot's own drain
/// owes (it removes both).
pub(crate) fn present_files(dir: &Path) -> BTreeSet<String> {
    ["inbox", "draining"]
        .iter()
        .flat_map(|stage| msg::list_msg_dir(&dir.join(stage)).unwrap_or_default())
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_string))
        .collect()
}

/// Would the scan give this seat a worker: any mail still present.
pub(crate) fn has_work(root: &Path, seat_name: &str) -> bool {
    !present_files(&seat::seat_dir(root, seat_name)).is_empty()
}

/// Is this seat still webhook-routed? The full routing rule, not just
/// the hapi half: a seat in `hapi-sessions.json` is hapi (even with
/// `webhook.url` staged), a seat whose `webhook.url` is gone is pull —
/// either way this worker must step aside, so the scan can hand the
/// seat to the hapi worker or leave it to drain when it chooses.
pub(crate) fn still_webhook(root: &Path, seat_name: &str) -> bool {
    !Bindings::load(root).contains(seat_name)
        && seat::seat_dir(root, seat_name)
            .join("webhook.url")
            .is_file()
}

fn now_utc() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn nonce_hex() -> Result<String> {
    msg::rand_hex(16)
}

/// The payload body: the doorbell's
/// `json.dumps(body, ensure_ascii=False, separators=(",", ":"))` with
/// its exact field order.
pub(crate) fn payload_body(seat: &str, ts: &str, nonce: &str, id: &str, text: &str) -> String {
    let q = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
    format!(
        concat!(
            "{{\"schema_version\":1,\"seat\":{},\"ts\":{},\"nonce\":{},",
            "\"from\":\"inbox\",\"id\":{},\"text\":{},\"thread\":\"inbox\"}}"
        ),
        q(seat),
        q(ts),
        q(nonce),
        q(id),
        q(text)
    )
}

/// Verified direct mail from the hq `inbox` seat, the doorbell's exact
/// rule: a real file (never a symlink) of at most 8192 bytes, a header
/// with `dest` = this seat, `from` = `inbox`, an 8-lowercase-hex id,
/// and a MAC over `dest|from|id|ts|body` (BOM stripped, no session)
/// keyed by the inbox seat's token. Everything else is None and gets
/// the signal.
pub(crate) fn verified_hq_mail(path: &Path, root: &Path, seat: &str) -> Option<(String, String)> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() || !meta.is_file() || meta.len() > MAX_BODY as u64 {
        return None;
    }
    let (header, body) = msg::read_msg(path).ok()?;
    if header.dest != seat || header.from != "inbox" {
        return None;
    }
    let token = msg::read_token(&root.join("seats").join("inbox").join("token")).ok()?;
    msg::verify_hmac(
        &token,
        &header.dest,
        &header.from,
        &header.id,
        &header.ts,
        body.as_bytes(),
        None,
        &header.hmac,
    )
    .ok()?
    .then(|| (header.id, body))
}

/// The at-most-once records (`webhook-attempted.json`,
/// `webhook-delivered.json` in the seat directory): arrays of ids, the
/// last 1000 kept. The whole file is valid or the list is empty (the
/// doorbell's all()-check).
pub(crate) fn id_list(path: &Path) -> Vec<String> {
    match std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
    {
        Some(list) if list.iter().all(|s| msg::valid_msg_id(s)) => list,
        _ => Vec::new(),
    }
}

fn save_id_list(path: &Path, ids: &[String]) -> Result<()> {
    let keep = &ids[ids.len().saturating_sub(1000)..];
    let mut json = serde_json::to_string(keep)?.into_bytes();
    json.push(b'\n');
    atomic_write_private(path, &json)
}

/// One ring: the doorbell for payload seats, the plain signal
/// otherwise. `Ok(true)` — a POST went out; `Ok(false)` — mail waits
/// but nothing was sent (payload mode's suppression); `Err` — a POST
/// was attempted and failed.
pub(crate) fn ring_once(
    root: &Path,
    seat: &str,
    cfg: &WebhookConfig,
    sleeper: &dyn Fn(Duration),
) -> std::result::Result<bool, String> {
    if !cfg.payload_mode {
        let body = signal_body(seat, &now_utc(), &nonce_hex().map_err(|e| e.to_string())?);
        return post(cfg, &body, 3, sleeper).map(|()| true);
    }
    let dir = seat::seat_dir(root, seat);
    let (attempted_path, delivered_path) = (
        dir.join("webhook-attempted.json"),
        dir.join("webhook-delivered.json"),
    );
    let mut attempted = id_list(&attempted_path);
    let mut delivered = id_list(&delivered_path);
    let seen: BTreeSet<String> = attempted.iter().chain(delivered.iter()).cloned().collect();
    let files = msg::list_msg_dir(&dir.join("inbox")).unwrap_or_default();
    let mut pending: Vec<(String, String)> = Vec::new();
    let mut pending_ids: BTreeSet<String> = BTreeSet::new(); // at-most-once per id
    let mut other_mail = files.is_empty(); // the doorbell's conservative default
    let mut attempted_hq_still_unread = false;
    for path in &files {
        match verified_hq_mail(path, root, seat) {
            Some((id, text)) if !seen.contains(&id) && pending_ids.insert(id.clone()) => {
                pending.push((id, text))
            }
            Some(_) => attempted_hq_still_unread = true,
            None => other_mail = true,
        }
    }
    if pending.is_empty() {
        // While an attempted hq message is still unread, a signal
        // would make the bot drain and answer it twice.
        if other_mail && !attempted_hq_still_unread {
            let body = signal_body(seat, &now_utc(), &nonce_hex().map_err(|e| e.to_string())?);
            return post(cfg, &body, 3, sleeper).map(|()| true);
        }
        return Ok(false);
    }
    let mut failures = Vec::new();
    for (id, text) in pending {
        // Attempt-before-send, durable: a record that cannot be
        // persisted stops the POST — a restart must not carry it twice.
        attempted.push(id.clone());
        save_id_list(&attempted_path, &attempted)
            .map_err(|e| format!("recording the attempt failed: {e}"))?;
        let body = payload_body(
            seat,
            &now_utc(),
            &nonce_hex().map_err(|e| e.to_string())?,
            &id,
            &text,
        );
        match post(cfg, &body, 1, sleeper) {
            Ok(()) => {
                delivered.push(id.clone());
                save_id_list(&delivered_path, &delivered)
                    .map_err(|e| format!("recording the delivery failed: {e}"))?;
                log_line(root, "delivered", seat, &format!("id={id} mode=payload"));
            }
            Err(e) => failures.push(format!("{id}: {e}")),
        }
    }
    if failures.is_empty() {
        Ok(true)
    } else {
        Err(failures.join("; "))
    }
}

/// Strikes 1 and 2 of the failed-ring ladder log one failure line
/// each; strike 3 is the `webhook-failing` alarm (once per cause).
fn strike(root: &Path, seat: &str, n: u32, detail: &str) -> Result<()> {
    if n >= 3 {
        alarm::fire(
            root,
            Some(seat),
            alarm::WEBHOOK_FAILING,
            &format!("{n} rings failed in a row ({detail})"),
        )
    } else {
        log_line(
            root,
            "failure",
            seat,
            &format!("ring attempt {n} failed: {detail}"),
        );
        Ok(())
    }
}

fn due_at(ring: &RingState, now: chrono::DateTime<chrono::Utc>) -> bool {
    !ring.rung.is_empty()
        && ring
            .next_at
            .as_deref()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .is_none_or(|at| now >= at.with_timezone(&chrono::Utc))
}

/// One pass for one webhook seat: ring on a burst of new mail, re-ring
/// with the doubling backoff until every rung message has left
/// `inbox/` and `draining/`. Three failed rings in a row are the
/// `webhook-failing` alarm; mail older than 15 minutes is
/// `undelivered` (both once per cause).
pub(crate) fn ring_pass(
    root: &Path,
    seat: &str,
    cfg: &WebhookConfig,
    waits: RingWaits,
    sleeper: &dyn Fn(Duration),
) -> Result<RingOutcome> {
    let dir = seat::seat_dir(root, seat);
    let mut state = load_state(&dir);
    let mut ring = state.ring.clone().unwrap_or_default();
    let now = chrono::Utc::now();
    let present = present_files(&dir);
    let delivered: BTreeSet<String> = if cfg.payload_mode {
        id_list(&dir.join("webhook-delivered.json"))
            .into_iter()
            .collect()
    } else {
        BTreeSet::new()
    };
    alarm::undelivered(root, seat, &delivered, &present)?;
    // Prune what the bot's drain took; an empty rung set closes the
    // burst and resets the backoff.
    ring.rung.retain(|f| present.contains(f));
    if ring.rung.is_empty() {
        ring = RingState::default();
    }
    let rung: BTreeSet<&String> = ring.rung.iter().collect();
    let has_new = present.iter().any(|f| !rung.contains(f));
    if !has_new && !due_at(&ring, now) {
        if state.ring != Some(ring.clone()) {
            state.ring = (!ring.rung.is_empty()).then_some(ring);
            store_state(&dir, &state)?;
        }
        return Ok(if state.ring.is_none() {
            RingOutcome::Idle
        } else {
            RingOutcome::Waiting
        });
    }
    let mut rang = false;
    match ring_once(root, seat, cfg, sleeper) {
        Ok(true) => {
            ring.failures = 0;
            alarm::clear(root, Some(seat), alarm::WEBHOOK_FAILING)?;
            rang = true;
        }
        Err(detail) => {
            ring.failures += 1;
            strike(root, seat, ring.failures, &detail)?;
        }
        // Suppression after a failed POST in this burst: still failing.
        Ok(false) if ring.failures > 0 => {
            ring.failures += 1;
            strike(root, seat, ring.failures, SUPPRESSED)?;
        }
        Ok(false) => {} // the doorbell working: no POST, no failure
    }
    ring.rung = present.into_iter().collect();
    let base = if ring.backoff_ms == 0 {
        waits.first
    } else {
        Duration::from_millis(ring.backoff_ms).saturating_mul(2)
    };
    let backoff = base.min(waits.max);
    ring.backoff_ms = backoff.as_millis() as u64;
    ring.next_at =
        Some((now + chrono::TimeDelta::from_std(backoff).unwrap_or_default()).to_rfc3339());
    state.ring = Some(ring);
    store_state(&dir, &state)?;
    Ok(if rang {
        RingOutcome::Rang
    } else {
        RingOutcome::Waiting
    })
}

/// The worker body: pass after pass while the burst is open, sleeping
/// to the next re-ring and woken at once by new mail. Idle, a broken
/// config or a route change (the seat became hapi-bound — the scan
/// spawns the hapi worker instead — or `webhook.url` was removed, the
/// seat falling back to pull) ends it; the scan retries in 2 s. The
/// first pass always runs even with stop already set (`--once` sets
/// stop immediately after spawning, and the worker it spawned must
/// still ring once before it returns); stop ends the loop only after
/// a completed pass. `cfg` overrides the on-disk config (tests point
/// the POST at a local receiver).
pub(crate) fn worker_loop(
    root: PathBuf,
    seat: String,
    stop: Arc<AtomicBool>,
    waits: RingWaits,
    cfg: Option<WebhookConfig>,
) {
    let dir = seat::seat_dir(&root, &seat);
    let loaded = cfg.map_or_else(|| WebhookConfig::load(&dir), Ok);
    let cfg = match loaded {
        Ok(cfg) => cfg, // fail closed like the scripts: nothing is sent
        Err(e) => {
            log_line(&root, "failure", &seat, &e.to_string());
            return;
        }
    };
    let sleeper = |d: Duration| std::thread::sleep(d);
    let mut first = true;
    loop {
        if (!first && stop.load(Ordering::SeqCst)) || !still_webhook(&root, &seat) {
            return;
        }
        match ring_pass(&root, &seat, &cfg, waits, &sleeper) {
            Ok(RingOutcome::Idle) => return,
            Ok(_) => {}
            Err(e) => {
                log_line(&root, "failure", &seat, &e.to_string());
                return;
            }
        }
        // A pass completed: stop may end the loop from here on.
        first = false;
        // Sleep to the next re-ring, woken early by new mail, a route
        // change or stop; the burst closing externally also wakes.
        let deadline = load_state(&dir)
            .ring
            .as_ref()
            .and_then(|r| r.next_at.as_deref())
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok());
        loop {
            if stop.load(Ordering::SeqCst) || !still_webhook(&root, &seat) {
                return;
            }
            let state = load_state(&dir);
            let rung: BTreeSet<&String> = state
                .ring
                .as_ref()
                .map(|r| r.rung.iter().collect())
                .unwrap_or_default();
            let present = present_files(&dir);
            if present.is_empty()
                || present.iter().any(|f| !rung.contains(f))
                || deadline.is_some_and(|d| chrono::Utc::now() >= d.with_timezone(&chrono::Utc))
            {
                break;
            }
            std::thread::sleep(SLICE);
        }
    }
}
