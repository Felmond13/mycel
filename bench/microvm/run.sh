#!/usr/bin/env bash
# Campagne de mesure démarrage microVM vs Docker — PROTOCOLE docs/isolation-engines.md §bench.
#
#   RUNS=30 (défaut)   exécutions mesurées par série
#   WARMUPS=5          échauffements non mesurés
#
# Ordre ABBA (A, C, C, A) pour détecter la dérive. La métrique de référence
# est le fichier .times (ns, T0 = lancement -> T1 = premier octet 200) ;
# le JSON hyperfine (temps total, nettoyage inclus) sert de contrôle.
# Exécuter en ROOT (le tap KVM le requiert, comme le daemon Docker).
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
cd "$HERE"
RESULTS=results
RUNS=${RUNS:-30}
WARMUPS=${WARMUPS:-5}

if [ "$(id -u)" -ne 0 ]; then
  echo "la campagne doit tourner en root (tap KVM)" >&2
  exit 1
fi
command -v hyperfine >/dev/null || { echo "hyperfine absent du PATH" >&2; exit 1; }
[ -f assets/rootfs.cpio ] || { echo "assets/rootfs.cpio absent (mkinitramfs.sh)" >&2; exit 1; }
docker image inspect benchhttp:microvm >/dev/null 2>&1 || { echo "image absente (mkimage.sh)" >&2; exit 1; }

mkdir -p "$RESULTS"
{
  echo "=== Mycel microVM boot bench — environnement ==="
  date -u
  uname -a
  lscpu | sed -n '1,20p'
  free -h
  docker version 2>/dev/null | head -20
  /usr/local/bin/cloud-hypervisor --version 2>/dev/null || true
  hyperfine --version
  rustc -Vv 2>/dev/null || true
  echo "--- SHA-256 ---"
  sha256sum assets/vmlinux assets/rootfs.cpio assets/benchhttp 2>/dev/null
  cat assets/vmlinux.version 2>/dev/null
  git -C "$HERE/../.." rev-parse HEAD 2>/dev/null || true
} > "$RESULTS/env.txt" 2>&1

series() {
  local id="$1" scn="$2" i
  echo ""
  echo "=== série $id ($scn) — $WARMUPS échauffements + $RUNS mesures ==="
  for i in $(seq 1 "$WARMUPS"); do
    ./run_until_ready.sh "$scn" /dev/null || { echo "échauffement $scn échoué (run $i)" >&2; exit 1; }
  done
  : > "$RESULTS/$id.times"
  hyperfine --runs "$RUNS" --style basic \
    --export-json "$RESULTS/$id.json" \
    "./run_until_ready.sh $scn $RESULTS/$id.times"
  ./stats.sh "$RESULTS/$id.times"
}

# ABBA : Docker (A), microVM (C), microVM, Docker.
series A docker
series C microvm
series C_b microvm
series A_b docker

echo ""
echo "=== Récapitulatif ==="
for f in "$RESULTS"/*.times; do
  printf '%-16s ' "$(basename "$f")"
  ./stats.sh "$f"
done
