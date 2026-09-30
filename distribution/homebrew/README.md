# Homebrew Compute

This is the official Homebrew tap for
[Compute](https://github.com/rkendel1/compute) distribution.

```sh
brew tap rkendel1/compute
brew trust rkendel1/compute
brew install compute
compute --version
compute
```

Base Compute is the standalone execution substrate. The separately verified
configured product uses the same Compute binary plus the exact published
ecosystem set in its compatibility manifest:

```sh
brew install compute-configured
compute-configured --version
compute-configured-verify
compute-configured-setup
```

Both formulas install the same seven certified/preview starter recipe assets.
List them without starting a controller, then make an editable user recipe:

```sh
compute recipe starters
compute recipe create developer --from dev
compute environment create workstation --recipe developer
```

The formula selects a platform release artifact:

- Linux x86_64 is Certified and contains the complete certified runtime set.
- macOS ARM64 is Preview and contains the supported native runtime subset.
  Linux-only runtimes are recorded as Unavailable with an explicit reason;
  they are not silently discovered from the host or described as certified.

Inspect the installed platform, status, and runtime availability with:

```sh
compute distribution inspect "$(brew --prefix compute)/libexec"
```

Both variants install a GitHub Release archive without rebuilding Compute or
downloading runtimes separately.

Homebrew owns the immutable Compute executable and pinned runtime bundle.
Mutable state remains in `$COMPUTE_HOME` (default `~/.compute`) and survives
`brew upgrade compute`.

`compute-configured` depends on this formula; it does not build or install a
second Compute binary. Its immutable configured asset contains the locked npm
artifacts, registry integrity metadata, stack manifest, and verification
evidence. Homebrew never resolves arbitrary latest package versions.
