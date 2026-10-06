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
artifacts, registry integrity metadata, stack manifest, the built configured
agent, and verification evidence. Homebrew never resolves arbitrary latest
package versions.

The configured stack:

```text
compute-configured
├── Compute       execution
├── Chip 0.54.4   agent runtime          (@appport/chip)
└── FX 0.1.0      model/provider layer   (@appport/fx)
```

Chip runs the agent; FX makes its model calls to the provider you configure;
Compute stays the execution substrate and loads neither. Serve the configured
agent with a provider of your choice; no Vercel or AI Gateway account is used:

```sh
export COMPUTE_CONFIGURED_CHIP_TOKEN=…        # bearer token for the HTTP API
export FX_BASE_URL=http://localhost:11434/v1  # any OpenAI-compatible endpoint
export FX_MODEL=qwen3-coder
compute-configured-chip start --port 3000     # POST /eve/v1/session
```

For an endpoint that needs a key, set `FX_API_KEY_ENV` to the *name* of the
variable holding it. `compute-configured-verify` reports the installed
versions and proves the Chip → FX → provider path against a local fixture.
