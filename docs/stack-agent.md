# stack-agent: the configured agent

[stack-agent](https://github.com/rkendel1/stack-agent) is one Rust product: Chip agent orchestration
and FX inference, built from one workspace and one lockfile. It is the only agent
`compute-configured` ships. Compute hosts it as `compute-stack-agent`, behind the
`compute-configured-stack-agent` launcher and the agent entry `compute-configured-agent` (whose
default, and only, agent is `stack-agent`).

| | |
| --- | --- |
| Source | `github:rkendel1/stack-agent`, pinned to one full commit in `crates/compute-stack-agent/Cargo.toml` and in the profile (`stack.json` → `agent`) |
| Executable | `compute-stack-agent` (`serve`, and the in-session worker `capability-exec`) |
| Launcher | `compute-configured-stack-agent` |
| Model | stack-agent's own FX, in process: `CHIP_PROVIDER`, `CHIP_MODEL`, `CHIP_ENDPOINT`, `CHIP_API_KEY` |
| Where its capabilities run | one Compute session per work, on a `compute serve` target |
| State | its works, in a FeltDB database (below) |

The npm Chip/Eve agent and the npm FX that earlier releases shipped (`compute-configured-chip`) are
gone: their packages are not in the pinned set, and no launcher, profile entry or build step refers to
them.

```text
                    compute-configured
            ┌─────────────────────────────────┐
            │ compute-stack-agent serve       │  the agent loop + FX + the model key
            │   (loopback HTTP, /v1/work)     │
            └───────────────┬─────────────────┘
                            │ one Compute session per work
                     Compute target
              ┌─────────────┼──────────────┐
            project       tools           PAX
              └─────────────┴──────────────┘
                           Reality
```

stack-agent depends on nothing in Compute. Compute depends only on its generic environment
contract (`chip-core`), never the other way round.

## How it fits together

stack-agent publishes a small contract in `chip-core` (`EnvironmentProvider`, `WorkEnvironment`,
`EnvironmentId`): a work acquires one environment, runs its whole trajectory in it, and releases it.
`crates/compute-stack-agent` implements that contract with the existing Compute primitive that gives one
workload an isolated computer plus `exec`: an ephemeral **session**.

| Contract | Compute primitive |
| --- | --- |
| `acquire` | `POST /compute/sessions` (placement, admission, a private workspace on a target), wait until ready, then load the project with a command in the session (`git clone`) |
| the work's operations | `exec(argv, env)`: a durable job with captured output and a receipt; no stdin |
| `release` | `POST /compute/sessions/{id}/destroy` (confirmed teardown; the record stays as evidence) |
| `isolation_capacity` | `COMPUTE_STACK_AGENT_MAX_ENVIRONMENTS` (default 2) |
| session resources | 1 CPU; memory `COMPUTE_STACK_AGENT_SESSION_MEMORY` in bytes (default 1 GiB), admitted against what the target has available |
| `EnvironmentId` | an opaque hash of the session id; never the session id, a path or a host |

**Who decides what.** stack-agent validates every capability request and owns its meaning
(`project.write`, `project.git.*`, `pax.test`, the shape of each observation, how PAX's
`pax.execution-result.v1` is read, goal evaluation). To perform one, stack-agent asks the environment
to run **its own executor**: `compute-stack-agent capability-exec --root project`, with the request in an
environment variable. Compute runs that command in the session and returns what it printed. Compute
does not know it is a `project.write`; stack-agent does not reimplement anything in Compute.

**Receipts.** A Compute job receipt establishes that Compute ran a command. It is not a stack-agent
receipt and never establishes that a goal was met. Completion comes from the observation stack-agent
interprets (PAX's own result, after the last change), exactly as it does outside Compute.

**Isolation.** It is the session's: a private workspace per session on the target. Compute documents
the workspace substrate as *not a security boundary* beyond the isolation profile requested (see
[isolation.md](isolation.md)); this integration adds no sandbox and claims none. What it does
guarantee is that two concurrent works never share a mutable project, because each has its own
session, and the environment boundary in stack-agent refuses to hand one environment to two works.

## Running it

```sh
# The environment: where sessions run and what each one loads. The model is stack-agent's own.
export COMPUTE_STACK_AGENT_TARGET=http://127.0.0.1:8080          # a `compute serve` target
export COMPUTE_STACK_AGENT_TARGET_TOKEN_FILE=target.token       # its bearer credential
export COMPUTE_STACK_AGENT_PROJECT=/srv/projects/app            # what each session clones
export COMPUTE_STACK_AGENT_COMMAND_PATH=/usr/local/bin:/usr/bin:/bin   # tools the target's commands need
export CHIP_PROVIDER=openai-compatible CHIP_MODEL=… CHIP_ENDPOINT=… CHIP_API_KEY=…

compute-configured-stack-agent serve --port 8765 --max-concurrent-work 2
```

| Variable | Meaning | Default |
| --- | --- | --- |
| `COMPUTE_STACK_AGENT_WORKER` | the worker executable, as the target sees it | the installed `compute-stack-agent` |
| `COMPUTE_STACK_AGENT_NETWORK` | the network each session asks for: `none`, `localhost` or `network`. A narrower policy needs a target whose session provider enforces it, and is refused (never weakened) by one that cannot | `network` |
| `COMPUTE_STACK_AGENT_MAX_ENVIRONMENTS` | sessions owned at once | 2 |
| `COMPUTE_STACK_AGENT_SESSION_MEMORY` | memory per session, bytes | 1 GiB |
| `COMPUTE_STACK_AGENT_STATE_DB` | the FeltDB database works are kept in | `$COMPUTE_HOME/stack-agent/works.feltdb` |

`serve` is stack-agent's runtime service: the same scheduler, queue, lifecycle and event endpoints as
`stack-agent serve`, over Compute sessions instead of the local machine. It refuses
`--max-concurrent-work` above the provider's isolation capacity, and a work whose session cannot be
created fails with no model call and no execution.

**Loopback only.** The service has no authentication of its own (it is a local trusted-client
interface), so `compute-stack-agent` refuses any `--host` that is not loopback before anything starts.

**What reaches the agent.** The launcher starts it from an allow-listed environment: `PATH`, `HOME`,
locale and proxy settings, `COMPUTE_HOME`, `COMPUTE_DAEMON[_TOKEN]`, `CHIP_*`, `OPENAI_API_KEY`,
`ANTHROPIC_API_KEY` and `COMPUTE_STACK_AGENT_*`. Every other variable in the caller's shell (other
services' tokens, cloud keys) is unset. Inside a session, a command sees only the command environment
the operator configured (`COMPUTE_STACK_AGENT_COMMAND_PATH`, `_CARGO_HOME`, `_RUSTUP_HOME`): the model
key never enters a session (`crates/compute-stack-agent/tests/launcher.rs`,
`the_provider_key_reaches_the_model_and_nothing_compute_stores_or_the_service_reports`).

## From the Work page

On a configured install there is nothing to start or export. The Work page's **Agent** panel is a goal
box; type a goal and press Run. The first time, it asks once where the model is (a provider, a model and
an endpoint, plus a key if the model needs one), and that is the only thing Compute cannot work out.

What Compute does on the first goal for an environment (`crates/compute-environment/src/agent_runtime.rs`):

| The service needs | Compute supplies |
| --- | --- |
| `COMPUTE_STACK_AGENT_TARGET` and its token file | the pool's first remote member (your `this-machine`) and its credential file |
| `COMPUTE_STACK_AGENT_PROJECT` | the first repository of the environment the goal is for |
| `COMPUTE_STACK_AGENT_WORKER` | the executable the install's profile names |
| the model (`CHIP_*`) | what you saved on the page, else the daemon's own `CHIP_*` variables |
| a port and a work database | a free loopback port and a per-project database under the node's state directory |

It starts `serve` from an empty environment (nothing else of the daemon's reaches the agent), waits for
`GET /health`, and relays the goal. One service runs per project; changing the model stops them and the
next goal starts fresh ones. A service that dies as it starts is reported with its own last output. The
daemon finds the install even when started as plain `compute start`, by the Homebrew layout it lives in.
The saved model is a file in the node's state directory (`agent/model.json`) that only the daemon's user
can read; the API never returns the key. Nothing else is stored: the services are working state.

| Route | Does |
| --- | --- |
| `GET /agent?environment=` | what the panel shows: whether there is an agent, whether the model is set, whether it is running (reads start nothing) |
| `POST /agent/model` | save `{provider, model, endpoint, api_key}`; the key is never echoed |
| `POST /agent/work` | `{goal, environment}`: start the service if needed, then `POST /v1/work` with only the goal |
| `GET /agent/work/{work}`, `…/events`, `POST …/cancel` | the service's work record, events and cancellation (`?environment=`) |
| `POST /agent/address` | use an agent service you run yourself instead: register `{url}` (empty resets) |

A registered address (under **Use a different agent service**) takes precedence over the native agent;
without one on a base install, `COMPUTE_STACK_AGENT_URL` and then `http://127.0.0.1:8765` apply. Only a
plain `http` loopback address is accepted, because the service has no authentication of its own. The
service runs each goal in its own session, never in the computer whose page you are looking at. To run it
yourself, see "Running it" above.

## Packaging

The configured formula installs `compute-configured-stack-agent` and `compute-configured-agent`. The
release pipeline builds `compute-stack-agent` with the workspace and ships it in the configured asset
as `stack-agent/bin/compute-stack-agent`. When an installation lacks it, the launcher says so and exits
non-zero. `distribution/tests/configured-stack-agent.sh` proves the launcher, its environment
allow-list and the agent entry, and, given an assembled asset, that it contains the executable and
nothing of the npm agent. `compute-configured-verify` checks the profile's pin against the source tree
or runs the installed executable, including its refusal of a non-loopback listener.

## State

Every work is persisted as it runs (its intents, observations, events with provenance, and the
verdict) in the FeltDB database above; the service says where when it starts. A work still running
when the service stopped is reported as interrupted at the next start. Nothing resumes on its own, and
there is no exactly-once guarantee for effects: running an interrupted work's goal again may repeat a
capability that already ran (stack-agent `docs/known-issues.md`, C).

## Limits

- Sessions are Compute's workspace substrate unless the target runs another session provider; that
  is not a security boundary ([isolation.md](isolation.md)).
- The project is loaded by `git clone` of an operator-configured source on the target.
- A request that carries more than 120 KiB is refused, not truncated; output Compute truncated is
  refused too.
- Each operation is a durable Compute job, which is slower than a local call; a work makes many.
- A session whose destroy fails is recorded as a cleanup failure and left to its TTL (default 30
  minutes). It does not change the work's result or hold its slot.
- stack-agent inside Compute's WASM runtime is not supported: run it natively, as above (compute#94).

## Cancellation

A request to stop work, reported as what actually stopped (compute#90):

- `compute_stack_agent::cancel::request(service, work)` calls stack-agent's `POST /v1/work/{id}/cancel`.
  A queued work is removed before it starts and never ran. A running work is asked to stop
  (`cancellation_requested`): no further model call is made, but what is in flight is not
  interrupted, and the work ends however Chip establishes. A finished work is unchanged.
- `ComputeSessionEnvironments::cancel_work(work)` (and `compute_agent::AgentSession::cancel`)
  cancels the work's running Compute commands with Compute's own job cancellation, which is
  idempotent and confirmed: a command is reported cancelled only when Compute recorded it
  cancelled, and one cancelled while it ran is reported as possibly effective, never as not
  having run. A command whose caller stops waiting is cancelled, not left running.
- An effect that already happened stays done. Nothing is retried or replayed.

## Work records (decision)

stack-agent owns the live agent loop and its own work records (FeltDB, above): what a work intended,
observed and concluded. Compute's control state stays the authority for Compute's own work: sessions,
jobs and receipts. Neither replays the other's records, and no automatic retry or replay is added as a
shortcut to recovery.
