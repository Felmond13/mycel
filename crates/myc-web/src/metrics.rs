//! Live resource metrics for managed containers, collected from `/proc`.
//!
//! Containers are child processes in their own process group and rootless
//! runs cannot rely on cgroup delegation, so everything is derived from
//! `/proc`: the group's PIDs are enumerated by scanning `/proc/*/stat` for
//! a matching pgrp, RSS comes from `/proc/<pid>/status`, and CPU usage is
//! the utime+stime delta between two samples (per pid), expressed as a
//! percentage of **one core** — values above 100 mean multiple cores.
//!
//! Disk usage is a recursive `du` of the container's run directory; it is
//! expensive for big rootfs trees, so results are cached and refreshed
//! lazily every [`DISK_REFRESH`].
//!
//! Every `/proc` read tolerates ENOENT: processes may exit mid-scan.

use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

/// How long a cached rootfs `du` stays fresh.
const DISK_REFRESH: Duration = Duration::from_secs(30);
/// Two scans closer than this reuse the previous result instead of
/// computing CPU percentages over a uselessly small time window.
const MIN_RESCAN: Duration = Duration::from_millis(500);

/// One process inside a container's process group.
#[derive(Clone, Debug, Serialize)]
pub struct ProcessSample {
    pub pid: i32,
    /// Command name from `/proc/<pid>/comm`.
    pub name: String,
    /// Percent of one core since the previous scan (0 on the first scan).
    pub cpu_percent: f64,
    /// Resident set size in bytes (VmRSS).
    pub memory_bytes: u64,
}

/// Aggregated metrics for one container.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Sample {
    pub cpu_percent: f64,
    pub memory_bytes: u64,
    pub disk_bytes: u64,
    pub pids_count: usize,
}

/// Per-container scan state kept between polls.
struct ScanState {
    at: Instant,
    /// utime+stime ticks per pid at the time of the last scan.
    ticks: HashMap<i32, u64>,
    /// Result of the last scan, reused for near-simultaneous callers.
    cached: Vec<ProcessSample>,
}

/// Keeps the last CPU sample and the disk cache per container so metrics
/// can be computed on demand from any request handler.
#[derive(Default)]
pub struct Sampler {
    scans: HashMap<String, ScanState>,
    disk: HashMap<String, (u64, Instant)>,
}

impl Sampler {
    /// The container's process list with per-process CPU/RSS, sorted by
    /// memory descending. The first call reports 0% CPU (no delta yet).
    pub fn processes(&mut self, id: &str, pgid: i32) -> Vec<ProcessSample> {
        let now = Instant::now();
        if let Some(prev) = self.scans.get(id) {
            if now.duration_since(prev.at) < MIN_RESCAN {
                return prev.cached.clone();
            }
        }

        let tick_hz = clock_ticks_per_sec();
        let mut ticks = HashMap::new();
        let mut procs = Vec::new();
        for pid in pids_in_group(pgid) {
            let Some(t) = read_cpu_ticks(pid) else {
                continue; // exited mid-scan
            };
            ticks.insert(pid, t);
            let cpu_percent = match self.scans.get(id) {
                Some(prev) => {
                    let elapsed = now.duration_since(prev.at).as_secs_f64();
                    match prev.ticks.get(&pid) {
                        Some(&t0) if elapsed > 0.0 => {
                            100.0 * (t.saturating_sub(t0) as f64 / tick_hz) / elapsed
                        }
                        _ => 0.0, // new pid since the last scan
                    }
                }
                None => 0.0,
            };
            procs.push(ProcessSample {
                pid,
                name: read_comm(pid).unwrap_or_else(|| "?".to_string()),
                cpu_percent: round1(cpu_percent),
                memory_bytes: read_rss_bytes(pid).unwrap_or(0),
            });
        }
        procs.sort_by(|a, b| b.memory_bytes.cmp(&a.memory_bytes).then(a.pid.cmp(&b.pid)));
        self.scans.insert(
            id.to_string(),
            ScanState {
                at: now,
                ticks,
                cached: procs.clone(),
            },
        );
        procs
    }

    /// Aggregate CPU/RAM/disk/pids for one container.
    pub fn sample(&mut self, id: &str, pgid: i32, run_dir: &Path) -> Sample {
        let procs = self.processes(id, pgid);
        let disk_bytes = self.disk_usage(id, run_dir);
        Sample {
            cpu_percent: round1(procs.iter().map(|p| p.cpu_percent).sum()),
            memory_bytes: procs.iter().map(|p| p.memory_bytes).sum(),
            disk_bytes,
            pids_count: procs.len(),
        }
    }

    /// `du` of the run directory, cached for [`DISK_REFRESH`].
    fn disk_usage(&mut self, id: &str, run_dir: &Path) -> u64 {
        let now = Instant::now();
        if let Some((bytes, at)) = self.disk.get(id) {
            if now.duration_since(*at) < DISK_REFRESH {
                return *bytes;
            }
        }
        let bytes = du(run_dir);
        self.disk.insert(id.to_string(), (bytes, now));
        bytes
    }

    /// Drop all state for a removed container.
    pub fn forget(&mut self, id: &str) {
        self.scans.remove(id);
        self.disk.remove(id);
    }
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

#[cfg(unix)]
fn clock_ticks_per_sec() -> f64 {
    // SAFETY: sysconf is async-signal-safe and has no memory effects.
    let hz = unsafe { nix::libc::sysconf(nix::libc::_SC_CLK_TCK) };
    if hz > 0 {
        hz as f64
    } else {
        100.0
    }
}

/// Non-Unix hosts have no `/proc`, so every scan is empty and this value is
/// never multiplied into anything meaningful.
#[cfg(not(unix))]
fn clock_ticks_per_sec() -> f64 {
    100.0
}

/// All PIDs whose process group is `pgid`, discovered by scanning
/// `/proc/*/stat`. Descendants inherit the group unless they call setsid,
/// which `myc run` never does.
pub fn pids_in_group(pgid: i32) -> Vec<i32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()))
        .filter(|&pid| {
            stat_after_comm(pid).is_some_and(|rest| field(&rest, 2) == Some(pgid as u64))
        })
        .collect()
}

/// The part of `/proc/<pid>/stat` after the `(comm)` field, which is the
/// only field that can contain spaces or parentheses.
fn stat_after_comm(pid: i32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    Some(stat[close + 1..].trim_start().to_string())
}

/// Space-separated field `n` of the post-comm remainder, parsed as u64.
/// `n = 0` is the state field (overall field 3 of stat).
fn field(rest: &str, n: usize) -> Option<u64> {
    rest.split_ascii_whitespace().nth(n)?.parse().ok()
}

/// utime + stime in clock ticks (stat fields 14 and 15).
fn read_cpu_ticks(pid: i32) -> Option<u64> {
    let rest = stat_after_comm(pid)?;
    Some(field(&rest, 11)? + field(&rest, 12)?)
}

/// VmRSS from `/proc/<pid>/status`, in bytes.
fn read_rss_bytes(pid: i32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kib: u64 = line.split_ascii_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}

/// Command name from `/proc/<pid>/comm` (max 15 chars, no path).
fn read_comm(pid: i32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
}

/// Recursive size of `dir` in bytes (regular files only, symlinks not
/// followed). Missing paths count as 0 — the run dir appears only once
/// `myc run` has materialized the rootfs.
fn du(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            total += du(&entry.path());
        } else if meta.is_file() {
            total += meta.len();
        }
    }
    total
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    /// Spawn `sh -c <script>` in its own process group; returns the child.
    fn spawn_group(script: &str) -> std::process::Child {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        cmd.spawn().expect("spawn test process")
    }

    fn kill_group(pid: i32) {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(-pid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }

    #[test]
    fn sample_reports_memory_and_pids_for_a_real_process() {
        let mut child = spawn_group("sleep 60");
        let pgid = child.id() as i32;
        std::thread::sleep(Duration::from_millis(200));

        let mut sampler = Sampler::default();
        let sample = sampler.sample("t1", pgid, Path::new("/nonexistent-dir"));
        kill_group(pgid);
        let _ = child.wait();

        assert!(sample.pids_count >= 1, "no pids found: {sample:?}");
        assert!(sample.memory_bytes > 0, "sleep should have RSS: {sample:?}");
        assert_eq!(sample.disk_bytes, 0, "missing dir must count as 0");
    }

    #[test]
    fn processes_lists_the_command_by_name() {
        let mut child = spawn_group("sleep 60");
        let pgid = child.id() as i32;
        std::thread::sleep(Duration::from_millis(200));

        let mut sampler = Sampler::default();
        let procs = sampler.processes("t2", pgid);
        kill_group(pgid);
        let _ = child.wait();

        assert!(!procs.is_empty(), "process list is empty");
        assert!(
            procs.iter().any(|p| p.name == "sleep" || p.name == "sh"),
            "expected sleep/sh in {procs:?}"
        );
        assert!(procs.iter().all(|p| p.pid > 0));
    }

    #[test]
    fn cpu_percent_rises_for_a_busy_loop() {
        let mut child = spawn_group("while :; do :; done");
        let pgid = child.id() as i32;
        std::thread::sleep(Duration::from_millis(200));

        let mut sampler = Sampler::default();
        sampler.sample("t3", pgid, Path::new("/nonexistent-dir")); // baseline
        std::thread::sleep(Duration::from_millis(700));
        let second = sampler.sample("t3", pgid, Path::new("/nonexistent-dir"));
        kill_group(pgid);
        let _ = child.wait();

        assert!(
            second.cpu_percent > 5.0,
            "busy loop should burn CPU: {second:?}"
        );
    }

    #[test]
    fn du_counts_nested_files_and_caches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("a"), vec![0u8; 1000]).unwrap();
        std::fs::write(dir.path().join("sub/b"), vec![0u8; 2000]).unwrap();
        assert_eq!(du(dir.path()), 3000);

        let mut sampler = Sampler::default();
        assert_eq!(sampler.disk_usage("t4", dir.path()), 3000);
        // Grow the tree: the cached value is served until the refresh window.
        std::fs::write(dir.path().join("c"), vec![0u8; 500]).unwrap();
        assert_eq!(sampler.disk_usage("t4", dir.path()), 3000);
    }

    #[test]
    fn dead_group_yields_empty_metrics() {
        let mut child = spawn_group("true");
        let pgid = child.id() as i32;
        let _ = child.wait();
        std::thread::sleep(Duration::from_millis(100));
        let mut sampler = Sampler::default();
        let sample = sampler.sample("t5", pgid, Path::new("/nonexistent-dir"));
        assert_eq!(sample.pids_count, 0);
        assert_eq!(sample.memory_bytes, 0);
    }
}
