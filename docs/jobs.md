# Durable execution jobs

A durable job is a filesystem-backed handle around the normal Compute
execution contract. It does not change workload, runtime, policy, result, or
receipt semantics. Synchronous execution waits for the result; asynchronous
execution returns a job ID and lets the caller observe it later.

`job_id` is the durable identity of a submitted remote job. It is used for
status, result, wait, receipt, artifact, cancellation, and lifecycle queries.
`execution_id` identifies the Compute execution represented by the job's
result and receipt; it is used for execution records, provenance, and receipt
correlation. The two identities are related but are not interchangeable.

`compute remote run` creates a durable job, waits for it, and reports both
identities. `compute remote submit` returns immediately with the `job_id`.

```sh
compute remote submit --provider dev \
  --bundle workload.compute --idempotency-key deploy-42
compute remote status --provider dev job_...
compute remote wait --provider dev job_... --timeout 60s
compute remote receipt --provider dev job_... --output receipt.json
compute remote artifacts --provider dev job_... --json
compute remote cancel --provider dev job_...
```

The lifecycle is `created → accepted → queued → preparing → running`, followed
by `succeeded`, `failed`, `cancelled`, `timed_out`, or `rejected`. Event
sequence numbers are observational; persisted job state is authoritative.
Job IDs identify submissions and never participate in workload, bundle, or
deterministic request identity.

`compute serve` stores atomic `request.json`, `status.json`, `result.json`,
`receipt.json`, event, and digest-addressed artifact files under `--job-store`.
`--max-concurrent-jobs` bounds execution; excess jobs remain queued.
`--job-retention` controls terminal evidence retention. Expired jobs retain a
small ownership tombstone so they remain distinguishable from unknown jobs.

An `Idempotency-Key` has provider-owner scope. The same key and request returns
the same job; the same key with a different request is rejected. Without a
key, every submission may create another job. Compute never silently retries
workloads and does not promise exactly-once execution. This matters for
workloads with external side effects.

Cancellation before execution is effective. During execution the provider
signals the controlled runtime, persists the cancelled result and final
receipt, and releases the reservation. A client disconnect or wait timeout
never cancels work.

Every job operation is authorized independently and bound to the submitting
owner. Artifact paths are never accepted from clients: retrieval maps the job
to digest-addressed bytes and verifies each digest against receipt evidence.
Receipts remain independently verifiable without the server.

On restart, accepted and queued jobs are recovered. A running job with valid
terminal evidence is reconciled; otherwise it becomes a structured
`provider_interrupted` failure. Compute never fabricates success after a
restart. There are no retries, priorities, schedules, DAGs, or workflow
semantics.
