# Remote execution

`compute.remote@1` uses the normal Compute bundle, runtime, policy, result,
and receipt contracts. Synchronous operations are stateless; asynchronous jobs
persist lifecycle and evidence in the configured filesystem job store.

Start a server:

```sh
compute serve --listen 127.0.0.1:8080 \
  --public-url http://127.0.0.1:8080
```

Then inspect or execute either a direct file or an existing bundle:

```sh
compute remote inspect --provider http://127.0.0.1:8080 script.py --json
compute remote run --provider http://127.0.0.1:8080 script.py
compute remote run --provider http://127.0.0.1:8080 \
  --bundle workload.compute --receipt receipt.json
compute remote health --provider http://127.0.0.1:8080
```

Direct inputs are first resolved into the same deterministic `.compute`
bundle used locally. The server verifies canonical archive form, workload and
bundle identities, embedded dependency capsules, paths, and requested
identities before extraction and execution. Remote inspect performs the same
planning without starting the workload.

The protocol exposes `GET /compute/health`, `GET /compute/capabilities`,
`GET /compute/inspect`, and `POST /compute/execute`. Requests carry a
deterministic SHA-256 hash over artifact, identities, policy, and optional
caller request ID. Authorization headers, timestamps, and transport metadata
are excluded. A caller request ID is correlation data only: v1 does not claim
exactly-once execution or automatic retries.

Authorization is an injectable `ProviderAuthorizer`; the command-line server
uses the explicit `AllowAllAuthorizer` default. Deployments should replace it
at their boundary. Server policy may reject or strengthen a request, but may
not weaken workload requirements. The built-in v1 client currently supports
plain `http://`; terminate TLS in a trusted reverse proxy when exposing it.

There is no scheduler, workflow engine, automatic retry, streaming output,
secret manager, runtime installer, or host dependency fallback. An official
server container must run from the pinned output of `compute distribution build`.

## Durable asynchronous jobs

The same protocol also supports `POST /compute/jobs` plus status, result,
receipt, artifact, cancellation, and optional event endpoints below
`/compute/jobs/{job_id}`. Jobs use a minimal bounded queue and filesystem
persistence; they do not add workflow or retry semantics. See
[jobs.md](jobs.md) for lifecycle, idempotency, retention, restart, ownership,
and exactly-once limitations.
