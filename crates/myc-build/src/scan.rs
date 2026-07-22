//! Post-step rootfs scanning: walk the rootfs a build step just ran in,
//! hash every file, and fold new/modified/removed paths back into the
//! in-memory entry tree.
//!
//! Runtime artifacts are excluded: `/proc`, `/dev`, `/tmp` and `/.oldroot`
//! are mounted or managed by the runner, and `/etc/resolv.conf` +
//! `/etc/hosts` are injected for host networking. Pre-step entries under
//! those paths are preserved verbatim so they never leak into or vanish
//! from the image.

use crate::tree::{insert, FileTree};
use crate::{BuildError, Result};
use myc_manifest::{Entry, EntryKind};
use myc_store::Store;
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;

/// Directory subtrees the runtime owns.
const SKIP_DIRS: [&str; 4] = ["/proc", "/dev", "/tmp", "/.oldroot"];
/// Files the runner injects for DNS / hostname resolution.
const SKIP_FILES: [&str; 2] = ["/etc/resolv.conf", "/etc/hosts"];

fn skipped(path: &str) -> bool {
    SKIP_FILES.contains(&path)
        || SKIP_DIRS
            .iter()
            .any(|d| path == *d || path.starts_with(&format!("{d}/")))
}

#[derive(Debug, Default)]
pub struct ScanOutcome {
    pub added: usize,
    pub modified: usize,
    pub removed: usize,
    pub blobs_added: u64,
    pub bytes_added: u64,
}

fn io_err(path: &Path, source: io::Error) -> BuildError {
    BuildError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(unix)]
fn disk_mode(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn disk_mode(_meta: &fs::Metadata) -> u32 {
    0o644
}

/// Diff `rootfs` against `tree` (the pre-step entry set) and update the tree
/// in place, storing new file contents.
pub fn scan_rootfs(store: &Store, rootfs: &Path, tree: &mut FileTree) -> Result<ScanOutcome> {
    let mut outcome = ScanOutcome::default();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    walk(store, rootfs, rootfs, tree, &mut seen, &mut outcome)?;

    // Removals: pre-step paths that no longer exist on disk (and are not
    // runtime-owned, which the walk never visits).
    let gone: Vec<String> = tree
        .keys()
        .filter(|p| !seen.contains(*p) && !skipped(p))
        .cloned()
        .collect();
    outcome.removed = gone.len();
    for p in gone {
        tree.remove(&p);
    }
    Ok(outcome)
}

fn walk(
    store: &Store,
    rootfs: &Path,
    dir: &Path,
    tree: &mut FileTree,
    seen: &mut BTreeSet<String>,
    outcome: &mut ScanOutcome,
) -> Result<()> {
    let mut children: Vec<_> = fs::read_dir(dir)
        .map_err(|e| io_err(dir, e))?
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| io_err(dir, e))?;
    children.sort_by_key(|c| c.file_name());

    for child in children {
        let child_path = child.path();
        let rel = child_path
            .strip_prefix(rootfs)
            .expect("walk stays under rootfs");
        let path = format!("/{}", rel.to_string_lossy().replace('\\', "/"));
        if skipped(&path) {
            continue;
        }
        let meta = fs::symlink_metadata(&child_path).map_err(|e| io_err(&child_path, e))?;
        let file_type = meta.file_type();

        if file_type.is_symlink() {
            let target = fs::read_link(&child_path)
                .map_err(|e| io_err(&child_path, e))?
                .to_string_lossy()
                .to_string();
            seen.insert(path.clone());
            let pre = tree.get(&path);
            let unchanged = pre.is_some_and(|e| {
                e.kind == EntryKind::Symlink && e.target.as_deref() == Some(target.as_str())
            });
            if !unchanged {
                if pre.is_some() {
                    outcome.modified += 1;
                } else {
                    outcome.added += 1;
                }
                insert(
                    tree,
                    Entry {
                        path,
                        kind: EntryKind::Symlink,
                        mode: 0o777,
                        uid: 0,
                        gid: 0,
                        size: 0,
                        blake3: None,
                        target: Some(target),
                        device: None,
                    },
                );
            }
        } else if file_type.is_dir() {
            seen.insert(path.clone());
            let mode = disk_mode(&meta);
            match tree.get(&path) {
                Some(pre) if pre.kind == EntryKind::Dir => {
                    if pre.mode != mode {
                        // chmod'ed directory: keep ownership, take the new mode.
                        let mut e = pre.clone();
                        e.mode = mode;
                        outcome.modified += 1;
                        tree.insert(path.clone(), e);
                    }
                }
                pre => {
                    if pre.is_some() {
                        outcome.modified += 1;
                    } else {
                        outcome.added += 1;
                    }
                    insert(
                        tree,
                        Entry {
                            path: path.clone(),
                            kind: EntryKind::Dir,
                            mode,
                            uid: 0,
                            gid: 0,
                            size: 0,
                            blake3: None,
                            target: None,
                            device: None,
                        },
                    );
                }
            }
            walk(store, rootfs, &child_path, tree, seen, outcome)?;
        } else if file_type.is_file() {
            seen.insert(path.clone());
            let mode = disk_mode(&meta);
            let size = meta.len();
            let hash = hash_file(&child_path)?;
            match tree.get(&path) {
                // Unchanged content: keep the pre-step entry's ownership
                // (rootless builds cannot observe the original uid/gid on
                // disk), but honor a chmod.
                Some(pre)
                    if pre.kind == EntryKind::File
                        && pre.size == size
                        && pre.blake3.as_deref() == Some(hash.as_str()) =>
                {
                    if pre.mode != mode {
                        let mut e = pre.clone();
                        e.mode = mode;
                        outcome.modified += 1;
                        tree.insert(path.clone(), e);
                    }
                }
                pre => {
                    if pre.is_some() {
                        outcome.modified += 1;
                    } else {
                        outcome.added += 1;
                    }
                    if !store.has_blob(&hash) {
                        let mut f =
                            fs::File::open(&child_path).map_err(|e| io_err(&child_path, e))?;
                        store.put_blob_with_mode(&mut f, mode)?;
                        outcome.blobs_added += 1;
                        outcome.bytes_added += size;
                    }
                    // Step ran as root in the user namespace; our uid IS
                    // uid 0 from the container's perspective.
                    insert(
                        tree,
                        Entry {
                            path: path.clone(),
                            kind: EntryKind::File,
                            mode,
                            uid: 0,
                            gid: 0,
                            size,
                            blake3: Some(hash),
                            target: None,
                            device: None,
                        },
                    );
                }
            }
        } else {
            // FIFOs created by a step are recorded; sockets and device
            // nodes are runtime state and skipped (mknod is impossible in a
            // rootless build anyway).
            #[cfg(unix)]
            {
                use std::os::unix::fs::FileTypeExt;
                if file_type.is_fifo() {
                    seen.insert(path.clone());
                    if !tree.contains_key(&path) {
                        outcome.added += 1;
                        insert(
                            tree,
                            Entry {
                                path,
                                kind: EntryKind::Fifo,
                                mode: disk_mode(&meta),
                                uid: 0,
                                gid: 0,
                                size: 0,
                                blake3: None,
                                target: None,
                                device: None,
                            },
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

fn hash_file(path: &Path) -> Result<String> {
    let mut f = fs::File::open(path).map_err(|e| io_err(path, e))?;
    let mut hasher = blake3::Hasher::new();
    io::copy(&mut f, &mut hasher).map_err(|e| io_err(path, e))?;
    Ok(hasher.finalize().to_hex().to_string())
}
