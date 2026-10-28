# seat

One command for a local board of named seats. Each seat has an inbox of
HMAC-signed messages. `drain` prints the inbox and empties it. `up`
starts the command stored on a seat inside one tmux session named
`swarm`.

```sh
cargo install seat
```

## Try it

The default board is `$HOME/.local/share/seat`. Pass `--root` to use
another directory. `init` is the only command that creates a board.

```sh
ROOT=$(mktemp -d)
seat --root "$ROOT" init
seat --root "$ROOT" register --seat alice --harness local
seat --root "$ROOT" register --seat bob --harness local
seat --root "$ROOT" send --from alice bob hello
seat --root "$ROOT" drain --seat bob
```

`drain` prints `hello`. A second `drain` prints nothing.

## Start a command

`register` stores an absolute directory and an argv. The words after `--`
are the command, not a shell line. `up` starts that command. A seat that
is already running keeps its process until `down` and `up`.

```sh
seat --root "$ROOT" register --seat worker --harness local --cwd "$ROOT" -- /bin/sleep 30
seat --root "$ROOT" up worker
seat --root "$ROOT" status
seat --root "$ROOT" down worker
```

`up` with no name starts every registered seat that has a command.
`down` with no name stops every registered seat. Neither command reads
the inbox.

## Verbs

`init`, `register`, `send`, `drain`, `dispatch`, `up`, `down`,
`gallery`, `status`, `reset`. An unknown verb exits 2 and prints this list.

```
seat init
seat register --seat <name> --harness <h> [--model M] [--cwd ABS] [--idle-after DUR] [-- CMD...]
seat send [--from <from>] <to> <body>
seat drain --seat <name>
seat dispatch [--once]
seat up [name...]
seat down [name...]
seat gallery
seat status
seat reset
```

`--idle-after` takes a duration (`30m`, `2h` — one integer plus
`s`/`m`/`h`). Omitted on a first register it means never; a later
register that omits it keeps the stored value, and a passed value
updates it even when the launch fields are omitted.

`gallery` prints the names that have a live pane in `swarm`, joins those
panes into one window named `gallery`, tiles it, and attaches. Each
tile's border shows that pane's `@seat` name. Mouse is on. A zoomed pane
stays full screen across a resize. It does not list a seat that is not
running, and it does not move any other pane.

`drain` takes a per-seat lock (`<seat>/lock`) for its whole run: a
second drain of the same seat waits, and gives up after 30 seconds with
an error naming the seat. Drains of other seats never wait.

## Deliver to a hub session

`dispatch` is the one service that delivers the board's mail. Run it
long-running; `--once` runs a single scan pass. It takes an exclusive,
non-blocking lock on `<root>/dispatch.lock` for its whole life (`--once`
included): one dispatcher per board, and a second dispatcher exits
nonzero at once with one line naming the lock. Every 2 s it scans the
seats and routes each one by configuration alone: a seat bound in
`<root>/hapi-sessions.json` is `hapi` (even if it also holds webhook
files), else a seat directory holding `webhook.url` is `webhook`, else
pull — no push and no receive, that seat drains when it chooses. A
slow hub delays only its own seat, every HTTP request is bounded at
10 s, and SIGTERM exits cleanly. Each hapi item is tracked by a stable
key recorded in `<seat>/dispatch.json` atomically before anything is
POSTed, so a crash resumes and a retry re-POSTs the same key. An item
commits only when the hub reports it invoked while its session is
still active; a pass that finds the session inactive with items in
flight moves them to fresh delivery keys and re-sends them once the
session is active again (the hub marks queued messages invoked at
session end, and a resume can keep the same session id — mail nobody
read is never archived). One line per delivery and one per failure
land in `<root>/dispatch.log`.

### Sleep and wake

A hapi seat registered with `--idle-after` sleeps when it has been
idle that long: its inbox empty and nothing in flight, its idle clock
(the first pass the dispatcher saw its pane, or the later of that and
a delivery to its currently bound session) past the threshold, the
bound session's `thinking` flag explicitly false (a missing, mistyped,
or unreadable answer — and a session that is gone — never counts as
quiet), a role for the seat in the kernel seat map (`sb seats
--json`), because the wake needs one, and the dispatcher's board being
the very board the wake script writes
(`<SEAT_ROLE_HOME|$HOME>/.local/share/seat`; seat-role fixes its
binding file there and calls `seat up` with no `--root`, so a
dispatcher on any other `--root` never sleeps a seat — its wake could
never rewrite this board's bindings). The clock belongs to one pane —
the fact in `dispatch.json` names the pane's id — so a pane that just
started (`seat up`, the gallery, a down-and-up inside one 2 s scan)
starts the clock over and is never killed on the previous pane's
timestamp. Only the pane the fact names goes down — a dead-but-listed
pane never shields it, and a different live pane under the same seat
name cancels the sleep instead of dying — and mail is looked for one
last time (`inbox/` and `draining/`, the pass's own owed set)
immediately before the record and again before any recovery kill, so
a seat holding mail is never put down. The pane goes down through the
same `down` path one pane already uses, `dispatch.json` records
`asleep: {at, reason}`, and `hapi-sessions.json` is left exactly as
the wake's own script wrote it — this crate never writes that file.

A sleep whose kill never landed (the dispatcher died between the
record and the kill) is settled on the next pass, and only when
everything the decision used still holds: the recorded pane id is
still the seat's live pane, the bound session's `thinking` reads
exactly `false` again, the binding stands, and nothing is owed. The
same quiet pane is killed; a pane that is already gone only clears
the fact. A decided miss — a replacement pane, a session that resumed
work or no longer reads quiet (a missing flag, a 404), no binding, or
owed mail — cancels the sleep: the mark and the pane fact clear
together, so the pane stands and the next pane fact starts a fresh
clock. A GET error is no decision: the mark, the fact, and the clock
all stay, and the next pass asks again. A pane brought back after a
completed sleep (the fact already clear) ends the sleep — the mark
clears and that pane starts a fresh clock; it is never killed for the
old decision.

Mail reverses it in the pass that sees it. The scan starts a worker
for an asleep seat whose inbox holds mail — even when the binding file
no longer names the seat, and even when a leftover `webhook.url` would
otherwise route it to the hook — and for an asleep seat whose mark
sits beside a pane fact, a pending kill, whether or not a binding or
mail waits: a restart's leftover mark is settled by the first scan
that sees it. The mail worker's first act is the
wake: it
runs the kit's own `kit/scripts/seat-role wake <role>` (found at
`$SEAT_KIT`, else the kit beside the binary, else PATH), waits for the
script to run to its own deadlines, and only a JSON `awake` answer
plus a binding on the re-read delivers in that pass, clearing the mark
and recording `woken: {at, key}`. Any other answer (`busy` from the
cross-role lock, `refused`, `failed`, or no binding after an `awake`)
leaves the seat asleep with the item in the inbox, one failure line
naming the script's reason, and the next scan retries. The script's
`up` answer is a hold, not a retry: a pane runs with no phone
session — `up` means stop calling, and a resume-style pane holds the
script's cross-role lock through its idle watch — so the wake is not
attempted again until that pane is verifiably gone.

### The webhook adapter

The Grok bots drain the board themselves, so the webhook adapter never
runs `receive` and never archives — it rings, and re-rings until every
rung message has left `inbox/` and `draining/` (backoff from 1 min
doubling to 1 h). A webhook worker rechecks its route before each pass
and during its waits, and steps aside when its seat becomes
hapi-bound. Two modes, byte for byte against the scripts they replace:

- **signal** (`wake-grokbot`): `webhook.url` (must start `https://`),
  `bridge.key`, optional `webhook.auth` (one line; an `Authorization:`
  prefix is stripped). The POST body is exactly
  `{"schema_version":1,"seat":<seat>,"ts":<UTC ts>,"nonce":<32 hex>}`
  with `X-Memo-Signature: sha256=<HMAC-SHA256 of the body with
  bridge.key>` — never a message body.
- **payload mode** (kotek-grokbot; enabled by a marker file
  `<seat>/webhook.payload`; the contract came from the retired
  `grok-doorbell.py`): verified direct mail from the
  `inbox` seat rides in the body at most once per message id, with its
  attempted/delivered records kept in the seat directory as
  `<seat>/webhook-attempted.json` and `<seat>/webhook-delivered.json`
  (moved from the retired script's `--runtime` directory; moving
  kotek's config files on Slix is the cutover, not this code). Everything
  else — other senders, mail over 8 KiB, mail whose MAC does
  not verify — gets the signal, and while an attempted hq message is
  still unread nothing at all is sent. When the seat directory also
  holds a marker file `<seat>/webhook.hq-ack`, each successful
  payload delivery is followed by one best-effort receipt POST to hq:
  `{"seat", "message_id"}` to `https://hq.bll.dev/ingest/seat-delivery`
  with the Cloudflare Access headers (`CF-Access-Client-Id` /
  `CF-Access-Client-Secret`) read from `~/.config/bll-ingest/env`,
  User-Agent `bll-seat-mail/1 (Slix)`, 5 s timeout. A missing env
  file or failed receipt is dropped — never resent — and the payload
  delivery stands; the secret values are never logged.

### Alarms

Five causes, each latched once (one line in `<root>/dispatch.log` and
one direct message from seat `sb-watch` to the seat named in
`<root>/alarm-seat` — one line; when that file is absent or empty the
alarm is the log line only) until the condition clears and returns:
`undelivered` (an item of that seat older than 15 minutes still
undelivered, by the age of its mail file in `inbox/` or `draining/`;
on the hapi pass the bound session is read first: such a file in
flight as `posted` on an active session reporting `thinking: true` is
`working` — the record gets that phase with the time of the read, and
every pass re-reads while any item is stored `working`, old files or
not — and such a file the hub reports invoked on
an active session is delivered, not paged, however long the turn;
every other old file still pages, a hub error pages with the error
named, and the page names what the pass saw — `posted, session idle
for 16 min` only for an explicit `false`, else `thinking unknown` or
`session not active` for what the read said),
`session-inactive` (the bound session is 404 or not
active), `no-answer` (the hub does not answer for that seat's
session), `hub-auth` (the hub refuses auth — board-wide), and
`webhook-failing` (3 failed rings in a row). The fired set persists
in `<root>/dispatch-alarms.json`; a successful hub response clears
`no-answer` and `hub-auth` even when it reports the session inactive.

### The identity rule on `--from`

`send` accepts an optional `--from`. Who the message is from is
decided like this:

- Sessions are bound to seats by `<root>/hapi-sessions.json` (written by
  seat-role; a missing or unreadable file means no bindings). When the
  caller's `HAPI_SESSION_ID` is bound to seat X, the message is from X:
  an omitted `--from` means X, and any other `--from` is refused with a
  line naming both seats.
- Without a binding (services, bridges, an ad-hoc terminal), an explicit
  `--from` behaves exactly as `--from` always did.
- An omitted `--from` on `send` means the message goes as `inbox`, and
  only to a factory seat: a seat named in `hapi-sessions.json`. Any
  other destination is refused.
- An unbound sender that has a `HAPI_SESSION_ID` writes it as an
  optional `session` field in the message header, covered by the
  message HMAC like the other header fields: the MAC input is
  `dest|from|<id>:<session>|ts|body` when the field is present and
  `dest|from|id|ts|body` when absent (a message id is exactly 8
  lowercase hex characters and can never contain `:`, so the two forms
  cannot collide, and the session cannot be moved into or out of the
  body without breaking the MAC). A header whose `id` is not exactly 8
  lowercase hex characters, or whose `session` is not 1-64 characters
  of `[A-Za-z0-9-]`, is refused as unreadable. Messages written before
  this field existed still verify.

This rule prevents mistakes on one Unix user, not a deliberate forgery.

`status` prints one row per registered seat, sorted by name:

```
<name> <presence> mail=<N>
```

Presence is `pane`, `asleep`, `awake`, or `unattended`, in that
precedence. `pane` is a live pane in `swarm` tagged `@seat`. `asleep`
is no live pane and the dispatcher's own sleep mark in
`<seat>/dispatch.json` (`asleep: {at, reason}` from `--idle-after`);
it outranks a delivery inside the awake window. `awake` is no pane and
no mark, with a delivery the dispatcher recorded for this seat's bound
session within the last 10 minutes (read from
`<seat>/dispatch.json`; status never asks the hub). `unattended` is
everything else. `mail=<N>` is the number of files in that seat's
inbox. It is printed on every row. Mail does not change presence.

`seat reset` removes the marker, `seats/` (the room cursors inside
it included), and any leftover `rooms/` from before 0.7.0 (when rooms
were removed). It removes the root directory only when that leaves
the directory empty.

## Board

```
<root>/marker                 the literal seat
<root>/seats/<name>/          meta.json, token, inbox/, archive/
<root>/alarm-seat             the seat alarms are mailed to (one line;
                              absent or empty: the log line only)
<root>/dispatch.lock          one dispatcher per board (non-blocking)
<root>/dispatch.log           one line per delivery, failure, alarm
<root>/dispatch-alarms.json   the latched alarm causes
<root>/hapi-sessions.json     seat → hapi session bindings (seat-role)
```

A board from before 0.7.0 may still carry `<root>/rooms/` and
`seats/<name>/cursors/`; nothing reads or writes them, and nothing in
normal operation (`drain`, `dispatch`, `send`) deletes them. Only
`seat reset` removes them, with the rest of the board.

A command other than `init` refuses a directory it does not recognize
and names the path.

## Kit

`kit/` is the home of the seat's host scripts, moved unchanged in
behaviour from the agent-skills library. One line each:

- `kit/scripts/seat-role` — wake one role's seat and link its phone
  (HAPI) session, answering one JSON result, or report why it cannot;
  `seat-role pins` reports each seat's model and effort drift.
- `kit/scripts/seat-sit` — the stored pane command for a gallery seat:
  it records the pane target, then replaces itself with seat-supervise.
- `kit/scripts/seat-supervise` — runs the harness as the pane's foreground
  process group: tmux still names the harness as the pane's command, a
  HAPI-wrapped harness waits for the hub first, and a harness that dies at
  launch leaves `harness-exit.log` (argv, exit code, last lines) in the
  seat's runtime directory, with up to three retries while the hub port is
  closed.
- `kit/scripts/seat-hub-wait` — wait until the HAPI hub accepts a
  connection on `127.0.0.1:3006`; an expired wait is logged and is never
  fatal. One script called from both places: the gallery unit's
  `ExecStartPre` and seat-supervise, so a boot and a hand `seat up` behave
  alike.
- `kit/scripts/seat-gallery-up` — boot: start every seat's pane, then
  link each session through the normal wake path.
- `kit/scripts/pane-door` — one localhost ttyd per seat pane plus the
  root grid on `127.0.0.1:7700`; its vendored xterm files are in
  `kit/assets/pane-door/`.
- `kit/systemd/` — the user units: `seat-gallery.service` (boot) and
  `pane-door.service` (the doors).
- `kit/tests/` — the kit's unittest suites; run each `*_test.py` with
  `python3`.

`kit/` is not part of the crate package. The live units and the
board's stored `seat-sit` command still point at the agent-skills copy
until the follow-up cutover repoints them here.

## Tests

`cargo test` covers the commands. `tests/smoke.sh` is a shell script and is not part of `cargo test`.

## License

MIT. See `LICENSE`.
