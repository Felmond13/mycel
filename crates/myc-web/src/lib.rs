//! Local web dashboard for the Mycel store.
//!
//! `serve` starts an HTTP server exposing a JSON API under `/api/` and a
//! single self-contained HTML dashboard at `/`. Everything is read through
//! the store API (`Store::resolve`, `Store::get_manifest`); no request
//! parameter is ever used as a filesystem path, so the server cannot be
//! walked out of the store directory.
//!
//! Containers started from the browser run as child processes of this
//! server (see [`containers`]); stacks are `mycel.toml` projects stored
//! under `<store>/stacks/` (see [`stacks`]).

mod containers;
mod metrics;
mod share;
mod stacks;
mod volumes;

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const INDEX_HTML: &str = include_str!("../assets/index.html");
const STYLE_CSS: &str = include_str!("../assets/style.css");
const VUE_JS: &str = include_str!("../assets/vendor/vue.global.prod.js");

/// The whole ES-module frontend, embedded at compile time. Adding a new
/// component file under `assets/js/` requires no route changes: requests to
/// `/js/{path}` are looked up as keys in this embedded map (never on the
/// filesystem).
static JS_ASSETS: include_dir::Dir<'_> = include_dir::include_dir!("$CARGO_MANIFEST_DIR/assets/js");

#[derive(Clone)]
struct App {
    root: PathBuf,
    jobs: Arc<Mutex<HashMap<u64, Job>>>,
    next_job: Arc<AtomicU64>,
    manager: Arc<containers::Manager>,
}

#[derive(Clone, Serialize)]
struct Job {
    image: String,
    /// `running`, `done` or `failed`.
    status: String,
    /// Unix seconds when the job was accepted (for the history list).
    created_at: u64,
    layers_total: usize,
    layers_done: usize,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    manifest_id: Option<String>,
    files: usize,
    logical_size: u64,
    bytes_added: u64,
}

/// One error shape for the whole API: `{"error": "..."}` + status code.
#[derive(Debug)]
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

fn bad_request(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}

fn not_found(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, msg.into())
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

type ApiResult = Result<Json<Value>, ApiError>;

/// References arriving over HTTP are only ever passed to `Store::resolve`,
/// but reject anything path-like outright as defense in depth.
fn check_reference(reference: &str) -> Result<(), ApiError> {
    let bad = reference.is_empty()
        || reference.len() > 512
        || reference.contains("..")
        || reference.contains('\\')
        || reference.starts_with('/');
    if bad {
        return Err(bad_request(format!("invalid reference '{reference}'")));
    }
    Ok(())
}

/// Run blocking store work on the blocking pool.
async fn blocking<T, F>(f: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ApiError> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| internal(format!("worker panicked: {e}")))?
}

fn open_store(root: &std::path::Path) -> Result<myc_store::Store, ApiError> {
    myc_store::Store::open(root).map_err(internal)
}

fn resolve(
    store: &myc_store::Store,
    reference: &str,
) -> Result<(String, myc_manifest::Manifest), ApiError> {
    let id = store
        .resolve(reference)
        .map_err(|_| not_found(format!("'{reference}' not found in store")))?;
    let manifest = store.get_manifest(&id).map_err(internal)?;
    Ok((id, manifest))
}

// ---- handlers ----

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

/// Embedded static asset with the right content type (all assets are
/// compiled into the binary; no filesystem access, no path parameters).
fn static_asset(content_type: &'static str, body: &'static str) -> Response {
    ([(axum::http::header::CONTENT_TYPE, content_type)], body).into_response()
}

async fn style_css() -> Response {
    static_asset("text/css; charset=utf-8", STYLE_CSS)
}

async fn vue_js() -> Response {
    static_asset("application/javascript; charset=utf-8", VUE_JS)
}

/// Serve one file of the embedded `assets/js/` tree. The request path is
/// only ever used as a lookup key into the compile-time map.
async fn js_asset(UrlPath(path): UrlPath<String>) -> Response {
    match JS_ASSETS.get_file(&path) {
        Some(file) if path.ends_with(".js") => static_asset(
            "application/javascript; charset=utf-8",
            file.contents_utf8().unwrap_or_default(),
        ),
        _ => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("no such asset '/js/{path}'") })),
        )
            .into_response(),
    }
}

async fn list_envs(State(app): State<App>) -> ApiResult {
    blocking(move || {
        let store = open_store(&app.root)?;
        let pins = store.pins().map_err(internal)?;
        let refs = store.list_refs().map_err(internal)?;
        let envs: Vec<Value> = store
            .list_manifests()
            .map_err(internal)?
            .into_iter()
            .map(|(id, m)| {
                let names: Vec<&str> = refs
                    .iter()
                    .filter(|(_, target)| *target == id)
                    .map(|(name, _)| name.as_str())
                    .collect();
                json!({
                    "id": id,
                    "name": m.name,
                    "refs": names,
                    "os": m.os,
                    "arch": m.arch,
                    "created": m.created,
                    "files": m.file_count(),
                    "logical_size": m.logical_size(),
                    "pinned": pins.contains(&id),
                })
            })
            .collect();
        Ok(Json(json!({ "environments": envs })))
    })
    .await
}

async fn env_detail(State(app): State<App>, UrlPath(id): UrlPath<String>) -> ApiResult {
    check_reference(&id)?;
    blocking(move || {
        let store = open_store(&app.root)?;
        let (id, m) = resolve(&store, &id)?;
        let pinned = store.pins().map_err(internal)?.contains(&id);
        let missing = store.missing_blobs(&m).len();
        // Shortest ref name makes the friendliest copy-paste command.
        let run_name = store
            .list_refs()
            .map_err(internal)?
            .into_iter()
            .filter(|(_, target)| *target == id)
            .map(|(name, _)| name)
            .min_by_key(String::len)
            .unwrap_or_else(|| id.clone());
        let entries: Vec<Value> = m
            .entries
            .iter()
            .map(|e| {
                json!({
                    "path": e.path,
                    "kind": e.kind,
                    "mode": format!("{:o}", e.mode),
                    "size": e.size,
                    "blake3": e.blake3,
                    "target": e.target,
                })
            })
            .collect();
        Ok(Json(json!({
            "id": id,
            "name": m.name,
            "origin": m.origin,
            "os": m.os,
            "arch": m.arch,
            "created": m.created,
            "files": m.file_count(),
            "logical_size": m.logical_size(),
            "pinned": pinned,
            "missing_blobs": missing,
            "run_command": format!("myc run {run_name}"),
            "shell_command": format!("myc shell {run_name}"),
            "config": {
                "env": m.config.env,
                "entrypoint": m.config.entrypoint,
                "cmd": m.config.cmd,
                "workdir": m.config.workdir,
                "user": m.config.user,
                "exposed_ports": m.config.exposed_ports,
                "volumes": m.config.volumes,
            },
            "entries": entries,
        })))
    })
    .await
}

async fn env_sbom(State(app): State<App>, UrlPath(id): UrlPath<String>) -> ApiResult {
    check_reference(&id)?;
    blocking(move || {
        let store = open_store(&app.root)?;
        let (id, m) = resolve(&store, &id)?;
        let sbom = myc_query::sbom(&store, &id, &m);
        Ok(Json(serde_json::to_value(sbom).map_err(internal)?))
    })
    .await
}

async fn pin_env(State(app): State<App>, UrlPath(id): UrlPath<String>) -> ApiResult {
    set_pin(app, id, true).await
}

async fn unpin_env(State(app): State<App>, UrlPath(id): UrlPath<String>) -> ApiResult {
    set_pin(app, id, false).await
}

async fn set_pin(app: App, id: String, pin: bool) -> ApiResult {
    check_reference(&id)?;
    blocking(move || {
        let store = open_store(&app.root)?;
        let (id, _) = resolve(&store, &id)?;
        if pin {
            store.pin(&id).map_err(internal)?;
        } else {
            store.unpin(&id).map_err(internal)?;
        }
        Ok(Json(json!({ "id": id, "pinned": pin })))
    })
    .await
}

async fn remove_env(State(app): State<App>, UrlPath(id): UrlPath<String>) -> ApiResult {
    check_reference(&id)?;
    blocking(move || {
        let store = open_store(&app.root)?;
        let (id, _) = resolve(&store, &id)?;
        store.remove_manifest(&id).map_err(internal)?;
        Ok(Json(json!({ "removed": id })))
    })
    .await
}

async fn stats(State(app): State<App>) -> ApiResult {
    blocking(move || {
        let store = open_store(&app.root)?;
        let manifests = store.list_manifests().map_err(internal)?;
        let blob_stats = store.blob_stats().map_err(internal)?;
        let logical: u64 = manifests.iter().map(|(_, m)| m.logical_size()).sum();
        let dedup = myc_query::dedup_report(&manifests);
        let ratio = if blob_stats.bytes > 0 {
            logical as f64 / blob_stats.bytes as f64
        } else {
            1.0
        };
        Ok(Json(json!({
            "environments": manifests.len(),
            "blobs": blob_stats.count,
            "physical_bytes": blob_stats.bytes,
            "logical_bytes": logical,
            "dedup_ratio": ratio,
            "saved_bytes": logical.saturating_sub(blob_stats.bytes),
            "per_environment": dedup,
        })))
    })
    .await
}

#[derive(Deserialize)]
struct DiffParams {
    a: String,
    b: String,
}

async fn diff(State(app): State<App>, Query(params): Query<DiffParams>) -> ApiResult {
    check_reference(&params.a)?;
    check_reference(&params.b)?;
    blocking(move || {
        let store = open_store(&app.root)?;
        let (id_a, ma) = resolve(&store, &params.a)?;
        let (id_b, mb) = resolve(&store, &params.b)?;
        let report = myc_query::diff(&ma, &mb);
        Ok(Json(json!({
            "a": { "id": id_a, "name": ma.name },
            "b": { "id": id_b, "name": mb.name },
            "report": report,
        })))
    })
    .await
}

#[derive(Deserialize)]
struct WhichParams {
    q: String,
}

async fn which(State(app): State<App>, Query(params): Query<WhichParams>) -> ApiResult {
    if params.q.is_empty() {
        return Err(bad_request("query is empty"));
    }
    blocking(move || {
        let store = open_store(&app.root)?;
        let manifests = store.list_manifests().map_err(internal)?;
        let hits = myc_query::which(&manifests, &params.q);
        Ok(Json(json!({
            "query": params.q,
            "count": hits.len(),
            "hits": hits,
        })))
    })
    .await
}

async fn gc(State(app): State<App>) -> ApiResult {
    blocking(move || {
        let store = open_store(&app.root)?;
        let (deleted, freed) = store.gc().map_err(internal)?;
        Ok(Json(json!({ "deleted": deleted, "freed_bytes": freed })))
    })
    .await
}

async fn doctor(State(app): State<App>) -> ApiResult {
    blocking(move || {
        let checks = myc_query::run_checks(&app.root);
        let ok = checks.iter().all(|c| c.ok);
        Ok(Json(json!({ "ok": ok, "checks": checks })))
    })
    .await
}

#[derive(Deserialize)]
struct IngestBody {
    image: String,
}

/// Progress sink writing into the shared job table so the frontend can poll.
struct JobProgress {
    jobs: Arc<Mutex<HashMap<u64, Job>>>,
    id: u64,
}

impl JobProgress {
    fn update(&self, f: impl FnOnce(&mut Job)) {
        if let Some(job) = self.jobs.lock().unwrap().get_mut(&self.id) {
            f(job);
        }
    }
}

impl myc_oci::IngestProgress for JobProgress {
    fn layer_started(&mut self, index: usize, total: usize, digest: &str) {
        let short: String = digest
            .strip_prefix("sha256:")
            .unwrap_or(digest)
            .chars()
            .take(12)
            .collect();
        self.update(|job| {
            job.layers_total = total;
            job.layers_done = index;
            job.message = format!("downloading layer {}/{total} ({short}…)", index + 1);
        });
    }
    fn layer_done(&mut self, index: usize, files_added: usize, bytes_added: u64) {
        self.update(|job| {
            job.layers_done = index + 1;
            job.files += files_added;
            job.bytes_added += bytes_added;
        });
    }
}

async fn ingest(State(app): State<App>, Json(body): Json<IngestBody>) -> ApiResult {
    let image = body.image.trim().to_string();
    check_reference(&image)?;
    myc_oci::ImageReference::parse(&image)
        .map_err(|e| bad_request(format!("invalid image reference: {e}")))?;

    let job_id = app.next_job.fetch_add(1, Ordering::SeqCst);
    app.jobs.lock().unwrap().insert(
        job_id,
        Job {
            image: image.clone(),
            status: "running".to_string(),
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            layers_total: 0,
            layers_done: 0,
            message: "resolving image…".to_string(),
            manifest_id: None,
            files: 0,
            logical_size: 0,
            bytes_added: 0,
        },
    );

    let jobs = Arc::clone(&app.jobs);
    let root = app.root.clone();
    // Ingestion is fully blocking (streaming HTTP + tar) and can run for
    // minutes; a dedicated thread keeps it off the async worker pool.
    std::thread::spawn(move || {
        let mut progress = JobProgress {
            jobs: Arc::clone(&jobs),
            id: job_id,
        };
        let result = myc_store::Store::open(&root)
            .map_err(|e| e.to_string())
            .and_then(|store| {
                myc_oci::ingest(&store, &image, &myc_oci::Platform::host(), &mut progress)
                    .map_err(|e| e.to_string())
            });
        let mut jobs = jobs.lock().unwrap();
        if let Some(job) = jobs.get_mut(&job_id) {
            match result {
                Ok(report) => {
                    job.status = "done".to_string();
                    job.message = format!("ingested {}", report.name);
                    job.manifest_id = Some(report.manifest_id);
                    job.files = report.file_count;
                    job.logical_size = report.logical_size;
                    job.bytes_added = report.bytes_added;
                }
                Err(e) => {
                    job.status = "failed".to_string();
                    job.message = humanize_ingest_error(&e, &image);
                }
            }
        }
    });

    Ok(Json(json!({ "job": job_id })))
}

/// Translate raw registry errors into one human sentence. Errors that
/// `myc-oci` already made friendly (its `ImageNotFound` includes typo
/// hints) are passed through untouched.
fn humanize_ingest_error(raw: &str, image: &str) -> String {
    if raw.contains("check the name") {
        return raw.to_string();
    }
    let lower = raw.to_lowercase();
    if lower.contains("404")
        || lower.contains("not found")
        || lower.contains("manifest unknown")
        || lower.contains("name unknown")
        || lower.contains("401")
        || lower.contains("unauthorized")
        || lower.contains("denied")
    {
        return format!(
            "'{image}' was not found on the registry — check the spelling and tag \
             (for example `nginx:latest`), or it may be private"
        );
    }
    if lower.contains("dns")
        || lower.contains("timed out")
        || lower.contains("connection refused")
        || lower.contains("connection reset")
        || lower.contains("network")
    {
        return format!(
            "cannot reach the registry to download '{image}' — check your internet connection \
             and try again ({raw})"
        );
    }
    raw.to_string()
}

async fn job_status(State(app): State<App>, UrlPath(id): UrlPath<u64>) -> ApiResult {
    let jobs = app.jobs.lock().unwrap();
    match jobs.get(&id) {
        Some(job) => Ok(Json(serde_json::to_value(job).map_err(internal)?)),
        None => Err(not_found(format!("no job {id}"))),
    }
}

/// The 20 most recent ingest jobs, newest first (ids grow monotonically),
/// so the Install page can show a session history.
async fn list_jobs(State(app): State<App>) -> ApiResult {
    let jobs = app.jobs.lock().unwrap();
    let mut items: Vec<(&u64, &Job)> = jobs.iter().collect();
    items.sort_by_key(|(id, _)| std::cmp::Reverse(**id));
    let out: Vec<Value> = items
        .into_iter()
        .take(20)
        .map(|(id, job)| {
            let mut v = serde_json::to_value(job).unwrap_or_else(|_| json!({}));
            v.as_object_mut()
                .expect("job serializes to an object")
                .insert("id".into(), json!(id));
            v
        })
        .collect();
    Ok(Json(json!({ "jobs": out })))
}

// ---- containers ----

/// Resolve a reference that must already be in the store, also trying its
/// canonical image name (`nginx:latest` -> `docker.io/library/nginx:latest`).
fn resolve_installed(store: &myc_store::Store, reference: &str) -> Result<String, ApiError> {
    if let Ok(id) = store.resolve(reference) {
        return Ok(id);
    }
    if let Ok(image) = myc_oci::ImageReference::parse(reference) {
        if let Ok(id) = store.resolve(&image.canonical_name()) {
            return Ok(id);
        }
    }
    Err(not_found(format!(
        "'{reference}' is not installed yet — install it first (Apps page or Install tab)"
    )))
}

/// Human-friendly container/hostname: lowercase alnum plus `-`, non-empty.
fn sanitize_name(input: &str) -> String {
    let cleaned: String = input
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('-').to_string();
    let mut out: String = cleaned.chars().take(40).collect();
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "app".to_string()
    } else {
        out
    }
}

/// Default container name derived from an image reference:
/// `docker.io/library/nginx:latest` -> `nginx`.
fn name_from_reference(reference: &str) -> String {
    let base = reference
        .rsplit('/')
        .next()
        .unwrap_or(reference)
        .split(':')
        .next()
        .unwrap_or(reference);
    sanitize_name(base)
}

/// Refuse to start a container whose ports are already taken on the host.
///
/// Containers share the host network namespace, so an app whose port is
/// busy will start, fail to bind and die with a cryptic `exited(1)` —
/// catching it here turns that into an actionable error *before* anything
/// is launched. Checked ports are the image's declared `ExposedPorts`
/// plus the user-declared service port, against the live listener set
/// parsed from `/proc/net/tcp{,6}`.
fn check_ports_free(
    manifest: &myc_manifest::Manifest,
    declared: Option<u16>,
) -> Result<(), ApiError> {
    let mut ports = manifest.config.exposed_ports.clone();
    if let Some(p) = declared {
        if !ports.contains(&p) {
            ports.push(p);
        }
    }
    if ports.is_empty() {
        return Ok(());
    }
    let listening = containers::listening_ports();
    if let Some(port) = ports.iter().find(|p| listening.contains(p)) {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!(
                "Port {port} is already in use by another program — probably another \
                 copy of this app. Stop the other one (or free the port), then try again."
            ),
        ));
    }
    Ok(())
}

// ---- network planning ----

/// Can this machine run containers in an isolated network? (pasta from
/// the `passt` package must be installed.)
fn network_isolation_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        myc_run::pasta_path().is_some()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// The network half of a start decision: host network (the default), or
/// an isolated namespace with explicit host→container port mappings.
#[derive(Debug, PartialEq)]
struct PortPlan {
    isolated: bool,
    ports: Vec<containers::PortMapping>,
    /// Host port answering for the user-declared service port (drives the
    /// dashboard's "Open" link).
    open_port: Option<u16>,
    /// Plain-language note when a busy port was remapped automatically.
    note: Option<String>,
}

/// First free host port after `preferred` (not listening, not already
/// chosen by this same plan).
fn pick_free_port(
    preferred: u16,
    listening: &std::collections::HashSet<u16>,
    chosen: &std::collections::HashSet<u16>,
) -> Option<u16> {
    ((preferred.saturating_add(1))..=u16::MAX)
        .find(|p| !listening.contains(p) && !chosen.contains(p))
}

/// Decide how a container joins the network.
///
/// Autonomy first: on the default path (`isolate_requested` false) the
/// container shares the host network, and a busy port — instead of the
/// old flat refusal — silently moves the app into an isolated network
/// with the busy port published on the nearest free one (6379 taken →
/// localhost:6380). The `note` tells the user in one sentence. When
/// isolation is not available on the machine, the refusal stays, plus an
/// installation hint.
///
/// `requested` carries the user's own host-port choices from the start
/// dialog (isolated mode); busy choices are bumped the same way.
fn plan_ports(
    exposed: &[u16],
    declared: Option<u16>,
    isolate_requested: bool,
    requested: &[containers::PortMapping],
    listening: &std::collections::HashSet<u16>,
    isolation_available: bool,
) -> Result<PortPlan, ApiError> {
    // Container-side ports the app is expected to serve.
    let mut wanted: Vec<u16> = exposed.to_vec();
    if let Some(p) = declared {
        if !wanted.contains(&p) {
            wanted.push(p);
        }
    }
    for m in requested {
        if !wanted.contains(&m.container) {
            wanted.push(m.container);
        }
    }

    if !isolate_requested {
        let busy: Vec<u16> = wanted
            .iter()
            .copied()
            .filter(|p| listening.contains(p))
            .collect();
        if busy.is_empty() {
            return Ok(PortPlan {
                isolated: false,
                ports: Vec::new(),
                open_port: declared,
                note: None,
            });
        }
        if !isolation_available {
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!(
                    "Port {} is already in use by another program — probably another \
                     copy of this app. Stop the other one (or free the port), then try \
                     again. Tip: install the 'passt' package (sudo apt install passt) \
                     and Mycel will move the app to a nearby free port automatically \
                     instead.",
                    busy[0]
                ),
            ));
        }
    }

    // Isolated: publish every wanted port, preferring the user's choice,
    // then the same-numbered host port, then the nearest free one.
    let mut ports = Vec::new();
    let mut chosen = std::collections::HashSet::new();
    let mut notes = Vec::new();
    if isolate_requested && !isolation_available {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Network isolation needs the 'passt' package on this computer — \
             run: sudo apt install passt"
                .to_string(),
        ));
    }
    for container in wanted {
        let preferred = requested
            .iter()
            .find(|m| m.container == container)
            .map(|m| m.host)
            .unwrap_or(container);
        let host = if !listening.contains(&preferred) && !chosen.contains(&preferred) {
            preferred
        } else {
            let bumped = pick_free_port(preferred, listening, &chosen)
                .ok_or_else(|| internal(format!("no free port found above {preferred}")))?;
            notes.push(format!(
                "Port {preferred} was busy — your app is available on \
                 localhost:{bumped} instead."
            ));
            bumped
        };
        chosen.insert(host);
        ports.push(containers::PortMapping { host, container });
    }
    let open_port = declared
        .and_then(|d| ports.iter().find(|m| m.container == d).map(|m| m.host))
        .or(declared);
    Ok(PortPlan {
        isolated: true,
        ports,
        open_port,
        note: if notes.is_empty() {
            None
        } else {
            Some(notes.join(" "))
        },
    })
}

fn check_env_entries(env: &[String]) -> Result<(), ApiError> {
    for kv in env {
        if !kv.contains('=') || kv.starts_with('=') || kv.len() > 4096 {
            return Err(bad_request(format!("env entry '{kv}' is not KEY=VALUE")));
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct StartContainerBody {
    reference: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    command: Vec<String>,
    #[serde(default)]
    env: Vec<String>,
    #[serde(default)]
    workdir: Option<String>,
    #[serde(default)]
    map_user: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    /// Persist the app's declared data directories across restarts by
    /// mapping them to stable named volumes (default: true — data should
    /// survive without anyone learning what a volume is).
    #[serde(default)]
    keep_data: Option<bool>,
    /// Advanced: store the app's data in this folder of the computer
    /// instead of a managed volume (absolute path).
    #[serde(default)]
    data_folder: Option<String>,
    /// Advanced: give the app its own private network (requires pasta).
    #[serde(default)]
    isolate_network: Option<bool>,
    /// With `isolate_network`: the user's host-port choices, one per
    /// container port (`[{"host": 6380, "container": 6379}]`). Ports the
    /// app declares but that are missing here keep their own number.
    #[serde(default)]
    ports: Vec<containers::PortMapping>,
}

/// `UID[:GID]`, both numeric — anything else is rejected.
fn check_map_user(spec: &str) -> Result<(), ApiError> {
    let ok = match spec.split_once(':') {
        Some((u, g)) => u.parse::<u32>().is_ok() && g.parse::<u32>().is_ok(),
        None => spec.parse::<u32>().is_ok(),
    };
    if ok {
        Ok(())
    } else {
        Err(bad_request(format!(
            "invalid map_user '{spec}' (expected UID or UID:GID, numeric)"
        )))
    }
}

async fn start_container(
    State(app): State<App>,
    Json(body): Json<StartContainerBody>,
) -> ApiResult {
    let reference = body.reference.trim().to_string();
    check_reference(&reference)?;
    check_env_entries(&body.env)?;
    if body.command.len() > 64 || body.command.iter().any(|c| c.len() > 4096) {
        return Err(bad_request("command is too long"));
    }
    if let Some(map) = &body.map_user {
        check_map_user(map)?;
    }
    blocking(move || {
        let store = open_store(&app.root)?;
        let manifest_id = resolve_installed(&store, &reference)?;
        let manifest = store.get_manifest(&manifest_id).map_err(internal)?;
        // Busy ports no longer refuse outright: when pasta is available
        // the app is moved into an isolated network with the busy port
        // published on the nearest free one (see `plan_ports`).
        let plan = plan_ports(
            &manifest.config.exposed_ports,
            body.port,
            body.isolate_network.unwrap_or(false),
            &body.ports,
            &containers::listening_ports(),
            network_isolation_available(),
        )?;
        let name = match &body.name {
            Some(n) if !n.trim().is_empty() => sanitize_name(n),
            _ => name_from_reference(&reference),
        };
        // Zero-concept persistence: unless opted out, every data directory
        // the image declares is mapped to a stable named volume derived
        // from the container name (`redis` -> `redis-data`), so the same
        // app finds the same data on every start.
        let binds = volumes::auto_data_binds(
            &store,
            &manifest,
            &name,
            &reference,
            body.keep_data.unwrap_or(true),
            body.data_folder.as_deref().filter(|f| !f.trim().is_empty()),
        )?;
        let spec = containers::ContainerSpec {
            reference,
            manifest_id,
            name,
            command: body.command,
            env: body.env,
            binds,
            workdir: body.workdir.filter(|w| !w.trim().is_empty()),
            map_user: body.map_user,
            port: plan.open_port,
            isolated_network: plan.isolated,
            ports: plan.ports,
            stack: None,
            service: None,
        };
        let id = app.manager.start(spec).map_err(internal)?;
        let mut container = app
            .manager
            .get(&id)
            .ok_or_else(|| internal("container vanished"))?;
        if let Some(note) = plan.note {
            container
                .as_object_mut()
                .expect("container json is an object")
                .insert("port_note".into(), json!(note));
        }
        Ok(Json(container))
    })
    .await
}

/// What this Mycel installation can do — the UI adapts to it (e.g. the
/// network-isolation toggle in the start dialog).
async fn capabilities() -> ApiResult {
    Ok(Json(json!({
        "network_isolation": network_isolation_available(),
    })))
}

async fn list_containers(State(app): State<App>) -> ApiResult {
    // Listing samples /proc (and occasionally walks a rootfs for disk
    // usage) — keep it off the async workers.
    blocking(move || Ok(Json(json!({ "containers": app.manager.list() })))).await
}

/// Live process tree of one container: `{pid, name, cpu_percent,
/// memory_bytes}` per process, memory-heaviest first. CPU is percent of a
/// single core, measured since the previous scan (first call reports 0).
async fn container_processes(State(app): State<App>, UrlPath(id): UrlPath<String>) -> ApiResult {
    blocking(move || {
        let procs = app.manager.processes(&id).map_err(not_found)?;
        let running = app
            .manager
            .get(&id)
            .map(|c| c["running"] == json!(true))
            .unwrap_or(false);
        Ok(Json(json!({ "processes": procs, "running": running })))
    })
    .await
}

#[derive(Deserialize)]
struct LogParams {
    #[serde(default)]
    offset: u64,
}

async fn container_logs(
    State(app): State<App>,
    UrlPath(id): UrlPath<String>,
    Query(params): Query<LogParams>,
) -> ApiResult {
    let container = app
        .manager
        .get(&id)
        .ok_or_else(|| not_found(format!("no container {id}")))?;
    let (data, next, size) = app.manager.logs(&id, params.offset).map_err(not_found)?;
    Ok(Json(json!({
        "data": data,
        "next": next,
        "size": size,
        "status": container["status"],
        "running": container["running"],
        "exit_code": container["exit_code"],
    })))
}

async fn stop_container(State(app): State<App>, UrlPath(id): UrlPath<String>) -> ApiResult {
    // stop() blocks up to ~7s (SIGTERM, then SIGKILL after the grace
    // period) — run it on the blocking pool.
    blocking(move || app.manager.stop(&id).map(Json).map_err(not_found)).await
}

async fn restart_container(State(app): State<App>, UrlPath(id): UrlPath<String>) -> ApiResult {
    blocking(move || {
        let mut spec = app
            .manager
            .spec_of(&id)
            .ok_or_else(|| not_found(format!("no container {id}")))?;
        app.manager.stop(&id).map_err(not_found)?;
        let _ = app.manager.remove(&id);
        // Re-plan the network after the stop (the outgoing container has
        // released its own ports by now): original ports are kept when
        // still free, busy ones are bumped, and a host-network container
        // whose port got taken in the meantime is auto-isolated.
        let store = open_store(&app.root)?;
        let mut note = None;
        if let Ok(manifest) = store.get_manifest(&spec.manifest_id) {
            // `spec.port` is host-side; the planner wants the app's own
            // (container-side) port number.
            let declared = spec
                .ports
                .iter()
                .find(|m| Some(m.host) == spec.port)
                .map(|m| m.container)
                .or(spec.port);
            let plan = plan_ports(
                &manifest.config.exposed_ports,
                declared,
                spec.isolated_network,
                &spec.ports,
                &containers::listening_ports(),
                network_isolation_available(),
            )?;
            spec.port = plan.open_port;
            spec.isolated_network = plan.isolated;
            spec.ports = plan.ports;
            note = plan.note;
        }
        let new_id = app.manager.start(spec).map_err(internal)?;
        let mut container = app
            .manager
            .get(&new_id)
            .ok_or_else(|| internal("container vanished"))?;
        if let Some(note) = note {
            container
                .as_object_mut()
                .expect("container json is an object")
                .insert("port_note".into(), json!(note));
        }
        Ok(Json(container))
    })
    .await
}

async fn remove_container(State(app): State<App>, UrlPath(id): UrlPath<String>) -> ApiResult {
    app.manager.remove(&id).map_err(bad_request)?;
    Ok(Json(json!({ "removed": id })))
}

// ---- stacks ----

fn check_stack_name(name: &str) -> Result<(), ApiError> {
    if stacks::valid_stack_name(name) {
        Ok(())
    } else {
        Err(bad_request(
            "invalid stack name (use 1-32 lowercase letters, digits, - and _)",
        ))
    }
}

async fn list_stacks(State(app): State<App>) -> ApiResult {
    blocking(move || {
        let mut out = Vec::new();
        for name in stacks::list_stacks(&app.root) {
            match stacks::load_stack(&app.root, &name) {
                Ok((project, _)) => {
                    let running = project
                        .services
                        .keys()
                        .filter(|svc| {
                            app.manager
                                .stack_service_state(&name, svc)
                                .map(|s| s["running"] == json!(true))
                                .unwrap_or(false)
                        })
                        .count();
                    out.push(json!({
                        "name": name,
                        "services": project.services.len(),
                        "running": running,
                        "images": project.services.values().map(|s| s.image.clone()).collect::<Vec<_>>(),
                    }));
                }
                Err(e) => out.push(json!({ "name": name, "error": e })),
            }
        }
        Ok(Json(json!({ "stacks": out })))
    })
    .await
}

fn stack_detail_json(app: &App, name: &str) -> Result<Value, ApiError> {
    let (project, text) = stacks::load_stack(&app.root, name).map_err(not_found)?;
    let order = project.start_order().map_err(internal)?;
    let store = open_store(&app.root)?;
    let services: Vec<Value> = order
        .iter()
        .map(|svc_name| {
            let svc = &project.services[svc_name];
            let installed = resolve_installed(&store, &svc.image).is_ok();
            json!({
                "name": svc_name,
                "image": svc.image,
                "command": svc.command,
                "env": svc.env,
                "depends_on": svc.depends_on,
                "workdir": svc.workdir,
                "installed": installed,
                "state": app.manager.stack_service_state(name, svc_name),
            })
        })
        .collect();
    Ok(json!({
        "name": name,
        "toml": text,
        "order": order,
        "services": services,
    }))
}

async fn stack_detail(State(app): State<App>, UrlPath(name): UrlPath<String>) -> ApiResult {
    check_stack_name(&name)?;
    blocking(move || Ok(Json(stack_detail_json(&app, &name)?))).await
}

#[derive(Deserialize)]
struct SaveStackBody {
    /// Raw `mycel.toml` text (advanced editor)…
    #[serde(default)]
    toml: Option<String>,
    /// …or the structured services from the visual builder.
    #[serde(default)]
    services: Option<std::collections::BTreeMap<String, stacks::ServiceInput>>,
}

async fn save_stack(
    State(app): State<App>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<SaveStackBody>,
) -> ApiResult {
    check_stack_name(&name)?;
    blocking(move || {
        let project = match (body.toml, body.services) {
            (Some(text), _) => myc_compose::ProjectFile::parse(&text)
                .map_err(|e| bad_request(format!("invalid mycel.toml: {e}")))?,
            (None, Some(services)) => {
                stacks::project_from_services(&name, services).map_err(bad_request)?
            }
            (None, None) => return Err(bad_request("provide either 'toml' or 'services'")),
        };
        let written = stacks::save_stack(&app.root, &name, &project).map_err(bad_request)?;
        Ok(Json(json!({ "name": name, "toml": written })))
    })
    .await
}

async fn delete_stack(State(app): State<App>, UrlPath(name): UrlPath<String>) -> ApiResult {
    check_stack_name(&name)?;
    blocking(move || {
        // Stop anything the stack is running, then delete its directory.
        for id in app.manager.stack_containers(&name, None) {
            let _ = app.manager.stop(&id);
            let _ = app.manager.remove(&id);
        }
        stacks::delete_stack(&app.root, &name).map_err(not_found)?;
        Ok(Json(json!({ "deleted": name })))
    })
    .await
}

async fn stack_up(State(app): State<App>, UrlPath(name): UrlPath<String>) -> ApiResult {
    check_stack_name(&name)?;
    blocking(move || {
        let (project, _) = stacks::load_stack(&app.root, &name).map_err(not_found)?;
        let order = project.start_order().map_err(internal)?;
        let store = open_store(&app.root)?;

        // Resolve every image up front so startup is all-or-nothing.
        let mut resolved = HashMap::new();
        let mut missing = Vec::new();
        for (svc_name, svc) in &project.services {
            match resolve_installed(&store, &svc.image) {
                Ok(id) => {
                    resolved.insert(svc_name.clone(), id);
                }
                Err(_) => missing.push(svc.image.clone()),
            }
        }
        if !missing.is_empty() {
            missing.sort();
            missing.dedup();
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!(
                    "these apps are not installed yet: {} — install them first",
                    missing.join(", ")
                ),
            ));
        }

        let stack_base = stacks::stack_dir(&app.root, &name);
        let mut started = Vec::new();
        let mut skipped = Vec::new();
        for svc_name in &order {
            let svc = &project.services[svc_name];
            let already_running = app
                .manager
                .stack_service_state(&name, svc_name)
                .map(|s| s["running"] == json!(true))
                .unwrap_or(false);
            if already_running {
                skipped.push(svc_name.clone());
                continue;
            }
            app.manager.prune_stack_service(&name, svc_name);
            let manifest = store.get_manifest(&resolved[svc_name]).ok();
            if let Some(m) = &manifest {
                check_ports_free(m, None)
                    .map_err(|e| ApiError(e.0, format!("cannot start '{svc_name}': {}", e.1)))?;
            }
            let created_for = format!("stack {name}/{svc_name}");
            let mut binds = Vec::new();
            for spec in &svc.binds {
                binds.push(volumes::resolve_stack_mount(
                    &store,
                    spec,
                    &stack_base,
                    &created_for,
                )?);
            }
            // Declared data directories not covered by an explicit bind
            // get stable named volumes, so stack data survives down/up.
            if let Some(m) = &manifest {
                binds.extend(volumes::stack_auto_binds(
                    &store, m, &name, svc_name, &binds,
                ));
            }
            let container_name = svc
                .hostname
                .clone()
                .unwrap_or_else(|| format!("{name}-{svc_name}"));
            let spec = containers::ContainerSpec {
                reference: svc.image.clone(),
                manifest_id: resolved[svc_name].clone(),
                name: sanitize_name(&container_name),
                command: svc.command.clone(),
                env: svc.env.clone(),
                binds,
                workdir: svc.workdir.clone(),
                map_user: None,
                port: None,
                isolated_network: false,
                ports: Vec::new(),
                stack: Some(name.clone()),
                service: Some(svc_name.clone()),
            };
            app.manager.start(spec).map_err(internal)?;
            started.push(svc_name.clone());
        }
        Ok(Json(
            json!({ "started": started, "already_running": skipped }),
        ))
    })
    .await
}

async fn stack_down(State(app): State<App>, UrlPath(name): UrlPath<String>) -> ApiResult {
    check_stack_name(&name)?;
    blocking(move || {
        let mut stopped = 0;
        for id in app.manager.stack_containers(&name, None) {
            if let Ok(state) = app.manager.stop(&id) {
                if state["exit_code"] != json!(null) {
                    stopped += 1;
                }
            }
        }
        Ok(Json(json!({ "stopped": stopped })))
    })
    .await
}

async fn stack_restart_service(
    State(app): State<App>,
    UrlPath((name, svc)): UrlPath<(String, String)>,
) -> ApiResult {
    check_stack_name(&name)?;
    check_stack_name(&svc)?;
    blocking(move || {
        for id in app.manager.stack_containers(&name, Some(&svc)) {
            let _ = app.manager.stop(&id);
            let _ = app.manager.remove(&id);
        }
        // Rebuild the spec from the stack file so edits take effect.
        let (project, _) = stacks::load_stack(&app.root, &name).map_err(not_found)?;
        let service = project
            .services
            .get(&svc)
            .ok_or_else(|| not_found(format!("no service '{svc}' in stack '{name}'")))?;
        let store = open_store(&app.root)?;
        let manifest_id = resolve_installed(&store, &service.image)?;
        let manifest = store.get_manifest(&manifest_id).ok();
        if let Some(m) = &manifest {
            check_ports_free(m, None)?;
        }
        let container_name = service
            .hostname
            .clone()
            .unwrap_or_else(|| format!("{name}-{svc}"));
        let stack_base = stacks::stack_dir(&app.root, &name);
        let created_for = format!("stack {name}/{svc}");
        let mut binds = Vec::new();
        for spec in &service.binds {
            binds.push(volumes::resolve_stack_mount(
                &store,
                spec,
                &stack_base,
                &created_for,
            )?);
        }
        if let Some(m) = &manifest {
            binds.extend(volumes::stack_auto_binds(&store, m, &name, &svc, &binds));
        }
        let spec = containers::ContainerSpec {
            reference: service.image.clone(),
            manifest_id,
            name: sanitize_name(&container_name),
            command: service.command.clone(),
            env: service.env.clone(),
            binds,
            workdir: service.workdir.clone(),
            map_user: None,
            port: None,
            isolated_network: false,
            ports: Vec::new(),
            stack: Some(name.clone()),
            service: Some(svc.clone()),
        };
        let id = app.manager.start(spec).map_err(internal)?;
        let container = app
            .manager
            .get(&id)
            .ok_or_else(|| internal("container vanished"))?;
        Ok(Json(container))
    })
    .await
}

/// Start the dashboard on `127.0.0.1:port` and serve until interrupted.
pub fn serve(store_root: PathBuf, port: u16) -> anyhow::Result<()> {
    // Fail fast (before printing a URL) if the store cannot be opened.
    myc_store::Store::open(&store_root)?;

    // Containers are spawned by re-invoking this very binary (`myc run …`).
    let exe = std::env::current_exe()?;
    let manager = containers::Manager::new(exe, store_root.clone())?;

    let app = App {
        root: store_root,
        jobs: Arc::new(Mutex::new(HashMap::new())),
        next_job: Arc::new(AtomicU64::new(1)),
        manager: Arc::new(manager),
    };

    let router = Router::new()
        .route("/", get(index))
        .route("/style.css", get(style_css))
        .route("/vendor/vue.js", get(vue_js))
        .route("/js/{*path}", get(js_asset))
        .route("/api/envs", get(list_envs))
        .route("/api/envs/{id}", get(env_detail))
        .route("/api/envs/{id}", delete(remove_env))
        .route("/api/envs/{id}/sbom", get(env_sbom))
        .route("/api/envs/{id}/pin", post(pin_env))
        .route("/api/envs/{id}/unpin", post(unpin_env))
        .route("/api/stats", get(stats))
        .route("/api/diff", get(diff))
        .route("/api/which", get(which))
        .route("/api/gc", post(gc))
        .route("/api/doctor", get(doctor))
        .route("/api/capabilities", get(capabilities))
        .route("/api/ingest", post(ingest))
        .route("/api/export/{reference}", get(share::export_env))
        .route(
            "/api/import",
            post(share::import_upload).layer(axum::extract::DefaultBodyLimit::max(
                share::IMPORT_BODY_LIMIT,
            )),
        )
        .route("/api/hub/push", post(share::hub_push))
        .route("/api/hub/pull", post(share::hub_pull))
        .route("/api/jobs", get(list_jobs))
        .route("/api/jobs/{id}", get(job_status))
        .route("/api/containers", post(start_container))
        .route("/api/containers", get(list_containers))
        .route("/api/containers/{id}/processes", get(container_processes))
        .route("/api/containers/{id}/data", get(volumes::container_data))
        .route("/api/volumes", get(volumes::list_volumes))
        .route("/api/volumes/{name}", delete(volumes::delete_volume))
        .route("/api/containers/{id}/logs", get(container_logs))
        .route("/api/containers/{id}/stop", post(stop_container))
        .route("/api/containers/{id}/restart", post(restart_container))
        .route("/api/containers/{id}", delete(remove_container))
        .route("/api/stacks", get(list_stacks))
        .route("/api/stacks/{name}", get(stack_detail))
        .route("/api/stacks/{name}", axum::routing::put(save_stack))
        .route("/api/stacks/{name}", delete(delete_stack))
        .route("/api/stacks/{name}/up", post(stack_up))
        .route("/api/stacks/{name}/down", post(stack_down))
        .route(
            "/api/stacks/{name}/services/{svc}/restart",
            post(stack_restart_service),
        )
        .with_state(app);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|e| anyhow::anyhow!("cannot listen on 127.0.0.1:{port}: {e}"))?;
        println!("mycel dashboard running at http://localhost:{port}  (Ctrl-C to stop)");
        axum::serve(listener, router)
            .await
            .map_err(anyhow::Error::from)
    })
}

#[cfg(test)]
mod tests {
    use super::{pick_free_port, plan_ports, PortPlan};
    use crate::containers::PortMapping;
    use std::collections::HashSet;

    fn set(ports: &[u16]) -> HashSet<u16> {
        ports.iter().copied().collect()
    }

    fn map(host: u16, container: u16) -> PortMapping {
        PortMapping { host, container }
    }

    #[test]
    fn free_port_skips_listening_and_already_chosen() {
        let listening = set(&[6380, 6381]);
        let chosen = set(&[6382]);
        assert_eq!(pick_free_port(6379, &listening, &chosen), Some(6383));
        assert_eq!(pick_free_port(9000, &set(&[]), &set(&[])), Some(9001));
    }

    #[test]
    fn free_ports_stay_on_the_host_network() {
        let plan = plan_ports(&[6379], Some(6379), false, &[], &set(&[]), true).unwrap();
        assert_eq!(
            plan,
            PortPlan {
                isolated: false,
                ports: Vec::new(),
                open_port: Some(6379),
                note: None,
            }
        );
    }

    #[test]
    fn busy_port_is_remapped_when_isolation_is_available() {
        let plan = plan_ports(&[6379], Some(6379), false, &[], &set(&[6379]), true).unwrap();
        assert!(plan.isolated);
        assert_eq!(plan.ports, vec![map(6380, 6379)]);
        assert_eq!(plan.open_port, Some(6380));
        let note = plan.note.expect("expected a remap note");
        assert!(note.contains("6379 was busy"), "note: {note}");
        assert!(note.contains("localhost:6380"), "note: {note}");
    }

    #[test]
    fn busy_port_without_isolation_is_refused_with_hint() {
        let err = plan_ports(&[6379], None, false, &[], &set(&[6379]), false).unwrap_err();
        assert_eq!(err.0, axum::http::StatusCode::CONFLICT);
        assert!(err.1.contains("already in use"), "error: {}", err.1);
        assert!(err.1.contains("passt"), "error: {}", err.1);
    }

    #[test]
    fn explicit_isolation_honors_user_port_choices() {
        let plan = plan_ports(
            &[6379, 8001],
            Some(6379),
            true,
            &[map(7000, 6379)],
            &set(&[]),
            true,
        )
        .unwrap();
        assert!(plan.isolated);
        assert_eq!(plan.ports, vec![map(7000, 6379), map(8001, 8001)]);
        assert_eq!(plan.open_port, Some(7000));
        assert_eq!(plan.note, None);
    }

    #[test]
    fn explicit_isolation_bumps_busy_choices() {
        let plan =
            plan_ports(&[6379], None, true, &[map(7000, 6379)], &set(&[7000]), true).unwrap();
        assert_eq!(plan.ports, vec![map(7001, 6379)]);
        assert!(plan.note.unwrap().contains("7000 was busy"));
    }

    #[test]
    fn explicit_isolation_without_pasta_is_refused() {
        let err = plan_ports(&[6379], None, true, &[], &set(&[]), false).unwrap_err();
        assert_eq!(err.0, axum::http::StatusCode::CONFLICT);
        assert!(err.1.contains("passt"), "error: {}", err.1);
    }

    #[test]
    fn two_busy_ports_get_distinct_free_ones() {
        let plan = plan_ports(&[80, 81], None, false, &[], &set(&[80, 81]), true).unwrap();
        assert_eq!(plan.ports, vec![map(82, 80), map(83, 81)]);
    }
}
