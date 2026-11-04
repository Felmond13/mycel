#!/usr/bin/env bash
# KIT D'EXPÉRIENCE — snapshot/restore Cloud Hypervisor v54 sur KVM NATIF.
#
# Décide, chiffres à l'appui, si le restore microVM tient la promesse
# sub-200 ms sur un vrai KVM (le KVM de WSL2 ne préserve pas l'état vCPU :
# invité inert — docs/isolation-engines.md §Snapshot). Auto-suffisant sur
# toute machine Linux + /dev/kvm : télécharge CH v54.0 si absent, réutilise
# ou construit le noyau/initramfs de bench/microvm.
#
# Phases (PHASES="baseline,snapshot,restore") :
#   baseline : n=RUNS boots à froid, T0 (lancement) -> T1 (1er octet 200)
#   snapshot : 1 boot, pause, snapshot (destination = répertoire), durée
#              de création consignée
#   restore  : matrice {copy,copyonwrite,ondemand} x {prefault=off,on},
#              n=RUNS restores par case, T0->T1 + VIVACITÉ (battement de
#              cœur console toutes les 200 ms) + nettoyage VÉRIFIÉ par run
#   verdict  : tableaux + GO/NO-GO (GO si une case donne >= 28/30 restores
#              < 200 ms avec invité vivant)
#
# Protocole complet : docs/benchmarks/snapshot-native-kit.md
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
MICROVM="$HERE/../microvm"
ASSETS="$MICROVM/assets"
STATS="$MICROVM/stats.sh"
RESULTS="$HERE/results"

CH_VERSION="v54.0"
CH_BASE_URL="https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/$CH_VERSION"
CH_BIN=${CH_BIN:-/usr/local/bin/cloud-hypervisor}
CH_REMOTE=${CH_REMOTE:-/usr/local/bin/ch-remote}

KERNEL="$ASSETS/vmlinux"
INITRAMFS="$ASSETS/rootfs.cpio"

# Topologie figée — identique à bench/microvm et crates/myc-backend kvm.rs.
TAP="myc0"
GUEST_IP="192.168.77.2"
GUEST_MAC="52:54:00:77:00:02"
HOST_IP="192.168.77.1"
NET_ARG="tap=$TAP,mac=$GUEST_MAC,ip=$HOST_IP,mask=255.255.255.0"
URL="http://$GUEST_IP:8080/"
MEM_SIZE=${MEM_SIZE:-512M}

RUNS=${RUNS:-30}
WARMUPS=${WARMUPS:-5}
RESTORE_RUNS=${RESTORE_RUNS:-$RUNS}
RESTORE_WARMUPS=${RESTORE_WARMUPS:-2}
PHASES=${PHASES:-baseline,snapshot,restore}
BOOT_TIMEOUT_MS=${BOOT_TIMEOUT_MS:-10000}
RESTORE_TIMEOUT_MS=${RESTORE_TIMEOUT_MS:-15000}
INERT_ABORT=${INERT_ABORT:-3}
GO_MS=${GO_MS:-200}
GO_MIN_OK=$(( RESTORE_RUNS * 14 / 15 ))   # 28/30 à RESTORE_RUNS=30
BOOT_MEDIAN=""                    # rempli par phase_baseline, sert au speedup

WORKROOT=""
SNAPDIR=""
VM_PID=""
VM_SOCK=""
PIDS_ALL=""

log()  { printf '\n=== %s ===\n' "$*"; }
info() { printf '[kit] %s\n' "$*"; }
fail() { printf '[kit] ERREUR : %s\n' "$*" >&2; finish 2; }

# Sortie UNIQUE du script : désarme le trap PUIS nettoie PUIS exit — ainsi
# le code de sortie (0/1/2/3) passe toujours, quelle que soit la façon dont
# la version de bash traite le statut à travers un trap EXIT ; le trap ne
# reste qu'un filet de sécurité anti-fuite pour les sorties imprévues.
finish() {
  local rc=$1
  trap - EXIT INT TERM
  on_exit
  exit "$rc"
}

on_exit() {
  [ -n "$VM_PID" ] && { kill -TERM "$VM_PID" 2>/dev/null || true; wait "$VM_PID" 2>/dev/null || true; }
  for p in $PIDS_ALL; do kill -KILL "$p" 2>/dev/null || true; done
  ip link del "$TAP" 2>/dev/null || true
  [ -n "$WORKROOT" ] && rm -rf "$WORKROOT"
  return 0
}
trap on_exit EXIT INT TERM

# ---------------------------------------------------------------- prérequis

if [ "$(id -u)" -ne 0 ]; then
  if command -v sudo >/dev/null 2>&1; then
    info "root requis (tap KVM) -> re-exec via sudo"
    exec sudo -E bash "$0" "$@"
  fi
  fail "doit tourner en root : le tap KVM et /usr/local/bin l'exigent (comme le daemon Docker du bench microvm)"
fi

[ -e /dev/kvm ] || fail "/dev/kvm absent — ce kit tranche le cas KVM NATIF, il ne s'exécute pas sans KVM"
[ -w /dev/kvm ] || fail "/dev/kvm non inscriptible (groupe kvm ?)"

for c in curl ip awk grep pgrep; do
  command -v "$c" >/dev/null 2>&1 || fail "commande absente : $c"
done
[ -x "$STATS" ] || fail "$STATS absent — le kit voyage avec son répertoire frère bench/microvm"

# --------------------------------------------------- cloud-hypervisor v54.0

ch_version_of() { "$1" --version 2>/dev/null || true; }

install_ch() {
  local dest="$1" url="$2"
  info "téléchargement $url"
  curl -fsSL -o "$dest" "$url" || fail "téléchargement impossible : $url"
  chmod 755 "$dest"
}

need_ch() {
  local sys="$1" url="$2" ver
  if [ -x "$sys" ] && ch_version_of "$sys" | grep -q "v54\.0"; then
    printf '%s' "$sys"
    return
  fi
  if [ -e "$sys" ] || [ -x "$sys" ]; then
    info "$sys présent mais pas v54.0 ($(ch_version_of "$sys")) -> version du kit utilisée"
  fi
  local local_bin="$HERE/.tools/$(basename "$url")"
  if [ ! -x "$local_bin" ] || ! ch_version_of "$local_bin" | grep -q "v54\.0"; then
    mkdir -p "$HERE/.tools"
    install_ch "$local_bin" "$url"
  fi
  printf '%s' "$local_bin"
}

log "Cloud Hypervisor $CH_VERSION"
CH_BIN=$(need_ch "$CH_BIN" "$CH_BASE_URL/cloud-hypervisor-static")
CH_REMOTE=$(need_ch "$CH_REMOTE" "$CH_BASE_URL/ch-remote-static")
info "VMM  : $CH_BIN ($(ch_version_of "$CH_BIN"))"
info "ctrl : $CH_REMOTE"

# ------------------------------------------------------------- assets invité

if [ ! -f "$KERNEL" ] || [ ! -f "$INITRAMFS" ]; then
  log "assets bench/microvm absents -> construction (long : noyau 6.12)"
  [ -x "$MICROVM/build_kernel.sh" ]   || fail "$MICROVM/build_kernel.sh introuvable"
  [ -x "$MICROVM/mkinitramfs.sh" ] || fail "$MICROVM/mkinitramfs.sh introuvable"
  "$MICROVM/build_kernel.sh"
  "$MICROVM/mkinitramfs.sh"
fi
[ -f "$KERNEL" ]   || fail "noyau manquant : $KERNEL"
[ -f "$INITRAMFS" ] || fail "initramfs manquant : $INITRAMFS"

# ------------------------------------------------------------------ espaces

mkdir -p "$RESULTS"
WORKROOT=$(mktemp -d /tmp/myc-snap-native.XXXXXX)
SNAPDIR=${SNAPDIR:-$WORKROOT/snapshot}   # overridable : disque réel au lieu de /tmp
SNAP_CONSOLE="$WORKROOT/snapvm-console.log"   # console de la VM snapshotée (remaplie par phase_snapshot)
mkdir -p "$SNAPDIR"

avail_mb=$(df -Pm /tmp | awk 'NR==2 {print $4}')
if [ "${avail_mb:-0}" -lt 1500 ]; then
  info "AVERTISSEMENT : /tmp n'a que ${avail_mb} Mo libres ; le snapshot mémoire ($MEM_SIZE) peut ne pas tenir — SNAPDIR=/chemin/disque pour changer"
fi

{
  echo "=== Mycel snapshot-native kit — environnement ==="
  date -u
  uname -a
  uptime
  lscpu | sed -n '1,12p'
  free -h
  echo "--- KVM ---"
  ls -l /dev/kvm
  grep -E 'vmx|svm' /proc/cpuinfo | head -1 | tr ' ' '\n' | grep -E '^(vmx|svm)$' || true
  if [ -e /dev/userfaultfd ]; then echo "userfaultfd: /dev/userfaultfd présent"; else echo "userfaultfd: /dev/userfaultfd ABSENT"; fi
  echo "vm.unprivileged_userfaultfd=$(cat /proc/sys/vm/unprivileged_userfaultfd 2>/dev/null || echo '?')"
  echo "--- versions ---"
  "$CH_BIN" --version
  hyperfine --version 2>/dev/null || true
  echo "--- SHA-256 ---"
  sha256sum "$KERNEL" "$INITRAMFS"
  cat "$ASSETS/vmlinux.version" 2>/dev/null || true
  git -C "$HERE/../.." rev-parse HEAD 2>/dev/null || true
} > "$RESULTS/env.txt" 2>&1

# ------------------------------------------------------------ briques VM
# Miroir exact de crates/myc-backend/src/kvm.rs (flags, ARP pré-seedé,
# anti-fuite), sans dépendre du build Rust : le kit tourne sur n'importe
# quelle Linux + KVM.

vm_launch() {           # $1 = rundir ; démarre un boot À FROID
  local d="$1"
  ip link del "$TAP" >/dev/null 2>&1 || true    # un tap résiduel détourne le routage
  VM_SOCK="$d/api.sock"; VM_PID=""
  "$CH_BIN" \
    --api-socket "$VM_SOCK" \
    --console "file=$d/console.log" \
    --kernel "$KERNEL" \
    --cmdline "console=hvc0 quiet" \
    --initramfs "$INITRAMFS" \
    --cpus "boot=1" \
    --memory "size=$MEM_SIZE" \
    --net "$NET_ARG" \
    >"$d/ch.log" 2>&1 &
  VM_PID=$!
  PIDS_ALL="$PIDS_ALL $VM_PID"
  arp_seed
}

vm_restore() {          # $1 = rundir, $2 = mode, $3 = prefault ; démarre un --restore
  local d="$1" mode="$2" prefault="$3"
  ip link del "$TAP" >/dev/null 2>&1 || true
  VM_SOCK="$d/api.sock"; VM_PID=""
  # resume=true explicite : la VM reprend dès la fin du chargement mémoire.
  # --kernel est exigé par la CLI v54 même en restore ; le net (tap/mac/ip)
  # vient du config.json du snapshot — pas de --net ici.
  "$CH_BIN" \
    --api-socket "$VM_SOCK" \
    --console "file=$d/console.log" \
    --kernel "$KERNEL" \
    --restore "source_url=file://$SNAPDIR,memory_restore_mode=$mode,prefault=$prefault,resume=true" \
    >"$d/ch.log" 2>&1 &
  VM_PID=$!
  PIDS_ALL="$PIDS_ALL $VM_PID"
  arp_seed
}

arp_seed() {            # entrée ARP permanente dès que le tap existe (cf. kvm.rs :
                        # sans elle, le p95 part à ~1 s — résolution ARP)
  local i
  for i in $(seq 1 100); do
    ip link show "$TAP" >/dev/null 2>&1 || { sleep 0.01; continue; }
    ip neigh replace "$GUEST_IP" lladdr "$GUEST_MAC" dev "$TAP" nud permanent >/dev/null 2>&1 || true
    return
  done
}

vm_cleanup() {          # arrêt du VMM (sans coopération invité : la VM est
                        # souvent en pause ici), puis mort forcée si besoin
  [ -n "$VM_PID" ] || return 0
  "$CH_REMOTE" --api-socket "$VM_SOCK" shutdown-vmm >/dev/null 2>&1 || true
  local i
  for i in $(seq 1 100); do
    kill -0 "$VM_PID" 2>/dev/null || break
    sleep 0.05
  done
  kill -KILL "$VM_PID" 2>/dev/null || true
  wait "$VM_PID" 2>/dev/null || true
  PIDS_ALL=$(printf '%s' "$PIDS_ALL" | tr ' ' '\n' | grep -vx "$VM_PID" | tr '\n' ' ')
  VM_PID=""
  rm -f "$VM_SOCK" 2>/dev/null || true
}

residue_check() {       # tout résidu fait échouer le run (discipline microvm)
  ip link show "$TAP" >/dev/null 2>&1 && return 1
  pgrep -f "$CH_BIN" >/dev/null 2>&1 && return 1
  return 0
}

wait_ready() {          # $1 = timeout ms ; READY=1 + T1_NS si 200 reçu
  local deadline=$(( $(date +%s%N) + $1 * 1000000 ))
  READY=0; T1_NS=""
  while [ "$(date +%s%N)" -lt "$deadline" ]; do
    kill -0 "$VM_PID" 2>/dev/null || return 0   # VMM mort -> sortie rapide
    if curl -fsS --max-time 0.05 -o /dev/null "$URL" 2>/dev/null; then
      READY=1; T1_NS=$(date +%s%N); return 0
    fi
    sleep 0.005
  done
}

hb_count() {            # battements de cœur visibles (console du run OU celle,
  local f n=0           # morte, de la VM snapshotée : l'invité restauré vivant
  for f in "$1/console.log" "$SNAP_CONSOLE"; do   # peut écrire dans l'une des deux)
    [ -f "$f" ] && n=$(( n + $(grep -c 'benchhttp: heartbeat' "$f" || true) ))
  done
  HBC=$n
}

guest_alive() {         # ALIVE=1 si >=2 nouveaux battements pendant 1,2 s
  local d="$1" before after
  hb_count "$d"; before=$HBC
  sleep 1.2
  hb_count "$d"; after=$HBC
  if [ $(( after - before )) -ge 2 ]; then ALIVE=1; else ALIVE=0; fi
}

new_rundir() {
  RUNDIR=$(mktemp -d "$WORKROOT/run.XXXXXX")
  RUNS_MADE=$(( RUNS_MADE + 1 ))
}

RUNS_MADE=0
INERT_STREAK=0

# ============================================================== phase baseline

phase_baseline() {
  log "BASELINE — $WARMUPS échauffements + $RUNS boots à froid (T0 -> T1 : 1er octet 200)"
  local i t0
  : > "$RESULTS/boot.times"
  for i in $(seq 1 "$WARMUPS"); do
    new_rundir; vm_launch "$RUNDIR"; wait_ready "$BOOT_TIMEOUT_MS"; vm_cleanup
    residue_check || fail "échauffement boot $i : résidu (tap/VMM)"
  done
  for i in $(seq 1 "$RUNS"); do
    new_rundir
    t0=$(date +%s%N)
    vm_launch "$RUNDIR"
    wait_ready "$BOOT_TIMEOUT_MS"
    if [ "$READY" -ne 1 ]; then
      echo "run boot $i : pas de 200 en ${BOOT_TIMEOUT_MS} ms" >&2
      tail -5 "$RUNDIR/ch.log" >&2 || true
      vm_cleanup; fail "baseline : invité jamais prêt"
    fi
    echo $(( T1_NS - t0 )) >> "$RESULTS/boot.times"
    guest_alive "$RUNDIR"
    vm_cleanup
    residue_check || fail "run boot $i : résidu (tap/VMM) — fuite"
    [ "$ALIVE" -eq 1 ] || fail "run boot $i : pas de battement de cœur (détecteur de vivacité cassé)"
  done
  local s; s=$(bash "$STATS" "$RESULTS/boot.times")
  BOOT_MEDIAN=$(printf '%s' "$s" | sed -n 's/.*median_ms=\([0-9.]*\).*/\1/p')
  info "baseline : $s"
}

# ============================================================== phase snapshot

phase_snapshot() {
  log "SNAPSHOT — boot, pause, snapshot vers $SNAPDIR"
  local t0 t1 pause_ms snap_ms
  new_rundir
  # La console de LA VM snapshotée vit à part : un invité restauré vivant
  # peut y écrire selon la façon dont la config console est reprise — on
  # surveille ce fichier ET celui du run (cf. guest_alive).
  vm_launch "$RUNDIR"
  SNAP_CONSOLE="$RUNDIR/console.log"
  wait_ready "$BOOT_TIMEOUT_MS"
  [ "$READY" -eq 1 ] || { vm_cleanup; fail "snapshot : la VM de base n'a jamais répondu"; }
  sleep 1.5                                  # laisser les battements s'accumuler
  guest_alive "$RUNDIR"
  [ "$ALIVE" -eq 1 ] || { vm_cleanup; fail "snapshot : invité de base inerte avant même le snapshot"; }

  "$CH_REMOTE" --api-socket "$VM_SOCK" pause >/dev/null     # OBLIGATOIRE en v54
  t0=$(date +%s%N)
  "$CH_REMOTE" --api-socket "$VM_SOCK" snapshot "file://$SNAPDIR" \
    || { vm_cleanup; fail "échec ch-remote snapshot (VM bien en pause ?) : voir ci-dessus"; }
  t1=$(date +%s%N)
  snap_ms=$(( (t1 - t0) / 1000000 ))

  [ -f "$SNAPDIR/config.json" ] && [ -f "$SNAPDIR/state.json" ] \
    || { vm_cleanup; fail "snapshot incomplet dans $SNAPDIR : $(ls -la "$SNAPDIR" 2>/dev/null | tr '\n' ' ')"; }

  {
    echo "snapshot_dir=$SNAPDIR"
    echo "snapshot_creation_ms=$snap_ms"
    echo "memory_size=$MEM_SIZE"
    echo "files:"
    ls -la "$SNAPDIR"
    du -sh "$SNAPDIR"
  } > "$RESULTS/snapshot-info.txt"
  vm_cleanup
  residue_check || fail "snapshot : résidu (tap/VMM) après shutdown"
  info "snapshot créé en ${snap_ms} ms (détail : results/snapshot-info.txt)"
}

# =============================================================== phase restore

restore_one() {         # $1=mode $2=prefault $3=timeout_ms $4=fichier-times -> classification
  local mode="$1" prefault="$2" tmo="$3" out="$4" t0
  new_rundir
  t0=$(date +%s%N)
  vm_restore "$RUNDIR" "$mode" "$prefault"
  wait_ready "$tmo"
  if [ "$READY" -eq 1 ]; then
    guest_alive "$RUNDIR"
    if [ "$ALIVE" -eq 1 ]; then
      echo $(( T1_NS - t0 )) >> "$out"
      CLASS=ok
    else
      CLASS=netdead    # 200 reçu puis figé, ou vivacité indétectable
    fi
  else
    if ! kill -0 "$VM_PID" 2>/dev/null; then
      CLASS=died       # le VMM est mort (restore refusé, uffd absent...)
    else
      guest_alive "$RUNDIR"
      if [ "$ALIVE" -eq 1 ]; then
        CLASS=netdead  # invité vivant mais injoignable (tap/routage)
      else
        CLASS=inert    # mémoire chargée, vCPU relancé, invité figé : WSL2
      fi
    fi
  fi
  if [ "$CLASS" != "ok" ]; then
    { echo "--- run $RUNDIR (mode=$mode prefault=$prefault class=$CLASS) ---"
      echo "ch.log:"; tail -10 "$RUNDIR/ch.log" 2>/dev/null
      echo "console.log:"; tail -5 "$RUNDIR/console.log" 2>/dev/null
    } >> "$RESULTS/failures.log"
  fi
  vm_cleanup
  residue_check || { echo "run restore $mode/$prefault : RÉSIDU (tap/VMM) — fuite" >&2; CLASS=leak; }
}

phase_restore() {
  local mode prefault i uf
  uf=no
  { [ -e /dev/userfaultfd ] && uf=yes; } || true
  [ "$(cat /proc/sys/vm/unprivileged_userfaultfd 2>/dev/null || echo 0)" = "1" ] && uf=yes
  log "RESTORE MATRIX — modes x prefault, $RESTORE_WARMUPS échauffements + $RESTORE_RUNS mesures par case (userfaultfd : $uf)"

  printf '%-28s %5s %5s %6s %7s %6s %9s %8s\n' \
    "variante" "ok" "inert" "autres" "med_ms" "p95_ms" "n<${GO_MS}ms" "x_boot" \
    > "$RESULTS/matrix.txt"
  BEST_OK=-1; BEST_ID=""

  for mode in copy copyonwrite ondemand; do
    if [ "$mode" = "ondemand" ] && [ "$uf" = "no" ]; then
      info "ondemand SAUTÉ : userfaultfd indisponible (/dev/userfaultfd absent et vm.unprivileged_userfaultfd=0)"
      printf '%-28s %5s %5s %6s %7s %6s %9s %8s\n' \
        "ondemand-prefault-*" "SKIP" "-" "-" "-" "-" "-" "-" >> "$RESULTS/matrix.txt"
      continue
    fi
    for prefault in off on; do
      # prefault ne concerne que le chemin copy (cf. --help v54) : la case
      # est quand même mesurée pour documenter le comportement réel.
      local id="$mode-prefault-$prefault"
      local times="$RESULTS/restore-$id.times"
      : > "$times"
      info "variante $id — échauffements..."
      for i in $(seq 1 "$RESTORE_WARMUPS"); do
        restore_one "$mode" "$prefault" "$RESTORE_TIMEOUT_MS" /dev/null
        [ "$CLASS" = "leak" ] && fail "échauffement $id : fuite"
        if [ "$CLASS" = "inert" ]; then
          INERT_STREAK=$(( INERT_STREAK + 1 ))
          [ "$INERT_STREAK" -ge "$INERT_ABORT" ] && { inert_abort "$id (échauffement $i)"; return 3; }
        else
          INERT_STREAK=0
        fi
      done
      info "variante $id — $RESTORE_RUNS mesures..."
      local n_inert=0 n_other=0 n_ok=0
      for i in $(seq 1 "$RESTORE_RUNS"); do
        restore_one "$mode" "$prefault" "$RESTORE_TIMEOUT_MS" "$times"
        case "$CLASS" in
          ok)    n_ok=$(( n_ok + 1 )); INERT_STREAK=0 ;;
          inert) n_inert=$(( n_inert + 1 ))
                 INERT_STREAK=$(( INERT_STREAK + 1 ))
                 [ "$INERT_STREAK" -ge "$INERT_ABORT" ] && { inert_abort "$id (run $i)"; return 3; } ;;
          *)     n_other=$(( n_other + 1 )); INERT_STREAK=0 ;;
        esac
      done
      local med="-" p95="-" nfast="-" speedup="-"
      if [ -s "$times" ]; then
        local s; s=$(bash "$STATS" "$times")
        med=$(printf '%s' "$s" | sed -n 's/.*median_ms=\([0-9.]*\).*/\1/p')
        p95=$(printf '%s' "$s" | sed -n 's/.*p95_ms=\([0-9.]*\).*/\1/p')
        nfast=$(awk -v th=$(( GO_MS * 1000000 )) '$1 < th { n++ } END { print n + 0 }' "$times")
        if [ -s "$RESULTS/boot.times" ] && [ "$med" != "-" ] && [ -n "$BOOT_MEDIAN" ]; then
          speedup=$(awk -v b="$BOOT_MEDIAN" -v m="$med" 'BEGIN { if (m > 0) printf "%.1f", b / m; else print "-" }')
        fi
      fi
      printf '%-28s %5d %5d %6d %7s %6s %9s %8s\n' \
        "$id" "$n_ok" "$n_inert" "$n_other" "$med" "$p95" "$nfast" "$speedup" >> "$RESULTS/matrix.txt"
      if [ "$n_ok" -gt "$BEST_OK" ]; then BEST_OK=$n_ok; BEST_ID=$id; fi
    done
  done
}

inert_abort() {
  cat >&2 <<EOF

[kit] ============================================================
[kit]  RESTORE INERT — l'hyperviseur hôte ne préserve pas l'état
[kit]  vCPU au restore (ni timer, ni interruptions dans l'invité).
[kit]  C'est la signature mesurée sous WSL2-KVM (docs/isolation-engines.md
[kit]  §Snapshot) : le KVM imbriqué de WSL2 charge la mémoire, relance le
[kit]  vCPU, mais l'invité reste figé — quelle que soit la variante.
[kit]  Ce kit doit trancher sur KVM NATIF : ré-exécuter sur une machine
[kit]  Linux avec /dev/kvm direct (bare-metal, pas imbriqué).
[kit]  Détails : results/failures.log — matrice interrompue à : $1
[kit] ============================================================
EOF
  printf '%-28s %5s %5s %6s %7s %6s %9s %8s\n' \
    "(interrompu : restore inert)" "-" "-" "-" "-" "-" "-" "-" >> "$RESULTS/matrix.txt"
  echo "VERDICT : NO-GO — restore inert sur CET hôte ($1)" >> "$RESULTS/matrix.txt"
  RESTORE_INERT=1
  return 3
}

# =================================================================== verdict

verdict() {
  log "VERDICT"
  {
    echo "=== Mycel snapshot-native kit — verdict ($(date -u)) ==="
    echo "hôte : $(uname -r) — $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | sed 's/^ //')"
    echo ""
    echo "Baseline boot à froid :"
    bash "$STATS" "$RESULTS/boot.times" 2>/dev/null || echo "  (absente)"
    [ -f "$RESULTS/snapshot-info.txt" ] && { echo ""; cat "$RESULTS/snapshot-info.txt" | grep -E 'snapshot_creation_ms|memory_size'; }
    echo ""
    echo "Matrice restore (ok = 200 reçu ET battement récent ; n<${GO_MS}ms = restores sous le seuil) :"
    cat "$RESULTS/matrix.txt" 2>/dev/null || echo "  (absente)"
    echo ""
  } | tee "$RESULTS/verdict.txt"

  if [ "${RESTORE_INERT:-0}" = "1" ]; then
    echo "GO/NO-GO : NO-GO — restore inert sur cet hôte (voir message ci-dessus)." | tee -a "$RESULTS/verdict.txt"
    return 3
  fi
  if [ ! -f "$RESULTS/matrix.txt" ] || [ "${PHASES#*restore}" = "$PHASES" ]; then
    echo "GO/NO-GO : non évalué (phase restore non exécutée — PHASES=$PHASES)." | tee -a "$RESULTS/verdict.txt"
    return 0
  fi
  # GO si une variante donne >= GO_MIN_OK restores < GO_MS ms avec invité vivant.
  local go=0 winner=""
  while read -r id ok inert other med p95 nfast speedup; do
    case "$id" in ""|"variante"|"("*) continue ;; esac
    case "$nfast" in ""|"-") continue ;; esac
    case "$ok" in ""|"-") continue ;; esac
    if [ "$nfast" -ge "$GO_MIN_OK" ] && [ "$ok" -ge "$GO_MIN_OK" ]; then
      go=1; winner="$id (ok=$ok, n<${GO_MS}ms=$nfast, med=${med} ms)"
    fi
  done < "$RESULTS/matrix.txt"
  if [ "$go" = "1" ]; then
    echo "GO/NO-GO : GO — $winner tient >= $GO_MIN_OK/$RESTORE_RUNS restores < ${GO_MS} ms avec invité vivant." | tee -a "$RESULTS/verdict.txt"
    return 0
  else
    echo "GO/NO-GO : NO-GO — aucune variante n'atteint $GO_MIN_OK/$RESTORE_RUNS restores < ${GO_MS} ms avec invité vivant." | tee -a "$RESULTS/verdict.txt"
    return 1
  fi
}

# ======================================================================= main

RC=0
case ",$PHASES," in
  *,baseline,*) phase_baseline ;;
esac
case ",$PHASES," in
  *,snapshot,*) phase_snapshot ;;
esac
RESTORE_INERT=0
case ",$PHASES," in
  *,restore,*) phase_restore || true ;;
esac
verdict || RC=$?
finish "$RC"
