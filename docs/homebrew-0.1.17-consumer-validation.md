# Compute 0.1.17 through the live Homebrew tap: consumer validation

Validated 2026-10-05 against release **`v0.1.17`** (tag object `5703ff7`,
commit `e149961`) and the live tap **`rkendel1/homebrew-compute@90eb29e`**.
The automated invariant that now protects this path is
[`distribution/tests/live-tap-release.sh`](../distribution/tests/live-tap-release.sh).

## Why this exists

v0.1.16 was a correct release with a wrong live tap. The tap sync fetched the
release templates and then rendered the tap's own stale copies, so the
published `compute-configured` formula had correct URLs and checksums but no
`compute-configured-chip`. Every repository-side test passed, because none of
them read the formulas consumers actually install.

## Reference evidence

| Status | Meaning |
| --- | --- |
| **PASS** | Exercised against the stated release and tap, and it held |
| **MANUAL** | Performed by the operator on the consumer host, not by the script |

### Live consumer upgrade (MANUAL)

Performed on the consumer host below, through the live tap:

| Step | Result |
| --- | --- |
| An existing Compute 0.1.16 consumer (base and configured) was present | MANUAL |
| `brew upgrade` through the live tap | MANUAL |
| Base upgraded to Compute 0.1.17 | MANUAL |
| Configured upgraded to Compute 0.1.17 | MANUAL |
| `compute-configured-chip` exists | MANUAL |
| `compute-configured-chip` executes Chip 0.54.3 | MANUAL |
| `compute-configured-verify` passes | MANUAL |
| Base Cellar contains no Chip references | MANUAL |

### Automated re-verification of the same host (PASS)

`distribution/tests/live-tap-release.sh` (consumer mode `require`), run after the upgrade:

| Invariant | Result |
| --- | --- |
| Local `v0.1.17` tag is the published tag object | PASS |
| Each `.sha256` sidecar equals GitHub's digest of the published artifact (4 assets) | PASS |
| Live formulas are valid Ruby with the expected `Formula` class and stanzas | PASS |
| Live formulas reference only `v0.1.17`, resolve to the release's own asset URLs for both platforms, and carry exactly the release checksums | PASS |
| Live configured formula ships `compute-configured-chip` executing the pinned `node_modules/.bin/chip` | PASS |
| Live and release-rendered base formula is Chip-free | PASS |
| Live formulas are byte-identical to the `v0.1.17` templates rendered with the release checksums | PASS |
| Both formulas installed from `rkendel1/compute` (receipt tap head `90eb29e`, installed 2026-10-05 17:50 UTC) | PASS |
| `compute --version` → `compute 0.1.17` (via Homebrew's opt link, not PATH) | PASS |
| `compute-configured --version` → `compute 0.1.17` | PASS |
| Installed launcher resolves `@appport/chip@0.54.3`, the version the `v0.1.17` profile declares | PASS |
| `compute-configured-chip --version` (the profile's health invocation) executes Chip and prints `0.54.3` | PASS |
| `compute-configured-verify` reports `"result": "pass"` | PASS |
| Installed base keg has no `*chip*` file and no `@appport/chip` / `compute-configured-chip` reference | PASS |

`distribution/tests/live-tap-release-failures.sh` replayed the v0.1.16
incident and six other corruptions of the live formulas. Each one failed with
its own label.

## Consumer host

| | |
| --- | --- |
| OS | macOS 27.0, arm64 (the Preview platform) |
| Homebrew | 7.0.8 |
| Ruby | 3.4.4 |

The Linux x86_64 (Certified) consumer path was not installed for this
validation. Its formula URL and checksum are verified by the script on every
host; its installed runtime is not.
