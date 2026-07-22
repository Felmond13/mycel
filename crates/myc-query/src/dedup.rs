//! Per-environment deduplication accounting: how much of each environment
//! is already shared with the rest of the store, and how much is unique to
//! it. This quantifies the "90% of your new image was already on disk" win.

use myc_manifest::Manifest;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize)]
pub struct ManifestDedup {
    pub id: String,
    pub name: String,
    /// Unique blobs referenced by this manifest.
    pub blobs: usize,
    /// Blobs also referenced by at least one other manifest.
    pub shared_blobs: usize,
    /// Blobs referenced by this manifest alone.
    pub unique_blobs: usize,
    pub shared_bytes: u64,
    pub unique_bytes: u64,
    /// shared_bytes / (shared_bytes + unique_bytes), 0..=1.
    pub shared_ratio: f64,
}

/// Compute per-manifest dedup statistics across the whole store.
pub fn dedup_report(manifests: &[(String, Manifest)]) -> Vec<ManifestDedup> {
    // How many manifests reference each blob.
    let mut owners: HashMap<&str, u32> = HashMap::new();
    for (_, m) in manifests {
        for hash in m.referenced_blobs().keys() {
            *owners.entry(*hash).or_insert(0) += 1;
        }
    }

    let mut out = Vec::with_capacity(manifests.len());
    for (id, m) in manifests {
        let mut shared_blobs = 0usize;
        let mut unique_blobs = 0usize;
        let mut shared_bytes = 0u64;
        let mut unique_bytes = 0u64;
        for (hash, size) in m.referenced_blobs() {
            if owners.get(hash).copied().unwrap_or(0) > 1 {
                shared_blobs += 1;
                shared_bytes += size;
            } else {
                unique_blobs += 1;
                unique_bytes += size;
            }
        }
        let total = shared_bytes + unique_bytes;
        out.push(ManifestDedup {
            id: id.clone(),
            name: m.name.clone(),
            blobs: shared_blobs + unique_blobs,
            shared_blobs,
            unique_blobs,
            shared_bytes,
            unique_bytes,
            shared_ratio: if total > 0 {
                shared_bytes as f64 / total as f64
            } else {
                0.0
            },
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use myc_manifest::{Entry, EntryKind, RuntimeConfig, SCHEMA_VERSION};

    fn file(path: &str, hash: &str, size: u64) -> Entry {
        Entry {
            path: path.into(),
            kind: EntryKind::File,
            mode: 0o644,
            uid: 0,
            gid: 0,
            size,
            blake3: Some(hash.into()),
            target: None,
            device: None,
        }
    }

    fn manifest(name: &str, entries: Vec<Entry>) -> Manifest {
        Manifest {
            schema: SCHEMA_VERSION,
            name: name.into(),
            origin: None,
            os: "linux".into(),
            arch: "amd64".into(),
            created: "2026-01-01T00:00:00Z".into(),
            config: RuntimeConfig::default(),
            entries,
        }
    }

    #[test]
    fn shared_and_unique_blobs_are_split() {
        let manifests = vec![
            (
                "myc1-a".to_string(),
                manifest(
                    "a",
                    vec![
                        file("/common", "hash-common", 100),
                        file("/only-a", "hash-a", 10),
                    ],
                ),
            ),
            (
                "myc1-b".to_string(),
                manifest(
                    "b",
                    vec![
                        file("/common", "hash-common", 100),
                        file("/only-b", "hash-b", 30),
                    ],
                ),
            ),
        ];
        let report = dedup_report(&manifests);
        assert_eq!(report.len(), 2);
        let a = &report[0];
        assert_eq!(a.blobs, 2);
        assert_eq!(a.shared_blobs, 1);
        assert_eq!(a.unique_blobs, 1);
        assert_eq!(a.shared_bytes, 100);
        assert_eq!(a.unique_bytes, 10);
        assert!((a.shared_ratio - 100.0 / 110.0).abs() < 1e-9);
    }

    #[test]
    fn single_manifest_is_all_unique() {
        let manifests = vec![(
            "myc1-a".to_string(),
            manifest("solo", vec![file("/f", "h1", 5)]),
        )];
        let report = dedup_report(&manifests);
        assert_eq!(report[0].shared_blobs, 0);
        assert_eq!(report[0].unique_blobs, 1);
        assert_eq!(report[0].shared_ratio, 0.0);
    }
}
