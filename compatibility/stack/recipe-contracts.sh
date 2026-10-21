#!/usr/bin/env bash
# AppWorks' own test suite at the locked commit, as its CI runs it (install
# from the frozen pnpm lockfile, build, test): the recipe model and the AppPort
# contracts it builds on.
source "$STACK_HARNESS/lib.sh"
repo="$STACK_WORK/appworks"; rm -rf "$repo"; cp -r "$(src appworks)" "$repo"; cd "$repo"
{ pnpm install --frozen-lockfile && pnpm build; } >"$STACK_OUT/install.log" 2>&1 || { tail -30 "$STACK_OUT/install.log"; result failed 0 1 0 "pnpm install --frozen-lockfile or pnpm build failed"; exit 1; }
set +e
pnpm exec vitest run --reporter=json --outputFile="$STACK_OUT/vitest.json" >"$STACK_OUT/vitest.txt" 2>&1
code=$?
set -e
read -r passed failed skipped < <(node -e 'const r=require(process.argv[1]);console.log(r.numPassedTests, r.numFailedTests, r.numPendingTests+r.numTodoTests)' "$STACK_OUT/vitest.json")
status=passed; { [ "$code" = 0 ] && [ "$passed" -gt 0 ]; } || status=failed
result "$status" "$passed" "$failed" "$skipped" "appworks vitest" report="$STACK_OUT/vitest.json"
