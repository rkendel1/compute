#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
  echo "usage: certify-distribution.sh ASSEMBLED_DISTRIBUTION" >&2
  exit 2
fi

distribution=$1
test -x "$distribution/bin/compute" || { echo "Compute distribution is missing" >&2; exit 1; }
COMPUTE_HOME="$distribution" COMPUTE_REQUIRE_ALL_RUNTIMES=1 \
  "$distribution/bin/compute" certify --json

image="compute-certification:${COMPUTE_CERTIFICATION_TAG:-local}"
docker build \
  -f distribution/Dockerfile \
  --build-arg COMPUTE_DISTRIBUTION="$distribution" \
  -t "$image" .
docker run --rm \
  -e COMPUTE_REQUIRE_ALL_RUNTIMES=1 \
  "$image" certify --json
