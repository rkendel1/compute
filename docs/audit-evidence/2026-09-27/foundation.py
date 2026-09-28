"""The foundation re-audit: the security and recovery journeys of the
2026-09-27 audit, re-run against a clean `compute` launch after the target
boundary (G-ARCH-1) and target reality (G-ARCH-4) landed. Results go to
docs/audit-evidence/2026-09-27/foundation.json, which
generate_audit_json.py merges into audit.json as `experiments.foundation`.

Run from the repository root after `cargo build -p compute-cli`:

    python3 docs/audit-evidence/2026-09-27/foundation.py

It demonstrates the boundary rather than describing it: every probe is a
request an attacker or an outage would make, and what came back.
"""
import hashlib
import json
import os
import platform
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), '..', '..', '..'))
B = os.path.join(ROOT, 'target', 'debug', 'compute')
WORK = tempfile.mkdtemp(prefix='compute-foundation-')
HOME = os.path.join(WORK, 'home')
OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'foundation.json')


def fixture_catalog(directory):
    """Shell from the host /bin/sh, as the tests' fixture catalog: no
    runtime is downloaded."""
    lock = json.load(open(os.path.join(ROOT, 'distribution', 'runtime-lock.json')))
    shell = lock['runtimes']['shell']
    script = ('#!/bin/sh\ncase "$1" in --version|--help) echo "BusyBox v%s"; exit 0;; esac\n'
              'exec /bin/sh "$@"\n' % shell['version'])
    os.makedirs(directory, exist_ok=True)
    artifact = os.path.join(directory, 'shell-fixture')
    open(artifact, 'w').write(script)
    os.chmod(artifact, 0o755)
    arch = 'aarch64' if platform.machine() in ('arm64', 'aarch64') else 'x86_64'
    catalog = os.path.join(directory, 'runtime-catalog.json')
    json.dump({'schema_version': 2, 'runtimes': {
        'wasm': lock['runtimes']['wasm'], 'native': lock['runtimes']['native'],
        'shell': {'version': shell['version'], 'executable': shell['executable'], 'artifacts': {
            'linux-' + arch: {'url': 'file://' + artifact, 'sha256': hashlib.sha256(script.encode()).hexdigest(),
                              'format': 'file', 'install': [{'source': 'artifact', 'destination': shell['executable']}]}}}}},
              open(catalog, 'w'))
    return catalog


env = dict(os.environ, COMPUTE_HOME=HOME, COMPUTE_LISTEN='127.0.0.1:18797',
           COMPUTE_TARGET_LISTEN='127.0.0.1:18798', COMPUTE_NO_BROWSER='1',
           COMPUTE_DAEMON='http://127.0.0.1:18797',
           COMPUTE_RUNTIME_CATALOG=fixture_catalog(os.path.join(WORK, 'catalog')),
           COMPUTE_RUNTIME_STORE=os.path.join(WORK, 'runtimes'))
for name in ('COMPUTE_DAEMON_TOKEN', 'COMPUTE_CONFIG'):
    env.pop(name, None)
D = 'http://127.0.0.1:18797'
T = 'http://127.0.0.1:18798'
P = {'X-Compute-Protocol': 'compute.remote@1'}
R = {}


def api(method, path, body=None, base=D, headers=None):
    h = {'Content-Type': 'application/json', **(headers or {})}
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(base + path, method=method, headers=h, data=data)
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return r.status, json.loads(r.read() or b'null')
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b'null')
    except Exception as e:  # connection refused, …
        return 0, str(e)


def compute(*args, check=True):
    out = subprocess.run([B, *args], env=env, capture_output=True, text=True)
    if check and out.returncode != 0:
        sys.exit(f'compute {args}: {out.stderr}{out.stdout}')
    return out


def until(what, f, timeout=120):
    t = time.time()
    while True:
        v = f()
        if v:
            return v, round(time.time() - t, 2)
        if time.time() - t > timeout:
            R['timeout'] = what
            dump()
            sys.exit(f'timeout: {what}')
        time.sleep(0.1)


def computer(name='probe'):
    s, v = api('GET', f'/environments/{name}/computer')
    return v if s == 200 else {}


def observed(value):
    return lambda: (lambda c: c if c.get('reality', {}).get('observed') == value else None)(computer())


def reality(c):
    return {'status': c.get('status'), 'reality': c.get('reality'),
            'failure': {k: (c.get('failure') or {}).get(k) for k in ('code', 'retryable')} if c.get('failure') else None}


def kill_host():
    pid = int(open(os.path.join(HOME, 'computers', 'host.pid')).read())
    os.kill(pid, 9)
    until('host gone', lambda: api('GET', '/compute/health', base=T, headers=P)[0] == 0, timeout=20)


def dump():
    json.dump(R, open(OUT, 'w'), indent=1)


compute('down', check=False)

# 1. A target that trusts nobody does not start.
out = compute('serve', '--listen', '127.0.0.1:0', '--credentials', os.path.join(WORK, 'none.json'), check=False)
R['serve_without_credentials'] = {'exit': out.returncode, 'stderr': out.stderr.strip()}

# 2. Launch: the host trusts this control plane, and says how.
out = compute()
R['launch_output'] = out.stdout.strip().splitlines()
pool = open(os.path.join(HOME, 'pool.toml')).read()
token_file = os.path.join(HOME, 'control-plane', 'targets', 'this-machine.token')
token = open(token_file).read().strip()
trust = json.load(open(os.path.join(HOME, 'computers', 'credentials.json')))
R['launcher_credential'] = {
    'pool_names_token_file': 'token_file' in pool,
    'pool_holds_token': token in pool,
    'trust_file_holds_secret': token.rsplit('_', 1)[1] in json.dumps(trust),
    'trusted_control_planes': [c['control_plane'] for c in trust['credentials']],
    'token_file_mode': oct(os.stat(token_file).st_mode & 0o777),
}
_, targets = api('GET', '/targets')
R['targets'] = [{k: x.get(k) for k in ['target_id', 'health', 'hosts_computers', 'authentication', 'credential']}
                for x in targets]
_, info = api('GET', '/info')
R['control_state'] = {'kind': info['control_plane']['state']['kind'], 'durability': info['control_plane']['durability']}

# 3. A computer, running, confirmed by its target.
api('POST', '/environments', {'name': 'probe', 'computer': {
    'lifecycle': 'persistent', 'requirements': {'cpu_count': 1, 'memory_bytes': 1 << 30}}})
v, sec = until('running', observed('running'))
session = v['session_id']
R['computer_running'] = {'seconds': sec, **reality(v)}

# 4. The target, called directly: no credential, a wrong one, another
#    control plane's, a revoked one.
bearer = lambda t: {**P, 'Authorization': f'Bearer {t}'}
s, body = api('GET', '/compute/sessions', base=T, headers=P)
R['target_without_credential'] = {'list_http': s, 'kind': body.get('kind') if isinstance(body, dict) else body}
s, body = api('POST', f'/compute/sessions/{session}/exec', {'command': ['sh', '-c', 'echo bypass > bypass.txt']},
              base=T, headers=P)
R['target_exec_without_credential'] = {'http': s, 'kind': body.get('kind') if isinstance(body, dict) else body}
wrong = token[:-1] + ('1' if token.endswith('0') else '0')
s, body = api('GET', '/compute/sessions', base=T, headers=bearer(wrong))
R['target_wrong_credential'] = {'http': s, 'kind': body.get('kind') if isinstance(body, dict) else body}
issued = json.loads(compute('target', 'credential', 'issue', '--credentials',
                            os.path.join(HOME, 'computers', 'credentials.json'),
                            '--control-plane', 'intruder', '--json').stdout)
s, listed = api('GET', '/compute/sessions', base=T, headers=bearer(issued['token']))
s2, inspect = api('GET', f'/compute/sessions/{session}', base=T, headers=bearer(issued['token']))
s3, execd = api('POST', f'/compute/sessions/{session}/exec', {'command': ['true']}, base=T,
                headers=bearer(issued['token']))
R['target_other_control_plane'] = {
    'list_http': s, 'sessions_seen': len(listed) if isinstance(listed, list) else listed,
    'inspect': [s2, inspect.get('kind') if isinstance(inspect, dict) else inspect],
    'exec': [s3, execd.get('kind') if isinstance(execd, dict) else execd]}
compute('target', 'credential', 'revoke', issued['credential_id'], '--credentials',
        os.path.join(HOME, 'computers', 'credentials.json'))
s, body = api('GET', '/compute/sessions', base=T, headers=bearer(issued['token']))
R['target_revoked_credential'] = {'http': s, 'message': body.get('message') if isinstance(body, dict) else body}
s, own = api('GET', f'/compute/sessions/{session}', base=T, headers=bearer(token))
R['target_own_credential'] = {'http': s, 'owner': own.get('owner') if isinstance(own, dict) else own}

# 5. The target goes away: unreachable, still wanted.
kill_host()
v, sec = until('unreachable', observed('unreachable'), timeout=60)
s, body = api('POST', '/environments/probe/exec', {'command': ['true']})
R['target_down'] = {'seconds_to_unreachable': sec, **reality(v),
                    'exec': {'http': s, 'kind': body.get('kind') if isinstance(body, dict) else body}}

# 6. It comes back (`compute` restarts the host): the same machine.
compute()
v, sec = until('recovered', observed('running'), timeout=90)
R['target_recovered'] = {'seconds': sec, 'same_session': v['session_id'] == session, **reality(v)}

# 7. It comes back without its sessions: lost, and it stays lost.
kill_host()
until('unreachable again', observed('unreachable'), timeout=60)
shutil.rmtree(os.path.join(HOME, 'computers', 'sessions'), ignore_errors=True)
compute()
v, sec = until('lost', observed('lost'), timeout=90)
R['machine_lost'] = {'seconds_after_target_back': sec, **reality(v)}
time.sleep(5)
s, reconciled = api('POST', '/environments/probe/reconcile')
R['lost_after_reconcile'] = reality(reconciled) if isinstance(reconciled, dict) else reconciled
compute('stop')
until('control plane stopped', lambda: api('GET', '/status')[0] == 0, timeout=30)
compute()
v, _ = until('control plane back', lambda: computer() or None, timeout=60)
R['lost_after_control_plane_restart'] = reality(v)

# 8. Replacement: a new machine for the same environment.
api('POST', '/environments/probe/replace', {'cpu_count': 1, 'memory_bytes': 1 << 30})
v, sec = until('replaced', lambda: (lambda c: c if c.get('reality', {}).get('observed') == 'running'
                                    and c.get('session_id') != session else None)(computer()), timeout=90)
R['replaced'] = {'seconds': sec, 'new_session': v['session_id'] != session, **reality(v)}
s, events = api('GET', '/events?environment=probe&limit=1000')
R['events'] = [e['kind'] for e in events if e['kind'].startswith('computer.')] if isinstance(events, list) else events

dump()
print(json.dumps(R, indent=1))
compute('down', check=False)
shutil.rmtree(WORK, ignore_errors=True)
