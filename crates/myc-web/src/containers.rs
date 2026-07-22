//! Container lifecycle management for the web dashboard.
//!
//! Containers started from the browser run as *child processes* of the UI
//! server: the manager re-invokes our own binary (`myc run <manifest-id> …`)
//! rather than calling `myc_run::run` in-process. Forking from a
//! multithreaded tokio server is exactly the hazard the CLI's
//! fork-single-threaded dance avoids, and subprocess isolation means a
//! crashing container can never take the dashboard down with it.
//!
//! Each run gets its own process group (so stop can signal the whole tree),
//! a caller-owned rootfs directory under `<store>/ui-runs/<id>` (cleaned up
//! by the manager even when the process is SIGKILLed) and a capped log file
//! under `<store>/ui-logs/<id>.log` fed by pipe-draining threads.
//!
//! The registry is persisted to `<store>/ui-containers.json` on every
//! change. When the server restarts it reconciles that file against
//! `/proc`: containers whose process is still alive (same pid *and* same
//! kernel start time, so a recycled pid is never mistaken for our child)
//! are re-adopted — visible, stoppable, logs still readable — and dead ones
//! are marked exited. No container ever becomes an invisible orphan.

use crate::metrics;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Hard cap for a container's log file. Once reached, further output is
/// discarded (the pipes keep being drained so the container never blocks).
const LOG_CAP_BYTES: u64 = 2 * 1024 * 1024;
/// Longest slice served by a single log poll.
const LOG_CHUNK_BYTES: u64 = 256 * 1024;
/// Grace period between SIGTERM and SIGKILL when stopping.
const STOP_GRACE: Duration = Duration::from_secs(5);
/// How often the watcher thread of an adopted container polls `/proc`.
const ADOPT_POLL: Duration = Duration::from_millis(300);
/// A non-zero exit within this many seconds of starting counts as a
/// "failed to start" and triggers log analysis for a human-readable hint.
const EARLY_FAILURE_WINDOW_SECS: u64 = 10;

/// One published port of an isolated-network container: connections to
/// `localhost:host` reach `container` inside the app's private network.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortMapping {
    pub host: u16,
    pub container: u16,
}

/// Everything needed to (re)start a container.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContainerSpec {
    /// The name the user asked for (image ref or stored ref) — display only.
    pub reference: String,
    /// Resolved manifest id; the only thing ever passed to `myc run`.
    pub manifest_id: String,
    /// Human-friendly name (also the container hostname).
    pub name: String,
    /// Command override (empty = image entrypoint/cmd).
    pub command: Vec<String>,
    /// KEY=VALUE environment variables.
    pub env: Vec<String>,
    /// Absolute `HOST:CONTAINER[:ro]` bind specs (stacks only).
    pub binds: Vec<String>,
    pub workdir: Option<String>,
    /// `UID[:GID]` mapping inside the container (`myc run --map-user`).
    pub map_user: Option<String>,
    /// User-declared service port, used for the "open in browser" link.
    /// Always host-side: when the network is isolated and remapped, this
    /// is the port that actually answers on localhost.
    pub port: Option<u16>,
    /// Run in an isolated network namespace (pasta) instead of sharing
    /// the host's network.
    #[serde(default)]
    pub isolated_network: bool,
    /// Published ports when isolated (`myc run -p host:container`).
    #[serde(default)]
    pub ports: Vec<PortMapping>,
    /// Owning stack, when started via stack orchestration.
    pub stack: Option<String>,
    /// Service name within the stack.
    pub service: Option<String>,
}

struct ContainerState {
    spec: ContainerSpec,
    pid: i32,
    /// Kernel start time of `pid` (`/proc/<pid>/stat` field 22), recorded at
    /// spawn. Together with the pid it identifies the process across UI
    /// server restarts; 0 when it could not be read.
    starttime: u64,
    started_at: u64,
    finished_at: Option<u64>,
    /// `None` while running; `Some(code)` once reaped (128+signal if killed).
    exit_code: Option<i32>,
    /// True when the container died while the server was down (or after
    /// adoption): it exited, but nobody was there to collect the real code.
    exit_unknown: bool,
    /// Human-readable explanation of an early non-zero exit (e.g. "port
    /// already in use"), derived from the log tail by the reaper.
    failure_hint: Option<String>,
    stopping: bool,
    /// True once the user asked for this container to be stopped (Stop
    /// button, restart, stack down…). A subsequent exit — SIGTERM's 143 or
    /// the SIGKILL escalation's 137 — is then a *successful* stop, not an
    /// error, and the UI must not paint it red.
    stop_requested: bool,
    log_path: PathBuf,
    run_dir: PathBuf,
}

impl ContainerState {
    fn status_str(&self) -> String {
        match self.exit_code {
            None if self.stopping => "stopping".to_string(),
            None => "running".to_string(),
            Some(_) if self.exit_unknown => "exited".to_string(),
            Some(code) => format!("exited({code})"),
        }
    }

    fn to_json(&self, id: &str) -> Value {
        json!({
            "id": id,
            "name": self.spec.name,
            "reference": self.spec.reference,
            "manifest_id": self.spec.manifest_id,
            "command": self.spec.command,
            "env": self.spec.env,
            "workdir": self.spec.workdir,
            "port": self.spec.port,
            "network": if self.spec.isolated_network { "isolated" } else { "host" },
            "ports": self.spec.ports.iter()
                .map(|m| json!({ "host": m.host, "container": m.container }))
                .collect::<Vec<_>>(),
            "stack": self.spec.stack,
            "service": self.spec.service,
            "pid": self.pid,
            "status": self.status_str(),
            "running": self.exit_code.is_none(),
            // The real code is unknowable for a container that died while
            // the server was down — report null rather than a made-up one.
            "exit_code": if self.exit_unknown { Value::Null } else { json!(self.exit_code) },
            "stopped_by_user": self.stop_requested && self.exit_code.is_some(),
            "failure_hint": self.failure_hint,
            "started_at": self.started_at,
            "finished_at": self.finished_at,
        })
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Shared sink the pipe-draining threads write into, with the byte cap.
struct LogSink {
    file: Mutex<std::fs::File>,
    written: AtomicU64,
}

impl LogSink {
    fn write_chunk(&self, chunk: &[u8]) {
        let before = self.written.fetch_add(chunk.len() as u64, Ordering::SeqCst);
        if before >= LOG_CAP_BYTES {
            return; // over cap: keep draining the pipe, drop the bytes
        }
        let mut file = self.file.lock().unwrap();
        if before + chunk.len() as u64 > LOG_CAP_BYTES {
            let keep = (LOG_CAP_BYTES - before) as usize;
            let _ = file.write_all(&chunk[..keep]);
            let _ = file.write_all(b"\n[log truncated: 2 MiB cap reached]\n");
        } else {
            let _ = file.write_all(chunk);
        }
        let _ = file.flush();
    }
}

fn drain_pipe(mut pipe: impl Read, sink: Arc<LogSink>) {
    let mut buf = [0u8; 8192];
    loop {
        match pipe.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => sink.write_chunk(&buf[..n]),
        }
    }
}

/// The shared registry: `Manager` methods and the reaper/watcher threads
/// all mutate it, and every mutation is followed by [`persist_registry`].
type Registry = Arc<Mutex<HashMap<String, ContainerState>>>;

/// One registry entry as stored in `<store>/ui-containers.json`. Log and
/// run-dir paths are derived from the id at load time, not persisted.
#[derive(Serialize, Deserialize)]
struct PersistedContainer {
    id: String,
    spec: ContainerSpec,
    pid: i32,
    #[serde(default)]
    starttime: u64,
    started_at: u64,
    finished_at: Option<u64>,
    exit_code: Option<i32>,
    #[serde(default)]
    exit_unknown: bool,
    #[serde(default)]
    failure_hint: Option<String>,
    #[serde(default)]
    stop_requested: bool,
}

/// Write the whole registry to `path` (atomically, via a temp file). Errors
/// are swallowed: persistence is best-effort and must never take a running
/// container down with it.
fn persist_registry(containers: &Registry, path: &Path) {
    let records: Vec<PersistedContainer> = {
        let map = containers.lock().unwrap();
        map.iter()
            .map(|(id, s)| PersistedContainer {
                id: id.clone(),
                spec: s.spec.clone(),
                pid: s.pid,
                starttime: s.starttime,
                started_at: s.started_at,
                finished_at: s.finished_at,
                exit_code: s.exit_code,
                exit_unknown: s.exit_unknown,
                failure_hint: s.failure_hint.clone(),
                stop_requested: s.stop_requested,
            })
            .collect()
    };
    let Ok(bytes) = serde_json::to_vec_pretty(&records) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, &bytes).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// The container registry plus everything needed to spawn runs.
pub struct Manager {
    exe: PathBuf,
    store_root: PathBuf,
    log_dir: PathBuf,
    run_root: PathBuf,
    state_path: PathBuf,
    containers: Registry,
    counter: AtomicU64,
    /// `/proc` metrics state (CPU deltas, disk cache), sampled on demand.
    sampler: Mutex<metrics::Sampler>,
}

impl Manager {
    /// `exe` is the binary re-invoked as `exe --store <root> run …` — in
    /// production `std::env::current_exe()`, in tests the built `myc`.
    ///
    /// Reloads `<store>/ui-containers.json` and reconciles it against
    /// `/proc`, so containers from a previous server run are re-adopted.
    pub fn new(exe: PathBuf, store_root: PathBuf) -> std::io::Result<Self> {
        let log_dir = store_root.join("ui-logs");
        let run_root = store_root.join("ui-runs");
        std::fs::create_dir_all(&log_dir)?;
        std::fs::create_dir_all(&run_root)?;
        let manager = Manager {
            exe,
            state_path: store_root.join("ui-containers.json"),
            store_root,
            log_dir,
            run_root,
            containers: Arc::new(Mutex::new(HashMap::new())),
            counter: AtomicU64::new(1),
            sampler: Mutex::new(metrics::Sampler::default()),
        };
        manager.reconcile();
        Ok(manager)
    }

    /// Load the persisted registry and sort every entry into one of three
    /// buckets: already exited (kept as-is), still alive (re-adopted, with
    /// a watcher thread standing in for the lost reaper) or died while the
    /// server was down (marked exited, run dir cleaned up).
    fn reconcile(&self) {
        let Ok(bytes) = std::fs::read(&self.state_path) else {
            return; // first start, or the file was removed — nothing to do
        };
        let Ok(records) = serde_json::from_slice::<Vec<PersistedContainer>>(&bytes) else {
            return; // corrupted state file: better an empty list than a panic
        };
        let mut adopted = Vec::new();
        {
            let mut map = self.containers.lock().unwrap();
            for r in records {
                let log_path = self.log_dir.join(format!("{}.log", r.id));
                let run_dir = self.run_root.join(&r.id);
                let mut state = ContainerState {
                    spec: r.spec,
                    pid: r.pid,
                    starttime: r.starttime,
                    started_at: r.started_at,
                    finished_at: r.finished_at,
                    exit_code: r.exit_code,
                    exit_unknown: r.exit_unknown,
                    failure_hint: r.failure_hint,
                    stopping: false,
                    stop_requested: r.stop_requested,
                    log_path: log_path.clone(),
                    run_dir: run_dir.clone(),
                };
                if state.exit_code.is_none() {
                    if process_alive(r.pid, r.starttime) {
                        // Still ours and still running: adopt it. Its stdout
                        // pipes died with the old server, so output from now
                        // on is lost — leave a marker in the log.
                        append_log_note(
                            &log_path,
                            "[dashboard restarted — the app kept running, \
                             but output after this point is not captured]",
                        );
                        adopted.push((r.id.clone(), r.pid, r.starttime));
                    } else {
                        // Died while nobody was watching: the exact exit
                        // code is gone for good.
                        state.exit_code = Some(-1);
                        state.exit_unknown = true;
                        state.finished_at.get_or_insert_with(now_secs);
                        remove_run_dir(&run_dir);
                    }
                }
                map.insert(r.id, state);
            }
        }
        persist_registry(&self.containers, &self.state_path);
        for (id, pid, starttime) in adopted {
            self.spawn_adopted_watcher(id, pid, starttime);
        }
    }

    /// Stand-in for the reaper thread of a container we did not spawn
    /// ourselves: poll `/proc` until the process disappears, then mark the
    /// container exited (code unknown — it was never our child to `wait()`
    /// on) and clean up, exactly like the real reaper would.
    fn spawn_adopted_watcher(&self, id: String, pid: i32, starttime: u64) {
        let containers = Arc::clone(&self.containers);
        let state_path = self.state_path.clone();
        std::thread::spawn(move || {
            while process_alive(pid, starttime) {
                std::thread::sleep(ADOPT_POLL);
            }
            // The group leader is gone; namespaced children may not be.
            // Sweep the whole group so nothing survives unmanaged.
            #[cfg(unix)]
            signal_group(pid, nix::sys::signal::Signal::SIGKILL);
            let run_dir = {
                let mut map = containers.lock().unwrap();
                map.get_mut(&id).map(|state| {
                    state.exit_code = Some(-1);
                    state.exit_unknown = true;
                    state.finished_at = Some(now_secs());
                    state.run_dir.clone()
                })
            };
            if let Some(dir) = run_dir {
                remove_run_dir(&dir);
            }
            persist_registry(&containers, &state_path);
        });
    }

    fn new_id(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let hash = blake3::hash(format!("{nanos}-{n}").as_bytes());
        hash.to_hex().as_str()[..12].to_string()
    }

    /// Spawn `myc run` for `spec` and register the container. Returns its id.
    pub fn start(&self, spec: ContainerSpec) -> Result<String, String> {
        let id = self.new_id();
        let log_path = self.log_dir.join(format!("{id}.log"));
        let run_dir = self.run_root.join(&id);

        let log_file = std::fs::File::create(&log_path)
            .map_err(|e| format!("cannot create log file {}: {e}", log_path.display()))?;

        let mut cmd = Command::new(&self.exe);
        cmd.arg("--store")
            .arg(&self.store_root)
            .arg("run")
            .arg("--hostname")
            .arg(&spec.name)
            .arg("--run-dir")
            .arg(&run_dir);
        for kv in &spec.env {
            cmd.arg("-e").arg(kv);
        }
        for bind in &spec.binds {
            cmd.arg("-v").arg(bind);
        }
        if let Some(wd) = &spec.workdir {
            cmd.arg("-w").arg(wd);
        }
        if let Some(map) = &spec.map_user {
            cmd.arg("--map-user").arg(map);
        }
        if spec.isolated_network {
            cmd.arg("--net").arg("isolated");
            for m in &spec.ports {
                cmd.arg("-p").arg(format!("{}:{}", m.host, m.container));
            }
        }
        cmd.arg(&spec.manifest_id);
        if !spec.command.is_empty() {
            cmd.arg("--");
            cmd.args(&spec.command);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Own process group: `myc run` forks a namespace tree, and stop()
        // must be able to signal every process in it at once.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot start container process: {e}"))?;
        let pid = child.id() as i32;
        // Recorded so a future server restart can tell "our container is
        // still running" from "the kernel recycled the pid".
        let starttime = proc_starttime(pid).unwrap_or(0);

        let sink = Arc::new(LogSink {
            file: Mutex::new(log_file),
            written: AtomicU64::new(0),
        });
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let sink_out = Arc::clone(&sink);
        let drain_out = std::thread::spawn(move || drain_pipe(stdout, sink_out));
        let drain_err = std::thread::spawn(move || drain_pipe(stderr, sink));

        let spec_port = spec.port;
        self.containers.lock().unwrap().insert(
            id.clone(),
            ContainerState {
                spec,
                pid,
                starttime,
                started_at: now_secs(),
                finished_at: None,
                exit_code: None,
                exit_unknown: false,
                failure_hint: None,
                stopping: false,
                stop_requested: false,
                log_path: log_path.clone(),
                run_dir: run_dir.clone(),
            },
        );
        persist_registry(&self.containers, &self.state_path);

        // Reaper: waits for the child, records the exit code, sweeps any
        // process-group survivors, explains early failures and removes any
        // rootfs leftovers (present only when the process was killed).
        let containers = Arc::clone(&self.containers);
        let state_path = self.state_path.clone();
        let reaper_id = id.clone();
        std::thread::spawn(move || {
            let code = match child.wait() {
                Ok(status) => status.code().unwrap_or_else(|| killed_exit_code(&status)),
                Err(_) => 1,
            };
            // The direct child is dead, but the namespace tree it forked
            // shares its process group and can outlive it (a PID-1 with no
            // SIGTERM handler ignores polite signals). Kill the group now —
            // it is the last moment the group id is guaranteed not reused.
            #[cfg(unix)]
            signal_group(pid, nix::sys::signal::Signal::SIGKILL);
            // The pipes are closed now that the group is dead; wait for the
            // drain threads to flush the last buffered output, otherwise the
            // log-tail analysis below can race ahead of the final lines.
            let _ = drain_out.join();
            let _ = drain_err.join();
            remove_run_dir(&run_dir);
            let finished_at = now_secs();
            let (started_at, stop_requested) = {
                let map = containers.lock().unwrap();
                match map.get(&reaper_id) {
                    Some(s) => (s.started_at, s.stop_requested),
                    None => (0, false),
                }
            };
            // A container that dies with an error right after starting most
            // likely failed to start at all — mine the log for a human
            // explanation (unless the user themselves asked it to stop).
            let hint = if code != 0
                && !stop_requested
                && finished_at.saturating_sub(started_at) <= EARLY_FAILURE_WINDOW_SECS
            {
                failure_hint_from_log(&log_path, spec_port)
            } else {
                None
            };
            if let Some(state) = containers.lock().unwrap().get_mut(&reaper_id) {
                state.exit_code = Some(code);
                state.finished_at = Some(finished_at);
                state.failure_hint = hint;
            }
            persist_registry(&containers, &state_path);
        });

        Ok(id)
    }

    /// All containers, running first, then most recently started first.
    /// Running entries carry live `/proc` metrics (`cpu_percent`,
    /// `memory_bytes`, `disk_bytes`, `pids_count`); exited ones have those
    /// fields set to null. Snapshot first, sample after — sampling reads
    /// `/proc` and must not run under the registry lock.
    pub fn list(&self) -> Vec<Value> {
        /// pid + run dir of a still-running container, `None` once exited.
        type LiveHandle = Option<(i32, PathBuf)>;
        let snapshot: Vec<(String, Value, LiveHandle)> = {
            let containers = self.containers.lock().unwrap();
            let mut items: Vec<(&String, &ContainerState)> = containers.iter().collect();
            items.sort_by_key(|(_, s)| (s.exit_code.is_some(), std::cmp::Reverse(s.started_at)));
            items
                .into_iter()
                .map(|(id, s)| {
                    let live = s.exit_code.is_none().then(|| (s.pid, s.run_dir.clone()));
                    (id.clone(), s.to_json(id), live)
                })
                .collect()
        };
        snapshot
            .into_iter()
            .map(|(id, mut value, live)| {
                let sample =
                    live.map(|(pid, dir)| self.sampler.lock().unwrap().sample(&id, pid, &dir));
                attach_metrics(&mut value, sample);
                value
            })
            .collect()
    }

    /// Live process list (pid, name, CPU%, RSS) for a running container,
    /// sorted by memory descending. Empty when the container has exited.
    pub fn processes(&self, id: &str) -> Result<Vec<metrics::ProcessSample>, String> {
        let pid = {
            let containers = self.containers.lock().unwrap();
            let state = containers.get(id).ok_or("no such container")?;
            if state.exit_code.is_some() {
                return Ok(Vec::new());
            }
            state.pid
        };
        Ok(self.sampler.lock().unwrap().processes(id, pid))
    }

    pub fn get(&self, id: &str) -> Option<Value> {
        self.containers
            .lock()
            .unwrap()
            .get(id)
            .map(|s| s.to_json(id))
    }

    pub fn spec_of(&self, id: &str) -> Option<ContainerSpec> {
        self.containers
            .lock()
            .unwrap()
            .get(id)
            .map(|s| s.spec.clone())
    }

    fn is_running(&self, id: &str) -> bool {
        self.containers
            .lock()
            .unwrap()
            .get(id)
            .map(|s| s.exit_code.is_none())
            .unwrap_or(false)
    }

    /// SIGTERM the container's process group; escalate to SIGKILL after the
    /// grace period. Blocks until the process is gone (bounded ~7s).
    ///
    /// The SIGKILL escalation matters: the containerized process is PID 1
    /// in its namespace, and a PID 1 without a SIGTERM handler ignores
    /// SIGTERM entirely (observed with redis run as a bare entrypoint).
    pub fn stop(&self, id: &str) -> Result<Value, String> {
        let pid = {
            let mut containers = self.containers.lock().unwrap();
            let state = containers.get_mut(id).ok_or("no such container")?;
            if state.exit_code.is_some() {
                return Ok(state.to_json(id)); // already exited
            }
            state.stopping = true;
            state.stop_requested = true;
            state.pid
        };
        // Persist the request right away: if the server dies mid-stop, the
        // next one must still classify the eventual exit as user-initiated.
        persist_registry(&self.containers, &self.state_path);

        #[cfg(unix)]
        {
            signal_group(pid, nix::sys::signal::Signal::SIGTERM);
            if !self.wait_exit(id, STOP_GRACE) {
                signal_group(pid, nix::sys::signal::Signal::SIGKILL);
                if !self.wait_exit(id, Duration::from_secs(2)) {
                    // SIGKILL cannot be ignored; if the registry still says
                    // "running" the reaper/watcher thread is gone or wedged.
                    // Don't lie to the user — report instead of pretending.
                    return Err(
                        "the container did not terminate — try again, or check `ps` on the host"
                            .to_string(),
                    );
                }
            }
        }
        // Windows has no process groups or SIGTERM: kill the whole tree at
        // once via `taskkill /T /F`. Reduced semantics (no graceful stop),
        // acceptable because on Windows the dashboard itself runs inside
        // WSL2 (myc.exe proxies `ui`); this path exists for completeness.
        #[cfg(windows)]
        {
            kill_tree_windows(pid);
            self.wait_exit(id, STOP_GRACE);
        }
        self.get(id).ok_or_else(|| "no such container".to_string())
    }

    /// Poll the registry (the reaper thread flips `exit_code`) until the
    /// container exits or `timeout` elapses.
    fn wait_exit(&self, id: &str, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if !self.is_running(id) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        !self.is_running(id)
    }

    /// Forget an exited container and delete its log file.
    pub fn remove(&self, id: &str) -> Result<(), String> {
        let mut containers = self.containers.lock().unwrap();
        let state = containers.get(id).ok_or("no such container")?;
        if state.exit_code.is_none() {
            return Err("container is still running — stop it first".to_string());
        }
        let log_path = state.log_path.clone();
        let run_dir = state.run_dir.clone();
        containers.remove(id);
        drop(containers);
        persist_registry(&self.containers, &self.state_path);
        self.sampler.lock().unwrap().forget(id);
        let _ = std::fs::remove_file(log_path);
        remove_run_dir(&run_dir);
        Ok(())
    }

    /// Tail the log file from `offset`. Returns (text, next_offset, size).
    pub fn logs(&self, id: &str, offset: u64) -> Result<(String, u64, u64), String> {
        let log_path = {
            let containers = self.containers.lock().unwrap();
            containers
                .get(id)
                .ok_or("no such container")?
                .log_path
                .clone()
        };
        let mut file = std::fs::File::open(&log_path).map_err(|e| format!("log file: {e}"))?;
        let size = file.metadata().map_err(|e| format!("log file: {e}"))?.len();
        let start = offset.min(size);
        let len = (size - start).min(LOG_CHUNK_BYTES);
        file.seek(SeekFrom::Start(start))
            .map_err(|e| format!("log seek: {e}"))?;
        let mut buf = vec![0u8; len as usize];
        file.read_exact(&mut buf)
            .map_err(|e| format!("log read: {e}"))?;
        Ok((
            String::from_utf8_lossy(&buf).into_owned(),
            start + len,
            size,
        ))
    }

    /// Container ids belonging to `stack`, optionally narrowed to a service.
    pub fn stack_containers(&self, stack: &str, service: Option<&str>) -> Vec<String> {
        self.containers
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, s)| {
                s.spec.stack.as_deref() == Some(stack)
                    && service.is_none_or(|svc| s.spec.service.as_deref() == Some(svc))
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Latest container (by start time) for a stack service, as JSON with
    /// live metrics when running (same shape as [`Manager::list`] items).
    pub fn stack_service_state(&self, stack: &str, service: &str) -> Option<Value> {
        let (id, mut value, live) = {
            let containers = self.containers.lock().unwrap();
            let (id, s) = containers
                .iter()
                .filter(|(_, s)| {
                    s.spec.stack.as_deref() == Some(stack)
                        && s.spec.service.as_deref() == Some(service)
                })
                .max_by_key(|(_, s)| (s.exit_code.is_none(), s.started_at))?;
            let live = s.exit_code.is_none().then(|| (s.pid, s.run_dir.clone()));
            (id.clone(), s.to_json(id), live)
        };
        let sample = live.map(|(pid, dir)| self.sampler.lock().unwrap().sample(&id, pid, &dir));
        attach_metrics(&mut value, sample);
        Some(value)
    }

    /// Drop exited registry entries for a stack service (before a restart,
    /// so the list doesn't fill with dead duplicates).
    pub fn prune_stack_service(&self, stack: &str, service: &str) {
        let ids: Vec<String> = {
            let containers = self.containers.lock().unwrap();
            containers
                .iter()
                .filter(|(_, s)| {
                    s.spec.stack.as_deref() == Some(stack)
                        && s.spec.service.as_deref() == Some(service)
                        && s.exit_code.is_some()
                })
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in ids {
            let _ = self.remove(&id);
        }
    }
}

/// Merge a metrics sample into a container JSON object (nulls when the
/// container is not running, so the field set is always the same).
fn attach_metrics(value: &mut Value, sample: Option<metrics::Sample>) {
    let obj = value.as_object_mut().expect("container json is an object");
    match sample {
        Some(s) => {
            obj.insert("cpu_percent".into(), json!(s.cpu_percent));
            obj.insert("memory_bytes".into(), json!(s.memory_bytes));
            obj.insert("disk_bytes".into(), json!(s.disk_bytes));
            obj.insert("pids_count".into(), json!(s.pids_count));
        }
        None => {
            for key in ["cpu_percent", "memory_bytes", "disk_bytes", "pids_count"] {
                obj.insert(key.into(), Value::Null);
            }
        }
    }
}

/// Kernel start time of a process (`/proc/<pid>/stat` field 22, in clock
/// ticks since boot). `None` when the process does not exist.
#[cfg(target_os = "linux")]
fn proc_starttime(pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm (field 2) may contain spaces and parentheses; the kernel always
    // terminates it with the LAST ')' of the line — parse after that.
    let rest = stat.rsplit_once(')')?.1;
    // Fields after comm start at 3 (state), so starttime (22) is index 19.
    rest.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(not(target_os = "linux"))]
fn proc_starttime(_pid: i32) -> Option<u64> {
    None
}

/// Is `pid` alive *and* the same process we spawned earlier? The pid alone
/// is not enough — the kernel recycles them — so the recorded start time
/// must match too (a `starttime` of 0 means it was unreadable at spawn;
/// fall back to plain existence in that case).
fn process_alive(pid: i32, starttime: u64) -> bool {
    match proc_starttime(pid) {
        Some(current) => starttime == 0 || current == starttime,
        None => false,
    }
}

/// Append a one-line note to a container's log (used at adoption time).
fn append_log_note(log_path: &Path, note: &str) {
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
    {
        let _ = writeln!(file, "\n{note}");
    }
}

/// Log-tail patterns that indicate "the port was taken", the single most
/// common reason an app dies right after starting (matched lowercase).
const PORT_TAKEN_PATTERNS: &[&str] = &[
    "address already in use",
    "address in use",
    "eaddrinuse",
    "bind:",
    "failed to bind",
    "port is already allocated",
];

/// Log-tail patterns of the rootless single-uid limitation: the app tried
/// to switch users or chown files to a uid that does not exist in the
/// container's user namespace (matched lowercase).
const UID_LIMIT_PATTERNS: &[&str] = &[
    // Any chown failure at startup ("chown: .: Invalid argument",
    // "chown: changing ownership of …: Operation not permitted"…) is in
    // practice always the missing-uid problem.
    "chown:",
    "can't set groups",
    "cannot set groups",
    "setgroups: operation not permitted",
    "setgid: operation not permitted",
    "setuid: operation not permitted",
    "su-exec:",
    "initdb: could not change permissions",
    "operation not permitted",
    "permission denied",
];

/// Inspect the last few KiB of a failed container's log for known failure
/// signatures and translate them into one plain-English sentence.
fn failure_hint_from_log(log_path: &Path, port: Option<u16>) -> Option<String> {
    let mut file = std::fs::File::open(log_path).ok()?;
    let size = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(size.saturating_sub(8192))).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).to_lowercase();
    if PORT_TAKEN_PATTERNS.iter().any(|p| text.contains(p)) {
        let which = match port {
            Some(p) => format!("Port {p} is"),
            None => "Its network port is".to_string(),
        };
        return Some(format!(
            "{which} already in use by another program — probably another \
             copy of this app. Stop the other one (or free the port), then \
             try again."
        ));
    }
    if UID_LIMIT_PATTERNS.iter().any(|p| text.contains(p)) {
        return Some(
            "This app tried to switch to its own internal user account, which \
             this rootless setup does not provide by default. Two easy fixes: \
             install the `uidmap` package (sudo apt install uidmap) so Mycel \
             can offer the app a full range of users, or start the app with a \
             user mapping (map_user — the built-in catalog apps set it \
             automatically)."
                .to_string(),
        );
    }
    None
}

/// TCP ports currently in LISTEN state on the host (IPv4 + IPv6), parsed
/// from `/proc/net/tcp{,6}`. Containers share the host network namespace,
/// so this is exactly the set a new container would collide with.
#[cfg(target_os = "linux")]
pub fn listening_ports() -> std::collections::HashSet<u16> {
    let mut out = std::collections::HashSet::new();
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let mut fields = line.split_whitespace();
            let local = fields.nth(1); // local_address (hex ip:port)
            let state = fields.nth(1); // st, two fields later
            if state != Some("0A") {
                continue; // 0A = TCP_LISTEN
            }
            if let Some(port_hex) = local.and_then(|l| l.rsplit(':').next()) {
                if let Ok(port) = u16::from_str_radix(port_hex, 16) {
                    out.insert(port);
                }
            }
        }
    }
    out
}

#[cfg(not(target_os = "linux"))]
pub fn listening_ports() -> std::collections::HashSet<u16> {
    std::collections::HashSet::new()
}

/// Exit code for a child that terminated without one (killed by a signal):
/// the conventional 128+signal on Unix, a plain 1 elsewhere.
#[cfg(unix)]
fn killed_exit_code(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map(|s| 128 + s).unwrap_or(1)
}

#[cfg(not(unix))]
fn killed_exit_code(_status: &std::process::ExitStatus) -> i32 {
    1
}

/// Signal an entire process group, ignoring "no such process" races.
#[cfg(unix)]
fn signal_group(pgid: i32, signal: nix::sys::signal::Signal) {
    let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(-pgid), signal);
}

/// Forcibly terminate a process and all of its descendants on Windows.
#[cfg(windows)]
fn kill_tree_windows(pid: i32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Delete a caller-owned run directory. Image directory modes may lack
/// write permission, so use the runtime's chmod-aware removal for the
/// rootfs before removing the wrapper.
fn remove_run_dir(dir: &Path) {
    if !dir.exists() {
        return;
    }
    #[cfg(target_os = "linux")]
    {
        let _ = myc_run::remove_rootfs(&dir.join("rootfs"));
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A fake `myc` binary: ignores every option the manager passes and
    /// executes whatever comes after `--` with `sh -c`. This isolates the
    /// manager's process/log/stop semantics from the container runtime.
    fn write_shim(dir: &Path) -> PathBuf {
        let path = dir.join("fake-myc");
        std::fs::write(
            &path,
            "#!/bin/sh\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = \"--\" ]; then shift; exec /bin/sh -c \"$*\"; fi\n  shift\ndone\nexit 97\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn test_manager() -> (tempfile::TempDir, Manager) {
        let dir = tempfile::tempdir().unwrap();
        let shim = write_shim(dir.path());
        let manager = Manager::new(shim, dir.path().to_path_buf()).unwrap();
        (dir, manager)
    }

    /// `start` with a retry on ETXTBSY: with tests running in parallel,
    /// another test's fork can briefly hold this test's freshly written
    /// shim open, making the first exec fail with "Text file busy". This
    /// cannot happen in production, where the spawned binary is the
    /// already-running `myc` executable.
    fn start(manager: &Manager, spec: ContainerSpec) -> String {
        for _ in 0..40 {
            match manager.start(spec.clone()) {
                Ok(id) => return id,
                Err(e) if e.contains("Text file busy") => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => panic!("start failed: {e}"),
            }
        }
        panic!("start kept failing with ETXTBSY");
    }

    fn spec(command: &[&str]) -> ContainerSpec {
        ContainerSpec {
            reference: "test:latest".to_string(),
            manifest_id: "myc1-testtesttest".to_string(),
            name: "test".to_string(),
            command: command.iter().map(|s| s.to_string()).collect(),
            env: Vec::new(),
            binds: Vec::new(),
            workdir: None,
            map_user: None,
            port: None,
            isolated_network: false,
            ports: Vec::new(),
            stack: None,
            service: None,
        }
    }

    #[test]
    fn run_to_completion_and_log_offsets() {
        let (_dir, manager) = test_manager();
        let id = start(&manager, spec(&["echo marker-one; echo marker-two"]));
        assert!(manager.wait_exit(&id, Duration::from_secs(10)));

        let state = manager.get(&id).unwrap();
        assert_eq!(state["exit_code"], 0);
        assert_eq!(state["status"], "exited(0)");

        // Full read from 0, then incremental read from `next` is empty.
        let (data, next, size) = manager.logs(&id, 0).unwrap();
        assert!(data.contains("marker-one"), "log was: {data:?}");
        assert!(data.contains("marker-two"));
        assert_eq!(next, size);
        let (tail, next2, _) = manager.logs(&id, next).unwrap();
        assert!(tail.is_empty());
        assert_eq!(next2, next);

        // Offsets slice the middle of the stream correctly.
        let mid = data.find("marker-two").unwrap() as u64;
        let (rest, _, _) = manager.logs(&id, mid).unwrap();
        assert!(rest.starts_with("marker-two"));
    }

    #[test]
    fn log_cap_is_enforced() {
        let (_dir, manager) = test_manager();
        // ~3 MB of output against a 2 MiB cap.
        let id = start(&manager, spec(&["yes cap-test-line | head -c 3000000"]));
        assert!(manager.wait_exit(&id, Duration::from_secs(20)));
        let (_, _, size) = manager.logs(&id, 0).unwrap();
        assert!(
            size <= LOG_CAP_BYTES + 64,
            "log grew past the cap: {size} bytes"
        );
        let (data, _, _) = manager
            .logs(&id, LOG_CAP_BYTES.saturating_sub(1024))
            .unwrap();
        assert!(data.contains("truncated"), "missing truncation marker");
    }

    #[test]
    fn stop_kills_the_whole_process_group() {
        let (_dir, manager) = test_manager();
        // The shell spawns a grandchild `sleep` and prints its pid.
        let id = start(&manager, spec(&["sleep 300 & echo grandchild=$!; wait"]));

        // Wait until the grandchild pid shows up in the log.
        let mut grandchild = None;
        for _ in 0..50 {
            let (data, _, _) = manager.logs(&id, 0).unwrap();
            if let Some(rest) = data.split("grandchild=").nth(1) {
                grandchild = rest.split_whitespace().next().map(str::to_string);
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let grandchild = grandchild.expect("grandchild pid never appeared in log");

        let state = manager.stop(&id).unwrap();
        assert_eq!(state["running"], false);
        // SIGTERM to the group: the shell exits with 128+15.
        assert_eq!(state["exit_code"], 128 + 15);

        // The grandchild must be gone too (group signal, not just the child).
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !Path::new(&format!("/proc/{grandchild}")).exists(),
            "grandchild sleep survived the group signal"
        );
    }

    #[test]
    fn stop_escalates_to_sigkill() {
        let (_dir, manager) = test_manager();
        let id = start(&manager, spec(&["trap '' TERM; sleep 300"]));
        std::thread::sleep(Duration::from_millis(300)); // let the trap install
        let started = std::time::Instant::now();
        let state = manager.stop(&id).unwrap();
        assert_eq!(state["running"], false);
        assert_eq!(state["exit_code"], 128 + 9, "expected SIGKILL exit");
        let took = started.elapsed();
        assert!(took >= STOP_GRACE, "killed before the grace period");
        assert!(took < STOP_GRACE + Duration::from_secs(4));
    }

    /// A user-initiated stop must never read as a failure: no red badge
    /// material (`stopped_by_user` set), no failure hint — for both the
    /// SIGTERM (143) and SIGKILL-escalation (137) paths — and the
    /// classification survives a server restart.
    #[test]
    fn user_stop_is_not_classified_as_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let shim = write_shim(dir.path());
        let manager = Manager::new(shim.clone(), dir.path().to_path_buf()).unwrap();

        // Polite path: plain sleep dies on SIGTERM (exit 143).
        let polite = start(&manager, spec(&["sleep 300"]));
        std::thread::sleep(Duration::from_millis(200));
        let state = manager.stop(&polite).unwrap();
        assert_eq!(state["exit_code"], 128 + 15);
        assert_eq!(state["stopped_by_user"], true);
        assert_eq!(state["failure_hint"], Value::Null);

        // Stubborn path: TERM ignored, SIGKILL escalation (exit 137).
        let stubborn = start(&manager, spec(&["trap '' TERM; sleep 300"]));
        std::thread::sleep(Duration::from_millis(300));
        let state = manager.stop(&stubborn).unwrap();
        assert_eq!(state["exit_code"], 128 + 9);
        assert_eq!(state["stopped_by_user"], true);
        assert_eq!(state["failure_hint"], Value::Null);

        // A container that dies on its own is NOT "stopped by you".
        let crashed = start(&manager, spec(&["exit 3"]));
        assert!(manager.wait_exit(&crashed, Duration::from_secs(10)));
        let state = manager.get(&crashed).unwrap();
        assert_eq!(state["exit_code"], 3);
        assert_eq!(state["stopped_by_user"], false);

        // The classification is persisted and survives a reload.
        drop(manager);
        let reloaded = Manager::new(shim, dir.path().to_path_buf()).unwrap();
        assert_eq!(reloaded.get(&polite).unwrap()["stopped_by_user"], true);
        assert_eq!(reloaded.get(&stubborn).unwrap()["stopped_by_user"], true);
        assert_eq!(reloaded.get(&crashed).unwrap()["stopped_by_user"], false);
    }

    #[test]
    fn remove_forgets_container_and_deletes_log() {
        let (_dir, manager) = test_manager();
        let id = start(&manager, spec(&["echo done"]));
        assert!(manager.wait_exit(&id, Duration::from_secs(10)));
        assert!(manager.remove(&id).is_ok());
        assert!(manager.get(&id).is_none());
        assert!(manager.logs(&id, 0).is_err());
        assert!(manager.list().is_empty());
    }

    #[test]
    fn remove_refuses_running_container() {
        let (_dir, manager) = test_manager();
        let id = start(&manager, spec(&["sleep 30"]));
        std::thread::sleep(Duration::from_millis(200));
        assert!(manager.remove(&id).is_err());
        manager.stop(&id).unwrap();
        assert!(manager.remove(&id).is_ok());
    }

    #[test]
    fn list_reports_live_metrics_and_processes() {
        let (_dir, manager) = test_manager();
        let id = start(&manager, spec(&["sleep 60"]));
        std::thread::sleep(Duration::from_millis(300));

        let list = manager.list();
        let entry = list.iter().find(|c| c["id"] == json!(id)).unwrap();
        assert!(
            entry["memory_bytes"].as_u64().unwrap() > 0,
            "running container should have RSS: {entry}"
        );
        assert!(entry["pids_count"].as_u64().unwrap() >= 1);
        assert!(entry["cpu_percent"].is_number());

        let procs = manager.processes(&id).unwrap();
        assert!(!procs.is_empty());
        assert!(
            procs.iter().any(|p| p.name == "sleep" || p.name == "sh"),
            "expected the entrypoint in {procs:?}"
        );

        manager.stop(&id).unwrap();
        let list = manager.list();
        let entry = list.iter().find(|c| c["id"] == json!(id)).unwrap();
        assert!(entry["memory_bytes"].is_null(), "exited metrics are null");
        assert!(manager.processes(&id).unwrap().is_empty());
    }

    #[test]
    fn stack_queries() {
        let (_dir, manager) = test_manager();
        let mut s1 = spec(&["echo a"]);
        s1.stack = Some("web".to_string());
        s1.service = Some("db".to_string());
        let id1 = start(&manager, s1);
        let mut s2 = spec(&["echo b"]);
        s2.stack = Some("web".to_string());
        s2.service = Some("api".to_string());
        start(&manager, s2);

        assert!(manager.wait_exit(&id1, Duration::from_secs(10)));
        assert_eq!(manager.stack_containers("web", None).len(), 2);
        assert_eq!(manager.stack_containers("web", Some("db")).len(), 1);
        assert_eq!(manager.stack_containers("other", None).len(), 0);
        let db = manager.stack_service_state("web", "db").unwrap();
        assert_eq!(db["service"], "db");

        // prune removes only exited entries of that service
        manager.prune_stack_service("web", "db");
        assert_eq!(manager.stack_containers("web", Some("db")).len(), 0);
        assert_eq!(manager.stack_containers("web", Some("api")).len(), 1);
    }

    #[test]
    fn running_container_is_adopted_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let shim = write_shim(dir.path());
        let first = Manager::new(shim.clone(), dir.path().to_path_buf()).unwrap();
        let id = start(&first, spec(&["sleep 300"]));
        std::thread::sleep(Duration::from_millis(300));
        drop(first); // "server restart": registry memory is gone

        let second = Manager::new(shim, dir.path().to_path_buf()).unwrap();
        let state = second.get(&id).expect("container was not reloaded");
        assert_eq!(state["running"], true, "not re-adopted: {state}");

        // The log survived the restart and grew an adoption note.
        let (data, _, _) = second.logs(&id, 0).unwrap();
        assert!(data.contains("dashboard restarted"), "log was: {data:?}");

        // The adopted container is still stoppable (watcher flips the exit).
        let stopped = second.stop(&id).unwrap();
        assert_eq!(stopped["running"], false);
        // Its real exit code was never observable by the new server.
        assert_eq!(stopped["exit_code"], Value::Null);
        assert_eq!(stopped["status"], "exited");
    }

    #[test]
    fn exited_and_dead_containers_survive_restart_as_exited() {
        let dir = tempfile::tempdir().unwrap();
        let shim = write_shim(dir.path());
        let first = Manager::new(shim.clone(), dir.path().to_path_buf()).unwrap();
        let done = start(&first, spec(&["echo done"]));
        assert!(first.wait_exit(&done, Duration::from_secs(10)));
        drop(first);

        // Forge an entry whose process "died while the server was down":
        // pid 1 is alive but its start time can never match.
        let state_path = dir.path().join("ui-containers.json");
        let mut records: Vec<serde_json::Value> =
            serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
        let mut ghost = records[0].clone();
        ghost["id"] = json!("ghost12345678");
        ghost["pid"] = json!(1);
        ghost["starttime"] = json!(u64::MAX);
        ghost["exit_code"] = json!(null);
        ghost["finished_at"] = json!(null);
        records.push(ghost);
        std::fs::write(&state_path, serde_json::to_vec(&records).unwrap()).unwrap();

        let second = Manager::new(shim, dir.path().to_path_buf()).unwrap();
        let done_state = second.get(&done).unwrap();
        assert_eq!(done_state["exit_code"], 0, "clean exit was not preserved");
        let ghost_state = second.get("ghost12345678").unwrap();
        assert_eq!(ghost_state["running"], false);
        assert_eq!(ghost_state["exit_code"], Value::Null);
        assert_eq!(ghost_state["status"], "exited");
        // Both are removable (nothing is left running behind them).
        assert!(second.remove(&done).is_ok());
        assert!(second.remove("ghost12345678").is_ok());
    }

    #[test]
    fn early_bind_failure_gets_a_human_hint() {
        let (_dir, manager) = test_manager();
        let mut s = spec(&["echo 'bind: Address already in use' >&2; exit 1"]);
        s.port = Some(6379);
        let id = start(&manager, s);
        assert!(manager.wait_exit(&id, Duration::from_secs(10)));
        // The reaper writes the hint right after flipping exit_code.
        std::thread::sleep(Duration::from_millis(300));
        let state = manager.get(&id).unwrap();
        assert_eq!(state["exit_code"], 1);
        let hint = state["failure_hint"].as_str().expect("expected a hint");
        assert!(hint.contains("6379"), "hint was: {hint}");
        assert!(hint.contains("already in use"), "hint was: {hint}");
    }

    #[test]
    fn uid_limit_failure_gets_a_human_hint() {
        let (_dir, manager) = test_manager();
        let s =
            spec(&["echo 'chown: changing ownership of /var/lib/x: Invalid argument' >&2; exit 1"]);
        let id = start(&manager, s);
        assert!(manager.wait_exit(&id, Duration::from_secs(10)));
        std::thread::sleep(Duration::from_millis(300));
        let state = manager.get(&id).unwrap();
        let hint = state["failure_hint"].as_str().expect("expected a hint");
        assert!(hint.contains("user account"), "hint was: {hint}");
        assert!(hint.contains("uidmap"), "hint was: {hint}");
    }

    #[test]
    fn clean_exit_has_no_failure_hint() {
        let (_dir, manager) = test_manager();
        let id = start(&manager, spec(&["echo ok"]));
        assert!(manager.wait_exit(&id, Duration::from_secs(10)));
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(manager.get(&id).unwrap()["failure_hint"], Value::Null);
    }

    #[test]
    fn listening_ports_sees_a_bound_socket() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(
            listening_ports().contains(&port),
            "bound port {port} not reported"
        );
    }
}
