#!/bin/sh
set -eu

if [ "$#" -ne 4 ]; then
  echo "usage: assemble.sh COMPUTE_BINARY RUNTIME_PAYLOAD_ROOT PLATFORM OUTPUT_DIR" >&2
  exit 2
fi

compute_binary=$1
payload_root=$2
platform=$3
output_dir=$4
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
lock_file="$script_dir/runtime-lock.json"

command -v jq >/dev/null 2>&1 || { echo "jq is required" >&2; exit 1; }
test -x "$compute_binary" || { echo "Compute binary is not executable: $compute_binary" >&2; exit 1; }
test ! -e "$output_dir" || { echo "Refusing to overwrite: $output_dir" >&2; exit 1; }
test -f "$payload_root/certification/fixtures.json" || { echo "Missing certification fixtures" >&2; exit 1; }

jq -r '.runtimes | to_entries[] | .value.entrypoint, (.value.files[]? // empty)' "$payload_root/certification/fixtures.json" |
while IFS= read -r fixture; do
  test -f "$payload_root/certification/$fixture" || { echo "Missing certification file: $fixture" >&2; exit 1; }
done

mkdir -p "$output_dir/bin" "$output_dir/runtimes"
cp "$compute_binary" "$output_dir/bin/compute"
chmod 0755 "$output_dir/bin/compute"
cp "$lock_file" "$output_dir/runtime-lock.json"
cp -R "$payload_root/certification" "$output_dir/certification"

jq -r '.runtimes | to_entries[] | [.key, .value.version, .value.executable] | @tsv' "$lock_file" |
while IFS="$(printf '\t')" read -r runtime version executable; do
  case "$executable" in
    '<embedded>'|'<workload-entrypoint>') continue ;;
  esac
  source_dir="$payload_root/runtimes/$runtime"
  test -d "$source_dir" || { echo "Missing runtime payload: $source_dir" >&2; exit 1; }
  cp -R "$source_dir" "$output_dir/runtimes/$runtime"
  installed="$output_dir/$executable"
  test -x "$installed" || { echo "Runtime executable is missing: $installed" >&2; exit 1; }
  detected=$($installed --version 2>&1) || { echo "Runtime probe failed: $runtime" >&2; exit 1; }
  case "$detected" in
    *"$version"*) ;;
    *) echo "Runtime version mismatch for $runtime: expected $version, detected $detected" >&2; exit 1 ;;
  esac
done

compute_version=$($compute_binary version --json | jq -r '.version')
jq \
  --arg compute_version "$compute_version" \
  --arg platform "$platform" \
  '{compute_version: $compute_version, distribution_version: ("compute-" + $compute_version + "-" + $platform), platform: $platform, runtimes: .runtimes}' \
  "$lock_file" > "$output_dir/runtime-manifest.json"

find "$output_dir" -exec touch -h -t 197001010000.00 {} +
archive="$output_dir.tar"
LC_ALL=C tar --sort=name --mtime='UTC 1970-01-01' --owner=0 --group=0 --numeric-owner -cf "$archive" -C "$(dirname "$output_dir")" "$(basename "$output_dir")"
echo "$archive"
