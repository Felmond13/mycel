#!/usr/bin/env bash
# KIT D'EXPÉRIENCE — Firecracker snapshot/restore SOUS WSL2.
#
# Question : le chemin snapshot existe-t-il sous Windows/WSL2 alors que le
# restore Cloud Hypervisor y est INERT (le KVM de WSL2 ne préserve pas
# l'état vCPU — docs/isolation-engines.md §Snapshot) ? Firecracker a son
# propre chemin save/restore (KVM_{GET,SET}_* distinct) : il faut le
# MESURER, pas le supposer.
#
# Phases (PHASES="boot,snapshot,restore") :
#   boot     : n=BOOT_RUNS boots à froid (T0 lancement -> T1 1er octet 200)
#   snapshot : 1 boot, Pause, PUT /snapshot/create (Full), durée consignée
#   restore  : n=RESTORE_RUNS restores (CLI --snapshot-path/--mem-file ou
#              PUT /snapshot/load selon la version), PATCH Resumed, T0->T1
#              + VIVACITÉ (battement de cœur console 200 ms — même
#              détecteur d'inertie que bench/snapshot-native)
#   verdict  : honnête — restore vivant sous WSL2 => le chemin Windows
#              existe (chiffres) ; inert => documenté, pool chaud en repli
#
# Auto-suffisant : télécharge Firecracker (FC_VERSION) si absent ; noyau
# = vmlinux ELF non compressé (Firecracker ne charge PAS les bzImage) —
# copié depuis l'arbre de build du noyau slim si présent, sinon extrait du
# bzImage assets/vmlinux-slim ; initramfs variante via mkinitramfs-fc.sh.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
MICROVM="$HERE/../microvm"
RESULTS="$HERE/results"
ASSETS="$HERE/assets"
STATS="$MICROVM/stats.sh"

FC_VERSION=${FC_VERSION:-v1.17.0}          # dernière release stable (2026-11)
FC_URL="https://github.com/firecracker-microvm/firecracker/releases/download/$FC_VERSION/firecracker-$FC_VERSION-x86_64.tgz"
FC_BIN=${FC_BIN:-$HERE/.tools/firecracker}

KERNEL_ELF="$ASSETS/vmlinux-fc.elf"        # ELF non compressé (requis FC)
INITRAMFS="$ASSETS/rootfs-fc.cpio"

# Topologie dédiée (ne touche pas au 192.168.77.x de bench/microvm) :
# FC ne crée PAS le tap — le script hôte le possède.
TAP="mycf0"
GUEST_IP="192.168.78.2"
GUEST_MAC="52:54:00:78:00:02"
HOST_IP="192.168.78.1"
URL="http://$GUEST_IP:8080/"
MEM_MIB=${MEM_MIB:-512}
BOOT_ARGS="console=ttyS0 quiet reboot=k panic=1 init=/init"

BOOT_RUNS=${BOOT_RUNS:-5}
RESTORE_RUNS=${RESTORE_RUNS:-10}
PHASES=${PHASES:-boot,snapshot,restore}
BOOT_TIMEOUT_MS=${BOOT_TIMEOUT_MS:-10000}
RESTORE_TIMEOUT_MS=${RESTORE_TIMEOUT_MS:-15000}
INERT_ABORT=${INERT_ABORT:-3}
GO_MS=${GO_MS:-200}

WORKROOT=""
SNAPDIR=""
VM_PID=""
PIDS_ALL=""

log()  { printf '\n=== %s ===\n' "$*"; }
info() { printf '[fc-kit] %s\n' "$*"; }
fail() {
  printf '[fc-kit] ERREUR : %s\n' "$*" >&2
  trap - EXIT INT TERM
  on_exit
  exit 2
}

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
  fail "doit tourner en root : le tap KVM l'exige"
fi

[ -e /dev/kvm ] || fail "/dev/kvm absent — ce kit mesure précisément le cas WSL2-KVM, il exige ce device"
[ -w /dev/kvm ] || fail "/dev/kvm non inscriptible (groupe kvm ?)"
for c in curl ip awk grep pgrep python3; do
  command -v "$c" >/dev/null 2>&1 || fail "commande absente : $c"
done
[ -x "$STATS" ] || fail "$STATS absent — le kit voyage avec son répertoire frère bench/microvm"

# ------------------------------------------------------- firecracker binaire

install_fc() {
  info "téléchargement $FC_URL"
  mkdir -p "$HERE/.tools"
  local tgz="$HERE/.tools/fc.tgz" xdir="$HERE/.tools/.extract"
  curl -fsSL -o "$tgz" "$FC_URL" || fail "téléchargement impossible : $FC_URL"
  rm -rf "$xdir"; mkdir -p "$xdir"
  tar -xzf "$tgz" -C "$xdir" || fail "extraction FC impossible"
  # layout v1.17 : release-vX.Y.Z-x86_64/firecracker-vX.Y.Z-x86_64
  # (layouts plus anciens : release/firecracker) — on cherche le binaire
  # principal (sans suffixe .debug) où qu'il vive.
  local bin
  bin=$(find "$xdir" -type f -name 'firecracker-v*' ! -name '*.debug' | head -1)
  [ -n "$bin" ] || bin=$(find "$xdir" -type f -name 'firecracker' ! -name '*.debug' | head -1)
  [ -n "$bin" ] || fail "binaire firecracker introuvable dans l'archive : $(find "$xdir" -type f | tr '\n' ' ')"
  mv "$bin" "$FC_BIN"
  rm -rf "$xdir" "$tgz"
  chmod 755 "$FC_BIN"
}

if [ ! -x "$FC_BIN" ]; then
  install_fc
fi
info "VMM : $FC_BIN ($("$FC_BIN" --version 2>/dev/null | head -1))"

# restore par flags CLI (--snapshot-path/--mem-file) ou par API (/snapshot/load) ?
FC_RESTORE_MODE="api"
if "$FC_BIN" --help 2>&1 | grep -q -- '--snapshot-path'; then
  FC_RESTORE_MODE="cli"
fi
info "chemin restore : $FC_RESTORE_MODE"

# ------------------------------------------------------------- assets invité

# Firecracker ne charge PAS les bzImage : il faut un vmlinux ELF. Le noyau
# slim (VIRTIO_MMIO + SERIAL_8250_CONSOLE intégrés — requis par le modèle
# device MMIO + UART de FC) vit en bzImage dans bench/microvm ; l'ELF de la
# MÊME compilation reste dans l'arbre de build ; en repli on l'extrait du
# bzImage (payload LZ4).
ensure_kernel_elf() {
  [ -f "$KERNEL_ELF" ] && return 0
  # l'arbre de build vit chez l'utilisateur (le kit tourne en root) :
  # chercher sous $HOME puis tout /home/*.
  local src="" extract="" h
  for h in "$HOME" /home/*; do
    [ -f "$h/build/mycel-kernel/build-slim/vmlinux" ] && { src="$h/build/mycel-kernel/build-slim/vmlinux"; break; }
  done
  mkdir -p "$ASSETS"
  if [ -n "$src" ]; then
    cp "$src" "$KERNEL_ELF"
    info "noyau ELF : copié depuis l'arbre de build slim ($src)"
  else
    for h in "$HOME" /home/*; do
      local e
      e=$(ls "$h"/build/mycel-kernel/linux-*/scripts/extract-vmlinux 2>/dev/null | head -1)
      [ -n "$e" ] && { extract="$e"; break; }
    done
    if [ -f "$MICROVM/assets/vmlinux-slim" ] && [ -n "$extract" ]; then
      "$extract" "$MICROVM/assets/vmlinux-slim" > "$KERNEL_ELF"
      info "noyau ELF : extrait du bzImage slim ($extract)"
    else
      fail "pas de vmlinux ELF disponible (arbre de build slim absent) : relance bench/microvm/build_kernel_slim.sh d'abord"
    fi
  fi
  # sanity : magie ELF
  [ "$(dd if="$KERNEL_ELF" bs=4 count=1 2>/dev/null | od -An -tx1 | tr -d ' \n')" = "7f454c46" ] \
    || fail "$KERNEL_ELF n'est pas un ELF (Firecracker exige un vmlinux non compressé)"
}
ensure_kernel_elf

if [ ! -f "$INITRAMFS" ]; then
  bash "$HERE/mkinitramfs-fc.sh"
fi
[ -f "$KERNEL_ELF" ]  || fail "noyau manquant : $KERNEL_ELF"
[ -f "$INITRAMFS" ]   || fail "initramfs manquant : $INITRAMFS"

# ------------------------------------------------------------------ espaces

mkdir -p "$RESULTS"
WORKROOT=$(mktemp -d /tmp/myc-fc-wsl2.XXXXXX)
SNAPDIR="$WORKROOT/snap"
mkdir -p "$SNAPDIR"
SNAP_FILE="$SNAPDIR/fc.snap"
MEM_FILE="$SNAPDIR/fc.mem"

avail_mb=$(df -Pm /tmp | awk 'NR==2 {print $4}')
if [ "${avail_mb:-0}" -lt 1500 ]; then
  info "AVERTISSEMENT : /tmp n'a que ${avail_mb} Mo libres ; le snapshot mémoire ($MEM_MIB Mo) peut ne pas tenir"
fi

{
  echo "=== Mycel snapshot-wsl2 (Firecracker) kit — environnement ==="
  date -u
  uname -a
  uptime
  lscpu | sed -n '1,12p'
  free -h
  echo "--- KVM ---"
  ls -l /dev/kvm
  echo "--- versions ---"
  "$FC_BIN" --version
  echo "--- SHA-256 ---"
  sha256sum "$KERNEL_ELF" "$INITRAMFS" "$FC_BIN"
  cat "$MICROVM/assets/vmlinux-slim.version" 2>/dev/null || true
  git -C "$HERE/../.." rev-parse HEAD 2>/dev/null || true
} > "$RESULTS/env.txt" 2>&1

# ------------------------------------------------------------ briques FC/tap

tap_up() {
  ip link del "$TAP" >/dev/null 2>&1 || true
  ip tuntap add dev "$TAP" mode tap || fail "ip tuntap add $TAP impossible"
  ip addr add "$HOST_IP/24" dev "$TAP"
  ip link set "$TAP" up
  ip neigh replace "$GUEST_IP" lladdr "$GUEST_MAC" dev "$TAP" nud permanent >/dev/null 2>&1 || true
}

tap_down() {
  ip link del "$TAP" 2>/dev/null || true
}

fc_api() {          # $1=sock $2=méthode $3=chemin $4=json
  curl -fsS --unix-socket "$1" -X "$2" "http://localhost$3" \
    -H 'Content-Type: application/json' -d "$4" >/dev/null
}

fc_wait_api() {     # $1=sock $2=timeout_ms
  local deadline=$(( $(date +%s%N) + $2 * 1000000 ))
  while [ "$(date +%s%N)" -lt "$deadline" ]; do
    kill -0 "$VM_PID" 2>/dev/null || return 1
    curl -fsS --unix-socket "$1" "http://localhost/version" >/dev/null 2>&1 && return 0
    sleep 0.01
  done
  return 1
}

vm_start() {        # $1 = rundir ; process FC nu + console série sur stdout
                    # (v1.17 : flag --api-sock ; logger pré-posé via CLI)
  VM_SOCK="$1/api.sock"; VM_PID=""
  "$FC_BIN" --api-sock "$VM_SOCK" --log-path "$1/fc.log" --level Info \
    >"$1/console.log" 2>&1 &
  VM_PID=$!
  PIDS_ALL="$PIDS_ALL $VM_PID"
}

vm_configure_and_start() {   # $1 = rundir (boot à froid complet)
  local d="$1"
  fc_wait_api "$d/api.sock" 5000 || return 1
  fc_api "$d/api.sock" PUT /boot-source \
    "{\"kernel_image_path\":\"$KERNEL_ELF\",\"initrd_path\":\"$INITRAMFS\",\"boot_args\":\"$BOOT_ARGS\"}" || return 1
  fc_api "$d/api.sock" PUT /machine-config \
    "{\"vcpu_count\":1,\"mem_size_mib\":$MEM_MIB,\"smt\":false}" || return 1
  fc_api "$d/api.sock" PUT /network-interfaces/net0 \
    "{\"iface_id\":\"net0\",\"guest_mac\":\"$GUEST_MAC\",\"host_dev_name\":\"$TAP\"}" || return 1
  fc_api "$d/api.sock" PUT /actions '{"action_type":"InstanceStart"}' || return 1
}

vm_restore() {      # $1 = rundir ; charge le snapshot puis Resumed
  local d="$1"
  if [ "$FC_RESTORE_MODE" = "cli" ]; then
    "$FC_BIN" --api-sock "$d/api.sock" --log-path "$d/fc.log" --level Info \
      --snapshot-path "$SNAP_FILE" --mem-file "$MEM_FILE" \
      >"$d/console.log" 2>&1 &
    VM_PID=$!
    PIDS_ALL="$PIDS_ALL $VM_PID"
    fc_wait_api "$d/api.sock" 10000 || return 1
  else
    vm_start "$d"
    fc_wait_api "$d/api.sock" 5000 || return 1
    fc_api "$d/api.sock" PUT /snapshot/load \
      "{\"snapshot_path\":\"$SNAP_FILE\",\"mem_backend\":{\"backend_type\":\"File\",\"backend_path\":\"$MEM_FILE\"}}" || return 1
  fi
  # le microVM restauré démarre en pause — le resume fait partie du chemin
  # restore (équivalent resume=true côté Cloud Hypervisor)
  local try
  for try in 1 2 3; do
    fc_api "$d/api.sock" PATCH /vm '{"state":"Resumed"}' && return 0
    sleep 0.1
  done
  return 1
}

vm_cleanup() {
  [ -n "$VM_PID" ] || return 0
  kill -TERM "$VM_PID" 2>/dev/null || true
  local i
  for i in $(seq 1 100); do
    kill -0 "$VM_PID" 2>/dev/null || break
    sleep 0.05
  done
  kill -KILL "$VM_PID" 2>/dev/null || true
  wait "$VM_PID" 2>/dev/null || true
  PIDS_ALL=$(printf '%s' "$PIDS_ALL" | tr ' ' '\n' | grep -vx "$VM_PID" | tr '\n' ' ')
  VM_PID=""
  rm -f "${VM_SOCK:-/nonexistent}" 2>/dev/null || true
}

residue_check() {
  ip link show "$TAP" >/dev/null 2>&1 && return 1
  pgrep -f "$FC_BIN" >/dev/null 2>&1 && return 1
  return 0
}

# Réchauffe ICMP (cf. bench/microvm/run_until_ready.sh) : aucun SYN tant que
# l'invité ne répond pas au ping — évite le backoff TCP ~9 s. NB : pas de
# `grep -q` en pipeline (pipefail + sortie anticipée = SIGPIPE = échec
# fantôme) ; on neutralise par || true et on teste la capture.
icmp_warm() {
  local deadline=$(( $(date +%s%N) + 10000 * 1000000 )) out
  while [ "$(date +%s%N)" -lt "$deadline" ]; do
    kill -0 "$VM_PID" 2>/dev/null || return 0
    out=$(timeout 1 ping -n -i 0.02 "$GUEST_IP" 2>/dev/null | grep -m1 'bytes from' || true)
    [ -n "$out" ] && return 0
    sleep 0.05
  done
}

wait_ready() {      # $1 = timeout_ms ; READY=1 + T1_NS si 200 reçu
  local deadline=$(( $(date +%s%N) + $1 * 1000000 ))
  READY=0; T1_NS=""
  while [ "$(date +%s%N)" -lt "$deadline" ]; do
    kill -0 "$VM_PID" 2>/dev/null || return 0
    if curl -fsS --max-time 0.05 -o /dev/null "$URL" 2>/dev/null; then
      READY=1; T1_NS=$(date +%s%N); return 0
    fi
    sleep 0.005
  done
}

hb_count() {        # battements de cœur visibles sur la console série
  local n=0
  [ -f "$1/console.log" ] && n=$(grep -c 'benchhttp: heartbeat' "$1/console.log" || true)
  HBC=$n
}

guest_alive() {     # ALIVE=1 si >=2 nouveaux battements pendant 1,2 s
  local before after
  hb_count "$1"; before=$HBC
  sleep 1.2
  hb_count "$1"; after=$HBC
  if [ $(( after - before )) -ge 2 ]; then ALIVE=1; else ALIVE=0; fi
}

new_rundir() {
  RUNDIR=$(mktemp -d "$WORKROOT/run.XXXXXX")
}

INERT_STREAK=0

# ================================================================= phase boot

phase_boot() {
  log "BOOT — $BOOT_RUNS boots à froid (T0 -> T1 : 1er octet 200)"
  local i t0
  : > "$RESULTS/boot.times"
  for i in $(seq 1 "$BOOT_RUNS"); do
    new_rundir
    # T0 AVANT tap_up : la création du tap fait partie du chemin mesuré
    # (le boot à froid de référence bench/microvm mesure, lui aussi, toute
    # la mise en place par le VMM).
    t0=$(date +%s%N)
    tap_up
    vm_start "$RUNDIR"
    vm_configure_and_start "$RUNDIR" || { dump_fail "boot $i : config API refusée"; fail "boot $i : config API refusée (voir failures.log)"; }
    icmp_warm || true
    wait_ready "$BOOT_TIMEOUT_MS"
    if [ "$READY" -ne 1 ]; then
      dump_fail "boot $i : pas de 200 en ${BOOT_TIMEOUT_MS} ms"
      fail "boot $i : invité jamais prêt (voir failures.log)"
    fi
    echo $(( T1_NS - t0 )) >> "$RESULTS/boot.times"
    guest_alive "$RUNDIR"
    [ "$ALIVE" -eq 1 ] || fail "boot $i : pas de battement de cœur (détecteur de vivacité cassé ?)"
    vm_cleanup
    tap_down
    residue_check || fail "run boot $i : résidu (tap/VMM) — fuite"
    info "boot $i : $(tail -1 "$RESULTS/boot.times") ns"
  done
  info "boot : $(bash "$STATS" "$RESULTS/boot.times")"
}

dump_fail() {       # $1 = contexte ; console+logs vers failures.log
  {
    echo "--- $1 (rundir $RUNDIR) ---"
    echo "console.log:"; tail -15 "$RUNDIR/console.log" 2>/dev/null
    echo "fc.log:"; tail -10 "$RUNDIR/fc.log" 2>/dev/null
  } >> "$RESULTS/failures.log"
  return 0
}

# ============================================================= phase snapshot

phase_snapshot() {
  log "SNAPSHOT — boot, Pause, PUT /snapshot/create (Full) vers $SNAPDIR"
  local t0 t1 snap_ms
  new_rundir
  tap_up
  vm_start "$RUNDIR"
  vm_configure_and_start "$RUNDIR" || { dump_fail "snapshot : config API refusée"; fail "snapshot : config API refusée"; }
  icmp_warm
  wait_ready "$BOOT_TIMEOUT_MS"
  [ "$READY" -eq 1 ] || { dump_fail "snapshot : jamais prêt"; fail "snapshot : la VM de base n'a jamais répondu"; }
  sleep 1.5
  guest_alive "$RUNDIR"
  [ "$ALIVE" -eq 1 ] || { dump_fail "snapshot : inerte avant snapshot"; fail "snapshot : invité de base inerte avant même le snapshot"; }

  fc_api "$RUNDIR/api.sock" PATCH /vm '{"state":"Paused"}' \
    || { dump_fail "snapshot : pause refusée"; fail "échec PATCH /vm Paused (voir failures.log)"; }
  t0=$(date +%s%N)
  fc_api "$RUNDIR/api.sock" PUT /snapshot/create \
    "{\"snapshot_path\":\"$SNAP_FILE\",\"mem_file_path\":\"$MEM_FILE\",\"snapshot_type\":\"Full\"}" \
    || { dump_fail "snapshot : create refusé"; fail "échec PUT /snapshot/create (voir failures.log)"; }
  t1=$(date +%s%N)
  snap_ms=$(( (t1 - t0) / 1000000 ))

  [ -f "$SNAP_FILE" ] && [ -f "$MEM_FILE" ] \
    || { dump_fail "snapshot : fichiers absents"; fail "snapshot incomplet dans $SNAPDIR : $(ls -la "$SNAPDIR" 2>/dev/null | tr '\n' ' ')"; }

  {
    echo "snapshot_dir=$SNAPDIR"
    echo "snapshot_creation_ms=$snap_ms"
    echo "mem_size_mib=$MEM_MIB"
    echo "files:"
    ls -la "$SNAPDIR"
  } > "$RESULTS/snapshot-info.txt"
  vm_cleanup
  tap_down
  residue_check || fail "snapshot : résidu (tap/VMM) après shutdown"
  info "snapshot créé en ${snap_ms} ms (détail : results/snapshot-info.txt)"
}

# ============================================================== phase restore

restore_one() {     # -> CLASS = ok | netdead | inert | died | leak
  local t0 rc=0
  new_rundir
  # T0 AVANT tap_up : la mise en place réseau fait partie du chemin restore
  t0=$(date +%s%N)
  tap_up
  vm_restore "$RUNDIR" || rc=$?
  if [ "$rc" -ne 0 ]; then
    CLASS=died
    dump_fail "restore : lancement/chargement refusé (class=died)"
  else
    wait_ready "$RESTORE_TIMEOUT_MS"
    if [ "$READY" -eq 1 ]; then
      guest_alive "$RUNDIR"
      if [ "$ALIVE" -eq 1 ]; then
        echo $(( T1_NS - t0 )) >> "$RESULTS/restore.times"
        CLASS=ok
      else
        CLASS=netdead
        dump_fail "restore : 200 reçu puis figé (class=netdead)"
      fi
    else
      if ! kill -0 "$VM_PID" 2>/dev/null; then
        CLASS=died
        dump_fail "restore : process FC mort (class=died)"
      else
        guest_alive "$RUNDIR"
        if [ "$ALIVE" -eq 1 ]; then
          CLASS=netdead
          dump_fail "restore : vivant mais injoignable (class=netdead)"
        else
          CLASS=inert
          dump_fail "restore : mémoire chargée, vCPU relancé, invité figé (class=inert)"
        fi
      fi
    fi
  fi
  vm_cleanup
  tap_down
  residue_check || { echo "run restore : RÉSIDU (tap/VMM) — fuite" >&2; CLASS=leak; }
}

phase_restore() {
  log "RESTORE — $RESTORE_RUNS restores (mode=$FC_RESTORE_MODE), vivacité par battement console"
  : > "$RESULTS/restore.times"
  local i n_ok=0 n_inert=0 n_other=0
  for i in $(seq 1 "$RESTORE_RUNS"); do
    restore_one
    info "restore $i/$RESTORE_RUNS : class=$CLASS"
    case "$CLASS" in
      ok)    n_ok=$(( n_ok + 1 )); INERT_STREAK=0 ;;
      inert) n_inert=$(( n_inert + 1 ))
             INERT_STREAK=$(( INERT_STREAK + 1 )) ;;
      *)     n_other=$(( n_other + 1 )); INERT_STREAK=0 ;;
    esac
    RESTORE_OK=$n_ok; RESTORE_INERT=$n_inert; RESTORE_OTHER=$n_other
    if [ "$INERT_STREAK" -ge "$INERT_ABORT" ]; then
      inert_abort "run $i"
      return 3
    fi
  done
}

inert_abort() {
  cat >&2 <<EOF

[fc-kit] ============================================================
[fc-kit]  RESTORE INERT SOUS FIRECRACKER AUSSI — même signature que
[fc-kit]  Cloud Hypervisor : la mémoire est chargée, le vCPU relancé,
[fc-kit]  mais l'invité reste figé (ni timer, ni interruption, ni
[fc-kit]  battement de cœur). Le KVM de WSL2 ne préserve pas l'état
[fc-kit]  vCPU au restore, quel que soit le VMM. Le chemin snapshot
[fc-kit]  Windows passe par le POOL CHAUD (bench/warm-pool), pas par
[fc-kit]  restore. Détails : results/failures.log — interrompu à : $1
[fc-kit] ============================================================
EOF
  RESTORE_OK=${RESTORE_OK:-0}; RESTORE_INERT=${RESTORE_INERT:-$INERT_STREAK}; RESTORE_OTHER=${RESTORE_OTHER:-0}
  return 3
}

# =================================================================== verdict

verdict() {
  log "VERDICT"
  {
    echo "=== Mycel snapshot-wsl2 (Firecracker $FC_VERSION) — verdict ($(date -u)) ==="
    echo "hôte : $(uname -r) — $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | sed 's/^ //')"
    echo ""
    echo "Boot à froid Firecracker sous WSL2 :"
    [ -s "$RESULTS/boot.times" ] && bash "$STATS" "$RESULTS/boot.times" || echo "  (absent)"
    [ -f "$RESULTS/snapshot-info.txt" ] && { echo ""; grep -E 'snapshot_creation_ms|mem_size_mib' "$RESULTS/snapshot-info.txt"; }
    echo ""
    echo "Restore (${RESTORE_RUNS} essais, mode=$FC_RESTORE_MODE) :"
    echo "  ok=$RESTORE_OK inert=$RESTORE_INERT autres=$RESTORE_OTHER"
    if [ "$RESTORE_OK" -gt 0 ] && [ -s "$RESULTS/restore.times" ]; then
      echo "  latence T0->200 : $(bash "$STATS" "$RESULTS/restore.times")"
    fi
    echo ""
  } | tee "$RESULTS/verdict.txt"

  if [ "${RESTORE_INERT:-0}" -gt 0 ] && [ "${RESTORE_OK:-0}" -eq 0 ]; then
    echo "VERDICT : Firecracker restore INERT sous WSL2 — pas de chemin snapshot Windows par restore ; repli = pool chaud (bench/warm-pool)." | tee -a "$RESULTS/verdict.txt"
    return 3
  fi
  if [ "${RESTORE_OK:-0}" -gt 0 ]; then
    echo "VERDICT : Firecracker restore VIVANT sous WSL2 — le chemin snapshot Windows existe (chiffres ci-dessus, n=${RESTORE_OK} vivants)." | tee -a "$RESULTS/verdict.txt"
    return 0
  fi
  echo "VERDICT : non évalué (phase restore non exécutée — PHASES=$PHASES)." | tee -a "$RESULTS/verdict.txt"
  return 0
}

# ======================================================================= main

RESTORE_OK=0; RESTORE_INERT=0; RESTORE_OTHER=0
RC=0
case ",$PHASES," in
  *,boot,*) phase_boot ;;
esac
case ",$PHASES," in
  *,snapshot,*) phase_snapshot ;;
esac
case ",$PHASES," in
  *,restore,*) phase_restore || true ;;
esac
verdict || RC=$?
finish "$RC"
