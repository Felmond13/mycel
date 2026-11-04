#!/usr/bin/env bash
# bench/snapshot-accel — MESURE RÉELLE de l'accélérateur snapshot Firecracker
# du moteur kvm-microvm (crates/myc-backend/src/firecracker.rs).
#
#   (a) FROID  : cache d'instantanés vidé avant CHAQUE run — materialize +
#               mkfs.ext4 + boot kernel Firecracker + création du snapshot
#               + exécution (le vrai coût d'un premier lancement) ;
#   (b) CHAUD  : snapshot présent — restore Firecracker (zéro init kernel)
#               + exécution ;
#   (c) REPLI  : Cloud Hypervisor voie B, cache coupé (contexte).
#
# Même commande, même environnement, n>=30 runs chacun (LOI N°1 : hyperfine,
# médiane + p95, aucun simulacre). Chaque run (one-run.sh) VÉRIFIE la sortie
# (`ok`) et le code de retour : un run silencieusement cassé échoue le bench.
# Sans ports publiés -> aucun besoin de root.
#
#   bash bench/snapshot-accel/run.sh              # n=30 par défaut
#   RUNS=50 bash bench/snapshot-accel/run.sh
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
RESULTS="$HERE/results"
ONE="$HERE/one-run.sh"
MYC=${MYC:-$REPO/target/debug/myc}
IMG=${IMG:-alpine:3.20}
RUNS=${RUNS:-30}
SNAPCACHE="$HERE/.snapcache"

fail() { echo "[snap-accel] ERREUR : $*" >&2; exit 2; }

[ -x "$MYC" ] || fail "myc manquant : $MYC (cargo build -p myc-cli)"
[ -x "$ONE" ] || fail "helper manquant : $ONE"
command -v hyperfine >/dev/null 2>&1 || fail "hyperfine absent (apt/cargo install hyperfine)"
command -v python3 >/dev/null 2>&1 || fail "python3 absent"

mkdir -p "$RESULTS" "$SNAPCACHE"
rm -rf "${SNAPCACHE:?}"/*

# ---------------------------------------------------------------- environnement
{
  date -u
  uname -a
  grep -m1 'model name' /proc/cpuinfo || true
  "$MYC" --version 2>/dev/null || true
  FC="$HOME/.mycel/vm-assets/firecracker"
  [ -x "$FC" ] && "$FC" --version 2>/dev/null | head -1 || true
  echo "runs=$RUNS img=$IMG myc=$MYC"
  git -C "$REPO" rev-parse HEAD 2>/dev/null || true
} > "$RESULTS/env.txt" 2>&1

# ---------------------------------------------------------------------- (a) froid
echo "=== (a) BOOT FROID — cache vidé avant chaque run (n=$RUNS) ==="
# --prepare (et PAS --setup) : avant CHAQUE run — sinon seul le premier est
# froid et tout le reste mesure des restores.
hyperfine --runs "$RUNS" \
  --prepare "rm -rf '${SNAPCACHE:?}'/*" \
  --style basic \
  --command-name "cold: FC boot + snapshot create" \
  --export-json "$RESULTS/cold.json" \
  "$ONE"

# ---------------------------------------------------------------------- (b) chaud
echo "=== (b) RESTORE — snapshot présent (n=$RUNS) ==="
# Le dernier run froid a laissé le snapshot en place ; un run de réchauffage
# vérifie qu'il est bien restaurable avant de mesurer.
"$ONE" || fail "le run de réchauffage a échoué (cache cassé ?)"
hyperfine --runs "$RUNS" \
  --style basic \
  --command-name "warm: FC snapshot restore" \
  --export-json "$RESULTS/warm.json" \
  "$ONE"

# --------------------------------------------------------- (c) repli CH (contexte)
# Le chemin de repli (Cloud Hypervisor voie B, cache coupé) — pour situer
# les deux nombres ci-dessus ; même commande, même image.
echo "=== (c) REPLI Cloud Hypervisor (MYCEL_SNAPSHOT_CACHE=0, n=$RUNS) ==="
CH_ONE="$HERE/.one-run-ch.sh" # à côté de one-run.sh : même résolution du repo
cat > "$CH_ONE" <<'CHSH'
#!/usr/bin/env bash
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
MYC=${MYC:-$(cd "$HERE/../.." && pwd)/target/debug/myc}
IMG=${IMG:-alpine:3.20}
out=$(MYCEL_SNAPSHOT_CACHE=0 MYCEL_SNAPSHOT_DIR="$HERE/.snapcache" \
  "$MYC" run --engine kvm-microvm "$IMG" -- /bin/echo ok 2>/dev/null)
code=$?
if [ "$code" -ne 0 ] || [ "$out" != "ok" ]; then
  echo "run invalide (code=$code sortie='$out')" >&2
  exit 1
fi
CHSH
chmod +x "$CH_ONE"
hyperfine --runs "$RUNS" \
  --style basic \
  --command-name "ch: cloud-hypervisor voie B" \
  --export-json "$RESULTS/ch.json" \
  "$CH_ONE"

# -------------------------------------------------------------------- statistiques
python3 - "$RESULTS" > "$RESULTS/verdict.txt" <<'PYEOF'
import json, sys, math
from pathlib import Path

def stats(path):
    data = json.loads(Path(path).read_text())
    # hyperfine 1.x : results[].times ; hyperfine 2.x : measurements
    result = data["results"][0]
    if "times" in result:
        times = sorted(result["times"])
    else:
        times = sorted(m["time_wall_clock"]["value"] for m in result["measurements"])
    n = len(times)
    median = times[n // 2] if n % 2 else (times[n // 2 - 1] + times[n // 2]) / 2
    p95 = times[max(0, math.ceil(0.95 * n) - 1)]
    return n, median, p95

nc, med_c, p95_c = stats(sys.argv[1] + "/cold.json")
nw, med_w, p95_w = stats(sys.argv[1] + "/warm.json")
print(f"boot FROID  (FC boot + snapshot) : n={nc} median={med_c*1000:.1f} ms  p95={p95_c*1000:.1f} ms")
print(f"RESTORE     (FC snapshot)        : n={nw} median={med_w*1000:.1f} ms  p95={p95_w*1000:.1f} ms")
print(f"accélération médiane             : x{med_c/med_w:.1f} (gain médian {(med_c-med_w)*1000:.0f} ms)")
try:
    nh, med_h, p95_h = stats(sys.argv[1] + "/ch.json")
    print(f"repli CH   (voie B, cache off)   : n={nh} median={med_h*1000:.1f} ms  p95={p95_h*1000:.1f} ms")
    print(f"restore vs repli CH              : x{med_h/med_w:.1f} (gain médian {(med_h-med_w)*1000:.0f} ms)")
except FileNotFoundError:
    pass
PYEOF
cat "$RESULTS/verdict.txt"
rm -f "$HERE/.one-run-ch.sh"
echo "résultats : $RESULTS/{cold,warm,ch}.json, verdict.txt, env.txt"
