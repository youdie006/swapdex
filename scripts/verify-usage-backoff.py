#!/usr/bin/env python3
"""Exercise usage backoff and automatic recovery in one isolated real picker.

Requires tmux. Uses a private tmux socket, fixture credentials and fake curl;
no real provider, OAuth or model requests are sent.
"""

if not __debug__:
    raise RuntimeError("Run this verifier without Python optimization")

import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time


FAKE_CURL = r'''#!/usr/bin/env python3
import json,os,shlex,sys,time
from pathlib import Path
root=Path(os.environ['SWAPDEX_ROOT'])
cfg=sys.stdin.read()
assert 'https://api.anthropic.com/api/oauth/usage' in cfg
assert 'Authorization: Bearer FIXTURE-USAGE-ACCESS' in cfg
status=int((root/'reply').read_text())
fd=os.open(root/'requests.jsonl',os.O_WRONLY|os.O_APPEND|os.O_CREAT,0o600)
os.write(fd,(json.dumps({'status':status,'at':time.time()})+'\n').encode())
os.close(fd)
for line in cfg.splitlines():
 key,sep,value=line.partition('=')
 if sep and key.strip()=='dump-header':
  path=Path(shlex.split(value.strip())[0])
  path.write_text('HTTP/2 '+str(status)+'\r\nRetry-After: 0\r\n\r\n')
if status==429:
 print('{"type":"error","error":{"type":"rate_limit_error"}}')
else:
 print(json.dumps({'five_hour':{'utilization':27,'resets_at':int(time.time())+3600},'seven_day':{'utilization':38,'resets_at':int(time.time())+86400}}))
print(status,end='')
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--swapdex', type=Path, required=True)
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    binary = args.swapdex.resolve(strict=True)
    tmux = shutil.which('tmux')
    assert tmux, 'tmux is required for a private test terminal'
    report = {'result': 'FAIL', 'version': subprocess.check_output(
        [binary, '--version'], text=True).strip(),
        'native_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
        'real_network_requests': 0, 'oauth_requests': 0, 'model_requests': 0}
    try:
        with tempfile.TemporaryDirectory(prefix='swapdex-usage-backoff-') as temporary:
            root = Path(temporary)
            store = root / '.local/share/swapdex'
            slot = store / 'slots/fixture'
            slot.mkdir(parents=True)
            identity = {'oauthAccount': {'accountUuid': 'fixture-user',
                'organizationUuid': 'fixture-org', 'emailAddress': 'fixture@example.com'}}
            (slot / '.claude.json').write_text(json.dumps(identity))
            credentials = slot / '.credentials.json'
            credentials.write_text(json.dumps({'claudeAiOauth': {
                'accessToken': 'FIXTURE-USAGE-ACCESS', 'refreshToken': 'FIXTURE-REFRESH',
                'expiresAt': 9_000_000_000_000}}))
            credentials.chmod(0o600)
            credential_before = credentials.read_bytes()
            (store / 'slots.json').write_text(json.dumps([{'name': 'fixture',
                'id': 'fixture', 'tool': 'claude-code', 'adopted': False,
                'config_dir': str(slot)}]))
            (store / 'active-claude').write_text(str(slot))
            started = int(time.time())
            cache = store / 'quota-cache.json'
            cache.write_text(json.dumps({'fixture': {'five_h': 12,
                'five_h_reset': started + 3600, 'seven_d': 34,
                'seven_d_reset': started + 86400, 'at': started - 600}}))
            cache_before = cache.read_bytes()
            (root / 'reply').write_text('429')
            fake = root / 'fake-curl'
            fake.write_text(FAKE_CURL)
            fake.chmod(0o700)
            env = dict(os.environ, SWAPDEX_ROOT=str(root), SWAPDEX_CURL=str(fake),
                TERM='xterm-256color', HTTPS_PROXY='http://127.0.0.1:1',
                HTTP_PROXY='http://127.0.0.1:1', ALL_PROXY='http://127.0.0.1:1')
            for name in ('TMUX', 'CLAUDE_CONFIG_DIR', 'CLAUDE_SECURESTORAGE_CONFIG_DIR',
                         'CODEX_HOME', 'ANTHROPIC_API_KEY', 'ANTHROPIC_AUTH_TOKEN',
                         'OPENAI_API_KEY', 'BASH_ENV', 'ENV'):
                env.pop(name, None)
            prefix = [tmux, '-S', str(root / 'tmux.sock'), '-f', '/dev/null']

            def terminal(*argv, check=True):
                result = subprocess.run([*prefix, *argv], env=env, text=True,
                    capture_output=True, timeout=10)
                if check:
                    assert result.returncode == 0, result.stderr
                return result.stdout

            def screen():
                return terminal('capture-pane', '-p', '-t', 'fixture:0.0')

            def calls():
                path = root / 'requests.jsonl'
                return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

            def wait_until(predicate, seconds, message):
                deadline = time.monotonic() + seconds
                while time.monotonic() < deadline:
                    if predicate():
                        return
                    time.sleep(.2)
                raise AssertionError(message)

            server_pid = None
            try:
                terminal('new-session', '-d', '-s', 'fixture', '-x', '180', '-y', '42', str(binary), 'ui')
                server_pid = int(terminal('display-message', '-p', '#{pid}').strip())
                picker_pid = int(terminal('display-message', '-p', '-t', 'fixture:0.0', '#{pane_pid}').strip())
                wait_until(lambda: 'usage lookup limited' in screen(), 20,
                           'limited lookup was not rendered')
                assert len(calls()) == 1, f'first throttled read made {len(calls())} requests'
                assert 'as of' in screen(), 'cached reading age was hidden'
                assert 'login expired' not in screen()

                def quota(_):
                    result = subprocess.run([binary, 'quota', '--json'], env=env,
                        cwd=root, text=True, capture_output=True, timeout=25)
                    assert result.returncode == 0, result.stderr
                    row = json.loads(result.stdout)['accounts'][0]
                    assert row['status'] == 'throttled'

                with ThreadPoolExecutor(max_workers=2) as pool:
                    list(pool.map(quota, range(2)))
                assert len(calls()) == 1, 'other processes bypassed the persisted deadline'
                assert credentials.read_bytes() == credential_before
                assert cache.read_bytes() == cache_before, 'deferred read restamped the cache'

                states = []
                for path in store.rglob('*.json'):
                    value = json.loads(path.read_text())
                    if isinstance(value, dict) and {'version', 'failures', 'throttled_at', 'retry_at'} <= value.keys():
                        states.append((path, value))
                assert len(states) == 1, 'expected one persisted retry deadline'
                state_path, state = states[0]
                assert state['failures'] == 1
                assert state['retry_at'] - state['throttled_at'] == 60
                assert state_path.stat().st_mode & 0o777 == 0o600
                # Advance only this fixture's cooldown; do not wait a real minute.
                state['retry_at'] = int(time.time()) - 1
                state['throttled_at'] = state['retry_at'] - 60
                state_path.write_text(json.dumps(state))
                (root / 'reply').write_text('200')

                def recovered():
                    latest = json.loads(cache.read_text())['fixture']
                    return latest.get('five_h') == 27 and latest['at'] >= started and 'usage lookup limited' not in screen()

                wait_until(recovered, 60, 'the open picker did not recover on its automatic refresh')
                assert int(terminal('display-message', '-p', '-t', 'fixture:0.0', '#{pane_pid}').strip()) == picker_pid
                assert [request['status'] for request in calls()] == [429, 200]
                assert credentials.read_bytes() == credential_before
                assert not state_path.exists(), 'successful response did not reset backoff history'
                report.update(result='PASS', same_picker_process=True,
                    initial_http_attempts=1, requests_during_cooldown=0,
                    credentials_unchanged=True, cached_observation_preserved=True,
                    automatic_recovery=True, final_http_statuses=[429, 200],
                    private_state=True)
                terminal('send-keys', '-t', 'fixture:0.0', 'q')
            finally:
                terminal('kill-server', check=False)
                if server_pid is not None:
                    for _ in range(40):
                        try:
                            os.kill(server_pid, 0)
                        except ProcessLookupError:
                            break
                        time.sleep(.05)
    except Exception as error:
        report['error'] = str(error)
        raise
    finally:
        if args.output:
            args.output.write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps(report), flush=True)


if __name__ == '__main__':
    main()
