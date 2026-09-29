#!/bin/sh
# Direct Compute installer. Homebrew is the primary product installation path.
set -eu

fail() {
  printf 'compute installer: %s\n' "$*" >&2
  exit 1
}

command -v curl >/dev/null 2>&1 || fail "curl is required"
command -v tar >/dev/null 2>&1 || fail "tar is required"

os=$(uname -s)
arch=$(uname -m)
[ "$os" = Linux ] && [ "$arch" = x86_64 ] ||
  fail "unsupported platform $os/$arch; Compute currently supports Linux x86_64"

repository=${COMPUTE_GITHUB_REPOSITORY:-rkendel1/compute}
version=${COMPUTE_VERSION:-}
if [ -z "$version" ]; then
  latest_url="https://github.com/$repository/releases/latest"
  resolved=$(curl -fsSL --proto '=https' --proto-redir '=https' --tlsv1.2 \
    -o /dev/null -w '%{url_effective}' "$latest_url") ||
    fail "could not resolve the latest stable Compute release"
  tag=${resolved##*/}
  case "$tag" in
    v*) version=${tag#v} ;;
    *) fail "latest release did not resolve to a v-prefixed tag" ;;
  esac
else
  version=${version#v}
fi
case "$version" in
  ''|*[!0-9A-Za-z._-]*) fail "invalid Compute version: $version" ;;
esac

asset="compute-$version-linux-x86_64.tar.gz"
release_base=${COMPUTE_RELEASE_BASE_URL:-https://github.com/$repository/releases/download/v$version}

: "${HOME:?HOME must be set}"
data_home=${XDG_DATA_HOME:-$HOME/.local/share}
install_root=${COMPUTE_INSTALL_ROOT:-$data_home/compute}
bin_dir=${COMPUTE_BIN_DIR:-$HOME/.local/bin}
case "$install_root" in /*) ;; *) fail "installation root must be an absolute path" ;; esac
case "$bin_dir" in /*) ;; *) fail "binary directory must be an absolute path" ;; esac

temporary=$(mktemp -d "${TMPDIR:-/tmp}/compute-install.XXXXXX") ||
  fail "could not create a temporary directory"
staged=
published_binary=
binary=
cleanup() {
  rm -rf "$temporary"
  [ -z "$staged" ] || rm -rf "$staged"
  [ -z "$published_binary" ] || rm -f "$binary"
}
trap cleanup EXIT HUP INT TERM

archive="$temporary/$asset"
checksum="$archive.sha256"
curl -fL --proto '=https' --proto-redir '=https' --tlsv1.2 \
  -o "$archive" "$release_base/$asset" ||
  fail "could not download $asset"
curl -fL --proto '=https' --proto-redir '=https' --tlsv1.2 \
  -o "$checksum" "$release_base/$asset.sha256" ||
  fail "could not download $asset.sha256"

expected=$(sed -n '1{s/[[:space:]].*//;p;}' "$checksum")
case "$expected" in *[!0-9A-Fa-f]*|'') fail "invalid SHA-256 checksum" ;; esac
[ "${#expected}" -eq 64 ] || fail "invalid SHA-256 checksum"
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$archive" | sed 's/[[:space:]].*//')
elif command -v shasum >/dev/null 2>&1; then
  actual=$(shasum -a 256 "$archive" | sed 's/[[:space:]].*//')
else
  fail "sha256sum or shasum is required"
fi
[ "$actual" = "$expected" ] || fail "checksum verification failed"

# Reject archive paths that could escape extraction, and require the builder's
# one canonical top-level directory.
tar -tzf "$archive" | awk '
  /^\// { exit 1 }
  $0 != "compute-distribution" && index($0, "compute-distribution/") != 1 { exit 1 }
  {
    count = split($0, parts, "/")
    for (i = 1; i <= count; i++) if (parts[i] == "..") exit 1
  }
' >/dev/null || fail "archive has an unsafe or unexpected layout"

mkdir "$temporary/extracted"
tar -xzf "$archive" -C "$temporary/extracted" || fail "could not extract distribution"
distribution="$temporary/extracted/compute-distribution"
[ -x "$distribution/bin/compute" ] || fail "distribution has no executable compute binary"
[ -d "$distribution/runtimes" ] || fail "distribution has no runtime bundle"
[ -f "$distribution/runtime-manifest.json" ] || fail "distribution has no runtime manifest"
[ -f "$distribution/runtime-inventory.json" ] || fail "distribution has no runtime inventory"
[ -f "$distribution/runtime-lock.json" ] || fail "distribution has no runtime lock"

COMPUTE_DISTRIBUTION_ROOT="$distribution" "$distribution/bin/compute" distribution verify "$distribution" >/dev/null ||
  fail "distribution validation failed"
reported=$("$distribution/bin/compute" --version) || fail "installed binary cannot report its version"
case " $reported " in
  *" $version "*) ;;
  *) fail "binary version does not match release $version: $reported" ;;
esac

installations="$install_root/installations"
target="$installations/$version-$expected"
mkdir -p "$installations" "$bin_dir"
if [ -e "$target" ] || [ -L "$target" ]; then
  target="$target-$$"
fi

binary="$bin_dir/compute"
binary_target="$install_root/current/bin/compute"
if [ -L "$binary" ]; then
  [ "$(readlink "$binary")" = "$binary_target" ] ||
    fail "$binary is not managed by the Compute installer"
elif [ -e "$binary" ]; then
  fail "$binary already exists and is not managed by the Compute installer"
fi
[ ! -e "$install_root/current" ] || [ -L "$install_root/current" ] ||
  fail "$install_root/current exists and is not managed by the Compute installer"

mv "$distribution" "$target" || fail "could not stage the installation"
staged=$target

# GNU mv provides the atomic, no-dereference replacement available on the
# supported Linux platform. The fallback keeps the bootstrap usable with a
# POSIX shell on development hosts whose mv lacks -T.
replace_link() {
  link_target=$1
  destination=$2
  link_tmp="$(dirname "$destination")/.$(basename "$destination").$$"
  rm -f "$link_tmp"
  ln -s "$link_target" "$link_tmp" || return 1
  if mv -Tf "$link_tmp" "$destination" 2>/dev/null; then
    return 0
  fi
  rm -f "$link_tmp"
  ln -sfn "$link_target" "$destination"
}

# `compute` is a stable link through `current`; upgrades therefore have one
# visible activation point. Never replace an unrelated file in the user's bin.
if [ ! -L "$binary" ]; then
  ln -s "$binary_target" "$binary" || fail "could not publish compute on PATH"
  published_binary=1
fi

# Versioned installation trees are immutable, so the one atomic replacement
# cannot damage the previous distribution.
replace_link "installations/$(basename "$target")" "$install_root/current" ||
  fail "could not activate installation"
staged=
published_binary=

printf 'Installed Compute %s in %s\n' "$version" "$target"
case ":${PATH:-}:" in
  *":$bin_dir:"*) printf 'Run: compute\n' ;;
  *) printf 'Add %s to PATH, then run: compute\n' "$bin_dir" ;;
esac
