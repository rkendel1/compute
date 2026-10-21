#!/usr/bin/env bash
# Chip reads PAX's versioned results: the PAX the stack locks, built from its
# locked commit, under stack-agent's own chip-pax tests. Target directories are
# passed as flags, never as CARGO_TARGET_DIR: PAX runs cargo in the projects
# the tests create, and those must build into their own target/.
source "$STACK_HARNESS/lib.sh"
pax="$(src pax)"; agent="$(src stack-agent)"
cargo build --target-dir $(cargo_target pax) --locked --release --manifest-path "$pax/Cargo.toml" >"$STACK_OUT/pax-build.log" 2>&1 \
  || { tail -40 "$STACK_OUT/pax-build.log"; result failed 0 1 0 "pax did not build"; exit 1; }
export PATH="$(cargo_target pax)/release:$PATH"
pax --version | tee "$STACK_OUT/pax-version.txt" || true
set +e
cargo test --target-dir $(cargo_target stack-agent) --locked --manifest-path "$agent/Cargo.toml" -p chip-pax 2>&1 | tee "$STACK_OUT/cargo.txt"
code=${PIPESTATUS[0]}
set -e
read -r passed failed skipped < <(cargo_counts "$STACK_OUT/cargo.txt")
status=passed; { [ "$code" = 0 ] && [ "$passed" -gt 0 ]; } || status=failed
result "$status" "$passed" "$failed" "$skipped" "chip-pax against pax $(lock_commit pax | cut -c1-7)" log="$STACK_OUT/cargo.txt"
