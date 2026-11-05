//! macOS microVM backend — libkrun over Hypervisor.framework.
//!
//! This is the macOS tier of the engine matrix (`hvf-microvm`): the
//! container workload runs inside a real virtual machine with its own
//! kernel, so a container escape never shares a kernel with the host.
//! The VMM is [libkrun] v1.19.x — the same library `crun --krun` uses —
//! because one code path covers KVM on Linux and HVF on macOS (the
//! long-term reason libkrun was chosen, see `docs/isolation-engines.md`).
//!
//! # Model (mirrors `crun krun`, the reference embedder)
//!
//! * kernel: the published guest asset (`vmlinux-vmfs-aarch64` on arm64
//!   Macs, `vmlinux-vmfs` on x86_64) loaded with `krun_set_kernel` —
//!   libkrun 1.19.x bundles no kernel of its own;
//! * rootfs: the materialized rootfs directory shared read-write via
//!   virtiofs with `krun_set_root` — libkrun implements the virtiofs
//!   device in-process, so there is NO host daemon (no virtiofsd) and
//!   zero copy: the kvm engine's "voie B", natively;
//! * init: libkrun injects its own built-in init (`/init.krun`) into the
//!   root share — no initramfs is needed for this model;
//! * workload: `krun_set_exec(path, argv, envp)` + `krun_set_workdir`;
//! * stdio: `krun_start_enter` takes over stdin/stdout and wires the
//!   workload's stdio to the host terminal (virtio-console ports
//!   `krun-stdin`/`krun-stdout`/`krun-stderr`);
//! * exit code: `krun_start_enter` runs the guest IN-PROCESS and, when
//!   the workload finishes, **calls exit() with the workload's exit
//!   code** (contract of `include/libkrun.h` v1.19.7). On success this
//!   backend's `run()` therefore never returns — the `myc` process
//!   itself exits with the container's code, exactly like `crun krun`;
//!   codes 125/126/127 are libkrun's own init error codes.
//!
//! # Honesty charter (read before touching this file)
//!
//! NOTHING here has run on Apple hardware: no boot, no end-to-end run,
//! no performance number exists for this tier, so none is claimed
//! anywhere. `myc doctor` says UNVALIDATED until someone with a Mac
//! walks the checklist in `bench/macos-libkrun/NOTES.md`. Two guards
//! make the honesty structural:
//!
//! * `MYCEL_VM_EXPERIMENTAL=1` is required for the probe to report the
//!   engine available ([`decide_availability`]) — and it can never leak
//!   into the automatic engine choice anyway, because `detect_engines`
//!   reports the tier `complete: false` until it is validated;
//! * capabilities the libkrun model cannot carry yet are REFUSED with
//!   actionable errors instead of being silently dropped (bind mounts,
//!   published ports — see [`plan_run`]); hostname and `--map-user` are
//!   ignored and listed among the tier's degraded features.
//!
//! The pure decision logic (planning, availability, exec resolution,
//! config parsing) is platform-independent and unit-tested on Linux,
//! exactly like the `wsl` backend's.
//!
//! [libkrun]: https://github.com/libkrun/libkrun

use std::path::{Path, PathBuf};

use crate::{BackendError, Result, RunRequest};

// ======================================================================
// Pure configuration surface (compiled and tested on every platform)
// ======================================================================

/// libkrun guest-kernel asset for THIS host architecture. x86_64 Macs
/// boot the bzImage (`vmlinux-vmfs`), arm64 Macs the uncompressed Image
/// (`vmlinux-vmfs-aarch64`) — the same v0.3.1 release assets the kvm
/// engine downloads; neither is an ELF file (see [`default_kernel_format`]).
pub(crate) const KERNEL_ASSET: &str = if cfg!(target_arch = "aarch64") {
    "vmlinux-vmfs-aarch64"
} else {
    "vmlinux-vmfs"
};

/// Directory where a sysadmin installs guest kernels (same layout as the
/// kvm engine; `install.sh --with-vm`). The auto-provisioned fallback is
/// the download cache `~/.mycel/vm-assets/<asset>` (see [`crate::assets`]).
pub(crate) const DEFAULT_KERNEL_DIR: &str = "/usr/local/share/mycel";

/// Default guest memory spec, overridable with `MYCEL_VM_MEM`
/// (`512M`, `1G`, …; parsed by [`parse_mem_mib`]).
pub(crate) const DEFAULT_VM_MEM: &str = "512M";

/// `KRUN_KERNEL_FORMAT_RAW` of include/libkrun.h v1.19.7.
pub(crate) const KRUN_KERNEL_FORMAT_RAW: u32 = 0;
/// `KRUN_KERNEL_FORMAT_ELF` of include/libkrun.h v1.19.7.
pub(crate) const KRUN_KERNEL_FORMAT_ELF: u32 = 1;

/// Candidate locations of `libkrun.dylib`: Homebrew arm64
/// (`/opt/homebrew`), Homebrew Intel and from-source installs
/// (`/usr/local`), MacPorts (`/opt/local`). `MYCEL_VM_LIBKRUN` overrides
/// everything. There is no core Homebrew formula today — the libkrun/krun
/// tap (krunkit) and MacPorts ship it (see NOTES.md).
pub(crate) const LIBKRUN_CANDIDATES: [&str; 3] = [
    "/opt/homebrew/lib/libkrun.dylib",
    "/usr/local/lib/libkrun.dylib",
    "/opt/local/lib/libkrun.dylib",
];

/// The libkrun line this FFI surface was written against; the probe
/// reports it so a mismatch is diagnosable. libkrun exposes no runtime
/// version query (verified: no such symbol in libkrun.h v1.19.7).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) const EXPECTED_LIBKRUN: &str = "1.19.x";

/// First `MYCEL_VM_LIBKRUN`, then the standard candidates; `None` when no
/// libkrun installation is visible. An explicit override that does not
/// exist is returned anyway so the probe can name it precisely.
pub(crate) fn locate_libkrun() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("MYCEL_VM_LIBKRUN") {
        return Some(PathBuf::from(p));
    }
    LIBKRUN_CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
}

/// Parse `MYCEL_VM_MEM`-style sizes into MiB (`512M` → 512, `1G` → 1024,
/// bare `768` → 768). Anything else is a configuration error.
pub(crate) fn parse_mem_mib(spec: &str) -> Option<u32> {
    let spec = spec.trim();
    let split = spec
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(spec.len());
    let (num, unit) = spec.split_at(split);
    let num: u32 = num.parse().ok()?;
    if num == 0 {
        return None;
    }
    match unit.trim().to_ascii_uppercase().as_str() {
        "" | "M" | "MB" | "MIB" => Some(num),
        "G" | "GB" | "GIB" => num.checked_mul(1024),
        _ => None,
    }
}

/// Parse a `MYCEL_VM_KRUN_KERNEL_FORMAT` value into a
/// `KRUN_KERNEL_FORMAT_*` integer. Names are the header's own (RAW, ELF,
/// PE_GZ, IMAGE_BZ2, IMAGE_GZ, IMAGE_ZSTD); raw integers are accepted too
/// for forward compatibility.
pub(crate) fn parse_kernel_format(spec: &str) -> Option<u32> {
    match spec.trim().to_ascii_uppercase().as_str() {
        "RAW" => Some(KRUN_KERNEL_FORMAT_RAW),
        "ELF" => Some(KRUN_KERNEL_FORMAT_ELF),
        "PE_GZ" => Some(2),
        "IMAGE_BZ2" => Some(3),
        "IMAGE_GZ" => Some(4),
        "IMAGE_ZSTD" => Some(5),
        other => other.parse().ok(),
    }
}

/// Default kernel format for this architecture: both published assets are
/// raw kernel images (x86_64 bzImage, arm64 uncompressed Image), which
/// libkrun loads as [`KRUN_KERNEL_FORMAT_RAW`]. Overridable for
/// experimentation when a differently-packaged kernel is used.
pub(crate) fn default_kernel_format() -> u32 {
    KRUN_KERNEL_FORMAT_RAW
}

/// Guest environment, most general first (libkrun's init exports the
/// array in order, so the last assignment of a key wins): sane defaults,
/// then the image's ENV, then `-e`. Mirrors `kvm::guest_env`.
pub(crate) fn guest_env_for(image_env: &[String], request_env: &[String]) -> Vec<String> {
    let mut env: Vec<String> = vec![
        "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into(),
        "HOME=/root".into(),
    ];
    env.extend(image_env.iter().cloned());
    env.extend(request_env.iter().cloned());
    env
}

/// Resolve `argv0` against the materialized rootfs the way the guest
/// would: paths containing `/` are used as guest-absolute paths
/// (existence-checked in the rootfs), bare names are searched in the
/// guest `PATH` directories. libkrun's init `exec`s the resolved path
/// directly — it is not a shell, so the resolution happens here. Returns
/// the guest-absolute path or `None` (a clear run error).
pub(crate) fn resolve_exec(rootfs: &Path, argv0: &str, guest_path: &str) -> Option<String> {
    if argv0.contains('/') {
        let abs = format!("/{}", argv0.trim_start_matches('/'));
        return rootfs
            .join(abs.trim_start_matches('/'))
            .is_file()
            .then_some(abs);
    }
    let dirs = guest_path
        .split('=')
        .nth(1)
        .unwrap_or_default()
        .split(':')
        .filter(|d| !d.is_empty());
    for dir in dirs {
        if rootfs.join(dir).join(argv0).is_file() {
            return Some(format!("{dir}/{argv0}"));
        }
    }
    None
}

/// Inputs of [`plan_run`] that come from the resolved manifest, kept as
/// plain data so the planner stays testable without a store.
#[derive(Debug, Clone)]
pub(crate) struct ManifestInputs {
    /// The manifest's resolved command (entrypoint+cmd, or the `--`
    /// override) — same semantics as `myc_manifest::Manifest::command`.
    pub command: Vec<String>,
    /// The image's ENV layer.
    pub env: Vec<String>,
    /// The image's working directory ("" = none).
    pub workdir: String,
}

/// Everything `run()` will feed into libkrun, decided BEFORE any FFI
/// call so the mapping is pure and unit-testable on Linux.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KrunPlan {
    /// Host path of the guest kernel asset.
    pub kernel: PathBuf,
    /// `KRUN_KERNEL_FORMAT_*` (see [`parse_kernel_format`]).
    pub kernel_format: u32,
    /// Kernel command line. No `console=`: libkrun wires the console
    /// itself; `quiet loglevel=3` keeps printk out of the stdio the
    /// workload owns.
    pub cmdline: String,
    /// Host directory shared as the guest root (`/dev/root`, virtiofs).
    pub rootfs: PathBuf,
    /// Guest-absolute executable path (see [`resolve_exec`]).
    pub exec_path: String,
    /// argv for the workload, argv[0] included (the CLI's command[0] —
    /// `execv` conventions, not the resolved path).
    pub argv: Vec<String>,
    /// Environment assignments (see [`guest_env_for`]).
    pub envp: Vec<String>,
    /// Working directory as a guest path (see `krun_set_workdir`).
    pub workdir: Option<String>,
    /// vCPU count.
    pub vcpus: u8,
    /// Guest RAM in MiB.
    pub ram_mib: u32,
}

/// Decide the exact libkrun call plan for one run. Pure: the only
/// filesystem access is exec resolution inside the given rootfs. Every
/// capability the libkrun model cannot carry yet is REFUSED here (with
/// the fix in the message), never silently dropped:
///
/// * bind mounts — libkrun 1.19.x has no guest-mount API for extra
///   virtiofs shares (its built-in init mounts only the root); carrying
///   them would need a custom initramfs, like the kvm tier's
///   `/myc/init-guest` (future work, NOTES.md);
/// * published ports — the tier is network-less in v1, so there is
///   nothing to publish onto (gvproxy path: NOTES.md);
/// * `--map-user` / hostname — no libkrun API for uid maps or UTS
///   namespaces; ignored and documented, not pretended.
pub(crate) fn plan_run(
    request: &RunRequest,
    manifest: &ManifestInputs,
    rootfs: &Path,
    kernel: &Path,
    kernel_format: u32,
    vcpus: u8,
    ram_mib: u32,
) -> Result<KrunPlan> {
    if !request.binds.is_empty() {
        return Err(BackendError::Unavailable(format!(
            "bind mounts are not supported by the hvf-microvm engine yet ({} requested) — \
             libkrun's built-in init mounts only the root share; \
             see bench/macos-libkrun/NOTES.md",
            request.binds.len()
        )));
    }
    if !request.publish.is_empty() {
        return Err(BackendError::Unavailable(
            "publishing ports (-p) is not supported by the hvf-microvm engine yet — the tier is \
             network-less in v1; see bench/macos-libkrun/NOTES.md"
                .into(),
        ));
    }
    let command = if request.command.is_empty() {
        manifest.command.clone()
    } else {
        request.command.clone()
    };
    if command.is_empty() {
        return Err(BackendError::Unavailable(
            "manifest has no command to execute; pass one after `--`".into(),
        ));
    }
    let envp = guest_env_for(&manifest.env, &request.env);
    // The effective guest PATH (the last PATH= assignment wins).
    let guest_path = envp
        .iter()
        .rev()
        .find(|e| e.starts_with("PATH="))
        .map(String::as_str)
        .unwrap_or("");
    let exec_path = resolve_exec(rootfs, &command[0], guest_path).ok_or_else(|| {
        BackendError::Unavailable(format!(
            "executable '{}' not found in the guest rootfs {} (searched the guest PATH)",
            command[0],
            rootfs.display()
        ))
    })?;
    let workdir = request
        .workdir
        .clone()
        .or_else(|| Some(manifest.workdir.clone()).filter(|w| !w.is_empty()));
    if let Some(w) = &workdir {
        if !w.starts_with('/') {
            return Err(BackendError::Unavailable(format!(
                "workdir '{w}' must be an absolute guest path"
            )));
        }
    }
    Ok(KrunPlan {
        kernel: kernel.to_path_buf(),
        kernel_format,
        cmdline: "quiet loglevel=3".into(),
        rootfs: rootfs.to_path_buf(),
        exec_path,
        argv: command,
        envp,
        workdir,
        vcpus,
        ram_mib,
    })
}

/// The honest availability state machine of this tier. Pure; the macOS
/// `probe()` renders it, the Linux tests pin it. `available` requires ALL
/// of: libkrun.dylib present, guest kernel present (system location or
/// size-checked download cache — an explicit run may still auto-download
/// it), and the explicit experimental opt-in. Hardware validation CANNOT
/// be checked from code — that is the point of the guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Availability {
    pub available: bool,
    pub experimental: bool,
    pub libkrun_found: bool,
    pub kernel_present: bool,
}

pub(crate) fn decide_availability(
    libkrun_found: bool,
    kernel_present: bool,
    experimental: bool,
) -> Availability {
    Availability {
        available: libkrun_found && kernel_present && experimental,
        experimental,
        libkrun_found,
        kernel_present,
    }
}

// ======================================================================
// The engine (env resolution is portable; probe/run are macOS-only)
// ======================================================================

/// The macOS microVM engine: paths and sizes resolved from env or
/// defaults. Construction is portable (pure env parsing) so the
/// resolution itself is testable anywhere; only `probe`/`run` touch
/// libkrun and are compiled on macOS.
#[derive(Debug, Clone)]
pub struct Vm {
    /// Resolved libkrun library (or the override that SHOULD be there).
    pub libkrun: PathBuf,
    /// Expected guest-kernel location (the system dir; the download
    /// cache is checked at probe/run time through [`crate::assets`]).
    pub kernel: PathBuf,
    /// Raw memory spec (`MYCEL_VM_MEM`), parsed per run.
    pub mem: String,
    /// vCPU count (`MYCEL_VM_CPUS`).
    pub cpus: u8,
}

impl Vm {
    /// Resolve paths from `MYCEL_VM_LIBKRUN`, `MYCEL_VM_KRUN_KERNEL`,
    /// `MYCEL_VM_MEM`, `MYCEL_VM_CPUS` or the documented defaults.
    pub fn from_env() -> Self {
        let kernel = std::env::var("MYCEL_VM_KRUN_KERNEL")
            .map(PathBuf::from)
            .unwrap_or_else(|_| Path::new(DEFAULT_KERNEL_DIR).join(KERNEL_ASSET));
        Self {
            libkrun: locate_libkrun().unwrap_or_else(|| PathBuf::from(LIBKRUN_CANDIDATES[0])),
            kernel,
            mem: std::env::var("MYCEL_VM_MEM").unwrap_or_else(|_| DEFAULT_VM_MEM.into()),
            cpus: std::env::var("MYCEL_VM_CPUS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1),
        }
    }

    /// The effective kernel source RIGHT NOW, without downloading: the
    /// expected system location, or the size-checked cache — the exact
    /// decision the kvm engine's probe makes for its assets.
    #[cfg(target_os = "macos")]
    fn kernel_source(&self) -> Option<PathBuf> {
        match crate::assets::locate_vm_asset(
            &self.kernel,
            &crate::assets::vm_assets_cache(),
            KERNEL_ASSET,
            crate::assets::MIN_KERNEL_BYTES,
        ) {
            crate::assets::AssetSource::System(p) | crate::assets::AssetSource::Cache(p) => Some(p),
            crate::assets::AssetSource::Missing => None,
        }
    }

    #[cfg(target_os = "macos")]
    fn experimental() -> bool {
        std::env::var("MYCEL_VM_EXPERIMENTAL").ok().as_deref() == Some("1")
    }

    #[cfg(target_os = "macos")]
    fn availability(&self) -> (Availability, Option<PathBuf>) {
        let libkrun_found = locate_libkrun().is_some_and(|p| p.is_file());
        let kernel = self.kernel_source();
        (
            decide_availability(libkrun_found, kernel.is_some(), Self::experimental()),
            kernel,
        )
    }

    /// Guard every run: the honest three-factor gate, with one
    /// actionable error per missing factor.
    #[cfg(target_os = "macos")]
    fn require_available(&self) -> Result<()> {
        let (avail, _) = self.availability();
        if avail.available {
            return Ok(());
        }
        if !avail.experimental {
            return Err(BackendError::Unavailable(
                "the hvf-microvm engine is EXPERIMENTAL and unvalidated on Apple hardware — \
                 opt in explicitly with MYCEL_VM_EXPERIMENTAL=1 (bench/macos-libkrun/NOTES.md)"
                    .into(),
            ));
        }
        if !avail.libkrun_found {
            return Err(BackendError::Unavailable(format!(
                "libkrun.dylib not found (looked at {} and $MYCEL_VM_LIBKRUN) — install it \
                 (brew tap libkrun/krun; or 'sudo port install libkrun'; or build from source, \
                 see bench/macos-libkrun/NOTES.md)",
                self.libkrun.display()
            )));
        }
        Err(BackendError::Unavailable(format!(
            "guest kernel '{}' not found (neither system location nor download cache) — it \
             auto-downloads from the mycel releases when the network allows, or build it: \
             bench/microvm/build_kernel_vmfs.sh ARCH={arch}",
            KERNEL_ASSET,
            arch = if cfg!(target_arch = "aarch64") {
                "aarch64"
            } else {
                "x86_64"
            }
        )))
    }
}

#[cfg(target_os = "macos")]
mod ffi {
    //! Runtime-loaded FFI to libkrun v1.19.x.
    //!
    //! Why dlopen (manual, no `libloading` dependency): `myc` must start
    //! and report an honest `myc doctor` on Macs WITHOUT libkrun
    //! installed. Linking `extern "C"` against `libkrun.dylib` at build
    //! time would make the whole binary fail to load when the dylib is
    //! absent, and would pin every install to a compile-time library
    //! path. dlopen lets the probe check presence first and `run()` load
    //! the library only when actually executing a guest. Declaring
    //! `dlopen`/`dlsym` ourselves (they live in libSystem, which is
    //! always linked on macOS) keeps myc-backend free of a new
    //! dependency; the cost is this small justified-`unsafe` module.
    //!
    //! Signatures are copied verbatim from include/libkrun.h of the
    //! libkrun v1.19.7 tag (surveyed 2026-11-03; the full header survey
    //! is archived in bench/macos-libkrun/NOTES.md).

    use std::ffi::{c_char, c_int, c_void, CString};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    use std::sync::OnceLock;

    use crate::{BackendError, Result};

    // macOS <dlfcn.h>: RTLD_NOW = 0x2, RTLD_LOCAL = 0x4 (on Linux
    // RTLD_LOCAL is 0 — keep these constants macOS-local).
    const RTLD_NOW: c_int = 0x2;
    const RTLD_LOCAL: c_int = 0x4;

    // SAFETY: dlopen/dlsym/dlerror are libSystem C functions present on
    // every macOS, with the platform's own calling convention.
    unsafe extern "C" {
        fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
        fn dlerror() -> *const c_char;
    }

    /// The krun_* functions this engine calls. The handle intentionally
    /// has no Drop: the guest runs until process exit (the
    /// `krun_start_enter` contract) and libkrun has no unregister API.
    #[allow(non_snake_case)]
    pub(super) struct Libkrun {
        /// Kept (never read) on purpose: it owns the dlopen handle. No
        /// Drop — the guest lives until process exit (`krun_start_enter`
        /// contract) and libkrun has no unregister API worth calling.
        _handle: *mut c_void,
        pub(super) krun_set_log_level: unsafe extern "C" fn(level: u32) -> i32,
        pub(super) krun_create_ctx: unsafe extern "C" fn() -> i32,
        pub(super) krun_free_ctx: unsafe extern "C" fn(ctx_id: u32) -> i32,
        pub(super) krun_set_vm_config:
            unsafe extern "C" fn(ctx_id: u32, num_vcpus: u8, ram_mib: u32) -> i32,
        pub(super) krun_set_kernel: unsafe extern "C" fn(
            ctx_id: u32,
            kernel_path: *const c_char,
            kernel_format: u32,
            initramfs: *const c_char,
            cmdline: *const c_char,
        ) -> i32,
        pub(super) krun_set_root:
            unsafe extern "C" fn(ctx_id: u32, root_path: *const c_char) -> i32,
        pub(super) krun_set_workdir:
            unsafe extern "C" fn(ctx_id: u32, workdir_path: *const c_char) -> i32,
        pub(super) krun_set_exec: unsafe extern "C" fn(
            ctx_id: u32,
            exec_path: *const c_char,
            argv: *const *const c_char,
            envp: *const *const c_char,
        ) -> i32,
        pub(super) krun_start_enter: unsafe extern "C" fn(ctx_id: u32) -> i32,
    }

    // SAFETY: the handle is an opaque libkrun handle only ever passed
    // back into libkrun; the function pointers are thread-safe C ABI.
    unsafe impl Send for Libkrun {}
    unsafe impl Sync for Libkrun {}

    /// dlopen + resolve every symbol; loaded at most once per process
    /// (repeated failures are remembered too, so a broken dylib is
    /// reported consistently).
    pub(super) fn open(path: &Path) -> Result<&'static Libkrun> {
        static LOADED: OnceLock<std::result::Result<Libkrun, String>> = OnceLock::new();
        if let Some(prev) = LOADED.get() {
            return prev
                .as_ref()
                .map_err(|e| BackendError::Unavailable(e.clone()));
        }
        let built = (|| -> Result<Libkrun> {
            let cpath = CString::new(path.as_os_str().as_bytes())
                .map_err(|e| BackendError::Unavailable(format!("libkrun path: {e}")))?;
            // SAFETY: cpath outlives the call. RTLD_NOW resolves every
            // symbol up front, so a mismatched build fails HERE, not
            // mid-boot.
            let handle = unsafe { dlopen(cpath.as_ptr(), RTLD_NOW | RTLD_LOCAL) };
            if handle.is_null() {
                // SAFETY: dlerror returns a thread-local C string valid
                // until the next dl* call on this thread; copied out
                // immediately.
                let err = unsafe {
                    let p = dlerror();
                    if p.is_null() {
                        "unknown dlopen error".to_string()
                    } else {
                        std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
                    }
                };
                return Err(BackendError::Unavailable(format!(
                    "cannot load {}: {err} (if this mentions code signing or HVF: \
                     Hypervisor.framework needs root or a signed hypervisor entitlement — \
                     bench/macos-libkrun/NOTES.md)",
                    path.display()
                )));
            }
            macro_rules! sym {
                ($name:literal) => {{
                    let cname = CString::new($name).unwrap();
                    // SAFETY (provided by the enclosing unsafe block at
                    // every call site): handle is valid (non-null, never
                    // closed); the transmute target at the call site
                    // states the exact signature from include/libkrun.h
                    // v1.19.7.
                    let p = dlsym(handle, cname.as_ptr());
                    if p.is_null() {
                        return Err(BackendError::Unavailable(format!(
                            "libkrun symbol '{}' is missing (expected {}): wrong or too-old \
                             library at {}",
                            $name,
                            super::EXPECTED_LIBKRUN,
                            path.display()
                        )));
                    }
                    p
                }};
            }
            // One transmute per exact signature — verbatim from the header.
            // SAFETY: each symbol exists (checked above) and its Rust
            // signature mirrors the C prototype exactly.
            Ok(Libkrun {
                _handle: handle,
                krun_set_log_level: unsafe {
                    std::mem::transmute::<*mut c_void, unsafe extern "C" fn(u32) -> i32>(sym!(
                        "krun_set_log_level"
                    ))
                },
                krun_create_ctx: unsafe {
                    std::mem::transmute::<*mut c_void, unsafe extern "C" fn() -> i32>(sym!(
                        "krun_create_ctx"
                    ))
                },
                krun_free_ctx: unsafe {
                    std::mem::transmute::<*mut c_void, unsafe extern "C" fn(u32) -> i32>(sym!(
                        "krun_free_ctx"
                    ))
                },
                krun_set_vm_config: unsafe {
                    std::mem::transmute::<*mut c_void, unsafe extern "C" fn(u32, u8, u32) -> i32>(
                        sym!("krun_set_vm_config"),
                    )
                },
                krun_set_kernel: unsafe {
                    std::mem::transmute::<
                        *mut c_void,
                        unsafe extern "C" fn(
                            u32,
                            *const c_char,
                            u32,
                            *const c_char,
                            *const c_char,
                        ) -> i32,
                    >(sym!("krun_set_kernel"))
                },
                krun_set_root: unsafe {
                    std::mem::transmute::<
                        *mut c_void,
                        unsafe extern "C" fn(u32, *const c_char) -> i32,
                    >(sym!("krun_set_root"))
                },
                krun_set_workdir: unsafe {
                    std::mem::transmute::<
                        *mut c_void,
                        unsafe extern "C" fn(u32, *const c_char) -> i32,
                    >(sym!("krun_set_workdir"))
                },
                krun_set_exec: unsafe {
                    std::mem::transmute::<
                        *mut c_void,
                        unsafe extern "C" fn(
                            u32,
                            *const c_char,
                            *const *const c_char,
                            *const *const c_char,
                        ) -> i32,
                    >(sym!("krun_set_exec"))
                },
                krun_start_enter: unsafe {
                    std::mem::transmute::<*mut c_void, unsafe extern "C" fn(u32) -> i32>(sym!(
                        "krun_start_enter"
                    ))
                },
            })
        })();
        let _ = LOADED.set(built.map_err(|e| e.to_string()));
        // After the set (ours or a racing thread's), return whatever is
        // now stored: one consistent answer per process.
        LOADED
            .get()
            .expect("LOADED was just set")
            .as_ref()
            .map_err(|e| BackendError::Unavailable(e.clone()))
    }
}

#[cfg(target_os = "macos")]
mod backend {
    //! `ExecBackend` wiring: an honest `probe` and the full `run` plumbing.

    use super::{ffi, Availability, Vm};
    use crate::assets;
    use crate::{BackendCheck, BackendError, BackendReport, ExecBackend, Result, RunRequest};
    use myc_store::Store;
    use std::ffi::{c_char, CString};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    use std::path::Path as StoreRoot;

    impl ExecBackend for Vm {
        fn name(&self) -> &'static str {
            "vm"
        }

        fn probe(&self, _store_root: &StoreRoot) -> BackendReport {
            let (avail, kernel) = self.availability();
            let Availability {
                available: _,
                experimental,
                libkrun_found,
                kernel_present,
            } = avail;
            let lib_path = super::locate_libkrun().unwrap_or_else(|| self.libkrun.clone());
            let checks = vec![
                BackendCheck {
                    name: "experimental guard".into(),
                    ok: experimental,
                    detail: if experimental {
                        "MYCEL_VM_EXPERIMENTAL=1 — you asked for the unvalidated tier".into()
                    } else {
                        "off (the default) — the engine reports itself unavailable".into()
                    },
                    fix: (!experimental).then(|| {
                        "export MYCEL_VM_EXPERIMENTAL=1 if you accept UNVALIDATED behavior \
                         (no Apple hardware in CI; nothing measured)"
                            .into()
                    }),
                },
                BackendCheck {
                    name: "libkrun".into(),
                    ok: libkrun_found,
                    detail: format!(
                        "{} (libkrun {}, HVF build — the same library crun --krun uses)",
                        lib_path.display(),
                        super::EXPECTED_LIBKRUN
                    ),
                    fix: (!libkrun_found).then(|| {
                        "brew tap libkrun/krun (krunkit ships libkrun.dylib), or 'sudo port \
                         install libkrun', or build from source — bench/macos-libkrun/NOTES.md"
                            .into()
                    }),
                },
                BackendCheck {
                    name: "guest kernel".into(),
                    ok: kernel_present,
                    detail: format!(
                        "{} ({}, {} build)",
                        kernel.unwrap_or_else(|| self.kernel.clone()).display(),
                        super::KERNEL_ASSET,
                        std::env::consts::ARCH
                    ),
                    fix: (!kernel_present).then(|| {
                        "auto-downloaded on the first explicit run; or build it: \
                         bench/microvm/build_kernel_vmfs.sh ARCH=<aarch64|x86_64>"
                            .into()
                    }),
                },
                BackendCheck {
                    name: "hardware validation".into(),
                    // Informational — the honesty row: this can NOT be
                    // green until someone boots it on a Mac.
                    ok: true,
                    detail: "UNVALIDATED — no Apple hardware in CI: no boot, no e2e run, no \
                             performance number exists for this tier. Walk the checklist in \
                             bench/macos-libkrun/NOTES.md before trusting it."
                        .into(),
                    fix: Some(
                        "validate on a Mac: MYCEL_VM_EXPERIMENTAL=1 myc doctor, then myc run \
                         --engine hvf-microvm alpine:3.20 -- echo hello"
                            .into(),
                    ),
                },
                BackendCheck {
                    name: "hypervisor entitlement".into(),
                    // Cannot be checked from userspace without loading
                    // libkrun and calling into HVF — informational.
                    ok: true,
                    detail: "Hypervisor.framework needs root OR the binary signed with the \
                             com.apple.vm.hypervisor entitlement — unverified for this \
                             install (a boot failing with HV_DENIED means exactly this)"
                        .into(),
                    fix: Some(
                        "run under sudo, or ad-hoc sign: codesign --force --sign - \
                         --entitlements <entitlements.plist> $(which myc) — NOTES.md has the \
                         exact plist"
                            .into(),
                    ),
                },
            ];
            BackendReport {
                backend: self.name().to_string(),
                available: avail.available,
                version: None,
                checks,
            }
        }

        fn run(&self, store_root: &StoreRoot, request: &RunRequest) -> Result<i32> {
            self.require_available()?;
            let lib = ffi::open(&self.libkrun)?;
            let ram_mib = super::parse_mem_mib(&self.mem).ok_or_else(|| {
                BackendError::Unavailable(format!(
                    "invalid MYCEL_VM_MEM '{}' (expected e.g. 512M or 1G)",
                    self.mem
                ))
            })?;
            let kernel_format = std::env::var("MYCEL_VM_KRUN_KERNEL_FORMAT")
                .ok()
                .and_then(|v| super::parse_kernel_format(&v))
                .unwrap_or_else(super::default_kernel_format);

            // Resolve the environment exactly like the kvm engine does.
            let store = Store::open(store_root)?;
            let id = store.resolve(&request.reference)?;
            let manifest = store.get_manifest(&id)?;
            let manifest_inputs = super::ManifestInputs {
                command: manifest.command(&request.command),
                env: manifest.config.env.clone(),
                workdir: manifest.config.workdir.clone(),
            };

            // Guest kernel: self-heals from the mycel releases.
            let kernel =
                assets::ensure_asset(&self.kernel, super::KERNEL_ASSET, assets::MIN_KERNEL_BYTES)?;

            // Materialize the rootfs the guest will live in. virtiofs,
            // zero copy — libkrun serves the share in-process. The temp
            // dir must outlive the VM: on the success path this function
            // never returns (see krun_start_enter below), so the TempDir
            // destructor only runs on the error paths — which is exactly
            // the cleanup we want there.
            let run_dir = tempfile::Builder::new()
                .prefix("run-vm-")
                .tempdir_in(store.root().join("tmp"))
                .map_err(BackendError::Io)?;
            let rootfs = run_dir.path().to_path_buf();
            myc_run::materialize(&store, &manifest, &rootfs)?;

            let plan = super::plan_run(
                request,
                &manifest_inputs,
                &rootfs,
                &kernel,
                kernel_format,
                self.cpus,
                ram_mib,
            )?;

            // ---- libkrun context: every call checked, freed on error --
            let cstr = |p: &Path| -> Result<CString> {
                CString::new(p.as_os_str().as_bytes())
                    .map_err(|e| BackendError::Unavailable(format!("path not C-compatible: {e}")))
            };
            let krun_err = |what: &str, rc: i32| {
                BackendError::Unavailable(format!(
                    "libkrun {what} returned {rc} ({})",
                    errno_name(rc)
                ))
            };
            let log_level: u32 = if std::env::var("MYCEL_VM_TRACE").ok().as_deref() == Some("1") {
                4
            } else {
                1
            };
            // SAFETY: plain function pointers into the loaded dylib;
            // level is a documented enum value.
            unsafe { (lib.krun_set_log_level)(log_level) };
            // SAFETY: first libkrun call creating the context.
            let ctx = unsafe { (lib.krun_create_ctx)() };
            if ctx < 0 {
                return Err(BackendError::Unavailable(format!(
                    "libkrun krun_create_ctx returned {ctx}"
                )));
            }
            let ctx = ctx as u32;
            let setup: Result<()> = (|| {
                // SAFETY: ctx exists and has not been consumed; every
                // CString lives until the end of run(), longer than the
                // VM needs them (libkrun copies configuration at set
                // time).
                unsafe {
                    let rc = (lib.krun_set_vm_config)(ctx, plan.vcpus, plan.ram_mib);
                    if rc < 0 {
                        return Err(krun_err("krun_set_vm_config", rc));
                    }
                    let kernel = cstr(&plan.kernel)?;
                    // No initramfs (NULL): libkrun injects its built-in
                    // init into the root share — the documented
                    // direct-kernel-boot shape of the container model.
                    let cmdline = CString::new(plan.cmdline.clone())
                        .map_err(|e| BackendError::Unavailable(format!("cmdline: {e}")))?;
                    let rc = (lib.krun_set_kernel)(
                        ctx,
                        kernel.as_ptr(),
                        plan.kernel_format,
                        std::ptr::null(),
                        cmdline.as_ptr(),
                    );
                    if rc < 0 {
                        return Err(krun_err("krun_set_kernel", rc));
                    }
                    let root = cstr(&plan.rootfs)?;
                    let rc = (lib.krun_set_root)(ctx, root.as_ptr());
                    if rc < 0 {
                        return Err(krun_err("krun_set_root", rc));
                    }
                    if let Some(w) = &plan.workdir {
                        // krun_set_workdir wants the path relative to the
                        // root share.
                        let w = CString::new(w.trim_start_matches('/'))
                            .map_err(|e| BackendError::Unavailable(format!("workdir: {e}")))?;
                        let rc = (lib.krun_set_workdir)(ctx, w.as_ptr());
                        if rc < 0 {
                            return Err(krun_err("krun_set_workdir", rc));
                        }
                    }
                    let exec = CString::new(plan.exec_path.as_str())
                        .map_err(|e| BackendError::Unavailable(format!("exec path: {e}")))?;
                    let argv_c: Vec<CString> = plan
                        .argv
                        .iter()
                        .map(|a| CString::new(a.as_str()))
                        .collect::<std::result::Result<_, _>>()
                        .map_err(|e| BackendError::Unavailable(format!("argv: {e}")))?;
                    let mut argv_p: Vec<*const c_char> =
                        argv_c.iter().map(|s| s.as_ptr()).collect();
                    argv_p.push(std::ptr::null());
                    let envp_c: Vec<CString> = plan
                        .envp
                        .iter()
                        .map(|e| CString::new(e.as_str()))
                        .collect::<std::result::Result<_, _>>()
                        .map_err(|e| BackendError::Unavailable(format!("env: {e}")))?;
                    let mut envp_p: Vec<*const c_char> =
                        envp_c.iter().map(|s| s.as_ptr()).collect();
                    envp_p.push(std::ptr::null());
                    let rc =
                        (lib.krun_set_exec)(ctx, exec.as_ptr(), argv_p.as_ptr(), envp_p.as_ptr());
                    if rc < 0 {
                        return Err(krun_err("krun_set_exec", rc));
                    }
                }
                Ok(())
            })();
            if let Err(e) = setup {
                // SAFETY: the context exists and was never started
                // (krun_start_enter consumes it), so freeing it here is
                // the documented cleanup path.
                unsafe { (lib.krun_free_ctx)(ctx) };
                return Err(e);
            }

            // Enter the guest. THIS CALL DOES NOT RETURN ON SUCCESS:
            // libkrun runs the VM in-process and calls exit() with the
            // workload's exit code once its init powers the VM off — the
            // `myc` process therefore exits with the container's code
            // (like `crun krun`); codes 125/126/127 are libkrun init
            // error codes. Reaching the line below means the VM never
            // started.
            let rc = unsafe { (lib.krun_start_enter)(ctx) };
            Err(BackendError::Unavailable(format!(
                "libkrun krun_start_enter returned {rc} ({}) — the microVM never started; check \
                 the hypervisor entitlement (sudo / codesign) and the kernel format, and see \
                 bench/macos-libkrun/NOTES.md",
                errno_name(rc)
            )))
        }
    }

    /// Best-effort errno names for the negative returns of the krun API
    /// (plain -errno values; only the handful this tier can realistically
    /// hit is named).
    fn errno_name(rc: i32) -> &'static str {
        match -rc {
            1 => "EPERM",
            2 => "ENOENT",
            12 => "ENOMEM",
            22 => "EINVAL",
            38 => "ENOSYS",
            95 => "EOPNOTSUPP",
            _ => "errno",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> RunRequest {
        RunRequest {
            reference: "myc1test".into(),
            ..RunRequest::default()
        }
    }

    fn manifest(cmd: &[&str]) -> ManifestInputs {
        ManifestInputs {
            command: cmd.iter().map(|s| s.to_string()).collect(),
            env: vec!["IMAGE_VAR=1".into()],
            workdir: "/srv".into(),
        }
    }

    // ---- availability -------------------------------------------------

    #[test]
    fn availability_needs_all_three_factors() {
        assert!(decide_availability(true, true, true).available);
        // The experimental guard is the loudest: without it nothing else
        // matters (the tier stays invisible to `myc doctor`).
        assert!(!decide_availability(true, true, false).available);
        assert!(!decide_availability(true, false, true).available);
        assert!(!decide_availability(false, true, true).available);
        assert_eq!(
            decide_availability(false, false, false),
            Availability {
                available: false,
                experimental: false,
                libkrun_found: false,
                kernel_present: false,
            }
        );
    }

    // ---- config parsing -----------------------------------------------

    #[test]
    fn mem_sizes_parse_into_mib() {
        assert_eq!(parse_mem_mib("512M"), Some(512));
        assert_eq!(parse_mem_mib("1G"), Some(1024));
        assert_eq!(parse_mem_mib("2GiB"), Some(2048));
        assert_eq!(parse_mem_mib("768"), Some(768));
        assert_eq!(parse_mem_mib("0M"), None);
        assert_eq!(parse_mem_mib(""), None);
        assert_eq!(parse_mem_mib("banana"), None);
    }

    #[test]
    fn kernel_formats_map_to_header_constants() {
        assert_eq!(parse_kernel_format("raw"), Some(0));
        assert_eq!(parse_kernel_format("ELF"), Some(1));
        assert_eq!(parse_kernel_format("pe_gz"), Some(2));
        assert_eq!(parse_kernel_format("image_zstd"), Some(5));
        assert_eq!(parse_kernel_format("3"), Some(3));
        assert_eq!(parse_kernel_format("nope"), None);
        assert_eq!(default_kernel_format(), 0);
    }

    // ---- guest env + exec resolution ----------------------------------

    #[test]
    fn guest_env_layers_defaults_image_then_request() {
        let env = guest_env_for(
            &["PATH=/custom".into(), "IMAGE=1".into()],
            &["REQ=2".into()],
        );
        // Last assignment wins: the request layer can override PATH.
        assert_eq!(
            env,
            vec![
                "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
                "HOME=/root".to_string(),
                "PATH=/custom".to_string(),
                "IMAGE=1".to_string(),
                "REQ=2".to_string(),
            ]
        );
    }

    #[test]
    fn exec_resolution_searches_the_guest_path_in_the_rootfs() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("usr/bin")).unwrap();
        std::fs::write(root.path().join("usr/bin/echo"), b"#!/bin/sh\n").unwrap();
        let path = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
        // Bare name → found in a PATH dir, returned guest-absolute.
        assert_eq!(
            resolve_exec(root.path(), "echo", path),
            Some("/usr/bin/echo".to_string())
        );
        // Bare name not in any PATH dir → None (plan_run refuses with a
        // clear error; nothing is silently executed elsewhere).
        assert_eq!(resolve_exec(root.path(), "nope", path), None);
        // Explicit absolute path → existence-checked only.
        assert_eq!(
            resolve_exec(root.path(), "/usr/bin/echo", path),
            Some("/usr/bin/echo".to_string())
        );
        assert_eq!(resolve_exec(root.path(), "/usr/bin/absent", path), None);
        // Relative with a slash → treated as guest-absolute.
        assert_eq!(
            resolve_exec(root.path(), "usr/bin/echo", path),
            Some("/usr/bin/echo".to_string())
        );
    }

    // ---- the planner ----------------------------------------------------

    #[test]
    fn plan_maps_a_full_request_onto_the_krun_calls() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("usr/bin")).unwrap();
        std::fs::write(root.path().join("usr/bin/env"), b"#!/bin/sh\n").unwrap();
        let req = RunRequest {
            command: vec!["env".into()],
            env: vec!["FOO=bar".into()],
            hostname: "ignored".into(),
            workdir: Some("/work".into()),
            ..request()
        };
        let man = manifest(&["/usr/bin/sleep", "300"]);
        let kernel = Path::new("/kern/vmlinux-vmfs-aarch64");
        let plan = plan_run(&req, &man, root.path(), kernel, 0, 1, 512).unwrap();
        // The `--` override wins over the manifest command, and argv[0]
        // keeps the user's spelling (execv conventions).
        assert_eq!(plan.exec_path, "/usr/bin/env");
        assert_eq!(plan.argv, vec!["env".to_string()]);
        // Defaults, image ENV, then -e.
        assert!(plan.envp.contains(&"IMAGE_VAR=1".to_string()));
        assert!(plan.envp.contains(&"FOO=bar".to_string()));
        assert!(plan.envp[0].starts_with("PATH="));
        assert_eq!(plan.workdir.as_deref(), Some("/work"));
        assert_eq!(plan.cmdline, "quiet loglevel=3");
        assert_eq!(plan.kernel, kernel);
        assert_eq!(plan.kernel_format, 0);
        assert_eq!(plan.vcpus, 1);
        assert_eq!(plan.ram_mib, 512);
    }

    #[test]
    fn plan_uses_the_manifest_command_and_workdir_when_no_override() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("bin")).unwrap();
        std::fs::write(root.path().join("bin/sh"), b"#!/bin/sh\n").unwrap();
        let plan = plan_run(
            &request(),
            &manifest(&["/bin/sh", "-c", "echo hi"]),
            root.path(),
            Path::new("/k"),
            0,
            1,
            256,
        )
        .unwrap();
        assert_eq!(plan.exec_path, "/bin/sh");
        assert_eq!(plan.argv.len(), 3);
        assert_eq!(plan.workdir.as_deref(), Some("/srv"));
    }

    #[test]
    fn plan_refuses_what_the_libkrun_model_cannot_carry() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("bin")).unwrap();
        std::fs::write(root.path().join("bin/x"), b"#!/bin/sh\n").unwrap();
        // Bind mounts: no guest-mount API for extra shares in v1.
        let with_bind = RunRequest {
            binds: vec!["./data:/data:ro".into()],
            ..request()
        };
        let err = plan_run(
            &with_bind,
            &manifest(&["/bin/x"]),
            root.path(),
            Path::new("/k"),
            0,
            1,
            512,
        )
        .unwrap_err();
        assert!(err.to_string().contains("bind mounts"), "{err}");
        // Published ports: network-less tier.
        let with_ports = RunRequest {
            publish: vec![(8080, 80)],
            ..request()
        };
        let err = plan_run(
            &with_ports,
            &manifest(&["/bin/x"]),
            root.path(),
            Path::new("/k"),
            0,
            1,
            512,
        )
        .unwrap_err();
        assert!(err.to_string().contains("publishing ports"), "{err}");
        // No command anywhere: same error shape as the kvm engine.
        let err = plan_run(
            &request(),
            &manifest(&[]),
            root.path(),
            Path::new("/k"),
            0,
            1,
            512,
        )
        .unwrap_err();
        assert!(err.to_string().contains("no command"), "{err}");
        // Missing executable: refused with the guest PATH context.
        let err = plan_run(
            &request(),
            &manifest(&["absent"]),
            root.path(),
            Path::new("/k"),
            0,
            1,
            512,
        )
        .unwrap_err();
        assert!(err.to_string().contains("'absent' not found"), "{err}");
        // Relative workdir: libkrun wants a root-relative guest path.
        let bad_wd = RunRequest {
            workdir: Some("rel".into()),
            ..request()
        };
        let err = plan_run(
            &bad_wd,
            &manifest(&["/bin/x"]),
            root.path(),
            Path::new("/k"),
            0,
            1,
            512,
        )
        .unwrap_err();
        assert!(err.to_string().contains("absolute guest path"), "{err}");
    }
}
