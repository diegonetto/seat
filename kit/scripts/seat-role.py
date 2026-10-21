"""Implementation of seat-role wake ROLE. Stdlib only; child output stays private.

Cutover/quit sequencing is adapted from ~/sb-reroute/hapi-seat-{cutover,move}.sh.
The host scripts are deliberately not executed: their unscoped `seat up` and
flavor-only session selection can affect or identify a different seat.
"""
import csv
import datetime
import fcntl
import json
import os
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path


UUID = r"[0-9a-fA-F]{8}(?:-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}"
URL = re.compile(r"https://hapi\.bll\.dev/sessions/(" + UUID + r")\Z")
BUSY = re.compile(
    r"esc to interrupt|thinking|working\s*\(|working…|^\s*[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏]|"
    r"background work|do you want|❯\s*1\.|"
    r"\(y/n\)|esc to cancel", re.I | re.M
)
# "N shells/agents" also counts the MCP bridge HAPI spawns under the agent CLI, so it is busy
# only when the agent CLI has some other child process. Unknown is busy: never send keys on a guess.
BACKGROUND = re.compile(r"\b[0-9]+\s+(?:shells?|agents?)\b", re.I)
AGENT_CLI = re.compile(r"(^|/)(claude|codex|grok|opencode|agy)$")
NOT_WORK = re.compile(r"mcp", re.I)
HUB = "http://127.0.0.1:3006/api"
RESUME = {"resume", "--resume", "--continue", "-c", "-r", "--session-id", "--fork-session"}
# Child commands named in plain words; their output is never echoed.
TOOLS = {"tmux": "the swarm's tmux", "seat": "the seat command", "curl": "the phone hub (HAPI)",
         "sb-quota": "the quota check", "ps": "the process list"}
# tmux's own words for a server that is not running (stale or missing socket).
NO_SERVER = re.compile(r"no server running on |error connecting to .*\((?:No such file or directory|Connection refused)\)")


def _proc_children(pid, proc="/proc"):
    out = set()
    try:
        for tid in os.listdir(f"{proc}/{pid}/task"):
            try:
                with open(f"{proc}/{pid}/task/{tid}/children") as f:
                    out.update(int(c) for c in f.read().split())
            except OSError:
                pass
    except OSError:
        pass
    return out


def _proc_cmd(pid, proc="/proc"):
    try:
        with open(f"{proc}/{pid}/cmdline", "rb") as f:
            return f.read().replace(b"\0", b" ").decode("utf-8", "replace").strip()
    except OSError:
        return ""


def _agent_pid(pid, proc="/proc", depth=0):
    argv0 = _proc_cmd(pid, proc).split(" ", 1)[0]
    if AGENT_CLI.search(argv0):
        return pid
    if depth > 3:
        return None
    for child in sorted(_proc_children(pid, proc)):
        found = _agent_pid(child, proc, depth + 1)
        if found:
            return found
    return None


def background_work(pane_pid, proc="/proc"):
    """True when the pane's agent CLI has a child that is real work, or when it cannot be told (unknown is busy)."""
    agent = _agent_pid(pane_pid, proc)
    if agent is None:
        return True
    return any(not NOT_WORK.search(_proc_cmd(c, proc)) for c in _proc_children(agent, proc))


class Result(Exception):
    def __init__(self, status, reason=None, url=None):
        self.value = {"status": status}
        if reason:
            self.value["reason"] = reason
        if url:
            self.value["url"] = url


def fail(reason):
    raise Result("failed", reason)


def read_json(path):
    try:
        return json.loads(path.read_text())
    except (OSError, ValueError):
        fail("the file " + path.name + " is missing or unreadable")


def command(argv, *, data=None, timeout=5, cwd=None, deadline=None):
    if deadline is not None:
        timeout = min(timeout, deadline - time.monotonic())
        if timeout <= 0:
            fail("the phone hub took too long to answer")
    try:
        done = subprocess.run(argv, input=data, text=True, capture_output=True,
                              timeout=timeout, cwd=cwd)
    except (OSError, subprocess.TimeoutExpired):
        fail(TOOLS.get(Path(argv[0]).name, "a helper command") + " timed out or could not be run")
    if done.returncode:
        fail(TOOLS.get(Path(argv[0]).name, "a helper command") + " returned an error")
    return done.stdout


def atomic_json(path, value):
    # Callers hold the role-wake lock; unique temporary names also avoid clobbers.
    fd, name = tempfile.mkstemp(prefix=path.name + ".", dir=path.parent)
    try:
        with os.fdopen(fd, "w") as handle:
            json.dump(value, handle)
            handle.write("\n")
        os.replace(name, path)
    finally:
        Path(name).unlink(missing_ok=True)


def atomic_bytes(path, value):
    fd, name = tempfile.mkstemp(prefix=path.name + ".", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as handle:
            handle.write(value)
        os.replace(name, path)
    finally:
        Path(name).unlink(missing_ok=True)


_SEAT_MAP = None


def seat_map(home=None):
    """The kernel's seat map: `sb seats --json` (the kernel reads $SWITCHBOARD_HOME), once per process.

    The kernel's own stderr words refuse an absent or malformed map.
    """
    global _SEAT_MAP
    if _SEAT_MAP is not None:
        return _SEAT_MAP
    try:
        done = subprocess.run(["sb", "seats", "--json"], text=True, capture_output=True, timeout=15)
    except FileNotFoundError:
        fail("the seat command is not on PATH")
    except (OSError, subprocess.TimeoutExpired):
        fail("the seat map could not be read")
    if done.returncode:
        fail("the seat map: " + done.stderr.strip())
    try:
        seats = json.loads(done.stdout)["seats"]
    except (ValueError, KeyError, TypeError):
        fail("the seat map gave an answer that could not be read")
    if not isinstance(seats, dict):
        fail("the seat map gave an answer that could not be read")
    _SEAT_MAP = seats
    return seats


def map_roles(home=None):
    """The map's roles in its file order; the map says which roles exist."""
    return [str(row["role"]) for row in seat_map(home).values()
            if isinstance(row, dict) and isinstance(row.get("role"), str)]


def role_row(role, home=None):
    """(seat, record) for the map record whose role matches."""
    for seat, row in seat_map(home).items():
        if isinstance(row, dict) and row.get("role") == role:
            return seat, row
    fail("role " + role + " is not in the seat map; use " + ", ".join(map_roles(home)))


class Wake:
    def __init__(self, role):
        # HOME is never changed by this program or its tests.
        self.home = Path(os.environ.get("SEAT_ROLE_HOME", str(Path.home())))
        self.projects = self.home / "Projects"
        # One seat list: the kernel's map names the seat, kind, label and account (the tables and roles.json are gone).
        self.seat, record = role_row(role, self.home)
        kind, account, label = record.get("harness"), record.get("account"), record.get("name")
        if (not isinstance(kind, str) or not kind
                or not isinstance(account, str)
                or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", account)):
            fail("the seat map record for role " + role + " is invalid")
        self.role = role
        self.kind = kind
        self.label = label if isinstance(label, str) and label else role.capitalize()
        self.account = account
        self.runtime = Path(os.environ.get("SEAT_RUNTIMES", str(self.projects / ".seat-runtimes"))) / self.seat
        self.harness_path = self.runtime / "harness.json"
        self.state = self.home / ".local/share/seat"
        self.map_path = self.state / "hapi-sessions.json"
        self.hapi = str(self.home / ".local/bin/hapi")
        self.token = None
        self.session_name = None

    def memory_guard(self):
        path = Path(os.environ.get("SEAT_ROLE_MEMORY_LOG", "/mnt/c/Users/diego/slix-watchdog/log.csv"))
        try:
            with path.open() as handle:
                last = None
                for row in csv.reader(handle):
                    if row:
                        last = row
            free = int(last[1])
            if free < 0:
                raise ValueError
        except (OSError, ValueError, TypeError, IndexError):
            fail("Windows free memory could not be read, so nothing was started")
        if free < 3000:
            raise Result("refused", f"Windows has {free} MB free; a wake needs 3000 MB")

    def quota_guard(self, deadline=None):
        # A cold sb-quota after a reboot took over 13 s (2026-10-14 00:05 CT); allow 30.
        raw = command(["sb-quota", "status", "--all", "--json"], timeout=30, deadline=deadline)
        try:
            rows = json.loads(raw)["rows"]
            if not isinstance(rows, list):
                raise ValueError
            selected = [row for row in rows if isinstance(row, dict) and row.get("account") == self.account]
            verdicts = [row.get("verdict") for row in selected]
        except (ValueError, KeyError, TypeError):
            fail("the quota check gave an answer that could not be read")
        if "HOLD" in verdicts:
            # Do not echo provider text: it may include credentials or pane output.
            raise Result("refused", f"account {self.account} is on quota HOLD")
        if not verdicts or any(v != "OK" for v in verdicts):
            fail(f"the quota for account {self.account} is unknown, so nothing was started")

    def load_target(self):
        target = read_json(self.runtime / "wake-target.json")
        if (not isinstance(target, dict) or target.get("seat") != self.seat
                or target.get("harness") != self.kind
                or not re.fullmatch(r"%[0-9]+", str(target.get("pane", "")))
                or not isinstance(target.get("pane_pid"), int) or target["pane_pid"] <= 0
                or not isinstance(target.get("socket"), str) or not target["socket"].startswith("/")):
            fail("the seat's wake target file is invalid")
        return target

    def down(self, target):
        """True when the seat has no pane: no target, no tmux server, or no such pane."""
        if target is None:
            return self.swarm_confirms_down()
        done = self.list_panes(target["socket"], "#{pane_id}")
        if done.returncode == 0:
            return target["pane"] not in done.stdout.splitlines()
        if NO_SERVER.match(done.stderr):
            return self.swarm_confirms_down()
        fail("the swarm's tmux returned an error, so nothing was started")

    def list_panes(self, socket, fmt):
        try:
            return subprocess.run(["tmux", "-S", socket, "list-panes", "-a", "-F", fmt],
                                  text=True, capture_output=True, timeout=5)
        except (OSError, subprocess.TimeoutExpired):
            fail("the swarm's tmux timed out or could not be run")

    def swarm_confirms_down(self):
        """A missing target or a dead recorded socket proves nothing by itself.

        seat-sit records the socket it started on, which may be an old one. Ask the
        swarm's own socket (tmux's default for this user, never $TMUX) for the seat.
        """
        base = os.environ.get("TMUX_TMPDIR") or "/tmp"
        done = self.list_panes(str(Path(base) / f"tmux-{os.getuid()}" / "default"), "#{@seat}")
        if done.returncode == 0:
            if self.seat in done.stdout.splitlines():
                fail("the seat looks like it is still running in the swarm; nothing was started")
            return True
        if NO_SERVER.match(done.stderr):
            return True
        fail("the swarm's tmux returned an error, so nothing was started")

    def tmux(self, target, *args, deadline=None):
        return command(["tmux", "-S", target["socket"], *args], deadline=deadline)

    def pane_guard(self, target, deadline=None, check_busy=True):
        observed = self.tmux(target, "display-message", "-p", "-t", target["pane"],
                             "#{pane_pid}|#{@seat}|#{pane_dead}|#{pane_current_command}",
                             deadline=deadline).strip().split("|")
        if len(observed) != 4 or observed[:3] != [str(target["pane_pid"]), self.seat, "0"]:
            fail("the seat's tmux pane changed or has died; inspect it before retrying")
        # An absent busy marker is not permission to send /exit into a shell.
        if observed[3] not in self.harness.get("expect_cmds", []):
            fail("the seat's pane is not running its expected program")
        if not check_busy:
            return
        title = self.tmux(target, "display-message", "-p", "-t", target["pane"], "#{pane_title}", deadline=deadline)
        pane = self.tmux(target, "capture-pane", "-p", "-t", target["pane"], deadline=deadline)
        tail = "\n".join([line for line in pane.splitlines() if line.strip()][-8:])
        text = title + "\n" + tail
        if BUSY.search(text) or (BACKGROUND.search(text) and background_work(target["pane_pid"])):
            raise Result("busy")

    def hub(self, path, deadline, auth=None, patch=None):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            fail("the phone hub took too long to answer")
        limit = min(4, remaining)
        argv = ["curl", "--disable", "--silent", "--show-error", "--fail", "--noproxy", "*",
                "--connect-timeout", "2", "--max-time", str(limit)]
        if auth is not None:
            argv += ["-H", "Content-Type: application/json", "--data-binary", "@-"]
            data = json.dumps({"accessToken": auth})
        else:
            # Secrets travel over stdin, never argv, a temp file, or diagnostic output.
            argv += ["--config", "-"]
            data = "header = " + json.dumps("Authorization: Bearer " + self.token) + "\n"
            if patch is not None:
                argv += ["-X", "PATCH"]
                data += "header = " + json.dumps("Content-Type: application/json") + "\n"
                data += "data-binary = " + json.dumps(json.dumps(patch)) + "\n"
        raw = command([*argv, HUB + path], data=data, timeout=limit)
        try:
            return json.loads(raw)
        except ValueError:
            fail("the phone hub gave an answer that could not be read")

    def connect_hub(self, deadline):
        self.token = None
        settings = read_json(self.home / ".hapi/settings.json")
        self.machine = settings.get("machineId")
        access = settings.get("cliApiToken")
        if not isinstance(self.machine, str) or not self.machine or not isinstance(access, str) or not access:
            fail("the phone hub credentials or machine id are missing")
        token = self.hub("/auth", deadline, auth=access).get("token")
        if not isinstance(token, str) or not re.fullmatch(r"[A-Za-z0-9_.-]+", token):
            fail("the phone hub sign-in answer could not be read")
        self.token = token

    def matching(self, session):
        meta = session.get("metadata") or {}
        return (session.get("active") is True and meta.get("machineId") == self.machine
                and meta.get("flavor") == ("claude" if self.kind == "glm" else self.kind)
                and meta.get("path") == str(self.projects)
                and re.fullmatch(UUID, str(session.get("id", ""))))

    def find_session(self, target, deadline):
        self.session_name = None
        rows = self.hub("/sessions", deadline).get("sessions")
        if not isinstance(rows, list):
            fail("the phone hub gave an answer that could not be read")
        # HAPI list summaries omit hostPid. Fetch details before identifying a seat.
        parents = {}
        for line in command(["ps", "-eo", "pid=,ppid="], deadline=deadline).splitlines():
            pid, parent = map(int, line.split())
            parents[pid] = parent

        def in_pane(pid):
            visited = set()
            while isinstance(pid, int) and pid > 0 and pid not in visited:
                if pid == target["pane_pid"]:
                    return pid in parents
                visited.add(pid)
                pid = parents.get(pid)
            return False

        found = []
        for row in rows:
            if not isinstance(row, dict) or not self.matching(row):
                continue
            detail = self.hub("/sessions/" + row["id"], deadline).get("session")
            if (isinstance(detail, dict) and detail.get("id") == row["id"] and self.matching(detail)
                    and in_pane(detail["metadata"].get("hostPid"))):
                found.append(detail["id"])
                self.session_name = detail.get("metadata", {}).get("name")
        if len(found) > 1:
            fail("more than one phone session claims this seat")
        return "https://hapi.bll.dev/sessions/" + found[0] if found else None

    def rename(self, url, deadline):
        """Name the session for its role and seat; the manual name beats any
        title the hub invents on its own.

        A hand-set name is never fought over. A failure warns without hiding
        the session link; child output and anything that could carry a token
        stay private.
        """
        # Seats are named for their lab (the map's name: GLM, Claude, GPT, Grok, Gemini, Flight), never a job.
        name = self.label + " · " + self.seat
        # Name only a session that has no real name yet: blank, the folder name HAPI falls back to, or the name this
        # code used to give it ("Lander · codex-main"). A name Diego set by hand is never overwritten.
        current = (self.session_name or "").strip()
        ours_before = current == self.role.capitalize() + " · " + self.seat
        if current == name or (current and current != self.projects.name and not ours_before):
            return
        try:
            self.hub("/sessions/" + URL.fullmatch(url).group(1), deadline, patch={"name": name})
        except Exception as error:
            reason = error.value.get("reason", "an internal error") if isinstance(error, Result) else "an internal error"
            print(f"seat-role: {self.seat} is linked but was not named: {reason}", file=sys.stderr)

    def awake(self, target, url, deadline):
        if not URL.fullmatch(url):
            fail("the phone session link is malformed")
        self.pane_guard(target, deadline, check_busy=False)
        self.rename(url, deadline)
        state = self.read_map()
        stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        state["seats"][self.seat] = {"url": url, "moved_at": stamp}
        atomic_json(self.map_path, state)
        raise Result("awake", url=url)

    def read_map(self):
        state = read_json(self.map_path) if self.map_path.exists() else {"schema_version": 1, "seats": {}}
        if not isinstance(state, dict) or not isinstance(state.get("seats"), dict):
            fail("the saved phone links file (hapi-sessions.json) is unreadable")
        return state

    def is_wrapped(self, argv):
        """True when the seat runs under the phone link: `hapi <cli> …`, or glm's `hapi-glm` wrapper, which sets Z.ai's
        endpoint and key and then execs `hapi claude`."""
        if argv[:2] == [self.hapi, "claude" if self.kind == "glm" else self.kind]:
            return True
        return self.kind == "glm" and bool(argv) and Path(argv[0]).name == "hapi-glm"

    def wrap_blocker(self):
        """Plain reason this plain harness cannot be moved under HAPI, or None."""
        argv = self.harness["argv"]
        if self.kind == "glm":
            return "glm has no phone session yet"
        if Path(argv[0]).name in ("env", "hapi"):
            return "its command uses a wrapper the phone link cannot take over yet"
        if any(arg.split("=", 1)[0] in RESUME for arg in argv[1:]):
            return "its command resumes an old session, so it has no phone session yet"
        return None

    def await_session(self, previous, reason, backup=None):
        # seat up can return before seat-sit writes the target or the hub
        # advertises the new process. Retry only within this wake.
        # SEAT_ROLE_TEST_SESSION_WAIT exists for tests only.
        deadline = time.monotonic() + float(os.environ.get("SEAT_ROLE_TEST_SESSION_WAIT", "60"))
        while time.monotonic() < deadline:
            url = None
            try:
                new_target = self.load_target()
                if new_target != previous:
                    self.pane_guard(new_target, deadline, check_busy=False)
                    if self.token is None:
                        self.connect_hub(deadline)
                    url = self.find_session(new_target, deadline)
            except Result as result:
                if result.value["status"] != "failed":
                    raise
            if url:
                self.awake(new_target, url, deadline)
            time.sleep(min(1, max(0, deadline - time.monotonic())))
        if backup:
            # No session to show for the cutover: the next start uses the old command.
            atomic_bytes(self.harness_path, backup.read_bytes())
        fail(reason)

    def start_down(self, wrapped, preflight_deadline):
        """The seat has no pane (after a reboot, no tmux server): start it.

        Guards already ran. There is no pane to exit and nothing to interrupt.
        """
        blocker = None if wrapped else self.wrap_blocker()
        if blocker is None:
            deadline = min(preflight_deadline, time.monotonic() + 10)
            self.connect_hub(deadline)
            self.hub("/sessions", deadline)  # Verify hub availability before editing.
        # The saved URL died with the pane; awake() writes the new one.
        state = self.read_map()
        if state["seats"].pop(self.seat, None) is not None:
            atomic_json(self.map_path, state)
        backup = None
        if not wrapped and blocker is None:
            self.harness, backup = self.cutover()
        try:
            command(["seat", "up", self.seat], cwd=self.projects, timeout=30)
        except Result:
            if backup:
                atomic_bytes(self.harness_path, backup.read_bytes())
            fail(f"the swarm was not running and the seat command could not start {self.role}")
        if blocker:
            # The seat is up; it just has no phone session. Say so ("up"), never "failed": the board shows it awake
            # without Connect, and a failed answer would invite a retry that restarts a working seat.
            raise Result("up", f"{self.role} is running in its terminal, but {blocker}")
        self.await_session(None, "the swarm was not running; started it, but no session appeared within 60 seconds",
                           backup)

    def cutover(self):
        argv = self.harness["argv"]
        # Do not inherit a stored resume/continue command on this fresh-wake path.
        if any(arg.split("=", 1)[0] in RESUME for arg in argv[1:]):
            fail("the seat's stored command resumes an old session; a fresh wake is required")
        changed = dict(self.harness)
        changed["argv"] = [self.hapi, self.kind, *argv[1:]]
        changed["cwd"] = str(self.projects)
        changed["expect_cmds"] = sorted(set(changed["expect_cmds"]) | {"hapi"})
        changed.pop("session", None)
        if self.kind == "codex":
            changed["codex_bin"] = argv[0]
        # Preserve the exact previous bytes for host recovery.
        backup = self.harness_path.with_name("harness.json.pre-hapi." + str(time.time_ns()))
        backup.write_bytes(self.harness_path.read_bytes())
        atomic_json(self.harness_path, changed)
        return changed, backup

    def run(self):
        self.state.mkdir(parents=True, exist_ok=True)
        # Serialize restarts and map writes across roles; do not queue another tap.
        with (self.state / "role-wake.lock").open("a") as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                raise Result("busy")
            # Leave polling/HTTP overhead room in the already-awake 30s target.
            preflight_deadline = time.monotonic() + 45
            self.harness = read_json(self.harness_path)
            argv = self.harness.get("argv")
            if (self.harness.get("harness") != self.kind or not isinstance(argv, list) or not argv
                    or not all(isinstance(arg, str) and arg for arg in argv)
                    or not isinstance(self.harness.get("expect_cmds"), list)):
                fail("the seat's harness settings (harness.json) are invalid")
            try:
                target = self.load_target()
            except Result:
                # No usable target: down() asks the swarm's own socket before calling it down.
                target = None
            wrapped = self.is_wrapped(argv)
            down = self.down(target) if wrapped else None
            if wrapped and not down:
                # Identifying an existing session sends nothing to the pane. A busy
                # CLI, low memory, or a quota HOLD must not hide its live URL.
                reason = "the seat is running under the phone link, but no phone session for it is active"
                url = None
                try:
                    self.pane_guard(target, preflight_deadline, check_busy=False)
                    deadline = min(preflight_deadline, time.monotonic() + 10)
                    self.connect_hub(deadline)
                    url = self.find_session(target, deadline)
                except Result as result:
                    if result.value["status"] != "failed":
                        raise
                    reason = result.value["reason"]
                if url:
                    self.awake(target, url, deadline)
                self.await_session(None, reason)
            self.memory_guard()
            self.quota_guard(preflight_deadline)
            if down is None:
                down = self.down(target)
            if down:
                self.start_down(wrapped, preflight_deadline)
            self.pane_guard(target, preflight_deadline)
            if self.kind == "glm" and not wrapped:
                raise Result("up", "glm is running in its terminal; it has no phone session yet")
            deadline = min(preflight_deadline, time.monotonic() + 10)
            self.connect_hub(deadline)
            # Unknown wrapper shapes are not safe cutover inputs.
            if Path(argv[0]).name in ("env", "hapi"):
                fail("the seat's command uses a wrapper the phone link cannot take over")
            self.hub("/sessions", deadline)  # Verify hub availability before editing/quit.
            # Observe idle for 10 seconds; recheck right before the only pane input.
            for _ in range(5):
                time.sleep(2)
                self.pane_guard(target)
            self.memory_guard()
            self.quota_guard()
            self.pane_guard(target)
            if read_json(self.harness_path) != self.harness or self.load_target() != target:
                fail("the seat's settings changed during the idle check; nothing was sent")
            changed, backup = self.cutover()
            try:
                self.pane_guard(target)
                # One tmux invocation eliminates the script's one-second type/Enter gap.
                self.tmux(target, "send-keys", "-t", target["pane"],
                          "/quit" if self.kind == "codex" else "/exit", "Enter")
            except Result:
                atomic_bytes(self.harness_path, backup.read_bytes())
                raise
            exit_deadline = time.monotonic() + 30
            while target["pane"] in self.tmux(target, "list-panes", "-a", "-F", "#{pane_id}").splitlines():
                if time.monotonic() >= exit_deadline:
                    atomic_bytes(self.harness_path, backup.read_bytes())
                    fail("the seat's CLI did not exit; inspect the pane before retrying")
                time.sleep(1)
            try:
                command(["seat", "up", self.seat], cwd=self.projects, timeout=15)
            except Result:
                atomic_bytes(self.harness_path, backup.read_bytes())
                raise
            self.harness = changed
            self.await_session(target, "the seat restarted, but no phone session appeared within 60 seconds", backup)


def launch_model(argv, wrapper_default=None):
    """The model an argv launches with: its --model/-m value, else the wrapper's default, else None."""
    for i, arg in enumerate(argv):
        if arg in ("--model", "-m") and i + 1 < len(argv):
            return argv[i + 1]
        if arg.startswith("--model="):
            return arg.split("=", 1)[1]
    return wrapper_default


def wrapper_model(argv0):
    """ANTHROPIC_MODEL exported by a wrapper script such as hapi-glm, or None."""
    try:
        text = Path(argv0).read_text(errors="replace")
    except (OSError, ValueError):
        return None
    found = re.search(r"^\s*(?:export\s+)?ANTHROPIC_MODEL=(\S+)", text, re.M)
    return found.group(1).strip("\"'") if found else None


def launch_effort(argv, kind):
    """The effort an argv launches with, or None: codex `-c model_reasoning_effort=`, grok `--reasoning-effort`.
    claude states none (its settings file carries it); glm and agy carry none."""
    for i, arg in enumerate(argv):
        if kind == "codex" and arg == "-c" and argv[i + 1:i + 2] and argv[i + 1].startswith("model_reasoning_effort="):
            return argv[i + 1].split("=", 1)[1]
        if kind == "grok" and arg == "--reasoning-effort" and argv[i + 1:i + 2]:
            return argv[i + 1]
        if kind == "grok" and arg.startswith("--reasoning-effort="):
            return arg.split("=", 1)[1]
    return None


def settings_effort(home):
    """The effort claude's settings file carries (its launch never states one), or None."""
    try:
        value = json.loads((Path(home) / ".claude" / "settings.json").read_text()).get("effort")
    except (OSError, ValueError, AttributeError):
        return None
    return value if isinstance(value, str) and value else None


def pin_status(pin, argv, wrapper_default=None):
    """'ok', 'drift (<found>)' or 'unpinned' for one seat's launch argv against the map's pin."""
    found = launch_model(argv, wrapper_default)
    if pin is None or found is None:
        return "unpinned"
    return "ok" if found == pin else "drift (" + found + ")"


def pins():
    """Report, per seat in the map, the launch's model and effort against the map's pins. Edits nothing."""
    home = Path(os.environ.get("SEAT_ROLE_HOME", str(Path.home())))
    runtimes = Path(os.environ.get("SEAT_RUNTIMES", str(home / "Projects/.seat-runtimes")))
    report = {}
    for seat, row in seat_map(home).items():
        try:
            argv = json.loads((runtimes / seat / "harness.json").read_text()).get("argv")
        except (OSError, ValueError):
            argv = None
        if not isinstance(argv, list) or not argv or not all(isinstance(a, str) for a in argv):
            report[seat] = "unpinned"
            continue
        default = wrapper_model(argv[0]) if Path(argv[0]).name.startswith("hapi-") else None
        words = pin_status(row.get("model"), argv, default)
        effort = row.get("effort")
        if isinstance(effort, str) and effort:
            kind = row.get("harness")
            found = settings_effort(home) if kind == "claude" else launch_effort(argv, kind)
            if found is not None:
                words += ", effort " + ("ok" if found == effort else "drift (" + found + ")")
        report[seat] = words
    result = Result("pins")
    result.value["seats"] = report
    raise result


def main(args):
    try:
        if args == ["pins"]:
            pins()
        if len(args) != 2 or args[0] != "wake":
            fail("usage: seat-role wake <role> | seat-role pins")
        Wake(args[1]).run()
    except Result as result:
        value = result.value
    except Exception:
        # No traceback/child stderr: settings, provider responses and panes are private.
        value = {"status": "failed", "reason": "seat-role hit an internal error; nothing more is known"}
    print(json.dumps(value, separators=(",", ":")))
    return 1 if value["status"] == "failed" else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
