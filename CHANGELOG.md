# Changelog

Notable changes to this crate. Versions follow semantic versioning.

## [0.7.1] - 2026-10-20

### Fixed

The three fixes the cutover needs — the required review of c111d09
(`reviews/2026-10-20-seat-c111d09-cutover-review-opus.md`) found 0.7.0
safe to install only after these.

- Never archive mail nobody read. A hapi pass that finds the bound
  session inactive while items are in flight (POSTed, not yet
  committed) no longer sits on those items waiting to commit on the
  hub's invoked mark: the hub marks queued messages invoked when a
  session ends, and a resume can keep the same session id, so that
  mark was made by the ending session. Every in-flight item now moves
  to a fresh delivery key (a generation suffix on `seat:<seat>:<file>`
  — a `localId` the hub has never seen, since it dedupes on
  `(session, localId)` forever) as Staged and is re-POSTed once the
  session is active again; only an invocation observed while the
  session was active commits. Item identity stays the file, so a
  rotated key still commits and archives its file when the new key is
  invoked.
- One dispatcher per board. `seat dispatch` takes an exclusive,
  non-blocking `flock` on `<root>/dispatch.lock` for its whole life
  (`--once` included); a second dispatcher exits nonzero at once with
  one line naming the lock, instead of interleaving two scanners over
  one board.
- Kotek's hq delivery receipt, ported unchanged from the doorbell's
  `report_delivery` (enabled by a marker file `<seat>/webhook.hq-ack`,
  the same shape as `webhook.payload`, in place of `KOTEK_HQ_ACK=1`):
  after each successful payload delivery the webhook adapter POSTs
  `{"seat", "message_id"}` to `https://hq.bll.dev/ingest/seat-delivery`
  with the Cloudflare Access service-token headers
  (`CF-Access-Client-Id`/`CF-Access-Client-Secret`) read from
  `~/.config/bll-ingest/env` (`KEY=VALUE` lines), User-Agent
  `bll-seat-mail/1 (Slix)`, a 5 s timeout. Best effort: a missing env
  file or failed receipt is dropped, never resent, and the payload
  delivery stands; the secret values are never logged.

## [0.7.0] - 2026-10-20

### Removed

- Seat rooms (BLL-110): the `seat room` verbs (`create`, `post`, `read`, `follow`, `list`) and every room path behind them — `drain`'s room receive and cursor handling, the dispatcher's room-post delivery (`fresh_work`, the room-cursor commit), and the webhook adapter's room-post owed mail. The data said so: 4 rooms held 188 posts, all between 2026-10-08 and 2026-10-15 and none since, while direct mail ran 15 to 877 messages a day and no skill, switchboard code or hq read path used rooms; the instruction files that sent seats to rooms changed already. Old data stays on disk untouched: `<root>/rooms/` and `seats/*/cursors/` are no longer read or written (`init` no longer creates them), nothing in normal operation (`drain`, `dispatch`, `send`) deletes them, and `seat drain` never errors because they exist; `seat reset` still removes them with the rest of the board — a leftover `rooms/` and the cursors inside `seats/`, as the burn switch always did.

### Changed

- The alarm sink: the five causes, once-per-cause latching and `dispatch.log` line are unchanged, but instead of posting to the room `blacklite-factory` each alarm now sends one direct message from seat `sb-watch` to the seat named in `<root>/alarm-seat` (one line; same body). When that file is absent or empty the alarm is the log line only; a file naming a seat that is not on the board is a failed sink — one `alarm-post-failed` line, the cause stays latched. 0.6.0 was never deployed, so the room-based sink never goes live.

## [0.6.0] - 2026-10-20

### Added

- The `webhook` adapter in `seat dispatch`, byte for byte against the scripts it replaces. Routing is by configuration alone: a seat in `hapi-sessions.json` is hapi (even with `webhook.url` staged), else a seat directory holding `webhook.url` is webhook (no hub configured anywhere), else pull. **signal** mode (eng-grokbot, sales-grokbot; source `wake-grokbot`) POSTs exactly `{"schema_version":1,"seat":..,"ts":..,"nonce":..}` HMAC'd with `bridge.key`, headers `Content-Type: application/json` and `X-Memo-Signature: sha256=<hex>`, an optional `webhook.auth` value (an `Authorization:` prefix stripped), retried 3x with the script's backoff — never a message body. **payload** mode (kotek-grokbot; source `grok-doorbell.py`; enabled by a marker file `<seat>/webhook.payload`) carries verified direct mail from the `inbox` seat in the body at most once per message id, with attempt-before-send, a payload POST attempted exactly once, suppression (no signal while an attempted hq message is still unread) and its `webhook-attempted.json`/`webhook-delivered.json` records moved into the seat directory; everything else gets the signal. The adapter never runs `receive` and never archives; it rings once per burst of new mail and re-rings (backoff 1 min doubling to 1 h) until every rung message has left `inbox/` and `draining/`. A webhook worker rechecks its route — the full rule: hapi binding, else `webhook.url`, else pull — before each pass and during its waits and exits when the route is no longer webhook (the seat became hapi-bound, so it gets the hapi worker, or `webhook.url` was removed, so it is pull). `seat dispatch --once` returns only after every spawned worker has completed its first pass, so one `--once` scan always rings the seats it spawned workers for.
- Alarms, once per cause (not per retry): one line in `<root>/dispatch.log` and one post to the room `blacklite-factory` as seat `sb-watch` for each of the five causes — `undelivered` (an item of that seat older than 15 minutes; the message header `ts`; only seats the dispatcher delivers), `session-inactive` (the bound session is 404 or not `active`), `no-answer` (the hub does not answer for that seat's session), `hub-auth` (the hub refuses auth; board-wide), and `webhook-failing` (3 failed rings in a row). The fired set persists durably in `<root>/dispatch-alarms.json` (temp, fsync, rename, directory fsync; errors returned and logged). A successful hub response clears `no-answer` and `hub-auth` even when it reports the session inactive; `session-inactive` clears when the session reads `active`; `undelivered` clears when the seat has no item older than 15 minutes; `webhook-failing` clears on a successful ring. The standing per-pass failure log lines are unchanged.

### Fixed

- A followed-room post wakes a hapi worker waiting out its queued-state backoff (`fresh_work`), skipping posts the receive path would refuse (bad mark, unreadable, dest/from refuse), so a rejected post can no longer spam the wake check while items are in flight.
- `dispatch.json` is written with the board's standard durable shape (`seat.rs` `write_meta`): temp file created fresh with mode 0600, fsync, rename, then a directory fsync — and the doorbell's id records and the alarm state use the same write.
- Hub settings discovery takes the address and the token each from its own override or field, independently: a settings file holding only `listenPort` no longer discards a file `cliApiToken` (and the reverse), so `HAPI_API_URL` with a file token, or `CLI_API_TOKEN` with a file address, now works.

## [0.5.0] - 2026-10-20

### Added

- `seat dispatch [--once]`, the one service that delivers a hapi-bound seat's mail to its hub session. Every 2 s it scans the board (polling; no inotify) and gives each seat with work its own worker thread, so a slow hub delays only its own seat; a seat already in flight is never handed a second worker. Every HTTP request is bounded at 10 s (new dependency `ureq` 3; no async runtime, no handwritten HTTP/TLS). Routing is by configuration alone: a seat in `hapi-sessions.json` goes to the hapi adapter; anything else is pull — no push and no `receive`, that seat drains when it chooses (the webhook adapter lands with 2c). The hapi adapter authenticates (`POST /api/auth`, re-exchanging the 4 h JWT near expiry or on any 401; hub address and token from `~/.hapi/settings.json`, `HAPI_API_URL`/`CLI_API_TOKEN` overriding), runs the same `receive` path `drain` uses under the seat lock (released before any HTTP), tracks each item by a delivery key (`seat:<seat>:<file>` for direct mail, `room:<room>:<file>:<seat>` for a room post — never the 4-byte id alone), records each in-flight key, its target session and its phase in `<seat>/dispatch.json` atomically before the first POST (a restart resumes; a binding that changed re-POSTs the same keys to the new session), never POSTs to a session that is not `active` (the mail stays staged and is recovered by the next pass; the dispatcher never calls `/resume`), POSTs one message per item with the key as `localId` and exactly the lines `drain` prints, then polls `queued-state` with backoff from 5 s doubling to 5 min and commits only items the hub reports invoked while the session is still `active` — exactly those items: no room cursor moves past a post still in flight, and a file a manual drain already took counts as done. One line per delivery (seat, key, accepted-at, invoked-at) and one per failure land in `<root>/dispatch.log` (the once-per-cause room alarm lands with 2c). SIGTERM exits cleanly; `--once` runs one scan pass for tests.
- `status` prints `awake` again, derived from the dispatcher's own state: a seat with no pane whose `dispatch.json` records a delivery to its currently bound session within the last 10 minutes (a local read; status never asks the hub). `pane` still wins over `awake`; `specs/status-presence.md` is updated.

## [0.4.0] - 2026-10-19

### Removed

- `seat arm` and everything waiter-shaped: `src/serve.rs`, the `arm` CLI entry and help, `wait.pid` and its helpers, and `register --lifecycle`. `meta.lifecycle` is still accepted when reading old `meta.json` files but is never written again (any metadata update drops it). `status`'s waiter-derived `awake` went with it: presence is `pane` or `unattended` until the dispatcher lands and redefines `awake` from its own delivery state.

### Fixed

- The session MAC framing. The optional `session` header value is now fed to the MAC inside the id slot (`dest|from|<id>:<session>|ts|body` when present, `dest|from|id|ts|body` when absent) instead of after the body. The old placement let a message without `session` whose body ends in `|X` share a MAC input with a message carrying `session = X` (bodies may contain `|`), so the field could be added or stripped without breaking the MAC; an id is exactly 8 lowercase hex characters and can never contain `:`, so the two forms cannot collide. Reading now refuses (as an unreadable message, quarantined like today) a header whose `id` is not exactly `ID_HEX_LEN` lowercase hex or whose `session` is not 1-64 characters of `[A-Za-z0-9-]`, and writing refuses the same, so a malformed `HAPI_SESSION_ID` is never published and then quarantined. Messages without `session` are unchanged and still verify.

## [0.3.0] - 2026-10-19

### Added

- One per-seat lock: `drain` takes an exclusive `flock` on `<seat>/lock` for its whole run, so two drains of one seat never interleave or deliver a message twice. A second drain waits, bounded: after 30s it gives up with `seat '<name>' is busy: another drain holds its lock (gave up after 30s)`. Other seats are never blocked.
- Atomic, serialized metadata: every change to an existing `meta.json` (register's launch update, send's `last_seen`, drain's `last_drained`) goes through one atomic read-modify-write under a short `<seat>/meta.lock` — temp file, fsync, rename, directory fsync. A reader never sees a half-written file, and overlapping updates of different fields both survive. A metadata update never waits on a drain.
- A reusable receive path: `drain` is now lock, `receive`, print, `commit`, unlock. `receive` stages the inbox to `draining/`, recovers leftovers, and verifies each message exactly as before (quarantining what fails); `commit` archives the delivered direct mail, advances the room cursors, and stamps `last_drained` — even when receive recorded failures, so a bad message never wedges the next drain. A receive with no commit leaves every message recoverable by the next drain. Each received item carries its file name (timestamp plus id). Output, exit codes, quarantine behaviour and stderr wording are unchanged.
- The identity rule on `send` and `room post`, from `<root>/hapi-sessions.json` (written by seat-role; a missing, unreadable, or schema-mismatching file means no bindings). A `HAPI_SESSION_ID` bound to seat X may send only as X: omitted `--from` means X, anything else is refused naming both. Otherwise an explicit `--from` behaves as before; an omitted `--from` on `send` means the message goes as `inbox` and only to a factory seat (one named in hapi-sessions.json); in rooms an unbound caller must give `--from`. An unbound sender with a session id writes it as an optional `session` header field, covered by the HMAC (fed in after the body); messages without it verify exactly as before. `--from` is therefore optional on both verbs. This prevents mistakes on one Unix user, not a deliberate forgery.

### Changed

- `seat send [--from <seat>] <to> <body>` and `seat room post <room> <body> [--from <seat>]`: `--from` is optional and resolved by the identity rule above.

## 0.2.1 — 2026-10-17

### Removed

- The interactive roster TUI (`src/tui.rs`, the `ratatui` and `crossterm` dependencies): bare `seat` now prints the CLI help.

## 0.2.0 — 2026-10-04

`seat status` prints two fields. The line is `<name> <presence> mail=<N>`.

Presence is `pane`, `awake`, or `unattended`. `pane` is a live pane in tmux session `swarm` tagged `@seat`. `awake` is no such pane and a live process named by `wait.pid`, including a process whose parent is pid 1. `unattended` is no pane and no live waiter. `mail=<N>` is the number of files in that seat's inbox, on every row. Mail does not change presence.

The words `running`, `stopped-with-unread`, and `needs-operator` are no longer printed. `seat gallery` still prints only the pane names. The roster state column is still the waiter word (`idle`, `live`, `orphan`, or `corrupt`).

`seat gallery` attaches on the caller's terminal. Capturing tmux's stdout made that attach exit with `open terminal failed: not a terminal` after the names had already been printed.

`seat gallery` joins every live seat pane into one window named `gallery` and tiles it, so one terminal shows every running seat. Each tile's top border shows that pane's `@seat` name. A zoomed pane stays zoomed when the window resizes. Mouse is on.

## 0.1.3 — 2026-09-30

crates.io categories are command-line-utilities and development-tools.
Keywords are agent, mail, tmux, inbox, and supervisor.

## 0.1.2 — 2026-09-30

The published repository is https://github.com/diegonetto/seat.

## 0.1.1 — 2026-09-30

The crates.io summary names the two jobs: offline agent mail, and a tmux supervisor for each stored command.

## 0.1.0 — 2026-09-30

First publish of the `seat` command, under MIT.

`cargo test` does not run `tests/smoke.sh`. That script is a separate shell check.
