# Changelog

Notable changes to this crate. Versions follow semantic versioning.

## 0.2.0 — 2026-10-04

`seat status` prints two fields. The line is `<name> <presence> mail=<N>`.

Presence is `pane`, `awake`, or `unattended`. `pane` is a live pane in tmux session `swarm` tagged `@seat`. `awake` is no such pane and a live process named by `wait.pid`, including a process whose parent is pid 1. `unattended` is no pane and no live waiter. `mail=<N>` is the number of files in that seat's inbox, on every row. Mail does not change presence.

The words `running`, `stopped-with-unread`, and `needs-operator` are no longer printed. `seat gallery` still prints only the pane names. The roster state column is still the waiter word (`idle`, `live`, `orphan`, or `corrupt`).

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
