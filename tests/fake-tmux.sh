#!/usr/bin/env bash
# tests/fake-tmux.sh — a stateful fake multiplexer (T015, T002).
#
# Point SEAT_TMUX at this script and SEAT_FAKE_STATE at a file, and
# every instance sharing that file shares one "server". It answers the
# argument shapes seat actually issues, the way real tmux answers them:
#
#   has-session -t NAME                 exit 0 iff the session exists
#   new-session -d -s S -n W [-c DIR]
#                                       [-P -F FMT] [-- ARGV...]
#                                       the session's first window;
#                                       prints FMT for its pane
#   new-window  -d -t S -n W [-c DIR]
#                                       [-P -F FMT] [-- ARGV...]
#                                       a later window in S
#   set-option  -p -t PANE OPT VALUE    a pane option (@seat, @cwd,
#                                       @cmd)
#   kill-pane   -t PANE                 kills the pane; killing the
#                                       last pane removes the session
#   list-panes  -a -F FMT               one expanded line per pane
#   attach      -t S                    records the call, exits 0
#                                       immediately, never blocks
#
# Format expansion covers #{session_name}, #{window_name},
# #{pane_id}, #{@seat}, #{@cwd}, #{@cmd}, and #{pane_dead}. A pane with
# no @seat expands to an empty field, like real tmux's unset user
# option. #{@cwd} and #{@cmd} are whatever JSON the caller set.
# #{pane_dead} is 0 unless the pane was recorded dead.
#
# A launch (words after `--`) is recorded, never executed:
#   L|<pane>|<cwd>                 the -c value as given
#   R|<pane>|<raw argv json>       JSON array of every word after --
#   W|<pane>|<stored words json>   JSON array after a leading
#                                  /usr/bin/env -- prefix
# The first stored word seat-fail exits 1 and writes no pane. The
# first stored word seat-die writes the pane dead:
#   D|<pane>
#
# State file grammar (one record per line, '|' separated):
#   P|<session>|<window>|<pane>
#   O|<pane>|<option>|<value>
#   D|<pane>
#   L|<pane>|<cwd>
#   R|<pane>|<raw argv json>
#   W|<pane>|<stored words json>
#   X|attach|<session>
#
# This file is new work for the seat operator; it is not a port of any
# Python test.

set -u

STATE="${SEAT_FAKE_STATE:?fake-tmux: SEAT_FAKE_STATE must name the state file}"
touch "$STATE" 2>/dev/null || { echo "fake-tmux: cannot write $STATE" >&2; exit 1; }

die() { echo "fake-tmux: $*" >&2; exit 1; }

# Panes double as sessions: a session exists while it has panes, the
# way a real session dies with its last window.
session_exists() { grep -qF "P|$1|" "$STATE" 2>/dev/null; }

pane_exists() {
    awk -F'|' -v p="$1" '$1 == "P" && $4 == p { found = 1 } END { exit !found }' "$STATE"
}

# A pane whose command died (seat-die): pane_dead expands to 1.
pane_dead() {
    awk -F'|' -v p="$1" '$1 == "D" && $2 == p { found = 1 } END { exit !found }' "$STATE"
}

next_pane_id() {
    local max=0 tag _s _w p n
    while IFS='|' read -r tag _s _w p; do
        [ "$tag" = P ] || continue
        n="${p#%}"
        case "$n" in '' | *[!0-9]*) continue ;; esac
        [ "$n" -gt "$max" ] && max=$n
    done < <(grep '^P|' "$STATE" 2>/dev/null || true)
    printf '%%%s' "$((max + 1))"
}

# The last value set on a pane option, empty when never set.
option_value() {
    grep -F "O|$1|$2|" "$STATE" 2>/dev/null | tail -n 1 | cut -d'|' -f4-
}

# One string as JSON string content (escape \ " tab nl cr).
json_escape() {
    local s="$1"
    s="${s//\\/\\\\}"
    s="${s//\"/\\\"}"
    s="${s//$'\t'/\\t}"
    s="${s//$'\n'/\\n}"
    s="${s//$'\r'/\\r}"
    printf '%s' "$s"
}

# argv words as a compact JSON array of strings.
json_array() {
    local out="[" first=1 w
    for w in "$@"; do
        [ "$first" -eq 1 ] || out+=","
        first=0
        out+="\"$(json_escape "$w")\""
    done
    printf '%s%s' "$out" "]"
}

# expand_format FMT SESSION WINDOW PANE — one line on stdout.
expand_format() {
    local out="$1" v=""
    v="$(option_value "$4" "@seat")"
    out="${out//'#{session_name}'/$2}"
    out="${out//'#{window_name}'/$3}"
    out="${out//'#{pane_id}'/$4}"
    out="${out//'#{@seat}'/$v}"
    v="$(option_value "$4" "@cwd")"
    out="${out//'#{@cwd}'/$v}"
    v="$(option_value "$4" "@cmd")"
    out="${out//'#{@cmd}'/$v}"
    if pane_dead "$4"; then v=1; else v=0; fi
    out="${out//'#{pane_dead}'/$v}"
    printf '%s\n' "$out"
}

[ $# -ge 1 ] || die "no command"
cmd="$1"
shift

sess="" target="" name="" fmt="" cwd="" print=0 all=0 paneflag=0
pos=()
while [ $# -gt 0 ]; do
    case "$1" in
        -t) [ $# -ge 2 ] || die "-t needs a value"; target="$2"; shift 2 ;;
        -s) [ $# -ge 2 ] || die "-s needs a value"; sess="$2"; shift 2 ;;
        -n) [ $# -ge 2 ] || die "-n needs a value"; name="$2"; shift 2 ;;
        -c) [ $# -ge 2 ] || die "-c needs a value"; cwd="$2"; shift 2 ;;
        -F) [ $# -ge 2 ] || die "-F needs a value"; fmt="$2"; shift 2 ;;
        -p) paneflag=1; shift ;;
        -P) print=1; shift ;;
        -a) all=1; shift ;;
        -d) shift ;;
        # Everything after -- is the command's raw argv: recorded, and
        # never executed. Words here may start with '-'.
        --) shift; while [ $# -gt 0 ]; do pos+=("$1"); shift; done ;;
        -*) die "unsupported flag: $1" ;;
        *) pos+=("$1"); shift ;;
    esac
done

# The stored command: the words after a leading /usr/bin/env --.
stored=()
if [ "${#pos[@]}" -ge 2 ] && [ "${pos[0]}" = "/usr/bin/env" ] && [ "${pos[1]}" = "--" ]; then
    stored=("${pos[@]:2}")
elif [ "${#pos[@]}" -gt 0 ]; then
    stored=("${pos[@]}")
fi

# Record one launch beside its pane, then report it. seat-fail exits 1
# before any pane exists; seat-die records the pane dead.
record_launch() {
    local pane="$1"
    printf 'L|%s|%s\n' "$pane" "$cwd" >>"$STATE"
    printf 'R|%s|%s\n' "$pane" "$(json_array "${pos[@]}")" >>"$STATE"
    printf 'W|%s|%s\n' "$pane" "$(json_array "${stored[@]}")" >>"$STATE"
}

case "$cmd" in
    has-session)
        [ -n "$target" ] || die "has-session needs -t"
        session_exists "$target" && exit 0
        exit 1
        ;;
    new-session)
        [ -n "$sess" ] || die "new-session needs -s"
        [ -n "$name" ] || die "new-session needs -n"
        session_exists "$sess" && die "duplicate session: $sess"
        if [ "${#stored[@]}" -gt 0 ] && [ "${stored[0]}" = "seat-fail" ]; then
            die "launch refused: seat-fail"
        fi
        pane="$(next_pane_id)"
        printf 'P|%s|%s|%s\n' "$sess" "$name" "$pane" >>"$STATE"
        if [ "${#stored[@]}" -gt 0 ] && [ "${stored[0]}" = "seat-die" ]; then
            printf 'D|%s\n' "$pane" >>"$STATE"
        fi
        [ "${#pos[@]}" -gt 0 ] && record_launch "$pane"
        [ "$print" -eq 1 ] && expand_format "$fmt" "$sess" "$name" "$pane"
        exit 0
        ;;
    new-window)
        [ -n "$target" ] || die "new-window needs -t"
        [ -n "$name" ] || die "new-window needs -n"
        session_exists "$target" || die "no such session: $target"
        if [ "${#stored[@]}" -gt 0 ] && [ "${stored[0]}" = "seat-fail" ]; then
            die "launch refused: seat-fail"
        fi
        pane="$(next_pane_id)"
        printf 'P|%s|%s|%s\n' "$target" "$name" "$pane" >>"$STATE"
        if [ "${#stored[@]}" -gt 0 ] && [ "${stored[0]}" = "seat-die" ]; then
            printf 'D|%s\n' "$pane" >>"$STATE"
        fi
        [ "${#pos[@]}" -gt 0 ] && record_launch "$pane"
        [ "$print" -eq 1 ] && expand_format "$fmt" "$target" "$name" "$pane"
        exit 0
        ;;
    set-option)
        [ "$paneflag" -eq 1 ] || die "set-option: only -p is supported here"
        [ -n "$target" ] || die "set-option needs -t"
        [ "${#pos[@]}" -eq 2 ] || die "set-option needs OPTION VALUE"
        pane_exists "$target" || die "no such pane: $target"
        printf 'O|%s|%s|%s\n' "$target" "${pos[0]}" "${pos[1]}" >>"$STATE"
        exit 0
        ;;
    kill-pane)
        [ -n "$target" ] || die "kill-pane needs -t"
        pane_exists "$target" || die "no such pane: $target"
        tmp="${STATE}.tmp.$$"
        awk -F'|' -v p="$target" '
            $1 == "P" && $4 == p { next }
            $1 == "O" && $2 == p { next }
            $1 == "D" && $2 == p { next }
            $1 == "L" && $2 == p { next }
            $1 == "R" && $2 == p { next }
            $1 == "W" && $2 == p { next }
            { print }' "$STATE" >"$tmp" || { rm -f "$tmp"; die "state rewrite failed"; }
        mv "$tmp" "$STATE"
        exit 0
        ;;
    list-panes)
        [ "$all" -eq 1 ] || die "list-panes: only -a is supported here"
        while IFS='|' read -r tag s w p; do
            [ "$tag" = P ] || continue
            expand_format "$fmt" "$s" "$w" "$p"
        done < <(grep '^P|' "$STATE" 2>/dev/null || true)
        exit 0
        ;;
    attach)
        # The one call a real multiplexer blocks on. The fake records
        # it and returns immediately so a test never hangs.
        [ -n "$target" ] || die "attach needs -t"
        printf 'X|attach|%s\n' "$target" >>"$STATE"
        exit 0
        ;;
    *)
        die "unsupported command: $cmd"
        ;;
esac
