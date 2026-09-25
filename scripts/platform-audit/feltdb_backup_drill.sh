#!/bin/bash
# FeltDB backup/restore drill with a real feltdb-server, for docs/platform-audit.md.
#   FELTDB_SERVER_BIN=/path/to/feltdb-server scripts/platform-audit/feltdb_backup_drill.sh
set -u
F="${FELTDB_SERVER_BIN:?set FELTDB_SERVER_BIN}"
WORK=$(mktemp -d); trap 'rm -rf "$WORK"' EXIT; cd "$WORK" && mkdir original
export FELTDB_MASTER_KEY=drill
TOKEN=$("$F" keys create --keys original/keys.json --name drill --scope state:read,state:write --namespace drill | tail -1)
start() { "$F" --host 127.0.0.1 --port 0 --namespace drill --data "$1/state.log" --keys "$1/$2" --audit "$1/audit.log" --auth > "$3" 2>&1 & echo $!; }
url() { for _ in $(seq 100); do U=$(grep -o "http://127.0.0.1:[0-9]*" "$1" | head -1); [ -n "$U" ] && { echo "$U"; return; }; sleep 0.1; done; }
P=$(start original keys.json s1.out); URL=$(url s1.out)
S=$(date +%s%N)
CODES=$(for i in $(seq 1 500); do curl -s -o /dev/null -w "%{http_code}\n" -X POST -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' "$URL/collections/records" -d "{\"id\":\"r$i\",\"value\":\"v$i\"}"; done | sort | uniq -c | tr -s ' ')
echo "writes: 500 sequential authenticated HTTP writes (one curl each): $(( ($(date +%s%N)-S)/1000000 )) ms; status counts:$CODES"
kill "$P"; wait "$P" 2>/dev/null
for OUT in backup-relative ./backup-dot "$WORK/backup-absolute"; do
  S=$(date +%s%N); "$F" backup create --data original/state.log --keys original/keys.json --audit original/audit.log --output "$OUT" >"create.out" 2>&1; RC=$?; MS=$(( ($(date +%s%N)-S)/1000000 ))
  echo "backup create --output $(basename "$OUT") ($([ "${OUT:0:1}" = / ] && echo absolute || { [ "${OUT:0:2}" = ./ ] && echo ./relative || echo bare-relative; })): exit $RC in $MS ms; archive present: $([ -f "$OUT/manifest.json" ] && echo yes || echo no); output: $(head -c 120 create.out)"
done
"$F" backup verify --archive "$WORK/backup-absolute" >/dev/null 2>&1; echo "backup verify: exit $?"
S=$(date +%s%N); "$F" backup restore --archive "$WORK/backup-absolute" --output "$WORK/restored" >/dev/null 2>&1; RC=$?; echo "backup restore (absolute): exit $RC in $(( ($(date +%s%N)-S)/1000000 )) ms"
"$F" backup restore --archive "$WORK/backup-absolute" --output restored-relative >/dev/null 2>&1; RC=$?; echo "backup restore --output restored-relative (bare-relative): exit $RC; restored present: $([ -d restored-relative/files ] && echo yes || echo no)"
P=$(start restored/files api-keys.json s2.out); URL=$(url s2.out)
echo "restored record r137: $(curl -s -H "Authorization: Bearer $TOKEN" "$URL/collections/records/r137")"
echo "restored record r500 status: $(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOKEN" "$URL/collections/records/r500")"
echo "the original API key still authenticates after restore: $([ "$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOKEN" "$URL/collections/records/r1")" = 200 ] && echo yes || echo no)"
kill "$P"; wait "$P" 2>/dev/null
echo "sizes: $(du -sh original backup-absolute restored | tr '\n' ' ' | tr '\t' ' ')"
