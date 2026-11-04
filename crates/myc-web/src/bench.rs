//! Snapshot/restore status (`/api/snapshots/status`) and the integrated
//! comparison bench (`/api/bench/compare`).
//!
//! Both features report *measurements*, never marketing numbers: the
//! snapshot page prefers the raw result files committed under the repo's
//! `bench/` directories (parsed with the same median/p95 math the bench
//! scripts use — nearest-rank p95) and falls back to the documented
//! constants, clearly marked `docs/…` as their source. The comparison
//! bench measures honestly on this machine, n runs per side, medians and
//! p95 computed server-side from the raw in-memory samples; the last run
//! is kept for the UI.

use crate::{ApiResult, App};
use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ---- statistics (pure, unit-tested) ----

/// Median of the samples (mean of the two middle values for an even
/// count). `None` when there is nothing to summarize.
pub fn median(samples: &[f64]) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("finite samples"));
    let mid = sorted.len() / 2;
    Some(if sorted.len() % 2 == 1 {
        sorted[mid]
    } else {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    })
}

/// p95 by the nearest-rank method (ceil(0.95·n)-th ordered value) — the
/// exact definition the `bench/` verdict scripts use, so endpoint numbers
/// match the archived verdicts.
pub fn p95(samples: &[f64]) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("finite samples"));
    let n = sorted.len();
    let rank = ((0.95_f64 * n as f64).ceil() as usize).clamp(1, n);
    Some(sorted[rank - 1])
}

/// Median and p95 rounded to whole milliseconds, plus the sample count —
/// the triple every measurement entry reports.
pub fn summarize_ms(samples: &[f64]) -> Option<(u64, u64, usize)> {
    Some((
        median(samples)?.round() as u64,
        p95(samples)?.round() as u64,
        samples.len(),
    ))
}

/// Parse a bench `*.times` file: one integer duration in nanoseconds per
/// line (blank lines ignored), returned in milliseconds.
pub fn parse_times(text: &str) -> Vec<f64> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|l| l.parse::<f64>().ok())
        .map(|ns| ns / 1_000_000.0)
        .collect()
}

/// Read a `*.times` file and summarize it; `None` when missing/empty.
fn measurement_from_file(path: &Path) -> Option<(u64, u64, usize)> {
    let text = std::fs::read_to_string(path).ok()?;
    summarize_ms(&parse_times(&text))
}

// ---- timestamps (pure, unit-tested) ----

/// Days/civil conversion (Howard Hinnant's algorithm) — no chrono dep.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn split_unix(secs: u64) -> (String, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, mo, d) = civil_from_days(days);
    (
        format!("{y:04}"),
        mo,
        d,
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    )
}

/// RFC 3339 UTC timestamp of `t` (second precision).
pub fn rfc3339(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = split_unix(secs);
    format!("{y}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// UTC calendar date (`YYYY-MM-DD`) of `t` — the `when` of a measurement.
pub fn date_of(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, _, _, _) = split_unix(secs);
    format!("{y}-{mo:02}-{d:02}")
}

// ---- environment detection ----

/// The Mycel repo root (which hosts `bench/`), found by walking up from
/// the running binary and the working directory. `None` outside a repo
/// checkout — callers then use the documented constants.
fn find_repo_root() -> Option<&'static Path> {
    static ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();
    ROOT.get_or_init(|| {
        let is_repo = |p: &Path| p.join("bench").is_dir() && p.join("crates").is_dir();
        let mut candidates = Vec::new();
        if let Ok(exe) = std::env::current_exe() {
            candidates.extend(exe.ancestors().map(Path::to_path_buf));
        }
        if let Ok(cwd) = std::env::current_dir() {
            candidates.extend(cwd.ancestors().map(Path::to_path_buf));
        }
        candidates.into_iter().find(|p| is_repo(p))
    })
    .as_deref()
}

/// Locate `firecracker` (PATH, then `/usr/local/bin`) and read its
/// version (`firecracker --version` prints e.g. `Firecracker v1.17.0`).
fn detect_firecracker() -> Option<(PathBuf, String)> {
    let mut candidates: Vec<PathBuf> = std::env::var("PATH")
        .ok()?
        .split(':')
        .map(|dir| Path::new(dir).join("firecracker"))
        .collect();
    candidates.push(PathBuf::from("/usr/local/bin/firecracker"));
    let path = candidates.into_iter().find(|p| p.is_file())?;
    let output = Command::new(&path)
        .arg("--version")
        .stderr(Stdio::piped())
        .output()
        .ok()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // The version is the `v1.2.3`-shaped token of the banner.
    let version = text
        .split_whitespace()
        .find(|tok| {
            let t = tok.strip_prefix('v').unwrap_or(tok);
            !t.is_empty() && t.chars().next().is_some_and(|c| c.is_ascii_digit()) && t.contains('.')
        })
        .map(|tok| tok.strip_prefix('v').unwrap_or(tok).to_string())
        .unwrap_or_else(|| "unknown".to_string());
    Some((path, version))
}

/// Is the `docker` CLI present in PATH? (Presence only — the daemon is
/// probed for real by the first measured run.)
fn docker_in_path() -> bool {
    std::env::var("PATH")
        .map(|paths| {
            paths
                .split(':')
                .any(|dir| Path::new(dir).join("docker").is_file())
        })
        .unwrap_or(false)
}

/// Is the kvm-microvm engine READY (available and complete) here? Mirrors
/// what `myc doctor` and `/api/engines` report.
fn kvm_ready(store_root: &Path) -> bool {
    myc_backend::detect_engines(store_root)
        .iter()
        .any(|e| e.kind == myc_backend::EngineKind::KvmMicroVm && e.available && e.complete)
}

// ---- GET /api/snapshots/status ----

/// Documented fallback constants (measured values from the bench
/// archives, quoted in docs/isolation-engines.md) used when the raw
/// result files are not on this machine.
const DOC_RESTORE: (u64, u64, usize) = (91, 161, 10);
const DOC_HANDOFF: (u64, u64, usize) = (28, 61, 30);
const DOC_BOOT_SLIM: (u64, u64, usize) = (118, 220, 30);
const DOC_SOURCE: &str = "docs/isolation-engines.md";

fn measurement_entry(
    label: &str,
    measured: Option<(u64, u64, usize)>,
    when: Option<String>,
    file_source: &str,
) -> Value {
    let (numbers, source) = match measured {
        Some(numbers) => (numbers, file_source),
        None => (
            DOC_FALLBACKS
                .iter()
                .find(|(l, _)| *l == label)
                .map(|(_, s)| *s)
                .unwrap_or((0, 0, 0)),
            DOC_SOURCE,
        ),
    };
    let mut entry = json!({
        "label": label,
        "median_ms": numbers.0,
        "p95_ms": numbers.1,
        "n": numbers.2,
        "source": source,
    });
    if let Some(when) = when {
        entry
            .as_object_mut()
            .expect("measurement is an object")
            .insert("when".into(), json!(when));
    }
    entry
}

/// Label → documented constant, for the fallback branch above.
const DOC_FALLBACKS: &[(&str, (u64, u64, usize))] = &[
    ("restore Firecracker 512Mo (WSL2)", DOC_RESTORE),
    ("handoff pool chaud", DOC_HANDOFF),
    ("boot microVM slim", DOC_BOOT_SLIM),
];

/// Build the `/api/snapshots/status` payload. Every number is either
/// parsed from this machine's raw bench results (source `bench/…`) or the
/// documented constant (source `docs/…`) — never invented.
pub fn snapshots_payload(store_root: &Path) -> Value {
    let repo = find_repo_root();
    let bench = |rel: &str| repo.map(|r| r.join(rel));

    // Firecracker: real detection (PATH + /usr/local/bin + --version).
    let firecracker = match detect_firecracker() {
        Some((path, version)) => json!({
            "available": true,
            "version": version,
            "path": path,
        }),
        None => json!({
            "available": false,
            "note": "firecracker introuvable (ni dans le PATH ni dans /usr/local/bin)",
        }),
    };

    // The warm-pool path needs the microVM boot stack (KVM engine).
    let kvm_ok = kvm_ready(store_root);
    let handoff_file = bench("bench/warm-pool/results/handoff.times");
    let handoff = handoff_file.as_deref().and_then(measurement_from_file);
    let handoff_median = handoff.map(|(m, _, _)| m).unwrap_or(DOC_HANDOFF.0);
    let warm_pool = json!({
        "available": kvm_ok,
        "note": format!("handoff mesuré {handoff_median} ms"),
    });

    // cloud-hypervisor snapshot/restore under WSL2: measured inert (no
    // live guest after restore) — the date is that of the archived run
    // when present, the documented one otherwise.
    let cloud_hypervisor_wsl2 = match bench("bench/snapshot-wsl2/results/restore.times")
        .as_deref()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
    {
        Some(t) => format!("restore inerte (mesuré {})", date_of(t)),
        None => "restore inerte (mesuré 2026-11-03)".to_string(),
    };

    let measurements = vec![
        labeled_file_measurement(
            "restore Firecracker 512Mo (WSL2)",
            bench("bench/snapshot-wsl2/results/restore.times"),
            "bench/snapshot-wsl2",
        ),
        labeled_file_measurement("handoff pool chaud", handoff_file, "bench/warm-pool"),
        labeled_file_measurement(
            "boot microVM slim",
            bench("bench/microvm/results-v3-slim/C.times"),
            "bench/microvm v3",
        ),
    ];

    json!({
        "firecracker": firecracker,
        "cloud_hypervisor_wsl2": cloud_hypervisor_wsl2,
        "warm_pool": warm_pool,
        "measurements": measurements,
    })
}

/// One measurement entry from a raw `.times` file: real medians and the
/// file's date when the file exists, the documented constant otherwise.
fn labeled_file_measurement(label: &str, path: Option<PathBuf>, source: &str) -> Value {
    let (measured, when) = match path.as_ref().and_then(|p| measurement_from_file(p)) {
        Some(m) => (
            Some(m),
            path.as_ref()
                .and_then(|p| std::fs::metadata(p).ok())
                .and_then(|meta| meta.modified().ok())
                .map(date_of),
        ),
        None => (None, None),
    };
    measurement_entry(label, measured, when, source)
}

/// `GET /api/snapshots/status`.
pub async fn snapshots_status(State(app): State<App>) -> ApiResult {
    crate::blocking(move || Ok(Json(snapshots_payload(&app.root)))).await
}

// ---- POST/GET /api/bench/compare ----

/// How many timed runs per side.
const RUNS: usize = 10;
const MYC_TIMEOUT: Duration = Duration::from_secs(60);
const VM_TIMEOUT: Duration = Duration::from_secs(120);
const PULL_TIMEOUT: Duration = Duration::from_secs(300);

/// Job phases as reported over the API.
#[derive(Debug, PartialEq)]
enum Phase {
    Idle,
    Running,
    Done,
    Error(String),
}

impl Phase {
    fn label(&self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Running => "running",
            Phase::Done => "done",
            Phase::Error(_) => "error",
        }
    }
}

/// Shared state of the comparison bench: one job at a time, the last
/// run's results kept for the UI.
pub struct BenchHub {
    state: Mutex<JobState>,
}

#[derive(Default)]
struct JobState {
    phase: Option<Phase>,
    progress: u32,
    results: Vec<Value>,
    when: Option<String>,
}

impl BenchHub {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(JobState {
                phase: Some(Phase::Idle),
                progress: 0,
                results: Vec::new(),
                when: None,
            }),
        }
    }

    /// Try to claim the hub for a new run. Fails while one is running.
    fn start(&self) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        if matches!(state.phase, Some(Phase::Running)) {
            return Err("un benchmark compare déjà en cours — attendez la fin".to_string());
        }
        state.phase = Some(Phase::Running);
        state.progress = 0;
        state.results = Vec::new();
        state.when = Some(rfc3339(SystemTime::now()));
        Ok(())
    }

    fn tick(&self) {
        let mut state = self.state.lock().unwrap();
        state.progress += 1;
    }

    fn finish(&self, results: Vec<Value>) {
        let mut state = self.state.lock().unwrap();
        state.phase = Some(Phase::Done);
        state.results = results;
    }

    /// Mark the job as failed, keeping the per-scenario results (the
    /// errors explaining the failure live in them).
    fn fail(&self, message: String, results: Vec<Value>) {
        let mut state = self.state.lock().unwrap();
        state.phase = Some(Phase::Error(message));
        state.results = results;
    }

    fn to_json(&self) -> Value {
        let state = self.state.lock().unwrap();
        let mut body = json!({
            "state": state.phase.as_ref().map(Phase::label).unwrap_or("idle"),
            "progress": state.progress,
            "results": state.results,
            "when": state.when,
        });
        if let Some(Phase::Error(msg)) = state.phase.as_ref() {
            body.as_object_mut()
                .expect("bench status is an object")
                .insert("error".into(), json!(msg));
        }
        body
    }
}

/// `GET /api/bench/compare`.
pub async fn compare_status(State(app): State<App>) -> ApiResult {
    Ok(Json(app.bench.to_json()))
}

/// `POST /api/bench/compare` — starts the honest on-machine measurement
/// job in the background. Only one job at a time.
pub async fn start_compare(State(app): State<App>) -> ApiResult {
    let exe = crate::blocking(|| {
        std::env::current_exe().map_err(|e| crate::internal(format!("binary path: {e}")))
    })
    .await?;
    app.bench.start().map_err(crate::conflict)?;
    let root = app.root.clone();
    let hub = app.bench.clone();
    std::thread::spawn(move || run_compare(&root, &exe, &hub));
    Ok(Json(json!({ "state": "running" })))
}

/// The whole comparison, measured on this machine. Scenario errors are
/// recorded and never kill the job.
fn run_compare(root: &Path, exe: &Path, hub: &BenchHub) {
    let alpine = "alpine:3.20";
    let mut results = Vec::new();

    // ---- scenario "run": myc vs docker, same workload ----
    let mut sides = Vec::new();
    let mut note = None;
    match ensure_in_store(root, alpine) {
        Ok(()) => {
            sides.push(measure_side(
                "myc",
                || {
                    let mut cmd = Command::new(exe);
                    cmd.arg("--store").arg(root).arg("run").arg(alpine);
                    cmd.arg("--").args(["echo", "ok"]);
                    cmd
                },
                MYC_TIMEOUT,
                hub,
            ));
        }
        Err(e) => sides.push(json!({ "label": "myc", "error": e })),
    }
    if docker_in_path() {
        // Warm the docker image cache first so the measured runs do not
        // pay the pull (the myc side got the same courtesy above).
        let mut pull = Command::new("docker");
        pull.arg("pull").arg(alpine);
        match run_timed(pull, PULL_TIMEOUT) {
            Ok(_) => {
                sides.push(measure_side(
                    "docker",
                    || {
                        let mut cmd = Command::new("docker");
                        cmd.arg("run").arg("--rm").arg(alpine).args(["echo", "ok"]);
                        cmd
                    },
                    MYC_TIMEOUT,
                    hub,
                ));
            }
            Err(e) => sides.push(json!({
                "label": "docker",
                "error": format!("docker pull {alpine} a échoué : {e}"),
            })),
        }
    } else {
        note = Some("docker introuvable — mesure myc seule".to_string());
    }
    results.push(json!({ "scenario": "run", "sides": sides, "note": note }));

    // ---- scenario "boot-microvm": real VM boot, engine kvm-microvm ----
    if kvm_ready(root) {
        let side = measure_side(
            "kvm-microvm",
            || {
                let mut cmd = Command::new(exe);
                cmd.arg("--store")
                    .arg(root)
                    .arg("run")
                    .arg("--engine")
                    .arg("kvm-microvm")
                    .arg(alpine);
                cmd.arg("--").args(["echo", "ok"]);
                cmd
            },
            VM_TIMEOUT,
            hub,
        );
        results.push(json!({ "scenario": "boot-microvm", "sides": [side] }));
    } else {
        results.push(json!({
            "scenario": "boot-microvm",
            "note": "engine kvm-microvm pas READY sur cette machine — scénario sauté (voir /api/engines)",
        }));
    }

    // A run where not a single side produced a number is a failed job
    // (the per-scenario errors explain why); otherwise it is done.
    let any_success = results.iter().any(|scenario| {
        scenario["sides"]
            .as_array()
            .is_some_and(|sides| sides.iter().any(|s| s.get("median_ms").is_some()))
    });
    if any_success {
        hub.finish(results);
    } else {
        hub.fail(
            "aucune mesure n'a abouti — voir results pour le détail par scénario".to_string(),
            results,
        );
    }
}

/// Make sure `reference` is in the store, ingesting it if needed (the
/// measured runs must not time network downloads).
fn ensure_in_store(root: &Path, reference: &str) -> Result<(), String> {
    let store = myc_store::Store::open(root).map_err(|e| format!("store: {e}"))?;
    let known = |name: &str| store.resolve(name).is_ok();
    if known(reference) {
        return Ok(());
    }
    if let Ok(image) = myc_oci::ImageReference::parse(reference) {
        if known(&image.canonical_name()) {
            return Ok(());
        }
    }
    myc_oci::ingest(
        &store,
        reference,
        &myc_oci::Platform::host(),
        &mut myc_oci::NoProgress,
    )
    .map(|_| ())
    .map_err(|e| format!("ingest {reference} : {e}"))
}

/// Run the command produced by `make_cmd` `RUNS` times, keep the
/// wall-clock durations of successful runs, and summarize them
/// (median/p95 computed here, from raw samples). The first hard failure
/// aborts the side — measuring 10 identical failures tells nothing new.
fn measure_side(
    label: &str,
    make_cmd: impl Fn() -> Command,
    timeout: Duration,
    hub: &BenchHub,
) -> Value {
    let mut samples: Vec<f64> = Vec::new();
    for i in 0..RUNS {
        match run_timed(make_cmd(), timeout) {
            Ok(duration) => samples.push(duration.as_secs_f64() * 1000.0),
            Err(e) => {
                let mut entry = json!({
                    "label": label,
                    "error": format!("échec au run {}/{} : {}", i + 1, RUNS, e),
                });
                if !samples.is_empty() {
                    entry
                        .as_object_mut()
                        .expect("side entry is an object")
                        .insert("measured_before_failure".into(), json!(samples.len()));
                }
                hub.tick();
                return entry;
            }
        }
        hub.tick();
    }
    match summarize_ms(&samples) {
        Some((median_ms, p95_ms, n)) => json!({
            "label": label,
            "median_ms": median_ms,
            "p95_ms": p95_ms,
            "n": n,
        }),
        None => json!({ "label": label, "error": "aucune mesure valide" }),
    }
}

/// Run one child to completion under a timeout. Success = exit code 0
/// within the budget; the whole process group is killed on timeout (a
/// `myc run` spawns a namespace tree that must not outlive its bench).
fn run_timed(mut cmd: Command, timeout: Duration) -> Result<Duration, String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let start = Instant::now();
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("lancement impossible : {e}"))?;
    let deadline = start + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(start.elapsed()),
            Ok(Some(status)) => {
                return Err(format!("code de sortie {}", status.code().unwrap_or(-1)))
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    #[cfg(unix)]
                    kill_group(child.id() as i32);
                    let _ = child.wait();
                    return Err(format!("timeout après {} s", timeout.as_secs()));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

#[cfg(unix)]
fn kill_group(pgid: i32) {
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(-pgid),
        nix::sys::signal::Signal::SIGKILL,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_matches_the_bench_math() {
        // Odd count: the middle value; even count: mean of the two middle.
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
        assert_eq!(median(&[]), None);
        // Unsorted input must not matter.
        assert_eq!(median(&[9.0, 1.0, 5.0]), Some(5.0));
    }

    #[test]
    fn p95_is_nearest_rank_like_the_bench_scripts() {
        // n=10: rank = ceil(9.5) = 10 → the max. This is exactly how the
        // archived verdicts compute p95 (restore.times: 160.867 ms).
        let ten: Vec<f64> = (1..=10).map(|i| i as f64).collect();
        assert_eq!(p95(&ten), Some(10.0));
        // n=20: rank = ceil(19) = 19 → the 19th ordered value, not the max.
        let twenty: Vec<f64> = (1..=20).map(|i| i as f64).collect();
        assert_eq!(p95(&twenty), Some(19.0));
        assert_eq!(p95(&[7.5]), Some(7.5));
        assert_eq!(p95(&[]), None);
    }

    #[test]
    fn parse_times_reads_nanosecond_lines_as_milliseconds() {
        let text = "323061571\n329020233\n\n347833558\n";
        let ms = parse_times(text);
        assert_eq!(ms, vec![323.061571, 329.020233, 347.833558]);
        assert!(parse_times("").is_empty());
        // The archived restore.times summarizes to the documented values.
        let restore = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../bench/snapshot-wsl2/results/restore.times"
        ))
        .unwrap_or_default();
        if !restore.is_empty() {
            assert_eq!(summarize_ms(&parse_times(&restore)), Some((91, 161, 10)));
        }
    }

    #[test]
    fn timestamps_are_utc_and_correct() {
        // 2024-11-04T00:00:00Z is unix second 1730678400.
        let t = UNIX_EPOCH + Duration::from_secs(1_730_678_400);
        assert_eq!(rfc3339(t), "2024-11-04T00:00:00Z");
        assert_eq!(date_of(t), "2024-11-04");
        // End of the same UTC day still lands on it.
        let late = UNIX_EPOCH + Duration::from_secs(1_730_678_400 + 86_399);
        assert_eq!(date_of(late), "2024-11-04");
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn snapshots_payload_shape_matches_the_contract() {
        let dir = tempfile::tempdir().unwrap();
        let payload = snapshots_payload(dir.path());
        for key in [
            "firecracker",
            "cloud_hypervisor_wsl2",
            "warm_pool",
            "measurements",
        ] {
            assert!(payload.get(key).is_some(), "missing '{key}' in {payload}");
        }
        assert!(payload["firecracker"]["available"].is_boolean());
        assert!(payload["warm_pool"]["available"].is_boolean());
        let measurements = payload["measurements"].as_array().unwrap();
        assert_eq!(measurements.len(), 3, "three documented measurements");
        for m in measurements {
            for key in ["label", "median_ms", "p95_ms", "n", "source"] {
                assert!(m.get(key).is_some(), "measurement missing '{key}': {m}");
            }
            assert!(m["n"].as_u64().unwrap() > 0);
        }
        // The fallback constants carry their docs source.
        let sources: Vec<&str> = measurements
            .iter()
            .map(|m| m["source"].as_str().unwrap())
            .collect();
        if !sources.iter().any(|s| s.starts_with("bench/")) {
            assert!(sources.iter().all(|s| *s == "docs/isolation-engines.md"));
        }
    }

    #[test]
    fn measurement_entry_prefers_real_numbers_over_constants() {
        let real = measurement_entry(
            "restore Firecracker 512Mo (WSL2)",
            Some((91, 161, 10)),
            Some("2026-11-04".to_string()),
            "bench/snapshot-wsl2",
        );
        assert_eq!(real["median_ms"], json!(91));
        assert_eq!(real["p95_ms"], json!(161));
        assert_eq!(real["n"], json!(10));
        assert_eq!(real["when"], json!("2026-11-04"));
        assert_eq!(real["source"], json!("bench/snapshot-wsl2"));

        let doc = measurement_entry("handoff pool chaud", None, None, "bench/warm-pool");
        assert_eq!(doc["median_ms"], json!(28));
        assert_eq!(doc["p95_ms"], json!(61));
        assert_eq!(doc["n"], json!(30));
        assert_eq!(doc["source"], json!("docs/isolation-engines.md"));
        assert!(doc.get("when").is_none());
    }

    #[test]
    fn job_status_round_trips_the_phases() {
        let hub = BenchHub::new();
        assert_eq!(hub.to_json()["state"], json!("idle"));
        assert_eq!(hub.to_json()["results"], json!([]));
        hub.start().unwrap();
        assert_eq!(hub.to_json()["state"], json!("running"));
        assert!(hub.start().is_err(), "no second concurrent job");
        hub.tick();
        hub.tick();
        assert_eq!(hub.to_json()["progress"], json!(2));
        hub.finish(vec![json!({ "scenario": "run" })]);
        let done = hub.to_json();
        assert_eq!(done["state"], json!("done"));
        assert_eq!(done["progress"], json!(2));
        assert_eq!(done["results"][0]["scenario"], json!("run"));
        assert!(done["when"].is_string());
        // A failure keeps its results and explains itself.
        hub.fail(
            "boom".to_string(),
            vec![json!({ "scenario": "run", "sides": [{ "label": "myc", "error": "x" }] })],
        );
        let failed = hub.to_json();
        assert_eq!(failed["state"], json!("error"));
        assert_eq!(failed["error"], json!("boom"));
        assert_eq!(failed["results"][0]["scenario"], json!("run"));
    }
}
