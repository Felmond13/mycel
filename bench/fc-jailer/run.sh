#!/usr/bin/env bash
# bench/fc-jailer — MESURE RÉELLE du durcissement Firecracker par le jailer
# officiel (MYCEL_FC_JAILER=1, crates/myc-backend/src/firecracker.rs).
#
# Question posée : que COÛTE l'isolement supplémentaire (chroot + pivot_root,
# mount namespace, drop uid/gid 20000, /dev recréé, seccomp) sur le chemin
# snapshot Firecracker ?
#
#   (a) RESTORE JAILÉ  : MYCEL_FC_JAILER=1, snapshot chaud (n runs) ;
#   (b) RESTORE DIRECT : mode défaut, snapshot chaud (n runs) ;
#   (c) FROID JAILÉ    : cache vidé avant chaque run — l'overhead du jailer
#                        noyé dans un boot complet (contexte) ;
#   (d) REPLI CH       : cloud-hypervisor, cache coupé (contexte).
#
# LOI N°1 : hyperfine, n>=30, médiane + p95, aucun simulacre. Chaque run
# VÉRIFIE la sortie (`ok`) et le code de retour. Les caches jailed/direct
# sont séparés (l'empreinte jailer=1 les distingue de toute façon — la
# vmstate embarque les chemins de disques tels que la VM les voit :
# /run/... jailé vs ~/.mycel/... direct).
#
# ROOT requis (le mode jailé exige chroot/mknod/possession du tap) :
#   sudo RUNS=30 bash bench/fc-jailer/run.sh
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
RESULTS="$HERE/results"
MYC=${MYC:-$REPO/target/debug/myc}
IMG=${IMG:-alpine:3.20}
RUNS=${RUNS:-30}
SNAP_JAILED="$HERE/.snapcache-jailed"
SNAP_DIRECT="$HERE/.snapcache-direct"

fail() { echo "[fc-jailer] ERREUR : $*" >&2; exit 2; }

[ "$(id -u)" -eq 0 ] || fail "le scénario jailé exige root (chroot, mknod, tap)"
[ -x "$MYC" ] || fail "myc manquant : $MYC (cargo build -p myc-cli)"
command -v hyperfine >/dev/null 2>&1 || fail "hyperfine absent (apt/cargo install hyperfine)"
command -v python3 >/dev/null 2>&1 || fail "python3 absent"
JAILER_BIN="${MYCEL_FC_JAILER_BIN:-$HOME/.mycel/vm-assets/jailer}"
[ -x "$JAILER_BIN" ] || fail "jailer absent : $JAILER_BIN (lancé une fois avec MYCEL_FC_JAILER=1)"

mkdir -p "$RESULTS"
rm -rf "${SNAP_JAILED:?}" "${SNAP_DIRECT:?}"

# ---------------------------------------------------------------- environnement
{
  date -u
  uname -a
  grep -m1 'model name' /proc/cpuinfo || true
  "$MYC" --version 2>/dev/null || true
  FC="$HOME/.mycel/vm-assets/firecracker"
  [ -x "$FC" ] && "$FC" --version 2>/dev/null | head -1 || true
  "$JAILER_BIN" --version 2>/dev/null | head -1 || true
  echo "runs=$RUNS img=$IMG myc=$MYC"
  git -C "$REPO" rev-parse HEAD 2>/dev/null || true
} > "$RESULTS/env.txt" 2>&1

# ------------------------------------------------------------- (a) restore jailé
echo "=== (a) RESTORE JAILÉ — MYCEL_FC_JAILER=1, snapshot chaud (n=$RUNS) ==="
"$HERE/one-run-jailed.sh" || fail "le run de réchauffage jailé a échoué"
hyperfine --runs "$RUNS" \
  --style basic \
  --command-name "warm-jailed: FC restore via jailer" \
  --export-json "$RESULTS/warm-jailed.json" \
  "$HERE/one-run-jailed.sh"

# ------------------------------------------------------------ (b) restore direct
echo "=== (b) RESTORE DIRECT — mode défaut, snapshot chaud (n=$RUNS) ==="
"$HERE/one-run.sh" || fail "le run de réchauffage direct a échoué"
hyperfine --runs "$RUNS" \
  --style basic \
  --command-name "warm-direct: FC restore sans jailer" \
  --export-json "$RESULTS/warm-direct.json" \
  "$HERE/one-run.sh"

# ---------------------------------------------------------------- (c) froid jailé
echo "=== (c) BOOT FROID JAILÉ — cache vidé avant chaque run (n=$RUNS) ==="
hyperfine --runs "$RUNS" \
  --prepare "rm -rf '${SNAP_JAILED:?}'" \
  --style basic \
  --command-name "cold-jailed: FC boot + snapshot via jailer" \
  --export-json "$RESULTS/cold-jailed.json" \
  "$HERE/one-run-jailed.sh"

# ----------------------------------------------------------- (d) repli CH (contexte)
echo "=== (d) REPLI Cloud Hypervisor (MYCEL_SNAPSHOT_CACHE=0, n=$RUNS) ==="
CH_ONE="$HERE/.one-run-ch.sh"
cat > "$CH_ONE" <<'CHSH'
#!/usr/bin/env bash
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
MYC=${MYC:-$(cd "$HERE/../.." && pwd)/target/debug/myc}
IMG=${IMG:-alpine:3.20}
out=$(MYCEL_SNAPSHOT_CACHE=0 MYCEL_SNAPSHOT_DIR="$HERE/.snapcache-ch" \
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
    result = data["results"][0]
    if "times" in result:
        times = sorted(result["times"])
    else:
        times = sorted(m["time_wall_clock"]["value"] for m in result["measurements"])
    n = len(times)
    median = times[n // 2] if n % 2 else (times[n // 2 - 1] + times[n // 2]) / 2
    p95 = times[max(0, math.ceil(0.95 * n) - 1)]
    return n, median, p95

R = Path(sys.argv[1])
nj, med_j, p95_j = stats(R / "warm-jailed.json")
nd, med_d, p95_d = stats(R / "warm-direct.json")
print(f"RESTORE JAILÉ  (MYCEL_FC_JAILER=1) : n={nj} median={med_j*1000:.1f} ms  p95={p95_j*1000:.1f} ms")
print(f"RESTORE DIRECT (défaut)            : n={nd} median={med_d*1000:.1f} ms  p95={p95_d*1000:.1f} ms")
print(f"overhead jailer                    : médiane +{(med_j-med_d)*1000:.1f} ms  p95 +{(p95_j-p95_d)*1000:.1f} ms"
      f"  (x{med_j/med_d:.2f})")
for name, f in (("FROID JAILÉ (boot+snapshot)", "cold-jailed.json"),):
    try:
        n, m, p = stats(R / f)
        print(f"{name:34s} : n={n} median={m*1000:.1f} ms  p95={p*1000:.1f} ms")
    except FileNotFoundError:
        pass
try:
    nh, med_h, p95_h = stats(R / "ch.json")
    print(f"{'REPLI CH (voie B, cache off)':34s} : n={nh} median={med_h*1000:.1f} ms  p95={p95_h*1000:.1f} ms")
    print(f"restore jailé vs repli CH          : x{med_h/med_j:.1f} (gain médian {(med_h-med_j)*1000:.0f} ms)")
except FileNotFoundError:
    pass
PYEOF
cat "$RESULTS/verdict.txt"
rm -f "$HERE/.one-run-ch.sh"
rm -rf "${SNAP_JAILED:?}" "${SNAP_DIRECT:?}" "$HERE/.snapcache-ch"
echo "résultats : $RESULTS/{warm-jailed,warm-direct,cold-jailed,ch}.json, verdict.txt, env.txt"
