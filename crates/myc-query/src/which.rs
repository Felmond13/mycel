//! Reverse queries over the whole store: "which environments contain this
//! exact blob / this path?"
//!
//! This is the "which machines run the vulnerable OpenSSL" story: given a
//! content hash (or a path fragment), every affected environment is found by
//! scanning manifests — no filesystem is ever walked.

use myc_manifest::Manifest;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchBy {
    /// The query is a prefix of the entry's BLAKE3 hash.
    Hash,
    /// The query is a substring of the entry's path.
    Path,
}

#[derive(Debug, Clone, Serialize)]
pub struct WhichHit {
    pub manifest_id: String,
    pub manifest_name: String,
    pub path: String,
    pub blake3: Option<String>,
    pub size: u64,
    pub matched: MatchBy,
}

fn looks_like_hash(query: &str) -> bool {
    query.len() >= 6 && query.chars().all(|c| c.is_ascii_hexdigit())
}

/// Find every entry across `manifests` matching `query`, by content-hash
/// prefix (when the query is plausible hex) and by path substring (always).
pub fn which(manifests: &[(String, Manifest)], query: &str) -> Vec<WhichHit> {
    let hash_query = looks_like_hash(query).then(|| query.to_ascii_lowercase());
    let mut hits = Vec::new();
    for (id, manifest) in manifests {
        for entry in &manifest.entries {
            let by_hash = hash_query
                .as_deref()
                .zip(entry.blake3.as_deref())
                .is_some_and(|(q, h)| h.starts_with(q));
            let matched = if by_hash {
                MatchBy::Hash
            } else if entry.path.contains(query) {
                MatchBy::Path
            } else {
                continue;
            };
            hits.push(WhichHit {
                manifest_id: id.clone(),
                manifest_name: manifest.name.clone(),
                path: entry.path.clone(),
                blake3: entry.blake3.clone(),
                size: entry.size,
                matched,
            });
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use myc_manifest::{Entry, EntryKind, RuntimeConfig, SCHEMA_VERSION};

    fn file(path: &str, hash: &str) -> Entry {
        Entry {
            path: path.into(),
            kind: EntryKind::File,
            mode: 0o755,
            uid: 0,
            gid: 0,
            size: 42,
            blake3: Some(hash.into()),
            target: None,
            device: None,
        }
    }

    fn store() -> Vec<(String, Manifest)> {
        let mk = |name: &str, entries: Vec<Entry>| Manifest {
            schema: SCHEMA_VERSION,
            name: name.into(),
            origin: None,
            os: "linux".into(),
            arch: "amd64".into(),
            created: "2026-01-01T00:00:00Z".into(),
            config: RuntimeConfig::default(),
            entries,
        };
        vec![
            (
                "myc1-aaa".into(),
                mk(
                    "web:v1",
                    vec![
                        file("/usr/lib/libssl.so.3", "deadbeef00112233"),
                        file("/bin/busybox", "cafebabe00112233"),
                    ],
                ),
            ),
            (
                "myc1-bbb".into(),
                mk(
                    "api:v1",
                    vec![file("/usr/lib/libssl.so.3", "deadbeef00112233")],
                ),
            ),
            (
                "myc1-ccc".into(),
                mk(
                    "db:v1",
                    vec![file("/usr/lib/libcrypto.so.3", "0123456789abcdef")],
                ),
            ),
        ]
    }

    #[test]
    fn finds_by_hash_prefix_across_manifests() {
        let hits = which(&store(), "deadbeef");
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.matched == MatchBy::Hash));
        let names: Vec<_> = hits.iter().map(|h| h.manifest_name.as_str()).collect();
        assert!(names.contains(&"web:v1") && names.contains(&"api:v1"));
    }

    #[test]
    fn finds_by_path_substring() {
        let hits = which(&store(), "libssl");
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.matched == MatchBy::Path));

        let hits = which(&store(), "/bin/busybox");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].manifest_name, "web:v1");
    }

    #[test]
    fn short_or_non_hex_query_is_path_only() {
        // "cafe" is hex but too short to be treated as a hash prefix.
        assert!(which(&store(), "cafe").is_empty());
        assert!(which(&store(), "no-such-thing").is_empty());
    }
}
