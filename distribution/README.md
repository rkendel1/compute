# Compute runtime distribution

`runtime-lock.json` is the authoritative runtime-version set for a Compute
release. Runtime payload production is deliberately separate from workload
execution: language toolchains may build the payloads, but `compute run` never
downloads, installs, or compiles dependencies.

Assemble an already-built Linux payload tree with:

```sh
distribution/assemble.sh \
  target/release/compute \
  /path/to/runtime-payload \
  linux-x86_64 \
  dist/compute-distribution
```

The payload root mirrors `runtimes/<id>/...` from the lock. Assembly verifies
every executable and exact pinned version, emits `runtime-manifest.json`,
normalizes metadata, and creates a deterministically ordered uncompressed tar.
An assembled Compute process sets `COMPUTE_HOME` to this root and fails closed
if a declared executable is missing, relocated, or reports another version.

Build the Docker packaging target from that exact directory:

```sh
docker build \
  -f distribution/Dockerfile \
  --build-arg COMPUTE_DISTRIBUTION=dist/compute-distribution \
  -t compute .
```

The Dockerfile contains no runtime versions. It packages the same assembled
distribution used for a host, VM, CI, Kubernetes, or Fly installation.

Release CI should run the shared suite against the assembled root without
permitting unavailable-runtime skips:

```sh
COMPUTE_HOME="$PWD/dist/compute-distribution" \
COMPUTE_REQUIRE_ALL_RUNTIMES=1 \
cargo test -p compute-runtime --test conformance
```

This mode fails if any first-class runtime is missing or incompatible. JVM,
.NET, and native fixture compilation happens before Compute execution and is
test setup, not a workload build feature.
