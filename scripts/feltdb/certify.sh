#!/usr/bin/env bash
# Certify Compute as a consumer of the certified FeltDB release.
#
#   FELTDB_SERVER_BIN=/path/to/feltdb-server \
#   FELTDB_PREVIOUS_SERVER_BIN=/path/to/previous/feltdb-server \
#     scripts/feltdb/certify.sh [evidence-dir]
#
# Runs every check and writes its log and result to the evidence directory
# (default docs/feltdb-0.11.8-evidence). A check that cannot run (a missing
# binary) is recorded NOT_RUN, never PASS. The report itself,
# docs/feltdb-0.11.8-consumer-certification.{md,json}, is written from this
# evidence.
set -u
root=$(cd "$(dirname "$0")/../.." && pwd)
out=${1:-$root/docs/feltdb-0.11.8-evidence}
mkdir -p "$out/logs"
cd "$root"
results="$out/checks.json"
echo "[" > "$results"
first=1

record() { # name status command log
  [ $first -eq 1 ] || echo "," >> "$results"
  first=0
  printf '  {"check": "%s", "status": "%s", "command": "%s", "log": "%s"}' "$1" "$2" "$3" "$4" >> "$results"
}

check() { # name requires command...
  local name=$1 requires=$2
  shift 2
  local log="logs/$name.log"
  for variable in $requires; do
    if [ -z "${!variable:-}" ]; then
      echo "NOT_RUN (needs $variable)" > "$out/$log"
      record "$name" NOT_RUN "$*" "$log"
      echo "$name: NOT_RUN (needs $variable)"
      return
    fi
  done
  if "$@" > "$out/$log" 2>&1; then
    record "$name" PASS "$*" "$log"
    echo "$name: PASS"
  else
    record "$name" FAIL "$*" "$log"
    echo "$name: FAIL (see $out/$log)"
  fi
}

check resolved-version "" node scripts/feltdb/verify-version.mjs --json
check model-compiles "" bash -c "cd packages/compute-state-model && npm run check && npm test"
check workspace-tests "" cargo test --workspace --no-fail-fast
check adapter-real-server FELTDB_SERVER_BIN \
  cargo test -p compute-state-feltdb -- --include-ignored --test-threads 1 --skip state_written_by_the_previous_server --skip feltdb_request_cost
check previous-server-compatibility "FELTDB_SERVER_BIN FELTDB_PREVIOUS_SERVER_BIN" \
  cargo test -p compute-state-feltdb --test consumer state_written_by_the_previous_server -- --ignored
check controller-real-server FELTDB_SERVER_BIN \
  cargo test -p compute-environment --test feltdb_consumer -- --ignored --test-threads 1 --skip benchmark
check end-to-end-real-server FELTDB_SERVER_BIN \
  cargo test -p compute-cli --test recovery managed_feltdb -- --ignored
check feltdb-request-cost FELTDB_SERVER_BIN \
  env COMPUTE_CERTIFICATION_OUT="$out" cargo test --release -p compute-state-feltdb --test consumer feltdb_request_cost -- --ignored --nocapture
check benchmark FELTDB_SERVER_BIN \
  env COMPUTE_CERTIFICATION_OUT="$out" cargo test --release -p compute-environment --test feltdb_consumer benchmark -- --ignored --nocapture
echo "" >> "$results"
echo "]" >> "$results"
echo "evidence: $out"
