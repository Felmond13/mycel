//! v1 blob cache for packs-only stores (`MYCEL_CHUNK_MODE=packs-only`).
//!
//! In packs-only mode chunked files have no permanent v1 blob: packs are
//! the source of truth. When something needs the actual *file* — rootfs
//! materialization hardlinks, export, hub transfer of a whole blob — the
//! blob is reassembled from its packs (BLAKE3-verified) and written into
//! `objects/` like any v1 blob. This module tracks those reconstructed
//! blobs so they stay a **cache**:
//!
//! * a marker file per reassembled blob (`blob-cache/<hash>`, content =
//!   the blob size, mtime = last use) records cache membership;
//! * [`evict`] keeps the cache under a budget (`MYCEL_BLOB_CACHE_MB`,
//!   default 512 MiB) by deleting the least-recently-used reassembled
//!   blobs that no stored manifest references — deleting a cached blob
//!   never deletes its chunks or packs, so it can always be rebuilt;
//! * [`sweep_orphans`] drops markers whose blob file disappeared (e.g.
//!   the v1 pass of `gc` deleted an unreferenced one).
//!
//! Blobs referenced by a live manifest are protected: they are the ones a
//! future `myc run` will hardlink again, so evicting them would only
//! trade a reassembly for a re-reassembly.

use crate::{io_err, Result};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Default cache budget in MiB (`MYCEL_BLOB_CACHE_MB` overrides).
pub const DEFAULT_BLOB_CACHE_MB: u64 = 512;

fn marker_dir(root: &Path) -> PathBuf {
    root.join("blob-cache")
}

fn marker_path(root: &Path, blob_hash: &str) -> PathBuf {
    marker_dir(root).join(blob_hash)
}

fn blob_file_path(root: &Path, blob_hash: &str) -> PathBuf {
    // Same layout as Store::blob_path (2-hex shard).
    let shard = &blob_hash[..2.min(blob_hash.len())];
    root.join("objects").join(shard).join(blob_hash)
}

/// `$MYCEL_BLOB_CACHE_MB` in bytes (default 512 MiB; 0 = keep the budget
/// but protect live blobs — eviction only removes unreferenced entries).
pub fn budget_from_env() -> u64 {
    std::env::var("MYCEL_BLOB_CACHE_MB")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_BLOB_CACHE_MB)
        .saturating_mul(1024 * 1024)
}

/// Record a freshly reassembled blob as cached (size in the marker, mtime
/// = now).
pub fn record(root: &Path, blob_hash: &str, size: u64) -> Result<()> {
    let dir = marker_dir(root);
    fs::create_dir_all(&dir).map_err(|e| io_err(&dir, e))?;
    let p = marker_path(root, blob_hash);
    // Rewriting the marker updates its mtime, which is the LRU clock.
    fs::write(&p, size.to_string()).map_err(|e| io_err(&p, e))
}

/// Mark a cached blob as used (LRU touch); no-op for unknown blobs.
pub fn touch(root: &Path, blob_hash: &str) {
    let p = marker_path(root, blob_hash);
    if let Ok(size) = fs::read_to_string(&p) {
        let _ = fs::write(&p, size);
    }
}

/// Is this blob currently cached (i.e. was it reassembled on demand)?
pub fn is_cached(root: &Path, blob_hash: &str) -> bool {
    marker_path(root, blob_hash).exists()
}

/// (count, total bytes) of cached blobs.
pub fn stats(root: &Path) -> Result<(u64, u64)> {
    let mut count = 0u64;
    let mut bytes = 0u64;
    for (hash, _size, _mtime) in entries(root)? {
        count += 1;
        bytes += blob_len(root, &hash);
    }
    Ok((count, bytes))
}

/// Delete LRU-first the reassembled blobs that no live manifest
/// references, until the cache fits `budget_bytes`. Returns (evicted
/// blobs, bytes freed). Protected (live) blobs are never evicted — even
/// past budget, deleting them would only cause a rebuild on next use.
pub fn evict(root: &Path, budget_bytes: u64, protected: &HashSet<String>) -> Result<(u64, u64)> {
    let mut entries = entries(root)?;
    let mut total: u64 = entries.iter().map(|(_, size, _)| size).sum();
    if total <= budget_bytes {
        return Ok((0, 0));
    }
    // Oldest last-use first.
    entries.sort_by_key(|(_, _, mtime)| *mtime);
    let mut evicted = 0u64;
    let mut freed = 0u64;
    for (hash, size, _) in entries {
        if total <= budget_bytes {
            break;
        }
        if protected.contains(&hash) {
            continue;
        }
        let blob = blob_file_path(root, &hash);
        if blob.exists() {
            // Blobs are stored read-only; restore write bits to unlink.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&blob, fs::Permissions::from_mode(0o644));
            }
            fs::remove_file(&blob).map_err(|e| io_err(&blob, e))?;
        }
        let _ = fs::remove_file(marker_path(root, &hash));
        evicted += 1;
        freed += size;
        total -= size;
    }
    Ok((evicted, freed))
}

/// Drop markers whose blob file no longer exists (returns the count).
pub fn sweep_orphans(root: &Path) -> Result<u64> {
    let mut swept = 0u64;
    for (hash, _, _) in entries(root)? {
        if !blob_file_path(root, &hash).exists() {
            let _ = fs::remove_file(marker_path(root, &hash));
            swept += 1;
        }
    }
    Ok(swept)
}

/// One entry per marker: (blob hash, size bytes, last-use mtime).
fn entries(root: &Path) -> Result<Vec<(String, u64, std::time::SystemTime)>> {
    let dir = marker_dir(root);
    let read = match fs::read_dir(&dir) {
        Ok(read) => read,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err(&dir, e)),
    };
    let mut out = Vec::new();
    for entry in read {
        let entry = entry.map_err(|e| io_err(&dir, e))?;
        let hash = entry.file_name().to_string_lossy().to_string();
        let meta = entry.metadata().map_err(|e| io_err(&entry.path(), e))?;
        let size = fs::read_to_string(entry.path())
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or_else(|| blob_len(root, &hash));
        let mtime = meta.modified().map_err(|e| io_err(&entry.path(), e))?;
        out.push((hash, size, mtime));
    }
    Ok(out)
}

/// On-disk size of the cached blob (0 if the file is gone).
fn blob_len(root: &Path, hash: &str) -> u64 {
    fs::metadata(blob_file_path(root, hash))
        .map(|m| m.len())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn write_blob(root: &Path, hash: &str, bytes: &[u8]) {
        let p = blob_file_path(root, hash);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, bytes).unwrap();
    }

    #[test]
    fn record_touch_and_stats() {
        let dir = tmp();
        let root = dir.path();
        let hash = format!("ab{}", "x".repeat(62));
        write_blob(root, &hash, b"0123456789");
        assert!(!is_cached(root, &hash));
        record(root, &hash, 10).unwrap();
        assert!(is_cached(root, &hash));
        touch(root, &hash);
        let (count, bytes) = stats(root).unwrap();
        assert_eq!((count, bytes), (1, 10));
    }

    #[test]
    fn evict_oldest_unprotected_first() {
        let dir = tmp();
        let root = dir.path();
        let a = format!("aa{}", "a".repeat(62));
        let b = format!("bb{}", "b".repeat(62));
        let c = format!("cc{}", "c".repeat(62));
        for (h, data) in [
            (&a, vec![0u8; 100]),
            (&b, vec![1u8; 100]),
            (&c, vec![2u8; 100]),
        ] {
            write_blob(root, h, &data);
        }
        record(root, &a, 100).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(15));
        record(root, &b, 100).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(15));
        record(root, &c, 100).unwrap();
        let mut protected = HashSet::new();
        protected.insert(c.clone()); // newest is live
                                     // Budget 250 < 300 total: only `a` (oldest, unprotected) goes.
        let (evicted, freed) = evict(root, 250, &protected).unwrap();
        assert_eq!((evicted, freed), (1, 100));
        assert!(!blob_file_path(root, &a).exists());
        assert!(blob_file_path(root, &b).exists());
        assert!(blob_file_path(root, &c).exists());
        // Budget 0: everything unprotected goes, protected survives.
        let (evicted, _) = evict(root, 0, &protected).unwrap();
        assert_eq!(evicted, 1);
        assert!(blob_file_path(root, &c).exists());
        assert!(is_cached(root, &c));
    }

    #[test]
    fn sweep_orphans_removes_dead_markers() {
        let dir = tmp();
        let root = dir.path();
        let a = format!("aa{}", "a".repeat(62));
        write_blob(root, &a, b"data");
        record(root, &a, 4).unwrap();
        assert_eq!(sweep_orphans(root).unwrap(), 0);
        fs::remove_file(blob_file_path(root, &a)).unwrap();
        assert_eq!(sweep_orphans(root).unwrap(), 1);
        assert!(!is_cached(root, &a));
    }
}
