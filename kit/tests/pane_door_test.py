"""pane-door argv, map refusal, Shift+Enter bytes, and the root grid. No live seat, no network."""

import importlib.machinery
import importlib.util
import contextlib
import io
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import tempfile
import textwrap
import unittest

import yaml


ROOT = Path(__file__).resolve().parents[1]
_LOADER = importlib.machinery.SourceFileLoader("pane_door", str(ROOT / "scripts" / "pane-door"))
_SPEC = importlib.util.spec_from_loader("pane_door", _LOADER)
DOOR = importlib.util.module_from_spec(_SPEC)
_LOADER.exec_module(DOOR)


def write_exec(path: Path, body: str) -> None:
    path.write_text(textwrap.dedent(body))
    path.chmod(path.stat().st_mode | stat.S_IEXEC)


class PaneDoorTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.state = self.root / "state"
        self.runtimes = self.root / "runtimes"
        self.bin = self.root / "bin"
        self.state.mkdir()
        self.runtimes.mkdir()
        self.bin.mkdir()
        self.argv_log = self.root / "argv.jsonl"
        write_exec(self.bin / "ttyd", """\
            #!/usr/bin/env python3
            import json, os, sys, time
            with open(os.environ["STUB_LOG"], "a") as handle:
                handle.write(json.dumps(sys.argv) + chr(10))
            time.sleep(60)
        """)
        write_exec(self.bin / "sb", """\
            #!/bin/sh
            if [ "$1" = "seats" ] && [ "$2" = "--names" ]; then
              printf '%s\\n' glm-main grok-main
              exit 0
            fi
            echo unexpected >&2
            exit 3
        """)
        write_exec(self.bin / "tmux", """\
            #!/bin/sh
            printf '%s\\n' '%7'
            exit 0
        """)
        self.env = os.environ.copy()
        self.env.update({
            "PANE_DOOR_STATE": str(self.state),
            "PANE_DOOR_RUNTIMES": str(self.runtimes),
            "PANE_DOOR_SB": str(self.bin / "sb"),
            "PANE_DOOR_TMUX": str(self.bin / "tmux"),
            "PANE_DOOR_TTYD": str(self.bin / "ttyd"),
            "STUB_LOG": str(self.argv_log),
        })
        self.target("glm-main", "claude")
        self.target("grok-main", "grok")

    def target(self, seat, harness, pane="%7", socket="/tmp/tmux-test/default"):
        runtime = self.runtimes / seat
        runtime.mkdir(exist_ok=True)
        (runtime / "wake-target.json").write_text(json.dumps({
            "seat": seat,
            "socket": socket,
            "pane": pane,
            "harness": harness,
        }))

    def run_door(self, *args):
        return subprocess.run(
            [str(ROOT / "scripts" / "pane-door"), *args],
            check=False, capture_output=True, text=True, env=self.env,
        )

    def test_newline_bytes(self):
        for harness in ("claude", "codex", "agy", "opencode", "glm"):
            self.assertEqual(DOOR.newline_for(harness)[0], (10,))
        self.assertEqual(DOOR.newline_for("grok"), ((27, 13), "alt-enter"))
        self.assertEqual(DOOR.newline_for("hermes")[0], (10,))

    def test_ttyd_argv_shape(self):
        argv = DOOR.ttyd_argv(
            Path("/usr/local/bin/ttyd"), 7704, "grok-main",
            Path("/var/page.html"), "/tmp/tmux-1/default", "%12", "/grok-main",
        )
        split = argv.index("--")
        flags = argv[:split]
        self.assertEqual(flags[:3], ["/usr/local/bin/ttyd", "-i", "127.0.0.1"])
        for flag in ("-W", "-O", "-m", "-P", "-I"):
            self.assertIn(flag, flags)
        self.assertEqual(flags[flags.index("-m") + 1], "1")
        self.assertEqual(flags[flags.index("-P") + 1], "5")
        self.assertEqual(flags[flags.index("-b") + 1], "/grok-main")
        for banned in ("-a", "-c", "-S"):
            self.assertNotIn(banned, flags)
        self.assertEqual(argv[split + 1:split + 3], ["sh", "-c"])
        script = argv[split + 3]
        self.assertEqual(script, "exec tmux -S /tmp/tmux-1/default attach -t door-grok-main")
        for gone in ("sleep", "resize-pane", "if-shell", ";"):
            self.assertNotIn(gone, script)
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            DOOR.attach_script("tmux", "/tmp/tmux-1/default", "grok-main", "swarm:gallery")

    def test_off_map_starts_nothing(self):
        result = self.run_door("up", "not-a-seat")
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("not on the seat map", result.stderr)
        self.assertFalse(self.argv_log.exists())

    def test_up_records_argv_and_down_stops_it(self):
        result = self.run_door("up")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("http://127.0.0.1:7701/glm-main/", result.stdout)
        self.assertIn("shift+enter=ctrl-j", result.stdout)
        self.assertIn("http://127.0.0.1:7702/grok-main/", result.stdout)
        self.assertIn("shift+enter=alt-enter", result.stdout)
        self.assertIn("root  http://127.0.0.1:7700/  grid", result.stdout)
        recorded = [json.loads(line) for line in self.argv_log.read_text().splitlines()]
        self.assertEqual(len(recorded), 2)
        for argv in recorded:
            split = argv.index("--")
            flags = argv[:split]
            self.assertIn("-i", flags)
            self.assertEqual(flags[flags.index("-i") + 1], "127.0.0.1")
            self.assertNotIn("-a", flags)
            self.assertNotIn("-c", flags)
            self.assertNotIn("-S", flags)
        grok_page = (self.state / "pages" / "grok-main.html").read_text()
        self.assertIn("new Uint8Array([27, 13])", grok_page)
        self.assertIn('ev.key !== "Enter"', grok_page)
        glm_page = (self.state / "pages" / "glm-main.html").read_text()
        self.assertIn("new Uint8Array([10])", glm_page)
        self.assertNotIn("__NEWLINE__", grok_page)
        again = self.run_door("up")
        self.assertEqual(again.returncode, 0, again.stderr)
        recorded_again = [json.loads(line) for line in self.argv_log.read_text().splitlines()]
        self.assertEqual(len(recorded_again), 2)
        down = self.run_door("down")
        self.assertEqual(down.returncode, 0, down.stderr)
        self.assertIn("glm-main  stopped", down.stdout)
        self.assertIn("grok-main  stopped", down.stdout)
        self.assertEqual(json.loads((self.state / "pids.json").read_text()), {})

    def test_missing_pane_is_skipped(self):
        self.target("grok-main", "grok", pane="%9")
        result = self.run_door("up", "grok-main")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("pane is not in tmux", result.stderr)
        self.assertFalse(self.argv_log.exists())

    def test_root_grid_page_is_one_iframe_per_seat(self):
        result = self.run_door("up")
        self.assertEqual(result.returncode, 0, result.stderr)
        page = (self.state / "pages" / "root.html").read_text()
        self.assertIn('<iframe src="/glm-main/" title="glm-main"></iframe>', page)
        self.assertIn('<iframe src="/grok-main/" title="grok-main"></iframe>', page)
        self.assertLess(page.index('src="/glm-main/"'), page.index('src="/grok-main/"'))
        self.assertNotIn("figcaption", page)
        self.assertNotIn("<figure", page)
        self.assertIn("overflow:hidden", page)
        self.assertIn("minmax(0,1fr)", page)
        self.assertIn('<button type="button" aria-label="Expand glm-main">', page)
        self.assertIn('<button type="button" aria-label="Expand grok-main">', page)
        self.assertIn('<path d="M16 4l4 0l0 4"/>', page)
        self.assertIn('<path d="M5 9l4 0l0 -4"/>', page)
        self.assertIn("border:0", page)
        self.assertNotIn("\u2922", page)
        self.assertIn(".tile.full{position:fixed;inset:0;z-index:10;}", page)
        self.assertEqual(page.count("<script"), 1)
        self.assertNotIn("<script src", page)
        self.assertNotIn("Escape", page)
        self.assertNotIn("keydown", page)
        self.assertNotIn("drag", page)
        # The iframe stays where it was: no node moves, no src rewrite.
        for moved in ("appendChild", "insertBefore", "replaceChild", "append(", "prepend(", ".src", "removeChild"):
            self.assertNotIn(moved, page)
        self.assertIn("contentWindow.focus()", page)
        self.assertNotIn("<link", page)
        self.assertNotIn("WebSocket", page)
        self.assertNotIn("http://", page)
        self.assertNotIn("https://", page)
        self.assertIsNone(re.search(r"\byou(?:r)?\b", page))
        self.assertNotIn("\u2014", page)
        self.assertIn('src="/a.b/"', DOOR.build_grid(["a.b"]))
        self.assertFalse((self.state / "pages" / "terminals.html").exists())
        pids = json.loads((self.state / "pids.json").read_text())
        self.assertNotIn("terminals", pids)
        self.assertNotIn("root", pids)

    def test_root_spans_seats_on_different_sockets(self):
        self.target("grok-main", "grok", socket="/tmp/tmux-other/default")
        result = self.run_door("up")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("root  http://127.0.0.1:7700/  grid", result.stdout)
        recorded = [json.loads(line) for line in self.argv_log.read_text().splitlines()]
        bases = sorted(a[a.index("-b") + 1] for a in recorded)
        self.assertEqual(bases, ["/glm-main", "/grok-main"])
        page = (self.state / "pages" / "root.html").read_text()
        self.assertIn('src="/glm-main/"', page)
        self.assertIn('src="/grok-main/"', page)
        pids = json.loads((self.state / "pids.json").read_text())
        self.assertNotIn("terminals", pids)

    def test_named_up_leaves_the_root_alone(self):
        first = self.run_door("up")
        self.assertEqual(first.returncode, 0, first.stderr)
        self.argv_log.unlink()
        result = self.run_door("up", "glm-main")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("root", result.stdout)
        self.assertNotIn("grok-main", result.stdout)
        self.assertFalse(self.argv_log.exists())
        pids = json.loads((self.state / "pids.json").read_text())
        self.assertIn("glm-main", pids)
        self.assertIn("grok-main", pids)

    def test_generated_xterm_stays_valid_javascript(self):
        vendor = DOOR.require_vendor()
        page = DOOR.build_page("glm-main", (10,), "ctrl-j", vendor)
        start = page.find("<script>\n") + len("<script>\n")
        script = page[start:page.find("</script>")]
        path = self.root / "xterm-check.js"
        path.write_text(script)
        result = subprocess.run(["node", "--check", str(path)], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr[-400:])

    def test_seat_page_gives_the_scrollbar_width_back_to_the_terminal(self):
        vendor = DOOR.require_vendor()
        page = DOOR.build_page("glm-main", (10,), "ctrl-j", vendor)
        self.assertIn(".xterm .xterm-viewport{overflow-y:hidden;scrollbar-width:none}", page)
        self.assertIn("#term{position:absolute;inset:0;overflow:hidden;}", page)
        self.assertIn("0.05", page)
        body = page[page.find("function fitTerm"):page.find("term.open(")]
        cleared = body.find("scrollBarWidth = 0")
        fitted = body.find("fit.fit()")
        stretched = body.find("scaleX(")
        self.assertGreaterEqual(cleared, 0)
        self.assertLess(cleared, fitted)
        self.assertLess(fitted, stretched)
        self.assertGreater(page.find("fitTerm();", page.find("term.open(")), page.find("term.open("))

    def test_resize_waits_until_the_window_settles(self):
        vendor = DOOR.require_vendor()
        page = DOOR.build_page("glm-main", (10,), "ctrl-j", vendor)
        self.assertIn("const RESIZE_WAIT = 100;", page)
        listener = page[page.find('window.addEventListener("resize"'):]
        timeout_at = listener.find("window.setTimeout")
        self.assertGreater(listener.find("window.clearTimeout"), -1)
        self.assertLess(listener.find("window.clearTimeout"), timeout_at)
        self.assertGreater(listener.find("fitTerm();", timeout_at), timeout_at)
        self.assertGreater(listener.find("sendSize(", timeout_at), timeout_at)
        self.assertLess(page.find("\nfitTerm();\n"), page.find('window.addEventListener("resize"'))

    def test_grid_script_stays_valid_javascript(self):
        page = DOOR.build_grid(["glm-main", "grok-main"])
        start = page.find("<script>\n") + len("<script>\n")
        path = self.root / "grid-check.js"
        path.write_text(page[start:page.find("</script>")])
        result = subprocess.run(["node", "--check", str(path)], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr[-400:])

    def test_ingress_prints_and_applies_nothing(self):
        write_exec(self.bin / "sb", """\
            #!/bin/sh
            if [ "$1" = "seats" ] && [ "$2" = "--names" ]; then
              printf '%s\\n' glm-main a.b grok-main
              exit 0
            fi
            echo unexpected >&2
            exit 3
            """)
        result = self.run_door("ingress")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("not applied", result.stdout)
        self.assertNotIn("pane.bll.dev", result.stdout)
        doc = yaml.safe_load(result.stdout)
        rules = doc["ingress"]
        seats = ["glm-main", "a.b", "grok-main"]
        seat_rules, tail, fallback = rules[:len(seats)], rules[len(seats):-1], rules[-1]
        self.assertEqual(len(seat_rules), len(seats))
        for rule, seat in zip(seat_rules, seats):
            self.assertEqual(rule["hostname"], "tmux.bll.dev")
            self.assertEqual(rule["service"], f"http://127.0.0.1:{7701 + seats.index(seat)}")
            pattern = re.compile(rule["path"])
            self.assertIsNotNone(pattern.search(f"/{seat}/"))
            self.assertIsNotNone(pattern.search(f"/{seat}/ws"))
            self.assertIsNone(pattern.search(f"/{seat}x/"))
        self.assertEqual(seat_rules[seats.index("a.b")]["path"], "^/a\\.b($|/)")
        self.assertIsNone(re.compile(seat_rules[seats.index("a.b")]["path"]).search("/axb/"))
        self.assertEqual(tail, [
            {"hostname": "tmux.bll.dev", "path": "^/$", "service": "http://127.0.0.1:7700"},
        ])
        self.assertEqual(fallback, {"service": "http_status:404"})
        self.assertNotIn("^/terminals", result.stdout)
        self.assertNotIn("7707", result.stdout)
        self.assertNotIn("^/token$", result.stdout)
        self.assertNotIn("^/ws$", result.stdout)
        self.assertFalse(self.argv_log.exists())


if __name__ == "__main__":
    unittest.main()


class DoorSessionTest(unittest.TestCase):
    """ensure_door_session against a scripted tmux: the fake answers display/has-session from a
    small table and records every mutating call, so group selection and recreation are checked."""

    def run_with(self, table):
        import tempfile, textwrap, json as _json
        root = Path(tempfile.mkdtemp())
        log = root / "calls.jsonl"
        fake = root / "tmux"
        fake.write_text(textwrap.dedent(f"""\
            #!/usr/bin/env python3
            import json, sys
            table = {table!r}
            args = sys.argv[1:]
            with open({str(log)!r}, "a") as f: f.write(json.dumps(args) + "\\n")
            if "display" in args:
                key = (args[args.index("-t") + 1], args[-1]); print(table.get(key, "")); sys.exit(0 if key in table else 1)
            if "has-session" in args:
                sys.exit(0 if args[-1] in table.get("sessions", ()) else 1)
            if "select-window" in args:
                sys.exit(0 if args[-1].split(":")[-1] in table.get("windows", ()) else 1)
            sys.exit(0)
            """))
        fake.chmod(0o755)
        os.environ["PANE_DOOR_TMUX"] = str(fake)
        try:
            DOOR.ensure_door_session("/tmp/tmux-1/default", "grok-main", "%8")
        finally:
            os.environ.pop("PANE_DOOR_TMUX", None)
        return [_json.loads(l) for l in log.read_text().splitlines()]

    def test_new_door_joins_the_panes_group_and_selects_its_window(self):
        calls = self.run_with({("%8", "#{session_group}"): "swarm", ("%8", "#{session_name}"): "swarm",
                               ("%8", "#{window_id}"): "@3", "windows": ("@3",)})
        kinds = [c[2] for c in calls]
        self.assertIn("new-session", kinds)
        self.assertNotIn("kill-session", kinds)
        self.assertEqual(calls[-1][2:], ["select-window", "-t", "door-grok-main:@3"])

    def test_door_in_another_group_is_recreated(self):
        calls = self.run_with({("%8", "#{session_group}"): "swarm", ("%8", "#{session_name}"): "swarm",
                               ("%8", "#{window_id}"): "@3", ("=door-grok-main", "#{session_group}"): "other",
                               "sessions": ("=door-grok-main",), "windows": ("@3",)})
        kinds = [c[2] for c in calls]
        self.assertIn("kill-session", kinds)
        self.assertIn("new-session", kinds)

    def test_ungrouped_pane_recreates_a_surviving_door(self):
        calls = self.run_with({("%8", "#{session_group}"): "", ("%8", "#{session_name}"): "swarm",
                               ("%8", "#{window_id}"): "@3", ("=door-grok-main", "#{session_group}"): "stale",
                               "sessions": ("=door-grok-main",), "windows": ("@3",)})
        kinds = [c[2] for c in calls]
        self.assertIn("kill-session", kinds)
        self.assertIn("new-session", kinds)

    def test_failed_window_select_fails_the_up(self):
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            self.run_with({("%8", "#{session_group}"): "swarm", ("%8", "#{session_name}"): "swarm",
                           ("%8", "#{window_id}"): "@3", "windows": ()})
