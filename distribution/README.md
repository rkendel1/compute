# Reproducible Compute distribution

`runtime-lock.json` is the authoritative runtime artifact set. Each downloadable
runtime is pinned by platform, immutable URL, SHA-256 digest, archive format,
and installation layout. The builder never discovers or substitutes a host
language runtime.

Build and certify the Linux distribution with the same command used by release
CI:

```sh
cargo build --release --locked
rustup target add wasm32-wasip1
target/release/compute distribution build \
  --output dist/compute-distribution \
  --verify
```

The builder downloads missing artifacts into
`$XDG_CACHE_HOME/compute/runtimes/sha256/<digest>` (or
`~/.cache/compute/runtimes/sha256/<digest>`), verifies every cached or newly
downloaded byte, installs the declared platform payload, launches each runtime,
and records its reported identity and payload hash. `--offline` disables all
artifact network access and fails on the first cache miss. An absent platform
entry is an error; another architecture is never substituted.

The output contains `runtime-manifest.json`, `runtime-inventory.json`, the lock,
the Compute executable, all runtime payloads, and—when `--verify` is used—the
compiled certification fixtures. The adjacent uncompressed `.tar` has sorted
entries, normalized ownership, permissions, timestamps, and a fixed internal
root name, so the same source, binary, lock, and platform produce identical
bytes.

Inspect or verify an existing output without rebuilding it:

```sh
compute distribution inspect dist/compute-distribution --json
compute distribution verify dist/compute-distribution --json
```

Verification rechecks lock compatibility, manifest identity, every payload-tree
hash, every required executable, and every runtime version probe. `--verify`
also runs `compute doctor` and `compute certify` with all runtimes required and
with the existing poisoned-host environment checks.

Docker consumes only that assembled directory:

```sh
docker build -f distribution/Dockerfile \
  --build-arg COMPUTE_DISTRIBUTION=dist/compute-distribution \
  -t compute .
docker run --rm compute certify --json
```

The Dockerfile does not download runtimes and contains no independent version
matrix. `certify-distribution.sh` additionally executes one portable workload
both bare and in Docker, exercises durable submit/wait/receipt/artifact,
cancellation, and server-restart recovery across the container boundary, and
verifies the container-produced receipts on the host
against the assembled distribution and output artifact, and compares semantic
receipt evidence after excluding timestamps, execution IDs, and receipt hashes.

## Development versus distribution

A source checkout is a development environment: runtimes may be missing and
`compute doctor` reports host reality. It cannot claim universal certification.
An assembled distribution is rooted by `COMPUTE_HOME`, carries provenance for
all pinned payloads, and is the only environment accepted by `compute certify`.
