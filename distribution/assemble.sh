#!/bin/sh
set -eu

if [ "$#" -ne 4 ]; then
  echo "usage: assemble.sh COMPUTE_BINARY IGNORED_PAYLOAD_ROOT PLATFORM OUTPUT_DIR" >&2
  echo "prefer: compute distribution build --output OUTPUT_DIR" >&2
  exit 2
fi

compute_binary=$1
platform=$3
output_dir=$4

echo "assemble.sh is a compatibility wrapper; using the canonical Compute builder" >&2
exec "$compute_binary" distribution build \
  --compute-binary "$compute_binary" \
  --platform "$platform" \
  --output "$output_dir"
