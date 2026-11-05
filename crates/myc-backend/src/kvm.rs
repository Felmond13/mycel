//! KVM microVM backend — Cloud Hypervisor driven as a child process.
//!
//! This is the strongest isolation tier on Linux: the container workload
//! runs inside a real virtual machine with its own kernel, so a container
//! escape never shares a kernel with the host (unlike [`crate::native`],
//! which is shared-kernel, Docker-level isolation).
//!
//! Two execution paths live here:
//!
//! * [`KvmMicroVm::run`] — the **engine**: executes an arbitrary
//!   [`RunRequest`] end to end (manifest command, env, binds, published
//!   ports, exit-code propagation). The materialized rootfs enters the
//!   guest through one of two "voies", selected automatically:
//!   - **voie B (production path, zero copy)** — virtiofsd shares the
//!     rootfs directory; the guest does `mount -t virtiofs mycroot
//!     /newroot` then `switch_root`. Bind mounts become extra shares, so
//!     no data is ever copied.
//!   - **voie A (guaranteed fallback)** — `mkfs.ext4 -d <rootfs>` builds an
//!     image exposed as a read-only virtio-blk disk; the guest mounts
//!     `/dev/vda` and `switch_root`s. Costs one copy; binds unsupported.
//! * [`KvmMicroVm::launch`] — the bench prototype (kernel + initramfs +
//!   bridge lease + permanent ARP + [`RunningVm::wait_ready`]), used by the
//!   `bench/microvm` measurements.
//!
//! The command/env reach the guest as `/myc/run.env` (strictly
//! single-quoted shell assignments, written into the rootfs before boot;
//! `/myc/run.json` carries the same data for humans). `/myc/init-guest`
//! (stage 2, PID 1 in the environment) runs the command as a child and
//! writes its exit code as raw decimal bytes at offset 0 of a scratch
//! virtio-blk disk — uniform across both voies, no filesystem needed,
//! readable by the host right after the VMM exits. The guest's console is
//! streamed live to the host stdout.
//!
//! Networking (docker0 pattern): a persistent bridge `br-myc` owns the
//! subnet (`MYCEL_VM_CIDR`, default 192.168.77.0/24) with the host address
//! on it, and every VM attaches its own tap `myc-<n>` with a sequential
//! lease `<base>.n` (n = 2, 3, … — N concurrent instances; the first lease
//! is always .2 so the bench workload keeps its deterministic address).
//! "Host" mode is emulated by DNAT'ing every exposed/published port from
//! `127.0.0.1:<host>` to the leased guest address. The bridge, taps and
//! iptables rules need root (exactly like the Docker daemon); runs without
//! ports need no privileges at all.
//!
//! Snapshot/restore status: Cloud Hypervisor v54.0 snapshots fine, but the
//! restored guest stays inert under WSL2's KVM (MSR state and interrupt
//! injection are not preserved across save/restore by that hypervisor
//! backend; evidence archived in the Capsule M0 investigation, 2026-11-03).
//! On native KVM this is expected to work — re-validate there before
//! building on it.

use std::collections::BTreeSet;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::is_root;
use crate::{BackendCheck, BackendError, BackendReport, ExecBackend, Result, RunRequest};
use myc_store::Store;
use std::path::Path as StoreRoot;

/// Where binaries and the kernel are expected (overridable via env).
pub const DEFAULT_CH_BIN: &str = "/usr/local/bin/cloud-hypervisor";
pub const DEFAULT_CH_REMOTE: &str = "/usr/local/bin/ch-remote";
pub const DEFAULT_KERNEL: &str = "/usr/local/share/mycel/vmlinux";
/// Rust virtiofsd (Ubuntu package `virtiofsd`) — voie B.
pub const DEFAULT_VIRTIOFSD: &str = "/usr/libexec/virtiofsd";
/// Guest kernel with `CONFIG_VIRTIO_FS=y` + `CONFIG_EXT4_FS=y`
/// (built by `bench/microvm/build_kernel_vmfs.sh`).
pub const DEFAULT_VM_KERNEL: &str = "/usr/local/share/mycel/vmlinux-vmfs";
/// Generic guest initramfs (built by `bench/microvm/mkinit-vm.sh`).
pub const DEFAULT_VM_INITRAMFS: &str = "/usr/local/share/mycel/init-vm.cpio";
/// e2fsprogs `mkfs.ext4` — voie A.
pub const DEFAULT_MKFS_EXT4: &str = "/usr/sbin/mkfs.ext4";
/// iptables (nft backend is fine) — published ports only.
pub const DEFAULT_IPTABLES: &str = "/usr/sbin/iptables";
/// Firecracker (snapshot accelerator) — system location; auto-provisioned
/// into the download cache when absent (see [`crate::firecracker`]).
pub const DEFAULT_FC_BIN: &str = "/usr/local/bin/firecracker";
/// Uncompressed kernel ELF for Firecracker (same build as
/// [`DEFAULT_VM_KERNEL`]; system location, else the local kernel build
/// tree, else the download cache).
pub const DEFAULT_FC_KERNEL: &str = "/usr/local/share/mycel/vmlinux-vmfs.elf";

/// The engine executes **x86_64 guests** through Cloud Hypervisor. The
/// aarch64 *distribution* exists (release binaries `myc-linux-aarch64`
/// and guest assets `vmlinux-vmfs-aarch64` / `init-vm-aarch64.cpio`,
/// built by the same scripts with `ARCH=aarch64`), but the engine itself
/// has not been wired nor validated on arm64 hardware: no boot, no e2e,
/// no number has been measured there. Rather than pretending, the engine
/// reports itself unavailable on non-x86_64 hosts (`myc doctor` shows
/// the honest per-check reason). Compile-time constant: on x86_64 builds
/// every gate below folds away — zero behavior change.
const ENGINE_ARCH_SUPPORTED: bool = cfg!(target_arch = "x86_64");

/// Guest subnet of the microVM network, a /24: the bridge holds `<base>.1`
/// and each VM leases `<base>.2`, `<base>.3`, … Override with
/// `MYCEL_VM_CIDR` (a /24 only — the generic guest initramfs configures a
/// hardcoded 255.255.255.0 mask).
pub const DEFAULT_VM_CIDR: &str = "192.168.77.0/24";
/// Persistent host bridge every microVM tap attaches to (the `docker0` of
/// myc VMs): created idempotently at first use, kept between runs, carries
/// the host address of [`DEFAULT_VM_CIDR`].
pub const BRIDGE_NAME: &str = "br-myc";
/// First lease of the default subnet — also the static IP baked into the
/// bench workload's initramfs (`bench/microvm` curls `.2` unconditionally,
/// so allocation determinism is a benchmark contract, not a nicety).
pub const GUEST_IP: [u8; 4] = [192, 168, 77, 2];
/// Port the bench workload listens on inside the guest.
pub const GUEST_PORT: u16 = 8080;

/// Guest memory size (override with `MYCEL_VM_MEM`, e.g. `1G`).
pub const DEFAULT_VM_MEM: &str = "512M";
/// Raw scratch disk carrying the exit code out of the guest.
pub(crate) const SCRATCH_SIZE: u64 = 1024 * 1024;

const STOP_TIMEOUT: Duration = Duration::from_secs(5);
static VM_COUNTER: AtomicU32 = AtomicU32::new(0);

// ======================================================================
// Guest asset auto-provision (binary distribution)
// ======================================================================
// The download/cache machinery is shared with the macOS (libkrun) engine:
// it lives in [`crate::assets`]. Re-exported here for historical callers
// (firecracker.rs imports `crate::kvm::vm_assets_cache` and the size
// guards).
pub(crate) use crate::assets::{
    locate_vm_asset, vm_assets_cache, AssetSource, MIN_INITRAMFS_BYTES, MIN_KERNEL_BYTES,
};

/// The KVM microVM engine: paths resolved from env or defaults.
#[derive(Debug, Clone)]
pub struct KvmMicroVm {
    /// `cloud-hypervisor` binary (v54.0 pinned by the build docs).
    pub ch_bin: PathBuf,
    /// `ch-remote` binary.
    pub ch_remote: PathBuf,
    /// Guest kernel (bzImage, virtio built in — see `bench/microvm`).
    pub kernel: PathBuf,
    /// Rust virtiofsd binary (voie B).
    pub virtiofsd: PathBuf,
    /// Guest kernel with VIRTIO_FS+EXT4 built in (used by `run()`).
    pub vm_kernel: PathBuf,
    /// Generic guest initramfs (stage 1: mount rootfs + switch_root).
    pub vm_initramfs: PathBuf,
    /// `mkfs.ext4` (voie A).
    pub mkfs_ext4: PathBuf,
    /// `iptables` (published ports).
    pub iptables: PathBuf,
    /// Firecracker binary (snapshot accelerator —
    /// [`crate::firecracker`]); auto-provisioned like the guest assets.
    pub fc_bin: PathBuf,
    /// Kernel ELF Firecracker boots (same build as `vm_kernel`; FC does
    /// not load bzImages). `MYCEL_FC_KERNEL` overrides.
    pub fc_kernel: PathBuf,
}

impl KvmMicroVm {
    /// Resolve engine paths from `MYCEL_CH_BIN`, `MYCEL_CH_REMOTE`,
    /// `MYCEL_KERNEL`, `MYCEL_VIRTIOFSD`, `MYCEL_VM_KERNEL`,
    /// `MYCEL_VM_INITRAMFS`, `MYCEL_MKFS_EXT4`, `MYCEL_IPTABLES`,
    /// `MYCEL_FC_BIN`, `MYCEL_FC_KERNEL` or the documented defaults.
    pub fn from_env() -> Self {
        let pick = |var: &str, default: &str| {
            std::env::var(var)
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from(default))
        };
        Self {
            ch_bin: pick("MYCEL_CH_BIN", DEFAULT_CH_BIN),
            ch_remote: pick("MYCEL_CH_REMOTE", DEFAULT_CH_REMOTE),
            kernel: pick("MYCEL_KERNEL", DEFAULT_KERNEL),
            virtiofsd: pick("MYCEL_VIRTIOFSD", DEFAULT_VIRTIOFSD),
            vm_kernel: pick("MYCEL_VM_KERNEL", DEFAULT_VM_KERNEL),
            vm_initramfs: pick("MYCEL_VM_INITRAMFS", DEFAULT_VM_INITRAMFS),
            mkfs_ext4: pick("MYCEL_MKFS_EXT4", DEFAULT_MKFS_EXT4),
            iptables: pick("MYCEL_IPTABLES", DEFAULT_IPTABLES),
            fc_bin: pick("MYCEL_FC_BIN", DEFAULT_FC_BIN),
            fc_kernel: pick("MYCEL_FC_KERNEL", DEFAULT_FC_KERNEL),
        }
    }

    fn file_exists(p: &Path) -> bool {
        p.is_file()
    }

    /// `/dev/kvm` opened read-write proves both presence and permission.
    pub(crate) fn kvm_usable() -> bool {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .is_ok()
    }

    /// Where `run()` would load a guest asset from RIGHT NOW without
    /// touching the network: the expected system location, or the
    /// size-checked download cache. `None` → a run would have to download
    /// it first (which an EXPLICIT `--engine kvm-microvm` still does — the
    /// automatic default must not depend on the network).
    fn vm_asset_source(expected: &Path, asset: &str, min_bytes: u64) -> Option<PathBuf> {
        match locate_vm_asset(expected, &vm_assets_cache(), asset, min_bytes) {
            AssetSource::System(path) | AssetSource::Cache(path) => Some(path),
            AssetSource::Missing => None,
        }
    }

    /// True when `run()` can execute environments right now (either voie),
    /// counting guest assets present in the download cache as ready.
    /// On non-x86_64 hosts this is always false: the engine wiring is
    /// x86_64-only (see [`ENGINE_ARCH_SUPPORTED`]) — aarch64 guests have
    /// published assets but no validated engine.
    fn engine_available(&self) -> bool {
        ENGINE_ARCH_SUPPORTED
            && Self::kvm_usable()
            && Self::file_exists(&self.ch_bin)
            && Self::file_exists(&self.ch_remote)
            && Self::vm_asset_source(&self.vm_kernel, "vmlinux-vmfs", MIN_KERNEL_BYTES).is_some()
            && Self::vm_asset_source(&self.vm_initramfs, "init-vm.cpio", MIN_INITRAMFS_BYTES)
                .is_some()
            && (Self::file_exists(&self.virtiofsd) || Self::file_exists(&self.mkfs_ext4))
    }

    /// True when the full production path is ready offline: engine
    /// available AND virtiofsd present (voie B: zero copy + bind mounts).
    /// `myc doctor` prints READY only for this; voie A alone is PARTIAL.
    /// This is what the automatic engine default keys on
    /// (`crate::kvm_engine_complete`).
    pub fn engine_ready(&self) -> bool {
        self.engine_available() && Self::file_exists(&self.virtiofsd)
    }

    /// Effective path for a guest asset, auto-provisioning when missing:
    /// the expected system location first, the download cache second, a
    /// one-shot download from the GitHub release last (this build's tag,
    /// then `latest`). Downloads land in the cache — never in the
    /// root-owned system dir. Both `run()` and `launch()` go through this
    /// (and the Firecracker snapshot path for the initramfs).
    pub(crate) fn resolve_vm_asset(
        &self,
        expected: &Path,
        asset: &str,
        min_bytes: u64,
    ) -> Result<PathBuf> {
        if let AssetSource::Missing =
            locate_vm_asset(expected, &vm_assets_cache(), asset, min_bytes)
        {
            eprintln!(
                "[myc-vm] guest asset '{asset}' missing at {} — downloading once \
                 (override with MYCEL_VM_KERNEL / MYCEL_VM_INITRAMFS)",
                expected.display()
            );
        }
        crate::assets::ensure_asset(expected, asset, min_bytes)
    }

    /// Boot a microVM running `initramfs`. The VM gets the first free
    /// network lease (`<base>.2` on an idle subnet — the bench contract),
    /// its tap attached to the shared bridge and a pre-seeded permanent
    /// ARP entry; [`RunningVm::wait_ready`] then waits until the guest
    /// accepts TCP on `<lease-ip>:GUEST_PORT`.
    pub fn launch(&self, initramfs: &Path) -> Result<RunningVm> {
        // Same honest arch gate as `run()` (folded away on x86_64 builds):
        // the bench path is no more wired on aarch64 than the engine path.
        if !ENGINE_ARCH_SUPPORTED {
            return Err(BackendError::Unavailable(
                "kvm-microvm executes x86_64 guests only — on aarch64 the guest assets are \
                 published but the engine wiring is pending hardware validation"
                    .into(),
            ));
        }
        if !Self::kvm_usable() {
            return Err(BackendError::Unavailable(
                "/dev/kvm is not usable (missing, or no permission — try the kvm group)".into(),
            ));
        }
        if !Self::file_exists(&self.ch_bin) {
            return Err(BackendError::Unavailable(format!(
                "cloud-hypervisor not found at {} (set MYCEL_CH_BIN?)",
                self.ch_bin.display()
            )));
        }
        // The bench kernels are not release artifacts, but the published
        // vmfs kernel is a superset of the slim config and boots the same
        // initramfs — auto-provision it instead of failing.
        let kernel = if Self::file_exists(&self.kernel) {
            self.kernel.clone()
        } else {
            eprintln!(
                "[myc-vm] kernel missing at {} — falling back to the published vmlinux-vmfs",
                self.kernel.display()
            );
            self.resolve_vm_asset(&self.kernel, "vmlinux-vmfs", MIN_KERNEL_BYTES)?
        };
        // The generic published initramfs self-heals the same way; a
        // bench-specific one (rootfs.cpio & co) must be built locally.
        let initramfs = if initramfs.is_file() {
            initramfs.to_path_buf()
        } else if initramfs == Path::new(DEFAULT_VM_INITRAMFS) {
            self.resolve_vm_asset(initramfs, "init-vm.cpio", MIN_INITRAMFS_BYTES)?
        } else {
            return Err(BackendError::Unavailable(format!(
                "initramfs not found: {} (bench initramfs: bench/microvm/mkinitramfs.sh)",
                initramfs.display()
            )));
        };
        if !is_root() {
            return Err(BackendError::Unavailable(
                "launch() needs root — it creates the bridge, the tap and the DNAT rules".into(),
            ));
        }
        let net = vm_net()?;

        let state_dir = std::env::temp_dir().join(format!(
            "myc-microvm-{}-{}",
            std::process::id(),
            VM_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&state_dir)?;
        let api_socket = state_dir.join("api.sock");
        let log = state_dir.join("ch.log");
        let ch_log = std::fs::File::create(&log)?;

        // Upgrade path: a leftover v1 tap myc0 holds the subnet address on
        // itself and would trip the conflict probe below forever.
        let _ = ip_run(&["link", "del", "myc0"]);

        // Bridge first (idempotent; persists across runs like docker0),
        // then the lease: first free IP from .2 — deterministic when the
        // subnet is idle, which the benchmark contract requires.
        ensure_bridge(&net)?;
        let lease = allocate_lease(&net, &state_dir)?;
        // Permanent neighbour entry before the guest even exists (the
        // route egresses the bridge — see seed_guest_arp).
        seed_guest_arp(&lease);

        // The VMM creates the tap itself; no ip= field — the bridge carries
        // the address. mycip= tells generic guest inits their address; the
        // stock bench initramfs ignores it (its IP is baked in).
        let net_arg = format!("tap={},mac={}", lease.tap, lease.mac);
        let spawned = Command::new(&self.ch_bin)
            .arg("--api-socket")
            .arg(&api_socket)
            .arg("--console")
            .arg(format!("file={}", state_dir.join("console.log").display()))
            .arg("--kernel")
            .arg(&kernel)
            .arg("--cmdline")
            .arg(format!(
                "console=hvc0 quiet mycip={} mycgw={}",
                lease.ip_str,
                dotted(net.host_ip)
            ))
            .arg("--initramfs")
            .arg(&initramfs)
            .arg("--cpus")
            .arg("boot=1")
            .arg("--memory")
            .arg("size=512M")
            .arg("--net")
            .arg(net_arg)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(ch_log))
            .spawn()
            .map_err(|e| BackendError::Unavailable(format!("spawn cloud-hypervisor: {e}")));
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                release_lease(&net, &lease, &state_dir);
                let _ = std::fs::remove_dir_all(&state_dir);
                return Err(e);
            }
        };

        // Fail fast with the actual VMM log if it dies immediately.
        thread::sleep(Duration::from_millis(30));
        match child.try_wait() {
            Ok(None) => {}
            Ok(Some(status)) => {
                let log_content = std::fs::read_to_string(&log).unwrap_or_default();
                detach_tap(&lease);
                release_lease(&net, &lease, &state_dir);
                let _ = std::fs::remove_dir_all(&state_dir);
                return Err(BackendError::Unavailable(format!(
                    "cloud-hypervisor died at startup ({status}): {}",
                    log_content.trim()
                )));
            }
            Err(e) => {
                release_lease(&net, &lease, &state_dir);
                let _ = std::fs::remove_dir_all(&state_dir);
                return Err(BackendError::Io(e));
            }
        }

        // Join the VMM-created tap to the bridge — without it no packet can
        // flow between host and guest (the tap carries no address itself).
        if !attach_tap(&lease) {
            let _ = child.kill();
            let _ = child.wait();
            detach_tap(&lease);
            release_lease(&net, &lease, &state_dir);
            let _ = std::fs::remove_dir_all(&state_dir);
            return Err(BackendError::Unavailable(format!(
                "tap {} never appeared to join {BRIDGE_NAME} (VMM log: {})",
                lease.tap,
                log.display()
            )));
        }

        Ok(RunningVm {
            child,
            ch_remote: self.ch_remote.clone(),
            api_socket,
            state_dir,
            net,
            lease,
            stopped: false,
        })
    }
}

// ======================================================================
// Engine: arbitrary RunRequest execution (voies A and B)
// ======================================================================

/// How the rootfs enters the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Voie {
    /// virtiofs share of the materialized rootfs (zero copy, binds work).
    VirtioFs,
    /// Read-only ext4 image built with `mkfs.ext4 -d` (fallback, one copy).
    Ext4,
}

impl Voie {
    /// Stable label used in traces and docs.
    pub fn label(self) -> &'static str {
        match self {
            Self::VirtioFs => "virtiofs",
            Self::Ext4 => "ext4",
        }
    }

    /// Pick the voie: `MYCEL_VM_ROOTFS=virtiofs|ext4` forces one; auto
    /// prefers virtiofs (production) and falls back to ext4.
    fn select(engine: &KvmMicroVm) -> Result<Self> {
        match std::env::var("MYCEL_VM_ROOTFS").ok().as_deref() {
            Some("virtiofs") => Ok(Self::VirtioFs),
            Some("ext4") => Ok(Self::Ext4),
            Some(other) => Err(BackendError::Unavailable(format!(
                "invalid MYCEL_VM_ROOTFS '{other}' (expected virtiofs or ext4)"
            ))),
            _ if engine.virtiofsd.is_file() => Ok(Self::VirtioFs),
            _ if engine.mkfs_ext4.is_file() => Ok(Self::Ext4),
            _ => Err(BackendError::Unavailable(
                "neither virtiofsd (voie B) nor mkfs.ext4 (voie A) is available — \
                 install the 'virtiofsd' or 'e2fsprogs' package"
                    .into(),
            )),
        }
    }
}

/// One virtiofs share: the rootfs (`tag=mycroot`) or a bind mount.
#[derive(Debug, Clone)]
pub(crate) struct Share {
    pub(crate) tag: String,
    pub(crate) host_dir: PathBuf,
    pub(crate) container_path: String,
    pub(crate) read_only: bool,
}

/// Everything the guest needs to run the workload, serialized into the
/// rootfs as `/myc/run.env` (+ `/myc/run.json` for humans) — or, on the
/// Firecracker snapshot path, written on the scratch disk by the host.
#[derive(Debug, Clone)]
pub(crate) struct GuestSpec {
    pub(crate) argv: Vec<String>,
    pub(crate) env: Vec<String>,
    pub(crate) workdir: Option<String>,
    pub(crate) hostname: String,
    pub(crate) binds: Vec<Share>,
    /// Which virtio-blk device carries the exit code out (`/dev/vda` on
    /// voie B where scratch is the only disk, `/dev/vdb` on voie A where
    /// vda is the read-only root image).
    pub(crate) exit_dev: String,
    /// Nameservers written to the guest /etc/resolv.conf at run time (sent
    /// in run.env, so a restored snapshot always gets the current ones).
    pub(crate) dns: Vec<String>,
}

/// Guest environment, most general first (the guest exports in order, so
/// the last assignment of a key wins): sane defaults, then the image's
/// ENV, then -e. Without a PATH nothing from a manifest CMD like
/// `redis-server` could be exec'd by the shell. Shared by both VMM paths.
pub(crate) fn guest_env(manifest: &myc_manifest::Manifest, request: &RunRequest) -> Vec<String> {
    let mut env: Vec<String> = vec![
        "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into(),
        "HOME=/root".into(),
    ];
    env.extend(manifest.config.env.iter().cloned());
    env.extend(request.env.iter().cloned());
    env
}

/// Quote `s` so a POSIX shell reads back exactly this string when the
/// variable is expanded (`'` → `'\''`, the standard trick).
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Stage 2 guest init (PID 1 inside the environment), written by the host
/// into the rootfs at `/myc/init-guest`. Sources `/myc/run.env`, mounts
/// /proc /sys /dev (here, not in the initramfs: voie A's root is
/// read-only, so mkdir must degrade silently), applies hostname +
/// virtiofs binds + env + workdir, runs the command as a child, forwards
/// its exit code to the raw scratch device, then powers the VM off —
/// which is what makes the VMM process exit on the host.
const INIT_GUEST: &str = r#"#!/bin/sh
. /myc/run.env || poweroff -f
mkdir -p /proc /sys /dev /tmp /run 2>/dev/null
mount -t proc proc /proc 2>/dev/null
mount -t sysfs sysfs /sys 2>/dev/null
mount -t devtmpfs devtmpfs /dev 2>/dev/null
[ -n "$MYC_HOSTNAME" ] && hostname "$MYC_HOSTNAME" 2>/dev/null
[ -n "$MYC_DNS" ] && { mkdir -p /etc 2>/dev/null; for d in $MYC_DNS; do echo "nameserver $d"; done > /etc/resolv.conf 2>/dev/null; }
i=0
while [ "$i" -lt "$MYC_BIND_N" ]; do
  eval "tag=\$MYC_BIND_TAG_$i"
  eval "dst=\$MYC_BIND_DST_$i"
  eval "ro=\$MYC_BIND_RO_$i"
  mkdir -p "$dst" 2>/dev/null
  if [ "$ro" = "1" ]; then
    mount -t virtiofs -o ro "$tag" "$dst" 2>/dev/null
  else
    mount -t virtiofs "$tag" "$dst" 2>/dev/null
  fi
  i=$((i+1))
done
i=0
while [ "$i" -lt "$MYC_ENV_N" ]; do
  eval "e=\$MYC_ENV_$i"
  export "$e"
  i=$((i+1))
done
set --
i=0
while [ "$i" -lt "$MYC_ARGC" ]; do
  eval "a=\$MYC_ARG_$i"
  set -- "$@" "$a"
  i=$((i+1))
done
if [ -n "$MYC_WORKDIR" ]; then
  mkdir -p "$MYC_WORKDIR" 2>/dev/null
  cd "$MYC_WORKDIR" 2>/dev/null
fi
"$@"
code=$?
printf '%s' "$code" > "$MYC_EXIT_DEV" 2>/dev/null
sync
poweroff -f
"#;

/// Generate `/myc/run.env` — shell-sourceable assignments; the guest reads
/// THIS file (zero JSON parsing inside the VM).
pub(crate) fn run_env_content(spec: &GuestSpec) -> String {
    let mut out = format!("MYC_ARGC={}\n", spec.argv.len());
    for (i, a) in spec.argv.iter().enumerate() {
        out.push_str(&format!("MYC_ARG_{i}={}\n", sh_quote(a)));
    }
    out.push_str(&format!("MYC_ENV_N={}\n", spec.env.len()));
    for (i, e) in spec.env.iter().enumerate() {
        out.push_str(&format!("MYC_ENV_{i}={}\n", sh_quote(e)));
    }
    out.push_str(&format!(
        "MYC_WORKDIR={}\n",
        spec.workdir
            .as_deref()
            .map(sh_quote)
            .unwrap_or_else(|| "''".into())
    ));
    out.push_str(&format!("MYC_HOSTNAME={}\n", sh_quote(&spec.hostname)));
    if !spec.dns.is_empty() {
        out.push_str(&format!("MYC_DNS={}\n", sh_quote(&spec.dns.join(" "))));
    }
    out.push_str(&format!("MYC_BIND_N={}\n", spec.binds.len()));
    for (i, b) in spec.binds.iter().enumerate() {
        out.push_str(&format!("MYC_BIND_TAG_{i}={}\n", sh_quote(&b.tag)));
        out.push_str(&format!(
            "MYC_BIND_DST_{i}={}\n",
            sh_quote(&b.container_path)
        ));
        out.push_str(&format!(
            "MYC_BIND_RO_{i}={}\n",
            if b.read_only { 1 } else { 0 }
        ));
    }
    out.push_str(&format!("MYC_EXIT_DEV={}\n", sh_quote(&spec.exit_dev)));
    out
}

/// Same spec as `/myc/run.json` — for humans and post-mortem debugging.
fn run_json_content(spec: &GuestSpec) -> String {
    let json = serde_json::json!({
        "argv": spec.argv,
        "env": spec.env,
        "workdir": spec.workdir,
        "hostname": spec.hostname,
        "binds": spec.binds
            .iter()
            .map(|b| serde_json::json!({
                "tag": b.tag,
                "container_path": b.container_path,
                "read_only": b.read_only,
            }))
            .collect::<Vec<_>>(),
        "exit_dev": spec.exit_dev,
    });
    serde_json::to_string_pretty(&json).unwrap_or_default()
}

/// Write `/myc/{init-guest,run.env,run.json}` into the materialized rootfs.
fn write_guest_files(rootfs: &Path, spec: &GuestSpec) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let myc = rootfs.join("myc");
    std::fs::create_dir_all(&myc)?;
    std::fs::write(myc.join("init-guest"), INIT_GUEST)?;
    std::fs::set_permissions(
        myc.join("init-guest"),
        std::fs::Permissions::from_mode(0o755),
    )?;
    std::fs::write(myc.join("run.env"), run_env_content(spec))?;
    std::fs::write(myc.join("run.json"), run_json_content(spec))?;
    Ok(())
}

/// Parse the exit code the guest wrote at offset 0 of the scratch disk:
/// leading ASCII decimal digits terminated by NUL/garbage/EOF. Anything
/// else means the guest never reported (boot failure).
fn parse_exit_code(bytes: &[u8]) -> Option<i32> {
    let end = bytes
        .iter()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(bytes.len());
    if end == 0 || end > 3 {
        return None;
    }
    std::str::from_utf8(&bytes[..end]).ok()?.parse().ok()
}

/// Recursive byte size of a directory tree (symlinks not followed) — used
/// to size voie A's ext4 image.
pub(crate) fn tree_size(path: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&p) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total += meta.len().max(512);
            }
        }
    }
    total
}

/// Delete EVERY iptables rule carrying this lease tag, across tables and
/// chains. Self-healing on two measured failure modes (2026-11-04): a run
/// killed with SIGKILL never runs [`VmRun::release`], and re-entering the
/// publish path used to stack duplicate rules. Idempotent, best-effort.
pub(crate) fn purge_tag_rules(tag: &str) {
    let needle = format!("--comment {tag}");
    let bin = iptables_bin();
    for table in ["nat", "filter"] {
        let Ok(out) = Command::new(&bin)
            .args(["-t", table, "-S"])
            .stdin(Stdio::null())
            .output()
        else {
            return;
        };
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            let Some(rest) = line.strip_prefix("-A ") else {
                continue;
            };
            if !rest.contains(&needle) {
                continue;
            }
            let mut parts = rest.split(' ');
            let Some(chain) = parts.next() else { continue };
            let mut del: Vec<String> = vec![
                "-t".into(),
                table.to_string(),
                "-D".into(),
                chain.to_string(),
            ];
            del.extend(parts.map(str::to_string));
            while iptables_run(&bin, &del) {}
        }
    }
}

pub(crate) fn iptables_bin() -> PathBuf {
    std::env::var("MYCEL_IPTABLES")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_IPTABLES))
}

pub(crate) fn iptables_run(bin: &Path, args: &[String]) -> bool {
    Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ======================================================================
// Networking: one shared bridge (br-myc) + one lease per VM
// ======================================================================
//
// docker0 pattern. A persistent L2 bridge owns the subnet (host side =
// <base>.1); every VM gets its own tap `myc-<n>` created by the VMM
// (`tap=myc-<n>,mac=<derived>` — no `ip=`, the bridge carries the address)
// and attached to the bridge by the host. Guest n leases <base>.n with
// n = 2, 3, … — N concurrent instances. The FIRST lease is always .2, so
// the bench workload (static 192.168.77.2 in its initramfs) and every
// script curling .2 keep working without modification.

/// Run `ip <args>` silently; true on success.
pub(crate) fn ip_run(args: &[&str]) -> bool {
    Command::new("ip")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn iface_exists(name: &str) -> bool {
    ip_run(&["link", "show", name])
}

fn dotted(ip: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
}

/// The subnet this process leases guests from: `MYCEL_VM_CIDR` or
/// [`DEFAULT_VM_CIDR`].
#[derive(Debug, Clone)]
pub(crate) struct VmNet {
    /// Network address, e.g. 192.168.77.0.
    base: [u8; 4],
    /// Bridge (= host) address, e.g. 192.168.77.1.
    host_ip: [u8; 4],
    /// `host_ip/24`, as `ip addr add` wants it.
    host_prefix: String,
    /// Canonical CIDR string, e.g. `192.168.77.0/24`.
    cidr: String,
}

/// Parse an `a.b.c.d/24` CIDR. Any other prefix is refused: the generic
/// guest initramfs configures a hardcoded 255.255.255.0 mask, so widening
/// the subnet means teaching it a mask parameter first.
fn parse_cidr(cidr: &str) -> Result<VmNet> {
    let invalid = || {
        BackendError::Unavailable(format!(
            "invalid MYCEL_VM_CIDR '{cidr}' (expected a /24 like '{DEFAULT_VM_CIDR}')"
        ))
    };
    let (addr, prefix) = cidr.split_once('/').ok_or_else(invalid)?;
    if prefix != "24" {
        return Err(BackendError::Unavailable(format!(
            "MYCEL_VM_CIDR '{cidr}': only /24 subnets are supported"
        )));
    }
    let parts: Vec<&str> = addr.split('.').collect();
    if parts.len() != 4 {
        return Err(invalid());
    }
    let mut base = [0u8; 4];
    for (b, p) in base.iter_mut().zip(parts) {
        *b = p.parse::<u8>().map_err(|_| invalid())?;
    }
    base[3] = 0; // normalize stray host bits
    let mut host_ip = base;
    host_ip[3] = 1;
    Ok(VmNet {
        host_prefix: format!("{}/24", dotted(host_ip)),
        cidr: format!("{}/24", dotted(base)),
        base,
        host_ip,
    })
}

pub(crate) fn vm_net() -> Result<VmNet> {
    parse_cidr(&std::env::var("MYCEL_VM_CIDR").unwrap_or_else(|_| DEFAULT_VM_CIDR.into()))
}

/// True when `a/pa` and `b/pb` share at least one address.
fn subnets_overlap(a: [u8; 4], pa: u8, b: [u8; 4], pb: u8) -> bool {
    let shift = 32 - pa.min(pb);
    if shift >= 32 {
        return true; // a /0 swallows everything
    }
    let (a, b) = (u32::from_be_bytes(a), u32::from_be_bytes(b));
    (a >> shift) == (b >> shift)
}

/// Every global IPv4 address with prefix and device, from
/// `ip -o -4 addr show scope global` — e.g. `("172.17.0.1", 16, "docker0")`.
fn global_addrs() -> Vec<(String, u8, String)> {
    let out = Command::new("ip")
        .args(["-o", "-4", "addr", "show", "scope", "global"])
        .output();
    let text = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => return Vec::new(),
    };
    let mut addrs = Vec::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let _idx = it.next();
        let dev = it.next().unwrap_or_default().trim_end_matches(':');
        if it.next() != Some("inet") {
            continue;
        }
        let Some(inet) = it.next() else { continue };
        let Some((ip, len)) = inet.split_once('/') else {
            continue;
        };
        if let Ok(len) = len.parse::<u8>() {
            addrs.push((ip.to_string(), len, dev.to_string()));
        }
    }
    addrs
}

/// A foreign interface whose subnet overlaps ours (our own bridge excepted).
fn subnet_conflict(net: &VmNet) -> Option<(String, String)> {
    global_addrs().into_iter().find_map(|(ip, len, dev)| {
        (dev != BRIDGE_NAME
            && ip
                .parse::<std::net::Ipv4Addr>()
                .map(|a| subnets_overlap(a.octets(), len, net.base, 24))
                .unwrap_or(false))
        .then(|| (dev, format!("{ip}/{len}")))
    })
}

/// Create [`BRIDGE_NAME`] idempotently (it persists across runs, exactly
/// like docker0) and make sure it holds the host address of the subnet.
/// Refuses to touch anything while another interface already occupies the
/// subnet — silently overlapping subnets route to the wrong device.
pub(crate) fn ensure_bridge(net: &VmNet) -> Result<()> {
    if !is_root() {
        return Err(BackendError::Unavailable(
            "networked microVMs need root (bridge, taps, DNAT) — run myc under sudo \
             or use --net none"
                .into(),
        ));
    }
    if let Some((dev, cidr)) = subnet_conflict(net) {
        return Err(BackendError::Unavailable(format!(
            "'{dev}' already uses {cidr}, which overlaps {} — free that interface \
             or set MYCEL_VM_CIDR to another free /24",
            net.cidr
        )));
    }
    if !iface_exists(BRIDGE_NAME) && !ip_run(&["link", "add", BRIDGE_NAME, "type", "bridge"]) {
        return Err(BackendError::Unavailable(format!(
            "cannot create bridge {BRIDGE_NAME} (ip link add failed)"
        )));
    }
    let want = dotted(net.host_ip);
    let has = Command::new("ip")
        .args(["-o", "-4", "addr", "show", "dev", BRIDGE_NAME])
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains(&want))
        .unwrap_or(false);
    if !has && !ip_run(&["addr", "add", &net.host_prefix, "dev", BRIDGE_NAME]) {
        return Err(BackendError::Unavailable(format!(
            "cannot give {BRIDGE_NAME} the address {} (already used elsewhere?)",
            net.host_prefix
        )));
    }
    let _ = ip_run(&["link", "set", BRIDGE_NAME, "up"]);
    // Guest egress: forward + masquerade the bridge subnet out of the
    // default-route interface (docker0 semantics). Idempotent — the
    // bridge persists across runs, so does the rule; skipped when the
    // host has no default route (offline).
    egress_nat(net);
    Ok(())
}

/// Host nameservers offered to guests: `MYCEL_VM_DNS` wins; else the
/// host's non-loopback /etc/resolv.conf entries (a 127.x resolver is
/// unreachable from a guest); else public fallbacks so DNS still works.
pub(crate) fn guest_dns() -> Vec<String> {
    if let Ok(v) = std::env::var("MYCEL_VM_DNS") {
        let list: Vec<String> = v.split_whitespace().map(str::to_owned).collect();
        if !list.is_empty() {
            return list;
        }
    }
    let mut ns: Vec<String> = std::fs::read_to_string("/etc/resolv.conf")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.strip_prefix("nameserver"))
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.starts_with("127."))
        .map(str::to_owned)
        .collect();
    ns.dedup();
    if ns.is_empty() {
        ns = vec!["1.1.1.1".into(), "8.8.8.8".into()];
    }
    ns
}

/// The bridge gateway as a dotted string, even when MYCEL_VM_CIDR is
/// invalid (the default keeps guests routable on the fallback subnet).
pub(crate) fn bridge_gateway() -> String {
    vm_net()
        .map(|n| dotted(n.host_ip))
        .unwrap_or_else(|_| "192.168.77.1".into())
}

/// First default-route interface (`ip route show default`), if any.
fn default_iface() -> Option<String> {
    let out = Command::new("ip")
        .args(["route", "show", "default"])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .find(|l| l.starts_with("default"))
        .and_then(|l| {
            let mut parts = l.split_whitespace();
            loop {
                match parts.next() {
                    Some("dev") => return parts.next().map(str::to_owned),
                    Some(_) => continue,
                    None => return None,
                }
            }
        })
}

/// Enable IPv4 forwarding (only when off) and install one tagged
/// MASQUERADE rule for the bridge subnet. Best-effort by design: an
/// offline host simply keeps its VMs offline, exactly like before.
fn egress_nat(net: &VmNet) {
    let fwd = Command::new("sysctl")
        .args(["-n", "net.ipv4.ip_forward"])
        .stdin(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "1")
        .unwrap_or(false);
    if !fwd {
        let _ = Command::new("sysctl")
            .args(["-w", "net.ipv4.ip_forward=1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let Some(dev) = default_iface() else { return };
    let ipt = iptables_bin();
    // `iptables -C` is unusable here: on nft-compat hosts it can exit 0
    // while printing "Bad rule" on stderr. Scan the chain listing instead
    // (same technique as purge_tag_rules).
    let listed = Command::new(&ipt)
        .args(["-t", "nat", "-S", "POSTROUTING"])
        .stdin(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let filter_listed = Command::new(&ipt)
        .args(["-t", "filter", "-S", "FORWARD"])
        .stdin(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    if listed.lines().any(|l| l.contains("--comment myc-egress"))
        && filter_listed
            .lines()
            .any(|l| l.contains("--comment myc-egress"))
    {
        return; // already installed
    }
    // FORWARD is often policy-DROP (Docker's default): without these two,
    // forwarded guest packets die silently between the bridge and eth0.
    for rule in [
        vec![
            "-A",
            "FORWARD",
            "-s",
            &net.cidr,
            "-o",
            &dev,
            "-m",
            "comment",
            "--comment",
            "myc-egress",
            "-j",
            "ACCEPT",
        ],
        vec![
            "-A",
            "FORWARD",
            "-d",
            &net.cidr,
            "-i",
            &dev,
            "-m",
            "comment",
            "--comment",
            "myc-egress",
            "-m",
            "conntrack",
            "--ctstate",
            "ESTABLISHED,RELATED",
            "-j",
            "ACCEPT",
        ],
    ] {
        let args: Vec<String> = rule.iter().map(|s| s.to_string()).collect();
        let _ = iptables_run(&ipt, &args); // tagged: purge_tag_rules cleans up
    }
    let add = [
        "-t",
        "nat",
        "-A",
        "POSTROUTING",
        "-s",
        &net.cidr,
        "-o",
        &dev,
        "-m",
        "comment",
        "--comment",
        "myc-egress",
        "-j",
        "MASQUERADE",
    ]
    .map(String::from);
    iptables_run(&ipt, &add);
}

/// One guest slot on the bridge: sequential IP, matching tap and MAC.
#[derive(Debug, Clone)]
pub(crate) struct Lease {
    /// Host part of the guest IP, 2..=254 — names the tap and the rules.
    pub(crate) n: u8,
    /// Guest IPv4 = `<base>.n`.
    pub(crate) ip: [u8; 4],
    /// Dotted guest IP.
    pub(crate) ip_str: String,
    /// Host-side tap, created by the VMM and attached to the bridge.
    pub(crate) tap: String,
    /// iptables comment tag (`-m comment --comment <tag>`), = tap name.
    pub(crate) tag: String,
    /// Guest MAC, derived from the IP: `52:54:00:77:<oct3>:<oct4>`.
    pub(crate) mac: String,
}

/// Deterministic per-lease MAC: locally administered 52:54:00 prefix, the
/// fixed 0x77 byte, then the two low octets of the guest IP in hex.
fn lease_mac(ip: [u8; 4]) -> String {
    format!("52:54:00:77:{:02x}:{:02x}", ip[2], ip[3])
}

/// Leases held by THIS process. The lock covers the whole scan-and-claim
/// sequence, so two threads in one myc never pick the same n; cross-process
/// exclusion comes from the lease registry directory below.
static LIVE_LEASES: Mutex<BTreeSet<u8>> = Mutex::new(BTreeSet::new());

/// Per-subnet lease registry under the temp dir: one directory per lease,
/// claimed atomically with `mkdir` (no flock dependency), containing the
/// owning pid so crashed runs self-heal on the next allocation.
fn lease_registry(net: &VmNet) -> PathBuf {
    std::env::temp_dir().join(format!("myc-leases-{}", dotted(net.base)))
}

pub(crate) fn pid_alive(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

/// Try to claim lease n in `reg`; `mkdir` is atomic, so exactly one
/// contender wins. A claim whose owner is dead (crashed run) is stale and
/// gets taken over.
fn claim_lease_dir(reg: &Path, n: u8) -> bool {
    let dir = reg.join(format!("myc-{n}"));
    for _ in 0..2 {
        if std::fs::create_dir(&dir).is_ok() {
            let _ = std::fs::write(dir.join("pid"), std::process::id().to_string());
            return true;
        }
        let owner = std::fs::read_to_string(dir.join("pid"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok());
        match owner {
            Some(p) if pid_alive(p) => return false,
            _ => {
                let _ = std::fs::remove_dir_all(&dir); // stale or unreadable
            }
        }
    }
    false
}

/// Taps named `myc-<n>` that already exist on the host — a single
/// `ip -o link show` sweep, not 253 subprocess spawns.
fn existing_myc_taps() -> BTreeSet<u8> {
    let out = Command::new("ip").args(["-o", "link", "show"]).output();
    let text = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => return BTreeSet::new(),
    };
    let mut taps = BTreeSet::new();
    for line in text.lines() {
        // Shape: "12: myc-3@if9: <BROADCAST,..." — name is the 2nd field.
        let Some(name) = line.split_whitespace().nth(1) else {
            continue;
        };
        let Some(n) = name.trim_end_matches([':', '@']).strip_prefix("myc-") else {
            continue;
        };
        if let Ok(n) = n.parse::<u8>() {
            if (2..=254).contains(&n) {
                taps.insert(n);
            }
        }
    }
    taps
}

/// First free lease in `reg`: lowest n of 2..=254 that is neither held by
/// this process (`live`), nor by a foreign tap (`foreign`), nor claimed in
/// the registry. Always returns .2 on an idle subnet (bench determinism).
fn claim_first_free(
    reg: &Path,
    net: &VmNet,
    state_dir: &Path,
    live: &mut BTreeSet<u8>,
    foreign: &BTreeSet<u8>,
) -> Result<Lease> {
    std::fs::create_dir_all(reg)?;
    for n in 2u8..=254 {
        if live.contains(&n) || foreign.contains(&n) || !claim_lease_dir(reg, n) {
            continue;
        }
        // Mirror the lease into the state dir for post-mortem debugging;
        // the registry directory stays the authoritative cross-process state.
        let _ = std::fs::write(state_dir.join("lease"), n.to_string());
        live.insert(n);
        let mut ip = net.base;
        ip[3] = n;
        let tap = format!("myc-{n}");
        return Ok(Lease {
            n,
            ip,
            ip_str: dotted(ip),
            tag: tap.clone(),
            mac: lease_mac(ip),
            tap,
        });
    }
    Err(BackendError::Unavailable(format!(
        "no free IP left in {} (253 concurrent guests max)",
        net.cidr
    )))
}

/// Allocate the first free lease for this VM.
pub(crate) fn allocate_lease(net: &VmNet, state_dir: &Path) -> Result<Lease> {
    let mut live = LIVE_LEASES.lock().unwrap_or_else(|e| e.into_inner());
    claim_first_free(
        &lease_registry(net),
        net,
        state_dir,
        &mut live,
        &existing_myc_taps(),
    )
}

/// Release a lease: registry claim, state-dir marker, in-memory set.
pub(crate) fn release_lease(net: &VmNet, lease: &Lease, state_dir: &Path) {
    release_lease_in(&lease_registry(net), lease, state_dir);
    if let Ok(mut live) = LIVE_LEASES.lock() {
        live.remove(&lease.n);
    }
}

/// Registry-level half of [`release_lease`] (no global state — testable).
fn release_lease_in(reg: &Path, lease: &Lease, state_dir: &Path) {
    let _ = std::fs::remove_file(state_dir.join("lease"));
    let _ = std::fs::remove_dir_all(reg.join(&lease.tap));
}

/// Pre-seed the permanent ARP entry for this guest. The route to the guest
/// egresses the bridge, so the entry on `br-myc` is the one that matters —
/// it is what killed the boot benchmark's p95 tail (a probe leaving before
/// the guest brings eth0 up used to sit ~1 s on an INCOMPLETE neighbour
/// entry; proof: docs/benchmarks/microvm-tail-analysis.md). Permanent
/// entries work before the VM even boots; the twin entry on the tap keeps
/// the pre-bridge behavior. Idempotent.
pub(crate) fn seed_guest_arp(lease: &Lease) {
    for dev in [BRIDGE_NAME, lease.tap.as_str()] {
        let _ = ip_run(&[
            "neigh",
            "replace",
            &lease.ip_str,
            "lladdr",
            &lease.mac,
            "dev",
            dev,
            "nud",
            "permanent",
        ]);
    }
}

/// Wait until the VMM has created the tap, then attach it to the bridge
/// and bring it up. Bounded (~2 s); false on timeout.
fn attach_tap(lease: &Lease) -> bool {
    for _ in 0..200 {
        if ip_run(&["link", "set", &lease.tap, "master", BRIDGE_NAME]) {
            let _ = ip_run(&["link", "set", &lease.tap, "up"]);
            seed_guest_arp(lease);
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    false
}

/// Tear the host side of one lease down (idempotent): the ARP entries on
/// bridge and tap, then the tap itself (normally already gone with the
/// VMM). The bridge persists between runs — docker0-style, by design.
pub(crate) fn detach_tap(lease: &Lease) {
    for dev in [BRIDGE_NAME, lease.tap.as_str()] {
        let _ = ip_run(&["neigh", "del", &lease.ip_str, "dev", dev]);
    }
    let _ = ip_run(&["link", "del", &lease.tap]);
}

/// The exact iptables rules for one published port (add and delete use
/// nearly the same spec — see [`VmRun::release`], which flips `-I`/`-A` to
/// `-D`). Two kinds of rules per port:
/// - `nat PREROUTING --dport <host> -j DNAT <guest>` for EXTERNAL clients
///   (any destination address — the standard `docker -p` semantics);
/// - two `filter FORWARD` ACCEPTs, inserted at the TOP of the chain so
///   Docker's DROP-everything FORWARD policy cannot shadow them (appended
///   rules matched 0 packets behind `DOCKER-FORWARD` — measured
///   2026-11-04), matching on `br-myc` since routed traffic egresses the
///   bridge, not the tap.
///
/// Loopback clients are NOT handled here: `nat OUTPUT` DNAT of
/// 127.0.0.1-sourced packets silently does nothing under WSL2's kernel
/// (verified with tcpdump — the SYN never leaves the bridge), so
/// `127.0.0.1:<host>` is served by the userland [`crate::portproxy`]
/// relay instead, exactly like docker-proxy. Every rule carries the lease
/// tag (`myc-<n>`) so cleanup deletes exactly ours.
pub(crate) fn dnat_rules(
    tag: &str,
    host_port: u16,
    guest_port: u16,
    guest_ip: &str,
) -> Vec<Vec<String>> {
    let build =
        |op: &str, table: &str, chain: &str, match_args: Vec<String>, jump_args: Vec<String>| {
            let mut v = vec![
                "-t".to_string(),
                table.to_string(),
                op.to_string(),
                chain.to_string(),
            ];
            // -I takes the position right after the chain name (top of chain).
            if op == "-I" {
                v.push("1".into());
            }
            v.extend(match_args);
            v.push("-m".into());
            v.push("comment".into());
            v.push("--comment".into());
            v.push(tag.to_string());
            v.extend(jump_args);
            v
        };
    let dnat_jump = vec![
        "-j".to_string(),
        "DNAT".into(),
        "--to-destination".into(),
        format!("{guest_ip}:{guest_port}"),
    ];
    vec![
        build(
            "-A",
            "nat",
            "PREROUTING",
            vec![
                "-p".into(),
                "tcp".into(),
                "-m".into(),
                "tcp".into(),
                "--dport".into(),
                host_port.to_string(),
            ],
            dnat_jump,
        ),
        build(
            "-I",
            "filter",
            "FORWARD",
            vec![
                "-d".into(),
                format!("{guest_ip}/32"),
                "-o".into(),
                BRIDGE_NAME.into(),
            ],
            vec!["-j".into(), "ACCEPT".into()],
        ),
        build(
            "-I",
            "filter",
            "FORWARD",
            vec![
                "-s".into(),
                format!("{guest_ip}/32"),
                "-i".into(),
                BRIDGE_NAME.into(),
            ],
            vec!["-j".into(), "ACCEPT".into()],
        ),
    ]
}

/// Everything a started run holds; `Drop` releases it all, so a panic or
/// an early `?`-return cannot leak the VMM, the virtiofsd daemons, the
/// tap, the network lease, the DNAT rules or the state directory. The
/// bridge itself is NOT per-run — it persists between runs (docker0).
struct VmRun {
    ch: Option<Child>,
    ch_remote: PathBuf,
    api_socket: PathBuf,
    vfsds: Vec<Child>,
    state_dir: PathBuf,
    net: Option<VmNet>,
    lease: Option<Lease>,
    ipt_rules: Vec<Vec<String>>,
    /// Loopback relays for published ports (see [`crate::portproxy`]).
    proxies: Vec<crate::portproxy::PortProxy>,
    console_tail: Option<thread::JoinHandle<()>>,
    console_stop: Arc<AtomicBool>,
    run_dir: Option<tempfile::TempDir>,
    /// `release()` is idempotent (explicit call + Drop).
    released: bool,
}

impl VmRun {
    fn ch_remote_shutdown(&self) {
        if self.ch.is_some() {
            let _ = Command::new(&self.ch_remote)
                .arg("--api-socket")
                .arg(&self.api_socket)
                .arg("shutdown")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }

    /// Idempotent teardown of everything this run created.
    fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        // 1. Ask the VMM to shut down (also removes its tap), then reap.
        self.ch_remote_shutdown();
        let deadline = Instant::now() + STOP_TIMEOUT;
        if let Some(mut ch) = self.ch.take() {
            loop {
                match ch.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(10))
                    }
                    Ok(None) => {
                        let _ = ch.kill();
                        let _ = ch.wait();
                        break;
                    }
                    Err(_) => break,
                }
            }
        }
        // 2. Stop streaming the console, then the daemon pool.
        self.console_stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.console_tail.take() {
            let _ = handle.join();
        }
        for mut d in self.vfsds.drain(..) {
            let _ = d.kill();
            let _ = d.wait();
        }
        // 3. iptables rules (delete every instance of each spec; -A and -I
        // rules alike become -D, dropping the -I position argument — which
        // sits AFTER the chain name: [-t, table, -I, CHAIN, 1, ...], NOT
        // next to the operator).
        if !self.ipt_rules.is_empty() {
            for rule in &self.ipt_rules {
                let mut del = rule.clone();
                if del.len() > 4 && (del[2] == "-A" || del[2] == "-I") {
                    let was_insert = del[2] == "-I";
                    del[2] = "-D".into();
                    if was_insert && del[4] == "1" {
                        del.remove(4);
                    }
                }
                while iptables_run(&iptables_bin(), &del) {}
            }
        }
        // Belt and braces: a spec-by-spec delete can miss rules whose exact
        // spec drifted (observed once with the FORWARD ACCEPTs on
        // 2026-11-04) — the tag purge guarantees zero leftovers of OUR tag.
        if let Some(lease) = &self.lease {
            purge_tag_rules(&lease.tag);
        }
        // 3b. Loopback port relays (stops accepting; live connections
        // drain — the guest is gone but in-flight bytes already copied).
        for mut proxy in self.proxies.drain(..) {
            proxy.stop();
        }
        // 4. Lease teardown: neighbour entries (bridge + tap) and the tap
        // itself (normally already gone with the VMM), then the lease so
        // the next run can take the same slot. The bridge persists.
        if let (Some(net), Some(lease)) = (&self.net, &self.lease) {
            detach_tap(lease);
            release_lease(net, lease, &self.state_dir);
        }
        // 5. State dir last (holds the sockets + logs used above) — unless
        // MYCEL_VM_KEEP=1 (post-mortem debugging aid).
        if std::env::var("MYCEL_VM_KEEP").ok().as_deref() == Some("1") {
            eprintln!("[myc-vm] state dir kept: {}", self.state_dir.display());
        } else {
            let _ = std::fs::remove_dir_all(&self.state_dir);
        }
    }
}

impl Drop for VmRun {
    fn drop(&mut self) {
        self.release();
    }
}

/// Spawn virtiofsd for one share and wait until its socket is listening.
fn spawn_virtiofsd(
    bin: &Path,
    dir: &Path,
    sock: &Path,
    read_only: bool,
    log: &Path,
) -> Result<Child> {
    let mut cmd = Command::new(bin);
    cmd.arg("--shared-dir")
        .arg(dir)
        .arg("--socket-path")
        .arg(sock)
        // "none" keeps the daemon working unprivileged (no mount ns).
        .arg("--sandbox")
        .arg("none")
        .arg("--cache")
        .arg("auto")
        .arg("--thread-pool-size")
        .arg("0")
        .arg("--log-level")
        .arg("warn");
    if read_only {
        cmd.arg("--readonly");
    }
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::from(std::fs::File::create(log)?))
        .stderr(Stdio::from(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log)?,
        ))
        .spawn()
        .map_err(|e| BackendError::Unavailable(format!("spawn virtiofsd: {e}")))?;
    // The socket file appears when the daemon is listening (~10 ms).
    for _ in 0..200 {
        if sock.exists() {
            return Ok(child);
        }
        thread::sleep(Duration::from_millis(5));
    }
    Err(BackendError::Unavailable(format!(
        "virtiofsd socket never appeared: {} (see the log next to it)",
        sock.display()
    )))
}

/// Stream the guest console file to our stdout while the VM runs, so the
/// workload's output is live even though the VMM writes it to a file.
/// Polls at 10 ms and does one final drain on stop — the whole output of
/// short runs (`echo hello`) still lands before `run()` returns.
///
/// The virtio console is a tty: the guest kernel translates LF to CRLF
/// (ONLCR), which would pollute piped output with stray CR. The stream is
/// therefore normalized to Unix line endings — a CR is dropped only when
/// directly followed by LF, and a pending CR carries across poll chunks.
///
/// Protocol lines of the snapshot-path guest agent (and busybox's reboot
/// banner) are DROPPED: they are host-side debug markers, not workload
/// output, and would break the stdout contract (`myc run … -- echo hello`
/// must print exactly `hello`). Everything else passes through untouched.
pub(crate) fn spawn_console_tailer(path: PathBuf, stop: Arc<AtomicBool>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut offset: u64 = 0;
        let mut pending_cr = false;
        let mut line = Vec::new();
        let emit = |line: &mut Vec<u8>| {
            if console_line_is_protocol(line) {
                line.clear();
                return;
            }
            line.push(b'\n');
            let mut stdout = std::io::stdout().lock();
            let _ = stdout.write_all(line);
            let _ = stdout.flush();
            line.clear();
        };
        let pump = |offset: &mut u64, pending_cr: &mut bool, line: &mut Vec<u8>| {
            if let Ok(mut f) = std::fs::File::open(&path) {
                if f.seek(SeekFrom::Start(*offset)).is_ok() {
                    let mut buf = Vec::new();
                    if f.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
                        *offset += buf.len() as u64;
                        for &b in &buf {
                            if *pending_cr {
                                *pending_cr = false;
                                if b != b'\n' {
                                    line.push(b'\r'); // literal CR, not CRLF
                                }
                            }
                            if b == b'\r' {
                                *pending_cr = true;
                            } else if b == b'\n' {
                                emit(line);
                            } else {
                                line.push(b);
                            }
                        }
                    }
                }
            }
        };
        while !stop.load(Ordering::SeqCst) {
            pump(&mut offset, &mut pending_cr, &mut line);
            thread::sleep(Duration::from_millis(10));
        }
        pump(&mut offset, &mut pending_cr, &mut line); // final drain
        if pending_cr {
            line.push(b'\r');
        }
        if !line.is_empty() {
            emit(&mut line);
        }
    })
}

/// True for the snapshot agent's protocol lines and the busybox reboot
/// banner (see [`spawn_console_tailer`]).
fn console_line_is_protocol(line: &[u8]) -> bool {
    let text = String::from_utf8_lossy(line);
    let text = text.trim_end_matches('\r');
    text == "reboot: Restarting system"
        || text == "myc-agent: ready"
        || text == "myc-agent: timeout"
        || text.starts_with("myc-agent: running: ")
}

impl ExecBackend for KvmMicroVm {
    fn name(&self) -> &'static str {
        "kvm-microvm"
    }

    fn probe(&self, _store_root: &StoreRoot) -> BackendReport {
        let kvm = Self::kvm_usable();
        let ch = Self::file_exists(&self.ch_bin);
        let remote = Self::file_exists(&self.ch_remote);
        // Guest assets count as present when the download cache holds a
        // size-checked copy — that is exactly what `run()` would use, with
        // no network involved (see vm_asset_source).
        let vm_kernel_path =
            Self::vm_asset_source(&self.vm_kernel, "vmlinux-vmfs", MIN_KERNEL_BYTES);
        let initramfs_path =
            Self::vm_asset_source(&self.vm_initramfs, "init-vm.cpio", MIN_INITRAMFS_BYTES);
        let vm_kernel = vm_kernel_path.is_some();
        let initramfs = initramfs_path.is_some();
        let virtiofsd = Self::file_exists(&self.virtiofsd);
        let mkfs = Self::file_exists(&self.mkfs_ext4);
        let ipt = Self::file_exists(&self.iptables);
        // Snapshot accelerator pieces — presence only, no download here
        // (mirrors the kernel/initramfs rows: auto-provisioned at first use).
        let cache = vm_assets_cache();
        let fc_bin_ok =
            Self::file_exists(&self.fc_bin) || Self::file_exists(&cache.join("firecracker"));
        let fc_kernel_ok = Self::file_exists(&self.fc_kernel)
            || Self::file_exists(&cache.join(crate::firecracker::FC_KERNEL_ASSET));
        // The guest subnet must not collide with any existing interface
        // (docker0, eth0, …) — the bridge refuses to operate in that case.
        let subnet_check = match vm_net() {
            Ok(net) => match subnet_conflict(&net) {
                None => BackendCheck {
                    name: "vm subnet".into(),
                    ok: true,
                    detail: format!(
                        "{} free — bridge {BRIDGE_NAME} holds {}",
                        net.cidr, net.host_prefix
                    ),
                    fix: None,
                },
                Some((dev, cidr)) => BackendCheck {
                    name: "vm subnet".into(),
                    ok: false,
                    detail: format!("{cidr} on '{dev}' overlaps {}", net.cidr),
                    fix: Some("free that interface or set MYCEL_VM_CIDR to another /24".into()),
                },
            },
            Err(e) => BackendCheck {
                name: "vm subnet".into(),
                ok: false,
                detail: e.to_string(),
                fix: Some(format!("set MYCEL_VM_CIDR to a /24 like {DEFAULT_VM_CIDR}")),
            },
        };
        let checks = vec![
            BackendCheck {
                name: "kvm".into(),
                ok: kvm,
                detail: if kvm {
                    "/dev/kvm opens read-write".into()
                } else {
                    "/dev/kvm missing or not permitted".into()
                },
                fix: (!kvm).then(|| {
                    "install/enable KVM; add your user to the 'kvm' group".into()
                }),
            },
            BackendCheck {
                name: "cloud-hypervisor".into(),
                ok: ch && remote,
                detail: format!("{} , {}", self.ch_bin.display(), self.ch_remote.display()),
                fix: (!(ch && remote)).then(|| "install cloud-hypervisor v54.0 (see bench/microvm/README)".into()),
            },
            BackendCheck {
                name: "engine kernel".into(),
                ok: vm_kernel,
                detail: format!(
                    "{} (VIRTIO_FS + EXT4 built in)",
                    vm_kernel_path
                        .unwrap_or_else(|| self.vm_kernel.clone())
                        .display()
                ),
                fix: (!vm_kernel).then(|| {
                    "auto-downloaded on the first kvm-microvm run; or build it: \
                     bench/microvm/build_kernel_vmfs.sh && sudo bench/microvm/mkinit-vm.sh --install"
                        .into()
                }),
            },
            BackendCheck {
                name: "engine initramfs".into(),
                ok: initramfs,
                detail: format!(
                    "{} (generic guest init)",
                    initramfs_path
                        .unwrap_or_else(|| self.vm_initramfs.clone())
                        .display()
                ),
                fix: (!initramfs).then(|| {
                    "auto-downloaded on the first kvm-microvm run; or: sudo \
                     bench/microvm/mkinit-vm.sh --install (or install.sh --with-vm)"
                        .into()
                }),
            },
            BackendCheck {
                name: "virtiofsd".into(),
                ok: virtiofsd,
                detail: format!(
                    "{} — voie B (zero-copy rootfs + bind mounts)",
                    self.virtiofsd.display()
                ),
                fix: (!virtiofsd).then(|| "sudo apt install virtiofsd (else ext4 fallback only)".into()),
            },
            BackendCheck {
                name: "mkfs.ext4".into(),
                ok: mkfs,
                detail: format!("{} — voie A fallback", self.mkfs_ext4.display()),
                fix: (!mkfs).then(|| "sudo apt install e2fsprogs".into()),
            },
            BackendCheck {
                name: "iptables".into(),
                ok: ipt,
                detail: format!(
                    "{} for published ports — bridge + tap + DNAT need a root myc ({}), \
                     runs without -p need no privileges",
                    self.iptables.display(),
                    if is_root() { "currently root" } else { "currently non-root" }
                ),
                fix: (!ipt).then(|| "sudo apt install iptables (only needed for -p)".into()),
            },
            BackendCheck {
                name: "firecracker".into(),
                ok: fc_bin_ok && fc_kernel_ok,
                detail: format!(
                    "{} + kernel ELF — snapshot accelerator (repeated runs restore a \
                     paused microVM instead of booting; disable with MYCEL_SNAPSHOT_CACHE=0)",
                    self.fc_bin.display()
                ),
                fix: (!(fc_bin_ok && fc_kernel_ok)).then(|| {
                    "auto-downloaded/copied on the first run; or install firecracker \
                     v1.17.0 and build the vmfs kernel (bench/microvm/build_kernel_vmfs.sh)"
                        .into()
                }),
            },
            subnet_check,
        ];
        // Non-x86_64 hosts: state the real reason up front instead of a
        // wall of failing binary checks. The aarch64 pipeline publishes
        // the guest assets, but no engine run has been validated there —
        // `myc doctor` must say exactly that (no simulated availability).
        let mut checks = checks;
        if !ENGINE_ARCH_SUPPORTED {
            checks.insert(
                0,
                BackendCheck {
                    name: "guest arch".into(),
                    ok: false,
                    detail: format!(
                        "{} host — aarch64 guests: assets published (vmlinux-vmfs-aarch64, \
                         init-vm-aarch64.cpio), engine wiring pending hardware validation",
                        std::env::consts::ARCH
                    ),
                    fix: Some(
                        "use the default native-namespaces engine on aarch64; kvm-microvm \
                         executes x86_64 guests only today"
                            .into(),
                    ),
                },
            );
        }
        BackendReport {
            backend: self.name().to_string(),
            available: self.engine_available(),
            version: None,
            checks,
        }
    }

    fn run(&self, store_root: &StoreRoot, request: &RunRequest) -> Result<i32> {
        let trace = std::env::var("MYCEL_VM_TRACE").ok().as_deref() == Some("1");
        let t0 = Instant::now();
        let mut mark = t0;
        let step = |name: &str, mark: &mut Instant| {
            if trace {
                let now = Instant::now();
                eprintln!("[myc-vm] {name}: {} ms", (now - *mark).as_millis());
                *mark = now;
            }
        };

        // --- prerequisites ----------------------------------------------
        // Arch gate first: on aarch64 nothing below is wired (and the
        // auto-provision would fetch the x86_64 guest kernel — worse than
        // useless). Folded away entirely on x86_64 builds.
        if !ENGINE_ARCH_SUPPORTED {
            return Err(BackendError::Unavailable(
                "kvm-microvm executes x86_64 guests only — on aarch64 the guest assets are \
                 published but the engine wiring is pending hardware validation (see \
                 `myc doctor`)"
                    .into(),
            ));
        }
        if !Self::kvm_usable() {
            return Err(BackendError::Unavailable(
                "/dev/kvm is not usable (missing, or no permission — try the kvm group)".into(),
            ));
        }
        for (path, role) in [
            (&self.ch_bin, "cloud-hypervisor"),
            (&self.ch_remote, "ch-remote"),
        ] {
            if !path.is_file() {
                return Err(BackendError::Unavailable(format!(
                    "{role} not found at {} (see `myc doctor` for the fix)",
                    path.display()
                )));
            }
        }
        // Guest assets self-heal on binary installs: a missing kernel or
        // initramfs is downloaded once into ~/.mycel/vm-assets/ (see
        // resolve_vm_asset); the resolved paths may differ from the env
        // ones from here on.
        let vm_kernel = self.resolve_vm_asset(&self.vm_kernel, "vmlinux-vmfs", MIN_KERNEL_BYTES)?;
        let vm_initramfs =
            self.resolve_vm_asset(&self.vm_initramfs, "init-vm.cpio", MIN_INITRAMFS_BYTES)?;
        let voie = Voie::select(self)?;
        if voie == Voie::Ext4 && !request.binds.is_empty() {
            return Err(BackendError::Unavailable(
                "bind mounts need the virtiofs path — install the 'virtiofsd' package \
                 (the ext4 fallback is read-only)"
                    .into(),
            ));
        }

        // --- resolve the environment ------------------------------------
        let store = Store::open(store_root)?;
        let id = store.resolve(&request.reference)?;
        let manifest = store.get_manifest(&id)?;
        let command = manifest.command(&request.command);
        if command.is_empty() {
            return Err(BackendError::Unavailable(
                "manifest has no command to execute; pass one after `--`".into(),
            ));
        }

        // --- Firecracker snapshot accelerator (automatic when available) --
        // Repeated runs of the same environment restore a paused microVM
        // snapshot instead of booting a kernel (see
        // docs/benchmarks/snapshot-accelerator.md). Any missing piece falls
        // back to the Cloud Hypervisor path below with one log line.
        if !request.no_snapshot_cache {
            step("pre-snapshot (assets + store resolve)", &mut mark);
            match crate::firecracker::snapshot_run(
                self, store_root, request, &id, &manifest, &command,
            )? {
                crate::firecracker::SnapshotOutcome::Ran(code) => return Ok(code),
                crate::firecracker::SnapshotOutcome::Unavailable(why) => {
                    eprintln!(
                        "[myc-vm] snapshot path not used ({why}) — cloud-hypervisor path instead"
                    );
                }
            }
        }

        // --- plan: binds and published ports ------------------------------
        let cwd = std::env::current_dir()?;
        let mut binds = Vec::new();
        for spec in &request.binds {
            let (host, container, ro) = crate::split_bind(spec, &cwd)?;
            binds.push(Share {
                tag: format!("mycb{len}", len = binds.len()),
                host_dir: host,
                container_path: container,
                read_only: ro,
            });
        }
        // Host-style networking cannot exist inside a VM: emulate it by
        // publishing every port the image declares. `isolated_network`
        // (== --net none) keeps the VM off the network entirely.
        let mut ports: Vec<(u16, u16)> = request.publish.clone();
        if ports.is_empty() && !request.isolated_network {
            ports = manifest
                .config
                .exposed_ports
                .iter()
                .map(|p| (*p, *p))
                .collect();
        }
        // Networked whenever the run is not isolated: bridge + egress NAT
        // give outbound internet even without published ports (docker
        // daemon semantics). Root only — bridge/tap need it; a non-root
        // no-port run keeps the offline VM rather than a hard error, and
        // non-root port publishing still fails below with the fix hint.
        let with_net = !request.isolated_network && (is_root() || !ports.is_empty());
        if with_net && !is_root() {
            return Err(BackendError::Unavailable(
                "the kvm-microvm engine needs root to publish ports (bridge, tap, \
                 iptables) — run myc under sudo, or use --net none"
                    .into(),
            ));
        }

        // --- materialize + guest files ------------------------------------
        let run_dir = tempfile::Builder::new()
            .prefix("run-vm-")
            .tempdir_in(store.root().join("tmp"))
            .map_err(BackendError::Io)?;
        let rootfs = run_dir.path().to_path_buf();
        myc_run::materialize(&store, &manifest, &rootfs)?;
        step("materialize", &mut mark);

        // Environment, most general first (the guest exports in order, so
        // the last assignment of a key wins): sane defaults, then the
        // image's ENV, then -e (shared with the snapshot path — see
        // [`guest_env`]).
        let spec = GuestSpec {
            argv: command,
            env: guest_env(&manifest, request),
            workdir: request
                .workdir
                .clone()
                .or_else(|| Some(manifest.config.workdir.clone()).filter(|w| !w.is_empty())),
            hostname: if request.hostname.is_empty() {
                "mycel".into()
            } else {
                request.hostname.clone()
            },
            binds: binds.clone(),
            exit_dev: if voie == Voie::Ext4 {
                "/dev/vdb".into() // vda = root image, vdb = scratch
            } else {
                "/dev/vda".into() // the scratch disk is the only disk
            },
            dns: guest_dns(),
        };
        write_guest_files(&rootfs, &spec)?;
        step("guest files", &mut mark);

        // --- state dir, scratch disk, (voie A) ext4 image ------------------
        let state_dir = std::env::temp_dir().join(format!(
            "myc-vm-{}-{}",
            std::process::id(),
            VM_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&state_dir)?;
        let console = state_dir.join("console.log");
        let ch_log = std::fs::File::create(state_dir.join("ch.log"))?;
        let scratch = state_dir.join("scratch.img");
        std::fs::File::create(&scratch)?.set_len(SCRATCH_SIZE)?;
        let ext4_image = state_dir.join("rootfs.ext4");

        let mut guard = VmRun {
            ch: None,
            ch_remote: self.ch_remote.clone(),
            api_socket: state_dir.join("api.sock"),
            vfsds: Vec::new(),
            state_dir: state_dir.clone(),
            net: None,
            lease: None,
            ipt_rules: Vec::new(),
            proxies: Vec::new(),
            console_tail: None,
            console_stop: Arc::new(AtomicBool::new(false)),
            run_dir: Some(run_dir),
            released: false,
        };

        // --- voie-specific rootfs plumbing --------------------------------
        let mut root_arg = "mycroot=virtiofs".to_string();
        match voie {
            Voie::VirtioFs => {
                let sock = state_dir.join("vfsd-root.sock");
                guard.vfsds.push(spawn_virtiofsd(
                    &self.virtiofsd,
                    &rootfs,
                    &sock,
                    false,
                    &state_dir.join("vfsd-root.log"),
                )?);
                for b in &binds {
                    let sock = state_dir.join(format!("vfsd-{}.sock", b.tag));
                    guard.vfsds.push(spawn_virtiofsd(
                        &self.virtiofsd,
                        &b.host_dir,
                        &sock,
                        b.read_only,
                        &state_dir.join(format!("vfsd-{}.log", b.tag)),
                    )?);
                }
                step("virtiofsd", &mut mark);
            }
            Voie::Ext4 => {
                root_arg = "mycroot=ext4".into();
                let size = tree_size(&rootfs).saturating_mul(2).max(64 * 1024 * 1024);
                std::fs::File::create(&ext4_image)?.set_len(size)?;
                let status = Command::new(&self.mkfs_ext4)
                    .arg("-q")
                    .arg("-F")
                    .arg("-L")
                    .arg("myc-rootfs")
                    .arg("-d")
                    .arg(&rootfs)
                    .arg(&ext4_image)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::from(std::fs::File::create(
                        state_dir.join("mkfs.log"),
                    )?))
                    .status();
                match status {
                    Ok(s) if s.success() => {}
                    _ => {
                        return Err(BackendError::Unavailable(format!(
                            "mkfs.ext4 -d failed (see {})",
                            state_dir.join("mkfs.log").display()
                        )))
                    }
                }
                step("mkfs.ext4", &mut mark);
            }
        }

        // --- network lease (bridge + guest IP slot) --------------------------
        // Bridge first (idempotent, persists across runs), then the first
        // free lease — N VMs can hold ports at the same time now.
        let net_lease = if with_net {
            let net = vm_net()?;
            ensure_bridge(&net)?;
            let lease = allocate_lease(&net, &state_dir)?;
            // Permanent neighbour entry before the guest even boots.
            seed_guest_arp(&lease);
            guard.net = Some(net.clone());
            guard.lease = Some(lease.clone());
            Some((net, lease))
        } else {
            None
        };
        let mut cmdline = format!("console=hvc0 quiet loglevel=3 {root_arg}");
        if let Some((net, lease)) = &net_lease {
            cmdline.push_str(&format!(
                " mycip={} mycgw={}",
                lease.ip_str,
                dotted(net.host_ip)
            ));
        }
        let mem = std::env::var("MYCEL_VM_MEM").unwrap_or_else(|_| DEFAULT_VM_MEM.into());
        let mut ch_cmd = Command::new(&self.ch_bin);
        ch_cmd
            .arg("--api-socket")
            .arg(&guard.api_socket)
            .arg("--console")
            .arg(format!("file={}", console.display()))
            .arg("--kernel")
            .arg(&vm_kernel)
            .arg("--initramfs")
            .arg(&vm_initramfs)
            .arg("--cmdline")
            .arg(&cmdline)
            .arg("--cpus")
            .arg("boot=1")
            .arg("--memory")
            // vhost-user devices (virtiofs) require shared memory backing.
            .arg(if voie == Voie::VirtioFs {
                format!("size={mem},shared=on")
            } else {
                format!("size={mem}")
            });
        // Disk order fixes virtio-blk names: voie A mounts vda (the root
        // image, writable — a fresh per-run copy, and the guest must create
        // /proc /sys /dev which OCI images don't ship) and writes its exit
        // code to vdb; voie B has only the scratch disk (vda, matching the
        // GuestSpec above).
        if voie == Voie::Ext4 {
            ch_cmd.arg("--disk").arg(format!(
                "path={},readonly=off,image_type=raw",
                ext4_image.display()
            ));
        }
        ch_cmd.arg("--disk").arg(format!(
            "path={},readonly=off,image_type=raw",
            scratch.display()
        ));
        if voie == Voie::VirtioFs {
            ch_cmd.arg("--fs").arg(format!(
                "tag=mycroot,socket={}",
                state_dir.join("vfsd-root.sock").display()
            ));
            for b in &binds {
                ch_cmd.arg("--fs").arg(format!(
                    "tag={},socket={}",
                    b.tag,
                    state_dir.join(format!("vfsd-{}.sock", b.tag)).display()
                ));
            }
        }
        if let Some((_, lease)) = &net_lease {
            // The VMM creates the tap; no ip= field — the bridge carries
            // the subnet's host address, the lease names this VM's slot.
            ch_cmd
                .arg("--net")
                .arg(format!("tap={},mac={}", lease.tap, lease.mac));
        }
        let child = ch_cmd
            .stdin(Stdio::null())
            .stdout(Stdio::from(ch_log.try_clone()?))
            .stderr(Stdio::from(ch_log))
            .spawn()
            .map_err(|e| BackendError::Unavailable(format!("spawn cloud-hypervisor: {e}")))?;
        guard.ch = Some(child);
        step("vmm spawn", &mut mark);

        guard.console_tail = Some(spawn_console_tailer(console, guard.console_stop.clone()));

        if let Some((_, lease)) = &net_lease {
            // Attach the VMM-created tap to the bridge (the permanent ARP
            // entry is already seeded — seed_guest_arp above).
            if !attach_tap(lease) {
                return Err(BackendError::Unavailable(format!(
                    "tap {} never appeared to join {BRIDGE_NAME} (VMM log: {})",
                    lease.tap,
                    state_dir.join("ch.log").display()
                )));
            }
            let ipt = iptables_bin();
            // Never stack rules: purge this tag's leftovers (SIGKILLed
            // previous run, re-entered publish path) before adding ours.
            purge_tag_rules(&lease.tag);
            for (host, guest_port) in &ports {
                for rule in dnat_rules(&lease.tag, *host, *guest_port, &lease.ip_str) {
                    if !iptables_run(&ipt, &rule) {
                        return Err(BackendError::Unavailable(format!(
                            "iptables rejected the DNAT rule for port {host} (running as root?)"
                        )));
                    }
                    guard.ipt_rules.push(rule);
                }
                // 127.0.0.1 cannot be DNAT'd out of loopback under WSL2's
                // kernel (see dnat_rules' doc) — serve loopback from a
                // userland relay bound to 127.0.0.1:<host>, docker-proxy
                // style. Fails cleanly when the host port is already taken.
                let target = SocketAddr::from((lease.ip, *guest_port));
                let proxy = crate::portproxy::PortProxy::start(*host, target).map_err(|e| {
                    BackendError::Unavailable(format!(
                        "cannot listen on 127.0.0.1:{host} to publish it: {e}"
                    ))
                })?;
                guard.proxies.push(proxy);
            }
            step("net (bridge + dnat + proxy)", &mut mark);
        }

        // --- wait for the workload (the guest powers the VM off itself) ----
        let interrupted = Arc::new(AtomicBool::new(false));
        {
            let flag = interrupted.clone();
            // Traps SIGINT *and* SIGTERM (the "termination" feature): a
            // Ctrl-C or a `kill` shuts the VM down through the normal path
            // instead of leaking it. Best effort if a handler exists.
            let _ = ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst));
        }
        let timeout = std::env::var("MYCEL_VM_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok());
        let deadline = timeout.map(|s| Instant::now() + Duration::from_secs(s));
        let mut timed_out = false;
        // The guest powers off by itself after the workload; on interrupt or
        // timeout we ask the VMM for an ACPI shutdown — but a guest kernel
        // without the ACPI power-button driver ignores it, so after
        // STOP_TIMEOUT we hard-kill the VMM instead of hanging forever.
        let mut killing_since: Option<Instant> = None;
        let mut announced = false;
        loop {
            let mut want_shutdown = false;
            if interrupted.load(Ordering::SeqCst) {
                if !announced {
                    eprintln!("[myc] interrupt — shutting the microVM down");
                    announced = true;
                }
                want_shutdown = true;
            }
            if let Some(dl) = deadline {
                if Instant::now() >= dl {
                    if !announced {
                        eprintln!("[myc] microVM timeout — shutting down");
                        announced = true;
                    }
                    timed_out = true;
                    want_shutdown = true;
                }
            }
            if want_shutdown {
                if killing_since.is_none() {
                    guard.ch_remote_shutdown();
                    killing_since = Some(Instant::now());
                } else if killing_since.map(|t| t.elapsed()).unwrap_or_default() > STOP_TIMEOUT {
                    if let Some(ch) = guard.ch.as_mut() {
                        let _ = ch.kill();
                    }
                }
            }
            match guard.ch.as_mut().unwrap().try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => thread::sleep(Duration::from_millis(20)),
                Err(e) => return Err(BackendError::Io(e)),
            }
        }
        step("guest run", &mut mark);

        // --- exit code ------------------------------------------------------
        let code = std::fs::read(&scratch)
            .ok()
            .and_then(|b| parse_exit_code(&b));
        let code = match code {
            Some(c) => c,
            None => {
                let last_lines: String = std::fs::read_to_string(state_dir.join("console.log"))
                    .unwrap_or_default()
                    .lines()
                    .rev()
                    .take(15)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n");
                let why = if timed_out {
                    "timed out before the guest reported"
                } else if interrupted.load(Ordering::SeqCst) {
                    "interrupted before the guest reported"
                } else {
                    "the guest never reported an exit code"
                };
                eprintln!("[myc] {why}; last console lines:\n{last_lines}");
                if interrupted.load(Ordering::SeqCst) {
                    130
                } else {
                    125
                }
            }
        };

        if request.keep_rootfs {
            if let Some(dir) = guard.run_dir.take() {
                let kept = dir.keep();
                eprintln!("rootfs kept at {}", kept.display());
            }
        }
        guard.release();
        if trace {
            eprintln!(
                "[myc-vm] total (voie {}): {} ms",
                voie.label(),
                t0.elapsed().as_millis()
            );
        }
        Ok(code)
    }
}

/// A running microVM; stop it (or let Drop do it) to release every resource
/// — the tap, the neighbour entries and the network lease. The shared
/// bridge persists (docker0-style).
pub struct RunningVm {
    child: Child,
    ch_remote: PathBuf,
    api_socket: PathBuf,
    state_dir: PathBuf,
    net: VmNet,
    lease: Lease,
    stopped: bool,
}

impl RunningVm {
    /// This VM's guest address (the lease it booted with).
    pub fn guest_ip(&self) -> &str {
        &self.lease.ip_str
    }

    /// Block until the guest accepts TCP on the bench port, or `timeout`
    /// elapses. Returns how long the guest took to become reachable.
    ///
    /// ICMP-first: a TCP SYN sent before the guest brings eth0 up is
    /// silently dropped (the permanent ARP entry means no resolution
    /// failure either), and the host's SYN retransmission backoff
    /// (~1+2+4 s) turns a sub-second boot into a multi-second outlier
    /// (campaign v2: 2/60 runs at ~9 s). The guest *kernel* answers ICMP
    /// echo as soon as eth0 has its address — no guest userspace needed —
    /// so we stream cheap pings until the first reply, and only then start
    /// connecting. If `ping` is missing the phase is skipped and the plain
    /// TCP probe loop below still works.
    pub fn wait_ready(&self, timeout: Duration) -> Result<Duration> {
        let start = Instant::now();
        let deadline = start + timeout;
        self.wait_guest_link_up(start + timeout.min(Duration::from_secs(10)));
        let addr = SocketAddr::from((self.lease.ip, GUEST_PORT));
        loop {
            if TcpStream::connect_timeout(&addr, Duration::from_millis(50)).is_ok() {
                return Ok(start.elapsed());
            }
            if Instant::now() >= deadline {
                return Err(BackendError::Unavailable(
                    "guest did not become ready before the timeout (see console log in the state dir)".into(),
                ));
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Stream one ICMP echo every 20 ms until the guest replies (first
    /// `bytes from` line), then return. Bounded by `until`; re-spawns the
    /// pinger while the tap does not exist yet (ping exits immediately
    /// with "network unreachable"). Best effort: any spawn failure ends
    /// the phase early and defers to TCP probing.
    fn wait_guest_link_up(&self, until: Instant) {
        use std::io::{BufRead, BufReader};
        while Instant::now() < until {
            let remaining = (until - Instant::now()).as_secs().max(1);
            let child = Command::new("ping")
                .args([
                    "-n",
                    "-i",
                    "0.02",
                    "-w",
                    &remaining.to_string(),
                    &self.lease.ip_str,
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();
            let mut child = match child {
                Ok(c) => c,
                // No ping on this host — not fatal, the TCP loop still probes.
                Err(_) => return,
            };
            let mut answered = false;
            if let Some(out) = child.stdout.take() {
                // One line per reply (iputils flushes per packet); the loop
                // ends on `-w` expiry, so the deadline above is honored.
                for line in BufReader::new(out).lines() {
                    match line {
                        Ok(l) if l.contains("bytes from") => {
                            answered = true;
                            break;
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
            }
            let _ = child.kill();
            let _ = child.wait();
            if answered {
                return;
            }
            // Either the tap is not up yet or the guest is still mute:
            // brief pause, then stream again.
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Clean shutdown: VMM shutdown (removes the tap), wait, kill if
    /// needed, release the lease (neighbour entries + tap + registry
    /// claim), remove the state dir. The bridge persists. Idempotent.
    pub fn stop(&mut self) -> Result<()> {
        if self.stopped {
            return Ok(());
        }
        let _ = Command::new(&self.ch_remote)
            .arg("--api-socket")
            .arg(&self.api_socket)
            .arg("shutdown")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let deadline = Instant::now() + STOP_TIMEOUT;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                Ok(None) => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
                Err(e) => return Err(BackendError::Io(e)),
            }
        }
        detach_tap(&self.lease);
        release_lease(&self.net, &self.lease, &self.state_dir);
        let _ = std::fs::remove_file(&self.api_socket);
        let _ = std::fs::remove_dir_all(&self.state_dir);
        self.stopped = true;
        Ok(())
    }
}

impl Drop for RunningVm {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

// ======================================================================
// Tests
// ======================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sh_quote_round_trip() {
        assert_eq!(sh_quote("simple"), "'simple'");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
        assert_eq!(sh_quote(""), "''");
        // A shell reading the quoted form back gets the original exactly —
        // including every character that is special to sh.
        let value = "echo 'hi' && exit $(printf '\\x41'); \"quoted\" `bt` $VAR";
        let script = format!("v={}\nprintf '%s' \"$v\"", sh_quote(value));
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            .output()
            .expect("sh");
        assert_eq!(String::from_utf8_lossy(&out.stdout), value);
    }

    #[test]
    fn exit_code_parsing() {
        assert_eq!(parse_exit_code(b"0\0\0\0"), Some(0));
        assert_eq!(parse_exit_code(b"7\0"), Some(7));
        assert_eq!(parse_exit_code(b"130"), Some(130));
        assert_eq!(parse_exit_code(b"255\0\0"), Some(255));
        assert_eq!(parse_exit_code(b""), None);
        assert_eq!(parse_exit_code(b"\0\0\0"), None);
        assert_eq!(parse_exit_code(b"abc"), None);
        // Digits terminated by garbage still parse (documented: "terminated
        // by NUL/garbage/EOF") — the scratch disk is zero-filled, so a real
        // code is always digit-then-NUL, but junk after digits is tolerated.
        assert_eq!(parse_exit_code(b"12x"), Some(12));
        assert_eq!(parse_exit_code(b"1234"), None); // exit codes are 0..255
    }

    #[test]
    fn run_env_shape_and_shell_round_trip() {
        let spec = GuestSpec {
            argv: vec!["/bin/sh".into(), "-c".into(), "echo 'hi'".into()],
            env: vec!["PATH=/bin".into(), "MSG=it's".into()],
            workdir: Some("/data".into()),
            hostname: "mycel".into(),
            binds: vec![Share {
                tag: "mycb0".into(),
                host_dir: "/tmp/x".into(),
                container_path: "/data".into(),
                read_only: true,
            }],
            exit_dev: "/dev/vdb".into(),
            dns: Vec::new(),
        };
        let env_file = run_env_content(&spec);
        for expected in [
            "MYC_ARGC=3",
            "MYC_ARG_0='/bin/sh'",
            "MYC_ENV_1='MSG=it'\\''s'",
            "MYC_WORKDIR='/data'",
            "MYC_BIND_TAG_0='mycb0'",
            "MYC_BIND_RO_0=1",
            "MYC_EXIT_DEV='/dev/vdb'",
        ] {
            assert!(
                env_file.contains(expected),
                "missing {expected} in:\n{env_file}"
            );
        }
        // The whole file must be valid shell: source it under /bin/sh and
        // check one round-tripped value.
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("run.env");
        std::fs::write(&f, &env_file).unwrap();
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(". {}; printf '%s' \"$MYC_ARG_2\"", f.display()))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "echo 'hi'");
    }

    #[test]
    fn run_json_shape() {
        let spec = GuestSpec {
            argv: vec!["echo".into(), "hi".into()],
            env: vec![],
            workdir: None,
            hostname: "mycel".into(),
            binds: vec![],
            exit_dev: "/dev/vda".into(),
            dns: Vec::new(),
        };
        let json = run_json_content(&spec);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["argv"][0], "echo");
        assert_eq!(parsed["exit_dev"], "/dev/vda");
        assert!(parsed.get("binds").is_some());
    }

    #[test]
    fn dnat_rules_cover_prerouting_forward_inserted() {
        let rules = dnat_rules("myc-3", 6399, 6379, "192.168.77.3");
        assert_eq!(rules.len(), 3);
        let joined: Vec<String> = rules.iter().map(|r| r.join(" ")).collect();
        // External clients: PREROUTING DNAT on any destination address…
        assert!(joined.iter().any(|r| r.contains("nat -A PREROUTING")
            && r.contains("--dport 6399")
            && !r.contains("127.0.0.1")
            && r.contains("192.168.77.3:6379")));
        // …and NO nat OUTPUT rule: loopback is served by the userland relay
        // (WSL2 kernels never apply that DNAT — see dnat_rules' doc).
        assert!(joined.iter().all(|r| !r.contains("OUTPUT")));
        // Both FORWARD directions ACCEPT, INSERTED at the top of the chain
        // (before Docker's DROP-everything DOCKER-FORWARD), on the bridge.
        let fwd: Vec<&String> = joined
            .iter()
            .filter(|r| r.contains("filter -I FORWARD 1"))
            .collect();
        assert_eq!(fwd.len(), 2);
        assert!(fwd
            .iter()
            .any(|r| r.contains("-d 192.168.77.3/32 -o br-myc")));
        assert!(fwd
            .iter()
            .any(|r| r.contains("-s 192.168.77.3/32 -i br-myc")));
        assert!(joined.iter().all(|r| r.contains("--comment myc-3")));
        // Cleanup flips -A/-I to -D (release() also drops the -I position).
        assert!(rules.iter().all(|r| {
            r.iter().any(|a| a == "-A")
                || (r.iter().any(|a| a == "-I") && r.contains(&"1".to_string()))
        }));
    }

    #[test]
    fn cidr_parsing_and_first_lease_default() {
        let net = parse_cidr(DEFAULT_VM_CIDR).unwrap();
        assert_eq!(net.base, [192, 168, 77, 0]);
        assert_eq!(net.host_ip, [192, 168, 77, 1]);
        assert_eq!(net.host_prefix, "192.168.77.1/24");
        // The first lease of the default subnet is the bench contract IP.
        let mut first = net.base;
        first[3] = 2;
        assert_eq!(first, GUEST_IP);
        // Stray host bits are normalized.
        assert_eq!(
            parse_cidr("10.5.9.77/24").unwrap().host_prefix,
            "10.5.9.1/24"
        );
        assert!(parse_cidr("10.0.0.0/16").is_err(), "only /24 supported");
        assert!(parse_cidr("10.0.0.0").is_err());
        assert!(parse_cidr("10.0.0/24").is_err());
        assert!(parse_cidr("10.0.0.300/24").is_err());
    }

    #[test]
    fn subnet_overlap_detection() {
        let ours = [192, 168, 77, 0];
        assert!(subnets_overlap(ours, 24, [192, 168, 77, 0], 24));
        assert!(subnets_overlap(ours, 24, [192, 168, 77, 200], 24));
        assert!(subnets_overlap(ours, 24, [192, 168, 76, 0], 22)); // wider swallows us
        assert!(subnets_overlap(ours, 24, [192, 168, 77, 9], 28)); // narrower inside us
        assert!(!subnets_overlap(ours, 24, [192, 168, 78, 0], 24));
        assert!(!subnets_overlap(ours, 24, [172, 17, 0, 0], 16)); // docker0 is fine
    }

    #[test]
    fn lease_mac_is_derived_from_ip() {
        assert_eq!(lease_mac([192, 168, 77, 2]), "52:54:00:77:4d:02");
        assert_eq!(lease_mac([192, 168, 77, 254]), "52:54:00:77:4d:fe");
        assert_eq!(lease_mac([10, 0, 9, 12]), "52:54:00:77:09:0c");
    }

    #[test]
    fn leases_are_sequential_reusable_and_self_healing() {
        let reg = tempfile::tempdir().unwrap();
        let net = parse_cidr("10.231.77.0/24").unwrap();
        let sd = tempfile::tempdir().unwrap();
        let mut live = BTreeSet::new();
        let foreign = BTreeSet::new();

        // Sequential from .2 — the bench determinism contract.
        let a = claim_first_free(reg.path(), &net, sd.path(), &mut live, &foreign).unwrap();
        assert_eq!(
            (a.n, a.ip_str.as_str(), a.tap.as_str(), a.mac.as_str()),
            (2, "10.231.77.2", "myc-2", "52:54:00:77:4d:02")
        );
        let b = claim_first_free(reg.path(), &net, sd.path(), &mut live, &foreign).unwrap();
        assert_eq!((b.n, b.ip_str.as_str()), (3, "10.231.77.3"));

        // A foreign tap on slot 4 is skipped: next free is 5.
        let mut with_foreign = BTreeSet::new();
        with_foreign.insert(4);
        let c = claim_first_free(reg.path(), &net, sd.path(), &mut live, &with_foreign).unwrap();
        assert_eq!(c.n, 5);

        // Releasing frees the slot for the next taker.
        release_lease_in(reg.path(), &b, sd.path());
        live.remove(&b.n);
        let d = claim_first_free(reg.path(), &net, sd.path(), &mut live, &foreign).unwrap();
        assert_eq!(d.n, 3);

        // A claim whose owner died is stale — the next taker reuses it.
        release_lease_in(reg.path(), &d, sd.path());
        live.remove(&d.n);
        let stale = reg.path().join("myc-3");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("pid"), "99999999").unwrap(); // no /proc/99999999
        let e = claim_first_free(reg.path(), &net, sd.path(), &mut live, &foreign).unwrap();
        assert_eq!(e.n, 3);

        // A live owner (this very process) blocks its slot.
        assert!(claim_lease_dir(reg.path(), 6));
        assert!(!claim_lease_dir(reg.path(), 6));
    }

    #[test]
    fn tree_size_counts_recursively() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), vec![0u8; 2048]).unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b"), vec![0u8; 10]).unwrap();
        assert!(tree_size(dir.path()) >= 2058);
    }

    #[test]
    fn voie_select_fails_without_any_backend() {
        let engine = KvmMicroVm {
            ch_bin: PathBuf::new(),
            ch_remote: PathBuf::new(),
            kernel: PathBuf::new(),
            virtiofsd: PathBuf::new(),
            vm_kernel: PathBuf::new(),
            vm_initramfs: PathBuf::new(),
            mkfs_ext4: PathBuf::new(),
            iptables: PathBuf::new(),
            fc_bin: PathBuf::new(),
            fc_kernel: PathBuf::new(),
        };
        assert!(Voie::select(&engine).is_err());
    }

    #[test]
    fn init_guest_is_valid_shell() {
        // The embedded stage-2 script must at least parse under /bin/sh.
        let out = std::process::Command::new("/bin/sh")
            .arg("-n")
            .arg("-c")
            .arg(INIT_GUEST)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "init-guest is not valid shell: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn console_protocol_lines_are_filtered_but_not_workload_output() {
        // Agent protocol lines and the busybox reboot banner never reach
        // myc's stdout (the stdout contract is the workload's output only).
        assert!(console_line_is_protocol(b"myc-agent: ready"));
        assert!(console_line_is_protocol(b"myc-agent: timeout"));
        assert!(console_line_is_protocol(b"myc-agent: running: echo hi"));
        assert!(console_line_is_protocol(b"reboot: Restarting system"));
        assert!(console_line_is_protocol(b"myc-agent: ready\r"));
        // Anything a workload might print passes through untouched.
        assert!(!console_line_is_protocol(b"hello"));
        assert!(!console_line_is_protocol(b"myc-agent: manual output"));
        assert!(!console_line_is_protocol(b""));
        assert!(!console_line_is_protocol(b"myc-agentX"));
    }

    #[test]
    fn asset_location_is_system_then_size_checked_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("vm-assets");
        std::fs::create_dir_all(&cache).unwrap();

        // Nothing anywhere → download needed.
        let expected = dir.path().join("vmlinux-vmfs");
        assert_eq!(
            locate_vm_asset(&expected, &cache, "vmlinux-vmfs", MIN_KERNEL_BYTES),
            AssetSource::Missing
        );

        // A user/sysadmin-placed file at the expected location always wins
        // (trusted input — no size policing on files we did not download).
        std::fs::write(&expected, b"tiny but deliberate").unwrap();
        assert_eq!(
            locate_vm_asset(&expected, &cache, "vmlinux-vmfs", MIN_KERNEL_BYTES),
            AssetSource::System(expected.clone())
        );

        // Cache hits must clear the minimum size: a truncated leftover is
        // Missing so the next run re-downloads it.
        let expected_bench = dir.path().join("vmlinux");
        std::fs::write(cache.join("init-vm.cpio"), vec![0u8; 100]).unwrap();
        assert_eq!(
            locate_vm_asset(&expected_bench, &cache, "init-vm.cpio", MIN_INITRAMFS_BYTES),
            AssetSource::Missing
        );
        std::fs::write(cache.join("init-vm.cpio"), vec![0u8; 128 * 1024]).unwrap();
        assert_eq!(
            locate_vm_asset(&expected_bench, &cache, "init-vm.cpio", MIN_INITRAMFS_BYTES),
            AssetSource::Cache(cache.join("init-vm.cpio"))
        );
    }

    #[test]
    fn resolve_vm_asset_returns_existing_paths_without_network() {
        let engine = KvmMicroVm {
            ch_bin: PathBuf::new(),
            ch_remote: PathBuf::new(),
            kernel: PathBuf::new(),
            virtiofsd: PathBuf::new(),
            vm_kernel: PathBuf::new(),
            vm_initramfs: PathBuf::new(),
            mkfs_ext4: PathBuf::new(),
            iptables: PathBuf::new(),
            fc_bin: PathBuf::new(),
            fc_kernel: PathBuf::new(),
        };
        let dir = tempfile::tempdir().unwrap();
        let kernel = dir.path().join("vmlinux-vmfs");
        std::fs::write(&kernel, vec![0u8; 64]).unwrap();
        // Present at the expected location → returned as-is; no cache, no
        // download (a network attempt would fail the test offline).
        assert_eq!(
            engine
                .resolve_vm_asset(&kernel, "vmlinux-vmfs", MIN_KERNEL_BYTES)
                .unwrap(),
            kernel
        );
    }
}
