#!/usr/bin/env python3
"""Synthetic CLI + real PTY lifecycle; no ANSI scraping or provider launches."""
import datetime
import fcntl
import json
import os
import pty
import select
import shutil
import struct
import subprocess
import sys
import tempfile
import termios
import time
from pathlib import Path

binary = Path(sys.argv[1] if len(sys.argv) > 1 else 'target/debug/tmux-agent-monitor').resolve()
with tempfile.TemporaryDirectory(prefix='am-sessions-') as tmp:
    root = Path(tmp)
    source = root / 'pi-sessions'
    source.mkdir()
    state = root / 'state'
    env = dict(os.environ, HOME=str(root / 'home'), PI_CODING_AGENT_SESSION_DIR=str(source),
               PI_CODING_AGENT_DIR=str(root / 'pi'), TMUX_TMPDIR=str(root / 'no-tmux'),
               CLAUDE_CONFIG_DIR=str(root / 'claude'), CODEX_HOME=str(root / 'codex'),
               XDG_DATA_HOME=str(root / 'data'), TERM='xterm-256color', NO_COLOR='1')
    for key in ('TMUX', 'TMUX_PANE', 'TMUX_AGENT_MONITOR_HOME', 'AGENT_MONITOR_HOME'):
        env.pop(key, None)

    def cli(*args):
        return subprocess.run([str(binary), '--data-dir', str(state), *args],
                              env=env, capture_output=True, text=True, check=True, timeout=15)

    def session(path, identity, name, timestamp, cwd=None):
        entries = [dict(type='session', version=3, id=identity, cwd=str(cwd or root), timestamp=timestamp),
                   dict(type='session_info', id='name', parentId=None, name=name),
                   dict(type='message', id='user', parentId='name', timestamp=timestamp,
                        message=dict(role='user', content='Check the synthetic session.')),
                   dict(type='message', id='reply', parentId='user', timestamp=timestamp,
                        message=dict(role='assistant', content=[dict(type='text', text='Synthetic reply.')]))]
        path.write_text(''.join(json.dumps(e) + '\n' for e in entries))

    session(source / 'old.jsonl', 'old-native', 'Historic onboarding', '2026-01-01T00:00:00Z')
    assert cli('--version').stdout.startswith('tmux-agent-monitor ')
    assert 'Usage: tmux-agent-monitor' in cli('--help').stdout
    rows = json.loads(cli('sessions', '--all', '--json').stdout)
    assert len(rows) == 1 and rows[0]['state'] == 'history'
    assert json.loads(cli('sessions', '--json').stdout) == []
    # Even chats created after discovery stay unmarked until an explicit action.
    future = (datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds=1)).isoformat()
    session(source / 'new.jsonl', 'new-native', 'New session', future)
    assert json.loads(cli('sessions', '--json').stdout) == []
    rows = json.loads(cli('sessions', '--all', '--json').stdout)
    saved = next(r for r in rows if r['metadata']['native_id'] == 'new-native')
    assert saved['state'] == 'history'
    identity = saved['id']
    cli('note', identity, 'Persistent next step')
    assert json.loads(cli('sessions', '--json').stdout) == []
    cli('reopen', identity)
    assert json.loads(cli('sessions', '--json').stdout)[0]['state'] == 'resume'
    cli('done', identity)
    assert json.loads(cli('sessions', '--json').stdout) == []
    cli('undo', identity)
    assert json.loads(cli('sessions', '--json').stdout)[0]['note'] == 'Persistent next step'
    # Rename fallback preserves existing decisions and notes without migrating data.
    legacy_state = Path(env['HOME']) / '.local/state/agent-monitor'
    shutil.copytree(state, legacy_state)
    def default_cli(*args, overrides=None):
        return subprocess.run([str(binary), *args], env=dict(env, **(overrides or {})), capture_output=True, text=True, check=True, timeout=15)
    default_cli('done', identity)
    legacy_rows = json.loads(default_cli('sessions', '--all', '--json').stdout)
    kept = next(r for r in legacy_rows if r['id'] == identity)
    assert kept['state'] == 'done' and kept['note'] == 'Persistent next step'
    assert not (Path(env['HOME']) / '.local/state/tmux-agent-monitor').exists()
    assert json.loads(cli('sessions', '--json').stdout)[0]['state'] == 'resume'
    for key in ('TMUX_AGENT_MONITOR_HOME', 'AGENT_MONITOR_HOME'):
        selected = root / ('selected-' + key.lower())
        default_cli('sync', overrides={key: str(selected)})
        assert (selected / 'sessions.db').exists()
    canonical, ignored = root / 'preferred-canonical', root / 'ignored-legacy'
    default_cli('sync', overrides={'TMUX_AGENT_MONITOR_HOME': str(canonical), 'AGENT_MONITOR_HOME': str(ignored)})
    assert (canonical / 'sessions.db').exists() and not ignored.exists()
    # Launching a provider requires explicit consent; no consent, no spawn.
    denied = subprocess.run([str(binary), '--data-dir', str(state), 'open', identity],
                            env=env, capture_output=True, text=True, timeout=15)
    assert denied.returncode != 0
    # Offline screen classification: pure, no tmux, no agent process.
    screen = root / 'screen.txt'
    screen.write_text('⠼ Working on it\n')
    out = json.loads(cli('agent', 'explain', '--file', str(screen), '--agent', 'codex', '--json').stdout)
    assert out['state'] == 'working' and out['rule'] == 'spinner' and out['manifest'] == 'bundled'
    diagnostic_state = root / 'diagnostics-must-not-create-state'
    for provider, text, expected in [('pi', '⠼ Working', 'working'),('claude', '❯ 1. Yes, proceed\nEsc to cancel', 'blocked'),('opencode', '△ Permission required', 'blocked'),('pi', 'enter to confirm\nesc to cancel', 'waiting'),('codex', 'API Error: authentication failed', 'error')]:
        screen.write_text(text)
        result = subprocess.run([str(binary), '--data-dir', str(diagnostic_state), 'agent', 'explain', '--file', str(screen), '--agent', provider, '--json'], env=env, capture_output=True, text=True, check=True, timeout=15)
        report = json.loads(result.stdout)
        assert report['state'] == expected and report['inferred']
    assert not diagnostic_state.exists(), 'offline classification must not initialize SQLite/state'
    fake_bin = root / 'bin'
    fake_bin.mkdir()
    fake_ps = fake_bin / 'ps'
    fake_ps.write_text(f'''#!{sys.executable}
import os, sys
from pathlib import Path
if '-p' in sys.argv:
    print(f"Mon Jan 1 00:00:00 2024 {{os.getuid()}}")
    sys.exit(0)
counter = Path(os.environ['AM_TEST_COUNTER'])
changed = os.environ.get('AM_TEST_REUSE') and counter.exists()
start = '00:00:01' if changed else '00:00:00'
print(f"1001 1 {{os.getuid()}} Mon Jan 1 00:00:00 2024 /bin/sh")
print(f"1002 1001 {{os.getuid()}} Mon Jan 1 {{start}} 2024 pi")
''')
    fake_tmux = fake_bin / 'tmux'
    fake_tmux.write_text(f'''#!{sys.executable}
import os, sys
from pathlib import Path
args = sys.argv[1:]
assert args[:2] == ['-S', '/fake/socket'], args
if args[2] == 'display-message': print('1:fixture')
elif args[2] == 'list-panes': print('%23\\t1001\\tsh\\t/repo/project\\tfixture\\t1\\tfixture:1.0\\tpi')
elif args[2] == 'capture-pane':
    assert args[3:] == ['-p', '-t', '%23', '-S', '0'], args
    counter = Path(os.environ['AM_TEST_COUNTER'])
    count = int(counter.read_text()) if counter.exists() else 0
    counter.write_text(str(count + 1))
    print('⠼ Working' if count == 0 else '>')
else: raise AssertionError('non-read-only command: ' + repr(args))
''')
    for executable in (fake_ps, fake_tmux): executable.chmod(0o700)
    counter = root / 'capture-counter'
    watch_env = dict(env, PATH=str(fake_bin) + os.pathsep + env['PATH'], TMUX='/fake/socket,1,0', AM_TEST_COUNTER=str(counter))
    result = subprocess.run([str(binary), '--data-dir', str(diagnostic_state), 'agent', 'explain', '%23', '--watch', '--samples', '3', '--json'], env=watch_env, capture_output=True, text=True, check=True, timeout=15)
    reports = [json.loads(line) for line in result.stdout.splitlines()]
    assert [r['state'] for r in reports] == ['working', 'idle', 'finished']
    assert reports[-1]['evidence']['observations'] == 2
    assert 'inferred turn end' in reports[-1]['evidence']['transition']
    counter.unlink()
    result = subprocess.run([str(binary), '--data-dir', str(diagnostic_state), 'agent', 'explain', '%23', '--watch', '--samples', '3', '--json'], env=dict(watch_env, AM_TEST_REUSE='1'), capture_output=True, text=True, timeout=15)
    assert result.returncode != 0 and 'identity changed' in result.stderr
    assert [json.loads(line)['state'] for line in result.stdout.splitlines()] == ['working']
    assert not diagnostic_state.exists(), 'watching must not initialize SQLite/state'
    # Verified Pi presence must not promote saved History or unsaved chats to [R].
    presence_dir = state / 'presence'
    presence_dir.mkdir(mode=0o700)
    record = presence_dir / 'fixture.json'
    bridge = dict(version=1, nonce='fixture', generation=1, pid=1002,
                  process_start='Mon Jan 1 00:00:00 2024', native_id='old-native',
                  file=str(source / 'old.jsonl'), cwd=str(root), name='Historic onboarding',
                  leaf=None, socket='/fake/socket', pane='%23', seen=int(time.time() * 1000))

    def write_bridge():
        record.write_text(json.dumps(bridge))
        record.chmod(0o600)

    def live_cli(*args):
        return subprocess.run([str(binary), '--data-dir', str(state), *args],
                              env=watch_env, capture_output=True, text=True, check=True, timeout=15)

    write_bridge()
    opened = json.loads(live_cli('sessions', '--open', '--json').stdout)['sessions']
    saved = next(s for s in opened if s['metadata']['native_id'] == 'old-native')
    assert saved['bindings'] and saved['state'] == 'history'
    bridge.update(native_id='unsaved-native', file=None, name='Unsaved chat')
    write_bridge()
    opened = json.loads(live_cli('sessions', '--open', '--json').stdout)['sessions']
    unsaved = next(s for s in opened if s['id'] == 'live-fixture')
    assert unsaved['bindings'] and unsaved['state'] == 'history'
    bridge.update(native_id='new-native', file=str(source / 'new.jsonl'), name='New session')
    write_bridge()
    for expected in ('resume', 'done'):
        opened = json.loads(live_cli('sessions', '--open', '--json').stdout)['sessions']
        assert next(s for s in opened if s['id'] == identity)['state'] == expected
        if expected == 'resume':
            live_cli('done', identity)
    live_cli('undo', identity)
    record.unlink()
    bad = subprocess.run([str(binary), '--data-dir', str(state), 'agent', 'explain',
                          '--file', str(screen), '--agent', 'bogus'],
                         env=env, capture_output=True, text=True, timeout=15)
    assert bad.returncode != 0

    # All providers use local fixtures; no real home-directory history is read.
    claude = root / 'claude' / 'projects' / 'fixture'
    claude.mkdir(parents=True)
    (claude / 'claude.jsonl').write_text(''.join(json.dumps(e) + '\n' for e in [
        dict(type='mode', sessionId='claude-native'),
        dict(type='user', sessionId='claude-native', cwd=str(root), uuid='cu',
             parentUuid=None, timestamp='2026-01-01T00:00:00Z',
             message=dict(role='user', content='Claude fixture')),
        dict(type='assistant', sessionId='claude-native', uuid='ca', parentUuid='cu',
             message=dict(role='assistant', content=[dict(type='text', text='Claude reply')]))]))
    codex = root / 'codex' / 'sessions'
    codex.mkdir(parents=True)
    (codex / 'codex.jsonl').write_text(''.join(json.dumps(e) + '\n' for e in [
        dict(type='session_meta', payload=dict(id='codex-native', cwd=str(root), timestamp='2026-01-01T00:00:00Z')),
        dict(type='response_item', payload=dict(type='message', role='user', content=[dict(type='input_text', text='Codex fixture')]))]))
    import sqlite3
    opencode = root / 'data' / 'opencode'
    opencode.mkdir(parents=True)
    with sqlite3.connect(opencode / 'opencode.db') as db:
        db.executescript('CREATE TABLE session_v2(id TEXT,directory TEXT,title TEXT,parent_id TEXT,time_created INTEGER,time_updated INTEGER); CREATE TABLE session_message(id TEXT,session_id TEXT,type TEXT,seq INTEGER,time_created INTEGER,data TEXT);')
        db.execute('INSERT INTO session_v2 VALUES(?,?,?,?,?,?)', ('ses_fixture', str(root), 'OpenCode fixture', None, 1, 2))
        db.execute('INSERT INTO session_message VALUES(?,?,?,?,?,?)', ('om', 'ses_fixture', 'user', 1, 1, json.dumps(dict(text='OpenCode request'))))
    rows = json.loads(cli('sessions', '--all', '--json').stdout)
    assert {s['metadata']['provider'] for s in rows} == {'pi', 'claude', 'codex', 'opencode'}
    for provider in ('claude', 'codex', 'opencode'):
        saved = next(s for s in rows if s['metadata']['provider'] == provider)
        preview = json.loads(cli('show', saved['id'], '--json').stdout)
        assert preview['conversation']['messages'] and not preview['conversation']['partial']
    opened = json.loads(cli('sessions', '--open', '--json').stdout)
    assert 'sessions' in opened and 'panes' in opened
    project_details = json.loads(cli('details', '--project', str(root), '--json').stdout)
    assert project_details['project']['folder'] == str(root.resolve())
    assert project_details['project']['saved_sessions'] == 5
    session_details = json.loads(cli('details', identity, '--json').stdout)
    assert session_details['session']['agent'] == 'pi'
    assert session_details['session']['source_file'] == str((source / 'new.jsonl').resolve())
    assert 'Folder:' in cli('details', identity).stdout
    # Stale project/view preferences must not override All projects / Open startup.
    with sqlite3.connect(state / 'sessions.db') as db:
        db.execute('INSERT OR REPLACE INTO preferences VALUES(?,?)', ('project', str(root)))
        db.execute('INSERT OR REPLACE INTO preferences VALUES(?,?)', ('explorer_view', json.dumps('done')))

    # Nested project folders for real keyboard folding/layout/sort coverage.
    (root / 'api' / 'tests').mkdir(parents=True)
    session(source / 'api.jsonl', 'api-native', 'API fixture', '2026-01-01T00:00:00Z', root / 'api')
    session(source / 'tests.jsonl', 'tests-native', 'Test fixture', '2026-01-01T00:00:00Z', root / 'api' / 'tests')

    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 32, 120, 0, 0))

    def controlling_terminal():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)

    process = subprocess.Popen([str(binary), '--data-dir', str(state)],
                               stdin=slave, stdout=slave, stderr=slave, env=env, cwd=root,
                               preexec_fn=controlling_terminal)
    os.close(slave)

    def drain(seconds):
        captured = bytearray()
        until = time.monotonic() + seconds
        while time.monotonic() < until:
            ready, _, _ = select.select([master], [], [], max(0, until - time.monotonic()))
            if ready:
                try:
                    chunk = os.read(master, 65536)
                    if not chunk:
                        break
                    captured.extend(chunk)
                except OSError:
                    break
        return bytes(captured)

    try:
        import re
        screen = re.sub(rb'\x1b\[[0-?]*[ -/]*[@-~]', b'', drain(1.2))
        assert process.poll() is None
        assert b'All projects' in screen and b'Open \xc2\xb7' in screen
        assert '\u203a All projects'.encode() in screen
        # No panes are open in this fixture: explicitly choose All before editing.
        os.write(master, b'fj\r')
        drain(0.4)
        os.write(master, b'1C')
        assert '▸'.encode() in drain(0.3)
        # Root disclosure at column 4 / row 6 (SGR mouse coordinates are 1-based).
        os.write(master, b'\x1b[<0;4;6M')
        assert '▾'.encode() in drain(0.3)
        os.write(master, b'\x1b[<0;4;6M')
        assert '▸'.encode() in drain(0.3)
        os.write(master, b'v')
        assert b'Flat' in drain(0.3)
        os.write(master, b's')
        assert b'Recent' in drain(0.3)
        os.write(master, b'vE')
        assert '▾'.encode() in drain(0.3)
        os.write(master, b's2')
        drain(0.2)
        os.write(master, b'nNote from the TUI\r')
        drain(0.8)
        os.write(master, b'd')
        drain(0.8)
        row = next(s for s in json.loads(cli('sessions', '--all', '--json').stdout) if s['id'] == identity)
        assert row['state'] == 'done'
        assert row['note'].endswith('Note from the TUI')
        # Categorized action menu: direct terminal input, no provider action.
        os.write(master, b'?')
        assert b'Session actions' in drain(0.3)
        os.write(master, b'\t')
        assert b'Folders' in drain(0.3)
        # Catalog category in the 82x16 modal centered within 120x32.
        os.write(master, b'\x1b[<0;22;17M')
        assert b'Refresh catalog' in drain(0.3)
        os.write(master, b'\x1b')
        drain(0.2)
        # Exercise project/filter/detail panels and a small terminal resize.
        os.write(master, b'fjj\r\tzk\x1b')
        fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack('HHHH', 12, 40, 0, 0))
        drain(0.4)
        start = time.monotonic()
        os.write(master, b'q')
        drain(0.2)
        process.wait(timeout=3)
        assert process.returncode == 0
        assert time.monotonic() - start < 1
        print('PASS: four-provider sessions, details, Open startup, project folding/flat/sorting, categorized menu, live-state diagnostics, rename compatibility, notes, undo, PTY resize and quit')
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=3)
        os.close(master)
