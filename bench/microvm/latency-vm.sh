#!/usr/bin/env bash
# latency-vm.sh — latence e2e d'un run « alpine echo » par moteur (critère
# prod #7) : kvm-microvm voie B (virtiofs), voie A (ext4) et natif
# namespaces (référence). hyperfine n=10 (+2 échauffements), export JSON.
#
#   bash bench/microvm/latency-vm.sh [runs]
#
# Aucun port publié → aucun besoin de root. Sortie : results/latency-vm.{txt,json}.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
MYC="$REPO/target/debug/myc"
RUNS=${1:-10}
OUT="$HERE/results"
command -v hyperfine >/dev/null || { echo "hyperfine absent du PATH (apt/brew install hyperfine)" >&2; exit 1; }
[ -x "$MYC" ] || { echo "myc manquant : cargo build -p myc-cli" >&2; exit 1; }
mkdir -p "$OUT"

echo "=== latence e2e (echo alpine) — n=$RUNS, 2 échauffements, $(date -u +%FT%TZ) ===" | tee "$OUT/latency-vm.txt"
hyperfine --runs "$RUNS" --warmup 2 --style basic \
  --export-json "$OUT/latency-vm.json" \
  --command-name "kvm-microvm voie B (virtiofs)" \
    "$MYC run --engine kvm-microvm alpine:3.20 -- echo hi" \
  --command-name "kvm-microvm voie A (ext4)" \
    "env MYCEL_VM_ROOTFS=ext4 $MYC run --engine kvm-microvm alpine:3.20 -- echo hi" \
  --command-name "native-namespaces (référence)" \
    "$MYC run alpine:3.20 -- echo hi" \
  2>&1 | tee -a "$OUT/latency-vm.txt"

echo "" | tee -a "$OUT/latency-vm.txt"
echo "Résultats archivés : $OUT/latency-vm.{txt,json}" | tee -a "$OUT/latency-vm.txt"
