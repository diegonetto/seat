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
seat send --from <from> <to> <body>
seat arm --seat <name>
seat drain --seat <name>
seat room create|post|read|follow|list
seat up [name...]
seat down [name...]
seat gallery
seat status
seat reset
seat
```

`arm` is the only waiter. The default lifecycle is `exit-wake`: one wake
and the process exits. `poller` stays up until SIGTERM. A second `arm`
sees the live waiter and exits 0. `gallery` prints the running names and
then attaches to `swarm`. On a tty, `seat` with no verb is a read-only
roster. `q` quits. The roster does not send, drain, or arm.

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
