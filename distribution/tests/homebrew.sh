#!/bin/sh
set -eu

repository=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/compute-homebrew-test.XXXXXX")
cleanup() { rm -rf "$work"; }
trap cleanup EXIT HUP INT TERM

version=1.2.3
checksum=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
"$repository/distribution/render-homebrew-tap.sh" "$work/tap" "$version" "$checksum"

formula="$work/tap/Formula/compute.rb"
ruby -c "$formula" | grep -q 'Syntax OK'
grep -q "releases/download/v$version/compute-$version-linux-x86_64.tar.gz" "$formula"
grep -q "version \"$version\"" "$formula"
grep -q "sha256 \"$checksum\"" "$formula"
grep -q 'depends_on :linux' "$formula"
grep -q 'depends_on arch: :x86_64' "$formula"
grep -q 'libexec.install' "$formula"
grep -q 'COMPUTE_HOME' "$formula"
test -f "$work/tap/.github/workflows/tests.yml"
test -f "$work/tap/README.md"

if "$repository/distribution/render-homebrew-tap.sh" "$work/bad" "$version" bad 2>/dev/null; then
  echo "invalid checksum was accepted" >&2
  exit 1
fi

printf 'homebrew contract passed\n'
