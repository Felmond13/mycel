#!/usr/bin/env bash
# Compile le noyau invité microVM « VMFS » : la config MINIMALE de
# build_kernel_slim.sh (allnoconfig + fragment explicite, tout intégré) plus
# VIRTIO_FS + FUSE_FS (voie B : partage virtiofs du rootfs, zéro copie),
# EXT4_FS (voie A : fallback par image disque ext4) et OVERLAY_FS (montage
# rw sur rootfs ro, réservé). C'est le noyau requis par le moteur
# kvm-microvm PROD (crates/myc-backend/src/kvm.rs) : le vmlinux-slim ne
# contient NI VIRTIO_FS NI EXT4 (vérifié : build-slim/.config).
# Sortie : bench/microvm/assets/vmlinux-vmfs (+ .version).
# N'ÉCRASE PAS assets/vmlinux-slim ni assets/vmlinux (comparaisons bench).
#
# Dérivé de build_kernel_slim.sh — mêmes règles d'hygiène : mrproper d'abord,
# PAS de merge_config.sh (append + olddefconfig, la dernière valeur gagne),
# builddir séparé (build-vmfs), vérification EXPLICITE des options critiques
# (olddefconfig retombe silencieusement à n quand une dépendance manque).
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ASSETS="$HERE/assets"
WORK=${WORK:-$HOME/build/mycel-kernel}
JOBS=$(nproc)
OUT_BUILD="$WORK/build-vmfs"

mkdir -p "$WORK"
cd "$WORK"

if [ -n "${KERNEL_VERSION:-}" ]; then
  echo "$KERNEL_VERSION" > .kernel_version_vmfs
elif [ ! -f .kernel_version_vmfs ]; then
  curl -fsSL https://cdn.kernel.org/pub/linux/kernel/v6.x/ \
    | grep -oE 'linux-6\.12\.[0-9]+\.tar\.xz' | sort -uV | tail -1 > .kernel_version_vmfs
fi
TARBALL=$(cat .kernel_version_vmfs)
VERSION=${TARBALL%.tar.xz}
echo "kernel: $VERSION"

if [ ! -d "$VERSION" ]; then
  [ -f "$TARBALL" ] || curl -fL -o "$TARBALL" "https://cdn.kernel.org/pub/linux/kernel/v6.x/$TARBALL"
  tar xf "$TARBALL"
fi
cd "$VERSION"

# Arbre propre (un arbre pollué casse le build hors-arbre).
make mrproper

# Config MINIMALE + fs invité. FUSE_FS : dépendance Kconfig dure de
# VIRTIO_FS (fs/fuse/Kconfig : "depends on FUSE_FS") — le protocole FUSE
# noyau seulement, /dev/fuse côté utilisateur n'est PAS requis pour
# virtiofs. EXT4_FS : voie A (root=/dev/vda). Le reste = copie du fragment
# slim (ACPI+IO-APIC pour les IRQ x86_64 CH, KVM_GUEST pour kvmclock,
# LZ4 pour la décompression, cmdline embarquée identique au VMM).
cat > "$WORK/vmfs.fragment" <<'EOF'
CONFIG_64BIT=y
CONFIG_SMP=y
CONFIG_RELOCATABLE=y
CONFIG_PRINTK=y
CONFIG_TTY=y
CONFIG_SERIAL_8250=y
CONFIG_SERIAL_8250_CONSOLE=y
CONFIG_PCI=y
CONFIG_PCI_MSI=y
CONFIG_NET=y
CONFIG_NETDEVICES=y
CONFIG_INET=y
CONFIG_UNIX=y
CONFIG_VIRTIO=y
CONFIG_VIRTIO_MENU=y
CONFIG_VIRTIO_PCI=y
CONFIG_VIRTIO_PCI_LEGACY=y
CONFIG_VIRTIO_NET=y
CONFIG_VIRTIO_BLK=y
CONFIG_VIRTIO_CONSOLE=y
CONFIG_VIRTIO_MMIO=y
CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES=y
CONFIG_BLOCK=y
CONFIG_BLK_DEV=y
CONFIG_BLK_DEV_INITRD=y
CONFIG_PROC_FS=y
CONFIG_SYSFS=y
CONFIG_DEVTMPFS=y
CONFIG_TMPFS=y
CONFIG_SHMEM=y
CONFIG_BINFMT_ELF=y
CONFIG_BINFMT_SCRIPT=y
CONFIG_MULTIUSER=y
CONFIG_FUTEX=y
CONFIG_EPOLL=y
CONFIG_SIGNALFD=y
CONFIG_TIMERFD=y
CONFIG_EVENTFD=y
CONFIG_POSIX_TIMERS=y
CONFIG_HIGH_RES_TIMERS=y
CONFIG_TICK_ONESHOT=y
CONFIG_HYPERVISOR_GUEST=y
CONFIG_PARAVIRT=y
CONFIG_KVM_GUEST=y
CONFIG_ACPI=y
CONFIG_X86_IO_APIC=y
CONFIG_CMDLINE_BOOL=y
CONFIG_CMDLINE="console=hvc0 quiet"
CONFIG_KERNEL_LZ4=y
# Voie B — virtiofs (objectif prod) : FUSE_FS est une dépendance de
# VIRTIO_FS ; DAX off (pas de CONFIG_VIRTIO_FS_DAX inutile en 1 vCPU).
CONFIG_FUSE_FS=y
CONFIG_VIRTIO_FS=y
# Voie A — ext4 par device (fallback garanti) : image mkfs.ext4 -d rootfs.
CONFIG_EXT4_FS=y
# Overlay rw sur rootfs ro (réservé, binds rw via partages dédiés en v1).
CONFIG_OVERLAY_FS=y
# Réductions explicites : allnoconfig OMET les lignes des symboles « default y »
# et olddefconfig les réactive alors par défaut — il faut un « not set »
# explicite pour les garder éteintes (chaîne SCHED_MC_PRIO/CPU_FREQ/THERMAL…).
# CONFIG_IPV6 is not set
# CONFIG_INET_DIAG is not set
# CONFIG_TCP_DIAG is not set
# CONFIG_SCHED_MC is not set
# CONFIG_CPU_FREQ is not set
# CONFIG_CPU_IDLE is not set
# CONFIG_THERMAL is not set
# CONFIG_ACPI_PROCESSOR is not set
# CONFIG_ACPI_THERMAL is not set
# CONFIG_PM is not set
# CONFIG_INTEL_MEI is not set
# CONFIG_I2C is not set
# CONFIG_PTP_1588_CLOCK is not set
# CONFIG_PPS is not set
# CONFIG_INPUT is not set
# CONFIG_RTC_CLASS is not set
# CONFIG_MAILBOX is not set
# CONFIG_POWER_SUPPLY is not set
EOF

make O="$OUT_BUILD" allnoconfig
cat "$WORK/vmfs.fragment" >> "$OUT_BUILD/.config"
make O="$OUT_BUILD" olddefconfig

# Vérification franche : olddefconfig retombe silencieusement à n sur une
# option dont une dépendance manque — pire que tout, un noyau livré sans
# VIRTIO_FS boote mais ne peut pas monter le partage (symptôme muet).
CRIT="CONFIG_64BIT CONFIG_PRINTK CONFIG_TTY CONFIG_SERIAL_8250 CONFIG_SERIAL_8250_CONSOLE \
CONFIG_PCI CONFIG_PCI_MSI CONFIG_NET CONFIG_NETDEVICES CONFIG_INET CONFIG_UNIX \
CONFIG_VIRTIO CONFIG_VIRTIO_MENU CONFIG_VIRTIO_PCI CONFIG_VIRTIO_NET CONFIG_VIRTIO_BLK CONFIG_VIRTIO_CONSOLE \
CONFIG_VIRTIO_MMIO CONFIG_BLOCK CONFIG_BLK_DEV_INITRD CONFIG_PROC_FS CONFIG_SYSFS \
CONFIG_DEVTMPFS CONFIG_TMPFS CONFIG_SHMEM CONFIG_BINFMT_ELF CONFIG_BINFMT_SCRIPT \
CONFIG_MULTIUSER CONFIG_FUTEX CONFIG_EPOLL CONFIG_SIGNALFD CONFIG_TIMERFD CONFIG_EVENTFD \
CONFIG_POSIX_TIMERS CONFIG_HIGH_RES_TIMERS CONFIG_HYPERVISOR_GUEST CONFIG_PARAVIRT \
CONFIG_KVM_GUEST CONFIG_ACPI CONFIG_CMDLINE_BOOL CONFIG_KERNEL_LZ4 \
CONFIG_FUSE_FS CONFIG_VIRTIO_FS CONFIG_EXT4_FS CONFIG_OVERLAY_FS"
MISSING=""
for opt in $CRIT; do
  grep -qx "$opt=y" "$OUT_BUILD/.config" || MISSING="$MISSING $opt"
done
[ -z "$MISSING" ] || { echo "options non appliquées par olddefconfig :$MISSING" >&2; exit 1; }

make O="$OUT_BUILD" -j"$JOBS" bzImage

mkdir -p "$ASSETS"
cp "$OUT_BUILD/arch/x86/boot/bzImage" "$ASSETS/vmlinux-vmfs"
UNAME_R=$(make O="$OUT_BUILD" -s kernelrelease)
echo "$VERSION vmfs (release $UNAME_R)" > "$ASSETS/vmlinux-vmfs.version"
SIZE=$(stat -c%s "$ASSETS/vmlinux-vmfs")
echo "guest kernel vmfs: $ASSETS/vmlinux-vmfs ($SIZE octets)"
if [ "$SIZE" -gt $((6 * 1024 * 1024)) ]; then
  echo "AVERTISSEMENT : bzImage > 6 Mo" >&2
fi
sha256sum "$ASSETS/vmlinux-vmfs"
cat "$ASSETS/vmlinux-vmfs.version"
