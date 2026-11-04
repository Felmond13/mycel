#!/usr/bin/env bash
# Comparatif côte à côte : images Docker réelles vs les mêmes images dans le
# store Mycel — TOUT est mesuré, rien d'inventé (résultats bruts archivés).
#
# Pour chaque image :
#   1. pull/ingest À FROID ×3 (docker rmi / myc rm+gc entre chaque essai)
#   2. pull/ingest À CHAUD ×3  (déjà présent)
#   3. démarrage ×10 via hyperfine (temps + mémoire max) : même commande
#      des deux côtés
#   4. taille : docker image inspect .Size vs octets réellement ajoutés au
#      store Mycel (du -sb, avant/après) — la dédup inter-images est le point
#      clé : les images suivantes ne paient que leurs fichiers nouveaux.
# Finale : myc stats + myc diff alpine:3.20 alpine:3.21.
#
# Usage : bash run.sh    (user ; docker et myc dans PATH ; réseau requis)
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
MYC="$REPO/target/release/myc"
RESULTS="$HERE/results"
STORE="$HOME/.mycel"
HYPERFINE=$(command -v hyperfine || echo /usr/local/bin/hyperfine)

mkdir -p "$RESULTS"
: > "$RESULTS/raw.txt"

# Images test — volontairement absentes du poste avant la campagne (les
# images préexistantes de l'utilisateur ne sont PAS touchées).
IMAGES=(alpine:3.20 alpine:3.21 nginx:1.27-alpine redis:7-alpine python:3.12-alpine)

cmd_for() {
  case "$1" in
    alpine:*)           echo "echo hi" ;;
    nginx:*)            echo "nginx -v" ;;
    redis:*)            echo "redis-server --version" ;;
    python:*)           echo "python3 -V" ;;
  esac
}

log() { echo "$@" | tee -a "$RESULTS/raw.txt"; }
store_bytes() { du -sb "$STORE" 2>/dev/null | cut -f1; }
now_ns() { date +%s%N; }

log "=== comparatif docker vs mycel — $(date -u) ==="
log "myc: $("$MYC" --version 2>/dev/null | head -1) ; docker: $(docker --version)"

# Vérifs : images absentes au départ (sinon la mesure à froid est fausse).
for img in "${IMAGES[@]}"; do
  if docker image inspect "$img" >/dev/null 2>&1; then
    log "AVERTISSEMENT : $img déjà présente côté docker — mesures à froid docker faussées pour elle"
  fi
done

log ""
log "=== phase 1 : à froid (réseau réel), puis à chaud, puis démarrage ==="

for img in "${IMAGES[@]}"; do
  CMD=$(cmd_for "$img")
  log ""
  log "--- $img (commande mesurée : $CMD) ---"

  # --- Docker : pull à froid ×3 (rmi entre) --------------------------------
  for rep in 1 2 3; do
    docker image inspect "$img" >/dev/null 2>&1 && docker rmi "$img" >/dev/null 2>&1
    t0=$(now_ns)
    docker pull -q "$img" >/dev/null 2>&1 || { log "$img docker-cold $rep ERREUR"; continue; }
    t1=$(now_ns)
    log "$img docker-cold $rep $(( (t1 - t0) / 1000000 )) ms"
  done
  docker pull -q "$img" >/dev/null 2>&1   # présente pour la suite

  # --- Docker : pull à chaud ×3 --------------------------------------------
  for rep in 1 2 3; do
    t0=$(now_ns)
    docker pull -q "$img" >/dev/null 2>&1
    t1=$(now_ns)
    log "$img docker-warm $rep $(( (t1 - t0) / 1000000 )) ms"
  done

  DSZ=$(docker image inspect "$img" --format '{{.Size}}' 2>/dev/null)
  log "$img docker-size $DSZ octets (taille image décompressée)"

  # --- Mycel : ingest à froid ×3 (rm+gc entre) ------------------------------
  for rep in 1 2 3; do
    "$MYC" ls 2>/dev/null | grep -q "$img" && { "$MYC" rm "$img" >/dev/null 2>&1; "$MYC" gc >/dev/null 2>&1; }
    BEFORE=$(store_bytes)
    t0=$(now_ns)
    "$MYC" ingest "$img" >/dev/null 2>&1 || { log "$img myc-cold $rep ERREUR"; continue; }
    t1=$(now_ns)
    AFTER=$(store_bytes)
    log "$img myc-cold $rep $(( (t1 - t0) / 1000000 )) ms ; +$(( AFTER - BEFORE )) octets au store"
  done

  # --- Mycel : ingest à chaud ×3 --------------------------------------------
  for rep in 1 2 3; do
    BEFORE=$(store_bytes)
    t0=$(now_ns)
    "$MYC" ingest "$img" >/dev/null 2>&1
    t1=$(now_ns)
    AFTER=$(store_bytes)
    log "$img myc-warm $rep $(( (t1 - t0) / 1000000 )) ms ; +$(( AFTER - BEFORE )) octets"
  done

  # --- Démarrage ×10 : hyperfine (temps + mémoire), même commande -----------
  "$HYPERFINE" --runs 10 --style none --export-json "$RESULTS/run-docker-${img//[:\/]/-}.json" \
    "docker run --rm $img $CMD" 2>/dev/null | grep -E "Wall Time|Memory" | sed "s/^/  docker : /" | tee -a "$RESULTS/raw.txt"
  "$HYPERFINE" --runs 10 --style none --export-json "$RESULTS/run-myc-${img//[:\/]/-}.json" \
    "$MYC run $img -- $CMD" 2>/dev/null | grep -E "Wall Time|Memory" | sed "s/^/  myc   : /" | tee -a "$RESULTS/raw.txt"
done

log ""
log "=== phase 2 : totaux et dédup inter-images ==="
log "docker — somme des .Size des 5 images :"
docker image inspect "${IMAGES[@]}" --format '{{.Size}}' 2>/dev/null | awk '{s+=$1} END {printf "  %d octets (%.1f Mo)\n", s, s/1048576}'
log "mycel — store complet après les 5 ingests :"
log "  $(store_bytes) octets ($(du -sh "$STORE" | cut -f1))"
"$MYC" stats 2>/dev/null | tee -a "$RESULTS/raw.txt"

log ""
log "=== phase 3 : diff réel entre deux versions (spécialité mycel) ==="
"$MYC" diff alpine:3.20 alpine:3.21 2>/dev/null | tee -a "$RESULTS/raw.txt"

log ""
log "brut complet : $RESULTS/raw.txt"
