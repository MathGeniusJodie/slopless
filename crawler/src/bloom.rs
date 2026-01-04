use std::cmp::max;
use std::f64::consts::LN_2;
use std::hash::{BuildHasher, Hash, Hasher};

/// A bit vector partitioned into u64 blocks for efficient storage and access
/// Used as the underlying storage for the Bloom filter
#[derive(Debug, Clone)]
pub struct BitVec {
    bits: Box<[u64]>, // Each u64 stores 64 bits
}

impl BitVec {
    #[inline]
    pub fn new(num_bits: usize) -> Self {
        let num_u64s = (num_bits + 63) / 64;
        Self {
            bits: vec![0u64; num_u64s].into_boxed_slice(),
        }
    }

    #[inline]
    pub const fn len(&self) -> usize {
        self.bits.len()
    }

    #[inline]
    pub const fn num_bits(&self) -> usize {
        self.len() * 64
    }

    #[inline]
    pub fn check(&self, index: usize) -> bool {
        let (idx, bit) = coord(index);
        self.bits[idx] & bit > 0
    }

    #[inline]
    pub fn set(&mut self, index: usize) -> bool {
        let (idx, bit) = coord(index);
        let previously_contained = self.bits[idx] & bit > 0;
        self.bits[idx] |= bit;
        previously_contained
    }

    #[inline]
    pub fn clear(&mut self) {
        for i in 0..self.len() {
            self.bits[i] = 0;
        }
    }
}

/// Convert bit index to (u64_block_index, bit_mask) coordinates
/// index >> 6 is equivalent to index / 64
/// index & 0b111111 is equivalent to index % 64
#[inline]
fn coord(index: usize) -> (usize, u64) {
    (index >> 6, 1u64 << (index & 0b111111))
}

/// Double hashing to derive multiple hash functions from a single source hash.
///
/// Instead of computing k different hash functions (expensive), we generate them using:
/// hash_i = h1 + i * h2 (with mixing for better distribution)
///
/// This is much faster while maintaining good hash quality.
/// Adapted from https://www.eecs.harvard.edu/~michaelm/postscripts/rsa2008.pdf
#[derive(Clone, Copy)]
pub struct DoubleHasher {
    h1: u64, // Primary hash value
    h2: u64, // Secondary hash value (increment)
}

impl DoubleHasher {
    /// Create a double hasher from a single hash value
    #[inline]
    pub fn new(hash: u64) -> Self {
        // Derive h2 from the upper 32 bits with additional mixing
        let h2 = hash
            .wrapping_shr(32)
            .wrapping_mul(0x51_7c_c1_b7_27_22_0a_95); // Prime-based mixer
        Self { h1: hash, h2 }
    }

    /// Generate the next hash value in the sequence
    /// Each call produces hash_i = hash_{i-1} + h2 (with rotation for mixing)
    #[inline]
    pub fn next(&mut self) -> u64 {
        self.h1 = self.h1.wrapping_add(self.h2).rotate_left(5);
        self.h1
    }
}

/// Returns the bit index for an item's hash using fast modulo reduction.
///
/// This computes `hash % num_bits` without using the expensive division operator.
/// It uses multiplication and shifting instead: (hash * num_bits) / 2^64
///
/// Works because: (hash % num_bits) ≈ ((hash >> 32) * num_bits) >> 32
/// See: https://lemire.me/blog/2016/06/27/a-fast-alternative-to-the-modulo-reduction/
#[inline]
pub fn index(num_bits: usize, hash: u64) -> usize {
    (((hash >> 32).wrapping_mul(num_bits as u64)) >> 32) as usize
}

/// Compute optimal number of hash functions for given filter size and expected items.
///
/// Formula: k = (m/n) * ln(2)
/// where m = number of bits, n = number of items, k = number of hash functions
///
/// More hashes reduce false positives but increase computation time.
#[inline]
pub fn optimal_hashes(num_u64s: usize, num_items: usize) -> u32 {
    let num_bits = (num_u64s * 64) as f64;
    let hashes = LN_2 * num_bits / num_items as f64;
    max(hashes as u32, 1)
}

/// Compute optimal filter size for given expected items and false positive rate.
///
/// Formula: m = -n * ln(p) / (ln(2)^2)
/// where m = bits, n = items, p = false positive rate
///
/// Example: 1M items with 1% false positive rate needs ~9.6M bits (~1.2MB)
#[inline]
pub fn optimal_size(items_count: usize, fp_rate: f64) -> usize {
    let log2_2 = LN_2 * LN_2;
    let result = 8 * ((items_count as f64) * fp_rate.ln() / (-8.0 * log2_2)).ceil() as usize;
    max(result, 512) // Minimum 512 bytes
}

/// A space-efficient probabilistic data structure for set membership testing.
#[derive(Debug, Clone)]
pub struct BloomFilter<S> {
    bits: BitVec,
    num_hashes: u32,
    hasher: S,
}

impl<S: BuildHasher> BloomFilter<S> {
    /// Create a new bloom filter with the specified number of bits and hasher.
    ///
    /// The number of hash functions is calculated automatically based on optimal_hashes().
    pub fn with_num_bits(num_bits: usize, hasher: S, expected_items: usize) -> Self {
        let bits = BitVec::new(num_bits);
        let num_hashes = optimal_hashes(bits.len(), expected_items);
        Self {
            bits,
            num_hashes,
            hasher,
        }
    }

    /// Create a new bloom filter with the specified false positive rate and expected items.
    ///
    /// Example: fp_rate = 0.01 means 1% false positive rate
    pub fn with_fp_rate(fp_rate: f64, hasher: S, expected_items: usize) -> Self {
        let num_bits = optimal_size(expected_items, fp_rate);
        Self::with_num_bits(num_bits, hasher, expected_items)
    }

    /// Compute the source hash for a value.
    #[inline]
    pub fn source_hash(&self, val: &(impl Hash + ?Sized)) -> u64 {
        let mut state = self.hasher.build_hasher();
        val.hash(&mut state);
        state.finish()
    }

    /// Get a fingerprint of the bit positions for a given source hash.
    ///
    /// Returns a hash of all k bit positions that would be set for this item.
    /// Useful for debugging or custom deduplication schemes.
    #[inline]
    pub fn bit_positions_hash(&self, source_hash: u64) -> u64 {
        let mut hasher = DoubleHasher::new(source_hash);
        let num_bits = self.bits.num_bits();
        let mut all_positions = 0;

        // XOR together hashes of all k bit positions
        for _ in 0..self.num_hashes {
            let h = hasher.next();
            let position = index(num_bits, h);
            let mut state = self.hasher.build_hasher();
            position.hash(&mut state);
            all_positions ^= state.finish();
        }
        all_positions
    }

    /// Check if a value is possibly in the filter.
    ///
    /// Returns false: definitely not in the set (no false negatives)
    /// Returns true: probably in the set (may have false positives)
    #[inline]
    pub fn contains(&self, val: &(impl Hash + ?Sized)) -> bool {
        let source_hash = self.source_hash(val);
        self.contains_hash(source_hash)
    }

    /// Check if a source hash is possibly in the filter.
    ///
    /// Checks all k bit positions - returns true only if ALL are set.
    #[inline]
    pub fn contains_hash(&self, source_hash: u64) -> bool {
        let mut hasher = DoubleHasher::new(source_hash);
        let num_bits = self.bits.num_bits();

        // All k bits must be set for item to be "in" the filter
        (0..self.num_hashes).all(|_| {
            let h = hasher.next();
            self.bits.check(index(num_bits, h))
        })
    }

    /// Insert a value into the filter. Returns true if the value may have been present.
    ///
    /// Return value: true if ALL k bits were already set (probably a duplicate)
    ///               false if at least one bit was newly set (definitely new)
    #[inline]
    pub fn insert(&mut self, val: &(impl Hash + ?Sized)) -> bool {
        let source_hash = self.source_hash(val);
        self.insert_hash(source_hash)
    }

    /// Insert a source hash into the filter. Returns true if it may have been present.
    ///
    /// Sets all k bit positions and returns whether they were all already set.
    #[inline]
    pub fn insert_hash(&mut self, source_hash: u64) -> bool {
        let mut hasher = DoubleHasher::new(source_hash);
        let num_bits = self.bits.num_bits();
        let mut all_bits_were_already_set = true;

        // Set all k bits
        for _ in 0..self.num_hashes {
            let h = hasher.next();
            let was_already_set = self.bits.set(index(num_bits, h));
            all_bits_were_already_set &= was_already_set;
        }

        all_bits_were_already_set
    }

    /// Returns the number of hash functions used.
    #[inline]
    pub fn num_hashes(&self) -> u32 {
        self.num_hashes
    }

    /// Returns the total number of bits in the filter.
    #[inline]
    pub fn num_bits(&self) -> usize {
        self.bits.num_bits()
    }

    /// Clear all bits in the filter.
    #[inline]
    pub fn clear(&mut self) {
        self.bits.clear();
    }

    #[inline]
    pub fn get_set_bits(&self) -> usize {
        self.bits.bits.iter().map(|b| b.count_ones() as usize).sum()
    }
}
