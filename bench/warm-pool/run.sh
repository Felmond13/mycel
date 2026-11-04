#!/usr/bin/env bash
# KIT D'EXPÉRIENCE — POOL CHAUD de microVMs (bench/warm-pool).
#
# Le pool chaud marche PARTOUT (ce ne sont que des processus + taps : ni
# save/restore vCPU, ni dépendance hyperviseur) — c'est le repli
# multiplateforme quand le restore snapshot n'existe pas (WSL2-KVM/Cloud
# Hypervisor inert, libkrun sans API) ou n'est pas encore validé (KVM natif).
#
# Mesure, à charge identique (benchhttp, noyau slim, ICMP-warm) :
#   cold    : n=RUNS boots À FROID, T0 (lancement CH, création du tap incluse)
#             -> T1 (1er octet 200) — même yardstick que bench/microvm.
#   pool    : POOL_SIZE VMs pré-bootées (chacune son tap/subnet, IP invité
#             passée par cmdline), prêtes depuis >5 s ; n=RUNS handoffs en
#             round-robin : T0 = curl déclenché -> T1 = 200 reçu. La VM
#             chaude est-elle vraiment plus rapide qu'un boot ? De combien ?
#
# HONNÊTETÉ (consigné dans le verdict) :
#   - un pool de N VMs ne détient que N invités PRÊTS : dans un moteur réel,
#     chaque handoff remet UNE VM à UN invité (1 prêt par VM) ; le
#     re-remplissage est un boot à froid en tâche de fond (coût = série cold)
#   - mémoire résidente : POOL_SIZE x 512 Mo pendant toute la vie du pool
#   - le handoff mesuré EST la vraie latence utilisateur (spawn curl + TCP +
#     réponse) : même mètre que la série cold
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
MICROVM="$HERE/../microvm"
RESULTS="$HERE/results"
ASSETS="$HERE/assets"
STATS="$MICROVM/stats.sh"

CH_VERSION="v54.0"
CH_BASE_URL="https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/$CH_VERSION"
CH_BIN=${CH_BIN:-/usr/local/bin/cloud-hypervisor}
CH_REMOTE=${CH_REMOTE:-/usr/local/bin/ch-remote}

# noyau slim si présent, sinon noyau stock (même règle que bench/microvm)
KERNEL="$MICROVM/assets/vmlinux-slim"
[ -f "$KERNEL" ] || KERNEL="$MICROVM/assets/vmlinux"
INITRAMFS="$ASSETS/rootfs-pool.cpio"

RUNS=${RUNS:-30}
WARMUPS=${WARMUPS:-5}
POOL_SIZE=${POOL_SIZE:-3}
MEM_SIZE=${MEM_SIZE:-512M}
BOOT_TIMEOUT_MS=${BOOT_TIMEOUT_MS:-10000}
HANDOFF_TIMEOUT_MS=${HANDOFF_TIMEOUT_MS:-2000}
# subnets dédiés au pool (80-82) + série cold (83) — ne touche pas au
# 192.168.77.x de bench/microvm ni au 78.x de snapshot-wsl2
POOL_NET_BASE=192.168.8
COLD_NET=192.168.83
COLD_TAP="mycC0"

WORKROOT=""
VM_PID=""
PIDS_ALL=""
POOL_PIDS=""
POOL_READY_NS=""

log()  { printf '\n=== %s ===\n' "$*"; }
info() { printf '[pool] %s\n' "$*"; }
fail() { printf '[pool] ERREUR : %s\n' "$*" >&2; finish 2; }

finish() {
  local rc=$1
  trap - EXIT INT TERM
  on_exit
  exit "$rc"
}

on_exit() {
  local p
  [ -n "$VM_PID" ] && { kill -TERM "$VM_PID" 2>/dev/null || true; wait "$VM_PID" 2>/dev/null || true; }
  for p in $PIDS_ALL $POOL_PIDS; do kill -KILL "$p" 2>/dev/null || true; done
  ip link del "$COLD_TAP" 2>/dev/null || true
  local i
  for i in $(seq 0 $(( POOL_SIZE - 1 ))); do ip link del "mycP$i" 2>/dev/null || true; done
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
  fail "doit tourner en root : le tap KVM l'exige"
fi

[ -e /dev/kvm ] || fail "/dev/kvm absent"
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
  local sys="$1" url="$2"
  if [ -x "$sys" ] && ch_version_of "$sys" | grep -q "v54\.0"; then
    printf '%s' "$sys"
    return
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
info "VMM : $CH_BIN ($(ch_version_of "$CH_BIN"))"

# ------------------------------------------------------------- assets invité

[ -f "$KERNEL" ]   || fail "noyau manquant : $KERNEL (build_kernel_slim.sh / build_kernel.sh)"
if [ ! -f "$INITRAMFS" ]; then
  bash "$HERE/mkinitramfs-pool.sh"
fi
[ -f "$INITRAMFS" ] || fail "initramfs manquant : $INITRAMFS"

# ------------------------------------------------------------------ espaces

mkdir -p "$RESULTS"
WORKROOT=$(mktemp -d /tmp/myc-warm-pool.XXXXXX)

{
  echo "=== Mycel warm-pool kit — environnement ==="
  date -u
  uname -a
  uptime
  lscpu | sed -n '1,12p'
  free -h
  echo "--- KVM ---"
  ls -l /dev/kvm
  echo "--- versions ---"
  "$CH_BIN" --version
  echo "--- SHA-256 ---"
  sha256sum "$KERNEL" "$INITRAMFS"
  cat "$MICROVM/assets/vmlinux-slim.version" 2>/dev/null || cat "$MICROVM/assets/vmlinux.version" 2>/dev/null || true
  git -C "$HERE/../.." rev-parse HEAD 2>/dev/null || true
} > "$RESULTS/env.txt" 2>&1

# ------------------------------------------------------------ briques VM

arp_seed() {        # $1=tap $2=ip $3=mac ; entrée ARP permanente dès que le tap existe
  local i
  for i in $(seq 1 100); do
    ip link show "$1" >/dev/null 2>&1 || { sleep 0.01; continue; }
    ip neigh replace "$2" lladdr "$3" dev "$1" nud permanent >/dev/null 2>&1 || true
    return
  done
}

# Réchauffe ICMP (cf. bench/microvm/run_until_ready.sh) ; PAS de grep -q en
# pipeline (pipefail + sortie anticipée = SIGPIPE = échec fantôme).
icmp_warm() {       # $1=ip $2=pid
  local deadline=$(( $(date +%s%N) + 10000 * 1000000 )) out
  while [ "$(date +%s%N)" -lt "$deadline" ]; do
    kill -0 "$2" 2>/dev/null || return 0
    out=$(timeout 1 ping -n -i 0.02 "$1" 2>/dev/null | grep -m1 'bytes from' || true)
    [ -n "$out" ] && return 0
    sleep 0.05
  done
}

wait_ready() {      # $1=url $2=timeout_ms $3=pid ; READY=1 + T1_NS si 200 reçu
  local deadline=$(( $(date +%s%N) + $2 * 1000000 ))
  READY=0; T1_NS=""
  while [ "$(date +%s%N)" -lt "$deadline" ]; do
    kill -0 "$3" 2>/dev/null || return 0
    if curl -fsS --max-time 0.05 -o /dev/null "$1" 2>/dev/null; then
      READY=1; T1_NS=$(date +%s%N); return 0
    fi
    sleep 0.005
  done
}

hb_count() {        # $1=console.log -> HBC
  local n=0
  [ -f "$1" ] && n=$(grep -c 'benchhttp: heartbeat' "$1" || true)
  HBC=$n
}

guest_alive() {     # $1=console.log ; ALIVE=1 si >=2 nouveaux battements/1,2 s
  local before after
  hb_count "$1"; before=$HBC
  sleep 1.2
  hb_count "$1"; after=$HBC
  if [ $(( after - before )) -ge 2 ]; then ALIVE=1; else ALIVE=0; fi
}

vm_kill() {         # $1=pid $2=api.sock
  [ -n "$1" ] || return 0
  "$CH_REMOTE" --api-socket "$2" shutdown-vmm >/dev/null 2>&1 || true
  local i
  for i in $(seq 1 100); do
    kill -0 "$1" 2>/dev/null || break
    sleep 0.05
  done
  kill -KILL "$1" 2>/dev/null || true
  wait "$1" 2>/dev/null || true
  rm -f "$2" 2>/dev/null || true
}

residue_check() {   # $@=taps à vérifier ; + aucun process CH résiduel
  local t
  for t in "$@"; do ip link show "$t" >/dev/null 2>&1 && return 1; done
  pgrep -f "$CH_BIN" >/dev/null 2>&1 && return 1
  return 0
}

# ============================================================== série COLD

phase_cold() {
  log "COLD — $WARMUPS échauffements + $RUNS boots à froid (T0 lancement -> T1 1er octet 200)"
  local i t0 host_ip="192.168.83.1" gip="$COLD_NET.2" mac="52:54:00:83:00:02"
  local url="http://$gip:8080/"
  : > "$RESULTS/cold.times"
  for i in $(seq 1 "$WARMUPS"); do
    ip link del "$COLD_TAP" >/dev/null 2>&1 || true
    VM_SOCK="$WORKROOT/warmup.sock"
    "$CH_BIN" --api-socket "$VM_SOCK" --console "file=$WORKROOT/warmup-console.log" \
      --kernel "$KERNEL" --cmdline "console=hvc0 quiet guest_ip=$gip" \
      --initramfs "$INITRAMFS" --cpus "boot=1" --memory "size=$MEM_SIZE" \
      --net "tap=$COLD_TAP,mac=$mac,ip=$host_ip,mask=255.255.255.0" \
      >"$WORKROOT/warmup-ch.log" 2>&1 &
    VM_PID=$!; PIDS_ALL="$PIDS_ALL $VM_PID"
    arp_seed "$COLD_TAP" "$gip" "$mac"
    icmp_warm "$gip" "$VM_PID" || true
    wait_ready "$url" "$BOOT_TIMEOUT_MS" "$VM_PID"
    [ "$READY" -eq 1 ] || { fail "échauffement cold $i : jamais prêt"; }
    vm_kill "$VM_PID" "$VM_SOCK"; VM_PID=""
    residue_check "$COLD_TAP" || fail "échauffement $i : résidu (tap/VMM)"
  done
  for i in $(seq 1 "$RUNS"); do
    ip link del "$COLD_TAP" >/dev/null 2>&1 || true
    VM_SOCK="$WORKROOT/cold-$i.sock"
    t0=$(date +%s%N)
    "$CH_BIN" --api-socket "$VM_SOCK" --console "file=$WORKROOT/cold-$i-console.log" \
      --kernel "$KERNEL" --cmdline "console=hvc0 quiet guest_ip=$gip" \
      --initramfs "$INITRAMFS" --cpus "boot=1" --memory "size=$MEM_SIZE" \
      --net "tap=$COLD_TAP,mac=$mac,ip=$host_ip,mask=255.255.255.0" \
      >"$WORKROOT/cold-$i-ch.log" 2>&1 &
    VM_PID=$!; PIDS_ALL="$PIDS_ALL $VM_PID"
    arp_seed "$COLD_TAP" "$gip" "$mac"
    icmp_warm "$gip" "$VM_PID" || true
    wait_ready "$url" "$BOOT_TIMEOUT_MS" "$VM_PID"
    [ "$READY" -eq 1 ] || { tail -5 "$WORKROOT/cold-$i-ch.log" >&2 || true; fail "run cold $i : jamais prêt"; }
    echo $(( T1_NS - t0 )) >> "$RESULTS/cold.times"
    vm_kill "$VM_PID" "$VM_SOCK"; VM_PID=""
    residue_check "$COLD_TAP" || fail "run cold $i : résidu (tap/VMM) — fuite"
  done
  info "cold : $(bash "$STATS" "$RESULTS/cold.times")"
}

# ============================================================== série POOL

phase_pool() {
  log "POOL — pré-boot de $POOL_SIZE VMs (taps mycP0-$(( POOL_SIZE - 1 )), subnets $POOL_NET_BASE.0-82.0/24)"
  local i tap host_ip gip mac url pid sock
  local pids=() urls=() socks=() ready_ns=()
  for i in $(seq 0 $(( POOL_SIZE - 1 ))); do
    tap="mycP$i"; host_ip="$POOL_NET_BASE$i.1"; gip="$POOL_NET_BASE$i.2"
    mac="52:54:00:8$i:00:02"; url="http://$gip:8080/"; sock="$WORKROOT/p$i.sock"
    ip link del "$tap" >/dev/null 2>&1 || true
    "$CH_BIN" --api-socket "$sock" --console "file=$WORKROOT/p$i-console.log" \
      --kernel "$KERNEL" --cmdline "console=hvc0 quiet guest_ip=$gip" \
      --initramfs "$INITRAMFS" --cpus "boot=1" --memory "size=$MEM_SIZE" \
      --net "tap=$tap,mac=$mac,ip=$host_ip,mask=255.255.255.0" \
      >"$WORKROOT/p$i-ch.log" 2>&1 &
    pid=$!; PIDS_ALL="$PIDS_ALL $pid"; POOL_PIDS="$POOL_PIDS $pid"
    arp_seed "$tap" "$gip" "$mac"
    icmp_warm "$gip" "$pid" || true     # pré-boot non mesuré : pas de backoff TCP
    wait_ready "$url" "$BOOT_TIMEOUT_MS" "$pid"
    [ "$READY" -eq 1 ] || { tail -5 "$WORKROOT/p$i-ch.log" >&2 || true; fail "pool VM $i : jamais prête"; }
    pids+=("$pid"); urls+=("$url"); socks+=("$sock"); ready_ns+=("$T1_NS")
    info "VM $i prête (${POOL_NET_BASE}${i}.2, pid $pid)"
  done

  # vivacité de chaque VM du pool : la promesse du pool est un invité VIVANT
  for i in $(seq 0 $(( POOL_SIZE - 1 )) ); do
    guest_alive "$WORKROOT/p$i-console.log"
    [ "$ALIVE" -eq 1 ] || fail "pool VM $i : inerte avant handoff"
  done

  # attends que la PLUS JEUNE VM du pool ait >= 5 s d'âge : le handoff mesuré
  # est bien "servi par une VM bootée depuis > 5 s", jamais un boot déguisé
  local youngest=${ready_ns[0]} now
  for i in $(seq 1 $(( POOL_SIZE - 1 )) ); do
    [ "${ready_ns[$i]}" -gt "$youngest" ] && youngest=${ready_ns[$i]}
  done
  now=$(date +%s%N)
  if [ $(( now - youngest )) -lt 5000000000 ]; then
    info "attente : plus jeune VM < 5 s d'âge..."
    sleep 5.2
  fi

  log "HANDOFF — $RUNS handoffs round-robin (T0 curl déclenché -> T1 200 reçu)"
  : > "$RESULTS/handoff.times"
  local n idx t0
  for n in $(seq 1 "$RUNS"); do
    idx=$(( (n - 1) % POOL_SIZE ))
    t0=$(date +%s%N)
    wait_ready "${urls[$idx]}" "$HANDOFF_TIMEOUT_MS" "${pids[$idx]}"
    [ "$READY" -eq 1 ] || fail "handoff $n (VM $idx) : pas de 200 en ${HANDOFF_TIMEOUT_MS} ms"
    echo $(( T1_NS - t0 )) >> "$RESULTS/handoff.times"
  done
  info "handoff : $(bash "$STATS" "$RESULTS/handoff.times")"

  # démontage du pool + vérification de non-fuite
  for i in $(seq 0 $(( POOL_SIZE - 1 )) ); do
    vm_kill "${pids[$i]}" "${socks[$i]}"
  done
  POOL_PIDS=""
  local taps=""
  for i in $(seq 0 $(( POOL_SIZE - 1 )) ); do taps="$taps mycP$i"; done
  # shellcheck disable=SC2086
  residue_check $taps || fail "démontage pool : résidu (tap/VMM) — fuite"
}

# =================================================================== verdict

verdict() {
  log "VERDICT"
  local cold_s hand_s cold_med hand_med speedup
  cold_s=$(bash "$STATS" "$RESULTS/cold.times")
  hand_s=$(bash "$STATS" "$RESULTS/handoff.times")
  cold_med=$(printf '%s' "$cold_s" | sed -n 's/.*median_ms=\([0-9.]*\).*/\1/p')
  hand_med=$(printf '%s' "$hand_s" | sed -n 's/.*median_ms=\([0-9.]*\).*/\1/p')
  speedup=$(awk -v c="$cold_med" -v h="$hand_med" 'BEGIN { if (h > 0) printf "%.1f", c / h; else print "-" }')
  {
    echo "=== Mycel warm-pool kit — verdict ($(date -u)) ==="
    echo "hôte : $(uname -r) — $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | sed 's/^ //')"
    echo "noyau : $(basename "$KERNEL") — initramfs : $(basename "$INITRAMFS")"
    echo ""
    echo "Boot À FROID (n=$RUNS, T0 lancement -> T1 1er octet 200) :"
    echo "  $cold_s"
    echo "HANDOFF pool chaud (n=$RUNS, POOL_SIZE=$POOL_SIZE, VMs prêtes > 5 s, T0 curl -> T1 200) :"
    echo "  $hand_s"
    echo ""
    echo "accélération médiane : x$speedup (cold ${cold_med} ms -> handoff ${hand_med} ms)"
    echo ""
    echo "Limites honnêtes du pool :"
    echo "  - $POOL_SIZE invités prêts pour $POOL_SIZE VMs résidentes (1 prêt par VM) ; le"
    echo "    re-remplissage est un boot à froid EN TÂCHE DE FOND (coût = série cold ci-dessus)"
    echo "  - mémoire résidente : $POOL_SIZE x $MEM_SIZE pendant toute la vie du pool"
    echo "  - marche partout (processes + taps only) : KVM natif, WSL2, et a priori tout"
    echo "    hyperviseur qui boote la microVM — aucune dépendance au save/restore vCPU"
  } | tee "$RESULTS/verdict.txt"
}

# ======================================================================= main

phase_cold
phase_pool
verdict
finish 0
