#!/usr/bin/env bash
# AuthBoundry's own tests for delegated agent authority at the locked commit:
# a delegation dies at the next decision after its delegator loses authority,
# through the real HTTP decision route (crates/server/tests/agent_credentials.rs).
source "$STACK_HARNESS/lib.sh"
repo="$(src authboundry)"
# The decision route stores through AuthBoundry's FeltDB bridge, which runs
# the @feltdb/core its own lockfile pins.
(cd "$repo" && npm ci --no-audit --no-fund >"$STACK_OUT/npm-ci.log" 2>&1) || { tail -30 "$STACK_OUT/npm-ci.log"; result failed 0 1 0 "npm ci failed in authboundry"; exit 1; }
set +e
cargo test --target-dir $(cargo_target authboundry) --locked --manifest-path "$repo/Cargo.toml" -p appport-auth-mesh-server --test agent_credentials 2>&1 | tee "$STACK_OUT/cargo.txt"
code=${PIPESTATUS[0]}
set -e
read -r passed failed skipped < <(cargo_counts "$STACK_OUT/cargo.txt")
status=passed; { [ "$code" = 0 ] && [ "$passed" -gt 0 ]; } || status=failed
result "$status" "$passed" "$failed" "$skipped" "authboundry agent_credentials" log="$STACK_OUT/cargo.txt"
