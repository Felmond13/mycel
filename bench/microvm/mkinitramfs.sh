#!/usr/bin/env bash
# Initramfs microVM : busybox + /init + benchhttp statique musl (le MÊME
# binaire que l'image Docker — comparaison à charge identique).
# Sortie : bench/microvm/assets/rootfs.cpio
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ASSETS="$HERE/assets"
BIN="$ASSETS/benchhttp"

[ -x /usr/bin/busybox ] || { echo "busybox manquant : apt-get install busybox-static" >&2; exit 1; }

# Charge : un fichier Rust std-only, compilé statique musl par rustc.
# (Re)compilé seulement si absent ou plus vieux que sa source — permet de
# générer l'initramfs en root alors que rustc vit dans l'environnement user.
if [ ! -x "$BIN" ] || [ "$HERE/benchhttp.rs" -nt "$BIN" ]; then
  rustc -O --target x86_64-unknown-linux-musl -o "$BIN" "$HERE/benchhttp.rs"
fi
file "$BIN" | grep -q static || { echo "benchhttp n'est pas statique" >&2; exit 1; }

STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$STAGE/bin" "$STAGE/proc" "$STAGE/sys" "$STAGE/dev" "$STAGE/etc"

cp /usr/bin/busybox "$STAGE/bin/busybox"
for applet in sh ip mount switch_root poweroff; do
  ln -sf busybox "$STAGE/bin/$applet"
done
cp "$BIN" "$STAGE/benchhttp"
chmod 755 "$STAGE/benchhttp"

# Nœuds /dev : sans /dev/console, les stderr de PID 1 sont perdus.
mknod -m 600 "$STAGE/dev/console" c 5 1 2>/dev/null || echo "AVERTISSEMENT : mknod impossible (non-root)"
mknod -m 666 "$STAGE/dev/null" c 1 3 2>/dev/null || true

cat > "$STAGE/init" <<'EOF'
#!/bin/sh
# Init microVM : montage minimal, eth0 en IP fixe (tap hôte 192.168.77.1/24
# créé par cloud-hypervisor), puis exec la charge — benchhttp devient PID 1.
mount -t proc proc /proc
mount -t sysfs sysfs /sys
ip addr add 192.168.77.2/24 dev eth0
ip link set eth0 up
ip link set lo up
exec /benchhttp
EOF
chmod 755 "$STAGE/init"

mkdir -p "$ASSETS"
(cd "$STAGE" && find . | cpio -o -H newc --quiet > "$ASSETS/rootfs.cpio")
echo "initramfs : $ASSETS/rootfs.cpio ($(stat -c%s "$ASSETS/rootfs.cpio") octets)"
sha256sum "$BIN" "$ASSETS/rootfs.cpio"
