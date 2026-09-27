"""Build docs/audit.json: machine-derived inventories (CLI, API, UI, tests)
joined with the audit's verified classifications.

Inputs are the inventories captured on 2026-09-27 (cli.json, routes.json,
tests.json in this directory) and the experiment results
(experiments.json). Run from the repository root:

    python3 docs/audit-evidence/2026-09-27/generate_audit_json.py

The classifications below are the audit's findings; each names its
evidence. `crates/compute-cli/tests/audit.rs` fails when the CLI or the API
gains or loses a command or route that audit.json does not list.
"""
import json
import os
import re

HERE = os.path.dirname(os.path.abspath(__file__))
cli = json.load(open(os.path.join(HERE, 'cli.json')))
api = json.load(open(os.path.join(HERE, 'api.json')))
tests = json.load(open(os.path.join(HERE, 'tests.json')))
experiments = json.load(open(os.path.join(HERE, 'experiments.json')))

STATUSES = ['IMPLEMENTED', 'IMPLEMENTED + VERIFIED', 'PARTIAL', 'DOCUMENTED ONLY', 'STUB',
            'UNUSED', 'BROKEN', 'MISSING', 'UNKNOWN']
JOURNEY = ['PASS', 'PARTIAL', 'FAIL', 'NOT IMPLEMENTED']


def cap(id, area, name, status, usable, complete, evidence, notes=''):
    assert status in STATUSES, status
    return {'id': id, 'area': area, 'name': name, 'status': status,
            'user_can_use': usable, 'in_complete_model': complete,
            'evidence': evidence, 'notes': notes}


CT = 'crates/compute-environment/tests/computers.rs'
PJ = 'crates/compute-cli/tests/product_journey.rs'

capabilities = [
    # Launch and surface
    cap('launch', 'launch', '`compute` launches the control plane and a local computer host', 'IMPLEMENTED + VERIFIED', 'yes', True,
        {'source': ['crates/compute-cli/src/launch_cmd.rs'], 'tests': [PJ], 'journey': 'first-launch'},
        'Starts `compute serve` (127.0.0.1:8788) and `compute start` (127.0.0.1:8787) with a generated pool; 3.7 s cold, 0.01 s when running. Opens a browser with xdg-open/open.'),
    cap('ui-modes', 'ui', 'Work / Manage modes of one control plane', 'IMPLEMENTED + VERIFIED', 'yes', True,
        {'source': ['crates/compute-environment/ui/app.js', 'crates/compute-environment/ui/index.html'], 'tests': ['crates/compute-cli/tests/work_mode_ui.rs', PJ]},
        'Browser tests skip where Playwright/Chromium are absent, which includes CI.'),
    cap('ui-home', 'ui', 'Action-first home ("What do you want to do?")', 'IMPLEMENTED + VERIFIED', 'yes', True,
        {'source': ['crates/compute-environment/ui/app.js#homeView'], 'tests': [PJ]},
        'Shows software and computers; environments without a computer are not on the home page.'),
    cap('ui-certification-package', 'ui', 'packages/compute-ui-e2e browser certification', 'BROKEN', 'n/a', True,
        {'tests': ['packages/compute-ui-e2e/src/control-plane.test.mjs'], 'journey': 'experiments.json#ui_certification_package'},
        'Fails since 69b70d9 changed the `#/` route; not run in CI.'),
    # Computer model
    cap('computer-environments', 'computer', 'Environments backed by a durable computer', 'IMPLEMENTED + VERIFIED', 'yes', True,
        {'source': ['crates/compute-environment/src/daemon/computers.rs', 'crates/compute-core/src/computers.rs'], 'tests': [CT]}),
    cap('persistent', 'computer', 'Persistent computers (no TTL, claimed)', 'IMPLEMENTED + VERIFIED', 'yes', True, {'tests': [CT]}),
    cap('ephemeral', 'computer', 'Ephemeral computers that expire and keep evidence', 'IMPLEMENTED + VERIFIED', 'yes', True,
        {'tests': [CT + '#an_ephemeral_environment_expires_and_keeps_its_evidence', PJ]}),
    cap('lifetime-change', 'computer', 'Change lifetime in place (claim on the target)', 'IMPLEMENTED + VERIFIED', 'yes', True,
        {'tests': [CT + '#go_changes_lifetime_and_configuration_in_place_and_refuses_stale_views']}),
    cap('in-place-change', 'computer', 'Contents changed in place, no redeployment', 'IMPLEMENTED + VERIFIED', 'yes', True,
        {'tests': [CT + '#deployment_is_reconciliation_of_the_same_computer', PJ]},
        'Same provider resource across releases, configuration changes, and controller restarts.'),
    cap('replacement', 'computer', 'Explicit replacement provisions a new machine and retires the old', 'IMPLEMENTED + VERIFIED', 'yes', True,
        {'tests': [CT + '#provider_failures_and_replacements_are_explicit', CT + '#deployment_is_reconciliation_of_the_same_computer']}),
    cap('stop-resume', 'computer', 'Stop and resume the same machine', 'IMPLEMENTED + VERIFIED', 'yes', True, {'tests': [CT, PJ]}),
    cap('orphan-sweep', 'computer', 'Sessions no computer record claims are torn down', 'IMPLEMENTED + VERIFIED', 'no (automatic)', True,
        {'tests': [CT + '#provisioning_survives_a_controller_restart_and_orphans_are_torn_down']}),
    cap('machine-loss', 'computer', 'A machine the target lost is reported (environment_lost)', 'PARTIAL', 'partial', True,
        {'source': ['crates/compute-environment/src/daemon/computers.rs#target_unreachable'], 'tests': [CT + '#provider_failures_and_replacements_are_explicit'],
         'journey': 'experiments.json#after_the_target_forgot_the_machine'},
        'Only noticed by the process probe or a lifecycle step. A computer with no processes stays "running" with no failure indefinitely, through reconcile and a control-plane restart.'),
    cap('target-down-visibility', 'computer', 'A computer whose target is unreachable says so', 'MISSING', 'no', True,
        {'journey': 'experiments.json#computer_while_target_down'},
        'The view keeps "running", failure null; exec fails with runtime_unavailable.'),
    cap('contents', 'computer', 'Repositories, packages, processes, projects, configuration as desired state', 'IMPLEMENTED + VERIFIED', 'yes', True, {'tests': [CT, PJ]}),
    cap('endpoints', 'computer', 'Process ports published as endpoints (target host:port)', 'IMPLEMENTED + VERIFIED', 'yes', True, {'tests': [CT, PJ]},
        'No port publishing for the container provider; no ingress/TLS/domains for computer endpoints.'),
    cap('go-fencing', 'computer', 'GO: one generation-fenced change; stale views refused', 'IMPLEMENTED + VERIFIED', 'yes', True,
        {'tests': [CT, 'crates/compute-cli/tests/work_mode_ui.rs']}),
    cap('work-sessions', 'computer', 'Work sessions (attached / ephemeral) in FeltDB', 'IMPLEMENTED + VERIFIED', 'yes', True,
        {'tests': [CT + '#work_sessions_enter_environments_and_temporary_ones_end_with_them']}),
    cap('files', 'computer', 'Files in the computer', 'PARTIAL', 'partial', True, {'source': ['crates/compute-environment/ui/app.js#listFiles'], 'tests': [PJ]},
        'List and read (head -c 64 KiB) through exec jobs; no upload, edit, or download.'),
    cap('terminal', 'computer', 'Terminal', 'PARTIAL', 'partial', True, {'tests': [PJ]},
        'Each line is a durable job; no interactive PTY. The `terminal` session capability is offered by no provider.'),
    # Targets, discovery, placement
    cap('target-inventory', 'targets', 'Targets listed with health, platform, resources, capabilities, features', 'IMPLEMENTED + VERIFIED', 'CLI/API only', True,
        {'api': ['GET /targets'], 'cli': ['compute target list'], 'tests': ['crates/compute-cli/tests/computers.rs']}, 'Not shown in the UI.'),
    cap('discovery-resources', 'discovery', 'CPU count, memory, disk, OS/architecture discovered', 'IMPLEMENTED + VERIFIED', 'indirect', True,
        {'journey': 'GET /compute/capabilities on this host: 4 CPU, 16.9 GB, 270 GB, linux-x86_64'}),
    cap('discovery-runtimes', 'discovery', 'Language runtimes discovered (installed/available/ready)', 'IMPLEMENTED + VERIFIED', 'CLI', True, {'cli': ['compute runtimes', 'compute doctor']}),
    cap('discovery-isolation', 'discovery', 'Isolation facilities discovered (landlock ABI, network namespaces, cgroups)', 'IMPLEMENTED + VERIFIED', 'CLI', True, {'cli': ['compute isolation']}),
    cap('discovery-features', 'discovery', 'Target features: kvm, virtualization, firecracker, containers, gpu', 'PARTIAL', 'indirect', True,
        {'source': ['crates/compute-provider/src/lib.rs#detect_target_features'], 'journey': 'experiments.json#placement_refusals'},
        '`containers` is inferred from a docker/podman binary on PATH: advertised on this host with no engine running. gpu = /dev/nvidia0 exists. kvm = /dev/kvm openable. No nested-virtualization, GPU model, or engine liveness check.'),
    cap('discovery-network', 'discovery', 'Network interfaces, reachability, exposed ports', 'MISSING', 'no', True, {}, 'Not discovered. Endpoint hosts come from the pool endpoint URL.'),
    cap('discovery-automatic-targets', 'discovery', 'Targets discovered automatically (no configuration)', 'MISSING', 'no', True, {},
        'Targets come from a pool file. The launcher writes one naming the local host; no LAN/cloud discovery.'),
    cap('placement', 'placement', 'Capability-matched placement with reasons', 'IMPLEMENTED + VERIFIED', 'yes', True,
        {'source': ['crates/compute-placement/src/matching.rs'], 'tests': ['crates/compute-placement/tests/matching.rs', 'crates/compute-placement/tests/selection.rs']}),
    cap('placement-override', 'placement', 'Constrain placement to a target', 'IMPLEMENTED + VERIFIED', 'yes (dialog field)', True, {'tests': [CT + '#placement_chooses_a_target_by_what_the_computer_needs']}),
    cap('placement-dead-options', 'placement', 'UI offers persistent storage / public endpoint', 'BROKEN', 'no', True,
        {'journey': 'experiments.json#placement_refusals', 'source': ['crates/compute-provider/src/sessions.rs#capabilities', 'crates/compute-provider/src/containers.rs#capabilities']},
        'No provider offers either; any computer requesting them is refused (with reasons).'),
    # Runtimes and substrates
    cap('runtime-wasm', 'runtime', 'WASM workloads (wasmtime, WASI p1)', 'IMPLEMENTED + VERIFIED', 'CLI (compute run)', True,
        {'source': ['crates/compute-runtime-wasm/src/lib.rs'], 'tests': ['crates/compute-runtime-wasm/tests/conformance.rs', 'crates/compute-runtime/tests/conformance.rs']},
        'Workload engine only: a computer cannot be a WASM sandbox.'),
    cap('runtime-process', 'runtime', 'Process runtimes: node, bun, deno, python, ruby, php, jvm, dotnet, native, shell', 'IMPLEMENTED + VERIFIED', 'CLI (compute run), daemon workloads', True,
        {'source': ['crates/compute-runtime-process/src/lib.rs'], 'tests': ['crates/compute-runtime/tests/conformance.rs'], 'journey': 'experiments.json#runtimes'},
        'Pinned distributions download on demand (python verified here). `compute run script.py` fails by default: network "none" is unenforceable for process runtimes.'),
    cap('substrate-workspace', 'runtime', 'Computers as private workspaces on the target host (native processes)', 'IMPLEMENTED + VERIFIED', 'yes', True,
        {'source': ['crates/compute-provider/src/sessions.rs#WorkspaceSessionProvider'], 'tests': ['crates/compute-provider/tests/sessions.rs', CT, PJ]},
        'A directory with a shell: not an isolation boundary.'),
    cap('substrate-container', 'runtime', 'Computers as containers (docker/podman)', 'IMPLEMENTED', 'CLI flag only', True,
        {'source': ['crates/compute-provider/src/containers.rs'], 'tests': ['crates/compute-provider/tests/sessions.rs#the_container_adapter_translates_the_session_contract']},
        'Tested against a fake docker script only; never run against a real engine in this audit (none available) or in CI. `compute up --containers` / `compute serve --session-provider container`.'),
    cap('substrate-firecracker', 'runtime', 'Firecracker microVM computers', 'MISSING', 'no', True, {}, 'Only a feature label for placement.'),
    cap('substrate-kvm', 'runtime', 'KVM virtual machine computers', 'MISSING', 'no', True, {}, 'Only a feature label for placement.'),
    cap('substrate-wasm', 'runtime', 'WASM computers', 'MISSING', 'no', True, {}),
    # Providers
    cap('provider-local', 'provider', 'Local provider (in-process engine)', 'IMPLEMENTED + VERIFIED', 'yes', True, {'source': ['crates/compute-provider/src/lib.rs#LocalProvider'], 'tests': ['crates/compute-cli/tests/cli.rs']},
        'Runs workloads; does not host computers (sessions_unsupported).'),
    cap('provider-remote', 'provider', 'Remote provider (`compute serve`, compute.remote@1)', 'IMPLEMENTED + VERIFIED', 'yes', True,
        {'source': ['crates/compute-provider/src/lib.rs#RemoteProvider'], 'tests': ['crates/compute-provider/tests/remote.rs', 'crates/compute-cli/tests/remote_pool.rs']},
        'The only kind of target that hosts computers.'),
    cap('provider-daemon-node', 'provider', 'A daemon node as a provider (deployments, /compute/*)', 'IMPLEMENTED + VERIFIED', 'yes', True, {'tests': ['crates/compute-cli/tests/product.rs']}),
    cap('provider-fly', 'provider', 'Fly Machines', 'MISSING', 'no', True, {}), cap('provider-railway', 'provider', 'Railway', 'MISSING', 'no', True, {}),
    cap('provider-render', 'provider', 'Render', 'MISSING', 'no', True, {}), cap('provider-cloud-vm', 'provider', 'Cloud VMs / bare metal provisioning', 'MISSING', 'no', True, {}),
    cap('provider-dns', 'provider', 'DNS providers (for domains)', 'IMPLEMENTED + VERIFIED', 'yes', True, {'source': ['crates/compute-network/src/dns.rs'], 'tests': ['crates/compute-network/tests/providers.rs', 'crates/compute-environment/tests/network.rs']}),
    # Projects and the lifecycle
    cap('run-a-project', 'projects', 'Inspect a source in the computer and propose an assembly', 'IMPLEMENTED + VERIFIED', 'yes', True,
        {'source': ['crates/compute-environment/src/daemon/software.rs#propose'], 'tests': ['crates/compute-environment/src/daemon/software.rs', PJ]},
        'Recognises package.json, Python, Go, Rust, Makefile, Procfile, .env.example, compose/postgres/redis hints. A local folder must be a Git repository.'),
    cap('multi-project', 'projects', 'Several projects on one computer', 'IMPLEMENTED + VERIFIED', 'yes', True, {'tests': [PJ]}),
    cap('build-test', 'projects', 'Build, test, checks, named commands in the computer', 'IMPLEMENTED + VERIFIED', 'yes', True, {'tests': [CT, PJ]}),
    cap('publish', 'release', 'Publish an immutable version (source, build, tests, checks, package digest)', 'IMPLEMENTED + VERIFIED', 'yes', True,
        {'source': ['crates/compute-environment/src/daemon/software.rs'], 'tests': [CT + '#versions_are_published_deployed_promoted_and_rolled_back_in_place', PJ]},
        'The version is a commit + digest + assembly; no artifact is stored (the package digest is computed, not kept).'),
    cap('deploy', 'release', 'Deploy a version to an environment (rollout with steps)', 'IMPLEMENTED + VERIFIED', 'yes', True, {'tests': [CT, PJ]}),
    cap('promote', 'release', 'Promote test → production with a reviewed plan', 'IMPLEMENTED + VERIFIED', 'yes', True, {'tests': [CT, PJ]},
        'No approval workflow (approvals list is always empty).'),
    cap('rollback', 'release', 'Roll back to an earlier version', 'IMPLEMENTED + VERIFIED', 'yes', True, {'tests': [CT, PJ]}),
    cap('zero-downtime', 'release', 'Zero-downtime release inside a computer', 'MISSING', 'no', True, {}, 'A release restarts processes. Zero-downtime traffic switching exists only for bundle projects on the daemon node.'),
    cap('approvals', 'release', 'Promotion approvals', 'MISSING', 'no', True, {'source': ['crates/compute-environment/src/status.rs#PromotionPlan']}),
    cap('artifact-store', 'release', 'Stored build artifacts for versions', 'MISSING', 'no', True, {}, 'Bundle revisions store artifacts; computer versions store only the commit and digest.'),
    # Legacy parallel models
    cap('bundle-projects', 'legacy', 'Bundle projects, revisions, zero-downtime releases on the daemon node', 'IMPLEMENTED + VERIFIED', 'yes', 'reconcile',
        {'source': ['crates/compute-environment/src/daemon/release.rs', 'crates/compute-environment/src/daemon/deploy.rs'], 'tests': ['crates/compute-environment/tests/releases.rs']},
        'Executes on the daemon host (supervisor). Refused for environments with a computer.'),
    cap('applications', 'legacy', '`compute init/deploy` applications on a provider node', 'IMPLEMENTED + VERIFIED', 'yes', 'reconcile',
        {'source': ['crates/compute-environment/src/daemon/applications.rs'], 'tests': ['crates/compute-cli/tests/product.rs'], 'journey': 'experiments.json#application_journey'},
        '`compute deploy <dir>` auto-starts a daemon with state in ./.compute/daemon; the app runs on that daemon node.'),
    cap('domains-tls', 'operations', 'Domains, DNS, ACME certificates, ingress', 'IMPLEMENTED + VERIFIED', 'yes (bundle projects)', True,
        {'tests': ['crates/compute-environment/tests/network.rs', 'crates/compute-network/tests/ingress.rs']}, 'Routes to bundle-project services only; not to computer endpoints.'),
    # Operations
    cap('logs', 'operations', 'Process logs and job output', 'IMPLEMENTED + VERIFIED', 'yes', True, {'tests': [CT, PJ]}, 'Read on demand (tail); no streaming for computer processes.'),
    cap('restart', 'operations', 'Restart a process in place', 'IMPLEMENTED + VERIFIED', 'yes', True, {'tests': ['crates/compute-environment/src/daemon/software.rs', PJ]}),
    cap('health', 'operations', 'Process health (probe) and endpoint reachability (rollouts)', 'PARTIAL', 'yes', True, {'tests': [CT]},
        'Process liveness is probed every 15 s; endpoints are TCP-checked only during a rollout; no HTTP health checks for computer processes.'),
    cap('metrics', 'observability', 'Metrics endpoint', 'IMPLEMENTED', 'API only', True, {'api': ['GET /metrics']}, 'Not surfaced in the UI or CLI.'),
    cap('events', 'observability', 'Durable lifecycle events and a live stream', 'IMPLEMENTED + VERIFIED', 'yes', True, {'tests': ['crates/compute-environment/tests/control_plane.rs']}),
    cap('receipts', 'observability', 'Verifiable execution receipts', 'IMPLEMENTED + VERIFIED', 'yes', True, {'tests': ['crates/compute-core/src/receipt.rs', 'crates/compute-cli/tests/product.rs']}),
    # State
    cap('state-feltdb', 'state', 'FeltDB as the durable authority (model generation 6)', 'IMPLEMENTED + VERIFIED', 'configuration', True,
        {'tests': ['crates/compute-state-feltdb/tests/consumer.rs', 'crates/compute-environment/tests/feltdb_consumer.rs']}, 'Opt-in: `--state feltdb` or compute.toml.'),
    cap('state-default-file', 'state', 'Default control state is a local file (control-state.json)', 'IMPLEMENTED', 'yes', 'reconcile',
        {'source': ['crates/compute-cli/src/control_state.rs'], 'journey': 'experiments.json#environment'}, 'The launcher and `compute start` default to the file backend, contrary to "FeltDB owns durable state".'),
    cap('restart-recovery', 'state', 'Control-plane restart keeps and resumes everything', 'IMPLEMENTED + VERIFIED', 'yes', True,
        {'tests': [CT, 'crates/compute-cli/tests/recovery.rs'], 'journey': 'experiments.json#after_control_plane_restart'}),
    # Security
    cap('daemon-auth', 'security', 'Daemon: operator credentials, scopes, audit; owner checks for computers', 'IMPLEMENTED + VERIFIED', 'yes', True,
        {'source': ['crates/compute-environment/src/auth.rs'], 'tests': ['crates/compute-environment/tests/security.rs', CT]},
        'Loopback without TLS admits requests with no credential (development mode); unknown bearer tokens get 401.'),
    cap('target-auth', 'security', 'Target (`compute serve`) authentication', 'MISSING', 'n/a', True,
        {'source': ['crates/compute-provider/src/lib.rs#AllowAllAuthorizer'], 'journey': 'experiments.json#target_exec_without_credential'},
        'Any caller that reaches a target can list sessions and exec in any computer; the daemon authenticates to no one, so its computers are owned by "anonymous".'),
    cap('isolation-computers', 'security', 'Isolation between computers on one host', 'MISSING', 'n/a', True, {'source': ['crates/compute-provider/src/sessions.rs']},
        'Workspace computers are directories under one user; processes share the host network and filesystem permissions.'),
    # Agents and API
    cap('appport', 'agents', 'AppPort client for every UI operation', 'IMPLEMENTED + VERIFIED', 'yes', True,
        {'source': ['packages/compute-appport/src/computers.ts'], 'tests': ['packages/compute-appport/src/test/computers.test.ts']}, 'Tested against a stub daemon, not a real one.'),
    cap('agent-identity', 'agents', 'Agents act under operator credentials with scopes', 'IMPLEMENTED', 'yes', True, {'source': ['crates/compute-environment/src/auth.rs']},
        'No agent-specific identity or delegation; an agent is an operator.'),
]

journeys = [
    ('first-launch', 'compute → UI → first-run experience', 'PASS', [PJ, 'experiments.json#launch_seconds'],
     'The home page offers actions; there is no guided first-run flow, and a machine with no free memory for the 1 GiB default gets an admission error in the wizard.'),
    ('run-a-project', 'choose → inspect → configure → GO → running', 'PASS', [PJ], ''),
    ('multi-project', 'project A → GO → project B → GO → same computer', 'PASS', [PJ], ''),
    ('change-in-place', 'change desired state → GO → same computer changes', 'PASS', [PJ, CT], 'Provider resource identical before and after.'),
    ('build', 'project → build → result', 'PASS', [PJ], ''),
    ('test', 'project → test → result', 'PASS', [PJ], ''),
    ('publish', 'project → version → publish', 'PASS', [PJ, CT], ''),
    ('deploy', 'version → environment → deploy', 'PASS', [PJ, CT], ''),
    ('promote', 'test → production', 'PASS', [PJ, CT], 'No approvals.'),
    ('rollback', 'production → previous version', 'PASS', [PJ, CT], ''),
    ('operate', 'logs, processes, health, restart, configuration, commands', 'PARTIAL', [PJ, CT], 'No streaming logs, no HTTP health, no metrics in the UI, no resource scaling short of replacement.'),
    ('session', 'create → connect → exec → logs → stop → resume → destroy', 'PASS', ['crates/compute-cli/tests/sessions.rs', 'crates/compute-provider/tests/sessions.rs'],
     'Target sessions (not through the daemon). Work sessions through the daemon: open/close only.'),
    ('ephemeral', 'create → use → expire → evidence retained', 'PASS', [CT, PJ], ''),
    ('replacement', 'environment → replace → new computer → old retired', 'PASS', [CT], 'CLI/API and the Manage dialog.'),
    ('container-computer', 'a computer in a real container', 'NOT IMPLEMENTED', ['experiments.json#environment'], 'Adapter exists; no engine available to verify; not in CI.'),
    ('machine-loss', 'the target loses the machine → Compute reports it', 'FAIL', ['experiments.json#after_the_target_forgot_the_machine'], 'Undetected for computers without processes.'),
    ('ui-certification', 'packages/compute-ui-e2e', 'FAIL', ['experiments.json#ui_certification_package'], 'Broken by the home-route change.'),
    ('readme-run', '`compute run script.py` from the README', 'FAIL', ['experiments.json#runtimes'], 'network "none" is unenforceable for python; needs --network.'),
    ('readme-application', '`compute init my-app; compute deploy my-app`', 'PASS', ['experiments.json#application_journey'], 'Auto-starts a second control plane in ./.compute/daemon.'),
]
journeys = [{'id': j[0], 'journey': j[1], 'status': j[2], 'evidence': j[3], 'notes': j[4]} for j in journeys]
for j in journeys:
    assert j['status'] in JOURNEY

runtimes = [
    {'runtime': 'native (process)', 'implemented': True, 'discoverable': True, 'placeable': True, 'executable': True, 'ui': 'computers (workspace)', 'cli': True, 'tests': True, 'production_ready': 'no (no isolation boundary)'},
    {'runtime': 'language runtimes (node, bun, deno, python, ruby, php, jvm, dotnet, shell)', 'implemented': True, 'discoverable': True, 'placeable': True, 'executable': True, 'ui': 'no (workload engine)', 'cli': True, 'tests': True, 'production_ready': 'partial (process isolation only; landlock/netns where available)'},
    {'runtime': 'wasm (wasmtime, WASI p1)', 'implemented': True, 'discoverable': True, 'placeable': True, 'executable': True, 'ui': 'no', 'cli': True, 'tests': True, 'production_ready': 'workload engine only'},
    {'runtime': 'containers (docker/podman)', 'implemented': True, 'discoverable': 'inferred from a binary on PATH', 'placeable': 'as a feature label, not as a substrate', 'executable': 'unverified (fake docker only)', 'ui': 'no', 'cli': '--containers / --session-provider container', 'tests': 'fake docker', 'production_ready': False},
    {'runtime': 'kvm', 'implemented': False, 'discoverable': True, 'placeable': 'label only', 'executable': False, 'ui': 'feature field', 'cli': False, 'tests': 'matching only', 'production_ready': False},
    {'runtime': 'firecracker', 'implemented': False, 'discoverable': 'binary + /dev/kvm', 'placeable': 'label only', 'executable': False, 'ui': 'feature field', 'cli': False, 'tests': 'matching only', 'production_ready': False},
    {'runtime': 'gpu', 'implemented': False, 'discoverable': '/dev/nvidia0 exists', 'placeable': 'label only', 'executable': False, 'ui': 'feature field', 'cli': False, 'tests': 'matching only', 'production_ready': False},
]

cli_out = cli
api_out = api

ui = {
    'routes': [
        ['#/', 'homeView', 'both'], ['#/run', 'runView', 'both'], ['#/software', 'softwareListView', 'both'],
        ['#/software/{project}', 'softwareView', 'both'], ['#/software/{project}/versions/{v}', 'versionView', 'manage'],
        ['#/operations/version/{project}/{v}', 'operationView', 'manage'], ['#/operations/rollout/{id}', 'operationView', 'manage'],
        ['#/environments', 'environmentsView', 'manage'], ['#/environments/{e}', 'environmentView', 'manage'],
        ['#/environments/{e}/projects/{p}/{tab}', 'projectView (bundle projects, 8 tabs)', 'manage'],
        ['#/work', 'workHomeView', 'work'], ['#/work/{e}', 'workView', 'work'],
        ['#/projects', 'projectsView (bundle)', 'manage'], ['#/projects/{p}', 'projectDetailView (bundle)', 'manage'],
        ['#/services', 'servicesView', 'manage'], ['#/deployments/{id}', 'deploymentView (bundle)', 'manage'],
        ['#/domains', 'domainsView', 'manage'], ['#/domains/{d}', 'domainView', 'manage'], ['#/events', 'eventsView', 'manage'],
    ],
    'not_in_ui': ['targets and their features', 'operator credentials and the audit trail', 'node upgrade/rollback', 'runtimes, doctor, isolation, capabilities',
                  'placement explanation', 'policy', 'target sessions (compute session create…)', 'metrics', 'network status', 'certificate/dns status tables',
                  'application deploy/pack', 'connect grant (credentials)', 'bundle revision registration (compute project push)', 'FeltDB provisioning/upgrade'],
}

tests_out = {'files': tests, 'ci': {
    '.github/workflows/test.yml': ['cargo fmt --check', 'cargo test --workspace --locked', 'compute-cli product acceptance', 'AppPort contract (npm test)'],
    '.github/workflows/feltdb-consumer.yml': ['state backends conform', 'FeltDB backend against feltdb-server (ignored tests)', 'controller against feltdb-server'],
    '.github/workflows/distribution-certification.yml': ['release build', 'distribution build/verify/certify'],
    'not_in_ci': ['browser tests (skip without Playwright/Chromium)', 'packages/compute-ui-e2e', 'container provider against a real engine']}}


models = [
    {'term': 'Environment', 'is': 'A durable record (Environment in control state) with a name, desired state (running/stopped), configuration, policy, and optionally an owner, a ComputerSpec, and EnvironmentContents.', 'source': 'crates/compute-state/src/model.rs#EnvironmentRecord',
     'collision': 'Two kinds share the name: an environment with a computer, and a "node environment" whose bundle projects run on the daemon host. `applications` is a reserved node environment for `compute deploy <dir>`.'},
    {'term': 'Computer', 'is': 'The machine behind an environment with a ComputerSpec: a Computer record (status, generation, target, session, provider_resource, observed contents) driven by the daemon.', 'source': 'crates/compute-state/src/model.rs#ComputerRecord',
     'collision': 'The UI says "Computer" and "Machine"; the CLI says `environment computer`; the target calls it a session.'},
    {'term': 'Session (target)', 'is': 'A durable record on a `compute serve` target: a workspace or container, commands run as durable jobs. A computer IS a persistent, referenced target session.', 'source': 'crates/compute-core/src/sessions.rs#ComputeSession',
     'collision': 'Stored in the target\'s session store, not FeltDB. `compute session create` makes one directly (no environment, no daemon).'},
    {'term': 'Work session', 'is': 'A WorkSession record in control state: an operator entering an environment (attached) or owning a temporary one (ephemeral).', 'source': 'crates/compute-state/src/model.rs#WorkSessionRecord',
     'collision': '`compute session open/close/opened` beside `compute session create/.../destroy`, which are target sessions.'},
    {'term': 'Target', 'is': 'A pool member that hosts computers: a `compute serve` node offering sessions. Listed by GET /targets.', 'source': 'crates/compute-placement/src/targets.rs', 'collision': 'Pool members are also called providers.'},
    {'term': 'Provider', 'is': 'A pool member answering the capability API: local (in-process engine) or remote (compute.remote@1). Session providers (workspace, container) are the substrates inside a target.', 'source': 'crates/compute-provider/src/lib.rs',
     'collision': '"Provider" names three things: pool members, session substrates, and DNS providers.'},
    {'term': 'Runtime', 'is': 'A workload language runtime (wasm, node, python, …, native, shell) resolved from a pinned catalog; used by `compute run` and daemon workloads.', 'source': 'crates/compute-core/src/lib.rs#RuntimeKind',
     'collision': 'Not the computer substrate: computers run whatever the target host has on PATH.'},
    {'term': 'Project', 'is': 'Two different things: (a) a computer project — a ProjectSpec in contents: a repository plus build/test/commands/checks; (b) a bundle project — registered revisions of workload bundles, released to node environments.', 'source': 'crates/compute-core/src/computers.rs#ProjectSpec; crates/compute-state/src/model.rs#ProjectRecord',
     'collision': 'Same word, same API prefix (/environments/{e}/projects), dispatched by whether the environment has a computer.'},
    {'term': 'Application', 'is': 'Three things: (a) a process of kind application in a computer; (b) an `compute init/deploy` application with versions on a provider node; (c) a project in the reserved `applications` environment.', 'source': 'crates/compute-core/src/computers.rs#ProcessKind; crates/compute-environment/src/daemon/applications.rs', 'collision': 'Three meanings.'},
    {'term': 'Service', 'is': 'Three things: (a) a process of kind service in a computer; (b) a bundle workload of kind service; (c) a registered shared service (`compute service register`).', 'source': 'crates/compute-core/src/computers.rs; crates/compute-environment/src/model.rs#WorkloadKind', 'collision': 'Three meanings.'},
    {'term': 'Execution job', 'is': 'A durable job in a provider\'s job store (filesystem on the target), with a result and a receipt. Computer operations reference jobs by id; FeltDB stores the references and events, not the jobs.', 'source': 'crates/compute-provider/src/jobs.rs', 'collision': 'Daemon node executions are Execution records in control state; target jobs are not.'},
    {'term': 'Version / Rollout', 'is': 'Version: a published commit + package digest + assembly + step evidence. Rollout: a version made real in an environment (deploy/promote/rollback) with steps.', 'source': 'crates/compute-state/src/model.rs#VersionRecord,RolloutRecord',
     'collision': 'Parallel to bundle Revisions/Deployments and to application versions.'},
]

execution_paths = [
    {'path': '`compute run` (local)', 'where': 'the caller\'s machine, in process', 'authority': 'none (local user)', 'durable': 'execution record + receipt on disk', 'canonical_job_path': False},
    {'path': '`compute pool run/submit`, `compute remote *`', 'where': 'the provider placement chose', 'authority': 'provider: AllowAll on compute serve; daemon /compute/* uses scopes', 'durable': 'provider job store', 'canonical_job_path': True},
    {'path': '`compute session create/exec` (target sessions)', 'where': 'the target', 'authority': 'target AllowAll; owner = hash of Authorization', 'durable': 'target session/job stores', 'canonical_job_path': True},
    {'path': 'Computer operations (sync, install, build, start/stop, probe, inspect, publish steps)', 'where': 'the environment\'s computer', 'authority': 'daemon controller', 'durable': 'target jobs; evidence in FeltDB', 'canonical_job_path': True},
    {'path': '`environment exec/run/build/test/propose`', 'where': 'the environment\'s computer', 'authority': 'daemon scope + owner', 'durable': 'target jobs; events in FeltDB', 'canonical_job_path': True},
    {'path': 'Bundle project workloads (services, tasks) and releases', 'where': 'THE DAEMON HOST (supervisor)', 'authority': 'daemon scopes, no owner', 'durable': 'Execution records in control state', 'canonical_job_path': False},
    {'path': 'Applications (`compute deploy <dir>`)', 'where': 'a provider node offering deployments — the daemon host by default', 'authority': 'daemon scopes', 'durable': 'deployments in control state', 'canonical_job_path': False},
    {'path': 'Daemon /compute/* (node as provider)', 'where': 'the daemon host', 'authority': 'daemon execute scope', 'durable': 'daemon job store', 'canonical_job_path': True},
]

state = [
    {'what': 'Environments, computers, contents, work sessions, versions, rollouts, events, deployments, executions, credentials, audit', 'where': 'control state: file (default) or FeltDB', 'survives_daemon_restart': True, 'survives_machine_restart': 'yes (file on disk / FeltDB)'},
    {'what': 'Target sessions and jobs (the machine, its commands, their results and receipts)', 'where': 'the target\'s session and job stores on its disk', 'survives_daemon_restart': True, 'survives_machine_restart': 'target records yes; workspace processes no (restarted by reconciliation)'},
    {'what': 'Computer workspaces (checkouts, builds, process pid/log files)', 'where': 'the target host filesystem', 'survives_daemon_restart': True, 'survives_machine_restart': 'files yes; processes no'},
    {'what': 'Desired snapshot, read cache, computer/operation drivers, orphan sweep schedule', 'where': 'daemon memory (derived)', 'survives_daemon_restart': 'rebuilt', 'survives_machine_restart': 'rebuilt'},
    {'what': 'Runtime distributions', 'where': 'runtime store on each host', 'survives_daemon_restart': True, 'survives_machine_restart': True},
]

authorization = [
    {'operation': 'Read environments/computers/software', 'check': 'read scope; any operator reads any computer view (only mutations are owner-bound)', 'every_operation': True},
    {'operation': 'Change a computer environment (contents, config, lifetime, replace, destroy, sessions)', 'check': 'operate/deploy scope + owner', 'every_operation': True},
    {'operation': 'Exec / run / connect / propose', 'check': 'execute scope + owner', 'every_operation': True},
    {'operation': 'Publish / deploy / promote / rollback versions', 'check': 'deploy scope + owner of the environment(s)', 'every_operation': True},
    {'operation': 'Node environments, bundle projects, applications, domains', 'check': 'scopes only; no ownership', 'every_operation': True},
    {'operation': 'Loopback daemon without TLS', 'check': 'no credential required (development mode); `--production` requires TLS and credentials', 'every_operation': 'bypassable locally'},
    {'operation': 'Target (`compute serve`) jobs and sessions', 'check': 'NONE (AllowAllAuthorizer)', 'every_operation': False},
]

placement = {
    'requirements': ['cpu_count', 'memory_bytes', 'disk_bytes', 'architecture', 'network (none/localhost/network)', 'isolation (process/sandboxed/strict)',
                     'session capabilities (exec, terminal, filesystem, network, public_endpoint, persistent_storage, suspend, resume, claim)',
                     'target features (kvm, firecracker, containers, gpu, virtualization)', 'runtime/version/distribution/dependencies (workloads)', 'explicit target'],
    'failure_modes': ['no_compatible_provider with per-target reasons (cpu_unavailable, memory_unavailable, architecture_mismatch, network_unsupported, sessions_unsupported, session_capability_unsupported, target_feature_unsupported, …)',
                      'placed, then refused by the target\'s admission (resource_unavailable) — shown as a computer failure; wizard shows it',
                      'feature advertised but not usable (containers without an engine) — placed anyway'],
    'understands': {'gpu': 'label only', 'virtualization': 'label only', 'persistence': 'capability no provider offers', 'networking': 'network policy only', 'storage': 'disk bytes only', 'runtimes': 'workload runtimes yes; computer substrates no'},
}

documentation = [
    ('README.md', 'MISLEADING', 'Leads with `compute run script.py`, which fails for Python by default; the launcher and control plane come later.'),
    ('docs/getting-started.md', 'MISLEADING', 'Same first example; describes the workload engine, not the product.'),
    ('docs/architecture.md', 'OUTDATED → rewritten in this audit', 'Did not show targets, sessions, the file default, or the daemon-host execution paths.'),
    ('docs/audit-2026-09-25.md', 'OUTDATED (historical; replaced by docs/audit.md)', 'The 2026-09-25 audit predates computers, work sessions, versions.'),
    ('docs/platform-audit.md', 'OUTDATED', 'Historical (dated).'), ('docs/hardening-audit.md', 'OUTDATED', 'Historical (dated).'),
    ('docs/computers.md', 'MISLEADING', '"Lost machines are reported, not recreated" is true only when a process probe or lifecycle step notices.'),
    ('docs/environment-control-plane.md', 'DOCUMENTED CORRECTLY', 'Matches the verified journeys; its limitations list is accurate.'),
    ('docs/product-surface/README.md', 'DOCUMENTED CORRECTLY', 'Screenshots from the passing journey.'),
    ('docs/sessions.md', 'INCOMPLETE', 'Target sessions correct; does not explain work sessions or that targets are unauthenticated.'),
    ('docs/session-architecture.md', 'INCOMPLETE', 'Describes an authority (ProviderAuthorizer) that `compute serve` wires as AllowAll.'),
    ('docs/providers.md', 'DOCUMENTED CORRECTLY', 'Describes target feature detection as implemented (binary-on-PATH), which is itself the defect.'),
    ('docs/placement.md', 'DOCUMENTED CORRECTLY', ''), ('docs/feltdb.md', 'INCOMPLETE', 'The FeltDB contract is accurate; it does not say the default backend is a local file.'),
    ('docs/daemon.md', 'OUTDATED', '"Environments are the first screen" — the UI opens on the action home with Work/Manage.'),
    ('docs/control-plane.md', 'DOCUMENTED CORRECTLY', 'Spot-checked; notes computer environments.'),
    ('docs/environments.md', 'DOCUMENTED CORRECTLY', 'Node environments; points to computers.'),
    ('docs/applications.md', 'INCOMPLETE', 'Does not say `compute deploy <dir>` starts a control plane in ./.compute/daemon.'),
    ('docs/releases.md', 'DOCUMENTED CORRECTLY', 'Bundle releases (spot-checked; tests pass).'),
    ('docs/networking.md', 'INCOMPLETE', 'Ingress/domains apply to bundle projects only; not said.'),
    ('docs/jobs.md', 'DOCUMENTED CORRECTLY', 'Spot-checked.'), ('docs/remote-execution.md', 'INCOMPLETE', 'Does not state that `compute serve` is unauthenticated.'),
    ('docs/receipts.md', 'DOCUMENTED CORRECTLY', 'Spot-checked.'), ('docs/policy.md', 'DOCUMENTED CORRECTLY', 'Spot-checked.'), ('docs/admission.md', 'DOCUMENTED CORRECTLY', 'Spot-checked.'),
    ('docs/isolation.md', 'DOCUMENTED CORRECTLY', 'Workload isolation; says nothing about computers (which have none).'),
    ('docs/dependencies.md', 'DOCUMENTED CORRECTLY', 'Spot-checked.'), ('docs/capacity.md', 'DOCUMENTED CORRECTLY', 'Spot-checked.'), ('docs/provider-pools.md', 'DOCUMENTED CORRECTLY', 'Spot-checked.'),
]
documentation = [{'doc': d, 'status': s, 'notes': n} for d, s, n in documentation]

security = [
    {'id': 'SEC-1', 'severity': 'critical', 'finding': '`compute serve` has no authentication (AllowAllAuthorizer). Anyone who reaches a target lists every session and runs commands in any computer.', 'evidence': 'experiments.json#target_exec_without_credential, #bypass_file_seen_through_the_daemon'},
    {'id': 'SEC-2', 'severity': 'high', 'finding': 'The daemon presents no credential to its targets by default (pool token_env unset, including the launcher), so every computer is owned by "anonymous" on its target.', 'evidence': 'crates/compute-cli/src/launch_cmd.rs; crates/compute-provider/src/lib.rs#owner'},
    {'id': 'SEC-3', 'severity': 'high', 'finding': 'Workspace computers are directories under one OS user on one host: no filesystem, process, or network isolation between computers or from the target.', 'evidence': 'crates/compute-provider/src/sessions.rs#WorkspaceSessionProvider'},
    {'id': 'SEC-4', 'severity': 'medium', 'finding': 'A loopback daemon without TLS admits requests with no credential (development mode). The launcher runs this way.', 'evidence': 'crates/compute-environment/src/auth.rs; docs/daemon.md'},
    {'id': 'SEC-5', 'severity': 'medium', 'finding': 'Configuration values are passed as environment variables into every process and job in the computer; there is no secret type distinct from configuration.', 'evidence': 'crates/compute-environment/src/daemon/computers.rs#exec_in'},
    {'id': 'SEC-6', 'severity': 'low', 'finding': 'Any operator with read scope can read any computer view (desired contents, configuration keys, endpoints); only mutations are owner-bound.', 'evidence': 'crates/compute-environment/src/daemon/computers.rs#computer'},
]

performance = {
    'launch_cold_seconds': experiments['launch_seconds'], 'launch_warm_seconds': experiments['relaunch_seconds'],
    'control_plane_restart_seconds': 2.46, 'computer_create_to_running_seconds': experiments['computer_running_seconds'],
    'exec_submit_ms': experiments['exec_submit_ms'], 'exec_roundtrip_seconds': experiments['exec_roundtrip_seconds'],
    'project_inspection_seconds': experiments['propose']['seconds'], 'ui_home_ready_ms': 83, 'ui_work_ready_ms': 68,
    'full_product_journey_seconds': 57, 'notes': ['Every lifecycle event re-renders the whole current page (a full refetch).',
    'GET /software computes every computer view and queries rollouts per environment and the latest version per project on each call.',
    'Computer drivers re-read their records by identity every step; rollout drivers poll every 250 ms.', 'Debug build on a 4-CPU VM; not a benchmark.']}


def gap(id, area, current, desired, impact, evidence, next):
    return {'id': id, 'area': area, 'current': current, 'desired': desired, 'impact': impact, 'evidence': evidence, 'next': next}

gaps = [
    gap('G-ARCH-1', 'Core architecture', 'Targets accept any caller (AllowAllAuthorizer); the daemon authenticates to targets with nothing.', 'Targets trust only their control plane (a credential or mTLS), and sessions belong to the daemon\'s identity.', 'Anyone who reaches a target controls every computer on it; the daemon is not actually the authority.', 'SEC-1, SEC-2', 'Give `compute serve` a required credential and the pool a token for it; the launcher generates both.'),
    gap('G-ARCH-2', 'Core architecture', 'Three deployment models: applications, bundle projects (node environments), computer versions/rollouts.', 'One: versions reconciled into an environment\'s computer.', 'Three vocabularies, three code paths, and work that still runs on the daemon host.', 'models, execution_paths', 'Decide the fate of node environments and applications: port their features (zero-downtime switch, ingress, domains) to computers, then retire or wrap them.'),
    gap('G-ARCH-3', 'Core architecture', 'Default control state is a local file; FeltDB is opt-in.', 'FeltDB as the one authority, or the file backend stated as a development convenience everywhere.', 'Contradicts the durability contract; the launcher never uses FeltDB.', 'state-default-file', 'Decide and document; make `compute` use FeltDB when configured and say which it uses in the UI.'),
    gap('G-ARCH-4', 'Core architecture', 'Machine loss and target unreachability are not detected for computers without processes; the view keeps "running".', 'Every computer is periodically confirmed with its target; unreachable/lost is visible and actionable.', 'The UI shows healthy computers that do not exist.', 'experiments.json', 'Add a session liveness check to the controller\'s running step independent of processes; surface "unreachable".'),
    gap('G-RT-1', 'Runtime support', 'Computers are workspaces (native processes) or unverified containers.', 'Containers verified; microVMs (Firecracker/KVM); WASM sandboxes; GPU.', 'No isolation for computers; features advertised but not provided.', 'runtime matrix', 'Verify the container provider against a real engine in CI; then a Firecracker session provider.'),
    gap('G-RT-2', 'Runtime support', 'Target features describe the host (`containers` = docker on PATH), not the computer.', 'Features describe what a computer on the target can be, verified live.', 'Placement puts a "containers" computer in a workspace.', 'placement_refusals.containers', 'Split host features from substrate; check engine liveness.'),
    gap('G-PROV-1', 'Providers', 'No Fly/Railway/Render/cloud/bare-metal provisioning; targets must already run `compute serve`.', 'Provider adapters that materialize targets or computers.', '"Put software on Railway" is impossible.', 'provider matrix', 'Define a provisioning interface (materialize a target) and one adapter.'),
    gap('G-DISC-1', 'Placement', 'Targets are configured in a pool file; network, GPU model, nested virtualization are not discovered.', 'Discovery of machines and their capabilities.', 'Placement only knows what a file says.', 'discovery', 'Liveness-checked feature discovery; optional registration of targets with the control plane.'),
    gap('G-PLACE-1', 'Placement', 'persistent_storage, public_endpoint, terminal are requestable (UI checkboxes) but offered by no provider.', 'Either implemented or not offered.', 'Dead-end options.', 'placement_refusals', 'Hide unavailable options using GET /targets; implement persistent volumes and public endpoints.'),
    gap('G-EXEC-1', 'Execution', 'Bundle workloads and applications execute on the daemon host.', 'The daemon coordinates; computers execute.', 'The daemon is both coordinator and executor.', 'execution_paths', 'Covered by G-ARCH-2.'),
    gap('G-EXEC-2', 'Execution', 'No cancellation or timeout controls in the UI; jobs have timeouts in the API.', 'Cancel/retry for every job from every surface.', 'Stuck builds need the CLI or waiting.', 'api: POST /compute/jobs/{job}/cancel has no computer-level route', 'Add cancel for computer jobs and operations.'),
    gap('G-PROJ-1', 'Projects', 'Local folders must be Git repositories; no upload.', 'Any folder.', 'Non-Git projects cannot run.', 'run-a-project', 'Upload a folder as an artifact into the computer.'),
    gap('G-APP-1', 'Applications', 'Endpoints are target-host:port; no domains, TLS, or ingress for computer applications.', 'Public endpoints with domains and certificates.', 'Production traffic cannot reach computer applications properly.', 'endpoints, domains-tls', 'Route domains to computer endpoints through the existing network layer.'),
    gap('G-SVC-1', 'Services', 'Database/Redis are command templates that assume binaries on the host.', 'Managed service images/volumes.', 'Templates fail where binaries are absent.', 'ui TEMPLATES', 'Depends on container computers and volumes.'),
    gap('G-DEP-1', 'Deployment', 'A release restarts processes (downtime); bundle releases have zero-downtime switching.', 'Zero-downtime rollouts for computers.', 'Production updates interrupt traffic.', 'zero-downtime', 'Two instances behind a switched endpoint inside the computer.'),
    gap('G-REL-1', 'Releases', 'A version is a commit and a digest; no artifact is kept.', 'Stored, verifiable artifacts (build outputs) per version.', 'A version cannot be redeployed if the repository changes history.', 'artifact-store', 'Store the package (and optional build outputs) in the artifact store.'),
    gap('G-PROD-1', 'Production', 'No approvals, no protected environments, no deploy freezes.', 'Promotion policy per environment.', 'Anyone with deploy scope who owns both environments promotes.', 'approvals', 'Environment policy for promotion (approvals, required checks).'),
    gap('G-UI-1', 'UI', 'Targets, credentials, audit, node upgrades, runtimes, placement explanation are CLI-only.', 'Every capability visible.', 'Operators need the terminal for setup and diagnosis.', 'ui.not_in_ui', 'Add Manage pages for targets and access.'),
    gap('G-UI-2', 'UI', 'The browser certification package fails; browser tests do not run in CI.', 'Browser tests in CI.', 'UI regressions ship (one already did).', 'ui-certification', 'Install Chromium in CI; fix the certification for the new home route.'),
    gap('G-CLI-1', 'CLI', '52 commands have broken or missing help; `compute session` mixes target sessions and work sessions.', 'Accurate help; one session concept.', 'Discoverability.', 'cli.json help_defect', 'Fix clap doc comments; rename target sessions (e.g. `compute target session`).'),
    gap('G-API-1', 'API', 'No versioning of the Compute API; routes without any client (/info, /metrics, …).', 'A versioned, documented API.', 'Clients break silently.', 'api.json', 'Publish an API description generated from ROUTES.'),
    gap('G-AGENT-1', 'Agents', 'Agents are operators; no delegation, budgets, or per-agent audit identity.', 'Agent identities with bounded authority.', 'An agent with deploy scope can do everything a human can.', 'agent-identity', 'Scoped, expiring agent credentials tied to an owner.'),
    gap('G-SEC-1', 'Security', 'See SEC-1…SEC-6.', 'Real boundaries at the target and between computers.', 'Critical.', 'security', 'G-ARCH-1, then isolation via container/microVM substrates.'),
    gap('G-OBS-1', 'Observability', 'Process logs are read on demand; no log streaming, metrics, or traces for computers in the UI.', 'Live logs and metrics per application.', 'Operating production is blind between refreshes.', 'logs, metrics', 'Stream process logs through the daemon; surface /metrics.'),
    gap('G-DOC-1', 'Documentation', 'README and getting-started lead with a command that fails by default; daemon.md describes an old UI.', 'Docs lead with `compute` and verified journeys.', 'First impressions fail.', 'documentation', 'Rewrite the first pages around the verified journey.'),
    gap('G-TEST-1', 'Testing', '87 CLI commands are never invoked by a test; container provider, target auth, and machine loss have no real tests.', 'Every product claim executable.', 'Regressions in untested paths.', 'cli.json tests', 'Add the missing journeys to CI.'),
    gap('G-PERF-1', 'Performance', 'Every event re-renders and refetches the whole page; /software fans out per environment.', 'Incremental updates.', 'Fine at 3 computers; unmeasured at scale.', 'performance', 'Measure at 100 computers; add a software index.'),
]

base_vs_complete = [
    ('Computer abstraction', 'Environments with a durable computer (workspace on a `compute serve` target)', 'Any machine: container, microVM, VM, bare metal, cloud', 'Substrates beyond workspaces'),
    ('Persistent environments', 'Yes, verified', 'Yes', 'None'),
    ('Ephemeral environments', 'Yes, verified (expire, evidence kept)', 'Yes', 'None'),
    ('Self-discovery', 'CPU/memory/disk/OS/arch, runtimes, isolation facilities; features by device/binary presence', 'Machines, capabilities, networks, GPUs, virtualization — live', 'Liveness, networks, automatic target discovery'),
    ('Native runtime', 'Yes (workspace computers; process workloads)', 'Yes, isolated', 'Isolation'),
    ('Containers', 'Adapter exists; unverified against a real engine', 'Verified, default substrate', 'Verification, images, volumes, ports'),
    ('WASM', 'Workload engine (compute run); not a computer', 'WASM computers/sandboxes', 'Substrate'),
    ('Firecracker', 'Feature label only', 'MicroVM computers', 'Everything'),
    ('KVM', 'Feature label only', 'VM computers', 'Everything'),
    ('Capability placement', 'Yes, with reasons; features are labels', 'Yes, against verified capabilities', 'Verified features, storage, public endpoints'),
    ('Multiple projects', 'Yes, verified', 'Yes', 'None'),
    ('Application assembly', 'Proposal from source + GO, verified', 'Yes, plus services/images/volumes', 'Managed services'),
    ('Build', 'Yes, in the computer', 'Yes', 'None'),
    ('Test', 'Yes, in the computer', 'Yes', 'None'),
    ('Publish', 'Versions: commit + digest + evidence', 'Versions with stored artifacts', 'Artifact storage'),
    ('Deploy', 'Rollouts to environments, in place, verified', 'Zero-downtime', 'Traffic switching'),
    ('Promote', 'Reviewed plan + rollout, verified', 'With approvals and policy', 'Approvals'),
    ('Production', 'An environment named production; no protection', 'Protected environments, domains, TLS', 'Policy, ingress for computers'),
    ('Rollback', 'Yes, verified', 'Yes', 'None'),
    ('Operations', 'Logs (on demand), restart, config, probe health', 'Streaming logs, metrics, alerts, scaling', 'Observability, scaling'),
    ('Agent execution', 'AppPort covers every UI operation; agents are operators', 'Scoped agent identities', 'Delegation'),
    ('Provider abstraction', 'Pool of local/remote targets; no cloud adapters', 'Fly/Railway/Render/cloud/bare metal', 'Adapters'),
    ('UI', 'Work/Manage, home, run, software, operations; verified in a browser', 'Every capability', 'Targets, access, diagnosis pages'),
    ('CLI', '182 commands; 52 with help defects', 'Consistent, documented', 'Help, naming'),
    ('API', '128 routes, scoped', 'Versioned, documented', 'Description'),
    ('Observability', 'Events, receipts, job evidence, /metrics (API only)', 'Live logs, metrics, traces', 'Streaming, dashboards'),
    ('Durable evidence', 'Events, versions, rollouts, receipts; jobs on targets', 'Same, in one authority', 'Jobs outside FeltDB; file default'),
    ('Security boundary', 'Daemon: real; targets: none', 'Every hop authenticated; computers isolated', 'Target auth, isolation'),
]
base_vs_complete = [{'capability': c, 'base_capture': b, 'complete_compute': r, 'gap': g} for c, b, r, g in base_vs_complete]

readiness = [
    ('Install', 'PARTIAL', 'cargo build; release distribution certified in CI', 'No installer/package; runtimes download on demand'),
    ('Launch', 'PASS', 'compute → UI in 3.7 s', 'Browser opener only on desktops'),
    ('Discovery', 'PARTIAL', 'resources/runtimes/isolation real; features inferred', 'G-DISC-1, G-RT-2'),
    ('Placement', 'PASS', 'matching tests; reasons shown', 'G-PLACE-1'),
    ('Computer creation', 'PASS', '0.6 s to running (workspace)', 'Substrates (G-RT-1)'),
    ('Project execution', 'PASS', 'journey', 'Git-only sources'),
    ('Multi-project', 'PASS', 'journey', ''),
    ('Runtime coverage', 'PARTIAL', 'workload runtimes yes; computer substrates: workspace only verified', 'G-RT-1'),
    ('Sessions', 'PASS', 'CLI + provider tests', 'Two session concepts (G-CLI-1)'),
    ('Build', 'PASS', 'journey', ''), ('Test', 'PASS', 'journey', ''), ('Publish', 'PASS', 'journey', 'G-REL-1'),
    ('Deploy', 'PASS', 'journey', 'G-DEP-1'), ('Promote', 'PASS', 'journey', 'G-PROD-1'),
    ('Production', 'PARTIAL', 'an environment; no domains/TLS/approvals for computers', 'G-APP-1, G-PROD-1'),
    ('Rollback', 'PASS', 'journey', ''),
    ('Operations', 'PARTIAL', 'restart/logs/config/health probe', 'G-OBS-1, G-ARCH-4'),
    ('UI', 'PARTIAL', 'journey passes; certification package fails; no CI browser', 'G-UI-1, G-UI-2'),
    ('CLI', 'PARTIAL', '182 commands; 52 help defects; 87 untested through the CLI', 'G-CLI-1'),
    ('Agents', 'PARTIAL', 'AppPort parity (stub-tested)', 'G-AGENT-1'),
    ('Providers', 'PARTIAL', 'local + remote targets only', 'G-PROV-1'),
    ('Security', 'FAIL', 'unauthenticated targets demonstrated', 'G-ARCH-1, SEC-3'),
    ('Recovery', 'PARTIAL', 'daemon restart, target restart recover; machine loss undetected', 'G-ARCH-4'),
]
readiness = [{'area': a, 'status': s, 'evidence': e, 'blocking_gap': g} for a, s, e, g in readiness]

backlog = [
    ('FOUNDATION', ['Authenticate targets; the daemon holds the credential (G-ARCH-1)', 'Detect machine loss and unreachable targets (G-ARCH-4)', 'Decide the durable-state default (G-ARCH-3)', 'Browser tests and the UI certification in CI; fix the home-route regression (G-UI-2)']),
    ('EXECUTION', ['One deployment model: retire or port node environments and applications (G-ARCH-2, G-EXEC-1)', 'Cancel/retry for computer jobs and operations (G-EXEC-2)']),
    ('RUNTIME COVERAGE', ['Container computers verified in CI, with ports and volumes (G-RT-1)', 'Live, substrate-accurate target features (G-RT-2, G-DISC-1)', 'A microVM session provider (Firecracker)']),
    ('PROJECT/APP ASSEMBLY', ['Non-Git sources (G-PROJ-1)', 'Managed services on container computers (G-SVC-1)', 'Persistent storage and public endpoints, or hide them (G-PLACE-1)']),
    ('DEVELOPMENT WORKFLOW', ['Interactive terminal (PTY) and file editing', 'Streaming logs (G-OBS-1)']),
    ('RELEASE', ['Stored version artifacts (G-REL-1)']),
    ('DEPLOYMENT', ['Zero-downtime rollouts in computers (G-DEP-1)', 'Provider adapters that materialize targets (G-PROV-1)']),
    ('PRODUCTION', ['Domains/TLS/ingress for computer endpoints (G-APP-1)', 'Protected environments and approvals (G-PROD-1)']),
    ('OPERATIONS', ['Metrics and alerts in the UI (G-OBS-1)', 'Scaling short of replacement', 'Agent identities and delegation (G-AGENT-1)']),
    ('PRODUCT POLISH', ['README/getting-started around `compute` (G-DOC-1)', 'CLI help and naming (G-CLI-1)', 'Targets/access/diagnosis pages (G-UI-1)', 'API description (G-API-1)', 'Performance at scale (G-PERF-1)']),
]
backlog = [{'stage': s, 'items': i} for s, i in backlog]

audit = {
    'audit': {'date': '2026-09-27', 'commit': '69b70d9', 'branch': 'claude/great-galileo-z1xed0',
              'supersedes': 'docs/audit-2026-09-25.md (the audit of 2026-09-25 at 7a3a160)',
              'environment': experiments['environment'], 'status_vocabulary': STATUSES, 'journey_vocabulary': JOURNEY},
    'capabilities': capabilities, 'journeys': journeys, 'runtimes': runtimes,
    'cli': cli_out, 'api': api_out, 'ui': ui, 'tests': tests_out,
    'experiments': experiments,
    'models': models, 'execution_paths': execution_paths, 'state': state, 'authorization': authorization,
    'placement': placement, 'documentation': documentation, 'security': security, 'performance': performance,
    'gaps': gaps, 'base_vs_complete': base_vs_complete, 'readiness': readiness, 'backlog': backlog,
}
json.dump(audit, open(os.path.join(HERE, '..', '..', 'audit.json'), 'w'), indent=1)
print('capabilities', len(capabilities), 'journeys', len(journeys), 'cli', len(cli_out), 'api', len(api_out))
