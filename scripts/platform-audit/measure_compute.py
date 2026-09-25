#!/usr/bin/env python3
"""Measure Compute on the machine this runs on, for docs/platform-audit.md.

Every number here is observed, never extrapolated. Run from the repository
root after `cargo build --release -p compute-cli`:

    python3 scripts/platform-audit/measure_compute.py --out docs/platform-audit-evidence/compute-measurements.json

Sections can be selected with --only runtimes,throughput,daemon,api,release,remote,capacity.
"""

import argparse
import concurrent.futures
import json
import os
import platform
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
COMPUTE = os.path.join(ROOT, "target", "release", "compute")

# A minimal WASI module whose _start returns immediately.
WASM = bytes([
    0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00, 0x01, 0x04, 0x01, 0x60, 0x00, 0x00,
    0x03, 0x02, 0x01, 0x00, 0x07, 0x0A, 0x01, 0x06, 0x5F, 0x73, 0x74, 0x61, 0x72, 0x74,
    0x00, 0x00, 0x0A, 0x04, 0x01, 0x02, 0x00, 0x0B,
])

HELLO = {
    "python": ("main.py", 'print("ok")\n'),
    "node": ("main.js", 'console.log("ok")\n'),
    "bun": ("main.ts", 'console.log("ok")\n'),
    "ruby": ("main.rb", 'puts "ok"\n'),
    "php": ("main.php", '<?php echo "ok\\n";\n'),
    "shell": ("main.sh", 'echo ok\n'),
    "deno": ("main.ts", 'console.log("ok")\n'),
}

HTTP_SERVICE = """import http.server, os
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b"{version}"
        self.send_response(200); self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)
    def log_message(self, *a): pass
class S(http.server.ThreadingHTTPServer):
    daemon_threads = True
{prelude}
S(("127.0.0.1", int(os.environ["PORT"])), H).serve_forever()
"""


def percentiles(samples):
    if not samples:
        return None
    ordered = sorted(samples)

    def pick(fraction):
        index = min(len(ordered) - 1, max(0, round(fraction * (len(ordered) - 1))))
        return round(ordered[index], 2)

    return {
        "n": len(ordered),
        "min": round(ordered[0], 2),
        "p50": pick(0.50),
        "p95": pick(0.95),
        "p99": pick(0.99),
        "max": round(ordered[-1], 2),
        "mean": round(statistics.mean(ordered), 2),
    }


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def rss_kib(pid):
    try:
        with open(f"/proc/{pid}/status") as status:
            for line in status:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1])
    except OSError:
        return 0
    return 0


def descendants(pid):
    children = []
    try:
        output = subprocess.run(["ps", "-e", "-o", "pid=,ppid="], capture_output=True, text=True).stdout
    except OSError:
        return children
    parents = {}
    for line in output.splitlines():
        child, parent = line.split()
        parents.setdefault(int(parent), []).append(int(child))
    stack = [pid]
    while stack:
        for child in parents.get(stack.pop(), []):
            children.append(child)
            stack.append(child)
    return children


def cpu_seconds(pid):
    try:
        fields = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
        return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")
    except OSError:
        return 0.0


# ---- Workloads -------------------------------------------------------------------------

def fixture(directory, runtime):
    """A workload that prints ok; returns its workload.json path or None."""
    os.makedirs(directory, exist_ok=True)
    entry = None
    if runtime in HELLO:
        entry, source = HELLO[runtime]
        open(os.path.join(directory, entry), "w").write(source)
    elif runtime == "wasm":
        entry = "main.wasm"
        open(os.path.join(directory, entry), "wb").write(WASM)
    elif runtime == "jvm":
        if not shutil.which("javac") or not shutil.which("jar"):
            return None
        open(os.path.join(directory, "Main.java"), "w").write(
            'public class Main { public static void main(String[] a) { System.out.println("ok"); } }\n')
        subprocess.run(["javac", "Main.java"], cwd=directory, check=True)
        subprocess.run(["jar", "--create", "--file", "main.jar", "--main-class", "Main", "Main.class"], cwd=directory, check=True)
        entry = "main.jar"
    elif runtime == "native":
        compiler = shutil.which("cc") or shutil.which("gcc")
        if not compiler:
            return None
        open(os.path.join(directory, "main.c"), "w").write('#include <stdio.h>\nint main(void){puts("ok");return 0;}\n')
        subprocess.run([compiler, "-O2", "-o", "main", "main.c"], cwd=directory, check=True)
        entry = "main"
    else:
        return None
    spec = {"version": "1", "runtime": runtime, "entrypoint": entry,
            "network": "none" if runtime == "wasm" else "network"}
    path = os.path.join(directory, "workload.json")
    json.dump(spec, open(path, "w"))
    return path


def run_once(workload):
    started = time.perf_counter()
    result = subprocess.run([COMPUTE, "run", "--workload", workload, "--json"], capture_output=True, text=True)
    wall = (time.perf_counter() - started) * 1000
    try:
        document = json.loads(result.stdout)
    except json.JSONDecodeError:
        return wall, None, f"no JSON: {result.stderr.strip()[:200]}"
    ok = document.get("status") == "completed" and document.get("stdout", {}).get("text", "").strip() == "ok"
    if not ok and not (workload.endswith("wasm/workload.json") and document.get("status") == "completed"):
        return wall, document, f"status={document.get('status')} error={str(document.get('error'))[:200]}"
    return wall, document, None


def measure_runtimes(work):
    doctor = json.loads(subprocess.run([COMPUTE, "doctor", "--json"], capture_output=True, text=True).stdout)
    rows = []
    for entry in doctor["runtimes"]:
        availability = entry["availability"]
        isolation = entry["capabilities"]["isolation"]
        kind = availability["kind"]
        row = {
            "runtime": kind,
            "available": availability["available"],
            "version": (availability.get("version") or "").splitlines()[0] if availability.get("version") else None,
            "source": availability.get("source"),
            "memory_enforced": isolation["memory_enforcement"],
            "cpu_enforced": isolation["cpu_enforcement"],
            "filesystem_boundary": isolation["filesystem_boundary"],
            "network_boundary": isolation["network_boundary"],
        }
        if availability["available"]:
            workload = fixture(os.path.join(work, "runtimes", kind), kind)
            if workload is None:
                row["error"] = "no fixture toolchain on this host"
            else:
                first_wall, first_doc, error = run_once(workload)
                walls, engine = [], []
                for _ in range(20):
                    wall, document, error = run_once(workload)
                    if error:
                        break
                    walls.append(wall)
                    engine.append(float(document.get("duration") or 0))
                row.update({
                    "first_run_ms": round(first_wall, 2),
                    "cli_end_to_end_ms": percentiles(walls),
                    "engine_duration_ms": percentiles(engine),
                    "receipt": bool(first_doc and first_doc.get("receipt")),
                    "error": error,
                })
        rows.append(row)
        print(f"  runtime {kind}: {row.get('cli_end_to_end_ms', {}) and row['cli_end_to_end_ms']['p50'] if row.get('cli_end_to_end_ms') else row.get('error') or 'unavailable'}", flush=True)
    return rows


def measure_throughput(work, seconds):
    results = {}
    for kind in ("wasm", "shell", "python", "node"):
        workload = fixture(os.path.join(work, "throughput", kind), kind)
        if workload is None:
            continue
        for workers in (1, 4, 8):
            deadline = time.perf_counter() + seconds
            counts = {"ok": 0, "failed": 0}
            lock = threading.Lock()

            def loop():
                while time.perf_counter() < deadline:
                    _, _, error = run_once(workload)
                    with lock:
                        counts["failed" if error else "ok"] += 1

            started = time.perf_counter()
            threads = [threading.Thread(target=loop) for _ in range(workers)]
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join()
            elapsed = time.perf_counter() - started
            results[f"{kind}/{workers}"] = {
                "runtime": kind, "concurrency": workers, "seconds": round(elapsed, 2),
                "completed": counts["ok"], "failed": counts["failed"],
                "per_second": round(counts["ok"] / elapsed, 2),
            }
            print(f"  throughput {kind} x{workers}: {results[f'{kind}/{workers}']['per_second']}/s", flush=True)
    return results


# ---- The daemon ----------------------------------------------------------------------------

class Daemon:
    def __init__(self, work, name, extra=(), state=("--state", "memory")):
        self.port = free_port()
        self.endpoint = f"http://127.0.0.1:{self.port}"
        self.node = os.path.join(work, name)
        os.makedirs(self.node, exist_ok=True)
        self.log = open(os.path.join(self.node, "daemon.out"), "w")
        self.args = [COMPUTE, "start", "--listen", f"127.0.0.1:{self.port}", "--state-dir", self.node,
                     "--reconcile-interval-ms", "1000", *state, *extra]
        self.process = None

    def start(self):
        started = time.perf_counter()
        self.process = subprocess.Popen(self.args, stdout=self.log, stderr=self.log)
        while True:
            try:
                self.get("/status")
                return (time.perf_counter() - started) * 1000
            except (urllib.error.URLError, ConnectionError, OSError):
                if self.process.poll() is not None:
                    raise RuntimeError(open(self.log.name).read()[-2000:])
                time.sleep(0.005)

    def stop(self):
        if self.process and self.process.poll() is None:
            self.process.send_signal(signal.SIGINT)
            try:
                self.process.wait(30)
            except subprocess.TimeoutExpired:
                self.process.kill()

    def request(self, method, path, body=None, timeout=600):
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(self.endpoint + path, data=data, method=method,
                                         headers={"content-type": "application/json"})
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                return json.loads(response.read() or b"null")
        except urllib.error.HTTPError as error:
            raise RuntimeError(f"{method} {path}: {error.code} {error.read()[:300]!r}") from None

    def get(self, path):
        return self.request("GET", path, timeout=10)

    def timed(self, method, path, body=None):
        started = time.perf_counter()
        value = self.request(method, path, body)
        return (time.perf_counter() - started) * 1000, value


def bundle(directory, runtime, source_name, source, network="network"):
    os.makedirs(directory, exist_ok=True)
    open(os.path.join(directory, source_name), "wb" if isinstance(source, bytes) else "w").write(source)
    spec = os.path.join(directory, "workload.json")
    json.dump({"version": "1", "runtime": runtime, "entrypoint": source_name, "network": network}, open(spec, "w"))
    output = os.path.join(directory, "workload.compute")
    subprocess.run([COMPUTE, "bundle", "create", "--workload", spec, "--output", output, "--json"],
                   check=True, capture_output=True)
    return list(open(output, "rb").read())


def wait_release(daemon, deployment_id, timeout=300):
    deadline = time.time() + timeout
    while time.time() < deadline:
        view = daemon.get(f"/deployments/{deployment_id}")
        if view["status"] in ("complete", "failed", "rolled_back"):
            return view
        time.sleep(0.02)
    raise RuntimeError(f"{deployment_id} did not settle")


def release_phases(daemon, deployment_id):
    events = daemon.get(f"/events?deployment={deployment_id}&limit=1000")
    stamps = {}
    for event in events:
        stamps.setdefault(event["kind"], event["at"])

    def parse(value):
        from datetime import datetime
        return datetime.fromisoformat(value.replace("Z", "+00:00")[:26] + "+00:00" if "." in value else value.replace("Z", "+00:00"))

    order = ["deployment.started", "deployment.placed", "deployment.ready", "deployment.switched",
             "deployment.draining", "deployment.completed", "deployment.failed", "deployment.rolled_back"]
    present = [kind for kind in order if kind in stamps]
    phases = {}
    for before, after in zip(present, present[1:]):
        phases[f"{before} → {after}"] = round((parse(stamps[after]) - parse(stamps[before])).total_seconds() * 1000, 1)
    if present:
        phases["total"] = round((parse(stamps[present[-1]]) - parse(stamps[present[0]])).total_seconds() * 1000, 1)
    return phases


def measure_daemon(work):
    results = {}
    memory, file_state = [], []
    for index in range(10):
        daemon = Daemon(work, f"startup-memory-{index}")
        memory.append(daemon.start())
        if index == 0:
            results["idle_rss_kib"] = rss_kib(daemon.process.pid)
        daemon.stop()
    node = os.path.join(work, "startup-file")
    for index in range(10):
        daemon = Daemon(work, "startup-file", state=("--state", "file"))
        file_state.append(daemon.start())
        if index == 0:
            for name in range(20):
                daemon.request("POST", "/environments", {"name": f"env-{name}"})
        daemon.stop()
    results["startup_to_api_ms_memory_state"] = percentiles(memory)
    results["startup_to_api_ms_file_state_20_environments"] = percentiles(file_state)
    print(f"  daemon startup p50 {results['startup_to_api_ms_memory_state']['p50']} ms", flush=True)
    return results


def measure_api(work):
    daemon = Daemon(work, "api")
    daemon.start()
    results = {}
    try:
        creates = [daemon.timed("POST", "/environments", {"name": f"e{i}"})[0] for i in range(30)]
        results["environment_create_ms"] = percentiles(creates)
        reads = [daemon.timed("GET", "/environments")[0] for _ in range(50)]
        results["environment_list_ms_30_environments"] = percentiles(reads)
        wasm = bundle(os.path.join(work, "api-wasm"), "wasm", "main.wasm", WASM, network="none")
        python = bundle(os.path.join(work, "api-python"), "python", "main.py", 'print("ok")\n')
        added_ms, _ = daemon.timed("POST", "/environments/e0/projects", {
            "name": "tasks", "revision": "r1",
            "workloads": [
                {"name": "wasm", "kind": "task", "bundle": wasm},
                {"name": "py", "kind": "task", "bundle": python},
            ]})
        results["project_add_task_only_ms"] = round(added_ms, 2)
        for task in ("wasm", "py"):
            samples, receipts = [], 0
            for _ in range(40):
                ms, execution = daemon.timed("POST", f"/environments/e0/projects/tasks/workloads/{task}/run")
                samples.append(ms)
                receipts += bool(execution.get("receipt_id"))
            results[f"api_task_run_ms_{task}"] = percentiles(samples)
            results[f"api_task_run_receipts_{task}"] = f"{receipts}/40"
            print(f"  api task {task} p50 {results[f'api_task_run_ms_{task}']['p50']} ms", flush=True)
        # Concurrent task runs through the API.
        # The same task, eight callers at once.
        def one(_):
            try:
                return daemon.timed("POST", "/environments/e0/projects/tasks/workloads/py/run")[0], None
            except RuntimeError as error:
                return None, str(error)[-120:]
        started = time.perf_counter()
        with concurrent.futures.ThreadPoolExecutor(8) as pool:
            outcomes = list(pool.map(one, range(80)))
        elapsed = time.perf_counter() - started
        failures = [error for _, error in outcomes if error]
        results["api_same_task_concurrency_8"] = {
            "latency_ms": percentiles([ms for ms, error in outcomes if ms is not None]),
            "succeeded": 80 - len(failures), "failed": len(failures),
            "failure_example": failures[0] if failures else None,
            "per_second": round((80 - len(failures)) / elapsed, 2)}
    finally:
        daemon.stop()
    return results


def http_get(port, timeout=5):
    with socket.create_connection(("127.0.0.1", port), timeout=timeout) as sock:
        sock.sendall(b"GET / HTTP/1.0\r\n\r\n")
        data = b""
        while True:
            chunk = sock.recv(4096)
            if not chunk:
                break
            data += chunk
    head, _, body = data.partition(b"\r\n\r\n")
    if not head.startswith(b"HTTP/1.0 200"):
        raise RuntimeError(head[:60])
    return body.decode()


def measure_release(work):
    daemon = Daemon(work, "release", extra=("--drain-timeout-ms", "5000"))
    daemon.start()
    results = {}
    try:
        daemon.request("POST", "/environments", {"name": "production"})

        def revision(label, prelude="", readiness=None):
            workload = {"name": "web", "kind": "service", "ports": [{"name": "http", "port": 8080}],
                        "bundle": bundle(os.path.join(work, f"release-{label}"), "python", "main.py",
                                         HTTP_SERVICE.format(version=label, prelude=prelude))}
            if readiness:
                workload["readiness"] = readiness
            daemon.request("POST", "/projects/site/revisions", {"revision": label, "workloads": [workload]})

        def deploy(label):
            started = time.perf_counter()
            created = daemon.request("POST", "/deployments", {"project": "site", "environment": "production", "revision": label})
            settled = wait_release(daemon, created["deployment_id"])
            return (time.perf_counter() - started) * 1000, settled

        revision("v1")
        first_ms, first = deploy("v1")
        results["first_release"] = {"status": first["status"], "wall_ms": round(first_ms, 1),
                                    "phases_ms": release_phases(daemon, first["deployment_id"])}
        endpoint = daemon.get("/environments/production/projects/site")["workloads"][0]["ports"][0]["host"]

        # Releases under load: requests never stop.
        stop = threading.Event()
        load = {"ok": 0, "failed": 0, "errors": [], "latency": []}

        def client():
            while not stop.is_set():
                started = time.perf_counter()
                try:
                    http_get(endpoint)
                    load["ok"] += 1
                    load["latency"].append((time.perf_counter() - started) * 1000)
                except Exception as error:  # noqa: BLE001 - every failure is data
                    load["failed"] += 1
                    load["errors"].append(str(error)[:80])

        clients = [threading.Thread(target=client) for _ in range(4)]
        for thread in clients:
            thread.start()
        releases = []
        for number in range(2, 7):
            label = f"v{number}"
            revision(label)
            wall, settled = deploy(label)
            releases.append({"revision": label, "status": settled["status"], "wall_ms": round(wall, 1),
                             "phases_ms": release_phases(daemon, settled["deployment_id"])})
        last = releases[-1]
        # Operator rollback of a complete release (a new release of v5).
        started = time.perf_counter()
        rolled = daemon.request("POST", f"/deployments/{daemon.get('/environments/production/projects/site')['deployment']['deployment_id']}/rollback")
        rolled = wait_release(daemon, rolled["deployment_id"])
        rollback_ms = (time.perf_counter() - started) * 1000
        # A release that fails readiness (the candidate crashes).
        revision("crash", prelude="import sys; sys.exit(3)")
        crash_ms, crashed = deploy("crash")
        # A release that never becomes ready (timeout 3 s).
        revision("hang", prelude="import time; time.sleep(3600)",
                 readiness={"check": "port", "timeout_ms": 3000, "interval_ms": 100})
        hang_ms, hung = deploy("hang")
        stop.set()
        for thread in clients:
            thread.join()
        results["releases_under_load"] = releases
        results["load"] = {"requests_ok": load["ok"], "requests_failed": load["failed"],
                           "errors": sorted(set(load["errors"]))[:5],
                           "request_latency_ms": percentiles(load["latency"])}
        results["operator_rollback_complete_release"] = {"status": rolled["status"], "wall_ms": round(rollback_ms, 1)}
        results["failed_readiness_crash"] = {"status": crashed["status"], "wall_ms": round(crash_ms, 1),
                                             "failure": crashed.get("failure")}
        results["readiness_timeout_3s"] = {"status": hung["status"], "wall_ms": round(hang_ms, 1)}
        switch = [release["phases_ms"].get("deployment.ready → deployment.switched") for release in releases]
        results["switch_ms"] = percentiles([value for value in switch if value is not None])
        results["release_total_ms"] = percentiles([release["wall_ms"] for release in releases])
        print(f"  releases p50 {results['release_total_ms']['p50']} ms, load failures {load['failed']}/{load['ok'] + load['failed']}", flush=True)
    finally:
        daemon.stop()
    return results


def measure_remote(work):
    port = free_port()
    store = os.path.join(work, "jobs")
    server = subprocess.Popen([COMPUTE, "serve", "--listen", f"127.0.0.1:{port}", "--job-store", store,
                               "--max-concurrent-jobs", "4"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    provider = f"http://127.0.0.1:{port}"
    results = {}
    try:
        for _ in range(200):
            if subprocess.run([COMPUTE, "remote", "health", "--provider", provider], capture_output=True).returncode == 0:
                break
            time.sleep(0.05)
        workload = fixture(os.path.join(work, "remote"), "python")
        script = os.path.join(os.path.dirname(workload), "main.py")
        runs = []
        for _ in range(20):
            started = time.perf_counter()
            result = subprocess.run([COMPUTE, "remote", "run", "--provider", provider, "--network", "network", script, "--json"],
                                    capture_output=True, text=True)
            runs.append((time.perf_counter() - started) * 1000)
            if result.returncode != 0:
                results["remote_run_error"] = result.stderr[-300:]
                break
        results["remote_run_ms_python"] = percentiles(runs)
        submits, jobs = [], []
        for _ in range(40):
            started = time.perf_counter()
            result = subprocess.run([COMPUTE, "remote", "submit", "--provider", provider, "--network", "network", script, "--json"],
                                    capture_output=True, text=True)
            submits.append((time.perf_counter() - started) * 1000)
            try:
                jobs.append(json.loads(result.stdout)["job_id"])
            except (json.JSONDecodeError, KeyError):
                results["remote_submit_error"] = (result.stdout + result.stderr)[-300:]
                break
        started = time.perf_counter()
        for job in jobs:
            subprocess.run([COMPUTE, "remote", "wait", "--provider", provider, job, "--timeout", "120s"], capture_output=True)
        drained = time.perf_counter() - started
        results["remote_submit_ms"] = percentiles(submits)
        results["remote_queue_40_jobs_drain_seconds_after_submit"] = round(drained, 2)
        print(f"  remote submit p50 {results['remote_submit_ms'] and results['remote_submit_ms']['p50']} ms", flush=True)
    finally:
        server.terminate()
        server.wait(10)
    return results


def measure_capacity(work, steps):
    daemon = Daemon(work, "capacity")
    daemon.start()
    results = {"steps": []}
    try:
        daemon.request("POST", "/environments", {"name": "capacity"})
        service = bundle(os.path.join(work, "capacity-bundle"), "python", "main.py", HTTP_SERVICE.format(version="ok", prelude=""))
        count = 0
        for target in steps:
            started = time.perf_counter()
            cpu_before = cpu_seconds(daemon.process.pid)
            while count < target:
                daemon.request("POST", "/environments/capacity/projects", {
                    "name": f"svc{count}", "revision": "r1",
                    "workloads": [{"name": "web", "kind": "service", "bundle": service,
                                   "ports": [{"name": "http", "port": 8080}]}]})
                count += 1
            added = time.perf_counter() - started
            status = daemon.get("/status")
            children = descendants(daemon.process.pid)
            child_rss = sum(rss_kib(pid) for pid in children)
            # Every endpoint answers.
            env = daemon.get("/environments/capacity")
            answered = 0
            for project in env["projects"]:
                try:
                    http_get(project["workloads"][0]["ports"][0]["host"], timeout=3)
                    answered += 1
                except Exception:  # noqa: BLE001
                    pass
            list_ms = percentiles([daemon.timed("GET", "/environments/capacity")[0] for _ in range(5)])
            load1, _, _ = os.getloadavg()
            step = {
                "services": count,
                "running_services": status["running_services"],
                "endpoints_answering": answered,
                "seconds_to_add_step": round(added, 2),
                "daemon_rss_mib": round(rss_kib(daemon.process.pid) / 1024, 1),
                "service_processes": len(children),
                "service_rss_mib_total": round(child_rss / 1024, 1),
                "daemon_cpu_seconds_during_step": round(cpu_seconds(daemon.process.pid) - cpu_before, 2),
                "environment_view_ms": list_ms,
                "load_average_1m": round(load1, 2),
                "memory_available_mib": int(open("/proc/meminfo").read().split("MemAvailable:")[1].split()[0]) // 1024,
            }
            results["steps"].append(step)
            print(f"  capacity {count}: running {status['running_services']} answering {answered} rss {step['service_rss_mib_total']} MiB", flush=True)
            if step["memory_available_mib"] < 1500:
                results["stopped_because"] = "less than 1.5 GiB of memory left"
                break
        # Idle cost at the largest step.
        cpu_before = cpu_seconds(daemon.process.pid)
        time.sleep(10)
        results["daemon_idle_cpu_percent_at_max"] = round((cpu_seconds(daemon.process.pid) - cpu_before) / 10 * 100, 2)
    finally:
        stopped = time.perf_counter()
        daemon.stop()
        results["shutdown_seconds"] = round(time.perf_counter() - stopped, 2)
    return results


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", required=True)
    parser.add_argument("--only", default="runtimes,throughput,daemon,api,release,remote,capacity")
    parser.add_argument("--throughput-seconds", type=float, default=10)
    parser.add_argument("--capacity-steps", default="10,50,100,200,300")
    args = parser.parse_args()
    if not os.path.exists(COMPUTE):
        sys.exit("build first: cargo build --release -p compute-cli")
    sections = args.only.split(",")
    work = tempfile.mkdtemp(prefix="compute-measure-")
    started = time.time()
    report = {
        "format": "compute.platform-audit.measurements@1",
        "measured_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "compute_version": subprocess.run([COMPUTE, "version"], capture_output=True, text=True).stdout.strip(),
        "git": subprocess.run(["git", "-C", ROOT, "rev-parse", "--short", "HEAD"], capture_output=True, text=True).stdout.strip(),
        "host": {
            "cpus": os.cpu_count(),
            "cpu_model": next((line.split(":", 1)[1].strip() for line in open("/proc/cpuinfo") if line.startswith("model name")), None),
            "memory_mib": int(open("/proc/meminfo").read().split("MemTotal:")[1].split()[0]) // 1024,
            "kernel": platform.release(),
            "machine": platform.machine(),
        },
        "build": "release",
    }
    existing = {}
    if os.path.exists(args.out):
        existing = json.load(open(args.out))
    def save():
        existing.update({key: value for key, value in report.items()})
        existing["total_seconds_last_run"] = round(time.time() - started, 1)
        os.makedirs(os.path.dirname(os.path.abspath(args.out)), exist_ok=True)
        json.dump(existing, open(args.out, "w"), indent=1, sort_keys=True)

    try:
        for section in sections:
            print(f"== {section}", flush=True)
            section_started = time.time()
            try:
                value = run_section(section, work, args)
            except Exception as error:  # noqa: BLE001 - a failed section is evidence too
                value = {"error": f"{type(error).__name__}: {error}"[:1000]}
                print(f"  {section} failed: {value['error'][:200]}", flush=True)
            if value is None:
                continue
            existing[section] = {"measured_seconds": round(time.time() - section_started, 1), "results": value}
            save()
    finally:
        shutil.rmtree(work, ignore_errors=True)
    save()
    print(f"wrote {args.out}")


def run_section(section, work, args):
    sections = {
        "runtimes": lambda: measure_runtimes(work),
        "throughput": lambda: measure_throughput(work, args.throughput_seconds),
        "daemon": lambda: measure_daemon(work),
        "api": lambda: measure_api(work),
        "release": lambda: measure_release(work),
        "remote": lambda: measure_remote(work),
        "capacity": lambda: measure_capacity(work, [int(step) for step in args.capacity_steps.split(",")]),
    }
    return sections[section]() if section in sections else None


if __name__ == "__main__":
    main()
