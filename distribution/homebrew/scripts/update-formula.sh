#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: $0 VERSION SHA256" >&2
  exit 2
fi

version=$1
checksum=$2
case "$version" in ''|*[!0-9A-Za-z._-]*) echo "invalid version: $version" >&2; exit 2 ;; esac
case "$checksum" in *[!0-9A-Fa-f]*|'') echo "invalid SHA-256: $checksum" >&2; exit 2 ;; esac
[ "${#checksum}" -eq 64 ] || { echo "invalid SHA-256: $checksum" >&2; exit 2; }

root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
sed -e "s/@VERSION@/$version/g" -e "s/@SHA256@/$checksum/g" \
  "$root/Formula/compute.rb.in" > "$root/Formula/compute.rb"
