# Compute applications

An application is the product-level identity above durable jobs. The
application says what should run; a job is one run, an execution is the
provider-side attempt, and a receipt is independently verifiable evidence of
that attempt.

Create and operate the smallest useful HTTP application with:

```sh
compute init my-app
compute run my-app --pool-config compute-pool.toml
compute status my-app --pool-config compute-pool.toml
compute logs my-app --follow --pool-config compute-pool.toml
compute stop my-app --pool-config compute-pool.toml
compute history my-app --pool-config compute-pool.toml
```

`compute init` creates a Node application by default; `--runtime python`
creates the equivalent Python server. `compute run` derives the workload
contract from `compute.toml`, performs normal compatibility, policy,
placement, capacity, reservation, and admission evaluation, and submits one
durable provider job. Running the same active application again observes the
existing job instead of creating a duplicate.

Application identity is sealed into the provider request, durable job, and
execution receipt. The lifecycle commands rediscover jobs by that identity;
there is no second application registry or database competing with provider
job state.

## Provider configuration

Applications currently require a remote provider in the caller-owned pool
because the remote job protocol supplies durable status, logs, cancellation,
history, and receipts. Configure an optional public application URL when the
provider publishes the declared application port:

```toml
[providers.dev]
kind = "remote"
endpoint = "http://compute-dev:8080"
application_endpoint = "https://my-app.dev.example"
priority = 100
```

The URL is explicit: Compute cannot safely infer public routing through a
proxy, NAT, or container port mapping. When configured, `compute run` and
`compute status` show it as `Endpoint`. Publishing and routing that port is
the provider operator's responsibility.

`compute deploy my-app` currently materializes the same durable application
with refreshed provider capabilities. It deliberately does not introduce a
separate cloud deployment model.

See [the runnable Node demo](../examples/compute-demo/) and
[jobs.md](jobs.md) for the underlying durable execution contract.
