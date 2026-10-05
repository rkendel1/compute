#!/bin/sh
# Guards the launcher set of the RENDERED Homebrew formulas against the launcher
# set of the release tag's templates.
#
# v0.1.16 published a configured formula with correct URLs and correct checksums
# and no `compute-configured-chip` launcher, because the sync fetched the release
# tag's templates and then rendered the tap's own older copies. The tap looked
# healthy in every dimension anyone had checked, and the installed product was
# missing the entry point to the agent runtime its own profile declares.
#
# The invariant:
#
#   the rendered formulas write exactly the launchers the release tag's
#   templates write, and the base formula ships no agent launcher at all
#
# so a template change can never again be dropped silently between a release and
# the tap that installs it. Nothing here recomputes a checksum:
# `compute distribution verify` remains the only authority for payload identity.
set -eu

if [ "$#" -ne 4 ]; then
  echo "usage: $0 RENDERED_CONFIGURED RENDERED_BASE TEMPLATE_CONFIGURED TEMPLATE_BASE" >&2
  exit 2
fi

rendered_configured=$1
rendered_base=$2
template_configured=$3
template_base=$4

fail() {
  echo "configured launcher contract violated: $1" >&2
  exit 1
}

for file in "$rendered_configured" "$rendered_base" "$template_configured" "$template_base"; do
  [ -f "$file" ] || fail "no file at $file"
done

# A launcher is `(bin/"NAME").write` in `install`. Anchoring on the line start
# keeps a mention in a comment or a `test do` assertion from counting as one.
launchers() {
  sed -n -E 's/^[[:space:]]*\(bin\/"([^"]+)"\)\.write.*/\1/p' "$1" | LC_ALL=C sort
}

require_same_launchers() {
  label=$1
  rendered=$2
  template=$3
  expected=$(launchers "$template")
  actual=$(launchers "$rendered")
  for launcher in $expected; do
    if ! echo "$actual" | grep -qx "$launcher"; then
      fail "$label ships $launcher in the release tag but the rendered formula drops it"
    fi
  done
  for launcher in $actual; do
    if ! echo "$expected" | grep -qx "$launcher"; then
      fail "$label adds $launcher, which the release tag does not ship"
    fi
  done
}

require_same_launchers "the configured formula" "$rendered_configured" "$template_configured"
require_same_launchers "the base formula" "$rendered_base" "$template_base"

# The one guarantee this guard was written for, stated so the failure reads as
# itself rather than as a diff: a release that ships the Chip launcher must
# produce a tap formula that ships it.
if launchers "$template_configured" | grep -qx 'compute-configured-chip'; then
  if ! launchers "$rendered_configured" | grep -qx 'compute-configured-chip'; then
    fail "the release ships compute-configured-chip and the rendered formula dropped it"
  fi
fi

# Chip is a configured component. The base artifact contains no Chip runtime for
# such a wrapper to run, so base Compute must never grow a launcher for one.
if launchers "$rendered_base" | grep -qi 'chip'; then
  fail "the base formula ships a Chip launcher; Chip is a configured component"
fi

printf 'configured launcher contract held\n'