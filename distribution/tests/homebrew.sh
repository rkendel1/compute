#!/bin/sh
set -eu

repository=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/compute-homebrew-test.XXXXXX")
cleanup() { rm -rf "$work"; }
trap cleanup EXIT HUP INT TERM

version=1.2.3
checksum=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
configured_checksum=abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789
macos_checksum=1111111111111111111111111111111111111111111111111111111111111111
macos_configured_checksum=2222222222222222222222222222222222222222222222222222222222222222
"$repository/distribution/render-homebrew-tap.sh" "$work/tap" "$version" "$checksum" "$configured_checksum" "$macos_checksum" "$macos_configured_checksum"

formula="$work/tap/Formula/compute.rb"
ruby -c "$formula" | grep -q 'Syntax OK'
grep -q "releases/download/v$version/compute-$version-#{platform}.tar.gz" "$formula"
grep -q 'platform = OS.mac? ? "macos-aarch64" : "linux-x86_64"' "$formula"
if grep -q '^  version ' "$formula"; then
  echo "formula must let Homebrew derive the version from the release URL" >&2
  exit 1
fi
grep -q "\"$checksum\"" "$formula"
grep -q "\"$macos_checksum\"" "$formula"
grep -q 'depends_on arch: OS.mac? ? :arm64 : :x86_64' "$formula"
grep -q 'skip_clean "libexec"' "$formula"
grep -q 'libexec.install' "$formula"
grep -q 'post_install_steps do' "$formula"
grep -q 'runtime-payload.tar' "$formula"
grep -q 'COMPUTE_HOME' "$formula"
grep -q 'recipe starters --json' "$formula"
grep -q 'recipes/starters/dev.json' "$formula"
configured_formula="$work/tap/Formula/compute-configured.rb"
ruby -c "$configured_formula" | grep -q 'Syntax OK'
grep -q "compute-configured-$version-#{platform}.tar.gz" "$configured_formula"
grep -q 'platform = OS.mac? ? "macos-aarch64" : "linux-x86_64"' "$configured_formula"
grep -q "\"$configured_checksum\"" "$configured_formula"
grep -q "\"$macos_configured_checksum\"" "$configured_formula"
grep -q 'depends_on "rkendel1/compute/compute"' "$configured_formula"
grep -q 'skip_clean "libexec"' "$configured_formula"
if grep -q 'depends_on "node' "$configured_formula"; then
  echo "configured formula must use Compute's certified bundled Node runtime" >&2
  exit 1
fi
grep -q 'formula_opt_libexec("compute")}/runtimes/node/bin/node' "$configured_formula"
grep -q 'compute-configured-verify' "$configured_formula"
grep -q 'compute-configured-setup' "$configured_formula"
grep -q 'COMPUTE_INSTALLED_VERSION' "$configured_formula"
grep -q 'compute-configured recipe starters --json' "$configured_formula"
grep -q 'recipes/starters/dev.json' "$configured_formula"
# The configured wrapper is the only thing that tells the shared Compute binary
# where the installed distribution lives. Without COMPUTE_CONFIGURED_HOME,
# `managed::distribution_home()` finds no profile, `compute up` starts no
# managed service, nothing is registered, and the Services page never discovers
# `GET /v1/ui`: a silently absent feature, not a visible failure.
grep -q 'export COMPUTE_CONFIGURED_HOME=' "$configured_formula"

# Chip belongs to the configured distribution and nowhere else. Its launcher,
# the real runtime it starts, and the failure it gives when that runtime is
# missing are proved in their own contract, which renders these same formulas.
"$repository/distribution/tests/configured-chip.sh"
"$repository/distribution/tests/configured-rust-chip.sh"

test -f "$work/tap/.github/workflows/tests.yml"
test -f "$work/tap/.github/workflows/sync.yml"
test -x "$work/tap/scripts/update-formula.sh"
test -f "$work/tap/README.md"
# A seeded tap must carry the guards it needs to check what it renders. v0.1.16
# shipped without them, so nothing in the tap could notice that its rendered
# formula had lost the Chip launcher.
test -x "$work/tap/scripts/check-runtime-payload-invariant.sh"
test -x "$work/tap/scripts/check-configured-launchers.sh"
test -x "$work/tap/scripts/install-release-templates.sh"

# The runtime payload postinstall must stage and verify before it replaces the
# live runtime tree. `runtime-payload-staging.sh` renders the real formula and
# runs its payload step, so it fails both if the postinstall reverts to deleting
# `libexec/runtimes` before extracting and if that step stops verifying. It is a
# behavioural test, not a source check.
"$repository/distribution/tests/runtime-payload-staging.sh"

# The release templates must actually reach the renderer. v0.1.16 fetched them,
# rendered from the tap's stale copies, and published a configured formula with no
# Chip launcher. This reproduces that: a stale tap template, the release
# templates installed into the renderer's own input path, and the rendered formula
# required to carry the launcher.
"$repository/distribution/tests/tap-sync-launchers.sh"

# The rendered formula is what the sync workflow publishes, and v0.1.15 shipped a
# formula carrying correct checksums and the delete-before-extract install logic.
# The invariant check therefore runs against Formula/compute.rb as rendered here,
# not against the template.
"$repository/distribution/homebrew/scripts/check-runtime-payload-invariant.sh" \
  "$formula" "the rendered compute formula"

# The launcher set is checked on the rendered formulas too, because the rendered
# file is what ships.
"$repository/distribution/homebrew/scripts/check-configured-launchers.sh" \
  "$configured_formula" "$formula" \
  "$work/tap/Formula/compute-configured.rb.in" "$work/tap/Formula/compute.rb.in"

if "$repository/distribution/render-homebrew-tap.sh" "$work/bad" "$version" bad "$configured_checksum" "$macos_checksum" "$macos_configured_checksum" 2>/dev/null; then
  echo "invalid checksum was accepted" >&2
  exit 1
fi

printf 'homebrew contract passed\n'
