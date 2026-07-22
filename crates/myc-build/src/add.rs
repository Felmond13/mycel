//! `[[build.add]]` application: hash and store source files, fold entries
//! into the build tree.

use crate::spec::{normalize_dest, AddSpec};
use crate::tree::{ensure_parents, insert, FileTree};
use crate::{BuildError, Result};
use myc_manifest::{Entry, EntryKind};
use myc_store::Store;
use std::fs;
use std::io;
use std::path::Path;

#[derive(Debug, Default)]
pub struct AddStats {
    pub files: usize,
    pub bytes_logical: u64,
    /// Bytes actually written to the store (misses); the rest was deduped.
    pub bytes_added: u64,
    pub blobs_added: u64,
}

fn io_err(path: &Path, source: io::Error) -> BuildError {
    BuildError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Hash a file, then store it only when the store does not already have it.
/// Returns (hash, size, newly_added).
fn store_file(store: &Store, path: &Path, mode: u32) -> Result<(String, u64, bool)> {
    let mut f = fs::File::open(path).map_err(|e| io_err(path, e))?;
    let mut hasher = blake3::Hasher::new();
    let size = io::copy(&mut f, &mut hasher).map_err(|e| io_err(path, e))?;
    let hash = hasher.finalize().to_hex().to_string();
    if store.has_blob(&hash) {
        return Ok((hash, size, false));
    }
    let mut f = fs::File::open(path).map_err(|e| io_err(path, e))?;
    let (stored, stored_size) = store.put_blob_with_mode(&mut f, mode)?;
    debug_assert_eq!(stored, hash);
    Ok((stored, stored_size, true))
}

#[cfg(unix)]
fn source_mode(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn source_mode(_meta: &fs::Metadata) -> u32 {
    0o644
}

/// Apply one `[[build.add]]` to the tree, storing file contents.
pub fn apply_add(
    store: &Store,
    tree: &mut FileTree,
    add: &AddSpec,
    context_dir: &Path,
) -> Result<AddStats> {
    let source = context_dir.join(&add.source);
    let meta =
        fs::symlink_metadata(&source).map_err(|_| BuildError::MissingSource(source.clone()))?;
    let dest = normalize_dest(&add.dest).map_err(BuildError::Spec)?;

    let mut stats = AddStats::default();
    if meta.is_dir() {
        add_dir_recursive(store, tree, add, &source, &dest, &mut stats)?;
    } else if meta.is_file() {
        if dest == "/" {
            return Err(BuildError::Spec(format!(
                "cannot add file '{}' as '/'",
                add.source
            )));
        }
        add_file(store, tree, add, &source, &dest, &meta, &mut stats)?;
    } else {
        return Err(BuildError::Spec(format!(
            "source '{}' is neither a file nor a directory",
            add.source
        )));
    }
    Ok(stats)
}

fn add_file(
    store: &Store,
    tree: &mut FileTree,
    add: &AddSpec,
    source: &Path,
    dest: &str,
    meta: &fs::Metadata,
    stats: &mut AddStats,
) -> Result<()> {
    let mode = add.mode.unwrap_or_else(|| source_mode(meta));
    let (hash, size, new) = store_file(store, source, mode)?;
    stats.files += 1;
    stats.bytes_logical += size;
    if new {
        stats.bytes_added += size;
        stats.blobs_added += 1;
    }
    ensure_parents(tree, dest);
    insert(
        tree,
        Entry {
            path: dest.to_string(),
            kind: EntryKind::File,
            mode,
            uid: add.uid,
            gid: add.gid,
            size,
            blake3: Some(hash),
            target: None,
            device: None,
        },
    );
    Ok(())
}

fn add_dir_recursive(
    store: &Store,
    tree: &mut FileTree,
    add: &AddSpec,
    source: &Path,
    dest: &str,
    stats: &mut AddStats,
) -> Result<()> {
    if dest != "/" {
        ensure_parents(tree, dest);
        insert(
            tree,
            Entry {
                path: dest.to_string(),
                kind: EntryKind::Dir,
                mode: 0o755,
                uid: add.uid,
                gid: add.gid,
                size: 0,
                blake3: None,
                target: None,
                device: None,
            },
        );
    }

    // Sort children by name: deterministic manifests regardless of readdir order.
    let mut children: Vec<_> = fs::read_dir(source)
        .map_err(|e| io_err(source, e))?
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| io_err(source, e))?;
    children.sort_by_key(|c| c.file_name());

    for child in children {
        let name = child.file_name().to_string_lossy().to_string();
        let child_dest = if dest == "/" {
            format!("/{name}")
        } else {
            format!("{dest}/{name}")
        };
        let child_path = child.path();
        let meta = fs::symlink_metadata(&child_path).map_err(|e| io_err(&child_path, e))?;
        if meta.is_dir() {
            add_dir_recursive(store, tree, add, &child_path, &child_dest, stats)?;
        } else if meta.is_file() {
            add_file(store, tree, add, &child_path, &child_dest, &meta, stats)?;
        } else if meta.is_symlink() {
            let target = fs::read_link(&child_path)
                .map_err(|e| io_err(&child_path, e))?
                .to_string_lossy()
                .to_string();
            insert(
                tree,
                Entry {
                    path: child_dest,
                    kind: EntryKind::Symlink,
                    mode: 0o777,
                    uid: add.uid,
                    gid: add.gid,
                    size: 0,
                    blake3: None,
                    target: Some(target),
                    device: None,
                },
            );
        }
        // Sockets, fifos, devices in a build context: ignored.
    }
    Ok(())
}
