#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
  echo "usage: certify-distribution.sh ASSEMBLED_DISTRIBUTION" >&2
  exit 2
fi

distribution=$1
test -x "$distribution/bin/compute" || { echo "Compute distribution is missing" >&2; exit 1; }
command -v jq >/dev/null 2>&1 || { echo "jq is required" >&2; exit 1; }
"$distribution/bin/compute" distribution verify "$distribution" --json
temporary=$(mktemp -d "${TMPDIR:-/tmp}/compute-distribution-certification.XXXXXX")
job_container=
cleanup() {
  if [ -n "$job_container" ]; then docker rm -f "$job_container" >/dev/null 2>&1 || true; fi
  rm -rf "$temporary"
}
trap cleanup EXIT HUP INT TERM
COMPUTE_HOME="$distribution" COMPUTE_REQUIRE_ALL_RUNTIMES=1 \
  "$distribution/bin/compute" certify --json > "$temporary/bare.json"

image="compute-certification:${COMPUTE_CERTIFICATION_TAG:-local}"
docker build \
  -f distribution/Dockerfile \
  --build-arg COMPUTE_DISTRIBUTION="$distribution" \
  -t "$image" .
docker run --rm \
  -e COMPUTE_REQUIRE_ALL_RUNTIMES=1 \
  "$image" certify --json > "$temporary/docker.json"

cat > "$temporary/main.py" <<'PY'
import os
data = open(os.path.join(os.environ["COMPUTE_WORK_DIR"], "input.txt"), encoding="utf-8").read()
open(os.path.join(os.environ["COMPUTE_OUTPUT_DIR"], "result.txt"), "w", encoding="utf-8").write(data)
PY
printf 'receipt-evidence' > "$temporary/input.txt"
cat > "$temporary/workload.json" <<'JSON'
{
  "version": "1",
  "runtime": "python",
  "entrypoint": "main.py",
  "inputs": [{"path": "input.txt", "source": {"type": "file", "path": "input.txt"}}],
  "outputs": [{"path": "result.txt", "required": true}],
  "resources": {"timeout_ms": 30000},
  "network": "network"
}
JSON

COMPUTE_HOME="$distribution" "$distribution/bin/compute" bundle create \
  --workload "$temporary/workload.json" \
  --output "$temporary/workload.compute" \
  --json > "$temporary/bundle.json"

mkdir "$temporary/jobs"
start_job_server() {
  job_container=$(docker run --rm -d \
    -p 127.0.0.1::8080 \
    -v "$temporary/jobs:/jobs" \
    "$image" serve \
    --listen 0.0.0.0:8080 \
    --public-url http://compute-docker:8080 \
    --job-store /jobs \
    --max-concurrent-jobs 1)
  job_port=$(docker port "$job_container" 8080/tcp | sed 's/.*://')
  job_provider="http://127.0.0.1:$job_port"
  attempts=0
  until "$distribution/bin/compute" remote health --provider "$job_provider" --json >/dev/null 2>&1; do
    attempts=$((attempts + 1))
    if [ "$attempts" -ge 50 ]; then echo "Compute job server did not become healthy" >&2; exit 1; fi
    sleep 0.1
  done
}

start_job_server
"$distribution/bin/compute" remote submit \
  --provider "$job_provider" \
  --bundle "$temporary/workload.compute" \
  --idempotency-key docker-certification \
  --json > "$temporary/job-submission.json"
job_id=$(jq -r .job_id "$temporary/job-submission.json")
"$distribution/bin/compute" remote wait \
  --provider "$job_provider" "$job_id" \
  --timeout 60s --json > "$temporary/job-result.json"
"$distribution/bin/compute" remote receipt \
  --provider "$job_provider" "$job_id" \
  --output "$temporary/job-receipt.json"
"$distribution/bin/compute" remote artifacts \
  --provider "$job_provider" "$job_id" --json > "$temporary/job-artifacts.json"

docker rm -f "$job_container" >/dev/null
job_container=
start_job_server
"$distribution/bin/compute" remote status \
  --provider "$job_provider" "$job_id" --json > "$temporary/job-recovered.json"
jq -e '.status == "succeeded"' "$temporary/job-recovered.json" >/dev/null

cat > "$temporary/slow.py" <<'PY'
import time
time.sleep(10)
PY
cat > "$temporary/slow-workload.json" <<'JSON'
{"version":"1","runtime":"python","entrypoint":"slow.py","network":"network"}
JSON
COMPUTE_HOME="$distribution" "$distribution/bin/compute" bundle create \
  --workload "$temporary/slow-workload.json" \
  --output "$temporary/slow.compute" --json >/dev/null
"$distribution/bin/compute" remote submit --provider "$job_provider" \
  --bundle "$temporary/slow.compute" --json > "$temporary/blocking-job.json"
sleep 0.2
"$distribution/bin/compute" remote submit --provider "$job_provider" \
  --bundle "$temporary/slow.compute" --json > "$temporary/queued-job.json"
queued_job_id=$(jq -r .job_id "$temporary/queued-job.json")
"$distribution/bin/compute" remote cancel --provider "$job_provider" \
  "$queued_job_id" --json > "$temporary/job-cancellation.json"
jq -e '.cancellation.requested == true' "$temporary/job-cancellation.json" >/dev/null

COMPUTE_HOME="$distribution" "$distribution/bin/compute" run \
  --workload "$temporary/workload.json" \
  --receipt "$temporary/bare-receipt.json" \
  --json > "$temporary/bare-execution.json"
docker run --rm \
  -v "$temporary:/receipt-fixture" \
  "$image" run \
  --workload /receipt-fixture/workload.json \
  --receipt /receipt-fixture/docker-receipt.json \
  --json > "$temporary/docker-execution.json"

mkdir "$temporary/artifacts"
cp "$temporary/input.txt" "$temporary/artifacts/result.txt"
"$distribution/bin/compute" receipt verify "$temporary/docker-receipt.json" \
  --distribution "$distribution" \
  --artifacts "$temporary/artifacts" \
  --json > "$temporary/docker-receipt-verification.json"
"$distribution/bin/compute" receipt verify "$temporary/job-receipt.json" \
  --distribution "$distribution" \
  --artifacts "$temporary/artifacts" \
  --json > "$temporary/job-receipt-verification.json"

jq -S 'del(.execution_id, .started_at, .finished_at, .receipt_hash)' \
  "$temporary/bare-receipt.json" > "$temporary/bare-evidence.json"
jq -S 'del(.execution_id, .started_at, .finished_at, .receipt_hash)' \
  "$temporary/docker-receipt.json" > "$temporary/docker-evidence.json"
cmp "$temporary/bare-evidence.json" "$temporary/docker-evidence.json" || {
  echo "Bare Linux and Docker receipt evidence differ" >&2
  exit 1
}

jq -S '[.runtimes[] | {runtime, locked_version, result}]' "$temporary/bare.json" > "$temporary/bare-matrix.json"
jq -S '[.runtimes[] | {runtime, locked_version, result}]' "$temporary/docker.json" > "$temporary/docker-matrix.json"
cmp "$temporary/bare-matrix.json" "$temporary/docker-matrix.json" || {
  echo "Bare Linux and Docker runtime matrices differ" >&2
  exit 1
}
cat "$temporary/bare.json"
cat "$temporary/docker.json"
cat "$temporary/docker-receipt-verification.json"
cat "$temporary/job-receipt-verification.json"
