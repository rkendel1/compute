#!/bin/bash
set -euo pipefail

# Update the published-stack version to match the current Compute version.
# This ensures the compatibility test doesn't fail due to version drift.
#
# Usage: update-stack-version.sh [version]
#
# If no version is provided, extracts it from Cargo.toml.

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
stack_file="$here/compatibility/published-stack/stack.json"

if [[ $# -eq 0 ]]; then
  version=$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[] | select(.name == "compute-cli") | .version')
else
  version="$1"
fi

# Update version references in the stack.json
jq --arg version "$version" '
  .compute = $version |
  .distribution.identity = "compute-configured-\($version)-linux-x86_64" |
  .distribution.base_asset = "compute-\($version)-linux-x86_64.tar.gz" |
  .distribution.configured_asset = "compute-configured-\($version)-linux-x86_64.tar.gz" |
  .distribution.release = "https://github.com/rkendel1/compute/releases/tag/v\($version)"
' "$stack_file" > "$stack_file.tmp"

mv "$stack_file.tmp" "$stack_file"
echo "Updated published-stack to version $version"
