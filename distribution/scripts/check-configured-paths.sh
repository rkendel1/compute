#!/bin/sh
set -eu

# A configured asset is assembled from published packages and this repository;
# nothing in it may name the machine that assembled it. Search everything the
# release ships outside the verbatim registry packages (whose integrity the
# profile verifier already checks) for checkout, home and runner paths and for
# local file or tarball references.
#
# Usage: check-configured-paths.sh <assembled configured root>

root=${1:?usage: check-configured-paths.sh <configured root>}
# A file: dependency spec or a local tarball shows up as a JSON value; the bare
# "file:" URL scheme literal inside bundled library code is not a reference.
pattern='/Users/[A-Za-z0-9._-]+/|/home/(runner|[a-z][a-z0-9_-]*/(work|Developer|src))/|~/Developer|/Developer/|file:///?[A-Za-z~]|": *"file:|": *"[./~][^"]*\.tgz"|/compatibility/published-stack'
if grep -rIlE "$pattern" --exclude-dir=node_modules "$root" >&2; then
  echo "the configured asset at $root names a machine-specific path (files above)" >&2
  exit 1
fi
# The lockfile and every installed manifest must resolve to the registry.
if grep -rIlE '"(resolved|_resolved)": *"(file:|git|https://github)' "$root/package-lock.json" >&2; then
  echo "the configured lockfile resolves a package outside the npm registry" >&2
  exit 1
fi
printf 'configured asset names no machine-specific path: %s\n' "$root"
