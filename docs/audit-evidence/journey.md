# Audit evidence: journeys run during the audit

Recorded 2026-09-25 on the audit machine: Linux x86_64 container, 4 vCPU,
debug build of this branch (`target/debug/compute`). Host tools: Node 22,
Python 3.11. Outbound network goes through a proxy that allows `nodejs.org`
and `github.com` but denies `dl-cdn.alpinelinux.org`. Apple Container was
**not** available. Output is trimmed; nothing was edited.

## 1. `compute init` + `compute run` with no pool configuration

```text
$ compute init my-app
Initialized application my-app
Run: compute run my-app
Deploy: compute deploy my-app

$ compute run my-app
runtime error: no provider proved it satisfies this workload contract and is
admitted by policy (1 evaluated: 1 incompatible, 0 policy-denied, 0 excluded)

$ compute run my-app --json   # providers[0].candidate.reasons
["jobs_unsupported", ...]
```

The implicit pool contains only `local`, and the local provider does not
accept durable jobs. `compute run APP` therefore needs a remote provider.

## 2. `compute deploy` on a machine without the exact catalog runtime (before the fix)

```text
$ compute runtimes | grep node
node  24.18.0  linux-x86_64  sha256:9d72…  available
$ compute deploy my-app
Compute daemon daemon_07da… started (pid 8922) at http://127.0.0.1:8787
my-app
Status       failed
Version      v1
Endpoint     http://127.0.0.1:20000
runtime error: app failed before it was ready: denied: the provider cannot
execute this contract: runtime_unavailable: node
```

After a one-off `compute run` of an unrelated Node script (the pool path
prepares runtimes), `compute runtimes` reported `node … ready`, and the same
`compute deploy my-app` succeeded as `v2`. The fix in this PR prepares the
runtime on the service path, too.

## 3. `compute deploy` journey after the fix (fresh runtime store)

```text
$ export COMPUTE_RUNTIME_STORE=$(mktemp -d)
$ compute init demo && compute deploy demo          # 6.4 s wall time
Status       running
Version      v1
Endpoint     http://127.0.0.1:20000
Provider     local
Deployment   dep_0cae9bfdd9230feb4f0e8d41
$ curl http://127.0.0.1:20000
Hello from Compute
$ compute runtimes | grep node
node  24.18.0  …  ready
```

Continuing the journey on the first app (`my-app`):

```text
$ compute logs my-app
listening on 30000                  # the instance port, not the endpoint port
$ sed -i 's/Hello from Compute/Hello v2/' my-app/server.js
$ compute deploy my-app             # -> v3, curl: "Hello v2"
$ compute rollback my-app 2         # -> v4, curl: "Hello from Compute"
$ compute history my-app
VERSION STATUS  PROVIDER DEPLOYMENT
v4      running local    dep_7405…
v3      running local    dep_f01d…    <- superseded, still labelled running
v2      running local    dep_9c9f…    <- superseded, still labelled running
v1      failed  local    dep_f7b6…
$ compute deployment receipt dep_7405… | jq .execution_receipts
[]                                   # service executions are not bound
$ compute stop my-app               # Status stopped; curl: connection reset
```

Execution receipts do exist (`GET /environments/applications/projects/my-app/receipts`)
and each names its deployment in the control record, but the deployment
receipt is written when the release completes, before its service execution
ends, so it never lists them.

## 4. Receipt fetched over the daemon API does not verify offline

```text
$ curl -s http://127.0.0.1:8787/receipts/sha256:1bd9… > r.json
$ compute receipt verify r.json
invalid execution receipt: invalid canonical encoding
```

`views.rs::receipt` parses the stored bytes into `serde_json::Value` and
re-serialises them, which reorders keys; `compute receipt verify` requires the
canonical byte encoding.

## 5. `--provider auto` as documented

```text
$ compute run .container/runtime-catalog/apps/deno --provider auto --json
placement_failed: provider_not_configured: provider auto is not configured in this pool
```

`--provider` help reads "auto, local, remote, or provider:<id>", but
`pool.rs::parse_placement_policy` treats every `--provider` value as a literal
provider ID. `--policy auto` (or no flag) is the working spelling.

## 6. `compute run APP` against a remote provider on plain Linux (no Apple Container)

```text
$ compute serve --listen 127.0.0.1:18080 --job-store ./jobs &
$ cat pool.toml
[providers.linux-a]
kind = "remote"
endpoint = "http://127.0.0.1:18080"
application_endpoint = "http://127.0.0.1:3000"
$ compute run examples/compute-demo --pool-config pool.toml
Status running · Provider linux-a · Runtime node 24.18.0 · Endpoint http://127.0.0.1:3000
$ curl http://127.0.0.1:3000          -> Hello from Compute
$ compute logs …                      -> compute-demo listening on 3000
$ compute stop …                      -> Status stopped, Receipt sha256:6a34…
$ compute history …                   -> linux-a node Cancelled job_a669…
```

Two applications on the same provider:

```text
$ compute run a --pool-config pool.toml   -> Status running, Endpoint http://127.0.0.1:3000
$ compute run b --pool-config pool.toml   -> Status running, Endpoint http://127.0.0.1:3000
$ compute status b                        -> Status failed   (EADDRINUSE in its logs)
```

The run-path endpoint is the provider's configured `application_endpoint`
string; it is not allocated per application.

## 7. Controller loss and restart (deploy path)

```text
$ kill -9 <controller pid>
$ curl http://127.0.0.1:21000       -> Hello v2   (supervisor keeps serving)
$ compute start --detach …          (same --state-dir)
$ compute status hello-api          -> Status running, Version v4
```

## 8. Runtime catalog fixtures

`.container/runtime-catalog/apps/*` could not be run as documented
(`--provider auto`, see 5). The Node path was exercised by journeys 3 and 6
(Node 24.18.0 acquired, verified, executed); Python 3.13.15 by the `worker`
deployment used for the UI screenshots. Deno, Bun and the Alpine-hosted
`shell` distribution were not exercised: `dl-cdn.alpinelinux.org` is denied
by this environment's network policy.

## 9. Workspace tests

`cargo test --workspace --no-fail-fast` on this machine: 25 failures, all
reproduced identically on the unmodified base commit (`git stash`), so none
were introduced by this PR:

- 21 in `compute-environment` (`availability`, `control_plane`,
  `environments`, `executions`, `scale`): their fixtures use the `shell`
  runtime, whose catalog distribution is a static busybox downloaded from
  `dl-cdn.alpinelinux.org`, which this network denies (HTTP 403 at the proxy).
- 2 in `compute-cli` (`dependency_capsules_*`, `capsule_binding_*`): the
  fixture capsule records the host's Python 3.11 and the catalog resolves
  Python 3.13.
- 2 in `compute-placement` (`dependencies::dependency_capsule_matrix`,
  `pool::placement_selects_only_providers_that_satisfy_each_workload`): both
  assert which local workloads are placeable; on this host one fewer is. The
  exact cause was not isolated. Both fail identically on the base commit.

All other test binaries passed. None of these tests runs in CI (see
[../audit.md#test-evidence](../audit.md#test-evidence)).
