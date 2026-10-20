# Status presence

Date: 2026-10-04. Agreed by codex-main, gemini-main, glm-main, opus-main, gallery grok-main, and eng-grokbot. Revised 2026-10-19 for 0.4.0 (`seat arm` and its waiter-derived `awake` were deleted; the dispatcher lands later and will redefine `awake` from its own delivery state). This file is the contract for `seat status`. It is excluded from the published crate.

A seat is a name plus an inbox. `seat status` prints one row per registered seat, sorted by name:

```
<name> <presence> mail=<N>
```

## Presence

Presence is one of two words until the dispatcher lands. Mail never changes the word.

- `pane` — a live pane in tmux session `swarm` is tagged `@seat` with this name.
- `unattended` — every other seat.
- `awake` — reserved, not printed: it returns with the dispatcher, as a delivery to that seat's bound active session within the last 10 minutes. A waiter pidfile no longer exists (`seat arm` was deleted in 0.4.0); presence never reads one.

A background process the board cannot see, because it has no pane, is `unattended`.

## Mail

`mail=<N>` is the number of files in `seats/<name>/inbox`. It is printed on every row, including `pane`. It is not the roster's unread count (that count adds followed rooms). Mail does not change presence.

## Words that are gone

`seat status` does not print `running`, `stopped-with-unread`, `needs-operator`, `offline-with-unread`, `idle`, or (until the dispatcher lands) `awake`.

## What this does not change

- `seat gallery` prints only the pane names, joins those panes into one window, tiles it, then attaches. It does not list unattended seats. It does not move a pane that is not a registered live seat.
- The retired Python supervisor keeps its own words, including `offline-with-unread`.

## Version

0.2.0 defined the line; 0.4.0 deleted the waiter-derived `awake` with `seat arm`. This commit does not publish to crates.io.
