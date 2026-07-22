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
//!   lock                         inter-process lock file
//! ```
//!
//! Blobs are written atomically (temp file + rename) and stored read-only.
//! Every file of every environment lives here exactly once; rootfs
//! materialization hardlinks out of this store.

pub mod archive;
pub mod volumes;

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
}

pub struct BlobStats {
    pub count: u64,
    pub bytes: u64,
}

impl Store {
    /// Open (creating if needed) the store at `root` and take a shared
    /// inter-process lock. GC upgrades to an exclusive lock.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        for sub in ["objects", "manifests", "refs", "pins", "tmp"] {
            let p = root.join(sub);
            fs::create_dir_all(&p).map_err(|e| io_err(&p, e))?;
        }
        let lock_path = root.join("lock");
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|e| io_err(&lock_path, e))?;
        lock.lock_shared().map_err(|e| io_err(&lock_path, e))?;
        Ok(Store { root, _lock: lock })
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
            return Ok((hash, size)); // dedup hit; drop the temp file
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
        Ok((hash, size))
    }

    pub fn has_blob(&self, hash: &str) -> bool {
        self.blob_path(hash).exists()
    }

    /// Absolute path of a blob, verifying existence.
    pub fn blob(&self, hash: &str) -> Result<PathBuf> {
        let p = self.blob_path(hash);
        if !p.exists() {
            return Err(StoreError::BlobMissing(hash.to_string()));
        }
        Ok(p)
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

    /// Delete every blob not referenced by any stored manifest.
    /// Returns (blobs deleted, bytes freed).
    pub fn gc(&self) -> Result<(u64, u64)> {
        // Upgrade to exclusive lock: no concurrent ingest/run during GC.
        let lock_path = self.root.join("lock");
        self._lock.unlock().map_err(|e| io_err(&lock_path, e))?;
        self._lock
            .lock_exclusive()
            .map_err(|e| io_err(&lock_path, e))?;

        let mut live: HashSet<String> = HashSet::new();
        for (_, manifest) in self.list_manifests()? {
            for (hash, _) in manifest.referenced_blobs() {
                live.insert(hash.to_string());
            }
        }
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
}
