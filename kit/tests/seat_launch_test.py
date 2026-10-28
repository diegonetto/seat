"""seat-hub-wait readiness controls and seat-sit's supervised launch against a real tmux.

The hub wait points only at ports the test owns (a live listener and a port
nothing listens on); seat-sit runs inside a real pane of a private tmux server
under a temp root, with its own SEAT_RUNTIMES tree and a stub harness — no
host seat, no host port, no network.
"""
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tempfile
import textwrap
import time
import unittest


ROOT = Path(__file__).resolve().parents[1]
WAIT = ROOT / "scripts" / "seat-hub-wait"
SIT = ROOT / "scripts" / "seat-sit"
REAL_TMUX = shutil.which("tmux")


def write_exec(path: Path, body: str) -> None:
    path.write_text(textwrap.dedent(body))
    path.chmod(path.stat().st_mode | stat.S_IEXEC)


def free_port() -> int:
    """A loopback port nothing listens on (bound to be chosen, then released)."""
    import socket
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


class HubWaitTest(unittest.TestCase):
    def run_wait(self, env):
        return subprocess.run([str(WAIT)], check=False, capture_output=True,
                              text=True, env=env, timeout=30)

    def test_open_port_returns_at_once(self):
        import socket
        with socket.socket() as listener:
            listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            listener.bind(("127.0.0.1", 0))
            listener.listen(1)
            env = dict(os.environ, SEAT_HUB_PORT=str(listener.getsockname()[1]))
            started = time.monotonic()
            done = self.run_wait(env)
            elapsed = time.monotonic() - started
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stderr, "")
        self.assertLess(elapsed, 1.0)

    def test_closed_port_expires_at_its_ceiling_and_says_so(self):
        env = dict(os.environ, SEAT_HUB_PORT=str(free_port()), SEAT_HUB_WAIT_TIMEOUT="1")
        started = time.monotonic()
        done = self.run_wait(env)
        elapsed = time.monotonic() - started
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("did not answer", done.stderr)
        self.assertIn("1 s", done.stderr)
        self.assertGreaterEqual(elapsed, 1.0)
        self.assertLess(elapsed, 10.0)


@unittest.skipUnless(REAL_TMUX, "real tmux is required to run seat-sit in a pane")
class SeatSitTest(unittest.TestCase):
    SEAT = "dying-seat"

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.runtimes = self.root / "runtimes"
        self.bin = self.root / "bin"
        self.runtimes.mkdir()
        self.bin.mkdir()
        self.runtime = self.runtimes / self.SEAT
        self.runtime.mkdir()
        self.sock = str(self.root / "tmux.sock")
        self.tmux = [REAL_TMUX, "-S", self.sock, "-f", "/dev/null"]
        self.addCleanup(subprocess.run, [REAL_TMUX, "-S", self.sock, "kill-server"],
                        capture_output=True)

    def harness(self, argv, cwd, kind="codex"):
        (self.runtime / "harness.json").write_text(json.dumps({
            "harness": kind, "argv": argv, "cwd": cwd, "expect_cmds": ["hapi"],
        }))

    def sit(self, extra_env=""):
        command = (f"env SEAT_RUNTIMES={self.runtimes} {extra_env} "
                   f"{SIT} {self.SEAT}")
        done = subprocess.run(self.tmux + ["new-session", "-d", "-s", "swarm", "-P", "-F",
                                           "#{pane_id} #{pane_pid}", command],
                              check=True, capture_output=True, text=True)
        pane, pid = done.stdout.split()
        self.addCleanup(subprocess.run, self.tmux + ["kill-session", "-t", "swarm"],
                        capture_output=True)
        return pane, int(pid)

    def pane_alive(self, pane):
        done = subprocess.run(self.tmux + ["list-panes", "-a", "-F", "#{pane_id}"],
                              capture_output=True, text=True)
        return done.returncode == 0 and pane in done.stdout.split()

    def wait_for(self, predicate, timeout=60):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if predicate():
                return True
            time.sleep(0.1)
        return False

    def test_harness_dying_at_launch_leaves_the_trace(self):
        stub = self.bin / "die-at-launch"
        write_exec(stub, """\
            #!/bin/sh
            echo "stub: hub unreachable, giving up"
            echo "stub: last words"
            exit 1
            """)
        self.harness([str(stub)], str(self.root))
        pane, pid = self.sit()
        trace = self.runtime / "harness-exit.log"
        self.assertTrue(self.wait_for(trace.exists), "harness-exit.log never appeared")
        self.assertTrue(self.wait_for(lambda: not self.pane_alive(pane)),
                        "the pane never closed after the harness died")
        body = trace.read_text()
        self.assertIn("seat: " + self.SEAT, body)
        self.assertIn("argv: " + str(stub), body)
        self.assertIn("exit: 1", body)
        self.assertIn("stub: hub unreachable, giving up", body)
        self.assertIn("stub: last words", body)
        target = json.loads((self.runtime / "wake-target.json").read_text())
        self.assertEqual(target["pane_pid"], pid)
        self.assertEqual((self.runtime / "pane.pid").read_text().strip(), str(pid))

    def test_wrapped_harness_dying_on_a_closed_hub_port_is_retried(self):
        runs = self.root / "runs.log"
        stub = self.bin / "hapi"
        write_exec(stub, """\
            #!/bin/sh
            echo run >>"$RUNS_LOG"
            echo "hapi: cannot reach the hub"
            exit 3
            """)
        self.harness([str(stub)], str(self.root))
        pane, _ = self.sit(extra_env=(
            f"RUNS_LOG={runs} SEAT_HUB_PORT={free_port()} "
            "SEAT_HUB_WAIT_TIMEOUT=1 SEAT_RETRY_GAP=0.5"))
        trace = self.runtime / "harness-exit.log"
        self.assertTrue(self.wait_for(trace.exists), "harness-exit.log never appeared")
        self.assertTrue(self.wait_for(lambda: not self.pane_alive(pane)),
                        "the pane never closed after the retries ran out")
        self.assertEqual(runs.read_text(), "run\n" * 4)  # one launch, three retries
        body = trace.read_text()
        self.assertIn("exit: 3", body)
        self.assertIn("hapi: cannot reach the hub", body)


if __name__ == "__main__":
    unittest.main()
