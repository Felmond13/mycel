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
pub mod native;
#[cfg(target_os = "macos")]
pub mod vm;
#[cfg(windows)]
pub mod wsl;

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
