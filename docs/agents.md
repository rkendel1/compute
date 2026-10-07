# Agents on Compute

**Compute is an agent-neutral execution environment.**

> Agent = agency. Compute = computer.

An *agent* is a workload Compute can launch against a Compute environment. Compute provides the
computer; the agent provides everything above it. This is an architectural rule, not a convention:
nothing in Compute may depend on how an agent works.

## The rule

Compute **must not** know how an agent reasons, selects capabilities, represents goals, makes
decisions, verifies goals, manages memory, selects models or providers, recovers, or defines what
a capability means.

Compute **must** provide what an agent needs to operate:

| Primitive | Compute mechanism |
| --- | --- |
| environment / session creation | a Compute *session* on a target (ephemeral, owned, placed, private workspace) |
| project loading | a command in the session (`git clone <operator-configured source> project`) |
| command execution | `exec(argv, env)`: a durable job; **no stdin** |
| environment variables | the `env` of each `exec`, plus the host's operator-set command environment |
| captured stdout / stderr | the job result (output Compute truncated is refused, never passed on as an answer) |
| execution receipts | the job's durable receipt |
| isolation | one agent work → one session → one private project |
| lifecycle and cleanup | confirmed `destroy`; the session TTL bounds a destroy that failed |
| authoritative execution state | the job and session records |

**Compute does not determine whether an agent succeeded at its goal. It reports what actually
happened in the environment.** An exit code, captured output and a receipt are reality. Compute
never reads them as goal satisfaction, agent success, capability success or work completion; only
the agent can say what they mean.

```text
                    Compute
                       │
             agent-neutral runtime            crates/compute-agent
                       │
        ┌──────────────┼──────────────┐
        │              │              │
     Chip/Eve       Rust Chip       Claude (future)
        │              │              │
       FX           Rust FX       provider/API
        │              │              │
        └──────────────┼──────────────┘
                       │
                  Compute session
                       │
             filesystem / Git / tools
                       │
                    reality
```

## The contract

`crates/compute-agent` is the whole Compute-side concept, and it is small:

```text
AgentSpec   { name, program, args, env }          identity, launcher, arguments, environment
AgentHost   acquire() -> AgentSession              a session + the project, or nothing
AgentSession launch(&AgentSpec) / exec(argv, env)  -> Outcome { exit_code, stdout, stderr, job_id }
            release()                              confirmed teardown
```

It has no dependency on any agent runtime (`tests/dependencies.rs` fails if one appears). There is
no agent registry, no plugin system and no database: an agent is an executable and its arguments.
Richer interaction than argv/env/stdout (a model conversation, a tool protocol) is the agent's
business above `exec`; Compute gains no stdin for it.

**Failure.** Acquisition fails closed: a session created but not made usable (not ready, project
not loaded) is destroyed and no agent runs. A failed cleanup is counted (`cleanup_failed`) and never
changes a finished work's result. One agent's failure never touches another's session.

**Isolation** is the session's: two agents started from the same source repository get separate
private projects and never see each other's work, identity or output; the source is never modified.
Compute documents the workspace substrate as *not a security boundary* beyond the isolation profile
requested ([isolation.md](isolation.md)); this contract adds no sandbox and claims none. One fact
to know: Compute's session execution contract gives every command in a session *its own* session's
`COMPUTE_SESSION_ID`, `COMPUTE_SESSION_WORKSPACE` and `HOME`. That is the agent's own computer,
never another's. An agent that must keep it from a model (Rust Chip does) does not forward it.

## Agents

| Agent | Runtime | Status | Launcher |
| --- | --- | --- | --- |
| Chip/Eve | npm (`@appport/chip`, npm FX) | existing, unchanged | `compute-configured-chip` |
| Rust Chip | Rust (`chip-rs`, Rust FX) | first new agent; **`--agent chip`** | `compute-configured-rust-chip` |
| Claude | external | future, not integrated | none |
| Codex | external | future, not integrated | none |

Chip/Eve and Rust Chip are different products and never route through each other. In prose:
*Chip/Eve* is the existing npm runtime; *Rust Chip* is the Rust implementation.

### Selecting an agent

```sh
compute-configured-agent [--agent NAME] ARGS...      # NAME defaults to `chip`
```

`chip` is Rust Chip: the entry runs `compute-configured-rust-chip ARGS...`. An unknown name is an
error; there is no fallback to another agent, and in particular never to Chip/Eve, which keeps its
own launcher and its own `agent` section in the distribution profile (`stack.json`), both
unchanged. The name `chip` in `--agent chip` is this entry's namespace; the profile's
`agent.runtimes[].name == "chip"` still means the npm runtime. Adding a name requires a concrete
launcher; none is invented ahead of one.

### Rust Chip

Rust Chip stays a separate runtime and owns its agent loop, model interaction, FX, capability
semantics and validation, observations, evidence, goal satisfaction, recovery and work lifecycle.
Compute only launches it and serves the environment contract it publishes. `chip-core` and
`chip-remote-env` do not depend on Compute (checked by `cargo tree`; `tests/dependencies.rs`).
Details: [rust-chip.md](rust-chip.md).

## Dependency direction

```text
Compute ───────▶ Agent executable          (compute-agent: argv/env only)
Compute ─ adapter ─▶ Rust Chip's published environment contract (compute-rust-chip)
```

never `Compute ──▶ Chip internals`. `compute-rust-chip` is the thin adapter implementing Rust
Chip's generic `EnvironmentProvider` on top of `compute-agent`; it imports no capability semantics.
Rust Chip's `serve`/`work` path has no Cargo dependency on any Compute crate. (`chip-cli` links
`chip-compute`, an unrelated demo executor in `work_demo.rs` that shells out to the `compute` CLI;
it is not used by `serve`.)
