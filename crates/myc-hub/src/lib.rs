//! The Mycel hub: share a content-addressed store over HTTP.
//!
//! A hub is just a Mycel store behind a small JSON+raw-blob protocol
//! (`/api/v1/...`). Because every file is content-addressed, synchronising
//! two stores reduces to one question — "which of these hashes are you
//! missing?" — and only that delta ever crosses the wire.
//!
//! - [`server`] — `myc hub serve`: axum HTTP server exposing a store.
//! - [`client`] — blocking client used by `myc push` / `myc pull` / lazy runs.
//! - [`transfer`] — push/pull negotiation built on the two.
//!
//! Protocol (all under `/api/v1`):
//!
//! | route | verb | meaning |
//! |---|---|---|
//! | `/ping` | GET | liveness + store stats + `"chunks"` capability |
//! | `/manifests` | GET | list manifest summaries |
//! | `/manifests/{id}` | GET/PUT | fetch / upload a manifest (id re-verified server-side) |
//! | `/refs` | GET | list name → id |
//! | `/refs/{name}` | GET/PUT | resolve / set a ref |
//! | `/blobs/{hash}` | HEAD/GET/PUT | probe / download / upload a blob (re-hashed server-side) |
//! | `/missing` | POST | body: hashes; response: the subset the server does NOT have |
//! | `/blobs/{hash}/chunks` | GET/POST | the blob's ordered chunk list / assemble the blob from chunks |
//! | `/chunks/missing` | POST | body: blob hash + chunk ids; response: chunks the server lacks |
//! | `/chunks/{chunk_id}` | GET/POST | download / upload one chunk's raw bytes (re-hashed both sides) |
//!
//! The blob routes are the v1 protocol and stay exactly as they were; the
//! chunk routes are the v2 layer (store `MYCEL_CHUNKED=1`, see
//! `docs/design/store-v2.md` §8): when both ends advertise the chunk
//! capability, push/pull negotiate at chunk granularity and only the chunks
//! the receiver is missing cross the wire. A v1 client never calls them and
//! is unaffected.
//!
//! Writes can be protected with a bearer token (`--token`); reads stay open.

pub mod chunkio;
pub mod client;
pub mod deploy;
pub mod remote;
pub mod server;
pub mod transfer;

pub use client::HubClient;
pub use server::{serve, spawn, spawn_chunked, SpawnedHub};
pub use transfer::{pull, pull_manifest_only, push, PullReport, PushReport};

use serde::{Deserialize, Serialize};

/// Summary row returned by `GET /api/v1/manifests`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestSummary {
    pub id: String,
    pub name: String,
    pub os: String,
    pub arch: String,
    pub file_count: usize,
    pub logical_size: u64,
}

/// Body of `POST /api/v1/missing`.
#[derive(Debug, Serialize, Deserialize)]
pub struct MissingRequest {
    pub hashes: Vec<String>,
}

/// Response of `POST /api/v1/missing`: the hashes the server does NOT have.
#[derive(Debug, Serialize, Deserialize)]
pub struct MissingResponse {
    pub missing: Vec<String>,
}

/// Body of `POST /api/v1/chunks/missing`: for one blob the client can
/// already describe (local chunk sidecar), the ordered chunk ids it is made
/// of. The server answers with the subset it does NOT hold — Bloom filters
/// first, pack indexes for the final word, so the answer never has a false
/// "present" and a false "missing" only costs a redundant upload.
#[derive(Debug, Serialize, Deserialize)]
pub struct ChunkMissingRequest {
    pub blob_hash: String,
    pub chunk_ids: Vec<String>,
}

/// Response of `POST /api/v1/chunks/missing`.
#[derive(Debug, Serialize, Deserialize)]
pub struct ChunkMissingResponse {
    /// Chunk ids (64-hex BLAKE3) the server lacks, in request order.
    pub missing: Vec<String>,
    /// Packs (64-hex pack names) that hold at least one of the requested
    /// chunks — hints so a future client can fetch whole packs instead of
    /// chunk-by-chunk. Informational; the chunk routes ignore them.
    pub pack_hints: Vec<String>,
}

/// The ordered chunk-id recipe of one blob. Response of
/// `GET /api/v1/blobs/{hash}/chunks` (the server's sidecar) and body of
/// `POST /api/v1/blobs/{hash}/chunks` ("here is the order — assemble the
/// blob from the chunks I just sent plus the ones you already have").
#[derive(Debug, Serialize, Deserialize)]
pub struct BlobChunks {
    pub chunk_ids: Vec<String>,
}

/// Body of `PUT /api/v1/refs/{name}`.
#[derive(Debug, Serialize, Deserialize)]
pub struct SetRefRequest {
    pub manifest_id: String,
}

/// Response of `GET /api/v1/refs/{name}`.
#[derive(Debug, Serialize, Deserialize)]
pub struct RefResponse {
    pub name: String,
    pub manifest_id: String,
}

/// Optional mode header on blob uploads, so the server can store the blob
/// inode with the right permission bits (an optimization, never trusted for
/// content: the server always re-hashes).
pub const MODE_HEADER: &str = "x-mycel-mode";

/// `true` for a well-formed BLAKE3 hex hash (64 lowercase hex chars).
pub fn valid_blob_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// `true` for a well-formed manifest id (`myc1-` + 64 hex chars).
pub fn valid_manifest_id(id: &str) -> bool {
    id.strip_prefix("myc1-").is_some_and(valid_blob_hash)
}

/// `true` for an acceptable ref name (stored under an escaped filename, so
/// this only rejects the obviously hostile or unusable).
pub fn valid_ref_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 512
        && name != "."
        && name != ".."
        && !name.contains('\0')
        && !name.contains('\n')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_validation() {
        assert!(valid_blob_hash(&"a".repeat(64)));
        assert!(!valid_blob_hash(&"A".repeat(64)));
        assert!(!valid_blob_hash(&"a".repeat(63)));
        assert!(!valid_blob_hash("../../../etc/passwd"));
        assert!(valid_manifest_id(&format!("myc1-{}", "0".repeat(64))));
        assert!(!valid_manifest_id(&"0".repeat(64)));
    }

    #[test]
    fn ref_name_validation() {
        assert!(valid_ref_name("docker.io/library/alpine:3.20"));
        assert!(valid_ref_name("web:v2"));
        assert!(!valid_ref_name(""));
        assert!(!valid_ref_name(".."));
        assert!(!valid_ref_name("a\nb"));
    }
}
