# Reproducible Compute distribution

`runtime-lock.json` is Compute's canonical runtime catalog. Each entry names a
runtime and exact version, executable, supported OS/architecture distributions,
immutable source artifacts, SHA-256 digests, archive format, and installation
layout. Runtime adapters declare execution capabilities; providers consume this
catalog to resolve and materialize distributions without runtime-specific
lifecycle branches. The builder never discovers or substitutes a host language
runtime.

Build and certify the Linux distribution with the same command used by release
CI:

```sh
cargo build --release --locked
rustup target add wasm32-wasip1
target/release/compute distribution build \
  --output dist/compute-distribution \
  --status certified \
  --verify
```

Build the macOS ARM64 Preview on that host with:

```sh
target/release/compute distribution build \
  --output dist/compute-distribution \
  --status preview
```

Preview is an explicit distribution state. Runtimes with pinned native payloads are
installed and labeled Preview; absent or Linux-only runtimes are retained in the
inventory as Unavailable with a reason. `--status certified` requires `--verify` and
fails if any locked runtime is unavailable.

The builder downloads missing artifacts into
`$XDG_CACHE_HOME/compute/runtimes/sha256/<digest>` (or
`~/.cache/compute/runtimes/sha256/<digest>`), verifies every cached or newly
downloaded byte, installs the declared platform payload, launches each runtime,
and records its reported identity and payload hash. `--offline` disables all
artifact network access and fails on the first cache miss. An absent platform
entry is an error; another architecture is never substituted.

The output contains `runtime-manifest.json`, `runtime-inventory.json`, the lock,
the Compute executable, all runtime payloads, and the seven immutable recipe
templates under `recipes/starters`. Their hashes are part of the distribution
identity, and verification launches the packaged binary to prove they are
discoverable without a controller. When `--verify` is used it also contains the
compiled certification fixtures. The adjacent uncompressed `.tar` has sorted
entries, normalized ownership, permissions, timestamps, and a fixed internal
root name, so the same source, binary, lock, and platform produce identical
bytes.

Inspect or verify an existing output without rebuilding it:

```sh
compute distribution inspect dist/compute-distribution --json
compute distribution verify dist/compute-distribution --json
```

Verification rechecks lock compatibility, manifest identity, every starter and
payload-tree hash, every required executable, and every runtime version probe. `--verify`
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
An assembled distribution is rooted by `COMPUTE_DISTRIBUTION_ROOT` (or is
discovered relative to its `bin/compute` executable), carries provenance for
all pinned payloads, and is the only environment accepted by `compute certify`.
`COMPUTE_HOME` is separate: it contains mutable per-user state and defaults to
`~/.compute`. Replacing either tree never replaces the other.

## Homebrew channel

The `rkendel1/homebrew-compute` tap synchronizes the latest stable certified
release using its repository-scoped GitHub Actions token. The renderer in
`distribution/homebrew` copies the release version and the adjacent release
asset's SHA-256 into the formula; the formula never builds Compute or downloads
runtimes independently. No cross-repository write credential is stored in this
repository.

Repository tests prove the templates; they cannot prove the tap. The tap is
proved by `distribution/tests/live-tap-release.sh [VERSION]`. It fetches the
live tap's formulas over the network and requires them to be byte-identical to
the release tag's own templates rendered with the release's own checksums, each
cross-checked against GitHub's digest of the published artifact. It also checks
version, artifact URLs, checksums, Ruby validity, the configured
`compute-configured-chip` payload and a Chip-free base formula, and labels each
failure with the invariant that broke. Where both formulas are installed from
the tap, it verifies the installed consumer too: versions, a Chip-free base
keg, and an actual Chip execution through the configured launcher. Set
`COMPUTE_LIVE_TAP_CONSUMER=require` to fail when that is not possible. The
`live-tap` workflow runs it daily, and `live-tap-release-failures.sh` proves
that each failure class is detected. Reference evidence:
[docs/homebrew-0.1.17-consumer-validation.md](../docs/homebrew-0.1.17-consumer-validation.md).
