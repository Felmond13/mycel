//! Firecracker snapshot accelerator for the kvm-microvm engine.
//!
//! Cloud Hypervisor snapshots fine but the restored guest stays inert under
//! WSL2's KVM; Firecracker's save/restore path is distinct and VIVANT there
//! (measured: 10/10 alive restores, median 91 ms vs ~389 ms cold boot —
//! `bench/snapshot-wsl2`, WSL2 2026-11-04). This module turns that into a
//! per-fingerprint cache so repeated runs of the same environment skip the
//! kernel boot entirely:
//!
//! * **cold** — Firecracker boots the same `init-vm.cpio` (voie A rootfs
//!   ext4) with `mycagent=1`: stage 1 mounts the rootfs image READ ONLY
//!   (it is a shared cache) under a tmpfs overlay, then the guest agent
//!   (`/myc/init-guest`) polls the scratch disk. The host waits for the
//!   `myc-agent: ready` console marker, pauses the VM, writes a Full
//!   snapshot, injects the command into the scratch file and resumes.
//! * **restore** — a new Firecracker loads the snapshot (the restored VM
//!   starts paused), the host rewrites the scratch file with the command
//!   and resumes. One snapshot serves arbitrary commands.
//!
//! The scratch protocol (host → guest): 4096-byte block at offset 0 —
//! `"MYC1"` magic, `u32` LE payload length, then the same shell-sourceable
//! assignments the classic path bakes into `/myc/run.env`. The guest agent
//! invalidates its page cache between polls (`blockdev --flushbufs`), so a
//! host write while the VM is paused is picked up right after resume.
//! Guest → host: `"MYCE"` + three decimal digits at the LAST 512-byte
//! sector, followed by `reboot -f` (Firecracker exits on guest reboot; it
//! has no ACPI S5 — `poweroff -f` leaves the VM halted forever, measured
//! in the pre-implementation spike).
//!
//! Snapshot layout (`~/.mycel/snapshots/<fingerprint>/`): `vmstate.bin`,
//! `mem.img`, `rootfs.ext4` (read-only, shared by every restore of this
//! fingerprint), `scratch.img` (per-run command channel), `meta.json`,
//! `lock/`. The Firecracker vmstate embeds the DRIVE PATHS, so everything
//! the VM re-opens must live at these stable paths; the per-run mutable
//! state is only the scratch content, rewritten while paused. The rootfs
//! ext4 is mounted read-only in the guest and overlaid with tmpfs, so runs
//! cannot dirty the cache. A directory lock serializes runs of one
//! fingerprint; a concurrent run of the same environment falls back to the
//! Cloud Hypervisor path.
//!
//! Networking is fully reused from [`crate::kvm`] (bridge `br-myc`, leases,
//! DNAT rules, userland port relay, permanent ARP + ICMP warmup semantics).
//! Firecracker does not create its own tap: the host creates `myc-<n>`
//! before the VMM starts and deletes it afterwards.

use std::io::{Read, Seek, SeekFrom, Write};
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use myc_manifest::Manifest;
use sha2::{Digest, Sha256};

use crate::kvm::{
    allocate_lease, detach_tap, dnat_rules, ensure_bridge, guest_env, iptables_bin, iptables_run,
    is_root, pid_alive, purge_tag_rules, release_lease, run_env_content, seed_guest_arp,
    spawn_console_tailer, tree_size, vm_assets_cache, vm_net, GuestSpec, Lease, VmNet, BRIDGE_NAME,
};
use crate::{BackendError, Result, RunRequest};

/// Firecracker release the accelerator is built against (matches
/// `bench/snapshot-wsl2`, which proved restore liveliness under WSL2-KVM).
const FC_RELEASE_TAG: &str = "v1.17.0";
/// Upstream release assets, proven URL first (v1.17 layout), legacy
/// `release-x86_64.tar.gz` second.
const FC_RELEASE_URLS: [&str; 2] = [
    "https://github.com/firecracker-microvm/firecracker/releases/download/v1.17.0/firecracker-v1.17.0-x86_64.tgz",
    "https://github.com/firecracker-microvm/firecracker/releases/download/v1.17.0/release-x86_64.tar.gz",
];
/// A firecracker binary is ~4 MB; below this the download is truncated.
const MIN_FC_BYTES: u64 = 1024 * 1024;
/// The kernel ELF Firecracker boots (same build as the installed
/// `vmlinux-vmfs` bzImage — Firecracker does not load bzImages).
pub(crate) const FC_KERNEL_ASSET: &str = "vmlinux-vmfs.elf";
const MIN_KERNEL_BYTES: u64 = 1024 * 1024;

/// Bump to invalidate every cached snapshot (agent protocol changes).
const AGENT_PROTOCOL: u32 = 1;
/// Console line the guest agent prints once it polls the scratch disk.
const READY_MARKER: &str = "myc-agent: ready";
/// Console line printed right before the agent execs the workload — proves
/// the workload started (vs a VM that died before doing anything).
const RUNNING_MARKER: &str = "myc-agent: running";
/// Command block magic (host → guest), followed by `u32` LE payload length.
const CMD_MAGIC: &[u8; 4] = b"MYC1";
/// Exit marker magic (guest → host) + three ASCII decimal digits.
const EXIT_MAGIC: &[u8; 4] = b"MYCE";
/// Size of the command block at scratch offset 0.
const CMD_BLOCK: usize = 4096;
/// Sector carrying the exit code = last sector of the 1 MiB scratch disk.
const EXIT_SECTOR: u64 = (crate::kvm::SCRATCH_SIZE / 512) - 1;

const STOP_TIMEOUT: Duration = Duration::from_secs(5);
const API_WAIT: Duration = Duration::from_secs(10);
const READY_WAIT: Duration = Duration::from_secs(30);
/// Generous read timeout: `snapshot/create` of a 512 MiB guest takes
/// seconds; the restore load ~0.1-2 s.
const API_READ_TIMEOUT: Duration = Duration::from_secs(180);

static FC_COUNTER: AtomicU32 = AtomicU32::new(0);

// ======================================================================
// Pure pieces: scratch protocol, fingerprint, memory parsing
// ======================================================================

/// Host → guest command block: `MYC1` + `u32` LE length + payload,
/// zero-padded to [`CMD_BLOCK`] bytes.
fn encode_command_block(payload: &str) -> Vec<u8> {
    let mut block = Vec::with_capacity(CMD_BLOCK);
    block.extend_from_slice(CMD_MAGIC);
    block.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    block.extend_from_slice(payload.as_bytes());
    block.resize(CMD_BLOCK, 0);
    block
}

/// Guest → host exit marker: `MYCE` + exactly three decimal digits
/// (0..=255).
fn parse_exit_marker(bytes: &[u8]) -> Option<i32> {
    if bytes.get(..4)? != EXIT_MAGIC {
        return None;
    }
    let digits = bytes.get(4..7)?;
    if !digits.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let code: i32 = std::str::from_utf8(digits).ok()?.parse().ok()?;
    (0..=255).contains(&code).then_some(code)
}

/// sha256 of length-prefixed parts — domain separation, so `[ab, c]` and
/// `[a, bc]` hash differently.
fn fingerprint(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    hex::encode(hasher.finalize())
}

fn file_sha256(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// [`file_sha256`] with a memo: the fingerprint inputs are immutable guest
/// assets (kernel ELF ~20 MB, initramfs ~2 MB — hashing them costs
/// ~114 ms, measured), so the digest is cached under the snapshots root,
/// keyed by path+size+mtime (a rebuilt kernel changes mtime → re-hash).
/// A corrupt sidecar yields a wrong fingerprint at worst = a cache MISS =
/// one cold boot (safe, never a wrong reuse: a hit still compares the
/// meta protocol/version).
fn file_sha256_cached(path: &Path) -> Result<String> {
    file_sha256_cached_in(&snapshots_root(), path)
}

fn file_sha256_cached_in(root: &Path, path: &Path) -> Result<String> {
    let meta = std::fs::metadata(path)?;
    let mut key = Sha256::new();
    key.update(path.to_string_lossy().as_bytes());
    key.update(meta.len().to_le_bytes());
    if let Ok(mtime) = meta.modified() {
        if let Ok(secs) = mtime.duration_since(std::time::UNIX_EPOCH) {
            key.update(secs.as_nanos().to_le_bytes());
        }
    }
    let sidecar = root.join("hashes").join(hex::encode(key.finalize()));
    if let Ok(cached) = std::fs::read_to_string(&sidecar) {
        if cached.len() == 64 && cached.chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(cached.to_lowercase());
        }
    }
    let digest = file_sha256(path)?;
    let _ = std::fs::create_dir_all(sidecar.parent().unwrap_or(Path::new(".")));
    let _ = std::fs::write(&sidecar, &digest);
    Ok(digest)
}

fn sha256_hex_of(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// `MYCEL_VM_MEM` parsing for the Firecracker machine config: `512M`,
/// `1G`, or plain MiB. Cloud Hypervisor consumes the raw string; the
/// snapshot path needs an integer — and the value is part of the snapshot
/// fingerprint, because a snapshot fixes the guest memory size.
fn parse_mem_mib(size: &str) -> Option<u32> {
    let size = size.trim();
    let (num, mult) = if let Some(n) = size.strip_suffix(['M', 'm']) {
        (n, 1u32)
    } else if let Some(n) = size.strip_suffix(['G', 'g']) {
        (n, 1024)
    } else {
        (size, 1)
    };
    let n: u32 = num.trim().parse().ok()?;
    n.checked_mul(mult).filter(|m| *m >= 16)
}

/// The published initramfs must contain the agent branch (`mycagent=`) —
/// cpio newc is uncompressed, so a plain byte search is authoritative.
fn initramfs_has_agent(path: &Path) -> bool {
    std::fs::read(path)
        .map(|bytes| bytes.windows(8).any(|w| w == b"mycagent"))
        .unwrap_or(false)
}

// ======================================================================
// Minimal HTTP client over the Firecracker API unix socket
// ======================================================================

/// Parse a response head: `(status code, content-length)`. Pure.
fn parse_response_head(head: &[u8]) -> Option<(u16, usize)> {
    let head = std::str::from_utf8(head).ok()?;
    let status = head.split("\r\n").next()?;
    let code = status.split(' ').nth(1)?.parse().ok()?;
    let mut length = 0usize;
    for line in head.split("\r\n").skip(1) {
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            length = v.trim().parse().unwrap_or(0);
        }
    }
    Some((code, length))
}

/// Read one HTTP response off `stream`: headers until `\r\n\r\n`, then
/// exactly `Content-Length` body bytes (correct whether or not the server
/// honors `Connection: close`).
fn read_http_response(stream: &mut UnixStream) -> Result<(u16, String)> {
    let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p;
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(BackendError::Unavailable(
                "firecracker API closed the connection before headers".into(),
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let (code, content_length) = parse_response_head(&buf[..head_end])
        .ok_or_else(|| BackendError::Unavailable("malformed firecracker API status line".into()))?;
    let body_start = head_end + 4;
    while buf.len() < body_start + content_length {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break; // short body — parse what we got
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = buf.get(body_start..).unwrap_or(&[]);
    let body = &body[..content_length.min(body.len())];
    Ok((code, String::from_utf8_lossy(body).into_owned()))
}

fn api_error_detail(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error_message")
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.trim().to_string())
}

/// Firecracker API client (HTTP/1.1 over a unix socket, one connection
/// per request — Firecracker's micro-http is happy with that and it keeps
/// the client trivially correct).
#[derive(Clone)]
pub(crate) struct FcClient {
    socket: PathBuf,
    read_timeout: Duration,
}

impl FcClient {
    fn request(&self, method: &str, path: &str, body: Option<&str>) -> Result<(u16, String)> {
        let mut stream = UnixStream::connect(&self.socket).map_err(|e| {
            BackendError::Unavailable(format!(
                "firecracker API socket {}: {e}",
                self.socket.display()
            ))
        })?;
        stream.set_read_timeout(Some(self.read_timeout))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        let body = body.unwrap_or("");
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes())?;
        stream.flush()?;
        read_http_response(&mut stream)
    }

    /// `api` = expect 2xx; the error carries Firecracker's message.
    fn api(&self, method: &str, path: &str, body: Option<&str>) -> Result<String> {
        let (code, response) = self.request(method, path, body)?;
        if (200..300).contains(&code) {
            Ok(response)
        } else {
            Err(BackendError::Unavailable(format!(
                "firecracker {method} {path} -> HTTP {code}: {}",
                api_error_detail(&response)
            )))
        }
    }

    fn ping(&self) -> bool {
        self.request("GET", "/version", None)
            .map(|(code, _)| (200..300).contains(&code))
            .unwrap_or(false)
    }
}

/// Poll the API socket until it answers (Firecracker binds it a few ms
/// after spawn); false if the process dies or the deadline passes.
fn wait_fc_api(socket: &Path, child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let probe = FcClient {
        socket: socket.to_path_buf(),
        read_timeout: Duration::from_secs(2),
    };
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return false,
            Ok(None) => {}
        }
        if probe.ping() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    false
}

// ======================================================================
// VMM abstraction (minimal): one lifecycle API over CH and Firecracker
// ======================================================================

/// Lifecycle handle over a started VMM. The Cloud Hypervisor variant goes
/// through `ch-remote`; the Firecracker variant through its HTTP API. The
/// classic CH run loop in [`crate::kvm`] keeps its own inline flow — this
/// abstraction serves the snapshot path today and exists so the two VMMs
/// stay interchangeable (pause/resume/snapshot/kill have equivalent
/// semantics on both sides).
pub(crate) enum VmmHandle {
    /// Cloud Hypervisor, driven through `ch-remote`. Wired for parity
    /// (pause/resume/snapshot build the exact ch-remote commands); the
    /// classic CH run loop in kvm.rs deliberately keeps its own inline
    /// flow — refactoring it is out of scope here, so this variant is
    /// never constructed outside its future call sites.
    #[allow(dead_code)]
    Ch {
        child: Child,
        ch_remote: PathBuf,
        api_socket: PathBuf,
    },
    Fc {
        child: Child,
        client: FcClient,
    },
}

fn remote_cmd(ch_remote: &Path, api_socket: &Path, cmd: &str) -> Result<()> {
    let out = Command::new(ch_remote)
        .arg("--api-socket")
        .arg(api_socket)
        .arg(cmd)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| BackendError::Unavailable(format!("run ch-remote {cmd}: {e}")))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(BackendError::Unavailable(format!(
            "ch-remote {cmd} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

/// Borrow the Firecracker API client off a handle (panic-free way to use
/// it while the handle stays owned by the run guard).
fn fc_of(vm: &VmmHandle) -> &FcClient {
    match vm {
        VmmHandle::Fc { client, .. } => client,
        VmmHandle::Ch { .. } => unreachable!("fc_of on a cloud-hypervisor handle"),
    }
}

impl VmmHandle {
    fn pause(&self) -> Result<()> {
        match self {
            Self::Fc { client, .. } => client
                .api("PATCH", "/vm", Some(r#"{"state":"Paused"}"#))
                .map(|_| ()),
            Self::Ch {
                ch_remote,
                api_socket,
                ..
            } => remote_cmd(ch_remote, api_socket, "pause"),
        }
    }

    fn resume(&self) -> Result<()> {
        match self {
            Self::Fc { client, .. } => client
                .api("PATCH", "/vm", Some(r#"{"state":"Resumed"}"#))
                .map(|_| ()),
            Self::Ch {
                ch_remote,
                api_socket,
                ..
            } => remote_cmd(ch_remote, api_socket, "resume"),
        }
    }

    /// Full (non-diff) snapshot of a paused VM.
    fn snapshot_full(&self, vmstate: &Path, mem: &Path) -> Result<()> {
        match self {
            Self::Fc { client, .. } => {
                let body = serde_json::json!({
                    "snapshot_path": vmstate.display().to_string(),
                    "mem_file_path": mem.display().to_string(),
                    "snapshot_type": "Full",
                });
                client
                    .api("PUT", "/snapshot/create", Some(&body.to_string()))
                    .map(|_| ())
            }
            Self::Ch {
                ch_remote,
                api_socket,
                ..
            } => {
                // ch-remote v54 snapshots into a directory URL. (CH
                // snapshots work; its RESTORE is inert under WSL2-KVM —
                // see the module docs.)
                let dir = vmstate.parent().unwrap_or(Path::new("."));
                remote_cmd(
                    ch_remote,
                    api_socket,
                    &format!("snapshot file://{}", dir.display()),
                )
            }
        }
    }

    /// Load a snapshot into a freshly started Firecracker (the restored VM
    /// starts paused). The Cloud Hypervisor variant is deliberately
    /// unwired: its restore is inert under WSL2-KVM — refusing beats a
    /// silent hang.
    fn load_snapshot(&self, vmstate: &Path, mem: &Path) -> Result<()> {
        match self {
            Self::Fc { client, .. } => {
                let body = serde_json::json!({
                    "snapshot_path": vmstate.display().to_string(),
                    "mem_backend": {
                        "backend_type": "File",
                        "backend_path": mem.display().to_string(),
                    },
                });
                client
                    .api("PUT", "/snapshot/load", Some(&body.to_string()))
                    .map(|_| ())
            }
            Self::Ch { .. } => Err(BackendError::Unavailable(
                "cloud-hypervisor snapshot restore is not wired (inert under WSL2-KVM)".into(),
            )),
        }
    }

    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        match self {
            Self::Ch { child, .. } | Self::Fc { child, .. } => child.try_wait(),
        }
    }

    /// SIGTERM the VMM process (Firecracker has no shutdown API).
    fn sigterm(&self) {
        let pid = match self {
            Self::Ch { child, .. } | Self::Fc { child, .. } => child.id(),
        };
        let _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    fn kill(&mut self) {
        let child = match self {
            Self::Ch { child, .. } | Self::Fc { child, .. } => child,
        };
        let _ = child.kill();
        let _ = child.wait();
    }
}

// ======================================================================
// Guest agent (stage 2, written into the cached rootfs image)
// ======================================================================

/// PID 1 of the environment on the snapshot path. Same contract as the
/// classic `/myc/init-guest` (env, workdir, hostname, exit code) except
/// the command arrives through the scratch disk: poll (`blockdev
/// --flushbufs` then read — the host rewrote the file while the VM was
/// paused), exec, write `MYCE%03d` at the last sector, then `reboot -f`
/// (Firecracker exits on guest reboot; it has no ACPI S5, so
/// `poweroff -f` would leave the run hanging).
///
/// Every external applet goes through the STATIC busybox embedded by the
/// host at `/myc/busybox` (see [`embed_guest_busybox`]): the environment
/// image cannot be trusted for tools — alpine ships no `blockdev` symlink
/// (measured: the agent looped on a stale page cache until timeout, then
/// `reboot` was not found either and PID 1 took the kernel down with it).
const AGENT_GUEST: &str = r#"#!/bin/sh
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
BB=/myc/busybox
mkdir -p /proc /sys /dev /tmp /run 2>/dev/null
mount -t proc proc /proc 2>/dev/null
mount -t sysfs sysfs /sys 2>/dev/null
mount -t devtmpfs devtmpfs /dev 2>/dev/null
mount -t tmpfs tmpfs /tmp 2>/dev/null
echo "myc-agent: ready" > /dev/console
i=0
while [ "$i" -lt 6000 ]; do
  # /dev/vdb est FIXE sur ce chemin (vda = rootfs, vdb = scratch) : la
  # boucle tourne AVANT que le payload ne fournisse MYC_EXIT_DEV.
  "$BB" blockdev --flushbufs /dev/vdb 2>/dev/null
  magic=$("$BB" dd if=/dev/vdb bs=1 count=4 2>/dev/null)
  if [ "$magic" = "MYC1" ]; then
    len=$("$BB" dd if=/dev/vdb bs=1 skip=4 count=4 2>/dev/null | "$BB" od -An -tu4 | "$BB" tr -d ' \n')
    nsec=$(( (len + 8 + 511) / 512 ))
    "$BB" dd if=/dev/vdb bs=512 count="$nsec" 2>/dev/null | "$BB" tail -c +9 | "$BB" head -c "$len" > /tmp/cmd.env
    . /tmp/cmd.env
    j=0
    while [ "$j" -lt "$MYC_ENV_N" ]; do
      eval "e=\$MYC_ENV_$j"
      export "$e"
      j=$((j+1))
    done
    set --
    j=0
    while [ "$j" -lt "$MYC_ARGC" ]; do
      eval "a=\$MYC_ARG_$j"
      set -- "$@" "$a"
      j=$((j+1))
    done
    [ -n "$MYC_HOSTNAME" ] && "$BB" hostname "$MYC_HOSTNAME" 2>/dev/null
    if [ -n "$MYC_WORKDIR" ]; then
      mkdir -p "$MYC_WORKDIR" 2>/dev/null
      cd "$MYC_WORKDIR" 2>/dev/null
    fi
    echo "myc-agent: running: $*" > /dev/console
    "$@"
    code=$?
    printf 'MYCE%03d' "$code" | "$BB" dd of="$MYC_EXIT_DEV" bs=512 seek=2047 count=1 conv=notrunc 2>/dev/null
    "$BB" sync
    "$BB" reboot -f
  fi
  "$BB" sleep 0.01
  i=$((i+1))
done
echo "myc-agent: timeout" > /dev/console
"$BB" reboot -f
"#;

// ======================================================================
// Asset provisioning (Firecracker binary + kernel ELF)
// ======================================================================

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Download the first URL that works, minimum-size checked. Runs on a
/// dedicated thread: reqwest's blocking client must not run under an
/// ambient tokio runtime (myc-web embeds the engine).
fn http_fetch(urls: &[&str], min_bytes: u64) -> Result<Vec<u8>> {
    let urls: Vec<String> = urls.iter().map(|u| u.to_string()).collect();
    let spawned = thread::Builder::new()
        .name("myc-fc-assets".into())
        .spawn(move || {
            let client = reqwest::blocking::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(600))
                .build()
                .map_err(|e| BackendError::Unavailable(format!("asset http client: {e}")))?;
            let mut errors: Vec<String> = Vec::new();
            for url in &urls {
                let fetched = (|| -> Result<Vec<u8>> {
                    let resp = client
                        .get(url)
                        .send()
                        .and_then(|r| r.error_for_status())
                        .map_err(|e| BackendError::Unavailable(format!("{url}: {e}")))?;
                    let bytes = resp
                        .bytes()
                        .map_err(|e| BackendError::Unavailable(format!("{url}: body: {e}")))?
                        .to_vec();
                    if (bytes.len() as u64) < min_bytes {
                        return Err(BackendError::Unavailable(format!(
                            "{url}: only {} bytes (expected >= {min_bytes})",
                            bytes.len()
                        )));
                    }
                    Ok(bytes)
                })();
                match fetched {
                    Ok(bytes) => return Ok(bytes),
                    Err(e) => errors.push(e.to_string()),
                }
            }
            Err(BackendError::Unavailable(format!(
                "cannot download firecracker: {}",
                errors.join("; ")
            )))
        })
        .map_err(|e| BackendError::Unavailable(format!("spawn asset thread: {e}")))?;
    match spawned.join() {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(BackendError::Unavailable(
            "asset download thread panicked".into(),
        )),
    }
}

fn find_file_recursive(dir: &Path, pred: &dyn Fn(&str) -> bool) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_file_recursive(&path, pred) {
                return Some(found);
            }
        } else if path
            .file_name()
            .map(|n| pred(&n.to_string_lossy()))
            .unwrap_or(false)
        {
            return Some(path);
        }
    }
    None
}

/// Locate the Firecracker binary: env/system path first, the download
/// cache second, a one-shot upstream release download last (extracted
/// from the release tarball; the jailer is ignored — parity with Cloud
/// Hypervisor, which runs unjailed).
pub(crate) fn resolve_fc_bin(engine: &crate::kvm::KvmMicroVm) -> Result<PathBuf> {
    if is_executable(&engine.fc_bin) {
        return Ok(engine.fc_bin.clone());
    }
    let cache = vm_assets_cache().join("firecracker");
    if is_executable(&cache) {
        return Ok(cache);
    }
    eprintln!(
        "[myc-vm] firecracker missing at {} — downloading once from the upstream \
         {FC_RELEASE_TAG} release into {} (override with MYCEL_FC_BIN)",
        engine.fc_bin.display(),
        cache.display()
    );
    std::fs::create_dir_all(vm_assets_cache())?;
    let bytes = http_fetch(&FC_RELEASE_URLS, MIN_FC_BYTES)?;
    let extract = tempfile::tempdir()?;
    std::fs::write(extract.path().join("fc.tgz"), &bytes)?;
    let status = Command::new("tar")
        .args(["-xzf", "fc.tgz"])
        .current_dir(extract.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| BackendError::Unavailable(format!("run tar: {e}")))?;
    if !status.success() {
        return Err(BackendError::Unavailable(
            "cannot extract the firecracker release tarball".into(),
        ));
    }
    // v1.17 layout: release-vX.Y.Z-x86_64/firecracker-vX.Y.Z-x86_64. The
    // tarball also ships firecracker_spec-vX.Y.Z.yaml (the API swagger —
    // plain text!) and .debug twins: match the version-suffixed binary
    // shape exactly, then verify the ELF magic before trusting the cache.
    let bin = find_file_recursive(extract.path(), &|name| {
        (name == "firecracker" || name.starts_with("firecracker-v"))
            && !name.ends_with(".debug")
            && !name.contains("jailer")
    })
    .ok_or_else(|| {
        BackendError::Unavailable("no firecracker binary inside the release tarball".into())
    })?;
    std::fs::copy(&bin, &cache)?;
    std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o755))?;
    if !is_elf(&cache) {
        let _ = std::fs::remove_file(&cache);
        return Err(BackendError::Unavailable(format!(
            "extracted {} is not an ELF binary — unexpected release layout",
            bin.display()
        )));
    }
    Ok(cache)
}

/// True when `path` starts with the ELF magic (0x7f "ELF").
fn is_elf(path: &Path) -> bool {
    let mut magic = [0u8; 4];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .map(|_| &magic == b"\x7fELF")
        .unwrap_or(false)
}

/// Locate the kernel ELF Firecracker boots: env/system path, download
/// cache, the local kernel build tree (the ELF of the same build as the
/// installed `vmlinux-vmfs` bzImage), then a mycel release download.
pub(crate) fn resolve_fc_kernel(engine: &crate::kvm::KvmMicroVm) -> Result<PathBuf> {
    if engine.fc_kernel.is_file() {
        return Ok(engine.fc_kernel.clone());
    }
    let cache = vm_assets_cache().join(FC_KERNEL_ASSET);
    if let Ok(meta) = std::fs::metadata(&cache) {
        if meta.len() >= MIN_KERNEL_BYTES {
            return Ok(cache);
        }
    }
    std::fs::create_dir_all(vm_assets_cache())?;
    // The build tree ships the ELF next to the bzImage the CH path uses.
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(Path::new(&home).join("build/mycel-kernel/build-vmfs/vmlinux"));
    }
    candidates.push(PathBuf::from("/usr/local/share/mycel/vmlinux-vmfs.elf"));
    if let Ok(homes) = std::fs::read_dir("/home") {
        for h in homes.flatten() {
            candidates.push(h.path().join("build/mycel-kernel/build-vmfs/vmlinux"));
        }
    }
    for cand in candidates {
        if let Ok(meta) = std::fs::metadata(&cand) {
            if meta.len() >= MIN_KERNEL_BYTES {
                std::fs::copy(&cand, &cache)?;
                eprintln!(
                    "[myc-vm] kernel ELF for firecracker: copied from the build tree ({}) \
                     into {}",
                    cand.display(),
                    cache.display()
                );
                return Ok(cache);
            }
        }
    }
    // Future published asset (same pattern as vmlinux-vmfs / init-vm.cpio).
    let tag = format!("v{}", env!("CARGO_PKG_VERSION"));
    let urls = [
        format!("https://github.com/Felmond13/mycel/releases/download/{tag}/{FC_KERNEL_ASSET}"),
        format!("https://github.com/Felmond13/mycel/releases/latest/download/{FC_KERNEL_ASSET}"),
    ];
    let url_refs: Vec<&str> = urls.iter().map(|s| s.as_str()).collect();
    match http_fetch(&url_refs, MIN_KERNEL_BYTES) {
        Ok(bytes) => {
            std::fs::write(&cache, bytes)?;
            Ok(cache)
        }
        Err(e) => Err(BackendError::Unavailable(format!(
            "no kernel ELF for firecracker: {e}. Firecracker only loads uncompressed \
             vmlinux ELF (not the bzImage) — build the vmfs kernel once and keep the tree: \
             bench/microvm/build_kernel_vmfs.sh (the ELF is then copied from \
             ~/build/mycel-kernel/build-vmfs/vmlinux), or publish a '{FC_KERNEL_ASSET}' \
             release asset"
        ))),
    }
}

/// `firecracker --version` → "1.17.0" (first line: "Firecracker v1.17.0").
fn fc_binary_version(bin: &Path) -> Result<String> {
    let out = Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| BackendError::Unavailable(format!("run {}: {e}", bin.display())))?;
    let line = String::from_utf8_lossy(&out.stdout);
    let version = line
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .find(|t| t.contains('.'))
        .unwrap_or_default()
        .trim_start_matches('v')
        .to_string();
    if version.is_empty() {
        return Err(BackendError::Unavailable(format!(
            "cannot parse firecracker version from: {}",
            line.trim()
        )));
    }
    Ok(version)
}

// ======================================================================
// Snapshot cache directory: files, meta, lock
// ======================================================================

/// `~/.mycel/snapshots/` (override with `MYCEL_SNAPSHOT_DIR` — used by the
/// bench to keep measurements hermetic).
fn snapshots_root() -> PathBuf {
    if let Ok(dir) = std::env::var("MYCEL_SNAPSHOT_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    Path::new(&home).join(".mycel").join("snapshots")
}

/// Stable file names inside one snapshot directory. The Firecracker
/// vmstate re-opens the drive files by the paths recorded at snapshot
/// time, so `rootfs.ext4` and `scratch.img` must stay exactly here.
struct SnapshotFiles;

impl SnapshotFiles {
    fn vmstate(dir: &Path) -> PathBuf {
        dir.join("vmstate.bin")
    }
    fn mem(dir: &Path) -> PathBuf {
        dir.join("mem.img")
    }
    fn rootfs(dir: &Path) -> PathBuf {
        dir.join("rootfs.ext4")
    }
    fn scratch(dir: &Path) -> PathBuf {
        dir.join("scratch.img")
    }
    fn meta(dir: &Path) -> PathBuf {
        dir.join("meta.json")
    }

    fn complete(dir: &Path, mem_mib: u32) -> bool {
        let mem_bytes = mem_mib as u64 * 1024 * 1024;
        [
            Self::vmstate(dir),
            Self::rootfs(dir),
            Self::scratch(dir),
            Self::meta(dir),
        ]
        .iter()
        .all(|p| std::fs::metadata(p).map(|m| m.len() > 0).unwrap_or(false))
            && std::fs::metadata(Self::mem(dir))
                .map(|m| m.len() >= mem_bytes)
                .unwrap_or(false)
    }

    /// Delete the snapshot content (the lock claim survives).
    fn wipe(dir: &Path) {
        for p in [
            Self::vmstate(dir),
            Self::mem(dir),
            Self::rootfs(dir),
            Self::scratch(dir),
        ] {
            let _ = std::fs::remove_file(p);
        }
    }

    fn meta_write(dir: &Path, fc_version: &str, mem_mib: u32) {
        let meta = serde_json::json!({
            "agent_protocol": AGENT_PROTOCOL,
            "fc_version": fc_version,
            "mem_mib": mem_mib,
            "created_unix": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        });
        let _ = std::fs::write(Self::meta(dir), meta.to_string());
    }

    /// A snapshot is only reusable with the same agent protocol and the
    /// same Firecracker (the vmstate format is version-specific).
    fn meta_matches(dir: &Path, fc_version: &str) -> bool {
        let Ok(text) = std::fs::read_to_string(Self::meta(dir)) else {
            return false;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            return false;
        };
        value.get("agent_protocol").and_then(|v| v.as_u64()) == Some(AGENT_PROTOCOL as u64)
            && value.get("fc_version").and_then(|v| v.as_str()) == Some(fc_version)
    }
}

/// Exclusive claim on one snapshot directory (mkdir is atomic), with the
/// same dead-owner takeover as the network lease registry. Held for the
/// whole VM run — the scratch file is per-run.
struct SnapshotLock {
    dir: PathBuf,
}

impl SnapshotLock {
    fn acquire(parent: &Path) -> Option<Self> {
        let dir = parent.join("lock");
        for _ in 0..2 {
            if std::fs::create_dir(&dir).is_ok() {
                let _ = std::fs::write(dir.join("pid"), std::process::id().to_string());
                return Some(Self { dir });
            }
            let owner = std::fs::read_to_string(dir.join("pid"))
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok());
            match owner {
                Some(p) if pid_alive(p) => return None,
                _ => {
                    let _ = std::fs::remove_dir_all(&dir); // stale claim
                }
            }
        }
        None
    }
}

impl Drop for SnapshotLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ======================================================================
// Scratch disk I/O (host side)
// ======================================================================

/// Rewrite the command block at offset 0 and reset the exit sector —
/// while the VM is paused, so the agent can never observe a half-written
/// block. The exit sector gets the `"MYCR"` sentinel (not plain zeros):
/// a stale `"MYCE..."` marker from a PREVIOUS run of this snapshot cannot
/// survive it, so the host can only ever read an exit code written by
/// THIS run's guest — a lost guest write surfaces as 125, never as a
/// silently wrong code (one such transient was observed 2026-11-04).
fn write_scratch(path: &Path, payload: &str) -> Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false) // fixed size; both regions are rewritten in place
        .open(path)?;
    f.write_all(&encode_command_block(payload))?;
    f.seek(SeekFrom::Start(EXIT_SECTOR * 512))?;
    let mut sentinel = [0u8; 512];
    sentinel[..4].copy_from_slice(b"MYCR");
    f.write_all(&sentinel)?;
    Ok(())
}

fn read_exit_marker(path: &Path) -> Option<i32> {
    let mut f = std::fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(EXIT_SECTOR * 512)).ok()?;
    let mut sector = [0u8; 512];
    f.read_exact(&mut sector).ok()?;
    parse_exit_marker(&sector)
}

// ======================================================================
// Orchestrator
// ======================================================================

/// Result of [`snapshot_run`].
pub enum SnapshotOutcome {
    /// The workload ran through Firecracker; workload exit code.
    Ran(i32),
    /// The snapshot path could not serve this run (reason); the caller
    /// falls back to the regular Cloud Hypervisor path.
    Unavailable(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Cold,
    Restore,
}

/// Internal failure classification for one VM attempt.
struct AttemptErr {
    err: BackendError,
    /// The workload was started (or its side effects are visible): errors
    /// from here on are hard — no silent double run via a fallback.
    committed: bool,
    /// The restore itself is blamed (API refused the load, or the restored
    /// VM died before running anything): wipe the cache entry and retry
    /// with a cold boot.
    reject_restore: bool,
}

impl From<BackendError> for AttemptErr {
    fn from(err: BackendError) -> Self {
        Self {
            err,
            committed: false,
            reject_restore: false,
        }
    }
}

type AttemptResult = std::result::Result<i32, AttemptErr>;

/// Everything one attempt needs; built once by [`snapshot_run`].
struct AttemptCtx<'a> {
    fc_bin: &'a Path,
    fc_kernel: &'a Path,
    initramfs: &'a Path,
    fc_version: &'a str,
    mem_mib: u32,
    snapdir: &'a Path,
    payload: &'a str,
    trace: bool,
}

/// Everything a started snapshot run holds; `release()` (also via Drop)
/// tears it all down. Mirrors `VmRun` in kvm.rs; the snapshot files in
/// `snapdir` are a CACHE and are never deleted here.
struct FcRun {
    vm: Option<VmmHandle>,
    console: PathBuf,
    state_dir: PathBuf,
    net: Option<VmNet>,
    lease: Option<Lease>,
    ipt_rules: Vec<Vec<String>>,
    proxies: Vec<crate::portproxy::PortProxy>,
    console_tail: Option<thread::JoinHandle<()>>,
    console_stop: Arc<AtomicBool>,
    run_dir: Option<tempfile::TempDir>,
    lock: Option<SnapshotLock>,
    released: bool,
}

impl FcRun {
    fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        // 1. VMM: SIGTERM, grace, SIGKILL, reap.
        let deadline = Instant::now() + STOP_TIMEOUT;
        if let Some(vm) = self.vm.as_mut() {
            vm.sigterm();
            loop {
                match vm.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(10))
                    }
                    Ok(None) => {
                        vm.kill();
                        break;
                    }
                    Err(_) => break,
                }
            }
        }
        // 2. Console streamer.
        self.console_stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.console_tail.take() {
            let _ = handle.join();
        }
        // 3. iptables rules (the -A/-I → -D flip, then the tag purge).
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
        if let Some(lease) = &self.lease {
            purge_tag_rules(&lease.tag);
        }
        // 4. Loopback relays.
        for mut proxy in self.proxies.drain(..) {
            proxy.stop();
        }
        // 5. Tap (ours — Firecracker never creates one) + lease.
        if let (Some(net), Some(lease)) = (&self.net, &self.lease) {
            detach_tap(lease);
            release_lease(net, lease, &self.state_dir);
        }
        // 6. Snapshot lock claim.
        self.lock = None; // Drop removes it
                          // 7. State dir last (MYCEL_VM_KEEP keeps it for post-mortem).
        if std::env::var("MYCEL_VM_KEEP").ok().as_deref() == Some("1") {
            eprintln!("[myc-vm] state dir kept: {}", self.state_dir.display());
        } else {
            let _ = std::fs::remove_dir_all(&self.state_dir);
        }
    }
}

impl Drop for FcRun {
    fn drop(&mut self) {
        self.release();
    }
}

/// The Firecracker tap is host-created (unlike Cloud Hypervisor, which
/// creates it itself; the host then attaches it to the bridge).
fn create_tap(lease: &Lease) -> bool {
    crate::kvm::ip_run(&["tuntap", "add", "dev", &lease.tap, "mode", "tap"])
        && crate::kvm::ip_run(&["link", "set", &lease.tap, "master", BRIDGE_NAME])
        && crate::kvm::ip_run(&["link", "set", &lease.tap, "up"])
}

fn spawn_fc(fc_bin: &Path, state_dir: &Path) -> Result<(Child, PathBuf)> {
    let socket = state_dir.join("api.sock");
    // Firecracker refuses to bind an existing socket path — a retry in the
    // same state dir (restore rejected -> cold boot) must clear it first.
    let _ = std::fs::remove_file(&socket);
    let console = std::fs::File::create(state_dir.join("console.log"))?;
    let child = Command::new(fc_bin)
        .arg("--api-sock")
        .arg(&socket)
        .arg("--log-path")
        .arg(state_dir.join("fc.log"))
        .arg("--level")
        .arg("Warning")
        .stdin(Stdio::null())
        .stdout(Stdio::from(console.try_clone()?))
        .stderr(Stdio::from(console))
        .spawn()
        .map_err(|e| BackendError::Unavailable(format!("spawn firecracker: {e}")))?;
    Ok((child, socket))
}

/// Wait for the ready marker on the guest serial console (cold boot only).
fn wait_ready_marker(console: &Path, vm: &mut VmmHandle, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(text) = std::fs::read_to_string(console) {
            if text.contains(READY_MARKER) {
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        match vm.try_wait() {
            Ok(Some(_)) | Err(_) => return false,
            Ok(None) => {}
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn console_contains(console: &Path, needle: &str) -> bool {
    std::fs::read_to_string(console)
        .map(|t| t.contains(needle))
        .unwrap_or(false)
}

fn console_tail(console: &Path, lines: usize) -> String {
    std::fs::read_to_string(console)
        .unwrap_or_default()
        .lines()
        .rev()
        .take(lines)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
}

/// One full VM attempt (cold boot + snapshot, or restore). Returns the
/// workload exit code, or a classified failure.
fn run_vm(g: &mut FcRun, mode: Mode, ctx: &AttemptCtx) -> AttemptResult {
    let t_spawn = Instant::now();
    let (child, socket) = spawn_fc(ctx.fc_bin, &g.state_dir)?;
    let mut child = child;
    if !wait_fc_api(&socket, &mut child, API_WAIT) {
        return Err(BackendError::Unavailable(format!(
            "firecracker API never answered (log: {})",
            g.state_dir.join("fc.log").display()
        ))
        .into());
    }
    g.vm = Some(VmmHandle::Fc {
        child,
        client: FcClient {
            socket,
            read_timeout: API_READ_TIMEOUT,
        },
    });
    g.console_tail = Some(spawn_console_tailer(
        g.console.clone(),
        g.console_stop.clone(),
    ));

    match mode {
        Mode::Restore => {
            let vmstate = SnapshotFiles::vmstate(ctx.snapdir);
            let mem = SnapshotFiles::mem(ctx.snapdir);
            if let Err(e) = g.vm.as_ref().unwrap().load_snapshot(&vmstate, &mem) {
                // A refused load = corrupt or version-drifted snapshot.
                let mut err: AttemptErr = e.into();
                err.reject_restore = true;
                return Err(err);
            }
        }
        Mode::Cold => {
            let boot_args = format!(
                "console=ttyS0 quiet loglevel=3 reboot=k panic=1 mycroot=ext4 mycagent=1{}",
                g.lease
                    .as_ref()
                    .map(|l| format!(" mycip={}", l.ip_str))
                    .unwrap_or_default()
            );
            let boot = serde_json::json!({
                "kernel_image_path": ctx.fc_kernel.display().to_string(),
                "initrd_path": ctx.initramfs.display().to_string(),
                "boot_args": boot_args,
            });
            let machine = serde_json::json!({
                "vcpu_count": 1,
                "mem_size_mib": ctx.mem_mib,
                "smt": false,
            });
            let rootfs_drive = serde_json::json!({
                "drive_id": "rootfs",
                "path_on_host": SnapshotFiles::rootfs(ctx.snapdir).display().to_string(),
                "is_root_device": true,
                "is_read_only": true,
            });
            let scratch_drive = serde_json::json!({
                "drive_id": "scratch",
                "path_on_host": SnapshotFiles::scratch(ctx.snapdir).display().to_string(),
                "is_root_device": false,
                "is_read_only": false,
            });
            let client = fc_of(g.vm.as_ref().unwrap());
            if let Err(e) = client
                .api("PUT", "/boot-source", Some(&boot.to_string()))
                .and_then(|_| client.api("PUT", "/machine-config", Some(&machine.to_string())))
                .and_then(|_| client.api("PUT", "/drives/rootfs", Some(&rootfs_drive.to_string())))
                .and_then(|_| {
                    client.api("PUT", "/drives/scratch", Some(&scratch_drive.to_string()))
                })
            {
                return Err(e.into());
            }
            if let Some(lease) = &g.lease {
                let net = serde_json::json!({
                    "iface_id": "net0",
                    "guest_mac": lease.mac,
                    "host_dev_name": lease.tap,
                });
                if let Err(e) =
                    client.api("PUT", "/network-interfaces/net0", Some(&net.to_string()))
                {
                    return Err(e.into());
                }
            }
            if let Err(e) = client.api(
                "PUT",
                "/actions",
                Some(r#"{"action_type":"InstanceStart"}"#),
            ) {
                return Err(e.into());
            }
            if !wait_ready_marker(&g.console, g.vm.as_mut().unwrap(), READY_WAIT) {
                return Err(BackendError::Unavailable(format!(
                    "guest agent never became ready (last console lines: {})",
                    console_tail(&g.console, 15)
                ))
                .into());
            }
            let vm = g.vm.as_ref().unwrap();
            if let Err(e) = vm.pause() {
                return Err(e.into());
            }
            let t_create = Instant::now();
            if let Err(e) = vm.snapshot_full(
                &SnapshotFiles::vmstate(ctx.snapdir),
                &SnapshotFiles::mem(ctx.snapdir),
            ) {
                return Err(e.into());
            }
            SnapshotFiles::meta_write(ctx.snapdir, ctx.fc_version, ctx.mem_mib);
            eprintln!(
                "[myc-vm] cold boot, snapshot saved (creation {} ms)",
                t_create.elapsed().as_millis()
            );
        }
    }

    // The VM is paused (freshly loaded, or paused post-snapshot): inject
    // the command into the scratch file, then resume. From the resume on,
    // failures are committed — no fallback that could run the workload
    // twice.
    if let Err(e) = write_scratch(&SnapshotFiles::scratch(ctx.snapdir), ctx.payload) {
        let mut err: AttemptErr = e.into();
        err.reject_restore = mode == Mode::Restore;
        return Err(err);
    }
    if let Err(e) = g.vm.as_ref().unwrap().resume() {
        let mut err: AttemptErr = e.into();
        err.reject_restore = mode == Mode::Restore;
        return Err(err);
    }
    if mode == Mode::Restore {
        eprintln!(
            "[myc-vm] restored from snapshot in {} ms",
            t_spawn.elapsed().as_millis()
        );
    }
    if ctx.trace {
        eprintln!(
            "[myc-vm] {} path: spawn->resumed {} ms",
            if mode == Mode::Restore {
                "restore"
            } else {
                "cold"
            },
            t_spawn.elapsed().as_millis()
        );
    }

    // --- wait for the guest (it reboots itself once the workload exits) --
    let interrupted = Arc::new(AtomicBool::new(false));
    {
        let flag = interrupted.clone();
        let _ = ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst));
    }
    let timeout = std::env::var("MYCEL_VM_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok());
    let deadline = timeout.map(|s| Instant::now() + Duration::from_secs(s));
    let mut timed_out = false;
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
                g.vm.as_ref().unwrap().sigterm();
                killing_since = Some(Instant::now());
            } else if killing_since.map(|t| t.elapsed()).unwrap_or_default() > STOP_TIMEOUT {
                g.vm.as_mut().unwrap().kill();
            }
        }
        match g.vm.as_mut().unwrap().try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(e) => {
                let mut err: AttemptErr = BackendError::Io(e).into();
                err.committed = true;
                return Err(err);
            }
        }
    }

    // --- exit code -------------------------------------------------------
    match read_exit_marker(&SnapshotFiles::scratch(ctx.snapdir)) {
        Some(code) => Ok(code),
        None => {
            let ran = console_contains(&g.console, RUNNING_MARKER);
            if mode == Mode::Restore && !ran {
                // Loaded, resumed, died before the agent exec'd anything:
                // blame the snapshot, wipe, cold-boot retry (once).
                let mut err: AttemptErr = BackendError::Unavailable(
                    "restored VM died before running the workload — snapshot wiped".into(),
                )
                .into();
                err.reject_restore = true;
                return Err(err);
            }
            let why = if timed_out {
                "timed out before the guest reported"
            } else if interrupted.load(Ordering::SeqCst) {
                "interrupted before the guest reported"
            } else {
                "the guest never reported an exit code"
            };
            eprintln!(
                "[myc] {why}; last console lines:\n{}",
                console_tail(&g.console, 15)
            );
            Ok(if interrupted.load(Ordering::SeqCst) {
                130
            } else {
                125
            })
        }
    }
}

/// Write the agent stage-2 script into the materialized rootfs (no
/// run.env/run.json: the cached rootfs content must be independent of the
/// command — that is the whole point of the scratch channel).
fn write_agent_guest(rootfs: &Path) -> Result<()> {
    let myc = rootfs.join("myc");
    std::fs::create_dir_all(&myc)?;
    std::fs::write(myc.join("init-guest"), AGENT_GUEST)?;
    std::fs::set_permissions(
        myc.join("init-guest"),
        std::fs::Permissions::from_mode(0o755),
    )?;
    Ok(())
}

/// Embed a static busybox at `/myc/busybox` for the agent: environment
/// images cannot be trusted for tools (alpine has no `blockdev` symlink;
/// distroless has nothing). Same requirement as the initramfs build.
fn embed_guest_busybox(rootfs: &Path) -> Result<()> {
    for src in ["/usr/bin/busybox", "/bin/busybox"] {
        if Path::new(src).is_file() {
            std::fs::copy(src, rootfs.join("myc/busybox"))?;
            std::fs::set_permissions(
                rootfs.join("myc/busybox"),
                std::fs::Permissions::from_mode(0o755),
            )?;
            return Ok(());
        }
    }
    Err(BackendError::Unavailable(
        "the snapshot path needs busybox on the host (apt install busybox-static)".into(),
    ))
}

/// Materialize the rootfs and build the cached ext4 image + scratch disk
/// (cold path, also used by the post-wipe retry).
fn cold_rootfs(
    engine: &crate::kvm::KvmMicroVm,
    store_root: &Path,
    manifest: &Manifest,
    snapdir: &Path,
    state_dir: &Path,
) -> Result<tempfile::TempDir> {
    if !engine.mkfs_ext4.is_file() {
        return Err(BackendError::Unavailable(format!(
            "cold snapshot boot needs mkfs.ext4 ({} missing — install e2fsprogs)",
            engine.mkfs_ext4.display()
        )));
    }
    let store = myc_store::Store::open(store_root)?;
    let run_dir = tempfile::Builder::new()
        .prefix("run-vm-")
        .tempdir_in(store.root().join("tmp"))?;
    let rootfs = run_dir.path().to_path_buf();
    myc_run::materialize(&store, manifest, &rootfs)?;
    write_agent_guest(&rootfs)?;
    embed_guest_busybox(&rootfs)?;
    let rootfs_img = SnapshotFiles::rootfs(snapdir);
    let size = tree_size(&rootfs).saturating_mul(2).max(64 * 1024 * 1024);
    std::fs::File::create(&rootfs_img)?.set_len(size)?;
    let status = Command::new(&engine.mkfs_ext4)
        .args(["-q", "-F", "-L", "myc-rootfs", "-d"])
        .arg(&rootfs)
        .arg(&rootfs_img)
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
    std::fs::File::create(SnapshotFiles::scratch(snapdir))?.set_len(crate::kvm::SCRATCH_SIZE)?;
    Ok(run_dir)
}

/// Try to serve `request` through the Firecracker snapshot cache. Falls
/// back to the caller (Cloud Hypervisor path) with
/// [`SnapshotOutcome::Unavailable`] whenever a piece is missing or a
/// concurrent run holds the snapshot. Hard errors only happen once the
/// workload was actually started.
pub fn snapshot_run(
    engine: &crate::kvm::KvmMicroVm,
    store_root: &Path,
    request: &RunRequest,
    id: &str,
    manifest: &Manifest,
    command: &[String],
) -> Result<SnapshotOutcome> {
    let unavail = |why: String| Ok(SnapshotOutcome::Unavailable(why));
    let trace = std::env::var("MYCEL_VM_TRACE").ok().as_deref() == Some("1");

    // --- gates ------------------------------------------------------------
    if request.no_snapshot_cache {
        return unavail("--no-snapshot-cache".into());
    }
    if std::env::var("MYCEL_SNAPSHOT_CACHE").ok().as_deref() == Some("0") {
        return unavail("MYCEL_SNAPSHOT_CACHE=0".into());
    }
    if std::env::var("MYCEL_VM_ROOTFS").ok().is_some() {
        return unavail("MYCEL_VM_ROOTFS forces a cloud-hypervisor voie".into());
    }
    if !request.binds.is_empty() {
        return unavail("bind mounts need virtiofs — the snapshot path is ext4-only".into());
    }
    if !crate::kvm::KvmMicroVm::kvm_usable() {
        return unavail("/dev/kvm not usable".into());
    }
    let fc_bin = match resolve_fc_bin(engine) {
        Ok(p) => p,
        Err(e) => return unavail(e.to_string()),
    };
    let fc_kernel = match resolve_fc_kernel(engine) {
        Ok(p) => p,
        Err(e) => return unavail(e.to_string()),
    };
    let initramfs = match engine.resolve_vm_asset(
        &engine.vm_initramfs,
        "init-vm.cpio",
        crate::kvm::MIN_INITRAMFS_BYTES,
    ) {
        Ok(p) => p,
        Err(e) => return unavail(e.to_string()),
    };
    if !initramfs_has_agent(&initramfs) {
        return unavail(
            "init-vm.cpio predates the snapshot agent — rebuild it: \
             sudo bench/microvm/mkinit-vm.sh --install"
                .into(),
        );
    }
    let fc_version = match fc_binary_version(&fc_bin) {
        Ok(v) => v,
        Err(e) => return unavail(e.to_string()),
    };
    let mem = std::env::var("MYCEL_VM_MEM").unwrap_or_else(|_| crate::kvm::DEFAULT_VM_MEM.into());
    let Some(mem_mib) = parse_mem_mib(&mem) else {
        return unavail(format!("unparsable MYCEL_VM_MEM '{mem}'"));
    };

    // --- ports plan (same semantics as the CH path) ------------------------
    let mut ports: Vec<(u16, u16)> = request.publish.clone();
    if ports.is_empty() && !request.isolated_network {
        ports = manifest
            .config
            .exposed_ports
            .iter()
            .map(|p| (*p, *p))
            .collect();
    }
    let with_net = !ports.is_empty();
    if with_net && !is_root() {
        return unavail("publishing ports needs root (bridge, tap, DNAT)".into());
    }

    // --- state dir + network lease ----------------------------------------
    // Distinct prefix from the CH path (which numbers its own dirs from 0
    // in the same process): a snapshot fallback must not collide with the
    // CH state dir, or both VMMs fight over api.sock.
    let state_dir = std::env::temp_dir().join(format!(
        "myc-vm-fc-{}-{}",
        std::process::id(),
        FC_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&state_dir)?;
    let mut guard = FcRun {
        vm: None,
        console: state_dir.join("console.log"),
        state_dir: state_dir.clone(),
        net: None,
        lease: None,
        ipt_rules: Vec::new(),
        proxies: Vec::new(),
        console_tail: None,
        console_stop: Arc::new(AtomicBool::new(false)),
        run_dir: None,
        lock: None,
        released: false,
    };
    if with_net {
        let net = match vm_net() {
            Ok(n) => n,
            Err(e) => return unavail(e.to_string()),
        };
        if let Err(e) = ensure_bridge(&net) {
            return unavail(e.to_string());
        }
        let lease = match allocate_lease(&net, &state_dir) {
            Ok(l) => l,
            Err(e) => return unavail(e.to_string()),
        };
        if !create_tap(&lease) {
            return unavail(format!("cannot create tap {} (root?)", lease.tap));
        }
        // Permanent neighbour entry before the guest even exists — the
        // same tail-p95 fix as the CH path
        // (docs/benchmarks/microvm-tail-analysis.md).
        seed_guest_arp(&lease);
        let ipt = iptables_bin();
        purge_tag_rules(&lease.tag);
        for (host, guest_port) in &ports {
            for rule in dnat_rules(&lease.tag, *host, *guest_port, &lease.ip_str) {
                if !iptables_run(&ipt, &rule) {
                    return unavail(format!(
                        "iptables rejected the DNAT rule for port {host} (running as root?)"
                    ));
                }
                guard.ipt_rules.push(rule);
            }
            let target = SocketAddr::from((lease.ip, *guest_port));
            match crate::portproxy::PortProxy::start(*host, target) {
                Ok(proxy) => guard.proxies.push(proxy),
                Err(e) => {
                    return unavail(format!(
                        "cannot listen on 127.0.0.1:{host} to publish it: {e}"
                    ))
                }
            }
        }
        guard.net = Some(net);
        guard.lease = Some(lease);
    }

    // --- fingerprint + snapshot dir ----------------------------------------
    let kernel_sha = match file_sha256_cached(&fc_kernel) {
        Ok(h) => h,
        Err(e) => return unavail(e.to_string()),
    };
    let initrd_sha = match file_sha256_cached(&initramfs) {
        Ok(h) => h,
        Err(e) => return unavail(e.to_string()),
    };
    let agent_sha = sha256_hex_of(AGENT_GUEST.as_bytes());
    let lease_ip = guard
        .lease
        .as_ref()
        .map(|l| l.ip_str.clone())
        .unwrap_or_else(|| "nonet".into());
    let fp = fingerprint(&[
        b"myc-fc-snapshot-v1",
        env!("CARGO_PKG_VERSION").as_bytes(),
        AGENT_PROTOCOL.to_string().as_bytes(),
        kernel_sha.as_bytes(),
        initrd_sha.as_bytes(),
        agent_sha.as_bytes(),
        id.as_bytes(),
        request.binds.join(";").as_bytes(),
        request.hostname.as_bytes(),
        if request.isolated_network {
            b"isolated=1"
        } else {
            b"isolated=0"
        },
        mem_mib.to_string().as_bytes(),
        lease_ip.as_bytes(),
    ]);
    let snapdir = snapshots_root().join(&fp);
    if let Err(e) = std::fs::create_dir_all(&snapdir) {
        return unavail(format!("cannot create {}: {e}", snapdir.display()));
    }
    if trace {
        eprintln!("[myc-vm] snapshot cache: {}", snapdir.display());
    }
    match SnapshotLock::acquire(&snapdir) {
        Some(lock) => guard.lock = Some(lock),
        None => {
            return unavail("another run currently holds the snapshot for this environment".into())
        }
    }

    // --- command payload (the same run.env format the classic path bakes) --
    let hostname = if request.hostname.is_empty() {
        "mycel".into()
    } else {
        request.hostname.clone()
    };
    let payload = run_env_content(&GuestSpec {
        argv: command.to_vec(),
        env: guest_env(manifest, request),
        workdir: request
            .workdir
            .clone()
            .or_else(|| Some(manifest.config.workdir.clone()).filter(|w| !w.is_empty())),
        hostname,
        binds: Vec::new(),
        exit_dev: "/dev/vdb".into(),
    });
    if payload.len() + 8 > CMD_BLOCK {
        return unavail(format!(
            "command+env too large for the scratch block ({} > {} bytes)",
            payload.len(),
            CMD_BLOCK - 8
        ));
    }

    // --- run (restore when a valid snapshot exists; one cold retry on a
    //     rejected restore) -------------------------------------------------
    let have_snapshot = SnapshotFiles::complete(&snapdir, mem_mib)
        && SnapshotFiles::meta_matches(&snapdir, &fc_version);
    let mut mode = if have_snapshot {
        Mode::Restore
    } else {
        Mode::Cold
    };
    if mode == Mode::Cold {
        // Partial leftovers from an interrupted create must not survive.
        SnapshotFiles::wipe(&snapdir);
        match cold_rootfs(engine, store_root, manifest, &snapdir, &state_dir) {
            Ok(dir) => guard.run_dir = Some(dir),
            Err(e) => return unavail(e.to_string()),
        }
    }
    let ctx = AttemptCtx {
        fc_bin: &fc_bin,
        fc_kernel: &fc_kernel,
        initramfs: &initramfs,
        fc_version: &fc_version,
        mem_mib,
        snapdir: &snapdir,
        payload: &payload,
        trace,
    };
    let t0 = Instant::now();
    let code = loop {
        match run_vm(&mut guard, mode, &ctx) {
            Ok(code) => break code,
            Err(e) => {
                if e.committed {
                    return Err(e.err);
                }
                if e.reject_restore {
                    eprintln!(
                        "[myc-vm] snapshot restore rejected ({}) — wiping and cold-booting",
                        e.err
                    );
                    SnapshotFiles::wipe(&snapdir);
                    mode = Mode::Cold;
                    match cold_rootfs(engine, store_root, manifest, &snapdir, &state_dir) {
                        Ok(dir) => guard.run_dir = Some(dir),
                        Err(e) => return unavail(format!("cold retry: {e}")),
                    }
                    continue;
                }
                return unavail(format!("firecracker attempt failed: {}", e.err));
            }
        }
    };
    if trace {
        eprintln!(
            "[myc-vm] snapshot path total: {} ms",
            t0.elapsed().as_millis()
        );
    }
    if request.keep_rootfs {
        if let Some(dir) = guard.run_dir.take() {
            let kept = dir.keep();
            eprintln!("rootfs kept at {}", kept.display());
        }
    }
    guard.release();
    Ok(SnapshotOutcome::Ran(code))
}

// ======================================================================
// Tests
// ======================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_block_shape_and_round_trip() {
        let payload = "MYC_ARGC=1\nMYC_ARG_0='echo hi'\n";
        let block = encode_command_block(payload);
        assert_eq!(block.len(), CMD_BLOCK);
        assert_eq!(&block[..4], CMD_MAGIC);
        let len = u32::from_le_bytes(block[4..8].try_into().unwrap());
        assert_eq!(len as usize, payload.len());
        assert_eq!(&block[8..8 + payload.len()], payload.as_bytes());
        assert!(block[8 + payload.len()..].iter().all(|b| *b == 0));
    }

    #[test]
    fn exit_marker_parsing() {
        assert_eq!(parse_exit_marker(b"MYCE000"), Some(0));
        assert_eq!(parse_exit_marker(b"MYCE042"), Some(42));
        assert_eq!(parse_exit_marker(b"MYCE255\0\0"), Some(255));
        assert_eq!(parse_exit_marker(b"MYCE127"), Some(127));
        assert_eq!(parse_exit_marker(b""), None);
        assert_eq!(parse_exit_marker(b"\0\0\0\0\0\0\0"), None);
        assert_eq!(parse_exit_marker(b"MYCD042"), None); // wrong magic
        assert_eq!(parse_exit_marker(b"MYCE04x"), None); // non-digit
        assert_eq!(parse_exit_marker(b"MYCE25"), None); // short
        assert_eq!(parse_exit_marker(b"MYCE256"), None); // guest codes are 0..=255
    }

    #[test]
    fn fingerprint_is_stable_separated_and_sensitive() {
        let a = fingerprint(&[b"ab", b"c"]);
        let b = fingerprint(&[b"a", b"bc"]);
        assert_ne!(a, b, "the length prefix must separate concatenations");
        assert_eq!(a, fingerprint(&[b"ab", b"c"]));
        assert_ne!(fingerprint(&[b"x"]), fingerprint(&[b"y"]));
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn mem_parsing() {
        assert_eq!(parse_mem_mib("512M"), Some(512));
        assert_eq!(parse_mem_mib("1G"), Some(1024));
        assert_eq!(parse_mem_mib("512m"), Some(512));
        assert_eq!(parse_mem_mib(" 256 "), Some(256));
        assert_eq!(parse_mem_mib("2G"), Some(2048));
        assert_eq!(parse_mem_mib(""), None);
        assert_eq!(parse_mem_mib("abc"), None);
        assert_eq!(parse_mem_mib("1.5G"), None);
        // 32-bit overflow refuses instead of wrapping.
        assert_eq!(parse_mem_mib("9999999999G"), None);
    }

    #[test]
    fn http_response_head_parsing() {
        assert_eq!(
            parse_response_head(b"HTTP/1.1 204 \r\nContent-Length: 0\r\n"),
            Some((204, 0))
        );
        assert_eq!(
            parse_response_head(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 42\r\nX: y\r\n"),
            Some((400, 42))
        );
        assert_eq!(parse_response_head(b"garbage"), None);
        assert_eq!(parse_response_head(b"HTTP/1.1\r\n"), None);
    }

    #[test]
    fn api_error_detail_extracts_firecracker_message() {
        assert_eq!(
            api_error_detail(r#"{"error_message":"Invalid request"}"#),
            "Invalid request"
        );
        assert_eq!(api_error_detail("plain text"), "plain text");
    }

    #[test]
    fn initramfs_agent_detection() {
        let dir = tempfile::tempdir().unwrap();
        let with = dir.path().join("with.cpio");
        std::fs::write(&with, b"...mycagent=1...").unwrap();
        assert!(initramfs_has_agent(&with));
        let without = dir.path().join("without.cpio");
        std::fs::write(&without, b"mycroot=virtiofs").unwrap();
        assert!(!initramfs_has_agent(&without));
        assert!(!initramfs_has_agent(
            without.with_file_name("missing.cpio").as_path()
        ));
    }

    #[test]
    fn agent_guest_is_valid_shell_and_protocol_constants_match() {
        let out = std::process::Command::new("/bin/sh")
            .arg("-n")
            .arg("-c")
            .arg(AGENT_GUEST)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "agent init-guest is not valid shell: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        // The dd seek in the script mirrors EXIT_SECTOR (1 MiB / 512 - 1).
        assert_eq!(EXIT_SECTOR, 2047);
        assert!(AGENT_GUEST.contains("seek=2047"));
        assert!(AGENT_GUEST.contains("MYC1"));
        assert!(AGENT_GUEST.contains("MYC_EXIT_DEV"));
    }

    #[test]
    fn snapshot_lock_is_exclusive_and_self_healing() {
        let dir = tempfile::tempdir().unwrap();
        let a = SnapshotLock::acquire(dir.path()).unwrap();
        assert!(
            SnapshotLock::acquire(dir.path()).is_none(),
            "a second claim must fail while the first lives"
        );
        drop(a);
        assert!(SnapshotLock::acquire(dir.path()).is_some(), "released");
        // A stale claim (dead owner) is taken over.
        let stale = dir.path().join("lock");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("pid"), "99999999").unwrap(); // no /proc/99999999
        assert!(SnapshotLock::acquire(dir.path()).is_some());
    }

    #[test]
    fn meta_matches_protocol_and_fc_version() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!SnapshotFiles::meta_matches(dir.path(), "1.17.0"));
        SnapshotFiles::meta_write(dir.path(), "1.17.0", 512);
        assert!(SnapshotFiles::meta_matches(dir.path(), "1.17.0"));
        assert!(
            !SnapshotFiles::meta_matches(dir.path(), "1.16.0"),
            "a different firecracker cannot load this vmstate"
        );
        // A bumped agent protocol invalidates old snapshots.
        std::fs::write(
            SnapshotFiles::meta(dir.path()),
            r#"{"agent_protocol":0,"fc_version":"1.17.0"}"#,
        )
        .unwrap();
        assert!(!SnapshotFiles::meta_matches(dir.path(), "1.17.0"));
    }

    #[test]
    fn snapshot_completeness_needs_every_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!SnapshotFiles::complete(dir.path(), 16));
        std::fs::write(SnapshotFiles::vmstate(dir.path()), b"x").unwrap();
        std::fs::write(SnapshotFiles::rootfs(dir.path()), b"x").unwrap();
        std::fs::write(SnapshotFiles::scratch(dir.path()), b"x").unwrap();
        SnapshotFiles::meta_write(dir.path(), "1.17.0", 16);
        assert!(!SnapshotFiles::complete(dir.path(), 16), "mem.img missing");
        std::fs::write(
            SnapshotFiles::mem(dir.path()),
            vec![0u8; 16 * 1024 * 1024 - 1],
        )
        .unwrap();
        assert!(
            !SnapshotFiles::complete(dir.path(), 16),
            "mem.img one byte short"
        );
        std::fs::write(SnapshotFiles::mem(dir.path()), vec![0u8; 16 * 1024 * 1024]).unwrap();
        assert!(SnapshotFiles::complete(dir.path(), 16));
        SnapshotFiles::wipe(dir.path());
        assert!(!SnapshotFiles::complete(dir.path(), 16));
    }

    #[test]
    fn sha256_memo_hits_and_invalidates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("snaproot"); // isolated memo root
        let asset = dir.path().join("asset");
        std::fs::write(&asset, b"hello").unwrap();
        let h1 = file_sha256_cached_in(&root, &asset).unwrap();
        assert_eq!(h1, sha256_hex_of(b"hello"));
        // Second call is a memo hit (same digest, sidecar present).
        assert_eq!(file_sha256_cached_in(&root, &asset).unwrap(), h1);
        assert!(root.join("hashes").read_dir().unwrap().next().is_some());
        // A rebuilt asset (different size => different key) re-hashes.
        std::fs::write(&asset, b"hello-again").unwrap();
        assert_eq!(
            file_sha256_cached_in(&root, &asset).unwrap(),
            sha256_hex_of(b"hello-again")
        );
        // A corrupt sidecar (not 64 hex) is ignored, not trusted.
        let mut entries = root.join("hashes").read_dir().unwrap();
        let side = entries.next().unwrap().unwrap().path();
        std::fs::write(&side, "garbage").unwrap();
        std::fs::write(&asset, b"third-content").unwrap();
        assert_eq!(
            file_sha256_cached_in(&root, &asset).unwrap(),
            sha256_hex_of(b"third-content")
        );
    }

    #[test]
    fn scratch_write_then_exit_read() {
        let dir = tempfile::tempdir().unwrap();
        let scratch = dir.path().join("scratch.img");
        std::fs::File::create(&scratch)
            .unwrap()
            .set_len(crate::kvm::SCRATCH_SIZE)
            .unwrap();
        write_scratch(&scratch, "MYC_ARGC=0\n").unwrap();
        let block = std::fs::read(&scratch).unwrap();
        assert_eq!(&block[..4], CMD_MAGIC);
        // The sentinel resets the exit sector: a stale MYCE from a previous
        // run cannot survive a new run's injection.
        assert_eq!(read_exit_marker(&scratch), None);
        let mut sector = [0u8; 512];
        sector[..7].copy_from_slice(b"MYCE042");
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(&scratch)
            .unwrap();
        f.seek(SeekFrom::Start(EXIT_SECTOR * 512)).unwrap();
        f.write_all(&sector).unwrap();
        drop(f);
        assert_eq!(read_exit_marker(&scratch), Some(42));
        // And the sentinel itself is not an exit code.
        sector[..4].copy_from_slice(b"MYCR");
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(&scratch)
            .unwrap();
        f.seek(SeekFrom::Start(EXIT_SECTOR * 512)).unwrap();
        f.write_all(&sector).unwrap();
        drop(f);
        assert_eq!(read_exit_marker(&scratch), None);
    }
}
