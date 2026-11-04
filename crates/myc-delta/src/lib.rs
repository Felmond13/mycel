//! Rsync-style binary delta encoding for similar-but-rewritten files.
//!
//! Block-level deduplication (the `myc-chunk` CDC prototype) only shares
//! *identical* blocks: two versions of a binary that rewrite most bytes at
//! the 4 KiB granularity share almost nothing (measured: alpine 3.20 vs 3.21
//! busybox, 2 % shared). This crate prototypes the layer above: given an old
//! file (the *source*) and a new similar file (the *target*), produce a
//! compact *delta* that references the parts of the source reappearing
//! anywhere in the target and stores the rest as literals.
//!
//! Algorithm (rsync, Tridgell 1999), pure safe Rust, no new dependencies:
//!
//! 1. the source is split into fixed [`BLOCK_SIZE`] (4 KiB) blocks; every
//!    *full* block is indexed by a weak rolling hash (buzhash, a cyclic
//!    polynomial with a compile-time table) and confirmed by BLAKE3;
//! 2. a 4 KiB buzhash window slides over the target one byte at a time. On a
//!    weak-hash hit confirmed by BLAKE3, a `COPY` of the matched source block
//!    is emitted and the scan resumes *after* the match; otherwise the byte
//!    left behind becomes a literal;
//! 3. the delta is a flat little-endian instruction stream:
//!
//! ```text
//! magic "MYDLT" (5 o) | version u8 | instructions…
//! COPY (tag 1) : u64 offset source | u32 longueur
//! LIT  (tag 2) : u32 longueur | octets bruts
//! ```
//!
//! Because the target window slides at *every* offset, content that merely
//! *shifted* between versions is still matched — exactly the case fixed
//! 4 KiB boundaries miss.
//!
//! Expected complexity O(source + target) time, O(source) memory for the
//! index. The weak hash only *locates* candidates; BLAKE3 decides, so
//! false positives cost one extra hash and can never corrupt output:
//! `apply(source, delta(source, target)) == target` always holds.
//!
//! Literals are stored uncompressed on purpose: compressing them needs a
//! compression library (zstd/deflate), out of scope for this prototype; a
//! deployment could compress the whole delta stream at the transport layer.
//!
//! ```
//! let source = b"The quick brown fox jumps over the lazy dog";
//! let target = b"The quick MYCEL fox jumps over the lazy dog";
//! let d = myc_delta::delta(source, target);
//! assert_eq!(myc_delta::apply(source, &d).unwrap(), target);
//! ```

use std::collections::HashMap;

use thiserror::Error;

/// Source-side block size, in bytes. 4 KiB matches the average chunk size
/// of the CDC prototype (`myc-chunk`), keeping per-block metadata ~0.1 % of
/// payload. Only *full* blocks are indexed: the trailing source bytes (fewer
/// than [`BLOCK_SIZE`]) can never be matched by a full-size sliding window
/// and are re-sent as literals if they changed.
pub const BLOCK_SIZE: usize = 4096;

/// The buzhash roll below assumes the window is a whole number of 64-byte
/// rotations; 4096 % 64 == 0 keeps the leaving byte's contribution unrotated.
const _: () = assert!(BLOCK_SIZE.is_multiple_of(64));

/// Delta format magic and version.
const MAGIC: [u8; 5] = *b"MYDLT";
const VERSION: u8 = 1;
const TAG_COPY: u8 = 1;
const TAG_LIT: u8 = 2;

/// Cheap pre-filter for weak hashes: 2^16 bits marking every weak hash
/// present in the source index. The sliding loop tests one bit per byte;
/// the full-width HashMap lookup only happens on a hit, and BLAKE3 only on
/// a true 32-bit collision.
const BLOOM_BITS: usize = 1 << 16;

/// Seed of the buzhash table generator (SplitMix64, same generator and
/// determinism rules as `myc-chunk`): the table is baked at compile time and
/// must never change, because changing it re-hashes every index entry.
const BUZ_SEED: u64 = 0x6d79_6365_6c64_6c74;

/// One SplitMix64 step, usable in `const` contexts.
const fn splitmix64(state: u64) -> (u64, u64) {
    let state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    (state, z ^ (z >> 31))
}

/// The 256-entry buzhash table: one pseudo-random 32-bit addend per byte
/// value, generated at compile time from [`BUZ_SEED`].
const BUZ: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut state = BUZ_SEED;
    let mut i = 0;
    while i < 256 {
        let (next, value) = splitmix64(state);
        state = next;
        table[i] = (value >> 32) as u32;
        i += 1;
    }
    table
};

/// Weak hash of a whole window (buzhash / cyclic polynomial): each byte
/// contributes its table entry rotated by its distance to the window end.
fn buzhash(data: &[u8]) -> u32 {
    let mut h: u32 = 0;
    for &b in data {
        h = h.rotate_left(1) ^ BUZ[b as usize];
    }
    h
}

/// Slide a [`BLOCK_SIZE`]-byte window one byte right: drop `out` (leaving on
/// left), append `add`. Valid because `BLOCK_SIZE % 64 == 0`: after 4096
/// rotations the leaving byte's entry is back to its unrotated value.
fn buzhash_roll(h: u32, out: u8, add: u8) -> u32 {
    h.rotate_left(1) ^ BUZ[out as usize] ^ BUZ[add as usize]
}

/// Mark `h` as present in the bloom filter.
fn bloom_set(bloom: &mut [u64], h: u32) {
    let bit = h as usize & (BLOOM_BITS - 1);
    bloom[bit >> 6] |= 1u64 << (bit & 63);
}

/// Test whether `h` may be present in the bloom filter.
fn bloom_test(bloom: &[u64], h: u32) -> bool {
    let bit = h as usize & (BLOOM_BITS - 1);
    (bloom[bit >> 6] >> (bit & 63)) & 1 == 1
}

/// Encode `source` (old) against `target` (new) into a delta stream.
///
/// The returned delta is self-delimiting and safe to store; applying it with
/// [`apply`] on the same `source` always reconstructs `target` exactly. See
/// the crate docs for the algorithm and the binary format.
pub fn delta(source: &[u8], target: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(MAGIC.len() + 1 + target.len() / 8);
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);

    let nblocks = source.len() / BLOCK_SIZE;
    if nblocks == 0 {
        // Source smaller than one block: nothing can match, all literal.
        emit_lit(&mut out, target);
        return out;
    }

    // 1. Index every full source block: weak hash -> block ids (a weak hash
    //    may map to several blocks), plus the BLAKE3 digest used to confirm.
    let mut index: HashMap<u32, Vec<u32>> = HashMap::with_capacity(nblocks * 2);
    let mut strong: Vec<blake3::Hash> = Vec::with_capacity(nblocks);
    let mut bloom = vec![0u64; BLOOM_BITS / 64];
    for i in 0..nblocks {
        let block = &source[i * BLOCK_SIZE..(i + 1) * BLOCK_SIZE];
        let weak = buzhash(block);
        index.entry(weak).or_default().push(i as u32);
        strong.push(blake3::hash(block));
        bloom_set(&mut bloom, weak);
    }

    // Weak-hash hit -> BLAKE3 confirmation over the candidate blocks.
    let confirm = |h: u32, window: &[u8]| -> Option<u32> {
        let ids = index.get(&h)?;
        ids.iter()
            .copied()
            .find(|&i| strong[i as usize] == blake3::hash(window))
    };

    // 2. Slide a full-size window over the target, one byte at a time.
    let mut lit_start = 0usize;
    let mut p = 0usize;
    if target.len() >= BLOCK_SIZE {
        let mut h = buzhash(&target[..BLOCK_SIZE]);
        while p + BLOCK_SIZE <= target.len() {
            if bloom_test(&bloom, h) {
                if let Some(block) = confirm(h, &target[p..p + BLOCK_SIZE]) {
                    emit_lit(&mut out, &target[lit_start..p]);
                    emit_copy(
                        &mut out,
                        (block as usize * BLOCK_SIZE) as u64,
                        BLOCK_SIZE as u32,
                    );
                    p += BLOCK_SIZE;
                    lit_start = p;
                    if p + BLOCK_SIZE > target.len() {
                        break;
                    }
                    h = buzhash(&target[p..p + BLOCK_SIZE]);
                    continue;
                }
            }
            if p + BLOCK_SIZE == target.len() {
                break; // last possible window: cannot slide further
            }
            h = buzhash_roll(h, target[p], target[p + BLOCK_SIZE]);
            p += 1;
        }
    }

    // Whatever follows the last full window (or all of a short target).
    emit_lit(&mut out, &target[lit_start..]);
    out
}

/// Reconstruct the target from `source` and a delta produced by [`delta`].
///
/// # Errors
///
/// Returns a [`DeltaError`] if the delta is truncated, has a bad magic or
/// version, an unknown instruction tag, or a `COPY` pointing outside the
/// source. A delta from this crate applied to the same source never errors.
pub fn apply(source: &[u8], delta: &[u8]) -> Result<Vec<u8>, DeltaError> {
    let mut pos = 0;
    parse_header(delta, &mut pos)?;
    let mut out = Vec::new();
    while pos < delta.len() {
        match next_op(delta, &mut pos)? {
            Op::Copy { src_off, len } => {
                let off =
                    usize::try_from(src_off).map_err(|_| out_of_bounds(src_off, len, source))?;
                let end = off
                    .checked_add(len as usize)
                    .filter(|end| *end <= source.len())
                    .ok_or_else(|| out_of_bounds(src_off, len, source))?;
                out.extend_from_slice(&source[off..end]);
            }
            Op::Lit(bytes) => out.extend_from_slice(bytes),
        }
    }
    Ok(out)
}

/// Instruction counters of a delta stream, for reporting and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeltaStats {
    /// Number of `COPY` instructions and source bytes they cover.
    pub copy_ops: u64,
    pub copy_bytes: u64,
    /// Number of `LIT` instructions and raw bytes they carry.
    pub lit_ops: u64,
    pub lit_bytes: u64,
}

impl DeltaStats {
    /// Share of the reconstructed target covered by `COPY` instructions
    /// (0.0–1.0); 0.0 for an empty target.
    pub fn covered_fraction(&self) -> f64 {
        let total = self.copy_bytes + self.lit_bytes;
        if total == 0 {
            0.0
        } else {
            self.copy_bytes as f64 / total as f64
        }
    }
}

/// Parse a delta stream and tally its instructions.
///
/// # Errors
///
/// Same conditions as [`apply`]: malformed streams are rejected.
pub fn delta_stats(delta: &[u8]) -> Result<DeltaStats, DeltaError> {
    let mut pos = 0;
    parse_header(delta, &mut pos)?;
    let mut stats = DeltaStats::default();
    while pos < delta.len() {
        match next_op(delta, &mut pos)? {
            Op::Copy { len, .. } => {
                stats.copy_ops += 1;
                stats.copy_bytes += len as u64;
            }
            Op::Lit(bytes) => {
                stats.lit_ops += 1;
                stats.lit_bytes += bytes.len() as u64;
            }
        }
    }
    Ok(stats)
}

/// One decoded instruction of the stream.
enum Op<'a> {
    Copy { src_off: u64, len: u32 },
    Lit(&'a [u8]),
}

/// Read and validate the `MYDLT` header, advancing `pos` past it.
fn parse_header(delta: &[u8], pos: &mut usize) -> Result<(), DeltaError> {
    let magic = take(delta, pos, MAGIC.len())?;
    if magic != &MAGIC[..] {
        return Err(DeltaError::BadMagic {
            found: magic.try_into().unwrap_or([0; 5]),
        });
    }
    let version = take(delta, pos, 1)?[0];
    if version != VERSION {
        return Err(DeltaError::UnsupportedVersion(version));
    }
    Ok(())
}

/// Decode the instruction at `pos`, advancing past it.
fn next_op<'a>(delta: &'a [u8], pos: &mut usize) -> Result<Op<'a>, DeltaError> {
    let tag = take(delta, pos, 1)?[0];
    match tag {
        TAG_COPY => Ok(Op::Copy {
            src_off: read_u64(delta, pos)?,
            len: read_u32(delta, pos)?,
        }),
        TAG_LIT => {
            let len = read_u32(delta, pos)? as usize;
            Ok(Op::Lit(take(delta, pos, len)?))
        }
        other => Err(DeltaError::UnknownTag(other)),
    }
}

/// Borrow `n` bytes at `pos`, advancing it; error if the stream is short.
fn take<'a>(delta: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8], DeltaError> {
    let end = *pos + n;
    if end > delta.len() {
        return Err(DeltaError::Truncated {
            at: *pos,
            need: n,
            have: delta.len() - *pos,
        });
    }
    let bytes = &delta[*pos..end];
    *pos = end;
    Ok(bytes)
}

fn read_u32(delta: &[u8], pos: &mut usize) -> Result<u32, DeltaError> {
    let bytes = take(delta, pos, 4)?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

fn read_u64(delta: &[u8], pos: &mut usize) -> Result<u64, DeltaError> {
    let bytes = take(delta, pos, 8)?;
    Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
}

/// Emit a `LIT` instruction (skipped when empty, chunked at u32 length).
fn emit_lit(out: &mut Vec<u8>, bytes: &[u8]) {
    let mut rest = bytes;
    while !rest.is_empty() {
        let n = rest.len().min(u32::MAX as usize);
        out.push(TAG_LIT);
        out.extend_from_slice(&(n as u32).to_le_bytes());
        out.extend_from_slice(&rest[..n]);
        rest = &rest[n..];
    }
}

/// Emit a `COPY` instruction referencing `src_off..src_off + len` of the source.
fn emit_copy(out: &mut Vec<u8>, src_off: u64, len: u32) {
    out.push(TAG_COPY);
    out.extend_from_slice(&src_off.to_le_bytes());
    out.extend_from_slice(&len.to_le_bytes());
}

/// Shorthand for the `COPY`-out-of-bounds error used twice by [`apply`].
fn out_of_bounds(src_off: u64, len: u32, source: &[u8]) -> DeltaError {
    DeltaError::CopyOutOfBounds {
        src_off,
        len,
        source_len: source.len(),
    }
}

/// Failures of [`apply`] and [`delta_stats`] on malformed deltas.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum DeltaError {
    /// The stream ends inside a field or instruction.
    #[error("delta tronquée à l'offset {at} : {need} octets attendus, {have} disponibles")]
    Truncated {
        /// Offset of the incomplete field.
        at: usize,
        /// Bytes the field needs.
        need: usize,
        /// Bytes actually left.
        have: usize,
    },
    /// The 5-byte magic is not `MYDLT`.
    #[error("magic invalide {found:02x?} (attendu 4d59444c54)")]
    BadMagic {
        /// The magic bytes read.
        found: [u8; 5],
    },
    /// Unknown format version.
    #[error("version de delta non supportée : {0}")]
    UnsupportedVersion(u8),
    /// Unknown instruction tag.
    #[error("tag d'instruction inconnu : {0}")]
    UnknownTag(u8),
    /// `COPY` points past the end of the source.
    #[error("COPY hors bornes : offset {src_off}, longueur {len}, source de {source_len} octets")]
    CopyOutOfBounds {
        /// Source offset requested by the instruction.
        src_off: u64,
        /// Length requested by the instruction.
        len: u32,
        /// Actual source length.
        source_len: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic byte generator (SplitMix64): tests never depend on
    /// ambient randomness.
    struct Rng(u64);

    impl Rng {
        fn next_u64(&mut self) -> u64 {
            let (state, z) = splitmix64(self.0);
            self.0 = state;
            z
        }

        fn bytes(&mut self, len: usize) -> Vec<u8> {
            (0..len).map(|_| (self.next_u64() >> 32) as u8).collect()
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next_u64() % n as u64) as usize
        }
    }

    #[test]
    fn round_trip_empty_and_tiny() {
        // vide -> vide : en-tête seul
        let d = delta(b"", b"");
        assert_eq!(d.len(), MAGIC.len() + 1);
        assert_eq!(apply(b"", &d).unwrap(), b"");
        // source vide : tout littéral
        let d = delta(b"", b"hello");
        assert_eq!(apply(b"", &d).unwrap(), b"hello");
        let s = delta_stats(&d).unwrap();
        assert_eq!((s.lit_bytes, s.lit_ops, s.copy_bytes), (5, 1, 0));
        // cible vide
        let d = delta(b"hello", b"");
        assert_eq!(apply(b"hello", &d).unwrap(), b"");
        // tous deux plus petits qu'un bloc
        let d = delta(b"abc", b"abd");
        assert_eq!(apply(b"abc", &d).unwrap(), b"abd");
        assert_eq!(delta_stats(&d).unwrap().copy_bytes, 0);
    }

    #[test]
    fn round_trip_identical_copies_everything() {
        let mut rng = Rng(0xdead_beef);
        let data = rng.bytes(100_000); // 24 blocs pleins + queue de 1 696 o
        let d = delta(&data, &data);
        assert_eq!(apply(&data, &d).unwrap(), data);
        let s = delta_stats(&d).unwrap();
        assert_eq!(s.copy_ops, 24);
        assert_eq!(s.copy_bytes, (24 * BLOCK_SIZE) as u64);
        assert_eq!(s.lit_bytes, (100_000 - 24 * BLOCK_SIZE) as u64);
        // ~13 o par COPY + queue littérale + en-tête : delta ~2 % du fichier
        assert!(d.len() < 4096, "delta étonnamment grosse : {}", d.len());
    }

    #[test]
    fn round_trip_insertions() {
        let mut rng = Rng(0x1234_5678);
        let base = rng.bytes(96_000);
        let mut t = Vec::new();
        t.extend_from_slice(&base[..10_000]);
        t.extend(rng.bytes(1024));
        t.extend_from_slice(&base[10_000..50_000]);
        t.extend(rng.bytes(1024));
        t.extend_from_slice(&base[50_000..]);
        let d = delta(&base, &t);
        assert_eq!(apply(&base, &d).unwrap(), t);
        let s = delta_stats(&d).unwrap();
        // les 1024 octets insérés + les fenêtres traversées restent LIT,
        // tout le reste est copié
        assert!(s.copy_bytes >= 80_000, "trop peu de MATCH : {s:?}");
    }

    #[test]
    fn round_trip_deletions() {
        let mut rng = Rng(0x9abc_def0);
        let base = rng.bytes(96_000);
        let mut t = Vec::new();
        t.extend_from_slice(&base[..5_000]);
        t.extend_from_slice(&base[6_000..40_000]);
        t.extend_from_slice(&base[41_000..]);
        let d = delta(&base, &t);
        assert_eq!(apply(&base, &d).unwrap(), t);
        let s = delta_stats(&d).unwrap();
        assert!(s.covered_fraction() > 0.85, "trop peu de MATCH : {s:?}");
    }

    #[test]
    fn round_trip_shift_by_one_byte() {
        // LE contre-exemple des blocs fixes : un octet en tête décale toutes
        // les bornes 4 Kio, mais la fenêtre glissante recolle partout.
        let mut rng = Rng(0x5a5a_5a5a);
        let base = rng.bytes(300_000); // 73 blocs pleins
        let mut t = vec![0x5a];
        t.extend_from_slice(&base);
        let d = delta(&base, &t);
        assert_eq!(apply(&base, &d).unwrap(), t);
        let s = delta_stats(&d).unwrap();
        assert!(
            s.copy_bytes >= 299_000,
            "shift d'un octet mal absorbé : {s:?}"
        );
    }

    #[test]
    fn round_trip_random_edits_seeded() {
        let mut rng = Rng(0xc0de_5eed);
        for _round in 0..40 {
            let len = 1_000 + rng.below(20_000);
            let base = rng.bytes(len);
            let mut t = base.clone();
            for _ in 0..(3 + rng.below(8)) {
                match rng.below(3) {
                    0 => {
                        let at = rng.below(t.len());
                        let end = (at + 1 + rng.below(2_000)).min(t.len());
                        for b in &mut t[at..end] {
                            *b ^= 0x5a;
                        }
                    }
                    1 => {
                        let at = rng.below(t.len() + 1);
                        let span = 1 + rng.below(2_000);
                        t.splice(at..at, rng.bytes(span));
                    }
                    _ => {
                        if t.len() > 10 {
                            let at = rng.below(t.len() - 1);
                            let end = (at + 1 + rng.below(2_000)).min(t.len());
                            t.drain(at..end);
                        }
                    }
                }
            }
            let d = delta(&base, &t);
            assert_eq!(apply(&base, &d).unwrap(), t);
            delta_stats(&d).unwrap();
        }
    }

    #[test]
    fn unrelated_data_is_all_literal() {
        let mut rng = Rng(0x1337_1337);
        let a = rng.bytes(20_000);
        let b = rng.bytes(20_000);
        let d = delta(&a, &b);
        assert_eq!(apply(&a, &d).unwrap(), b);
        let s = delta_stats(&d).unwrap();
        assert_eq!((s.copy_bytes, s.lit_ops), (0, 1));
        assert_eq!(s.lit_bytes, 20_000);
        // surcoût exact : en-tête + tag + longueur
        assert_eq!(d.len(), 20_000 + MAGIC.len() + 1 + 1 + 4);
    }

    #[test]
    fn large_round_trip_mixed_edits() {
        let mut rng = Rng(0x1a2b_3c4d);
        let base = rng.bytes(1024 * 1024);
        let mut t = base.clone();
        for _ in 0..30 {
            let at = rng.below(t.len() - 128);
            for b in &mut t[at..at + 64] {
                *b ^= 0xa5;
            }
        }
        let d = delta(&base, &t);
        assert_eq!(apply(&base, &d).unwrap(), t);
        let s = delta_stats(&d).unwrap();
        // 30 réécritures locales : ~1 Mo de MATCH, seuls les blocs touchés
        // et la queue < 4 Kio restent littéraux
        assert!(s.covered_fraction() > 0.80, "trop peu de MATCH : {s:?}");
        assert!(d.len() < 200_000, "delta étonnamment grosse : {}", d.len());
    }

    #[test]
    fn buzhash_roll_matches_recompute() {
        let mut rng = Rng(0xb072_0011);
        let data = rng.bytes(3 * BLOCK_SIZE);
        let mut h = buzhash(&data[..BLOCK_SIZE]);
        for p in 1..=(2 * BLOCK_SIZE) {
            h = buzhash_roll(h, data[p - 1], data[p + BLOCK_SIZE - 1]);
            assert_eq!(h, buzhash(&data[p..p + BLOCK_SIZE]), "position {p}");
        }
    }

    #[test]
    fn apply_rejects_malformed_deltas() {
        let good = delta(b"hello", b"world");
        // tronquée à toutes les positions (sauf en-tête seul = delta valide
        // qui décode en sortie vide)
        for cut in 0..good.len() {
            if cut == MAGIC.len() + 1 {
                assert_eq!(apply(b"hello", &good[..cut]).unwrap(), b"");
                continue;
            }
            assert!(
                matches!(
                    apply(b"hello", &good[..cut]),
                    Err(DeltaError::Truncated { .. })
                ),
                "cut {cut}"
            );
        }
        // magic invalide
        let mut d = good.clone();
        d[0] = b'X';
        assert_eq!(
            apply(b"hello", &d),
            Err(DeltaError::BadMagic { found: *b"XYDLT" })
        );
        // version inconnue
        let mut d = good.clone();
        d[5] = 99;
        assert_eq!(apply(b"hello", &d), Err(DeltaError::UnsupportedVersion(99)));
        // tag inconnu
        let mut d = Vec::new();
        d.extend_from_slice(&MAGIC);
        d.push(VERSION);
        d.push(9);
        assert_eq!(apply(b"hello", &d), Err(DeltaError::UnknownTag(9)));
        // COPY hors bornes
        let mut d = Vec::new();
        d.extend_from_slice(&MAGIC);
        d.push(VERSION);
        d.push(TAG_COPY);
        d.extend_from_slice(&100u64.to_le_bytes());
        d.extend_from_slice(&10u32.to_le_bytes());
        assert_eq!(
            apply(b"hello", &d),
            Err(DeltaError::CopyOutOfBounds {
                src_off: 100,
                len: 10,
                source_len: 5
            })
        );
        // delta_stats rejette aussi les flux mal formés
        assert!(delta_stats(&good[..3]).is_err());
    }
}
