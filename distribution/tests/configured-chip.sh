#!/bin/sh
set -eu

# Chip is a component of the *configured* distribution only. This test proves
# both halves of that statement:
#
#   * the configured formula owns the launcher, and it runs the real Chip
#     runtime from an installed-shaped tree;
#   * a missing runtime fails loudly and non-zero, never falling back to
#     another agent;
#   * base Compute grows no Chip launcher and claims no Chip component.

repository=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/compute-chip-test.XXXXXX")
cleanup() { rm -rf "$work"; }
trap cleanup EXIT HUP INT TERM

version=1.2.3
checksum=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
configured_checksum=abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789
macos_checksum=1111111111111111111111111111111111111111111111111111111111111111
macos_configured_checksum=2222222222222222222222222222222222222222222222222222222222222222
"$repository/distribution/render-homebrew-tap.sh" "$work/tap" "$version" \
  "$checksum" "$configured_checksum" "$macos_checksum" "$macos_configured_checksum"
base_formula="$work/tap/Formula/compute.rb"
configured_formula="$work/tap/Formula/compute-configured.rb"
stack="$repository/compatibility/published-stack"

# ---- The configured formula owns the launcher; base Compute must not --------

grep -q 'bin/"compute-configured-chip"' "$configured_formula"
if grep -q 'compute-chip' "$base_formula"; then
  echo "base Compute must not ship a Chip launcher" >&2
  exit 1
fi
# The base profile is the source of truth for what base Compute claims to be.
if grep -qi chip "$repository/compatibility/base-compute.json"; then
  echo "the base distribution profile must not reference Chip" >&2
  exit 1
fi

# ---- The configured profile declares and verifies Chip ---------------------
#
# Chip is proved by running it, but the declaration is what makes the run
# meaningful: `verify.mjs` executes the runtime named in the profile and fails
# certification if it is absent, so the packaging path cannot ship a configured
# distribution that merely mentions Chip.

grep -q 'agent_runtime' "$stack/verify.mjs"
grep -q '@appport/chip' "$stack/package.json"
grep -q '"@appport/chip"' "$stack/package-lock.json"

# ---- The launcher runs the real Chip runtime -------------------------------
#
# Running the launcher needs an installed runtime: a Node, and a `node_modules`
# the profile has actually resolved. Release certification points this at the
# *assembled* configured distribution it has just built, so the launcher is
# proved against the bytes that ship rather than against the source tree.

node=${COMPUTE_TEST_NODE:-/opt/homebrew/opt/compute/libexec/runtimes/node/bin/node}
modules=${COMPUTE_TEST_MODULES:-$stack/node_modules}
# The mirror below links each package by absolute path, because a package's own
# resolution is relative to the package, not to whatever directory the caller
# happened to be in. A relative COMPUTE_TEST_MODULES would therefore be linked
# from the wrong root and look like a missing runtime.
case $modules in
  /*) ;;
  *) modules="$PWD/$modules" ;;
esac
case $node in
  /*) ;;
  *) node="$PWD/$node" ;;
esac
if [ ! -d "$modules/@appport/chip" ] || [ ! -x "$node" ]; then
  # The launcher was not exercised. A partial check must never read as the whole
  # contract, so this says so plainly, and a caller that requires the runtime
  # (release certification) turns the skip into a failure.
  echo "chip launcher NOT verified: no installed Chip runtime at $modules (node: $node)" >&2
  if [ -n "${COMPUTE_TEST_REQUIRE_RUNTIME:-}" ]; then
    echo "COMPUTE_TEST_REQUIRE_RUNTIME is set: the launcher must be exercised" >&2
    exit 1
  fi
  printf 'chip integration contract: formulas only, launcher NOT verified\n'
  exit 0
fi

# A mirror of the installed tree the test owns: every package and bin is a
# symlink into the real tree, so the packages run exactly as they ship, but
# `.bin/chip` can be removed to exercise the failure path without touching the
# source checkout or the assembled distribution.
mkdir -p "$work/modules/.bin"
(cd "$modules" && find . -maxdepth 1 -mindepth 1 ! -name .bin \
  -exec ln -s "$PWD/{}" "$work/modules/{}" \;)
(cd "$modules/.bin" && find . -maxdepth 1 -mindepth 1 ! -name chip \
  -exec ln -s "$PWD/{}" "$work/modules/.bin/{}" \;)
ln -s "$modules/.bin/chip" "$work/modules/.bin/chip"

# The distribution root the launcher resolves, shaped like an installed keg.
root="$work/keg/libexec"
mkdir -p "$root/stacks" "$root/runtimes/node/bin" "$work/keg/bin"
ln -s "$work/modules" "$root/node_modules"
ln -s "$node" "$root/runtimes/node/bin/node"
# The configured agent the launcher serves ships beside the modules it uses.
configured=$(dirname "$modules")
for entry in agent package.json .output; do
  if [ ! -e "$configured/$entry" ]; then
    echo "the configured tree at $configured has no $entry (build it with distribution/scripts/build-configured-agent.sh)" >&2
    exit 1
  fi
  ln -s "$configured/$entry" "$root/$entry"
done

# The launcher body, extracted from the rendered formula and stripped of the
# six-space heredoc indent, with the two formula-provided paths substituted the
# way Homebrew does. Executing the rendered text is what makes this a test of
# the shipped launcher rather than of a copy of it.
sed -n -E '/bin\/"compute-configured-chip"\)\.write/,/^    SH$/p' "$configured_formula" \
  | sed -e '1d' -e '$d' -e 's/^      //' \
  | sed -e "s|#{libexec}|$root|g" -e "s|#{formula_opt_libexec(\"compute\")}|$work/keg/libexec|g" \
  > "$work/keg/bin/compute-configured-chip"
chmod +x "$work/keg/bin/compute-configured-chip"
launcher="$work/keg/bin/compute-configured-chip"

expected=$("$node" "$modules/.bin/chip" --version)
actual=$("$launcher" --version)
test "$actual" = "$expected" || {
  echo "launcher reported '$actual', the runtime reports '$expected'" >&2
  exit 1
}
printf 'chip launcher ran the installed runtime: %s\n' "$actual"

# ---- A missing runtime fails loudly, and does not fall back ----------------

rm "$work/modules/.bin/chip"
if "$launcher" --version 2>"$work/missing.err"; then
  echo "the launcher must fail when the Chip runtime is missing" >&2
  exit 1
fi
grep -q 'Chip runtime is missing' "$work/missing.err" || {
  echo "the launcher must name the missing runtime" >&2
  cat "$work/missing.err" >&2
  exit 1
}
# A missing Chip must not be papered over by another agent.
if grep -qE '@appport/(core|runtime|services)' "$work/missing.err"; then
  echo "the launcher must not substitute a different agent runtime" >&2
  exit 1
fi

printf 'chip integration contract passed\n'