//! Block-level deduplication layer (store format v2, opt-in).
//!
//! When chunking is enabled ([`crate::Store::open_chunked`] or
//! `MYCEL_CHUNKED=1` for [`crate::Store::open`]), every blob of at least
//! [`CHUNK_MIN_BLOB_SIZE`] bytes is additionally cut into content-defined
//! blocks ([`myc_chunk`]: FastCDC/Gear, 2–16 KiB, average 4 KiB). The v1 blob
//! file keeps being written unchanged (`objects/`), so v1 readers, rootfs
//! hardlinking and GC keep working exactly as before — the chunk layer is
//! purely additive and serves network deduplication: a hub push/pull only
//! needs to transfer the chunks it is missing.
//!
//! Layout added under the store root:
//!
//! ```text
//! .mycel/
//!   packs/
//!     pack-<blake3-of-pack>.zst     concatenation of per-chunk zstd frames
//!     pack-<blake3-of-pack>.idx     chunk hash -> (offset, sizes) records
//!     pack-<blake3-of-pack>.bloom   Bloom filter over the pack's chunk hashes
//!   chunks/
//!     <blob-hash>.chunks            sidecar: ordered chunk list of one blob
//! ```
//!
//! * A **pack** is immutable and named by the BLAKE3 of its bytes, so the
//!   same missing-chunk set always produces the same deterministic name
//!   (idempotent ingest). Chunks are compressed individually (independent
//!   zstd frames, level 3, restic-style) so a single chunk can be read,
//!   decompressed and reassembled without touching the rest of the pack.
//! * The **Bloom filter** answers "is this chunk in this pack?" without
//!   reading the index; a hit falls through to the in-memory index. The
//!   overall `has_chunk` answer therefore has no false positives — a Bloom
//!   false positive only costs one index load (the Data Domain
//!   summary-vector trick).
//! * The **sidecar** gives the ordered chunk list of one blob plus the pack
//!   table it depends on; it is what a future hub protocol will diff to
//!   compute `missing_chunks`.
//!
//! ## Disk modes (`MYCEL_CHUNK_MODE`, see [`ChunkMode`])
//!
//! The +48,7 % disk overhead of the original design (v1 blobs *and* packs
//! coexisting) is now arbitrated per store:
//!
//! * **transfer** — v1 blob + packs (the original behavior, default);
//! * **lean** — v1 blob + sidecars only, no pack is ever written. The
//!   sidecars are enough for chunk *negotiation* (`has_chunk` /
//!   `missing_chunks`), which is all the hub needs to know what we have;
//! * **packs-only** — chunked files store no v1 blob at all; packs are the
//!   source of truth and the blob is reassembled on demand
//!   ([`crate::Store::blob`]) into a size-bounded LRU cache
//!   (`MYCEL_BLOB_CACHE_MB`).
//!
//! The mode is recorded in `chunk-mode` at the store root on first v2 use
//! and must stay coherent (see [`crate::Store::open`]).
//!
//! In every mode the *knowledge* oracle (`has_chunk`, `missing_chunks`) is
//! sidecar-driven: a chunk is known iff some sidecar lists it. This holds
//! for packs too, because a pack is only ever written together with the
//! sidecar that references it and GC keeps packs alive only while a
//! surviving sidecar references them — so scanning sidecars is both
//! complete and mode-independent.

use crate::{io_err, Result, StoreError};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Identity of one chunk: the BLAKE3 digest of its bytes.
pub type ChunkId = [u8; 32];

/// Blobs smaller than this are not chunked (whole-file dedup already covers
/// them; the survey asks for chunking only from 64 KiB up).
pub const CHUNK_MIN_BLOB_SIZE: u64 = 64 * 1024;

/// Blobs larger than this are not chunked either: indexing reads the whole
/// blob into memory, and the chunk layer is pointless for disk-image-sized
/// blobs until the hub protocol exists.
pub const CHUNK_MAX_BLOB_SIZE: u64 = 256 * 1024 * 1024;

/// Target Bloom false-positive rate per pack (measured bound in tests).
pub const BLOOM_TARGET_FP: f64 = 0.005;

/// zstd level for per-chunk frames inside a pack.
const ZSTD_LEVEL: i32 = 3;

const PACK_IDX_MAGIC: &[u8; 9] = b"MYCPACKI1";
const BLOOM_MAGIC: &[u8; 9] = b"MYCBLOOM1";
const SIDECAR_MAGIC: &[u8; 9] = b"MYCCHNKI1";
/// Lean sidecars (no pack table): `[magic][u32 count][count x 32B ids]`.
const SIDECAR_LEAN_MAGIC: &[u8; 9] = b"MYCCHNKL1";

/// Name of the file (under the store root) that pins the store's chunk mode.
const CHUNK_MODE_FILE: &str = "chunk-mode";

// ---------------------------------------------------------------------------
// Chunk modes
// ---------------------------------------------------------------------------

/// How the v2 chunk layer spends disk, per store (`MYCEL_CHUNK_MODE`).
///
/// The mode is written to `chunk-mode` on first v2 use and is then part of
/// the store's identity: switching is allowed only when it cannot orphan
/// data that exists solely as packs (see [`crate::Store::open`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkMode {
    /// v1 blob + packs (default, the original v2 behavior). Maximum disk,
    /// chunked transfers possible, `myc-run` hardlinks the v1 files.
    Transfer,
    /// v1 blob + chunk sidecars, **no packs**: the chunks are only used for
    /// negotiation (knowing what we have). Disk = v1 + a few KiB of
    /// sidecars.
    Lean,
    /// No v1 blob for chunked files: packs are the source of truth and the
    /// blob is reassembled on demand into a size-bounded LRU cache
    /// (`MYCEL_BLOB_CACHE_MB`, see [`crate::blob_cache`]). Minimum disk,
    /// first use of an environment pays reassembly.
    PacksOnly,
}

impl ChunkMode {
    pub const TRANSFER: &str = "transfer";
    pub const LEAN: &str = "lean";
    pub const PACKS_ONLY: &str = "packs-only";

    pub const fn as_str(self) -> &'static str {
        match self {
            ChunkMode::Transfer => Self::TRANSFER,
            ChunkMode::Lean => Self::LEAN,
            ChunkMode::PacksOnly => Self::PACKS_ONLY,
        }
    }

    /// Parse the canonical names used in `MYCEL_CHUNK_MODE` and in the
    /// `chunk-mode` file.
    pub fn parse(name: &str) -> Option<ChunkMode> {
        match name.trim() {
            Self::TRANSFER => Some(ChunkMode::Transfer),
            Self::LEAN => Some(ChunkMode::Lean),
            Self::PACKS_ONLY => Some(ChunkMode::PacksOnly),
            _ => None,
        }
    }

    /// `$MYCEL_CHUNK_MODE` (default [`ChunkMode::Transfer`]); an unknown
    /// value is an error rather than a silent fallback.
    pub fn from_env() -> Result<ChunkMode> {
        match std::env::var("MYCEL_CHUNK_MODE") {
            Ok(v) => Self::parse(&v).ok_or_else(|| {
                StoreError::Chunk(format!(
                    "invalid MYCEL_CHUNK_MODE '{v}' (expected {}, {} or {})",
                    Self::TRANSFER,
                    Self::LEAN,
                    Self::PACKS_ONLY
                ))
            }),
            Err(_) => Ok(ChunkMode::Transfer),
        }
    }
}

// ---------------------------------------------------------------------------
// Bloom filter
// ---------------------------------------------------------------------------

/// Classic Bloom filter over [`ChunkId`]s, sized for a target false-positive
/// rate. Hashing is double-hash (Kirsch–Mitzenmacher): the first 16 bytes of
/// the BLAKE3 chunk id provide two independent 64-bit lanes, so `k` probes
/// cost no extra hashing. No false negatives by construction (bits are only
/// ever set, and a pack never loses chunks).
pub struct Bloom {
    bits: Vec<u8>,
    num_bits: u64,
    k: u32,
}

impl Bloom {
    /// Size the filter for `n` distinct ids at (at most) `fp` false
    /// positives: `m = -n·ln(p)/ln(2)²` bits, `k = (m/n)·ln(2)` probes.
    pub fn new(n: usize, fp: f64) -> Bloom {
        let n = n.max(1) as f64;
        let ln2 = 2f64.ln();
        let num_bits = (-(n * fp.ln()) / (ln2 * ln2)).ceil().max(8.0) as u64;
        let k = ((num_bits as f64 / n) * ln2).round().max(1.0) as u32;
        Bloom {
            bits: vec![0u8; (num_bits as usize).div_ceil(8)],
            num_bits,
            k,
        }
    }

    fn lanes(id: &ChunkId) -> (u64, u64) {
        let mut h1 = [0u8; 8];
        let mut h2 = [0u8; 8];
        h1.copy_from_slice(&id[0..8]);
        h2.copy_from_slice(&id[8..16]);
        // |1 keeps the stride odd (never zero) without losing coverage.
        (u64::from_le_bytes(h1), u64::from_le_bytes(h2) | 1)
    }

    pub fn add(&mut self, id: &ChunkId) {
        let (h1, h2) = Self::lanes(id);
        for i in 0..self.k as u64 {
            let bit = h1.wrapping_add(h2.wrapping_mul(i)) % self.num_bits;
            self.bits[(bit / 8) as usize] |= 1u8 << (bit % 8);
        }
    }

    pub fn might_contain(&self, id: &ChunkId) -> bool {
        let (h1, h2) = Self::lanes(id);
        (0..self.k as u64).all(|i| {
            let bit = h1.wrapping_add(h2.wrapping_mul(i)) % self.num_bits;
            self.bits[(bit / 8) as usize] & (1u8 << (bit % 8)) != 0
        })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(9 + 4 + 8 + self.bits.len());
        out.extend_from_slice(BLOOM_MAGIC);
        out.extend_from_slice(&self.k.to_be_bytes());
        out.extend_from_slice(&self.num_bits.to_be_bytes());
        out.extend_from_slice(&(self.bits.len() as u32).to_be_bytes());
        out.extend_from_slice(&self.bits);
        out
    }

    /// Parse a serialized filter; `None` on any size or magic mismatch.
    pub fn from_bytes(bytes: &[u8]) -> Option<Bloom> {
        if bytes.len() < 9 + 4 + 8 + 4 || &bytes[0..9] != BLOOM_MAGIC {
            return None;
        }
        let k = u32::from_be_bytes(bytes[9..13].try_into().ok()?);
        let num_bits = u64::from_be_bytes(bytes[13..21].try_into().ok()?);
        let len = u32::from_be_bytes(bytes[21..25].try_into().ok()?) as usize;
        let bits = bytes.get(25..25 + len)?.to_vec();
        if (num_bits as usize).div_ceil(8) != len || k == 0 || num_bits == 0 {
            return None;
        }
        Some(Bloom { bits, num_bits, k })
    }
}

// ---------------------------------------------------------------------------
// Binary codecs
// ---------------------------------------------------------------------------

/// pack-<hex>.idx: one record per chunk, in pack order.
struct PackIdx {
    /// chunk id -> (frame offset, compressed len, uncompressed len).
    entries: HashMap<ChunkId, (u64, u32, u32)>,
    count: usize,
    logical_bytes: u64,
}

impl PackIdx {
    fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(9 + 4 + self.count * 48);
        out.extend_from_slice(PACK_IDX_MAGIC);
        out.extend_from_slice(&(self.count as u32).to_be_bytes());
        // Deterministic order (offset) so equal packs encode identically.
        let mut recs: Vec<_> = self.entries.iter().collect();
        recs.sort_by_key(|(_, (off, _, _))| *off);
        for (id, (off, clen, ulen)) in recs {
            out.extend_from_slice(id);
            out.extend_from_slice(&off.to_be_bytes());
            out.extend_from_slice(&clen.to_be_bytes());
            out.extend_from_slice(&ulen.to_be_bytes());
        }
        out
    }

    fn from_bytes(bytes: &[u8]) -> Option<PackIdx> {
        if bytes.len() < 13 || &bytes[0..9] != PACK_IDX_MAGIC {
            return None;
        }
        let count = u32::from_be_bytes(bytes[9..13].try_into().ok()?) as usize;
        let expected = 13 + count * 48;
        let body = bytes.get(13..expected)?;
        let mut entries = HashMap::with_capacity(count);
        let mut logical = 0u64;
        for i in 0..count {
            let r = &body[i * 48..(i + 1) * 48];
            let mut id = [0u8; 32];
            id.copy_from_slice(&r[0..32]);
            let off = u64::from_be_bytes(r[32..40].try_into().ok()?);
            let clen = u32::from_be_bytes(r[40..44].try_into().ok()?);
            let ulen = u32::from_be_bytes(r[44..48].try_into().ok()?);
            logical += ulen as u64;
            entries.insert(id, (off, clen, ulen));
        }
        Some(PackIdx {
            entries,
            count,
            logical_bytes: logical,
        })
    }

    fn find(&self, id: &ChunkId) -> Option<(u64, u32, u32)> {
        self.entries.get(id).copied()
    }
}

/// chunks/<blob-hash>.chunks: the blob's ordered chunk recipe.
#[derive(Debug)]
struct Sidecar {
    /// Pack content hashes (raw 32 bytes) referenced by this blob.
    packs: Vec<[u8; 32]>,
    /// Chunk ids in blob order.
    chunks: Vec<ChunkId>,
    /// For each chunk, its index in `packs`.
    slots: Vec<u16>,
}

impl Sidecar {
    fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(13 + self.packs.len() * 32 + self.chunks.len() * 34);
        out.extend_from_slice(SIDECAR_MAGIC);
        out.extend_from_slice(&(self.packs.len() as u32).to_be_bytes());
        for p in &self.packs {
            out.extend_from_slice(p);
        }
        out.extend_from_slice(&(self.chunks.len() as u32).to_be_bytes());
        for (id, slot) in self.chunks.iter().zip(&self.slots) {
            out.extend_from_slice(id);
            out.extend_from_slice(&slot.to_be_bytes());
        }
        out
    }

    fn from_bytes(bytes: &[u8]) -> Option<Sidecar> {
        if bytes.len() < 9 || &bytes[0..9] != SIDECAR_MAGIC {
            return None;
        }
        let packs = u32::from_be_bytes(bytes[9..13].try_into().ok()?) as usize;
        let pos = 13 + packs * 32;
        let body = bytes.get(13..pos)?;
        let mut pack_list = Vec::with_capacity(packs);
        for i in 0..packs {
            let mut id = [0u8; 32];
            id.copy_from_slice(&body[i * 32..(i + 1) * 32]);
            pack_list.push(id);
        }
        let count = u32::from_be_bytes(bytes.get(pos..pos + 4)?.try_into().ok()?) as usize;
        let body = bytes.get(pos + 4..pos + 4 + count * 34)?;
        let mut chunks = Vec::with_capacity(count);
        let mut slots = Vec::with_capacity(count);
        for i in 0..count {
            let r = &body[i * 34..(i + 1) * 34];
            let mut id = [0u8; 32];
            id.copy_from_slice(&r[0..32]);
            let slot = u16::from_be_bytes(r[32..34].try_into().ok()?);
            if slot as usize >= packs {
                return None;
            }
            chunks.push(id);
            slots.push(slot);
        }
        Some(Sidecar {
            packs: pack_list,
            chunks,
            slots,
        })
    }
}

/// A sidecar as found on disk, in either format.
///
/// * full (`MYCCHNKI1`): pack table + ordered chunk ids (transfer,
///   packs-only) — the reassembly recipe;
/// * lean (`MYCCHNKL1`): ordered chunk ids only — negotiation knowledge
///   without the pack bytes.
#[derive(Debug)]
enum AnySidecar {
    Full(Sidecar),
    Lean(Vec<ChunkId>),
}

impl AnySidecar {
    fn decode(bytes: &[u8]) -> Option<AnySidecar> {
        if bytes.len() < 9 {
            return None;
        }
        if bytes[0..9] == *SIDECAR_MAGIC {
            Sidecar::from_bytes(bytes).map(AnySidecar::Full)
        } else if bytes[0..9] == *SIDECAR_LEAN_MAGIC {
            let count = u32::from_be_bytes(bytes.get(9..13)?.try_into().ok()?) as usize;
            let body = bytes.get(13..13 + count * 32)?;
            let mut chunks = Vec::with_capacity(count);
            for i in 0..count {
                let mut id = [0u8; 32];
                id.copy_from_slice(&body[i * 32..(i + 1) * 32]);
                chunks.push(id);
            }
            Some(AnySidecar::Lean(chunks))
        } else {
            None
        }
    }

    fn chunks(&self) -> &[ChunkId] {
        match self {
            AnySidecar::Full(sc) => &sc.chunks,
            AnySidecar::Lean(ids) => ids,
        }
    }

    /// Pack content hashes this sidecar keeps alive (empty for lean).
    fn pack_refs(&self) -> &[[u8; 32]] {
        match self {
            AnySidecar::Full(sc) => &sc.packs,
            AnySidecar::Lean(_) => &[],
        }
    }

    fn encode_lean(chunks: &[ChunkId]) -> Vec<u8> {
        let mut out = Vec::with_capacity(13 + chunks.len() * 32);
        out.extend_from_slice(SIDECAR_LEAN_MAGIC);
        out.extend_from_slice(&(chunks.len() as u32).to_be_bytes());
        for id in chunks {
            out.extend_from_slice(id);
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Pack scanning / query
// ---------------------------------------------------------------------------

struct PackSlot {
    /// 64-hex pack name (BLAKE3 of the pack file).
    name: String,
    /// None = no (readable) Bloom: always fall through to the index.
    bloom: Option<Bloom>,
    idx: Option<PackIdx>,
}

/// In-memory view of the pack layer. Blooms are loaded eagerly (a few KiB
/// each); pack indexes are loaded lazily, only when a Bloom filter admits
/// the queried chunk.
struct ChunkIndex {
    packs_dir: PathBuf,
    packs: Vec<PackSlot>,
}

impl ChunkIndex {
    fn scan(root: &Path) -> Result<ChunkIndex> {
        let packs_dir = root.join("packs");
        let mut packs = Vec::new();
        let entries = match fs::read_dir(&packs_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ChunkIndex { packs_dir, packs })
            }
            Err(e) => return Err(io_err(&packs_dir, e)),
        };
        for entry in entries {
            let entry = entry.map_err(|e| io_err(&packs_dir, e))?;
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(hex) = name
                .strip_prefix("pack-")
                .and_then(|s| s.strip_suffix(".zst"))
            else {
                continue;
            };
            if hex.len() != 64 {
                continue;
            }
            let bloom = fs::read(packs_dir.join(format!("pack-{hex}.bloom")))
                .ok()
                .and_then(|b| Bloom::from_bytes(&b));
            packs.push(PackSlot {
                name: hex.to_string(),
                bloom,
                idx: None,
            });
        }
        packs.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(ChunkIndex { packs_dir, packs })
    }

    fn load_idx(&mut self, i: usize) -> Result<&PackIdx> {
        if self.packs[i].idx.is_none() {
            let path = self
                .packs_dir
                .join(format!("pack-{}.idx", self.packs[i].name));
            let bytes = fs::read(&path).map_err(|e| io_err(&path, e))?;
            let idx = PackIdx::from_bytes(&bytes).ok_or_else(|| {
                StoreError::Chunk(format!("corrupt pack index pack-{}", self.packs[i].name))
            })?;
            self.packs[i].idx = Some(idx);
        }
        Ok(self.packs[i].idx.as_ref().expect("just set"))
    }

    /// Where does this chunk live? Bloom filters first, index on admission.
    fn locate(&mut self, id: &ChunkId) -> Result<Option<(String, u64, u32, u32)>> {
        for i in 0..self.packs.len() {
            if let Some(bloom) = &self.packs[i].bloom {
                if !bloom.might_contain(id) {
                    continue;
                }
            }
            if let Some((off, clen, ulen)) = self.load_idx(i)?.find(id) {
                let name = self.packs[i].name.clone();
                return Ok(Some((name, off, clen, ulen)));
            }
        }
        Ok(None)
    }

    /// Look a chunk up in one named pack (sidecar-driven reads).
    fn find_in(&mut self, name: &str, id: &ChunkId) -> Result<Option<(u64, u32, u32)>> {
        let i = self
            .packs
            .iter()
            .position(|p| p.name == name)
            .ok_or_else(|| StoreError::Chunk(format!("unknown pack pack-{name}")))?;
        Ok(self.load_idx(i)?.find(id))
    }
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

fn sidecar_path(root: &Path, blob_hash: &str) -> PathBuf {
    root.join("chunks").join(format!("{blob_hash}.chunks"))
}

fn pack_path(root: &Path, name: &str) -> PathBuf {
    root.join("packs").join(format!("pack-{name}.zst"))
}

/// Does this blob have a (full or lean) chunk sidecar?
pub fn sidecar_exists(root: &Path, blob_hash: &str) -> bool {
    sidecar_path(root, blob_hash).exists()
}

/// The store's recorded chunk mode, or `None` before the first v2 use.
/// A present-but-unreadable `chunk-mode` file is an error, not a silent
/// reset: the mode decides where data lives.
pub fn read_chunk_mode(root: &Path) -> Result<Option<ChunkMode>> {
    match fs::read_to_string(root.join(CHUNK_MODE_FILE)) {
        Ok(s) => ChunkMode::parse(&s).map(Some).ok_or_else(|| {
            StoreError::Chunk(format!(
                "corrupt {} file (content: '{s}') — expected {}, {} or {}",
                CHUNK_MODE_FILE,
                ChunkMode::TRANSFER,
                ChunkMode::LEAN,
                ChunkMode::PACKS_ONLY
            ))
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err(&root.join(CHUNK_MODE_FILE), e)),
    }
}

/// Record the mode at first v2 use (first writer wins, like every other
/// store file).
pub fn write_chunk_mode(root: &Path, mode: ChunkMode) -> Result<()> {
    write_noclobber(root, &root.join(CHUNK_MODE_FILE), mode.as_str().as_bytes())
}

/// Overwrite the mode file after a validated switch (plain write: the file
/// is a single tiny line and switches are rare).
pub fn overwrite_chunk_mode(root: &Path, mode: ChunkMode) -> Result<()> {
    let p = root.join(CHUNK_MODE_FILE);
    fs::write(&p, mode.as_str().as_bytes()).map_err(|e| io_err(&p, e))
}

/// Write `bytes` to `dest` atomically (temp file + noclobber rename).
fn write_noclobber(root: &Path, dest: &Path, bytes: &[u8]) -> Result<()> {
    let tmp_dir = root.join("tmp");
    let mut tmp = tempfile::NamedTempFile::new_in(&tmp_dir).map_err(|e| io_err(&tmp_dir, e))?;
    tmp.write_all(bytes).map_err(|e| io_err(tmp.path(), e))?;
    tmp.flush().map_err(|e| io_err(tmp.path(), e))?;
    match tmp.persist_noclobber(dest) {
        Ok(_) => Ok(()),
        Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(io_err(dest, e.error)),
    }
}

// ---------------------------------------------------------------------------
// Indexing (write path)
// ---------------------------------------------------------------------------

/// What [`index_blob`] did for one blob.
#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
pub struct IndexStats {
    /// True when the blob already had a sidecar (nothing recomputed).
    pub skipped: bool,
    /// Chunks covering the blob.
    pub total_chunks: usize,
    /// Chunks written into the new pack.
    pub new_chunks: usize,
    /// Logical bytes of the new chunks (the bytes a hub transfer would send).
    pub new_chunk_bytes: u64,
    /// Logical bytes of chunks already present (intra-blob duplicates count
    /// as reused: they are stored once).
    pub reused_chunk_bytes: u64,
}

/// Where a plan step decided a chunk lives.
#[derive(Clone)]
enum PackRef {
    /// Existing pack (64-hex name).
    Pack(String),
    /// To be written in the new pack being built.
    New,
}

/// Cut `data` into chunks, store the ones the store is missing into a new
/// deterministic pack, and write the blob's sidecar. Idempotent: a blob
/// with an existing sidecar is left untouched (and counted as `skipped`).
pub fn index_blob(root: &Path, blob_hash: &str, data: &[u8]) -> Result<IndexStats> {
    if sidecar_path(root, blob_hash).exists() {
        return Ok(IndexStats {
            skipped: true,
            ..IndexStats::default()
        });
    }
    let bounds = myc_chunk::chunk_bounds(data);
    if bounds.is_empty() {
        return Ok(IndexStats::default());
    }
    let mut index = ChunkIndex::scan(root)?;
    let mut cache: HashMap<ChunkId, PackRef> = HashMap::new();
    let mut plan: Vec<ChunkId> = Vec::with_capacity(bounds.len());
    let mut refs: Vec<PackRef> = Vec::with_capacity(bounds.len());
    let mut new: Vec<(ChunkId, usize, usize)> = Vec::new(); // (id, start, end)
    let mut reused_bytes = 0u64;
    for &(s, e) in &bounds {
        let id: ChunkId = *blake3::hash(&data[s..e]).as_bytes();
        let r = if let Some(r) = cache.get(&id) {
            reused_bytes += (e - s) as u64;
            r.clone()
        } else {
            let r = match index.locate(&id)? {
                Some(located) => {
                    reused_bytes += (e - s) as u64;
                    PackRef::Pack(located.0)
                }
                None => {
                    new.push((id, s, e));
                    PackRef::New
                }
            };
            cache.insert(id, r.clone());
            r
        };
        plan.push(id);
        refs.push(r);
    }

    let mut new_chunk_bytes = 0u64;
    if !new.is_empty() {
        // Build the pack: one independent zstd frame per new chunk.
        let mut frames: Vec<u8> = Vec::new();
        let mut records: Vec<(ChunkId, u64, u32, u32)> = Vec::with_capacity(new.len());
        let mut bloom = Bloom::new(new.len(), BLOOM_TARGET_FP);
        for &(id, s, e) in &new {
            let enc = zstd::bulk::compress(&data[s..e], ZSTD_LEVEL)
                .map_err(|e| StoreError::Chunk(format!("zstd compress: {e}")))?;
            records.push((id, frames.len() as u64, enc.len() as u32, (e - s) as u32));
            bloom.add(&id);
            new_chunk_bytes += (e - s) as u64;
            frames.extend_from_slice(&enc);
        }
        // Pack name = BLAKE3 of the pack bytes: same missing set, same name.
        let name = blake3::hash(&frames).to_hex().to_string();
        let packs_dir = root.join("packs");
        fs::create_dir_all(&packs_dir).map_err(|e| io_err(&packs_dir, e))?;
        write_noclobber(root, &pack_path(root, &name), &frames)?;
        write_noclobber(
            root,
            &packs_dir.join(format!("pack-{name}.idx")),
            &PackIdx {
                entries: records
                    .into_iter()
                    .map(|(id, o, c, u)| (id, (o, c, u)))
                    .collect(),
                count: new.len(),
                logical_bytes: new_chunk_bytes,
            }
            .to_bytes(),
        )?;
        write_noclobber(
            root,
            &packs_dir.join(format!("pack-{name}.bloom")),
            &bloom.to_bytes(),
        )?;
        // The pack exists now: point the new refs at it.
        for r in &mut refs {
            if matches!(r, PackRef::New) {
                *r = PackRef::Pack(name.clone());
            }
        }
    }

    // Sidecar: pack table (first-use order) + ordered chunk list.
    let chunks_dir = root.join("chunks");
    fs::create_dir_all(&chunks_dir).map_err(|e| io_err(&chunks_dir, e))?;
    let mut table: Vec<[u8; 32]> = Vec::new();
    let mut slot_of: HashMap<String, u16> = HashMap::new();
    let mut slots: Vec<u16> = Vec::with_capacity(plan.len());
    for r in &refs {
        let PackRef::Pack(name) = r else {
            unreachable!("every ref points at a pack after the fixup");
        };
        let slot = match slot_of.get(name) {
            Some(s) => *s,
            None => {
                let id32: [u8; 32] = hex::decode(name)
                    .ok()
                    .and_then(|b| b.try_into().ok())
                    .expect("pack names are 64-hex blake3");
                table.push(id32);
                slot_of.insert(name.clone(), (table.len() - 1) as u16);
                (table.len() - 1) as u16
            }
        };
        slots.push(slot);
    }
    let sidecar = Sidecar {
        packs: table,
        chunks: plan.clone(),
        slots,
    };
    write_noclobber(root, &sidecar_path(root, blob_hash), &sidecar.to_bytes())?;

    Ok(IndexStats {
        skipped: false,
        total_chunks: plan.len(),
        new_chunks: new.len(),
        new_chunk_bytes,
        reused_chunk_bytes: reused_bytes,
    })
}

/// Lean indexing: compute the blob's chunk ids and *record what we now
/// know*, without writing any pack. If every chunk is already known (some
/// other sidecar lists it — we then hold a blob containing it, so a hub
/// could serve it by re-cutting), nothing is written at all; otherwise a
/// lean sidecar records the full ordered id list. Disk cost: ~32 B/chunk,
/// only when the blob teaches the store something new.
pub fn lean_index_blob(root: &Path, blob_hash: &str, data: &[u8]) -> Result<IndexStats> {
    if sidecar_exists(root, blob_hash) {
        return Ok(IndexStats {
            skipped: true,
            ..IndexStats::default()
        });
    }
    let bounds = myc_chunk::chunk_bounds(data);
    if bounds.is_empty() {
        return Ok(IndexStats::default());
    }
    let mut known = known_chunk_ids(root)?;
    let mut plan: Vec<ChunkId> = Vec::with_capacity(bounds.len());
    let mut new_chunk_bytes = 0u64;
    let mut reused_bytes = 0u64;
    let mut new_count = 0usize;
    for &(s, e) in &bounds {
        let id: ChunkId = *blake3::hash(&data[s..e]).as_bytes();
        if known.insert(id) {
            new_count += 1;
            new_chunk_bytes += (e - s) as u64;
        } else {
            reused_bytes += (e - s) as u64;
        }
        plan.push(id);
    }
    if new_count > 0 {
        let chunks_dir = root.join("chunks");
        fs::create_dir_all(&chunks_dir).map_err(|e| io_err(&chunks_dir, e))?;
        write_noclobber(
            root,
            &sidecar_path(root, blob_hash),
            &AnySidecar::encode_lean(&plan),
        )?;
    }
    Ok(IndexStats {
        skipped: false,
        total_chunks: plan.len(),
        new_chunks: new_count,
        new_chunk_bytes,
        reused_chunk_bytes: reused_bytes,
    })
}

// ---------------------------------------------------------------------------
// Read path
// ---------------------------------------------------------------------------

/// Ordered chunk ids of a chunked blob, or `None` if it has no sidecar
/// (blob below the size threshold, or ingested while chunking was off).
/// Works for both sidecar formats (full and lean).
pub fn blob_chunks(root: &Path, blob_hash: &str) -> Result<Option<Vec<ChunkId>>> {
    match fs::read(sidecar_path(root, blob_hash)) {
        Ok(bytes) => {
            let sc = AnySidecar::decode(&bytes).ok_or_else(|| {
                StoreError::Chunk(format!("corrupt chunk sidecar for blob {blob_hash}"))
            })?;
            Ok(Some(sc.chunks().to_vec()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err(&sidecar_path(root, blob_hash), e)),
    }
}

/// Every chunk id any sidecar (full or lean) lists — the store's complete
/// chunk knowledge, whatever the mode. Complete for packs too: a pack is
/// only written alongside the sidecar referencing it, and GC keeps a pack
/// only while some surviving sidecar references it. Unreadable sidecar
/// files are skipped: one corrupt sidecar must not blind the oracle.
pub fn known_chunk_ids(root: &Path) -> Result<HashSet<ChunkId>> {
    let mut known = HashSet::new();
    let dir = root.join("chunks");
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(known),
        Err(e) => return Err(io_err(&dir, e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| io_err(&dir, e))?;
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".chunks") {
            continue;
        }
        if let Ok(bytes) = fs::read(entry.path()) {
            if let Some(sc) = AnySidecar::decode(&bytes) {
                known.extend(sc.chunks().iter().copied());
            }
        }
    }
    Ok(known)
}

/// Chunks of `wanted` that this store does not have, in `wanted` order.
/// Knowledge = what the sidecars list (see [`known_chunk_ids`]).
pub fn missing_chunks(root: &Path, wanted: &[ChunkId]) -> Result<Vec<ChunkId>> {
    let known = known_chunk_ids(root)?;
    Ok(wanted
        .iter()
        .filter(|id| !known.contains(*id))
        .copied()
        .collect())
}

/// Does the store hold this chunk? True iff some sidecar lists it — the
/// store then has a blob containing those bytes and can produce the chunk
/// (from a pack, or by re-cutting the v1 blob).
pub fn has_chunk(root: &Path, id: &ChunkId) -> bool {
    known_chunk_ids(root)
        .map(|k| k.contains(id))
        .unwrap_or(false)
}

/// Rebuild the full content of a chunked blob from its packs and verify it
/// against the blob hash. This is the round-trip guarantee: the chunk layer
/// never loses information the v1 blob file carries.
pub fn reassemble(root: &Path, blob_hash: &str) -> Result<Vec<u8>> {
    let bytes = fs::read(sidecar_path(root, blob_hash))
        .map_err(|_| StoreError::Chunk(format!("blob {blob_hash} has no chunk sidecar")))?;
    let sc = match AnySidecar::decode(&bytes) {
        Some(AnySidecar::Full(sc)) => sc,
        Some(AnySidecar::Lean(_)) => {
            return Err(StoreError::Chunk(format!(
                "blob {blob_hash} has a lean sidecar (no packs were written for it) — \
                 reassembly is impossible; the v1 blob file must exist"
            )));
        }
        None => {
            return Err(StoreError::Chunk(format!(
                "corrupt chunk sidecar for blob {blob_hash}"
            )));
        }
    };
    let mut index = ChunkIndex::scan(root)?;
    let mut handles: HashMap<String, fs::File> = HashMap::new();
    let mut out = Vec::new();
    for (id, slot) in sc.chunks.iter().zip(&sc.slots) {
        let name = hex::encode(sc.packs[*slot as usize]);
        let Some((off, clen, ulen)) = index.find_in(&name, id)? else {
            return Err(StoreError::Chunk(format!(
                "chunk {} of blob {blob_hash} missing from pack-{name}",
                hex::encode(id)
            )));
        };
        let path = pack_path(root, &name);
        let file = match handles.get_mut(&name) {
            Some(f) => f,
            None => {
                let f = fs::File::open(&path).map_err(|e| io_err(&path, e))?;
                handles.insert(name.clone(), f);
                handles.get_mut(&name).expect("just inserted")
            }
        };
        file.seek(SeekFrom::Start(off))
            .map_err(|e| io_err(&path, e))?;
        let mut frame = vec![0u8; clen as usize];
        file.read_exact(&mut frame).map_err(|e| io_err(&path, e))?;
        let raw = zstd::bulk::decompress(&frame, ulen as usize)
            .map_err(|e| StoreError::Chunk(format!("zstd decompress: {e}")))?;
        out.extend_from_slice(&raw);
    }
    let actual = blake3::hash(&out).to_hex().to_string();
    if actual != blob_hash {
        return Err(StoreError::HashMismatch {
            expected: blob_hash.to_string(),
            actual,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// GC & stats
// ---------------------------------------------------------------------------

/// Delete sidecars of blobs that no longer exist and packs no surviving
/// sidecar references. Returns (files deleted, bytes freed). Works in all
/// three modes: lean sidecars reference no packs, so in a lean store any
/// leftover pack (from a previous transfer era) becomes unreferenced and is
/// reclaimed. In packs-only mode this deliberately never touches the packs
/// of *live* sidecars: a blob evicted from the v1 cache keeps its chunks.
/// Chunk-level reference counting stays a future step; this reachability
/// sweep is exact for whole packs, which is the granularity the format
/// controls.
pub fn gc_sweep(root: &Path, live_blobs: &HashSet<String>) -> Result<(u64, u64)> {
    let mut deleted = 0u64;
    let mut freed = 0u64;
    let mut referenced: HashSet<String> = HashSet::new();

    let chunks_dir = root.join("chunks");
    if chunks_dir.exists() {
        for entry in fs::read_dir(&chunks_dir).map_err(|e| io_err(&chunks_dir, e))? {
            let entry = entry.map_err(|e| io_err(&chunks_dir, e))?;
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(blob) = name.strip_suffix(".chunks") else {
                continue;
            };
            if !live_blobs.contains(blob) {
                let meta = entry.metadata().map_err(|e| io_err(&entry.path(), e))?;
                fs::remove_file(entry.path()).map_err(|e| io_err(&entry.path(), e))?;
                deleted += 1;
                freed += meta.len();
                continue;
            }
            if let Ok(bytes) = fs::read(entry.path()) {
                if let Some(sc) = AnySidecar::decode(&bytes) {
                    for p in sc.pack_refs() {
                        referenced.insert(hex::encode(p));
                    }
                }
            }
        }
    }

    let packs_dir = root.join("packs");
    if packs_dir.exists() {
        // Sum the three files of each pack, then drop unreferenced packs.
        let mut stems: HashMap<String, u64> = HashMap::new();
        for entry in fs::read_dir(&packs_dir).map_err(|e| io_err(&packs_dir, e))? {
            let entry = entry.map_err(|e| io_err(&packs_dir, e))?;
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(stem) = name.strip_prefix("pack-").and_then(|s| {
                s.strip_suffix(".zst")
                    .or_else(|| s.strip_suffix(".idx"))
                    .or_else(|| s.strip_suffix(".bloom"))
            }) else {
                continue;
            };
            let len = entry
                .metadata()
                .map_err(|e| io_err(&entry.path(), e))?
                .len();
            *stems.entry(stem.to_string()).or_insert(0) += len;
        }
        for (stem, bytes) in stems {
            if referenced.contains(&stem) {
                continue;
            }
            for ext in ["zst", "idx", "bloom"] {
                let p = packs_dir.join(format!("pack-{stem}.{ext}"));
                let _ = fs::remove_file(&p);
            }
            deleted += 1;
            freed += bytes;
        }
    }
    Ok((deleted, freed))
}

/// Physical statistics of the chunk layer (for reporting and measurements).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ChunkStats {
    pub packs: u64,
    /// Bytes on disk of packs + indexes + blooms.
    pub pack_disk_bytes: u64,
    /// Distinct chunks stored across all packs.
    pub unique_chunks: u64,
    /// Logical (uncompressed) bytes of those chunks.
    pub unique_chunk_bytes: u64,
    pub sidecars: u64,
    pub sidecar_bytes: u64,
}

pub fn chunk_stats(root: &Path) -> Result<ChunkStats> {
    let mut stats = ChunkStats::default();
    let packs_dir = root.join("packs");
    if packs_dir.exists() {
        let mut counted: HashSet<String> = HashSet::new();
        for entry in fs::read_dir(&packs_dir).map_err(|e| io_err(&packs_dir, e))? {
            let entry = entry.map_err(|e| io_err(&packs_dir, e))?;
            let name = entry.file_name().to_string_lossy().to_string();
            stats.pack_disk_bytes += entry
                .metadata()
                .map_err(|e| io_err(&entry.path(), e))?
                .len();
            if let Some(idx) = name
                .strip_suffix(".idx")
                .and_then(|s| s.strip_prefix("pack-"))
            {
                if counted.insert(idx.to_string()) {
                    stats.packs += 1;
                    if let Ok(bytes) = fs::read(entry.path()) {
                        if let Some(pi) = PackIdx::from_bytes(&bytes) {
                            stats.unique_chunks += pi.count as u64;
                            stats.unique_chunk_bytes += pi.logical_bytes;
                        }
                    }
                }
            }
        }
    }
    let chunks_dir = root.join("chunks");
    if chunks_dir.exists() {
        for entry in fs::read_dir(&chunks_dir).map_err(|e| io_err(&chunks_dir, e))? {
            let entry = entry.map_err(|e| io_err(&chunks_dir, e))?;
            stats.sidecars += 1;
            stats.sidecar_bytes += entry
                .metadata()
                .map_err(|e| io_err(&entry.path(), e))?
                .len();
        }
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    /// Deterministic byte generator (tests never touch ambient randomness).
    struct Rng(u64);

    impl Rng {
        fn next_u64(&mut self) -> u64 {
            let mut z = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            self.0 = z;
            z ^ (z >> 31)
        }

        fn bytes(&mut self, len: usize) -> Vec<u8> {
            (0..len).map(|_| (self.next_u64() >> 32) as u8).collect()
        }
    }

    #[test]
    fn bloom_has_no_false_negatives_and_bounded_fp() {
        let n = 10_000;
        let mut bloom = Bloom::new(n, BLOOM_TARGET_FP);
        let mut present: Vec<ChunkId> = Vec::with_capacity(n);
        let mut rng = Rng(0xb100);
        for _ in 0..n {
            let mut id = [0u8; 32];
            rng_fill(&mut rng, &mut id);
            bloom.add(&id);
            present.push(id);
        }
        for id in &present {
            assert!(bloom.might_contain(id), "false negative");
        }
        // Absent ids (continuation of the same stream): measure the FP rate.
        let probes = 100_000;
        let mut fps = 0;
        for _ in 0..probes {
            let mut id = [0u8; 32];
            rng_fill(&mut rng, &mut id);
            if bloom.might_contain(&id) {
                fps += 1;
            }
        }
        let rate = fps as f64 / probes as f64;
        assert!(
            rate < BLOOM_TARGET_FP * 4.0,
            "false-positive rate {rate} far above target {BLOOM_TARGET_FP}"
        );
    }

    fn rng_fill(rng: &mut Rng, buf: &mut [u8; 32]) {
        for chunk in buf.chunks_mut(8) {
            chunk.copy_from_slice(&rng.next_u64().to_le_bytes()[..chunk.len()]);
        }
    }

    #[test]
    fn bloom_serialization_roundtrip() {
        let mut bloom = Bloom::new(100, 0.01);
        let mut rng = Rng(7);
        let mut ids = Vec::new();
        for _ in 0..100 {
            let mut id = [0u8; 32];
            rng_fill(&mut rng, &mut id);
            bloom.add(&id);
            ids.push(id);
        }
        let back = Bloom::from_bytes(&bloom.to_bytes()).expect("parse");
        for id in &ids {
            assert_eq!(bloom.might_contain(id), back.might_contain(id));
        }
        assert!(Bloom::from_bytes(b"garbage").is_none());
    }

    fn chunked_store(tag: &str) -> (tempfile::TempDir, Store) {
        mode_store(tag, ChunkMode::Transfer)
    }

    fn mode_store(tag: &str, mode: ChunkMode) -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open_chunked_with_mode(dir.path().join(tag), mode).expect("open");
        (dir, store)
    }

    /// Path of the v1 blob file inside the store (tests only).
    fn object_file(store: &Store, hash: &str) -> std::path::PathBuf {
        store.root().join("objects").join(&hash[..2]).join(hash)
    }

    /// 256 KiB with realistic structure: random head, a repeated middle, and
    /// a tail copied from the head with a small edit.
    fn structured_blob(seed: u64) -> Vec<u8> {
        let mut rng = Rng(seed);
        let head = rng.bytes(96 * 1024);
        let mut data = head.clone();
        let repeated = rng.bytes(4 * 1024);
        for _ in 0..16 {
            data.extend_from_slice(&repeated);
        }
        let mut tail = head[..64 * 1024].to_vec();
        tail[..64].copy_from_slice(&rng.bytes(64));
        data.extend_from_slice(&tail);
        data
    }

    #[test]
    fn chunk_roundtrip_reassembles_original() {
        let (_d, store) = chunked_store("rt");
        let data = structured_blob(1);
        let (hash, size) = store.put_blob(&mut &data[..]).unwrap();
        assert_eq!(size, data.len() as u64);
        assert!(store.blob_chunks(&hash).unwrap().is_some());
        let ids = store.blob_chunks(&hash).unwrap().unwrap();
        assert!(!ids.is_empty());
        for id in &ids {
            assert!(store.has_chunk(id), "chunk should be present");
        }
        assert_eq!(store.reassemble_blob(&hash).unwrap(), data);
    }

    #[test]
    fn small_blob_is_not_chunked() {
        let (_d, store) = chunked_store("small");
        let (hash, _) = store.put_blob(&mut &vec![7u8; 1024][..]).unwrap();
        assert!(store.blob_chunks(&hash).unwrap().is_none());
        assert!(!store
            .root()
            .join("packs")
            .join("pack-does-not-exist.zst")
            .exists());
    }

    #[test]
    fn second_similar_blob_needs_few_new_chunks() {
        let (_d, store) = chunked_store("sim");
        let a = structured_blob(2);
        let b = {
            // Same blob with a 64-byte edit in the middle and a short append:
            // FastCDC must keep almost every chunk identical.
            let mut b = a.clone();
            let mid = b.len() / 2;
            for byte in &mut b[mid..mid + 64] {
                *byte ^= 0x5a;
            }
            b.extend_from_slice(&vec![9u8; 8192]);
            b
        };
        let (ha, _) = store.put_blob(&mut &a[..]).unwrap();
        let before = chunk_stats(store.root()).unwrap();

        // Before ingesting B: its chunk list is almost fully covered.
        let wanted = myc_chunk::chunk_hashes(&b);
        let missing = store.missing_chunks(&wanted);
        assert!(
            missing.len() <= 8,
            "a 64-byte edit should invalidate only a couple of chunks, got {}",
            missing.len()
        );
        let missing_bytes: u64 = {
            // Bytes the hub would have to transfer for B.
            let bounds = myc_chunk::chunk_bounds(&b);
            let set: HashSet<ChunkId> = missing.iter().copied().collect();
            bounds
                .iter()
                .filter(|&&(s, e)| set.contains(blake3::hash(&b[s..e]).as_bytes()))
                .map(|&(s, e)| (e - s) as u64)
                .sum()
        };
        assert!(
            missing_bytes < b.len() as u64 / 4,
            "edited blob should transfer <25% of its bytes, got {missing_bytes}/{}",
            b.len()
        );

        let (hb, _) = store.put_blob(&mut &b[..]).unwrap();
        assert_ne!(ha, hb);
        let after = chunk_stats(store.root()).unwrap();
        let new_unique = after.unique_chunk_bytes - before.unique_chunk_bytes;
        assert!(new_unique < b.len() as u64 / 4);
        assert_eq!(store.reassemble_blob(&hb).unwrap(), b);
        // And now nothing of B is missing anymore.
        assert!(store.missing_chunks(&wanted).is_empty());
    }

    #[test]
    fn repetitive_blob_dedups_within_itself() {
        let (_d, store) = chunked_store("rep");
        let data = vec![0u8; 256 * 1024];
        let (hash, _) = store.put_blob(&mut &data[..]).unwrap();
        let ids = store.blob_chunks(&hash).unwrap().unwrap();
        let stats = chunk_stats(store.root()).unwrap();
        assert!(
            (stats.unique_chunks as usize) < ids.len(),
            "constant data must produce duplicate chunks"
        );
        assert!(stats.unique_chunk_bytes < data.len() as u64 / 8);
        assert_eq!(store.reassemble_blob(&hash).unwrap(), data);
    }

    #[test]
    fn reingest_is_idempotent() {
        let (dir, store) = chunked_store("idem");
        let data = structured_blob(3);
        let (hash, _) = store.put_blob(&mut &data[..]).unwrap();
        let before = chunk_stats(store.root()).unwrap();
        // A second handle replays the same blob (the dedup-hit path).
        let other = Store::open_chunked_with_mode(store.root(), ChunkMode::Transfer).unwrap();
        let (h2, _) = other.put_blob(&mut &data[..]).unwrap();
        assert_eq!(hash, h2);
        let after = chunk_stats(store.root()).unwrap();
        assert_eq!(before, after);
        drop(store);
        drop(dir);
    }

    #[test]
    fn gc_keeps_packs_referenced_by_live_sidecars() {
        use myc_manifest::{Entry, EntryKind, Manifest, RuntimeConfig, SCHEMA_VERSION};

        let (_d, store) = chunked_store("gc");
        let a = structured_blob(4);
        let mut b = a.clone();
        b.extend_from_slice(&vec![1u8; 128 * 1024]);
        let (ha, _) = store.put_blob(&mut &a[..]).unwrap();
        let (hb, _) = store.put_blob(&mut &b[..]).unwrap();

        let manifest_for = |hash: &str, name: &str| Manifest {
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
                size: 0,
                blake3: Some(hash.into()),
                target: None,
                device: None,
            }],
        };
        // Only B is live: A dies, but B's sidecar still references chunks
        // living in A's pack — the pack must survive.
        store.put_manifest(&manifest_for(&hb, "app:live")).unwrap();
        let packs_before: Vec<String> = pack_names(store.root());
        store.gc().unwrap();
        assert!(!store.has_blob(&ha));
        assert!(store.has_blob(&hb));
        assert_eq!(store.reassemble_blob(&hb).unwrap(), b);
        let packs_after: Vec<String> = pack_names(store.root());
        assert_eq!(packs_before, packs_after, "live sidecar must pin its packs");
        assert!(!sidecar_path(store.root(), &ha).exists());

        // Drop the last reference: the pack layer empties completely.
        let id = store.list_manifests().unwrap().remove(0).0;
        store.remove_manifest(&id).unwrap();
        store.gc().unwrap();
        assert!(pack_names(store.root()).is_empty());
        assert!(!sidecar_path(store.root(), &hb).exists());
    }

    fn pack_names(root: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(root.join("packs"))
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter_map(|e| {
                        let n = e.file_name().to_string_lossy().to_string();
                        n.strip_suffix(".zst").map(|s| s.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    #[test]
    fn non_chunked_store_writes_no_chunk_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_inner(dir.path().join("s"), false, None).unwrap();
        let (hash, _) = store.put_blob(&mut &structured_blob(5)[..]).unwrap();
        assert!(!store.root().join("packs").exists());
        assert!(!store.root().join("chunks").exists());
        assert!(store.blob_chunks(&hash).unwrap().is_none());
    }

    // ---- disk modes (MYCEL_CHUNK_MODE) ----

    #[test]
    fn chunk_mode_names_round_trip() {
        for mode in [ChunkMode::Transfer, ChunkMode::Lean, ChunkMode::PacksOnly] {
            assert_eq!(ChunkMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(ChunkMode::parse("transfer"), Some(ChunkMode::Transfer));
        assert_eq!(ChunkMode::parse(" lean\n"), Some(ChunkMode::Lean));
        assert_eq!(ChunkMode::parse("packs-only"), Some(ChunkMode::PacksOnly));
        assert_eq!(ChunkMode::parse("turbo"), None);
    }

    #[test]
    fn lean_mode_round_trip_writes_no_packs() {
        let (_d, store) = mode_store("lean", ChunkMode::Lean);
        let data = structured_blob(7);
        let (hash, _) = store.put_blob(&mut &data[..]).unwrap();
        // v1 blob present, sidecar present, ZERO pack bytes on disk.
        assert!(object_file(&store, &hash).exists());
        let ids = store.blob_chunks(&hash).unwrap().expect("lean sidecar");
        assert_eq!(ids, myc_chunk::chunk_hashes(&data));
        let stats = chunk_stats(store.root()).unwrap();
        assert_eq!(stats.packs, 0);
        assert_eq!(stats.pack_disk_bytes, 0);
        assert_eq!(stats.sidecars, 1);
        // Knowledge: everything this blob is made of is known, so a hub
        // would transfer nothing for it again.
        assert!(store.missing_chunks(&ids).is_empty());
        // Lean sidecars cannot reassemble (that's the deal: no packs).
        let err = store.reassemble_blob(&hash).unwrap_err();
        assert!(err.to_string().contains("lean"), "got {err}");
        // Idempotent re-ingest.
        let before = chunk_stats(store.root()).unwrap();
        let other = Store::open_chunked_with_mode(store.root(), ChunkMode::Lean).unwrap();
        let (h2, _) = other.put_blob(&mut &data[..]).unwrap();
        assert_eq!(hash, h2);
        assert_eq!(chunk_stats(store.root()).unwrap(), before);
    }

    #[test]
    fn lean_mode_writes_nothing_when_all_chunks_known() {
        let (_d, store) = mode_store("lean2", ChunkMode::Lean);
        let a = structured_blob(8);
        let (ha, _) = store.put_blob(&mut &a[..]).unwrap();
        // B = strict prefix of A cut on a chunk boundary: every chunk of B
        // is already known via A's sidecar, and B is still chunkable.
        let bounds = myc_chunk::chunk_bounds(&a);
        let cut = bounds[bounds.len() - 2].1;
        let b = a[..cut].to_vec();
        assert!(b.len() as u64 >= CHUNK_MIN_BLOB_SIZE);
        let (hb, _) = store.put_blob(&mut &b[..]).unwrap();
        assert_ne!(ha, hb);
        // Nothing new to teach: no sidecar for B, no pack ever.
        assert!(store.blob_chunks(&hb).unwrap().is_none());
        assert!(object_file(&store, &hb).exists()); // v1 blob is the truth
        assert!(store
            .missing_chunks(&myc_chunk::chunk_hashes(&b))
            .is_empty());
        assert_eq!(chunk_stats(store.root()).unwrap().sidecars, 1);
    }

    #[test]
    fn packs_only_round_trip_materializes_on_demand() {
        let (_d, store) = mode_store("po", ChunkMode::PacksOnly);
        let data = structured_blob(9);
        let (hash, _) = store.put_blob(&mut &data[..]).unwrap();
        // Chunked blob: NO v1 file on disk, packs + sidecar are the truth…
        assert!(!object_file(&store, &hash).exists());
        assert!(sidecar_exists(store.root(), &hash));
        assert!(store.has_blob(&hash)); // …but the blob counts as present
        assert!(store.chunk_stats().unwrap().packs > 0);
        // Small files keep their plain v1 blob (nothing else backs them).
        let (small, _) = store.put_blob(&mut &vec![1u8; 4096][..]).unwrap();
        assert!(object_file(&store, &small).exists());
        // On-demand materialization: blob() reassembles, verifies, caches.
        let path = store.blob(&hash).unwrap();
        assert!(path.exists());
        assert_eq!(fs::read(&path).unwrap(), data);
        assert!(crate::blob_cache::is_cached(store.root(), &hash));
        // Second call takes the fast path (file is there now).
        assert_eq!(store.blob(&hash).unwrap(), path);
        store.verify_blob(&hash).unwrap();
    }

    #[test]
    fn packs_only_eviction_never_touches_chunks() {
        use myc_manifest::{Entry, EntryKind, Manifest, RuntimeConfig, SCHEMA_VERSION};

        let (_d, store) = mode_store("poev", ChunkMode::PacksOnly);
        let data = structured_blob(10);
        let (hash, _) = store.put_blob(&mut &data[..]).unwrap();
        store
            .put_manifest(&Manifest {
                schema: SCHEMA_VERSION,
                name: "app:live".into(),
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
                    size: data.len() as u64,
                    blake3: Some(hash.clone()),
                    target: None,
                    device: None,
                }],
            })
            .unwrap();
        // Materialize once, then shrink the budget to nothing.
        let path = store.blob(&hash).unwrap();
        assert!(path.exists());
        let (evicted, _) = store.evict_blob_cache_with_budget(0).unwrap();
        assert_eq!(evicted, 0, "live blobs are protected");
        assert!(path.exists());

        // Unprotected path: a blob no manifest references gets evicted,
        // and its chunks/packs survive — it can be rebuilt.
        let (hash2, _) = store.put_blob(&mut &structured_blob(11)[..]).unwrap();
        let p2 = store.blob(&hash2).unwrap();
        assert!(p2.exists());
        let (evicted, _) = store.evict_blob_cache_with_budget(0).unwrap();
        assert_eq!(evicted, 1);
        assert!(!p2.exists());
        assert!(sidecar_exists(store.root(), &hash2));
        assert!(store.chunk_stats().unwrap().packs > 0);
        assert!(store.has_blob(&hash2));
        // …and rebuilt on demand:
        assert_eq!(
            fs::read(store.blob(&hash2).unwrap()).unwrap(),
            structured_blob(11)
        );
        // GC with a live manifest keeps the packs-backed blob alive.
        store.gc().unwrap();
        assert!(store.has_blob(&hash));
    }

    #[test]
    fn mode_switch_refused_until_blobs_are_regenerated() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("s");
        {
            let store = Store::open_chunked_with_mode(&root, ChunkMode::PacksOnly).unwrap();
            let (hash, _) = store.put_blob(&mut &structured_blob(12)[..]).unwrap();
            assert!(!object_file(&store, &hash).exists()); // packs-only shape
        }
        // Switching away while a blob exists only as packs: refused, with
        // regeneration instructions.
        for mode in [ChunkMode::Lean, ChunkMode::Transfer] {
            let err = match Store::open_chunked_with_mode(&root, mode) {
                Err(e) => e.to_string(),
                Ok(_) => panic!("switch to {:?} should have been refused", mode),
            };
            assert!(err.contains("cannot switch chunk mode"), "got {err}");
            assert!(err.contains("myc run"), "regeneration hint missing: {err}");
        }
        // Same mode still opens fine.
        Store::open_chunked_with_mode(&root, ChunkMode::PacksOnly).unwrap();
        // Regenerate (any use materializes + caches), then the switch is
        // accepted.
        {
            let store = Store::open_chunked_with_mode(&root, ChunkMode::PacksOnly).unwrap();
            // Materialize every packs-only blob: scan sidecars directly.
            for entry in fs::read_dir(store.root().join("chunks")).unwrap() {
                let name = entry.unwrap().file_name().to_string_lossy().to_string();
                if let Some(h) = name.strip_suffix(".chunks") {
                    store.blob(h).unwrap();
                }
            }
        }
        Store::open_chunked_with_mode(&root, ChunkMode::Lean).unwrap();
    }
}
