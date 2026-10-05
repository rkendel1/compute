#!/bin/sh
# Installs the release tag's Homebrew formula templates and guards into the tap.
#
# v0.1.16 fetched these files from the release tag into `distribution/homebrew/`
# and then rendered from the tap's own older copies, because the renderer reads
# `Formula/*.rb.in` and `scripts/*.sh`. The correct template was fetched, never
# used, and the configured formula silently lost the Chip launcher.
#
# This puts the release tag's copies exactly where the renderer and the guards
# read them, so there is one set of templates and it is the release's.
#
# The set is derived from the directory, not listed here: every `*.rb.in` under
# `Formula/` and every `*.sh` under `scripts/` is installed. A template added to
# the release therefore cannot be left behind in the tap, and none is
# special-cased here.
set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: $0 TEMPLATE_DIRECTORY TAP_ROOT" >&2
  exit 2
fi

templates=$1
tap=$2

fail() {
  echo "release template install failed: $1" >&2
  exit 1
}

[ -d "$templates" ] || fail "no template directory at $templates"

# `find` derives the set. Paths are sorted so the install is deterministic, and
# printed so the workflow log records exactly what the tap received.
install_list() {
  find "$templates/Formula" -type f -name '*.rb.in' 2>/dev/null | LC_ALL=C sort
  find "$templates/scripts" -type f -name '*.sh' 2>/dev/null | LC_ALL=C sort
}

relative() {
  # Strip the template directory prefix, leaving Formula/... or scripts/...
  printf '%s\n' "${1#"$templates"/}"
}

list=$(install_list)
[ -n "$list" ] || fail "$templates contains no Formula/*.rb.in and scripts/*.sh to install"

# Every file is checked before any file is copied. A partial install is worse
# than none: it would leave the tap mixing templates from two releases.
for path in $list; do
  [ -s "$path" ] || fail "$path is missing or empty in the release"
done

mkdir -p "$tap/Formula" "$tap/scripts"

for path in $list; do
  target="$tap/$(relative "$path")"
  mkdir -p "$(dirname "$target")"
  cp "$path" "$target" || fail "could not install $target"
  printf 'installed %s\n' "$(relative "$path")"
done

chmod +x "$tap"/scripts/*.sh

# The renderer is only useful if its two inputs are present; refuse to leave a
# tap that would render from nothing.
for required in Formula/compute.rb.in Formula/compute-configured.rb.in scripts/update-formula.sh; do
  [ -s "$tap/$required" ] || fail "the renderer's input $required was not installed"
done