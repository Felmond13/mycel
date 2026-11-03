#!/usr/bin/env bash
# Compile le noyau invité microVM : Linux LTS 6.12.x + fragment virtio TOUT
# INTÉGRÉ (pas de modules). Sortie : bench/microvm/assets/vmlinux (+ .version).
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ASSETS="$HERE/assets"
WORK=${WORK:-$HOME/build/mycel-kernel}
JOBS=$(nproc)

mkdir -p "$WORK"
cd "$WORK"

if [ ! -f .kernel_version ]; then
  curl -fsSL https://cdn.kernel.org/pub/linux/kernel/v6.x/ \
    | grep -oE 'linux-6\.12\.[0-9]+\.tar\.xz' | sort -uV | tail -1 > .kernel_version
fi
TARBALL=$(cat .kernel_version)
VERSION=${TARBALL%.tar.xz}
echo "kernel: $VERSION"

if [ ! -d "$VERSION" ]; then
  [ -f "$TARBALL" ] || curl -fL -o "$TARBALL" "https://cdn.kernel.org/pub/linux/kernel/v6.x/$TARBALL"
  tar xf "$TARBALL"
fi
cd "$VERSION"

# mrproper d'abord : un arbre source pollué casse le build hors-arbre
# (« The source tree is not clean », Makefile:648). Et PAS de
# merge_config.sh (il appelle make SANS O= et pollue l'arbre) : append +
# olddefconfig, la dernière valeur gagne.
make mrproper
cat > "$WORK/virtio.fragment" <<'EOF'
CONFIG_VIRTIO=y
CONFIG_VIRTIO_PCI=y
CONFIG_VIRTIO_PCI_LEGACY=y
CONFIG_VIRTIO_NET=y
CONFIG_VIRTIO_BLK=y
CONFIG_VIRTIO_CONSOLE=y
CONFIG_VIRTIO_MMIO=y
CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES=y
CONFIG_HW_RANDOM_VIRTIO=y
CONFIG_SERIAL_8250=y
CONFIG_SERIAL_8250_CONSOLE=y
CONFIG_KVM_GUEST=y
CONFIG_PARAVIRT=y
EOF

make O="$WORK/build" defconfig
cat "$WORK/virtio.fragment" >> "$WORK/build/.config"
make O="$WORK/build" olddefconfig
make O="$WORK/build" -j"$JOBS" bzImage

mkdir -p "$ASSETS"
cp "$WORK/build/arch/x86/boot/bzImage" "$ASSETS/vmlinux"
UNAME_R=$(make O="$WORK/build" -s kernelrelease)
echo "$VERSION (release $UNAME_R)" > "$ASSETS/vmlinux.version"
echo "guest kernel: $ASSETS/vmlinux"
sha256sum "$ASSETS/vmlinux"
cat "$ASSETS/vmlinux.version"
