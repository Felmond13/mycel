//! Rootless container execution on Linux.
//!
//! Uses unprivileged user namespaces — the same mechanism as Podman and
//! rootless Docker, but with no daemon and no setuid helper:
//!
//! 1. fork: the child is single-threaded, a requirement for unshare(NEWUSER)
//! 2. unshare(USER | MOUNT | PID | UTS | IPC), self-map uid/gid -> 0
//! 3. fork again: the grandchild is PID 1 of the new PID namespace
//! 4. grandchild: private mounts, /proc, minimal /dev, bind mounts,
//!    pivot_root, exec

use crate::materialize::materialize;
use crate::{Result, RunError, RunOptions};
use myc_manifest::Manifest;
use myc_store::Store;
use nix::mount::{mount, umount2, MntFlags, MsFlags};
use nix::sched::{unshare, CloneFlags};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{chdir, execvpe, fork, pivot_root, sethostname, ForkResult, Pid};
use std::ffi::CString;
use std::fs;
use std::io::{Read as _, Write};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

fn setup_err(context: &str, e: impl std::fmt::Display) -> RunError {
    RunError::Setup(format!("{context}: {e}"))
}

pub fn run_linux(
    store: &Store,
    manifest: &Manifest,
    work_dir: &Path,
    options: &RunOptions,
    command: &[String],
) -> Result<i32> {
    let rootfs = work_dir.join("rootfs");
    materialize(store, manifest, &rootfs)?;
    let code = exec_rootfs(manifest, &rootfs, options, command)?;
    if !options.keep_rootfs {
        remove_rootfs(&rootfs)?;
    }
    Ok(code)
}

/// Execute `command` in an already-built rootfs (used by lazy runs, where
/// materialization happened elsewhere and the rootfs contains a live FUSE
/// mount). The caller owns the rootfs lifecycle.
pub fn exec_rootfs(
    manifest: &Manifest,
    rootfs: &Path,
    options: &RunOptions,
    command: &[String],
) -> Result<i32> {
    prepare_rootfs_extras(rootfs, options)?;

    // Isolated network: resolve the pasta binary before any fork so a
    // missing package fails with the actionable hint, not a setup error.
    let pasta = match &options.network {
        crate::Network::Isolated { .. } => Some(
            crate::pasta_path()
                .ok_or_else(|| RunError::Setup(crate::NETWORK_ISOLATION_HINT.to_string()))?,
        ),
        _ => None,
    };
    // Ports to publish: the explicit mappings, or — when none were given —
    // the manifest's exposed ports on the same host ports.
    let publish: Vec<crate::PortMap> = match &options.network {
        crate::Network::Isolated { publish } if publish.is_empty() => manifest
            .config
            .exposed_ports
            .iter()
            .map(|&p| crate::PortMap {
                host: p,
                container: p,
            })
            .collect(),
        crate::Network::Isolated { publish } => publish.clone(),
        _ => Vec::new(),
    };

    let env = build_env(manifest, options);
    let workdir = options.workdir.clone().unwrap_or_else(|| {
        if manifest.config.workdir.is_empty() {
            "/".to_string()
        } else {
            manifest.config.workdir.clone()
        }
    });

    let outer_uid = nix::unistd::getuid().as_raw();
    let outer_gid = nix::unistd::getgid().as_raw();

    // Multi-uid mapping (see module docs of `subids`): only attempted for
    // the default root mapping — an explicit --map-user keeps the exact
    // single-uid semantics it always had.
    let subids = if options.map_uid == 0 && options.map_gid == 0 {
        detect_subid_maps()
    } else {
        None
    };
    // Handshake pipes for the parent-assisted setup: the child reports
    // "unshared" upward, the parent maps uids and/or attaches pasta to
    // the new network namespace, then replies with the outcome. Needed
    // whenever there is work only the parent can do: range mapping
    // through the setuid helpers, or pasta (which must join the child's
    // namespaces from outside).
    let handshake = if subids.is_some() || pasta.is_some() {
        let ready = nix::unistd::pipe().map_err(|e| setup_err("pipe", e))?;
        let result = nix::unistd::pipe().map_err(|e| setup_err("pipe", e))?;
        Some((ready, result))
    } else {
        None
    };

    // fork so the namespace work happens in a single-threaded process.
    let exit_code = match unsafe { fork() }.map_err(|e| setup_err("fork", e))? {
        ForkResult::Parent { child } => {
            if let Some(((ready_r, ready_w), (result_r, result_w))) = handshake {
                drop(ready_w);
                drop(result_r);
                let byte = if wait_for_byte(&ready_r) {
                    // uid/gid maps must exist before pasta joins the user
                    // namespace, so map first. Without subid ranges the
                    // parent writes the single-uid map itself (an
                    // unprivileged process may map its own uid into a
                    // child user namespace).
                    let mapped = match &subids {
                        Some(maps) => apply_subid_maps(maps, child, outer_uid, outer_gid),
                        None => {
                            pasta.is_some()
                                && write_single_maps(child, options, outer_uid, outer_gid)
                        }
                    };
                    let net_ok = match &pasta {
                        Some(bin) => spawn_pasta(bin, child, &publish),
                        None => true,
                    };
                    if !net_ok {
                        RESULT_NET_FAILED
                    } else if mapped {
                        RESULT_MAPPED
                    } else {
                        RESULT_MAP_YOURSELF
                    }
                } else {
                    RESULT_MAP_YOURSELF
                };
                let _ = nix::unistd::write(&result_w, &[byte]);
            }
            let status = waitpid(child, None).map_err(|e| setup_err("waitpid", e))?;
            wait_status_to_code(status)
        }
        ForkResult::Child => {
            let child_handshake = handshake.map(|((ready_r, ready_w), (result_r, result_w))| {
                drop(ready_r);
                drop(result_w);
                (ready_w, result_r)
            });
            let code = child_main(
                rootfs,
                options,
                command,
                &env,
                &workdir,
                outer_uid,
                outer_gid,
                child_handshake,
            )
            .unwrap_or_else(|e| {
                eprintln!("myc: {e}");
                126
            });
            std::process::exit(code);
        }
    };
    Ok(exit_code)
}

/// Runs in the forked (single-threaded) child. Creates the namespaces and
/// forks the container init.
#[allow(clippy::too_many_arguments)]
fn child_main(
    rootfs: &Path,
    options: &RunOptions,
    command: &[String],
    env: &[String],
    workdir: &str,
    outer_uid: u32,
    outer_gid: u32,
    handshake: Option<(OwnedFd, OwnedFd)>,
) -> Result<i32> {
    let mut flags = CloneFlags::CLONE_NEWUSER
        | CloneFlags::CLONE_NEWNS
        | CloneFlags::CLONE_NEWPID
        | CloneFlags::CLONE_NEWUTS
        | CloneFlags::CLONE_NEWIPC;
    if options.network != crate::Network::Host {
        flags |= CloneFlags::CLONE_NEWNET;
    }
    unshare(flags).map_err(|e| {
        setup_err(
            "unshare (are unprivileged user namespaces enabled? check /proc/sys/kernel/unprivileged_userns_clone)",
            e,
        )
    })?;

    // Preferred path: the parent maps a whole uid/gid range through the
    // setuid newuidmap/newgidmap helpers, so uids other than root exist in
    // the container (USER directives, chown, su all work). setgroups stays
    // "allow" — the privileged helpers permit it. The same handshake also
    // covers pasta: the parent attaches it to our fresh network namespace
    // and reports failure, which is fatal (an isolated container without
    // its network tool would silently have no connectivity).
    let mapped_by_parent = match handshake {
        Some((ready_w, result_r)) => {
            let _ = nix::unistd::write(&ready_w, b"r");
            drop(ready_w);
            match wait_for_byte_value(&result_r) {
                Some(RESULT_MAPPED) => true,
                Some(RESULT_NET_FAILED) => {
                    return Err(RunError::Setup(
                        "network isolation failed: pasta could not attach to the container \
                         (see its error above — a busy host port is the usual cause)"
                            .to_string(),
                    ));
                }
                _ => false,
            }
        }
        None => false,
    };

    if !mapped_by_parent {
        // Fallback: map exactly one uid/gid ourselves (no helper needed).
        // The container then has a single user: the requested uid (root by
        // default). As the namespace creator we hold every capability in
        // it regardless of the mapped uid, so mounts/pivot_root still work.
        // Tolerate a partially successful parent mapping: each of these
        // files can only ever be written once.
        let uid_mapped = proc_map_nonempty("/proc/self/uid_map");
        let gid_mapped = proc_map_nonempty("/proc/self/gid_map");
        if !gid_mapped {
            write_file("/proc/self/setgroups", "deny")?;
        }
        if !uid_mapped {
            write_file(
                "/proc/self/uid_map",
                &format!("{} {outer_uid} 1", options.map_uid),
            )?;
        }
        if !gid_mapped {
            write_file(
                "/proc/self/gid_map",
                &format!("{} {outer_gid} 1", options.map_gid),
            )?;
        }
    }

    // Second fork: the grandchild becomes PID 1 of the new PID namespace.
    match unsafe { fork() }.map_err(|e| setup_err("fork(init)", e))? {
        ForkResult::Parent { child } => {
            let status = waitpid(child, None).map_err(|e| setup_err("waitpid(init)", e))?;
            Ok(wait_status_to_code(status))
        }
        ForkResult::Child => {
            let e = container_init(rootfs, options, command, env, workdir);
            // Only reached on error: exec replaces the process image.
            eprintln!("myc: {}", e.unwrap_err());
            std::process::exit(126);
        }
    }
}

/// PID 1 inside the container: mounts, pivot_root, exec. Never returns Ok.
fn container_init(
    rootfs: &Path,
    options: &RunOptions,
    command: &[String],
    env: &[String],
    workdir: &str,
) -> Result<std::convert::Infallible> {
    // Stop mount events from propagating to the host.
    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None::<&str>,
    )
    .map_err(|e| setup_err("make / rprivate", e))?;

    // The new root must be a mount point for pivot_root.
    mount(
        Some(rootfs),
        rootfs,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .map_err(|e| setup_err("bind rootfs", e))?;

    setup_dev(rootfs)?;
    setup_proc(rootfs)?;

    // User-requested bind mounts.
    for (host, inside, read_only) in &options.binds {
        let target = rootfs.join(inside.trim_start_matches('/'));
        if host.is_dir() {
            fs::create_dir_all(&target)
                .map_err(|e| setup_err(&format!("mkdir {}", target.display()), e))?;
        } else {
            if let Some(p) = target.parent() {
                fs::create_dir_all(p)
                    .map_err(|e| setup_err(&format!("mkdir {}", p.display()), e))?;
            }
            if !target.exists() {
                fs::File::create(&target)
                    .map_err(|e| setup_err(&format!("touch {}", target.display()), e))?;
            }
        }
        mount(
            Some(host.as_path()),
            &target,
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REC,
            None::<&str>,
        )
        .map_err(|e| setup_err(&format!("bind {}", host.display()), e))?;
        if *read_only {
            // A read-only remount in a user namespace must preserve the
            // source mount's existing flags (nosuid, nodev, ...), otherwise
            // the kernel refuses with EPERM.
            let existing = statvfs_mount_flags(&target)?;
            mount(
                None::<&str>,
                &target,
                None::<&str>,
                MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY | existing,
                None::<&str>,
            )
            .map_err(|e| setup_err("remount ro", e))?;
        }
    }

    // pivot_root into the new rootfs.
    let old_root = rootfs.join(".oldroot");
    fs::create_dir_all(&old_root).map_err(|e| setup_err("mkdir .oldroot", e))?;
    pivot_root(rootfs, &old_root).map_err(|e| setup_err("pivot_root", e))?;
    chdir("/").map_err(|e| setup_err("chdir /", e))?;
    umount2("/.oldroot", MntFlags::MNT_DETACH).map_err(|e| setup_err("umount old root", e))?;
    let _ = fs::remove_dir("/.oldroot");

    sethostname(&options.hostname).map_err(|e| setup_err("sethostname", e))?;

    // A fresh network namespace starts with loopback down — even
    // 127.0.0.1 is unreachable until someone brings it up. We own the
    // namespace, so no privileges are needed.
    if options.network != crate::Network::Host {
        set_loopback_up()?;
    }

    let wd = if workdir.is_empty() { "/" } else { workdir };
    fs::create_dir_all(wd).map_err(|e| setup_err(&format!("mkdir workdir {wd}"), e))?;
    chdir(wd).map_err(|e| setup_err(&format!("chdir {wd}"), e))?;

    // exec: resolves through the container's PATH (we chdir'd + pivoted).
    let argv: Vec<CString> = command
        .iter()
        .map(|s| CString::new(s.as_str()).unwrap())
        .collect();
    let envp: Vec<CString> = env
        .iter()
        .map(|s| CString::new(s.as_str()).unwrap())
        .collect();
    execvpe(&argv[0], &argv, &envp).map_err(|e| setup_err(&format!("exec {:?}", command[0]), e))?;
    unreachable!()
}

/// Translate a mount's current statvfs flags into MsFlags for remounting.
fn statvfs_mount_flags(path: &Path) -> Result<MsFlags> {
    use nix::sys::statvfs::{statvfs, FsFlags};
    let st = statvfs(path).map_err(|e| setup_err("statvfs", e))?;
    let f = st.flags();
    let mut out = MsFlags::empty();
    if f.contains(FsFlags::ST_NOSUID) {
        out |= MsFlags::MS_NOSUID;
    }
    if f.contains(FsFlags::ST_NODEV) {
        out |= MsFlags::MS_NODEV;
    }
    if f.contains(FsFlags::ST_NOEXEC) {
        out |= MsFlags::MS_NOEXEC;
    }
    if f.contains(FsFlags::ST_NOATIME) {
        out |= MsFlags::MS_NOATIME;
    }
    if f.contains(FsFlags::ST_NODIRATIME) {
        out |= MsFlags::MS_NODIRATIME;
    }
    if f.contains(FsFlags::ST_RELATIME) {
        out |= MsFlags::MS_RELATIME;
    }
    Ok(out)
}

/// tmpfs /dev with the standard node set, bind-mounted from the host
/// (mknod is not permitted in an unprivileged user namespace).
fn setup_dev(rootfs: &Path) -> Result<()> {
    let dev = rootfs.join("dev");
    fs::create_dir_all(&dev).map_err(|e| setup_err("mkdir /dev", e))?;
    mount(
        Some("tmpfs"),
        &dev,
        Some("tmpfs"),
        MsFlags::MS_NOSUID | MsFlags::MS_STRICTATIME,
        Some("mode=755,size=65536k"),
    )
    .map_err(|e| setup_err("mount /dev tmpfs", e))?;

    for node in ["null", "zero", "full", "random", "urandom", "tty"] {
        let host = PathBuf::from("/dev").join(node);
        if !host.exists() {
            continue;
        }
        let target = dev.join(node);
        fs::File::create(&target).map_err(|e| setup_err(&format!("touch /dev/{node}"), e))?;
        mount(
            Some(&host),
            &target,
            None::<&str>,
            MsFlags::MS_BIND,
            None::<&str>,
        )
        .map_err(|e| setup_err(&format!("bind /dev/{node}"), e))?;
    }

    // pts + shm
    let pts = dev.join("pts");
    fs::create_dir_all(&pts).map_err(|e| setup_err("mkdir /dev/pts", e))?;
    mount(
        Some("devpts"),
        &pts,
        Some("devpts"),
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
        Some("newinstance,ptmxmode=0666,mode=0620"),
    )
    .map_err(|e| setup_err("mount devpts", e))?;
    let shm = dev.join("shm");
    fs::create_dir_all(&shm).map_err(|e| setup_err("mkdir /dev/shm", e))?;
    mount(
        Some("tmpfs"),
        &shm,
        Some("tmpfs"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        Some("mode=1777,size=65536k"),
    )
    .map_err(|e| setup_err("mount /dev/shm", e))?;

    // Standard symlinks.
    let links = [
        ("ptmx", "pts/ptmx"),
        ("fd", "/proc/self/fd"),
        ("stdin", "/proc/self/fd/0"),
        ("stdout", "/proc/self/fd/1"),
        ("stderr", "/proc/self/fd/2"),
    ];
    for (name, target) in links {
        let _ = std::os::unix::fs::symlink(target, dev.join(name));
    }
    Ok(())
}

fn setup_proc(rootfs: &Path) -> Result<()> {
    let proc = rootfs.join("proc");
    fs::create_dir_all(&proc).map_err(|e| setup_err("mkdir /proc", e))?;
    mount(
        Some("proc"),
        &proc,
        Some("proc"),
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC | MsFlags::MS_NODEV,
        None::<&str>,
    )
    .map_err(|e| setup_err("mount /proc", e))?;
    Ok(())
}

/// DNS/hosts: containers expect these files to exist.
fn prepare_rootfs_extras(rootfs: &Path, options: &RunOptions) -> Result<()> {
    let etc = rootfs.join("etc");
    fs::create_dir_all(&etc).map_err(|e| setup_err("mkdir /etc", e))?;
    let resolv: Option<Vec<u8>> = match &options.network {
        // Host network: the host's resolvers work as-is.
        crate::Network::Host => fs::read("/etc/resolv.conf").ok(),
        // Isolated: pasta intercepts queries to the forward address and
        // relays them to the host's first resolver.
        crate::Network::Isolated { .. } => {
            Some(format!("nameserver {PASTA_DNS_FORWARD}\n").into_bytes())
        }
        // No connectivity, no resolver.
        crate::Network::Loopback => None,
    };
    if let Some(content) = resolv {
        let dest = etc.join("resolv.conf");
        let _ = fs::remove_file(&dest); // may be a dangling symlink
        fs::write(&dest, content).map_err(|e| setup_err("write resolv.conf", e))?;
    }
    let hosts = etc.join("hosts");
    if !hosts.exists() {
        let _ = fs::write(
            &hosts,
            format!("127.0.0.1 localhost\n127.0.1.1 {}\n", options.hostname),
        );
    }
    // tmp must exist and be world-writable.
    let tmp = rootfs.join("tmp");
    let _ = fs::create_dir_all(&tmp);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o1777));
    }
    Ok(())
}

fn build_env(manifest: &Manifest, options: &RunOptions) -> Vec<String> {
    let mut env: Vec<String> = Vec::new();
    let push = |kv: &str, env: &mut Vec<String>| {
        if let Some((k, _)) = kv.split_once('=') {
            env.retain(|e| e.split_once('=').map(|(ek, _)| ek) != Some(k));
            env.push(kv.to_string());
        }
    };
    for kv in &manifest.config.env {
        push(kv, &mut env);
    }
    for kv in &options.env {
        push(kv, &mut env);
    }
    let has = |k: &str, env: &[String]| env.iter().any(|e| e.starts_with(&format!("{k}=")));
    if !has("PATH", &env) {
        env.push("PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into());
    }
    if !has("HOME", &env) {
        env.push("HOME=/root".into());
    }
    if !has("TERM", &env) {
        if let Ok(term) = std::env::var("TERM") {
            env.push(format!("TERM={term}"));
        }
    }
    env.push(format!("HOSTNAME={}", options.hostname));
    env
}

// ---- isolated network via pasta ----
//
// An isolated container gets its own network namespace (CLONE_NEWNET) and
// pasta — the user-mode networking tool from the `passt` package, the
// same one rootless Podman uses — provides connectivity from outside the
// container: it joins the namespace, creates a tap device (--config-net
// copies the host's addressing onto it), NATs outbound traffic through
// ordinary unprivileged sockets, and binds each published host port,
// splicing connections through to the container. No privileges, no
// daemon; pasta exits by itself when the namespace dies.

/// Address the container's /etc/resolv.conf points at; pasta intercepts
/// queries to it and relays them to the host's first resolver (the same
/// convention Podman uses).
const PASTA_DNS_FORWARD: &str = "169.254.1.1";

/// Handshake result bytes (parent -> child).
const RESULT_MAPPED: u8 = b'o';
const RESULT_MAP_YOURSELF: u8 = b'f';
const RESULT_NET_FAILED: u8 = b'x';

/// Command line for attaching pasta to the namespaces of `child`.
/// Published ports are TCP (the protocol OCI ExposedPorts overwhelmingly
/// declare); an empty list opens nothing (outbound-only container).
fn pasta_args(child: i32, publish: &[crate::PortMap]) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--config-net".into(),
        "--quiet".into(),
        "--dns-forward".into(),
        PASTA_DNS_FORWARD.into(),
        "--userns".into(),
        format!("/proc/{child}/ns/user"),
        "--netns".into(),
        format!("/proc/{child}/ns/net"),
        "-u".into(),
        "none".into(),
        // No namespace-side forwards: pasta's default (`-T auto`) binds
        // every host-bound port *inside* the namespace to splice
        // guest-to-host loopback traffic — which would steal the very
        // port the app is about to bind (observed with a second redis:
        // host 6379 busy → pasta bound 6379 in the namespace → redis
        // died with "Address in use"). Host services stay reachable from
        // the container through the gateway address.
        "-T".into(),
        "none".into(),
        "-U".into(),
        "none".into(),
    ];
    if publish.is_empty() {
        args.push("-t".into());
        args.push("none".into());
    } else {
        for map in publish {
            args.push("-t".into());
            args.push(format!("{}:{}", map.host, map.container));
        }
    }
    args
}

/// Parent side: attach pasta to the child's fresh namespaces. pasta
/// daemonizes only after the tap device and all port bindings are ready,
/// so a successful exit status means the network is usable; it exits by
/// itself when the namespace disappears. stderr is inherited so bind
/// errors reach the user's terminal / container log.
fn spawn_pasta(pasta: &Path, child: Pid, publish: &[crate::PortMap]) -> bool {
    std::process::Command::new(pasta)
        .args(pasta_args(child.as_raw(), publish))
        .stdin(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Parent side, no-subid fallback: write the child's single-uid map from
/// outside (an unprivileged process may always map its own uid/gid into a
/// child user namespace it created). Needed before pasta joins the user
/// namespace, so pasta runs with a mapped identity.
fn write_single_maps(child: Pid, options: &RunOptions, outer_uid: u32, outer_gid: u32) -> bool {
    let pid = child.as_raw();
    let write = |path: String, content: String| -> bool {
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .and_then(|mut f| f.write_all(content.as_bytes()))
            .is_ok()
    };
    write(format!("/proc/{pid}/setgroups"), "deny".to_string())
        && write(
            format!("/proc/{pid}/uid_map"),
            format!("{} {outer_uid} 1", options.map_uid),
        )
        && write(
            format!("/proc/{pid}/gid_map"),
            format!("{} {outer_gid} 1", options.map_gid),
        )
}

/// Bring `lo` up inside the (already joined) network namespace, via the
/// classic SIOCGIFFLAGS/SIOCSIFFLAGS ioctls — no netlink dependency.
fn set_loopback_up() -> Result<()> {
    let err = |ctx: &str| setup_err(ctx, std::io::Error::last_os_error());
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if fd < 0 {
            return Err(err("socket (lo up)"));
        }
        let mut ifr: libc::ifreq = std::mem::zeroed();
        for (i, b) in b"lo".iter().enumerate() {
            ifr.ifr_name[i] = *b as libc::c_char;
        }
        if libc::ioctl(fd, libc::SIOCGIFFLAGS, &mut ifr) < 0 {
            libc::close(fd);
            return Err(err("SIOCGIFFLAGS lo"));
        }
        ifr.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
        if libc::ioctl(fd, libc::SIOCSIFFLAGS, &ifr) < 0 {
            libc::close(fd);
            return Err(err("SIOCSIFFLAGS lo"));
        }
        libc::close(fd);
    }
    Ok(())
}

// ---- subuid/subgid range mapping ----
//
// A user namespace whose map is written by the process itself can only
// contain ONE uid — everything else appears as `nobody`, chown to any
// other uid fails with EINVAL, and setuid/setgroups (su, USER directives)
// fail with EPERM. Official images that switch to a service user
// (postgres, mongo, …) break on that.
//
// The way out — the same one Podman and rootless Docker use — is the pair
// of setuid helpers `newuidmap`/`newgidmap` (package `uidmap` on
// Debian/Ubuntu), which may write *ranges* delegated to the user in
// /etc/subuid and /etc/subgid. When both the helpers and the ranges are
// present, we map:
//
//   container 0        -> the invoking user   (so files it creates on
//                         volumes stay owned — and readable — by you)
//   container 1..count -> the subordinate range
//
// and every uid an image cares about actually exists. When they are not,
// we fall back to the single-uid map and the dashboard explains failures
// caused by it (see `myc-web`'s failure hints).
//
// Set MYCEL_SINGLE_UID=1 to force the single-uid fallback.

/// Everything needed for a parent-assisted multi-uid mapping.
struct SubIdMaps {
    newuidmap: PathBuf,
    newgidmap: PathBuf,
    uid_start: u32,
    uid_count: u32,
    gid_start: u32,
    gid_count: u32,
}

/// Cap on mapped range length: 65536 uids covers every real image and
/// keeps the mapping identical across machines with larger delegations.
const SUBID_MAX: u32 = 65536;

fn find_helper(name: &str) -> Option<PathBuf> {
    ["/usr/bin", "/bin", "/usr/sbin", "/sbin", "/usr/local/bin"]
        .iter()
        .map(|dir| Path::new(dir).join(name))
        .find(|p| p.exists())
}

/// First `/etc/subuid`-style range delegated to `user` (by name or numeric
/// uid). Lines look like `alice:100000:65536`.
fn parse_subid_range(content: &str, user: &str, uid: u32) -> Option<(u32, u32)> {
    let uid_str = uid.to_string();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split(':');
        let (name, start, count) = (fields.next()?, fields.next()?, fields.next()?);
        if name != user && name != uid_str {
            continue;
        }
        if let (Ok(start), Ok(count)) = (start.parse::<u32>(), count.parse::<u32>()) {
            if count >= 1 {
                return Some((start, count));
            }
        }
    }
    None
}

/// Are the helpers and delegated ranges available for the current user?
fn detect_subid_maps() -> Option<SubIdMaps> {
    if std::env::var_os("MYCEL_SINGLE_UID").is_some_and(|v| v == "1") {
        return None;
    }
    let newuidmap = find_helper("newuidmap")?;
    let newgidmap = find_helper("newgidmap")?;
    let uid = nix::unistd::getuid().as_raw();
    let user = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
        .ok()
        .flatten()
        .map(|u| u.name)
        .unwrap_or_default();
    let subuid = fs::read_to_string("/etc/subuid").ok()?;
    let subgid = fs::read_to_string("/etc/subgid").ok()?;
    let (uid_start, uid_count) = parse_subid_range(&subuid, &user, uid)?;
    let (gid_start, gid_count) = parse_subid_range(&subgid, &user, uid)?;
    Some(SubIdMaps {
        newuidmap,
        newgidmap,
        uid_start,
        uid_count,
        gid_start,
        gid_count,
    })
}

/// Parent side: write the child's uid_map/gid_map through the setuid
/// helpers. Returns false on any failure (the child then self-maps).
fn apply_subid_maps(maps: &SubIdMaps, child: Pid, outer_uid: u32, outer_gid: u32) -> bool {
    let run = |helper: &Path, outer: u32, start: u32, count: u32| -> bool {
        std::process::Command::new(helper)
            .arg(child.as_raw().to_string())
            .args(["0", &outer.to_string(), "1"])
            .args(["1", &start.to_string(), &count.min(SUBID_MAX).to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    run(&maps.newuidmap, outer_uid, maps.uid_start, maps.uid_count)
        && run(&maps.newgidmap, outer_gid, maps.gid_start, maps.gid_count)
}

/// Block until one byte arrives (true) or the peer closes the pipe (false).
fn wait_for_byte(fd: &OwnedFd) -> bool {
    wait_for_byte_value(fd).is_some()
}

fn wait_for_byte_value(fd: &OwnedFd) -> Option<u8> {
    let mut file = fs::File::from(fd.try_clone().ok()?);
    let mut buf = [0u8; 1];
    match file.read(&mut buf) {
        Ok(1) => Some(buf[0]),
        _ => None,
    }
}

/// Has this map file already been written? (Each can be written only once;
/// an unwritten map reads back empty.)
fn proc_map_nonempty(path: &str) -> bool {
    fs::read_to_string(path)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

fn wait_status_to_code(status: WaitStatus) -> i32 {
    match status {
        WaitStatus::Exited(_, code) => code,
        WaitStatus::Signaled(_, sig, _) => 128 + sig as i32,
        _ => 1,
    }
}

fn write_file(path: &str, content: &str) -> Result<()> {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|e| setup_err(&format!("open {path}"), e))?;
    f.write_all(content.as_bytes())
        .map_err(|e| setup_err(&format!("write {path}"), e))?;
    Ok(())
}

/// Remove a materialized rootfs. Directory modes from the image may lack
/// write permission, so chmod parents on the way.
pub fn remove_rootfs(rootfs: &Path) -> Result<()> {
    if !rootfs.exists() {
        return Ok(());
    }
    make_tree_removable(rootfs);
    fs::remove_dir_all(rootfs).map_err(|e| RunError::Io {
        path: rootfs.to_path_buf(),
        source: e,
    })
}

fn make_tree_removable(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() && !p.is_symlink() {
                make_tree_removable(&p);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_subid_range, pasta_args};

    #[test]
    fn pasta_cmdline_publishes_tcp_ports() {
        let args = pasta_args(
            1234,
            &[
                crate::PortMap {
                    host: 6380,
                    container: 6379,
                },
                crate::PortMap {
                    host: 8080,
                    container: 80,
                },
            ],
        );
        let joined = args.join(" ");
        assert!(joined.contains("--config-net"));
        assert!(joined.contains("--userns /proc/1234/ns/user"));
        assert!(joined.contains("--netns /proc/1234/ns/net"));
        assert!(joined.contains("-t 6380:6379"));
        assert!(joined.contains("-t 8080:80"));
        assert!(joined.contains("-u none"), "no UDP forwards: {joined}");
        assert!(
            joined.contains("-T none") && joined.contains("-U none"),
            "namespace-side forwards must be off: {joined}"
        );
        assert!(joined.contains("--dns-forward 169.254.1.1"));
    }

    #[test]
    fn pasta_cmdline_without_ports_forwards_nothing() {
        let joined = pasta_args(7, &[]).join(" ");
        assert!(joined.contains("-t none"));
        assert!(joined.contains("-u none"));
    }

    #[test]
    fn subid_lines_are_parsed_by_name_or_uid() {
        let content = "# comment\n\nroot:0:1\nalice:100000:65536\n1001:200000:65536\n";
        assert_eq!(
            parse_subid_range(content, "alice", 1000),
            Some((100000, 65536))
        );
        // Numeric-uid entries match too (some distros write them that way).
        assert_eq!(
            parse_subid_range(content, "bob", 1001),
            Some((200000, 65536))
        );
        assert_eq!(parse_subid_range(content, "carol", 1002), None);
        assert_eq!(parse_subid_range("", "alice", 1000), None);
        assert_eq!(parse_subid_range("garbage line\n", "alice", 1000), None);
        assert_eq!(parse_subid_range("alice:xx:yy\n", "alice", 1000), None);
    }
}
