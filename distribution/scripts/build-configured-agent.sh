#!/bin/sh
set -eu

# Build the configured Chip agent into <stack>/.output.
#
# <stack> is a configured stack tree: `agent/`, `package.json` and the
# `node_modules` that `npm ci` installed from the published registry packages.
# `chip build` compiles the agent with the Chip that tree pins and records the
# directory it built in as provenance. It also embeds the real path of every
# package it bundles, so building in a CI checkout would put that runner's path
# into the release. The build therefore runs at one fixed, machine-independent
# root, with `node_modules` physically inside it (a symlink would leak its
# target), and only `.output` is carried back. Chip serves a relocated build
# from wherever it is started; the provenance path is never read at run time.
#
# Usage: build-configured-agent.sh <stack> [node]

stack=$(CDPATH='' cd -- "${1:?usage: build-configured-agent.sh <stack> [node]}" && pwd)
node=${2:-node}
build_root=/tmp/compute-configured-agent

test -f "$stack/agent/agent.ts" || { echo "no configured agent at $stack/agent" >&2; exit 1; }
test -x "$stack/node_modules/.bin/chip" || { echo "no installed Chip at $stack/node_modules" >&2; exit 1; }

rm -rf "$build_root"
mkdir -p "$build_root"
cp -R "$stack/agent" "$stack/package.json" "$build_root/"
# Moved, not copied, so the build uses exactly the installed bytes; it is moved
# back whatever happens.
mv "$stack/node_modules" "$build_root/node_modules"
restore() { [ -d "$build_root/node_modules" ] && mv "$build_root/node_modules" "$stack/node_modules"; rm -rf "$build_root"; }
trap restore EXIT HUP INT TERM

(cd "$build_root" && EVE_TELEMETRY_DISABLED=1 "$node" node_modules/.bin/chip build >/dev/null)

rm -rf "$stack/.output"
cp -R "$build_root/.output" "$stack/.output"

# The only paths a release may carry are the fixed build root above and the
# installation's own. A checkout, home directory or runner workspace here means
# the build leaked the machine it ran on.
if grep -rlE "/Users/[A-Za-z0-9._-]+/|/home/runner/|/Developer/|$stack" "$stack/.output" >&2; then
  echo "the configured agent build embeds a machine-specific path" >&2
  exit 1
fi
printf 'configured agent built: %s\n' "$stack/.output"
