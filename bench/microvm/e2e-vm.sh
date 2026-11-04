#!/usr/bin/env bash
# e2e-vm.sh — validation END-TO-END rejouable du moteur kvm-microvm
# (crates/myc-backend/src/kvm.rs). Chaque critère « prod » est testé, et le
# script ÉCHOUE si un run laisse un résidu (processus VMM/virtiofsd, tap
# myc-*, règles iptables, entrée ARP du lease, répertoire d'état).
#
# Les assertions de résidu sont DELTA : le pont `br-myc` persiste entre les
# runs (design docker0) et d'autres runs/agents peuvent occuper la machine
# en parallèle — on vérifie donc qu'un test ne LAISSE RIEN DE PLUS qu'au
# moment où il a démarré.
#
#   bash bench/microvm/e2e-vm.sh              # tests sans réseau (user ok)
#   bash bench/microvm/e2e-vm.sh --with-ports # + ports publiés (ROOT requis,
#                                             # et MYCEL_STORE si le store
#                                             # vit dans le HOME d'un user)
#
# Exemple complet en root (WSL) :
#   wsl -d Ubuntu -u root -- env MYCEL_STORE=/home/<user>/.local/share/mycel \
#       bash /home/<user>/mycel/bench/microvm/e2e-vm.sh --with-ports
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
MYC="$REPO/target/debug/myc"
WITH_PORTS=0
[ "${1:-}" = "--with-ports" ] && WITH_PORTS=1

PASS=0; FAIL=0
ERRLOG=$(mktemp /tmp/myc-e2e-err.XXXX)
trap 'rm -f "$ERRLOG"' EXIT
say()  { printf '%s\n' "$*"; }
ok()   { PASS=$((PASS+1)); say "PASS  $*"; }
bad()  { FAIL=$((FAIL+1)); say "FAIL  $*"; }

[ -x "$MYC" ] || { say "myc manquant : cargo build -p myc-cli"; exit 2; }

# ---------------------------------------------------------------- résidus
# NB: pgrep -c / grep -c impriment déjà « 0 » en cas d'absence (avec exit 1)
# — pas de « || echo 0 » qui doublerait la ligne.
n_ch()   { local n; n=$(pgrep -c -f '[c]loud-hypervisor' 2>/dev/null); echo "${n:-0}"; }
n_vfsd() { local n; n=$(pgrep -c -f '[v]irtiofsd' 2>/dev/null); echo "${n:-0}"; }
n_taps() { local n; n=$(ip -o link show 2>/dev/null | grep -c ' myc-[0-9]*:'); echo "${n:-0}"; }
n_arp()  { local n; n=$(ip neigh show 2>/dev/null | grep -cE '192\.168\.7[0-9]\.[0-9]+'); echo "${n:-0}"; }
n_ipt()  { local n; n=$({ iptables -t nat -S 2>/dev/null; iptables -S 2>/dev/null; } | grep -c 'myc-'); echo "${n:-0}"; }
n_st()   { ls -d /tmp/myc-vm-* 2>/dev/null | wc -l; }

snap() { echo "$(n_ch) $(n_vfsd) $(n_taps) $(n_arp) $(n_ipt) $(n_st)"; }
BASE="$(snap)"

# Vérifie que l'état global est revenu à (ou sous) la ligne de base.
check_clean() {
    local label="$1" dirty=0
    local now; now="$(snap)"
    if [ "$(echo "$now" | awk '{print $1}')" -gt "$(echo "$BASE" | awk '{print $1}')" ]; then dirty=1; say "  résidu: process cloud-hypervisor"; fi
    if [ "$(echo "$now" | awk '{print $2}')" -gt "$(echo "$BASE" | awk '{print $2}')" ]; then dirty=1; say "  résidu: process virtiofsd"; fi
    if [ "$(echo "$now" | awk '{print $3}')" -gt "$(echo "$BASE" | awk '{print $3}')" ]; then dirty=1; say "  résidu: tap myc-*"; fi
    if [ "$(echo "$now" | awk '{print $4}')" -gt "$(echo "$BASE" | awk '{print $4}')" ]; then dirty=1; say "  résidu: entrée ARP lease"; fi
    if [ "$(echo "$now" | awk '{print $5}')" -gt "$(echo "$BASE" | awk '{print $5}')" ]; then dirty=1; say "  résidu: règles iptables myc-*"; fi
    if [ "$(echo "$now" | awk '{print $6}')" -gt "$(echo "$BASE" | awk '{print $6}')" ]; then dirty=1; say "  résidu: state dir /tmp/myc-vm-*"; fi
    if [ "$dirty" = 0 ]; then ok "$label : aucun résidu (VMM, virtiofsd, tap, DNAT, ARP, /tmp)"; else bad "$label : résidu détecté (base=$BASE maintenant=$now)"; fi
}

# run_case CODE_ATTENDU SORTIE_ATTENDUE(""-ignorée si "-") label args...-- cmd...
run_case() {
    local want_code="$1" want_out="$2" label="$3"; shift 3
    local out code
    out=$(timeout 120 "$MYC" run --engine kvm-microvm "$@" 2>"$ERRLOG")
    code=$?
    if [ "$code" = "$want_code" ]; then ok "$label : code de sortie $code"; else bad "$label : code $code != $want_code"; fi
    if [ "$want_out" != "-" ] && [ "$out" = "$want_out" ]; then
        ok "$label : sortie « $out »"
    elif [ "$want_out" != "-" ]; then
        bad "$label : sortie « $out » != « $want_out »"
    fi
    check_clean "$label"
}

say "=== moteur kvm-microvm — e2e ($(date -u +%FT%TZ)) ==="
say "myc: $MYC"
[ -n "${MYCEL_STORE:-}" ] && say "MYCEL_STORE=$MYCEL_STORE"
say "ligne de base résidus: $BASE"

# 1. commande du manifeste + sortie stdout + code 0 (voie B virtiofs)
run_case 0 "hello" "1. echo (voie B virtiofs)" alpine:3.20 -- echo hello

# 2. variables d'environnement transmises (-e)
run_case 0 "v=custom1" "2. env -e" -e MYC_E2E=custom1 alpine:3.20 -- sh -c 'echo v=$MYC_E2E'

# 3. bind monté dans l'invité (-v, ro) : contenu visible + écriture refusée
BD=$(mktemp -d /tmp/myc-e2e-bind.XXXX)
echo "bind-data-42" > "$BD/file.txt"
run_case 0 "bind-data-42" "3a. bind -v ro : lecture" -v "$BD:/data:ro" alpine:3.20 -- cat /data/file.txt
run_case 1 "-" "3b. bind -v ro : écriture refusée" -v "$BD:/data:ro" alpine:3.20 -- sh -c 'echo x > /data/f'
rm -rf "$BD"

# 4. code de sortie non-zéro propagé depuis l'invité
run_case 7 "" "4. exit 7 propagé" alpine:3.20 -- sh -c "exit 7"
run_case 42 "" "4b. exit 42 propagé" alpine:3.20 -- sh -c "exit 42"

# 5. workdir + hostname appliqués
run_case 0 "/data" "5. workdir" -w /data alpine:3.20 -- pwd
run_case 0 "vm-e2e" "5b. hostname" --hostname vm-e2e alpine:3.20 -- hostname

# 6. voie A (fallback ext4) : même commande, zéro virtiofsd
out=$(MYCEL_VM_ROOTFS=ext4 timeout 120 "$MYC" run --engine kvm-microvm alpine:3.20 -- echo hello 2>"$ERRLOG"); code=$?
if [ "$code" = 0 ] && [ "$out" = "hello" ]; then ok "6. echo (voie A ext4) : sortie+code"; else bad "6. echo (voie A ext4) : code=$code sortie=« $out »"; fi
check_clean "6. voie A"

# 7. ports publiés (root) : redis -p 6399:6379 + PING depuis 127.0.0.1
if [ "$WITH_PORTS" = 1 ]; then
    if [ "$(id -u)" -ne 0 ]; then
        say "SKIP  7. ports publiés (requiert root : relancer avec --with-ports en root)"
    else
        REDISLOG=$(mktemp /tmp/myc-e2e-redis.XXXX)
        MYCEL_VM_TIMEOUT_SECS=120 "$MYC" run --engine kvm-microvm -p 6399:6379 redis:7-alpine >"$REDISLOG" 2>&1 &
        RPID=$!
        pong=""
        for _ in $(seq 1 90); do
            if exec 3<>/dev/tcp/127.0.0.1/6399 2>/dev/null; then
                printf 'PING\r\n' >&3
                # Une LIGNE : head -c bloque quand redis garde la connexion
                # ouverte, et la connexion ne s'établit parfois qu'à ~59 s —
                # boucle élargie à 90 (diagnostiqué 2026-11-04).
                IFS= read -r -t 4 pong <&3 || pong=""
                pong=${pong%$'\r'}
                exec 3>&-
                [ "$pong" = "+PONG" ] && break
                pong=""
            fi
            sleep 1
        done
        if [ "$pong" = "+PONG" ]; then ok "7. redis -p 6399:6379 : PING→+PONG via 127.0.0.1"; else
            bad "7. redis : réponse « $pong »"
            mkdir -p "$HERE/results"
            cp "$REDISLOG" "$HERE/results/e2e-redis-failed.log" 2>/dev/null || true
            say "      log conservé : $HERE/results/e2e-redis-failed.log"
        fi
        kill -TERM "$RPID" 2>/dev/null
        wait "$RPID" 2>/dev/null
        rm -f "$REDISLOG"
        check_clean "7. redis (arrêt SIGTERM)"
    fi
else
    say "SKIP  7. ports publiés (--with-ports non demandé)"
fi

say ""
say "=== résultat : $PASS PASS, $FAIL FAIL ==="
[ "$FAIL" = 0 ]
