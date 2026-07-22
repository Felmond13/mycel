//! Push/pull negotiation: move a manifest between a local store and a hub
//! while transferring only the blobs the receiving side is missing.
//!
//! Both directions run blob transfers on a small thread pool — blobs are
//! independent files, the store's atomic writes make concurrent inserts
//! safe, and reqwest's connection pool keeps it to a few sockets.

use crate::client::{HubClient, HubError, Result};
use myc_manifest::Manifest;
use myc_store::Store;
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
    /// Blobs the hub was missing (actually uploaded).
    pub blobs_uploaded: usize,
    pub bytes_total: u64,
    pub bytes_uploaded: u64,
    pub elapsed: Duration,
}

/// Outcome of a `myc pull` from a hub.
#[derive(Debug)]
pub struct PullReport {
    pub manifest_id: String,
    pub blobs_total: usize,
    /// Blobs the local store was missing (actually downloaded).
    pub blobs_downloaded: usize,
    pub bytes_total: u64,
    pub bytes_downloaded: u64,
    pub elapsed: Duration,
}

/// Progress callback: (blobs done, blobs to transfer).
pub type Progress<'a> = &'a (dyn Fn(usize, usize) + Sync);

/// No-op progress.
pub fn silent() -> Progress<'static> {
    &|_, _| {}
}

/// Push `manifest` (already resolved locally) to the hub under `ref_name`:
/// upload the manifest, negotiate missing blobs, upload only those, set the
/// ref. Safe to re-run; a second push uploads nothing.
pub fn push(
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

/// Pull `reference` from the hub: manifest first, then only the blobs the
/// local store is missing.
pub fn pull(
    store: &Store,
    client: &HubClient,
    reference: &str,
    progress: Progress<'_>,
) -> Result<PullReport> {
    let start = Instant::now();
    let (id, manifest) = pull_manifest_only(store, client, reference)?;

    let blobs = manifest.referenced_blobs();
    let bytes_total: u64 = blobs.values().sum();
    // Best permission bits per blob so the stored inode is materialization-
    // ready (put_blob strips write bits anyway).
    let mode_of = |hash: &str| -> u32 {
        manifest
            .entries
            .iter()
            .find(|e| e.blake3.as_deref() == Some(hash))
            .map(|e| e.mode)
            .unwrap_or(0o444)
    };

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
        elapsed: start.elapsed(),
    })
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
