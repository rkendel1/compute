#!/bin/sh
set -eu

# live-tap-release.sh is only a release gate if it fails, and fails with the
# boundary that broke. This test runs it against the real release and the real
# live tap, with a `curl` shim that passes every request through unchanged and
# then corrupts exactly one fetched tap formula. Each corruption must fail with
# its own label.
#
# The first case replays the v0.1.16 incident itself: the live configured
# formula is rendered from the stale v0.1.15 template with the current
# release's own version and checksums, so URLs and checksums are all correct
# and only the Chip launcher is gone.
#
# Needs the network, like the test it exercises. Usage:
#   live-tap-release-failures.sh [VERSION]

repository=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/compute-live-tap-failures.XXXXXX")
cleanup() { rm -rf "$work"; }
trap cleanup EXIT HUP INT TERM

real_curl=$(command -v curl)
git -C "$repository" show v0.1.15:distribution/homebrew/Formula/compute-configured.rb.in > "$work/stale.rb.in"

mkdir -p "$work/bin"
cat > "$work/bin/curl" <<SHIM
#!/bin/sh
out=; url=; previous=
for argument; do
  [ "\$previous" = -o ] && out=\$argument
  case "\$argument" in https://*) url=\$argument ;; esac
  previous=\$argument
done
"$real_curl" "\$@" || exit
case "\$url" in
  https://raw.githubusercontent.com/rkendel1/homebrew-compute/*/Formula/\$LIVE_TAP_MUTATE.rb)
    sh -c "\$LIVE_TAP_MUTATION" sh "\$out" ;;
esac
SHIM
chmod +x "$work/bin/curl"

# expect LABEL FORMULA MUTATION: MUTATION is a shell snippet that edits the
# fetched FORMULA at "$1"; the live tap test must then fail with LABEL.
expect() {
  label=$1
  formula=$2
  if PATH="$work/bin:$PATH" LIVE_TAP_MUTATE=$formula LIVE_TAP_MUTATION=$3 \
      COMPUTE_LIVE_TAP_CONSUMER=skip \
      "$repository/distribution/tests/live-tap-release.sh" ${version:+"$version"} \
      > "$work/out" 2> "$work/err"; then
    echo "live tap test passed a $formula formula that should fail with $label" >&2
    exit 1
  fi
  grep -q "^FAIL: $label:" "$work/err" || {
    echo "live tap test failed for the wrong reason; expected $label:" >&2
    cat "$work/err" >&2
    exit 1
  }
  printf 'ok  %s\n' "$label"
}
version=${1:-}

# The v0.1.16 incident: the stale template, the current version and checksums.
expect 'CONFIGURED CHIP PAYLOAD MISSING' compute-configured "
  version=\$(grep -o 'releases/download/v[^/]*/' \"\$1\" | head -n 1 | sed 's|releases/download/v||; s|/||')
  set -- \"\$1\" \$(grep -oE '\"[0-9a-f]{64}\"' \"\$1\" | tr -d '\"')
  sed -e \"s/@VERSION@/\$version/g\" -e \"s/@MACOS_CONFIGURED_SHA256@/\$2/g\" \
      -e \"s/@LINUX_CONFIGURED_SHA256@/\$3/g\" '$work/stale.rb.in' > \"\$1\""
expect 'LIVE TAP VERSION MISMATCH' compute \
  "sed -i.bak 's|releases/download/v[^/]*/|releases/download/v0.0.1/|' \"\$1\""
expect 'LIVE TAP ARTIFACT URL MISMATCH' compute-configured \
  "sed -i.bak 's|#{platform}|linux-x86_64|' \"\$1\""
expect 'LIVE TAP CHECKSUM MISMATCH' compute \
  "perl -pi -e 's/\"[0-9a-f]{64}\"/\"\${\\(\"0\" x 64)}\"/' \"\$1\""
expect 'BASE COMPUTE CONTAINS CHIP PAYLOAD' compute \
  "printf '# chip\n' >> \"\$1\""
expect 'INVALID HOMEBREW FORMULA' compute-configured \
  "printf 'end\n' >> \"\$1\""
expect 'LIVE TAP FORMULA DRIFT' compute \
  "sed -i.bak 's|^  desc \".*\"|  desc \"Something else\"|' \"\$1\""

printf 'live tap failure classes verified\n'
