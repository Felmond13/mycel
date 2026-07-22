//! Lazy streaming rootfs: start an environment before its blobs are local.
//!
//! Instead of pulling every blob up front, `myc run --lazy`:
//!
//! 1. fetches only the manifest (KBs) from the hub,
//! 2. materializes everything cheap immediately — the directory tree,
//!    symlinks, and every file whose blob is already in the local store
//!    (hardlinked, exactly like a normal run),
//! 3. represents each *missing* file as a symlink into `/.myc-lazy`, a
//!    read-only FUSE mount ([`blobfs::BlobFs`]) that on first `open` fetches
//!    the blob from the hub, stores it in the local content-addressed store
//!    (cached forever, shared with every other environment), and serves it.
//!
//! `stat`/`ls -l` never touch the network: attributes come straight from
//! the manifest. Only actually-read file contents cross the wire.
//!
//! Writes: image content stays immutable (the FUSE mount is read-only —
//! the same contract as hardlink materialization, where store inodes are
//! write-protected). New files, directories and deletions happen in the
//! real rootfs directories exactly as in a normal run.
//!
//! The mount is created *outside* the container namespaces by the regular
//! user; the container's recursive bind + pivot_root carries it inside.

pub mod blobfs;

use blobfs::{BlobFs, LazyBlob, LazyCounters};
use myc_hub::HubClient;
use myc_manifest::{EntryKind, Manifest};
use myc_store::Store;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Directory (inside the rootfs) where the blob FUSE mount lives.
pub const LAZY_DIR: &str = ".myc-lazy";

#[derive(Debug, thiserror::Error)]
pub enum LazyError {
    #[error(transparent)]
    Store(#[from] myc_store::StoreError),
    #[error("i/o error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot mount the lazy filesystem at {path}: {source} — is /dev/fuse available? (`myc doctor`)")]
    Mount {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Hub(#[from] myc_hub::client::HubError),
}

pub type Result<T> = std::result::Result<T, LazyError>;

fn io_err(path: &Path, source: std::io::Error) -> LazyError {
    LazyError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// What `prepare` did up front, before the container started.
#[derive(Debug, Clone, Copy)]
pub struct LazyPrepareStats {
    /// Files hardlinked from the local store (blob already present).
    pub local_files: usize,
    /// Files deferred to on-demand fetch.
    pub deferred_files: usize,
    /// Bytes deferred (logical size of the missing files).
    pub deferred_bytes: u64,
    pub dirs: usize,
    pub symlinks: usize,
}

/// Post-run fetch accounting.
#[derive(Debug, Clone, Copy)]
pub struct LazyRunStats {
    /// Blobs fetched from the hub during the run.
    pub fetched_blobs: u64,
    /// Bytes fetched from the hub during the run.
    pub fetched_bytes: u64,
    /// Opens of deferred files served from the local store
    /// (fetched earlier in the run, or landed there since prepare).
    pub local_hits: u64,
    /// Fetch failures surfaced to the app as EIO.
    pub errors: u64,
}

/// A prepared lazy rootfs: materialized skeleton + live FUSE mount.
/// Dropping it unmounts; call [`LazyRootfs::cleanup`] to also remove files.
pub struct LazyRootfs {
    rootfs: PathBuf,
    session: Option<fuser::BackgroundSession>,
    counters: Arc<LazyCounters>,
    pub prepare_stats: LazyPrepareStats,
}

impl LazyRootfs {
    /// Build `rootfs` from `manifest`, hardlinking every blob already in
    /// `store` and deferring the rest to a FUSE mount backed by `hub_url`.
    ///
    /// `store_root` is reopened for the FUSE thread (the store type is not
    /// shareable across threads by design — each holds its own flock).
    pub fn prepare(
        store: &Store,
        store_root: &Path,
        manifest: &Manifest,
        hub_url: &str,
        hub_token: Option<String>,
        rootfs: &Path,
    ) -> Result<LazyRootfs> {
        fs::create_dir_all(rootfs).map_err(|e| io_err(rootfs, e))?;

        let mut stats = LazyPrepareStats {
            local_files: 0,
            deferred_files: 0,
            deferred_bytes: 0,
            dirs: 0,
            symlinks: 0,
        };
        let mut deferred: Vec<LazyBlob> = Vec::new();
        let mut seen_hashes = std::collections::HashSet::new();

        for entry in &manifest.entries {
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
                EntryKind::Symlink => {
                    let target = entry.target.as_deref().unwrap_or("/");
                    let _ = fs::remove_file(&dest);
                    std::os::unix::fs::symlink(target, &dest).map_err(|e| io_err(&dest, e))?;
                    stats.symlinks += 1;
                }
                EntryKind::File => {
                    let hash = entry.blake3.as_deref().expect("validated manifest");
                    if store.has_blob(hash) {
                        materialize_file(store, hash, entry.mode, &dest)?;
                        stats.local_files += 1;
                    } else {
                        // Symlink into the FUSE mount; content follows on
                        // first open. Absolute target: resolved after
                        // pivot_root, when / is the rootfs.
                        let _ = fs::remove_file(&dest);
                        std::os::unix::fs::symlink(format!("/{LAZY_DIR}/{hash}"), &dest)
                            .map_err(|e| io_err(&dest, e))?;
                        stats.deferred_files += 1;
                        stats.deferred_bytes += entry.size;
                        if seen_hashes.insert(hash.to_string()) {
                            deferred.push(LazyBlob {
                                hash: hash.to_string(),
                                size: entry.size,
                                mode: entry.mode,
                            });
                        }
                    }
                }
                EntryKind::Fifo | EntryKind::CharDevice | EntryKind::BlockDevice => {}
            }
        }

        let counters = Arc::new(LazyCounters::default());
        let mount_point = rootfs.join(LAZY_DIR);
        fs::create_dir_all(&mount_point).map_err(|e| io_err(&mount_point, e))?;

        // Mount even when nothing is deferred: the run path stays uniform.
        let fs_store = Store::open(store_root)?;
        let hub = HubClient::new(hub_url, hub_token)?;
        let blobfs = BlobFs::new(fs_store, hub, deferred, Arc::clone(&counters));
        let session = fuser::spawn_mount2(blobfs, &mount_point, &BlobFs::mount_options()).map_err(
            |source| LazyError::Mount {
                path: mount_point.clone(),
                source,
            },
        )?;

        Ok(LazyRootfs {
            rootfs: rootfs.to_path_buf(),
            session: Some(session),
            counters,
            prepare_stats: stats,
        })
    }

    pub fn rootfs(&self) -> &Path {
        &self.rootfs
    }

    pub fn run_stats(&self) -> LazyRunStats {
        LazyRunStats {
            fetched_blobs: self.counters.fetched_blobs.load(Ordering::Relaxed),
            fetched_bytes: self.counters.fetched_bytes.load(Ordering::Relaxed),
            local_hits: self.counters.local_hits.load(Ordering::Relaxed),
            errors: self.counters.errors.load(Ordering::Relaxed),
        }
    }

    /// Unmount the FUSE session (blocks until the kernel releases it).
    pub fn unmount(&mut self) {
        if let Some(session) = self.session.take() {
            session.join();
        }
    }

    /// Unmount and delete the rootfs.
    pub fn cleanup(mut self) -> Result<()> {
        self.unmount();
        make_tree_removable(&self.rootfs);
        fs::remove_dir_all(&self.rootfs).map_err(|e| io_err(&self.rootfs, e))
    }
}

impl Drop for LazyRootfs {
    fn drop(&mut self) {
        self.unmount();
    }
}

/// Hardlink a locally-present blob into place (copy when the mode needs to
/// differ beyond write bits, mirroring myc-run's materializer).
fn materialize_file(store: &Store, hash: &str, mode: u32, dest: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let blob = store.blob(hash)?;
    match fs::hard_link(&blob, dest) {
        Ok(()) => {
            let have = fs::metadata(dest)
                .map_err(|e| io_err(dest, e))?
                .permissions()
                .mode()
                & 0o7777;
            if (have | 0o222) != ((mode & 0o7777) | 0o222) {
                fs::remove_file(dest).map_err(|e| io_err(dest, e))?;
                fs::copy(&blob, dest).map_err(|e| io_err(dest, e))?;
                set_mode(dest, mode)?;
            }
        }
        Err(_) => {
            fs::copy(&blob, dest).map_err(|e| io_err(dest, e))?;
            set_mode(dest, mode)?;
        }
    }
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|e| io_err(path, e))
}

fn make_tree_removable(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() && !p.is_symlink() {
                make_tree_removable(&p);
            }
        }
    }
}
