#!/usr/bin/env bash
# Manual E2E for container persistence/adoption + port preflight.
# Runs a throwaway UI server on :7878 against the default store.
set -uo pipefail

MYC=/home/nboukadoum-ext/mycel/target/release/myc
API=http://localhost:7878/api
PASS=0; FAIL=0
ok()   { PASS=$((PASS+1)); echo "PASS: $1"; }
bad()  { FAIL=$((FAIL+1)); echo "FAIL: $1"; }

echo "== start UI server (A) on :7878 =="
nohup "$MYC" ui --port 7878 > /tmp/ui-a.log 2>&1 &
SRV=$!
sleep 2

echo "== start redis on port 16379 =="
CID=$(curl -s -X POST "$API/containers" -H 'Content-Type: application/json' \
  -d '{"reference":"redis:7-alpine","name":"adopt-test","command":["redis-server","--port","16379"],"map_user":"999","port":16379}' \
  | python3 -c 'import sys,json;print(json.load(sys.stdin).get("id",""))')
[ -n "$CID" ] && ok "container started (id=$CID)" || bad "container did not start"
sleep 3
STATE=$(curl -s "$API/containers" | python3 -c 'import sys,json;cs=json.load(sys.stdin)["containers"];print(cs[0]["status"] if cs else "none")')
[ "$STATE" = running ] && ok "redis is running before restart" || bad "redis state before restart: $STATE"

echo "== kill UI server (simulate dashboard restart) =="
kill -9 "$SRV"; sleep 1

echo "== relaunch UI server (B) on :7878 =="
nohup "$MYC" ui --port 7878 > /tmp/ui-b.log 2>&1 &
SRV=$!
sleep 2

ADOPTED=$(curl -s "$API/containers/$CID/processes" >/dev/null; curl -s "$API/containers" \
  | python3 -c "import sys,json;cs=json.load(sys.stdin)['containers'];m=[c for c in cs if c['id']=='$CID'];print(m[0]['status'] if m else 'gone')")
[ "$ADOPTED" = running ] && ok "container re-adopted after restart" || bad "adoption state: $ADOPTED"

LOGTAIL=$(curl -s "$API/containers/$CID/logs?offset=0" | python3 -c 'import sys,json;print(json.load(sys.stdin)["data"][-300:])')
case "$LOGTAIL" in *"dashboard restarted"*) ok "log survived + adoption note present";; *) bad "log/adoption note missing: $LOGTAIL";; esac

echo "== port preflight: second redis on same port must be refused =="
REFUSE=$(curl -s -X POST "$API/containers" -H 'Content-Type: application/json' \
  -d '{"reference":"redis:7-alpine","name":"conflict-test","command":["redis-server","--port","16379"],"map_user":"999","port":16379}')
case "$REFUSE" in *"already in use"*) ok "port conflict refused with clear message: $REFUSE";; *) bad "unexpected preflight answer: $REFUSE";; esac

echo "== failure hint: force a real bind failure (no declared port) =="
HINTID=$(curl -s -X POST "$API/containers" -H 'Content-Type: application/json' \
  -d '{"reference":"redis:7-alpine","name":"hint-test","command":["redis-server","--port","16379"],"map_user":"999"}' \
  | python3 -c 'import sys,json;print(json.load(sys.stdin).get("id",""))')
sleep 4
HINT=$(curl -s "$API/containers" | python3 -c "import sys,json;cs=json.load(sys.stdin)['containers'];m=[c for c in cs if c['id']=='$HINTID'];print((m[0].get('failure_hint') or '')+'|'+m[0]['status'] if m else 'gone')")
case "$HINT" in *"already in use"*) ok "failure_hint set on early bind failure: $HINT";; *) bad "no failure hint: $HINT";; esac

echo "== stop adopted container =="
STOPPED=$(curl -s -X POST "$API/containers/$CID/stop" | python3 -c 'import sys,json;d=json.load(sys.stdin);print(d.get("status","?"),d.get("running"))')
case "$STOPPED" in exited*False*) ok "adopted container stopped: $STOPPED";; *) bad "stop result: $STOPPED";; esac

echo "== cleanup: remove test containers, kill server B =="
for id in $CID $HINTID; do
  [ -n "$id" ] && curl -s -X DELETE "$API/containers/$id" > /dev/null
done
LEFT=$(curl -s "$API/containers" | python3 -c 'import sys,json;print(len(json.load(sys.stdin)["containers"]))')
[ "$LEFT" = 0 ] && ok "registry clean after removals" || bad "$LEFT containers left"
kill -9 "$SRV" 2>/dev/null
# server B's kill leaves its state file; it should be empty ([]) already
sleep 1
pgrep -f 'redis-server' > /dev/null && bad "a redis process survived cleanup" || ok "no stray redis processes"

echo
echo "RESULT: $PASS passed, $FAIL failed"
exit $((FAIL > 0))
