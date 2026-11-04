//! The Mycel content-addressed store.
//!
//! Layout under the store root (default `~/.mycel`):
//!
//! ```text
//! .mycel/
//!   objects/ab/abcdef....        content blobs, named by BLAKE3 hex hash
//!   manifests/myc1-....json      manifests, named by manifest id
//!   refs/<escaped name>          name -> manifest id
//!   pins/<manifest id>           GC roots
//!   packs/pack-<hash>.zst        v2 chunk packs (opt-in, see `chunks`)
//!   chunks/<blob-hash>.chunks    v2 chunk sidecars (opt-in)
//!   chunk-mode                   v2 disk mode: transfer | lean | packs-only
//!   blob-cache/<hash>            packs-only: markers of reassembled blobs
//!   lock                         inter-process lock file
//! ```
//!
//! Blobs are written atomically (temp file + rename) and stored read-only.
//! Every file of every environment lives here exactly once; rootfs
//! materialization hardlinks out of this store.
//!
//! The v2 chunk layer (`chunks` module, enabled by `MYCEL_CHUNKED=1`) is
//! purely additive: blob files, manifests and their ids are untouched.
//! Its disk cost is arbitrated per store by `MYCEL_CHUNK_MODE`
//! ([`ChunkMode`]): `transfer` keeps v1 blobs *and* packs (chunked
//! transfers, hardlinks, +~50 % disk), `lean` records chunk ids without
//! packs (negotiation-ready, ~0 % overhead) and `packs-only` stores
//! chunked files as packs only, reassembling the v1 blob on demand into a
//! LRU-bounded cache (`blob_cache` module, `MYCEL_BLOB_CACHE_MB`). See
//! `docs/design/store-v2.md` for the full design and measurements.

pub mod archive;
pub mod blob_cache;
pub mod chunks;
pub mod volumes;

pub use chunks::{ChunkId, ChunkMode, ChunkStats};

use fs2::FileExt;
use myc_manifest::Manifest;
use std::collections::HashSet;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("i/o error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("blob {0} not found in store")]
    BlobMissing(String),
    #[error("manifest {0} not found in store")]
    ManifestMissing(String),
    #[error("hash mismatch: expected {expected}, got {actual}")]
    HashMismatch { expected: String, actual: String },
    #[error("invalid volume name '{0}' (use 1-64 lowercase letters, digits, . _ -, starting with a letter or digit)")]
    InvalidVolumeName(String),
    #[error("chunk layer: {0}")]
    Chunk(String),
    #[error(
        "the store is busy (another process is writing to it right now) — try again in a moment"
    )]
    Busy,
    #[error("volume '{0}' not found (myc volume ls lists them)")]
    VolumeMissing(String),
    #[error(transparent)]
    Manifest(#[from] myc_manifest::ManifestError),
}

type Result<T> = std::result::Result<T, StoreError>;

pub(crate) fn io_err(path: &Path, source: io::Error) -> StoreError {
    StoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Keep permission bits but strip all write bits (blobs are immutable).
#[cfg_attr(not(unix), allow(dead_code))]
fn readonly_mode(mode: u32) -> u32 {
    let m = mode & 0o7777;
    let m = if m == 0 { 0o444 } else { m };
    m & !0o222
}

pub struct Store {
    root: PathBuf,
    _lock: fs::File,
    chunked: bool,
    /// None iff chunking is off; the store's settled disk mode otherwise.
    chunk_mode: Option<ChunkMode>,
}

pub struct BlobStats {
    pub count: u64,
    pub bytes: u64,
}

impl Store {
    /// Open (creating if needed) the store at `root` and take a shared
    /// inter-process lock. GC upgrades to an exclusive lock. The v2 chunk
    /// layer is active iff `MYCEL_CHUNKED=1`, with the disk mode from
    /// `MYCEL_CHUNK_MODE` (default `transfer`).
    ///
    /// The mode is recorded in `chunk-mode` at first v2 use and is then
    /// part of the store's identity. Opening with a *different* mode is
    /// refused while any blob exists only as packs (packs-only data whose
    /// v1 file was never reassembled) — switching would make that data
    /// invisible. Materialize those environments once (any `myc run`
    /// repopulates the blob cache) or re-ingest them, then switch.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let chunked = Self::chunking_enabled_from_env();
        let mode = if chunked {
            Some(ChunkMode::from_env()?)
        } else {
            None
        };
        Self::open_inner(root, chunked, mode)
    }

    /// Like [`Store::open`] but with the v2 chunk layer forced on,
    /// regardless of the environment. Behavior is otherwise identical
    /// (the disk mode still comes from `MYCEL_CHUNK_MODE`).
    pub fn open_chunked(root: impl Into<PathBuf>) -> Result<Self> {
        Self::open_inner(root, true, Some(ChunkMode::from_env()?))
    }

    /// Like [`Store::open_chunked`] with an explicit disk mode — the
    /// hermetic variant used by tests and tools that already know the
    /// mode they want.
    pub fn open_chunked_with_mode(root: impl Into<PathBuf>, mode: ChunkMode) -> Result<Self> {
        Self::open_inner(root, true, Some(mode))
    }

    fn open_inner(
        root: impl Into<PathBuf>,
        chunked: bool,
        mode: Option<ChunkMode>,
    ) -> Result<Self> {
        let root = root.into();
        for sub in ["objects", "manifests", "refs", "pins", "tmp"] {
            let p = root.join(sub);
            fs::create_dir_all(&p).map_err(|e| io_err(&p, e))?;
        }
        if chunked {
            debug_assert!(mode.is_some());
            // Lean never writes packs; the other modes do.
            if mode != Some(ChunkMode::Lean) {
                let p = root.join("packs");
                fs::create_dir_all(&p).map_err(|e| io_err(&p, e))?;
            }
            let p = root.join("chunks");
            fs::create_dir_all(&p).map_err(|e| io_err(&p, e))?;
        }
        let mode = match mode {
            Some(m) if chunked => Some(Self::settle_chunk_mode(&root, m)?),
            _ => None,
        };
        let lock_path = root.join("lock");
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|e| io_err(&lock_path, e))?;
        lock.lock_shared().map_err(|e| io_err(&lock_path, e))?;
        Ok(Store {
            root,
            _lock: lock,
            chunked,
            chunk_mode: mode,
        })
    }

    /// Make the requested mode coherent with what the store already
    /// holds: first v2 use records it; a later *change* is accepted only
    /// when no blob exists solely as packs.
    fn settle_chunk_mode(root: &Path, requested: ChunkMode) -> Result<ChunkMode> {
        match chunks::read_chunk_mode(root)? {
            None => {
                chunks::write_chunk_mode(root, requested)?;
                Ok(requested)
            }
            Some(current) if current == requested => Ok(current),
            Some(current) => {
                let orphans = Self::blobs_only_in_packs(root)?;
                if !orphans.is_empty() {
                    return Err(StoreError::Chunk(format!(
                        "cannot switch chunk mode from '{}' to '{}': {} blob(s) exist only \
                         as packs, without a v1 blob file (first: {}). They would become \
                         unreadable. Re-materialize them first — run each environment once \
                         (any myc run reassembles and caches its blobs) — or re-ingest, \
                         then switch modes.",
                        current.as_str(),
                        requested.as_str(),
                        orphans.len(),
                        orphans[0]
                    )));
                }
                chunks::overwrite_chunk_mode(root, requested)?;
                Ok(requested)
            }
        }
    }

    /// Blobs that have a chunk sidecar but no v1 blob file — data whose
    /// only backing store is the packs (normal in packs-only until first
    /// use; a hazard for any other mode).
    fn blobs_only_in_packs(root: &Path) -> Result<Vec<String>> {
        let dir = root.join("chunks");
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_err(&dir, e)),
        };
        let mut orphans = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| io_err(&dir, e))?;
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(hash) = name.strip_suffix(".chunks") else {
                continue;
            };
            let shard = &hash[..2.min(hash.len())];
            if !root.join("objects").join(shard).join(hash).exists() {
                orphans.push(hash.to_string());
            }
        }
        orphans.sort();
        Ok(orphans)
    }

    /// `MYCEL_CHUNKED=1` opts this store into the v2 chunk layer.
    fn chunking_enabled_from_env() -> bool {
        std::env::var("MYCEL_CHUNKED").ok().as_deref() == Some("1")
    }

    /// Default store location: `$MYCEL_STORE` or `~/.mycel`.
    ///
    /// "Home" is `$HOME` on Unix; on Windows (where `HOME` is usually
    /// absent) it falls back to `%USERPROFILE%`, giving
    /// `C:\Users\<name>\.mycel`.
    pub fn default_root() -> PathBuf {
        if let Ok(p) = std::env::var("MYCEL_STORE") {
            return PathBuf::from(p);
        }
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| ".".into());
        PathBuf::from(home).join(".mycel")
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn blob_path(&self, hash: &str) -> PathBuf {
        let shard = &hash[..2.min(hash.len())];
        self.root.join("objects").join(shard).join(hash)
    }

    /// Stream `reader` into the store, returning `(blake3 hex, size)`.
    /// If the blob already exists the write is skipped (dedup).
    pub fn put_blob(&self, reader: &mut dyn Read) -> Result<(String, u64)> {
        self.put_blob_with_mode(reader, 0o444)
    }

    /// Like `put_blob`, but stores the blob inode with `mode` (write bits
    /// always stripped). Ingestion passes the file's real mode so that the
    /// common case — first materialization — needs no extra inode.
    pub fn put_blob_with_mode(&self, reader: &mut dyn Read, mode: u32) -> Result<(String, u64)> {
        let _ = mode; // used on unix below
        let tmp_dir = self.root.join("tmp");
        let mut tmp = tempfile::NamedTempFile::new_in(&tmp_dir).map_err(|e| io_err(&tmp_dir, e))?;
        let mut hasher = blake3::Hasher::new();
        let mut buf = [0u8; 128 * 1024];
        let mut size: u64 = 0;
        loop {
            let n = reader.read(&mut buf).map_err(|e| io_err(tmp.path(), e))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            tmp.write_all(&buf[..n])
                .map_err(|e| io_err(tmp.path(), e))?;
            size += n as u64;
        }
        let hash = hasher.finalize().to_hex().to_string();
        let dest = self.blob_path(&hash);
        if dest.exists() {
            // v2: backfill the chunk layer for pre-v2 blobs on re-ingest
            // (no-op as soon as the sidecar exists).
            self.index_chunks_if_enabled(&hash, size, tmp.path())?;
            return Ok((hash, size)); // dedup hit; drop the temp file
        }
        // packs-only: a chunked blob lives only as packs + sidecar. Index
        // from the temp file; if that produced a sidecar, the v1 blob is
        // deliberately not persisted (rebuilt on demand by `blob`).
        // Without a sidecar (chunking off, size out of range, empty cut)
        // we fall through and store the plain v1 blob — data is never at
        // risk.
        if self.skip_blob_write(size) {
            self.index_chunks_if_enabled(&hash, size, tmp.path())?;
            if chunks::sidecar_exists(&self.root, &hash) {
                return Ok((hash, size)); // packs-only: blob file omitted
            }
        }
        let parent = dest.parent().unwrap();
        fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
        tmp.flush().map_err(|e| io_err(tmp.path(), e))?;
        // Strip write bits before publishing: blobs are immutable, and rootfs
        // materialization hardlinks straight into the store.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perm = fs::Permissions::from_mode(readonly_mode(mode));
            fs::set_permissions(tmp.path(), perm).map_err(|e| io_err(tmp.path(), e))?;
        }
        match tmp.persist_noclobber(&dest) {
            Ok(_) => {}
            Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io_err(&dest, e.error)),
        }
        self.index_chunks_if_enabled(&hash, size, &dest)?;
        Ok((hash, size))
    }

    /// packs-only + chunkable size ⇒ the v1 blob is not the backing store.
    fn skip_blob_write(&self, size: u64) -> bool {
        self.chunk_mode == Some(ChunkMode::PacksOnly) && self.chunkable(size)
    }

    fn chunkable(&self, size: u64) -> bool {
        (chunks::CHUNK_MIN_BLOB_SIZE..=chunks::CHUNK_MAX_BLOB_SIZE).contains(&size)
    }

    /// v2 chunk indexing (opt-in): cut blobs in
    /// `[chunks::CHUNK_MIN_BLOB_SIZE, chunks::CHUNK_MAX_BLOB_SIZE]` into
    /// content-defined chunks and record what the mode requires —
    /// missing chunks into a pack + sidecar (transfer, packs-only), or a
    /// lean sidecar with no pack (lean). Idempotent: an existing sidecar
    /// (either format) short-circuits.
    fn index_chunks_if_enabled(&self, blob_hash: &str, size: u64, data_file: &Path) -> Result<()> {
        if !self.chunked || !self.chunkable(size) {
            return Ok(());
        }
        if chunks::sidecar_exists(&self.root, blob_hash) {
            return Ok(()); // already indexed — re-ingest is a no-op
        }
        let data = fs::read(data_file).map_err(|e| io_err(data_file, e))?;
        match self.chunk_mode {
            Some(ChunkMode::Lean) => chunks::lean_index_blob(&self.root, blob_hash, &data),
            _ => chunks::index_blob(&self.root, blob_hash, &data),
        }
        .map(|_| ())
    }

    /// Is the blob present? In packs-only mode (and generally, once a
    /// sidecar exists) a blob backed by packs counts as present even when
    /// its v1 file is not on disk: [`Store::blob`] will rebuild it.
    pub fn has_blob(&self, hash: &str) -> bool {
        self.blob_path(hash).exists() || (self.chunked && chunks::sidecar_exists(&self.root, hash))
    }

    /// Absolute path of a blob, verifying existence. If the v1 file is
    /// missing but the packs hold it (packs-only mode, or a v1 file lost
    /// after a transfer-mode ingest), it is reassembled — BLAKE3-verified —
    /// written into `objects/` like any v1 blob and recorded in the
    /// size-bounded blob cache (see [`crate::blob_cache`]).
    pub fn blob(&self, hash: &str) -> Result<PathBuf> {
        let p = self.blob_path(hash);
        if p.exists() {
            if self.chunk_mode == Some(ChunkMode::PacksOnly) {
                blob_cache::touch(&self.root, hash); // LRU clock
            }
            return Ok(p);
        }
        if self.chunked && chunks::sidecar_exists(&self.root, hash) {
            return self.reassemble_into_cache(hash);
        }
        Err(StoreError::BlobMissing(hash.to_string()))
    }

    /// Rebuild a packs-backed blob into `objects/` and cache it. Deleting
    /// the cached file later never touches its chunks or packs.
    fn reassemble_into_cache(&self, hash: &str) -> Result<PathBuf> {
        let data = chunks::reassemble(&self.root, hash)?; // verifies BLAKE3
        let dest = self.blob_path(hash);
        let parent = dest.parent().unwrap();
        fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
        let tmp_dir = self.root.join("tmp");
        let mut tmp = tempfile::NamedTempFile::new_in(&tmp_dir).map_err(|e| io_err(&tmp_dir, e))?;
        tmp.write_all(&data).map_err(|e| io_err(tmp.path(), e))?;
        tmp.flush().map_err(|e| io_err(tmp.path(), e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perm = fs::Permissions::from_mode(readonly_mode(0o444));
            fs::set_permissions(tmp.path(), perm).map_err(|e| io_err(tmp.path(), e))?;
        }
        match tmp.persist_noclobber(&dest) {
            Ok(_) => {}
            Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io_err(&dest, e.error)),
        }
        blob_cache::record(&self.root, hash, data.len() as u64)?;
        // Best-effort: eviction must never fail the materialization.
        let _ = self.evict_blob_cache();
        Ok(dest)
    }

    /// Evict LRU-first from the blob cache until it fits
    /// `$MYCEL_BLOB_CACHE_MB` (default 512 MiB). Only blobs no stored
    /// manifest references are evictable; their packs stay untouched.
    /// Returns (evicted blobs, bytes freed).
    pub fn evict_blob_cache(&self) -> Result<(u64, u64)> {
        let budget = blob_cache::budget_from_env();
        self.evict_blob_cache_with_budget(budget)
    }

    /// [`Store::evict_blob_cache`] with an explicit byte budget.
    pub fn evict_blob_cache_with_budget(&self, budget_bytes: u64) -> Result<(u64, u64)> {
        if !self.chunked {
            return Ok((0, 0));
        }
        let protected = self.live_blob_hashes()?;
        blob_cache::evict(&self.root, budget_bytes, &protected)
    }

    /// Blob hashes referenced by every stored manifest (the GC live set).
    fn live_blob_hashes(&self) -> Result<HashSet<String>> {
        let mut live = HashSet::new();
        for (_, manifest) in self.list_manifests()? {
            for (hash, _) in manifest.referenced_blobs() {
                live.insert(hash.to_string());
            }
        }
        Ok(live)
    }

    /// (count, bytes) of v1 blobs currently cached after on-demand
    /// reassembly (packs-only mode reporting).
    pub fn blob_cache_stats(&self) -> Result<(u64, u64)> {
        blob_cache::stats(&self.root)
    }

    /// Re-hash a blob and verify integrity.
    pub fn verify_blob(&self, hash: &str) -> Result<()> {
        let p = self.blob(hash)?;
        let mut f = fs::File::open(&p).map_err(|e| io_err(&p, e))?;
        let mut hasher = blake3::Hasher::new();
        io::copy(&mut f, &mut hasher).map_err(|e| io_err(&p, e))?;
        let actual = hasher.finalize().to_hex().to_string();
        if actual != hash {
            return Err(StoreError::HashMismatch {
                expected: hash.to_string(),
                actual,
            });
        }
        Ok(())
    }

    // ---- chunk layer (v2, opt-in) ----

    /// Is the v2 chunk layer active on this handle? (See `chunks`.)
    pub fn chunked(&self) -> bool {
        self.chunked
    }

    /// The store's disk mode (`None` when chunking is off): how the v2
    /// layer trades disk for transfer-readiness. See [`ChunkMode`].
    pub fn chunk_mode(&self) -> Option<ChunkMode> {
        self.chunk_mode
    }

    /// Does the store hold this chunk? A sidecar lists it iff the store
    /// has (or can produce) those bytes, in every disk mode — see
    /// `chunks::known_chunk_ids`. Unavailable (scan failure) reports
    /// `false`: the safe direction for a transfer is to resend.
    pub fn has_chunk(&self, id: &ChunkId) -> bool {
        chunks::has_chunk(&self.root, id)
    }

    /// The subset of `wanted` this store is missing — exactly the chunks a
    /// hub push/pull would have to transfer once the protocol negotiates at
    /// chunk granularity.
    pub fn missing_chunks(&self, wanted: &[ChunkId]) -> Vec<ChunkId> {
        chunks::missing_chunks(&self.root, wanted).unwrap_or_default()
    }

    /// Ordered chunk ids covering one blob (`None` if it has no sidecar:
    /// too small, or ingested before chunking was enabled).
    pub fn blob_chunks(&self, blob_hash: &str) -> Result<Option<Vec<ChunkId>>> {
        chunks::blob_chunks(&self.root, blob_hash)
    }

    /// Rebuild a chunked blob from its packs; the result is verified
    /// against the blob hash (the round-trip guarantee).
    pub fn reassemble_blob(&self, blob_hash: &str) -> Result<Vec<u8>> {
        chunks::reassemble(&self.root, blob_hash)
    }

    /// Physical statistics of the chunk layer (packs, unique chunks,
    /// sidecars) for reporting and measurements.
    pub fn chunk_stats(&self) -> Result<ChunkStats> {
        chunks::chunk_stats(&self.root)
    }

    // ---- manifests ----

    pub fn put_manifest(&self, manifest: &Manifest) -> Result<String> {
        let id = manifest.id()?;
        let path = self.root.join("manifests").join(format!("{id}.json"));
        if !path.exists() {
            let bytes = manifest.to_canonical_json()?;
            let tmp_dir = self.root.join("tmp");
            let mut tmp =
                tempfile::NamedTempFile::new_in(&tmp_dir).map_err(|e| io_err(&tmp_dir, e))?;
            tmp.write_all(&bytes).map_err(|e| io_err(tmp.path(), e))?;
            match tmp.persist_noclobber(&path) {
                Ok(_) => {}
                Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(io_err(&path, e.error)),
            }
        }
        Ok(id)
    }

    pub fn get_manifest(&self, id: &str) -> Result<Manifest> {
        let path = self.root.join("manifests").join(format!("{id}.json"));
        let bytes = fs::read(&path).map_err(|_| StoreError::ManifestMissing(id.to_string()))?;
        Ok(Manifest::from_json(&bytes)?)
    }

    pub fn list_manifests(&self) -> Result<Vec<(String, Manifest)>> {
        let dir = self.root.join("manifests");
        let mut out = Vec::new();
        for entry in fs::read_dir(&dir).map_err(|e| io_err(&dir, e))? {
            let entry = entry.map_err(|e| io_err(&dir, e))?;
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(id) = name.strip_suffix(".json") {
                if let Ok(m) = self.get_manifest(id) {
                    out.push((id.to_string(), m));
                }
            }
        }
        out.sort_by(|a, b| a.1.name.cmp(&b.1.name));
        Ok(out)
    }

    pub fn remove_manifest(&self, id: &str) -> Result<()> {
        let path = self.root.join("manifests").join(format!("{id}.json"));
        if !path.exists() {
            return Err(StoreError::ManifestMissing(id.to_string()));
        }
        fs::remove_file(&path).map_err(|e| io_err(&path, e))?;
        let _ = fs::remove_file(self.root.join("pins").join(id));
        // Drop refs pointing at this manifest.
        for (name, target) in self.list_refs()? {
            if target == id {
                let _ = fs::remove_file(self.ref_path(&name));
            }
        }
        Ok(())
    }

    // ---- refs (name -> manifest id) ----

    fn ref_path(&self, name: &str) -> PathBuf {
        // Escape '/' and ':' so any image reference is a valid file name.
        let escaped = name
            .replace('%', "%25")
            .replace('/', "%2F")
            .replace(':', "%3A");
        self.root.join("refs").join(escaped)
    }

    fn ref_name_from_file(file: &str) -> String {
        file.replace("%3A", ":")
            .replace("%2F", "/")
            .replace("%25", "%")
    }

    pub fn set_ref(&self, name: &str, manifest_id: &str) -> Result<()> {
        let p = self.ref_path(name);
        fs::write(&p, manifest_id).map_err(|e| io_err(&p, e))
    }

    pub fn resolve_ref(&self, name: &str) -> Result<Option<String>> {
        let p = self.ref_path(name);
        match fs::read_to_string(&p) {
            Ok(s) => Ok(Some(s.trim().to_string())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_err(&p, e)),
        }
    }

    pub fn list_refs(&self) -> Result<Vec<(String, String)>> {
        let dir = self.root.join("refs");
        let mut out = Vec::new();
        for entry in fs::read_dir(&dir).map_err(|e| io_err(&dir, e))? {
            let entry = entry.map_err(|e| io_err(&dir, e))?;
            let file = entry.file_name().to_string_lossy().to_string();
            let target = fs::read_to_string(entry.path())
                .map_err(|e| io_err(&entry.path(), e))?
                .trim()
                .to_string();
            out.push((Self::ref_name_from_file(&file), target));
        }
        out.sort();
        Ok(out)
    }

    /// Resolve a user-supplied reference: exact manifest id, ref name, or
    /// unique manifest-id prefix.
    pub fn resolve(&self, reference: &str) -> Result<String> {
        if self
            .root
            .join("manifests")
            .join(format!("{reference}.json"))
            .exists()
        {
            return Ok(reference.to_string());
        }
        if let Some(id) = self.resolve_ref(reference)? {
            return Ok(id);
        }
        // Try prefix match on ids.
        let mut matches = Vec::new();
        for (id, _) in self.list_manifests()? {
            if id.starts_with(reference)
                || id
                    .strip_prefix("myc1-")
                    .is_some_and(|s| s.starts_with(reference))
            {
                matches.push(id);
            }
        }
        match matches.len() {
            1 => Ok(matches.remove(0)),
            0 => Err(StoreError::ManifestMissing(reference.to_string())),
            _ => Err(StoreError::ManifestMissing(format!(
                "ambiguous reference '{reference}' ({} matches)",
                matches.len()
            ))),
        }
    }

    // ---- pins & GC ----

    pub fn pin(&self, manifest_id: &str) -> Result<()> {
        self.get_manifest(manifest_id)?;
        let p = self.root.join("pins").join(manifest_id);
        fs::write(&p, b"").map_err(|e| io_err(&p, e))
    }

    pub fn unpin(&self, manifest_id: &str) -> Result<()> {
        let p = self.root.join("pins").join(manifest_id);
        match fs::remove_file(&p) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_err(&p, e)),
        }
    }

    pub fn pins(&self) -> Result<HashSet<String>> {
        let dir = self.root.join("pins");
        let mut out = HashSet::new();
        for entry in fs::read_dir(&dir).map_err(|e| io_err(&dir, e))? {
            let entry = entry.map_err(|e| io_err(&dir, e))?;
            out.insert(entry.file_name().to_string_lossy().to_string());
        }
        Ok(out)
    }

    /// Release this handle's inter-process lock early.
    ///
    /// A container holds its `Store` open for as long as it runs, but only
    /// needs the store until its rootfs is materialized (hardlinks keep the
    /// data alive regardless of what happens in the store afterwards).
    /// Dropping the lock at that point keeps a long-running container from
    /// blocking `gc` forever. After this call the handle must only be used
    /// for reads that tolerate concurrent GC (blob paths already
    /// hardlinked, volume paths, …).
    pub fn release_lock(&self) -> Result<()> {
        let lock_path = self.root.join("lock");
        fs2::FileExt::unlock(&self._lock).map_err(|e| io_err(&lock_path, e))
    }

    /// Delete every blob not referenced by any stored manifest, then sweep
    /// the v2 chunk layer (sidecars of dead blobs, packs referenced by no
    /// surviving sidecar). Returns (deleted files, bytes freed) — the count
    /// covers blob objects and swept chunk files, the bytes cover both.
    ///
    /// Waits up to ~10 s for exclusive store access, then fails with
    /// [`StoreError::Busy`] instead of blocking forever (an in-flight
    /// ingest or a container still materializing holds the lock shared).
    pub fn gc(&self) -> Result<(u64, u64)> {
        self.gc_with_wait(std::time::Duration::from_secs(10))
    }

    /// [`Store::gc`] with an explicit bound on how long to wait for the
    /// exclusive lock.
    pub fn gc_with_wait(&self, max_wait: std::time::Duration) -> Result<(u64, u64)> {
        // Upgrade to exclusive lock: no concurrent ingest/run during GC.
        // fs2 cannot upgrade in place, so drop the shared lock first; the
        // bounded retry loop (instead of a blocking lock_exclusive) is what
        // keeps a stuck or long-lived shared holder from hanging us forever.
        let lock_path = self.root.join("lock");
        self._lock.unlock().map_err(|e| io_err(&lock_path, e))?;
        let deadline = std::time::Instant::now() + max_wait;
        loop {
            match self._lock.try_lock_exclusive() {
                Ok(()) => break,
                Err(e) if e.kind() == fs2::lock_contended_error().kind() => {
                    if std::time::Instant::now() >= deadline {
                        // Give the shared lock back before reporting busy so
                        // this handle stays in its normal locked state.
                        self._lock
                            .lock_shared()
                            .map_err(|e| io_err(&lock_path, e))?;
                        return Err(StoreError::Busy);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => return Err(io_err(&lock_path, e)),
            }
        }

        let live = self.live_blob_hashes()?;
        let mut deleted = 0u64;
        let mut freed = 0u64;
        let objects = self.root.join("objects");
        for shard in fs::read_dir(&objects).map_err(|e| io_err(&objects, e))? {
            let shard = shard.map_err(|e| io_err(&objects, e))?;
            if !shard.path().is_dir() {
                continue;
            }
            for blob in fs::read_dir(shard.path()).map_err(|e| io_err(&shard.path(), e))? {
                let blob = blob.map_err(|e| io_err(&shard.path(), e))?;
                let name = blob.file_name().to_string_lossy().to_string();
                if !live.contains(&name) {
                    let meta = blob.metadata().map_err(|e| io_err(&blob.path(), e))?;
                    // Blobs are stored read-only; restore write permission so
                    // unlink works everywhere.
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = fs::set_permissions(blob.path(), fs::Permissions::from_mode(0o644));
                    }
                    fs::remove_file(blob.path()).map_err(|e| io_err(&blob.path(), e))?;
                    deleted += 1;
                    freed += meta.len();
                }
            }
        }
        // v2 chunk layer: sidecars of dead blobs go first, then every pack
        // no surviving sidecar references. In packs-only mode this never
        // touches the packs of live sidecars — a blob evicted from the v1
        // cache keeps its chunks and can be reassembled again.
        let (extra_deleted, extra_freed) = chunks::gc_sweep(&self.root, &live)?;
        deleted += extra_deleted;
        freed += extra_freed;
        // Blob-cache markers whose blob is gone (v1 pass above or an
        // earlier eviction) are stale: drop them.
        let _ = blob_cache::sweep_orphans(&self.root);
        self._lock.unlock().map_err(|e| io_err(&lock_path, e))?;
        self._lock
            .lock_shared()
            .map_err(|e| io_err(&lock_path, e))?;
        Ok((deleted, freed))
    }

    /// Physical store statistics (deduplicated).
    pub fn blob_stats(&self) -> Result<BlobStats> {
        let objects = self.root.join("objects");
        let mut count = 0u64;
        let mut bytes = 0u64;
        for shard in fs::read_dir(&objects).map_err(|e| io_err(&objects, e))? {
            let shard = shard.map_err(|e| io_err(&objects, e))?;
            if !shard.path().is_dir() {
                continue;
            }
            for blob in fs::read_dir(shard.path()).map_err(|e| io_err(&shard.path(), e))? {
                let blob = blob.map_err(|e| io_err(&shard.path(), e))?;
                count += 1;
                bytes += blob.metadata().map_err(|e| io_err(&blob.path(), e))?.len();
            }
        }
        Ok(BlobStats { count, bytes })
    }

    /// Check that every blob referenced by `manifest` is present.
    /// Returns the list of missing hashes (empty = complete, provably offline-ready).
    pub fn missing_blobs(&self, manifest: &Manifest) -> Vec<String> {
        manifest
            .referenced_blobs()
            .keys()
            .filter(|h| !self.has_blob(h))
            .map(|h| h.to_string())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use myc_manifest::{Entry, EntryKind, RuntimeConfig, SCHEMA_VERSION};

    fn temp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("store")).unwrap();
        (dir, store)
    }

    fn manifest_with_blob(name: &str, hash: &str, size: u64) -> Manifest {
        Manifest {
            schema: SCHEMA_VERSION,
            name: name.into(),
            origin: None,
            os: "linux".into(),
            arch: "amd64".into(),
            created: "2026-01-01T00:00:00Z".into(),
            config: RuntimeConfig::default(),
            entries: vec![Entry {
                path: "/data".into(),
                kind: EntryKind::File,
                mode: 0o644,
                uid: 0,
                gid: 0,
                size,
                blake3: Some(hash.into()),
                target: None,
                device: None,
            }],
        }
    }

    #[test]
    fn blob_roundtrip_and_dedup() {
        let (_d, store) = temp_store();
        let data = b"hello mycel".to_vec();
        let (h1, s1) = store.put_blob(&mut &data[..]).unwrap();
        let (h2, _) = store.put_blob(&mut &data[..]).unwrap();
        assert_eq!(h1, h2);
        assert_eq!(s1, data.len() as u64);
        assert!(store.has_blob(&h1));
        store.verify_blob(&h1).unwrap();
        let stats = store.blob_stats().unwrap();
        assert_eq!(stats.count, 1); // deduplicated
    }

    #[test]
    fn manifest_refs_and_resolve() {
        let (_d, store) = temp_store();
        let (h, _) = store.put_blob(&mut &b"x"[..]).unwrap();
        let m = manifest_with_blob("app:1", &h, 1);
        let id = store.put_manifest(&m).unwrap();
        store.set_ref("app:1", &id).unwrap();
        assert_eq!(store.resolve("app:1").unwrap(), id);
        assert_eq!(store.resolve(&id).unwrap(), id);
        // prefix
        assert_eq!(store.resolve(&id[..12]).unwrap(), id);
        assert!(store.missing_blobs(&m).is_empty());
    }

    #[test]
    fn gc_keeps_live_deletes_dead() {
        let (_d, store) = temp_store();
        let (live, _) = store.put_blob(&mut &b"live"[..]).unwrap();
        let (dead, _) = store.put_blob(&mut &b"dead"[..]).unwrap();
        let m = manifest_with_blob("app:1", &live, 4);
        store.put_manifest(&m).unwrap();
        let (deleted, _) = store.gc().unwrap();
        assert_eq!(deleted, 1);
        assert!(store.has_blob(&live));
        assert!(!store.has_blob(&dead));
    }

    #[test]
    fn gc_reports_busy_instead_of_blocking_when_store_is_held() {
        let (_d, store) = temp_store();
        store.put_blob(&mut &b"dead"[..]).unwrap();
        // A second handle (another process in real life: an ingest, a
        // container still materializing) holds the shared lock.
        let holder = Store::open(store.root()).unwrap();
        let err = store
            .gc_with_wait(std::time::Duration::from_millis(200))
            .unwrap_err();
        assert!(matches!(err, StoreError::Busy), "got {err:?}");
        // The handle recovered its shared lock and still works.
        assert_eq!(store.blob_stats().unwrap().count, 1);
        // Once the holder lets go, gc proceeds normally.
        drop(holder);
        let (deleted, _) = store.gc().unwrap();
        assert_eq!(deleted, 1);
    }

    #[test]
    fn release_lock_unblocks_gc_like_a_running_container() {
        let (_d, store) = temp_store();
        store.put_blob(&mut &b"dead"[..]).unwrap();
        // Simulates a running container: its Store stays open for the whole
        // run, but the lock is released right after materialization.
        let runner = Store::open(store.root()).unwrap();
        runner.release_lock().unwrap();
        let (deleted, _) = store
            .gc_with_wait(std::time::Duration::from_millis(200))
            .unwrap();
        assert_eq!(deleted, 1);
    }

    #[test]
    fn remove_manifest_drops_refs() {
        let (_d, store) = temp_store();
        let (h, _) = store.put_blob(&mut &b"y"[..]).unwrap();
        let m = manifest_with_blob("gone:1", &h, 1);
        let id = store.put_manifest(&m).unwrap();
        store.set_ref("gone:1", &id).unwrap();
        store.remove_manifest(&id).unwrap();
        assert!(store.resolve_ref("gone:1").unwrap().is_none());
        assert!(store.get_manifest(&id).is_err());
    }

    #[test]
    fn chunked_flag_follows_constructor() {
        let dir = tempfile::tempdir().unwrap();
        let plain = Store::open_inner(dir.path().join("a"), false, None).unwrap();
        assert!(!plain.chunked());
        assert!(!plain.root().join("packs").exists());
        let chunked =
            Store::open_chunked_with_mode(dir.path().join("b"), ChunkMode::Transfer).unwrap();
        assert!(chunked.chunked());
        assert_eq!(chunked.chunk_mode(), Some(ChunkMode::Transfer));
        assert!(chunked.root().join("packs").is_dir());
        assert_eq!(
            std::fs::read_to_string(chunked.root().join("chunk-mode")).unwrap(),
            "transfer"
        );
    }

    #[test]
    fn dedup_hit_backfills_chunk_sidecar_for_v1_blob() {
        // A big blob stored with chunking off (v1) is re-put with chunking
        // on: the blob dedups as before, and the sidecar appears (upgrade
        // path for existing stores).
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("s");
        let v1 = Store::open_inner(&root, false, None).unwrap();
        let data = vec![5u8; 128 * 1024];
        let (hash, _) = v1.put_blob(&mut &data[..]).unwrap();
        assert!(v1.blob_chunks(&hash).unwrap().is_none());
        drop(v1);
        let v2 = Store::open_chunked_with_mode(&root, ChunkMode::Transfer).unwrap();
        let (h2, _) = v2.put_blob(&mut &data[..]).unwrap();
        assert_eq!(hash, h2);
        assert!(v2.blob_chunks(&hash).unwrap().is_some());
        assert_eq!(v2.reassemble_blob(&hash).unwrap(), data);
    }
}
