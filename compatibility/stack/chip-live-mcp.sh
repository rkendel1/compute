#!/usr/bin/env bash
# Chip's work loop (scripted model, durable FeltDB state) against the live
# endpoint for the same generated bridge: stage 3 of the L1 experiment
# (stack-agent stage3.py at the locked commit), with Chip built from the
# locked stack-agent.
source "$STACK_HARNESS/lib.sh"
prepare_desk; prepare_l1_harness
agent="$(src stack-agent)"; target=$(cargo_target stack-agent)
{ cargo build --target-dir "$target" --locked --release --manifest-path "$agent/Cargo.toml" -p chip-appport-source --example mcp_live \
  && cargo build --target-dir "$target" --locked --release --manifest-path "$agent/Cargo.toml" -p stack-agent --bin stack-agent; } >"$STACK_OUT/build.log" 2>&1 \
  || { tail -40 "$STACK_OUT/build.log"; result failed 0 1 0 "stack-agent did not build"; exit 1; }
sha256sum "$target/release/examples/mcp_live" "$target/release/stack-agent" >"$STACK_OUT/binaries.sha256"
set +e
python3 "$STACK_WORK/l1-harness/stage3.py" "$STACK_WORK/desk" "$target/release/examples/mcp_live" "$target/release/stack-agent" "$STACK_OUT/stage3" | tee "$STACK_OUT/stage3.txt"
code=${PIPESTATUS[0]}
set -e
passed=$(grep -c '^PASS' "$STACK_OUT/stage3.txt" || true); failed=$(grep -c '^FAIL' "$STACK_OUT/stage3.txt" || true)
status=passed; { [ "$code" = 0 ] && [ "$failed" = 0 ] && [ "$passed" -gt 0 ]; } || status=failed
result "$status" "$passed" "$failed" 0 "stage 3 checks: reads, an authorized write, forbidden calls, restart without repeating writes, outcome evidence" results="$STACK_OUT/stage3/results.json" binaries="$STACK_OUT/binaries.sha256"
