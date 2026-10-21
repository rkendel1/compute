#!/usr/bin/env bash
# A generic MCP client (the official SDK) against the bridge a CLI packed from
# the locked appboundry generates for the locked invoice desk, served by
# `appport dev`, importing the stack's own @appport/sdk. Grants filter the
# tools, the authorized write persists, refusals change nothing; every state
# claim is read from the application's SQLite database. The cases are the L1
# experiment's (stack-agent mcp-client-cases.mjs at the locked commit).
source "$STACK_HARNESS/lib.sh"
prepare_desk; prepare_l1_harness
tools="$STACK_WORK/l1-harness"; desk="$STACK_WORK/desk"; out="$STACK_OUT"
cp "$STACK_WORK/generated.sha256" "$STACK_WORK/appport-add.txt" "$out/"
db="$out/invoices.db"
cd "$desk"
DB_PATH="$db" PORT=3911 ./node_modules/.bin/tsx src/server.ts >"$out/app.log" 2>&1 & app=$!
reader="reader-$(head -c 24 /dev/urandom | base64 | tr -d '/+=')xxxxxxxxxxxxxxxx"
writer="writer-$(head -c 24 /dev/urandom | base64 | tr -d '/+=')xxxxxxxxxxxxxxxx"
APPPORT_UPSTREAM=http://127.0.0.1:3911 APPPORT_MCP_TOKEN=$reader APPPORT_MCP_SCOPES="invoices.read" ./node_modules/.bin/appport dev --port 3921 --no-studio </dev/null >"$out/dev-reader.log" 2>&1 & r=$!
APPPORT_UPSTREAM=http://127.0.0.1:3911 APPPORT_MCP_TOKEN=$writer APPPORT_MCP_SCOPES="invoices.read invoices.write" ./node_modules/.bin/appport dev --port 3922 --no-studio </dev/null >"$out/dev-writer.log" 2>&1 & w=$!
trap 'kill $app $r $w 2>/dev/null || true' EXIT
for port in 3911 3921 3922; do for _ in $(seq 1 150); do curl -s -o /dev/null "http://127.0.0.1:$port/" && break; sleep 0.2; done; done
endpoint=/apps/com.example.invoice-desk/mcp
unauthorized=$(curl -s -o "$out/no-token.json" -w '%{http_code}' -X POST -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' --data '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' "http://127.0.0.1:3922$endpoint")
set +e
node "$tools/mcp-client-cases.mjs" "http://127.0.0.1:3921$endpoint" "$reader" "http://127.0.0.1:3922$endpoint" "$writer" "$db" >"$out/mcp-cases.json"
code=$?
set -e
passed=$(grep -c '^PASS' "$out/mcp-cases.json" || true); failed=$(grep -c '^FAIL' "$out/mcp-cases.json" || true)
if [ "$unauthorized" = 401 ]; then passed=$((passed+1)); else failed=$((failed+1)); echo "FAIL a caller without the token got $unauthorized"; fi
grep -E '^(PASS|FAIL)' "$out/mcp-cases.json"
status=passed; { [ "$code" = 0 ] && [ "$failed" = 0 ] && [ "$passed" -gt 0 ]; } || status=failed
result "$status" "$passed" "$failed" 0 "MCP cases over the generated bridge with @appport/sdk $(lock_version @appport/sdk); tokenless caller: $unauthorized" cases="$out/mcp-cases.json" generated="$out/generated.sha256"
