#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
  echo "usage: prepare-certification-fixtures.sh RUNTIME_PAYLOAD_ROOT" >&2
  exit 2
fi

payload_root=$1
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repository_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
source_root="$script_dir/certification-src"
destination="$payload_root/certification"
test ! -e "$destination" || { echo "Refusing to overwrite: $destination" >&2; exit 1; }
command -v jq >/dev/null 2>&1 || { echo "jq is required" >&2; exit 1; }
command -v javac >/dev/null 2>&1 || { echo "javac is required to build the JVM fixture" >&2; exit 1; }
command -v jar >/dev/null 2>&1 || { echo "jar is required to build the JVM fixture" >&2; exit 1; }
command -v dotnet >/dev/null 2>&1 || { echo "dotnet SDK is required to build the .NET fixture" >&2; exit 1; }
command -v rustc >/dev/null 2>&1 || { echo "rustc is required to build native and WASM fixtures" >&2; exit 1; }
test -f "$repository_root/packages/compute-appport/dist/certify.js" || {
  echo "Build packages/compute-appport before preparing certification fixtures" >&2
  exit 1
}
test -d "$repository_root/packages/compute-appport/node_modules" || {
  echo "Install AppPort production dependencies before preparing certification fixtures" >&2
  exit 1
}

temporary=$(mktemp -d "${TMPDIR:-/tmp}/compute-certification.XXXXXX")
trap 'rm -rf "$temporary"' EXIT HUP INT TERM
mkdir -p "$temporary/certification"
cp "$source_root/fixtures.json" "$temporary/certification/fixtures.json"
for runtime in python node bun deno ruby php shell; do
  mkdir -p "$temporary/certification/$runtime"
  cp "$source_root/$runtime"/* "$temporary/certification/$runtime/"
done

mkdir -p "$temporary/certification/jvm/classes"
javac -encoding UTF-8 -d "$temporary/certification/jvm/classes" "$source_root/jvm/Certification.java"
jar --create --file "$temporary/certification/jvm/certify.jar" --main-class Certification -C "$temporary/certification/jvm/classes" Certification.class
rm -rf "$temporary/certification/jvm/classes"

mkdir -p "$temporary/dotnet-home" "$temporary/nuget" "$temporary/certification/dotnet"
DOTNET_CLI_HOME="$temporary/dotnet-home" \
NUGET_PACKAGES="$temporary/nuget" \
DOTNET_SKIP_FIRST_TIME_EXPERIENCE=1 \
dotnet build "$source_root/dotnet/certify.csproj" --nologo --verbosity quiet --output "$temporary/certification/dotnet"

native_version=$(jq -r '.runtimes.native.version' "$script_dir/runtime-lock.json")
wasm_version=$(jq -r '.runtimes.wasm.version' "$script_dir/runtime-lock.json")
mkdir -p "$temporary/certification/native" "$temporary/certification/wasm"
CERTIFICATION_RUNTIME=native CERTIFICATION_VERSION="$native_version" \
rustc -C opt-level=s -C target-feature=+crt-static "$source_root/compiled/certify.rs" -o "$temporary/certification/native/certify-native"
CERTIFICATION_RUNTIME=wasm CERTIFICATION_VERSION="$wasm_version" \
rustc --target wasm32-wasip1 -C opt-level=s "$source_root/compiled/certify.rs" -o "$temporary/certification/wasm/certify.wasm"

mkdir -p "$temporary/certification/appport/dist"
cp "$repository_root/packages/compute-appport/package.json" "$temporary/certification/appport/package.json"
cp -R "$repository_root/packages/compute-appport/dist/." "$temporary/certification/appport/dist/"
cp -R "$repository_root/packages/compute-appport/node_modules" "$temporary/certification/appport/node_modules"

mkdir -p "$payload_root"
mv "$temporary/certification" "$destination"
echo "$destination"
