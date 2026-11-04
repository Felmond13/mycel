#!/usr/bin/env bash
# store-modes.sh — arbitrage disque des modes v2 (transfer / lean / packs-only).
# Mesures pour docs/design/store-v2.md : corpus alpine:3.20 + alpine:3.21 +
# nginx:1.27-alpine, store vierge par mode, binaire release.
#   (1) disque total par mode (après ingest, avant tout run)
#   (2) temps d'ingest total
#   (3) temps de `myc run alpine -- echo hi` x3, médiane
#   (4) packs-only : premier run (réassemblage) vs suivants
set -euo pipefail

REPO=/home/nboukadoum-ext/mycel
BUILD=/home/nboukadoum-ext/build
MYC=$REPO/target/release/myc
IMAGES="docker.io/library/alpine:3.20 docker.io/library/alpine:3.21 docker.io/library/nginx:1.27-alpine"
REF=docker.io/library/alpine:3.20
RESULTS=$BUILD/store-modes-results.txt

cd "$REPO"

# Build release avec retry (verrou cargo partagé avec d'autres agents).
# Sécu : les mtimes des fichiers écrits via UNC (Windows->WSL) sont parfois
# invisibles pour cargo — on vérifie donc que le binaire contient bien les
# symboles v2, sinon on force via touch.
build_ok() {
  cargo build --release -p myc-cli -p myc-store --examples >/tmp/sm-build.log 2>&1 &&
    grep -q MYCEL_CHUNK_MODE "$MYC" 2>/dev/null
}
ok=0
for i in $(seq 1 10); do
  if build_ok; then ok=1; break; fi
  if [ ! -x "$MYC" ] || ! grep -q MYCEL_CHUNK_MODE "$MYC" 2>/dev/null; then
    touch "$REPO/crates/myc-store/src/lib.rs" \
      "$REPO/crates/myc-store/src/chunks.rs" \
      "$REPO/crates/myc-store/src/blob_cache.rs" \
      "$REPO/crates/myc-cli/src/main.rs"
  fi
  echo "build retry $i"; sleep 10
done
if [ "$ok" != 1 ]; then echo "BUILD FAILED"; tail -40 /tmp/sm-build.log; exit 1; fi

# now_ms : ms depuis l'époque. $EPOCHREALTIME (bash >= 5) évite les
# différences entre GNU date et uutils date (WSL : %N/%3N non fiables).
now_ms() {
  local t=${EPOCHREALTIME/./}
  echo $((t / 1000))
}

set_env() { # $1 store, $2 chunked 1|0, $3 mode
  export MYCEL_STORE=$1
  if [ "$2" = 1 ]; then
    export MYCEL_CHUNKED=1 MYCEL_CHUNK_MODE=$3
  else
    unset MYCEL_CHUNKED MYCEL_CHUNK_MODE
  fi
}

ingest_all() { # affiche le total ms ; détail sur stderr
  local total=0 t0 t1 img tag
  for img in $IMAGES; do
    tag=${img##*:}
    t0=$(now_ms)
    "$MYC" ingest "$img" >"/tmp/sm-ingest-${MYCEL_CHUNK_MODE:-v1}-${tag}.log" 2>&1
    t1=$(now_ms)
    echo "  [${MYCEL_CHUNK_MODE:-v1}] ingest $img: $((t1-t0)) ms" >&2
    total=$((total + t1 - t0))
  done
  echo "$total"
}

run_ms() { # affiche ms ; échoue bruyamment sinon
  local t0 t1 rc
  t0=$(now_ms)
  rc=0
  "$MYC" run "$REF" -- echo hi >/tmp/sm-run.log 2>&1 || rc=$?
  t1=$(now_ms)
  if [ "$rc" != 0 ] || ! grep -q hi /tmp/sm-run.log; then
    echo "RUN FAILED (rc=$rc)"; tail -20 /tmp/sm-run.log >&2; exit 1
  fi
  echo $((t1 - t0))
}

median3() { printf '%s\n' "$1" "$2" "$3" | sort -n | sed -n 2p; }

: > "$RESULTS"
printf 'MODE\tBLOBS_B\tPACKS_B\tSIDECARS_B\tTOTAL_B\tDU_B\tINGEST_MS\tRUN1_MS\tRUN2_MS\tRUN3_MS\tRUN_MED_MS\tRUN_WARM_MS\n' |
  tee -a "$RESULTS"

for mode in v1 transfer lean packs-only; do
  store=$BUILD/store-$mode
  rm -rf "$store"
  echo "=== mode $mode (store $store) ===" >&2
  if [ "$mode" = v1 ]; then set_env "$store" 0 ""; else set_env "$store" 1 "$mode"; fi
  t_ingest=$(ingest_all)

  # (1) disque APRÈS ingest, AVANT tout run (sinon le cache packs-only
  # fausserait la mesure).
  line=$(env -u MYCEL_CHUNKED -u MYCEL_CHUNK_MODE MYCEL_STORE=$store \
    cargo run --release -q -p myc-store --example store-v2-report -- "$store" 2>/dev/null)
  blobs=$(awk '/^blobs /{print $3}' <<<"$line")
  packsb=$(awk '/^packs /{print $3}' <<<"$line")
  sideb=$(awk '/^sidecars /{print $3}' <<<"$line")
  total=$(awk '/^total_disk /{print $2}' <<<"$line")
  du_b=$(du -sb "$store" | cut -f1)

  # (3)+(4) runs.
  if [ "$mode" = v1 ]; then set_env "$store" 0 ""; else set_env "$store" 1 "$mode"; fi
  r1=$(run_ms)
  r2=$(run_ms)
  r3=$(run_ms)
  med=$(median3 "$r1" "$r2" "$r3")
  warm=$(((r2 + r3) / 2))

  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$mode" "$blobs" "$packsb" "$sideb" "$total" "$du_b" "$t_ingest" "$r1" "$r2" "$r3" "$med" "$warm" |
    tee -a "$RESULTS"
done

echo >&2
echo "=== packs-only : état du cache de blobs après les 3 runs ===" >&2
du -sb "$BUILD/store-packs-only/blob-cache" 2>/dev/null >&2 || true
ls "$BUILD/store-packs-only/blob-cache" 2>/dev/null | wc -l >&2
echo "chunk-mode files:" >&2
for m in transfer lean packs-only; do
  echo "  $m -> $(cat "$BUILD/store-$m/chunk-mode" 2>/dev/null)" >&2
done
echo "DONE — results in $RESULTS"
