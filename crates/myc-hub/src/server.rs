//! The hub HTTP server: a Mycel store exposed under `/api/v1`.
//!
//! Trust model: the server never believes a client-supplied hash or id.
//! Blob uploads are re-hashed with BLAKE3 (`Store::put_blob` hashes the
//! bytes it writes), manifest uploads are re-hashed via `Manifest::id()`,
//! and both are rejected on mismatch. Path parameters are format-validated
//! before touching the filesystem, so they can never traverse out of the
//! store. All writes go through the store's atomic temp+rename, so
//! concurrent pushes are safe.

use crate::{
    valid_blob_hash, valid_manifest_id, valid_ref_name, ManifestSummary, MissingRequest,
    MissingResponse, RefResponse, SetRefRequest, MODE_HEADER,
};
use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use myc_manifest::Manifest;
use myc_store::Store;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone)]
struct App {
    root: PathBuf,
    /// Bearer token required for writes (reads stay open).
    write_token: Option<Arc<String>>,
}

/// Handler error: status + plain-text message.
struct Fail(StatusCode, String);

impl IntoResponse for Fail {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}

fn internal(e: impl std::fmt::Display) -> Fail {
    Fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}"))
}

fn bad_request(msg: impl Into<String>) -> Fail {
    Fail(StatusCode::BAD_REQUEST, msg.into())
}

fn not_found(msg: impl Into<String>) -> Fail {
    Fail(StatusCode::NOT_FOUND, msg.into())
}

type HandlerResult<T> = std::result::Result<T, Fail>;

impl App {
    fn store(&self) -> HandlerResult<Store> {
        Store::open(&self.root).map_err(internal)
    }

    /// Writes require the bearer token when one is configured.
    fn check_write(&self, headers: &HeaderMap) -> HandlerResult<()> {
        let Some(token) = &self.write_token else {
            return Ok(());
        };
        let presented = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if presented == Some(token.as_str()) {
            Ok(())
        } else {
            Err(Fail(
                StatusCode::UNAUTHORIZED,
                "write requires a valid bearer token".into(),
            ))
        }
    }
}

fn router(app: App) -> Router {
    Router::new()
        .route("/api/v1/ping", get(ping))
        .route("/api/v1/manifests", get(list_manifests))
        .route(
            "/api/v1/manifests/{id}",
            get(get_manifest).put(put_manifest),
        )
        .route("/api/v1/refs", get(list_refs))
        .route("/api/v1/refs/{*name}", get(get_ref).put(put_ref))
        .route("/api/v1/blobs/{hash}", get(get_blob).put(put_blob))
        .route("/api/v1/missing", post(missing))
        // Blobs can be large; the store re-hashes and rejects garbage anyway.
        .layer(axum::extract::DefaultBodyLimit::disable())
        .with_state(app)
}

async fn ping(State(app): State<App>) -> HandlerResult<Json<serde_json::Value>> {
    let store = app.store()?;
    let stats = store.blob_stats().map_err(internal)?;
    let manifests = store.list_manifests().map_err(internal)?.len();
    Ok(Json(serde_json::json!({
        "ok": true,
        "service": "mycel-hub",
        "manifests": manifests,
        "blobs": stats.count,
        "bytes": stats.bytes,
    })))
}

async fn list_manifests(State(app): State<App>) -> HandlerResult<Json<Vec<ManifestSummary>>> {
    let store = app.store()?;
    let out = store
        .list_manifests()
        .map_err(internal)?
        .into_iter()
        .map(|(id, m)| ManifestSummary {
            id,
            name: m.name.clone(),
            os: m.os.clone(),
            arch: m.arch.clone(),
            file_count: m.file_count(),
            logical_size: m.logical_size(),
        })
        .collect();
    Ok(Json(out))
}

async fn get_manifest(
    State(app): State<App>,
    UrlPath(id): UrlPath<String>,
) -> HandlerResult<Response> {
    if !valid_manifest_id(&id) {
        return Err(bad_request(format!("malformed manifest id '{id}'")));
    }
    let store = app.store()?;
    let manifest = store
        .get_manifest(&id)
        .map_err(|_| not_found(format!("manifest {id} not on this hub")))?;
    let bytes = manifest.to_canonical_json().map_err(internal)?;
    Ok(([(header::CONTENT_TYPE, "application/json")], bytes).into_response())
}

async fn put_manifest(
    State(app): State<App>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> HandlerResult<StatusCode> {
    app.check_write(&headers)?;
    if !valid_manifest_id(&id) {
        return Err(bad_request(format!("malformed manifest id '{id}'")));
    }
    let manifest =
        Manifest::from_json(&body).map_err(|e| bad_request(format!("invalid manifest: {e}")))?;
    let actual = manifest.id().map_err(internal)?;
    if actual != id {
        return Err(bad_request(format!(
            "manifest id mismatch: url says {id}, content hashes to {actual}"
        )));
    }
    let store = app.store()?;
    store.put_manifest(&manifest).map_err(internal)?;
    Ok(StatusCode::CREATED)
}

async fn list_refs(State(app): State<App>) -> HandlerResult<Json<Vec<RefResponse>>> {
    let store = app.store()?;
    let out = store
        .list_refs()
        .map_err(internal)?
        .into_iter()
        .map(|(name, manifest_id)| RefResponse { name, manifest_id })
        .collect();
    Ok(Json(out))
}

async fn get_ref(
    State(app): State<App>,
    UrlPath(name): UrlPath<String>,
) -> HandlerResult<Json<RefResponse>> {
    let store = app.store()?;
    if let Some(manifest_id) = store.resolve_ref(&name).map_err(internal)? {
        return Ok(Json(RefResponse { name, manifest_id }));
    }
    // Docker-style short names: `alpine:3.20` is stored under its canonical
    // ref `docker.io/library/alpine:3.20`. Store::resolve also accepts
    // manifest-id prefixes; anything resolvable is answerable.
    match store.resolve(&name) {
        Ok(manifest_id) => Ok(Json(RefResponse { name, manifest_id })),
        Err(_) => Err(not_found(format!("ref '{name}' not on this hub"))),
    }
}

async fn put_ref(
    State(app): State<App>,
    UrlPath(name): UrlPath<String>,
    headers: HeaderMap,
    Json(req): Json<SetRefRequest>,
) -> HandlerResult<StatusCode> {
    app.check_write(&headers)?;
    if !valid_ref_name(&name) {
        return Err(bad_request(format!("unacceptable ref name '{name}'")));
    }
    if !valid_manifest_id(&req.manifest_id) {
        return Err(bad_request(format!(
            "malformed manifest id '{}'",
            req.manifest_id
        )));
    }
    let store = app.store()?;
    // A ref must point at a manifest the hub actually has.
    store
        .get_manifest(&req.manifest_id)
        .map_err(|_| bad_request(format!("manifest {} not on this hub", req.manifest_id)))?;
    store.set_ref(&name, &req.manifest_id).map_err(internal)?;
    Ok(StatusCode::CREATED)
}

async fn get_blob(
    State(app): State<App>,
    UrlPath(hash): UrlPath<String>,
) -> HandlerResult<Response> {
    if !valid_blob_hash(&hash) {
        return Err(bad_request(format!("malformed blob hash '{hash}'")));
    }
    let store = app.store()?;
    let path = store
        .blob(&hash)
        .map_err(|_| not_found(format!("blob {hash} not on this hub")))?;
    let file = tokio::fs::File::open(&path).await.map_err(internal)?;
    let len = file.metadata().await.map_err(internal)?.len();
    let stream = tokio_util::io::ReaderStream::new(file);
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CONTENT_LENGTH, len.to_string()),
        ],
        Body::from_stream(stream),
    )
        .into_response())
}

async fn put_blob(
    State(app): State<App>,
    UrlPath(hash): UrlPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> HandlerResult<StatusCode> {
    app.check_write(&headers)?;
    if !valid_blob_hash(&hash) {
        return Err(bad_request(format!("malformed blob hash '{hash}'")));
    }
    let mode = headers
        .get(MODE_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| u32::from_str_radix(v, 8).ok())
        .unwrap_or(0o444);
    let store = app.store()?;
    // put_blob_with_mode re-hashes every byte: the client's claim is checked,
    // never trusted. A mismatching upload is discarded (it landed under its
    // true hash, which no manifest references, so the next GC removes it).
    let (actual, _) = store
        .put_blob_with_mode(&mut &body[..], mode)
        .map_err(internal)?;
    if actual != hash {
        return Err(bad_request(format!(
            "blob hash mismatch: url says {hash}, content hashes to {actual}"
        )));
    }
    Ok(StatusCode::CREATED)
}

async fn missing(
    State(app): State<App>,
    Json(req): Json<MissingRequest>,
) -> HandlerResult<Json<MissingResponse>> {
    for h in &req.hashes {
        if !valid_blob_hash(h) {
            return Err(bad_request(format!("malformed blob hash '{h}'")));
        }
    }
    let store = app.store()?;
    let missing = req
        .hashes
        .into_iter()
        .filter(|h| !store.has_blob(h))
        .collect();
    Ok(Json(MissingResponse { missing }))
}

/// Serve `root` on `0.0.0.0:port`, blocking forever. Used by `myc hub serve`.
pub fn serve(root: PathBuf, port: u16, token: Option<String>) -> anyhow::Result<()> {
    // Fail early with a clear message if the store cannot be opened.
    Store::open(&root)?;
    let app = App {
        root,
        write_token: token.map(Arc::new),
    };
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let addr = SocketAddr::from(([0, 0, 0, 0], port));
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let local = listener.local_addr()?;
        eprintln!("mycel hub listening on http://{local} (Ctrl-C to stop)");
        axum::serve(listener, router(app)).await?;
        Ok(())
    })
}

/// A hub running on a background thread; shuts down on drop.
/// Used by tests and the benchmark harness.
pub struct SpawnedHub {
    pub addr: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SpawnedHub {
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for SpawnedHub {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Start a hub on `127.0.0.1:port` (0 = ephemeral) on a background thread.
pub fn spawn(root: PathBuf, port: u16, token: Option<String>) -> anyhow::Result<SpawnedHub> {
    Store::open(&root)?;
    let app = App {
        root,
        write_token: token.map(Arc::new),
    };
    let (addr_tx, addr_rx) = std::sync::mpsc::channel();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let thread = std::thread::spawn(move || {
        let runtime = match tokio::runtime::Runtime::new() {
            Ok(r) => r,
            Err(e) => {
                let _ = addr_tx.send(Err(anyhow::anyhow!("tokio runtime: {e}")));
                return;
            }
        };
        runtime.block_on(async move {
            let listener =
                match tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await
                {
                    Ok(l) => l,
                    Err(e) => {
                        let _ = addr_tx.send(Err(anyhow::anyhow!("bind: {e}")));
                        return;
                    }
                };
            let local = listener.local_addr().expect("local_addr");
            let _ = addr_tx.send(Ok(local));
            let _ = axum::serve(listener, router(app))
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await;
        });
    });
    let addr = addr_rx
        .recv()
        .map_err(|_| anyhow::anyhow!("hub thread died before binding"))??;
    Ok(SpawnedHub {
        addr,
        shutdown: Some(shutdown_tx),
        thread: Some(thread),
    })
}
