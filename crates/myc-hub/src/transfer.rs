//! Push/pull negotiation: move a manifest between a local store and a hub
//! while transferring only what the receiving side is missing.
//!
//! Two protocols, one entry point each:
//!
//! - **v1 (blobs)** — negotiate whole-blob hashes (`POST /missing`) and
//!   upload/download the missing blob files. Always available; the only
//!   path when either end is not on the store v2 chunk layer.
//! - **v2 (chunks)** — when the local store runs with the chunk layer on
//!   (`MYCEL_CHUNKED=1`) *and* the hub advertises the `chunks` capability
//!   (negotiated once via `/ping`), each missing blob is negotiated at
//!   chunk granularity: only the chunks the receiver lacks cross the wire,
//!   then the receiver reassembles the blob (re-hashed end to end) so the
//!   v1 world (blobs, manifests, v1 pulls) sees exactly the same result.
//!
//! Blob transfers run on a small thread pool — blobs are independent
//! files, the store's atomic writes make concurrent inserts safe, and
//! reqwest's connection pool keeps it to a few sockets. The chunked pull
//! assembles one blob at a time (assembly is RAM-bound on the blob size,
//! like the store's own indexer).

use crate::chunkio::{read_chunk, ChunkLocator};
use crate::client::{Capabilities, HubClient, HubError, Result};
use myc_manifest::Manifest;
use myc_store::{ChunkId, Store};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How many blob transfers run in parallel.
const TRANSFER_THREADS: usize = 8;

/// Outcome of a `myc push`.
#[derive(Debug)]
pub struct PushReport {
    pub manifest_id: String,
    pub ref_name: String,
    /// Total blobs referenced by the manifest.
    pub blobs_total: usize,
    /// Blobs the hub was missing (actually uploaded, in whole or in chunks).
    pub blobs_uploaded: usize,
    /// Payload bytes that crossed the wire: whole blobs, individual chunks,
    /// and the chunk recipes (assembly requests). Negotiation requests
    /// (`/missing`, `/chunks/missing`) are not counted, matching the v1
    /// report's semantics.
    pub bytes_uploaded: u64,
    pub bytes_total: u64,
    /// Individual chunks sent (0 in v1 blob mode).
    pub chunks_uploaded: usize,
    /// Whether the transfer ran the v2 chunk protocol.
    pub chunked: bool,
    pub elapsed: Duration,
}

/// Outcome of a `myc pull` from a hub.
#[derive(Debug)]
pub struct PullReport {
    pub manifest_id: String,
    pub blobs_total: usize,
    /// Blobs the local store was missing (actually downloaded, in whole or
    /// in chunks).
    pub blobs_downloaded: usize,
    /// Payload bytes that crossed the wire (see `PushReport::bytes_uploaded`).
    pub bytes_downloaded: u64,
    pub bytes_total: u64,
    /// Individual chunks downloaded (0 in v1 blob mode).
    pub chunks_downloaded: usize,
    /// Whether the transfer ran the v2 chunk protocol.
    pub chunked: bool,
    pub elapsed: Duration,
}

/// Progress callback: (blobs done, blobs to transfer).
pub type Progress<'a> = &'a (dyn Fn(usize, usize) + Sync);

/// No-op progress.
pub fn silent() -> Progress<'static> {
    &|_, _| {}
}

/// Which protocol a transfer should speak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Whole blobs (`POST /missing` + `blobs/{hash}`).
    Blobs,
    /// Per-chunk negotiation (`chunks/*` routes).
    Chunks,
}

/// Pick the transfer mode: chunks only when the local store has the v2
/// layer on and the hub advertises the capability. Anything else — v1 hub,
/// v1 local store — silently falls back to the whole-blob protocol, which
/// is the same wire format as before.
fn negotiate_mode(store: &Store, client: &HubClient) -> Result<Mode> {
    if !store.chunked() {
        return Ok(Mode::Blobs);
    }
    let caps: Capabilities = client.capabilities()?;
    if caps.chunks {
        Ok(Mode::Chunks)
    } else {
        eprintln!(
            "myc: hub at {} does not support chunk negotiation — transferring whole blobs",
            client.base_url()
        );
        Ok(Mode::Blobs)
    }
}

/// The permission bits to store a blob's inode with (from the manifest
/// entry, so the common case — first materialization — needs no chmod).
fn mode_of(manifest: &Manifest, hash: &str) -> u32 {
    manifest
        .entries
        .iter()
        .find(|e| e.blake3.as_deref() == Some(hash))
        .map(|e| e.mode)
        .unwrap_or(0o444)
}

/// Decode a 64-hex chunk id to bytes.
fn decode_id(hex_id: &str) -> Result<ChunkId> {
    hex::decode(hex_id)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| {
            HubError::Store(myc_store::StoreError::Chunk(format!(
                "malformed chunk id '{hex_id}'"
            )))
        })
}

/// Push `manifest` (already resolved locally) to the hub under `ref_name`:
/// upload the manifest, negotiate what the hub is missing, upload only
/// that, set the ref. Safe to re-run; a second push uploads nothing.
pub fn push(
    store: &Store,
    client: &HubClient,
    manifest: &Manifest,
    ref_name: &str,
    progress: Progress<'_>,
) -> Result<PushReport> {
    match negotiate_mode(store, client)? {
        Mode::Chunks => push_chunks(store, client, manifest, ref_name, progress),
        Mode::Blobs => push_blobs(store, client, manifest, ref_name, progress),
    }
}

/// The v1 path, unchanged in behavior: whole blobs only.
fn push_blobs(
    store: &Store,
    client: &HubClient,
    manifest: &Manifest,
    ref_name: &str,
    progress: Progress<'_>,
) -> Result<PushReport> {
    let start = Instant::now();
    let blobs = manifest.referenced_blobs();
    let bytes_total: u64 = blobs.values().sum();
    let hashes: Vec<String> = blobs.keys().map(|h| h.to_string()).collect();

    // The whole point: ask once, send only the delta.
    let missing = client.missing(&hashes)?;
    let uploaded_bytes = AtomicU64::new(0);
    run_pool(&missing, progress, |hash| {
        let n = client.put_blob(store, hash)?;
        uploaded_bytes.fetch_add(n, Ordering::Relaxed);
        Ok(())
    })?;

    // Manifest and ref last: the hub never advertises an environment whose
    // blobs are not yet all present.
    let manifest_id = client.put_manifest(manifest)?;
    client.set_ref(ref_name, &manifest_id)?;

    Ok(PushReport {
        manifest_id,
        ref_name: ref_name.to_string(),
        blobs_total: hashes.len(),
        blobs_uploaded: missing.len(),
        bytes_total,
        bytes_uploaded: uploaded_bytes.load(Ordering::Relaxed),
        chunks_uploaded: 0,
        chunked: false,
        elapsed: start.elapsed(),
    })
}

/// The v2 path: per-blob chunk negotiation.
///
/// For each blob the hub is missing: send the chunks the hub lacks (raw
/// bytes, re-hashed server-side), then the ordered recipe so the hub can
/// assemble and store the whole blob (which keeps v1 pulls working).
/// Blobs without a local chunk sidecar (small files, pre-v2 ingests) take
/// the v1 whole-blob route — the chunk layer only exists from 64 KiB up.
fn push_chunks(
    store: &Store,
    client: &HubClient,
    manifest: &Manifest,
    ref_name: &str,
    progress: Progress<'_>,
) -> Result<PushReport> {
    let start = Instant::now();
    let blobs = manifest.referenced_blobs();
    let bytes_total: u64 = blobs.values().sum();
    let hashes: Vec<String> = blobs.keys().map(|h| h.to_string()).collect();

    let missing = client.missing(&hashes)?;
    // One locator for the whole push: chunk reads hit the local packs.
    let locator = ChunkLocator::scan(store.root());
    let uploaded_bytes = AtomicU64::new(0);
    let chunks_uploaded = AtomicUsize::new(0);
    let uploaded_blobs = AtomicUsize::new(0);

    run_pool(&missing, progress, |hash| {
        let Some(ids) = store.blob_chunks(hash)? else {
            // No local sidecar: whole blob, the v1 route.
            let n = client.put_blob(store, hash)?;
            uploaded_bytes.fetch_add(n, Ordering::Relaxed);
            uploaded_blobs.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        };
        let hex_ids: Vec<String> = ids.iter().map(hex::encode).collect();
        let resp = client.chunks_missing(hash, &hex_ids)?;
        for hex_id in &resp.missing {
            let id = decode_id(hex_id)?;
            // The sidecar says we have it; a failed read means the pack
            // layer is inconsistent (GC bug, disk loss) — fail loudly
            // rather than silently sending the whole blob.
            let Some(bytes) = read_chunk(store.root(), &locator, &id) else {
                return Err(HubError::Store(myc_store::StoreError::Chunk(format!(
                    "local pack lacks chunk {hex_id} of blob {hash}"
                ))));
            };
            client.put_chunk(hex_id, &bytes)?;
            uploaded_bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
            chunks_uploaded.fetch_add(1, Ordering::Relaxed);
        }
        // Even with zero missing chunks the hub needs the recipe to build
        // the blob file it does not have yet.
        let recipe = client.assemble_blob(hash, &hex_ids, mode_of(manifest, hash))?;
        uploaded_bytes.fetch_add(recipe, Ordering::Relaxed);
        uploaded_blobs.fetch_add(1, Ordering::Relaxed);
        Ok(())
    })?;

    let manifest_id = client.put_manifest(manifest)?;
    client.set_ref(ref_name, &manifest_id)?;

    Ok(PushReport {
        manifest_id,
        ref_name: ref_name.to_string(),
        blobs_total: hashes.len(),
        blobs_uploaded: uploaded_blobs.load(Ordering::Relaxed),
        bytes_total,
        bytes_uploaded: uploaded_bytes.load(Ordering::Relaxed),
        chunks_uploaded: chunks_uploaded.load(Ordering::Relaxed),
        chunked: true,
        elapsed: start.elapsed(),
    })
}

/// Resolve `reference` on the hub (ref name first, then manifest id) and
/// fetch the manifest into the local store, without any blobs.
/// Sets a local ref of the same name when `reference` is a name.
pub fn pull_manifest_only(
    store: &Store,
    client: &HubClient,
    reference: &str,
) -> Result<(String, Manifest)> {
    let (id, is_name) = match client.get_ref(reference)? {
        Some(id) => (id, true),
        // Only a well-formed manifest id can be fetched directly; anything
        // else is a name the hub does not have.
        None if crate::valid_manifest_id(reference) => (reference.to_string(), false),
        None => {
            return Err(HubError::NotFound {
                url: client.base_url().to_string(),
                what: format!("ref '{reference}'"),
            })
        }
    };
    let manifest = client.get_manifest(&id).map_err(|e| match e {
        HubError::NotFound { url, .. } => HubError::NotFound {
            url,
            what: format!("'{reference}' (neither a ref nor a manifest id)"),
        },
        other => other,
    })?;
    let stored_id = store.put_manifest(&manifest)?;
    debug_assert_eq!(stored_id, id, "hub verified the id on its side");
    if is_name {
        store.set_ref(reference, &id)?;
    }
    Ok((id, manifest))
}

/// Pull `reference` from the hub: manifest first, then only what the local
/// store is missing — chunks when both ends speak v2, whole blobs otherwise.
pub fn pull(
    store: &Store,
    client: &HubClient,
    reference: &str,
    progress: Progress<'_>,
) -> Result<PullReport> {
    match negotiate_mode(store, client)? {
        Mode::Chunks => pull_chunks(store, client, reference, progress),
        Mode::Blobs => pull_blobs(store, client, reference, progress),
    }
}

/// The v1 path, unchanged in behavior: whole blobs only.
fn pull_blobs(
    store: &Store,
    client: &HubClient,
    reference: &str,
    progress: Progress<'_>,
) -> Result<PullReport> {
    let start = Instant::now();
    let (id, manifest) = pull_manifest_only(store, client, reference)?;

    let blobs = manifest.referenced_blobs();
    let bytes_total: u64 = blobs.values().sum();
    let mode_of = |hash: &str| -> u32 { mode_of(&manifest, hash) };

    let missing = store.missing_blobs(&manifest);
    let downloaded_bytes = AtomicU64::new(0);
    run_pool(&missing, progress, |hash| {
        let n = client.fetch_blob(store, hash, mode_of(hash))?;
        downloaded_bytes.fetch_add(n, Ordering::Relaxed);
        Ok(())
    })?;

    Ok(PullReport {
        manifest_id: id,
        blobs_total: blobs.len(),
        blobs_downloaded: missing.len(),
        bytes_total,
        bytes_downloaded: downloaded_bytes.load(Ordering::Relaxed),
        chunks_downloaded: 0,
        chunked: false,
        elapsed: start.elapsed(),
    })
}

/// The v2 path: per-blob chunk negotiation, one blob at a time (assembly
/// is RAM-bound on the blob size, like the store's own indexer).
///
/// For each missing blob: fetch the hub's chunk recipe, download only the
/// chunks the local packs lack (each re-hashed against its id), rebuild
/// the full bytes from local + downloaded chunks, verify the blob hash,
/// and let `put_blob_with_mode` store it — which also indexes the new
/// chunks into the local packs, so the next delta is even smaller.
/// Blobs the hub has no recipe for (pushed via v1) fall back to the
/// whole-blob route.
fn pull_chunks(
    store: &Store,
    client: &HubClient,
    reference: &str,
    progress: Progress<'_>,
) -> Result<PullReport> {
    let start = Instant::now();
    let (id, manifest) = pull_manifest_only(store, client, reference)?;
    let blobs = manifest.referenced_blobs();
    let bytes_total: u64 = blobs.values().sum();
    let missing = store.missing_blobs(&manifest);
    let locator = ChunkLocator::scan(store.root());
    let downloaded_bytes = AtomicU64::new(0);
    let chunks_downloaded = AtomicUsize::new(0);

    for (done, hash) in missing.iter().enumerate() {
        pull_one_blob_chunked(
            store,
            client,
            &locator,
            hash,
            mode_of(&manifest, hash),
            &downloaded_bytes,
            &chunks_downloaded,
        )?;
        progress(done + 1, missing.len());
    }

    Ok(PullReport {
        manifest_id: id,
        blobs_total: blobs.len(),
        blobs_downloaded: missing.len(),
        bytes_total,
        bytes_downloaded: downloaded_bytes.load(Ordering::Relaxed),
        chunks_downloaded: chunks_downloaded.load(Ordering::Relaxed),
        chunked: true,
        elapsed: start.elapsed(),
    })
}

fn pull_one_blob_chunked(
    store: &Store,
    client: &HubClient,
    locator: &ChunkLocator,
    hash: &str,
    mode: u32,
    downloaded_bytes: &AtomicU64,
    chunks_downloaded: &AtomicUsize,
) -> Result<()> {
    let Some(hex_ids) = client.blob_chunks(hash)? else {
        // The hub has no recipe for this blob (v1 push or below the
        // chunking threshold): fetch it whole.
        let n = client.fetch_blob(store, hash, mode)?;
        downloaded_bytes.fetch_add(n, Ordering::Relaxed);
        return Ok(());
    };
    let ids: Vec<ChunkId> = hex_ids
        .iter()
        .map(|h| decode_id(h))
        .collect::<Result<_>>()?;

    // Exactly the chunks the local packs lack.
    let lacking = store.missing_chunks(&ids);
    let mut bank: std::collections::HashMap<ChunkId, Vec<u8>> = std::collections::HashMap::new();
    for id in &lacking {
        let bytes = client.get_chunk(&hex::encode(id))?;
        downloaded_bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        chunks_downloaded.fetch_add(1, Ordering::Relaxed);
        bank.insert(*id, bytes);
    }

    // Rebuild in recipe order: downloaded chunks + ones already local.
    let mut full = Vec::new();
    for id in &ids {
        let bytes = match bank.get(id) {
            Some(b) => b,
            None => {
                let b = read_chunk(store.root(), locator, id).ok_or_else(|| {
                    HubError::Store(myc_store::StoreError::Chunk(format!(
                        "local pack lacks chunk {} of blob {hash}",
                        hex::encode(id)
                    )))
                })?;
                // Cache so repeated ids (self-similar blobs) read once.
                bank.entry(*id).or_insert(b)
            }
        };
        full.extend_from_slice(bytes);
    }
    // End-to-end guarantee, same as the server's assembly: the rebuilt
    // bytes must hash to the blob the manifest references.
    let actual = blake3::hash(&full).to_hex().to_string();
    if actual != hash {
        return Err(HubError::CorruptBlob {
            expected: hash.to_string(),
            actual,
        });
    }
    store.put_blob_with_mode(&mut &full[..], mode)?;
    Ok(())
}

/// Run `work` over `items` on up to [`TRANSFER_THREADS`] scoped threads,
/// reporting progress and propagating the first error.
fn run_pool<F>(items: &[String], progress: Progress<'_>, work: F) -> Result<()>
where
    F: Fn(&str) -> Result<()> + Sync,
{
    if items.is_empty() {
        return Ok(());
    }
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let first_error: Mutex<Option<HubError>> = Mutex::new(None);
    let threads = TRANSFER_THREADS.min(items.len());
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| loop {
                if first_error.lock().unwrap().is_some() {
                    return;
                }
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(hash) = items.get(i) else { return };
                match work(hash) {
                    Ok(()) => {
                        let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                        progress(d, items.len());
                    }
                    Err(e) => {
                        first_error.lock().unwrap().get_or_insert(e);
                        return;
                    }
                }
            });
        }
    });
    match first_error.into_inner().unwrap() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
