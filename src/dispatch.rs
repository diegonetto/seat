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
    direct_key, direct_text, load_state, log_line, room_key, room_text, store_state, DispatchState,
    LastDelivery, Phase,
};
use crate::error::{Result, SeatError};
use crate::hub::{Hub, HubConfig};
use crate::identity::Bindings;
use crate::msg;
use crate::room;
use crate::seat::{self, SeatLock};
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

/// One pass of the hapi adapter for one seat: receive under the lock,
/// update the state, gate on the session, POST the staged items, then
/// one queued-state round.
pub(crate) fn pass_seat(root: &Path, seat: &str, hub: &mut Hub) -> Result<Pass> {
    let dir = seat::seat_dir(root, seat);
    // Pull seats drain themselves: never `receive` for them.
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
    // payload-delivered record; committed mail is archived away). A
    // pending followed-room post counts too — only the posts this
    // seat's `receive` would accept, aged by their header `ts` (the
    // file-name prefix): a post owed for 15 minutes is as undelivered
    // as mail sitting in `inbox/` or `draining/`, which is all the
    // seat's own files can see.
    let mut owed = webhook::present_files(&dir);
    for room_name in room::followed_rooms(&dir) {
        let at = room::read_cursor(&dir, &room_name).unwrap_or_else(|_| "0".into());
        for post in room::posts_since(root, &room_name, &at).unwrap_or_default() {
            if drain::check_room_post(root, &room_name, &post).is_ok() {
                if let Some(name) = post.file_name().and_then(|n| n.to_str()) {
                    owed.insert(name.to_string());
                }
            }
        }
    }
    alarm::undelivered(root, seat, &BTreeSet::new(), &owed)?;
    let mut state = load_state(&dir);
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
    for r in &received.rooms {
        texts.insert(room_key(seat, &r.room, &r.file_name), room_text(r));
    }
    // A key whose file is gone (a manual drain took it) is pruned.
    let before_prune = state.items.len();
    state.items.retain(|k, _| texts.contains_key(k));
    for k in texts.keys() {
        state.items.entry(k.clone()).or_default();
    }
    if state.items.is_empty() {
        if before_prune > 0 {
            store_state(&dir, &state)?; // persist the prune
        }
        // Nothing deliverable: advance cursors past rejected room
        // posts under the lock, or every scan re-reads them forever.
        if !received.room_cursors.is_empty() {
            let _lock = SeatLock::acquire(&dir, seat)?;
            for (room_name, ts) in &received.room_cursors {
                if ts > &room::read_cursor(&dir, room_name)? {
                    room::write_cursor(&dir, room_name, ts)?;
                }
            }
        }
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
            hub.post_message(&session, key, &texts[key])?;
            item.phase = Phase::Posted;
            item.accepted_at = Some(seat::now_ts());
        }
    }
    store_state(&dir, &state)?;
    poll_round(root, seat, hub, &session, &mut state, &received, &texts)
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
    if (!absent.is_empty() || !invoked.is_empty()) && !hub.session_active(session)? {
        log_line(
            root,
            "failure",
            seat,
            &format!("session {session} is no longer active; nothing committed or re-POSTed"),
        );
        return Ok(Pass::Blocked);
    }
    for key in &absent {
        hub.post_message(session, key, &texts[key])?;
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
/// done), move no room cursor past a post still in flight and never
/// backwards — `receive` re-reports posts past the cursor, so a post
/// committed in an earlier round counts as done (two posts invoking in
/// different rounds must not wedge the cursor at "0" forever).
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
        if !invoked.contains(&direct_key(seat, &d.file_name)) {
            continue;
        }
        let to = dir.join("archive").join(&d.file_name);
        if let Err(e) = std::fs::rename(dir.join("draining").join(&d.file_name), &to) {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(e.into());
            }
        }
    }
    for (room_name, ts) in &received.room_cursors {
        let still = received.rooms.iter().any(|r| {
            &r.room == room_name
                && !invoked.contains(&room_key(seat, &r.room, &r.file_name))
                && !state
                    .items
                    .get(&room_key(seat, &r.room, &r.file_name))
                    .map(|i| i.phase == Phase::Done)
                    .unwrap_or(false)
        });
        if !still && ts > &room::read_cursor(&dir, room_name)? {
            room::write_cursor(&dir, room_name, ts)?;
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
/// observe after every pass and inside every sleep.
pub fn cmd_dispatch(root: &Path, once: bool) -> Result<()> {
    board::ensure_board(root)?;
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
        let (hapi_seats, webhook_seats) =
            route_seats(root, &Bindings::load(root), &sorted_seat_names(root)?);
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
/// mail in `inbox/`, a binding that moved while items are in flight,
/// or a new followed-room post whose delivery key this seat's state
/// does not carry yet. Posts `receive` would refuse never gain a key
/// and never wake; the cursor does not move past them while items are
/// in flight, so waking on them would void the backoff on every slice.
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
    for room_name in room::followed_rooms(&dir) {
        let at = room::read_cursor(&dir, &room_name).unwrap_or_else(|_| "0".into());
        for post in room::posts_since(root, &room_name, &at).unwrap_or_default() {
            if let Ok(received) = drain::check_room_post(root, &room_name, &post) {
                if !state
                    .items
                    .contains_key(&room_key(seat, &room_name, &received.file_name))
                {
                    return true;
                }
            }
        }
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
