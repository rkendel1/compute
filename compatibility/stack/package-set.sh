#!/usr/bin/env bash
# The stack's exact packages install together from the registry, each with
# the integrity the lock recorded, and each loads.
source "$STACK_HARNESS/lib.sh"
dir="$STACK_OUT/install"; mkdir -p "$dir"; cd "$dir"
node -e '
  const lock = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"));
  const deps = Object.fromEntries(lock.packages.map(p => [p.package, p.version]));
  require("fs").writeFileSync("package.json", JSON.stringify({ name: "stack-package-set", private: true, dependencies: deps }, null, 1));
' "$STACK_LOCK"
npm install --no-audit --no-fund --ignore-scripts --no-bin-links >install.log 2>&1 || { cat install.log; result failed 0 1 0 "npm install failed"; exit 1; }
passed=0; failed=0; notes=()
for package in $(node -e 'for (const p of JSON.parse(require("fs").readFileSync(process.argv[1],"utf8")).packages) console.log(p.package)' "$STACK_LOCK"); do
  want=$(lock_version "$package"); integrity=$(lock_integrity "$package")
  got=$(node -p "require('./node_modules/$package/package.json').version")
  resolved=$(node -p "require('./package-lock.json').packages['node_modules/$package'].integrity")
  if [ "$got" = "$want" ] && [ "$resolved" = "$integrity" ]; then
    echo "ok   $package $got (integrity matches the lock)"; passed=$((passed+1))
  else
    echo "FAIL $package installed $got ($resolved), lock $want ($integrity)"; failed=$((failed+1)); notes+=("$package mismatch")
  fi
  if node --input-type=module -e "await import('$package')" >>load.log 2>&1 || node -e "require('$package')" >>load.log 2>&1; then
    echo "ok   $package loads"; passed=$((passed+1))
  else
    echo "FAIL $package does not load"; failed=$((failed+1)); notes+=("$package does not load")
  fi
done
# npm's own view of the tree: every dependency of every stack package resolves.
if npm ls --all >ls.txt 2>ls.err; then echo "ok   npm ls: the tree is valid"; passed=$((passed+1)); else echo "FAIL npm ls"; cat ls.err; failed=$((failed+1)); notes+=("npm ls reports an invalid tree"); fi
status=passed; [ "$failed" = 0 ] || status=failed
result "$status" "$passed" "$failed" 0 "${notes[*]:-every package installed with its locked integrity and loaded; npm ls valid}" lock=package-lock.json tree=ls.txt
