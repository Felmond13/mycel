//! Rootfs materialization and container execution.
//!
//! Materialization builds a rootfs directory from a manifest by hardlinking
//! file contents out of the content-addressed store — no data is copied, so
//! ten environments sharing 90% of their files share 90% of their disk.
//!
//! Execution (Linux only) uses unprivileged user namespaces: mount, PID, UTS
//! and IPC namespaces, pivot_root, /proc + /dev setup. No daemon, no root.

#[cfg(target_os = "linux")]
mod linux;
mod materialize;

pub use materialize::{materialize, MaterializeStats};

#[cfg(target_os = "linux")]
pub use linux::remove_rootfs;

use myc_manifest::Manifest;
use myc_store::Store;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Store(#[from] myc_store::StoreError),
    #[error("i/o error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{count} blobs referenced by the manifest are missing from the store (run `myc pull` first)")]
    IncompleteStore { count: usize },
    #[error("manifest has no command to execute; pass one explicitly")]
    NoCommand,
    #[error("container execution is only supported on Linux (on Windows, run myc inside WSL2)")]
    UnsupportedPlatform,
    #[error("container setup failed: {0}")]
    Setup(String),
}

pub type Result<T> = std::result::Result<T, RunError>;

/// One published port: connections to `host` on the machine reach
/// `container` inside the isolated network namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortMap {
    pub host: u16,
    pub container: u16,
}

/// How the container sees the network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Network {
    /// Share the host network namespace (the rootless default: no
    /// privileges needed, services are reachable on their own ports).
    Host,
    /// Own network namespace with no connectivity at all (loopback only).
    Loopback,
    /// Own network namespace with user-mode connectivity provided by
    /// pasta (package `passt`, the rootless tool Podman uses): outbound
    /// traffic works, and each [`PortMap`] opens one host port into the
    /// container. An empty list publishes the manifest's exposed ports
    /// on the same host ports.
    Isolated { publish: Vec<PortMap> },
}

/// The message shown when isolation is requested but pasta is missing.
pub const NETWORK_ISOLATION_HINT: &str =
    "Network isolation needs the 'passt' package — run: sudo apt install passt";

/// Locate the `pasta` binary (network isolation backend). Searches $PATH
/// plus the usual system directories and `~/.local/bin`.
pub fn pasta_path() -> Option<PathBuf> {
    let name = "pasta";
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let mut dirs: Vec<PathBuf> = ["/usr/bin", "/usr/local/bin", "/bin", "/usr/sbin"]
        .iter()
        .map(PathBuf::from)
        .collect();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".local/bin"));
    }
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file())
}

/// Options for a single container run.
pub struct RunOptions {
    /// Override command (empty = use manifest entrypoint/cmd).
    pub command: Vec<String>,
    /// Extra environment variables (KEY=VALUE).
    pub env: Vec<String>,
    /// Container hostname.
    pub hostname: String,
    /// Bind mounts: (host path, container path, read-only).
    pub binds: Vec<(PathBuf, String, bool)>,
    /// Working directory override.
    pub workdir: Option<String>,
    /// Keep the materialized rootfs after exit (for debugging).
    pub keep_rootfs: bool,
    /// Network mode (default: share the host network namespace).
    pub network: Network,
    /// Container uid/gid the host user is mapped to (default 0 = root).
    /// Mapping to a service uid (e.g. 999 for `postgres`) makes official
    /// images skip their root-only chown/su-exec entrypoint paths, which
    /// cannot work in a single-uid user namespace.
    pub map_uid: u32,
    pub map_gid: u32,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions {
            command: Vec::new(),
            env: Vec::new(),
            hostname: "mycel".to_string(),
            binds: Vec::new(),
            workdir: None,
            keep_rootfs: false,
            network: Network::Host,
            map_uid: 0,
            map_gid: 0,
        }
    }
}

/// Probe whether unprivileged user namespaces work on this machine.
///
/// Forks a throwaway child that calls `unshare(CLONE_NEWUSER)` and reports
/// the result — the same operation `run` performs for real, so a passing
/// probe means containers will start.
#[cfg(target_os = "linux")]
pub fn probe_userns() -> std::result::Result<(), String> {
    // Debian/Ubuntu-specific kernel gate: absent on most distros, must be 1.
    if let Ok(v) = std::fs::read_to_string("/proc/sys/kernel/unprivileged_userns_clone") {
        if v.trim() == "0" {
            return Err("kernel.unprivileged_userns_clone is 0".to_string());
        }
    }
    use nix::sys::wait::{waitpid, WaitStatus};
    use nix::unistd::{fork, ForkResult};
    match unsafe { fork() } {
        Ok(ForkResult::Child) => {
            // Only async-signal-safe operations between fork and _exit.
            let ok = nix::sched::unshare(nix::sched::CloneFlags::CLONE_NEWUSER).is_ok();
            unsafe { libc::_exit(if ok { 0 } else { 1 }) }
        }
        Ok(ForkResult::Parent { child }) => match waitpid(child, None) {
            Ok(WaitStatus::Exited(_, 0)) => Ok(()),
            _ => Err("unshare(CLONE_NEWUSER) was denied by the kernel".to_string()),
        },
        Err(e) => Err(format!("fork failed: {e}")),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn probe_userns() -> std::result::Result<(), String> {
    Err("user namespaces require Linux".to_string())
}

/// Materialize `manifest` into a fresh rootfs under `work_dir` and execute it.
/// Returns the child's exit code.
pub fn run(
    store: &Store,
    manifest: &Manifest,
    work_dir: &std::path::Path,
    options: &RunOptions,
) -> Result<i32> {
    let missing = store.missing_blobs(manifest);
    if !missing.is_empty() {
        return Err(RunError::IncompleteStore {
            count: missing.len(),
        });
    }
    let command = manifest.command(&options.command);
    if command.is_empty() {
        return Err(RunError::NoCommand);
    }

    #[cfg(target_os = "linux")]
    {
        linux::run_linux(store, manifest, work_dir, options, &command)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (work_dir, command);
        Err(RunError::UnsupportedPlatform)
    }
}

/// Execute a command in a rootfs that was already built by the caller
/// (e.g. a lazy rootfs containing a live FUSE mount). The caller owns the
/// rootfs lifecycle: nothing is materialized and nothing is deleted here.
pub fn exec_prepared(
    manifest: &Manifest,
    rootfs: &std::path::Path,
    options: &RunOptions,
) -> Result<i32> {
    let command = manifest.command(&options.command);
    if command.is_empty() {
        return Err(RunError::NoCommand);
    }
    #[cfg(target_os = "linux")]
    {
        linux::exec_rootfs(manifest, rootfs, options, &command)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (rootfs, command);
        Err(RunError::UnsupportedPlatform)
    }
}
