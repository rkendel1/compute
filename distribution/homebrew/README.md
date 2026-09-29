# Homebrew Compute

This is the official Homebrew tap for the certified
[Compute](https://github.com/rkendel1/compute) distribution.

```sh
brew tap rkendel1/compute
brew install compute
compute --version
compute
```

Base Compute is the standalone execution substrate. The separately certified
configured product uses the same Compute binary plus the exact published
ecosystem set in its compatibility manifest:

```sh
brew install compute-configured
compute-configured --version
compute-configured-verify
compute-configured-setup
```

The formula is currently available only on Linux x86_64, the platform for
which Compute publishes a certified distribution. It installs that GitHub
Release archive without rebuilding Compute or downloading runtimes separately.

Homebrew owns the immutable Compute executable and pinned runtime bundle.
Mutable state remains in `$COMPUTE_HOME` (default `~/.compute`) and survives
`brew upgrade compute`.

`compute-configured` depends on this formula; it does not build or install a
second Compute binary. Its immutable configured asset contains the locked npm
artifacts, registry integrity metadata, stack manifest, and verification
evidence. Homebrew never resolves arbitrary latest package versions.
