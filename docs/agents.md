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
               ┌───────┴────────┐
               │                │
          stack-agent      Claude (future)
         (Chip + FX)        provider/API
               │                │
               └───────┬────────┘
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
never another's. An agent that must keep it from a model (stack-agent does) does not forward it.

## Agents

| Agent | Runtime | Status | Launcher |
| --- | --- | --- | --- |
| stack-agent | Rust (`github:rkendel1/stack-agent`: Chip + FX in one workspace) | the configured agent; **`--agent stack-agent`** (default) | `compute-configured-stack-agent` |
| Claude | external | future, not integrated | none |
| Codex | external | future, not integrated | none |

The npm Chip/Eve agent and Rust Chip from `chip-rs` that earlier releases shipped are retired: both
are stack-agent now.

### Selecting an agent

```sh
compute-configured-agent [--agent NAME] ARGS...      # NAME defaults to `stack-agent`
```

`stack-agent` runs `compute-configured-stack-agent ARGS...`. Any other name, `chip` included, is an
error; there is no fallback. Adding a name requires a concrete launcher; none is invented ahead of
one.

### stack-agent

stack-agent owns its agent loop, model interaction (FX), capability semantics and validation,
observations, evidence, goal satisfaction, work records and lifecycle. Compute only launches it and
serves the environment contract it publishes. `chip-core` and `chip-remote-env` do not depend on
Compute (checked by `cargo tree`; `tests/dependencies.rs`). Details: [stack-agent.md](stack-agent.md).

## Dependency direction

```text
Compute ───────▶ Agent executable          (compute-agent: argv/env only)
Compute ─ adapter ─▶ stack-agent's published environment contract (compute-stack-agent)
```

never `Compute ──▶ Chip internals`. `compute-stack-agent` is the thin adapter implementing
stack-agent's generic `EnvironmentProvider` on top of `compute-agent`; it imports no capability
semantics. stack-agent's `serve`/`work` path has no Cargo dependency on any Compute crate. (Its
`chip-compute` crate is an unrelated demo executor that shells out to the `compute` CLI; `serve` does
not use it.)
