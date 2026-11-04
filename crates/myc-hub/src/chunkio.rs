//! Read-only access to the store v2 pack format (`packs/pack-<h>.zst|.idx`).
//!
//! The store's public v2 API answers "is this chunk present?" (`has_chunk`,
//! `missing_chunks`) and "reassemble this whole blob" (`reassemble_blob`) —
//! but the hub needs the step in between: **read one chunk's bytes by id**,
//! to serve `GET /api/v1/chunks/{id}` and to rebuild a blob from a mix of
//! chunks already in the local packs and chunks just received over the wire.
//! `myc-store` does not expose that (its `ChunkIndex`/`PackIdx` are private;
//! noted in `docs/design/store-v2.md` §8 as an API gap), so this module
//! re-implements the minimal reader for the documented, magic-validated
//! format:
//!
//! ```text
//! pack-<h>.idx : "MYCPACKI1" | u32 BE count | count ×
//!                [ chunk-id[32] | u64 BE frame offset |
//!                  u32 BE compressed len | u32 BE raw len ]     (48 B/rec)
//! pack-<h>.zst : concatenation of independent zstd frames, one per chunk
//! ```
//!
//! Packs are immutable and content-addressed, so this is a stable,
//! race-free read. A pack that fails to parse (wrong magic, truncated) is
//! skipped — its chunks simply read as absent, which makes the peer resend
//! them: the safe direction. Every chunk read is re-hashed with BLAKE3 and
//! must match its id before being returned, so a corrupted pack can never
//! hand out wrong bytes.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use myc_store::ChunkId;

/// Magic of `pack-<h>.idx` (see `myc-store::chunks`).
const PACK_IDX_MAGIC: &[u8; 9] = b"MYCPACKI1";
/// Bytes per index record: 32 (id) + 8 (offset) + 4 + 4 (lengths).
const IDX_RECORD: usize = 48;

/// Where one chunk lives inside the pack layer of a store.
struct Location {
    /// 64-hex pack name (BLAKE3 of the pack file).
    pack: String,
    offset: u64,
    clen: u32,
    ulen: u32,
}

/// Chunk id → location, over every pack of one store root. Built once per
/// push/pull negotiation (a directory scan + one small index per pack), then
/// shared by every blob transfer of the session.
pub struct ChunkLocator {
    entries: HashMap<ChunkId, Location>,
}

impl ChunkLocator {
    /// Index every `packs/pack-<h>.idx` under `root`. Unreadable or corrupt
    /// packs are skipped; a missing `packs/` directory yields an empty
    /// locator (every chunk reads as absent).
    pub fn scan(root: &Path) -> ChunkLocator {
        let mut entries = HashMap::new();
        let Ok(dir) = fs::read_dir(root.join("packs")) else {
            return ChunkLocator { entries };
        };
        for entry in dir.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(hex) = name
                .strip_prefix("pack-")
                .and_then(|s| s.strip_suffix(".idx"))
            else {
                continue;
            };
            if hex.len() != 64 {
                continue;
            }
            let Ok(bytes) = fs::read(entry.path()) else {
                continue;
            };
            for (id, (offset, clen, ulen)) in parse_idx(&bytes) {
                // The writer never duplicates a chunk across packs, but if
                // it ever happened the bytes would be identical (content-
                // addressed): keep the first found.
                entries.entry(id).or_insert(Location {
                    pack: hex.to_string(),
                    offset,
                    clen,
                    ulen,
                });
            }
        }
        ChunkLocator { entries }
    }

    /// Distinct pack names holding at least one of `ids` (sorted) — the
    /// `pack_hints` of the negotiation response.
    pub fn packs_of(&self, ids: &[ChunkId]) -> Vec<String> {
        let mut packs: Vec<String> = ids
            .iter()
            .filter_map(|id| self.entries.get(id))
            .map(|l| l.pack.clone())
            .collect();
        packs.sort();
        packs.dedup();
        packs
    }
}

/// Parse the records of one `.idx` file (prefix-tolerant: records are only
/// taken while a full 48-byte record fits, so a truncated tail cannot
/// fabricate an entry).
fn parse_idx(bytes: &[u8]) -> Vec<(ChunkId, (u64, u32, u32))> {
    let mut out = Vec::new();
    if bytes.len() < 13 || &bytes[0..9] != PACK_IDX_MAGIC {
        return out;
    }
    let count = u32::from_be_bytes(bytes[9..13].try_into().expect("4 bytes")) as usize;
    let records = bytes
        .get(13..)
        .map(|b| b.len() / IDX_RECORD)
        .unwrap_or(0)
        .min(count);
    for i in 0..records {
        let r = &bytes[13 + i * IDX_RECORD..13 + (i + 1) * IDX_RECORD];
        let mut id = [0u8; 32];
        id.copy_from_slice(&r[0..32]);
        let offset = u64::from_be_bytes(r[32..40].try_into().expect("8 bytes"));
        let clen = u32::from_be_bytes(r[40..44].try_into().expect("4 bytes"));
        let ulen = u32::from_be_bytes(r[44..48].try_into().expect("4 bytes"));
        out.push((id, (offset, clen, ulen)));
    }
    out
}

/// Read and decompress one chunk, verifying its BLAKE3 against `id`.
/// `None` when the chunk is not in any indexed pack or anything on the read
/// path disagrees (missing file, short read, zstd error, hash mismatch).
pub fn read_chunk(root: &Path, locator: &ChunkLocator, id: &ChunkId) -> Option<Vec<u8>> {
    let loc = locator.entries.get(id)?;
    let path = root.join("packs").join(format!("pack-{}.zst", loc.pack));
    let mut file = fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(loc.offset)).ok()?;
    let mut frame = vec![0u8; loc.clen as usize];
    file.read_exact(&mut frame).ok()?;
    let raw = zstd::bulk::decompress(&frame, loc.ulen as usize).ok()?;
    (blake3::hash(&raw).as_bytes() == id).then_some(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use myc_store::Store;

    #[test]
    fn reads_chunks_the_store_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_chunked(dir.path().join("s")).unwrap();
        // 64 KiB+ of compressible, structured data → chunked by the store.
        let mut data = Vec::new();
        for i in 0..16 {
            data.extend_from_slice(&vec![i; 4 * 1024]);
        }
        let (hash, _) = store.put_blob(&mut &data[..]).unwrap();
        let ids = store.blob_chunks(&hash).unwrap().unwrap();
        assert!(!ids.is_empty());

        let locator = ChunkLocator::scan(store.root());
        // The locator sees the same packs the store advertised.
        assert_eq!(locator.packs_of(&ids).len(), 1);
        for id in &ids {
            let raw = read_chunk(store.root(), &locator, id).expect("chunk readable");
            assert_eq!(blake3::hash(&raw).as_bytes(), id);
        }
        // A chunk nobody has reads as absent, not as an error.
        let absent = [7u8; 32];
        assert!(read_chunk(store.root(), &locator, &absent).is_none());
        // Reassembling by hand equals the original blob (this is exactly
        // what both hub assembly paths rely on).
        let mut rebuilt = Vec::new();
        for id in &ids {
            rebuilt.extend_from_slice(&read_chunk(store.root(), &locator, id).unwrap());
        }
        assert_eq!(rebuilt, data);
    }

    #[test]
    fn empty_store_has_empty_locator() {
        let dir = tempfile::tempdir().unwrap();
        let locator = ChunkLocator::scan(dir.path());
        assert!(locator.packs_of(&[[1u8; 32]]).is_empty());
    }
}
