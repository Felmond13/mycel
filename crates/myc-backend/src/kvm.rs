//! KVM microVM backend — Cloud Hypervisor driven as a child process.
//!
//! This is the strongest isolation tier on Linux: the container workload
//! runs inside a real virtual machine with its own kernel, so a container
//! escape never shares a kernel with the host (unlike [`crate::native`],
//! which is shared-kernel, Docker-level isolation).
//!
//! Honest scope (prototype stage, `bench/microvm`): the launcher boots a
//! pinned minimal Linux kernel (virtio built-in) with an initramfs and a
//! host-side tap, runs the workload as PID 1 in the guest, and tears
//! everything down cleanly. Arbitrary `RunRequest`s (binds, volumes,
//! published ports, arbitrary images) are NOT wired yet — `run()` below
//! refuses them explicitly instead of pretending.
//!
//! Snapshot/restore status: Cloud Hypervisor v54.0 snapshots fine, but the
//! restored guest stays inert under WSL2's KVM (MSR state and interrupt
//! injection are not preserved across save/restore by that hypervisor
//! backend; evidence archived in the Capsule M0 investigation, 2026-11-03).
//! On native KVM this is expected to work — re-validate there before
//! building on it.

use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::{BackendCheck, BackendError, BackendReport, ExecBackend, Result, RunRequest};
use std::path::Path as StoreRoot;

/// Where binaries and the kernel are expected (overridable via env).
pub const DEFAULT_CH_BIN: &str = "/usr/local/bin/cloud-hypervisor";
pub const DEFAULT_CH_REMOTE: &str = "/usr/local/bin/ch-remote";
pub const DEFAULT_KERNEL: &str = "/usr/local/share/mycel/vmlinux";

/// Guest-side subnet used by this backend: host tap `.1`, guest `.2`.
pub const GUEST_IP: [u8; 4] = [192, 168, 77, 2];
/// Port the bench workload listens on inside the guest.
pub const GUEST_PORT: u16 = 8080;
/// Host-side tap address.
pub const HOST_IP: &str = "192.168.77.1";

const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// The KVM microVM engine: paths resolved from env or defaults.
#[derive(Debug, Clone)]
pub struct KvmMicroVm {
    /// `cloud-hypervisor` binary (v54.0 pinned by the build docs).
    pub ch_bin: PathBuf,
    /// `ch-remote` binary.
    pub ch_remote: PathBuf,
    /// Guest kernel (bzImage, virtio built in — see `bench/microvm`).
    pub kernel: PathBuf,
}

impl KvmMicroVm {
    /// Resolve engine paths from `MYCEL_CH_BIN`, `MYCEL_CH_REMOTE`,
    /// `MYCEL_KERNEL` or the documented defaults.
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
        }
    }

    fn file_exists(p: &Path) -> bool {
        p.is_file()
    }

    /// `/dev/kvm` opened read-write proves both presence and permission.
    fn kvm_usable() -> bool {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .is_ok()
    }

    /// Boot a microVM running `initramfs` and wait until the guest accepts
    /// TCP connections on `GUEST_IP:GUEST_PORT` (the workload's readiness
    /// signal). Returns the running VM handle.
    pub fn launch(&self, initramfs: &Path) -> Result<RunningVm> {
        if !Self::kvm_usable() {
            return Err(BackendError::Unavailable(
                "/dev/kvm is not usable (missing, or no permission — try the kvm group)".into(),
            ));
        }
        for (path, role) in [(&self.ch_bin, "cloud-hypervisor"), (&self.kernel, "kernel")] {
            if !Self::file_exists(path) {
                return Err(BackendError::Unavailable(format!(
                    "{role} not found at {} (set MYCEL_{role}?)",
                    path.display()
                )));
            }
        }
        if !initramfs.is_file() {
            return Err(BackendError::Unavailable(format!(
                "initramfs not found: {}",
                initramfs.display()
            )));
        }

        let state_dir = std::env::temp_dir().join(format!("myc-microvm-{}", std::process::id()));
        std::fs::create_dir_all(&state_dir)?;
        let api_socket = state_dir.join("api.sock");
        let log = state_dir.join("ch.log");

        // A leftover tap holding HOST_IP silently breaks routing (Linux then
        // prefers it over the fresh one, and the guest never sees traffic —
        // diagnosed 2026-11-03). Delete any straggler before we start.
        let _ = Command::new("ip")
            .args(["link", "del", "myc0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        let net = format!("tap=myc0,mac=52:54:00:77:00:02,ip={HOST_IP},mask=255.255.255.0");
        let mut child = Command::new(&self.ch_bin)
            .arg("--api-socket")
            .arg(&api_socket)
            .arg("--console")
            .arg(format!("file={}", state_dir.join("console.log").display()))
            .arg("--kernel")
            .arg(&self.kernel)
            .arg("--cmdline")
            .arg("console=hvc0 quiet")
            .arg("--initramfs")
            .arg(initramfs)
            .arg("--cpus")
            .arg("boot=1")
            .arg("--memory")
            .arg("size=512M")
            .arg("--net")
            .arg(net)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(&log)?))
            .spawn()
            .map_err(|e| BackendError::Unavailable(format!("spawn cloud-hypervisor: {e}")))?;

        // Fail fast with the actual VMM log if it dies immediately.
        thread::sleep(Duration::from_millis(30));
        match child.try_wait() {
            Ok(None) => {}
            Ok(Some(status)) => {
                let log_content = std::fs::read_to_string(&log).unwrap_or_default();
                let _ = std::fs::remove_dir_all(&state_dir);
                return Err(BackendError::Unavailable(format!(
                    "cloud-hypervisor died at startup ({status}): {}",
                    log_content.trim()
                )));
            }
            Err(e) => return Err(BackendError::Io(e)),
        }

        Ok(RunningVm {
            child,
            ch_remote: self.ch_remote.clone(),
            api_socket,
            state_dir,
            stopped: false,
        })
    }
}

impl ExecBackend for KvmMicroVm {
    fn name(&self) -> &'static str {
        "kvm-microvm"
    }

    fn probe(&self, _store_root: &StoreRoot) -> BackendReport {
        let kvm = Self::kvm_usable();
        let ch = Self::file_exists(&self.ch_bin);
        let remote = Self::file_exists(&self.ch_remote);
        let kernel = Self::file_exists(&self.kernel);
        let checks = vec![
            BackendCheck {
                name: "kvm".into(),
                ok: kvm,
                detail: if kvm {
                    "/dev/kvm opens read-write".into()
                } else {
                    "/dev/kvm missing or not permitted".into()
                },
                fix: if kvm {
                    None
                } else {
                    Some("install/enable KVM; add your user to the 'kvm' group".into())
                },
            },
            BackendCheck {
                name: "cloud-hypervisor".into(),
                ok: ch && remote,
                detail: format!("{} , {}", self.ch_bin.display(), self.ch_remote.display()),
                fix: if ch && remote {
                    None
                } else {
                    Some("install cloud-hypervisor v54.0 (see bench/microvm/README)".into())
                },
            },
            BackendCheck {
                name: "guest kernel".into(),
                ok: kernel,
                detail: self.kernel.display().to_string(),
                fix: if kernel {
                    None
                } else {
                    Some("build it: bench/microvm/build_kernel.sh".into())
                },
            },
        ];
        // Even with everything present, this tier only runs the prototype
        // workload today — say so in the report rather than overselling.
        BackendReport {
            backend: self.name().to_string(),
            available: kvm && ch && remote && kernel,
            version: None,
            checks,
        }
    }

    fn run(&self, _store_root: &StoreRoot, _request: &RunRequest) -> Result<i32> {
        // Deliberately explicit: the microVM tier boots the pinned kernel +
        // initramfs prototype (see `examples/boot.rs` and `bench/microvm`),
        // but arbitrary environments (binds, volumes, ports, any image) are
        // not wired through the guest yet. Refuse cleanly.
        Err(BackendError::Unavailable(
            "the kvm-microvm engine runs the prototype workload only (bench/microvm) — \
             arbitrary environments still need the guest integration; \
             the native namespaces engine remains the default"
                .into(),
        ))
    }
}

/// A running microVM; stop it (or let Drop do it) to release every resource.
pub struct RunningVm {
    child: Child,
    ch_remote: PathBuf,
    api_socket: PathBuf,
    state_dir: PathBuf,
    stopped: bool,
}

impl RunningVm {
    /// Block until the guest accepts TCP on the bench port, or `deadline`.
    pub fn wait_ready(&self, deadline: Instant) -> Result<Duration> {
        let addr = SocketAddr::from((GUEST_IP, GUEST_PORT));
        loop {
            if TcpStream::connect_timeout(&addr, Duration::from_millis(50)).is_ok() {
                return Ok(deadline.elapsed());
            }
            if Instant::now() >= deadline {
                return Err(BackendError::Unavailable(
                    "guest did not become ready before the deadline (see console log in the state dir)".into(),
                ));
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Clean shutdown: VMM shutdown (removes the tap), wait, kill if needed,
    /// remove the state dir. Idempotent.
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
