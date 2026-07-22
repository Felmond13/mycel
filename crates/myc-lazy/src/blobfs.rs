//! BlobFs: a flat, read-only FUSE filesystem exposing missing blobs by hash.
//!
//! The lazy rootfs materializes a missing file `/usr/bin/x` as a symlink to
//! `/.myc-lazy/<blake3>`. This filesystem serves that directory: `lookup`
//! and `getattr` answer from the manifest alone (no network — `ls -l` stays
//! instant), and only `open` faults the blob in from the hub, writing it
//! into the local store so every later access (and every other environment
//! sharing the file) is a local hit forever.
//!
//! Writes: none. The mount is read-only, matching hardlink materialization
//! semantics — image file *contents* are immutable; new files and
//! directories are created in the real rootfs directories around it.

use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry,
    ReplyOpen, Request,
};
use myc_hub::HubClient;
use myc_store::Store;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// Attribute cache TTL: content-addressed files never change.
const TTL: Duration = Duration::from_secs(3600);
const ROOT_INO: u64 = 1;
/// Inodes 1..FIRST_BLOB_INO are reserved (root dir).
const FIRST_BLOB_INO: u64 = 2;

/// One blob the filesystem can serve.
#[derive(Debug, Clone)]
pub struct LazyBlob {
    pub hash: String,
    pub size: u64,
    /// Permission bits from the manifest entry (write bits ignored).
    pub mode: u32,
}

/// Shared fetch bookkeeping, readable from outside the FUSE thread.
#[derive(Debug, Default)]
pub struct LazyCounters {
    /// Blobs fetched from the hub by this mount.
    pub fetched_blobs: AtomicU64,
    /// Bytes fetched from the hub by this mount.
    pub fetched_bytes: AtomicU64,
    /// Opens served straight from the local store (already cached).
    pub local_hits: AtomicU64,
    /// Failed fetches (network/hub errors surfaced to the app as EIO).
    pub errors: AtomicU64,
}

pub struct BlobFs {
    store: Store,
    hub: HubClient,
    /// ino -> blob (dense, starting at FIRST_BLOB_INO).
    blobs: Vec<LazyBlob>,
    /// hash -> ino.
    by_hash: HashMap<String, u64>,
    /// Open file handles onto store blob files.
    handles: HashMap<u64, File>,
    next_fh: u64,
    counters: Arc<LazyCounters>,
    uid: u32,
    gid: u32,
}

impl BlobFs {
    pub fn new(
        store: Store,
        hub: HubClient,
        blobs: Vec<LazyBlob>,
        counters: Arc<LazyCounters>,
    ) -> Self {
        let by_hash = blobs
            .iter()
            .enumerate()
            .map(|(i, b)| (b.hash.clone(), FIRST_BLOB_INO + i as u64))
            .collect();
        BlobFs {
            store,
            hub,
            blobs,
            by_hash,
            handles: HashMap::new(),
            next_fh: 1,
            counters,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        }
    }

    /// Mount options for the lazy blob mount.
    pub fn mount_options() -> Vec<MountOption> {
        vec![
            MountOption::RO,
            MountOption::FSName("mycel-lazy".into()),
            MountOption::NoAtime,
        ]
    }

    fn blob(&self, ino: u64) -> Option<&LazyBlob> {
        self.blobs.get((ino.checked_sub(FIRST_BLOB_INO)?) as usize)
    }

    fn attr_for(&self, ino: u64, blob: &LazyBlob) -> FileAttr {
        // Read-only view of the manifest mode; exec bits preserved so
        // binaries fetched on demand stay executable.
        let perm = ((blob.mode & 0o7777) & !0o222) | 0o444;
        FileAttr {
            ino,
            size: blob.size,
            blocks: blob.size.div_ceil(512),
            atime: SystemTime::UNIX_EPOCH,
            mtime: SystemTime::UNIX_EPOCH,
            ctime: SystemTime::UNIX_EPOCH,
            crtime: SystemTime::UNIX_EPOCH,
            kind: FileType::RegularFile,
            perm: perm as u16,
            nlink: 1,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 128 * 1024,
            flags: 0,
        }
    }

    fn root_attr(&self) -> FileAttr {
        FileAttr {
            ino: ROOT_INO,
            size: 0,
            blocks: 0,
            atime: SystemTime::UNIX_EPOCH,
            mtime: SystemTime::UNIX_EPOCH,
            ctime: SystemTime::UNIX_EPOCH,
            crtime: SystemTime::UNIX_EPOCH,
            kind: FileType::Directory,
            perm: 0o555,
            nlink: 2,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    /// Ensure the blob is in the local store, fetching from the hub on a
    /// cold miss, and open it.
    fn open_blob(&mut self, blob: &LazyBlob) -> std::io::Result<File> {
        if self.store.has_blob(&blob.hash) {
            self.counters.local_hits.fetch_add(1, Ordering::Relaxed);
        } else {
            match self.hub.fetch_blob(&self.store, &blob.hash, blob.mode) {
                Ok(bytes) => {
                    self.counters.fetched_blobs.fetch_add(1, Ordering::Relaxed);
                    self.counters
                        .fetched_bytes
                        .fetch_add(bytes, Ordering::Relaxed);
                }
                Err(e) => {
                    self.counters.errors.fetch_add(1, Ordering::Relaxed);
                    eprintln!("myc: lazy fetch of {} failed: {e}", &blob.hash[..12]);
                    return Err(std::io::Error::other(e.to_string()));
                }
            }
        }
        let path = self
            .store
            .blob(&blob.hash)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        File::open(path)
    }
}

impl Filesystem for BlobFs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        if parent != ROOT_INO {
            return reply.error(libc::ENOENT);
        }
        let Some(hash) = name.to_str() else {
            return reply.error(libc::ENOENT);
        };
        match self.by_hash.get(hash) {
            Some(&ino) => {
                let blob = self.blob(ino).expect("by_hash maps into blobs").clone();
                reply.entry(&TTL, &self.attr_for(ino, &blob), 0)
            }
            None => reply.error(libc::ENOENT),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        if ino == ROOT_INO {
            return reply.attr(&TTL, &self.root_attr());
        }
        match self.blob(ino).cloned() {
            Some(blob) => reply.attr(&TTL, &self.attr_for(ino, &blob)),
            None => reply.error(libc::ENOENT),
        }
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        if flags & libc::O_ACCMODE != libc::O_RDONLY {
            return reply.error(libc::EROFS);
        }
        let Some(blob) = self.blob(ino).cloned() else {
            return reply.error(libc::ENOENT);
        };
        match self.open_blob(&blob) {
            Ok(file) => {
                let fh = self.next_fh;
                self.next_fh += 1;
                self.handles.insert(fh, file);
                reply.opened(fh, fuser::consts::FOPEN_KEEP_CACHE);
            }
            Err(_) => reply.error(libc::EIO),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn read(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        use std::os::unix::fs::FileExt;
        let Some(file) = self.handles.get(&fh) else {
            return reply.error(libc::EBADF);
        };
        let mut buf = vec![0u8; size as usize];
        let mut read = 0usize;
        // read_at may return short; loop to fill or EOF.
        while read < buf.len() {
            match file.read_at(&mut buf[read..], offset as u64 + read as u64) {
                Ok(0) => break,
                Ok(n) => read += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return reply.error(libc::EIO),
            }
        }
        reply.data(&buf[..read]);
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: fuser::ReplyEmpty,
    ) {
        self.handles.remove(&fh);
        reply.ok();
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        if ino != ROOT_INO {
            return reply.error(libc::ENOTDIR);
        }
        let entries = std::iter::once((ROOT_INO, FileType::Directory, ".".to_string()))
            .chain(std::iter::once((
                ROOT_INO,
                FileType::Directory,
                "..".to_string(),
            )))
            .chain(self.blobs.iter().enumerate().map(|(i, b)| {
                (
                    FIRST_BLOB_INO + i as u64,
                    FileType::RegularFile,
                    b.hash.clone(),
                )
            }));
        for (i, (ino, kind, name)) in entries.enumerate().skip(offset as usize) {
            if reply.add(ino, (i + 1) as i64, kind, name) {
                break;
            }
        }
        reply.ok();
    }
}
