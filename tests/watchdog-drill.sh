#!/usr/bin/env bash
# Mail watchdog drill: real `seat init/register/send` on throwaway
# boards, a stub systemctl first on PATH that logs its argv, and one
# watchdog run plus one status check per case.
#
#   SEAT=$PWD/target/debug/seat tests/watchdog-drill.sh        (alone)
#
# tests/smoke.sh calls this with its own $SEAT, so the gate runs it
# too. Never the operator's live board.

set -u

: "${SEAT:?set SEAT to the seat binary (e.g. \$PWD/target/debug/seat)}"
case "$SEAT" in
    /*) ;;
    *) SEAT="$PWD/$SEAT" ;;
esac
if [ ! -x "$SEAT" ]; then
    echo "watchdog-drill: SEAT is not executable: $SEAT" >&2
    exit 70
fi

# Throwaway roots, always (smoke.sh's rule).
DRILL_DIR="$(mktemp -d)"
case "$DRILL_DIR" in
    "$HOME"/.local/share/seat)
        echo "watchdog-drill: refusing to run against the live board ($DRILL_DIR)" >&2
        exit 70
        ;;
esac
trap 'rm -rf "$DRILL_DIR"' EXIT

HERE="$(cd "$(dirname "$0")" && pwd)"
WATCHDOG="$HERE/../kit/scripts/seat-mail-watchdog.py"
if [ ! -f "$WATCHDOG" ]; then
    echo "watchdog-drill: watchdog script missing: $WATCHDOG" >&2
    exit 70
fi

# The stub systemctl: first on PATH, logs each argv, exit 0 (or 1 when
# $DRILL_DIR/exit-1 exists). The real systemctl is never reached.
STUBBIN="$DRILL_DIR/bin"
mkdir -p "$STUBBIN"
cat >"$STUBBIN/systemctl" <<'STUB'
#!/bin/sh
printf '%s\n' "$0 $*" >>"$STUB_SYSTEMCTL_LOG"
[ ! -f "${STUB_SYSTEMCTL_FAIL:-}" ] || exit 1
exit 0
STUB
chmod +x "$STUBBIN/systemctl"

S="$SEAT"
PASS=0
FAIL=0

note() {
    if [ "$1" -eq 0 ]; then
        PASS=$((PASS + 1))
    else
        FAIL=$((FAIL + 1))
        printf 'FAIL: %s (rc=%s)\n' "$2" "$1"
    fi
}

# One throwaway board per case, fresh heartbeat, $1 = case tag.
# Under DRILL_DIR so the EXIT trap removes it (smoke.sh's rule).
new_board() {
    R="$(mktemp -d "$DRILL_DIR/board.XXXXXX")"
    "$S" --root "$R" init >/dev/null 2>&1
    printf 'beat\n' >"$R/dispatch-heartbeat"
    touch -d 'now' "$R/dispatch-heartbeat"
}

# The watchdog pass (stub systemctl first on PATH), then the status
# file's problem keys, one per line.
dog() {
    STUB_SYSTEMCTL_LOG="$LOG" STUB_SYSTEMCTL_FAIL="$FAILFLAG" \
        PATH="$STUBBIN:$PATH" python3 -I "$WATCHDOG" --root "$R"
}
keys() {
    python3 -c 'import json,sys;[print(p["key"]) for p in json.load(open(sys.argv[1]))["problems"]]' \
        "$R/watchdog-status.json"
}
ok_is() {  # $1 expected ok value
    python3 -c 'import json,sys;sys.exit(0 if json.load(open(sys.argv[2]))["ok"]==(sys.argv[1]=="true") else 1)' \
        "$1" "$R/watchdog-status.json"
}

# Rename a sent mail file to a 20-minutes-older name ts (same id).
age_name() {  # $1 dir, $2 age spec for date (e.g. '20 minutes ago')
    f="$(ls "$1" | head -1)"
    ts="$(date -u -d "$2" +'%Y%m%dT%H%M%S.%6N')"
    mv "$1/$f" "$1/$ts-${f#*-}"
}

echo "watchdog-drill: cases"

# (1) stale heartbeat: run 1 restarts once and reports nothing; run 2,
# still stale, reports heartbeat and does not call again.
new_board heartbeat
LOG="$DRILL_DIR/hb.log"; : >"$LOG"
FAILFLAG=""
touch -d '5 minutes ago' "$R/dispatch-heartbeat"
out="$(dog)"; rc=$?
[ "$rc" -eq 0 ] && [ "$out" = "seat mail watchdog pass ok: 0 problems; restarted seat-dispatch.service" ] \
    && ok_is true && [ "$(keys)" = "" ]
                                                 note $? "stale heartbeat: restart once, report nothing"
[ "$(wc -l <"$LOG")" -eq 1 ];                    note $? "stale heartbeat: exactly one systemctl call"
grep -qx "$STUBBIN/systemctl --user restart seat-dispatch.service" "$LOG"
                                                 note $? "stale heartbeat: the call restarts seat-dispatch.service"
out="$(dog)"; rc=$?
[ "$rc" -eq 0 ] && [ "$out" = "seat mail watchdog pass ok: 1 problems" ] \
    && [ "$(keys)" = "heartbeat" ] && ok_is false
                                                 note $? "stale heartbeat: second run reports heartbeat"
[ "$(wc -l <"$LOG")" -eq 1 ];                    note $? "stale heartbeat: no second restart call"

# (2) inbox mail 20 minutes old on a webhook seat: overdue.
new_board overdue-inbox
"$S" --root "$R" register --seat A --harness sb >/dev/null 2>&1
"$S" --root "$R" register --seat B --harness sb >/dev/null 2>&1
printf 'https://127.0.0.1:9/hook\n' >"$R/seats/B/webhook.url"
"$S" --root "$R" send --from A B 'drill mail' >/dev/null 2>&1
age_name "$R/seats/B/inbox" '20 minutes ago'
out="$(dog)"; rc=$?
[ "$rc" -eq 0 ] && [ "$(keys)" = "overdue:B" ] && ok_is false
                                                 note $? "inbox: 20m mail on a webhook seat is overdue"
python3 -c 'import json,sys;d=json.load(open(sys.argv[1]));p=d["problems"][0];
sys.exit(0 if p["seat"]=="B" and p["kind"]=="overdue" and "route webhook" in p["detail"] and "limit 15m" in p["detail"] else 1)' \
    "$R/watchdog-status.json"
                                                 note $? "inbox: the problem names seat, route and limit"

# (3) the same in draining/, not leased: overdue. The lease
# deadline is a future mtime; an expired lease is a past one, so the
# mtime goes back to now and the age rides the name's ts.
new_board overdue-draining
"$S" --root "$R" register --seat A --harness sb >/dev/null 2>&1
"$S" --root "$R" register --seat B --harness sb >/dev/null 2>&1
printf 'https://127.0.0.1:9/hook\n' >"$R/seats/B/webhook.url"
"$S" --root "$R" send --from A B 'drill mail' >/dev/null 2>&1
"$S" --root "$R" read --seat B --lease >/dev/null 2>&1
age_name "$R/seats/B/draining" '20 minutes ago'
touch "$R/seats/B/draining/"*.msg
out="$(dog)"; rc=$?
[ "$rc" -eq 0 ] && [ "$(keys)" = "overdue:B" ]
                                                 note $? "draining: 20m unleased mail is overdue"

# (4) leased: nothing (a lease's deadline is still in the future).
# The mail is aged past the limit by name first, so only the lease
# can be what keeps it unreported.
new_board leased
"$S" --root "$R" register --seat A --harness sb >/dev/null 2>&1
"$S" --root "$R" register --seat B --harness sb >/dev/null 2>&1
printf 'https://127.0.0.1:9/hook\n' >"$R/seats/B/webhook.url"
"$S" --root "$R" send --from A B 'drill mail' >/dev/null 2>&1
"$S" --root "$R" read --seat B --lease >/dev/null 2>&1
age_name "$R/seats/B/draining" '20 minutes ago'
touch -d '+20 min' "$R/seats/B/draining/"*.msg
out="$(dog)"; rc=$?
[ "$rc" -eq 0 ] && [ "$(keys)" = "" ] && ok_is true
                                                 note $? "leased: mail under a lease reports nothing"

# (5) a quarantine file: quarantine.
new_board quarantine
"$S" --root "$R" register --seat A --harness sb >/dev/null 2>&1
"$S" --root "$R" register --seat B --harness sb >/dev/null 2>&1
printf 'https://127.0.0.1:9/hook\n' >"$R/seats/B/webhook.url"
"$S" --root "$R" send --from A B 'drill mail' >/dev/null 2>&1
mkdir -p "$R/seats/B/quarantine"
f="$(ls "$R/seats/B/inbox" | head -1)"
mv "$R/seats/B/inbox/$f" "$R/seats/B/quarantine/$f"
out="$(dog)"; rc=$?
[ "$rc" -eq 0 ] && [ "$(keys)" = "quarantine:B" ]
                                                 note $? "quarantine: a quarantined file is a problem"

# (6) garbage bindings: bindings.
new_board bindings
printf '{ nope' >"$R/hapi-sessions.json"
out="$(dog)"; rc=$?
[ "$rc" -eq 0 ] && [ "$(keys)" = "bindings" ] && ok_is false
                                                 note $? "bindings: garbage hapi-sessions.json is a problem"

# (7) a pull seat's old mail: nothing; after --mail-limit 5m: overdue.
new_board pull
"$S" --root "$R" register --seat A --harness sb >/dev/null 2>&1
"$S" --root "$R" register --seat P --harness sb >/dev/null 2>&1
"$S" --root "$R" send --from A P 'pull mail' >/dev/null 2>&1
age_name "$R/seats/P/inbox" '20 minutes ago'
out="$(dog)"; rc=$?
[ "$rc" -eq 0 ] && [ "$(keys)" = "" ] && ok_is true
                                                 note $? "pull: old mail is not checked without a limit"
"$S" --root "$R" register --seat P --harness sb --mail-limit 5m >/dev/null 2>&1
out="$(dog)"; rc=$?
[ "$rc" -eq 0 ] && [ "$(keys)" = "overdue:P" ] \
    && python3 -c 'import json,sys;d=json.load(open(sys.argv[1]));sys.exit(0 if "limit 5m" in d["problems"][0]["detail"] else 1)' \
        "$R/watchdog-status.json"
                                                 note $? "pull: a set --mail-limit is checked"

# (8) a failed restart is reported at once.
new_board failed-restart
LOG="$DRILL_DIR/fr.log"; : >"$LOG"
FAILFLAG="$DRILL_DIR/exit-1"; : >"$FAILFLAG"
touch -d '5 minutes ago' "$R/dispatch-heartbeat"
out="$(dog)"; rc=$?
[ "$rc" -eq 0 ] && [ "$(keys)" = "heartbeat" ] && ok_is false
                                                 note $? "failed restart: nonzero exit reported at once"
[ "$(wc -l <"$LOG")" -eq 1 ];                    note $? "failed restart: one call, then no more"

# (9) --mail-limit refuses a bad duration and writes nothing.
new_board bad-limit
"$S" --root "$R" register --seat A --harness sb >/dev/null 2>&1
before="$(cat "$R/seats/A/meta.json")"
"$S" --root "$R" register --seat A --harness sb --mail-limit 1.5h >/dev/null 2>&1; rc=$?
[ "$rc" -ne 0 ] && [ "$(cat "$R/seats/A/meta.json")" = "$before" ]
                                                 note $? "register refuses a bad --mail-limit"

# (10) no marker: one Error line, exit 1, no status written.
R="$(mktemp -d "$DRILL_DIR/board.XXXXXX")"
mkdir -p "$R/bin"
out="$(PATH="$STUBBIN:$PATH" python3 -I "$WATCHDOG" --root "$R" 2>&1)"; rc=$?
[ "$rc" -eq 1 ] && [ "$(printf '%s\n' "$out" | wc -l)" -eq 1 ] \
    && [ ! -e "$R/watchdog-status.json" ] && printf '%s' "$out" | grep -q '^Error:'
                                                 note $? "no marker: one Error line, exit 1"

# (11) a reader leases and dies: the mail prints again under the
# redelivery line naming its id, and `seat done` archives it by id,
# board-wide; a repeat succeeds.
new_board ack-hold-die
"$S" --root "$R" register --seat A --harness sb >/dev/null 2>&1
"$S" --root "$R" register --seat B --harness sb >/dev/null 2>&1
"$S" --root "$R" send --from A B 'drill held mail' >/dev/null 2>&1
out="$("$S" --root "$R" read --seat B --lease 2>&1)"; rc=$?
[ "$rc" -eq 0 ] && printf '%s\n' "$out" | grep -qF 'drill held mail' \
    && [ -z "$(ls "$R/seats/B/archive")" ]
                                                 note $? "lease prints the mail and archives nothing"
f="$(ls "$R/seats/B/draining" | head -1)"
id="${f#*-}"; id="${id%.msg}"
touch -d '1 min ago' "$R/seats/B/draining/$f"
out="$("$S" --root "$R" read --seat B --lease 2>&1)"; rc=$?
[ "$rc" -eq 0 ] \
    && printf '%s\n' "$out" | grep -qF "[redelivery] check whether you already did this; if so, only run: seat done $id"
                                                 note $? "redelivery: the line names the id"
printf '%s\n' "$out" | grep -qF 'drill held mail'
                                                 note $? "redelivery: the mail prints again"
"$S" --root "$R" done "$id" >/dev/null 2>&1
                                                 note $? "seat done exits 0"
[ -n "$(ls "$R/seats/B/archive")" ] && [ -z "$(ls "$R/seats/B/draining")" ]
                                                 note $? "done archives the leased file"
"$S" --root "$R" done "$id" >/dev/null 2>&1
                                                 note $? "a repeat done exits 0"

# (12) a mistyped id: refused saying why; nothing moves.
new_board ack-refused
"$S" --root "$R" register --seat A --harness sb >/dev/null 2>&1
"$S" --root "$R" register --seat B --harness sb >/dev/null 2>&1
"$S" --root "$R" send --from A B 'still unread' >/dev/null 2>&1
out="$("$S" --root "$R" done 0000000f 2>&1)"; rc=$?
[ "$rc" -eq 1 ] && printf '%s\n' "$out" | grep -qF 'not on this board' \
    && [ -n "$(ls "$R/seats/B/inbox")" ]
                                                 note $? "an unknown id exits 1 saying not on this board; nothing moves"
f="$(ls "$R/seats/B/inbox" | head -1)"
id="${f#*-}"; id="${id%.msg}"
out="$("$S" --root "$R" done "$id" 2>&1)"; rc=$?
[ "$rc" -eq 1 ] && printf '%s\n' "$out" | grep -qF 'unread' \
    && [ -n "$(ls "$R/seats/B/inbox")" ]
                                                 note $? "an inbox id exits 1 saying unread; nothing moves"

# (13) an ack under the wrong seat: the id is found board-wide.
new_board ack-wrong-seat
"$S" --root "$R" register --seat A --harness sb >/dev/null 2>&1
"$S" --root "$R" register --seat B --harness sb >/dev/null 2>&1
"$S" --root "$R" send --from A B 'wrong seat mail' >/dev/null 2>&1
"$S" --root "$R" read --seat B --lease >/dev/null 2>&1
f="$(ls "$R/seats/B/draining" | head -1)"
id="${f#*-}"; id="${id%.msg}"
"$S" --root "$R" read --seat A --done "$id" >/dev/null 2>&1
                                                 note $? "an ack under the wrong seat exits 0"
[ -n "$(ls "$R/seats/B/archive")" ] && [ -z "$(ls "$R/seats/B/draining")" ]
                                                 note $? "it archives B's file"

echo "watchdog-drill: $PASS passed, $FAIL failed (SEAT=$S)"
[ "$FAIL" -eq 0 ] || exit 1
exit 0
