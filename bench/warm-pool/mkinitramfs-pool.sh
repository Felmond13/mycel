#!/usr/bin/env bash
# Initramfs POOL CHAUD (bench/warm-pool).
#
# Même recette que bench/microvm/mkinitramfs.sh (busybox + benchhttp statique
# musl), avec UNE différence : l'IP invité n'est PAS codée en dur — /init la
# lit dans la cmdline noyau (guest_ip=192.168.8X.2), ce qui permet à UN seul
# initramfs de servir N VMs de pool, chacune sur son propre tap/subnet.
# Sortie : bench/warm-pool/assets/rootfs-pool.cpio
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ASSETS="$HERE/assets"
MICROVM="$HERE/../microvm"
BIN="$MICROVM/assets/benchhttp"

[ -x /usr/bin/busybox ] || { echo "busybox manquant : apt-get install busybox-static" >&2; exit 1; }
[ -x "$BIN" ] || {
  echo "benchhttp manquant : $BIN" >&2
  echo "lance d'abord bench/microvm/mkinitramfs.sh" >&2
  exit 1
}

STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$STAGE/bin" "$STAGE/proc" "$STAGE/sys" "$STAGE/dev" "$STAGE/etc"

cp /usr/bin/busybox "$STAGE/bin/busybox"
for applet in sh ip mount grep cut poweroff; do
  ln -sf busybox "$STAGE/bin/$applet"
done
cp "$BIN" "$STAGE/benchhttp"
chmod 755 "$STAGE/benchhttp"

mknod -m 600 "$STAGE/dev/console" c 5 1 2>/dev/null || echo "AVERTISSEMENT : mknod impossible (non-root)"
mknod -m 666 "$STAGE/dev/null" c 1 3 2>/dev/null || true

cat > "$STAGE/init" <<'EOF'
#!/bin/sh
# Init microVM de pool : IP lue dans la cmdline (guest_ip=), défaut identique
# à bench/microvm (192.168.77.2) pour rester bootable hors pool.
mount -t proc proc /proc
mount -t sysfs sysfs /sys
GIP=$(grep -o 'guest_ip=[0-9.]*' /proc/cmdline | head -1 | cut -d= -f2)
[ -n "$GIP" ] || GIP=192.168.77.2
ip addr add "$GIP/24" dev eth0
ip link set eth0 up
ip link set lo up
exec /benchhttp
EOF
chmod 755 "$STAGE/init"

mkdir -p "$ASSETS"
(cd "$STAGE" && find . | cpio -o -H newc --quiet > "$ASSETS/rootfs-pool.cpio")
echo "initramfs pool : $ASSETS/rootfs-pool.cpio ($(stat -c%s "$ASSETS/rootfs-pool.cpio") octets)"
sha256sum "$BIN" "$ASSETS/rootfs-pool.cpio"
