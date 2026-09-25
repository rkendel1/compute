#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
image="local/compute-placement-acceptance:dev"
prefix="compute-policy-accept-$$"
temporary=$(mktemp -d /tmp/compute-placement-acceptance.XXXXXX)
build_context="$root/.compute-acceptance-context-$$"
export COMPUTE_CAPABILITY_CACHE="$temporary/provider-capabilities.json"

cleanup() {
  for name in "$prefix-a" "$prefix-b" "$prefix-c"; do
    container stop "$name" >/dev/null 2>&1 || true
  done
  rm -rf "$build_context"
  rm -rf "$temporary"
}
trap cleanup EXIT INT TERM

free_port() {
  python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()'
}

port_a=$(free_port)
port_b=$(free_port)
port_c=$(free_port)
app_port_a=$(free_port)
app_port_b=$(free_port)
app_port_c=$(free_port)

# Apple Container otherwise walks the entire workspace (including the multi-GB
# Rust target directory) before Dockerfile excludes are applied. Build from a
# disposable, pruned copy that still includes uncommitted source changes.
mkdir -p "$build_context"
tar -C "$root" -cf - \
  Cargo.toml Cargo.lock crates distribution .container/acceptance/Dockerfile |
  tar -C "$build_context" -xf -

if test "${COMPUTE_ACCEPTANCE_SKIP_BUILD:-0}" != 1; then
  container build --progress plain \
    --file "$build_context/.container/acceptance/Dockerfile" \
    --tag "$image" "$build_context"
fi

start_provider() {
  name=$1
  port=$2
  cpu=$3
  container_memory=$4
  resource_memory=$5
  runtime=$6
  concurrency=$7
  application_port=$8
  container run --detach --rm --name "$name" \
    --cpus "$cpu" --memory "$container_memory" \
    --publish "127.0.0.1:$port:8080" \
    --publish "127.0.0.1:$application_port:3000" \
    "$image" serve \
    --listen 0.0.0.0:8080 \
    --public-url "http://127.0.0.1:$port" \
    --allow-runtime "$runtime" \
    --resource-cpu "$cpu" \
    --resource-memory "$resource_memory" \
    --resource-disk 8GiB \
    --max-concurrent-jobs "$concurrency" >/dev/null
}

start_provider "$prefix-a" "$port_a" 2 2G 2GiB node 2 "$app_port_a"
start_provider "$prefix-b" "$port_b" 8 8G 8GiB node 4 "$app_port_b"
start_provider "$prefix-c" "$port_c" 4 4G 4GiB python 2 "$app_port_c"

cat >"$temporary/compute-pool.toml" <<EOF
[pool]
capability_ttl_seconds = 30

[providers.provider-a]
kind = "remote"
endpoint = "http://127.0.0.1:$port_a"
application_endpoint = "http://127.0.0.1:$app_port_a"
priority = 100

[providers.provider-b]
kind = "remote"
endpoint = "http://127.0.0.1:$port_b"
application_endpoint = "http://127.0.0.1:$app_port_b"
priority = 50

[providers.provider-c]
kind = "remote"
endpoint = "http://127.0.0.1:$port_c"
application_endpoint = "http://127.0.0.1:$app_port_c"
priority = 25
EOF

mkdir -p "$temporary/small-node" "$temporary/large-node" "$temporary/python"
cat >"$temporary/small-node/main.js" <<'EOF'
setTimeout(() => console.log("small-node"), 10000)
EOF
cat >"$temporary/small-node/compute.toml" <<'EOF'
[runtime]
name = "node"
[run]
entrypoint = "main.js"
[network]
mode = "network"
[resources]
cpu = 1
memory = "256MiB"
[placement]
policy = "auto"
EOF
cp "$temporary/small-node/main.js" "$temporary/large-node/main.js"
cat >"$temporary/large-node/compute.toml" <<'EOF'
[runtime]
name = "node"
[run]
entrypoint = "main.js"
[network]
mode = "network"
[resources]
cpu = 6
memory = "2GiB"
EOF
cat >"$temporary/python/main.py" <<'EOF'
print("python")
EOF
cat >"$temporary/python/compute.toml" <<'EOF'
[runtime]
name = "python"
[run]
entrypoint = "main.py"
[network]
mode = "network"
[resources]
cpu = 1
memory = "256MiB"
EOF

cargo build --manifest-path "$root/Cargo.toml" --release -p compute-cli
compute="$root/target/release/compute"
pool="$temporary/compute-pool.toml"

wait_health() {
  provider=$1
  attempts=0
  until "$compute" provider capabilities "$provider" --pool-config "$pool" --json >/dev/null 2>&1; do
    attempts=$((attempts + 1))
    test "$attempts" -lt 100
    sleep 0.1
  done
}
wait_health provider-a
wait_health provider-b
wait_health provider-c

assert_selected() {
  workload=$1
  expected=$2
  shift 2
  result=$("$compute" placement "$workload" --pool-config "$pool" --refresh --json "$@")
  actual=$(printf '%s' "$result" | python3 -c 'import json,sys; print(json.load(sys.stdin)["selected"]["provider_id"])')
  test "$actual" = "$expected"
}

assert_selected "$temporary/small-node" provider-a
assert_selected "$temporary/large-node" provider-b
assert_selected "$temporary/python" provider-c
assert_selected "$temporary/small-node" provider-b --prefer-provider provider-b

# Explicit selection remains strict.
if "$compute" placement "$temporary/python" --provider provider-a \
  --pool-config "$pool" --refresh --json >/dev/null 2>&1; then
  echo "incompatible explicit provider unexpectedly succeeded" >&2
  exit 1
fi

# Two jobs reserve provider-a; the third waits and is promoted after release.
job_a=$("$compute" pool submit "$temporary/small-node" --provider provider-a --pool-config "$pool" --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["job_id"])')
job_b=$("$compute" pool submit "$temporary/small-node" --provider provider-a --pool-config "$pool" --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["job_id"])')
job_c=$("$compute" pool submit "$temporary/small-node" --provider provider-a --pool-config "$pool" --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["job_id"])')
sleep 0.1
status_c=$("$compute" remote status --provider provider-a "$job_c" --pool-config "$pool" --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["status"])')
test "$status_c" = "waiting_for_capacity"
"$compute" remote wait --provider provider-a "$job_a" --pool-config "$pool" --timeout 30s --json >/dev/null
"$compute" remote wait --provider provider-a "$job_b" --pool-config "$pool" --timeout 30s --json >/dev/null
"$compute" remote wait --provider provider-a "$job_c" --pool-config "$pool" --timeout 30s --json >/dev/null

"$compute" remote receipt --provider provider-a "$job_c" --pool-config "$pool" --json |
  python3 -c 'import json,sys; r=json.load(sys.stdin); assert r["receipt_version"]=="compute.receipt@1"; assert r["reservation"]["reservation_id"].startswith("rsv_"); assert r["placement"]["policy"]["mode"]=="provider"'

# Product-level application lifecycle: real HTTP, logs, stop, history, and
# application identity sealed into the final receipt.
mkdir -p "$temporary/compute-demo"
cat >"$temporary/compute-demo/server.js" <<'EOF'
const http = require("http");
http.createServer((_request, response) => response.end("Hello from Compute\n"))
  .listen(3000, "0.0.0.0", () => console.log("compute-demo ready"));
EOF
cat >"$temporary/compute-demo/compute.toml" <<'EOF'
[application]
name = "compute-demo"
port = 3000
[runtime]
name = "node"
version = ">=24"
[run]
entrypoint = "server.js"
[network]
mode = "network"
[resources]
cpu = 1
memory = "512MiB"
[placement]
policy = "auto"
EOF

application=$temporary/compute-demo
run=$("$compute" run "$application" --provider provider-a --pool-config "$pool" --json)
printf '%s' "$run" | python3 -c 'import json,sys; r=json.load(sys.stdin); assert r["application"]["name"]=="compute-demo"; assert r["provider"]=="provider-a"; assert r["endpoint"].startswith("http://127.0.0.1:")'
attempts=0
until test "$(curl -fsS "http://127.0.0.1:$app_port_a")" = "Hello from Compute"; do
  attempts=$((attempts + 1))
  test "$attempts" -lt 100
  sleep 0.1
done
$compute logs "$application" --pool-config "$pool" --json |
  python3 -c 'import json,sys; logs=json.load(sys.stdin); assert "compute-demo ready" in logs["stdout"]; assert not logs["complete"]'
$compute status "$application" --pool-config "$pool" --json |
  python3 -c 'import json,sys; status=json.load(sys.stdin); assert status["status"]=="running"; assert status["endpoint"].startswith("http://127.0.0.1:")'
$compute stop "$application" --pool-config "$pool" --json >/dev/null
history=$("$compute" history "$application" --pool-config "$pool" --json)
application_job=$(printf '%s' "$history" | python3 -c 'import json,sys; h=json.load(sys.stdin); assert h[0]["job"]["status"]=="cancelled"; print(h[0]["job"]["job_id"])')
$compute remote receipt --provider provider-a "$application_job" --pool-config "$pool" --json |
  python3 -c 'import json,sys; receipt=json.load(sys.stdin); assert receipt["application"]["name"]=="compute-demo"; assert receipt["application"]["port"]==3000'

echo "Apple Container placement/scheduling acceptance passed"
