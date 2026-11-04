//! The hub HTTP server: a Mycel store exposed under `/api/v1`.
//!
//! Trust model: the server never believes a client-supplied hash or id.
//! Blob uploads are re-hashed with BLAKE3 (`Store::put_blob` hashes the
//! bytes it writes), manifest uploads are re-hashed via `Manifest::id()`,
//! and both are rejected on mismatch. Chunk uploads follow the same rule
//! (`POST /chunks/{id}` re-hashes the body against the id), and a blob
//! assembled from chunks is re-hashed against the claimed blob hash before
//! it is stored. Path parameters are format-validated before touching the
//! filesystem, so they can never traverse out of the store. All writes go
//! through the store's atomic temp+rename, so concurrent pushes are safe.

use crate::{
    valid_blob_hash, valid_manifest_id, valid_ref_name, BlobChunks, ChunkMissingRequest,
    ChunkMissingResponse, ManifestSummary, MissingRequest, MissingResponse, RefResponse,
    SetRefRequest, MODE_HEADER,
};
use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use myc_manifest::Manifest;
use myc_store::{chunks::CHUNK_MAX_BLOB_SIZE, ChunkId, Store};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use crate::chunkio::{read_chunk, ChunkLocator};

#[derive(Clone)]
struct App {
    root: PathBuf,
    /// Bearer token required for writes (reads stay open).
    write_token: Option<Arc<String>>,
    /// Open the store with the v2 chunk layer on. Advertised in `/ping` as
    /// the `chunks` capability; when false the hub serves (and clients use)
    /// only the v1 blob protocol.
    chunked: bool,
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
        if self.chunked {
            Store::open_chunked(&self.root).map_err(internal)
        } else {
            Store::open(&self.root).map_err(internal)
        }
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

    /// Spool area for chunks received via `POST /chunks/{id}` but not yet
    /// consumed by a blob assembly. Lives under the store root (same
    /// filesystem as the packs, so renames are atomic); content-addressed,
    /// so re-uploads are harmless. Deleted as soon as an assembly consumes
    /// the chunk.
    fn staging_dir(&self) -> PathBuf {
        self.root.join("hub-staging")
    }

    fn staged_chunk(&self, chunk_id: &str) -> PathBuf {
        self.staging_dir().join(chunk_id)
    }
}

/// Decode a 64-hex chunk id to bytes (validated first by the caller).
fn decode_chunk_id(hex_id: &str) -> Option<ChunkId> {
    hex::decode(hex_id).ok().and_then(|b| b.try_into().ok())
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
        .route(
            "/api/v1/blobs/{hash}/chunks",
            get(get_blob_chunks).post(assemble_blob),
        )
        .route("/api/v1/chunks/missing", post(chunks_missing))
        .route("/api/v1/chunks/{chunk_id}", get(get_chunk).post(put_chunk))
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
        // Capability flag: v2 chunk negotiation is available (store opened
        // with the chunk layer). v1 clients ignore it; v1 hubs don't have it.
        "chunks": app.chunked,
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

// ---- chunk layer (v2) ----
//
// Endpoints are deliberately format-tolerant about the store's chunked flag:
// reads (`chunks/missing`, `GET /chunks/{id}`, `GET /blobs/{hash}/chunks`)
// work on any store — a store without packs simply reports everything
// missing / 404. Writes stage chunks and assemble blobs through
// `Store::put_blob_with_mode`, which indexes chunks only when the store is
// chunked; on a v1 store the assembly still produces a correct whole blob
// (just without the pack layer). The `chunks` capability in `/ping` is what
// makes well-behaved clients pick the chunk protocol in the first place.

/// Parse and validate a request listing chunk ids (`missing` negotiation or
/// assembly recipe). Returns the ids as bytes, or the first bad one.
fn parse_chunk_ids(ids: &[String]) -> HandlerResult<Vec<ChunkId>> {
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if !valid_blob_hash(id) {
            return Err(bad_request(format!("malformed chunk id '{id}'")));
        }
        out.push(
            decode_chunk_id(id).ok_or_else(|| bad_request(format!("malformed chunk id '{id}'")))?,
        );
    }
    Ok(out)
}

/// `POST /api/v1/chunks/missing`: for one blob, which chunks does the hub
/// NOT hold? Bloom filters first, pack indexes for the final word — the
/// answer can only err towards "missing" (safe: the client resends).
async fn chunks_missing(
    State(app): State<App>,
    Json(req): Json<ChunkMissingRequest>,
) -> HandlerResult<Json<ChunkMissingResponse>> {
    if !valid_blob_hash(&req.blob_hash) {
        return Err(bad_request(format!(
            "malformed blob hash '{}'",
            req.blob_hash
        )));
    }
    let ids = parse_chunk_ids(&req.chunk_ids)?;
    let store = app.store()?;
    // Whole-blob shortcut: nothing to negotiate if the hub already has the
    // blob (same answer the v1 `/missing` route gives).
    if store.has_blob(&req.blob_hash) {
        return Ok(Json(ChunkMissingResponse {
            missing: Vec::new(),
            pack_hints: Vec::new(),
        }));
    }
    // One pack scan answers every id (Bloom-first per pack).
    let lacking: Vec<ChunkId> = store.missing_chunks(&ids);
    let lacking_set: std::collections::HashSet<ChunkId> = lacking.iter().copied().collect();
    let staged: Vec<ChunkId> = lacking
        .iter()
        .copied()
        .filter(|id| app.staged_chunk(&hex::encode(id)).exists())
        .collect();
    let staged_set: std::collections::HashSet<ChunkId> = staged.iter().copied().collect();
    let missing = ids
        .iter()
        .zip(&req.chunk_ids)
        .filter(|(id, _)| lacking_set.contains(*id) && !staged_set.contains(*id))
        .map(|(_, hex)| hex.clone())
        .collect();
    // Hints: the packs holding the requested chunks that ARE present.
    let present: Vec<ChunkId> = ids
        .iter()
        .copied()
        .filter(|id| !lacking_set.contains(id))
        .collect();
    let pack_hints = ChunkLocator::scan(&app.root).packs_of(&present);
    Ok(Json(ChunkMissingResponse {
        missing,
        pack_hints,
    }))
}

/// `GET /api/v1/chunks/{id}`: the chunk's raw (decompressed) bytes.
///
/// Decision (documented in store-v2.md §8): the server decompresses the
/// zstd frame itself and re-hashes the result against the id before
/// answering, rather than serving a `?pack=&offset=&len` raw-frame window.
/// Chunks are ≤ 16 KiB so the cost is trivial, and the client cannot be
/// misled by stale offsets after a GC renamed the pack landscape.
async fn get_chunk(
    State(app): State<App>,
    UrlPath(chunk_id): UrlPath<String>,
) -> HandlerResult<Response> {
    if !valid_blob_hash(&chunk_id) {
        return Err(bad_request(format!("malformed chunk id '{chunk_id}'")));
    }
    let id = decode_chunk_id(&chunk_id)
        .ok_or_else(|| bad_request(format!("malformed chunk id '{chunk_id}'")))?;
    let locator = ChunkLocator::scan(&app.root);
    let Some(bytes) = read_chunk(&app.root, &locator, &id) else {
        return Err(not_found(format!("chunk {chunk_id} not on this hub")));
    };
    Ok((
        [(header::CONTENT_TYPE, "application/octet-stream".to_string())],
        bytes,
    )
        .into_response())
}

/// `POST /api/v1/chunks/{id}`: receive one chunk's raw bytes. The body is
/// re-hashed against the id and rejected on mismatch — never trusted.
/// The chunk is staged (content-addressed spool) until an assembly of the
/// blob it belongs to consumes it.
async fn put_chunk(
    State(app): State<App>,
    UrlPath(chunk_id): UrlPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> HandlerResult<StatusCode> {
    app.check_write(&headers)?;
    if !valid_blob_hash(&chunk_id) {
        return Err(bad_request(format!("malformed chunk id '{chunk_id}'")));
    }
    let actual = blake3::hash(&body).to_hex().to_string();
    if actual != chunk_id {
        return Err(bad_request(format!(
            "chunk hash mismatch: url says {chunk_id}, content hashes to {actual}"
        )));
    }
    let dir = app.staging_dir();
    std::fs::create_dir_all(&dir).map_err(internal)?;
    // Atomic publish: temp in the store's tmp dir, rename over any previous
    // stage of the same chunk (content is identical anyway).
    let tmp = app.root.join("tmp").join(format!("chunk-{chunk_id}"));
    std::fs::write(&tmp, &body).map_err(internal)?;
    std::fs::rename(&tmp, app.staged_chunk(&chunk_id)).map_err(internal)?;
    Ok(StatusCode::CREATED)
}

/// `GET /api/v1/blobs/{hash}/chunks`: the hub's ordered chunk recipe for
/// one blob (its sidecar), or 404 when the blob has none — the client then
/// falls back to the v1 whole-blob route for that blob.
async fn get_blob_chunks(
    State(app): State<App>,
    UrlPath(hash): UrlPath<String>,
) -> HandlerResult<Json<BlobChunks>> {
    if !valid_blob_hash(&hash) {
        return Err(bad_request(format!("malformed blob hash '{hash}'")));
    }
    let store = app.store()?;
    match store.blob_chunks(&hash).map_err(internal)? {
        Some(ids) => Ok(Json(BlobChunks {
            chunk_ids: ids.iter().map(hex::encode).collect(),
        })),
        None => Err(not_found(format!(
            "blob {hash} has no chunk list on this hub"
        ))),
    }
}

/// `POST /api/v1/blobs/{hash}/chunks`: assemble the blob from the ordered
/// recipe — staged chunks plus the ones the hub's packs already hold —
/// re-hash the result against `hash`, and store it as a regular v1 blob
/// (`put_blob_with_mode`), which also indexes its chunks into the hub's own
/// packs (chunked stores only). This keeps every v1 client able to pull the
/// blob the plain way. Server cost: one decompress+hash+re-chunk pass per
/// blob (measured in store-v2.md §8).
async fn assemble_blob(
    State(app): State<App>,
    UrlPath(hash): UrlPath<String>,
    headers: HeaderMap,
    Json(req): Json<BlobChunks>,
) -> HandlerResult<StatusCode> {
    app.check_write(&headers)?;
    if !valid_blob_hash(&hash) {
        return Err(bad_request(format!("malformed blob hash '{hash}'")));
    }
    if req.chunk_ids.is_empty() {
        return Err(bad_request("chunk recipe is empty"));
    }
    let ids = parse_chunk_ids(&req.chunk_ids)?;
    let store = app.store()?;
    if store.has_blob(&hash) {
        // Idempotent no-op: an interrupted push retrying the assembly.
        return Ok(StatusCode::OK);
    }

    // Fetch each distinct chunk once (recipes repeat ids on self-similar
    // blobs), from staging first, packs second. Corrupt staged files are
    // deleted and retried from the packs. The total is capped at the store's
    // chunked-blob bound so a bogus recipe cannot amplify memory.
    let locator = ChunkLocator::scan(&app.root);
    let mut bank: HashMap<ChunkId, Vec<u8>> = HashMap::new();
    let mut full = Vec::new();
    let mut absent: Vec<String> = Vec::new();
    for (id, hex_id) in ids.iter().zip(&req.chunk_ids) {
        if let Some(bytes) = bank.get(id) {
            full.extend_from_slice(bytes);
            if full.len() as u64 > CHUNK_MAX_BLOB_SIZE {
                return Err(bad_request(format!(
                    "chunk recipe exceeds the {} byte chunking limit",
                    CHUNK_MAX_BLOB_SIZE
                )));
            }
            continue;
        }
        let staged = app.staged_chunk(hex_id);
        let mut found: Option<Vec<u8>> = None;
        if staged.exists() {
            match std::fs::read(&staged) {
                Ok(bytes) if blake3::hash(&bytes).to_hex().to_string() == *hex_id => {
                    found = Some(bytes)
                }
                _ => {
                    // Torn write or corruption: drop and fall through.
                    let _ = std::fs::remove_file(&staged);
                }
            }
        }
        let found = match found {
            Some(b) => Some(b),
            None => read_chunk(&app.root, &locator, id),
        };
        match found {
            Some(bytes) => {
                full.extend_from_slice(&bytes);
                if full.len() as u64 > CHUNK_MAX_BLOB_SIZE {
                    return Err(bad_request(format!(
                        "chunk recipe exceeds the {} byte chunking limit",
                        CHUNK_MAX_BLOB_SIZE
                    )));
                }
                bank.insert(*id, bytes);
            }
            None => absent.push(hex_id.clone()),
        }
    }
    if !absent.is_empty() {
        return Err(bad_request(format!(
            "hub is missing {} chunk(s) of blob {hash} (e.g. {}) — push them first",
            absent.len(),
            absent[0]
        )));
    }
    let actual = blake3::hash(&full).to_hex().to_string();
    if actual != hash {
        return Err(bad_request(format!(
            "blob hash mismatch: url says {hash}, recipe hashes to {actual}"
        )));
    }
    let mode = headers
        .get(MODE_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| u32::from_str_radix(v, 8).ok())
        .unwrap_or(0o444);
    store
        .put_blob_with_mode(&mut &full[..], mode)
        .map_err(internal)?;
    // The blob now exists (and its chunks are packed): staging for this
    // recipe is consumed.
    for hex_id in &req.chunk_ids {
        let _ = std::fs::remove_file(app.staged_chunk(hex_id));
    }
    Ok(StatusCode::CREATED)
}

/// Serve `root` on `0.0.0.0:port`, blocking forever. Used by `myc hub serve`.
/// The v2 chunk layer follows the store's own opt-in: serve a store opened
/// with `MYCEL_CHUNKED=1` and the hub advertises and accepts chunk
/// negotiation; otherwise it is a plain v1 blob hub.
pub fn serve(root: PathBuf, port: u16, token: Option<String>) -> anyhow::Result<()> {
    // Fail early with a clear message if the store cannot be opened.
    let chunked = Store::open(&root)?.chunked();
    let app = App {
        root,
        write_token: token.map(Arc::new),
        chunked,
    };
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let addr = SocketAddr::from(([0, 0, 0, 0], port));
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let local = listener.local_addr()?;
        eprintln!(
            "mycel hub listening on http://{local} (Ctrl-C to stop){}",
            if chunked {
                " — chunk negotiation enabled"
            } else {
                ""
            }
        );
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

/// Start a hub on `127.0.0.1:port` (0 = ephemeral) on a background thread,
/// with the v2 chunk layer off (a v1 blob hub).
pub fn spawn(root: PathBuf, port: u16, token: Option<String>) -> anyhow::Result<SpawnedHub> {
    spawn_inner(root, port, token, false)
}

/// Like [`spawn`] but serving the store with the v2 chunk layer on: the hub
/// advertises the `chunks` capability and accepts chunk negotiation. Used
/// by tests and benchmarks; in production this is what `myc hub serve` does
/// for a `MYCEL_CHUNKED=1` store.
pub fn spawn_chunked(
    root: PathBuf,
    port: u16,
    token: Option<String>,
) -> anyhow::Result<SpawnedHub> {
    spawn_inner(root, port, token, true)
}

fn spawn_inner(
    root: PathBuf,
    port: u16,
    token: Option<String>,
    chunked: bool,
) -> anyhow::Result<SpawnedHub> {
    if chunked {
        Store::open_chunked(&root)?;
    } else {
        Store::open(&root)?;
    }
    let app = App {
        root,
        write_token: token.map(Arc::new),
        chunked,
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
