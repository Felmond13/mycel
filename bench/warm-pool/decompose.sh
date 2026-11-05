#!/usr/bin/env bash
# bench/warm-pool/decompose.sh — DÉCOMPOSITION du e2e warm par phase.
#
# La baseline (baseline.sh) dit COMBIEN ; ce script dit OÙ : n runs
# instrumentés (MYCEL_VM_TRACE=1, PhaseTimer dans firecracker.rs), chaque
# phase agrégée en médiane. Répond à la question du verdict : qu'est-ce
# qu'un pool PEUT encore éliminer (spawn FC + restore + réseau si le pool
# le pré-détient + teardown si un daemon l'absorbe) vs ce qu'aucun pool ne
# peut toucher (ramasse agent + exec + reboot invité).
#
#   sudo bash bench/warm-pool/decompose.sh            # n=20 par défaut
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
RESULTS="$HERE/results/baseline"
MYC=${MYC:-$REPO/target/release/myc}
IMG=${IMG:-alpine:3.20}
RUNS=${RUNS:-20}
SNAPCACHE="$HERE/.snapcache-base"

fail() { echo "[warm-pool] ERREUR : $*" >&2; exit 2; }
[ -x "$MYC" ] || fail "myc manquant : $MYC"

export MYCEL_SNAPSHOT_DIR="$SNAPCACHE"
mkdir -p "$RESULTS" "$SNAPCACHE"

# Réchauffage : snapshot présent (créé par baseline.sh sinon ici).
MYCEL_VM_TRACE=1 "$MYC" run --engine kvm-microvm "$IMG" -- /bin/echo ok \
  >/dev/null 2>"$RESULTS/decompose-trace.log" \
  || fail "run de réchauffage échoué"

: > "$RESULTS/decompose-traces.txt"
for _ in $(seq "$RUNS"); do
  out=$(MYCEL_VM_TRACE=1 "$MYC" run --engine kvm-microvm "$IMG" -- /bin/echo ok 2>"$RESULTS/decompose-trace.log")
  [ "$out" = "ok" ] || fail "run invalide : $out"
  cat "$RESULTS/decompose-trace.log" >> "$RESULTS/decompose-traces.txt"
done

python3 - "$RESULTS/decompose-traces.txt" "$RUNS" > "$RESULTS/decompose.txt" <<'PYEOF'
import re, sys
from collections import defaultdict
from statistics import median

src, runs = sys.argv[1], int(sys.argv[2])
# Chaque ligne : "[myc-vm] phase <nom>: X ms (total Y ms)" — on prend le
# delta X (phase). Les sondes marker/reboot sont des lignes dédiées.
pat = re.compile(r"\[myc-vm\] phase ([^:]+): (\d+) ms")
marker_pat = re.compile(r"exit marker observed (\d+) ms")
exit_pat = re.compile(r"VM process exited (\d+) ms")
phases = defaultdict(list)
markers, exits = [], []
for line in open(src, encoding="utf-8", errors="replace"):
    m = pat.search(line)
    if m:
        phases[m.group(1)].append(int(m.group(2)))
        continue
    m = marker_pat.search(line)
    if m:
        markers.append(int(m.group(1)))
        continue
    m = exit_pat.search(line)
    if m:
        exits.append(int(m.group(1)))

print(f"décomposition e2e warm (n={runs}, médiane ms par phase) :")
total = 0.0
for name in phases:
    vals = sorted(phases[name])
    med = median(vals)
    total += med
    p95 = vals[max(0, int(0.95 * len(vals)) - 1)]
    print(f"  {name:55s}: median={med:6.1f}  p95={p95:6.1f}")
if markers:
    ms = sorted(markers)
    print(f"  sonde: marker de sortie observé après resume      : median={median(ms):6.1f}  p95={ms[max(0,int(0.95*len(ms))-1)]:6.1f}  (n={len(ms)})")
if exits:
    es = sorted(exits)
    print(f"  sonde: mort du process VM après le marker         : median={median(es):6.1f}  p95={es[max(0,int(0.95*len(es))-1)]:6.1f}  (n={len(es)})")
print(f"  somme des médianes                                : {total:.0f} ms")
PYEOF
cat "$RESULTS/decompose.txt"
