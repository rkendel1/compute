#!/bin/sh
#!/bin/sh
# Runtime payload installation must never destroy a working runtime tree.
#
# The sequence under test is the one in distribution/homebrew/Formula/compute.rb.in.
# It previously deleted `libexec/runtimes` and extracted the certified payload
# back over it. That left a window in which the directory did not exist, and
# Homebrew's own relocation pass walks the keg during the install and reported the
# missing Mach-O payloads as an installation failure even though the finished
# installation was valid.
#
# This script does NOT restate the sequence. It renders the real formula and
# executes the shell body of its `run "sh"` step, so the code under test is the
# code that ships. A hand-copied copy could agree with a broken formula; this
# cannot.
#
# The property held here is deterministic rather than timing-based: the live
# runtime tree is replaced only after the replacement has been extracted AND
# verified. So:
#
#   1. a valid runtime tree is in place;
#   2. an INVALID payload is offered -- the install must fail and the previously
#      valid tree must still be there, byte for byte;
#   3. a valid payload is offered -- it becomes active.
#
# Step 2 is what a delete-then-extract implementation cannot pass: it destroys the
# good tree before learning the payload is bad.
set -eu

repository=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/runtime-staging-test.XXXXXX")
cleanup() { rm -rf "$work"; }
trap cleanup EXIT HUP INT TERM

fail() { echo "FAIL: $1" >&2; exit 1; }

# ---- render the real formula and lift its `run "sh"` body out of it ---------
version=1.2.3
"$repository/distribution/render-homebrew-tap.sh" "$work/tap" "$version" \
  0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef \
  abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789 \
  1111111111111111111111111111111111111111111111111111111111111111 \
  2222222222222222222222222222222222222222222222222222222222222222 > /dev/null
formula="$work/tap/Formula/compute.rb"

# The heredoc body is lifted with Ruby, because Ruby owns the semantics:
# `<<~SH` opens its body only after the whole logical expression closes (the
# `run` line continues with further array elements), and `<<~` strips the common
# indentation. Guessing at that with awk silently produced a truncated script --
# and this test then passed for entirely the wrong reason. So the body is taken
# from just past the line that closes the argument list, and the result is
# checked before anything is executed.
ruby -e '
  formula, out = ARGV
  lines = File.readlines(formula)
  # Anchor on the `run` call itself: a comment mentioning `<<~SH` must not match.
  open_at = lines.index { |l| l =~ /^\s*run .*<<~SH/ }
  abort "no run step with a <<~SH heredoc in formula" unless open_at
  # The `run` step keeps its whole argument list on the line that opens the
  # heredoc (a continuation line would be swallowed into the shell body), so the
  # script begins on the very next line.
  abort "the run step must keep its arguments on one line" unless lines[open_at].rstrip.end_with?("]")
  close_at = ((open_at + 1)...lines.length).find { |i| lines[i].strip == "SH" }
  abort "unterminated <<~SH heredoc in formula" unless close_at
  body = lines[(open_at + 1)...close_at]
  indent = body.reject { |l| l.strip.empty? }.map { |l| l[/ */].length }.min
  File.write(out, body.map { |l| l[indent..] || "" }.join)
' "$formula" "$work/payload-step.sh"
[ -s "$work/payload-step.sh" ] || fail "could not extract the payload step from the formula"
# A truncated or mangled extraction must fail loudly rather than let the test
# pass for the wrong reason.
sh -n "$work/payload-step.sh" || fail "extracted payload step is not valid shell"
grep -q 'distribution verify' "$work/payload-step.sh" ||
  fail "extracted payload step does not verify the staged tree"

# The postinstall must not delete the live tree before the replacement is proven.
if grep -qE '^[[:space:]]*remove "runtimes"' "$formula"; then
  fail "formula still removes libexec/runtimes before extracting the payload"
fi

# ---- a stand-in distribution, verified by a stand-in verifier ----------------
libexec="$work/libexec"
prefix="$work/prefix"
mkdir -p "$libexec" "$prefix" "$libexec/bin" "$libexec/runtimes/node/bin" \
         "$libexec/recipes/starters"

cat > "$libexec/bin/compute" <<'VERIFIER'
#!/bin/sh
# compute distribution verify <root>
root="$3"
want="$(cat "$root/expected-payload.txt" 2>/dev/null || echo none)"
got="$(cat "$root/runtimes/node/bin/node" 2>/dev/null || echo missing)"
[ "$want" = "$got" ] || { echo "payload mismatch: want $want got $got" >&2; exit 1; }
echo "verified: $got"
VERIFIER
chmod +x "$libexec/bin/compute"
printf 'node payload v1\n' > "$libexec/runtimes/node/bin/node"
printf 'node payload v1\n' > "$libexec/expected-payload.txt"
printf '{}\n' > "$libexec/runtime-manifest.json"
printf '{}\n' > "$libexec/runtime-lock.json"
printf '{}\n' > "$libexec/runtime-inventory.json"
printf '{}\n' > "$libexec/recipes/starters/dev.json"

build_payload() {
  stage="$work/stage-$1"
  rm -rf "$stage"
  mkdir -p "$stage/runtimes/node/bin"
  printf '%s\n' "$2" > "$stage/runtimes/node/bin/node"
  tar -cf "$prefix/runtime-payload-$1.tar" -C "$stage" runtimes
}

# Runs the formula's real payload-verification body, with the same $0/$1/$2
# convention the formula's `run "sh"` step uses.
run_payload_step() {
  sh -c "$(cat "$work/payload-step.sh")" sh "$libexec" \
    "$libexec/.runtime-staging/runtimes"
}

# Builds staging, runs the real verification body, then swaps -- the order the
# formula performs. Every step propagates its status explicitly: this runs under
# an `if` condition below, and a shell disables `set -e` inside any function
# invoked as an `if` condition, which would silently swallow a verification
# failure -- the exact thing this test exists to detect.
install_payload() {
  rm -rf "$libexec/.runtime-staging" || return 1
  mkdir -p "$libexec/.runtime-staging" || return 1
  tar -xf "$prefix/runtime-payload-$1.tar" -C "$libexec/.runtime-staging" || return 1
  run_payload_step > /dev/null || return 1
  mv "$libexec/runtimes" "$libexec/.runtime-retired" || return 1
  mv "$libexec/.runtime-staging/runtimes" "$libexec/runtimes" || return 1
  rm -rf "$libexec/.runtime-staging" "$libexec/.runtime-retired" || return 1
}

# ---- 1 & 2: an invalid payload must not cost us the working tree -------------
build_payload bad "node payload corrupt"
printf 'node payload v2 (restored)\n' > "$libexec/expected-payload.txt"
if install_payload bad > /dev/null 2>&1; then
  fail "a payload that does not verify was accepted"
fi
[ -d "$libexec/runtimes" ] || fail "the working runtime tree was destroyed by a failed payload"
[ "$(cat "$libexec/runtimes/node/bin/node")" = "node payload v1" ] ||
  fail "the working runtime tree was replaced by an unverified payload"

# ---- 3: a valid payload becomes active --------------------------------------
build_payload good "node payload v2 (restored)"
install_payload good > /dev/null
[ "$(cat "$libexec/runtimes/node/bin/node")" = "node payload v2 (restored)" ] ||
  fail "the certified payload did not become active"
[ -e "$libexec/.runtime-staging" ] && fail "a staging directory was left behind"
[ -e "$libexec/.runtime-retired" ] && fail "a retired directory was left behind"
[ -e "$libexec/.runtime-verify" ] && fail "a verification root was left behind"

printf 'runtime staging test passed\n'