#!/bin/sh
set -eu

# Rust Chip (chip-rs) and the npm Chip/Eve agent are two different products in the configured
# distribution. This test proves the packaging keeps them apart:
#
#   * each has its own launcher, and the launchers do not refer to each other;
#   * the npm launcher still runs the npm Chip runtime and nothing of Rust Chip;
#   * the Rust Chip launcher runs only the Rust Chip executable, fails loudly when that
#     executable is absent, and never falls back to the npm agent or the npm FX;
#   * base Compute grows neither launcher.

repository=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/compute-rust-chip-test.XXXXXX")
cleanup() { rm -rf "$work"; }
trap cleanup EXIT HUP INT TERM

checksum=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
"$repository/distribution/render-homebrew-tap.sh" "$work/tap" 1.2.3 \
  "$checksum" "$checksum" "$checksum" "$checksum"
configured="$work/tap/Formula/compute-configured.rb"
base="$work/tap/Formula/compute.rb"

grep -q 'bin/"compute-configured-chip"' "$configured"
grep -q 'bin/"compute-configured-rust-chip"' "$configured"
if grep -q 'rust-chip' "$base"; then
  echo "base Compute must not ship a Rust Chip launcher" >&2
  exit 1
fi

# Pull each launcher's script body out of the formula.
launcher() {
  awk -v name="bin/\"$1\"" '
    index($0, name) { capture = 1; next }
    capture && /^[[:space:]]*SH$/ { exit }
    capture { print }
  ' "$configured"
}
npm_launcher=$(launcher compute-configured-chip)
rust_launcher=$(launcher compute-configured-rust-chip)
[ -n "$npm_launcher" ] && [ -n "$rust_launcher" ]

# The npm launcher is the existing one: it runs the npm Chip, and mentions nothing of Rust Chip.
printf '%s\n' "$npm_launcher" | grep -q 'node_modules/.bin/chip'
if printf '%s\n' "$npm_launcher" | grep -qi 'rust'; then
  echo "the npm Chip launcher must not refer to Rust Chip" >&2
  exit 1
fi

# The Rust Chip launcher runs the Rust executable, and never the npm agent or the npm FX.
printf '%s\n' "$rust_launcher" | grep -q 'compute-rust-chip'
for forbidden in 'node_modules' 'runtimes/node' 'compute-configured-chip' '@appport' '\.output'; do
  if printf '%s\n' "$rust_launcher" | grep -q "$forbidden"; then
    echo "the Rust Chip launcher must not touch the npm agent ($forbidden)" >&2
    exit 1
  fi
done

# Behaviour: run the Rust launcher body against a fake install.
libexec="$work/libexec"
mkdir -p "$libexec"
body=$(printf '%s\n' "$rust_launcher" | sed "s|#{libexec}|$libexec|g")
printf '%s\n' "$body" > "$work/launch"
chmod +x "$work/launch"
# A build without the executable fails loudly and non-zero.
if "$work/launch" serve 2>"$work/err"; then
  echo "the launcher must fail when Rust Chip is not installed" >&2
  exit 1
fi
grep -q 'not part of this distribution build' "$work/err"
# With the executable present, the launcher runs it (and only it) with the arguments given.
mkdir -p "$libexec/rust-chip/bin"
cat > "$libexec/rust-chip/bin/compute-rust-chip" <<STUB
#!/bin/sh
echo "rust-chip \$* worker=\$COMPUTE_RUST_CHIP_WORKER"
STUB
chmod +x "$libexec/rust-chip/bin/compute-rust-chip"
out=$("$work/launch" serve --port 0)
[ "$out" = "rust-chip serve --port 0 worker=$libexec/rust-chip/bin/compute-rust-chip" ]

# The agent entry maps a name to a launcher and nothing else. `chip` (the default) is Rust Chip;
# the npm Chip/Eve launcher is not reachable from it, and an unknown name fails loudly.
grep -q 'bin/"compute-configured-agent"' "$configured"
if grep -q 'compute-configured-agent' "$base"; then
  echo "base Compute must not ship the agent entry" >&2
  exit 1
fi
agent_launcher=$(launcher compute-configured-agent)
[ -n "$agent_launcher" ]
for forbidden in 'node_modules' 'runtimes/node' 'compute-configured-chip' '@appport' 'chip-eve'; do
  if printf '%s\n' "$agent_launcher" | grep -q "$forbidden"; then
    echo "the agent entry must not touch the npm agent ($forbidden)" >&2
    exit 1
  fi
done
bindir="$work/bin"
mkdir -p "$bindir"
printf '%s\n' "$agent_launcher" | sed "s|#{bin}|$bindir|g" > "$bindir/compute-configured-agent"
cat > "$bindir/compute-configured-rust-chip" <<STUB
#!/bin/sh
echo "rust-chip-launcher \$*"
STUB
cat > "$bindir/compute-configured-chip" <<STUB
#!/bin/sh
echo "NPM CHIP MUST NOT RUN" >&2
exit 99
STUB
chmod +x "$bindir"/*
[ "$("$bindir/compute-configured-agent" serve --port 0)" = "rust-chip-launcher serve --port 0" ]
[ "$("$bindir/compute-configured-agent" --agent chip serve)" = "rust-chip-launcher serve" ]
[ "$("$bindir/compute-configured-agent" --agent=chip serve)" = "rust-chip-launcher serve" ]
for unknown in claude codex chip-eve; do
  if "$bindir/compute-configured-agent" --agent "$unknown" serve 2>"$work/err"; then
    echo "an unknown agent ($unknown) must not launch" >&2
    exit 1
  fi
  grep -q "unknown agent '$unknown'" "$work/err"
done
if "$bindir/compute-configured-agent" --agent 2>"$work/err"; then
  echo "--agent without a name must fail" >&2
  exit 1
fi

# An assembled configured asset (COMPUTE_TEST_CONFIGURED_ROOT) must carry the Rust Chip executable
# where the launcher looks for it, apart from the npm agent's files.
if [ -n "${COMPUTE_TEST_CONFIGURED_ROOT:-}" ]; then
  root=$COMPUTE_TEST_CONFIGURED_ROOT
  test -x "$root/rust-chip/bin/compute-rust-chip" || {
    echo "the configured asset at $root has no rust-chip/bin/compute-rust-chip" >&2
    exit 1
  }
  test -d "$root/node_modules" && test -f "$root/stack.json"
  # The executable is a separate runtime: it answers its own usage, and starts nothing of npm.
  set +e
  "$root/rust-chip/bin/compute-rust-chip" 2>"$work/usage"
  status=$?
  set -e
  [ "$status" -eq 2 ] && grep -q 'usage: compute-rust-chip' "$work/usage"
  echo "ok: the assembled configured asset contains the Rust Chip agent"
fi

echo "ok: Rust Chip and the npm Chip/Eve agent are launched distinctly"
