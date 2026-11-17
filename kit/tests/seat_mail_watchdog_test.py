"""seat-mail-watchdog behavior tests: routes, grammar, leases, ts-less
names, mode-000 mail, bindings cases, restart-once recovery, status
shape and the one stdout line. No live board, no real systemctl, no
network: the restart is patched and every board is a throwaway temp
directory with the marker written."""

import datetime as dt
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch


sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("seat_watchdog", ROOT / "scripts/seat-mail-watchdog.py")
DOG = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(DOG)


def write_msg(directory: Path, name: str, age_s: float = 0, mode: int | None = None,
              body: str = "x") -> None:
    """One mail file, aged by setting its mtime (and its name's ts when
    the name carries one)."""
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / name
    path.write_text(body)
    if mode is not None:
        path.chmod(mode)
    if age_s:
        stamp = dt.datetime.now(dt.timezone.utc) - dt.timedelta(seconds=age_s)
        os.utime(path, (stamp.timestamp(), stamp.timestamp()))


def ts_name(age_s: float, ident: str = "0000aaaa") -> str:
    stamp = dt.datetime.now(dt.timezone.utc) - dt.timedelta(seconds=age_s)
    return stamp.strftime("%Y%m%dT%H%M%S.%f") + "-" + ident + ".msg"


class Board:
    """A throwaway board the watchdog accepts."""

    def __init__(self, tmp: Path):
        self.root = tmp
        (self.root / "seats").mkdir(parents=True)
        (self.root / "marker").write_text("seat\n")

    def seat(self, name: str, mail_limit: str | None = None, asleep: bool = False,
             webhook: bool = False, bound: bool = False) -> Path:
        directory = self.root / "seats" / name
        (directory / "inbox").mkdir(parents=True, exist_ok=True)
        (directory / "draining").mkdir(exist_ok=True)
        meta = {"harness": "sb"}
        if mail_limit:
            meta["mail_limit"] = mail_limit
        (directory / "meta.json").write_text(json.dumps(meta))
        if asleep:
            (directory / "dispatch.json").write_text(
                json.dumps({"asleep": {"at": "2026-11-17T00:00:00Z", "reason": "idle 30m"}}))
        if webhook:
            (directory / "webhook.url").write_text("https://127.0.0.1:9/hook\n")
        if bound:
            self.bind(name)
        return directory

    def bind(self, name: str, url: str | None = None):
        bindings = self.root / "hapi-sessions.json"
        doc = json.loads(bindings.read_text()) if bindings.exists() else {
            "schema_version": 1, "seats": {}}
        doc["seats"][name] = {"url": url or f"https://hapi.bll.dev/sessions/{name}-id"}
        bindings.write_text(json.dumps(doc))

    def heartbeat(self, age_s: float = 0):
        path = self.root / "dispatch-heartbeat"
        path.write_text("2026-11-17T20:00:00+00:00\n")
        if age_s:
            stamp = dt.datetime.now(dt.timezone.utc) - dt.timedelta(seconds=age_s)
            os.utime(path, (stamp.timestamp(), stamp.timestamp()))


class WatchdogTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.board = Board(Path(self.tmp.name))
        self.board.heartbeat()  # fresh unless a case ages it

    def run_dog(self, restarts=None, exit_code=0):
        """One watchdog pass with the restart patched. `restarts`
        collects each restart argv."""
        calls = []

        def fake_restart():
            calls.append(["systemctl", "--user", "restart", DOG.SERVICE])
            return exit_code

        with patch.object(DOG, "restart_dispatcher", side_effect=fake_restart):
            code = DOG.main(["--root", str(self.board.root)])
        status = json.loads((self.board.root / "watchdog-status.json").read_text())
        if restarts is not None:
            restarts.extend(calls)
        return code, status

    def keys(self, status):
        return [p["key"] for p in status["problems"]]

    def test_fresh_heartbeat_clears_state_and_reports_ok(self):
        self.board.heartbeat(age_s=300)
        _, first = self.run_dog()  # restart recorded, no problem this run
        self.assertEqual(self.keys(first), [])
        self.assertTrue((self.board.root / "watchdog-state.json").exists())
        self.board.heartbeat()  # fresh: the incident cleared
        _, second = self.run_dog()
        self.assertEqual(self.keys(second), [])
        self.assertFalse((self.board.root / "watchdog-state.json").exists())

    def test_route_bound_or_asleep_is_hapi(self):
        directory = self.board.seat("plain")
        self.assertEqual(DOG.route_of(directory, bound=False), "pull")
        self.assertEqual(DOG.route_of(directory, bound=True), "hapi")
        hooked = self.board.seat("hook", webhook=True)
        self.assertEqual(DOG.route_of(hooked, bound=False), "webhook")
        self.assertEqual(DOG.route_of(hooked, bound=True), "hapi")
        sleeper = self.board.seat("snooze", asleep=True, webhook=True)
        self.assertEqual(DOG.route_of(sleeper, bound=False), "hapi",
                         "an asleep seat wakes through hapi even with a hook staged")

    def test_route_detail_names_the_route(self):
        self.board.seat("hook", webhook=True)
        write_msg(self.board.root / "seats/hook/inbox", ts_name(20 * 60))
        _, status = self.run_dog()
        self.assertEqual(self.keys(status), ["overdue:hook"])
        self.assertIn("route webhook", status["problems"][0]["detail"])

    # -- grammar ----------------------------------------------------------

    def test_duration_grammar(self):
        for good, seconds in [("45s", 45), ("30m", 1800), ("2h", 7200), ("1s", 1)]:
            self.assertEqual(DOG.parse_duration(good), seconds, good)
        for bad in ["1.5h", "10", "10d", "m", "", None, "30x", "h2", "-5m", "0x10", "９０"]:
            self.assertIsNone(DOG.parse_duration(bad), bad)

    # -- owed: leases, names, modes ----------------------------------------

    def test_leased_draining_mail_is_not_owed_and_plain_draining_is(self):
        directory = self.board.seat("hook", webhook=True)
        leased = ts_name(20 * 60, "0000bbbb")
        plain = ts_name(20 * 60, "0000cccc")
        write_msg(directory / "draining", leased)
        write_msg(directory / "draining", plain)
        future = dt.datetime.now(dt.timezone.utc) + dt.timedelta(minutes=20)
        os.utime(directory / "draining" / leased, (future.timestamp(), future.timestamp()))
        _, status = self.run_dog()
        self.assertEqual(self.keys(status), ["overdue:hook"])
        self.assertIn("1 of 1 owed", status["problems"][0]["detail"])
        self.assertIn(plain, status["problems"][0]["detail"])

    def test_a_ts_less_name_is_aged_by_its_mtime(self):
        directory = self.board.seat("hook", webhook=True)
        write_msg(directory / "inbox", "noname.msg", age_s=20 * 60)
        _, status = self.run_dog()
        self.assertEqual(self.keys(status), ["overdue:hook"])
        self.assertIn("noname.msg", status["problems"][0]["detail"])

    def test_mode_000_msg_files_are_counted_without_being_read(self):
        directory = self.board.seat("hook", webhook=True)
        name = ts_name(20 * 60)
        write_msg(directory / "inbox", name, mode=0)
        _, status = self.run_dog()
        self.assertEqual(self.keys(status), ["overdue:hook"])
        self.assertEqual((directory / "inbox" / name).stat().st_mode & 0o777, 0,
                         "the file was never opened for reading or writing")

    def test_pull_seat_old_mail_is_unchecked_until_a_limit_is_set(self):
        directory = self.board.seat("pull")
        write_msg(directory / "inbox", ts_name(20 * 60))
        _, status = self.run_dog()
        self.assertEqual(self.keys(status), [], "pull seats are run 3's, not this run's")
        # A set limit is checked whatever the route.
        (directory / "meta.json").write_text(json.dumps(
            {"harness": "sb", "mail_limit": "5m"}))
        _, status = self.run_dog()
        self.assertEqual(self.keys(status), ["overdue:pull"])
        self.assertIn("limit 5m", status["problems"][0]["detail"])

    def test_an_unparseable_limit_falls_back_to_the_default(self):
        directory = self.board.seat("hook", webhook=True)
        (directory / "meta.json").write_text(json.dumps(
            {"harness": "sb", "mail_limit": "1.5h"}))
        write_msg(directory / "inbox", ts_name(20 * 60))
        _, status = self.run_dog()
        self.assertEqual(self.keys(status), ["overdue:hook"])
        self.assertIn("limit 15m", status["problems"][0]["detail"])

    # -- bindings -----------------------------------------------------------

    def test_bindings_cases(self):
        path = self.board.root / "hapi-sessions.json"
        cases = [
            "{ nope",
            json.dumps({"schema_version": 2, "seats": {}}),
            json.dumps({"schema_version": 1, "seats": []}),
            json.dumps({"schema_version": 1, "seats": {"a": {"moved_at": "x"}}}),
            json.dumps({"schema_version": 1, "seats": {"a": {"url": 7}}}),
        ]
        for bad in cases:
            path.write_text(bad)
            ok, moment = DOG.bindings_ok(self.board.root)
            self.assertFalse(ok, bad)
        path.write_text(json.dumps(
            {"schema_version": 1, "seats": {"a": {"url": "https://h/x", "moved_at": "y"}}}))
        self.assertTrue(DOG.bindings_ok(self.board.root)[0])
        path.unlink()
        self.assertTrue(DOG.bindings_ok(self.board.root)[0], "absent is no bindings, not a problem")

    def test_garbage_bindings_is_one_board_wide_problem(self):
        (self.board.root / "hapi-sessions.json").write_text("{ nope")
        _, status = self.run_dog()
        self.assertEqual(self.keys(status), ["bindings"])
        problem = status["problems"][0]
        self.assertIsNone(problem["seat"])
        self.assertEqual(problem["kind"], "bindings")

    # -- quarantine ---------------------------------------------------------

    def test_any_quarantine_file_is_a_problem_while_it_stays(self):
        directory = self.board.seat("hook", webhook=True)
        write_msg(directory / "quarantine", ts_name(3 * 3600, "deadbeef"))
        _, status = self.run_dog()
        self.assertEqual(self.keys(status), ["quarantine:hook"])
        self.assertIn("1 file(s) in quarantine", status["problems"][0]["detail"])
        _, again = self.run_dog()
        self.assertEqual(self.keys(again), ["quarantine:hook"], "open while it stays")

    # -- heartbeat recovery ---------------------------------------------------

    def test_stale_heartbeat_restarts_once_and_reports_next_run(self):
        self.board.heartbeat(age_s=300)
        restarts = []
        _, first = self.run_dog(restarts=restarts)
        self.assertEqual(self.keys(first), [], "a clean restart reports nothing this run")
        self.assertEqual(restarts, [["systemctl", "--user", "restart", "seat-dispatch.service"]])
        state = json.loads((self.board.root / "watchdog-state.json").read_text())
        self.assertEqual(sorted(state), ["exit", "incident", "restarted_at"])
        # Still stale, same incident: the problem names the restart, no second call.
        restarts2 = []
        _, second = self.run_dog(restarts=restarts2)
        self.assertEqual(self.keys(second), ["heartbeat"])
        self.assertIn("restarted once", second["problems"][0]["detail"])
        self.assertEqual(restarts2, [])

    def test_a_new_incident_restarts_again(self):
        self.board.heartbeat(age_s=300)
        restarts = []
        self.run_dog(restarts=restarts)
        self.board.heartbeat(age_s=500)  # a different mtime: a new incident
        self.run_dog(restarts=restarts)
        self.assertEqual(len(restarts), 2)

    def test_absent_heartbeat_is_its_own_incident(self):
        (self.board.root / "dispatch-heartbeat").unlink()
        restarts = []
        _, status = self.run_dog(restarts=restarts)
        self.assertEqual(restarts, [["systemctl", "--user", "restart", "seat-dispatch.service"]])
        self.assertEqual(self.keys(status), [], "exit 0 restart: no report this run")
        state = json.loads((self.board.root / "watchdog-state.json").read_text())
        self.assertEqual(state["incident"], "absent")
        _, second = self.run_dog(restarts=restarts)
        self.assertEqual(self.keys(second), ["heartbeat"])
        self.assertEqual(len(restarts), 1, "no second restart for the same incident")

    def test_a_failed_restart_is_reported_at_once(self):
        self.board.heartbeat(age_s=300)
        restarts = []
        _, status = self.run_dog(restarts=restarts, exit_code=1)
        self.assertEqual(self.keys(status), ["heartbeat"])
        self.assertIn("exited nonzero", status["problems"][0]["detail"])

    def test_a_fresh_heartbeat_never_restarts(self):
        restarts = []
        self.run_dog(restarts=restarts)
        self.assertEqual(restarts, [])
        self.assertFalse((self.board.root / "watchdog-state.json").exists())

    # -- output shape ----------------------------------------------------------

    def test_status_shape_and_order(self):
        self.board.heartbeat(age_s=300)  # heartbeat (already restarted? no: patched, run 1)
        (self.board.root / "hapi-sessions.json").write_text("{ nope")
        hooked = self.board.seat("zhook", webhook=True)
        write_msg(hooked / "inbox", ts_name(20 * 60))
        ahooked = self.board.seat("ahook", webhook=True)
        write_msg(ahooked / "inbox", ts_name(20 * 60))
        write_msg(ahooked / "quarantine", ts_name(60, "deadbeef"))
        _, status = self.run_dog()
        self.assertEqual(self.keys(status),
                         ["bindings", "overdue:ahook", "overdue:zhook", "quarantine:ahook"])
        self.assertEqual(status["ok"], False)
        self.assertIn("T", status["checked_at"])
        self.assertTrue(status["checked_at"].endswith("Z"))
        for problem in status["problems"]:
            self.assertEqual(problem["kind"], problem["key"].split(":")[0])
            if ":" not in problem["key"]:
                self.assertIsNone(problem["seat"])

    def test_the_status_file_is_0600_and_json(self):
        _, status = self.run_dog()
        path = self.board.root / "watchdog-status.json"
        self.assertEqual(path.stat().st_mode & 0o777, 0o600)
        self.assertEqual(status["ok"], True)
        self.assertEqual(status["problems"], [])
        leftovers = [p.name for p in self.board.root.iterdir() if ".tmp" in p.name
                     or p.name.startswith(".watchdog")]
        self.assertEqual(leftovers, [])

    def test_one_stdout_line_even_with_problems(self):
        import contextlib
        import io
        hooked = self.board.seat("hook", webhook=True)
        write_msg(hooked / "inbox", ts_name(20 * 60))
        buffer = io.StringIO()
        with patch.object(DOG, "restart_dispatcher", return_value=0):
            with contextlib.redirect_stdout(buffer):
                code = DOG.main(["--root", str(self.board.root)])
        self.assertEqual(code, 0)
        self.assertEqual(buffer.getvalue().splitlines(),
                         ["seat mail watchdog pass ok: 1 problems"])

    def test_the_line_names_a_restart(self):
        import contextlib
        import io
        self.board.heartbeat(age_s=300)
        buffer = io.StringIO()
        with patch.object(DOG, "restart_dispatcher", return_value=0):
            with contextlib.redirect_stdout(buffer):
                DOG.main(["--root", str(self.board.root)])
        self.assertEqual(buffer.getvalue().splitlines(),
                         ["seat mail watchdog pass ok: 0 problems;"
                          " restarted seat-dispatch.service"])

    def test_no_marker_is_one_error_line_and_exit_1(self):
        import contextlib
        import io
        (self.board.root / "marker").unlink()
        buffer = io.StringIO()
        with contextlib.redirect_stdout(buffer):
            code = DOG.main(["--root", str(self.board.root)])
        self.assertEqual(code, 1)
        self.assertEqual(len(buffer.getvalue().splitlines()), 1)
        self.assertIn("Error:", buffer.getvalue())


if __name__ == "__main__":
    unittest.main()
