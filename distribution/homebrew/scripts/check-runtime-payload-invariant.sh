#!/bin/sh
# Guards the runtime-payload installation invariant of a RENDERED Homebrew formula.
#
# The formula that ships is not the template: it is `compute.rb.in` with the
# release version and checksums substituted. v0.1.15 published a formula with
# correct v0.1.15 checksums and the v0.1.14 installation logic, so validating the
# template -- or the rendered file only in the repository's own tests -- is not
# enough. This script takes the rendered formula and refuses to pass if the
# payload is activated by deleting the live runtime tree first.
#
# The invariant:
#
#   stage -> verify -> rename/swap -> live runtimes
#
# and the forbidden behaviour:
#
#   remove "runtimes", base: :libexec, recursive: true
#   tar -xf runtime-payload.tar -C libexec
#
# which destroys a working runtime tree before its replacement is known good.
#
# This deliberately checks the rendered installation logic, not the whole
# payload: `compute distribution verify` remains the only authority for runtime
# payload identities and checksums. Nothing here recomputes a checksum.
set -eu

if [ "$#" -lt 1 ]; then
  echo "usage: $0 RENDERED_FORMULA [LABEL]" >&2
  exit 2
fi

formula=$1
label=${2:-$formula}

fail() {
  echo "runtime payload invariant violated in $label: $1" >&2
  exit 1
}

[ -f "$formula" ] || fail "no rendered formula at $formula"

# Any `remove` of the runtimes tree, however spelled, destroys the live tree
# before a replacement is staged and verified. Refuse the whole class rather
# than one literal, so a reformatted copy cannot slip through.
if grep -Eq '^[[:space:]]*remove[[:space:]]+("runtimes"|%q\{runtimes\})' "$formula"; then
  fail "the live runtime tree is removed before its replacement is staged and verified"
fi

# The safe shape stages first, proves the staged tree, then swaps by rename.
# Require all three so a partial "fix" cannot satisfy the guard.
grep -q 'runtime-staging' "$formula" ||
  fail "the payload is not staged before activation"
grep -q 'distribution verify' "$formula" ||
  fail "the staged payload is not proved with compute distribution verify"
grep -Eq 'run "mv", args:' "$formula" ||
  fail "the staged tree is not activated by rename"

printf 'runtime payload invariant held in %s\n' "$label"
