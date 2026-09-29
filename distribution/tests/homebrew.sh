#!/bin/sh
set -eu

repository=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/compute-homebrew-test.XXXXXX")
cleanup() { rm -rf "$work"; }
trap cleanup EXIT HUP INT TERM

version=1.2.3
checksum=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
configured_checksum=abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789
"$repository/distribution/render-homebrew-tap.sh" "$work/tap" "$version" "$checksum" "$configured_checksum"

formula="$work/tap/Formula/compute.rb"
ruby -c "$formula" | grep -q 'Syntax OK'
grep -q "releases/download/v$version/compute-$version-linux-x86_64.tar.gz" "$formula"
if grep -q '^  version ' "$formula"; then
  echo "formula must let Homebrew derive the version from the release URL" >&2
  exit 1
fi
grep -q "sha256 \"$checksum\"" "$formula"
grep -q 'depends_on :linux' "$formula"
grep -q 'depends_on arch: :x86_64' "$formula"
grep -q 'libexec.install' "$formula"
grep -q 'COMPUTE_HOME' "$formula"
configured_formula="$work/tap/Formula/compute-configured.rb"
ruby -c "$configured_formula" | grep -q 'Syntax OK'
grep -q "compute-configured-$version-linux-x86_64.tar.gz" "$configured_formula"
grep -q "sha256 \"$configured_checksum\"" "$configured_formula"
grep -q 'depends_on "rkendel1/compute/compute"' "$configured_formula"
if grep -q 'depends_on "node' "$configured_formula"; then
  echo "configured formula must use Compute's certified bundled Node runtime" >&2
  exit 1
fi
grep -q 'opt_libexec}/runtimes/node/bin/node' "$configured_formula"
grep -q 'compute-configured-verify' "$configured_formula"
grep -q 'compute-configured-setup' "$configured_formula"
grep -q 'COMPUTE_INSTALLED_VERSION' "$configured_formula"
test -f "$work/tap/.github/workflows/tests.yml"
test -f "$work/tap/.github/workflows/sync.yml"
test -x "$work/tap/scripts/update-formula.sh"
test -f "$work/tap/README.md"

if "$repository/distribution/render-homebrew-tap.sh" "$work/bad" "$version" bad "$configured_checksum" 2>/dev/null; then
  echo "invalid checksum was accepted" >&2
  exit 1
fi

printf 'homebrew contract passed\n'
