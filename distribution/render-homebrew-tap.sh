#!/bin/sh
set -eu

if [ "$#" -ne 3 ]; then
  echo "usage: $0 TAP_DIRECTORY VERSION SHA256" >&2
  exit 2
fi

tap=$1
version=$2
checksum=$3
case "$version" in ''|*[!0-9A-Za-z._-]*) echo "invalid version: $version" >&2; exit 2 ;; esac
case "$checksum" in *[!0-9A-Fa-f]*|'') echo "invalid SHA-256: $checksum" >&2; exit 2 ;; esac
[ "${#checksum}" -eq 64 ] || { echo "invalid SHA-256: $checksum" >&2; exit 2; }

source_dir=$(CDPATH='' cd -- "$(dirname "$0")/homebrew" && pwd)
mkdir -p "$tap/Formula" "$tap/.github/workflows"
sed -e "s/@VERSION@/$version/g" -e "s/@SHA256@/$checksum/g" \
  "$source_dir/Formula/compute.rb.in" > "$tap/Formula/compute.rb"
cp "$source_dir/README.md" "$tap/README.md"
cp "$source_dir/.github/workflows/tests.yml" "$tap/.github/workflows/tests.yml"
