//! `seat dispatch`: the one service delivering the board's mail.
//! Routing is by configuration alone, at the seam in `run`: a seat in
//! `hapi-sessions.json` is `hapi` (its pass body below, client in
//! `hub`), else a seat directory holding `webhook.url` is `webhook`
//! (`webhook` — no hub needed), else pull — no push, the seat drains
//! when it chooses. The alarm hooks (item 6) live in the hapi pass
//! where it is.
use crate::alarm;
use crate::board;
use crate::drain::{self, Received};
use crate::dstate::{
    direct_key, direct_text, key_base, load_state, log_line, next_delivery_key, store_state,
    DispatchState, ItemState, LastDelivery, Phase, WakeHold, Woken,
};
use crate::error::{Result, SeatError};
use crate::hub::{Hub, HubConfig};
use crate::identity::Bindings;
use crate::msg;
use crate::seat::{self, SeatLock};
use crate::sleep::{self, WakeOutcome};
use crate::verbs::sorted_seat_names;
use crate::webhook;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

const SCAN_EVERY: Duration = Duration::from_secs(2);
const QUEUED_CHUNK: usize = 1000; // localIds per queued-state call (hub limit)
const SLICE: Duration = Duration::from_millis(100);

/// The worker's queued-state poll backoff (5 s doubling to 5 min in
/// production; tests pass something tiny).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Waits {
    pub first: Duration,
    pub max: Duration,
}

const PROD_WAITS: Waits = Waits {
    first: Duration::from_secs(5),
    max: Duration::from_secs(5 * 60),
};

/// What one worker pass ended with.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Pass {
    Idle,
    Delivered,
    Waiting,
    Blocked,
}

/// One pass of the hapi adapter for one seat: an asleep seat with
/// mail wakes first (its worker's first act), then receive under the
/// lock, update the state, gate on the session, POST the staged
/// items, then one queued-state round. An idle seat sleeps here too —
/// never in `fresh_work`.
pub(crate) fn pass_seat(root: &Path, seat: &str, hub: &mut Hub) -> Result<Pass> {
    let dir = seat::seat_dir(root, seat);
    let mut state = load_state(&dir);
    // An asleep seat: nothing owed means nothing to do. A pane fact
    // beside the mark is a sleep whose kill never landed — a crash or
    // a failed kill between recording the sleep and stopping the pane
    // — settled first, before anything else (the wake needs the pane
    // down, and the settlement decides whether this pane is still the
    // one to kill). A pane that came back after a completed sleep has
    // no pane fact: the early return below ends the sleep — the mark
    // clears and that pane starts a fresh clock.
    if state.asleep.is_some() && state.pane_seen_at.is_some() {
        sleep::settle_pending_sleep(root, seat, &mut state, hub)?;
    }
    if state.asleep.is_some() {
        let owed = webhook::present_files(&dir);
        alarm::undelivered(root, seat, &BTreeSet::new(), &owed)?;
        let in_flight = state.items.values().any(|i| i.phase != Phase::Done);
        if owed.is_empty() && !in_flight {
            // A completed sleep (the pane fact is already clear) with
            // a live pane back beside the mark: someone brought the
            // seat back — `seat up`, a resume. The sleep is over: the
            // mark clears, this pane starts a fresh clock on its own
            // id, and it is never killed for the old decision.
            if state.pane_seen_at.is_none() {
                if let Some(Some(id)) = crate::session::live_pane_id(seat) {
                    state.asleep = None;
                    state.wake_hold = None;
                    state.pane_seen_at = Some(seat::now_ts());
                    state.pane_id = Some(id);
                    store_state(&dir, &state)?;
                }
            }
            return Ok(Pass::Idle);
        }
        // The script's `up` answer holds the wake: while the pane it
        // started stands the script is not called again — `up` means
        // stop calling — and only a pane that is verifiably gone
        // re-arms it. `busy`, `refused`, and `failed` stay next-pass
        // retries (the failure line was written when the hold was).
        if state.wake_hold.is_some() && crate::session::pane_is_up(seat) != Some(false) {
            return Ok(Pass::Blocked);
        }
        if state.wake_hold.is_some() {
            state.wake_hold = None; // the pane is gone: the wake may run again
            store_state(&dir, &state)?;
        }
        let key = wake_key(&dir, seat, &state);
        match sleep::wake_seat(root, seat) {
            WakeOutcome::Awake => {
                state.asleep = None;
                state.wake_hold = None;
                state.woken = Some(Woken {
                    at: seat::now_ts(),
                    key,
                });
                store_state(&dir, &state)?; // before any POST
                log_line(root, "wake", seat, "seat-role wake answered awake");
            }
            // busy, refused, failed, or no binding after it: the seat
            // stays asleep, the item stays in the inbox, and the next
            // scan retries (the failure is reported like an
            // undelivered item, naming the script's reason).
            WakeOutcome::Failed(reason) => {
                log_line(
                    root,
                    "failure",
                    seat,
                    &format!("wake failed: {reason}; mail stays in the inbox"),
                );
                return Ok(Pass::Blocked);
            }
            // The script's `up`: a pane runs with no phone session.
            // The seat stays asleep, the mail stays, and the wake is
            // held — not retried — until that pane is verifiably gone.
            WakeOutcome::Up(reason) => {
                state.wake_hold = Some(WakeHold {
                    at: seat::now_ts(),
                    reason: reason.clone(),
                });
                store_state(&dir, &state)?;
                log_line(
                    root,
                    "failure",
                    seat,
                    &format!(
                        "wake failed: {reason}; mail stays in the inbox while the pane stands"
                    ),
                );
                return Ok(Pass::Blocked);
            }
        }
    }
    // Pull seats drain themselves: never `receive` for them. The
    // binding is re-read here, after any wake rewrote it.
    let Some(session) = Bindings::load(root).session_id(seat).map(str::to_string) else {
        log_line(
            root,
            "failure",
            seat,
            "no binding in hapi-sessions.json; mail stays staged",
        );
        return Ok(Pass::Blocked);
    };
    // The `undelivered` cause, by message age (hapi has no
    // payload-delivered record; committed mail is archived away): mail
    // sitting in `inbox/` or `draining/` — the seat's own files —
    // older than 15 minutes by its verified header `ts`.
    let owed = webhook::present_files(&dir);
    alarm::undelivered(root, seat, &BTreeSet::new(), &owed)?;
    // The idle clock's pane fact: the first pass a pane is seen starts
    // the clock on that pane's own id, a gone pane clears it, and a
    // live pane the clock was not timing — `seat up`, the gallery, or
    // a down-and-up that landed inside one 2 s scan — starts the
    // clock over, so a pane that just started is never killed on the
    // previous pane's old timestamp. A multiplexer that cannot be
    // asked changes nothing (unknown is never permission to sleep).
    let mut pane_fact_changed = false;
    match crate::session::live_pane_id(seat) {
        Some(Some(id)) if state.pane_id.as_deref() != Some(id.as_str()) => {
            state.pane_seen_at = Some(seat::now_ts());
            state.pane_id = Some(id);
            pane_fact_changed = true;
        }
        Some(None) if state.pane_seen_at.is_some() => {
            state.pane_seen_at = None;
            state.pane_id = None;
            pane_fact_changed = true;
        }
        _ => {}
    }
    // The lock covers receive only; a manual drain can run between
    // receive and commit.
    let received = {
        let _lock = SeatLock::acquire(&dir, seat)?;
        drain::receive(root, seat)?
    };
    if !received.bad_marks.is_empty() || received.unreadable {
        log_line(
            root,
            "failure",
            seat,
            "unreadable or badly marked mail was rejected",
        );
    }
    let mut texts: BTreeMap<String, String> = BTreeMap::new();
    for d in &received.directs {
        texts.insert(direct_key(seat, &d.file_name), direct_text(d));
    }
    // A key whose file is gone (a manual drain took it) is pruned; a
    // rotated key (a generation suffix) is its file's key too, so
    // exactly one key per present file stays in flight. Item identity
    // is the file; the delivery key is the hub `localId` and may be
    // rotated (`next_delivery_key`).
    let before_prune = state.items.len();
    state.items.retain(|k, _| texts.contains_key(key_base(k)));
    for t in texts.keys() {
        if !state.items.keys().any(|k| key_base(k) == t.as_str()) {
            state.items.insert(t.clone(), ItemState::default());
        }
    }
    if state.items.is_empty() {
        if before_prune > 0 || pane_fact_changed {
            store_state(&dir, &state)?; // persist the prune / the pane fact
        }
        // Sleep, decided here: a seat past its threshold with no mail,
        // a quiet session, and a role in the kernel seat map.
        sleep::try_sleep(root, seat, &session, &mut state, hub)?;
        return Ok(Pass::Idle);
    }
    // A changed binding re-POSTs in-flight items to the new session.
    if state.session.as_deref() != Some(session.as_str()) {
        for item in state.items.values_mut() {
            if item.phase != Phase::Done {
                item.phase = Phase::Staged;
            }
        }
        state.session = Some(session.clone());
    }
    store_state(&dir, &state)?; // before any POST
                                // An inactive or missing session is never POSTed; the mail stays
                                // staged, recovered by the next pass. Any successful hub
                                // response — this one, active or not — clears the transport
                                // causes (`no-answer`, `hub-auth`).
    let active = hub.session_active(&session)?;
    alarm::clear(root, Some(seat), alarm::NO_ANSWER)?;
    alarm::clear(root, None, alarm::HUB_AUTH)?;
    if active {
        alarm::clear(root, Some(seat), alarm::SESSION_INACTIVE)?;
    } else {
        alarm::fire(
            root,
            Some(seat),
            alarm::SESSION_INACTIVE,
            &format!("session {session} is not active"),
        )?;
        // Items in flight must not commit on an invoked mark the
        // ending session made (see `rotate_in_flight`); one recorded
        // Staged may be mail the hub accepted before a POST loop
        // failed or crashed partway, so every in-flight item rotates.
        if rotate_in_flight(&mut state) {
            store_state(&dir, &state)?;
        }
        log_line(
            root,
            "failure",
            seat,
            &format!("session {session} is not active; mail stays staged"),
        );
        return Ok(Pass::Blocked);
    }
    for (key, item) in state.items.iter_mut() {
        if item.phase == Phase::Staged {
            hub.post_message(&session, key, &texts[key_base(key)])?;
            item.phase = Phase::Posted;
            item.accepted_at = Some(seat::now_ts());
        }
    }
    store_state(&dir, &state)?;
    poll_round(root, seat, hub, &session, &mut state, &received, &texts)
}

/// The delivery key that woke an asleep seat: its oldest present
/// mail (the wake runs before `receive`, so the mail is still in the
/// inbox), else the oldest in-flight key.
fn wake_key(dir: &Path, seat: &str, state: &DispatchState) -> String {
    if let Some(first) = msg::list_msg_dir(&dir.join("inbox"))
        .unwrap_or_default()
        .first()
    {
        return direct_key(
            seat,
            first
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default(),
        );
    }
    state
        .items
        .iter()
        .find(|(_, i)| i.phase != Phase::Done)
        .map(|(k, _)| k.clone())
        .unwrap_or_default()
}

/// A pass found the bound session inactive while items were in
/// flight: move every in-flight item — any phase but Done, Posted
/// included — to a fresh delivery key (`next_delivery_key` — a
/// `localId` the hub has never seen) as Staged, so it is re-POSTed
/// once the session is active again. Posted is not the whole story:
/// the POST loop persists only after it succeeds, so a loop that
/// failed partway (or a crash between POST and store) leaves mail the
/// hub accepted recorded Staged — its key must move too, or the
/// invoked mark the ending session left on it would archive unread
/// mail after a resume. The hub dedupes on `(session, localId)`
/// forever, so the old mark can never commit the new key, and only an
/// invocation observed while the session was active ever does.
/// Returns whether anything moved.
fn rotate_in_flight(state: &mut DispatchState) -> bool {
    let mut moved = false;
    let old = std::mem::take(&mut state.items);
    for (key, item) in old {
        if item.phase != Phase::Done {
            state
                .items
                .insert(next_delivery_key(&key), ItemState::default());
            moved = true;
        } else {
            state.items.insert(key, item);
        }
    }
    moved
}

/// One queued-state round: re-POST ids the hub lost, commit exactly
/// the invoked items — either only while the session is still active
/// (a retry is a POST too). `queued`/`indeterminate` mean wait; absent
/// means re-POST with the same key.
fn poll_round(
    root: &Path,
    seat: &str,
    hub: &mut Hub,
    session: &str,
    state: &mut DispatchState,
    received: &Received,
    texts: &BTreeMap<String, String>,
) -> Result<Pass> {
    let dir = seat::seat_dir(root, seat);
    let keys: Vec<String> = state
        .items
        .iter()
        .filter(|(_, i)| i.phase != Phase::Done)
        .map(|(k, _)| k.clone())
        .collect();
    let mut invoked: BTreeSet<String> = BTreeSet::new();
    let mut absent: Vec<String> = Vec::new();
    for chunk in keys.chunks(QUEUED_CHUNK) {
        let qs = hub.queued_state(session, chunk)?;
        invoked.extend(qs.invoked.iter().filter(|k| chunk.contains(k)).cloned());
        for k in chunk {
            if !qs.queued.contains(k) && !qs.indeterminate.contains(k) && !qs.invoked.contains(k) {
                absent.push(k.clone());
            }
        }
    }
    // The never-archive-unread rule: the session is gone while items
    // are in flight. The hub marks queued messages invoked when a
    // session ends, and a resume can keep the same session id, so an
    // invoked mark observed around an inactive session was made by the
    // ending session — nobody read that mail. Nothing commits on it
    // and nothing re-POSTs on the old keys: every in-flight item gets
    // a fresh delivery key and is re-sent once the session is active
    // again; only an invocation observed while the session was active
    // commits.
    if !keys.is_empty() && !hub.session_active(session)? {
        if rotate_in_flight(state) {
            store_state(&dir, state)?;
        }
        log_line(
            root,
            "failure",
            seat,
            &format!(
                "session {session} is no longer active; in-flight mail moves to a fresh delivery key"
            ),
        );
        return Ok(Pass::Blocked);
    }
    for key in &absent {
        hub.post_message(session, key, &texts[key_base(key)])?;
        if let Some(item) = state.items.get_mut(key) {
            item.accepted_at = Some(seat::now_ts());
        }
    }
    if invoked.is_empty() {
        store_state(&dir, state)?;
        return Ok(Pass::Waiting);
    }
    let invoked_at = seat::now_ts();
    commit_invoked(root, seat, received, state, &invoked)?;
    state.last_delivery = Some(LastDelivery {
        session: session.to_string(),
        invoked_at: invoked_at.clone(),
    });
    for key in &invoked {
        let accepted = state
            .items
            .get(key)
            .and_then(|i| i.accepted_at.clone())
            .unwrap_or_default();
        log_line(
            root,
            "delivered",
            seat,
            &format!("key={key} accepted-at={accepted} invoked-at={invoked_at}"),
        );
    }
    store_state(&dir, state)?;
    Ok(if state.items.values().all(|i| i.phase == Phase::Done) {
        Pass::Delivered
    } else {
        Pass::Waiting
    })
}

/// Commit exactly the invoked items under the seat lock: archive the
/// invoked directs (a file already taken by a manual drain counts as
/// done). A rotated key (a generation suffix) is its file's key.
pub(crate) fn commit_invoked(
    root: &Path,
    seat: &str,
    received: &Received,
    state: &mut DispatchState,
    invoked: &BTreeSet<String>,
) -> Result<()> {
    let dir = seat::seat_dir(root, seat);
    let _lock = SeatLock::acquire(&dir, seat)?;
    for d in &received.directs {
        let base = direct_key(seat, &d.file_name);
        if !invoked.iter().any(|k| key_base(k) == base) {
            continue;
        }
        let to = dir.join("archive").join(&d.file_name);
        if let Err(e) = std::fs::rename(dir.join("draining").join(&d.file_name), &to) {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(e.into());
            }
        }
    }
    seat::update_meta(&dir, |meta| meta.last_drained = Some(seat::now_ts()))?;
    for key in invoked {
        if let Some(item) = state.items.get_mut(key) {
            item.phase = Phase::Done;
        }
    }
    Ok(())
}

/// Where each seat's mail goes, by configuration alone: a seat in
/// `hapi-sessions.json` is hapi (even with `webhook.url`), else a seat
/// directory holding `webhook.url` is webhook, else pull.
pub(crate) fn route_seats(
    root: &Path,
    bindings: &Bindings,
    names: &[String],
) -> (Vec<String>, Vec<String>) {
    let mut hapi = Vec::new();
    let mut hook = Vec::new();
    for seat in names {
        if bindings.contains(seat) {
            hapi.push(seat.clone());
        } else if seat::seat_dir(root, seat).join("webhook.url").is_file() {
            hook.push(seat.clone());
        }
    }
    (hapi, hook)
}

static SHUTDOWN: OnceLock<Arc<AtomicBool>> = OnceLock::new();

extern "C" fn on_term(_sig: libc::c_int) {
    if let Some(flag) = SHUTDOWN.get() {
        flag.store(true, Ordering::SeqCst);
    }
}

/// `seat dispatch [--once]`: scan every 2 s, one worker per routed
/// seat with work; `--once` runs one scan and waits for its workers.
/// SIGTERM (long-running mode only) sets the stop flag the workers
/// observe after every pass and inside every sleep. The whole life of
/// the command — `--once` included — holds an exclusive, non-blocking
/// lock on `<root>/dispatch.lock`: one dispatcher per board, a second
/// exits nonzero with the lock named.
pub fn cmd_dispatch(root: &Path, once: bool) -> Result<()> {
    board::ensure_board(root)?;
    let _guard = seat::DispatchLock::acquire(root)?;
    let stop = SHUTDOWN
        .get_or_init(|| Arc::new(AtomicBool::new(false)))
        .clone();
    run(root, once, &stop, PROD_WAITS)
}

pub(crate) fn run(root: &Path, once: bool, stop: &Arc<AtomicBool>, waits: Waits) -> Result<()> {
    if !once {
        unsafe { libc::signal(libc::SIGTERM, on_term as *const () as libc::sighandler_t) };
    }
    let live: Arc<Mutex<BTreeMap<String, std::thread::JoinHandle<()>>>> =
        Arc::new(Mutex::new(BTreeMap::new()));
    loop {
        // Reap finished workers; a seat whose worker still runs is
        // never spawned twice (the worker itself stays responsive).
        live.lock().unwrap().retain(|_, h| !h.is_finished());
        let names = sorted_seat_names(root)?;
        let (mut hapi_seats, mut webhook_seats) = route_seats(root, &Bindings::load(root), &names);
        // An asleep seat with inbox mail gets a worker even when
        // hapi-sessions.json no longer names it: seat-role's
        // `start_down` pops the binding before `seat up`, so a wake
        // that fails after that point is retried from the asleep
        // record plus the inbox, not from the binding file. The retry
        // outranks a `webhook.url` the seat also holds too — it slept
        // as a hapi seat (hapi wins while bound), so it wakes as one,
        // never through the hook.
        for name in &names {
            if !hapi_seats.contains(name)
                && crate::dstate::is_asleep(root, name)
                && !msg::list_msg_dir(&seat::seat_dir(root, name).join("inbox"))
                    .unwrap_or_default()
                    .is_empty()
            {
                hapi_seats.push(name.clone());
                webhook_seats.retain(|s| s != name);
            }
        }
        // The hub is a hapi concern only: webhook seats spawn with no
        // hub configured at all.
        if !hapi_seats.is_empty() {
            match HubConfig::discover() {
                Ok(cfg) => {
                    let mut live = live.lock().unwrap();
                    for seat in hapi_seats {
                        if !live.contains_key(&seat) {
                            let (r, name, s) = (root.to_path_buf(), seat.clone(), Arc::clone(stop));
                            let c = cfg.clone();
                            let h = std::thread::spawn(move || worker_loop(r, name, c, s, waits));
                            live.insert(seat, h);
                        }
                    }
                }
                Err(e) => {
                    for seat in hapi_seats {
                        log_line(root, "failure", &seat, &e.to_string());
                    }
                }
            }
        }
        for seat in webhook_seats {
            if !live.lock().unwrap().contains_key(&seat) && webhook::has_work(root, &seat) {
                let (r, name, s) = (root.to_path_buf(), seat.clone(), Arc::clone(stop));
                let h = std::thread::spawn(move || {
                    webhook::worker_loop(r, name, s, webhook::PROD_RING_WAITS, None)
                });
                live.lock().unwrap().insert(seat, h);
            }
        }
        if once {
            stop.store(true, Ordering::SeqCst); // workers finish this pass, then stop
            break;
        }
        if stop.load(Ordering::SeqCst) {
            break;
        }
        for _ in 0..(SCAN_EVERY.as_millis() / SLICE.as_millis()) {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(SLICE);
        }
    }
    for (_, h) in std::mem::take(&mut *live.lock().unwrap()) {
        let _ = h.join();
    }
    Ok(())
}

/// Fresh work a waiting worker must not sit behind its backoff for:
/// mail in `inbox/`, or a binding that moved while items are in
/// flight.
fn fresh_work(root: &Path, seat: &str) -> bool {
    let dir = seat::seat_dir(root, seat);
    if !msg::list_msg_dir(&dir.join("inbox"))
        .unwrap_or_default()
        .is_empty()
    {
        return true;
    }
    let state = load_state(&dir);
    if Bindings::load(root).session_id(seat).map(str::to_string) != state.session {
        return true;
    }
    false
}

/// The worker body: pass after pass until idle, blocked, or stopped.
/// A Waiting pass backs off (doubling to waits.max) but is woken by
/// fresh work, so new mail and binding changes are received and
/// POSTed at once — the 2-second scan's promise, held mid-backoff. A
/// failure is logged, classified into the no-answer / hub-auth alarms
/// (once per cause), and ends the worker; the scan retries in 2 s.
pub(crate) fn worker_loop(
    root: PathBuf,
    seat: String,
    cfg: HubConfig,
    stop: Arc<AtomicBool>,
    waits: Waits,
) {
    let mut hub = Hub::new(cfg);
    let mut backoff = waits.first;
    loop {
        let (wait, again) = match pass_seat(&root, &seat, &mut hub) {
            Ok(Pass::Delivered) => (false, true), // look again at once
            Ok(Pass::Waiting) => (true, true),    // back off, then refresh
            Ok(_) => (false, false),
            Err(e) => {
                log_failure(&root, &seat, &e);
                (false, false)
            }
        };
        if !again || stop.load(Ordering::SeqCst) {
            break;
        }
        if wait {
            let mut left = backoff;
            while left > Duration::ZERO && !stop.load(Ordering::SeqCst) && !fresh_work(&root, &seat)
            {
                std::thread::sleep(left.min(SLICE));
                left -= left.min(SLICE);
            }
            // The full backoff elapsed (not a fresh-work wake): grow it.
            if left == Duration::ZERO {
                backoff = backoff.saturating_mul(2).min(waits.max);
            }
        } else {
            backoff = waits.first;
        }
    }
}

/// One failure line plus its alarm classification: a hub refusing auth
/// is one board-wide cause; any other hub error means that seat's
/// session got no answer. Both once per cause (alarm.rs).
pub(crate) fn log_failure(root: &Path, seat: &str, e: &SeatError) {
    let detail = e.to_string();
    log_line(root, "failure", seat, &detail);
    let fired = match e {
        SeatError::Hub(msg) if msg.contains("auth") => {
            alarm::fire(root, None, alarm::HUB_AUTH, &detail)
        }
        SeatError::Hub(_) => alarm::fire(
            root,
            Some(seat),
            alarm::NO_ANSWER,
            &format!("the hub does not answer for this seat's session: {detail}"),
        ),
        _ => Ok(()),
    };
    if let Err(pe) = fired {
        log_line(root, "failure", seat, &format!("alarm not persisted: {pe}"));
    }
}
