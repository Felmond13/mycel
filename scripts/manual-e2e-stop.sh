#!/usr/bin/env bash
# Manual check: a user-initiated stop must come back clean (stopped_by_user
# true, no failure_hint, neutral display). Throwaway server on :7879.
set -uo pipefail
MYC=/home/nboukadoum-ext/mycel/target/release/myc
API=http://localhost:7879/api
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); echo "PASS: $1"; }
bad() { FAIL=$((FAIL+1)); echo "FAIL: $1"; }

nohup "$MYC" ui --port 7879 > /tmp/ui-stop-test.log 2>&1 &
SRV=$!
sleep 2

CID=$(curl -s -X POST "$API/containers" -H 'Content-Type: application/json' \
  -d '{"reference":"redis:7-alpine","name":"stop-test","command":["redis-server","--port","16380"],"map_user":"999","port":16380}' \
  | python3 -c 'import sys,json;print(json.load(sys.stdin).get("id",""))')
[ -n "$CID" ] && ok "started (id=$CID)" || bad "start failed"
sleep 3

RES=$(curl -s -X POST "$API/containers/$CID/stop" | python3 -c '
import sys,json
d=json.load(sys.stdin)
print(d.get("exit_code"), d.get("stopped_by_user"), d.get("failure_hint"), d.get("status"))')
echo "stop result: $RES"
case "$RES" in
  *True*None*exited*) ok "clean user stop: exit=$RES";;
  *) bad "unexpected stop classification: $RES";;
esac

# The classification must survive a server restart (persisted).
kill -9 "$SRV"; sleep 1
nohup "$MYC" ui --port 7879 > /tmp/ui-stop-test2.log 2>&1 &
SRV=$!
sleep 2
RES2=$(curl -s "$API/containers" | python3 -c "
import sys,json
cs=json.load(sys.stdin)['containers']
m=[c for c in cs if c['id']=='$CID']
print(m[0].get('stopped_by_user'), m[0].get('failure_hint')) if m else print('gone')")
[ "$RES2" = "True None" ] && ok "classification survives restart: $RES2" || bad "after restart: $RES2"

curl -s -X DELETE "$API/containers/$CID" > /dev/null
kill -9 "$SRV" 2>/dev/null
echo "RESULT: $PASS passed, $FAIL failed"
exit $((FAIL > 0))
