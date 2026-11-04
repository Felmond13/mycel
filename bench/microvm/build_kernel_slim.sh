#!/usr/bin/env bash
# Compile le noyau invité microVM ALLÉGÉ : Linux 6.12.x en config MINIMALE
# (allnoconfig + fragment explicite), tout intégré, aucun module. Objectif :
# bzImage ≤ 6 Mo (le defconfig+virtio de build_kernel.sh pèse ~14 Mo) qui
# boote quand même la charge benchhttp (smoke : run_until_ready.sh microvm).
# Sortie : bench/microvm/assets/vmlinux-slim (+ .version).
# N'ÉCRASE PAS assets/vmlinux — l'original est gardé pour comparer avant/après.
#
# Différences avec build_kernel.sh : allnoconfig (au lieu de defconfig) +
# fragment minimal (au lieu de tout-virtio), builddir séparé (build-slim).
# KERNEL_VERSION=linux-6.12.115.tar.xz permet d'épingler la même version que
# le noyau stock (comparaison propre) ; par défaut : dernière 6.12.x publiée.
# Mêmes règles d'hygiène : mrproper d'abord, PAS de merge_config.sh (append +
# olddefconfig, la dernière valeur gagne — merge_config pollue l'arbre).
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ASSETS="$HERE/assets"
WORK=${WORK:-$HOME/build/mycel-kernel}
JOBS=$(nproc)
OUT_BUILD="$WORK/build-slim"

mkdir -p "$WORK"
cd "$WORK"

if [ -n "${KERNEL_VERSION:-}" ]; then
  echo "$KERNEL_VERSION" > .kernel_version_slim
elif [ ! -f .kernel_version_slim ]; then
  curl -fsSL https://cdn.kernel.org/pub/linux/kernel/v6.x/ \
    | grep -oE 'linux-6\.12\.[0-9]+\.tar\.xz' | sort -uV | tail -1 > .kernel_version_slim
fi
TARBALL=$(cat .kernel_version_slim)
VERSION=${TARBALL%.tar.xz}
echo "kernel: $VERSION"

if [ ! -d "$VERSION" ]; then
  [ -f "$TARBALL" ] || curl -fL -o "$TARBALL" "https://cdn.kernel.org/pub/linux/kernel/v6.x/$TARBALL"
  tar xf "$TARBALL"
fi
cd "$VERSION"

# Arbre propre (un arbre pollué casse le build hors-arbre, cf. build_kernel.sh).
make mrproper

# Config MINIMALE. Le cpio invité est NON compressé → pas de RD_GZIP ;
# rootfs = initramfs → pas d'EXT4 ; pas d'IPV6. ACPI + IO-APIC : ils sont
# seuls à décrire les interruptions sur x86_64 Cloud Hypervisor (pas de PIC
# 8259 émulé). KVM_GUEST/PARAVIRT : kvmclock. LZ4 : décompression la plus
# rapide au boot. CMDLINE embarquée = même cmdline que le VMM (append sans
# danger) — le noyau reste amorçable sans cmdline passée.
cat > "$WORK/slim.fragment" <<'EOF'
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
# Réductions explicites : allnoconfig OMET les lignes des symboles « default y »
# et olddefconfig les réactive alors par défaut — il faut un « not set »
# explicite pour les garder éteintes. Chaîne typique : SCHED_MC_PRIO
# (default y avec SMP) sélectionne CPU_FREQ + X86_INTEL_PSTATE ; ACPI_THERMAL
# sélectionne THERMAL ; ACPI_PROCESSOR tire cpuidle. Rien de tout ça n'est
# requis pour booter une microVM de bench (1 vCPU, pas de gestion d'énergie).
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
cat "$WORK/slim.fragment" >> "$OUT_BUILD/.config"
make O="$OUT_BUILD" olddefconfig

# olddefconfig retombe silencieusement à n sur une option dont une dépendance
# manque — vérifier les critiques et échouer FRANCHEMENT plutôt que livrer un
# noyau qui ne boote pas.
CRIT="CONFIG_64BIT CONFIG_PRINTK CONFIG_TTY CONFIG_SERIAL_8250 CONFIG_SERIAL_8250_CONSOLE \
CONFIG_PCI CONFIG_PCI_MSI CONFIG_NET CONFIG_NETDEVICES CONFIG_INET CONFIG_UNIX \
CONFIG_VIRTIO CONFIG_VIRTIO_MENU CONFIG_VIRTIO_PCI CONFIG_VIRTIO_NET CONFIG_VIRTIO_BLK CONFIG_VIRTIO_CONSOLE \
CONFIG_VIRTIO_MMIO CONFIG_BLOCK CONFIG_BLK_DEV_INITRD CONFIG_PROC_FS CONFIG_SYSFS \
CONFIG_DEVTMPFS CONFIG_TMPFS CONFIG_SHMEM CONFIG_BINFMT_ELF CONFIG_BINFMT_SCRIPT \
CONFIG_MULTIUSER CONFIG_FUTEX CONFIG_EPOLL CONFIG_SIGNALFD CONFIG_TIMERFD CONFIG_EVENTFD \
CONFIG_POSIX_TIMERS CONFIG_HIGH_RES_TIMERS CONFIG_HYPERVISOR_GUEST CONFIG_PARAVIRT \
CONFIG_KVM_GUEST CONFIG_ACPI CONFIG_CMDLINE_BOOL CONFIG_KERNEL_LZ4"
MISSING=""
for opt in $CRIT; do
  grep -qx "$opt=y" "$OUT_BUILD/.config" || MISSING="$MISSING $opt"
done
[ -z "$MISSING" ] || { echo "options non appliquées par olddefconfig :$MISSING" >&2; exit 1; }

make O="$OUT_BUILD" -j"$JOBS" bzImage

mkdir -p "$ASSETS"
cp "$OUT_BUILD/arch/x86/boot/bzImage" "$ASSETS/vmlinux-slim"
UNAME_R=$(make O="$OUT_BUILD" -s kernelrelease)
echo "$VERSION slim (release $UNAME_R)" > "$ASSETS/vmlinux-slim.version"
SIZE=$(stat -c%s "$ASSETS/vmlinux-slim")
echo "guest kernel slim: $ASSETS/vmlinux-slim ($SIZE octets)"
if [ "$SIZE" -gt $((6 * 1024 * 1024)) ]; then
  echo "AVERTISSEMENT : bzImage > 6 Mo" >&2
fi
sha256sum "$ASSETS/vmlinux-slim"
cat "$ASSETS/vmlinux-slim.version"
