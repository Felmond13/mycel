//! Pod holders: one private network for a whole stack.
//!
//! A *pod* gives every service of a `mycel.toml` stack the same private
//! network: services talk to each other on localhost, and only the ports
//! listed in the project file are published to the host. The moving parts:
//!
//! * The **holder** is a tiny durable process (`myc _pod-holder`) that owns
//!   the pod's user + network namespaces. It unshares them, waits for its
//!   parent to write the uid/gid maps and attach pasta (the same
//!   parent-assisted handshake single containers use — the maps must exist
//!   before pasta joins the user namespace), brings loopback up, then
//!   sleeps forever. Its only job is to keep the namespaces alive between
//!   service restarts.
//! * **One pasta instance** per stack, attached to the holder's namespaces
//!   with `-t HOST:CONTAINER` for each published port (and `-T none -U
//!   none`, so it never binds ports *inside* the pod — the real trap: with
//!   the default `-T auto` pasta steals the very ports the services are
//!   about to bind). pasta exits by itself when the namespaces die.
//! * Each service container **joins** the holder with
//!   [`crate::Network::Join`]: `setns` into the holder's user namespace
//!   (which grants the full capability set there — that is what makes the
//!   rest work rootless), then into its network namespace, then unshares
//!   the usual mount/PID/UTS/IPC namespaces of its own.
//!
//! Lifecycle: the holder dies (a) when it is killed explicitly (`myc down`,
//! the dashboard's "Stop all"), or (b) for CLI runs, automatically with the
//! `myc up` process that spawned it (`PR_SET_PDEATHSIG`), so even a
//! SIGKILLed `up` leaves nothing behind. The namespaces themselves — and
//! pasta — survive exactly as long as any process is still inside them.
//!
//! A small state file under `<store>/pods/<name>.json` records the holder's
//! pid + kernel start time (so a recycled pid is never mistaken for a live
//! holder) and the published ports; it lets `myc down` and the dashboard
//! find or clean up holders across processes and restarts.

use crate::{PortMap, Result, RunError};
use std::path::{Path, PathBuf};

/// The persisted identity of a stack's pod holder.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PodRecord {
    pub pid: i32,
    /// Kernel start time of `pid` (`/proc/<pid>/stat` field 22) — pid +
    /// start time identify a process across restarts; 0 = unreadable.
    #[serde(default)]
    pub starttime: u64,
    #[serde(default)]
    pub ports: Vec<PortMap>,
}

/// Directory holding one state file per pod.
pub fn state_dir(store_root: &Path) -> PathBuf {
    store_root.join("pods")
}

/// `<store>/pods/<name>.json`, with `name` sanitized so an arbitrary
/// project name can never traverse out of the pods directory.
pub fn state_path(store_root: &Path, name: &str) -> PathBuf {
    let safe: String = name
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .take(64)
        .collect();
    let safe = if safe.is_empty() {
        "mycel".to_string()
    } else {
        safe
    };
    state_dir(store_root).join(format!("{safe}.json"))
}

pub fn load(store_root: &Path, name: &str) -> Option<PodRecord> {
    let bytes = std::fs::read(state_path(store_root, name)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn save(store_root: &Path, name: &str, record: &PodRecord) -> Result<()> {
    let path = state_path(store_root, name);
    let io_err = |e: std::io::Error| RunError::Io {
        path: path.clone(),
        source: e,
    };
    std::fs::create_dir_all(state_dir(store_root)).map_err(io_err)?;
    let bytes = serde_json::to_vec_pretty(record).expect("pod record serializes");
    std::fs::write(&path, bytes).map_err(io_err)
}

pub fn remove(store_root: &Path, name: &str) {
    let _ = std::fs::remove_file(state_path(store_root, name));
}

/// Is the recorded holder still the process it was when recorded?
pub fn alive(record: &PodRecord) -> bool {
    match proc_starttime(record.pid) {
        Some(current) => record.starttime == 0 || current == record.starttime,
        None => false,
    }
}

/// Kernel start time of a process (`/proc/<pid>/stat` field 22).
#[cfg(target_os = "linux")]
pub fn proc_starttime(pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm (field 2) may contain spaces/parens; parse after the last ')'.
    let rest = stat.rsplit_once(')')?.1;
    rest.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(not(target_os = "linux"))]
pub fn proc_starttime(_pid: i32) -> Option<u64> {
    None
}

/// A freshly spawned holder. The caller decides what to do with `child`:
/// keep it and `kill()`+`wait()` at the end (CLI), or hand it to a reaper
/// thread and manage the holder by pid across restarts (dashboard).
pub struct HolderChild {
    pub child: std::process::Child,
    pub record: PodRecord,
}

/// Start a pod holder: spawn `exe _pod-holder`, write its uid/gid maps
/// (subid ranges through newuidmap/newgidmap when available, single-uid
/// otherwise), attach one pasta instance publishing `ports`, and wait until
/// the holder reports its loopback is up.
///
/// Fails with the standard install hint when pasta is missing, and with a
/// busy-port explanation when pasta cannot bind a published port (pasta
/// only daemonizes after every bind succeeded, so its exit status is
/// trustworthy).
#[cfg(target_os = "linux")]
pub fn spawn_holder(exe: &Path, ports: &[PortMap], die_with_parent: bool) -> Result<HolderChild> {
    use std::io::{BufRead, BufReader, Write};

    let pasta = crate::pasta_path()
        .ok_or_else(|| RunError::Setup(crate::NETWORK_ISOLATION_HINT.to_string()))?;

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("_pod-holder");
    if die_with_parent {
        cmd.arg("--die-with-parent");
    }
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit());
    let mut child = cmd
        .spawn()
        .map_err(|e| RunError::Setup(format!("cannot start the pod holder: {e}")))?;
    let pid = child.id() as i32;

    let mut stdin = child.stdin.take().expect("stdin piped");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout piped"));
    let mut fail = |msg: String| -> RunError {
        let _ = child.kill();
        let _ = child.wait();
        RunError::Setup(msg)
    };

    let mut line = String::new();
    let _ = stdout.read_line(&mut line);
    if line.trim() != "ready" {
        return Err(fail(
            "the pod holder did not come up (see its error above)".to_string(),
        ));
    }

    // uid/gid maps first — pasta joins the user namespace and needs a
    // mapped identity. Same order and same helpers as single containers.
    if !crate::linux::map_child_ids(nix::unistd::Pid::from_raw(pid)) {
        return Err(fail(
            "cannot write the pod's uid/gid maps (is /proc mounted normally?)".to_string(),
        ));
    }

    // One pasta for the whole stack, publishing the project's ports.
    let status = std::process::Command::new(&pasta)
        .args(crate::linux::pasta_args(pid, ports))
        .stdin(std::process::Stdio::null())
        .status();
    if !status.map(|s| s.success()).unwrap_or(false) {
        let _ = stdin.write_all(b"fail\n");
        return Err(fail(
            "the stack's ports could not be published (see the pasta error \
             above — a busy host port is the usual cause)"
                .to_string(),
        ));
    }

    let _ = stdin.write_all(b"ok\n");
    let _ = stdin.flush();
    line.clear();
    let _ = stdout.read_line(&mut line);
    if line.trim() != "up" {
        return Err(fail(
            "the pod holder failed to bring its network up (see its error above)".to_string(),
        ));
    }

    let record = PodRecord {
        pid,
        starttime: proc_starttime(pid).unwrap_or(0),
        ports: ports.to_vec(),
    };
    Ok(HolderChild { child, record })
}

#[cfg(not(target_os = "linux"))]
pub fn spawn_holder(_exe: &Path, _ports: &[PortMap], _die: bool) -> Result<HolderChild> {
    Err(RunError::UnsupportedPlatform)
}

/// The holder's own body (`myc _pod-holder`), run in a fresh single-threaded
/// process. Protocol on stdio: print `ready` after unsharing, wait for
/// `ok`/`fail` from the parent (maps written, pasta attached), bring
/// loopback up, print `up`, then sleep forever. Never returns 0 — the
/// holder only exits by being killed.
#[cfg(target_os = "linux")]
pub fn holder_main(die_with_parent: bool) -> i32 {
    use std::io::{BufRead, Write};

    if die_with_parent {
        // Die when the process that spawned us dies (whatever the reason —
        // this is what keeps a SIGKILLed `myc up` from leaking holders).
        unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) };
        // Race guard: the parent may already be gone.
        if nix::unistd::getppid().as_raw() == 1 {
            return 1;
        }
    }

    if let Err(e) = nix::sched::unshare(
        nix::sched::CloneFlags::CLONE_NEWUSER | nix::sched::CloneFlags::CLONE_NEWNET,
    ) {
        eprintln!("myc: pod holder: unshare(user+net): {e}");
        return 1;
    }

    println!("ready");
    let _ = std::io::stdout().flush();

    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    if line.trim() != "ok" {
        return 1; // parent gave up (pasta failed) or died
    }

    // A fresh network namespace starts with lo down; the services that join
    // later rely on the holder having brought it up.
    if let Err(e) = crate::linux::set_loopback_up() {
        eprintln!("myc: pod holder: {e}");
        return 1;
    }

    println!("up");
    let _ = std::io::stdout().flush();

    loop {
        unsafe { libc::pause() };
    }
}

#[cfg(not(target_os = "linux"))]
pub fn holder_main(_die_with_parent: bool) -> i32 {
    eprintln!("myc: pod holders require Linux");
    1
}

/// Kill a recorded holder if it is still that same live process, waiting
/// (bounded ~2s, then SIGKILL) until it is gone. Returns true when a live
/// holder was actually stopped. The holder is not necessarily our child;
/// whoever spawned it (or init, after adoption) reaps it.
#[cfg(target_os = "linux")]
pub fn stop_holder(record: &PodRecord) -> bool {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    if !alive(record) {
        return false;
    }
    let pid = Pid::from_raw(record.pid);
    let _ = kill(pid, Signal::SIGTERM);
    for _ in 0..20 {
        if !alive(record) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = kill(pid, Signal::SIGKILL);
    true
}

#[cfg(not(target_os = "linux"))]
pub fn stop_holder(_record: &PodRecord) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_roundtrip_and_liveness() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(load(root, "blog").is_none());

        let record = PodRecord {
            pid: 1234567, // almost certainly not running
            starttime: u64::MAX,
            ports: vec![PortMap {
                host: 8080,
                container: 80,
            }],
        };
        save(root, "blog", &record).unwrap();
        assert_eq!(load(root, "blog").unwrap(), record);
        // A dead (or recycled) pid is never reported alive.
        assert!(!alive(&record));

        // pid 1 is alive but its start time cannot match u64::MAX.
        let ghost = PodRecord {
            pid: 1,
            starttime: u64::MAX,
            ports: Vec::new(),
        };
        assert!(!alive(&ghost));

        remove(root, "blog");
        assert!(load(root, "blog").is_none());
        remove(root, "blog"); // idempotent
    }

    #[test]
    fn state_path_is_sanitized() {
        let root = Path::new("/store");
        let p = state_path(root, "../../etc/passwd");
        assert!(p.starts_with("/store/pods/"), "path was {p:?}");
        assert!(!p.to_string_lossy().contains(".."), "path was {p:?}");
        assert_eq!(state_path(root, ""), Path::new("/store/pods/mycel.json"));
        assert_eq!(
            state_path(root, "My Blog"),
            Path::new("/store/pods/my-blog.json")
        );
    }
}
