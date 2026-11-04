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
# Multi-arch : ARCH=x86_64 (défaut, inchangé) ou ARCH=aarch64 (build NATIF
# sur un hôte/runner arm64 — pas de cross ; make ARCH=arm64). Le noyau
# aarch64 sort sous assets/vmlinux-vmfs-aarch64 : les assets arm64 sont
# publiés en release, mais le moteur kvm-microvm n'est PAS câblé arm64
# (voir kvm.rs — aucun claim non mesuré, LOI du projet).
#
# Dérivé de build_kernel_slim.sh — mêmes règles d'hygiène : mrproper d'abord,
# PAS de merge_config.sh (append + olddefconfig, la dernière valeur gagne),
# builddir séparé (build-vmfs[-aarch64]), vérification EXPLICITE des options
# critiques (olddefconfig retombe silencieusement à n quand une dépendance
# manque).
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ASSETS="$HERE/assets"
WORK=${WORK:-$HOME/build/mycel-kernel}
JOBS=$(nproc)

# ------------------------------------------------------------------ arch ---
# ARCH=x86_64 par défaut : comportement historique au octet près (mêmes
# options, même sortie vmlinux-vmfs). ARCH=aarch64 : make ARCH=arm64, image
# « Image » (non compressée — bootable par tout loader), console PL011
# (la UART standard des machines virt arm64 ; 8250 n'y existe pas),
# cmdline embarquée « console=ttyAMA0 quiet » (arm64 n'a pas CMDLINE_BOOL :
# CMDLINE + CMDLINE_FROM_BOOTLOADER, cf. arch/arm64/Kconfig v6.12).
# VIRTIO_PCI/VIRTIO_MMIO sont arch-indépendants (drivers/virtio/Kconfig) :
# PCI pour cloud-hypervisor, MMIO pour les machines virt type QEMU.
ARCH=${ARCH:-x86_64}
case "$ARCH" in
  x86_64)
    KARCH=x86_64
    IMAGE_TARGET=bzImage
    IMAGE_PATH=arch/x86/boot/bzImage
    OUT=vmlinux-vmfs
    OUT_BUILD="$WORK/build-vmfs"
    WARN_MAX=$((6 * 1024 * 1024)) ;;
  aarch64)
    KARCH=arm64
    IMAGE_TARGET=Image
    IMAGE_PATH=arch/arm64/boot/Image
    OUT=vmlinux-vmfs-aarch64
    OUT_BUILD="$WORK/build-vmfs-aarch64"
    # Image arm64 non compressée = plus grosse qu'un bzImage LZ4 compressé.
    WARN_MAX=$((12 * 1024 * 1024)) ;;
  *)
    echo "ARCH invalide : '$ARCH' (attendu : x86_64 ou aarch64)" >&2; exit 1 ;;
esac

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
echo "kernel: $VERSION (arch: $ARCH)"

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
# virtiofs. EXT4_FS : voie A (root=/dev/vda). Le tronc commun ci-dessous
# est arch-indépendant ; les options spécifiques (paravirt x86, 8250,
# IO-APIC, cmdline bool vs string, PL011 arm64…) sont ajoutées par arch
# après le fragment commun.
cat > "$WORK/vmfs.fragment" <<'EOF'
CONFIG_64BIT=y
CONFIG_SMP=y
CONFIG_PRINTK=y
CONFIG_TTY=y
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
CONFIG_ACPI=y
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
# Les symboles inexistants sur une arch (INTEL_MEI sur arm64…) sont ignorés.
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

case "$ARCH" in
  x86_64)
    # Copie du bloc slim x86_64 : ACPI+IO-APIC pour les IRQ CH, KVM_GUEST
    # pour kvmclock, cmdline embarquée identique au VMM (console=hvc0 =
    # virtio-console).
    cat >> "$WORK/vmfs.fragment" <<'EOF'
CONFIG_RELOCATABLE=y
CONFIG_SERIAL_8250=y
CONFIG_SERIAL_8250_CONSOLE=y
CONFIG_HYPERVISOR_GUEST=y
CONFIG_PARAVIRT=y
CONFIG_KVM_GUEST=y
CONFIG_X86_IO_APIC=y
CONFIG_CMDLINE_BOOL=y
CONFIG_CMDLINE="console=hvc0 quiet"
EOF
    ;;
  aarch64)
    # arm64 : pas de PARAVIRT/KVM_GUEST (options x86, absentes de
    # arch/arm64/Kconfig). Console = PL011 (dépend de ARM_AMBA, bool caché
    # — drivers/tty/serial/Kconfig v6.12), la UART de la machine virt ;
    # VIRTIO_CONSOLE reste intégré (hvc0) pour le VMM. arm64 n'a pas
    # CMDLINE_BOOL : CMDLINE + CMDLINE_FROM_BOOTLOADER (cmdline du loader
    # prioritaire, cmdline embarquée en secours — même ceinture+bretelles
    # que x86).
    cat >> "$WORK/vmfs.fragment" <<'EOF'
CONFIG_ARM_AMBA=y
CONFIG_SERIAL_AMBA_PL011=y
CONFIG_SERIAL_AMBA_PL011_CONSOLE=y
CONFIG_CMDLINE="console=ttyAMA0 quiet"
CONFIG_CMDLINE_FROM_BOOTLOADER=y
EOF
    ;;
esac

make O="$OUT_BUILD" ARCH="$KARCH" allnoconfig
cat "$WORK/vmfs.fragment" >> "$OUT_BUILD/.config"
make O="$OUT_BUILD" ARCH="$KARCH" olddefconfig

# Vérification franche : olddefconfig retombe silencieusement à n sur une
# option dont une dépendance manque — pire que tout, un noyau livré sans
# VIRTIO_FS boote mais ne peut pas monter le partage (symptôme muet).
CRIT_COMMON="CONFIG_64BIT CONFIG_PRINTK CONFIG_TTY \
CONFIG_PCI CONFIG_PCI_MSI CONFIG_NET CONFIG_NETDEVICES CONFIG_INET CONFIG_UNIX \
CONFIG_VIRTIO CONFIG_VIRTIO_MENU CONFIG_VIRTIO_PCI CONFIG_VIRTIO_NET CONFIG_VIRTIO_BLK CONFIG_VIRTIO_CONSOLE \
CONFIG_VIRTIO_MMIO CONFIG_BLOCK CONFIG_BLK_DEV_INITRD CONFIG_PROC_FS CONFIG_SYSFS \
CONFIG_DEVTMPFS CONFIG_TMPFS CONFIG_SHMEM CONFIG_BINFMT_ELF CONFIG_BINFMT_SCRIPT \
CONFIG_MULTIUSER CONFIG_FUTEX CONFIG_EPOLL CONFIG_SIGNALFD CONFIG_TIMERFD CONFIG_EVENTFD \
CONFIG_POSIX_TIMERS CONFIG_HIGH_RES_TIMERS CONFIG_ACPI CONFIG_KERNEL_LZ4 \
CONFIG_FUSE_FS CONFIG_VIRTIO_FS CONFIG_EXT4_FS CONFIG_OVERLAY_FS"
case "$ARCH" in
  x86_64)
    CRIT_ARCH="CONFIG_SERIAL_8250 CONFIG_SERIAL_8250_CONSOLE \
CONFIG_HYPERVISOR_GUEST CONFIG_PARAVIRT CONFIG_KVM_GUEST CONFIG_X86_IO_APIC CONFIG_CMDLINE_BOOL" ;;
  aarch64)
    CRIT_ARCH="CONFIG_ARM_AMBA CONFIG_SERIAL_AMBA_PL011 CONFIG_SERIAL_AMBA_PL011_CONSOLE \
CONFIG_CMDLINE_FROM_BOOTLOADER" ;;
esac
MISSING=""
for opt in $CRIT_COMMON $CRIT_ARCH; do
  grep -qx "$opt=y" "$OUT_BUILD/.config" || MISSING="$MISSING $opt"
done
# La cmdline embarquée est une string (pas vérifiable par « =y ») : grep
# dédié — une cmdline vide rendrait le guest muet sans échec de build.
grep -q '^CONFIG_CMDLINE="console=' "$OUT_BUILD/.config" \
  || MISSING="$MISSING CONFIG_CMDLINE(string)"
[ -z "$MISSING" ] || { echo "options non appliquées par olddefconfig :$MISSING" >&2; exit 1; }

make O="$OUT_BUILD" ARCH="$KARCH" -j"$JOBS" "$IMAGE_TARGET"

mkdir -p "$ASSETS"
cp "$OUT_BUILD/$IMAGE_PATH" "$ASSETS/$OUT"
UNAME_R=$(make O="$OUT_BUILD" ARCH="$KARCH" -s kernelrelease)
# Contenu .version inchangé pour x86_64 (contrat historique) ; l'arch est
# marquée dans le nom de fichier côté aarch64.
case "$ARCH" in
  x86_64) echo "$VERSION vmfs (release $UNAME_R)" > "$ASSETS/$OUT.version" ;;
  *)      echo "$VERSION vmfs $ARCH (release $UNAME_R)" > "$ASSETS/$OUT.version" ;;
esac
SIZE=$(stat -c%s "$ASSETS/$OUT")
echo "guest kernel vmfs: $ASSETS/$OUT ($SIZE octets)"
if [ "$SIZE" -gt "$WARN_MAX" ]; then
  echo "AVERTISSEMENT : $IMAGE_TARGET > $((WARN_MAX / 1024 / 1024)) Mo ($ARCH)" >&2
fi
sha256sum "$ASSETS/$OUT"
cat "$ASSETS/$OUT.version"
