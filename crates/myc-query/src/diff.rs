//! Exact file-by-file comparison of two environments.
//!
//! Docker can only diff opaque layers; Mycel manifests list every file with
//! its content hash, so a diff is a set operation — no filesystems touched.

use myc_manifest::{Entry, Manifest};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// Present in B, absent in A.
    Added,
    /// Present in A, absent in B.
    Removed,
    /// Content, kind or symlink target changed.
    Modified,
    /// Same content, different mode/uid/gid.
    MetaChanged,
}

#[derive(Debug, Clone, Serialize)]
pub struct Change {
    pub path: String,
    pub change: ChangeKind,
    /// Size in A (0 when added).
    pub size_a: u64,
    /// Size in B (0 when removed).
    pub size_b: u64,
    /// Human-readable explanation, e.g. `mode 644 -> 755`.
    pub detail: String,
}

#[derive(Debug, Default, Serialize)]
pub struct DiffReport {
    pub added: usize,
    pub removed: usize,
    pub modified: usize,
    pub meta_changed: usize,
    pub unchanged: usize,
    /// Bytes of content in B whose blobs A does not reference — the true
    /// transfer/storage cost of going from A to B.
    pub new_bytes: u64,
    /// Sorted by path.
    pub changes: Vec<Change>,
}

impl DiffReport {
    pub fn total_changes(&self) -> usize {
        self.added + self.removed + self.modified + self.meta_changed
    }
}

fn entry_detail(a: &Entry, b: &Entry) -> Option<(ChangeKind, String)> {
    if a.kind != b.kind {
        return Some((
            ChangeKind::Modified,
            format!("kind {:?} -> {:?}", a.kind, b.kind),
        ));
    }
    if a.blake3 != b.blake3 {
        return Some((ChangeKind::Modified, "content changed".to_string()));
    }
    if a.target != b.target {
        return Some((
            ChangeKind::Modified,
            format!(
                "symlink target {} -> {}",
                a.target.as_deref().unwrap_or("?"),
                b.target.as_deref().unwrap_or("?")
            ),
        ));
    }
    if a.mode != b.mode {
        return Some((
            ChangeKind::MetaChanged,
            format!("mode {:o} -> {:o}", a.mode, b.mode),
        ));
    }
    if a.uid != b.uid || a.gid != b.gid {
        return Some((
            ChangeKind::MetaChanged,
            format!("owner {}:{} -> {}:{}", a.uid, a.gid, b.uid, b.gid),
        ));
    }
    None
}

/// Compare environment `a` to environment `b` (direction: what changed
/// going from A to B).
pub fn diff(a: &Manifest, b: &Manifest) -> DiffReport {
    let map_a: BTreeMap<&str, &Entry> = a.entries.iter().map(|e| (e.path.as_str(), e)).collect();
    let map_b: BTreeMap<&str, &Entry> = b.entries.iter().map(|e| (e.path.as_str(), e)).collect();

    let mut report = DiffReport::default();

    for (path, ea) in &map_a {
        match map_b.get(path) {
            None => {
                report.removed += 1;
                report.changes.push(Change {
                    path: path.to_string(),
                    change: ChangeKind::Removed,
                    size_a: ea.size,
                    size_b: 0,
                    detail: String::new(),
                });
            }
            Some(eb) => match entry_detail(ea, eb) {
                Some((kind, detail)) => {
                    match kind {
                        ChangeKind::Modified => report.modified += 1,
                        ChangeKind::MetaChanged => report.meta_changed += 1,
                        _ => unreachable!(),
                    }
                    report.changes.push(Change {
                        path: path.to_string(),
                        change: kind,
                        size_a: ea.size,
                        size_b: eb.size,
                        detail,
                    });
                }
                None => report.unchanged += 1,
            },
        }
    }
    for (path, eb) in &map_b {
        if !map_a.contains_key(path) {
            report.added += 1;
            report.changes.push(Change {
                path: path.to_string(),
                change: ChangeKind::Added,
                size_a: 0,
                size_b: eb.size,
                detail: String::new(),
            });
        }
    }
    report.changes.sort_by(|x, y| x.path.cmp(&y.path));

    // Content B brings that A does not already have, deduplicated by blob.
    let blobs_a = a.referenced_blobs();
    report.new_bytes = b
        .referenced_blobs()
        .iter()
        .filter(|(h, _)| !blobs_a.contains_key(*h))
        .map(|(_, size)| *size)
        .sum();

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use myc_manifest::{EntryKind, RuntimeConfig, SCHEMA_VERSION};

    fn entry(path: &str, hash: Option<&str>, size: u64, mode: u32) -> Entry {
        Entry {
            path: path.into(),
            kind: if hash.is_some() {
                EntryKind::File
            } else {
                EntryKind::Dir
            },
            mode,
            uid: 0,
            gid: 0,
            size,
            blake3: hash.map(String::from),
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
    fn detects_added_removed_modified_meta() {
        let a = manifest(
            "app:v1",
            vec![
                entry("/bin", None, 0, 0o755),
                entry("/bin/app", Some("aaaa"), 100, 0o755),
                entry("/etc/old.conf", Some("bbbb"), 10, 0o644),
                entry("/etc/perm", Some("cccc"), 5, 0o644),
                entry("/lib/same", Some("dddd"), 50, 0o644),
            ],
        );
        let b = manifest(
            "app:v2",
            vec![
                entry("/bin", None, 0, 0o755),
                entry("/bin/app", Some("eeee"), 120, 0o755), // content changed
                entry("/etc/new.conf", Some("ffff"), 20, 0o644), // added
                entry("/etc/perm", Some("cccc"), 5, 0o600),  // mode only
                entry("/lib/same", Some("dddd"), 50, 0o644), // unchanged
            ],
        );
        let r = diff(&a, &b);
        assert_eq!(r.added, 1);
        assert_eq!(r.removed, 1);
        assert_eq!(r.modified, 1);
        assert_eq!(r.meta_changed, 1);
        assert_eq!(r.unchanged, 2); // /bin and /lib/same
        assert_eq!(r.total_changes(), 4);
        // new blobs in B: eeee (120) + ffff (20)
        assert_eq!(r.new_bytes, 140);

        let by_path: std::collections::HashMap<_, _> =
            r.changes.iter().map(|c| (c.path.as_str(), c)).collect();
        assert_eq!(by_path["/bin/app"].change, ChangeKind::Modified);
        assert_eq!(by_path["/etc/new.conf"].change, ChangeKind::Added);
        assert_eq!(by_path["/etc/old.conf"].change, ChangeKind::Removed);
        assert_eq!(by_path["/etc/perm"].change, ChangeKind::MetaChanged);
        assert!(by_path["/etc/perm"].detail.contains("644"));
    }

    #[test]
    fn identical_manifests_produce_empty_diff() {
        let m = manifest("x:1", vec![entry("/bin/app", Some("aaaa"), 100, 0o755)]);
        let r = diff(&m, &m.clone());
        assert_eq!(r.total_changes(), 0);
        assert_eq!(r.new_bytes, 0);
        assert_eq!(r.unchanged, 1);
    }

    #[test]
    fn shared_blob_not_counted_as_new_bytes() {
        // Same content moved to a new path: no new bytes to transfer.
        let a = manifest("a", vec![entry("/old/path", Some("aaaa"), 100, 0o644)]);
        let b = manifest("b", vec![entry("/new/path", Some("aaaa"), 100, 0o644)]);
        let r = diff(&a, &b);
        assert_eq!(r.added, 1);
        assert_eq!(r.removed, 1);
        assert_eq!(r.new_bytes, 0);
    }
}
