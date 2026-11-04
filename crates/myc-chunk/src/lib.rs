//! Content-defined chunking (CDC) for block-level deduplication.
//!
//! Mycel v1 deduplicates whole files only: two files share storage when their
//! BLAKE3 hashes are identical. This crate prototypes the next step — cutting
//! every file into variable-size blocks whose *boundaries depend on the
//! content itself*, so that an edit in the middle of a file only rewrites the
//! blocks it touches, and two similar files (two versions of the same binary,
//! two revisions of the same text) share almost all of their blocks.
//!
//! The algorithm is inspired by FastCDC (Xia et al., 2016): a 64-bit Gear
//! hash slides over the data (`h = (h << 1) + GEAR[byte]`, one table lookup
//! and one add per byte) and a cut is declared when the hash matches a
//! probabilistic pattern. Two phase masks ("normalized chunking") keep the
//! average block size near [`AVG_SIZE`] instead of the long tail a single
//! geometric mask would produce.
//!
//! Chunk boundaries are fully deterministic: the Gear table and both masks
//! are generated at compile time from fixed seeds, so the same bytes always
//! produce the same cut points, across machines and releases.
//!
//! ```
//! let data = b"any file contents";
//! let bounds = myc_chunk::chunk_bounds(data);
//! let hashes = myc_chunk::chunk_hashes(data);
//! assert_eq!(bounds.len(), hashes.len());
//! ```

/// Minimum block size: no cut point is tested below this length. Bounded
/// below by 2 KiB so that per-block metadata (hash + offset) stays negligible.
pub const MIN_SIZE: usize = 2048;

/// Target average block size. The phase masks are tuned around this value:
/// [`MASK_S`] expects a cut every 2^11 bytes, [`MASK_L`] every 2^13, and the
/// phase switch at `MIN_SIZE + (MAX_SIZE - MIN_SIZE) / 2` normalizes the
/// measured mean to ~4.1 KiB (see `tests::sizes_distribute_around_average`).
pub const AVG_SIZE: usize = 4096;

/// Maximum block size: a hard cut is forced here even if no pattern matched.
pub const MAX_SIZE: usize = 16384;

/// Fixed seed for the Gear table generator. Any constant works; it must
/// simply never change, because changing it re-slices every chunk ever
/// stored.
const GEAR_SEED: u64 = 0x6d79_6365_6c30_3100;

/// One SplitMix64 step, usable in `const` contexts.
const fn splitmix64(state: u64) -> (u64, u64) {
    let state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    (state, z ^ (z >> 31))
}

/// The 256-entry Gear table: one pseudo-random 64-bit addend per byte value,
/// generated at compile time from [`GEAR_SEED`]. `h = (h << 1) + GEAR[b]`
/// accumulates a positional hash in which every output bit depends on the
/// last 64 input bytes — the implicit sliding window of the Gear hash.
const GEAR: [u64; 256] = {
    let mut table = [0u64; 256];
    let mut state = GEAR_SEED;
    let mut i = 0;
    while i < 256 {
        let (next, value) = splitmix64(state);
        state = next;
        table[i] = value;
        i += 1;
    }
    table
};

/// Both cut masks, drawn from a single SplitMix64 stream: the first 11
/// distinct scattered bits form [`MASK_S`], then two more complete
/// [`MASK_L`]. This guarantees exact popcounts (11 and 13) with `MASK_S` a
/// strict subset of `MASK_L`.
const MASKS: (u64, u64) = {
    let mut state = 0xfeed_face_00c0_ffee;
    let mut mask_s = 0u64;
    let mut mask_l = 0u64;
    let mut set_s = 0;
    let mut set_l = 0;
    while set_l < 13 {
        let (next, value) = splitmix64(state);
        state = next;
        let bit = 1u64 << (value % 64);
        if mask_l & bit == 0 {
            mask_l |= bit;
            set_l += 1;
            if set_s < 11 {
                mask_s |= bit;
                set_s += 1;
            }
        }
    }
    (mask_s, mask_l)
};

/// Cut mask for the first phase (block lengths in `[MIN_SIZE, CENTER)` where
/// `CENTER = MIN_SIZE + (MAX_SIZE - MIN_SIZE) / 2`). Exactly 11 bits are set,
/// so `(h & MASK_S) == 0` fires with probability 2^-11: the expected run
/// after `MIN_SIZE` is 2048 bytes.
const MASK_S: u64 = MASKS.0;

/// Cut mask for the second phase (block lengths in `[CENTER, MAX_SIZE)`).
/// Exactly 13 bits are set (probability 2^-13, expected run 8192 bytes):
/// rarer than [`MASK_S`], which is what pulls the average back toward
/// [`AVG_SIZE`].
const MASK_L: u64 = MASKS.1;

/// Length at which the search switches from [`MASK_S`] to [`MASK_L`].
const CENTER_SIZE: usize = MIN_SIZE + (MAX_SIZE - MIN_SIZE) / 2;

/// Find the end (exclusive) of the block starting at `start`; the final block
/// of the input may end at `data.len()` short of [`MIN_SIZE`]. Normalized
/// FastCDC search: mask [`MASK_S`] up to `CENTER_SIZE`, then the rarer
/// [`MASK_L`] up to `MAX_SIZE`, then a forced cut.
fn find_cut(data: &[u8], start: usize) -> usize {
    let hard_end = (start + MAX_SIZE).min(data.len());
    if hard_end - start <= MIN_SIZE {
        // Too little data left to reach the minimum: final short block.
        return hard_end;
    }
    let min_end = start + MIN_SIZE;
    let center_end = start + CENTER_SIZE;
    let mut h: u64 = 0;
    let mut i = start;
    while i < hard_end {
        h = (h << 1).wrapping_add(GEAR[data[i] as usize]);
        i += 1;
        if i >= min_end {
            let mask = if i < center_end { MASK_S } else { MASK_L };
            if h & mask == 0 {
                return i;
            }
        }
    }
    hard_end
}

/// Cut `data` into content-defined blocks and return their `(start, end)`
/// bounds, in order. The bounds tile the input exactly: the end of block *n*
/// is the start of block *n+1*, and the last block may be shorter than
/// [`MIN_SIZE`]. Empty input yields no blocks.
///
/// Deterministic: the same bytes always produce the same bounds. Complexity
/// is O(n) time over the input and O(1) space per block decision.
pub fn chunk_bounds(data: &[u8]) -> Vec<(usize, usize)> {
    let mut bounds = Vec::new();
    let mut start = 0;
    while start < data.len() {
        let end = find_cut(data, start);
        bounds.push((start, end));
        start = end;
    }
    bounds
}

/// Cut `data` into content-defined blocks and return the BLAKE3 digest of
/// each block. Two blocks with equal digests have equal content (up to hash
/// collisions), which is the key a block-level store will deduplicate on.
///
/// The digests are raw 32-byte arrays; hex-encode them (e.g. with the `hex`
/// crate) when a textual form is needed.
pub fn chunk_hashes(data: &[u8]) -> Vec<[u8; 32]> {
    chunk_bounds(data)
        .into_iter()
        .map(|(s, e)| *blake3::hash(&data[s..e]).as_bytes())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic byte generator (SplitMix64 again): tests must never
    /// depend on ambient randomness.
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
    }

    #[test]
    fn masks_have_expected_popcounts() {
        // 2^11 and 2^13 cut probabilities normalize the mean to ~AVG_SIZE
        // with the phase switch at CENTER_SIZE.
        assert_eq!(MASK_S.count_ones(), 11);
        assert_eq!(MASK_L.count_ones(), 13);
        assert_eq!(MASK_L & MASK_S, MASK_S);
    }

    #[test]
    fn empty_input_yields_no_chunks() {
        assert!(chunk_bounds(b"").is_empty());
        assert!(chunk_hashes(b"").is_empty());
    }

    #[test]
    fn short_input_is_one_chunk() {
        assert_eq!(chunk_bounds(b"hello world"), vec![(0, 11)]);
        // Exactly MIN_SIZE bytes: the minimum guard returns immediately.
        assert_eq!(chunk_bounds(&vec![7u8; MIN_SIZE]), vec![(0, MIN_SIZE)]);
        // One byte more and a cut becomes possible in the last byte, but the
        // result must stay a clean tiling within the profile either way.
        let data = vec![7u8; MIN_SIZE + 1];
        let bounds = chunk_bounds(&data);
        assert!(bounds.len() <= 2);
        assert_eq!(bounds[0].0, 0);
        assert!(bounds[0].1 >= MIN_SIZE);
        assert_eq!(bounds.last().unwrap().1, data.len());
    }

    #[test]
    fn bounds_tile_the_input() {
        let mut rng = Rng(0xdede_d0d0);
        let data = rng.bytes(300_000);
        let bounds = chunk_bounds(&data);
        assert_eq!(bounds.first().unwrap().0, 0);
        assert_eq!(bounds.last().unwrap().1, data.len());
        for w in bounds.windows(2) {
            assert_eq!(w[0].1, w[1].0, "blocks must be contiguous");
        }
    }

    #[test]
    fn determinism() {
        let mut rng = Rng(42);
        let a = rng.bytes(100_000);
        let b = a.clone();
        assert_eq!(chunk_bounds(&a), chunk_bounds(&b));
        assert_eq!(chunk_hashes(&a), chunk_hashes(&b));
    }

    #[test]
    fn min_max_respected_on_random_data() {
        let mut rng = Rng(0x5eed_0001);
        let data = rng.bytes(1_000_000);
        let bounds = chunk_bounds(&data);
        assert!(bounds.len() > 2);
        for &(s, e) in &bounds[..bounds.len() - 1] {
            let sz = e - s;
            assert!(sz >= MIN_SIZE, "chunk too small: {sz}");
            assert!(sz <= MAX_SIZE, "chunk too big: {sz}");
        }
        let (s, e) = *bounds.last().unwrap();
        assert!(e - s <= MAX_SIZE);
    }

    #[test]
    fn min_max_respected_on_pathological_data() {
        // Constant bytes are the worst case for a Gear hash (the digest
        // follows a fixed recurrence); cuts may fall back to MAX_SIZE but the
        // bounds must still hold.
        for fill in [0u8, 0xff, 0xaa] {
            let data = vec![fill; 100_000];
            for (s, e) in chunk_bounds(&data) {
                let sz = e - s;
                assert!(sz >= MIN_SIZE || e == data.len());
                assert!(sz <= MAX_SIZE);
            }
        }
    }

    #[test]
    fn unshift_stability() {
        // Inserting a single byte at the front must only disturb the blocks
        // around the insertion point: the Gear hash of a position depends
        // only on the preceding bytes, so after the first re-synchronized
        // cut every following block is byte-identical, hence hash-identical.
        let mut rng = Rng(0xf1f1_f1f1);
        let data = rng.bytes(512 * 1024);
        let base_hashes = chunk_hashes(&data);
        let mut shifted = Vec::with_capacity(data.len() + 1);
        shifted.push(0x5a);
        shifted.extend_from_slice(&data);
        let shifted_hashes = chunk_hashes(&shifted);

        let delta = shifted_hashes.len() as isize - base_hashes.len() as isize;
        assert!(
            delta.abs() <= 2,
            "one inserted byte must not reshape the stream: {} -> {} blocks",
            base_hashes.len(),
            shifted_hashes.len()
        );

        // Longest common suffix of the two hash sequences: all but a couple
        // of blocks survive the insertion untouched.
        let mut common_suffix = 0;
        while common_suffix < base_hashes.len()
            && common_suffix < shifted_hashes.len()
            && base_hashes[base_hashes.len() - 1 - common_suffix]
                == shifted_hashes[shifted_hashes.len() - 1 - common_suffix]
        {
            common_suffix += 1;
        }
        assert!(
            common_suffix + 3 >= base_hashes.len(),
            "only {} of {} blocks survived a 1-byte insertion",
            common_suffix,
            base_hashes.len()
        );
    }

    #[test]
    fn sizes_distribute_around_average() {
        let mut rng = Rng(0x51fa_5eed);
        let data = rng.bytes(4 * 1024 * 1024);
        let bounds = chunk_bounds(&data);
        let sizes: Vec<f64> = bounds.iter().map(|(s, e)| (e - s) as f64).collect();
        let mean = sizes.iter().sum::<f64>() / sizes.len() as f64;
        assert!(
            (mean - AVG_SIZE as f64).abs() / (AVG_SIZE as f64) < 0.25,
            "mean block size {mean} drifted from AVG_SIZE {AVG_SIZE}"
        );
        // Cuts must come from the hash, not from the MAX_SIZE backstop.
        let at_max = sizes.iter().filter(|&&s| s == MAX_SIZE as f64).count();
        assert!(
            at_max * 10 < sizes.len(),
            "{at_max} of {} blocks hit the hard MAX_SIZE cut",
            sizes.len()
        );
        // The two-phase normalization keeps blocks spread on both sides of
        // the average instead of piling up at MIN_SIZE or CENTER_SIZE.
        let below_avg = sizes.iter().filter(|&&s| s < AVG_SIZE as f64).count();
        let third = sizes.len() / 3;
        assert!(
            below_avg > third && below_avg < sizes.len() - third,
            "distribution lopsided: {below_avg}/{} blocks below average",
            sizes.len()
        );
    }

    #[test]
    fn edit_in_the_middle_only_touches_local_blocks() {
        // The motivating use case: two versions of a file where a few bytes
        // change in the middle. Nearly every block must be shared.
        let mut rng = Rng(0xe0e0_e0e0);
        let base = rng.bytes(512 * 1024);
        let mut edited = base.clone();
        let mid = edited.len() / 2;
        for b in &mut edited[mid..mid + 64] {
            *b ^= 0x5a;
        }
        let a = chunk_hashes(&base);
        let b_set: std::collections::HashSet<[u8; 32]> =
            chunk_hashes(&edited).into_iter().collect();
        let shared = a.iter().filter(|h| b_set.contains(*h)).count();
        assert!(
            shared + 4 >= a.len(),
            "a 64-byte edit in the middle invalidated {} of {} blocks",
            a.len() - shared,
            a.len()
        );
    }
}
