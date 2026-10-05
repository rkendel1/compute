#!/bin/sh
set -eu

if [ "$#" -ne 6 ]; then
  echo "usage: $0 TAP_DIRECTORY VERSION LINUX_BASE_SHA256 LINUX_CONFIGURED_SHA256 MACOS_BASE_SHA256 MACOS_CONFIGURED_SHA256" >&2
  exit 2
fi

tap=$1
version=$2
checksum=$3
configured_checksum=$4
macos_checksum=$5
macos_configured_checksum=$6
case "$version" in ''|*[!0-9A-Za-z._-]*) echo "invalid version: $version" >&2; exit 2 ;; esac
case "$checksum" in *[!0-9A-Fa-f]*|'') echo "invalid SHA-256: $checksum" >&2; exit 2 ;; esac
[ "${#checksum}" -eq 64 ] || { echo "invalid SHA-256: $checksum" >&2; exit 2; }
case "$configured_checksum" in *[!0-9A-Fa-f]*|'') echo "invalid configured SHA-256: $configured_checksum" >&2; exit 2 ;; esac
[ "${#configured_checksum}" -eq 64 ] || { echo "invalid configured SHA-256: $configured_checksum" >&2; exit 2; }
case "$macos_checksum" in *[!0-9A-Fa-f]*|'') echo "invalid macOS SHA-256: $macos_checksum" >&2; exit 2 ;; esac
[ "${#macos_checksum}" -eq 64 ] || { echo "invalid macOS SHA-256: $macos_checksum" >&2; exit 2; }
case "$macos_configured_checksum" in *[!0-9A-Fa-f]*|'') echo "invalid configured macOS SHA-256: $macos_configured_checksum" >&2; exit 2 ;; esac
[ "${#macos_configured_checksum}" -eq 64 ] || { echo "invalid configured macOS SHA-256: $macos_configured_checksum" >&2; exit 2; }

source_dir=$(CDPATH='' cd -- "$(dirname "$0")/homebrew" && pwd)
mkdir -p "$tap/Formula" "$tap/.github/workflows"
mkdir -p "$tap/scripts"
# Every template and every guard, derived from the directory rather than listed, so
# a tap seeded from this release can render and can check what it renders. The tap
# runs these same scripts when it synchronizes a later release.
for path in $(find "$source_dir/Formula" -type f -name '*.rb.in' | LC_ALL=C sort) \
            $(find "$source_dir/scripts" -type f -name '*.sh' | LC_ALL=C sort); do
  cp "$path" "$tap/${path#"$source_dir"/}"
done
chmod +x "$tap"/scripts/*.sh
# A seeded tap that cannot install and check its own render is not seeded.
for required in Formula/compute.rb.in Formula/compute-configured.rb.in \
                scripts/update-formula.sh \
                scripts/check-runtime-payload-invariant.sh \
                scripts/check-configured-launchers.sh; do
  [ -s "$tap/$required" ] || { echo "seeded tap is missing $required" >&2; exit 1; }
done
"$tap/scripts/update-formula.sh" "$version" "$checksum" "$configured_checksum" "$macos_checksum" "$macos_configured_checksum"
cp "$source_dir/README.md" "$tap/README.md"
cp "$source_dir/.github/workflows/tests.yml" "$tap/.github/workflows/tests.yml"
cp "$source_dir/.github/workflows/sync.yml" "$tap/.github/workflows/sync.yml"
