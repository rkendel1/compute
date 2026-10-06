#!/bin/sh
set -eu

# Run the configured runtime smoke test (configured-chip-fx.mjs):
#
#   compute-configured-chip start -> Chip -> fx() -> @appport/fx -> model fixture
#
# Against an installation, point COMPUTE_TEST_LAUNCHER at its launcher (for
# Homebrew: "$(brew --prefix)/bin/compute-configured-chip"). Against an
# assembled configured tree, set COMPUTE_TEST_CONFIGURED_ROOT to it: the
# launcher is then rendered from the formula template and executed with that
# tree as its libexec, so the shipped launcher text is what runs. Either way the
# Chip, FX and agent under test are the artifacts, never the source checkout.
#
# COMPUTE_TEST_NODE is the Node the rendered launcher uses (the bundled one on
# an installation; the runner's Node 24 on a build runner).
# COMPUTE_TEST_EVIDENCE optionally names a file for the JSON evidence.

repository=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
node=${COMPUTE_TEST_NODE:-/opt/homebrew/opt/compute/libexec/runtimes/node/bin/node}
work=$(mktemp -d "${TMPDIR:-/tmp}/compute-chip-fx-test.XXXXXX")
cleanup() { rm -rf "$work"; }
trap cleanup EXIT HUP INT TERM

if [ -n "${COMPUTE_TEST_LAUNCHER:-}" ]; then
  launcher=$COMPUTE_TEST_LAUNCHER
else
  configured=$(CDPATH='' cd -- "${COMPUTE_TEST_CONFIGURED_ROOT:?set COMPUTE_TEST_LAUNCHER or COMPUTE_TEST_CONFIGURED_ROOT}" && pwd)
  case $node in /*) ;; *) node="$PWD/$node" ;; esac
  "$repository/distribution/render-homebrew-tap.sh" "$work/tap" 1.2.3 \
    0000000000000000000000000000000000000000000000000000000000000000 \
    1111111111111111111111111111111111111111111111111111111111111111 \
    2222222222222222222222222222222222222222222222222222222222222222 \
    3333333333333333333333333333333333333333333333333333333333333333 >/dev/null
  mkdir -p "$work/compute/runtimes/node/bin" "$work/bin"
  ln -s "$node" "$work/compute/runtimes/node/bin/node"
  sed -n -E '/bin\/"compute-configured-chip"\)\.write/,/^    SH$/p' "$work/tap/Formula/compute-configured.rb" \
    | sed -e '1d' -e '$d' -e 's/^      //' \
    | sed -e "s|#{libexec}|$configured|g" -e "s|#{formula_opt_libexec(\"compute\")}|$work/compute|g" \
    > "$work/bin/compute-configured-chip"
  chmod +x "$work/bin/compute-configured-chip"
  launcher="$work/bin/compute-configured-chip"
fi

"$node" "$repository/distribution/tests/configured-chip-fx.mjs" "$launcher" ${COMPUTE_TEST_EVIDENCE:+"$COMPUTE_TEST_EVIDENCE"}
