#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
  echo "usage: certify-distribution.sh ASSEMBLED_DISTRIBUTION" >&2
  exit 2
fi

distribution=$1
test -x "$distribution/bin/compute" || { echo "Compute distribution is missing" >&2; exit 1; }
command -v jq >/dev/null 2>&1 || { echo "jq is required" >&2; exit 1; }
"$distribution/bin/compute" distribution verify "$distribution" --json
temporary=$(mktemp -d "${TMPDIR:-/tmp}/compute-distribution-certification.XXXXXX")
trap 'rm -rf "$temporary"' EXIT HUP INT TERM
COMPUTE_HOME="$distribution" COMPUTE_REQUIRE_ALL_RUNTIMES=1 \
  "$distribution/bin/compute" certify --json > "$temporary/bare.json"

image="compute-certification:${COMPUTE_CERTIFICATION_TAG:-local}"
docker build \
  -f distribution/Dockerfile \
  --build-arg COMPUTE_DISTRIBUTION="$distribution" \
  -t "$image" .
docker run --rm \
  -e COMPUTE_REQUIRE_ALL_RUNTIMES=1 \
  "$image" certify --json > "$temporary/docker.json"

jq -S '[.runtimes[] | {runtime, locked_version, result}]' "$temporary/bare.json" > "$temporary/bare-matrix.json"
jq -S '[.runtimes[] | {runtime, locked_version, result}]' "$temporary/docker.json" > "$temporary/docker-matrix.json"
cmp "$temporary/bare-matrix.json" "$temporary/docker-matrix.json" || {
  echo "Bare Linux and Docker runtime matrices differ" >&2
  exit 1
}
cat "$temporary/bare.json"
cat "$temporary/docker.json"
