# Installing Compute

The supported product installation is Homebrew:

```sh
brew tap rkendel1/compute
brew install compute
compute --version
compute
```

The initial certified platform is Linux x86_64. The formula declares both
constraints and does not advertise macOS or ARM merely because Homebrew runs
there. Unsupported platforms fail rather than substituting a source build or
host runtimes.

The formula's version, URL, and SHA-256 are generated from the stable `v*`
GitHub Release by the same workflow that certifies and publishes
`compute-<version>-linux-x86_64.tar.gz`. Homebrew verifies that checksum and
installs the archive unchanged. It contains the Compute executable, pinned
runtime bundle, runtime lock, manifest, and inventory.

Homebrew owns the immutable installation under its Cellar:

```text
$(brew --prefix compute)/
  bin/compute -> ../libexec/bin/compute
  libexec/
    runtimes/
    runtime-manifest.json
    runtime-inventory.json
```

Mutable per-user state remains in `$COMPUTE_HOME`, defaulting to `~/.compute`.
It is never stored in an installation directory. `COMPUTE_DISTRIBUTION_ROOT`
is the explicit development/operations override for an immutable distribution
root; normal installed execution discovers that root relative to the executable.

Upgrade through Homebrew:

```sh
brew update
brew upgrade compute
```

The new keg replaces only immutable installation assets. Environments, FeltDB
state, checkpoints, artifacts, configuration, and receipts under
`COMPUTE_HOME` remain untouched.

`compute --version` reports the installed Compute version. Runtime versions
and payload identities remain inspectable in the installed
`runtime-manifest.json` and `runtime-inventory.json`.

For a direct user-local installation, download `install.sh` from a trusted
Compute source checkout and run `sh install.sh`. The script remains supported,
but no domain-backed curl command is documented until that domain is operated
by the project.

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
