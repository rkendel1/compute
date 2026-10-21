#!/usr/bin/env bash
# A write whose outcome is unknown is never retried, and the live MCP
# transport classifies what it cannot know (stack-agent at the locked commit).
# outcome_unknown runs its tests one at a time: they hold FeltDB leases of tens
# of milliseconds, which two concurrent tests on a loaded runner can outlive.
source "$STACK_HARNESS/lib.sh"
repo="$(src stack-agent)"
set +e
cargo test --target-dir $(cargo_target stack-agent) --locked --manifest-path "$repo/Cargo.toml" -p chip-agent-state --test outcome_unknown -- --test-threads 1 2>&1 | tee "$STACK_OUT/cargo.txt"
a=${PIPESTATUS[0]}
cargo test --target-dir $(cargo_target stack-agent) --locked --manifest-path "$repo/Cargo.toml" -p chip-appport-source --test live_transport 2>&1 | tee -a "$STACK_OUT/cargo.txt"
b=${PIPESTATUS[0]}
set -e
read -r passed failed skipped < <(cargo_counts "$STACK_OUT/cargo.txt")
status=passed; { [ "$a" = 0 ] && [ "$b" = 0 ] && [ "$passed" -gt 0 ]; } || status=failed
result "$status" "$passed" "$failed" "$skipped" "chip-agent-state outcome_unknown, chip-appport-source live_transport" log="$STACK_OUT/cargo.txt"
