#!/bin/sh
set -eu

# stack-agent is the configured distribution's only agent. This test proves the packaging:
#
#   * the configured formula has the stack-agent launcher and the agent entry, and no npm agent
#     launcher; base Compute grows neither;
#   * the launcher runs only the stack-agent executable, fails loudly when it is absent, and
#     starts it from an allow-listed environment: the model key and Compute's own settings
#     reach it, anything else the shell holds does not;
#   * the agent entry maps `stack-agent` (the default) to that launcher and refuses every other
#     name, `chip` included;
#   * an assembled configured asset carries the executable and nothing of the npm agent.

repository=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/compute-stack-agent-test.XXXXXX")
cleanup() { rm -rf "$work"; }
trap cleanup EXIT HUP INT TERM

checksum=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
"$repository/distribution/render-homebrew-tap.sh" "$work/tap" 1.2.3 \
  "$checksum" "$checksum" "$checksum" "$checksum"
configured="$work/tap/Formula/compute-configured.rb"
base="$work/tap/Formula/compute.rb"

grep -q 'bin/"compute-configured-stack-agent"' "$configured"
grep -q 'bin/"compute-configured-agent"' "$configured"
if grep -q 'bin/"compute-configured-chip"' "$configured"; then
  echo "the configured formula must not ship the npm Chip launcher" >&2
  exit 1
fi
if grep -qE 'stack-agent|compute-configured-agent' "$base"; then
  echo "base Compute must not ship an agent launcher" >&2
  exit 1
fi

# Pull each launcher's script body out of the formula, with Ruby's heredoc escapes undone.
launcher() {
  awk -v name="bin/\"$1\"" '
    index($0, name) { capture = 1; next }
    capture && /^[[:space:]]*SH$/ { exit }
    capture { sub(/^      /, ""); gsub(/\\\\/, "\\"); print }
  ' "$configured"
}
agent_launcher_body=$(launcher compute-configured-stack-agent)
[ -n "$agent_launcher_body" ]
printf '%s\n' "$agent_launcher_body" | grep -q 'compute-stack-agent'
for forbidden in 'node_modules' 'runtimes/node' 'compute-configured-chip' '@appport' '\.output'; do
  if printf '%s\n' "$agent_launcher_body" | grep -q "$forbidden"; then
    echo "the stack-agent launcher must not touch an npm agent ($forbidden)" >&2
    exit 1
  fi
done

# Behaviour: run the launcher body against a fake install.
libexec="$work/libexec"
mkdir -p "$libexec"
printf '%s\n' "$agent_launcher_body" | sed "s|#{libexec}|$libexec|g" > "$work/launch"
chmod +x "$work/launch"
# A build without the executable fails loudly and non-zero.
if "$work/launch" serve 2>"$work/err"; then
  echo "the launcher must fail when stack-agent is not installed" >&2
  exit 1
fi
grep -q 'not part of this distribution build' "$work/err"
# With the executable present, the launcher runs it (and only it) with the arguments given, from
# an allow-listed environment.
mkdir -p "$libexec/stack-agent/bin"
cat > "$libexec/stack-agent/bin/compute-stack-agent" <<'STUB'
#!/bin/sh
echo "stack-agent $* worker=$COMPUTE_STACK_AGENT_WORKER key=${CHIP_API_KEY:-} target=${COMPUTE_STACK_AGENT_TARGET:-} other=${UNRELATED_CLOUD_KEY:-absent}"
STUB
chmod +x "$libexec/stack-agent/bin/compute-stack-agent"
out=$(CHIP_API_KEY=sk-model COMPUTE_STACK_AGENT_TARGET=http://t UNRELATED_CLOUD_KEY=sk-other \
  "$work/launch" serve --port 0)
expected="stack-agent serve --port 0 worker=$libexec/stack-agent/bin/compute-stack-agent key=sk-model target=http://t other=absent"
[ "$out" = "$expected" ] || {
  echo "unexpected launch: $out" >&2
  exit 1
}

# The agent entry maps a name to a launcher and nothing else. `stack-agent` is the default and
# the only name; an unknown name (the retired `chip` included) fails loudly.
agent_entry=$(launcher compute-configured-agent)
[ -n "$agent_entry" ]
bindir="$work/bin"
mkdir -p "$bindir"
printf '%s\n' "$agent_entry" | sed "s|#{bin}|$bindir|g" > "$bindir/compute-configured-agent"
cat > "$bindir/compute-configured-stack-agent" <<'STUB'
#!/bin/sh
echo "stack-agent-launcher $*"
STUB
chmod +x "$bindir"/*
[ "$("$bindir/compute-configured-agent" serve --port 0)" = "stack-agent-launcher serve --port 0" ]
[ "$("$bindir/compute-configured-agent" --agent stack-agent serve)" = "stack-agent-launcher serve" ]
[ "$("$bindir/compute-configured-agent" --agent=stack-agent serve)" = "stack-agent-launcher serve" ]
for unknown in chip claude codex chip-eve; do
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

# An assembled configured asset (COMPUTE_TEST_CONFIGURED_ROOT) must carry the stack-agent
# executable where the launcher looks for it, and nothing of the npm agent.
if [ -n "${COMPUTE_TEST_CONFIGURED_ROOT:-}" ]; then
  root=$COMPUTE_TEST_CONFIGURED_ROOT
  test -x "$root/stack-agent/bin/compute-stack-agent" || {
    echo "the configured asset at $root has no stack-agent/bin/compute-stack-agent" >&2
    exit 1
  }
  test -d "$root/node_modules" && test -f "$root/stack.json"
  for retired in node_modules/@appport/chip node_modules/@appport/fx agent .output; do
    if [ -e "$root/$retired" ]; then
      echo "the configured asset still contains the npm agent ($retired)" >&2
      exit 1
    fi
  done
  set +e
  "$root/stack-agent/bin/compute-stack-agent" 2>"$work/usage"
  status=$?
  set -e
  [ "$status" -eq 2 ] && grep -q 'usage: compute-stack-agent' "$work/usage"
  echo "ok: the assembled configured asset contains stack-agent and no npm agent"
fi

echo "ok: stack-agent is the configured agent"
