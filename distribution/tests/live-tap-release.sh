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
# Usage: live-tap-release.sh [VERSION]
#
# VERSION (`0.1.17` or `v0.1.17`) names the release under test; without it the
# latest published GitHub release is tested. Everything else is derived from
# that release, so nothing here is specific to one version:
#
#   release tag (must match the published tag object)
#     -> the tag's distribution/homebrew templates, rendered by the tag's renderer
#     -> the release's `.sha256` sidecars, each cross-checked against GitHub's
#        own digest of the published artifact bytes
#     -> expected formulas
#     -> the live tap, pinned to its current commit, over the network
#     -> Ruby validity, version, URLs, checksums, Chip payload invariants
#     -> byte-for-byte comparison
#     -> the installed consumer, where this host is one
#
# Nothing is rebuilt and nothing local is substituted for the tap.
#
# COMPUTE_LIVE_TAP_CONSUMER selects consumer-runtime verification:
#   auto     (default) verify the Homebrew installation when both formulas are
#            installed from the tap, and report NOT VERIFIED otherwise;
#   require  fail unless the installed consumer is verified;
#   skip     formula verification only.
# GITHUB_TOKEN, when set, authenticates the three GitHub API reads; it is
# passed to curl on stdin and never printed.
set -eu

[ "$#" -le 1 ] || {
  echo "usage: live-tap-release.sh [VERSION]" >&2
  exit 2
}

repository=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
source_repository=rkendel1/compute
tap_repository=rkendel1/homebrew-compute
tap_branch=main
tap_name=rkendel1/compute
consumer=${COMPUTE_LIVE_TAP_CONSUMER:-auto}
case "$consumer" in
  auto|require|skip) ;;
  *) echo "COMPUTE_LIVE_TAP_CONSUMER must be auto, require or skip" >&2; exit 2 ;;
esac

fail() { printf 'FAIL: %s\n' "$1" >&2; exit 1; }

work=$(mktemp -d "${TMPDIR:-/tmp}/compute-live-tap-test.XXXXXX")
cleanup() { rm -rf "$work"; }
trap cleanup EXIT HUP INT TERM

for tool in curl git python3 ruby; do
  command -v "$tool" > /dev/null 2>&1 || fail "TOOL MISSING: $tool is required to verify the live tap"
done

fetch() {
  curl -fsSL --proto '=https' --proto-redir '=https' --retry 3 "$1" -o "$2"
}

# The token, if any, reaches curl through a config on stdin rather than argv,
# so it is never visible in the process table or in failure output.
api() {
  if [ -n "${GITHUB_TOKEN:-}" ]; then
    printf 'header = "Authorization: Bearer %s"\n' "$GITHUB_TOKEN" |
      curl -fsSL --proto '=https' --retry 3 -K - \
        -H 'Accept: application/vnd.github+json' "https://api.github.com/$1" -o "$2"
  else
    curl -fsSL --proto '=https' --retry 3 \
      -H 'Accept: application/vnd.github+json' "https://api.github.com/$1" -o "$2"
  fi
}

# json FILE EXPRESSION: print a Python expression evaluated over the document `d`.
json() {
  python3 -c 'import json, sys; d = json.load(open(sys.argv[1])); print(eval(sys.argv[2]))' "$1" "$2"
}

# ---- the release under test -------------------------------------------------
if [ "$#" -eq 1 ]; then
  requested=${1#v}
  api "repos/$source_repository/releases/tags/v$requested" "$work/release.json" \
    || fail "RELEASE UNREACHABLE: no published release v$requested in $source_repository"
else
  api "repos/$source_repository/releases/latest" "$work/release.json" \
    || fail "RELEASE UNREACHABLE: could not read the latest release of $source_repository"
fi
tag=$(json "$work/release.json" 'd["tag_name"]')
version=${tag#v}
case "$version" in
  [0-9]*.[0-9]*.[0-9]*) ;;
  *) fail "RELEASE TAG INVALID: $tag is not a v<major>.<minor>.<patch> release tag" ;;
esac
case "$version" in *[!0-9.]*) fail "RELEASE TAG INVALID: $tag is not a v<major>.<minor>.<patch> release tag" ;; esac
[ "$(json "$work/release.json" 'd["draft"] or d["prerelease"]')" = False ] \
  || fail "RELEASE NOT STABLE: $tag is a draft or prerelease; the tap publishes stable releases only"

# ---- the release tag is the source of truth --------------------------------
# The local tag must be the published tag, or the templates rendered below are
# not the release's.
api "repos/$source_repository/git/ref/tags/$tag" "$work/tag-ref.json" \
  || fail "RELEASE TAG MISSING: $source_repository has no published tag $tag"
published_tag=$(json "$work/tag-ref.json" 'd["object"]["sha"]')
local_tag=$(git -C "$repository" rev-parse --verify --quiet "refs/tags/$tag") \
  || fail "RELEASE TAG MISSING: release tag $tag does not exist in this repository; fetch the tags"
[ "$local_tag" = "$published_tag" ] \
  || fail "RELEASE TAG MISMATCH: local $tag is $local_tag but the published tag is $published_tag"
release_commit=$(git -C "$repository" rev-parse "$tag^{commit}")
for template in Formula/compute.rb.in Formula/compute-configured.rb.in \
    scripts/update-formula.sh scripts/install-release-templates.sh; do
  git -C "$repository" cat-file -e "$tag:distribution/homebrew/$template" 2>/dev/null \
    || fail "RELEASE TEMPLATE MISSING: release tag $tag has no distribution/homebrew/$template"
done
mkdir -p "$work/tag"
git -C "$repository" archive "$tag" distribution/homebrew | tar -x -C "$work/tag"

# The Chip runtime the release's configured profile declares. The configured
# distribution is Chip-capable by definition, so a release without one is not
# a release this tap can publish.
git -C "$repository" show "$tag:compatibility/published-stack/stack.json" > "$work/stack.json" 2>/dev/null \
  || fail "CONFIGURED CHIP PAYLOAD MISSING: release tag $tag has no compatibility/published-stack/stack.json"
chip() {
  json "$work/stack.json" "(lambda r: $1)(next(r for r in d['agent']['runtimes'] if r['name'] == 'chip'))" 2>/dev/null \
    || fail "CONFIGURED CHIP PAYLOAD MISSING: the $tag configured profile declares no chip agent runtime"
}
chip_package=$(chip "r['package']")
chip_version=$(chip "r['version']")
chip_health=$(chip "' '.join(r['health']['command'])")
chip_expect=$(chip "r['health']['expect']")

# ---- the release's checksums, proved against GitHub's own digests -----------
# Each `.sha256` sidecar is what the release declares; GitHub's asset digest is
# what it actually serves. They must agree before either is trusted.
release_sha() {
  asset=$1
  digest=$(json "$work/release.json" \
    "next((a.get('digest') or '') for a in d['assets'] if a['name'] == '$asset')" 2>/dev/null) \
    || fail "RELEASE ARTIFACT MISSING: $tag publishes no $asset"
  case "$digest" in
    sha256:*) digest=${digest#sha256:} ;;
    *) fail "RELEASE DIGEST UNAVAILABLE: GitHub reports no sha256 digest for the $tag asset $asset" ;;
  esac
  fetch "https://github.com/$source_repository/releases/download/$tag/$asset.sha256" "$work/$asset.sha256" \
    || fail "RELEASE CHECKSUM MISSING: $tag publishes no $asset.sha256"
  read -r declared declared_name < "$work/$asset.sha256" \
    || fail "RELEASE CHECKSUM MALFORMED: $asset.sha256 is empty"
  [ "${declared_name#\*}" = "$asset" ] \
    || fail "RELEASE CHECKSUM MALFORMED: $asset.sha256 names $declared_name"
  [ "$declared" = "$digest" ] \
    || fail "RELEASE CHECKSUM MISMATCH: $asset.sha256 declares $declared but GitHub serves $digest"
  printf '%s\n' "$declared"
}
linux_sha=$(release_sha "compute-$version-linux-x86_64.tar.gz")
macos_sha=$(release_sha "compute-$version-macos-aarch64.tar.gz")
configured_linux_sha=$(release_sha "compute-configured-$version-linux-x86_64.tar.gz")
macos_configured_sha=$(release_sha "compute-configured-$version-macos-aarch64.tar.gz")

# ---- expected: the tag's own templates rendered with the release checksums --
sh "$work/tag/distribution/homebrew/scripts/install-release-templates.sh" \
  "$work/tag/distribution/homebrew" "$work/expected" > /dev/null \
  || fail "TEMPLATE INSTALL FAILED: could not install the $tag templates into the renderer's input path"
sh "$work/expected/scripts/update-formula.sh" \
  "$version" "$linux_sha" "$configured_linux_sha" \
  "$macos_sha" "$macos_configured_sha" > /dev/null \
  || fail "EXPECTED RENDER FAILED: could not render the expected formulas from the $tag templates"

# ---- actual: the live tap, over the network, not a local checkout ----------
# The branch is resolved to a commit first and the formulas are read at that
# commit, so both files come from one tap state and the report names it.
api "repos/$tap_repository/commits/$tap_branch" "$work/tap-commit.json" \
  || fail "LIVE TAP UNREACHABLE: could not resolve $tap_repository@$tap_branch"
tap_commit=$(json "$work/tap-commit.json" 'd["sha"]')
for formula in compute compute-configured; do
  fetch "https://raw.githubusercontent.com/$tap_repository/$tap_commit/Formula/$formula.rb" \
    "$work/$formula.live.rb" \
    || fail "LIVE TAP UNREACHABLE: could not fetch the live $formula formula from $tap_repository@$tap_commit"
done

# ---- the live formulas must be valid Homebrew Ruby before anything else -----
for formula in compute compute-configured; do
  live="$work/$formula.live.rb"
  ruby -c "$live" > /dev/null 2> "$work/ruby.err" \
    || fail "INVALID HOMEBREW FORMULA: the live $formula formula is not valid Ruby: $(head -n 3 "$work/ruby.err")"
  class=$(printf '%s\n' "$formula" | awk -F- '{ for (i = 1; i <= NF; i++) printf "%s%s", toupper(substr($i, 1, 1)), substr($i, 2) }')
  grep -Eq "^class $class < Formula\$" "$live" \
    || fail "INVALID HOMEBREW FORMULA: the live $formula formula does not define class $class < Formula"
  for stanza in desc homepage url sha256 license; do
    grep -Eq "^  $stanza " "$live" \
      || fail "INVALID HOMEBREW FORMULA: the live $formula formula has no $stanza stanza"
  done
done

# ---- version, artifact URLs and checksums, on the live bytes ----------------
# Every release URL must point at this release's own asset for both platforms,
# and the formula must carry exactly this release's two checksums: no stale,
# foreign, substituted or extra artifact survives.
check_artifacts() {
  python3 - "$work/$1.live.rb" "$work/release.json" "$1" "$tag" "$version" "$2" "$3" <<'PY'
import json, re, sys
path, release, name, tag, version, macos, linux = sys.argv[1:]
text = open(path).read()
assets = {a["name"]: a["browser_download_url"] for a in json.load(open(release))["assets"]}

def fail(message):
    print(message)
    sys.exit(1)

for foreign in sorted(set(re.findall(r"releases/download/([^/\"]+)/", text))):
    if foreign != tag:
        fail("LIVE TAP VERSION MISMATCH: the live %s formula references release %s, expected %s"
             % (name, foreign, tag))
urls = re.findall(r'^\s*url "([^"]+)"', text, re.M)
if len(urls) != 1:
    fail("LIVE TAP ARTIFACT URL MISMATCH: the live %s formula declares %d urls, expected 1" % (name, len(urls)))
for platform in ("linux-x86_64", "macos-aarch64"):
    asset = "%s-%s-%s.tar.gz" % (name, version, platform)
    expected = assets.get(asset)
    if expected is None:
        fail("LIVE TAP ARTIFACT URL MISMATCH: %s publishes no %s" % (tag, asset))
    actual = urls[0].replace("#{platform}", platform)
    if actual != expected:
        fail("LIVE TAP ARTIFACT URL MISMATCH: the live %s formula resolves %s to %s, expected %s"
             % (name, platform, actual, expected))
sums = set(re.findall(r'"([0-9A-Fa-f]{64})"', text))
if sums != {macos, linux}:
    fail("LIVE TAP CHECKSUM MISMATCH: the live %s formula carries %s; the %s artifacts are %s"
         % (name, ", ".join(sorted(sums)) or "no checksum", tag, ", ".join(sorted({macos, linux}))))
PY
}
message=$(check_artifacts compute "$macos_sha" "$linux_sha") || fail "$message"
message=$(check_artifacts compute-configured "$macos_configured_sha" "$configured_linux_sha") || fail "$message"

# ---- the Chip payload belongs to configured Compute, and only there ---------
launcher='bin/"compute-configured-chip"'
grep -qF "$launcher" "$work/expected/Formula/compute-configured.rb" \
  || fail "CONFIGURED CHIP PAYLOAD MISSING: the $tag configured template ships no compute-configured-chip launcher"
grep -qF "$launcher" "$work/compute-configured.live.rb" \
  || fail "CONFIGURED CHIP PAYLOAD MISSING: the live configured formula ships no compute-configured-chip launcher"
grep -qF 'node_modules/.bin/chip' "$work/compute-configured.live.rb" \
  || fail "CONFIGURED CHIP PAYLOAD MISSING: the live configured launcher does not execute the pinned chip runtime"
for formula in "$work/expected/Formula/compute.rb" "$work/compute.live.rb"; do
  if grep -qi 'chip' "$formula"; then
    fail "BASE COMPUTE CONTAINS CHIP PAYLOAD: the $( [ "$formula" = "$work/compute.live.rb" ] && echo live || echo "$tag" ) base formula mentions chip"
  fi
done

# ---- the live tap must be exactly what the release renders ------------------
for formula in compute compute-configured; do
  if ! cmp -s "$work/expected/Formula/$formula.rb" "$work/$formula.live.rb"; then
    diff -u "$work/expected/Formula/$formula.rb" "$work/$formula.live.rb" \
      | sed -e "s|$work/expected/Formula/|release $tag: |" -e "s|$work/|live tap: |" \
      | head -n 60 >&2 || true
    fail "LIVE TAP FORMULA DRIFT: the live $formula formula differs from the $tag render; the tap has drifted from the release"
  fi
done

printf 'release:   %s (tag %s, commit %s)\n' "$tag" "$published_tag" "$release_commit"
printf 'live tap:  %s@%s (Formula/compute.rb, Formula/compute-configured.rb)\n' "$tap_repository" "$tap_commit"
printf 'live tap validation: PASS -- both formulas are valid Ruby, byte-identical to the %s render, reference only %s artifacts with their published checksums; configured ships compute-configured-chip (%s@%s); base is Chip-free\n' \
  "$tag" "$tag" "$chip_package" "$chip_version"

# ---- live consumer: the installed product, where this host is that consumer --
# Formula verification above is mandatory everywhere. Consumer verification
# needs both formulas installed from the tap; it resolves them through
# Homebrew's own opt links, never through PATH, so a development `compute`
# elsewhere on PATH cannot stand in for the installed product.
not_verified() {
  [ "$consumer" = require ] && fail "LIVE CONSUMER NOT VERIFIED: $1"
  printf 'live consumer validation: NOT VERIFIED -- %s\n' "$1"
  exit 0
}
[ "$consumer" = skip ] && not_verified "skipped by COMPUTE_LIVE_TAP_CONSUMER=skip"
command -v brew > /dev/null 2>&1 || not_verified "Homebrew is not installed on this host"
brew_prefix=$(brew --prefix)
for formula in compute compute-configured; do
  receipt="$brew_prefix/opt/$formula/INSTALL_RECEIPT.json"
  [ -f "$receipt" ] || not_verified "$formula is not installed through Homebrew on this host"
  [ "$(json "$receipt" 'd["source"]["tap"]')" = "$tap_name" ] \
    || not_verified "$formula is not installed from the $tap_name tap on this host"
done

base_keg=$(CDPATH='' cd -- "$brew_prefix/opt/compute" && pwd -P)
configured_keg=$(CDPATH='' cd -- "$brew_prefix/opt/compute-configured" && pwd -P)

installed=$("$base_keg/bin/compute" --version 2>/dev/null) \
  || fail "INSTALLED BASE FAILED: compute --version did not run from $base_keg"
[ "$installed" = "compute $version" ] \
  || fail "INSTALLED VERSION MISMATCH: installed compute reports '$installed', expected 'compute $version' (brew upgrade)"
chip_files=$(find "$base_keg" -iname '*chip*' | head -n 5)
[ -z "$chip_files" ] \
  || fail "BASE COMPUTE CONTAINS CHIP PAYLOAD: the installed base keg contains $chip_files"
chip_refs=$(grep -rIlF -e '@appport/chip' -e 'compute-configured-chip' "$base_keg" | head -n 5 || true)
[ -z "$chip_refs" ] \
  || fail "BASE COMPUTE CONTAINS CHIP PAYLOAD: the installed base keg references Chip in $chip_refs"

configured_installed=$("$configured_keg/bin/compute-configured" --version 2>/dev/null) \
  || fail "INSTALLED CONFIGURED FAILED: compute-configured --version did not run from $configured_keg"
[ "$configured_installed" = "compute $version" ] \
  || fail "INSTALLED VERSION MISMATCH: installed compute-configured reports '$configured_installed', expected 'compute $version' (brew upgrade)"
chip_launcher="$configured_keg/bin/compute-configured-chip"
[ -x "$chip_launcher" ] \
  || fail "CONFIGURED CHIP PAYLOAD MISSING: the installed configured keg has no executable compute-configured-chip"
chip_manifest="$configured_keg/libexec/node_modules/$chip_package/package.json"
[ -f "$chip_manifest" ] \
  || fail "CONFIGURED CHIP PAYLOAD MISSING: the installed configured keg has no $chip_package"
resolved_chip=$(json "$chip_manifest" 'd["name"] + "@" + d["version"]')
[ "$resolved_chip" = "$chip_package@$chip_version" ] \
  || fail "CONFIGURED CHIP RUNTIME FAILED: the installed launcher resolves $resolved_chip, the $tag profile declares $chip_package@$chip_version"
# The profile's own health invocation is the execution proof: it runs Chip
# through the launcher, the bundled Node and the pinned node_modules.
# shellcheck disable=SC2086 # the declared health command is a word list
actual_chip=$("$chip_launcher" $chip_health 2>"$work/chip.err") \
  || fail "CONFIGURED CHIP RUNTIME FAILED: compute-configured-chip $chip_health exited non-zero: $(head -n 3 "$work/chip.err")"
[ "$actual_chip" = "$chip_expect" ] \
  || fail "CONFIGURED CHIP RUNTIME FAILED: compute-configured-chip $chip_health printed '$actual_chip', the $tag profile expects '$chip_expect'"
"$configured_keg/bin/compute-configured-verify" > "$work/verify.json" 2>/dev/null \
  || fail "CONFIGURED VERIFY FAILED: compute-configured-verify exited non-zero"
[ "$(json "$work/verify.json" 'd["result"]')" = pass ] \
  || fail "CONFIGURED VERIFY FAILED: compute-configured-verify did not report result pass"

printf 'live consumer validation: PASS -- %s and compute-configured %s installed from %s; compute-configured-chip executes %s@%s; configured verify passes; base keg is Chip-free\n' \
  "$installed" "$version" "$tap_name" "$chip_package" "$actual_chip"
