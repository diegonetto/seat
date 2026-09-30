#!/usr/bin/env bash
# Smoke suite for the seat operator binary (constitution 3.0, T014).
#
# Execs "$SEAT" as a program against throwaway --root directories.
# Never the operator's live board. Covers quickstart steps 1 through 7
# (step 8 is --help, checked by cargo test): fresh drain, second arm,
# room follow, bad mark, session verbs, status, foreign root.
#
#   SEAT=$PWD/target/debug/seat tests/smoke.sh
#
# Exit 0 = all checks passed; exit 1 = any failed.

set -u

: "${SEAT:?set SEAT to the seat binary (e.g. \$PWD/target/debug/seat)}"
case "$SEAT" in
    /*) ;;
    *) SEAT="$PWD/$SEAT" ;;
esac
if [ ! -x "$SEAT" ]; then
    echo "smoke: SEAT is not executable: $SEAT" >&2
    exit 70
fi

# Throwaway board, always. mktemp -d lands under TMPDIR (default /tmp).
ROOT="$(mktemp -d)"
EXTRA_ROOTS=""
case "$ROOT" in
    "$HOME"/.local/share/seat)
        echo "smoke: refusing to run against the live board ($ROOT)" >&2
        exit 70
        ;;
esac

PASS=0
FAIL=0

# Background waiter pids: always reaped so a failing check cannot leak
# an armed waiter past this script.
BG_PIDS=""
cleanup() {
    for p in $BG_PIDS; do kill "$p" 2>/dev/null; done
    for p in $BG_PIDS; do wait "$p" 2>/dev/null; done
    rm -rf "$ROOT"
    for r in $EXTRA_ROOTS; do rm -rf "$r"; done
}
trap cleanup EXIT

note() {
    if [ "$1" -eq 0 ]; then
        PASS=$((PASS + 1))
    else
        FAIL=$((FAIL + 1))
        printf 'FAIL: %s (rc=%s)\n' "$2" "$1"
    fi
}

# wait_log <file> <substr> <ticks>: 0 once the file contains substr
# (0.1s per tick; 50 ticks = the 5-second wake budget).
wait_log() {
    f=$1; s=$2; i=0; max=$3
    while [ "$i" -lt "$max" ]; do
        grep -qF -- "$s" "$f" 2>/dev/null && return 0
        sleep 0.1; i=$((i + 1))
    done
    grep -qF -- "$s" "$f" 2>/dev/null
}

S="$SEAT"
R="$ROOT"

## quickstart step 1: fresh drain (SC-002) ----------------------------
"$S" --root "$R" init >/dev/null 2>&1;               note $? "init"
"$S" --root "$R" register --seat A --harness sb >/dev/null 2>&1
                                                     note $? "register A (default lifecycle)"
"$S" --root "$R" register --seat B --harness sb --lifecycle poller >/dev/null 2>&1
                                                     note $? "register B (poller)"
"$S" --root "$R" send --from A B "hello seat" >/dev/null 2>&1
                                                     note $? "send A -> B"

# Arm B in the background: it must notice the mail within 5 seconds,
# naming the seat and the count, without draining it. B is never `up`.
"$S" --root "$R" arm --seat B >"$R/arm-b.log" 2>&1 &
ARMB=$!; BG_PIDS="$BG_PIDS $ARMB"
wait_log "$R/arm-b.log" "1 new message" 50;          note $? "arm B wakes within 5s naming the count"
grep -qF "seat 'B'" "$R/arm-b.log";                  note $? "the wake notice names seat B"
grep -qF "hello seat" "$R/arm-b.log" && rc=1 || rc=0
                                                     note $rc "arm never prints the body"
[ -s "$R/seats/B/wait.pid" ];                        note $? "the armed waiter holds wait.pid"

out="$("$S" --root "$R" drain --seat B 2>&1)"; rc=$?
[ "$rc" -eq 0 ] && printf '%s\n' "$out" | grep -qF "hello seat"
                                                     note $? "drain B returns the body (B never up)"
printf '%s\n' "$out" | grep -qF "hello seat" && [ "$(printf '%s\n' "$out" | grep -cF 'hello seat')" -eq 1 ]
                                                     note $? "the body appears exactly once"

# The default lifecycle (exit-wake) wakes once and exits 0.
"$S" --root "$R" send --from B A "reply seat" >/dev/null 2>&1
                                                     note $? "send B -> A"
"$S" --root "$R" arm --seat A >"$R/arm-a.log" 2>&1;  note $? "arm A (exit-wake) exits 0"
grep -qF "1 new message" "$R/arm-a.log";             note $? "arm A wake notice counts one"
out="$("$S" --root "$R" drain --seat A 2>&1)"; rc=$?
[ "$rc" -eq 0 ] && printf '%s\n' "$out" | grep -qF "reply seat"
                                                     note $? "drain A returns the reply"

## quickstart step 2: second arm --------------------------------------
out="$("$S" --root "$R" arm --seat B 2>&1)"; rc=$?
[ "$rc" -eq 0 ] && printf '%s\n' "$out" | grep -qF "already has a live waiter"
                                                     note $? "second arm reports the waiter and exits 0"
printf '%s\n' "$out" | grep -qF "waiting for mail" && rc=1 || rc=0
                                                     note $rc "second arm starts no second waiter"
kill -0 "$ARMB" 2>/dev/null;                         note $? "the first waiter is still the only one"
[ "$(cat "$R/seats/B/wait.pid")" = "$ARMB" ];        note $? "wait.pid still names the first waiter"

## quickstart step 3: rooms (FR-011) ----------------------------------
"$S" --root "$R" room create ops >/dev/null 2>&1;    note $? "room create ops"
"$S" --root "$R" room post ops "room bulletin" --from A >/dev/null 2>&1
                                                     note $? "room post from A"
out="$("$S" --root "$R" drain --seat B 2>&1)"; rc=$?
[ "$rc" -eq 0 ] && [ -z "$out" ];                    note $? "unfollowed room post is absent from drain"
"$S" --root "$R" room follow ops --seat B >/dev/null 2>&1
                                                     note $? "B follows ops"
out="$("$S" --root "$R" drain --seat B 2>&1)"; rc=$?
[ "$rc" -eq 0 ] && printf '%s\n' "$out" | grep -qF "room bulletin"
                                                     note $? "followed room post is drained"
[ "$(printf '%s\n' "$out" | grep -cF 'room bulletin')" -eq 1 ]
                                                     note $? "the post appears exactly once"
out="$("$S" --root "$R" drain --seat B 2>&1)"; rc=$?
[ "$rc" -eq 0 ] && [ -z "$out" ];                    note $? "cursor advanced: second drain empty"
"$S" --root "$R" room read ops 2>/dev/null | grep -qF "room bulletin"
                                                     note $? "room read shows the post"
"$S" --root "$R" room list 2>/dev/null | grep -qF "ops"
                                                     note $? "room list shows ops"

## quickstart step 4: bad mark (FR-012) -------------------------------
"$S" --root "$R" send --from A B "tamper target" >/dev/null 2>&1
                                                     note $? "send a message to tamper with"
f="$(ls "$R/seats/B/inbox" | sort | tail -1)"
sed -i 's/tamper target/tampered body/' "$R/seats/B/inbox/$f"
out="$("$S" --root "$R" drain --seat B 2>"$R/drain-bad.err")"; rc=$?
[ "$rc" -ne 0 ];                                     note $? "bad mark exits nonzero"
printf '%s\n' "$out" | grep -qF "tampered body" && rc=1 || rc=0
                                                     note $rc "bad mark prints no body"
grep -qF "from 'A'" "$R/drain-bad.err";              note $? "drain names the claimed from"
out="$("$S" --root "$R" drain --seat B 2>&1)"; rc=$?
[ "$rc" -eq 0 ] && [ -z "$out" ];                    note $? "board not wedged: next drain is clean"
"$S" --root "$R" send --from A B "byte flip" >/dev/null 2>&1
                                                     note $? "send a message to flip a body byte"
f="$(ls "$R/seats/B/inbox" | sort | tail -1)"
python3 -c 'import pathlib,sys; p=pathlib.Path(sys.argv[1]); b=bytearray(p.read_bytes()); i=b.rindex(b"byte flip"); b[i]=0xff; p.write_bytes(b)' "$R/seats/B/inbox/$f"
out="$("$S" --root "$R" drain --seat B 2>"$R/drain-utf8.err")"; rc=$?
[ "$rc" -ne 0 ];                                     note $? "non-utf8 body exits nonzero"
printf '%s\n' "$out" | grep -qF "byte" && rc=1 || rc=0
                                                     note $rc "non-utf8 body is not printed"
grep -qF "from 'A'" "$R/drain-utf8.err";             note $? "non-utf8 body names the claimed from"
# One line, not the same sentence twice.
[ "$(grep -cF "from 'A'" "$R/drain-utf8.err")" -eq 1 ]
                                                     note $? "claimed from is named once"

## quickstart step 5: session (SC-005) --------------------------------
# A fresh root, the fake multiplexer behind SEAT_TMUX, and no mail
# anywhere: this step never sends and never drains (US2 is mail-free).
SMOKE_DIR="$(cd "$(dirname "$0")" && pwd)"
FAKE="$SMOKE_DIR/fake-tmux.sh"
if [ ! -x "$FAKE" ]; then
    echo "smoke: fake multiplexer missing or not executable: $FAKE" >&2
    exit 70
fi
R4="$(mktemp -d)"; EXTRA_ROOTS="$EXTRA_ROOTS $R4"
FSTATE="$R4/tmux.state"
tm4() { SEAT_TMUX="$FAKE" SEAT_FAKE_STATE="$FSTATE" "$S" --root "$R4" "$@"; }

"$S" --root "$R4" init >/dev/null 2>&1;            note $? "step5: init a fresh root"
"$S" --root "$R4" register --seat A --harness sb --cwd "$R4" -- /bin/sleep 30 >/dev/null 2>&1
                                                    note $? "step5: register seat A with a launch"
"$S" --root "$R4" register --seat B --harness sb --cwd "$R4" -- /bin/sleep 30 >/dev/null 2>&1
                                                    note $? "step5: register seat B with a launch"

# `up` with no names brings every registered seat into one session.
tm4 up >/dev/null 2>&1;                            note $? "up with no names brings the roster"
[ "$(grep -c '^P|' "$FSTATE")" -eq 2 ];            note $? "one window per seat after up"
out="$(tm4 gallery)"; rc=$?
[ "$rc" -eq 0 ] && [ "$out" = "$(printf 'A\nB')" ]
                                                   note $? "gallery lists both running seats"
grep -q '^X|attach|' "$FSTATE" && rc=1 || rc=0
                                                   note $rc "gallery with SEAT_TMUX set does not attach"

# A second up reuses the session and duplicates nothing.
tm4 up >/dev/null 2>&1;                            note $? "second up exits 0"
[ "$(grep -c '^P|' "$FSTATE")" -eq 2 ];            note $? "second up opens no duplicate window"
out="$(tm4 gallery)"; [ "$out" = "$(printf 'A\nB')" ]
                                                   note $? "gallery still lists each seat once"

# An unknown name exits nonzero and names the seat.
out="$(tm4 up ghost 2>&1)"; rc=$?
[ "$rc" -ne 0 ] && printf '%s\n' "$out" | grep -qF "ghost"
                                                   note $? "up with an unknown seat names it"
out="$(tm4 down ghost 2>&1)"; rc=$?
[ "$rc" -ne 0 ] && printf '%s\n' "$out" | grep -qF "ghost"
                                                   note $? "down with an unknown seat names it"

# down kills the pane, prints no body, and keeps no duplicate state.
out="$(tm4 down B)"; rc=$?
[ "$rc" -eq 0 ] && [ -z "$out" ];                  note $? "down prints no message body"
out="$(tm4 gallery)"; [ "$out" = "A" ];            note $? "gallery lists the still-running seat only"
out="$(tm4 down B)"; rc=$?
[ "$rc" -eq 0 ] && [ -z "$out" ];                  note $? "down on an already-stopped seat exits 0"

# With nothing running, gallery reports empty and registers nothing.
tm4 down A >/dev/null 2>&1;                        note $? "down the last running seat"
out="$(tm4 gallery)"; rc=$?
[ "$rc" -eq 0 ] && [ "$out" = "gallery: empty" ];  note $? "gallery with nothing running reports empty"
[ "$(ls "$R4/seats" | wc -l)" -eq 2 ];             note $? "gallery registered no seat"
# No names stops every running seat, and still prints no body.
tm4 up >/dev/null 2>&1;                            note $? "up both seats again"
out="$(tm4 down)"; rc=$?
[ "$rc" -eq 0 ] && [ -z "$out" ];                  note $? "down with no names prints no body"
out="$(tm4 gallery)"; [ "$out" = "gallery: empty" ]
                                                   note $? "down with no names stops every seat"

## quickstart step 6: status (SC-004) ----------------------------------
# A fresh root with three seats in three states: runner stays up,
# fresh is never started with an empty inbox, parked is stopped with
# unread mail. Only runner may appear in gallery.
R5="$(mktemp -d)"; EXTRA_ROOTS="$EXTRA_ROOTS $R5"
F5="$R5/tmux.state"
tm5() { SEAT_TMUX="$FAKE" SEAT_FAKE_STATE="$F5" "$S" --root "$R5" "$@"; }

"$S" --root "$R5" init >/dev/null 2>&1;            note $? "step6: init a fresh root"
"$S" --root "$R5" register --seat fresh --harness sb >/dev/null 2>&1
                                                    note $? "step6: register fresh (never started)"
"$S" --root "$R5" register --seat parked --harness sb --cwd "$R5" -- /bin/sleep 30 >/dev/null 2>&1
                                                    note $? "step6: register parked (will stop with mail)"
"$S" --root "$R5" register --seat runner --harness sb --cwd "$R5" -- /bin/sleep 30 >/dev/null 2>&1
                                                    note $? "step6: register runner (stays up)"
"$S" --root "$R5" send --from runner parked "unread for the parked seat" >/dev/null 2>&1
                                                   note $? "step6: send mail to the seat that will stop"
tm5 up runner >/dev/null 2>&1;                     note $? "bring runner up"
tm5 up parked >/dev/null 2>&1;                     note $? "bring parked up before stopping it"

# Mail is unread while parked runs: running beats stopped-with-unread.
tm5 status | grep -qx "parked running";            note $? "running beats stopped-with-unread"

out="$(tm5 down parked)"; rc=$?
[ "$rc" -eq 0 ] && [ -z "$out" ];                  note $? "down prints no message body (parked)"
out="$(tm5 status)"; rc=$?
[ "$rc" -eq 0 ] && [ "$out" = "$(printf 'fresh needs-operator\nparked stopped-with-unread\nrunner running')" ]
                                                   note $? "status prints three rows and three states"
out="$(tm5 gallery)"; [ "$out" = "runner" ];       note $? "gallery prints only the running seat"
r1="$(tm5 status | awk '$2 == "running" { print $1 }')"
r2="$(tm5 gallery)"
[ "$r1" = "$r2" ];                                 note $? "the running set matches gallery"
[ "$(ls "$R5/seats/parked/inbox" | wc -l)" -eq 1 ]
                                                   note $? "down drained nothing: parked keeps its mail"

## seat-launch quickstart (003) steps 1, 2, 3, 6, 7 ---------------------
# The session record, read without attaching: tab-separated session,
# pane id, seat, dead, cwd-json, cmd-json (data model: Session record).
R6="$(mktemp -d)"; EXTRA_ROOTS="$EXTRA_ROOTS $R6"
F6="$R6/tmux.state"
tm6() { SEAT_TMUX="$FAKE" SEAT_FAKE_STATE="$F6" "$S" --root "$R6" "$@"; }
RECFMT=$'#{session_name}\t#{pane_id}\t#{@seat}\t#{pane_dead}\t#{@cwd}\t#{@cmd}'
record6() { SEAT_FAKE_STATE="$F6" "$FAKE" list-panes -a -F "$RECFMT"; }

"$S" --root "$R6" init >/dev/null 2>&1;            note $? "launch step 1: init"
# The third word, "sleep 30", is one word with a space inside it.
tm6 register --seat run --harness h --cwd "$R6" -- /bin/sh -c 'sleep 30' >/dev/null 2>&1
                                                    note $? "launch step 1: register run (cwd, 3-word command)"
tm6 register --seat mail --harness h >/dev/null 2>&1
                                                    note $? "launch step 1: register mail-only seat"
tm6 up >/dev/null 2>&1;                            note $? "launch step 1: bare up exits 0"
record6 >"$R6/record.txt"
python3 - "$R6/record.txt" "$F6" <<'PYEOF'
import json, sys
rows = [l.rstrip("\n").split("\t") for l in open(sys.argv[1])]
rows = [r for r in rows if len(r) >= 6 and r[2]]
assert len(rows) == 1, rows
r = rows[0]
assert r[0] == "swarm" and r[2] == "run" and r[3] == "0", r
cmd = json.loads(r[5])
cwd = json.loads(r[4])
assert cmd == ["/bin/sh", "-c", "sleep 30"], cmd
state = open(sys.argv[2]).read().splitlines()
take = lambda tag: [l.split("|", 2)[2] for l in state if l.startswith(tag + "|")]
L, R, W = take("L"), take("R"), take("W")
assert len(L) == len(R) == len(W) == 1, (L, R, W)
assert L[0] == cwd, (L[0], cwd)
raw = json.loads(R[0])
stored = json.loads(W[0])
assert raw[:2] == ["/usr/bin/env", "--"], raw
assert raw[2:] == stored == cmd, (raw, stored)
PYEOF
                                                    note $? "launch step 1: record, argv, -c, and @cmd agree; mail absent"

# Second up: still one pane, the words recorded once.
tm6 up >/dev/null 2>&1;                            note $? "launch step 2: second up exits 0"
[ "$(grep -c '^P|' "$F6")" -eq 1 ];                note $? "launch step 2: still one pane for run"
[ "$(grep -c '^W|' "$F6")" -eq 1 ];                note $? "launch step 2: command words appear once"

# One bad named target starts nothing.
: >"$R6/plainfile"
tm6 register --seat bad --harness h --cwd "$R6/plainfile" -- /bin/sleep 1 >/dev/null 2>&1
                                                    note $? "launch step 3: register bad (cwd not a directory)"
out="$(tm6 up bad run 2>&1)"; rc=$?
[ "$rc" -ne 0 ] && printf '%s\n' "$out" | grep -qF "bad"
                                                    note $? "launch step 3: up bad run fails naming bad"
[ "$(grep -c '^P|' "$F6")" -eq 1 ];                note $? "launch step 3: no pane added"

## seat-launch quickstart step 4: mail survives a new command ---------
# box starts mail-only, receives `kept`, then records a launch. The
# token is byte-for-byte the same and the inbox still drains (US2,
# SC-004).
tm6 register --seat box --harness h >/dev/null 2>&1
                                                    note $? "launch step 4: register box (mail-only)"
tm6 send --from run box kept >/dev/null 2>&1;       note $? "launch step 4: send kept from run to box"
cp "$R6/seats/box/token" "$R6/box.token"
tm6 register --seat box --harness h --cwd "$R6" -- /bin/sleep 5 >/dev/null 2>&1
                                                    note $? "launch step 4: box records a launch"
cmp -s "$R6/box.token" "$R6/seats/box/token";       note $? "launch step 4: token byte-for-byte the same"
out="$(tm6 drain --seat box 2>&1)"; rc=$?
[ "$rc" -eq 0 ] && printf '%s\n' "$out" | grep -qF "kept"
                                                    note $? "launch step 4: drain still prints kept"

## seat-launch quickstart step 5: the running command stays -----------
# A running seat keeps the command it was started with until down and
# up. down prints nothing and drains nothing (FR-007, FR-009).
box_cmd6() { record6 | awk -F'\t' '$3 == "box" { print $6 }'; }
tm6 up box >/dev/null 2>&1;                         note $? "launch step 5: up box starts the stored command"
[ "$(box_cmd6)" = '["/bin/sleep","5"]' ];           note $? "launch step 5: the record shows /bin/sleep 5"
tm6 register --seat box --harness h -- /bin/sleep 9 >/dev/null 2>&1
                                                    note $? "launch step 5: box records a new command"
tm6 up box >/dev/null 2>&1;                         note $? "launch step 5: up of a running box exits 0"
[ "$(box_cmd6)" = '["/bin/sleep","5"]' ];           note $? "launch step 5: the record still shows 5, not 9"
out="$(tm6 down box 2>&1)"; rc=$?
[ "$rc" -eq 0 ] && [ -z "$out" ];                   note $? "launch step 5: down prints nothing"
tm6 up box >/dev/null 2>&1;                         note $? "launch step 5: up box after down"
[ "$(box_cmd6)" = '["/bin/sleep","9"]' ];           note $? "launch step 5: the record shows the new command"
out="$(tm6 drain --seat box 2>&1)"; rc=$?
[ "$rc" -eq 0 ] && [ -z "$out" ];                   note $? "launch step 5: drain of box is empty"

## seat-launch quickstart step 6: fail mid-up ---------------------------
R7="$(mktemp -d)"; EXTRA_ROOTS="$EXTRA_ROOTS $R7"
F7="$R7/tmux.state"
tm7() { SEAT_TMUX="$FAKE" SEAT_FAKE_STATE="$F7" "$S" --root "$R7" "$@"; }
record7() { SEAT_FAKE_STATE="$F7" "$FAKE" list-panes -a -F "$RECFMT"; }

"$S" --root "$R7" init >/dev/null 2>&1;            note $? "launch step 6: init a fresh root"
for n in ok after; do
    tm7 register --seat "$n" --harness h --cwd "$R7" -- /bin/sleep 30 >/dev/null 2>&1
done;                                              note $? "launch step 6: register ok and after (/bin/sleep 30)"
tm7 register --seat die --harness h --cwd "$R7" -- seat-die >/dev/null 2>&1
                                                    note $? "launch step 6: register die (seat-die)"
out="$(tm7 up ok die after 2>&1)"; rc=$?
[ "$rc" -ne 0 ] && printf '%s\n' "$out" | grep -qF "die"
                                                    note $? "launch step 6: mid-up failure names die"
record7 | awk -F'\t' '
    $3 == "ok"     { ok = $4 }
    $3 == "die"    { die = $4 }
    $3 == "after"  { after = 1 }
    END { exit !(ok == "0" && die == "1" && after == 0) }'
                                                    note $? "launch step 6: ok alive, die not running, no pane for after"

## seat-launch quickstart step 7: cannot run ----------------------------
tm7 register --seat nope --harness h --cwd "$R7" -- seat-fail >/dev/null 2>&1
                                                    note $? "launch step 7: register nope (seat-fail)"
out="$(tm7 up nope 2>&1)"; rc=$?
[ "$rc" -ne 0 ] && printf '%s\n' "$out" | grep -qF "nope"
                                                    note $? "launch step 7: up nope fails naming nope"
grep -qF 'P|swarm|nope|' "$F7" && rc=1 || rc=0
                                                    note $rc "launch step 7: no pane for nope"

## quickstart step 7: foreign root (FR-010) ---------------------------
R2="$(mktemp -d)"; EXTRA_ROOTS="$EXTRA_ROOTS $R2"
mkdir -p "$R2/seats/ghost"
out="$("$S" --root "$R2" init 2>&1)"; rc=$?
[ "$rc" -ne 0 ] && printf '%s\n' "$out" | grep -qF "$R2"
                                                     note $? "init on a foreign root exits nonzero, names the path"
out="$("$S" --root "$R2" register --seat X --harness sb 2>&1)"; rc=$?
[ "$rc" -ne 0 ] && printf '%s\n' "$out" | grep -qF "$R2"
                                                     note $? "register refuses the foreign root"
out="$("$S" --root "$R2" drain --seat X 2>&1)"; rc=$?
[ "$rc" -ne 0 ] && printf '%s\n' "$out" | grep -qF "$R2"
                                                     note $? "drain refuses the foreign root"
"$S" --root "$R2" reset >/dev/null 2>&1;             note $? "reset works on the foreign root"
[ ! -e "$R2/seats" ];                                note $? "reset removed the foreign seats/"
"$S" --root "$R2" init >/dev/null 2>&1;              note $? "reset then init succeeds"
R3="$(mktemp -d)"; EXTRA_ROOTS="$EXTRA_ROOTS $R3"
"$S" --root "$R3" init >/dev/null 2>&1;              note $? "an empty directory is not refused"

## summary ----------------------------------------------------------------
echo "smoke: $PASS passed, $FAIL failed (SEAT=$S)"
[ "$FAIL" -eq 0 ] || exit 1
exit 0
