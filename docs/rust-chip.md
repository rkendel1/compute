# Rust Chip as a Compute workload

**Terminology.** *Rust Chip* is the Rust autonomous-agent runtime implemented in
[`chip-rs`](https://github.com/rkendel1/chip-rs) (`chip-core`, the Rust FX, the Rust Chip CLI and
runtime service). *Chip/Eve* is the npm-based agent the configured distribution already ships
(npm Chip, npm FX, launched by `compute-configured-chip`). They are different products. This page is
only about Rust Chip. Nothing here replaces, wraps, renames or shares an executable with Chip/Eve.

| | Chip/Eve (existing) | Rust Chip (this page) |
| --- | --- | --- |
| Implementation | npm `@appport/chip`, npm FX | Rust `chip-rs`, Rust FX |
| Launcher | `compute-configured-chip` | `compute-configured-rust-chip` |
| Executable | `node_modules/.bin/chip` | `compute-rust-chip` |
| Declared in the distribution profile | yes (`agent`) | no (an optional workload, below) |

Rust Chip is the first agent on Compute's agent-neutral boundary ([agents.md](agents.md)):
`compute-configured-agent --agent chip` (the default agent) runs it.

Compute-configured already ships an npm/Eve agent. This integration additionally makes Rust Chip a
first-class Compute workload.

```text
                    Compute-configured
                 ┌──────────────────────┐
                 │  Chip/Eve + npm FX   │   unchanged
                 │                      │
                 │  Rust Chip workload  │
                 └───────┬──────────────┘
                         │ one Compute session per Rust Chip work
                    Compute env
              ┌──────────┼──────────┐
            project     tools      PAX
              └──────────┴──────────┘
                       Reality
```

Rust Chip is a standalone agent runtime. Compute-configured is an environment that can host it.
Compute provides the computer; Rust Chip provides the agency. Rust Chip has no dependency on Compute
(its `chip-core` and `chip-cli` build, test and run without it); this repository depends on Rust
Chip's *generic* environment contract, never the other way round.

## How it fits together

Rust Chip publishes a small contract in `chip-core` (`EnvironmentProvider`, `WorkEnvironment`,
`EnvironmentId`): a work acquires one environment, runs its whole trajectory in it, and releases it.
`crates/compute-rust-chip` implements that contract with the existing Compute primitive that gives one
workload an isolated computer plus `exec`: an ephemeral **session**.

| Contract | Compute primitive |
| --- | --- |
| `acquire` | `POST /compute/sessions` (placement, admission, a private workspace on a target), wait until ready, then load the project with a command in the session (`git clone`) |
| the work's operations | `exec(argv, env)`: a durable job with captured output and a receipt; no stdin |
| `release` | `POST /compute/sessions/{id}/destroy` (confirmed teardown; the record stays as evidence) |
| `isolation_capacity` | `COMPUTE_RUST_CHIP_MAX_ENVIRONMENTS` (default 2) |
| `EnvironmentId` | an opaque hash of the session id; never the session id, a path or a host |

**Who decides what.** Rust Chip validates every capability request and owns its meaning
(`project.write`, `project.git.*`, `pax.test`, the shape of each observation, how PAX's
`pax.execution-result.v1` is read, goal evaluation). To perform one, Rust Chip asks the environment
to run **its own executor**: `compute-rust-chip capability-exec --root project`, with the request in an
environment variable. Compute runs that command in the session and returns what it printed. Compute
does not know it is a `project.write`; Rust Chip does not reimplement anything in Compute.

**Receipts.** A Compute job receipt establishes that Compute ran a command. It is not a Rust Chip
receipt and never establishes that a goal was met. Completion comes from the observation Rust Chip
interprets (PAX's own result, after the last change), exactly as it does outside Compute.

**Isolation.** It is the session's: a private workspace per session on the target. Compute documents
the workspace substrate as *not a security boundary* beyond the isolation profile requested (see
[isolation.md](isolation.md)); this integration adds no sandbox and claims none. What it does
guarantee is that two concurrent works never share a mutable project, because each has its own
session, and the environment boundary in Rust Chip refuses to hand one environment to two works.

## Running it

```sh
# Compute-configured supplies the environment configuration; the model is Rust Chip's own.
export COMPUTE_RUST_CHIP_TARGET=http://127.0.0.1:8080          # a `compute serve` target
export COMPUTE_RUST_CHIP_TARGET_TOKEN_FILE=target.token       # its bearer credential
export COMPUTE_RUST_CHIP_PROJECT=/srv/projects/app            # what each session clones
export COMPUTE_RUST_CHIP_WORKER=/path/to/compute-rust-chip    # Rust Chip as the target sees it
export COMPUTE_RUST_CHIP_COMMAND_PATH=/usr/local/bin:/usr/bin:/bin   # tools the target's commands need
export CHIP_PROVIDER=openai-compatible CHIP_MODEL=… CHIP_ENDPOINT=…  # Rust FX, unchanged

compute-configured-rust-chip serve --port 8765 --max-concurrent-work 2
```

`serve` is Rust Chip's runtime service (see the `chip-rs` README): the same scheduler, queue,
lifecycle and event endpoints as `chip serve`, over Compute sessions instead of the local machine.
It refuses `--max-concurrent-work` above the provider's isolation capacity, and a work whose session
cannot be created fails with no model call and no execution.

The model, Rust FX and its credentials are configured exactly as for `chip work`; Compute supplies
network and environment variables and is not the model provider. Nothing in this path starts or
calls the npm Chip, the npm FX or the distribution's `agent` runtime.

## Packaging

The configured formula installs `compute-configured-rust-chip`, a launcher that runs
`libexec/rust-chip/bin/compute-rust-chip` and nothing else, and `compute-configured-agent`, which
maps `--agent chip` (the default) to it. The release pipeline builds `compute-rust-chip` with the
workspace and ships it in the configured asset as `rust-chip/bin/compute-rust-chip`, beside the
npm agent's files and never inside them. When an installation lacks the executable the launcher
says so and exits non-zero; it does not fall back to the npm agent, and the agent entry has no
other agent to fall back to. `distribution/tests/configured-rust-chip.sh` proves the launchers are
distinct and do not refer to each other, and, given an assembled asset, that the asset contains
the executable. The distribution profile (`stack.json`) still declares only the npm agent; that is
deliberate and unchanged.

## Limits

- Sessions are Compute's workspace substrate unless the target runs another session provider; that is
  not a security boundary (above).
- The project is loaded by `git clone` of an operator-configured source on the target. Sessions have
  no project loader of their own; a persistent environment's repositories do, but they belong to a
  persistent environment, not to one ephemeral session.
- A request that carries more than 120 KiB (Linux's per-variable limit, with margin) is refused, not
  truncated; output Compute truncated is refused too.
- Each operation is a durable Compute job, which is slower than a local call; a work makes many.
- A session whose destroy fails (the target unreachable at release) is recorded as a cleanup failure
  and left to its TTL (default 30 minutes). It does not change the work's result or hold its slot.
