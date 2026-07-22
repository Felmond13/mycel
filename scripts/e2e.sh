#!/usr/bin/env bash
# End-to-end smoke test: exercises the full ingest/run/export/import/gc cycle
# against a real registry image, in an isolated store.
set -euo pipefail
cd "$(dirname "$0")/.."
MYC="$PWD/target/release/myc"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
export MYCEL_STORE="$WORK/store"

echo "== ingest =="
"$MYC" ingest alpine:3.20

echo "== run =="
out="$("$MYC" run alpine:3.20 -- /bin/echo e2e-ok)"
[ "$out" = "e2e-ok" ] || { echo "unexpected output: $out"; exit 1; }

echo "== pull (offline guarantee) =="
"$MYC" pull alpine:3.20

echo "== named volumes: write, stop, run again, data still there =="
"$MYC" run -v e2edata:/data alpine:3.20 -- /bin/sh -c 'echo persisted > /data/witness'
out="$("$MYC" run -v e2edata:/data alpine:3.20 -- /bin/cat /data/witness)"
[ "$out" = "persisted" ] || { echo "volume did not persist: $out"; exit 1; }
# Capture before grepping: `grep -q` exits at the first match and, under
# pipefail, the resulting EPIPE in myc would fail the pipeline spuriously.
"$MYC" volume ls | grep -q e2edata || { echo "volume ls missing e2edata"; exit 1; }
inspect_out="$("$MYC" volume inspect e2edata)"
echo "$inspect_out" | grep -q "path:" \
  || { echo "volume inspect has no path"; exit 1; }
"$MYC" volume rm e2edata
"$MYC" volume ls | grep -q e2edata && { echo "volume rm left e2edata"; exit 1; }
# Host-path binds still work exactly as before.
mkdir -p "$WORK/bindsrc" && echo bound > "$WORK/bindsrc/f"
out="$("$MYC" run -v "$WORK/bindsrc":/mnt/b alpine:3.20 -- /bin/cat /mnt/b/f)"
[ "$out" = "bound" ] || { echo "host bind broken: $out"; exit 1; }

echo "== export / import into second store =="
cd "$WORK"
"$MYC" export alpine:3.20 -o img.tar.gz
MYCEL_STORE="$WORK/store2" "$MYC" import img.tar.gz
out="$(MYCEL_STORE="$WORK/store2" "$MYC" run alpine:3.20 -- /bin/echo import-ok)"
[ "$out" = "import-ok" ] || { echo "unexpected output: $out"; exit 1; }

echo "== export --oci: standard OCI image (docker load / podman / skopeo) =="
"$MYC" export --oci alpine:3.20 "$WORK/alpine-oci.tar"
# Structure: both formats in one tar (OCI layout + docker-save manifest.json).
# List once into a variable: `tar tf | grep -q` dies of EPIPE under pipefail
# when grep quits at the first entry (oci-layout is the first one).
oci_toc="$(tar tf "$WORK/alpine-oci.tar")"
echo "$oci_toc" | grep -q '^oci-layout$' || { echo "oci-layout missing"; exit 1; }
echo "$oci_toc" | grep -q '^index.json$' || { echo "index.json missing"; exit 1; }
echo "$oci_toc" | grep -q '^manifest.json$' || { echo "manifest.json missing"; exit 1; }
echo "$oci_toc" | grep -q '^blobs/sha256/' || { echo "blobs/sha256 missing"; exit 1; }
# Determinism: a second export is byte-identical.
"$MYC" export --oci alpine:3.20 "$WORK/alpine-oci2.tar"
cmp "$WORK/alpine-oci.tar" "$WORK/alpine-oci2.tar" \
  || { echo "OCI export not deterministic"; exit 1; }
# Digest coherence: every blob's file name is its sha256; the config's
# diff_id is the sha256 of the uncompressed layer.
mkdir -p "$WORK/oci-x" && tar xf "$WORK/alpine-oci.tar" -C "$WORK/oci-x"
for blob in "$WORK/oci-x/blobs/sha256/"*; do
  [ "$(sha256sum "$blob" | awk '{print $1}')" = "$(basename "$blob")" ] \
    || { echo "blob digest mismatch: $blob"; exit 1; }
done
if command -v python3 >/dev/null; then
  python3 - "$WORK/oci-x" <<'EOF'
import gzip, hashlib, json, sys
root = sys.argv[1]
index = json.load(open(f"{root}/index.json"))
mdigest = index["manifests"][0]["digest"].split(":")[1]
manifest = json.load(open(f"{root}/blobs/sha256/{mdigest}"))
cdigest = manifest["config"]["digest"].split(":")[1]
ldigest = manifest["layers"][0]["digest"].split(":")[1]
config = json.load(open(f"{root}/blobs/sha256/{cdigest}"))
layer = gzip.open(f"{root}/blobs/sha256/{ldigest}").read()
diff_id = "sha256:" + hashlib.sha256(layer).hexdigest()
assert config["rootfs"]["diff_ids"] == [diff_id], "diff_id mismatch"
assert config["os"] == "linux", config["os"]
docker = json.load(open(f"{root}/manifest.json"))
assert docker[0]["RepoTags"], "no RepoTags"
assert docker[0]["Layers"] == [f"blobs/sha256/{ldigest}"], "legacy layer path wrong"
print("oci structure ok:", docker[0]["RepoTags"][0], "diff_id", diff_id[:19])
EOF
fi
# Extra validation with whatever real consumer is installed.
if command -v skopeo >/dev/null; then
  skopeo inspect "oci-archive:$WORK/alpine-oci.tar" | grep -q '"Architecture"' \
    || { echo "skopeo rejected the export"; exit 1; }
  echo "skopeo inspect ok"
elif docker info >/dev/null 2>&1; then
  docker load < "$WORK/alpine-oci.tar" | grep -q "Loaded image" \
    || { echo "docker load rejected the export"; exit 1; }
  out="$(docker run --rm alpine:3.20 /bin/echo oci-roundtrip-ok)"
  [ "$out" = "oci-roundtrip-ok" ] || { echo "docker run of exported image failed: $out"; exit 1; }
  docker rmi alpine:3.20 >/dev/null
  echo "docker load + run ok"
else
  echo "skopeo/docker not available — structural checks only"
fi
rm -rf "$WORK/oci-x" "$WORK/alpine-oci.tar" "$WORK/alpine-oci2.tar"

echo "== verify =="
"$MYC" verify alpine:3.20

echo "== doctor =="
"$MYC" doctor

echo "== diff (alpine:3.20 vs alpine:3.21) =="
"$MYC" ingest alpine:3.21
diff_out="$("$MYC" diff alpine:3.20 alpine:3.21)"
echo "$diff_out" | tail -1
echo "$diff_out" | grep -q "differs from" \
  || { echo "diff produced no summary"; exit 1; }

echo "== which (reverse query) =="
which_out="$("$MYC" which /bin/busybox)"
echo "$which_out" | tail -1
echo "$which_out" | grep -q "alpine" \
  || { echo "which found no busybox"; exit 1; }

echo "== sbom (valid JSON, packages detected) =="
sbom_out="$("$MYC" sbom alpine:3.20 2>/dev/null)"
[ -n "$sbom_out" ] || { echo "sbom output empty"; exit 1; }
if command -v python3 >/dev/null; then
  echo "$sbom_out" | python3 -m json.tool > /dev/null
  echo "$sbom_out" | python3 -c '
import json, sys
d = json.load(sys.stdin)
assert d["package_source"] == "apk", "expected apk package db"
assert len(d["packages"]) > 5, "too few packages"
assert d["file_count"] > 50, "too few files"
print("sbom ok:", len(d["packages"]), "packages,", d["file_count"], "files")
'
elif command -v jq >/dev/null; then
  echo "$sbom_out" | jq -e '.packages | length > 5' > /dev/null
fi

echo "== ui smoke test =="
"$MYC" ui --port 7871 &
UI_PID=$!
sleep 2
curl -sf http://localhost:7871/api/stats | grep -q '"environments"' \
  || { echo "ui /api/stats failed"; kill "$UI_PID"; exit 1; }
curl -sf http://localhost:7871/ | grep -qi "<html" \
  || { echo "ui index failed"; kill "$UI_PID"; exit 1; }
# Download assets fully before grepping (grep -q closing the pipe early
# would make curl fail under pipefail).
curl -sf -o "$WORK/style.css" http://localhost:7871/style.css \
  && grep -q "mycel" "$WORK/style.css" \
  || { echo "ui style.css failed"; kill "$UI_PID"; exit 1; }
curl -sf -o "$WORK/vue.js" http://localhost:7871/vendor/vue.js \
  && grep -q "Vue" "$WORK/vue.js" \
  || { echo "ui vendor/vue.js failed"; kill "$UI_PID"; exit 1; }
# The ES-module graph: entry point plus a page component and a core module.
for js in js/main.js js/store.js js/components/AppsPage.js; do
  curl -sf -o "$WORK/asset.js" "http://localhost:7871/$js" \
    || { echo "ui $js failed"; kill "$UI_PID"; exit 1; }
done
grep -q "AppsPage" "$WORK/asset.js" \
  || { echo "ui js asset content wrong"; kill "$UI_PID"; exit 1; }
# Unknown assets must 404, not fall through to anything else.
code="$(curl -s -o /dev/null -w '%{http_code}' http://localhost:7871/js/nope.js)"
[ "$code" = "404" ] || { echo "ui /js 404 handling broken (got $code)"; kill "$UI_PID"; exit 1; }
# Sharing endpoints: export a .mycel archive over HTTP, check it is a real
# archive (CLI import into a fresh store), and round-trip the upload API.
curl -sf -D "$WORK/exp-headers" -o "$WORK/exp.mycel" http://localhost:7871/api/export/alpine:3.20 \
  || { echo "ui export failed"; kill "$UI_PID"; exit 1; }
grep -qi 'filename="alpine-3.20.mycel"' "$WORK/exp-headers" \
  || { echo "ui export filename wrong"; kill "$UI_PID"; exit 1; }
MYCEL_STORE="$WORK/ui-import" "$MYC" import "$WORK/exp.mycel" | grep -q offline-ready \
  || { echo "ui-exported archive not importable"; kill "$UI_PID"; exit 1; }
curl -sf -X POST --data-binary @"$WORK/exp.mycel" -H 'Content-Type: application/octet-stream' \
    http://localhost:7871/api/import | grep -q '"refs"' \
  || { echo "ui import failed"; kill "$UI_PID"; exit 1; }
kill "$UI_PID"

echo "== pin / rm / gc =="
"$MYC" pin alpine:3.20
"$MYC" unpin alpine:3.20
"$MYC" rm alpine:3.20
"$MYC" gc
"$MYC" stats

echo "== bind mounts =="
mkdir -p "$WORK/bind" && echo bind-data > "$WORK/bind/f"
MYCEL_STORE="$WORK/store2" "$MYC" run -v "$WORK/bind:/data:ro" alpine:3.20 -- /bin/cat /data/f | grep -q bind-data

echo "== network isolation (pasta) =="
if command -v pasta >/dev/null 2>&1; then
  # Publish host 18099 into an isolated container listening on 8099.
  MYCEL_STORE="$WORK/store2" "$MYC" run --net isolated -p 18099:8099 alpine:3.20 -- \
    /bin/sh -c 'echo net-isolated-ok | nc -l -p 8099' &
  NET_PID=$!
  net_ok=""
  for _ in $(seq 1 50); do
    if out="$( (exec 3<>/dev/tcp/127.0.0.1/18099 && head -c 15 <&3) 2>/dev/null )" \
       && [ "$out" = "net-isolated-ok" ]; then net_ok=1; break; fi
    sleep 0.2
  done
  wait "$NET_PID" 2>/dev/null || true
  [ -n "$net_ok" ] || { echo "isolated port publish failed"; exit 1; }
  # Loopback is up inside an isolated namespace.
  MYCEL_STORE="$WORK/store2" "$MYC" run --net isolated alpine:3.20 -- \
    /bin/ping -c1 -W1 127.0.0.1 > /dev/null || { echo "isolated loopback down"; exit 1; }
  # A machine without pasta gets the actionable install hint. The check
  # simulates that machine by trimming PATH to /usr/bin:/bin, so it only
  # makes sense when pasta lives elsewhere (on GitHub runners and some
  # distros it is /usr/bin/pasta, and the simulation cannot work).
  if [ ! -x /usr/bin/pasta ] && [ ! -x /bin/pasta ]; then
    err="$(env PATH=/usr/bin:/bin HOME="$WORK" MYCEL_STORE="$WORK/store2" \
      "$MYC" run --net isolated alpine:3.20 -- /bin/true 2>&1 || true)"
    echo "$err" | grep -q "passt" || { echo "missing passt hint: $err"; exit 1; }
  else
    echo "pasta is in /usr/bin — skipping missing-pasta hint check"
  fi
else
  echo "pasta not installed — SKIPPING network isolation e2e (sudo apt install passt to enable)"
fi

echo "== stack pod mode: internal localhost, published/unpublished ports, clean exit =="
if command -v pasta >/dev/null 2>&1 || [ -x "$HOME/.local/bin/pasta" ]; then
  POD_DIR="$WORK/podstack"
  mkdir -p "$POD_DIR"
  # server listens on the pod-internal 18086 (never published); client reaches
  # it on the pod's OWN localhost, then serves the published 8085 to the host.
  cat > "$POD_DIR/mycel.toml" <<'EOF'
[project]
name = "e2epod"

[network]
mode = "pod"
ports = ["18085:8085"]

[services.server]
image = "alpine:3.20"
command = ["/bin/sh", "-c", "echo pod-hello | nc -l -p 18086"]

[services.client]
image = "alpine:3.20"
depends_on = ["server"]
command = ["/bin/sh", "-c", "i=0; while [ $i -lt 100 ]; do out=$(nc 127.0.0.1 18086 2>/dev/null); if [ -n \"$out\" ]; then echo \"client-got: $out\"; echo pod-web-ok | nc -l -p 8085; exit 0; fi; i=$((i+1)); sleep 0.2; done; echo client-timeout; exit 1"]
EOF
  MYCEL_STORE="$WORK/store2" "$MYC" up -f "$POD_DIR/mycel.toml" > "$WORK/pod-up.log" 2>&1 &
  POD_UP_PID=$!
  # Service A joined service B on the pod's internal localhost.
  internal_ok=""
  for _ in $(seq 1 150); do
    grep -q "client-got: pod-hello" "$WORK/pod-up.log" && internal_ok=1 && break
    sleep 0.2
  done
  [ -n "$internal_ok" ] || { echo "pod internal localhost failed:"; cat "$WORK/pod-up.log"; exit 1; }
  pgrep -f "_pod-holder" >/dev/null || { echo "no pod holder while the stack is up"; exit 1; }
  # The unpublished internal port must NOT be reachable from the host.
  if (exec 3<>/dev/tcp/127.0.0.1/18086) 2>/dev/null; then
    echo "unpublished pod port 18086 is reachable from the host"; exit 1
  fi
  # The published port answers on the host.
  pub_ok=""
  for _ in $(seq 1 100); do
    if out="$( (exec 3<>/dev/tcp/127.0.0.1/18085 && head -c 10 <&3) 2>/dev/null )" \
       && [ "$out" = "pod-web-ok" ]; then pub_ok=1; break; fi
    sleep 0.2
  done
  [ -n "$pub_ok" ] || { echo "published pod port 18085 not reachable"; cat "$WORK/pod-up.log"; exit 1; }
  wait "$POD_UP_PID" || { echo "pod up exited non-zero"; cat "$WORK/pod-up.log"; exit 1; }
  grep -q "private network" "$WORK/pod-up.log" \
    || { echo "missing private-network message:"; cat "$WORK/pod-up.log"; exit 1; }
  # `up` cleaned up after itself: no holder, no pasta, nothing for `myc down`.
  # (pasta exits when the namespace empties, which can lag a little.)
  pod_gone=""
  for _ in $(seq 1 50); do
    if ! pgrep -f "_pod-holder" >/dev/null && ! pgrep -f "18085:8085" >/dev/null; then
      pod_gone=1; break
    fi
    sleep 0.2
  done
  [ -n "$pod_gone" ] || { echo "holder or pasta survived myc up"; pgrep -af "_pod-holder|18085:8085"; exit 1; }
  MYCEL_STORE="$WORK/store2" "$MYC" down -f "$POD_DIR/mycel.toml" | grep -q "nothing to clean up" \
    || { echo "myc down after a clean exit should have nothing to do"; exit 1; }

  echo "== stack pod mode: a SIGKILLed up leaves no holder; myc down cleans the record =="
  cat > "$POD_DIR/mycel.toml" <<'EOF'
[project]
name = "e2epod"

[network]
mode = "pod"

[services.napper]
image = "alpine:3.20"
command = ["/bin/sleep", "2"]
EOF
  MYCEL_STORE="$WORK/store2" "$MYC" up -f "$POD_DIR/mycel.toml" > "$WORK/pod-kill.log" 2>&1 &
  POD_KILL_PID=$!
  # Wait for the holder AND its state record (written right after the
  # network handshake) so the kill exercises the stale-record path.
  for _ in $(seq 1 100); do
    pgrep -f "_pod-holder" >/dev/null && [ -f "$WORK/store2/pods/e2epod.json" ] && break
    sleep 0.1
  done
  pgrep -f "_pod-holder" >/dev/null || { echo "holder never appeared"; cat "$WORK/pod-kill.log"; exit 1; }
  [ -f "$WORK/store2/pods/e2epod.json" ] || { echo "pod record never written"; cat "$WORK/pod-kill.log"; exit 1; }
  kill -9 "$POD_KILL_PID"
  wait "$POD_KILL_PID" 2>/dev/null || true
  sleep 1
  # PR_SET_PDEATHSIG: the holder must die with the up that spawned it.
  pgrep -f "_pod-holder" >/dev/null && { echo "holder survived SIGKILL of myc up"; exit 1; }
  MYCEL_STORE="$WORK/store2" "$MYC" down -f "$POD_DIR/mycel.toml" | grep -q "leftover" \
    || { echo "myc down did not clean the stale record"; exit 1; }

  # Without pasta: --net pod fails with the install hint, while pod mode
  # from the file falls back to the host network with a warning. The
  # simulation trims PATH to /usr/bin:/bin, so it only proves something
  # when pasta lives elsewhere (on GitHub runners it is /usr/bin/pasta).
  if [ ! -x /usr/bin/pasta ] && [ ! -x /bin/pasta ]; then
    err="$(env PATH=/usr/bin:/bin HOME="$WORK" MYCEL_STORE="$WORK/store2" \
      "$MYC" up -f "$POD_DIR/mycel.toml" --net pod 2>&1 || true)"
    echo "$err" | grep -q "passt" || { echo "missing passt hint for --net pod: $err"; exit 1; }
    out="$(env PATH=/usr/bin:/bin HOME="$WORK" MYCEL_STORE="$WORK/store2" \
      "$MYC" up -f "$POD_DIR/mycel.toml" 2>&1 || true)"
    echo "$out" | grep -q "host network instead" || { echo "missing pod fallback warning: $out"; exit 1; }
  else
    echo "pasta is in /usr/bin — skipping missing-pasta hint checks for pod mode"
  fi
else
  echo "pasta not installed — SKIPPING stack pod e2e (sudo apt install passt to enable)"
fi

echo "== hub: serve, pull into empty store, run =="
HUB_PORT=9673
MYCEL_STORE="$WORK/store2" "$MYC" hub serve --port "$HUB_PORT" &
HUB_PID=$!
trap 'kill "$HUB_PID" 2>/dev/null; rm -rf "$WORK"' EXIT
for _ in $(seq 1 50); do
  curl -sf "http://127.0.0.1:$HUB_PORT/api/v1/ping" >/dev/null 2>&1 && break
  sleep 0.1
done
MYCEL_STORE="$WORK/hub-b" "$MYC" pull alpine:3.20 --from "http://127.0.0.1:$HUB_PORT"
out="$(MYCEL_STORE="$WORK/hub-b" "$MYC" run alpine:3.20 -- /bin/echo hub-ok)"
[ "$out" = "hub-ok" ] || { echo "unexpected output: $out"; exit 1; }
MYCEL_STORE="$WORK/hub-b" "$MYC" verify alpine:3.20

echo "== hub: incremental push uploads only the delta =="
push1="$(MYCEL_STORE="$WORK/hub-b" "$MYC" push alpine:3.20 --to "http://127.0.0.1:$HUB_PORT" 2>/dev/null)"
echo "$push1" | grep -q "0 blobs of" || { echo "re-push should upload nothing: $push1"; exit 1; }
# store still has alpine:3.21 (ingested for diff above): push the version bump.
push2="$(MYCEL_STORE="$WORK/store" "$MYC" push alpine:3.21 --to "http://127.0.0.1:$HUB_PORT" 2>/dev/null)"
echo "$push2"
echo "$push2" | grep -q "pushed alpine:3.21" || { echo "delta push failed"; exit 1; }

echo "== hub: lazy run from a cold empty store =="
lazy_err="$WORK/lazy.err"
out="$(MYCEL_STORE="$WORK/hub-c" "$MYC" run --lazy --from "http://127.0.0.1:$HUB_PORT" alpine:3.20 -- /bin/echo lazy-ok 2>"$lazy_err")"
[ "$out" = "lazy-ok" ] || { echo "unexpected lazy output: $out"; cat "$lazy_err"; exit 1; }
grep -q "fetched" "$lazy_err" || { echo "no lazy fetch accounting"; cat "$lazy_err"; exit 1; }
# The store was populated on demand: the blobs that were touched are now local.
[ "$(find "$WORK/hub-c/objects" -type f | wc -l)" -gt 0 ] || { echo "lazy store not populated"; exit 1; }
# Second lazy run of the same command fetches nothing new.
out="$(MYCEL_STORE="$WORK/hub-c" "$MYC" run --lazy --from "http://127.0.0.1:$HUB_PORT" alpine:3.20 -- /bin/echo lazy-warm 2>"$lazy_err")"
[ "$out" = "lazy-warm" ] || { echo "unexpected warm lazy output: $out"; exit 1; }
grep -q "fetched 0 of" "$lazy_err" || { echo "warm lazy run should fetch nothing:"; cat "$lazy_err"; exit 1; }
kill "$HUB_PID" 2>/dev/null

echo "== deploy: incremental push to a 'server' (injected transport) =="
# The ssh transport is injectable: `env MYCEL_STORE=… bash -c` runs the
# remote side locally against a separate store — same code path as a real
# ssh deploy, no sshd needed.
# (alpine:3.20 lives in store2 at this point; alpine:3.21 in store.)
DEPLOY_REMOTE="$WORK/deploy-remote"
export MYCEL_SSH_CMD="env MYCEL_STORE=$DEPLOY_REMOTE bash -c"
dep1="$(MYCEL_STORE="$WORK/store2" "$MYC" deploy alpine:3.20 e2e@server --myc-path "$MYC" 2>/dev/null)"
echo "$dep1"
echo "$dep1" | grep -q "deployed alpine:3.20" || { echo "deploy failed: $dep1"; exit 1; }
echo "$dep1" | grep -q " 0 files sent" && { echo "first deploy sent nothing?!"; exit 1; }
# The 'server' now resolves the ref, complete and verified.
MYCEL_STORE="$DEPLOY_REMOTE" "$MYC" ls | grep -q alpine || { echo "remote ls missing alpine"; exit 1; }
MYCEL_STORE="$DEPLOY_REMOTE" "$MYC" verify alpine:3.20
# Second deploy of the same ref: zero blobs cross the wire.
dep2="$(MYCEL_STORE="$WORK/store2" "$MYC" deploy alpine:3.20 e2e@server --myc-path "$MYC" 2>/dev/null)"
echo "$dep2" | grep -q "0 files sent" || { echo "re-deploy should send nothing: $dep2"; exit 1; }
# A sibling version only sends the delta (alpine:3.21 shares most of 3.20).
dep3="$(MYCEL_STORE="$WORK/store" "$MYC" deploy alpine:3.21 e2e@server --myc-path "$MYC" 2>/dev/null)"
echo "$dep3"
sent3="$(echo "$dep3" | grep -o '[0-9]* files* sent' | grep -o '^[0-9]*')"
present3="$(echo "$dep3" | grep -o '[0-9]* already present' | grep -o '^[0-9]*')"
[ "$sent3" -gt 0 ] && [ "$present3" -gt "$sent3" ] \
  || { echo "delta deploy not incremental: sent=$sent3 present=$present3"; exit 1; }
# --run: launches detached on the 'server' with a pidfile + log.
MYCEL_STORE="$WORK/store2" "$MYC" deploy alpine:3.20 e2e@server --myc-path "$MYC" --run 2>/dev/null \
  | grep -q "started alpine:3.20" || { echo "deploy --run failed"; exit 1; }
[ -f "$DEPLOY_REMOTE/deploys/alpine_3.20.pid" ] || { echo "no remote pidfile"; exit 1; }
[ -f "$DEPLOY_REMOTE/deploys/alpine_3.20.log" ] || { echo "no remote log"; exit 1; }
unset MYCEL_SSH_CMD
# Real ssh loopback, only when an sshd actually listens on this machine
# and key-based auth to localhost works.
if ss -tln 2>/dev/null | grep -q ':22 ' && ssh -o BatchMode=yes -o ConnectTimeout=2 localhost true 2>/dev/null; then
  echo "== deploy: real ssh to localhost =="
  MYCEL_STORE="$WORK/store2" "$MYC" deploy alpine:3.20 localhost --myc-path "$MYC" \
    | grep -q "deployed alpine:3.20" || { echo "real-ssh deploy failed"; exit 1; }
else
  echo "no sshd on :22 (or no key auth) — SKIPPING real-ssh deploy test"
fi

echo "== build: declarative V1 + exec step V2 =="
BUILD_CTX="$WORK/build-ctx"
mkdir -p "$BUILD_CTX/dist"
printf '#!/bin/sh\necho built-app-ok\n' > "$BUILD_CTX/dist/app"
cat > "$BUILD_CTX/mycel-build.toml" <<'EOF'
[build]
base = "alpine:3.20"
name = "e2e-built:1"

[[build.add]]
source = "dist/app"
dest = "/usr/local/bin/app"
mode = 0o755

[[build.run]]
command = ["/bin/sh", "-c", "echo built-by-mycel > /etc/build-stamp && apk add --no-cache ca-certificates 2>/dev/null || true"]

[config]
cmd = ["/usr/local/bin/app"]
EOF
build1="$(MYCEL_STORE="$WORK/store2" "$MYC" build -f "$BUILD_CTX/mycel-build.toml" --timestamp 2026-01-01T00:00:00Z)"
id1="$(echo "$build1" | head -1 | awk '{print $1}')"
out="$(MYCEL_STORE="$WORK/store2" "$MYC" run e2e-built:1)"
[ "$out" = "built-app-ok" ] || { echo "unexpected built-app output: $out"; exit 1; }
stamp="$(MYCEL_STORE="$WORK/store2" "$MYC" run e2e-built:1 -- /bin/cat /etc/build-stamp)"
[ "$stamp" = "built-by-mycel" ] || { echo "V2 run-step stamp missing: $stamp"; exit 1; }
# Deterministic rebuild: same inputs + fixed timestamp -> same manifest id.
build2="$(MYCEL_STORE="$WORK/store2" "$MYC" build -f "$BUILD_CTX/mycel-build.toml" --timestamp 2026-01-01T00:00:00Z)"
id2="$(echo "$build2" | head -1 | awk '{print $1}')"
[ "$id1" = "$id2" ] || { echo "build not deterministic: $id1 vs $id2"; exit 1; }

echo "== ls --paths =="
ls_out="$(MYCEL_STORE="$WORK/store2" "$MYC" ls --paths)"
echo "$ls_out" | grep -q "path: " || { echo "ls --paths printed no path lines"; exit 1; }
manifest_path="$(echo "$ls_out" | grep "path: " | head -1 | awk '{print $2}')"
[ -f "$manifest_path" ] || { echo "manifest path does not exist: $manifest_path"; exit 1; }

echo "== friendly registry error =="
err_out="$("$MYC" ingest postgresql 2>&1 || true)"
echo "$err_out" | grep -qi "not found" || { echo "missing not-found error: $err_out"; exit 1; }
echo "$err_out" | grep -q "'postgres'" || { echo "missing typo hint: $err_out"; exit 1; }

echo "== ui v2: run a container from the API =="
UI2_PORT=7873
MYCEL_STORE="$WORK/store2" "$MYC" ui --port "$UI2_PORT" &
UI2_PID=$!
trap 'kill "$UI2_PID" "$HUB_PID" 2>/dev/null; rm -rf "$WORK"' EXIT
for _ in $(seq 1 60); do
  curl -sf "http://127.0.0.1:$UI2_PORT/api/stats" >/dev/null 2>&1 && break
  sleep 0.1
done

start_json="$(curl -sf -X POST -H 'Content-Type: application/json' \
  -d '{"reference":"alpine:3.20","command":["/bin/echo","ui-run-ok"],"name":"e2e-echo"}' \
  "http://127.0.0.1:$UI2_PORT/api/containers")"
cid="$(echo "$start_json" | sed -E 's/.*"id":"([0-9a-f]+)".*/\1/')"
[ -n "$cid" ] || { echo "no container id in: $start_json"; exit 1; }
marker=""
for _ in $(seq 1 150); do
  logs="$(curl -sf "http://127.0.0.1:$UI2_PORT/api/containers/$cid/logs?offset=0" || true)"
  echo "$logs" | grep -q "ui-run-ok" && marker=1 && break
  sleep 0.2
done
[ -n "$marker" ] || { echo "ui-run-ok never appeared in container logs: $logs"; exit 1; }
for _ in $(seq 1 50); do
  curl -sf "http://127.0.0.1:$UI2_PORT/api/containers" | grep -q '"status":"exited(0)"' && break
  sleep 0.2
done
curl -sf "http://127.0.0.1:$UI2_PORT/api/containers" | grep -q '"status":"exited(0)"' \
  || { echo "container never listed as exited(0)"; exit 1; }

echo "== ui v2: live metrics + process list for a running container =="
sleep_json="$(curl -sf -X POST -H 'Content-Type: application/json' \
  -d '{"reference":"alpine:3.20","command":["/bin/sleep","60"],"name":"e2e-metrics"}' \
  "http://127.0.0.1:$UI2_PORT/api/containers")"
mid="$(echo "$sleep_json" | sed -E 's/.*"id":"([0-9a-f]+)".*/\1/')"
[ -n "$mid" ] || { echo "no container id in: $sleep_json"; exit 1; }
metrics_ok=""
for _ in $(seq 1 100); do
  entry="$(curl -sf "http://127.0.0.1:$UI2_PORT/api/containers" \
    | tr '{' '\n' | grep "\"$mid\"" || true)"
  mem="$(echo "$entry" | grep -o '"memory_bytes":[0-9]*' | grep -o '[0-9]*$' || true)"
  if [ -n "$mem" ] && [ "$mem" -gt 0 ]; then metrics_ok=1; break; fi
  sleep 0.2
done
[ -n "$metrics_ok" ] || { echo "running container never reported memory_bytes>0: $entry"; exit 1; }
# The runner process reports RSS before /bin/sleep has exec'd, so poll until
# the entrypoint shows up in the process list (the sampler also caches 500ms).
proc_ok=""
for _ in $(seq 1 50); do
  if curl -sf "http://127.0.0.1:$UI2_PORT/api/containers/$mid/processes" | grep -q '"name":"sleep"'; then
    proc_ok=1; break
  fi
  sleep 0.2
done
[ -n "$proc_ok" ] || { echo "process list missing the sleep entrypoint"; exit 1; }

echo "== ui v2: gc terminates (with stats) while a container is running =="
# Regression test: a running container used to hold the store lock for its
# whole life, so /api/gc blocked forever and the UI spinner never stopped.
gc_json="$(curl -sf --max-time 30 -X POST "http://127.0.0.1:$UI2_PORT/api/gc")" \
  || { echo "gc did not terminate while a container was running"; exit 1; }
echo "$gc_json" | grep -q '"deleted":' || { echo "gc response missing stats: $gc_json"; exit 1; }
echo "$gc_json" | grep -q '"freed_bytes":' || { echo "gc response missing stats: $gc_json"; exit 1; }
# The running container survived the GC…
curl -sf "http://127.0.0.1:$UI2_PORT/api/containers/$mid/processes" | grep -q '"name":"sleep"' \
  || { echo "running container died during gc"; exit 1; }
# …and the store is still fully intact.
MYCEL_STORE="$WORK/store2" "$MYC" verify alpine:3.20 || { echo "store corrupt after gc"; exit 1; }

curl -sf -X POST "http://127.0.0.1:$UI2_PORT/api/containers/$mid/stop" >/dev/null \
  || { echo "metrics container stop failed"; exit 1; }
curl -sf -X DELETE "http://127.0.0.1:$UI2_PORT/api/containers/$mid" >/dev/null \
  || { echo "metrics container delete failed"; exit 1; }

echo "== ui v2: stack of two services with depends_on =="
curl -sf -X PUT -H 'Content-Type: application/json' \
  -d '{"services":{"one":{"image":"alpine:3.20","command":["/bin/echo","stack-one-ok"]},"two":{"image":"alpine:3.20","command":["/bin/echo","stack-two-ok"],"depends_on":["one"]}}}' \
  "http://127.0.0.1:$UI2_PORT/api/stacks/e2e-stack" | grep -q '"e2e-stack"' \
  || { echo "stack save failed"; exit 1; }
curl -sf "http://127.0.0.1:$UI2_PORT/api/stacks/e2e-stack" | grep -q '"order":\["one","two"\]' \
  || { echo "stack order wrong"; exit 1; }
curl -sf -X POST "http://127.0.0.1:$UI2_PORT/api/stacks/e2e-stack/up" | grep -q '"started"' \
  || { echo "stack up failed"; exit 1; }
stack_ok=""
for _ in $(seq 1 150); do
  detail="$(curl -sf "http://127.0.0.1:$UI2_PORT/api/stacks/e2e-stack")"
  if [ "$(echo "$detail" | grep -o '"exit_code":0' | wc -l)" -ge 2 ]; then stack_ok=1; break; fi
  sleep 0.2
done
[ -n "$stack_ok" ] || { echo "stack services did not finish: $detail"; exit 1; }
# both service logs carry their marker (service container ids come from the
# stack detail: every "id" there is a 12-hex container id)
svc_ids="$(echo "$detail" | grep -o '"id":"[0-9a-f]\{12\}"' | grep -o '[0-9a-f]\{12\}' | sort -u)"
[ "$(echo "$svc_ids" | wc -l)" -eq 2 ] || { echo "expected 2 stack containers, got: $svc_ids"; exit 1; }
for svc_cid in $svc_ids; do
  curl -sf "http://127.0.0.1:$UI2_PORT/api/containers/$svc_cid/logs?offset=0" | grep -q "stack-.*-ok" \
    || { echo "stack service log missing marker"; exit 1; }
done
curl -sf -X POST "http://127.0.0.1:$UI2_PORT/api/stacks/e2e-stack/down" >/dev/null \
  || { echo "stack down failed"; exit 1; }
curl -sf -X DELETE "http://127.0.0.1:$UI2_PORT/api/stacks/e2e-stack" | grep -q '"deleted"' \
  || { echo "stack delete failed"; exit 1; }

if command -v pasta >/dev/null 2>&1 || [ -x "$HOME/.local/bin/pasta" ]; then
  echo "== ui v2: pod-mode stack via the API =="
  curl -sf -X PUT -H 'Content-Type: application/json' \
    -d '{"toml":"[project]\nname = \"e2e-pod\"\n\n[network]\nmode = \"pod\"\nports = [\"18087:8087\"]\n\n[services.web]\nimage = \"alpine:3.20\"\ncommand = [\"/bin/sh\", \"-c\", \"echo ui-pod-ok | nc -l -p 8087\"]\n"}' \
    "http://127.0.0.1:$UI2_PORT/api/stacks/e2e-pod" | grep -q '"e2e-pod"' \
    || { echo "pod stack save failed"; exit 1; }
  curl -sf "http://127.0.0.1:$UI2_PORT/api/stacks/e2e-pod" | grep -q '"mode":"pod"' \
    || { echo "pod stack detail missing network mode"; exit 1; }
  curl -sf -X POST "http://127.0.0.1:$UI2_PORT/api/stacks/e2e-pod/up" | grep -q '"started"' \
    || { echo "pod stack up failed"; exit 1; }
  pgrep -f "_pod-holder" >/dev/null || { echo "no holder for the ui pod stack"; exit 1; }
  ui_pod_ok=""
  for _ in $(seq 1 100); do
    if out="$( (exec 3<>/dev/tcp/127.0.0.1/18087 && head -c 9 <&3) 2>/dev/null )" \
       && [ "$out" = "ui-pod-ok" ]; then ui_pod_ok=1; break; fi
    sleep 0.2
  done
  [ -n "$ui_pod_ok" ] || { echo "ui pod published port not answering"; exit 1; }
  # The service's container reports the pod network.
  curl -sf "http://127.0.0.1:$UI2_PORT/api/stacks/e2e-pod" | grep -q '"network":"pod"' \
    || { echo "pod service state missing network=pod"; exit 1; }
  curl -sf -X POST "http://127.0.0.1:$UI2_PORT/api/stacks/e2e-pod/down" >/dev/null \
    || { echo "pod stack down failed"; exit 1; }
  ui_pod_gone=""
  for _ in $(seq 1 50); do
    if ! pgrep -f "_pod-holder" >/dev/null && ! pgrep -f "18087:8087" >/dev/null; then
      ui_pod_gone=1; break
    fi
    sleep 0.2
  done
  [ -n "$ui_pod_gone" ] || { echo "holder or pasta survived stack down"; pgrep -af "_pod-holder|18087:8087"; exit 1; }
  curl -sf -X DELETE "http://127.0.0.1:$UI2_PORT/api/stacks/e2e-pod" | grep -q '"deleted"' \
    || { echo "pod stack delete failed"; exit 1; }
else
  echo "pasta not installed — SKIPPING ui pod stack e2e"
fi
kill "$UI2_PID" 2>/dev/null

echo "ALL E2E TESTS PASSED"
