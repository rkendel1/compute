# Installing Compute

The supported product installation is:

```sh
curl -fsSL https://get.compute.dev | sh
compute --version
compute
```

The initial certified platform is Linux x86_64. The installer fails closed on
every other operating system or architecture; it does not substitute a source
build or host runtimes.

The bootstrap resolves the latest stable `v*` GitHub Release, downloads
`compute-<version>-linux-x86_64.tar.gz` and its adjacent `.sha256`, verifies the
archive, validates the extracted distribution with Compute's existing
distribution verifier, and only then atomically activates it. The archive is
the artifact that passed release certification and contains the Compute
executable, pinned runtime bundle, runtime lock, manifest, and inventory.

By default, immutable installations live under:

```text
~/.local/share/compute/
  installations/<version>-<sha256>/
  current -> installations/<version>-<sha256>
```

`~/.local/bin/compute` points through `current` to the active distribution.
Add `~/.local/bin` to `PATH` if it is not already present. `XDG_DATA_HOME`,
`COMPUTE_INSTALL_ROOT`, and `COMPUTE_BIN_DIR` can relocate the installation.

Mutable per-user state remains in `$COMPUTE_HOME`, defaulting to `~/.compute`.
It is never stored in an installation directory. `COMPUTE_DISTRIBUTION_ROOT`
is the explicit development/operations override for an immutable distribution
root; normal installed execution discovers that root relative to the executable.

Run the installation command again to upgrade. The new version is downloaded,
verified, extracted, and validated before the active symlink changes. A failed
download, checksum, extraction, or validation leaves the prior installation
and all user state untouched. For reproducible installation of a specific
release:

```sh
curl -fsSL https://get.compute.dev | COMPUTE_VERSION=0.1.0 sh
```

`compute --version` reports the installed Compute version. Runtime versions
and payload identities remain inspectable in the installed
`runtime-manifest.json` and `runtime-inventory.json`.

`cargo install` is not the product installation mechanism: a Cargo-built
binary does not include the certified pinned runtime bundle.

## Path audit

The installation contract classifies the repository's path uses as follows:

| Access | Classification | Contract |
| --- | --- | --- |
| `compute_core::paths::installation_root`, runtime adapters, certification, distribution verification | installation/runtime lookup | `$COMPUTE_DISTRIBUTION_ROOT`, otherwise executable-relative |
| `compute_core::paths::state_root`, control-plane identity, computers, environments, artifacts, receipts, FeltDB/file state | mutable user state | `$COMPUTE_HOME`, otherwise `~/.compute` |
| runtime downloads | temporary/cache data | `$XDG_CACHE_HOME/compute/runtimes`, otherwise `~/.cache/compute/runtimes` |
| CLI, browser, launcher, stack, and product test homes | test isolation | per-test `$COMPUTE_HOME` |
| distribution certification scripts and Docker image | CI/distribution packaging | `$COMPUTE_DISTRIBUTION_ROOT` |
| control-plane and remote-execution documentation | documentation/example | `$COMPUTE_HOME` describes mutable state |

There is no local fallback database or second durable state format. This path
split only separates replaceable product bytes from existing durable state.
