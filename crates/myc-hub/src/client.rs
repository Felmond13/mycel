//! Blocking client for the hub protocol, used by `myc push`, `myc pull
//! --from` and the lazy runtime's on-demand blob faults.

use crate::{
    BlobChunks, ChunkMissingRequest, ChunkMissingResponse, ManifestSummary, MissingRequest,
    MissingResponse, RefResponse, SetRefRequest, MODE_HEADER,
};
use myc_manifest::Manifest;
use myc_store::Store;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum HubError {
    #[error("cannot reach hub at {url}: {source}")]
    Unreachable {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("hub request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("hub {url} has no {what}")]
    NotFound { url: String, what: String },
    #[error("hub rejected the request ({status}): {message}")]
    Rejected {
        status: reqwest::StatusCode,
        message: String,
    },
    #[error("hub sent a corrupt blob: expected {expected}, got {actual}")]
    CorruptBlob { expected: String, actual: String },
    #[error("hub sent a corrupt chunk: expected {expected}, got {actual}")]
    CorruptChunk { expected: String, actual: String },
    #[error("protocol encoding failed: {0}")]
    Serde(#[from] serde_json::Error),
    #[error(transparent)]
    Store(#[from] myc_store::StoreError),
    #[error(transparent)]
    Manifest(#[from] myc_manifest::ManifestError),
}

pub type Result<T> = std::result::Result<T, HubError>;

/// Blocking HTTP client bound to one hub base URL.
pub struct HubClient {
    base: String,
    token: Option<String>,
    http: reqwest::blocking::Client,
}

impl HubClient {
    /// `base` like `http://hub:9600` (trailing slashes tolerated).
    pub fn new(base: &str, token: Option<String>) -> Result<Self> {
        let base = base.trim_end_matches('/').to_string();
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            // No overall timeout: blob transfers may legitimately be long.
            .build()?;
        Ok(HubClient { base, token, http })
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/v1/{path}", self.base)
    }

    fn auth(&self, req: reqwest::blocking::RequestBuilder) -> reqwest::blocking::RequestBuilder {
        match &self.token {
            Some(t) => req.bearer_auth(t),
            None => req,
        }
    }

    /// Check a response, turning HTTP errors into typed ones.
    fn checked(
        &self,
        resp: reqwest::blocking::Response,
        what: &str,
    ) -> Result<reqwest::blocking::Response> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(HubError::NotFound {
                url: self.base.clone(),
                what: what.to_string(),
            });
        }
        let message = resp.text().unwrap_or_default();
        Err(HubError::Rejected { status, message })
    }

    fn send(&self, req: reqwest::blocking::RequestBuilder) -> Result<reqwest::blocking::Response> {
        req.send().map_err(|source| HubError::Unreachable {
            url: self.base.clone(),
            source,
        })
    }

    pub fn ping(&self) -> Result<serde_json::Value> {
        let resp = self.send(self.http.get(self.url("ping")))?;
        Ok(self.checked(resp, "ping")?.json()?)
    }

    pub fn list_manifests(&self) -> Result<Vec<ManifestSummary>> {
        let resp = self.send(self.http.get(self.url("manifests")))?;
        Ok(self.checked(resp, "manifest list")?.json()?)
    }

    pub fn get_manifest(&self, id: &str) -> Result<Manifest> {
        let resp = self.send(self.http.get(self.url(&format!("manifests/{id}"))))?;
        let bytes = self.checked(resp, &format!("manifest {id}"))?.bytes()?;
        Ok(Manifest::from_json(&bytes)?)
    }

    pub fn put_manifest(&self, manifest: &Manifest) -> Result<String> {
        let id = manifest.id()?;
        let bytes = manifest.to_canonical_json()?;
        let resp = self.send(
            self.auth(self.http.put(self.url(&format!("manifests/{id}"))))
                .header("content-type", "application/json")
                .body(bytes),
        )?;
        self.checked(resp, &format!("manifest {id}"))?;
        Ok(id)
    }

    pub fn list_refs(&self) -> Result<Vec<RefResponse>> {
        let resp = self.send(self.http.get(self.url("refs")))?;
        Ok(self.checked(resp, "ref list")?.json()?)
    }

    /// Resolve a ref name on the hub. `Ok(None)` when the hub doesn't have it.
    pub fn get_ref(&self, name: &str) -> Result<Option<String>> {
        let resp = self.send(self.http.get(self.url(&format!("refs/{name}"))))?;
        match self.checked(resp, &format!("ref '{name}'")) {
            Ok(r) => Ok(Some(r.json::<RefResponse>()?.manifest_id)),
            Err(HubError::NotFound { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn set_ref(&self, name: &str, manifest_id: &str) -> Result<()> {
        let resp = self.send(
            self.auth(self.http.put(self.url(&format!("refs/{name}"))))
                .json(&SetRefRequest {
                    manifest_id: manifest_id.to_string(),
                }),
        )?;
        self.checked(resp, &format!("ref '{name}'"))?;
        Ok(())
    }

    /// The dedup negotiation primitive: which of `hashes` is the hub missing?
    pub fn missing(&self, hashes: &[String]) -> Result<Vec<String>> {
        let resp = self.send(self.http.post(self.url("missing")).json(&MissingRequest {
            hashes: hashes.to_vec(),
        }))?;
        let out: MissingResponse = self.checked(resp, "missing")?.json()?;
        Ok(out.missing)
    }

    /// Upload one blob from the local store.
    pub fn put_blob(&self, store: &Store, hash: &str) -> Result<u64> {
        let path = store.blob(hash)?;
        let file = std::fs::File::open(&path).map_err(|e| {
            HubError::Store(myc_store::StoreError::Io {
                path: path.clone(),
                source: e,
            })
        })?;
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        let mode = blob_mode(&path);
        let resp = self.send(
            self.auth(self.http.put(self.url(&format!("blobs/{hash}"))))
                .header(MODE_HEADER, format!("{mode:o}"))
                .header("content-type", "application/octet-stream")
                .body(reqwest::blocking::Body::sized(file, len)),
        )?;
        self.checked(resp, &format!("blob {hash}"))?;
        Ok(len)
    }

    /// Download one blob into the local store, verifying its hash.
    /// `mode` is the permission bits to store the blob inode with (from the
    /// manifest entry, so the common case needs no extra inode later).
    /// Returns the blob size.
    pub fn fetch_blob(&self, store: &Store, hash: &str, mode: u32) -> Result<u64> {
        let resp = self.send(self.http.get(self.url(&format!("blobs/{hash}"))))?;
        let mut resp = self.checked(resp, &format!("blob {hash}"))?;
        // Store::put_blob re-hashes the stream; a corrupt transfer lands
        // under its true hash (unreferenced -> GC fodder) and is reported.
        let (actual, size) = store.put_blob_with_mode(&mut resp, mode)?;
        if actual != hash {
            return Err(HubError::CorruptBlob {
                expected: hash.to_string(),
                actual,
            });
        }
        Ok(size)
    }

    // ---- chunk layer (v2) ----

    /// What the hub supports, from `GET /ping`. A hub that does not answer
    /// the `chunks` field (any v1 hub) reports `chunks: false`, and callers
    /// fall back to the whole-blob protocol.
    pub fn capabilities(&self) -> Result<Capabilities> {
        let ping = self.ping()?;
        Ok(Capabilities {
            chunks: ping
                .get("chunks")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        })
    }

    /// Which chunks of one blob the hub is missing (`POST /chunks/missing`).
    pub fn chunks_missing(
        &self,
        blob_hash: &str,
        chunk_ids: &[String],
    ) -> Result<ChunkMissingResponse> {
        let resp = self.send(self.http.post(self.url("chunks/missing")).json(
            &ChunkMissingRequest {
                blob_hash: blob_hash.to_string(),
                chunk_ids: chunk_ids.to_vec(),
            },
        ))?;
        Ok(self.checked(resp, "chunk negotiation")?.json()?)
    }

    /// The hub's ordered chunk list for one blob (`GET /blobs/{hash}/chunks`).
    /// `Ok(None)` when the hub has no chunk sidecar for it (blob pushed via
    /// the v1 route, or below the chunking threshold).
    pub fn blob_chunks(&self, blob_hash: &str) -> Result<Option<Vec<String>>> {
        let resp = self.send(
            self.http
                .get(self.url(&format!("blobs/{blob_hash}/chunks"))),
        )?;
        match self.checked(resp, &format!("chunk list of blob {blob_hash}")) {
            Ok(r) => Ok(Some(r.json::<BlobChunks>()?.chunk_ids)),
            Err(HubError::NotFound { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Download one chunk's raw bytes, verifying the BLAKE3 against the
    /// requested id — the symmetric guarantee of `put_chunk`.
    pub fn get_chunk(&self, chunk_id: &str) -> Result<Vec<u8>> {
        let resp = self.send(self.http.get(self.url(&format!("chunks/{chunk_id}"))))?;
        let bytes = self.checked(resp, &format!("chunk {chunk_id}"))?.bytes()?;
        let actual = blake3::hash(&bytes).to_hex().to_string();
        if actual != chunk_id {
            return Err(HubError::CorruptChunk {
                expected: chunk_id.to_string(),
                actual,
            });
        }
        Ok(bytes.to_vec())
    }

    /// Upload one chunk's raw bytes (`POST /chunks/{id}`). The server
    /// re-hashes and rejects on mismatch, exactly like blob uploads.
    pub fn put_chunk(&self, chunk_id: &str, bytes: &[u8]) -> Result<()> {
        let resp = self.send(
            self.auth(self.http.post(self.url(&format!("chunks/{chunk_id}"))))
                .header("content-type", "application/octet-stream")
                .body(bytes.to_vec()),
        )?;
        self.checked(resp, &format!("chunk {chunk_id}"))?;
        Ok(())
    }

    /// Ask the hub to assemble one blob from the chunks it now has (staged
    /// by `put_chunk` plus whatever its packs already held), following the
    /// ordered recipe. The hub re-hashes the assembled blob against `hash`
    /// before storing it. Returns the request body size (the recipe bytes
    /// that crossed the wire).
    pub fn assemble_blob(&self, blob_hash: &str, chunk_ids: &[String], mode: u32) -> Result<u64> {
        let body = serde_json::to_vec(&BlobChunks {
            chunk_ids: chunk_ids.to_vec(),
        })?;
        let resp = self.send(
            self.auth(
                self.http
                    .post(self.url(&format!("blobs/{blob_hash}/chunks"))),
            )
            .header(MODE_HEADER, format!("{mode:o}"))
            .header("content-type", "application/json")
            .body(body.clone()),
        )?;
        self.checked(resp, &format!("assembly of blob {blob_hash}"))?;
        Ok(body.len() as u64)
    }
}

/// Hub feature flags, negotiated through `GET /ping`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// The hub serves the v2 chunk routes (`MYCEL_CHUNKED=1` on its store).
    pub chunks: bool,
}

/// Permission bits of a stored blob (for forwarding on upload).
/// Shared with the deploy protocol, which forwards modes the same way.
pub(crate) fn blob_mode(path: &std::path::Path) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            return meta.permissions().mode() & 0o7777;
        }
    }
    let _ = path;
    0o444
}
