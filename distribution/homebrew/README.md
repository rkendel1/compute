# Homebrew Compute

This is the official Homebrew tap for the certified
[Compute](https://github.com/rkendel1/compute) distribution.

```sh
brew tap rkendel1/compute
brew install compute
compute --version
compute
```

The formula is currently available only on Linux x86_64, the platform for
which Compute publishes a certified distribution. It installs that GitHub
Release archive without rebuilding Compute or downloading runtimes separately.

Homebrew owns the immutable Compute executable and pinned runtime bundle.
Mutable state remains in `$COMPUTE_HOME` (default `~/.compute`) and survives
`brew upgrade compute`.
