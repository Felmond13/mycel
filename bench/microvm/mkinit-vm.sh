#!/usr/bin/env bash
# Initramfs « VM » du moteur kvm-microvm (crates/myc-backend/src/kvm.rs) :
# busybox + un /init GÉNÉRIQUE qui monte le rootfs réel de l'environnement —
# voie B : partage virtiofs « mycroot » (zéro copie), voie A : image ext4
# /dev/vda (fallback) — puis switch_root vers /myc/init-guest (stage 2,
# écrit par l'hôte DANS le rootfs avec /myc/run.env ; ce script ne contient
# donc AUCUNE connaissance de la charge).
# Sortie : bench/microvm/assets/init-vm.cpio
# Option --install : copie aussi kernel vmfs + initramfs vers
# /usr/local/share/mycel (chemins par défaut du moteur, root requis).
#
# Différences avec mkinitramfs.sh (bench) : pas de benchhttp, pas d'IP fixe
# codée en dur (mycip= sur la cmdline), montage du vrai rootfs + switch_root.
#
# Branche mycagent=1 (chemin snapshot Firecracker) : le rootfs ext4 est
# monté LECTURE SEULE (l'image est un cache partagé entre les runs, clé par
# empreinte) et recouvert d'un overlay tmpfs rw — /myc/init-guest y est
# l'« agent » qui attend la commande sur le disque scratch (/dev/vdb),
# réécrit par l'hôte pendant que la VM est en pause. Le même cpio sert les
# deux chemins (kvm.rs ajoute mycagent=1 à la cmdline pour Firecracker).
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ASSETS="$HERE/assets"
OUT="$ASSETS/init-vm.cpio"

[ -x /usr/bin/busybox ] || { echo "busybox manquant : apt-get install busybox-static" >&2; exit 1; }

STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$STAGE/bin" "$STAGE/proc" "$STAGE/sys" "$STAGE/dev" "$STAGE/newroot"

cp /usr/bin/busybox "$STAGE/bin/busybox"
# Applets utilisés par /init (stage 1) — généreux plutôt que minimal.
# blockdev (l'agent invalide le cache page du scratch entre deux lectures)
# et reboot (Firecracker sort au reboot du guest, il n'a pas d'ACPI S5).
for applet in sh mount switch_root mkdir cat sleep ip grep printf sync poweroff reboot dd hostname awk sed ls mknod chmod blockdev; do
  ln -sf busybox "$STAGE/bin/$applet"
done

# Nœuds /dev précoces : sans /dev/console, les stderr de PID 1 sont perdus
# avant le devtmpfs.
mknod -m 600 "$STAGE/dev/console" c 5 1 2>/dev/null || echo "AVERTISSEMENT : mknod impossible (non-root)"
mknod -m 666 "$STAGE/dev/null" c 1 3 2>/dev/null || true

cat > "$STAGE/init" <<'EOF'
#!/bin/sh
# myc microVM guest init — stage 1 (initramfs générique, sans connaissance
# de la charge). Monte /proc /sys /dev, attend et monte le rootfs réel
# (mycroot=virtiofs|ext4 sur la cmdline kernel), configure l'IP fixe guest
# si mycip= est présent (ports publiés : DNAT hôte vers cette IP), puis
# switch_root vers /myc/init-guest (stage 2). Boucles de réessai courtes :
# les devices virtio peuvent apparaître un instant après /init.
export PATH=/bin

mkdir -p /proc /sys /dev /newroot
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev 2>/dev/null
ip link set lo up 2>/dev/null

root="virtiofs"
ipaddr=""
agent=0
for x in $(cat /proc/cmdline); do
  case "$x" in
    mycroot=*) root=${x#mycroot=} ;;
    mycip=*) ipaddr=${x#mycip=} ;;
    mycagent=1) agent=1 ;;
  esac
done

if [ -n "$ipaddr" ]; then
  i=0
  while [ "$i" -lt 200 ] && ! ip link set eth0 up 2>/dev/null; do
    sleep 0.05; i=$((i+1))
  done
  ip addr add "$ipaddr/24" dev eth0 2>/dev/null
fi

# ---- chemin snapshot Firecracker : ext4 RO + overlay tmpfs RW ------------
# L'image rootfs est un cache partagé (jamais écrite par les guests) ; le
# tmpfs /upper sert de upperdir/workdir à l'overlay. busybox switch_root ne
# supprime pas les points de montage (test st_dev) : le tmpfs et l'ext4
# restent vivants sous la nouvelle racine, référencés par l'overlay.
if [ "$agent" = "1" ]; then
  i=0
  while [ "$i" -lt 300 ] && ! [ -b /dev/vda ]; do
    sleep 0.05; i=$((i+1))
  done
  mkdir -p /upper /newroot2
  if [ -b /dev/vda ] \
     && mount -t ext4 -o ro /dev/vda /newroot 2>/dev/null \
     && mount -t tmpfs tmpfs /upper 2>/dev/null \
     && mkdir -p /upper/up /upper/work \
     && mount -t overlay overlay \
          -o lowerdir=/newroot,upperdir=/upper/up,workdir=/upper/work \
          /newroot2 2>/dev/null; then
    mkdir -p /newroot2/proc /newroot2/sys /newroot2/dev /newroot2/tmp /newroot2/run 2>/dev/null
    exec switch_root /newroot2 /myc/init-guest
  fi
  echo "myc-init: agent rootfs illisible (ext4/overlay, /dev/vda=$([ -b /dev/vda ] && echo oui || echo non))" > /dev/console
  reboot -f
fi

mounted=""
i=0
while [ "$i" -lt 300 ]; do
  if [ "$root" = "ext4" ]; then
    # rw : image jetable par run ; les images OCI n'embarquent pas /dev,
    # /proc… et le guest doit pouvoir les créer (mkdir impossible en ro).
    if [ -b /dev/vda ] && mount -t ext4 /dev/vda /newroot 2>/dev/null; then
      mounted="ext4"; break
    fi
  else
    if mount -t virtiofs mycroot /newroot 2>/dev/null; then
      mounted="virtiofs"; break
    fi
  fi
  sleep 0.05
  i=$((i+1))
done

if [ -z "$mounted" ]; then
  echo "myc-init: rootfs introuvable (mycroot=$root)" > /dev/console
  poweroff -f
fi

# Les répertoires de montage peuvent manquer dans l'image (OCI ne livre
# pas /dev) — les créer ici, quand le rootfs est encore inscriptible
# (ext4 rw ou partage virtiofs rw), avant le switch_root.
mkdir -p /newroot/proc /newroot/sys /newroot/dev /newroot/tmp /newroot/run 2>/dev/null

exec switch_root /newroot /myc/init-guest
EOF
chmod 755 "$STAGE/init"

(cd "$STAGE" && find . | cpio -o -H newc --quiet > "$OUT")
echo "initramfs vm : $OUT ($(stat -c%s "$OUT") octets)"
sha256sum "$OUT"

if [ "${1:-}" = "--install" ]; then
  DST=/usr/local/share/mycel
  [ "$(id -u)" -eq 0 ] || { echo "--install requiert root" >&2; exit 1; }
  mkdir -p "$DST"
  cp "$OUT" "$DST/init-vm.cpio"
  cp "$ASSETS/vmlinux-vmfs" "$DST/vmlinux-vmfs"
  echo "installé : $DST/init-vm.cpio + $DST/vmlinux-vmfs"
fi
