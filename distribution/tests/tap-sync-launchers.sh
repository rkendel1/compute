#!/bin/sh
# A release that ships the Chip launcher must produce a tap that ships it.
#
# This is the whole v0.1.16 defect in one test. The sync workflow fetched the
# release tag's templates, then rendered the tap's own older copies, so 0.1.16
# published a configured formula with correct URLs and correct checksums and no
# `compute-configured-chip`. Every check anyone had written for that formula
# passed; the installed product simply had no way to reach its agent runtime.
#
# The test builds a tap whose committed templates are that stale shape, renders it
# to reproduce the published defect, proves the guard refuses to publish that
# render, and then runs the real install-and-render path against the repository's
# release templates and requires the launcher to survive.
#
# The install-and-render path is the one the sync workflow runs: the same
# `install-release-templates.sh` and the same `update-formula.sh`, not copies.
set -eu

repository=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/compute-tap-sync-test.XXXXXX")
cleanup() { rm -rf "$work"; }
trap cleanup EXIT HUP INT TERM

fail() { echo "FAIL: $1" >&2; exit 1; }

version=1.2.3
checksum=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
configured_checksum=abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789
macos_checksum=1111111111111111111111111111111111111111111111111111111111111111
macos_configured_checksum=2222222222222222222222222222222222222222222222222222222222222222

templates="$repository/distribution/homebrew"
tap="$work/tap"

render() {
  sh "$tap/scripts/update-formula.sh" "$version" "$checksum" "$configured_checksum" \
    "$macos_checksum" "$macos_configured_checksum" > /dev/null
}

# ---- a tap whose committed templates predate the Chip launcher ----------------
#
# The stale copy is the real shape under test: the configured template with the
# agent-runtime launcher block removed, which is what the tap still carried when
# 0.1.16 was published on top of it.
mkdir -p "$tap/Formula" "$tap/scripts"
cp "$templates/Formula/compute.rb.in" "$tap/Formula/compute.rb.in"
cp "$templates/scripts/update-formula.sh" "$tap/scripts/update-formula.sh"
chmod +x "$tap/scripts/update-formula.sh"
sed -e '/# The agent runtime the configured profile declares/,/^    SH$/d' \
  "$templates/Formula/compute-configured.rb.in" > "$tap/Formula/compute-configured.rb.in"

# The launcher is the `(bin/"NAME").write` line. Matching on the write rather
# than on the bare name matters: the `test do` block also names the launcher, and
# a check that only looked for the name would pass on a stale template and prove
# nothing.
launcher='bin/"compute-configured-chip"'
if grep -qF "$launcher" "$tap/Formula/compute-configured.rb.in"; then
  fail "the stale tap template still ships the Chip launcher; it is not the shape under test"
fi

# Rendering the stale tap is exactly what published 0.1.16's formula.
render
if grep -qF "$launcher" "$tap/Formula/compute-configured.rb"; then
  fail "the stale template unexpectedly produced a Chip launcher"
fi

# ...and the guard must refuse that render rather than publish a tap that only
# looks healthy.
if "$templates/scripts/check-configured-launchers.sh" \
  "$tap/Formula/compute-configured.rb" "$tap/Formula/compute.rb" \
  "$templates/Formula/compute-configured.rb.in" \
  "$templates/Formula/compute.rb.in" > "$work/guard.log" 2>&1; then
  fail "the launcher guard accepted a tap formula that dropped compute-configured-chip"
fi
if ! grep -qF 'compute-configured-chip' "$work/guard.log"; then
  fail "the guard failed without naming the launcher it was protecting"
fi

# ---- the real sync path: install the release templates, then render ----------
"$templates/scripts/install-release-templates.sh" "$templates" "$tap" > /dev/null
if ! grep -qF "$launcher" "$tap/Formula/compute-configured.rb.in"; then
  fail "installing the release templates did not install the Chip launcher template"
fi

render
if ! grep -qF "$launcher" "$tap/Formula/compute-configured.rb"; then
  fail "a release that ships compute-configured-chip produced a tap formula without it"
fi

# The guard now accepts it, because the rendered launchers match the release tag.
"$tap/scripts/check-configured-launchers.sh" \
  "$tap/Formula/compute-configured.rb" "$tap/Formula/compute.rb" \
  "$templates/Formula/compute-configured.rb.in" \
  "$templates/Formula/compute.rb.in" > /dev/null ||
  fail "the launcher guard rejected a render that matches the release tag"

# The runtime-payload guard must survive the same install, or 0.1.16's staging
# guarantee would go with it.
[ -x "$tap/scripts/check-runtime-payload-invariant.sh" ] ||
  fail "the runtime payload guard was not installed into the tap"
"$tap/scripts/check-runtime-payload-invariant.sh" \
  "$tap/Formula/compute.rb" "the rendered compute formula" > /dev/null ||
  fail "the installed runtime payload guard rejected a correct render"

# Base Compute must never grow an agent launcher.
if grep -qF "$launcher" "$tap/Formula/compute.rb"; then
  fail "the base formula ships a Chip launcher"
fi

printf 'tap sync launcher contract passed\n'