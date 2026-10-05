#!/bin/sh
# The v0.1.16 incident was a correct release with a wrong live tap: the sync
# fetched the release templates and rendered the tap's own stale copies, so the
# published configured formula carried correct URLs and checksums and no
# `compute-configured-chip`. Every repository-side check passed; the installed
# product simply had no way to reach its agent runtime.
#
# This test closes that gap. It fetches the LIVE tap's published formulas and
# requires them to be byte-identical to what the authoritative release tag's
# own templates render for the release's own checksums. It is the only test
# that fails when the tap drifts from the release, and the only one that can
# certify the consumer path rather than the repository path.
#
# Usage: live-tap-release.sh <version> <linux_sha> <configured_linux_sha> \
#          <macos_sha> <macos_configured_sha>
#
# The five arguments are the release's own identity: version plus the four
# authoritative checksums from the release's `.sha256` sidecars. Nothing is
# rebuilt and nothing local is substituted: the expected formulas are rendered
# from the release TAG's templates (`git show <tag>:...`), which is the
# publication source of truth, and the actual formulas come from the live tap
# over the network.
set -eu

[ "$#" -eq 5 ] || {
  echo "usage: live-tap-release.sh <version> <linux_sha> <configured_linux_sha> <macos_sha> <macos_configured_sha>" >&2
  exit 1
}

repository=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
version=$1
linux_sha=$2
configured_linux_sha=$3
macos_sha=$4
macos_configured_sha=$5
tag="v$version"

fail() { echo "FAIL: $1" >&2; exit 1; }

work=$(mktemp -d "${TMPDIR:-/tmp}/compute-live-tap-test.XXXXXX")
cleanup() { rm -rf "$work"; }
trap cleanup EXIT HUP INT TERM

# ---- the release tag is the source of truth --------------------------------
git -C "$repository" rev-parse --verify --quiet "$tag" > /dev/null \
  || fail "release tag $tag does not exist in this repository"
for template in Formula/compute.rb.in Formula/compute-configured.rb.in scripts/update-formula.sh; do
  git -C "$repository" cat-file -e "$tag:distribution/homebrew/$template" 2>/dev/null \
    || fail "release tag $tag has no distribution/homebrew/$template"
done
mkdir -p "$work/tag"
git -C "$repository" archive "$tag" distribution/homebrew | tar -x -C "$work/tag"

# ---- expected: the tag's own templates rendered with the release checksums --
sh "$work/tag/distribution/homebrew/scripts/install-release-templates.sh" \
  "$work/tag/distribution/homebrew" "$work/expected" > /dev/null \
  || fail "could not install the $tag templates into the renderer's input path"
sh "$work/expected/scripts/update-formula.sh" \
  "$version" "$linux_sha" "$configured_linux_sha" \
  "$macos_sha" "$macos_configured_sha" > /dev/null \
  || fail "could not render the expected formulas from the $tag templates"

# ---- actual: the live tap, over the network, not the local checkout --------
for formula in compute compute-configured; do
  curl -fsSL --proto '=https' --proto-redir '=https' \
    "https://raw.githubusercontent.com/rkendel1/homebrew-compute/main/Formula/$formula.rb" \
    -o "$work/$formula.live.rb" \
    || fail "could not fetch the live $formula formula from the tap"
done

# ---- the live tap must be exactly what the release renders ------------------
for formula in compute compute-configured; do
  if ! cmp -s "$work/expected/Formula/$formula.rb" "$work/$formula.live.rb"; then
    fail "the live $formula formula differs from the $tag render; the tap has drifted from the release"
  fi
done

# ---- the consumer-relevant invariants, on the live bytes --------------------
launcher='bin/"compute-configured-chip"'
grep -qF "$launcher" "$work/compute-configured.live.rb" \
  || fail "the live configured formula ships no compute-configured-chip launcher"
if grep -qF 'compute-chip' "$work/compute.live.rb"; then
  fail "the live base formula ships a Chip launcher"
fi
grep -q "compute-$version-#{platform}.tar.gz" "$work/compute.live.rb" \
  || fail "the live base formula does not reference the v$version artifacts"
grep -q "compute-configured-$version-#{platform}.tar.gz" "$work/compute-configured.live.rb" \
  || fail "the live configured formula does not reference the v$version artifacts"
for sum in "$linux_sha" "$configured_linux_sha" "$macos_sha" "$macos_configured_sha"; do
  grep -q "\"$sum\"" "$work/compute.live.rb" || grep -q "\"$sum\"" "$work/compute-configured.live.rb" \
    || fail "live checksum $sum matches neither live formula"
done
ruby -c "$work/compute.live.rb" | grep -q 'Syntax OK' \
  || fail "the live base formula is not valid Ruby"
ruby -c "$work/compute-configured.live.rb" | grep -q 'Syntax OK' \
  || fail "the live configured formula is not valid Ruby"

printf 'live tap matches the %s release render\\n' "$tag"
