# Changelog

Notable changes to this crate. Versions follow semantic versioning.

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
