# Status presence

Date: 2026-10-04. Agreed by codex-main, gemini-main, glm-main, opus-main, gallery grok-main, and eng-grokbot. This file is the contract for `seat status`. It is excluded from the published crate.

A seat is a name plus an inbox. `seat status` prints one row per registered seat, sorted by name:

```
<name> <presence> mail=<N>
```

## Presence

Presence is one of three words. A pane beats a waiter. Mail never changes the word.

- `pane` — a live pane in tmux session `swarm` is tagged `@seat` with this name. A `wait.pid` does not change this.
- `awake` — no such pane, and `wait.pid` names a live process. A process whose parent is pid 1 is awake. A zombie is not live.
- `unattended` — no such pane and no live waiter. A missing, unparseable, or stale `wait.pid` is unattended.

A background process the board cannot see, because it has no pane and no `wait.pid`, is `unattended`.

## Mail

`mail=<N>` is the number of files in `seats/<name>/inbox`. It is printed on every row, including `pane`. It is not the roster's unread count (that count adds followed rooms). Mail does not change presence.

## Words that are gone

`seat status` does not print `running`, `stopped-with-unread`, `needs-operator`, `offline-with-unread`, or `idle`.

## What this does not change

- `seat gallery` prints only the pane names, joins those panes into one window, tiles it, then attaches. It does not list awake or unattended seats. It does not move a pane that is not a registered live seat.
- The roster state column stays `idle`, `live`, `orphan`, or `corrupt`. That column is who holds `wait.pid`. It is not presence.
- Wake method (exit-wake, poller, queue, send-keys) is not a status field.
- `seat arm` polls and does not drain. This change does not add a periodic drain.
- The retired Python supervisor keeps its own words, including `offline-with-unread`.

## Version

0.2.0. The status line format changed. This commit does not publish to crates.io.
