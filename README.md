# seat

One command for a local board of named seats. Each seat has an inbox of
HMAC-signed messages. `arm` waits until mail arrives and does not read it.
`drain` prints the inbox and empties it. `up` starts the command stored on
a seat inside one tmux session named `swarm`.

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

`init`, `register`, `send`, `arm`, `drain`, `room`, `up`, `down`,
`gallery`, `status`, `reset`. An unknown verb exits 2 and prints this list.

```
seat init
seat register --seat <name> --harness <h> [--model M] [--lifecycle exit-wake|poller] [--cwd ABS] [-- CMD...]
seat send [--from <from>] <to> <body>
seat arm --seat <name>
seat drain --seat <name>
seat room create|post|read|follow|list
seat up [name...]
seat down [name...]
seat gallery
seat status
seat reset
```

`arm` is the only waiter. The default lifecycle is `exit-wake`: one wake
and the process exits. `poller` stays up until SIGTERM. A second `arm`
sees the live waiter and exits 0. `gallery` prints the names that have a
live pane in `swarm`, joins those panes into one window named `gallery`,
tiles it, and attaches. Each tile's border shows that pane's `@seat`
name. Mouse is on. A zoomed pane stays full screen
across a resize. It does not list a seat that is only awake, and it does
not move any other pane.

`drain` takes a per-seat lock (`<seat>/lock`) for its whole run: a
second drain of the same seat waits, and gives up after 30 seconds with
an error naming the seat. Drains of other seats never wait.

### The identity rule on `--from`

`send` and `room post` accept an optional `--from`. Who the message is
from is decided like this:

- Sessions are bound to seats by `<root>/hapi-sessions.json` (written by
  seat-role; a missing or unreadable file means no bindings). When the
  caller's `HAPI_SESSION_ID` is bound to seat X, the message is from X:
  an omitted `--from` means X, and any other `--from` is refused with a
  line naming both seats.
- Without a binding (services, bridges, an ad-hoc terminal), an explicit
  `--from` behaves exactly as `--from` always did.
- An omitted `--from` on `send` means the message goes as `inbox`, and
  only to a factory seat: a seat named in `hapi-sessions.json`. Any
  other destination is refused. In rooms there is no implicit sender:
  an unbound caller must give `--from`.
- An unbound sender that has a `HAPI_SESSION_ID` writes it as an
  optional `session` field in the message header, covered by the
  message HMAC like the other header fields. Messages written before
  this field existed still verify.

This rule prevents mistakes on one Unix user, not a deliberate forgery.

`status` prints one row per registered seat, sorted by name:

```
<name> <presence> mail=<N>
```

Presence is `pane`, `awake`, or `unattended`. `pane` is a live pane in
`swarm` tagged `@seat`. `awake` is no pane and a live waiter in `wait.pid`,
including a waiter whose parent is pid 1. `unattended` is no pane and no
live waiter. `mail=<N>` is the number of files in that seat's inbox. It is
printed on every row. Mail does not change presence.

`seat reset` removes the marker, `seats/`, and `rooms/`. It removes the
root directory only when that leaves the directory empty.

## Board

```
<root>/marker                 the literal seat
<root>/seats/<name>/          meta.json, token, inbox/, archive/
<root>/rooms/<name>/          posted messages
```

A command other than `init` refuses a directory it does not recognize
and names the path.

## Tests

`cargo test` covers the commands. `tests/smoke.sh` is a shell script and is not part of `cargo test`.

## License

MIT. See `LICENSE`.
