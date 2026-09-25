# Provider pools

A provider pool is a caller-owned list of Compute providers. It can mix the
local provider with any number of remote Compute instances. There is no
central registry and no service discovery: the pool is exactly what your
configuration says.

## Configuration

```toml
# compute-pool.toml
[pool]
require_healthy = false          # default: health never affects selection
capability_ttl_seconds = 300     # how long discovered capabilities stay valid
allow_stale_capabilities = false # default: stale capabilities are unknown

[providers.local]
kind = "local"
priority = 100

[providers.dev]
kind = "remote"
endpoint = "http://compute-dev:8080"
priority = 50

[providers.production]
kind = "remote"
endpoint = "https://compute.example"
token_env = "COMPUTE_PRODUCTION_TOKEN"
```

Compute finds the configuration in this order:

1. `--pool-config PATH`
2. `$COMPUTE_POOL_CONFIG`
3. `./compute-pool.toml`
4. otherwise, a pool containing only `local`

Provider IDs are 1–64 ASCII letters, digits, `-`, or `_`. Unknown keys are
rejected. Credentials are never written in the configuration: `token_env`
names an environment variable read when the pool is built. Credentials
embedded in an endpoint URL are rejected. Credentials never appear in output,
caches, placement IDs, or receipts.

Every `compute remote --provider <id>` command resolves `<id>` through this
caller-owned pool. Provider endpoints are configuration details, not provider
identities accepted by the remote CLI.

```sh
compute provider pool --json
```

## Priorities

`priority` is an integer (default `0`). Higher values are considered first,
and the provider ID (ascending) breaks ties.

**Priority applies only after compatibility.** A priority-100 provider that
cannot satisfy strict isolation never beats a compatible priority-50
provider. Selection is described in [placement.md](placement.md).

## Health

Health is observational: `healthy`, `unhealthy`, or `unknown`. It is
recorded during capability discovery and shown in `compute provider list`
and in every placement report.

Health never makes an incompatible provider compatible. By default it does
not affect selection at all. With `require_healthy = true`, compatible
providers whose health is not `healthy` are reported as `excluded_unhealthy`
and are not selected. Compute has no autonomous health management: it does
not probe in the background, evict providers, or reorder them.

## Capability cache and freshness

Discovered capabilities can be cached in
`./.compute/provider-capabilities.json`. Override the location with
`--capability-cache` or `$COMPUTE_CAPABILITY_CACHE`. Each entry records:

- `fetched_at`
- `expires_at` (`fetched_at + capability_ttl_seconds`)
- a fingerprint of the provider's configured kind and endpoint
- the raw capability response

The descriptor derived from that response carries a `capability_version`
digest.

```sh
compute provider refresh               # rediscover every provider, rewrite the cache
compute provider refresh production
compute placement inspect --refresh ./script.py
```

- A **fresh** entry (`now < expires_at`) is used as-is.
- A **stale** entry is reported as `capabilities_unknown`. It is never
  assumed valid, so a stale provider is not selected unless the pool sets
  `allow_stale_capabilities = true`.
- A provider with **no entry**, or whose configuration changed, is
  discovered live.
- Cached responses are re-validated on every use: the cache is input too.

Only `compute provider refresh` and `--refresh` write the cache. The command
`compute provider list --refresh` also refreshes and writes it.

## Local and remote providers in one pool

The local provider and a Docker-hosted Compute server built from the same
deterministic distribution can join one pool:

```sh
docker run -d -p 8080:8080 compute-image \
  serve --listen 0.0.0.0:8080 --public-url http://compute-docker:8080

cat > compute-pool.toml <<'EOF'
[providers.local]
kind = "local"
priority = 10

[providers.docker]
kind = "remote"
endpoint = "http://127.0.0.1:8080"
priority = 100
EOF

compute placement inspect --bundle workload.compute
compute pool submit --bundle workload.compute --json
```

`distribution/certify-distribution.sh` runs exactly this, then verifies the
job, result, receipt, provider identity, distribution identity, and
dependency identity.

## Non-goals

A pool is not a scheduler. Compute does not provide:

- workflows, job dependencies, retries, or failover
- autoscaling, load balancing, or geographic or cost optimization
- cloud provisioning, Kubernetes integration, or a provider registry
- service discovery, billing, or a multi-tenant control plane

Those belong to the layer above Compute, such as Factory, AppPort Services,
CI systems, or another orchestrator. That layer can submit explicit
executions and read the placement evidence in each receipt.
