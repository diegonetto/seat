"""Behavior tests with stub tmux, seat, quota, process tree and HAPI transport.

No localhost listener or host command is used. A private PATH contains only stubs;
each rejects unknown operations, so a missing stub cannot reach a real seat. The
down-path gate execs the real tmux and ps, but only on a private socket under a
temp TMUX_TMPDIR; every tmux argv is checked against the operator's socket.
"""
import contextlib
import shutil
import fcntl
import importlib.util
import io
import json
import os
from pathlib import Path
import secrets
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch


sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("seat_role", ROOT / "scripts/seat-role.py")
ROLE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ROLE)
SESSION = "11111111-2222-4333-8444-555555555555"
OTHER = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"
EXPECTED_URL = "https://hapi.bll.dev/sessions/" + SESSION
# The kernel's seat map (one seat list), as `sb seats --json` prints it.
MAP = {"schema_version": 1, "seats": {
    "glm-main": {"tenant": "internal", "account": "zai-personal", "model": "glm-5.3", "role": "builder",
                 "name": "GLM", "harness": "glm", "effort": None, "cwd": "/home/slix/Projects"},
    "opus-main": {"tenant": "internal", "account": "claude-personal", "model": "claude-opus-5-5", "role": "reviewer",
                  "name": "Claude", "harness": "claude", "effort": "xhigh", "cwd": "/home/slix/Projects"},
    "codex-main": {"tenant": "internal", "account": "codex-personal", "model": "gpt-6.1-sol", "role": "lander",
                   "name": "GPT", "harness": "codex", "effort": "medium", "cwd": "/home/slix/Projects"},
    "grok-main": {"tenant": "internal", "account": "grok-personal", "model": "grok-4.7", "role": "fixer",
                  "name": "Grok", "harness": "grok", "effort": "high", "cwd": "/home/slix/Projects"},
    "gemini-main": {"tenant": "internal", "account": "gemini-personal", "model": "gemini-3.8-flash-high", "role": "critic",
                    "name": "Gemini", "harness": "agy", "effort": None, "cwd": "/home/slix/Projects"},
    "fable-main": {"tenant": "internal", "account": "claude-personal", "model": "claude-fable-5-1", "role": "adjudicator",
                   "name": "Flight", "harness": "claude", "effort": "high", "cwd": "/home/slix/Projects"},
}}

REAL_TMUX = shutil.which('tmux')
REAL_PS = shutil.which('ps')
# Pane commands run with the stub-only PATH, so they name sleep absolutely.
REAL_SLEEP = shutil.which('sleep')
OPERATOR_TMUX = os.environ.get('TMUX', '').split(',')[0]
SENTINEL_TMUX = '/sentinel/operator-socket'

STUB = r'''
import json, os, pathlib, subprocess, sys, time
root = pathlib.Path(os.environ['STUB_ROOT'])
state = json.loads((root / 'stub.json').read_text())
name, args = pathlib.Path(sys.argv[0]).name, sys.argv[1:]
with (root / 'calls.jsonl').open('a') as log:
    log.write(json.dumps([name, *args]) + '\n')
def save():
    (root / 'stub.json').write_text(json.dumps(state))
def out(value):
    print(json.dumps(value))
def reject():
    sys.exit(91)
def sock():
    return str(root / 'tmux.sock')
def swarm_sock():
    # The swarm's default socket, only ever under the test's private TMUX_TMPDIR.
    return str(root / 'tmux-tmp' / ('tmux-' + str(os.getuid())) / 'default')
if name == 'sb':
    assert args in (['seats', '--json'], ['seats', '--names'])
    if state.get('sb_error'):
        print(state['sb_error'], file=sys.stderr)
        sys.exit(1)
    if args[1] == '--names':
        print('\n'.join(state['map']['seats']))
        sys.exit(0)
    out(state['map'])
elif name == 'seat-role':
    assert len(args) == 2 and args[0] == 'wake'
    value = state.get('wake_results', {}).get(args[1], {'status': 'awake', 'url': 'https://hapi.bll.dev/sessions/11111111-2222-4333-8444-555555555555'})
    out(value)
    sys.exit(1 if value['status'] == 'failed' else 0)
elif name == 'sb-quota':
    assert args == ['status', '--all', '--json']
    if state.get('quota_error'):
        print(state['access'], file=sys.stderr)
        sys.exit(1)
    out({'rows': state['quota']})
elif name == 'ps':
    assert args == ['-eo', 'pid=,ppid=']
    if state.get('real'):
        os.execv(state['real_ps'], ['ps', *args])
    print('200 1\n201 200\n300 1\n301 300\n900 1')
elif name == 'curl':
    data = sys.stdin.read()
    assert '--disable' in args and '--fail' in args and '--max-time' in args
    assert state['access'] not in ' '.join(args) and state['jwt'] not in ' '.join(args)
    assert args[-1].startswith('http://127.0.0.1:3006/api/')
    if state.get('hub_error'):
        print(data, file=sys.stderr)
        sys.exit(7)
    if state.get('started') and state.get('hub_after_failures', 0) > 0:
        state['hub_after_failures'] -= 1
        save()
        sys.exit(7)
    path = args[-1].split('/api', 1)[1]
    if path == '/auth':
        assert json.loads(data) == {'accessToken': state['access']}
        if state.get('auth_failures', 0):
            state['auth_failures'] -= 1
            save()
            sys.exit(7)
        if state.get('bad_tokens', 0):
            state['bad_tokens'] -= 1
            save()
            out({'token': 'invalid token'})
            sys.exit(0)
        out({'token': state['jwt']})
    else:
        # curl config files repeat the header key once per sent header.
        config = {}
        for line in data.splitlines():
            if line:
                key, value = line.split(' = ', 1)
                config.setdefault(key, []).append(json.loads(value))
        patch = '-X' in args and 'PATCH' in args
        assert config['header'][0] == 'Authorization: Bearer ' + state['jwt']
        assert config['header'][1:] == (['Content-Type: application/json'] if patch else [])
        sessions = state['sessions_after'] if state.get('started') else state['sessions']
        if state.get('real') and state.get('started'):
            # The real HAPI host process is the pane's child.
            for session in sessions:
                session['metadata']['hostPid'] = int((root / 'child.pid').read_text())
        if path == '/sessions':
            if state.get('pending_sessions', 0):
                state['pending_sessions'] -= 1
                save()
                out({'sessions': []})
                sys.exit(0)
            # The real list endpoint strips hostPid; require a detail lookup.
            summaries = json.loads(json.dumps(sessions))
            for session in summaries:
                session['metadata'].pop('hostPid', None)
            out({'sessions': summaries})
        elif path.startswith('/sessions/'):
            sid = path.rsplit('/', 1)[1]
            found = [s for s in sessions if s['id'] == sid]
            if not found:
                reject()
            if patch:
                if state.get('patch_error'):
                    print(data, file=sys.stderr)
                    sys.exit(7)
                name = json.loads(config['data-binary'][0])['name']
                state.setdefault('renames', {})[sid] = name
                found[0]['metadata']['name'] = name
                save()
                out({'session': {'id': sid, 'metadata': {'name': name}}})
            else:
                out({'session': found[0]})
        else:
            reject()
elif name == 'tmux' and args[:2] == ['-S', swarm_sock()]:
    assert args[2:] == ['list-panes', '-a', '-F', '#{@seat}']
    if state.get('real'):
        os.execv(state['real_tmux'], ['tmux', *args])
    if state.get('swarm') is None:
        print('error connecting to ' + swarm_sock() + ' (No such file or directory)', file=sys.stderr)
        sys.exit(1)
    if state['swarm'] == 'error':
        print('open terminal failed: not a terminal', file=sys.stderr)
        sys.exit(1)
    print('\n'.join(state['swarm']))
elif name == 'tmux':
    assert args[:2] == ['-S', sock()]
    if state.get('real'):
        os.execv(state['real_tmux'], ['tmux', *args])
    verb, rest = args[2], args[3:]
    if verb == 'display-message':
        assert rest[:3] == ['-p', '-t', '%2' if state.get('started') else '%1']
        if state.get('pane_error'):
            sys.exit(1)
        if rest[-1] == '#{pane_title}':
            print(state.get('title', 'Ready'))
        else:
            assert rest[-1] == '#{pane_pid}|#{@seat}|#{pane_dead}|#{pane_current_command}'
            print(str(300 if state.get('started') else state.get('pid', 200)) + '|' +
                  state.get('stamp', state['seat']) + '|0|' + state.get('command', state['kind']))
    elif verb == 'capture-pane':
        state['captures'] = state.get('captures', 0) + 1
        save()
        if state.get('late_busy') and state['captures'] >= state['late_busy']:
            print('1 agent')
        else:
            print(state.get('pane', '>'))
    elif verb == 'send-keys':
        assert rest == ['-t', '%1', '/quit' if state['kind'] == 'codex' else '/exit', 'Enter']
        state['quit'] = True
        save()
    elif verb == 'list-panes':
        assert rest == ['-a', '-F', '#{pane_id}']
        if state.get('down') == 'server' and not state.get('started'):
            print('no server running on ' + sock(), file=sys.stderr)
            sys.exit(1)
        if state.get('down') == 'tmux_error':
            print('open terminal failed: not a terminal', file=sys.stderr)
            sys.exit(1)
        if state.get('down') == 'pane' and not state.get('started'):
            print('%7')
        elif not state.get('quit') or state.get('never_exits'):
            print('%1')
    else:
        reject()
elif name == 'seat':
    if state.get('gallery'):
        assert len(args) == 2 and args[0] == 'up' and args[1] in state['map']['seats']
        sys.exit(1 if args[1] in state.get('start_failures', []) else 0)
    assert args == ['up', state['seat']]
    assert os.getcwd() == str(root / 'home/Projects')
    assert (state.get('quit') or state.get('down')) and not state.get('started')
    runtime = root / 'home/Projects/.seat-runtimes' / state['seat']
    harness = json.loads((runtime / 'harness.json').read_text())
    if not state.get('plain_up'):
        assert harness['argv'][:2] == [str(root / 'home/.local/bin/hapi'), state['kind']]
        assert harness['cwd'] == str(root / 'home/Projects')
        assert 'session' not in harness
    saved = root / 'home/.local/share/seat/hapi-sessions.json'
    state['map_at_up'] = json.loads(saved.read_text()) if saved.exists() else None
    if state.get('start_error'):
        save()
        sys.exit(1)
    state['started'] = True
    save()
    target = {'seat': state['seat'], 'harness': state['kind'], 'pane': '%2', 'pane_pid': 300, 'socket': sock()}
    if state.get('real'):
        # Cold start: a new tmux server on the same private socket, one pane.
        tmux = [state['real_tmux'], '-S', sock(), '-f', '/dev/null']
        child = root / 'child.pid'
        pane, pid = subprocess.run(tmux + ['new-session', '-d', '-s', 'swarm', '-P', '-F', '#{pane_id} #{pane_pid}',
            state['real_sleep'] + ' 600 & echo $! > ' + str(child) + '; exec ' + state['real_sleep'] + ' 601'],
            check=True, capture_output=True, text=True).stdout.split()
        subprocess.run(tmux + ['set-option', '-p', '-t', pane, '@seat', state['seat']], check=True)
        while not child.exists() or not child.read_text().strip():
            time.sleep(0.05)
        target.update(pane=pane, pane_pid=int(pid))
    if not state.get('stale_target'):
        (runtime / 'wake-target.json').write_text(json.dumps(target))
else:
    reject()
'''


class Clock:
    def __init__(self):
        self.now = 0

    def monotonic(self):
        return self.now

    def sleep(self, seconds):
        self.now += seconds

    def time_ns(self):
        return 1000000000 + int(self.now * 1e9)


class RoleWakeTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="role-wake-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.home = self.root / 'home'
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        for name in ('tmux', 'seat', 'curl', 'sb-quota', 'ps', 'sb'):
            executable = self.bin / name
            executable.write_text('#!' + sys.executable + '\n' + STUB)
            executable.chmod(0o755)
        (self.bin / 'python3').symlink_to(sys.executable)
        (self.bin / 'dirname').symlink_to('/usr/bin/dirname')
        self.sb = self.root / 'switchboard'
        self.sb.mkdir()
        # seat_map is fetched once per process; every test starts cold against this test's stub.
        ROLE._SEAT_MAP = None
        self.memory = self.root / 'memory.csv'
        self.memory.write_text('timestamp,free_mb\n2026-10-14T02:30:00Z,3000\n')
        (self.home / '.hapi').mkdir(parents=True)
        self.clock = Clock()
        self.tmux_tmp = self.root / 'tmux-tmp'
        self.tmux_tmp.mkdir()
        self.env = patch.dict(os.environ, {
            'HOME': str(self.home), 'TMUX_TMPDIR': str(self.tmux_tmp), 'TMUX': SENTINEL_TMUX + ',1,0',
            'PATH': str(self.bin), 'SEAT_ROLE_HOME': str(self.home),
            'SEAT_ROLE_MEMORY_LOG': str(self.memory), 'SWITCHBOARD_HOME': str(self.sb),
            'SEAT_RUNTIMES': str(self.home / 'Projects/.seat-runtimes'), 'STUB_ROOT': str(self.root),
        })
        self.env.start()
        self.addCleanup(self.env.stop)
        self.fake_time = patch.object(ROLE, 'time', self.clock)
        self.fake_time.start()
        self.addCleanup(self.fake_time.stop)
        self.fixture()

    def session(self, sid=SESSION, pid=201, active=True, **metadata):
        return {'id': sid, 'active': active, 'metadata': {
            'machineId': 'test-machine', 'path': str(self.home / 'Projects'),
            'flavor': 'claude' if self.kind == 'glm' else self.kind, 'hostPid': pid, **metadata,
        }}

    def fixture(self, role='reviewer', wrapped=True):
        self.role = role
        self.seat = next(s for s, row in MAP['seats'].items() if row['role'] == role)
        row = MAP['seats'][self.seat]
        self.kind = row['harness']
        self.runtime = self.home / 'Projects/.seat-runtimes' / self.seat
        self.runtime.mkdir(parents=True, exist_ok=True)
        self.harness = self.runtime / 'harness.json'
        argv = [str(self.home / '.local/bin/hapi'), self.kind, '--fixture-flag'] if wrapped else ['/stub/' + self.kind, '--fixture-flag']
        self.harness.write_text(json.dumps({'harness': self.kind, 'argv': argv,
            'cwd': str(self.home / 'elsewhere'), 'expect_cmds': [self.kind, 'hapi'], 'session': 'old-native-session'}))
        (self.runtime / 'wake-target.json').write_text(json.dumps({
            'seat': self.seat, 'harness': self.kind, 'pane': '%1', 'pane_pid': 200,
            'socket': str(self.root / 'tmux.sock'),
        }))
        self.state = {'seat': self.seat, 'kind': self.kind, 'map': json.loads(json.dumps(MAP)),
            'access': secrets.token_hex(20), 'jwt': secrets.token_hex(20),
            'quota': [{'tenant': 'test', 'account': row['account'], 'verdict': 'OK'}],
            'sessions': [self.session()] if wrapped else [], 'sessions_after': [self.session(pid=301)]}
        (self.home / '.hapi/settings.json').write_text(json.dumps({
            'machineId': 'test-machine', 'cliApiToken': self.state['access']}))
        self.save()

    def save(self):
        (self.root / 'stub.json').write_text(json.dumps(self.state))

    def calls(self):
        path = self.root / 'calls.jsonl'
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def invoke(self, args=None, stderr=''):
        out, err = io.StringIO(), io.StringIO()
        before = {p.name: p.read_bytes() for p in self.sb.iterdir()}
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = ROLE.main(args if args is not None else ['wake', self.role])
        self.assertEqual(err.getvalue(), stderr)
        self.assertEqual(len(out.getvalue().splitlines()), 1)
        value = json.loads(out.getvalue())
        self.assertEqual(code, 1 if value['status'] == 'failed' else 0)
        visible = out.getvalue() + err.getvalue() + json.dumps(self.calls())
        for key in ('access', 'jwt'):
            self.assertNotIn(self.state[key], visible)
        # Isolation: no argv may name the operator's tmux or the real default socket;
        # the swarm probe may only read the private default under the temp TMUX_TMPDIR.
        uid = str(os.getuid())
        forbidden = [v for v in (OPERATOR_TMUX, SENTINEL_TMUX, '/tmp/tmux-' + uid) if v]
        for call in self.calls():
            if call[0] == 'tmux':
                self.assertIn(call[1:3], [['-S', str(self.root / 'tmux.sock')], ['-S', self.swarm_sock()]])
                if call[2] == self.swarm_sock():
                    self.assertEqual(call[3:], ['list-panes', '-a', '-F', '#{@seat}'])
            for arg in call:
                for bad in forbidden:
                    self.assertNotIn(bad, arg)
        self.assertEqual(before, {p.name: p.read_bytes() for p in self.sb.iterdir()})
        return value

    def swarm_sock(self):
        return str(self.tmux_tmp / ('tmux-' + str(os.getuid())) / 'default')

    def swarm_probes(self):
        return [c for c in self.calls() if c[0] == 'tmux' and c[2] == self.swarm_sock()]

    def no_mutation(self, expected):
        original = self.harness.read_bytes()
        value = self.invoke()
        self.assertEqual(value['status'], expected)
        self.assertEqual(self.harness.read_bytes(), original)
        self.assertFalse(list(self.runtime.glob('harness.json.pre-hapi.*')))
        self.assertFalse(any(c[0] == 'seat' or 'send-keys' in c for c in self.calls()))
        return value

    def test_awake_has_verified_url_and_no_restart(self):
        self.assertEqual(self.no_mutation('awake'), {'status': 'awake', 'url': EXPECTED_URL})
        saved = json.loads((self.home / '.local/share/seat/hapi-sessions.json').read_text())
        self.assertEqual(saved['seats'][self.seat]['url'], EXPECTED_URL)
        self.assertIn('moved_at', saved['seats'][self.seat])
        self.assertTrue(any('/sessions/' + SESSION in c[-1] for c in self.calls() if c[0] == 'curl'))

    def test_wrapped_session_returns_url_during_low_memory_hold_and_busy_work(self):
        self.memory.write_text('at,2999\n')
        self.state['quota'][0]['verdict'] = 'HOLD'
        self.state['pane'] = 'Working\n1 shell\nDo you want to continue? (y/n)'
        self.save()
        self.assertEqual(self.no_mutation('awake'), {'status': 'awake', 'url': EXPECTED_URL})
        self.assertFalse(any(c[0] == 'sb-quota' for c in self.calls()))

    def test_awake_patch_names_the_session_once(self):
        self.assertEqual(self.invoke(), {'status': 'awake', 'url': EXPECTED_URL})
        patches = [c for c in self.calls() if c[0] == 'curl' and 'PATCH' in c]
        self.assertEqual(len(patches), 1)
        joined = ' '.join(patches[0])
        self.assertIn('--config', patches[0])
        self.assertNotIn(self.state['jwt'], joined)
        name = 'Claude · opus-main'
        self.assertNotIn(name, joined)
        self.assertNotIn(json.dumps(name)[1:-1], joined)
        stub = json.loads((self.root / 'stub.json').read_text())
        self.assertEqual(stub['renames'][SESSION], name)
        self.assertEqual(self.invoke(), {'status': 'awake', 'url': EXPECTED_URL})
        self.assertEqual(len([c for c in self.calls() if c[0] == 'curl' and 'PATCH' in c]), 1)

    def test_unchanged_name_sends_no_patch(self):
        self.state['sessions'][0] = self.session(name='Claude · opus-main')
        self.save()
        self.assertEqual(self.invoke(), {'status': 'awake', 'url': EXPECTED_URL})
        curls = [c for c in self.calls() if c[0] == 'curl']
        self.assertFalse(any('PATCH' in c for c in curls))
        self.assertTrue(any(c[-1].endswith('/api/sessions/' + SESSION) and '-X' not in c for c in curls))

    def test_hand_set_name_is_never_overwritten(self):
        # Diego renamed this session himself; a wake must not fight him for it.
        self.state['sessions'][0] = self.session(name='My critic')
        self.save()
        self.assertEqual(self.invoke(), {'status': 'awake', 'url': EXPECTED_URL})
        self.assertFalse(any('PATCH' in c for c in self.calls() if c[0] == 'curl'))

    def test_old_role_name_is_replaced(self):
        # "Reviewer · opus-main" was this code's own name for the seat; it becomes "Claude · opus-main".
        self.state['sessions'][0] = self.session(name='Reviewer · opus-main')
        self.save()
        self.assertEqual(self.invoke(), {'status': 'awake', 'url': EXPECTED_URL})
        stub = json.loads((self.root / 'stub.json').read_text())
        self.assertEqual(stub['renames'][SESSION], 'Claude · opus-main')

    def test_folder_name_is_replaced(self):
        # HAPI falls back to the folder name ("Projects"); that is not a real name, so the seat names it.
        self.state['sessions'][0] = self.session(name='Projects')
        self.save()
        self.assertEqual(self.invoke(), {'status': 'awake', 'url': EXPECTED_URL})
        self.assertEqual(len([c for c in self.calls() if c[0] == 'curl' and 'PATCH' in c]), 1)

    def test_patch_failure_is_non_fatal(self):
        self.state['patch_error'] = True
        self.save()
        warning = f'seat-role: {self.seat} is linked but was not named: the phone hub (HAPI) returned an error\n'
        self.assertEqual(self.invoke(stderr=warning), {'status': 'awake', 'url': EXPECTED_URL})
        self.assertEqual(self.saved_map()['seats'][self.seat]['url'], EXPECTED_URL)
        self.assertTrue(any('PATCH' in c for c in self.calls() if c[0] == 'curl'))

    def test_running_wrapped_waits_for_late_session_without_restart(self):
        self.state['pending_sessions'] = 3
        self.save()
        self.assertEqual(self.no_mutation('awake')['url'], EXPECTED_URL)
        self.assertGreaterEqual(self.clock.now, 2)

    def test_running_wrapped_waits_for_hub_and_valid_token(self):
        for key in ('auth_failures', 'bad_tokens'):
            with self.subTest(key=key):
                self.fixture()
                self.state[key] = 2
                self.save()
                self.assertEqual(self.no_mutation('awake')['url'], EXPECTED_URL)

    def test_running_wrapped_reloads_target_after_it_is_rewritten(self):
        for missing in (False, True):
            with self.subTest(missing=missing):
                self.fixture()
                target = self.runtime / 'wake-target.json'
                original = target.read_text()
                self.state['pending_sessions'] = 1
                self.save()
                find = ROLE.Wake.find_session

                def remove_or_stale(wake, current, deadline):
                    value = find(wake, current, deadline)
                    if value is None:
                        if missing:
                            target.unlink()
                        else:
                            stale = json.loads(original)
                            stale['pane_pid'] = 999
                            target.write_text(json.dumps(stale))
                    return value

                sleep = self.clock.sleep

                def rewrite(seconds):
                    sleep(seconds)
                    target.write_text(original)

                with patch.object(ROLE.Wake, 'find_session', remove_or_stale), patch.object(self.clock, 'sleep', rewrite):
                    self.assertEqual(self.no_mutation('awake')['url'], EXPECTED_URL)

    def test_running_wrapped_no_session_preserves_reason_and_bound(self):
        self.state['sessions'] = []
        self.save()
        with patch.dict(os.environ, {'SEAT_ROLE_TEST_SESSION_WAIT': '3'}):
            value = self.no_mutation('failed')
        self.assertEqual(value['reason'], 'the seat is running under the phone link, but no phone session for it is active')
        self.assertEqual(self.clock.now, 3)

    def test_rename_unexpected_error_warns_without_leaking_details(self):
        original = ROLE.Wake.hub

        def broken_patch(wake, path, deadline, **kwargs):
            if kwargs.get('patch') is not None:
                raise RuntimeError(self.state['access'] + self.state['jwt'])
            return original(wake, path, deadline, **kwargs)

        warning = f'seat-role: {self.seat} is linked but was not named: an internal error\n'
        with patch.object(ROLE.Wake, 'hub', broken_patch):
            self.assertEqual(self.invoke(stderr=warning), {'status': 'awake', 'url': EXPECTED_URL})
        self.assertEqual(self.saved_map()['seats'][self.seat]['url'], EXPECTED_URL)

    def test_plain_idle_cutover_restart_and_fresh_session(self):
        self.fixture('fixer', wrapped=False)
        original = self.harness.read_bytes()
        self.assertEqual(self.invoke(), {'status': 'awake', 'url': EXPECTED_URL})
        changed = json.loads(self.harness.read_text())
        self.assertEqual(changed['argv'], [str(self.home / '.local/bin/hapi'), 'grok', '--fixture-flag'])
        self.assertEqual(changed['cwd'], str(self.home / 'Projects'))
        self.assertNotIn('session', changed)
        self.assertEqual(next(self.runtime.glob('harness.json.pre-hapi.*')).read_bytes(), original)
        self.assertEqual([c for c in self.calls() if c[0] == 'seat'], [['seat', 'up', 'grok-main']])
        self.assertGreaterEqual(self.clock.now, 10)

    def test_codex_keeps_real_binary_and_uses_quit(self):
        self.fixture('lander', wrapped=False)
        self.assertEqual(self.invoke()['status'], 'awake')
        self.assertEqual(json.loads(self.harness.read_text())['codex_bin'], '/stub/codex')
        self.assertTrue(any(c[-2:] == ['/quit', 'Enter'] for c in self.calls()))

    def test_plain_claude_cutover(self):
        self.fixture('reviewer', wrapped=False)
        self.assertEqual(self.invoke()['status'], 'awake')
        changed = json.loads(self.harness.read_text())
        self.assertEqual(changed['argv'][1], 'claude')
        self.assertTrue(any(c[-2:] == ['/exit', 'Enter'] for c in self.calls()))

    def test_busy_one_shell_never_sends(self):
        self.fixture('fixer', wrapped=False)
        self.state['pane'] = 'Ready\n1 shell\n>'
        self.save()
        self.assertEqual(self.no_mutation('busy'), {'status': 'busy'})

    def test_busy_markers(self):
        self.fixture('fixer', wrapped=False)
        for marker in ('esc to interrupt', 'Thinking', '2 shells', '1 agent', '3 agents', '⠙ Working',
                       'Do you want to continue?', '❯ 1. Yes', '(y/n)', 'Esc to cancel'):
            with self.subTest(marker=marker):
                self.state['title'] = marker
                self.save()
                self.no_mutation('busy')

    def test_busy_during_idle_observation(self):
        self.fixture('fixer', wrapped=False)
        self.state['late_busy'] = 3
        self.save()
        self.no_mutation('busy')

    def test_busy_after_cutover_rolls_back_without_sending(self):
        self.fixture('fixer', wrapped=False)
        original = self.harness.read_bytes()
        self.state['late_busy'] = 8
        self.save()
        self.assertEqual(self.invoke()['status'], 'busy')
        self.assertEqual(json.loads(self.harness.read_text()), json.loads(original))
        self.assertFalse(any('send-keys' in c or c[0] == 'seat' for c in self.calls()))

    def test_low_memory(self):
        self.fixture('fixer', wrapped=False)
        self.memory.write_text('at,2999\n')
        self.assertIn('2999', self.no_mutation('refused')['reason'])
        self.assertEqual(self.calls(), [['sb', 'seats', '--json']])

    def test_memory_unavailable(self):
        self.fixture('fixer', wrapped=False)
        self.memory.write_text('at,unknown\n')
        self.no_mutation('failed')

    def test_unknown_role_and_verb(self):
        for args in (['sleep', 'reviewer'], []):
            with self.subTest(args=args):
                self.assertEqual(self.invoke(args)['status'], 'failed')
                self.assertEqual(self.calls(), [])
        (self.root / 'calls.jsonl').unlink(missing_ok=True)
        for args in (['wake', 'admin'], ['wake', '../reviewer']):
            with self.subTest(args=args):
                self.assertEqual(self.invoke(args)['status'], 'failed')
        self.assertEqual(self.calls(), [['sb', 'seats', '--json']])

    def test_six_seat_map_resolves_every_role(self):
        for seat, row in MAP['seats'].items():
            wake = ROLE.Wake(row['role'])
            self.assertEqual((wake.seat, wake.kind, wake.label, wake.account),
                             (seat, row['harness'], row['name'], row['account']))

    def test_wake_loop_order_is_the_maps_file_order(self):
        self.assertEqual(ROLE.map_roles(), ['builder', 'reviewer', 'lander', 'fixer', 'critic', 'adjudicator'])
        self.assertEqual(len([c for c in self.calls() if c[0] == 'sb']), 1)

    def test_role_missing_from_the_map_fails_with_its_words(self):
        value = self.invoke(['wake', 'admin'])
        self.assertEqual(value['reason'],
                         'role admin is not in the seat map; use builder, reviewer, lander, fixer, critic, adjudicator')

    def test_refusing_map_fails_with_the_kernel_words(self):
        self.state['sb_error'] = 'seats.json: unknown key tenantx in seat glm-main'
        self.save()
        value = self.invoke()
        self.assertEqual(value['reason'], 'the seat map: seats.json: unknown key tenantx in seat glm-main')
        self.assertEqual(self.calls(), [['sb', 'seats', '--json']])

    def test_invalid_map_record_for_a_role(self):
        self.state['map']['seats']['opus-main']['account'] = 'not valid!'
        self.save()
        value = self.invoke()
        self.assertEqual(value['reason'], 'the seat map record for role reviewer is invalid')

    def test_gallery_script_refusal_prints_the_kernel_words(self):
        self.state['sb_error'] = 'seats.json: seat glm-main has no account'
        self.save()
        script = subprocess.run(['/bin/bash', str(ROOT / 'scripts/seat-gallery-up')],
                                capture_output=True, text=True, timeout=15)
        self.assertEqual(script.returncode, 1)
        self.assertIn('seats.json: seat glm-main has no account', script.stderr)

    def gallery(self):
        self.state['gallery'] = True
        self.save()
        (self.root / 'calls.jsonl').unlink(missing_ok=True)
        scripts = self.root / 'gallery'
        scripts.mkdir(exist_ok=True)
        (scripts / 'seat-gallery-up').write_text((ROOT / 'scripts/seat-gallery-up').read_text())
        wake = scripts / 'seat-role'
        wake.write_text('#!' + sys.executable + '\n' + STUB)
        wake.chmod(0o755)
        return subprocess.run(['/bin/bash', str(scripts / 'seat-gallery-up')],
                              capture_output=True, text=True, timeout=15)

    def assert_gallery_attempts_every_seat_and_role(self, result):
        seats = MAP['seats']
        roles = [row['role'] for row in seats.values()]
        starts = [c for c in self.calls() if c[0] == 'seat']
        wakes = [c for c in self.calls() if c[0] == 'seat-role']
        self.assertEqual(starts, [['seat', 'up', name] for name in seats])
        self.assertEqual(wakes, [['seat-role', 'wake', role] for role in roles])
        self.assertEqual([line.split(' ', 1)[0] for line in result.stdout.splitlines()], roles)
        for line in result.stdout.splitlines():
            json.loads(line.split(' ', 1)[1])
        self.assertFalse(any(c[0] == 'sleep' for c in self.calls()))
        self.assertEqual(result.stderr, '')

    def test_gallery_starts_panes_and_links_every_role(self):
        result = self.gallery()
        self.assertEqual(result.returncode, 0)
        self.assert_gallery_attempts_every_seat_and_role(result)

    def test_gallery_continues_after_a_pane_start_fails(self):
        self.state['start_failures'] = ['codex-main']
        result = self.gallery()
        self.assertEqual(result.returncode, 1)
        self.assert_gallery_attempts_every_seat_and_role(result)

    def test_gallery_continues_after_a_wake_fails(self):
        self.state['wake_results'] = {'reviewer': {'status': 'failed', 'reason': 'HAPI unavailable'}}
        result = self.gallery()
        self.assertEqual(result.returncode, 1)
        self.assert_gallery_attempts_every_seat_and_role(result)

    def test_gallery_logs_up_busy_and_refused_without_failing(self):
        self.state['wake_results'] = {
            'builder': {'status': 'up', 'reason': 'plain terminal'},
            'reviewer': {'status': 'busy'},
            'lander': {'status': 'refused', 'reason': 'quota HOLD'},
        }
        result = self.gallery()
        self.assertEqual(result.returncode, 0)
        self.assert_gallery_attempts_every_seat_and_role(result)
        self.assertEqual([json.loads(line.split(' ', 1)[1])['status']
                          for line in result.stdout.splitlines()[:3]], ['up', 'busy', 'refused'])

    def test_hold_is_scoped_to_role_account(self):
        self.state['quota'].append({'account': 'unrelated', 'verdict': 'HOLD'})
        self.save()
        self.no_mutation('awake')
        self.fixture('fixer', wrapped=False)
        self.state['quota'][0]['verdict'] = 'HOLD'
        self.save()
        self.assertIn('HOLD', self.no_mutation('refused')['reason'])

    def test_unknown_quota_fails_closed(self):
        self.fixture('fixer', wrapped=False)
        self.state['quota'][0]['verdict'] = 'unknown'
        self.save()
        self.no_mutation('failed')

    def test_secret_in_child_error_is_not_printed(self):
        for key in ('quota_error', 'hub_error'):
            with self.subTest(key=key):
                self.fixture('fixer', wrapped=False)
                self.state[key] = True
                self.save()
                self.no_mutation('failed')
                self.state.pop(key)

    def test_unknown_pane_identity_or_command_fails_closed(self):
        for key, value in (('stamp', 'another-seat'), ('pid', 900), ('command', 'bash'), ('pane_error', True)):
            with self.subTest(key=key):
                self.state[key] = value
                self.save()
                with patch.dict(os.environ, {'SEAT_ROLE_TEST_SESSION_WAIT': '3'}):
                    self.no_mutation('failed')
                self.state.pop(key)

    def test_hapi_glm_wrapper_counts_as_wrapped(self):
        # glm under the phone link runs a wrapper (Z.ai endpoint and key, then `hapi claude`); seat-role treats it as
        # wrapped so it finds and records the session (Connect) instead of answering up.
        self.fixture('builder', wrapped=False)
        role = ROLE.Wake.__new__(ROLE.Wake)
        role.hapi = str(self.home / '.local/bin/hapi')
        role.kind = 'glm'
        self.assertTrue(role.is_wrapped([str(self.runtime / 'hapi-glm'), '--dangerously-skip-permissions']))
        self.assertTrue(role.is_wrapped([role.hapi, 'claude', '--x']))
        self.assertFalse(role.is_wrapped([str(self.runtime / 'claude-glm'), '--dangerously-skip-permissions']))
        role.kind = 'grok'
        self.assertFalse(role.is_wrapped([str(self.runtime / 'hapi-glm')]))

    def test_live_plain_glm_answers_up_and_is_left_as_is(self):
        # glm runs in its terminal with no phone session: the seat is up, so the answer says up, and nothing changes.
        self.fixture('builder', wrapped=False)
        self.assertIn('no phone session', self.no_mutation('up')['reason'])

    def test_stale_saved_url_cannot_identify_a_session(self):
        state_dir = self.home / '.local/share/seat'
        state_dir.mkdir(parents=True)
        (state_dir / 'hapi-sessions.json').write_text(json.dumps({'schema_version': 1, 'seats': {
            self.seat: {'url': 'https://hapi.bll.dev/sessions/' + OTHER}, 'other-seat': {'url': 'preserve'}}}))
        self.state['sessions'].append(self.session(OTHER, pid=900))
        self.save()
        self.assertEqual(self.no_mutation('awake')['url'], EXPECTED_URL)
        saved = json.loads((state_dir / 'hapi-sessions.json').read_text())
        self.assertEqual(saved['seats']['other-seat'], {'url': 'preserve'})

    def test_wrong_session_identity_inactive_and_invalid_id_are_rejected(self):
        for session in (self.session(pid=900), self.session(active=False),
                        self.session(machineId='elsewhere'), self.session(path='/another/path'),
                        self.session(sid='../../bad'), self.session(sid='https://evil.invalid')):
            self.state['sessions'] = [session]
            self.save()
            with patch.dict(os.environ, {'SEAT_ROLE_TEST_SESSION_WAIT': '3'}):
                self.no_mutation('failed')

    def test_ambiguous_session_fails(self):
        self.state['sessions'].append(self.session(OTHER))
        self.save()
        with patch.dict(os.environ, {'SEAT_ROLE_TEST_SESSION_WAIT': '3'}):
            value = self.no_mutation('failed')
        self.assertEqual(value['reason'], 'more than one phone session claims this seat')

    def test_quit_timeout_does_not_force_stop_or_restart(self):
        self.fixture('fixer', wrapped=False)
        original = self.harness.read_bytes()
        self.state['never_exits'] = True
        self.save()
        self.assertIn('did not exit', self.invoke()['reason'])
        self.assertFalse(any(c[0] == 'seat' for c in self.calls()))
        self.assertEqual(self.harness.read_bytes(), original)
        self.assertEqual(self.clock.now, 40)

    def test_session_timeout_and_stale_target(self):
        for stale in (False, True):
            with self.subTest(stale=stale):
                self.fixture('fixer', wrapped=False)
                self.state['stale_target'] = stale
                self.state['sessions_after'] = []
                self.save()
                start = self.clock.now
                self.assertIn('60 seconds', self.invoke()['reason'])
                self.assertEqual(self.clock.now - start, 70)

    def test_startup_retries_missing_target_and_hub_failure(self):
        self.fixture('fixer', wrapped=False)
        self.state['hub_after_failures'] = 1
        self.save()
        load = ROLE.Wake.load_target
        pending = [True]

        def delayed_target(wake):
            state = json.loads((self.root / 'stub.json').read_text())
            if state.get('started') and pending:
                pending.pop()
                ROLE.fail('missing or invalid wake-target.json')
            return load(wake)

        with patch.object(ROLE.Wake, 'load_target', delayed_target):
            self.assertEqual(self.invoke()['status'], 'awake')
        self.assertEqual(len([c for c in self.calls() if c[0] == 'seat']), 1)
        self.assertEqual(self.clock.now, 12)

    def test_command_timeout_returns_one_safe_result(self):
        ROLE.seat_map()  # The once-per-process map read precedes the patched child commands.
        with patch.object(ROLE.subprocess, 'run', side_effect=subprocess.TimeoutExpired('stub', 1)):
            self.assertIn('timed out', self.no_mutation('failed')['reason'])

    def test_hub_discovery_uses_remaining_deadline(self):
        wake = ROLE.Wake('reviewer')
        wake.token = self.state['jwt']
        with patch.object(ROLE.subprocess, 'run') as run:
            run.return_value = subprocess.CompletedProcess([], 0, '{"sessions": []}', '')
            wake.hub('/sessions', self.clock.monotonic() + 0.25)
            self.assertEqual(run.call_args.kwargs['timeout'], 0.25)
        with self.assertRaises(ROLE.Result):
            wake.hub('/sessions', self.clock.monotonic())

    def test_restart_failure_has_no_retry_or_forced_stop(self):
        self.fixture('fixer', wrapped=False)
        self.state['start_error'] = True
        self.save()
        original = self.harness.read_bytes()
        self.assertEqual(self.invoke()['status'], 'failed')
        self.assertEqual(len([c for c in self.calls() if c[0] == 'seat']), 1)
        self.assertEqual(self.harness.read_bytes(), original)

    def test_seat_up_timeout_restores_harness(self):
        real_run = subprocess.run

        def seat_hangs(argv, **kwargs):
            if Path(argv[0]).name == 'seat':
                raise subprocess.TimeoutExpired(argv, kwargs.get('timeout'))
            return real_run(argv, **kwargs)

        for down in (None, 'server'):
            with self.subTest(down=down):
                if down:
                    self.down_fixture('fixer', wrapped=False, down=down)
                else:
                    self.fixture('fixer', wrapped=False)
                original = self.harness.read_bytes()
                with patch.object(ROLE.subprocess, 'run', side_effect=seat_hangs) as run:
                    value = self.invoke()
                self.assertEqual(value['status'], 'failed')
                self.assertTrue(any(Path(c.args[0][0]).name == 'seat' for c in run.call_args_list))
                self.assertEqual(self.harness.read_bytes(), original)
                self.assertEqual(next(self.runtime.glob('harness.json.pre-hapi.*')).read_bytes(), original)
                for path in self.runtime.glob('harness.json.pre-hapi.*'):
                    path.unlink()

    def test_no_session_after_cutover_restores_harness(self):
        # Shorten the 60-second wait; the reason still names the real limit.
        for down in (None, 'server'):
            with self.subTest(down=down), patch.dict(os.environ, {'SEAT_ROLE_TEST_SESSION_WAIT': '5'}):
                if down:
                    self.down_fixture('fixer', wrapped=False, down=down)
                else:
                    self.fixture('fixer', wrapped=False)
                self.state['sessions_after'] = []
                self.save()
                original = self.harness.read_bytes()
                start = self.clock.now
                value = self.invoke()
                self.assertEqual(value['status'], 'failed')
                self.assertIn('60 seconds', value['reason'])
                self.assertEqual(self.clock.now - start, 5 if down else 15)
                self.assertEqual([c for c in self.calls() if c[0] == 'seat'], [['seat', 'up', 'grok-main']])
                self.assertEqual(self.harness.read_bytes(), original)
                self.assertEqual(next(self.runtime.glob('harness.json.pre-hapi.*')).read_bytes(), original)
                for path in self.runtime.glob('harness.json.pre-hapi.*'):
                    path.unlink()
                (self.root / 'calls.jsonl').unlink()

    def test_resume_command_is_rejected(self):
        self.fixture('lander', wrapped=False)
        value = json.loads(self.harness.read_text())
        value['argv'] += ['resume', 'old-id']
        self.harness.write_text(json.dumps(value))
        self.assertIn('resume', self.no_mutation('failed')['reason'])

    def test_concurrent_tap_is_busy(self):
        state_dir = self.home / '.local/share/seat'
        state_dir.mkdir(parents=True)
        with (state_dir / 'role-wake.lock').open('a') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.no_mutation('busy')
        self.assertEqual(self.calls(), [['sb', 'seats', '--json']])

    def down_fixture(self, role='reviewer', wrapped=True, down='server'):
        self.fixture(role, wrapped)
        if wrapped:
            # A wrapped seat already has the HAPI cwd and no stored session.
            harness = json.loads(self.harness.read_text())
            harness['cwd'] = str(self.home / 'Projects')
            harness.pop('session')
            self.harness.write_text(json.dumps(harness))
        self.state['down'] = down
        self.state['sessions'] = []
        self.save()
        state_dir = self.home / '.local/share/seat'
        state_dir.mkdir(parents=True, exist_ok=True)
        (state_dir / 'hapi-sessions.json').write_text(json.dumps({'schema_version': 1, 'seats': {
            self.seat: {'url': 'https://hapi.bll.dev/sessions/' + OTHER}, 'other-seat': {'url': 'preserve'}}}))

    def saved_map(self):
        return json.loads((self.home / '.local/share/seat/hapi-sessions.json').read_text())

    def assert_started_down(self, value):
        self.assertEqual(value, {'status': 'awake', 'url': EXPECTED_URL})
        stub = json.loads((self.root / 'stub.json').read_text())
        # The stale entry was gone before seat up ran; awake() wrote the new one.
        self.assertNotIn(self.seat, stub['map_at_up']['seats'])
        self.assertEqual(stub['map_at_up']['seats']['other-seat'], {'url': 'preserve'})
        saved = self.saved_map()['seats']
        self.assertEqual(saved[self.seat]['url'], EXPECTED_URL)
        self.assertEqual(saved['other-seat'], {'url': 'preserve'})
        self.assertEqual([c for c in self.calls() if c[0] == 'seat'], [['seat', 'up', self.seat]])
        self.assertFalse(any('send-keys' in c for c in self.calls()))

    @unittest.skipUnless(REAL_TMUX and REAL_PS and REAL_SLEEP, 'real tmux and ps are required for the down gate')
    def test_kill_tmux_then_wake_starts_the_seat(self):
        # Required gate: a real tmux server on a private socket, killed as by a reboot.
        self.down_fixture()
        self.state.update(real=True, real_tmux=REAL_TMUX, real_ps=REAL_PS, real_sleep=REAL_SLEEP)
        self.save()
        harness = json.loads(self.harness.read_text())
        harness['expect_cmds'] = ['claude', 'hapi', 'sleep']
        self.harness.write_text(json.dumps(harness))
        sock = str(self.root / 'tmux.sock')
        tmux = [REAL_TMUX, '-S', sock, '-f', '/dev/null']
        self.addCleanup(subprocess.run, [REAL_TMUX, '-S', sock, 'kill-server'], capture_output=True)
        self.addCleanup(self.kill_child)
        pane, pid = subprocess.run(tmux + ['new-session', '-d', '-s', 'swarm', '-P', '-F',
                                           '#{pane_id} #{pane_pid}', 'exec ' + REAL_SLEEP + ' 600'],
                                   check=True, capture_output=True, text=True).stdout.split()
        subprocess.run(tmux + ['set-option', '-p', '-t', pane, '@seat', self.seat], check=True)
        (self.runtime / 'wake-target.json').write_text(json.dumps({
            'seat': self.seat, 'harness': self.kind, 'pane': pane, 'pane_pid': int(pid), 'socket': sock}))
        subprocess.run(tmux + ['kill-server'], check=True)
        self.assertNotEqual(subprocess.run(tmux + ['list-panes', '-a'], capture_output=True).returncode, 0)
        original = self.harness.read_bytes()
        self.assert_started_down(self.invoke())
        self.assertEqual(self.harness.read_bytes(), original)
        target = json.loads((self.runtime / 'wake-target.json').read_text())
        self.assertTrue(any(c[0] == 'tmux' and c[3] == 'display-message' and target['pane'] in c
                            for c in self.calls()))
        self.assertFalse((self.tmux_tmp / ('tmux-' + str(os.getuid()))).exists())

    def kill_child(self):
        child = self.root / 'child.pid'
        if child.exists():
            with contextlib.suppress(OSError, ValueError):
                os.kill(int(child.read_text()), 15)

    def test_down_plain_seat_is_cut_over_then_started(self):
        for down in ('server', 'pane'):
            with self.subTest(down=down):
                self.down_fixture('fixer', wrapped=False, down=down)
                original = self.harness.read_bytes()
                self.assert_started_down(self.invoke())
                changed = json.loads(self.harness.read_text())
                self.assertEqual(changed['argv'], [str(self.home / '.local/bin/hapi'), 'grok', '--fixture-flag'])
                self.assertEqual(next(self.runtime.glob('harness.json.pre-hapi.*')).read_bytes(), original)
                # No idle observation: nothing is there to interrupt.
                self.assertLess(self.clock.now, 10)
                self.assertFalse(any(c[0] == 'tmux' and c[3] == 'capture-pane' for c in self.calls()))
                for path in self.runtime.glob('harness.json.pre-hapi.*'):
                    path.unlink()
                (self.root / 'calls.jsonl').unlink()

    def test_down_missing_target_counts_as_down(self):
        self.down_fixture()
        (self.runtime / 'wake-target.json').unlink()
        self.assert_started_down(self.invoke())
        self.assertEqual(len(self.swarm_probes()), 1)

    def test_down_confirmed_when_swarm_has_no_pane_for_the_seat(self):
        self.down_fixture()
        (self.runtime / 'wake-target.json').unlink()
        self.state['swarm'] = ['other-seat', '']
        self.save()
        self.assert_started_down(self.invoke())

    def test_seat_still_running_in_swarm_is_never_started(self):
        # The recorded target is missing or names an old socket, but the swarm's
        # default socket still has a pane stamped with this seat.
        for role, wrapped in (('reviewer', True), ('fixer', False)):
            for missing in (True, False):
                with self.subTest(role=role, missing=missing):
                    self.down_fixture(role, wrapped)
                    if missing:
                        (self.runtime / 'wake-target.json').unlink()
                    self.state['swarm'] = ['other-seat', self.seat]
                    self.save()
                    saved = (self.home / '.local/share/seat/hapi-sessions.json').read_bytes()
                    value = self.no_mutation('failed')
                    self.assertEqual(value['reason'],
                                     'the seat looks like it is still running in the swarm; nothing was started')
                    self.assertEqual((self.home / '.local/share/seat/hapi-sessions.json').read_bytes(), saved)
                    self.assertEqual(len(self.swarm_probes()), 1)
                    (self.root / 'calls.jsonl').unlink()

    def test_swarm_probe_error_is_not_down(self):
        self.down_fixture()
        (self.runtime / 'wake-target.json').unlink()
        self.state['swarm'] = 'error'
        self.save()
        self.assertEqual(self.no_mutation('failed')['reason'],
                         "the swarm's tmux returned an error, so nothing was started")

    def test_down_with_low_memory_is_refused_before_start(self):
        self.down_fixture()
        self.memory.write_text('at,2999\n')
        self.assertEqual(self.no_mutation('refused')['status'], 'refused')
        self.assertIn(self.seat, self.saved_map()['seats'])

    def test_down_with_quota_hold_is_refused_before_start(self):
        self.down_fixture('fixer', wrapped=False)
        self.state['quota'][0]['verdict'] = 'HOLD'
        self.save()
        self.no_mutation('refused')

    def test_glm_down_starts_plain_and_says_why(self):
        self.down_fixture('builder', wrapped=False)
        self.state['plain_up'] = True
        self.save()
        original = self.harness.read_bytes()
        value = self.invoke()
        self.assertEqual(value, {'status': 'up',
                                 'reason': 'builder is running in its terminal, but glm has no phone session yet'})
        self.assertEqual(self.harness.read_bytes(), original)
        self.assertFalse(list(self.runtime.glob('harness.json.pre-hapi.*')))
        self.assertEqual([c for c in self.calls() if c[0] == 'seat'], [['seat', 'up', 'glm-main']])
        self.assertNotIn(self.seat, self.saved_map()['seats'])
        self.assertFalse(any(c[0] == 'curl' for c in self.calls()))

    def test_unwrappable_down_seat_starts_plain(self):
        for argv in (['/usr/bin/env', 'grok'], ['/stub/grok', '--resume', 'old']):
            with self.subTest(argv=argv):
                self.down_fixture('fixer', wrapped=False)
                self.state['plain_up'] = True
                self.save()
                value = json.loads(self.harness.read_text())
                value['argv'] = argv
                self.harness.write_text(json.dumps(value))
                original = self.harness.read_bytes()
                result = self.invoke()
                self.assertEqual(result['status'], 'up')
                self.assertTrue(result['reason'].startswith('fixer is running in its terminal, but '))
                self.assertEqual(self.harness.read_bytes(), original)
                (self.root / 'calls.jsonl').unlink()

    def test_down_start_failure_restores_harness(self):
        self.down_fixture('fixer', wrapped=False)
        self.state['start_error'] = True
        self.save()
        original = self.harness.read_bytes()
        value = self.invoke()
        self.assertEqual(value['status'], 'failed')
        self.assertIn('could not start fixer', value['reason'])
        self.assertEqual(self.harness.read_bytes(), original)

    def test_down_no_session_within_60_seconds(self):
        self.down_fixture()
        self.state['sessions_after'] = []
        self.save()
        self.assertEqual(self.invoke()['reason'],
                         'the swarm was not running; started it, but no session appeared within 60 seconds')

    def test_pane_pid_changed_is_identity_failure_not_down(self):
        for role, wrapped in (('reviewer', True), ('fixer', False)):
            with self.subTest(role=role):
                self.fixture(role, wrapped)
                self.state['pid'] = 900
                self.save()
                with patch.dict(os.environ, {'SEAT_ROLE_TEST_SESSION_WAIT': '3'}):
                    self.assertIn('pane changed', self.no_mutation('failed')['reason'])

    def test_unexpected_tmux_error_is_not_down(self):
        self.down_fixture(down='tmux_error')
        value = self.no_mutation('failed')
        self.assertEqual(value['reason'], "the swarm's tmux returned an error, so nothing was started")

    def test_reasons_never_name_bare_tool_failures(self):
        self.fixture('fixer', wrapped=False)
        self.state['start_error'] = True
        self.save()
        self.assertNotIn('tmux failed', self.invoke()['reason'])

    def test_shell_entrypoint(self):
        result = subprocess.run(['/bin/bash', str(ROOT / 'scripts/seat-role'), 'wake', 'reviewer'],
                                capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stderr, '')
        self.assertEqual(json.loads(result.stdout), {'status': 'awake', 'url': EXPECTED_URL})
        for key in ('access', 'jwt'):
            self.assertNotIn(self.state[key], result.stdout + result.stderr)


class BackgroundWorkTest(unittest.TestCase):
    """The MCP bridge HAPI spawns is not work; anything else under the agent CLI is; unknown is busy."""

    def tree(self, root, procs):
        # procs: {pid: (cmdline, [child pids])}
        for pid, (cmd, kids) in procs.items():
            task = Path(root) / str(pid) / "task" / str(pid)
            task.mkdir(parents=True, exist_ok=True)
            (Path(root) / str(pid) / "cmdline").write_bytes(cmd.replace(" ", "\0").encode())
            (task / "children").write_text(" ".join(str(k) for k in kids))
        return root

    def test_mcp_bridge_only_is_idle(self):
        with tempfile.TemporaryDirectory() as root:
            self.tree(root, {10: ("/home/x/.local/bin/claude --strict-mcp-config", [11]),
                             11: ("/home/x/.local/share/hapi/hapi-c87da61 mcp --url http://127.0.0.1", [])})
            self.assertFalse(ROLE.background_work(10, proc=root))

    def test_real_child_is_busy(self):
        with tempfile.TemporaryDirectory() as root:
            self.tree(root, {10: ("/home/x/.local/bin/claude", [11, 13]),
                             11: ("/home/x/.local/share/hapi/hapi-c87da61 mcp --url http://127.0.0.1", []),
                             13: ("node build.mjs", [])})
            self.assertTrue(ROLE.background_work(10, proc=root))

    def test_hapi_wrapper_with_mcp_bridge_is_idle(self):
        with tempfile.TemporaryDirectory() as root:
            self.tree(root, {20: ("/home/x/.local/bin/hapi codex --cwd /p", [21]),
                             21: ("/home/x/node/lib/node_modules/@openai/codex/bin/codex", [22]),
                             22: ("/home/x/.local/share/hapi/hapi-c87da61 mcp --url http://127.0.0.1", [])})
            self.assertFalse(ROLE.background_work(20, proc=root))

    def test_unknown_agent_is_busy(self):
        with tempfile.TemporaryDirectory() as root:
            self.tree(root, {30: ("sh -c sleep", [])})
            self.assertTrue(ROLE.background_work(30, proc=root))


class PinsTest(unittest.TestCase):
    def test_status(self):
        self.assertEqual(ROLE.pin_status("claude-opus-5-5", ["hapi", "claude", "--model", "claude-opus-5-5"]), "ok")
        self.assertEqual(ROLE.pin_status("claude-opus-5-5", ["hapi", "claude", "-m", "claude-opus-4-1"]), "drift (claude-opus-4-1)")
        self.assertEqual(ROLE.pin_status("grok-4.7", ["hapi", "grok"]), "unpinned")
        self.assertEqual(ROLE.pin_status("glm-5.3", ["hapi-glm"], "glm-5.3"), "ok")

    def test_effort_flags(self):
        self.assertEqual(ROLE.launch_effort(["hapi", "codex", "-c", "model_reasoning_effort=low"], "codex"), "low")
        self.assertEqual(ROLE.launch_effort(["hapi", "grok", "--reasoning-effort", "high"], "grok"), "high")
        self.assertEqual(ROLE.launch_effort(["hapi", "grok", "--reasoning-effort=medium"], "grok"), "medium")
        self.assertIsNone(ROLE.launch_effort(["hapi", "claude", "--model", "claude-opus-5-5"], "claude"))
        self.assertIsNone(ROLE.launch_effort(["hapi", "agy"], "agy"))

    def test_report_reads_the_map_and_harness_files(self):
        seats = {
            "opus-main": {"model": "claude-opus-5-5", "effort": "xhigh", "harness": "claude", "role": "reviewer",
                          "name": "Claude", "account": "claude-personal"},
            "grok-main": {"model": "grok-4.7", "effort": "high", "harness": "grok", "role": "fixer",
                          "name": "Grok", "account": "grok-personal"},
            "glm-main": {"model": "glm-5.3", "effort": None, "harness": "glm", "role": "builder",
                         "name": "GLM", "account": "zai-personal"},
            "fable-main": {"model": "claude-fable-5-1", "effort": "high", "harness": "claude", "role": "adjudicator",
                           "name": "Flight", "account": "claude-personal"},
        }
        root = Path(tempfile.mkdtemp(prefix="pins-"))
        self.addCleanup(shutil.rmtree, root, ignore_errors=True)
        rt = root / "rt"
        harnesses = {
            "opus-main": ["hapi", "claude", "--model=claude-opus-5-5"],
            "grok-main": ["hapi", "grok", "--model", "grok-4.1", "--reasoning-effort", "medium"],
            "glm-main": [str(root / "rt/glm-main/hapi-glm")],
        }
        for seat, argv in harnesses.items():
            (rt / seat).mkdir(parents=True)
            (rt / seat / "harness.json").write_text(json.dumps({"argv": argv}))
        (rt / "glm-main/hapi-glm").write_text("#!/bin/sh\nexport ANTHROPIC_MODEL=glm-5.2\n")
        # claude states no effort flag; its settings file carries it.
        (root / "home/.claude").mkdir(parents=True)
        (root / "home/.claude/settings.json").write_text(json.dumps({"effort": "xhigh"}))
        stub = root / "sb"
        stub.write_text("#!" + sys.executable + "\n" + STUB)
        stub.chmod(0o755)
        (root / "stub.json").write_text(json.dumps({"map": {"schema_version": 1, "seats": seats}}))
        ROLE._SEAT_MAP = None
        out = io.StringIO()
        with patch.dict(os.environ, {"PATH": str(root), "STUB_ROOT": str(root),
                                     "SEAT_ROLE_HOME": str(root / "home"), "SEAT_RUNTIMES": str(rt)}):
            with contextlib.redirect_stdout(out):
                self.assertEqual(ROLE.main(["pins"]), 0)
        report = json.loads(out.getvalue())["seats"]
        self.assertEqual(report["opus-main"], "ok, effort ok")
        self.assertEqual(report["grok-main"], "drift (grok-4.1), effort drift (medium)")
        self.assertEqual(report["glm-main"], "drift (glm-5.2)")
        self.assertEqual(report["fable-main"], "unpinned")


if __name__ == '__main__':
    unittest.main(verbosity=2)
