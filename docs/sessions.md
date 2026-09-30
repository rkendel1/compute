# Compute sessions

> A Compute session is a portable, authorized, durable handle to an execution
> environment — not a VM-specific abstraction.

A session is "give me a computer I can run work on" as one Compute
operation. You ask for resources and a lifetime; Compute places the request
on a provider that can satisfy it, provisions an environment there, and
hands back durable identities. You then run commands in that environment,
read their output, and destroy it — without knowing whether the machine was
a local workspace, a container, a cloud VM, or something that does not exist
yet.

```sh
compute session create --cpu 2 --memory 2Gi --ttl 1h
compute session exec ses_… -- make test
compute session logs ses_…
compute session destroy ses_…
```

The architecture behind it — session, placement, node, provider, execution —
is in [session-architecture.md](session-architecture.md).

## When to use a session

| You want | Use |
| --- | --- |
| Run one workload and get its result and receipt | `compute run`, `compute remote run`, `compute pool run` |
| Submit work and collect it later | `compute remote submit`, `compute pool submit` ([jobs.md](jobs.md)) |
| A long-running service with releases and an endpoint | `compute deploy` ([applications.md](applications.md)) |
| **A temporary environment you run several commands in, with state between them** | **`compute session`** |

A session is for work that needs a place to live for a while: an agent that
checks out a repository, builds it, runs tests, inspects the results, and
tries again; a human debugging a build on a machine shaped like CI; a batch
of commands that share a working directory. When the work is done, the
environment is destroyed and the record stays as evidence.

## What a session is

```text
ComputeSession
├── session_id        ses_…      the session
├── node_id                      the pool provider that holds it
├── job_id            job_…      the durable job that provisioned it
├── execution_id      exec_…     that job's execution: evidence it ran under admission
├── owner                        the principal the Compute authority resolved
├── ownership                    ephemeral | claimed
├── resources                    cpu, memory, disk
├── network                      none | localhost | network
├── capabilities                 what this environment actually supports
├── connection                   how to reach it (mode, address)
├── endpoints                    exposed ports, when any were authorized
├── status                       the durable lifecycle state
├── created_at / ready_at / expires_at / ended_at
├── failure                      phase, provider, code, retryable, message
└── executions                   every command run in it (job_id, execution_id)
```

`session_id`, `job_id`, and `execution_id` are assigned and persisted when the
session is created, before any provider is asked for anything, and never
change — not when the provider is slow, not across a restart of the Compute
service.

## Lifecycle

```text
requested → provisioning → ready ⇄ running
                             │
                    stop     ▼        resume
                  ready → stopping → stopped → resuming → ready
                             │
  destroy (any live state) → destroying → destroyed
  TTL passes (ephemeral)   → expiring   → expired
  provisioning or reconciliation fails  → failed
```

| Status | Meaning |
| --- | --- |
| `requested` | Durable; nothing has been provisioned yet. |
| `provisioning` | The provider is building the environment, then the readiness execution runs in it. |
| `ready` | The environment is up and idle. |
| `running` | At least one command is executing. It returns to `ready` when they finish. |
| `stopping` / `stopped` | Active executions were cancelled; the environment and the record are kept. |
| `resuming` | The provider is resuming the *same* environment. |
| `expiring` / `destroying` | Teardown is durable and in progress; it is retried until the provider confirms. |
| `destroyed` / `expired` / `failed` | Terminal. A terminal session never changes status again. |

Every transition is written to durable storage before the provider is asked
to act, and each is recorded as an event (`compute session info --json` and
`GET /compute/sessions/{id}/events`). A restarted server reconciles every
in-flight state: provisioning resumes (provisioning is idempotent per
session), stopping, resuming, and teardown are repeated until they complete,
and an environment the provider no longer has is recorded as
`failed` / `environment_lost` rather than recreated.

## Commands

```sh
compute session create [--cpu N] [--memory SIZE] [--disk SIZE] [--ttl DURATION]
                       [--network none|localhost|network] [--isolation PROFILE]
                       [--require CAPABILITY]... [--expose PORT[/PROTO][:public]]...
                       [--provider ID | --policy auto|local|remote | --prefer-provider ID]
                       [--wait] [--json]
compute session list    [--provider ID] [--json]
compute session info    <session-id> [--json]          # alias: inspect
compute session connect <session-id> [--json]
compute session exec    <session-id> [--env K=V]... [--timeout D] [--detach]
                        [--receipt FILE] [--json] -- <command>...
compute session logs    <session-id> [--json]
compute session stop    <session-id>
compute session resume  <session-id>
compute session claim   <session-id>
compute session destroy <session-id>
```

`create` returns as soon as the session is durable, with its identities and
`status: requested`; `--wait` waits until it is `ready` (or `failed`).

Only `create` involves placement. Every other command takes the session ID
alone: the CLI asks the providers in your pool, and only the provider that
holds the session *for you* answers. `--provider` skips the search.

`exec` runs the command as a durable job and waits for it, printing the
command's output, its `job_id`, and its `execution_id`, and exiting with the
command's exit code — exactly like `compute remote run`. `--detach` prints
the identities and returns; follow the job with `compute remote status` or
`compute remote wait`, and fetch its receipt with `compute remote receipt`.

`info` shows the whole lifecycle in one place:

```text
$ compute session info ses_0a9d…

Session:     ses_0a9d6a4d2f62325d75fe4a7726a4f1a734bb18e1b325ee323105250700dee3ca
Status:      ready
Node:        node
Provider:    workspace (http://127.0.0.1:8080)
Job:         job_faecb3ade132d9a0516e6165abe3534202c3c5be9d707da178f14f981f37e9a2
Execution:   exec_1790476908558868495_1
Ownership:   ephemeral
Placement:   sha256:547e23fc…

Resources:
  CPU:        2
  Memory:     2 GiB
  Network:    network

Capabilities:
  exec                 yes
  terminal             no
  filesystem           yes
  network              yes
  public_endpoint      no
  persistent_storage   no
  suspend              yes
  resume               yes
  claim                yes

Connection:  exec
Created:     2026-09-27T02:41:48+00:00
Ready:       2026-09-27T02:41:48+00:00
Expires:     2026-09-27T03:41:48+00:00

Executions:  2 (0 active)
  job_45cd… exec Succeeded sh -c make test
  job_faec… provision Succeeded
```

## Placement

`compute session create` is placed by the same machinery as every other
submission ([placement.md](placement.md)). The session's environment is
described as a canonical shell workload — its resources, network, and
isolation — whose entrypoint is the readiness check. Placement derives
requirements from it with submission mode `session`, and evaluates:

- the runtime, isolation, network, and resource requirements, exactly as for
  a workload;
- whether the provider hosts sessions at all (`sessions_unsupported`);
- whether its session environments offer every capability required with
  `--require`, and `network` when the session asks for a network
  (`session_capability_unsupported`);
- the caller's execution policy, intersected with the provider's
  (admission), and provider health and capacity.

There is no second provider-selection mechanism, and nothing is retried on
another provider. `--provider`, `--policy`, and `--prefer-provider` mean
what they mean for `compute pool`. A failed placement exits with status 2
and explains every provider's reasons.

The selected provider re-evaluates admission against its own policy before
anything is provisioned. A denied session is never created.

## Capabilities

Every session reports what its environment actually supports. Capabilities
come from the provider for the environment it built; they are never inferred
from the provider's name.

| Capability | Meaning |
| --- | --- |
| `exec` | Commands can run in the session (`compute session exec`). |
| `terminal` | The provider offers an interactive terminal connection. |
| `filesystem` | Commands share a working directory that persists between them. |
| `network` | Commands can reach the network the session asked for. |
| `public_endpoint` | The provider can expose a port publicly. |
| `persistent_storage` | Storage outlives the session. |
| `suspend` | The provider can suspend the environment on `stop`. |
| `resume` | A stopped session can be resumed. |
| `claim` | The session can be claimed (kept beyond its TTL). |
| `process_tree_termination` | `stop` and `destroy` succeed only once every process the environment owns is confirmed gone, else they fail `termination_failed` ([lifecycle.md](lifecycle.md)). |

An operation the session's capabilities do not include fails with
`operation_unsupported`, and nothing is sent to the provider. A requirement
the provider cannot meet (`--require terminal` on a provider without
terminals, a public `--expose` on one without public endpoints) is refused at
creation. An unknown capability name is an error, never "not required".

## TTL, expiry, and claiming

Every session has a TTL (`--ttl`, default `1h`); `expires_at` is fixed and
durable at creation. When it passes, the server:

1. marks the session `expiring` (durably, before anything else);
2. authorizes the teardown through the Compute authority
   (`ProviderAuthorizer::authorize_expiry`); a refusal is recorded as a
   `failure` in phase `authorization` and nothing is torn down — the server
   retries;
3. cancels active executions and asks the provider to destroy the
   environment;
4. records the provider's answer; a failed teardown stays `expiring` with the
   failure recorded, and is retried;
5. marks the session `expired`, keeping the record, its events, and every
   execution's evidence.

Expiry does not depend on the server being up at the moment the TTL passes:
a server that starts after `expires_at` expires the session during
reconciliation. A provider-side timeout never makes Compute forget a
session; if the provider lost the environment, the session says so.

`compute session claim` moves an ephemeral session to persistent ownership,
where the provider supports it (`claim` capability). The owner does not
change — claiming changes how long the session lives, never who it belongs
to. A claimed session has no `expires_at` and lives until it is destroyed.

## Connection modes

Networking is modelled separately from the session. A session's
`connection.mode` says how it is reached; SSH is one transport among several
and none of them is the Compute protocol:

| Mode | Meaning |
| --- | --- |
| `exec` | Commands through `compute session exec` (durable jobs). Every provider that offers `exec` supports it. |
| `ssh` | An SSH endpoint. |
| `websocket` | A WebSocket session. |
| `terminal` | A provider-native terminal. |
| `port_forward` | Port forwarding. |
| `appport` | An AppPort connection. |
| `local_process` | A process handle on this machine. |

`compute session connect` returns the connection plus anything the provider
issued for this one connection (a command, short-lived credentials, an
expiry). That material goes to the authorized caller only: it is never
written to the session record or its events, and the human output of
`connect` only says that credentials were issued (`--json` returns them).

Endpoints are resources of a session, not a requirement of one. A session has
none unless the caller asked with `--expose`, and each requested endpoint is
its own authorization decision (`session_expose`).

## Providers

The provider is an execution backend implementing the session contract; it
is not an authority. `compute serve` hosts sessions with the built-in
**workspace** provider: each session is a private (`0700`) directory on the
node, and each command runs as a durable job with that directory as its
working directory and `$HOME`. The workspace is kept across `stop`/`resume`
and removed on destroy or expiry.

```sh
compute serve --listen 0.0.0.0:8080 --public-url https://node.example \
  --job-store /var/lib/compute/jobs --session-store /var/lib/compute/sessions \
  --credentials /var/lib/compute/target-credentials.json
```

`--offer` includes `sessions` by default; `--offer run,jobs` withholds them,
and a provider that does not offer them is `incompatible` for sessions with
`sessions_unsupported`. The Compute daemon (`compute start`) does not host
sessions yet: it keeps its durable state in FeltDB, and session records live
beside the `compute serve` job store.

`--session-provider container` hosts each session in its own container
instead (`--container-runtime docker|podman`, `--container-image`,
default `debian:stable-slim`): the workspace directory is mounted at
`/workspace`, `--cpu`/`--memory`/`--network none` become the container's
limits, commands run through `docker exec`, `stop` and `resume` stop and
start the same container, and destroy removes it and its workspace. A
container that vanished is reported lost, never silently recreated.

Two session fields exist for environment computers
([computers.md](computers.md)): `persistent` (created claimed, with no
TTL; requires the `claim` capability) and `reference`, an idempotency key
unique among the owner's live sessions — a create that names the reference
of a live session returns that session instead of making a second.

Other environments — Apple Container, Fly Machines, cloud VMs, local Linux
VMs, WASM runtimes — plug in by implementing `SessionProvider`
([session-architecture.md](session-architecture.md#the-provider-contract)).
The session API does not change when the provider does.

## Security

- **Authorization is Compute's.** Every session operation passes through the
  server's `ProviderAuthorizer` on every request (`session_create`,
  `session_inspect`, `session_exec`, `session_connect`, `session_destroy`,
  …), not once at creation. It fails closed.
- **Ownership comes from the authority.** The owner is the principal the
  authorizer resolves from the request's credential; nothing a client sends
  can set or change it. Another principal cannot list, inspect, connect to,
  run commands in, stop, claim, or destroy your session, or read the jobs it
  ran; to it, your sessions are `unknown_session`. `compute serve`
  authenticates every request with a target credential and resolves the
  principal to the control plane that credential names, so a session stays
  its control plane's across restarts and credential rotation
  ([remote-execution.md](remote-execution.md#target-credentials)).
- **Endpoints need a decision.** Each requested endpoint is authorized
  separately; a refusal creates nothing.
- **Provider credentials are never authority.** A provider's identifier for
  an environment (`provider_session_id`) is its handle, never a Compute
  identity, and never authorizes anything. Connection credentials are never
  persisted.
- **Destroyed means destroyed.** A terminal session never changes again. A
  provider answer that arrives after the session moved on (for example,
  provisioning that completes after a destroy) is discarded and the
  environment it built is torn down — it is never adopted, and a destroyed
  session is never recreated.
- **Policy applies.** The environment's contract is admitted under the
  caller's and the provider's execution policy before provisioning, and
  every command is admitted again as the job it is.

## Durable execution semantics

Every command run in a session — including the readiness check that makes it
`ready` — is an ordinary durable job ([jobs.md](jobs.md)), accepted through
the same path as `compute remote submit`. It has a `job_id` and an
`execution_id` (both reserved and recorded in the session before the job
exists), a lifecycle, logs, a result, and a receipt that verifies
independently. The job record carries the `session_id`; its receipt carries
the session's placement. `compute remote status|result|receipt|cancel` work
on a session's jobs exactly as on any other.

A restart during a command follows the job rules: the command's job becomes
`provider_interrupted`, never a fabricated success, and the session returns
to `ready`. A restart during provisioning resumes provisioning; if it
interrupted the readiness execution itself, the session fails with a
retryable `provider_interrupted` failure.

## Failures

A provider's failure is never reported as Compute's, and Compute's lifecycle
state is never a provider error. A failed session says where:

```json
"status": "failed",
"failure": {
  "phase": "provisioning",
  "provider": "workspace",
  "code": "provider_unavailable",
  "message": "session provider workspace: …",
  "retryable": true,
  "at": "2026-09-27T02:41:48Z"
}
```

`phase` is one of `placement`, `authorization`, `admission`, `provisioning`,
`connection`, `execution`, `stopping`, `resuming`, `claim`, `teardown`,
`expiration`, or `reconciliation`. Errors from session operations carry a
stable kind: `unknown_session`, `session_conflict` (the session's state does
not allow the operation, for example a command in a `stopped` session),
`operation_unsupported`, `unauthorized`, `admission_denied`, or the
provider's own error kind.

## Programmatic API

The protocol is `compute.remote@1`:

| Route | Operation |
| --- | --- |
| `POST /compute/sessions` | `session_create` (body: `{ "request": ProviderRequest, "spec": { "ttl_seconds", "required_capabilities", "endpoints" } }`) |
| `GET /compute/sessions` | `session_list` |
| `GET /compute/sessions/{id}` | `session_inspect` |
| `GET /compute/sessions/{id}/events` | `session_events` |
| `POST /compute/sessions/{id}/connect` | `session_connect` |
| `POST /compute/sessions/{id}/exec` | `session_exec` (body: `{ "command": [...], "env": {...}, "timeout": <milliseconds> }`) |
| `GET /compute/sessions/{id}/logs` | `session_logs` |
| `POST /compute/sessions/{id}/stop` · `/resume` · `/claim` · `/destroy` | lifecycle |

In Rust, `compute_provider::RemoteProvider` has a method per route
(`create_session`, `session_exec`, `session_logs`, `destroy_session`, …) and
`SessionCreateRequest::new` builds the request; `compute_placement::dispatch::create_session`
places it through a pool.

In TypeScript, `@compute/appport` wraps `compute session` for programs and
agents:

```ts
import { createComputeSessions } from "@compute/appport";

const compute = createComputeSessions({ poolConfig: "compute-pool.toml" });
const session = await compute.create({ resources: { cpu: 2, memory: "2Gi" }, ttl: "1h" });
const result = await session.exec(["make", "test"]);   // { jobId, executionId, exitCode, stdout, … }
await session.logs();
await session.destroy();
```

Both are clients of the same lifecycle: neither keeps state of its own.

## Non-goals

Sessions do not make SSH the Compute protocol, add a new authority system,
put provider-specific APIs in the public contract, manage VMs in general,
require persistent machines or public networking, or require every provider
to support suspend, resume, or claim. They do not replace AppPort or
AppBoundry.
