# Remote execution

`compute.remote@1` uses the normal Compute bundle, runtime, policy, result,
and receipt contracts. Synchronous operations are stateless; asynchronous jobs
persist lifecycle and evidence in the configured filesystem job store.

Start a server that trusts one control plane (or one developer's CLI):

```sh
# On the target machine: issue a credential; the target keeps its verifier.
compute target credential issue --credentials /etc/compute/target-credentials.json \
  --control-plane dev --token-file ./dev.token
compute serve --listen 127.0.0.1:8080 \
  --public-url http://127.0.0.1:8080 \
  --credentials /etc/compute/target-credentials.json
```

and name the token in the caller's pool:

```toml
[providers.dev]
kind = "remote"
endpoint = "http://127.0.0.1:8080"
token_file = "./dev.token"      # or token_env = "DEV_TARGET_TOKEN"
```

Then inspect or execute either a direct file or an existing bundle:

```sh
compute remote inspect --provider dev script.py --json
compute remote run --provider dev script.py
compute remote run --provider dev \
  --bundle workload.compute --receipt receipt.json
compute remote health --provider dev
```

Here `dev` is a provider ID in the caller-owned pool (for example,
`[providers.dev]` in `compute-pool.toml`). `--provider` is always resolved
through that pool; the remote endpoint belongs in provider configuration, not
in the command invocation. Pool discovery follows `--pool-config`, then
`$COMPUTE_POOL_CONFIG`, then `./compute-pool.toml`, with the usual local-only
fallback.

Direct inputs are first resolved into the same deterministic `.compute`
bundle used locally. The server verifies canonical archive form, workload and
bundle identities, embedded dependency capsules, paths, and requested
identities before extraction and execution. Remote inspect performs the same
planning without starting the workload.

Remote execution is job-backed. `compute remote run` waits synchronously but
still reports the durable `job_id` required by subsequent lifecycle commands,
alongside the distinct `execution_id` recorded in its result and receipt.

The protocol exposes `GET /compute/health`, `GET /compute/capabilities`,
`GET /compute/inspect`, and `POST /compute/execute`. Requests carry a
deterministic SHA-256 hash over artifact, identities, policy, and optional
caller request ID. Authorization headers, timestamps, and transport metadata
are excluded. A caller request ID is correlation data only: v1 does not claim
exactly-once execution or automatic retries.

## Target credentials

A target is controlled only by the control planes it trusts. Its trust file
(`compute serve --credentials FILE`, format `compute.target.credentials@1`)
lists, per credential, the control plane it identifies and the SHA-256
verifier of its secret — never the secret. The token has Compute's
credential form, `cmpt_tcred_<16 hex>_<64 hex>`, and is shown (or written to
`--token-file`, owner-only) once.

```sh
compute target credential issue  --credentials FILE --control-plane NAME [--token-file PATH]
compute target credential list   --credentials FILE          # never a secret
compute target credential revoke CREDENTIAL_ID --credentials FILE
```

- Every request needs a credential, reads and health included: none, a
  malformed one, a wrong secret, or a revoked credential gets `401
  unauthorized`.
- What a request creates (sessions, jobs) belongs to the **control plane**
  its credential names (`control-plane:NAME`), not to the token. Rotating a
  control plane's credential (issue a new one, revoke the old) keeps what it
  owns; another control plane's valid credential lists none of it, and its
  sessions are `unknown_session` to it.
- The file is re-read whenever it changes: a revocation takes effect at the
  next request, without a restart. A missing or unreadable file admits
  nobody.
- `compute serve` refuses to start without a trust file. The only open mode
  is the named `--insecure-unauthenticated`, for local development: it warns
  at start, and the target advertises `authentication:
  "insecure-unauthenticated"` in its capabilities (and `compute target list`
  shows it). A server built in a program with no authority configured
  (`ServerConfig::local`) refuses every request.
- `compute` (the launcher) does all of this for its own computer host: it
  keeps a control-plane identity in `$COMPUTE_HOME/identity`, issues the
  host a credential, and writes a pool that names the token file.

`ProviderAuthorizer` stays injectable for programs that embed a target. The
daemon's own `/compute/*` service admits only requests its API
authenticated. Server policy may reject or strengthen a request, but may
not weaken workload requirements. The built-in v1 client currently supports
plain `http://`; terminate TLS in a trusted reverse proxy when exposing it.

There is no scheduler, workflow engine, automatic retry, streaming output,
secret manager, runtime installer, or host dependency fallback. An official
server container must run from the pinned output of `compute distribution build`.

## Durable asynchronous jobs

A server that offers `sessions` also serves `/compute/sessions`: create,
list, inspect, events, connect, exec, logs, stop, resume, claim, and destroy.
Each command a session runs is a durable job below `/compute/jobs`. See
[sessions.md](sessions.md).

The same protocol also supports `POST /compute/jobs` plus status, result,
receipt, artifact, cancellation, and optional event endpoints below
`/compute/jobs/{job_id}`. Jobs use a minimal bounded queue and filesystem
persistence; they do not add workflow or retry semantics. See
[jobs.md](jobs.md) for lifecycle, idempotency, retention, restart, ownership,
and exactly-once limitations.
