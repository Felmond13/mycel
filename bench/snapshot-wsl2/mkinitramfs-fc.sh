#!/usr/bin/env bash
# Initramfs VARIANTE FIRECRACKER (bench/snapshot-wsl2).
#
# Même recette que bench/microvm/mkinitramfs.sh (busybox + benchhttp statique
# musl — la MÊME charge), avec deux différences imposées par Firecracker :
#   1. IP invité 192.168.78.2/24 (tap hôte mycf0 192.168.78.1/24, subnet
#      dédié au bench FC — ne touche pas au 192.168.77.x de bench/microvm).
#   2. Sortie PID 1 redirigée vers /dev/ttyS0 : la console Firecracker est un
#      UART 8250 (pas de virtio-console hvc0), et le noyau slim embarque
#      CONFIG_CMDLINE="console=hvc0 quiet" qui serait AJOUTÉ en fin de
#      cmdline — /dev/console pointerait alors sur hvc0 (inexistant sous FC)
#      et le battement de cœur de benchhttp serait perdu.
# Sortie : bench/snapshot-wsl2/assets/rootfs-fc.cpio
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ASSETS="$HERE/assets"
MICROVM="$HERE/../microvm"
BIN="$MICROVM/assets/benchhttp"

[ -x /usr/bin/busybox ] || { echo "busybox manquant : apt-get install busybox-static" >&2; exit 1; }
[ -x "$BIN" ] || {
  echo "benchhttp manquant : $BIN" >&2
  echo "lance d'abord bench/microvm/mkinitramfs.sh (ne modifie pas bench/microvm ici)" >&2
  exit 1
}

STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$STAGE/bin" "$STAGE/proc" "$STAGE/sys" "$STAGE/dev" "$STAGE/etc"

cp /usr/bin/busybox "$STAGE/bin/busybox"
for applet in sh ip mount poweroff; do
  ln -sf busybox "$STAGE/bin/$applet"
done
cp "$BIN" "$STAGE/benchhttp"
chmod 755 "$STAGE/benchhttp"

# Nœuds /dev : sans /dev/console, les stderr de PID 1 sont perdus.
mknod -m 600 "$STAGE/dev/console" c 5 1 2>/dev/null || echo "AVERTISSEMENT : mknod impossible (non-root)"
mknod -m 666 "$STAGE/dev/null" c 1 3 2>/dev/null || true

cat > "$STAGE/init" <<'EOF'
#!/bin/sh
# Init microVM Firecracker : montage minimal, eth0 en IP fixe 192.168.78.2
# (tap hôte mycf0 192.168.78.1/24 créé par le script hôte), sortie PID 1
# forcée sur ttyS0 (cf. en-tête du script), puis exec de la charge —
# benchhttp devient PID 1.
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev 2>/dev/null || true
[ -c /dev/ttyS0 ] || mknod -m 660 /dev/ttyS0 c 4 64 2>/dev/null || true
ip addr add 192.168.78.2/24 dev eth0
ip link set eth0 up
ip link set lo up
exec >/dev/ttyS0 2>&1
exec /benchhttp
EOF
chmod 755 "$STAGE/init"

mkdir -p "$ASSETS"
(cd "$STAGE" && find . | cpio -o -H newc --quiet > "$ASSETS/rootfs-fc.cpio")
echo "initramfs FC : $ASSETS/rootfs-fc.cpio ($(stat -c%s "$ASSETS/rootfs-fc.cpio") octets)"
sha256sum "$BIN" "$ASSETS/rootfs-fc.cpio"
