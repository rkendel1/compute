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
trap 'rm -rf "$temporary"' EXIT HUP INT TERM
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
