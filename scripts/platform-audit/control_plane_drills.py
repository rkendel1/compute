#!/usr/bin/env python3
"""Control-plane availability and remote-operation drills, for docs/platform-audit.md.

Runs the real `compute` release binary against a real `feltdb-server`
(FELTDB_SERVER_BIN) and records what happens when parts of the control plane
fail, and what an operator can do through the remote API alone.

    FELTDB_SERVER_BIN=/path/to/feltdb-server \\
      python3 scripts/platform-audit/control_plane_drills.py --out docs/platform-audit-evidence/control-plane-drills.json
"""

import argparse
import json
import os
import re
import shutil
import signal
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
COMPUTE = os.path.join(ROOT, "target", "release", "compute")
MASTER_KEY = "platform-audit-drill"

SERVICE = """import http.server, os
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = ("%s pid=%d" % (os.environ.get("REVISION", "?"), os.getpid())).encode()
        self.send_response(200); self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)
    def log_message(self, *a): pass
http.server.ThreadingHTTPServer(("127.0.0.1", int(os.environ["PORT"])), H).serve_forever()
"""


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def http_get(port):
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=3) as sock:
            sock.sendall(b"GET / HTTP/1.0\r\n\r\n")
            data = b""
            while chunk := sock.recv(4096):
                data += chunk
        return data.partition(b"\r\n\r\n")[2].decode() or None
    except OSError:
        return None


class Api:
    def __init__(self, endpoint, token=None):
        self.endpoint, self.token = endpoint, token

    def call(self, method, path, body=None, token=True, timeout=120):
        headers = {"content-type": "application/json"}
        if token and self.token:
            headers["authorization"] = f"Bearer {self.token}"
        request = urllib.request.Request(self.endpoint + path, method=method, headers=headers,
                                         data=None if body is None else json.dumps(body).encode())
        started = time.perf_counter()
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                return response.status, json.loads(response.read() or b"null"), (time.perf_counter() - started) * 1000
        except urllib.error.HTTPError as error:
            try:
                value = json.loads(error.read() or b"null")
            except json.JSONDecodeError:
                value = None
            return error.code, value, (time.perf_counter() - started) * 1000
        except (urllib.error.URLError, OSError) as error:
            return None, str(error), (time.perf_counter() - started) * 1000


class FeltDb:
    def __init__(self, data):
        self.data = data
        self.binary = os.environ["FELTDB_SERVER_BIN"]
        self.port = free_port()
        self.url = f"http://127.0.0.1:{self.port}"
        self.process = None

    def key(self):
        output = subprocess.run([self.binary, "keys", "create", "--keys", os.path.join(self.data, "keys.json"),
                                 "--name", "compute", "--namespace", "compute", "--scope",
                                 "state:read,state:write,events:read,application:read,application:write,"
                                 "application:revision:read,application:revision:create,application:revision:promote,"
                                 "application:environment:read,application:environment:write"],
                                capture_output=True, text=True, env={**os.environ, "FELTDB_MASTER_KEY": MASTER_KEY}, check=True)
        return next(word for word in output.stdout.split() if word.startswith("fdb_live_"))

    def start(self):
        self.process = subprocess.Popen(
            [self.binary, "--host", "127.0.0.1", "--port", str(self.port), "--namespace", "compute", "--auth",
             "--data", os.path.join(self.data, "state.log"), "--keys", os.path.join(self.data, "keys.json"),
             "--audit", os.path.join(self.data, "audit.log")],
            env={**os.environ, "FELTDB_MASTER_KEY": MASTER_KEY}, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        started = time.perf_counter()
        for line in self.process.stdout:
            if "http://" in line:
                break
        return (time.perf_counter() - started) * 1000

    def kill(self):
        self.process.send_signal(signal.SIGKILL)
        self.process.wait()


class Daemon:
    def __init__(self, node, config, token_env):
        self.port = free_port()
        self.endpoint = f"http://127.0.0.1:{self.port}"
        self.node, self.config, self.token_env = node, config, token_env
        self.process = None

    def start(self, env):
        os.makedirs(self.node, exist_ok=True)
        self.log = open(os.path.join(self.node, "daemon.out"), "a")
        self.process = subprocess.Popen([COMPUTE, "start", "--listen", f"127.0.0.1:{self.port}", "--state-dir", self.node,
                                         "--config", self.config, "--reconcile-interval-ms", "500",
                                         "--require-token-env", self.token_env],
                                        env=env, stdout=self.log, stderr=self.log)
        started = time.perf_counter()
        api = Api(self.endpoint)
        while True:
            status, _, _ = api.call("GET", "/status", token=False, timeout=2)
            if status == 200:
                return (time.perf_counter() - started) * 1000
            if self.process.poll() is not None:
                return None
            time.sleep(0.02)

    def kill(self):
        self.process.send_signal(signal.SIGKILL)
        self.process.wait()

    def stop(self):
        if self.process and self.process.poll() is None:
            self.process.send_signal(signal.SIGINT)
            self.process.wait(30)


def bundle(directory, source):
    os.makedirs(directory, exist_ok=True)
    open(os.path.join(directory, "main.py"), "w").write(source)
    json.dump({"version": "1", "runtime": "python", "entrypoint": "main.py", "network": "network"},
              open(os.path.join(directory, "workload.json"), "w"))
    output = os.path.join(directory, "w.compute")
    subprocess.run([COMPUTE, "bundle", "create", "--workload", os.path.join(directory, "workload.json"), "--output", output, "--json"],
                   check=True, capture_output=True)
    return list(open(output, "rb").read())


def wait(predicate, seconds):
    deadline = time.time() + seconds
    while time.time() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.1)
    return None


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", required=True)
    args = parser.parse_args()
    work = tempfile.mkdtemp(prefix="compute-drills-")
    report = {"format": "compute.platform-audit.control-plane-drills@1",
              "measured_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
              "git": subprocess.run(["git", "-C", ROOT, "rev-parse", "--short", "HEAD"], capture_output=True, text=True).stdout.strip(),
              "drills": {}}
    drills = report["drills"]
    feltdb = FeltDb(os.path.join(work, "feltdb"))
    os.makedirs(feltdb.data)
    token = feltdb.key()
    drills["feltdb_start_ms"] = round(feltdb.start(), 1)
    env = {**os.environ, "COMPUTE_FELTDB_TOKEN": token, "COMPUTE_DAEMON_TOKEN": "drill-operator-token"}
    provision = subprocess.run([COMPUTE, "control-plane", "provision", "--feltdb-url", feltdb.url, "--json"],
                               capture_output=True, text=True, env=env)
    application = json.loads(provision.stdout)["application_id"]
    config = os.path.join(work, "compute.toml")
    open(config, "w").write(f'[state]\nbackend = "feltdb"\n\n[state.feltdb]\nurl = "{feltdb.url}"\napplication = "{application}"\n')
    daemon = Daemon(os.path.join(work, "node"), config, "COMPUTE_DAEMON_TOKEN")
    try:
        drills["daemon_start_on_feltdb_ms"] = round(daemon.start(env), 1)
        api = Api(daemon.endpoint, "drill-operator-token")
        anonymous = Api(daemon.endpoint)

        # ---- Remote operation, through the API alone ------------------------------------
        remote = {}
        latency = {}

        def op(name, method, path, body=None, expect=(200, 201)):
            status, value, ms = api.call(method, path, body)
            remote[name] = {"status": status, "ok": status in expect}
            latency.setdefault(name, []).append(round(ms, 1))
            if status not in expect:
                remote[name]["error"] = value
            return value

        for name in ("preprod", "production"):
            op(f"environment create {name}", "POST", "/environments", {"name": name})
        for label in ("v1", "v2"):
            op(f"revision register {label}", "POST", "/projects/site/revisions", {"revision": label, "workloads": [
                {"name": "web", "kind": "service", "ports": [{"name": "http", "port": 8080}],
                 "bundle": bundle(os.path.join(work, label), SERVICE)}]})
        created = op("deploy v1 to preprod", "POST", "/deployments",
                     {"project": "site", "environment": "preprod", "revision": "v1", "config": {"REVISION": "v1"}})
        wait(lambda: api.call("GET", f"/deployments/{created['deployment_id']}")[1]["status"] == "complete", 60)
        promoted = op("promote preprod → production", "POST", "/deployments/promote",
                      {"project": "site", "from": "preprod", "to": "production", "config": {"REVISION": "v1"}})
        wait(lambda: api.call("GET", f"/deployments/{promoted['deployment_id']}")[1]["status"] == "complete", 60)
        second = op("deploy v2 to production", "POST", "/deployments",
                    {"project": "site", "environment": "production", "revision": "v2", "config": {"REVISION": "v2"}})
        wait(lambda: api.call("GET", f"/deployments/{second['deployment_id']}")[1]["status"] == "complete", 60)
        back = op("rollback production", "POST", f"/deployments/{second['deployment_id']}/rollback")
        wait(lambda: api.call("GET", f"/deployments/{back['deployment_id']}")[1]["status"] == "complete", 60)
        op("inspect deployment", "GET", f"/deployments/{back['deployment_id']}")
        op("deployment receipt", "GET", f"/deployments/{second['deployment_id']}/receipt")
        op("project status", "GET", "/environments/production/projects/site/status")
        op("restart project", "POST", "/environments/production/projects/site/restart")
        op("stop workload", "POST", "/environments/production/projects/site/workloads/web/stop")
        op("start workload", "POST", "/environments/production/projects/site/workloads/web/start")
        op("logs", "GET", "/environments/production/projects/site/workloads/web/logs")
        op("events", "GET", "/events?limit=50")
        op("domain add (no ingress, dns none)", "POST", "/domains",
           {"name": "site.example.test", "environment": "production", "project": "site", "dns_provider": "none", "tls": False})
        op("domain inspect", "GET", "/domains/site.example.test")
        op("certificate status", "GET", "/certificates")
        op("dns status", "GET", "/dns")
        op("domain remove", "DELETE", "/domains/site.example.test")
        # Authorization and idempotency.
        status, _, _ = anonymous.call("POST", "/environments", {"name": "intruder"}, token=False)
        remote["mutation without token"] = {"status": status, "ok": status == 401}
        status, _, _ = anonymous.call("GET", "/environments", token=False)
        remote["read without token"] = {"status": status, "ok": status == 200,
                                        "note": "reads are open to anyone who can reach the API"}
        status, value, _ = api.call("POST", "/environments", {"name": "preprod"})
        remote["environment create repeated"] = {"status": status, "ok": status == 409, "kind": (value or {}).get("kind")}
        status, value, _ = api.call("POST", "/projects/site/revisions", {"revision": "v1", "workloads": [
            {"name": "web", "kind": "service", "ports": [{"name": "http", "port": 8080}], "bundle": bundle(os.path.join(work, "v1-again"), SERVICE)}]})
        remote["revision register repeated (same content)"] = {"status": status, "ok": status in (200, 201)}
        drills["remote_operation"] = remote
        drills["remote_operation_latency_ms"] = latency

        endpoint = api.call("GET", "/environments/production/projects/site")[1]["workloads"][0]["ports"][0]["host"]
        drills["serving_before_failures"] = http_get(endpoint)

        # ---- FeltDB dies while Compute runs ------------------------------------------------
        feltdb.kill()
        outage = {}
        status, value, _ = api.call("POST", "/environments", {"name": "during-outage"})
        outage["mutation during outage"] = {"status": status, "kind": (value or {}).get("kind") if isinstance(value, dict) else value}
        status, value, _ = api.call("GET", "/status", token=False)
        outage["status reports"] = {"state_available": (value or {}).get("state_available"), "state_error": ((value or {}).get("state_error") or "")[:160]}
        time.sleep(2)
        outage["service still answers"] = http_get(endpoint)
        outage["daemon still running"] = daemon.process.poll() is None
        drills["feltdb_outage"] = outage
        restart_ms = feltdb.start()
        recovered = wait(lambda: api.call("POST", "/environments", {"name": "after-outage"})[0] == 201, 60)
        drills["feltdb_restart"] = {"restart_ms": round(restart_ms, 1), "mutations_recovered": bool(recovered),
                                    "status_available": api.call("GET", "/status", token=False)[1].get("state_available")}

        # ---- Compute dies (SIGKILL) and restarts -------------------------------------------
        before = http_get(endpoint)
        daemon.kill()
        down = {"endpoint_while_daemon_down": http_get(endpoint)}
        started = time.perf_counter()
        restart_ms = daemon.start(env)
        answered = wait(lambda: http_get(endpoint), 60)
        down.update({
            "daemon_restart_to_api_ms": round(restart_ms, 1) if restart_ms else None,
            "endpoint_restored_ms": round((time.perf_counter() - started) * 1000, 1) if answered else None,
            "same_process_before": before, "after": answered,
            "revision_after": api.call("GET", "/environments/production/projects/site")[1].get("revision"),
        })
        drills["compute_sigkill"] = down

        # ---- Compute cannot start without FeltDB ------------------------------------------
        daemon.stop()
        feltdb.kill()
        refused = Daemon(os.path.join(work, "node-refused"), config, "COMPUTE_DAEMON_TOKEN")
        refused_ms = refused.start(env)
        drills["compute_start_without_feltdb"] = {
            "started": refused_ms is not None,
            "exit_code": refused.process.poll(),
            "log_tail": open(os.path.join(refused.node, "daemon.out")).read()[-240:],
        }
        if refused.process.poll() is None:
            refused.stop()

        # ---- A fresh node restores everything from FeltDB ---------------------------------
        feltdb.start()
        fresh = Daemon(os.path.join(work, "node-fresh"), config, "COMPUTE_DAEMON_TOKEN")
        fresh_ms = fresh.start(env)
        fresh_api = Api(fresh.endpoint, "drill-operator-token")
        project = wait(lambda: (lambda value: value if value and value.get("actual_state") == "running" else None)(
            fresh_api.call("GET", "/environments/production/projects/site")[1]), 60)
        drills["fresh_node_restore"] = {
            "start_ms": round(fresh_ms, 1) if fresh_ms else None,
            "project_running": bool(project),
            "revision": (project or {}).get("revision"),
            "environments": [item["name"] for item in fresh_api.call("GET", "/environments")[1]],
        }
        fresh.stop()
    finally:
        for process in (daemon.process, feltdb.process):
            if process and process.poll() is None:
                process.kill()
        shutil.rmtree(work, ignore_errors=True)
    os.makedirs(os.path.dirname(os.path.abspath(args.out)), exist_ok=True)
    json.dump(report, open(args.out, "w"), indent=1, sort_keys=True)
    print(json.dumps(report, indent=1)[:6000])


if __name__ == "__main__":
    main()
