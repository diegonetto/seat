# Status presence

Date: 2026-10-04. Agreed by codex-main, gemini-main, glm-main, opus-main, gallery grok-main, and eng-grokbot. Revised 2026-10-19 for 0.4.0 (`seat arm` and its waiter-derived `awake` were deleted), 2026-10-20 for 0.5.0 (`awake` returned, derived from the dispatcher's delivery state), and 2026-10-23 for 0.8.0 (`asleep`, the dispatcher's idle sleep mark). This file is the contract for `seat status`. It is excluded from the published crate.

A seat is a name plus an inbox. `seat status` prints one row per registered seat, sorted by name:

```
<name> <presence> mail=<N>
```

## Presence

Presence is one of four words, in this precedence. Mail never changes the word.

- `pane` — a live pane in tmux session `swarm` is tagged `@seat` with this name.
- `asleep` — the dispatcher's own `<seat>/dispatch.json` records `asleep: {at, reason}` (0.8.0: the dispatcher stopped the seat's pane because it was idle past its `--idle-after` threshold), and no live pane. Status reads that file and never asks the hub. `asleep` outranks `awake`: a seat whose threshold is shorter than the 10-minute awake window still prints `asleep` until a wake clears the mark.
- `awake` — no live pane and no sleep mark, and the dispatcher's `<seat>/dispatch.json` records a delivery to this seat's currently bound session within the last 10 minutes. Status reads that file and never asks the hub; a delivery to a session the seat is no longer bound to does not count.
- `unattended` — every other seat.

A background process the board cannot see, because it has no pane and no dispatcher delivery, is `unattended`. A live pane outranks the sleep record: a seat someone started again prints `pane`.

## Mail

`mail=<N>` is the number of files in `seats/<name>/inbox`. It is printed on every row, including `pane`. Mail does not change presence.

## Words that are gone

`seat status` does not print `running`, `stopped-with-unread`, `needs-operator`, `offline-with-unread`, or `idle`.

## What this does not change

- `seat gallery` prints only the pane names, joins those panes into one window, tiles it, then attaches. It does not list unattended or awake seats. It does not move a pane that is not a registered live seat.
- The retired Python supervisor keeps its own words, including `offline-with-unread`.

## Version

0.2.0 defined the line; 0.4.0 deleted the waiter-derived `awake` with `seat arm`; 0.5.0 returned `awake` from the dispatcher's delivery state; 0.8.0 added `asleep` (the dispatcher's idle sleep mark) between `pane` and `awake`. This commit does not publish to crates.io.
