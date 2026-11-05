#!/usr/bin/env bash
# bench/warm-pool/baseline.sh — MESURE DE BASE pour le verdict warm-pool.
#
# Référence à battre : le chemin PRODUIT actuel (accélérateur snapshot
# Firecracker, restore à chaud) en ROOT, e2e complet `myc run alpine --
# /bin/echo ok`, hyperfine n>=30, médiane + p95. Plus deux planchers de
# contexte : le démarrage du CLI seul (`myc --version`) et la voie sans
# réseau (`--net none`, pour isoler ce que le réseau hôte ajoute).
#
#   sudo bash bench/warm-pool/baseline.sh            # n=30 par défaut
#   RUNS=50 sudo bash bench/warm-pool/baseline.sh
#
# Prérequis : /dev/kvm, hyperfine, python3, build release (`cargo build
# --release -p myc-cli`), alpine:3.20 dans MYCEL_STORE.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
RESULTS="$HERE/results/baseline"
ONE="$HERE/e2e-one-run.sh"
MYC=${MYC:-$REPO/target/release/myc}
IMG=${IMG:-alpine:3.20}
RUNS=${RUNS:-30}
SNAPCACHE="$HERE/.snapcache-base"

fail() { echo "[warm-pool] ERREUR : $*" >&2; exit 2; }

[ -x "$MYC" ] || fail "myc manquant : $MYC (cargo build --release -p myc-cli)"
[ -x "$ONE" ] || fail "helper manquant : $ONE"
command -v hyperfine >/dev/null 2>&1 || fail "hyperfine absent"
command -v python3 >/dev/null 2>&1 || fail "python3 absent"

export MYCEL_SNAPSHOT_DIR="$SNAPCACHE"
mkdir -p "$RESULTS" "$SNAPCACHE"

# ---------------------------------------------------------------- environnement
{
  date -u
  uname -a
  grep -m1 'model name' /proc/cpuinfo || true
  "$MYC" --version 2>/dev/null || true
  FC="$HOME/.mycel/vm-assets/firecracker"
  [ -x "$FC" ] && "$FC" --version 2>/dev/null | head -1 || true
  echo "runs=$RUNS img=$IMG myc=$MYC user=$(id -un)"
  git -C "$REPO" rev-parse HEAD 2>/dev/null || true
} > "$RESULTS/env.txt" 2>&1

# --------------------------------------------------------------- (0) plancher CLI
echo "=== (0) PLANCHER CLI — myc --version (n=$RUNS) ==="
hyperfine --runs "$RUNS" --style basic \
  --command-name "cli-floor: myc --version" \
  --export-json "$RESULTS/cli-floor.json" \
  "$MYC --version"

# --------------------------------------------------------- (1) e2e ROOT réseau
echo "=== (1) E2E ROOT — snapshot warm, réseau complet (n=$RUNS) ==="
# Un run de réchauffage crée le snapshot (froid) et vérifie qu'il restore.
"$ONE" || fail "le run de réchauffage a échoué"
hyperfine --runs "$RUNS" --style basic \
  --command-name "warm-root: e2e snapshot restore (root, net)" \
  --export-json "$RESULTS/warm-root.json" \
  "$ONE"

# ------------------------------------------------------- (2) e2e ROOT sans réseau
echo "=== (2) E2E ROOT — snapshot warm, --net none (n=$RUNS) ==="
NONET="$HERE/.e2e-one-run-nonet.sh"
cat > "$NONET" <<'EOSH'
#!/usr/bin/env bash
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
MYC=${MYC:-$(cd "$HERE/../.." && pwd)/target/release/myc}
IMG=${IMG:-alpine:3.20}
out=$("$MYC" run --engine kvm-microvm --net none "$IMG" -- /bin/echo ok 2>/dev/null)
code=$?
if [ "$code" -ne 0 ] || [ "$out" != "ok" ]; then
  echo "run invalide (code=$code sortie='$out')" >&2
  exit 1
fi
exit 0
EOSH
chmod +x "$NONET"
MYC="$MYC" "$NONET" || fail "le run de réchauffage --net none a échoué"
hyperfine --runs "$RUNS" --style basic \
  --command-name "warm-nonet: e2e snapshot restore (root, --net none)" \
  --export-json "$RESULTS/warm-nonet.json" \
  "$NONET"
rm -f "$NONET"

# -------------------------------------------------------------------- statistiques
python3 - "$RESULTS" > "$RESULTS/verdict.txt" <<'PYEOF'
import json, sys, math
from pathlib import Path

def stats(path):
    data = json.loads(Path(path).read_text())
    result = data["results"][0]
    if "times" in result:
        times = sorted(result["times"])
    else:
        times = sorted(m["time_wall_clock"]["value"] for m in result["measurements"])
    n = len(times)
    median = times[n // 2] if n % 2 else (times[n // 2 - 1] + times[n // 2]) / 2
    p95 = times[max(0, math.ceil(0.95 * n) - 1)]
    return n, median, p95

res = sys.argv[1]
for name, label in [
    ("cli-floor.json", "plancher CLI (myc --version)"),
    ("warm-root.json", "e2e ROOT snapshot warm (réseau)"),
    ("warm-nonet.json", "e2e ROOT snapshot warm (--net none)"),
]:
    try:
        n, med, p95 = stats(f"{res}/{name}")
        print(f"{label:42s}: n={n} median={med*1000:7.1f} ms  p95={p95*1000:7.1f} ms")
    except FileNotFoundError:
        pass
PYEOF
cat "$RESULTS/verdict.txt"
echo "résultats : $RESULTS/{cli-floor,warm-root,warm-nonet}.json, verdict.txt, env.txt"
