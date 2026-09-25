"""Control-plane latency on a FeltDB backend: reads and mutations through the
Compute API, against a real feltdb-server on this host (so network latency to
a remote FeltDB is excluded; add it separately).

    FELTDB_SERVER_BIN=/path/to/feltdb-server \\
      python3 scripts/platform-audit/feltdb_latency.py --out latency.json

Every figure is measured, never estimated: p50/p95/p99 over `--samples`
requests each. It also counts the FeltDB requests each API call causes, from
feltdb-server's own audit log.
"""

import argparse
import json
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from control_plane_drills import COMPUTE, Api, FeltDb, bundle, free_port  # noqa: E402

SERVICE = """import http.server, os
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = os.environ.get("REVISION", "?").encode()
        self.send_response(200); self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)
    def log_message(self, *a): pass
http.server.ThreadingHTTPServer(("127.0.0.1", int(os.environ["PORT"])), H).serve_forever()
"""


def stats(values):
    values = sorted(values)
    pick = lambda q: values[min(len(values) - 1, max(0, int(round(q * (len(values) - 1)))))]
    return {"n": len(values), "p50": round(statistics.median(values), 2), "p95": round(pick(0.95), 2),
            "p99": round(pick(0.99), 2), "max": round(values[-1], 2)}


def audit_lines(path):
    try:
        with open(path) as handle:
            return sum(1 for _ in handle)
    except OSError:
        return 0


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", required=True)
    parser.add_argument("--samples", type=int, default=40)
    parser.add_argument("--compute", default=COMPUTE)
    args = parser.parse_args()
    work = tempfile.mkdtemp(prefix="compute-feltdb-latency-")
    feltdb = FeltDb(os.path.join(work, "feltdb"))
    os.makedirs(feltdb.data)
    token = feltdb.key()
    feltdb.start()
    env = {**os.environ, "COMPUTE_FELTDB_TOKEN": token}
    provision = subprocess.run([args.compute, "control-plane", "provision", "--feltdb-url", feltdb.url, "--json"],
                               capture_output=True, text=True, env=env, check=True)
    application = json.loads(provision.stdout)["application_id"]
    config = os.path.join(work, "compute.toml")
    with open(config, "w") as handle:
        handle.write(f'[state]\nbackend = "feltdb"\n\n[state.feltdb]\nurl = "{feltdb.url}"\napplication = "{application}"\n')
    port = free_port()
    node = os.path.join(work, "node")
    daemon = subprocess.Popen([args.compute, "start", "--listen", f"127.0.0.1:{port}", "--state-dir", node,
                               "--config", config, "--port-range", "28000-28099",
                               "--instance-port-range", "48000-48099"],
                              env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    api = Api(f"http://127.0.0.1:{port}")
    report = {"format": "compute.feltdb-latency@1",
              "measured_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
              "git": subprocess.run(["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True).stdout.strip(),
              "backend": "feltdb-server on this host", "samples": args.samples, "reads": {}, "mutations": {},
              "feltdb_requests_per_call": {}}
    audit = os.path.join(feltdb.data, "audit.log")
    try:
        for _ in range(500):
            if api.call("GET", "/health", timeout=2)[0] == 200:
                break
            time.sleep(0.05)
        # A realistic small control plane: 3 environments, a service project.
        source = os.path.join(work, "app")
        bundle(os.path.join(source, "api"), SERVICE)
        with open(os.path.join(source, "api", "workload.json"), "w") as handle:
            json.dump({"version": "1", "runtime": "python", "entrypoint": "main.py", "network": "network"}, handle)
        with open(os.path.join(source, "compute.project.toml"), "w") as handle:
            handle.write('[project]\nname = "app"\n\n[[workload]]\nname = "api"\nkind = "service"\n'
                         'workload = "api/workload.json"\nports = [{ name = "http", port = 8000 }]\n')
        for name in ["preprod", "staging", "production"]:
            api.call("POST", "/environments", {"name": name})
        subprocess.run([args.compute, "deploy", "app", "--environment", "production", "--source", source,
                        "--revision", "v1", "--set", "REVISION=v1", "--wait", "--daemon", f"http://127.0.0.1:{port}"],
                       env=env, check=True, capture_output=True)

        def measure(group, name, method, path, body=None, samples=args.samples):
            times = []
            before = audit_lines(audit)
            for index in range(samples):
                request_body = body(index) if callable(body) else body
                status, value, ms = api.call(method, path(index) if callable(path) else path, request_body)
                assert status and status < 300, (name, status, value)
                times.append(ms)
            report[group][name] = stats(times)
            report["feltdb_requests_per_call"][name] = round((audit_lines(audit) - before) / samples, 1)

        measure("reads", "status", "GET", "/status")
        measure("reads", "environments list", "GET", "/environments")
        measure("reads", "environment inspect", "GET", "/environments/production")
        measure("reads", "project status", "GET", "/environments/production/projects/app")
        measure("reads", "projects list", "GET", "/projects")
        measure("reads", "deployments list", "GET", "/deployments?limit=20")
        measure("reads", "events", "GET", "/events?limit=50")
        measure("reads", "network", "GET", "/network")
        measure("mutations", "environment create", "POST", "/environments",
                lambda index: {"name": f"bench-{index}"})
        measure("mutations", "environment stop", "POST", lambda index: f"/environments/bench-{index}/stop")
        measure("mutations", "environment start", "POST", lambda index: f"/environments/bench-{index}/start")
        measure("mutations", "project restart", "POST", "/environments/production/projects/app/restart",
                samples=max(5, args.samples // 8))
    finally:
        daemon.send_signal(2)
        try:
            daemon.wait(60)
        except subprocess.TimeoutExpired:
            daemon.kill()
        feltdb.kill()
        shutil.rmtree(work, ignore_errors=True)
    with open(args.out, "w") as handle:
        json.dump(report, handle, indent=2)
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
