# Changelog

Notable changes to this crate. Versions follow semantic versioning.

## [Unreleased]

### Added

- A claude seat may launch through the `hapi-chrome` wrapper (Flight's
  Claude-for-Chrome launcher: it exports `CLAUDE_CODE_ENABLE_CFC=1` and
  execs `hapi claude`; the script itself stays outside the repo). It is
  accepted wherever `hapi-glm` already is for glm, with the same shape —
  exact basename, one kind, and the wrapped `claude` word following it:
  `seat-role`'s `is_wrapped` (`kit/scripts/seat-role.py`) treats a
  claude seat recorded as `<dir>/hapi-chrome claude …` as wrapped, so a
  wake finds and records its live phone session over the up pane
  instead of failing on the down-seat branch, and `seat-supervise`'s
  `is_wrapped` (`kit/scripts/seat-supervise`) runs the HAPI readiness
  wait — and the closed-hub-port launch retries — for it as for `hapi`
  and `hapi-glm`. A `hapi-chrome` argv on a codex or glm seat is not
  wrapped, nothing else changed: the `("env", "hapi")` cutover
  refusals and `wrapper_model` (already generic over `hapi-*` names)
  behave exactly as before. `kit/tests/role_wake_test.py` and
  `kit/tests/seat_launch_test.py` cover the linked wake, the
  kind-scoped rule and the readiness wait.

### Changed

- `seat-role wake` takes seat names only (slice 3 of 3 of "seats have
  no roles"). The transitional role-word fallback in `seat_row`
  (`kit/scripts/seat-role.py`) is gone: a word that is not a seat name
  fails with `seat X is not in the seat map; use <seat names>`, even
  when a map record still carries a matching `role`. Nothing live
  changes: hq's poller, the dispatcher and the boot already pass seat
  names, and the live map carries no `role`. `map_roles` is now
  `seat_names`. `seat-role`, `role-wake.lock` and `role_wake_test.py`
  keep their names. `kit/tests/role_wake_test.py` drops the fallback
  test for one that refuses every role word, and the
  `src/dispatch_tests.rs` map fixtures drop their `role` keys (except
  the case showing a role never stands in for a seat).
- Seats are woken and slept by seat name, not role (slice 1 of 3 of
  "seats have no roles"). `seat-role wake <seat>`
  looks the seat up in the docket seat map by its name; its unknown-name
  failure reads `seat X is not in the seat map; use <seat names>`, the
  session label falls back to the seat name, and the `up` and
  start-failure reasons name the seat. Transitional: a word that is no
  seat name but is exactly one record's `role` still names that seat,
  because hq's poller sends role words until slice 2; slice 3 deletes
  that fallback with `role` itself. The dispatcher (`src/sleep.rs`) runs
  `seat-role wake <seat>` directly — the `seat_role()` lookup and its
  "no role for seat" failure are gone — and its sleep precondition is
  now that the seat is a record in `dkt seats --json` (a map that cannot
  be read is still never permission to sleep); wake failure reasons
  name the seat. `seat-gallery-up` starts and wakes exactly the seats
  `dkt seats --names` prints, one per line, never re-filtering that
  startup list. `role-wake.lock`, `seat-role` and
  `role-wake-poll.service` keep their names.
- Hard switch: `seat drain` is `seat read`, and `--hold` is `--lease`
  (`seat read --lease`). No alias: `seat drain` and `--hold` fail with
  clap's ordinary error. A plain read still prints and archives;
  `--lease` still prints and leases the mail until `seat done`. The
  on-disk stage directory stays `seats/<name>/draining/`, and
  `meta.json` still stamps `last_drained`.
- Hard switch onto docket: `sb` is `dkt`, `sb-quota` is `dkt quota`.
  The seat map is `dkt seats` (`--json` and `--names`). The quota
  check is `dkt quota status --all --json`; a blocking verdict is
  `BLOCK` (was `HOLD`). Docket reads `$DOCKET_HOME` (was
  `$SWITCHBOARD_HOME`; state at `~/.local/share/docket`). `pane-door`
  takes its binary from `PANE_DOOR_DKT` (was `PANE_DOOR_SB`), default
  `dkt`. No alias. Callers: `seat-role`, `seat-gallery-up`,
  `pane-door`, and the dispatcher's sleep path.

## [0.11.0] - 2026-11-18

### Added

- `seat done <ID>...`: the ack by id, found anywhere on the board.
  Before, `drain::done` skipped an id with no file in that seat's
  `draining/`, so a mistyped id or the right id under the wrong
  `--seat` exited 0 and the held mail came back 30 minutes later — a
  wrong ack succeeded and moved nothing. The new verb (one or more
  ids, each 8 lowercase hex) searches every seat's `inbox/`,
  `draining/`, `archive/` and `quarantine/` per id — the four
  listings taken in lifecycle order, so a file moving between
  folders mid-scan is still found — and decides in this order: two
  or more `*-<id>.msg` matches in `draining/` (across seats) is
  refused naming both as `<seat>/draining/<file>`; exactly one is
  archived, leased or not, under that seat's lock alone, where the
  id's folder is re-resolved (an id that reached `draining/` by lock
  time is archived from there; a NotFound on a rename is success);
  a match already in `archive/` is success (nothing moves); a match
  in `inbox/` is refused (`drain it first`) unless the seat carries
  the `webhook.payload` marker and `webhook-delivered.json` records
  the id as delivered in the webhook body — then it is retired by
  one rename under the lock; a match in `quarantine/` is refused
  (never delivered); an id nowhere is refused (`not on this board`).
  Each id settles on its own: the archivable ids archive even when
  others refuse, each refusal is reported exactly once in one
  `SeatError::Done` that `main` prints (`done: ...`), and any
  refusal exits nonzero. Every rename stamps `last_drained` on the
  seat it archived for, and each archived id appends one trace line
  to `dispatch.log` — `done seat=<owner> id=<id> by=<--seat or
  unknown>` — with one stderr line naming both seats when the
  caller's `--seat` differs from the owner.

### Changed

- `seat drain --seat X --done ID...` is the same command as
  `seat done`: the `--seat` is still checked (an unknown or corrupt
  seat keeps its named error) and the ids are then found board-wide,
  so an ack under the wrong seat archives the right file. The
  per-seat body it replaced treated an id with no file in that
  seat's `draining/` as success.
- A hold stamps the reader alive: `drain --hold` now stamps
  `last_drained` on every call, after printing, including a call that
  printed nothing — a reader that only holds no longer looks like it
  never reads.
- The redelivery line. A message already in `draining/` before this
  drain's receive moved the inbox in — an expired hold, a crashed
  drain, or mail today's hapi dispatcher staged and pushed — may have
  been shown before, so `drain` and `drain --hold` print exactly
  `[redelivery] check whether you already did this; if so, only run:
  seat done <id>` (the real id) before that message's direct text.
  `ReceivedDirect` carries the new `redelivery` flag (`receive` sets
  it from a listing of `draining/` taken before the inbox loop;
  `check_direct` leaves it false); `direct_text` and the
  dispatcher's POST text are byte for byte unchanged. On 11-18
  matter-grokbot's held dcf449d2 was rung and printed twice with
  nothing saying it had been shown before; the line names it.

## [0.10.0] - 2026-11-17

### Added

- A dispatcher heartbeat and a mail watchdog that notices what the
  dispatcher cannot report about itself. `seat dispatch` writes
  `<root>/dispatch-heartbeat` (one `seat::now_ts()` timestamp plus a
  newline, via `atomic_write_private`) after the webhook spawn loop on
  every pass, `--once` included; a failed write never stops the loop
  and is not logged — the stale heartbeat is the report. The watchdog,
  `kit/scripts/seat-mail-watchdog.py` (stdlib only, run as
  `python3 -I`, root by `--root`/`SEAT_ROOT`/`~/.local/share/seat`),
  reads files only: it never opens a `.msg` file, takes no lock, runs
  no `seat` verb and calls no hub. Its checks: a heartbeat absent or
  older than 60 s; `hapi-sessions.json` present but not schema 1 with
  `seats` of objects holding a string `url` (such a file silently
  drops every binding, identity.rs's fallback); per-seat overdue mail —
  `inbox/*.msg` plus unleased `draining/*.msg`, aged by the name's `ts`
  and, unlike the alarm, by the file's mtime when the name carries no
  ts — under the seat's `--mail-limit` (a new register flag with
  `--idle-after`'s grammar, refusal (`BadMailLimit`) and keep-when-
  omitted rule, stored as `mail_limit` on meta.json), else 15 minutes
  for seats the dispatcher routes as hapi (bound or asleep) or webhook,
  with pull seats unchecked unless a limit is set; and any file in
  `seats/<seat>/quarantine/`, a problem while it stays (a rename keeps
  the send-time mtime, so "newer than the last run" cannot be told).
  Recovery before
  reporting: the first stale pass runs `systemctl --user restart
  seat-dispatch.service` once (by name from PATH, output private,
  120 s timeout) and records `{incident, restarted_at, exit}` in
  `<root>/watchdog-state.json` — reporting nothing that run when the
  exit is 0, the problem naming the restart on the next stale pass,
  no second restart, and a fresh heartbeat clearing the incident.
  Output is one atomic `<root>/watchdog-status.json` (temp in the same
  directory, fsync, `os.replace`, mode 0600):
  `{checked_at, ok, problems: [{key, seat, kind, since, detail}]}`
  sorted by key, details carrying counts, ages, routes, limits and
  file names only — never a body, header, token or clock time — and
  one stdout line, `seat mail watchdog pass ok: N problems` (plus `;
  restarted seat-dispatch.service`). Units
  `kit/systemd/seat-mail-watchdog.service` (oneshot,
  `TimeoutStartSec=180`) and `.timer` (`OnBootSec=2min`,
  `OnUnitActiveSec=2min`, `AccuracySec=10s`) install beside the
  existing units; the unit name is load-bearing (run 1b keys hq
  liveness on it), and stopping the dispatcher on purpose now means
  stopping the timer first. `tests/watchdog-drill.sh` (also run from
  `tests/smoke.sh`) drills the failure shapes with real seat verbs
  and a stub systemctl. `src/alarm.rs` and its mail keep working
  unchanged.

## [0.9.0] - 2026-11-17

### Added

- `seat drain --seat X --hold` and `seat drain --seat X --done ID...`,
  so a bot run that dies after reading its mail loses none of it. A
  plain `drain` receives, prints and commits in one call, so a Grok
  bot run that died after its drain left the mail in `archive/`:
  nothing rang again and nobody was told. `--hold` receives exactly
  as `drain` does, sets each staged file's mtime to the lease
  deadline (`now + LEASE`, one constant, 30 minutes — the Matter
  bot's longest run waits up to 20 minutes on an approval), prints
  the mail exactly as `drain` prints it, and commits nothing; the
  order per file is staged, deadline set, printed, and a file whose
  deadline cannot be set is not printed and fails the command naming
  it. `--done ID...` (each id 8 lowercase hex, else refused before
  anything is archived; two staged files matching one id refused,
  naming both) archives exactly the one `*-<id>.msg` file in
  `draining/`, leased or not, never touches `inbox/`, stamps
  `last_drained` and prints nothing; an id with no file is success,
  and `--hold` together with `--done` is refused by the parser. One
  predicate, `leased` — a file in `draining/` whose mtime is later
  than now; nothing else ever sets a future mtime — is used in
  exactly two places: `drain::receive` skips leased files (not
  printed, not quarantined, not committed, so a plain `drain`, a
  second `--hold` and the hapi pass all leave held mail alone until
  its deadline passes) and `webhook::present_files` leaves them out,
  so the 2 s scan, `ring_pass`, the worker's waits, the sleep checks
  and the `undelivered` alarm all see held mail as not owed until
  the deadline and as owed again after it — the scan starts a worker
  and the ring fires with no new mail. On 11-17's ping (eeb38468,
  sent 17:38:59Z, drained 17:39:11Z): with the bot on `--hold` the
  mail stays in `draining/` until its `--done`, and had the run
  died the scan would ring again at 18:09:11Z. The README gains
  "Hold, then done" (the bot's loop; a duplicate after a clock jump
  is possible and a loss is not; the lease clock is the file's
  mtime, the alarm's the header `ts`) and the verbs table.

### Changed

- A successful signal ring now leaves one line in
  `<root>/dispatch.log`: `rung seat=<seat> id=<id>[,<id>...]`, the
  ids of the files the ring newly covers (present minus the previous
  rung set), or of every present file when a re-ring found nothing
  new. Before, the `Ok(true)` arm logged nothing in signal mode and
  only `strike` wrote, so the 11-17 ring of message eeb38468 had no
  line. Payload mode keeps its `delivered` lines and adds nothing.

## [0.8.5] - 2026-11-14

### Fixed

- A webhook seat's corrected files take effect at the next ring, every
  failed ring says why, and the staging steps are written down. On
  11-14 a new webhook seat was staged with no `webhook.auth`; every
  ring got HTTP 401, and three defects made that slow to find and slow
  to fix. The worker loaded the seat's webhook files once and reused
  them for the whole burst (`webhook::worker_loop` lived until every
  rung message drained), so the added `webhook.auth` was ignored and
  the next ring failed again — only a hand-made POST, which let the
  bot drain, ended the burst; the worker now re-reads the files before
  every ring pass, and a load that fails (a file removed mid-burst)
  logs one `failure` line naming it, sends nothing, leaves the burst,
  its rung list and its backoff exactly as they were, and keeps the
  worker alive for the next due ring. `strike` logged the `failure`
  detail for the first two failed rings only — from the third the
  once-per-cause `webhook-failing` alarm fired and later rings logged
  nothing, so `dispatch.json` showed `failures: 6` while
  `dispatch.log` held two lines; every failed ring now writes its line
  (an HTTP 401 or 403 detail ends with
  `(check webhook.auth: one line, "Bearer <token>")`; no secret is
  ever logged), and the alarm still fires once per cause. The README
  gained "Staging a Grok bot seat": the exact files and formats
  (`webhook.url` starting `https://`; `bridge.key`, the HMAC secret
  for `X-Memo-Signature`; `webhook.auth`, one line, the Authorization
  value), the register command, that `seat dispatch` (the
  `seat-dispatch` user service) must run on the machine that holds
  the board, and that ring results are in `<root>/dispatch.log`.

## [0.8.4] - 2026-10-30

### Changed

- An unknown verb is refused by the argument parser: it still exits 2, now
  with `error: unrecognized subcommand '<verb>'` and a usage line instead of
  `seat: unknown verb '<verb>'` and a `verbs:` list (BLL-210).
- `meta.json` no longer models the retired `lifecycle` key or the unused
  `session` key. Both are ignored on read and dropped on the next write, so a
  seat whose `lifecycle` held an unknown value now loads instead of reading as
  corrupt.
- `seat-role` no longer renames a session still titled with the old
  `{Role} · {seat}` name; it is left as a hand-set name would be.

### Removed

- Dead code and residue the audit proved unneeded: the three flock guards are
  one, the duplicate atomic writers are one (in Rust and in `seat-role.py`),
  the `ureq` agent setup is one helper, pass-through wrappers are inlined, the
  unread `thinkingAt` field and `MAC_HEX_LEN` are gone, and tests that pinned
  the absence of rooms, `arm`, `MEMO_ROOT` and `wait.pid` are gone. Spec tags,
  version and ticket narration left the code comments. `seat reset` still
  removes a leftover `rooms/`.

## [0.8.3] - 2026-10-28

### Fixed

- The gallery waits for the HAPI hub before seating, and a harness that dies
  at launch leaves a trace (BLL-151). At the 2026-10-28 boot
  `hapi-hub.service` ran at 13:28:22, the gallery seated codex-main at
  13:28:31, and the hub first accepted connections at 13:28:36: systemd
  counts a `Type=simple` unit started the moment its process runs, so
  `After=hapi-hub.service` plus `ExecStartPre=/bin/sleep 5` seated every seat
  inside the hub's 14-second startup window, and the Codex harness — which
  exits when the hub is unreachable at launch — took its pane with it with
  nothing logged: the lander seat silently down after every reboot.
  `kit/scripts/seat-hub-wait` (new) polls `127.0.0.1:3006` for a TCP accept
  up to `SEAT_HUB_WAIT_TIMEOUT` seconds (default 60; 14 measured on this
  box, `SEAT_HUB_HOST`/`SEAT_HUB_PORT` retarget the probe for tests) —
  readiness is the port accepting, never the hub process existing, and the
  wait never starts, stops or restarts the hub. An expired wait says so and
  is not fatal (exit 0), so seats still start and a dead hub is visible in
  `seat status` rather than hidden behind a unit that never ran. The
  gallery unit's `ExecStartPre` now runs this one script (its comment says
  why the ExecStartPre must stay: `seat-gallery-up` wakes roles right after
  seating them, and seat-sit runs the same wait, so removing it races the
  wake's pane check against a still-waiting seat). `seat-sit` no longer
  execs the harness itself: it execs `kit/scripts/seat-supervise` (new),
  which starts the harness as the pane's foreground process group — tmux
  still reports the harness as the pane's current command, the recorded
  pane pid still matches the pane, and `seat down` still stops the harness
  with the pane — waits for the hub first when the harness is HAPI-wrapped
  (`hapi`/`hapi-glm`), and on a harness exit at launch (within
  `SEAT_LAUNCH_WINDOW`, default 30 s) overwrites `harness-exit.log` in the
  seat's runtime directory with the argv words (never environment values),
  the exit code and the pane's last 20 lines; while the hub port is closed
  such an exit is retried up to three times, `SEAT_RETRY_GAP` seconds apart
  (default 5), each retry waiting for the hub again. A harness that exits
  for any other reason is not relaunched, and `seat up` keeps its
  idempotence (a second call never duplicates a running pane).
  `kit/tests/seat_launch_test.py` covers both controls of the wait (an
  open port returns in under a second; a closed port expires at its
  ceiling, says so and exits 0) and the supervised launch (a harness
  exiting 1 at launch leaves `harness-exit.log` with the code and the tail
  beside a matching wake target; a wrapped harness dying on a closed hub
  port runs once plus the three retries).

## [0.8.2] - 2026-10-23

### Fixed

- A dispatcher restart left an unbound asleep seat carrying a pending
  sleep mark with an empty inbox unscheduled (BLL-137, accepted at
  0.8.0): the scan added an asleep seat to its workers only when
  inbox mail existed, so for that seat the settlement's no-binding
  cancellation never ran and the mark and the pane fact lingered with
  no settlement log until mail arrived — never a wrong kill, never
  lost mail, but a reader was left a wrong fact. The scan now
  schedules every seat whose state carries a pending sleep mark (the
  `asleep` record beside a pane fact), whether or not the seat is
  bound and whether or not mail waits; the pass settles it exactly as
  before (`settle_pending_sleep` decides and logs its one line), and
  a settlement that cancels a seat with no binding and no owed mail
  now ends the pass clean instead of falling through to the
  staged-mail failure line (nothing was staged). The guards before
  any kill — the pane id, the re-read session, the inbox — are
  untouched.
- The hapi pass reconciled stored `working` items only when something
  was old (BLL-139, accepted at 0.8.1): a thinking turn marks every
  in-flight posted item `working`, younger mail included, and once
  the older item was invoked and archived the empty old-file set
  returned early, so a younger item stayed `working` after the
  session stopped thinking — until its own file crossed the 15-minute
  threshold, a late alarm, never a false one. Every pass that holds a
  stored `working` item now re-reads the session and returns the item
  to `posted` (or on) when the session is no longer thinking, before
  the alarm's age filter runs; a pass with nothing old and nothing
  stored `working` still never pays for the read, and the threshold
  and the alarm's words are unchanged.

## [0.8.1] - 2026-10-23

### Fixed

- The `undelivered` alarm checks the session before it pages
  (BLL-124). On 2026-10-22 `sb watch` paged a seat whose item was
  delivered: the hub marks a queued message invoked only when the turn
  ends, so a posted item on a session still thinking stays `posted`
  while the turn runs, and every turn past 15 minutes tripped the
  file-age alarm — a false page, no lost mail. The hapi pass now reads
  the bound session (0.8.0's `Hub::session_state`; no hub field was
  added) before the decision, per old file. An in-flight `posted` item
  on an active session reporting `thinking` exactly `true` is
  `working`: the record gets that phase with the time of the read,
  there is no new timer, and the next pass re-reads — a read that
  does not say thinking (a missing flag, a quiet or inactive session,
  a GET error) returns the item to `posted`, the phase the alarm
  fires on. An old `posted` item the hub reports invoked on an active
  session is delivered, not paged: the decision runs before the
  pass's poll commits and archives that mark, so the invoked state is
  read there too — only a pass that reads the item still not invoked,
  the session not thinking, past the threshold, pages. Every other
  old file still pages: never-posted mail, the asleep seat's call,
  and the webhook seats keep today's file-age rule unchanged; a
  missing or non-boolean `thinking` never skips; and a hub error
  never skips — today's page happens with the error named. The page
  text names what the pass saw (`posted, session idle for 16 min` —
  N the file's own age — only for an explicit `false`; `thinking
  unknown` or `session not active` when that is the read), so a dead
  delivery reads apart from a long turn; and when every old file is
  skipped the latch clears, so a later genuine page can fire.

## [0.8.0] - 2026-10-23

### Added

- Seats sleep when idle and wake on mail (BLL-127). `seat register
  --idle-after <duration>` (one integer plus `s`/`m`/`h`, e.g. `30m`,
  `2h`; anything else is refused and writes nothing) stores a per-seat
  threshold on `seats/<name>/meta.json`: a first register that omits it
  means never, a later register that omits it keeps the stored value,
  and a passed flag updates it even when cwd and command are omitted.
  Inside the hapi pass — never in `fresh_work` — a seat sleeps when
  all hold: it has `idle_after`; its idle clock is at least that old,
  where the clock is `pane_seen_at` (recorded in `dispatch.json` the
  first pass a pane is seen, stamped with that pane's id, and cleared
  when the pane is gone — a live pane with a different id — `seat up`,
  the gallery, or a down-and-up that landed inside one 2 s scan —
  starts the clock over, so a pane that just started is never killed
  on the previous pane's timestamp, and a seat with no seen pane is
  never slept) or the later of it and a
  delivery to the currently bound session (a delivery to a session the
  seat is no longer bound to does not count); its inbox is empty and
  nothing is in flight; the bound session's `GET /api/sessions/<id>`
  reports `thinking` explicitly `false` — a missing key, a non-boolean,
  a 404, or a GET error means not idle, so a missing flag is never
  permission to sleep; the seat has a role in the kernel seat map
  (`sb seats --json`, the same map `seat-role` reads; a map failure
  means not idle), because the wake takes a role; and the dispatcher's
  board is the very board the wake script writes
  (`<SEAT_ROLE_HOME|$HOME>/.local/share/seat` — seat-role fixes its
  binding file there and calls `seat up` with no `--root`, so a
  dispatcher on any other `--root` never sleeps a seat; its wake could
  never rewrite this board's bindings). An idle seat goes
  down through the same `down` path one pane already uses (tmux
  kill-pane of the seat's own pane; never a signal by pid, never a
  pane the board did not start), and `dispatch.json` records
  `asleep: {at, reason}`; `hapi-sessions.json` is left as it is. The
  kill names only the pane the fact times — dead-but-listed panes
  never shield it, and a different live pane under the same seat name
  cancels the sleep it just recorded — and the owed set (`inbox/` and
  `draining/`) is read once more immediately before the record and
  before any recovery kill, so a seat holding mail is never put down.
  A sleep whose kill never landed (a crash between the record and the
  kill) is settled on the next pass only when everything the decision
  used still holds: the recorded pane id is still the seat's live
  pane, the bound session's `thinking` reads exactly `false` again,
  the binding stands, and nothing is owed. The same quiet pane is
  killed and a pane already gone only clears the fact; a decided miss
  — a replacement pane, a session that resumed work or no longer
  reads quiet (a missing flag, a 404), no binding, or owed mail —
  cancels the sleep, clearing the mark and the pane fact together so
  the clock restarts, and a GET error keeps the mark, the fact, and
  the clock for a retry on the next pass. A recovery kill refused
  because the pane set moved between recovery's own pane query and
  the kill is decided by the same re-query the first sleep runs — a
  different live pane cancels, an unanswerable multiplexer keeps the
  mark pending, and only a gone pane completes — and every settlement
  outcome logs the case it saw. A pane brought back after a
  completed sleep ends it: the mark clears and that pane starts a
  fresh clock, never killed for the old decision.
- `seat status` prints a fourth presence word, `asleep`, for a seat
  whose `dispatch.json` records the dispatcher's sleep mark and whose
  pane is gone. Precedence: `pane`, then `asleep`, then `awake`, then
  `unattended` — a threshold under ten minutes still prints `asleep`.
  `specs/status-presence.md` and `README.md` carry the fourth word.
- Mail wakes an asleep seat in the pass that sees it: the 2-second
  scan starts a worker for an asleep seat whose inbox holds mail —
  even when `hapi-sessions.json` no longer names the seat, because
  seat-role's `start_down` pops the binding before `seat up` — and
  that worker's first act is the wake: it runs
  `kit/scripts/seat-role wake <role>` (found at `$SEAT_KIT`, else the
  kit beside the binary, else PATH; called, never reimplemented — the
  script stays the only writer of `hapi-sessions.json`, and the
  dispatcher waits for the script's own deadlines rather than cutting
  a live wake at 45 s). Only a JSON `status: awake` answer plus a
  binding for the seat on the re-read delivers in that pass, clears
  `asleep`, and records `woken: {at, key}` with the delivery key that
  woke it; `busy` (the cross-role lock, no reason string), `refused`,
  `failed`, or no binding after an `awake` leaves the seat asleep with
  the item in the inbox, reported like undelivered mail naming the
  script's reason, and the next scan retries. The script's `up`
  answer is a hold, not a retry: a pane runs with no phone session —
  `up` means stop calling, and a resume-style pane holds
  `role-wake.lock` through its idle watch — so the wake is not
  attempted again until that pane is verifiably gone (`wake_hold` in
  `dispatch.json` records it, one failure line naming the script's
  reason).
- `Hub::session_state(id)` returns `{active, thinking, thinking_at}`
  from the `GET /api/sessions/<id>` body already fetched (`thinking`
  and `thinking_at` are unset when the key is missing or not the right
  type, and a 404 leaves them unset while `active` reads false);
  `session_active` keeps its name and meaning on top of it.

## [0.7.3] - 2026-10-20

### Added

- `kit/`, the seat's host kit, moved from the agent-skills library
  (the skill `seat-arm`) unchanged in behaviour: `seat-role` (wake a
  role's seat and link its phone session, plus `pins`), `seat-sit`
  (the stored pane command), `seat-gallery-up` (boot), `pane-door`
  with its vendored xterm assets (the localhost web terminal grid),
  the two systemd user units, and the kit's unittest suites. Paths
  inside the copies now name `kit/` (`%h/Projects/seat/kit/...` in
  the units); the live units and the board's stored `seat-sit`
  command still point at the agent-skills copy until the follow-up
  cutover. `kit/` is excluded from the crate package, like `specs/`.

## [0.7.2] - 2026-10-20

### Fixed

- `session_active` read the hub's `active` at the top level of the
  `GET /api/sessions/<id>` answer; the live hub nests it inside a
  `session` object (`{"session": {..., "active": true, ...}}`), so at
  the 2026-10-20 cutover every bound session read "not active" and
  every pass re-logged it — no mail was lost, but none was delivered
  either. The flag is now read from `session.active` (a top-level
  `active` is still accepted), and the fake hub in the tests answers
  the nested shape, so the miss cannot hide again.

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
