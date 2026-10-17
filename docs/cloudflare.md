# Cloudflare Workers provider

Compute can run short-lived, request-driven workloads on Cloudflare
Workers, including on Cloudflare's free plan. It is an execution provider
behind the same boundary as every other one:

```text
Compute contract ──▶ Cloudflare provider adapter ──▶ Cloudflare API / Workers runtime
 (workload, admission,     (compute-provider-cloudflare)
  placement, receipts)
```

Compute stays the authority. Cloudflare holds the Workers Compute deployed;
it holds no Compute state. Admission, placement, and evidence are Compute's,
and they work exactly as they do for local and remote providers.

**This is not free general-purpose compute.** A Worker is a V8 isolate that
answers one request at a time. It is not a Linux VM and not a container.
The free plan gives a low-cost entry point for workloads that fit it, with
the same portable contract, and an explicit path to other providers for
workloads that don't.

## What runs, and what doesn't

The `workers-free` profile runs **WASI (preview 1) command modules**, one
invocation per request. Compute deploys each module as a Worker, together
with a small harness that runs the module and returns its exit code,
stdout, and stderr. Args, environment, and stdin arrive with each
invocation.

| Workload needs | On `workers-free` |
| --- | --- |
| A WASI module (`runtime: wasm`), request in, result out | **Supported** |
| Arguments, environment variables, stdin, stdout/stderr, exit code | **Supported** |
| A wall-clock timeout (`resources.timeout_ms`) | **Supported** (Compute stops waiting; Cloudflare cancels the request) |
| Node, Python, Ruby, Bun, Deno, PHP, JVM, .NET | **Unsupported** (`process_runtime`): these run as processes |
| A native executable (ELF, Mach-O) | **Unsupported** (`native_executable`) |
| A shell, a terminal, a persistent process, a server | **Unsupported** (`interactive_shell`, `persistent_process`) |
| A container image | **Unsupported** (`container`) |
| Input files, output files, scratch disk, dependency capsules | **Unsupported** (`filesystem`) |
| Outbound network from the workload | **Unsupported** (`outbound_network`) |
| A chosen memory or CPU-time limit | **Refused** (`capability_mismatch`): Cloudflare enforces fixed plan limits, not chosen ones |
| `strict` isolation | **Refused**: it needs chosen limits |
| Sessions, durable jobs, application deployments | **Not offered**: placement refuses them with `sessions_unsupported`, `jobs_unsupported`, `deployment_unsupported` |
| Local model inference | **Unsupported** (`local_inference`) |

Every refusal is explicit. A refused workload is never weakened, never moved
to another provider, and nothing is sent to Cloudflare.

### Workers, Containers, and VMs

| | Cloudflare Workers (this provider) | Cloudflare Containers | A VM or a Compute target |
| --- | --- | --- | --- |
| Unit | V8 isolate, per request | Linux container | Linux machine |
| Runs | JavaScript / WebAssembly | Any image | Anything |
| Lifetime | One request | Long-running | Long-running |
| Free plan | Yes (limits below) | No (paid) | n/a |
| In Compute | `kind = "cloudflare"` | Not implemented | `kind = "remote"` (`compute serve`) |

Cloudflare Containers is a separate product with a different contract.
This provider does not use it and never pretends to offer containers.

## The free-tier profile

`compute cloudflare profile` shows it; `--json` makes it machine-readable.
The limits are Cloudflare's, as [Cloudflare documents
them](https://developers.cloudflare.com/workers/platform/limits/), checked
2026-10-17:

| Limit (Workers Free) | Value | What happens at the limit |
| --- | --- | --- |
| CPU time per invocation | 10 ms | Cloudflare stops it (error 1102): the execution is `killed` with `resource_limit` |
| Requests per day (account) | 100,000, reset 00:00 UTC | Cloudflare refuses (error 1027): `quota_exhausted`, nothing ran |
| Memory per isolate | 128 MiB | Cloudflare stops it (error 1102) |
| Worker size | 64 MiB | Compute refuses larger modules before uploading |
| Workers per account | 100 | Cloudflare refuses the upload: `quota_exhausted` |
| Startup (global scope) | 1 s | Cloudflare refuses the Worker |

10 ms is CPU time, not wall time: a module waiting on nothing uses little.
It still rules out heavy computation. Short agent tasks, lightweight
orchestration steps, validation, transformation, and request-driven
handlers fit; builds, test suites, and inference do not.

Cloudflare may change these limits. You are responsible for your Cloudflare
account and its usage. Compute does not make execution free, unlimited, or
guaranteed; it reports what Cloudflare reports.

Compute adds its own limits, each bounded by the profile, in the pool
configuration:

| Setting (`[providers.<id>.cloudflare.limits]`) | Default | Bound |
| --- | --- | --- |
| `timeout_ms` | 30000 | 900000 |
| `max_module_bytes` | 10 MiB | Worker size − 256 KiB |
| `max_stdin_bytes` | 1 MiB | 32 MiB |
| `max_output_bytes` (each of stdout and stderr) | 1 MiB | 32 MiB |
| `max_concurrent_invocations` (per Compute process) | 6 | 100 |

A configuration above a bound is refused when the pool is loaded. An
invocation beyond `max_concurrent_invocations` fails with `quota_exhausted`
and is never queued. Compute does not count daily requests itself; it keeps
no usage database. Cloudflare's own refusal (error 1027) is the signal.

## Setup

1. **A workers.dev subdomain.** In the Cloudflare dashboard, open *Workers &
   Pages* once; Cloudflare asks you to choose a subdomain. Note your
   **Account ID** (on the same page). It identifies the account; it is not a
   secret.
2. **An API token.** *My Profile → API Tokens → Create Token → Edit
   Cloudflare Workers* template, scoped to that one account. Compute needs
   Workers Scripts Edit (and Read).
3. **Keep the token in your environment, never in a file Compute reads or a
   command line:**

   ```sh
   read -rs CLOUDFLARE_API_TOKEN && export CLOUDFLARE_API_TOKEN
   ```

   (or load it from your password manager or secret store into the
   environment). `token_file` also works, pointing at a file only you can
   read.
4. **A pool member.** `compute cloudflare init` writes one, refusing to
   overwrite an existing file:

   ```sh
   compute cloudflare init --account-id 0123456789abcdef0123456789abcdef \
     --output ~/.compute/cloudflare-pool.toml
   export COMPUTE_POOL_CONFIG=~/.compute/cloudflare-pool.toml
   ```

   or add it to your own pool file
   ([example](../examples/cloudflare-workers/compute-pool.toml)):

   ```toml
   [providers.cloudflare]
   kind = "cloudflare"
   token_env = "CLOUDFLARE_API_TOKEN"   # the variable's name, never its value
   priority = -100                      # selected when named, or when nothing else fits

   [providers.cloudflare.cloudflare]
   account_id = "0123456789abcdef0123456789abcdef"
   profile = "workers-free"
   ```

5. **Check it:**

   ```sh
   compute cloudflare validate --provider cloudflare --offline   # configuration only, no network
   compute cloudflare validate --provider cloudflare             # credential, account, subdomain
   ```

Credentials never appear in pool files, capability caches, placement
reports, receipts, deployment receipts, invocation records, errors, or logs.
Compute derives each Worker's invocation secret from the API token (HMAC),
so it stores no secret of its own; rotating the token makes existing Workers
refuse invocations until they are deployed again.

## Deploy, invoke, run, delete

Deployment, invocation, and deletion are distinct operations with distinct
evidence.

```sh
cd examples/cloudflare-workers

# Deploy: explicit, authenticated, idempotent.
compute cloudflare deploy --provider cloudflare hello.wasm
compute cloudflare status --provider cloudflare hello.wasm

# Invoke once, directly.
compute cloudflare invoke --provider cloudflare hello.wasm
compute cloudflare invoke --provider cloudflare hello.wasm \
  --arg --name --arg edge --env GREETING=hi --env API_KEY --stdin input.txt --timeout 5s

# Run as a portable workload, with placement, admission, and a receipt.
compute run hello.wasm --provider cloudflare
compute run workload.json --provider cloudflare --json

# Delete.
compute cloudflare list --provider cloudflare
compute cloudflare delete --provider cloudflare --module hello.wasm
```

`--env NAME` (without `=`) passes this process's value of `NAME`, so a
secret the workload needs is never written on the command line.

- **Execution never deploys.** `compute run --provider cloudflare` on a
  module that is not deployed fails with `runtime_unavailable` and the
  `deploy` command to run. Nothing is uploaded implicitly.
- **Deployment is idempotent.** A Worker's name is
  `<worker_prefix><digest>`, a function of the harness, the module, and the
  compatibility date. Deploying the same module again uploads nothing
  (`already_deployed`). A deployment retried after a lost response finds the
  Worker and does not create a second one. Invocations are never retried
  automatically: when an invocation's outcome is unknown, Compute says so.
- **Deletion is limited to Compute's own Workers.** Only names with this
  provider's prefix and a digest are listed or deleted. Deleting an absent
  Worker reports `absent`.

## Evidence

| Record | Proves | Does not prove |
| --- | --- | --- |
| Deployment receipt (`compute.cloudflare.deployment@1`) | Cloudflare accepted a Worker with exactly this harness and module | That anything ran, or succeeded |
| Invocation record (`compute.cloudflare.invocation@1`) | What one request returned; whether the answering Worker reported the deployed workload (`workload_identity_matched`); Cloudflare's `cf-ray` | Anything about other invocations |
| Execution result + `compute.receipt@1` | The outcome, bound to the placement, admission, provider identity, and deployment (`distribution.id` is the deployment ID; `runtime.executable_identity` is the module digest) | That Compute observed the execution itself: Cloudflare ran it, and Compute's harness reported it |
| Deletion record (`compute.cloudflare.deletion@1`) | Whether the Worker existed and was deleted | |

Each record carries a content hash and verifies offline. A Worker that
answers for a different workload than the one deployed is rejected
(`evidence_invalid`); its result is not reported. A timed-out or cancelled
invocation is recorded as such, with the note that Cloudflare's own outcome
is unknown.

## Errors

| Situation | Compute error |
| --- | --- |
| No token in the named variable | `unauthorized`, naming the variable |
| Token refused (codes 10000, 9109, …) | `unauthorized`, with Cloudflare's code |
| Unknown account (codes 7000, 7003) | `unauthorized`, "check account_id" |
| No workers.dev subdomain | `operation_unsupported`, with how to create one |
| Workload unsupported by the profile | `operation_unsupported` / `capability_mismatch`, naming the capability |
| Module not deployed | `runtime_unavailable`, with the deploy command |
| Daily quota, API rate limit, Worker count | `quota_exhausted` |
| CPU or memory limit hit | execution `killed`, error `resource_limit` |
| Wall-clock limit | execution `timed_out`, outcome at Cloudflare unknown |
| Cloudflare unavailable (5xx, connection) | `provider_unavailable` |
| Invocation secret refused (token rotated) | `unauthorized`, "deploy it again" |

No error moves a workload to another provider. `compute run` reports
`not retried on another provider`; choosing another provider is your call.

## When a workload outgrows the profile

Run it where its requirements hold. The workload doesn't change:

```sh
compute run app.wasm --provider local          # this machine
compute run app.wasm --provider production     # a Compute server (compute serve)
compute placement explain app.wasm             # who can run it, and why not
```

A WASI module that needs files, longer CPU time, or more memory runs on any
Compute provider unchanged. A Node or Python workload needs a provider with
that runtime. A session, a service, or a persistent environment needs a
Compute target. Placement explains each refusal. A paid Workers plan or
Cloudflare Containers would be separate profiles or providers; neither is
implemented.

## Local development and tests

Everything is tested without an account:

```sh
cargo test -p compute-provider-cloudflare            # provider, placement, fake Cloudflare API
COMPUTE_REQUIRE_NODE=1 cargo test -p compute-provider-cloudflare --test harness
cargo test -p compute-cli --test cloudflare          # the CLI without credentials
```

- `tests/support` is a local fake of the Cloudflare API and workers.dev. It
  really runs the uploaded module, with wasmtime, and can simulate quota
  exhaustion (1027), CPU limits (1102), rate limits, script limits, outages,
  hung invocations, lost responses, and a Worker that answers for the wrong
  workload.
- `tests/harness.rs` runs the JavaScript harness (`worker/harness.mjs`) under
  Node against real WASI modules. Node and Workers share V8, WebAssembly,
  and the Request/Response APIs. Without Node the test prints `SKIPPED`;
  `COMPUTE_REQUIRE_NODE=1` (as CI sets) makes it fail instead.
- `api_base` and `workers_base` point a provider at a local fake. They
  accept only `https://` or loopback `http://`.

### Live smoke test

```sh
export CLOUDFLARE_API_TOKEN=…   # see Setup
export COMPUTE_CLOUDFLARE_LIVE_ACCOUNT_ID=…
cargo test -p compute-provider-cloudflare --test live -- --ignored --nocapture
```

It validates the account, deploys a uniquely named Worker
(`compute-smoke-…`), invokes it and checks the output, the invocation
record, and that the answering Worker reported the deployed workload, runs
it again through the provider boundary and verifies the receipt, then
deletes the Worker and confirms it no longer exists. It uses only free-plan
resources. It is `#[ignore]`d in normal runs. Asked to run without its
variables, it **fails** (`NOT RUN`) rather than passing. The
`cloudflare-live` workflow runs it on demand with repository secrets.

## Configuration reference

```toml
[providers.<id>]
kind = "cloudflare"
token_env = "CLOUDFLARE_API_TOKEN"   # or token_file = "path"; one is required
priority = -100

[providers.<id>.cloudflare]
account_id = "<32 hex>"              # required
profile = "workers-free"             # the only profile today
worker_prefix = "compute-"           # 1-24 of a-z 0-9 -, starting with a letter
compatibility_date = "2026-10-01"    # the Workers compatibility date of the harness

[providers.<id>.cloudflare.limits]
timeout_ms = 30000
max_module_bytes = 10485760
max_stdin_bytes = 1048576
max_output_bytes = 1048576
max_concurrent_invocations = 6
```

A cloudflare member has no `endpoint`. Unknown keys are refused, including
any attempt to put a token in the table.

## Implementation notes

- Crate: `crates/compute-provider-cloudflare`. Core contract additions are
  additive: `ProviderIdentity::Cloudflare` (`compute.cloudflare@1`),
  `ProviderKind::Cloudflare`, `ProviderErrorKind::QuotaExhausted`, and
  `RuntimeSource::Provider`. Existing providers, pools, receipts, and
  identities are unchanged.
- Capabilities come from the profile and configuration alone. Discovery
  needs no credential and makes no network call; health is `unhealthy`
  until the credential reaches the account.
- Admission is Compute's (`compute-policy`). It runs in the adapter, before
  anything is sent, and reproduces the decision placement made, as the
  receipt check requires.
- The only in-memory state is the account's workers.dev subdomain, a cache
  rebuilt from the API by each process.
