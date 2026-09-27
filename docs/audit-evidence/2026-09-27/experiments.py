"""Audit experiments against a clean `compute` launch: timings, the target
bypass, target loss, machine loss, daemon restart. Results go to
docs/audit-evidence/2026-09-27/experiments.json."""
import glob
import json
import os
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.request

B = '/home/user/compute/target/debug/compute'
HOME = '/tmp/claude-0/aud/home'
shutil.rmtree(HOME, ignore_errors=True)
CAT = sorted(glob.glob('/tmp/.tmp*/runtime-catalog.json'))[0]
env = dict(os.environ, COMPUTE_HOME=HOME, COMPUTE_LISTEN='127.0.0.1:18787',
           COMPUTE_TARGET_LISTEN='127.0.0.1:18788', COMPUTE_NO_BROWSER='1',
           COMPUTE_RUNTIME_CATALOG=CAT, COMPUTE_RUNTIME_STORE='/tmp/claude-0/aud/rs')
env.pop('COMPUTE_DAEMON_TOKEN', None)
D = 'http://127.0.0.1:18787'
T = 'http://127.0.0.1:18788'
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


def until(what, f, timeout=120):
    t = time.time()
    while True:
        v = f()
        if v:
            return v, time.time() - t
        if time.time() - t > timeout:
            R['timeout'] = what
            dump()
            sys.exit(f'timeout: {what}')
        time.sleep(0.1)


def computer(name):
    s, v = api('GET', f'/environments/{name}/computer')
    return v if s == 200 else {}


def job(env_name, job_id):
    s, v = api('GET', f'/environments/{env_name}/jobs/{job_id}')
    return v if s == 200 and v.get('result') else None


def dump():
    os.makedirs('/tmp/claude-0/aud', exist_ok=True)
    json.dump(R, open('/tmp/claude-0/aud/results.json', 'w'), indent=1)


subprocess.run([B, 'down'], env=env, capture_output=True)

# 1. First launch, relaunch, first requests.
t = time.time()
out = subprocess.run([B], env=env, capture_output=True, text=True)
R['launch_seconds'] = round(time.time() - t, 2)
R['launch_output'] = out.stdout.strip().splitlines()
t = time.time()
subprocess.run([B], env=env, capture_output=True, text=True)
R['relaunch_seconds'] = round(time.time() - t, 2)
t = time.time()
urllib.request.urlopen(D + '/').read()
R['ui_html_ms'] = round((time.time() - t) * 1000, 1)
t = time.time()
api('GET', '/status')
R['status_ms'] = round((time.time() - t) * 1000, 1)
_, targets = api('GET', '/targets')
R['targets'] = [{k: x.get(k) for k in ['target_id', 'kind', 'health', 'hosts_computers', 'features']} for x in targets]

# 2. A computer from nothing.
t = time.time()
s, _ = api('POST', '/environments', {'name': 'probe', 'computer': {
    'lifecycle': 'persistent', 'requirements': {'cpu_count': 1, 'memory_bytes': 1 << 30}}})
R['create_http'] = [s, round((time.time() - t) * 1000, 1)]
v, sec = until('running', lambda: (lambda c: c if c.get('status') == 'running' else None)(computer('probe')))
R['computer_running_seconds'] = round(sec, 2)
session = v['machine']['session_id']

# 3. A command's latency.
t = time.time()
s, ex = api('POST', '/environments/probe/exec', {'command': ['echo', 'hi']})
R['exec_submit_ms'] = round((time.time() - t) * 1000, 1)
_, sec = until('job', lambda: job('probe', ex['job_id']))
R['exec_roundtrip_seconds'] = round(sec + R['exec_submit_ms'] / 1000, 2)

# 4. The target, called directly, with no credential at all.
P = {'X-Compute-Protocol': 'compute.remote@1'}
s, sessions = api('GET', '/compute/sessions', base=T, headers=P)
R['target_sessions_listed_without_credential'] = {
    'http': s, 'sessions': [x['session_id'] for x in sessions] if isinstance(sessions, list) else sessions}
s, direct = api('POST', f'/compute/sessions/{session}/exec',
                {'command': ['sh', '-c', 'echo written-by-an-unauthenticated-caller > bypass.txt']},
                base=T, headers=P)
R['target_exec_without_credential'] = {'http': s, 'body': direct if s >= 300 else 'accepted'}
if 200 <= s < 300:
    time.sleep(2)
    _, j = api('POST', '/environments/probe/exec', {'command': ['cat', 'bypass.txt']})
    jr, _ = until('job2', lambda: job('probe', j['job_id']))
    R['bypass_file_seen_through_the_daemon'] = jr['result']['result']['stdout']['text'].strip()
s, _ = api('POST', '/environments/probe/exec', {'command': ['true']}, headers={'Authorization': 'Bearer anything'})
R['daemon_loopback_any_bearer_token_http'] = s

# 5. Inspecting a project.
repo = '/tmp/claude-0/aud/repo'
shutil.rmtree(repo, ignore_errors=True)
os.makedirs(repo)
open(repo + '/package.json', 'w').write('{"scripts":{"build":"true","test":"true","start":"node index.js"}}')
open(repo + '/index.js', 'w').write('require("http").createServer((q,s)=>s.end("ok")).listen(process.env.PORT)')
subprocess.run(['sh', '-c', f'cd {repo} && git init -q -b main && git add . && '
                'git -c user.name=a -c user.email=a@b commit -qm one'], check=True)
t = time.time()
s, prop = api('POST', '/environments/probe/propose', {'url': repo})
R['propose'] = {'http': s, 'seconds': round(time.time() - t, 2),
                'runtime': prop.get('runtime') if isinstance(prop, dict) else prop,
                'start': [p['command'] for p in prop['assembly'].get('processes', [])] if isinstance(prop, dict) else None}

# 6. The target disappears, then comes back.
pid = int(open(HOME + '/computers/host.pid').read())
os.kill(pid, 15)
time.sleep(1)
s, body = api('POST', '/environments/probe/exec', {'command': ['true']})
R['exec_while_target_down'] = {'http': s, 'body': body}
time.sleep(5)
c = computer('probe')
R['computer_while_target_down'] = {'status': c.get('status'), 'failure': c.get('failure')}
subprocess.run([B], env=env, capture_output=True)
v3, sec = until('recovered', lambda: (lambda c: c if c.get('status') == 'running' and c.get('converged')
                                      and not c.get('failure') else None)(computer('probe')), timeout=90)
R['recovery_after_target_restart'] = {'seconds': round(sec, 2),
                                      'same_session': v3['machine']['session_id'] == session}

# 7. The machine disappears: the target forgets the session (its store is
#    wiped while it is down).
pid = int(open(HOME + '/computers/host.pid').read())
os.kill(pid, 15)
time.sleep(1)
shutil.rmtree(HOME + '/computers/sessions', ignore_errors=True)
subprocess.run([B], env=env, capture_output=True)
v4, sec = until('loss noticed', lambda: (lambda c: c if c.get('status') != 'running' or c.get('failure')
                                         else None)(computer('probe')), timeout=90)
R['after_the_target_forgot_the_machine'] = {'status': v4.get('status'), 'failure': v4.get('failure'),
                                            'seconds': round(sec, 2)}

# 8. The control plane restarts; state is kept.
subprocess.run([B, 'stop'], env=dict(env, COMPUTE_DAEMON=D), capture_output=True)
time.sleep(1)
subprocess.run([B], env=env, capture_output=True)
s, v5 = api('GET', '/environments/probe/computer')
R['after_control_plane_restart'] = {'http': s, 'status': v5.get('status') if isinstance(v5, dict) else v5,
                                    'failure': v5.get('failure') if isinstance(v5, dict) else None}
dump()
print(json.dumps(R, indent=1))
subprocess.run([B, 'down'], env=env, capture_output=True)
