use std::cmp::max;
use std::f64::consts::LN_2;
use std::hash::{BuildHasher, Hash, Hasher};

/// A bit vector partitioned into u64 blocks.
#[derive(Debug, Clone)]
pub struct BitVec {
    bits: Box<[u64]>,
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

#[inline]
fn coord(index: usize) -> (usize, u64) {
    (index >> 6, 1u64 << (index & 0b111111))
}

/// Double hashing to derive multiple hash functions from a single source hash.
/// Adapted from https://www.eecs.harvard.edu/~michaelm/postscripts/rsa2008.pdf
#[derive(Clone, Copy)]
pub struct DoubleHasher {
    h1: u64,
    h2: u64,
}

impl DoubleHasher {
    #[inline]
    pub fn new(hash: u64) -> Self {
        let h2 = hash
            .wrapping_shr(32)
            .wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
        Self { h1: hash, h2 }
    }

    #[inline]
    pub fn next(&mut self) -> u64 {
        self.h1 = self.h1.wrapping_add(self.h2).rotate_left(5);
        self.h1
    }
}

/// Returns the bit index for an item's hash using fast modulo reduction.
/// https://lemire.me/blog/2016/06/27/a-fast-alternative-to-the-modulo-reduction/
#[inline]
pub fn index(num_bits: usize, hash: u64) -> usize {
    (((hash >> 32).wrapping_mul(num_bits as u64)) >> 32) as usize
}

/// Compute optimal number of hash functions for given filter size and expected items.
#[inline]
pub fn optimal_hashes(num_u64s: usize, num_items: usize) -> u32 {
    let num_bits = (num_u64s * 64) as f64;
    let hashes = LN_2 * num_bits / num_items as f64;
    max(hashes as u32, 1)
}

/// Compute optimal filter size for given expected items and false positive rate.
#[inline]
pub fn optimal_size(items_count: usize, fp_rate: f64) -> usize {
    let log2_2 = LN_2 * LN_2;
    let result = 8 * ((items_count as f64) * fp_rate.ln() / (-8.0 * log2_2)).ceil() as usize;
    max(result, 512)
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
    pub fn with_num_bits(num_bits: usize, hasher: S, expected_items: usize) -> Self {
        let bits = BitVec::new(num_bits);
        let num_hashes = optimal_hashes(bits.len(), expected_items);
        Self {
            bits,
            num_hashes,
            hasher,
        }
    }

    /// Compute the source hash for a value.
    #[inline]
    pub fn source_hash(&self, val: &(impl Hash + ?Sized)) -> u64 {
        let mut state = self.hasher.build_hasher();
        val.hash(&mut state);
        state.finish()
    }

    /// Get the bit positions that would be set/checked for a given source hash.
    /// This is the key method that fastbloom doesn't expose!
    #[inline]
    pub fn bit_positions_hash(&self, source_hash: u64) -> u64 {
        let mut hasher = DoubleHasher::new(source_hash);
        let num_bits = self.bits.num_bits();
        let mut all_positions = 0;
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
    #[inline]
    pub fn contains(&self, val: &(impl Hash + ?Sized)) -> bool {
        let source_hash = self.source_hash(val);
        self.contains_hash(source_hash)
    }

    /// Check if a source hash is possibly in the filter.
    #[inline]
    pub fn contains_hash(&self, source_hash: u64) -> bool {
        let mut hasher = DoubleHasher::new(source_hash);
        let num_bits = self.bits.num_bits();
        (0..self.num_hashes).all(|_| {
            let h = hasher.next();
            self.bits.check(index(num_bits, h))
        })
    }

    /// Insert a value into the filter. Returns true if the value may have been present.
    #[inline]
    pub fn insert(&mut self, val: &(impl Hash + ?Sized)) -> bool {
        let source_hash = self.source_hash(val);
        self.insert_hash(source_hash)
    }

    /// Insert a source hash into the filter. Returns true if it may have been present.
    #[inline]
    pub fn insert_hash(&mut self, source_hash: u64) -> bool {
        let mut hasher = DoubleHasher::new(source_hash);
        let num_bits = self.bits.num_bits();
        let mut previously_contained = true;
        for _ in 0..self.num_hashes {
            let h = hasher.next();
            previously_contained &= self.bits.set(index(num_bits, h));
        }
        previously_contained
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
