//! Execution backends: where containers actually run.
//!
//! Mycel containers are Linux processes. On Linux they run natively
//! (user namespaces, see `myc-run`). On Windows, `myc.exe` transparently
//! proxies every command to a "twin" Linux binary inside WSL2 — one store,
//! one feature set, no daemon (see [`wsl`]). On macOS a lightweight-VM
//! backend is planned but not yet available (see [`vm`] and
//! `docs/desktop-architecture.md`).
//!
//! The [`ExecBackend`] trait is the seam between the CLI and those
//! strategies: `probe()` reports whether the backend can work here (with an
//! actionable fix when it cannot), `run()` executes one container.

#[cfg(target_os = "linux")]
pub mod kvm;
#[cfg(target_os = "linux")]
pub mod native;
/// Userland TCP relay for published ports (used by [`kvm`]; generic).
#[cfg(target_os = "linux")]
pub mod portproxy;
#[cfg(target_os = "macos")]
pub mod vm;
/// Windows WSL2 proxy backend. Also compiled under `cfg(test)` on every
/// platform so its pure detection/state logic (parsing of wsl.exe output,
/// setup-marker state machine) is unit-tested on Linux too.
#[cfg(any(windows, test))]
pub mod wsl;

/// Isolation tiers, strongest first. Mycel never depends on a single one:
/// `detect_engines()` reports what this machine can do and
/// [`default_backend`] picks the best *complete* engine (see
/// `docs/isolation-engines.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineKind {
    /// Real virtual machine, own kernel (Linux + KVM). Strongest isolation;
    /// carries the full `RunRequest` surface (see [`kvm`]).
    KvmMicroVm,
    /// Real virtual machine via Hypervisor.framework (macOS plan,
    /// `docs/desktop-architecture.md`).
    HypervisorFramework,
    /// Transparent proxy to the Linux twin inside WSL2 (Windows).
    Wsl2,
    /// User namespaces: shared kernel, Docker-level isolation. The reliable
    /// default everywhere Mycel runs today.
    NativeNamespaces,
}

impl EngineKind {
    /// Stable identifier used in `myc doctor` output and reports.
    pub fn label(self) -> &'static str {
        match self {
            Self::KvmMicroVm => "kvm-microvm",
            Self::HypervisorFramework => "hvf-microvm",
            Self::Wsl2 => "wsl2-twin",
            Self::NativeNamespaces => "native-namespaces",
        }
    }

    /// True when containers get their own kernel (a real VM).
    pub fn is_micro_vm(self) -> bool {
        matches!(self, Self::KvmMicroVm | Self::HypervisorFramework)
    }

    /// What this tier does *not* give you, shown next to the engine.
    pub fn degraded_features(self) -> &'static [&'static str] {
        match self {
            Self::KvmMicroVm => &[
                "published ports (-p) and the br-myc bridge need a root myc",
                "networked VMs share one /24 bridge (253 guests, MYCEL_VM_CIDR)",
                "--map-user ignored in v1 (workloads run as guest root)",
                "x86_64 guests only (multi-arch not wired)",
            ],
            Self::HypervisorFramework => &["architecture only (see docs/desktop-architecture.md)"],
            Self::Wsl2 => &["containers execute inside the WSL2 Linux twin"],
            Self::NativeNamespaces => &[
                "shared kernel with the host — isolation is Docker-level, not better",
                "no whole-memory snapshot/restore",
            ],
        }
    }
}

/// One detected tier: what it is, whether it works here, whether it can run
/// every environment (complete) or only part of them yet.
#[derive(Debug, Clone)]
pub struct EngineInfo {
    pub kind: EngineKind,
    pub available: bool,
    /// True when the tier can run arbitrary environments end to end.
    pub complete: bool,
}

/// Every isolation tier this machine could use, strongest first.
///
/// The engine choice is automatic — users never configure it; `myc doctor`
/// prints this list so the choice is never a mystery. The default backend is
/// the strongest *complete* tier: the microVM tiers are deliberately not
/// default until they carry the full `RunRequest` surface.
pub fn detect_engines(_store_root: &std::path::Path) -> Vec<EngineInfo> {
    let mut engines = Vec::new();
    #[cfg(target_os = "linux")]
    {
        let kvm = kvm::KvmMicroVm::from_env();
        let report = kvm.probe(_store_root);
        // complete = the full production path (voie B virtiofs): arbitrary
        // environments end to end. ext4-only hosts stay PARTIAL.
        engines.push(EngineInfo {
            kind: EngineKind::KvmMicroVm,
            available: report.available,
            complete: report.available && kvm.engine_ready(),
        });
    }
    #[cfg(target_os = "macos")]
    {
        engines.push(EngineInfo {
            kind: EngineKind::HypervisorFramework,
            available: false,
            complete: false,
        });
    }
    #[cfg(windows)]
    {
        engines.push(EngineInfo {
            kind: EngineKind::Wsl2,
            available: true,
            complete: true,
        });
    }
    #[cfg(target_os = "linux")]
    {
        let native = native::NativeLinux.probe(_store_root);
        engines.push(EngineInfo {
            kind: EngineKind::NativeNamespaces,
            available: native.available,
            complete: true,
        });
    }
    engines
}

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("{0}")]
    Unavailable(String),
    #[error(transparent)]
    Run(#[from] myc_run::RunError),
    #[error(transparent)]
    Store(#[from] myc_store::StoreError),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Provision(String),
}

pub type Result<T> = std::result::Result<T, BackendError>;

/// One probe finding: `ok` plus a human detail line, and a fix when broken.
#[derive(Debug, Clone)]
pub struct BackendCheck {
    pub name: String,
    pub ok: bool,
    pub detail: String,
    pub fix: Option<String>,
}

/// Everything `probe()` learned about a backend on this machine.
#[derive(Debug, Clone)]
pub struct BackendReport {
    /// Backend identifier: `native-linux`, `wsl2`, `vm`.
    pub backend: String,
    /// True when `run()` is expected to work right now.
    pub available: bool,
    /// Version of the runtime that will execute containers (twin version on
    /// Windows, our own on Linux).
    pub version: Option<String>,
    pub checks: Vec<BackendCheck>,
}

/// A single container run, expressed portably (no manifest/store types so
/// proxy backends can forward it without resolving anything host-side).
#[derive(Debug, Clone, Default)]
pub struct RunRequest {
    /// Manifest id, ref name or image reference.
    pub reference: String,
    /// Command override (empty = image entrypoint/cmd).
    pub command: Vec<String>,
    /// KEY=VALUE environment variables.
    pub env: Vec<String>,
    /// `HOST:CONTAINER[:ro]` bind specs (host paths in host syntax).
    pub binds: Vec<String>,
    /// Container hostname (empty = backend default).
    pub hostname: String,
    pub workdir: Option<String>,
    /// `UID[:GID]` to map the host user to inside the container.
    pub map_user: Option<String>,
    pub isolated_network: bool,
    pub keep_rootfs: bool,
    /// Published ports `(host, container)`. Empty + non-isolated network
    /// means "publish the manifest's exposed ports" for VM backends (a VM
    /// cannot literally share the host network namespace).
    pub publish: Vec<(u16, u16)>,
}

/// A strategy for executing containers on this machine.
pub trait ExecBackend {
    /// Short identifier (`native-linux`, `wsl2`, `vm`).
    fn name(&self) -> &'static str;
    /// Can this backend run containers here? Never panics; broken setups
    /// yield `available: false` plus actionable fixes.
    fn probe(&self, store_root: &std::path::Path) -> BackendReport;
    /// Run one container to completion; returns its exit code.
    fn run(&self, store_root: &std::path::Path, request: &RunRequest) -> Result<i32>;
}

/// The backend for the current platform.
///
/// - Linux: [`native::NativeLinux`] (user namespaces, exactly `myc run`).
/// - Windows: [`wsl::Wsl2`] (transparent proxy to the WSL2 twin).
/// - macOS: [`vm::Vm`] (scaffold; returns "not yet available").
pub fn default_backend() -> Box<dyn ExecBackend> {
    #[cfg(target_os = "linux")]
    {
        Box::new(native::NativeLinux)
    }
    #[cfg(windows)]
    {
        Box::new(wsl::Wsl2::from_env())
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(vm::Vm)
    }
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    {
        compile_error!("no execution backend for this platform");
    }
}

/// Parse a `RunRequest` bind spec against `cwd`, returning
/// `(host, container, ro)` — shared by native backends.
#[cfg(target_os = "linux")]
pub(crate) fn split_bind(
    spec: &str,
    cwd: &std::path::Path,
) -> Result<(std::path::PathBuf, String, bool)> {
    let (rest, ro) = match spec.strip_suffix(":ro") {
        Some(r) => (r, true),
        None => (spec, false),
    };
    let (host, container) = rest
        .split_once(':')
        .ok_or_else(|| BackendError::Unavailable(format!("invalid bind spec '{spec}'")))?;
    let host_path = if std::path::Path::new(host).is_absolute() {
        std::path::PathBuf::from(host)
    } else {
        cwd.join(host)
    };
    Ok((host_path, container.to_string(), ro))
}
