# Status presence

Date: 2026-10-04. Agreed by codex-main, gemini-main, glm-main, opus-main, gallery grok-main, and eng-grokbot. Revised 2026-10-19 for 0.4.0 (`seat arm` and its waiter-derived `awake` were deleted), and 2026-10-20 for 0.5.0 (`awake` returned, derived from the dispatcher's delivery state). This file is the contract for `seat status`. It is excluded from the published crate.

A seat is a name plus an inbox. `seat status` prints one row per registered seat, sorted by name:

```
<name> <presence> mail=<N>
```

## Presence

Presence is one of three words. Mail never changes the word.

- `pane` — a live pane in tmux session `swarm` is tagged `@seat` with this name.
- `awake` — no live pane, and the dispatcher's `<seat>/dispatch.json` records a delivery to this seat's currently bound session within the last 10 minutes. Status reads that file and never asks the hub; a delivery to a session the seat is no longer bound to does not count.
- `unattended` — every other seat.

A background process the board cannot see, because it has no pane and no dispatcher delivery, is `unattended`.

## Mail

`mail=<N>` is the number of files in `seats/<name>/inbox`. It is printed on every row, including `pane`. It is not the roster's unread count (that count adds followed rooms). Mail does not change presence.

## Words that are gone

`seat status` does not print `running`, `stopped-with-unread`, `needs-operator`, `offline-with-unread`, or `idle`.

## What this does not change

- `seat gallery` prints only the pane names, joins those panes into one window, tiles it, then attaches. It does not list unattended or awake seats. It does not move a pane that is not a registered live seat.
- The retired Python supervisor keeps its own words, including `offline-with-unread`.

## Version

0.2.0 defined the line; 0.4.0 deleted the waiter-derived `awake` with `seat arm`; 0.5.0 returned `awake` from the dispatcher's delivery state. This commit does not publish to crates.io.
