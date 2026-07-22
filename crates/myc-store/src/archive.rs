//! Portable environment archives (`.mycel` files): one manifest plus every
//! blob it references, packed as a gzipped tar. This is the single
//! implementation behind `myc export` / `myc import` and the dashboard's
//! `/api/export` / `/api/import` endpoints.
//!
//! Layout inside the archive:
//!
//! ```text
//! manifest.json      canonical manifest JSON (first entry)
//! blobs/<hash>       raw blob content, named by BLAKE3 hex hash
//! ```

use crate::{Store, StoreError};
use myc_manifest::Manifest;
use std::io::{Read, Write};

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("{0} blob(s) missing from the local store — pull the environment first")]
    MissingBlobs(usize),
    #[error("this is not a mycel archive (no manifest.json inside)")]
    NoManifest,
    #[error("archive corrupt: blob {expected} hashed to {actual}")]
    CorruptBlob { expected: String, actual: String },
    #[error("archive incomplete: {0} blob(s) referenced by the manifest are not in it")]
    Incomplete(usize),
    #[error("cannot read the archive (is it really a .mycel file?): {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Manifest(#[from] myc_manifest::ManifestError),
}

/// Outcome of [`export_archive`].
#[derive(Debug)]
pub struct ExportReport {
    /// Blobs written into the archive.
    pub blobs: usize,
    /// Uncompressed bytes of blob content written.
    pub bytes: u64,
}

/// Outcome of [`import_archive`].
#[derive(Debug)]
pub struct ImportReport {
    pub manifest_id: String,
    /// The manifest's name, also set as a ref pointing at it.
    pub name: String,
    /// Blobs read from the archive (already-present ones are deduped by the
    /// store but still counted here).
    pub blobs: usize,
}

/// Write `manifest` and all its blobs as a gzipped tar to `out`.
/// Fails with [`ArchiveError::MissingBlobs`] before writing anything if the
/// store does not hold every referenced blob.
pub fn export_archive<W: Write>(
    store: &Store,
    manifest: &Manifest,
    out: W,
) -> Result<ExportReport, ArchiveError> {
    let missing = store.missing_blobs(manifest);
    if !missing.is_empty() {
        return Err(ArchiveError::MissingBlobs(missing.len()));
    }

    let gz = flate2::write::GzEncoder::new(out, flate2::Compression::default());
    let mut builder = tar::Builder::new(gz);

    // manifest.json first, then blobs under blobs/<hash>.
    let manifest_bytes = manifest.to_canonical_json()?;
    let mut header = tar::Header::new_gnu();
    header.set_size(manifest_bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder.append_data(&mut header, "manifest.json", manifest_bytes.as_slice())?;

    let mut blobs = 0usize;
    let mut bytes = 0u64;
    for (hash, _) in manifest.referenced_blobs() {
        let blob_path = store.blob(hash)?;
        let mut f = std::fs::File::open(&blob_path)?;
        let size = f.metadata()?.len();
        let mut header = tar::Header::new_gnu();
        header.set_size(size);
        header.set_mode(0o444);
        header.set_cksum();
        builder.append_data(&mut header, format!("blobs/{hash}"), &mut f)?;
        blobs += 1;
        bytes += size;
    }
    builder.into_inner()?.finish()?;
    Ok(ExportReport { blobs, bytes })
}

/// Read a `.mycel` archive from `input` into the store: every blob is
/// re-hashed on the way in, the manifest is stored last, and a ref named
/// after the manifest is set. Fails if any blob lies about its hash or if
/// the archive does not contain everything the manifest references.
pub fn import_archive<R: Read>(store: &Store, input: R) -> Result<ImportReport, ArchiveError> {
    let gz = flate2::read::GzDecoder::new(input);
    let mut ar = tar::Archive::new(gz);

    let mut manifest: Option<Manifest> = None;
    let mut blobs = 0usize;
    for entry in ar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().to_string();
        if path == "manifest.json" {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            manifest = Some(Manifest::from_json(&bytes)?);
        } else if let Some(expected_hash) = path.strip_prefix("blobs/") {
            let expected_hash = expected_hash.to_string();
            let (actual, _) = store.put_blob(&mut entry)?;
            if actual != expected_hash {
                return Err(ArchiveError::CorruptBlob {
                    expected: expected_hash,
                    actual,
                });
            }
            blobs += 1;
        }
    }
    let manifest = manifest.ok_or(ArchiveError::NoManifest)?;
    let id = store.put_manifest(&manifest)?;
    store.set_ref(&manifest.name, &id)?;
    let missing = store.missing_blobs(&manifest);
    if !missing.is_empty() {
        return Err(ArchiveError::Incomplete(missing.len()));
    }
    Ok(ImportReport {
        manifest_id: id,
        name: manifest.name,
        blobs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use myc_manifest::{Entry, EntryKind, Manifest, RuntimeConfig, SCHEMA_VERSION};

    fn bare_manifest(name: &str, entries: Vec<Entry>) -> Manifest {
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

    fn file_entry(path: &str, hash: String, size: u64) -> Entry {
        Entry {
            path: path.into(),
            kind: EntryKind::File,
            mode: 0o644,
            uid: 0,
            gid: 0,
            size,
            blake3: Some(hash),
            target: None,
            device: None,
        }
    }

    fn manifest_with_blob(store: &Store, content: &[u8]) -> Manifest {
        let (hash, size) = store.put_blob(&mut &content[..]).unwrap();
        bare_manifest("archive-test:1", vec![file_entry("/hello", hash, size)])
    }

    #[test]
    fn export_then_import_round_trips() {
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();
        let src = Store::open(src_dir.path()).unwrap();
        let dst = Store::open(dst_dir.path()).unwrap();

        let manifest = manifest_with_blob(&src, b"hello archive");
        let id = src.put_manifest(&manifest).unwrap();

        let mut buf = Vec::new();
        let report = export_archive(&src, &manifest, &mut buf).unwrap();
        assert_eq!(report.blobs, 1);
        assert_eq!(report.bytes, b"hello archive".len() as u64);

        let imported = import_archive(&dst, buf.as_slice()).unwrap();
        assert_eq!(imported.manifest_id, id);
        assert_eq!(imported.name, "archive-test:1");
        assert_eq!(imported.blobs, 1);
        assert_eq!(dst.resolve("archive-test:1").unwrap(), id);
        assert!(dst.missing_blobs(&manifest).is_empty());
    }

    #[test]
    fn export_refuses_missing_blobs() {
        let src_dir = tempfile::tempdir().unwrap();
        let src = Store::open(src_dir.path()).unwrap();
        let manifest = bare_manifest("no-blobs:1", vec![file_entry("/gone", "0".repeat(64), 1)]);
        let err = export_archive(&src, &manifest, Vec::new()).unwrap_err();
        assert!(matches!(err, ArchiveError::MissingBlobs(1)));
    }

    #[test]
    fn import_rejects_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let err = import_archive(&store, &b"definitely not a tarball"[..]).unwrap_err();
        assert!(matches!(
            err,
            ArchiveError::Io(_) | ArchiveError::NoManifest
        ));
    }
}
