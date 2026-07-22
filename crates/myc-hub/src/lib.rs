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
//! | `/ping` | GET | liveness + store stats |
//! | `/manifests` | GET | list manifest summaries |
//! | `/manifests/{id}` | GET/PUT | fetch / upload a manifest (id re-verified server-side) |
//! | `/refs` | GET | list name → id |
//! | `/refs/{name}` | GET/PUT | resolve / set a ref |
//! | `/blobs/{hash}` | HEAD/GET/PUT | probe / download / upload a blob (re-hashed server-side) |
//! | `/missing` | POST | body: hashes; response: the subset the server does NOT have |
//!
//! Writes can be protected with a bearer token (`--token`); reads stay open.

pub mod client;
pub mod deploy;
pub mod server;
pub mod transfer;

pub use client::HubClient;
pub use server::{serve, spawn, SpawnedHub};
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
