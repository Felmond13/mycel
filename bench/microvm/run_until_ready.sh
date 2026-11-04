#!/usr/bin/env bash
# Un run élémentaire : T0 -> T1 (premier octet d'une réponse 200), puis
# nettoyage complet VÉRIFIÉ — tout résidu fait échouer le run.
# Usage : run_until_ready.sh <docker|microvm> <fichier-times|/dev/null>
#
# docker  : conteneur Docker de la même charge statique (127.0.0.1:18080).
# microvm : boot KVM via myc-backend/examples/boot (invité 192.168.77.2:8080 —
#           premier bail, déterministe — joint via le bridge br-myc, aucun
#           proxy userland).
set -u

SCN="$1"
OUT="$2"
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
BOOT="$REPO/target/release/examples/boot"
ASSETS="$HERE/assets"
PID=""
OK=0
URL=""

[ -x "$BOOT" ] || { echo "exemple boot manquant : $BOOT (cargo build --release --example boot)" >&2; exit 2; }

t0=$(date +%s%N)
case "$SCN" in
  docker)
    docker run --rm -d --name bench-http -p 127.0.0.1:18080:8080 benchhttp:microvm >/dev/null
    URL="http://127.0.0.1:18080/"
    ;;
  microvm)
    "$BOOT" "$ASSETS/rootfs.cpio" 2>/dev/null &
    PID=$!
    URL="http://192.168.77.2:8080/"

    # Réchauffe ICMP (anti-outlier ~9 s, cf. microvm-boot.md § v2) : le noyau
    # invité répond à l'ICMP echo dès qu'eth0 a son IP — aucun userspace
    # requis. On n'émet donc AUCUN SYN tant que l'invité n'a pas répondu à un
    # ping : un SYN parti avant le montage d'eth0 est jeté silencieusement
    # (entrée ARP permanente) et le backoff TCP le retarde de ~9 s. Flux
    # continu à 50 Hz plutôt que des ping one-shot : ici -W est en SECONDES
    # (iputils 20250605), un one-shot échoué bloque donc 1 s plein ; le flux
    # + grep -m1 détecte la première réponse en ~20 ms. Reprises tant que le
    # tap n'existe pas encore (ping sort aussitôt : réseau inaccessible).
    # T0 reste le lancement : cette phase est DANS le délai mesuré, comme le
    # reste du boot — elle n'est pas chronométrée séparément.
    WARM_DEADLINE=$((SECONDS + 10))
    while [ "$SECONDS" -lt "$WARM_DEADLINE" ]; do
      if timeout 1 ping -n -i 0.02 192.168.77.2 2>/dev/null | grep -m1 -q 'bytes from'; then
        break
      fi
      sleep 0.05
    done
    ;;
  *)
    echo "scenario inconnu : $SCN" >&2
    exit 2
    ;;
esac

for _ in $(seq 1 2000); do
  if curl -fsS --max-time 0.25 -o /dev/null "$URL" 2>/dev/null; then
    OK=1
    break
  fi
  sleep 0.005
done
t1=$(date +%s%N)

case "$SCN" in
  docker)
    docker rm -f bench-http >/dev/null 2>&1
    ;;
  microvm)
    if [ -n "$PID" ]; then
      kill -TERM "$PID" 2>/dev/null
      wait "$PID" 2>/dev/null
    fi
    ;;
esac

# Anti-fuite : conteneur, tap, VMM — tout résidu échoue le run. Le motif
# myc-[0-9] attrape les taps myc-2, myc-3… sans matcher le bridge br-myc
# (persistant entre les runs, comme docker0 — ce n'est PAS un résidu).
RESIDUAL=0
if docker ps -a --format '{{.Names}}' 2>/dev/null | grep -qx bench-http; then RESIDUAL=1; fi
if ip link 2>/dev/null | grep -Eq 'myc-[0-9]'; then RESIDUAL=1; fi
if pgrep -f /usr/local/bin/cloud-hypervisor >/dev/null 2>&1; then RESIDUAL=1; fi

if [ "$OK" -ne 1 ] || [ "$RESIDUAL" -ne 0 ]; then
  echo "run $SCN echoue : pret=$OK residuel=$RESIDUAL" >&2
  exit 1
fi

echo $(( t1 - t0 )) >> "$OUT"
