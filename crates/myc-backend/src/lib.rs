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

/// Firecracker snapshot accelerator for the kvm-microvm engine (used by
/// [`kvm`]; reached through kvm's run path).
#[cfg(target_os = "linux")]
pub(crate) mod firecracker;
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
/// prints this list so the choice is never a mystery. The automatic choice
/// ([`decide_auto_engine`]) is the strongest *complete* tier that can carry
/// the exact request: kvm-microvm when READY, native namespaces otherwise
/// (see `docs/isolation-engines.md`).
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

// ======================================================================
// Automatic engine selection (the "auto" policy)
// ======================================================================
//
// `myc run` (no --engine, or --engine auto) never asks the user to pick an
// isolation tier: it takes the strongest *complete* engine that can carry
// the exact request — kvm-microvm when READY, native namespaces otherwise.
// The policy lives in ONE pure function shared by every entry point (CLI,
// web dashboard), so a container started from the terminal and one started
// from the browser get the same treatment.

/// What one run request needs from its engine — the inputs of the automatic
/// choice ([`decide_auto_engine`]). Everything the kvm-microvm tier cannot
/// carry today is a boolean here; add new incompatibilities as fields, never
/// as scattered `if`s at call sites.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AutoEngineInputs {
    /// The request joins a pod-holder's namespaces (`--join-net`): pods are
    /// namespaces by construction, a VM cannot join them.
    pub join_net: bool,
    /// The caller manages the rootfs directory itself (`--run-dir`, the web
    /// UI's lifecycle contract) — a VM rootfs is not a host directory.
    pub run_dir: bool,
    /// The run is interactive (no command override + a terminal on stdin):
    /// the VM console is output-only, so shells stay native. `myc shell`
    /// forces native for the same reason.
    pub interactive: bool,
    /// The request asked for pasta user-namespace networking (`--net
    /// isolated`): that isolation IS the namespaces feature; the VM tier
    /// emulates published ports instead (no userland egress).
    pub net_isolated: bool,
    /// The run publishes ports (`-p` or image-exposed ports, with network):
    /// the VM bridge/tap/DNAT path needs a root myc.
    pub publishes_ports: bool,
    /// Is this myc process running as root?
    pub is_root: bool,
}

/// The automatic engine decision for one run (see `docs/isolation-engines.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineDecision {
    /// kvm-microvm: strongest tier — real VM, own kernel.
    KvmMicroVm,
    /// native-namespaces. `why` is the one-line human reason printed on
    /// stderr ("engine: native-namespaces (auto; …)"); `None` means quiet
    /// fallback — kvm is simply not complete on this machine and
    /// `myc doctor` already explains why.
    NativeNamespaces { why: Option<&'static str> },
}

/// The automatic engine policy, strongest tier first:
///
/// 1. request incompatibilities (pods, caller-managed dirs) → namespaces,
///    whatever the kvm state — the reason is actionable;
/// 2. kvm incomplete → namespaces, quietly (doctor explains);
/// 3. kvm-specific capability guards (interactivity, pasta isolation,
///    rootless port publishing) → namespaces with the reason;
/// 4. otherwise kvm-microvm.
pub fn decide_auto_engine(kvm_complete: bool, inputs: AutoEngineInputs) -> EngineDecision {
    if inputs.join_net {
        return EngineDecision::NativeNamespaces {
            why: Some("--join-net requires namespaces (pods)"),
        };
    }
    if inputs.run_dir {
        return EngineDecision::NativeNamespaces {
            why: Some("--run-dir requires namespaces (caller-managed rootfs)"),
        };
    }
    if !kvm_complete {
        return EngineDecision::NativeNamespaces { why: None };
    }
    if inputs.interactive {
        return EngineDecision::NativeNamespaces {
            why: Some("interactive runs stay on namespaces (the VM console is output-only)"),
        };
    }
    if inputs.net_isolated {
        return EngineDecision::NativeNamespaces {
            why: Some("--net isolated is pasta-backed (namespaces)"),
        };
    }
    if inputs.publishes_ports && !inputs.is_root {
        return EngineDecision::NativeNamespaces {
            why: Some("publishing ports on kvm-microvm needs a root myc (bridge, tap, DNAT)"),
        };
    }
    EngineDecision::KvmMicroVm
}

/// True when the kvm-microvm engine is COMPLETE on this machine — the READY
/// line of `myc doctor`: /dev/kvm usable + engine binaries + guest assets +
/// virtiofsd (the full production path, voie B). The probe is pure file
/// checking (measured < 1 ms, no subprocess); cached per process anyway
/// because myc-web asks before every container start.
pub fn kvm_engine_complete() -> bool {
    #[cfg(target_os = "linux")]
    {
        static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *CACHE.get_or_init(|| kvm::KvmMicroVm::from_env().engine_ready())
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Is this process running as root? (the VM network path — bridge, taps,
/// iptables — needs it, exactly like the Docker daemon).
pub fn is_root() -> bool {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("Uid:"))
                    .and_then(|l| l.split_whitespace().nth(1).map(|u| u == "0"))
            })
            == Some(true)
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
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
    /// Skip the Firecracker snapshot cache (`--no-snapshot-cache`): every
    /// kvm-microvm run cold-boots through Cloud Hypervisor. Ignored by
    /// non-VM backends.
    pub no_snapshot_cache: bool,
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

#[cfg(test)]
mod tests {
    use super::{decide_auto_engine, AutoEngineInputs, EngineDecision};

    fn inputs() -> AutoEngineInputs {
        AutoEngineInputs::default()
    }

    #[test]
    fn kvm_ready_plus_simple_request_picks_the_microvm() {
        assert_eq!(
            decide_auto_engine(true, inputs()),
            EngineDecision::KvmMicroVm
        );
        // Ports to publish are fine when myc runs as root (bridge + DNAT).
        let with_ports = AutoEngineInputs {
            publishes_ports: true,
            is_root: true,
            ..inputs()
        };
        assert_eq!(
            decide_auto_engine(true, with_ports),
            EngineDecision::KvmMicroVm
        );
    }

    #[test]
    fn kvm_incomplete_falls_back_to_native_quietly() {
        // Quiet: `myc doctor` already explains what is missing — the run
        // output stays identical to the pre-microvm era.
        assert_eq!(
            decide_auto_engine(false, inputs()),
            EngineDecision::NativeNamespaces { why: None }
        );
    }

    #[test]
    fn pods_and_caller_dirs_stay_native_even_when_kvm_is_ready() {
        let join_net = AutoEngineInputs {
            join_net: true,
            ..inputs()
        };
        assert_eq!(
            decide_auto_engine(true, join_net),
            EngineDecision::NativeNamespaces {
                why: Some("--join-net requires namespaces (pods)")
            }
        );
        // The incompatibility reason wins over "kvm incomplete" — it is
        // the actionable one.
        assert_eq!(
            decide_auto_engine(false, join_net),
            EngineDecision::NativeNamespaces {
                why: Some("--join-net requires namespaces (pods)")
            }
        );
        let run_dir = AutoEngineInputs {
            run_dir: true,
            ..inputs()
        };
        assert!(matches!(
            decide_auto_engine(true, run_dir),
            EngineDecision::NativeNamespaces { why: Some(_) }
        ));
    }

    #[test]
    fn interactive_and_pasta_isolated_runs_stay_native() {
        let interactive = AutoEngineInputs {
            interactive: true,
            ..inputs()
        };
        assert!(matches!(
            decide_auto_engine(true, interactive),
            EngineDecision::NativeNamespaces { why: Some(w) } if w.contains("interactive")
        ));
        let isolated = AutoEngineInputs {
            net_isolated: true,
            ..inputs()
        };
        assert!(matches!(
            decide_auto_engine(true, isolated),
            EngineDecision::NativeNamespaces { why: Some(w) } if w.contains("--net isolated")
        ));
        // These guards only matter when kvm would otherwise win: an
        // incomplete kvm machine stays quiet about them.
        assert_eq!(
            decide_auto_engine(false, interactive),
            EngineDecision::NativeNamespaces { why: None }
        );
    }

    #[test]
    fn rootless_port_publishing_falls_back_to_native() {
        let rootless = AutoEngineInputs {
            publishes_ports: true,
            is_root: false,
            ..inputs()
        };
        assert!(matches!(
            decide_auto_engine(true, rootless),
            EngineDecision::NativeNamespaces { why: Some(w) } if w.contains("root")
        ));
        // The same request as root takes the microVM (bridge + tap + DNAT).
        let root = AutoEngineInputs {
            publishes_ports: true,
            is_root: true,
            ..inputs()
        };
        assert_eq!(decide_auto_engine(true, root), EngineDecision::KvmMicroVm);
        // Without ports, rootless runs still take the microVM (no network
        // needed): `myc run alpine -- echo hi` works unprivileged.
        let rootless_offline = AutoEngineInputs {
            publishes_ports: false,
            is_root: false,
            ..inputs()
        };
        assert_eq!(
            decide_auto_engine(true, rootless_offline),
            EngineDecision::KvmMicroVm
        );
    }
}
