#!/usr/bin/env bash
# bench/warm-pool/ab-fix.sh — A/B INTERCALÉ baseline vs correctif early-exit.
#
# Mesure avant/après du correctif livré avec le verdict warm-pool
# (docs/benchmarks/warm-pool-verdict.md) : retour dès le marker de sortie
# MYCE + SIGKILL du VMM (au lieu d'attendre le reboot invité + la sortie
# du process FC) + purge iptables conditionnelle au teardown.
#
# Les deux binaires sont mesurés EN ALTERNANCE STRICTE (A,B,A,B,...) pour
# que la dérive de charge de la machine (partagée) frappe les deux séries
# symétriquement. Chaque run vérifie sortie ET code (LOI N°1).
#
#   sudo BIN_BASELINE=... BIN_FIXED=... bash bench/warm-pool/ab-fix.sh
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
RESULTS="$HERE/results/fix-earlyexit"
BASE=${BIN_BASELINE:-/home/nboukadoum-ext/build/bin/myc-baseline}
FIXED=${BIN_FIXED:-/home/nboukadoum-ext/build/bin/myc-fixed}
IMG=${IMG:-alpine:3.20}
N=${N:-30}
SNAPCACHE="$HERE/.snapcache-ab"

fail() { echo "[warm-pool] ERREUR : $*" >&2; exit 2; }
[ -x "$BASE" ] || fail "binaire baseline manquant : $BASE"
[ -x "$FIXED" ] || fail "binaire fixed manquant : $FIXED"

export MYCEL_SNAPSHOT_DIR="$SNAPCACHE"
mkdir -p "$RESULTS" "$SNAPCACHE"

one() { # $1 = binaire, $2 = fichier de temps
  local bin=$1 out code t0 t1
  t0=$(date +%s%N)
  out=$("$bin" run --engine kvm-microvm "$IMG" -- /bin/echo ok 2>/dev/null)
  code=$?
  t1=$(date +%s%N)
  if [ "$code" -ne 0 ] || [ "$out" != "ok" ]; then
    fail "run invalide ($bin, code=$code sortie='$out')"
  fi
  echo "$(( (t1 - t0) / 1000000 )) $(awk '{print $1}' /proc/loadavg)" >> "$2"
}

# Réchauffage : crée le snapshot (les DEUX binaires partagent le même
# format de snapshot — même empreinte) et vérifie les deux chemins.
one "$BASE" /dev/null
one "$FIXED" /dev/null

: > "$RESULTS/baseline.times"
: > "$RESULTS/fixed.times"
for _ in $(seq "$N"); do
  one "$BASE" "$RESULTS/baseline.times"
  one "$FIXED" "$RESULTS/fixed.times"
done

python3 - "$RESULTS" "$N" > "$RESULTS/verdict.txt" <<'PYEOF'
import sys, math
from pathlib import Path
from statistics import median

res, n = sys.argv[1], int(sys.argv[2])

def stats(path):
    rows = [l.split() for l in Path(path).read_text().splitlines() if l.strip()]
    times = sorted(int(r[0]) for r in rows)
    loads = [float(r[1]) for r in rows]
    return times, median(times), times[max(0, math.ceil(0.95 * len(times)) - 1)], loads

bt, bm, bp95, bl = stats(f"{res}/baseline.times")
ft, fm, fp95, fl = stats(f"{res}/fixed.times")
print(f"baseline (attente reboot invité) : n={len(bt)} median={bm:7.1f} ms  p95={bp95:7.1f} ms  (charge médiane {median(bl):.1f})")
print(f"fix early-exit (marker + kill)   : n={len(ft)} median={fm:7.1f} ms  p95={fp95:7.1f} ms  (charge médiane {median(fl):.1f})")
print(f"gain médian {bm - fm:.1f} ms ({(bm - fm) / bm * 100:.0f}%), gain p95 {bp95 - fp95:.1f} ms")
PYEOF
cat "$RESULTS/verdict.txt"
