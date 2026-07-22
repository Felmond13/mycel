#!/usr/bin/env bash
# The two-minute demo, measured: hub pull, lazy cold start, incremental push.
#
# Sets up a hub store (served with `myc hub serve`) and empty client stores,
# then measures real wall-clock times:
#   1. full `myc pull` of alpine:3.20 from the hub into an empty store
#   2. `myc run --lazy` cold start (time to first output) from the hub
#   3. incremental push: alpine:3.21 after 3.20 is already on the hub —
#      only the delta blobs cross the wire
# Compares with docker where docker exists (otherwise N/A).
set -euo pipefail
cd "$(dirname "$0")/.."

echo "== building release binary =="
bash scripts/dev.sh release >/dev/null
MYC="$PWD/target/release/myc"

WORK="$(mktemp -d)"
HUB_PORT="${HUB_PORT:-9641}"
HUB_URL="http://127.0.0.1:$HUB_PORT"
HUB_PID=""
trap '[ -n "$HUB_PID" ] && kill "$HUB_PID" 2>/dev/null; rm -rf "$WORK"' EXIT

now_ms() { echo $(( $(date +%s%N) / 1000000 )); }

echo "== seeding hub store (ingest alpine:3.20 + 3.21) =="
MYCEL_STORE="$WORK/hub" "$MYC" ingest alpine:3.20 >/dev/null
MYCEL_STORE="$WORK/hub" "$MYC" ingest alpine:3.21 >/dev/null

echo "== starting hub =="
MYCEL_STORE="$WORK/hub" "$MYC" hub serve --port "$HUB_PORT" &
HUB_PID=$!
for _ in $(seq 1 50); do
  curl -sf "$HUB_URL/api/v1/ping" >/dev/null 2>&1 && break
  sleep 0.1
done

# ---- 1. full pull from hub into an empty store ------------------------------
t0=$(now_ms)
MYCEL_STORE="$WORK/client-pull" "$MYC" pull alpine:3.20 --from "$HUB_URL" >"$WORK/pull.out" 2>/dev/null
t1=$(now_ms)
PULL_MS=$((t1 - t0))
PULL_LINE="$(grep 'pulled' "$WORK/pull.out" | head -1)"

# ---- 2. lazy cold start from hub (empty store, time to first output) --------
t0=$(now_ms)
MYCEL_STORE="$WORK/client-lazy" "$MYC" run --lazy --from "$HUB_URL" alpine:3.20 \
  -- /bin/echo lazy-hello >"$WORK/lazy.out" 2>"$WORK/lazy.err"
t1=$(now_ms)
LAZY_MS=$((t1 - t0))
grep -q lazy-hello "$WORK/lazy.out" || { echo "lazy run produced no output"; exit 1; }
LAZY_FETCH_LINE="$(grep 'lazy run done' "$WORK/lazy.err" | sed 's/^myc: //')"

# Warm lazy start: same store, everything the app touches is now cached.
t0=$(now_ms)
MYCEL_STORE="$WORK/client-lazy" "$MYC" run --lazy --from "$HUB_URL" alpine:3.20 \
  -- /bin/echo lazy-hello >/dev/null 2>/dev/null
t1=$(now_ms)
LAZY_WARM_MS=$((t1 - t0))

# ---- 3. incremental push: 3.21 after 3.20 is already on the hub -------------
# Fresh hub so the numbers are clean: push 3.20 (full), then 3.21 (delta).
PUSH_PORT=$((HUB_PORT + 1))
PUSH_URL="http://127.0.0.1:$PUSH_PORT"
MYCEL_STORE="$WORK/dev" "$MYC" ingest alpine:3.20 >/dev/null
MYCEL_STORE="$WORK/dev" "$MYC" ingest alpine:3.21 >/dev/null
MYCEL_STORE="$WORK/hub2" "$MYC" hub serve --port "$PUSH_PORT" &
HUB2_PID=$!
for _ in $(seq 1 50); do
  curl -sf "$PUSH_URL/api/v1/ping" >/dev/null 2>&1 && break
  sleep 0.1
done

t0=$(now_ms)
MYCEL_STORE="$WORK/dev" "$MYC" push alpine:3.20 --to "$PUSH_URL" >"$WORK/push1.out" 2>/dev/null
t1=$(now_ms)
PUSH1_MS=$((t1 - t0))
PUSH1_LINE="$(grep 'pushed' "$WORK/push1.out")"

t0=$(now_ms)
MYCEL_STORE="$WORK/dev" "$MYC" push alpine:3.21 --to "$PUSH_URL" >"$WORK/push2.out" 2>/dev/null
t1=$(now_ms)
PUSH2_MS=$((t1 - t0))
PUSH2_LINE="$(grep 'pushed' "$WORK/push2.out")"
kill "$HUB2_PID" 2>/dev/null

# ---- docker comparison (only if docker is actually usable) ------------------
DOCKER_PULL="N/A (docker not installed)"
DOCKER_RUN="N/A (docker not installed)"
if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
  docker rmi alpine:3.20 >/dev/null 2>&1 || true
  t0=$(now_ms); docker pull alpine:3.20 >/dev/null 2>&1; t1=$(now_ms)
  DOCKER_PULL="$((t1 - t0)) ms"
  t0=$(now_ms); docker run --rm alpine:3.20 echo docker-hello >/dev/null 2>&1; t1=$(now_ms)
  DOCKER_RUN="$((t1 - t0)) ms (image already local)"
fi

echo
echo "======================= mycel benchmark ======================="
printf '%-46s %s\n' "full pull from hub (alpine:3.20, empty store)" "$PULL_MS ms"
printf '%-46s %s\n'   "  $PULL_LINE" ""
printf '%-46s %s\n' "lazy cold start, time to first output"         "$LAZY_MS ms"
printf '%-46s %s\n'   "  $LAZY_FETCH_LINE" ""
printf '%-46s %s\n' "lazy warm start (fetched files now cached)"    "$LAZY_WARM_MS ms"
printf '%-46s %s\n' "push alpine:3.20 to empty hub (baseline)"      "$PUSH1_MS ms"
printf '%-46s %s\n'   "  $PUSH1_LINE" ""
printf '%-46s %s\n' "push alpine:3.21 after 3.20 (the delta)"       "$PUSH2_MS ms"
printf '%-46s %s\n'   "  $PUSH2_LINE" ""
printf '%-46s %s\n' "docker pull alpine:3.20 (reference)"           "$DOCKER_PULL"
printf '%-46s %s\n' "docker run echo (reference)"                   "$DOCKER_RUN"
echo "================================================================"
