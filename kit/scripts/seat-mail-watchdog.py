"""seat mail watchdog: reads board files only, restarts the dispatcher
once per stale-heartbeat incident, writes one status file, prints one
line.

Stdlib only; run as `python3 -I`. It opens only the oldest overdue
`.msg` (header and first body line), takes no lock, never runs `seat`,
calls no hub, sends no mail. The one thing it runs is `systemctl --user
restart seat-dispatch.service`, once per incident, output kept private.
"""
import argparse
import datetime as dt
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

HEARTBEAT = "dispatch-heartbeat"
BINDINGS = "hapi-sessions.json"
STATUS = "watchdog-status.json"
STATE = "watchdog-state.json"
MARKER_FILE = "marker"
MARKER = "seat"
SEATS = "seats"
HEARTBEAT_STALE_S = 60
DEFAULT_LIMIT_S = 15 * 60
RESTART_TIMEOUT_S = 120
SERVICE = "seat-dispatch.service"


def now():
    return dt.datetime.now(dt.timezone.utc)


def iso(moment):
    return moment.astimezone(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def parse_duration(raw):
    """`30m`, `2h`, `45s` — one integer plus one unit, an optional
    leading `+` (u64::from_str takes one, so register stores `+90m`)
    — else None (register's grammar)."""
    if not isinstance(raw, str) or not raw:
        return None
    unit = raw[-1:]
    per_s = {"s": 1, "m": 60, "h": 3600}.get(unit)
    if per_s is None:
        return None
    digits = raw[:-1]
    if digits.startswith("+"):
        digits = digits[1:]
    if not digits.isdigit() or not digits.isascii():
        return None
    seconds = int(digits) * per_s
    return seconds if seconds < 2 ** 63 else None


def file_ts(name):
    """A mail file name's `ts` prefix (`<ts>-<id>.msg`, UTC `%Y%m%dT%H%M%S.%f`)."""
    head = name.split("-", 1)[0]
    try:
        return dt.datetime.strptime(head, "%Y%m%dT%H%M%S.%f").replace(tzinfo=dt.timezone.utc)
    except ValueError:
        return None


def age_words(seconds):
    seconds = max(0, int(seconds))
    if seconds < 60:
        return f"{seconds}s"
    minutes, sec = divmod(seconds, 60)
    if minutes < 60:
        return f"{minutes}m" + (f"{sec:02d}s" if sec else "")
    hours, minutes = divmod(minutes, 60)
    return f"{hours}h{minutes:02d}m"


def mtime_of(path):
    try:
        return dt.datetime.fromtimestamp(path.stat().st_mtime, dt.timezone.utc)
    except OSError:
        return None


def owed_files(seat_dir, at):
    """(name, age seconds) for each not-leased `.msg` in inbox/ and
    draining/, oldest first. Age is the name's `ts`, else the file's
    mtime (the alarm skips ts-less names; this must not). A draining
    file whose mtime is in the future is leased and not owed."""
    found = []
    for stage in ("inbox", "draining"):
        for entry in sorted((seat_dir / stage).glob("*.msg")):
            try:
                mtime = dt.datetime.fromtimestamp(entry.stat().st_mtime, dt.timezone.utc)
            except OSError:
                continue
            if stage == "draining" and mtime > at:
                continue  # leased: a `--hold` deadline still in the future
            stamp = file_ts(entry.name)
            age = (at - stamp).total_seconds() if stamp is not None else (at - mtime).total_seconds()
            found.append((entry.name, max(0.0, age)))
    found.sort(key=lambda row: row[0])
    return found


def mail_fact(seat_dir, name):
    """`from`, `id`, `sent`, `ask` from an overdue `.msg`, or None when
    it cannot be read or parsed. `sent` is the header `ts` as ISO UTC;
    `ask` is the first body line, whitespace collapsed, 160 chars."""
    path = next((seat_dir / stage / name for stage in ("inbox", "draining")
                 if (seat_dir / stage / name).is_file()), None)
    if path is None:
        return None
    try:
        lines = path.read_text().splitlines()
        header = json.loads(lines[0])
        stamp = file_ts(header["ts"])
        sender, ident = header["from"], header["id"]
        ask = " ".join(next(ln for ln in lines[1:] if ln.strip()).split())[:160]
    except Exception:
        return None
    if stamp is None or not isinstance(sender, str) or not isinstance(ident, str):
        return None
    return {"from": sender, "id": ident, "sent": iso(stamp), "ask": ask}


def route_of(seat_dir, bound):
    """hapi / webhook / pull, the dispatcher's own rule: a binding (or
    an asleep mark — its wake retry runs through hapi) is hapi, else a
    `webhook.url` file is webhook, else pull."""
    if bound or has_asleep(seat_dir):
        return "hapi"
    if (seat_dir / "webhook.url").is_file():
        return "webhook"
    return "pull"


def has_asleep(seat_dir):
    try:
        state = json.loads((seat_dir / "dispatch.json").read_text())
    except (OSError, ValueError):
        return False
    return isinstance(state, dict) and isinstance(state.get("asleep"), dict)


def limit_for(seat_dir, route):
    """The seat's overdue limit in seconds: `mail_limit` if it parses
    (whatever the route — a set limit is checked), else 15 minutes for
    hapi and webhook, else no check at all."""
    try:
        meta = json.loads((seat_dir / "meta.json").read_text())
    except (OSError, ValueError):
        meta = None
    raw = meta.get("mail_limit") if isinstance(meta, dict) else None
    parsed = parse_duration(raw)
    if parsed is not None:
        return parsed, raw
    if route in ("hapi", "webhook"):
        return DEFAULT_LIMIT_S, "15m"
    return None, None


def bindings_ok(root):
    """(ok, mtime): a present bindings file must be an object with
    schema_version 1 and `seats` of objects each holding a string
    `url`. An absent file is fine (no bindings)."""
    path = root / BINDINGS
    moment = mtime_of(path)
    if moment is None:
        return True, None
    try:
        doc = json.loads(path.read_text())
    except (OSError, ValueError):
        return False, moment
    if not isinstance(doc, dict) or doc.get("schema_version") != 1:
        return False, moment
    seats = doc.get("seats")
    if not isinstance(seats, dict):
        return False, moment
    for row in seats.values():
        if not isinstance(row, dict) or not isinstance(row.get("url"), str):
            return False, moment
    return True, moment


def bound_seats(root):
    try:
        doc = json.loads((root / BINDINGS).read_text())
    except (OSError, ValueError):
        return set()
    seats = doc.get("seats") if isinstance(doc, dict) else None
    if not isinstance(seats, dict):
        return set()
    return {name for name in seats if isinstance(seats[name], dict)
            and isinstance(seats[name].get("url"), str)}


def load_state(root):
    try:
        doc = json.loads((root / STATE).read_text())
    except (OSError, ValueError):
        return None
    return doc if isinstance(doc, dict) else None


def store_state(root, doc):
    atomic_write(root / STATE, (json.dumps(doc) + "\n").encode())


def clear_state(root):
    try:
        (root / STATE).unlink()
    except OSError:
        pass


def atomic_write(path, payload):
    """Replace `path` in one step: a fresh 0600 temp in the same
    directory, fsync, rename, fsync the directory."""
    fd, name = tempfile.mkstemp(prefix="." + path.name + ".", dir=path.parent)
    try:
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, "wb") as handle:
            handle.write(payload)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(name, path)
    except BaseException:
        try:
            os.close(fd)
        except OSError:
            pass
        Path(name).unlink(missing_ok=True)
        raise
    dir_fd = os.open(path.parent, os.O_RDONLY)
    try:
        os.fsync(dir_fd)
    finally:
        os.close(dir_fd)


def restart_dispatcher():
    """One restart attempt, output kept private. Returns the exit
    code (nonzero also for a spawn failure or timeout)."""
    try:
        done = subprocess.run(["systemctl", "--user", "restart", SERVICE],
                              capture_output=True, timeout=RESTART_TIMEOUT_S)
        return done.returncode
    except (OSError, subprocess.TimeoutExpired):
        return 1


def heartbeat_fact(root, at):
    """(stale, incident): the heartbeat is stale when absent or its
    mtime is older than 60 s. The incident names the stale fact — its
    mtime, or `absent` — so one restart serves one incident."""
    moment = mtime_of(root / HEARTBEAT)
    if moment is None:
        return True, "absent", None
    age = (at - moment).total_seconds()
    return age > HEARTBEAT_STALE_S, iso(moment), moment


def check_board(root):
    at = now()
    problems = []
    restarted = False

    stale, incident, beat_moment = heartbeat_fact(root, at)
    if not stale:
        clear_state(root)
    else:
        state = load_state(root)
        if state is None or state.get("incident") != incident:
            code = restart_dispatcher()
            restarted = True
            store_state(root, {"incident": incident, "restarted_at": iso(at), "exit": code})
            if code != 0:
                problems.append({
                    "key": "heartbeat", "seat": None, "kind": "heartbeat",
                    "since": iso(beat_moment) if beat_moment else None,
                    "detail": "heartbeat stale"
                    + ("" if beat_moment else " (absent)")
                    + f"; restart of {SERVICE} exited nonzero; no second restart",
                })
        else:
            age = age_words((at - beat_moment).total_seconds()) if beat_moment else "unknown"
            problems.append({
                "key": "heartbeat", "seat": None, "kind": "heartbeat",
                "since": iso(beat_moment) if beat_moment else None,
                "detail": "heartbeat "
                + ("absent" if beat_moment is None else f"stale {age}")
                + f"; {SERVICE} was restarted once for this incident and it stayed stale",
            })

    ok, moment = bindings_ok(root)
    if not ok:
        problems.append({
            "key": "bindings", "seat": None, "kind": "bindings",
            "since": iso(moment) if moment else None,
            "detail": f"{BINDINGS} is present but not schema 1 with seats of url strings;"
                      " every binding is silently undelivered until it is fixed",
        })

    bound = bound_seats(root)
    seats_dir = root / SEATS
    try:
        names = sorted(p.name for p in seats_dir.iterdir()
                       if p.is_dir() and (p / "meta.json").is_file())
    except OSError:
        names = []
    for name in names:
        seat_dir = seats_dir / name
        route = route_of(seat_dir, name in bound)
        limit, limit_raw = limit_for(seat_dir, route)
        if limit is not None:
            owed = owed_files(seat_dir, at)
            past = [(n, a) for n, a in owed if a > limit]
            if past:
                oldest_name, oldest_age = max(past, key=lambda row: row[1])
                overdue_at = at - dt.timedelta(seconds=oldest_age - limit)
                problems.append({
                    "key": f"overdue:{name}", "seat": name, "kind": "overdue",
                    "since": iso(overdue_at),
                    "detail": f"{len(past)} of {len(owed)} owed past limit {limit_raw}"
                    f" on route {route}; oldest {oldest_name} age {age_words(oldest_age)}",
                    **(mail_fact(seat_dir, oldest_name) or {}),
                })
        try:
            quarantined = sorted(p for p in (seat_dir / "quarantine").iterdir() if p.is_file())
        except OSError:
            quarantined = []
        if quarantined:
            stamps = [file_ts(p.name) or mtime_of(p) for p in quarantined]
            stamps = [s for s in stamps if s is not None]
            oldest = min(stamps) if stamps else None
            problems.append({
                "key": f"quarantine:{name}", "seat": name, "kind": "quarantine",
                "since": iso(oldest) if oldest else None,
                "detail": f"{len(quarantined)} file(s) in quarantine; nothing reads them",
            })

    problems.sort(key=lambda row: row["key"])
    doc = {"checked_at": iso(at), "ok": not problems, "problems": problems}
    atomic_write(root / STATUS, (json.dumps(doc, indent=2) + "\n").encode())
    line = f"seat mail watchdog pass ok: {len(problems)} problems"
    if restarted:
        line += f"; restarted {SERVICE}"
    print(line)
    return 0


def resolve_root(argv):
    parser = argparse.ArgumentParser(prog="seat-mail-watchdog",
                                     description="read-only watchdog for the seat board")
    parser.add_argument("--root", help="board root (overrides SEAT_ROOT;"
                          " default ~/.local/share/seat)")
    args = parser.parse_args(argv)
    if args.root:
        return Path(args.root)
    env = os.environ.get("SEAT_ROOT")
    if env:
        return Path(env)
    return Path.home() / ".local" / "share" / "seat"


def marker_ok(root):
    try:
        return (root / MARKER_FILE).read_text().strip() == MARKER
    except OSError:
        return False


def main(argv):
    root = resolve_root(argv)
    if not marker_ok(root):
        print(f"Error: no seat board at {root} (run `seat init` or set SEAT_ROOT)")
        return 1
    return check_board(root)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
