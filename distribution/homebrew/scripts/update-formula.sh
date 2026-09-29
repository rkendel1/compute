#!/bin/sh
set -eu

if [ "$#" -ne 5 ]; then
  echo "usage: $0 VERSION LINUX_BASE_SHA256 LINUX_CONFIGURED_SHA256 MACOS_BASE_SHA256 MACOS_CONFIGURED_SHA256" >&2
  exit 2
fi

version=$1
checksum=$2
configured_checksum=$3
macos_checksum=$4
macos_configured_checksum=$5
case "$version" in ''|*[!0-9A-Za-z._-]*) echo "invalid version: $version" >&2; exit 2 ;; esac
case "$checksum" in *[!0-9A-Fa-f]*|'') echo "invalid SHA-256: $checksum" >&2; exit 2 ;; esac
[ "${#checksum}" -eq 64 ] || { echo "invalid SHA-256: $checksum" >&2; exit 2; }
case "$configured_checksum" in *[!0-9A-Fa-f]*|'') echo "invalid configured SHA-256: $configured_checksum" >&2; exit 2 ;; esac
[ "${#configured_checksum}" -eq 64 ] || { echo "invalid configured SHA-256: $configured_checksum" >&2; exit 2; }
case "$macos_checksum" in *[!0-9A-Fa-f]*|'') echo "invalid macOS SHA-256: $macos_checksum" >&2; exit 2 ;; esac
[ "${#macos_checksum}" -eq 64 ] || { echo "invalid macOS SHA-256: $macos_checksum" >&2; exit 2; }
case "$macos_configured_checksum" in *[!0-9A-Fa-f]*|'') echo "invalid configured macOS SHA-256: $macos_configured_checksum" >&2; exit 2 ;; esac
[ "${#macos_configured_checksum}" -eq 64 ] || { echo "invalid configured macOS SHA-256: $macos_configured_checksum" >&2; exit 2; }

root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
sed -e "s/@VERSION@/$version/g" -e "s/@LINUX_SHA256@/$checksum/g" -e "s/@MACOS_SHA256@/$macos_checksum/g" \
  "$root/Formula/compute.rb.in" > "$root/Formula/compute.rb"
sed -e "s/@VERSION@/$version/g" -e "s/@LINUX_CONFIGURED_SHA256@/$configured_checksum/g" -e "s/@MACOS_CONFIGURED_SHA256@/$macos_configured_checksum/g" \
  "$root/Formula/compute-configured.rb.in" > "$root/Formula/compute-configured.rb"
