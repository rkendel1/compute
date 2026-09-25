#!/usr/bin/env python3
"""Assemble docs/feltdb-0.11.8-consumer-certification.json from the evidence
scripts/feltdb/certify.sh wrote. Statuses come from the evidence; nothing
unavailable is turned into a pass.

    python3 scripts/feltdb/report.py <compute-commit> [evidence-dir]
"""
import json
import os
import platform
import subprocess
import sys

root = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
commit = sys.argv[1]
evidence = sys.argv[2] if len(sys.argv) > 2 else os.path.join(root, "docs/feltdb-0.11.8-evidence")


def load(name):
    path = os.path.join(evidence, name)
    return json.load(open(path)) if os.path.exists(path) else None


checks = {check["check"]: check for check in load("checks.json") or []}
after = load("feltdb-consumer-benchmark.json") or {}
before = load("feltdb-consumer-benchmark-before.json") or {}
cost = load("feltdb-request-cost.json") or {}
upgrade = load("upgrade-report.json")
backup = load("backup-restore.json")


def status(check):
    return checks.get(check, {}).get("status", "NOT_RUN")


def row(check, statement, evidence_ref):
    return {"item": statement, "status": status(check), "evidence": evidence_ref, "check": check}


host = after.get("host", {})
before_by_operation = {result["operation"]: result for result in before.get("results", [])}
comparison = []
for result in after.get("results", []):
    entry = {
        "operation": result["operation"],
        "scope": result["scope"],
        "pattern": result["pattern"],
        "after": {key: result[key] for key in ("n", "p50_ms", "p95_ms", "max_ms")},
        "feltdb_per_call": {
            key.replace("feltdb_", "").replace("_per_call", ""): result[key]
            for key in result
            if key.startswith("feltdb_")
        },
        "status": "MEASURED",
    }
    previous = before_by_operation.get(result["operation"]) if result["pattern"] == "after" else None
    if previous:
        entry["before_compute_eac2be8"] = {key: previous[key] for key in ("n", "p50_ms", "p95_ms", "max_ms")}
    comparison.append(entry)

report = {
    "format": "compute.feltdb-consumer-certification@1",
    "compute_commit": commit,
    "feltdb": {
        "certified": "@feltdb/core 0.11.8",
        "resolved": (json.loads(open(os.path.join(evidence, "logs/resolved-version.log")).read()) if os.path.exists(os.path.join(evidence, "logs/resolved-version.log")) else None),
        "server": "feltdb-server built from the source shipped in @feltdb/core 0.11.8 (reports version 0.2.0)",
        "previous_server": "feltdb-server built from @feltdb/core 0.11.7 (also reports 0.2.0)",
    },
    "model": {"format": "compute.state@1", "generation": 2},
    "environment": {
        "os": host.get("os") or f"{platform.system()} {platform.release()}",
        "cpu": host.get("cpu"),
        "cpus": host.get("cpus"),
        "memory": host.get("memory"),
        "feltdb_transport": "feltdb-server on the same host, HTTP over loopback",
        "benchmark_build": host.get("build"),
        "test_build": "debug (CARGO_INCREMENTAL=0, CARGO_PROFILE_DEV_DEBUG=0)",
    },
    "statuses": ["PASS", "MEASURED", "SUPPORTED", "NOT_RUN", "UNKNOWN", "FAIL"],
    "checks": list(checks.values()),
    "commands": {
        "certification": "FELTDB_SERVER_BIN=… FELTDB_PREVIOUS_SERVER_BIN=… scripts/feltdb/certify.sh",
        "tests": [check["command"] for check in checks.values() if check["check"] != "benchmark"],
        "benchmark": checks.get("benchmark", {}).get("command"),
        "benchmark_before": "the same benchmark on Compute eac2be8 (docs/feltdb-0.11.8-evidence/before_bench.rs.txt), cargo test --release -p compute-environment --test before_bench -- --ignored",
    },
    "results": {
        "version": [
            row("resolved-version", "Compute resolves exactly @feltdb/core 0.11.8 (lockfile and node_modules)", "logs/resolved-version.log"),
            row("model-compiles", "compute.flow compiles with 0.11.8 to the checked-in manifest", "logs/model-compiles.log"),
        ],
        "bounded_query": [
            row("adapter-real-server", "An identity lookup is one index probe (1 row examined); a project's newest executions examine only that project's rows; get_many never scans", "consumer.rs::targeted_reads_are_indexed_and_bounded"),
            row("controller-real-server", "A quiet reconcile cycle runs no queries (two revision reads); controller paths scan only the documented shapes", "feltdb_consumer.rs::the_controller_keeps_authority_in_feltdb_through_an_outage"),
        ],
        "snapshot": [
            row("adapter-real-server", "Snapshots are coherent under concurrent writers (never half a transaction), bounded, reused while current, stale after any write, deterministic in identity; on memory, file, and a real FeltDB", "compute_state::conformance via feltdb_is_a_conforming_durable_authority"),
            row("adapter-real-server", "A key without state:read cannot build a snapshot", "consumer.rs::snapshots_are_authorized_by_the_authority"),
        ],
        "authority_and_recovery": [
            row("controller-real-server", "healthy → outage → read (stale, labelled) → mutation (state_unavailable) → recovery (snapshots rebuilt, feltdb.recovered) → mutation, in process on a real feltdb-server", "feltdb_consumer.rs::the_controller_keeps_authority_in_feltdb_through_an_outage"),
            row("end-to-end-real-server", "The CLI controller on a real feltdb-server: degraded start, refused changes, recovery, restore on a fresh node", "recovery.rs::managed_feltdb_is_the_durable_authority"),
            row("adapter-real-server", "An outage is unavailability for reads and writes; a failed build keeps the published snapshot; recovery is automatic", "consumer.rs::an_outage_is_unavailability_and_recovery_is_automatic"),
        ],
        "concurrency": [
            row("controller-real-server", "40 concurrent executions keep every record, receipt, and event; event sequences are unique", "feltdb_consumer.rs::concurrent_operations_keep_every_record_and_event"),
            row("workspace-tests", "1,000 concurrent executions lose no evidence (memory backend)", "executions.rs"),
        ],
        "upgrade": [
            row("adapter-real-server", "compute control-plane upgrade: dry run; refuses an unverifiable backup before changing anything; with a verified FeltDB archive: applies, verifies, backfills identities, smoke-tests; old records readable through the index; idempotent", "consumer.rs::the_upgrade_backs_up_migrates_and_verifies"),
            row("adapter-real-server", "A newer model is never downgraded (upgrade and provision both refuse)", "consumer.rs::a_newer_model_is_never_downgraded"),
            row("adapter-real-server", "A controller refuses to start on an older model", "consumer.rs::the_upgrade_backs_up_migrates_and_verifies"),
            row("previous-server-compatibility", "State written by the 0.11.7 server opens unchanged on 0.11.8 and passes conformance; the 0.11.7 server opens what 0.11.8 wrote", "consumer.rs::state_written_by_the_previous_server_opens_on_this_one"),
        ],
        "backup_restore": [
            row("adapter-real-server", "FeltDB offline backup create → verify → restore → serve: record-by-record equivalence (executions, events, audit), indexes answer", "consumer.rs::a_feltdb_backup_restores_compute_state_exactly"),
            {"item": "FeltDB online backup of application state verifies", "status": "FAIL", "evidence": "backup-restore.json (online_backup.verification): 'snapshot contains an invalid state row'", "attribution": "FeltDB 0.11.8"},
        ],
        "workspace": [row("workspace-tests", "Full Compute workspace tests", "logs/workspace-tests.log")],
    },
    "remaining_gaps": [
        {"gap": "FeltDB 0.11.8's cost per request grows linearly with committed state: at 4,000 committed operations /v1/state/version takes ~42 ms and an index probe returning nothing ~81 ms (FeltDB reports ~32 ms of execution), while /health stays ~0.6 ms", "status": "MEASURED", "attribution": "FeltDB (inside its request handling; the exact cause is not isolated)", "evidence": "feltdb-request-cost.json"},
        {"gap": "Controller start with state is slower: 22.6 s vs 14.3 s before, at this dataset", "status": "MEASURED", "attribution": "Partly attributed: the start now runs the identity-backfill probe (21 FeltDB requests, each scanning its collection for records without record_id) and builds the desired snapshot by identity probes (~68 FeltDB requests), each paying FeltDB's per-request cost; the split between them was not measured", "evidence": "feltdb-consumer-benchmark.json, feltdb-consumer-benchmark-before.json"},
        {"gap": "Execution lookup (135 vs 102 ms) and release lookup (270 vs 195 ms) are slower, with the same number of FeltDB requests as before", "status": "UNKNOWN", "attribution": "Not isolated. Both are FeltDB-request bound; the state size at the moment of measurement and the model's added indexes differ between the runs"},
        {"gap": "FeltDB 0.11.8 backup verify rejects an online backup of application state ('snapshot contains an invalid state row'); compute control-plane upgrade therefore stops before changing anything when given --backup, and --backup-archive (an offline FeltDB backup) is the working path", "status": "FAIL", "attribution": "FeltDB 0.11.8 (export_snapshot / install_snapshot)"},
        {"gap": "A restored FeltDB archive names its sidecars applications.json and platform.json; a server on files/state.log reads state.applications.json and state.platform.json, so they must be renamed before serving", "status": "MEASURED", "attribution": "FeltDB 0.11.8"},
        {"gap": "The 0.11.7 and 0.11.8 servers report the same version (0.2.0) and capability fingerprint; the server release cannot be verified from the wire", "status": "UNKNOWN", "attribution": "FeltDB"},
        {"gap": "FeltDB 0.11.8's planner uses no ordered index: the newest event (start, recovery) and unscoped operator listings scan server-side", "status": "SUPPORTED", "attribution": "FeltDB planner; bounded by what FeltDB returns (limit), not by what it examines"},
        {"gap": "Placement never re-discovers a provider's capabilities after their cache entry expires, so a long-running controller eventually cannot place new workloads (reproduced on Compute eac2be8 too)", "status": "FAIL", "attribution": "Compute placement (pre-existing; out of this change's scope, reported)"},
        {"gap": "Managed FeltDB over a network; half-open partitions; more than one controller per control plane", "status": "NOT_RUN"},
        {"gap": "Rollback of a Compute model upgrade: FeltDB revisions are immutable and the previous revision can be promoted again, but Compute has no command for it and it was not exercised", "status": "NOT_RUN"},
    ],
    "upgrade_report": upgrade,
    "backup_restore_evidence": backup,
    "feltdb_request_cost": cost.get("rows"),
    "benchmark": {
        "dataset": after.get("dataset"),
        "samples": after.get("samples"),
        "scopes": after.get("scopes"),
        "controller_after": after.get("controller"),
        "controller_before": before.get("controller"),
        "operations": comparison,
    },
}
json.dump(report, open(os.path.join(root, "docs/feltdb-0.11.8-consumer-certification.json"), "w"), indent=2)
print("wrote docs/feltdb-0.11.8-consumer-certification.json")
