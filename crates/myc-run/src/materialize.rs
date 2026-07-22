//! Build a rootfs directory from a manifest by hardlinking blobs out of the
//! content-addressed store.
//!
//! Hardlinks mean zero copied bytes and shared page cache across all
//! environments using the same files. When a hardlink is impossible (store
//! and rootfs on different filesystems) we fall back to copying.

use crate::{Result, RunError};
use myc_manifest::{EntryKind, Manifest};
use myc_store::Store;
use std::fs;
use std::io;
use std::path::Path;

#[derive(Debug)]
pub struct MaterializeStats {
    pub files: usize,
    pub dirs: usize,
    pub symlinks: usize,
    pub hardlinked: usize,
    pub copied: usize,
}

fn io_err(path: &Path, source: io::Error) -> RunError {
    RunError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Materialize `manifest` into `rootfs` (created if missing, must be empty).
pub fn materialize(store: &Store, manifest: &Manifest, rootfs: &Path) -> Result<MaterializeStats> {
    let missing = store.missing_blobs(manifest);
    if !missing.is_empty() {
        return Err(RunError::IncompleteStore {
            count: missing.len(),
        });
    }
    fs::create_dir_all(rootfs).map_err(|e| io_err(rootfs, e))?;

    let mut stats = MaterializeStats {
        files: 0,
        dirs: 0,
        symlinks: 0,
        hardlinked: 0,
        copied: 0,
    };

    for entry in &manifest.entries {
        // Entry paths are normalized ('/x/y', no '..'), enforced at ingest
        // and by Manifest::validate.
        let dest = rootfs.join(entry.path.trim_start_matches('/'));
        if let Some(parent) = dest.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
            }
        }
        match entry.kind {
            EntryKind::Dir => {
                if !dest.exists() {
                    fs::create_dir_all(&dest).map_err(|e| io_err(&dest, e))?;
                }
                set_mode(&dest, entry.mode)?;
                stats.dirs += 1;
            }
            EntryKind::File => {
                let blob = store.blob(entry.blake3.as_deref().unwrap())?;
                match fs::hard_link(&blob, &dest) {
                    Ok(()) => {
                        stats.hardlinked += 1;
                        // The store inode carries read-only bits. If the
                        // manifest wants different (e.g. setuid) bits we must
                        // copy instead: chmod would affect every link.
                        if mode_differs_beyond_write(&dest, entry.mode)? {
                            fs::remove_file(&dest).map_err(|e| io_err(&dest, e))?;
                            fs::copy(&blob, &dest).map_err(|e| io_err(&dest, e))?;
                            set_mode(&dest, entry.mode)?;
                            stats.hardlinked -= 1;
                            stats.copied += 1;
                        }
                    }
                    Err(_) => {
                        fs::copy(&blob, &dest).map_err(|e| io_err(&dest, e))?;
                        set_mode(&dest, entry.mode)?;
                        stats.copied += 1;
                    }
                }
                stats.files += 1;
            }
            EntryKind::Symlink => {
                #[cfg(unix)]
                {
                    let target = entry.target.as_deref().unwrap();
                    let _ = fs::remove_file(&dest);
                    std::os::unix::fs::symlink(target, &dest).map_err(|e| io_err(&dest, e))?;
                    stats.symlinks += 1;
                }
                #[cfg(not(unix))]
                {
                    return Err(RunError::Setup(format!(
                        "cannot create symlink {} on this platform \
                         (on Windows, run without --no-proxy so it executes inside WSL2)",
                        dest.display()
                    )));
                }
            }
            // FIFOs and devices: skipped at materialization. Devices are
            // provided by the runtime (/dev tmpfs); FIFOs in images are rare
            // and recreated by the apps that need them.
            EntryKind::Fifo | EntryKind::CharDevice | EntryKind::BlockDevice => {}
        }
    }
    Ok(stats)
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|e| io_err(path, e))?;
    }
    let _ = (path, mode);
    Ok(())
}

/// True when the desired mode differs from the link's mode in more than the
/// write bits (which the store strips on purpose).
fn mode_differs_beyond_write(path: &Path, want: u32) -> Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let have = fs::metadata(path)
            .map_err(|e| io_err(path, e))?
            .permissions()
            .mode()
            & 0o7777;
        let want = want & 0o7777;
        Ok((have | 0o222) != (want | 0o222))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, want);
        Ok(false)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use myc_manifest::{Entry, RuntimeConfig, SCHEMA_VERSION};

    #[test]
    fn materialize_hardlinks_and_dedups() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("store")).unwrap();
        let (hash, size) = store.put_blob(&mut &b"#!/bin/sh\necho hi\n"[..]).unwrap();

        let manifest = Manifest {
            schema: SCHEMA_VERSION,
            name: "t:1".into(),
            origin: None,
            os: "linux".into(),
            arch: "amd64".into(),
            created: "2026-01-01T00:00:00Z".into(),
            config: RuntimeConfig::default(),
            entries: vec![
                Entry {
                    path: "/bin".into(),
                    kind: EntryKind::Dir,
                    mode: 0o755,
                    uid: 0,
                    gid: 0,
                    size: 0,
                    blake3: None,
                    target: None,
                    device: None,
                },
                Entry {
                    path: "/bin/a".into(),
                    kind: EntryKind::File,
                    mode: 0o444,
                    uid: 0,
                    gid: 0,
                    size,
                    blake3: Some(hash.clone()),
                    target: None,
                    device: None,
                },
                Entry {
                    path: "/bin/b".into(),
                    kind: EntryKind::File,
                    mode: 0o444,
                    uid: 0,
                    gid: 0,
                    size,
                    blake3: Some(hash.clone()),
                    target: None,
                    device: None,
                },
                Entry {
                    path: "/bin/l".into(),
                    kind: EntryKind::Symlink,
                    mode: 0o777,
                    uid: 0,
                    gid: 0,
                    size: 0,
                    blake3: None,
                    target: Some("a".into()),
                    device: None,
                },
            ],
        };
        store.put_manifest(&manifest).unwrap();

        let rootfs = dir.path().join("rootfs");
        let stats = materialize(&store, &manifest, &rootfs).unwrap();
        assert_eq!(stats.files, 2);
        assert_eq!(stats.symlinks, 1);
        assert_eq!(stats.hardlinked, 2);
        assert_eq!(stats.copied, 0);
        assert_eq!(
            fs::read(rootfs.join("bin/a")).unwrap(),
            b"#!/bin/sh\necho hi\n"
        );
        assert_eq!(
            fs::read_link(rootfs.join("bin/l"))
                .unwrap()
                .to_str()
                .unwrap(),
            "a"
        );

        // Same inode: true dedup on disk.
        use std::os::unix::fs::MetadataExt;
        let ia = fs::metadata(rootfs.join("bin/a")).unwrap().ino();
        let ib = fs::metadata(rootfs.join("bin/b")).unwrap().ino();
        assert_eq!(ia, ib);
    }

    #[test]
    fn refuses_incomplete_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("store")).unwrap();
        let manifest = Manifest {
            schema: SCHEMA_VERSION,
            name: "t:1".into(),
            origin: None,
            os: "linux".into(),
            arch: "amd64".into(),
            created: "2026-01-01T00:00:00Z".into(),
            config: RuntimeConfig::default(),
            entries: vec![Entry {
                path: "/missing".into(),
                kind: EntryKind::File,
                mode: 0o644,
                uid: 0,
                gid: 0,
                size: 1,
                blake3: Some("0".repeat(64)),
                target: None,
                device: None,
            }],
        };
        let err = materialize(&store, &manifest, &dir.path().join("rootfs")).unwrap_err();
        assert!(matches!(err, RunError::IncompleteStore { count: 1 }));
    }
}
