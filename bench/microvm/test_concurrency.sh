#!/usr/bin/env bash
# Concurrence réseau du backend kvm-microvm (crates/myc-backend/src/kvm.rs)
# — la preuve que N instances coexistent : deux VMs lancées SIMULTANÉMENT
# via myc-backend/examples/boot (launch()/RunningVm), chacune sur SON bail
# (192.168.77.2 ET 192.168.77.3, taps myc-2/myc-3 sur le bridge br-myc),
# servent toutes les deux leur 200 AU MÊME MOMENT, puis arrêt propre et
# ZÉRO résidu (taps myc-N, règles iptables taggées myc-N, entrées neigh,
# bails /tmp/myc-leases-*). br-myc, lui, DOIT persister (pattern docker0).
#
# Usage : sudo ./test_concurrency.sh   (root : bridge + taps KVM)
set -u

HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
BOOT="$REPO/target/release/examples/boot"
ASSETS="$HERE/assets"
INIT="$ASSETS/rootfs-dyn.cpio"
IP1="192.168.77.2"
IP2="192.168.77.3"
PORT="8080"
LOG1=$(mktemp /tmp/myc-conc-1.XXXXXX.log)
LOG2=$(mktemp /tmp/myc-conc-2.XXXXXX.log)

fail() { echo "FAIL : $*" >&2; exit 1; }
ok()   { echo "PASS : $*"; }
trap 'rm -f "$LOG1" "$LOG2"; [ -n "${STAGE:-}" ] && rm -rf "$STAGE"' EXIT

[ "$(id -u)" = 0 ] || fail "root requis (bridge br-myc + taps KVM)"
[ -x "$BOOT" ]     || fail "exemple boot manquant : cargo build --release --example boot"
[ -f "$ASSETS/rootfs.cpio" ] || fail "initramfs bench manquant : ./mkinitramfs.sh"

pgrep -f /usr/local/bin/cloud-hypervisor >/dev/null 2>&1 \
  && fail "un cloud-hypervisor tourne déjà — environnement non idle"
# Nettoyage préalable ciblé (un run avorté précédent laisserait myc-2/myc-3
# pris et l'allocation partirait sur .4/.5) : taps + registre de bails.
for t in myc-2 myc-3; do ip link del "$t" >/dev/null 2>&1 || true; done
rm -rf /tmp/myc-leases-192.168.77.0

# --- initramfs à IP dynamique -------------------------------------------
# Copie du bench rootfs.cpio dont l'/init lit mycip= sur la cmdline kernel
# (le bail passe par launch() → cmdline) au lieu d'une IP codée en dur.
STAGE=$(mktemp -d)
(cd "$STAGE" && cpio -id --quiet < "$ASSETS/rootfs.cpio")
cat > "$STAGE/init" <<'EOF'
#!/bin/sh
# IP dynamique : mycip=<bail> vient de la cmdline kernel (côté hôte).
mount -t proc proc /proc
mount -t sysfs sysfs /sys
MYCIP=""
for x in $(cat /proc/cmdline); do
  case "$x" in mycip=*) MYCIP=${x#mycip=} ;; esac
done
[ -n "$MYCIP" ] || MYCIP=192.168.77.2
ip addr add "$MYCIP/24" dev eth0
ip link set eth0 up
ip link set lo up
exec /benchhttp
EOF
chmod 755 "$STAGE/init"
(cd "$STAGE" && find . | cpio -o -H newc --quiet > "$INIT")
rm -rf "$STAGE"
echo "initramfs dynamique : $INIT"

# --- deux VMs démarrées SIMULTANÉMENT ------------------------------------
# Barrière sleep : les deux processes exec boot à ~5 ms d'écart au lieu de
# l'écart spawn-séquentiel ; l'allocation de bail doit rester sans course.
( sleep 0.5; exec "$BOOT" "$INIT" ) >"$LOG1" 2>&1 & P1=$!
( sleep 0.5; exec "$BOOT" "$INIT" ) >"$LOG2" 2>&1 & P2=$!

ready=0
for _ in $(seq 1 300); do
  if grep -q 'guest ready' "$LOG1" && grep -q 'guest ready' "$LOG2"; then ready=1; break; fi
  kill -0 "$P1" 2>/dev/null || { cat "$LOG1"; fail "VM1 morte avant d'être prête"; }
  kill -0 "$P2" 2>/dev/null || { cat "$LOG2"; fail "VM2 morte avant d'être prête"; }
  sleep 0.1
done
[ "$ready" = 1 ] || { cat "$LOG1" "$LOG2"; fail "les deux VMs ne sont pas prêtes en 30 s"; }
ok "les deux VMs sont prêtes : $(grep -h 'guest ready' "$LOG1" "$LOG2" | tr '\n' ' ')"

# Deux taps distincts rattachés au bridge = deux bail distincts.
TAPS=$(ip link show master br-myc 2>/dev/null | grep -oE 'myc-[0-9]+' | sort | tr '\n' ' ')
[ "$TAPS" = "myc-2 myc-3 " ] || fail "taps attendus 'myc-2 myc-3', vus : '$TAPS'"
ok "taps myc-2 et myc-3 attachés à br-myc"

# --- les DEUX IP servent en même temps -----------------------------------
simul_ok() {
  curl -fsS --max-time 2 -o /dev/null "http://$IP1:$PORT/" & C1=$!
  curl -fsS --max-time 2 -o /dev/null "http://$IP2:$PORT/" & C2=$!
  wait "$C1" 2>/dev/null && wait "$C2" 2>/dev/null
}
for round in 1 2 3 4 5; do
  simul_ok || fail "round $round : pas de 200 simultané sur $IP1 ET $IP2"
done
ok "5 rounds : 200 simultané sur $IP1 et $IP2"

# --- arrêt propre ---------------------------------------------------------
kill -TERM "$P1" "$P2" 2>/dev/null
S1=0; S2=0
wait "$P1" 2>/dev/null || S1=$?
wait "$P2" 2>/dev/null || S2=$?
[ "$S1" = 0 ] || { cat "$LOG1"; fail "VM1 ne s'est pas arrêtée proprement ($S1)"; }
[ "$S2" = 0 ] || { cat "$LOG2"; fail "VM2 ne s'est pas arrêtée proprement ($S2)"; }
ok "arrêt propre des deux VMs (stop() idempotent, exit 0)"

# --- zéro résidu (br-myc persiste, lui — docker0) -------------------------
RESIDUAL=0
if ip link 2>/dev/null | grep -Eq 'myc-[0-9]'; then RESIDUAL=1; echo "  résidu : tap myc-N" >&2; fi
if iptables -S 2>/dev/null | grep -Eq 'myc-[0-9]' || iptables -t nat -S 2>/dev/null | grep -Eq 'myc-[0-9]'
then RESIDUAL=1; echo "  résidu : règles iptables myc-N" >&2; fi
if ip neigh show 2>/dev/null | grep -Eq '192\.168\.77\.(2|3) '; then RESIDUAL=1; echo "  résidu : entrée neigh 192.168.77.2/3" >&2; fi
if ls -d /tmp/myc-microvm-* >/dev/null 2>&1; then RESIDUAL=1; echo "  résidu : state dir /tmp/myc-microvm-*" >&2; fi
if find /tmp/myc-leases-192.168.77.0 -mindepth 1 -maxdepth 1 -name 'myc-*' 2>/dev/null | grep -q .; then RESIDUAL=1; echo "  résidu : bail non libéré (registre)" >&2; fi
if pgrep -f /usr/local/bin/cloud-hypervisor >/dev/null 2>&1; then RESIDUAL=1; echo "  résidu : process cloud-hypervisor" >&2; fi
[ "$RESIDUAL" = 0 ] || fail "résidu détecté (voir ci-dessus)"
ok "zéro résidu : taps, iptables, neigh, bails, VMM"

if ip link show br-myc >/dev/null 2>&1; then
  ok "br-myc persiste après les runs (pattern docker0)"
else
  fail "br-myc a disparu — il doit persister entre les runs"
fi

echo "OK : concurrence 2 VMs validée ($IP1 + $IP2 simultanées, zéro résidu)"
