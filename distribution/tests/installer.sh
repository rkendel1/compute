#!/bin/sh
set -eu

repository=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/compute-installer-test.XXXXXX")
cleanup() { rm -rf "$work"; }
trap cleanup EXIT HUP INT TERM

fake_bin="$work/fake-bin"
releases="$work/releases"
home="$work/home"
mkdir -p "$fake_bin" "$releases" "$home"

cat > "$fake_bin/uname" <<'EOF'
#!/bin/sh
case "$1" in
  -s) printf 'Linux\n' ;;
  -m) printf 'x86_64\n' ;;
  *) exit 1 ;;
esac
EOF

cat > "$fake_bin/curl" <<'EOF'
#!/bin/sh
output=
url=
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o) output=$2; shift 2 ;;
    --proto|--proto-redir|-w) shift 2 ;;
    -f|-L|-fL|-fsSL|--tlsv1.2) shift ;;
    *) url=$1; shift ;;
  esac
done
[ -n "$output" ] && [ -n "$url" ]
cp "$FAKE_RELEASE_DIR/${url##*/}" "$output"
EOF
chmod +x "$fake_bin/uname" "$fake_bin/curl"

digest() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | sed 's/[[:space:]].*//'
  else
    shasum -a 256 "$1" | sed 's/[[:space:]].*//'
  fi
}

make_release() {
  version=$1
  stage="$work/stage-$version/compute-distribution"
  mkdir -p "$stage/bin" "$stage/runtimes/fake"
  cat > "$stage/bin/compute" <<EOF
#!/bin/sh
case "\${1:-}" in
  --version) printf 'compute $version\\n' ;;
  distribution)
    [ "\${2:-}" = verify ] && [ -f "\${3:-}/runtime-manifest.json" ]
    ;;
  *) printf 'compute $version started\\n' ;;
esac
EOF
  chmod +x "$stage/bin/compute"
  printf '{"compute_version":"%s"}\n' "$version" > "$stage/runtime-manifest.json"
  printf '{}\n' > "$stage/runtime-inventory.json"
  printf '{}\n' > "$stage/runtime-lock.json"
  asset="compute-$version-linux-x86_64.tar.gz"
  tar -czf "$releases/$asset" -C "$(dirname "$stage")" compute-distribution
  printf '%s  %s\n' "$(digest "$releases/$asset")" "$asset" > "$releases/$asset.sha256"
}

install() {
  version=$1
  PATH="$fake_bin:$PATH" \
  HOME="$home" \
  COMPUTE_VERSION="$version" \
  COMPUTE_RELEASE_BASE_URL="https://releases.invalid/v$version" \
  COMPUTE_INSTALL_ROOT="$home/install" \
  COMPUTE_BIN_DIR="$home/bin" \
  FAKE_RELEASE_DIR="$releases" \
    sh "$repository/install.sh" >/dev/null
}

make_release 1.0.0
mkdir -p "$home/.compute"
printf 'durable state\n' > "$home/.compute/evidence"
install 1.0.0

(cd /tmp && "$home/bin/compute" --version) | grep -q '1.0.0'
(cd /tmp && "$home/bin/compute") | grep -q '1.0.0 started'
test "$(cat "$home/.compute/evidence")" = "durable state"

make_release 1.1.0
install 1.1.0
"$home/bin/compute" --version | grep -q '1.1.0'
test "$(cat "$home/.compute/evidence")" = "durable state"

printf 'tampered\n' >> "$releases/compute-1.0.0-linux-x86_64.tar.gz"
if install 1.0.0 2>/dev/null; then
  echo "tampered archive was installed" >&2
  exit 1
fi
"$home/bin/compute" --version | grep -q '1.1.0'
test "$(cat "$home/.compute/evidence")" = "durable state"

printf 'not an archive\n' > "$releases/compute-1.2.0-linux-x86_64.tar.gz"
printf '%s  %s\n' \
  "$(digest "$releases/compute-1.2.0-linux-x86_64.tar.gz")" \
  compute-1.2.0-linux-x86_64.tar.gz \
  > "$releases/compute-1.2.0-linux-x86_64.tar.gz.sha256"
if install 1.2.0 2>/dev/null; then
  echo "invalid archive was installed" >&2
  exit 1
fi
"$home/bin/compute" --version | grep -q '1.1.0'
test "$(cat "$home/.compute/evidence")" = "durable state"

printf 'installer contract passed\n'
