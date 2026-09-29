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
policy_container=
cleanup() {
  if [ -n "$job_container" ]; then docker rm -f "$job_container" >/dev/null 2>&1 || true; fi
  if [ -n "$policy_container" ]; then docker rm -f "$policy_container" >/dev/null 2>&1 || true; fi
  if [ -n "${image:-}" ] && [ -d "$temporary/jobs" ]; then
    docker run --rm --entrypoint /bin/chmod -v "$temporary:/cleanup" \
      "$image" -R a+rwX /cleanup >/dev/null 2>&1 || true
  fi
  rm -rf "$temporary"
}
trap cleanup EXIT HUP INT TERM
if ! COMPUTE_DISTRIBUTION_ROOT="$distribution" COMPUTE_REQUIRE_ALL_RUNTIMES=1 \
  "$distribution/bin/compute" certify --json > "$temporary/bare.json"; then
  cat "$temporary/bare.json" >&2
  echo "bare distribution certification failed" >&2
  exit 1
fi

image="compute-certification:${COMPUTE_CERTIFICATION_TAG:-local}"
docker build \
  -f distribution/Dockerfile \
  --build-arg COMPUTE_DISTRIBUTION="$distribution" \
  -t "$image" .
if ! docker run --rm \
  -e COMPUTE_REQUIRE_ALL_RUNTIMES=1 \
  "$image" certify --json > "$temporary/docker.json"; then
  cat "$temporary/docker.json" >&2
  echo "container distribution certification failed" >&2
  exit 1
fi

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

free_host_port() {
  PYTHONDONTWRITEBYTECODE=1 PYTHONNOUSERSITE=1 \
    "$distribution/runtimes/python/bin/python3" -c \
    'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()'
}

COMPUTE_DISTRIBUTION_ROOT="$distribution" "$distribution/bin/compute" bundle create \
  --workload "$temporary/workload.json" \
  --output "$temporary/workload.compute" \
  --json > "$temporary/bundle.json"

mkdir "$temporary/jobs"
start_job_server() {
  job_port=$(free_host_port)
  job_container=$(docker run -d \
    -p "127.0.0.1:$job_port:8080/tcp" \
    -v "$temporary/jobs:/jobs" \
    "$image" serve --insecure-unauthenticated \
    --listen 0.0.0.0:8080 \
    --public-url http://compute-docker:8080 \
    --job-store /jobs \
    --max-concurrent-jobs 1)
  job_provider="http://127.0.0.1:$job_port"
  cat > "$temporary/job-pool.toml" <<TOML
[providers.job]
kind = "remote"
endpoint = "$job_provider"
TOML
  attempts=0
  until job_remote health --provider job --json >/dev/null 2>"$temporary/job-health.err"; do
    attempts=$((attempts + 1))
    if [ "$attempts" -ge 50 ]; then
      echo "Compute job server did not become healthy" >&2
      cat "$temporary/job-health.err" >&2
      docker port "$job_container" >&2 || true
      docker logs "$job_container" >&2 || true
      exit 1
    fi
    sleep 0.1
  done
}

job_remote() {
  "$distribution/bin/compute" remote "$@" \
    --pool-config "$temporary/job-pool.toml" \
    --capability-cache "$temporary/job-capabilities.json"
}

start_job_server
job_remote submit \
  --provider job \
  --bundle "$temporary/workload.compute" \
  --idempotency-key docker-certification \
  --json > "$temporary/job-submission.json"
job_id=$(jq -r .job_id "$temporary/job-submission.json")
job_remote wait \
  --provider job "$job_id" \
  --timeout 60s --json > "$temporary/job-result.json"
job_remote receipt \
  --provider job "$job_id" \
  --output "$temporary/job-receipt.json"
job_remote artifacts \
  --provider job "$job_id" --json > "$temporary/job-artifacts.json"

# Provider pool: the local provider and the Docker provider built from the
# same distribution participate in one caller-owned pool. Placement must pick
# the job-capable Docker provider, pin the distribution and dependency
# capsule, and the receipt must prove all of it.
distribution_id=$(jq -r .distribution_id "$distribution/runtime-manifest.json")
mkdir "$temporary/pool" "$temporary/pool/resolved"
printf "VALUE = 'pooled'\n" > "$temporary/pool/resolved/pool_dependency.py"
COMPUTE_DISTRIBUTION_ROOT="$distribution" "$distribution/bin/compute" deps create \
  --runtime python --resolved "$temporary/pool/resolved" \
  --output "$temporary/pool/deps.capsule" --json > "$temporary/pool/deps.json"
capsule_id=$(jq -r .capsule_id "$temporary/pool/deps.json")
cat > "$temporary/pool/main.py" <<'PY'
import pool_dependency
print(pool_dependency.VALUE, end="")
PY
cat > "$temporary/pool/workload.json" <<JSON
{"version":"1","runtime":"python","entrypoint":"main.py","network":"network",
 "resources":{"timeout_ms":30000},"dependencies":{"capsule":"$capsule_id"}}
JSON
COMPUTE_DISTRIBUTION_ROOT="$distribution" "$distribution/bin/compute" bundle create \
  --workload "$temporary/pool/workload.json" --deps "$temporary/pool/deps.capsule" \
  --output "$temporary/pool/workload.compute" --json > "$temporary/pool/bundle.json"
cat > "$temporary/pool/compute-pool.toml" <<TOML
[providers.local]
kind = "local"
priority = 100

[providers.docker]
kind = "remote"
endpoint = "$job_provider"
priority = 10
TOML
pool() {
  COMPUTE_DISTRIBUTION_ROOT="$distribution" "$distribution/bin/compute" "$@" \
    --pool-config "$temporary/pool/compute-pool.toml" \
    --capability-cache "$temporary/pool/capabilities.json"
}
pool provider list --refresh --json > "$temporary/pool/providers.json"
jq -e '[.providers[] | select(.discovery == "discovered")] | length == 2' \
  "$temporary/pool/providers.json" >/dev/null
pool placement inspect --bundle "$temporary/pool/workload.compute" \
  --distribution "$distribution_id" --submit --json > "$temporary/pool/placement.json"
jq -e --arg dist "$distribution_id" --arg capsule "$capsule_id" '
  .outcome == "placed"
  and .selected.provider_id == "docker"
  and .compatible_providers == ["docker"]
  and ([.providers[] | select(.provider_id == "local") | .reasons[].code] == ["jobs_unsupported"])
  and .requirements.distribution.id == $dist
  and .requirements.dependencies.id == $capsule' "$temporary/pool/placement.json" >/dev/null
pool pool submit --bundle "$temporary/pool/workload.compute" \
  --distribution "$distribution_id" --json > "$temporary/pool/submission.json"
jq -e --slurpfile placement "$temporary/pool/placement.json" '
  .provider_id == "docker" and .placement_id == $placement[0].placement_id' \
  "$temporary/pool/submission.json" >/dev/null
pool_job=$(jq -r .job_id "$temporary/pool/submission.json")
job_remote wait --provider job "$pool_job" \
  --timeout 60s --json > "$temporary/pool/result.json"
jq -e '.status == "completed" and .stdout.text == "pooled"' "$temporary/pool/result.json" >/dev/null
job_remote receipt --provider job "$pool_job" \
  --output "$temporary/pool/receipt.json"
"$distribution/bin/compute" receipt verify "$temporary/pool/receipt.json" \
  --distribution "$distribution" --json >/dev/null
jq -e --arg dist "$distribution_id" --arg capsule "$capsule_id" \
  --slurpfile placement "$temporary/pool/placement.json" '
  .placement.provider_id == "docker"
  and .placement.placement_id == $placement[0].placement_id
  and .placement.selection_mode == "pool"
  and .provider.endpoint == "http://compute-docker:8080"
  and .provider_protocol == "compute.remote@1"
  and .distribution.id == $dist
  and .dependencies.capsule_id == $capsule
  and .dependencies.verified' "$temporary/pool/receipt.json" >/dev/null
if pool pool run --provider local --bundle "$temporary/pool/workload.compute" \
  --distribution "sha256:0000000000000000000000000000000000000000000000000000000000000000" \
  --json > "$temporary/pool/explicit.json" 2>/dev/null; then
  echo "an incompatible explicit provider executed" >&2
  exit 1
fi
jq -e '.placement.failure.code == "explicit_provider_incompatible"' \
  "$temporary/pool/explicit.json" >/dev/null

# Execution policy: a Docker server from the same distribution enforces a
# restrictive production policy. The allowed workload executes with
# admission evidence; disallowed workloads are refused before any runtime
# starts, which the server's executions_started counter proves.
mkdir "$temporary/policy"
cat > "$temporary/policy/production.json" <<'JSON'
{"version": 1, "name": "production-policy",
 "allowed_runtimes": ["python"], "allowed_networks": ["network"],
 "limits": {"max_timeout_ms": 60000}}
JSON
"$distribution/bin/compute" policy validate "$temporary/policy/production.json" --json \
  > "$temporary/policy/validation.json"
policy_port=$(free_host_port)
policy_container=$(docker run -d \
  -p "127.0.0.1:$policy_port:8080/tcp" \
  -v "$temporary/policy:/policy:ro" \
  "$image" serve --insecure-unauthenticated \
  --listen 0.0.0.0:8080 \
  --public-url http://compute-policy:8080 \
  --job-store /tmp/jobs \
  --policy /policy/production.json)
policy_provider="http://127.0.0.1:$policy_port"
cat > "$temporary/policy-pool.toml" <<TOML
[providers.policy]
kind = "remote"
endpoint = "$policy_provider"
TOML
policy_remote() {
  "$distribution/bin/compute" remote "$@" \
    --pool-config "$temporary/policy-pool.toml" \
    --capability-cache "$temporary/policy-capabilities.json"
}
attempts=0
until policy_remote health --provider policy --json \
  >/dev/null 2>"$temporary/policy-health.err"; do
  attempts=$((attempts + 1))
  if [ "$attempts" -ge 50 ]; then
    echo "policy server did not become healthy" >&2
    cat "$temporary/policy-health.err" >&2
    docker port "$policy_container" >&2 || true
    docker logs "$policy_container" >&2 || true
    exit 1
  fi
  sleep 0.1
done
policy_remote capabilities --provider policy --json \
  > "$temporary/policy/capabilities.json"
jq -e '.policy.name == "production-policy" and .policy.allowed_runtimes == ["python"]' \
  "$temporary/policy/capabilities.json" >/dev/null
policy_remote run --provider policy \
  --bundle "$temporary/workload.compute" \
  --receipt "$temporary/policy/receipt.json" --json > "$temporary/policy/allowed.json"
jq -e '.status == "completed" and .admission.admission_status == "admitted"' \
  "$temporary/policy/allowed.json" >/dev/null
jq -e --slurpfile run "$temporary/policy/allowed.json" '
  .admission_status == "admitted"
  and .admission_id == $run[0].admission.admission_id
  and .policy_id == $run[0].admission.policy_id
  and .provider.endpoint == "http://compute-policy:8080"' \
  "$temporary/policy/receipt.json" >/dev/null
"$distribution/bin/compute" receipt verify "$temporary/policy/receipt.json" \
  --distribution "$distribution" --json >/dev/null
started_before=$(curl -fsS -H 'X-Compute-Protocol: compute.remote@1' \
  "$policy_provider/compute/health" | jq -r .executions_started)
# slow.compute declares no timeout (timeout_unbounded); the node bundle is a
# runtime the policy does not allow (runtime_denied).
cat > "$temporary/slow.py" <<'PY'
import time
time.sleep(10)
PY
cat > "$temporary/slow-workload.json" <<'JSON'
{"version":"1","runtime":"python","entrypoint":"slow.py","network":"network"}
JSON
COMPUTE_DISTRIBUTION_ROOT="$distribution" "$distribution/bin/compute" bundle create \
  --workload "$temporary/slow-workload.json" \
  --output "$temporary/slow.compute" --json >/dev/null
printf 'console.log("must not run")\n' > "$temporary/policy/denied.js"
cat > "$temporary/policy/denied.json" <<'JSON'
{"version":"1","runtime":"node","entrypoint":"denied.js","network":"network",
 "resources":{"timeout_ms":1000}}
JSON
COMPUTE_DISTRIBUTION_ROOT="$distribution" "$distribution/bin/compute" bundle create \
  --workload "$temporary/policy/denied.json" \
  --output "$temporary/policy/denied.compute" --json >/dev/null
for denied in "$temporary/slow.compute" "$temporary/policy/denied.compute"; do
  if policy_remote run --provider policy \
    --bundle "$denied" --json > "$temporary/policy/denied-run.json" 2> "$temporary/policy/denied-run.err"; then
    echo "policy server executed a disallowed workload: $denied" >&2
    exit 1
  fi
  grep -q "AdmissionDenied" "$temporary/policy/denied-run.err"
  if policy_remote submit --provider policy \
    --bundle "$denied" --json > /dev/null 2>&1; then
    echo "policy server accepted a job for a disallowed workload: $denied" >&2
    exit 1
  fi
done
started_after=$(curl -fsS -H 'X-Compute-Protocol: compute.remote@1' \
  "$policy_provider/compute/health" | jq -r .executions_started)
if [ "$started_before" != "$started_after" ]; then
  echo "a denied workload reached the runtime ($started_before -> $started_after)" >&2
  exit 1
fi
docker rm -f "$policy_container" >/dev/null
policy_container=

docker rm -f "$job_container" >/dev/null
job_container=
start_job_server
job_remote status \
  --provider job "$job_id" --json > "$temporary/job-recovered.json"
jq -e '.status == "succeeded"' "$temporary/job-recovered.json" >/dev/null

job_remote submit --provider job \
  --bundle "$temporary/slow.compute" --json > "$temporary/blocking-job.json"
sleep 0.2
job_remote submit --provider job \
  --bundle "$temporary/slow.compute" --json > "$temporary/queued-job.json"
queued_job_id=$(jq -r .job_id "$temporary/queued-job.json")
job_remote cancel --provider job \
  "$queued_job_id" --json > "$temporary/job-cancellation.json"
jq -e '.cancellation.requested == true' "$temporary/job-cancellation.json" >/dev/null

COMPUTE_DISTRIBUTION_ROOT="$distribution" "$distribution/bin/compute" run \
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
