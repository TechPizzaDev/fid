use crate::fid::FID;
use crate::{bit_array::BitArray, tables::*};
use std::fmt;

use roxygen::*;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

#[cfg(feature = "mem_dbg")]
use mem_dbg::{MemDbg, MemSize};

const SBLOCK_SIZE: u64 = 7; // ceil(log(SBLOCK_SIZE + 1))
const LBLOCK_WIDTH: u64 = 1024;
const LBLOCK_SIZE: u64 = 10;
const SELECT_UNIT_NUM: u64 = 4096;

#[macro_export]
macro_rules! bit_vec {
    () => (
        $crate::BitVector::new()
    );
    ($b:expr; $n:expr) => (
        $crate::BitVector::from_bit($b, $n)
    );
    ($($b:expr),+ $(,)?) => (
        $crate::BitVector::from([$($b),+].as_slice())
    );
}

/// A succinct bit vector that supports FID operations (rank and select) in constant time.
///
/// Bits are divided in small and large blocks. Each small block is identified by
/// a class (number of 1s in the block) and an index within the class.
/// Classes are stored in `ceil(log(SBLOCK_WIDTH + 1))` bits.
/// Indices are stored in `log(C(SBLOCK_WIDTH, index))` bits with enumerative code
/// if its compressed size is less than `MAX_CODE_SIZE`. Otherwise the bit pattern
/// of the small block is explicitly stored as an index for the sake of efficiency.
/// This idea originally comes from [2]. For each large block, we store the number
/// of 1s up to its beginning and a pointer for the index of the first small block.
///
/// # Examples
///
/// ```
/// # use fid::{bit_vec, FID};
/// // 01101101
/// let mut bv = bit_vec![false, true, true, false, true, true, false, true];
/// assert_eq!(bv.rank0(5), 2);
/// assert_eq!(bv.rank1(5), 3);
/// assert_eq!(bv.select0(2), 6);
/// assert_eq!(bv.select1(2), 4);
/// ```
///
/// # References
/// [1] Gonzalo Navarro and Eliana Providel. 2012. Fast, small, simple rank/select on bitmaps.
/// In Proceedings of the 11th international conference on Experimental Algorithms (SEA'12),
/// Ralf Klasing (Ed.). Springer-Verlag, Berlin, Heidelberg, 295-306.
/// DOI=http://dx.doi.org/10.1007/978-3-642-30850-5_26
///
/// [2] rsdic by Daisuke Okanohara.
/// [https://github.com/hillbig/rsdic](https://github.com/hillbig/rsdic)
#[derive(Clone, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "mem_dbg", derive(MemDbg, MemSize))]
pub struct BitVector {
    /// Length of the vector (number of bits).
    len: u64,
    /// Number of 1s.
    ones: u64,
    /// Class identifiers (number of 1s) of small blocks of width `SBLOCK_WIDTH`,
    /// which are represented with `SBLOCK_SIZE` bits.
    sblocks: BitArray,
    /// Rank1 (number of 1s) up to the i-th super block.
    lblocks: Vec<u64>,
    /// Indices of each small block.
    indices: BitArray,
    /// Pointers to `indices`.
    pointers: Vec<u64>,

    select1_unit_pointers: Vec<usize>,
    select0_unit_pointers: Vec<usize>,

    last_sblock_bits: u64,
    pointer: u64,
}

impl BitVector {
    /// Constructs a new, empty [`BitVector`].
    ///
    /// The vector will not allocate until elements are pushed onto it.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_bit(b: bool, len: u64) -> Self {
        let true_odds = (b as u8) as f64;
        let false_odds = (!b as u8) as f64;
        let mut vec = Self::with_odds_and_code_size(len, true_odds, false_odds, 0);
        for _ in 0..len {
            vec.push(b);
        }
        vec
    }

    fn with_odds_and_code_size(
        capacity: u64,
        true_odds: f64,
        false_odds: f64,
        code_size: u32,
    ) -> Self {
        if capacity == 0 {
            return Self::new();
        }

        let sblock_len = capacity.div_ceil(SBLOCK_WIDTH);
        let lblock_len = capacity.div_ceil(LBLOCK_WIDTH) as usize;

        let select_units = capacity.div_ceil(SELECT_UNIT_NUM) as f64;
        let predicted_one_units = (select_units * true_odds).ceil() as usize;
        let predicted_zero_units = (select_units * false_odds).ceil() as usize;

        BitVector {
            len: 0,
            ones: 0,
            sblocks: BitArray::with_capacity(sblock_len * SBLOCK_SIZE),
            lblocks: Vec::with_capacity(lblock_len),
            indices: BitArray::with_capacity(sblock_len * code_size as u64),
            pointers: Vec::with_capacity(lblock_len),
            select1_unit_pointers: Vec::with_capacity(predicted_one_units),
            select0_unit_pointers: Vec::with_capacity(predicted_zero_units),
            last_sblock_bits: 0,
            pointer: 0,
        }
    }

    /// Interpolate estimates for code size. Underlying lookup is generated at build.
    fn get_avg_code_size(true_odds: f64) -> u32 {
        let pivot = SBLOCK_WIDTH as f64 * true_odds;
        let idx = pivot as usize;
        let a = *AVG_CODE_SIZE.get(idx + 0).unwrap_or(&0) as f64;
        let b = *AVG_CODE_SIZE.get(idx + 1).unwrap_or(&0) as f64;
        let t = pivot % 1.0;
        let lerped = a * (1.0 - t) + b * t;
        return lerped.ceil() as u32;
    }

    #[roxygen]
    /// Constructs a new, empty [`BitVector`] with at least the specified capacity.
    pub fn with_odds(
        /// Amount of bits to store.
        capacity: u64,
        /// Probability between `0.0` and `1.0` for which `true` bits occur,
        /// and is used to predict the storage required.
        ///
        /// Probabilities around `0.5` represent the highest entropy and
        /// allocate the maximum required since it is unlikely to compress.
        odds: f64,
    ) -> Self {
        let true_odds = odds.clamp(0.0, 1.0);
        let false_odds = 1.0 - true_odds;
        let code_size = Self::get_avg_code_size(true_odds);
        Self::with_odds_and_code_size(capacity, true_odds, false_odds, code_size)
    }

    /// Constructs a new, empty [`BitVector`] with at least the specified capacity.
    ///
    /// Equivalent to [`with_odds`] with odds `0.5` (highest entropy).
    ///
    /// [`with_odds`]: BitVector::with_odds
    pub fn with_capacity(capacity: u64) -> Self {
        Self::with_odds_and_code_size(capacity, 0.5, 0.5, SBLOCK_WIDTH as u32)
    }

    /// Appends a bit at the end of the vector.
    pub fn push(&mut self, b: bool) {
        if b {
            self.last_sblock_bits |= 1 << (self.len % SBLOCK_WIDTH);
            self.ones += 1;
            if self.ones % SELECT_UNIT_NUM == 0 {
                self.select1_unit_pointers
                    .push((self.len >> LBLOCK_SIZE) as usize);
            }
        } else {
            let zeros = self.len - self.ones + 1;
            if zeros % SELECT_UNIT_NUM == 0 {
                self.select0_unit_pointers
                    .push((self.len >> LBLOCK_SIZE) as usize);
            }
        }
        self.len += 1;

        if self.len % SBLOCK_WIDTH == 0 {
            self.push_blocks();
        }
    }

    #[cold]
    fn push_blocks(&mut self) {
        let last_sblock = self.last_sblock_bits.count_ones();
        let last_sblock_pos = self.len / SBLOCK_WIDTH - 1;
        self.sblocks
            .set_word(last_sblock_pos, SBLOCK_SIZE, last_sblock as u64);

        let (index, index_size) = encode(self.last_sblock_bits, last_sblock as usize);
        self.indices.set_slice(self.pointer, index_size, index);
        self.pointer += index_size;

        self.last_sblock_bits = 0;

        if self.len % LBLOCK_WIDTH == 0 {
            self.lblocks.push(self.ones);
            self.pointers.push(self.pointer);
        }
    }

    pub fn shrink_to_fit(&mut self) {
        self.sblocks.shrink_to_fit();
        self.lblocks.shrink_to_fit();
        self.indices.shrink_to_fit();
        self.pointers.shrink_to_fit();
        self.select1_unit_pointers.shrink_to_fit();
        self.select0_unit_pointers.shrink_to_fit();
    }

    fn get_unit(&self, b: bool, r: u64) -> usize {
        let vec = if b {
            &self.select1_unit_pointers
        } else {
            &self.select0_unit_pointers
        };
        let index = (r / SELECT_UNIT_NUM) as usize;
        *vec.get(index.wrapping_sub(1)).unwrap_or(&0)
    }

    fn get_lblock(&self, pos: usize) -> u64 {
        *self.lblocks.get(pos.wrapping_sub(1)).unwrap_or(&0)
    }

    fn get_pointer(&self, pos: usize) -> u64 {
        *self.pointers.get(pos.wrapping_sub(1)).unwrap_or(&0)
    }
}

impl fmt::Debug for BitVector {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f, "len:    {}", self.len)?;
        writeln!(f, "ones:   {}", self.ones)?;
        write!(f, "sblock: ")?;
        for i in 0..(self.len / SBLOCK_WIDTH) {
            write!(f, "{} ", self.sblocks.get_word(i, SBLOCK_SIZE))?;
        }
        writeln!(f, "{}", self.last_sblock_bits.count_ones())?;
        write!(f, "lblock: ")?;
        for lb in &self.lblocks {
            write!(f, "{} ", lb)?;
        }
        Ok(())
    }
}

impl FID for BitVector {
    fn len(&self) -> u64 {
        self.len
    }

    fn get(&self, i: u64) -> bool {
        debug_assert!(i < self.len);

        let excess = self.len - i;
        let last_sblock_width = self.len % SBLOCK_WIDTH;
        if excess <= last_sblock_width {
            return (self.last_sblock_bits >> (last_sblock_width - excess)) & 1 == 1;
        }

        let lblock_pos = i / LBLOCK_WIDTH;
        let sblock_start_pos = lblock_pos * (LBLOCK_WIDTH / SBLOCK_WIDTH);
        let sblock_end_pos = i / SBLOCK_WIDTH;
        let mut pointer = self.get_pointer(lblock_pos as usize);

        for j in sblock_start_pos..sblock_end_pos {
            let k = self.sblocks.get_word(j, SBLOCK_SIZE);
            pointer += CODE_SIZE[k as usize] as u64;
        }

        let sblock = self.sblocks.get_word(sblock_end_pos, SBLOCK_SIZE);
        let code_size = CODE_SIZE[sblock as usize] as u64;
        let index = self.indices.get_slice(pointer, code_size);

        decode_bit(
            index,
            sblock as usize,
            (i - sblock_end_pos * SBLOCK_WIDTH) as usize,
        )
    }

    fn rank1(&self, i: u64) -> u64 {
        if self.len <= i {
            return self.ones;
        }
        let excess = self.len - i;
        let last_sblock_width = self.len % SBLOCK_WIDTH;
        if excess <= last_sblock_width {
            let last_ones = (self.last_sblock_bits >> (last_sblock_width - excess)).count_ones();
            return self.ones - last_ones as u64;
        }

        let lblock_pos = i / LBLOCK_WIDTH;
        let sblock_start_pos = lblock_pos * (LBLOCK_WIDTH / SBLOCK_WIDTH);
        let sblock_end_pos = i / SBLOCK_WIDTH;
        let mut pointer = self.get_pointer(lblock_pos as usize);
        let mut rank = self.get_lblock(lblock_pos as usize);

        for j in sblock_start_pos..sblock_end_pos {
            let k = self.sblocks.get_word(j, SBLOCK_SIZE);
            rank += k;
            pointer += CODE_SIZE[k as usize] as u64;
        }
        let sblock = self.sblocks.get_word(sblock_end_pos, SBLOCK_SIZE);
        let code_size = CODE_SIZE[sblock as usize] as u64;
        let index = self.indices.get_slice(pointer, code_size);

        rank + decode_rank1(index, sblock as usize, i - sblock_end_pos * SBLOCK_WIDTH)
    }

    fn select1(&self, r: u64) -> u64 {
        if self.ones <= r {
            return self.len;
        }

        let mut lblock_pos = self.get_unit(true, r);
        while lblock_pos < self.lblocks.len() {
            let lblock = self.lblocks[lblock_pos];
            if lblock >= r {
                break;
            }
            lblock_pos += 1;
        }

        let last_sblock = self.last_sblock_bits.count_ones() as u64;
        if self.ones - r <= last_sblock {
            let k = self.len - self.len % SBLOCK_WIDTH;
            let rank = r - (self.ones - last_sblock);
            let select = select1_raw(self.last_sblock_bits, rank as usize);
            return k + select;
        }

        let mut sblock_pos = lblock_pos as u64 * (LBLOCK_WIDTH / SBLOCK_WIDTH);
        let mut sblock = self.sblocks.get_word(sblock_pos, SBLOCK_SIZE);
        let mut rank = self.get_lblock(lblock_pos);
        let mut pointer = self.get_pointer(lblock_pos);
        while rank + sblock <= r {
            rank += sblock;
            pointer += CODE_SIZE[sblock as usize] as u64;
            sblock_pos += 1;
            sblock = self.sblocks.get_word(sblock_pos, SBLOCK_SIZE);
        }

        let code_size = CODE_SIZE[sblock as usize] as u64;
        let index = self.indices.get_slice(pointer, code_size);
        let select_sblock = decode_select1(index, sblock as usize, (r - rank) as usize);

        sblock_pos as u64 * SBLOCK_WIDTH + select_sblock
    }

    fn select0(&self, r: u64) -> u64 {
        let zeros = self.len - self.ones;
        if zeros <= r {
            return self.len;
        }

        let mut lblock_pos = self.get_unit(false, r);
        while lblock_pos < self.lblocks.len() {
            let lblock = LBLOCK_WIDTH * (lblock_pos as u64 + 1) - self.lblocks[lblock_pos];
            if lblock >= r {
                break;
            }
            lblock_pos += 1;
        }

        let last_sblock_width = self.len % SBLOCK_WIDTH;
        let last_sblock = last_sblock_width - u64::from(self.last_sblock_bits.count_ones());
        if zeros - r <= last_sblock {
            let rank = r - (zeros - last_sblock);
            let k = self.len - last_sblock_width;
            let select = select0_raw(self.last_sblock_bits, rank as usize);
            return k + select;
        }

        let mut sblock_pos = lblock_pos as u64 * (LBLOCK_WIDTH / SBLOCK_WIDTH);
        let mut sblock = self.sblocks.get_word(sblock_pos, SBLOCK_SIZE);
        let mut rank = LBLOCK_WIDTH * (lblock_pos as u64) - self.get_lblock(lblock_pos);
        let mut pointer = self.get_pointer(lblock_pos);
        loop {
            let sblock_zero = SBLOCK_WIDTH - sblock;
            if rank + sblock_zero > r {
                break;
            }
            rank += sblock_zero;
            pointer += CODE_SIZE[sblock as usize] as u64;
            sblock_pos += 1;
            sblock = self.sblocks.get_word(sblock_pos, SBLOCK_SIZE);
        }

        let code_size = CODE_SIZE[sblock as usize] as u64;
        let index = self.indices.get_slice(pointer, code_size);
        let select_sblock = decode_select0(index, sblock as usize, (r - rank) as usize);

        sblock_pos as u64 * SBLOCK_WIDTH + select_sblock
    }
}

impl From<&[bool]> for BitVector {
    fn from(value: &[bool]) -> Self {
        // `set_bit_slice` will reserve capacity
        let mut vec = Self::with_capacity(value.len() as u64);
        for b in value {
            vec.push(*b);
        }
        vec
    }
}

fn select1_raw(mut bits: u64, mut r: usize) -> u64 {
    let mut i = 0;
    while bits > 0 {
        if bits & 1 == 1 {
            if r == 0 {
                return i;
            }
            r -= 1;
        }
        i += 1;
        bits >>= 1;
    }
    64
}

fn select0_raw(mut bits: u64, mut r: usize) -> u64 {
    let mut i = 0;
    while i < 64 {
        if bits & 1 == 0 {
            if r == 0 {
                return i;
            }
            r -= 1;
        }
        i += 1;
        bits >>= 1;
    }
    64
}

#[inline(always)]
#[roxygen]
/// Access [`COMBINATION`] without bounds checking.  
/// Asserting conditions for skipping bound checks allows
/// loops to be unrolled further and without a panic branch per iteration.
unsafe fn get_combination_size_unchecked(
    /// Combination index.
    c: u64,
    /// Size index.
    s: usize,
) -> u64 {
    debug_assert!(c <= SBLOCK_WIDTH);
    debug_assert!(s <= SBLOCK_WIDTH as usize);

    let sizes = &COMBINATION[(SBLOCK_WIDTH - c - 1) as usize];
    unsafe { *sizes.get_unchecked(s) }
}

#[roxygen]
/// Encode an integer using a table of combinations.
#[arguments_section]
/// # Panics
/// `k` exceeds the count of ones in `bits`.
fn encode(
    /// Value to encode.
    bits: u64,
    /// Count of ones in `bits`.
    k: usize,
) -> (u64, u64) {
    debug_assert!(bits.count_ones() as usize == k);

    let code_size = CODE_SIZE[k] as u64;
    if code_size == SBLOCK_WIDTH {
        return (bits, code_size);
    }

    let mut l = 0;
    let mut code = 0;
    for i in 0..SBLOCK_WIDTH {
        if (bits >> i) & 1 > 0 {
            // SAFETY: `bits` assert ensures `l` will not exceed `k`
            code += unsafe { get_combination_size_unchecked(i, k - l) };
            l += 1;
        }
    }
    (code, code_size)
}

fn decode_rank1(mut index: u64, k: usize, p: u64) -> u64 {
    assert!(k <= SBLOCK_WIDTH as usize);
    assert!(p <= SBLOCK_WIDTH);

    let code_size = CODE_SIZE[k] as u64;
    if code_size == SBLOCK_WIDTH {
        return (index & ((1 << p) - 1)).count_ones() as u64;
    }

    let mut l = 0;
    for i in 0..p {
        // SAFETY: `k` and `p` asserts
        let base = unsafe { get_combination_size_unchecked(i, k - l) };

        if index >= base {
            index -= base;
            l += 1;
            if l == k {
                break;
            }
        }
    }
    l as u64
}

fn decode_select1(mut index: u64, k: usize, r: usize) -> u64 {
    assert!(k <= SBLOCK_WIDTH as usize);

    let code_size = CODE_SIZE[k] as u64;
    if code_size == SBLOCK_WIDTH {
        return select1_raw(index, r);
    }

    let mut l = 0;
    for i in 0..SBLOCK_WIDTH {
        // SAFETY: `k` assert
        let base = unsafe { get_combination_size_unchecked(i, k - l) };

        if index >= base {
            if l == r {
                return i;
            }
            index -= base;
            l += 1;
        }
    }
    64
}

fn decode_select0(mut index: u64, k: usize, r: usize) -> u64 {
    assert!(k <= SBLOCK_WIDTH as usize);

    let code_size = CODE_SIZE[k] as u64;
    if code_size == SBLOCK_WIDTH {
        return select0_raw(index, r);
    }

    let mut l = 0;
    for i in 0..SBLOCK_WIDTH {
        // SAFETY: `k` assert
        let base = unsafe { get_combination_size_unchecked(i, k - l) };

        if index >= base {
            index -= base;
            l += 1;
        } else if i as usize - l == r {
            return i;
        }
    }
    64
}

fn decode_bit(mut index: u64, k: usize, p: usize) -> bool {
    assert!(k <= SBLOCK_WIDTH as usize);
    assert!(p <= SBLOCK_WIDTH as usize);

    let code_size = CODE_SIZE[k] as u64;
    if code_size == SBLOCK_WIDTH {
        return (index >> p) & 1 == 1;
    }

    // SAFETY: `k` and `p` asserts
    unsafe {
        let mut l = 0;
        for i in 0..p {
            let base = get_combination_size_unchecked(i as u64, k - l);
            if index >= base {
                index -= base;
                l += 1;
                if l == k {
                    break;
                }
            }
        }
        index >= get_combination_size_unchecked(p as u64, k - l)
    }
}

#[cfg(test)]
mod tests {
    extern crate rand;
    use self::rand::{Rng, SeedableRng, StdRng};
    use super::*;
    use crate::bit_arr;

    fn decode(mut index: u64, k: usize) -> u64 {
        let code_size = CODE_SIZE[k] as u64;
        if code_size == SBLOCK_WIDTH {
            return index;
        }

        let mut l = 0;
        let mut bits = 0;
        for i in 0..SBLOCK_WIDTH {
            let base = COMBINATION[(SBLOCK_WIDTH - i - 1) as usize][k - l];
            if index >= base {
                bits |= 1 << i;
                index -= base;
                l += 1;
                if l == k {
                    break;
                }
            }
        }
        bits
    }

    #[test]
    fn test_encode_decode_rng() {
        let n = 1000;
        let mut rng: StdRng = SeedableRng::from_seed([0; 32]);
        for _ in 0..n {
            let bits: u64 = rng.gen();
            let k = bits.count_ones() as usize;
            assert_eq!(decode(encode(bits, k).0, k), bits);
        }
    }

    #[test]
    fn test_encode_decode_log() {
        for i in 0..u64::BITS {
            let bits: u64 = !0 >> i;
            let k = bits.count_ones() as usize;
            assert_eq!(decode(encode(bits, k).0, k), bits);
        }
    }

    #[test]
    fn test_decode_rank1() {
        let n = 100;
        let mut rng: StdRng = SeedableRng::from_seed([0; 32]);
        for _ in 0..n {
            let bits: u64 = rng.gen();
            let k = bits.count_ones() as usize;
            for p in 0..64 {
                let ans = u64::from((bits & ((1 << p) - 1)).count_ones());
                assert_eq!(decode_rank1(encode(bits, k).0, k, p), ans);
            }
        }
    }

    #[test]
    fn test_decode_select1() {
        assert_eq!(decode_select1(encode(u64::MAX, 64).0, 64, 64), 64);

        let n = 100;
        let mut rng: StdRng = SeedableRng::from_seed([0; 32]);
        for _ in 0..n {
            let bits: u64 = rng.gen();
            let k = bits.count_ones() as usize;
            let mut ans = 0;
            for r in 0..k {
                while ans < 64 {
                    if bits & (1 << ans) > 0 {
                        break;
                    }
                    ans += 1;
                }
                assert_eq!(decode_select1(encode(bits, k).0, k, r), ans);
                ans += 1;
            }
        }
    }

    #[test]
    fn test_decode_select0() {
        assert_eq!(decode_select0(encode(!0u64 >> 32, 32).0, 32, 64), 64);

        let n = 100;
        let mut rng: StdRng = SeedableRng::from_seed([0; 32]);
        for _ in 0..n {
            let bits: u64 = rng.gen();
            let k = bits.count_ones() as usize;
            let mut ans = 0;
            for r in 0..(64 - k) {
                while ans < 64 {
                    if bits & (1 << ans) == 0 {
                        break;
                    }
                    ans += 1;
                }
                assert_eq!(decode_select0(encode(bits, k).0, k, r), ans);
                ans += 1;
            }
        }
    }

    #[test]
    fn test_decode_bit() {
        let n = 100;
        let mut rng: StdRng = SeedableRng::from_seed([0; 32]);
        for _ in 0..n {
            let bits: u64 = rng.gen();
            let k = bits.count_ones() as usize;
            for p in 0..64 {
                let ans = (bits >> p) & 1 == 1;
                assert_eq!(
                    decode_bit(encode(bits, k).0, k, p),
                    ans,
                    "the {}-th bit of {:064b} is {}",
                    p,
                    bits,
                    ans as u8,
                );
            }
        }
    }

    const TEST_PROB: &[f64] = &[0.01, 0.5, 0.99];
    const TEST_SIZE: &[u64] = &[
        1,
        SBLOCK_WIDTH / 2,
        SBLOCK_WIDTH,
        LBLOCK_WIDTH - SBLOCK_WIDTH,
        LBLOCK_WIDTH - SBLOCK_WIDTH / 2,
        LBLOCK_WIDTH,
        SELECT_UNIT_NUM - LBLOCK_WIDTH,
        SELECT_UNIT_NUM,
        SELECT_UNIT_NUM + LBLOCK_WIDTH,
        SELECT_UNIT_NUM + LBLOCK_WIDTH + SBLOCK_WIDTH / 2,
        SELECT_UNIT_NUM + LBLOCK_WIDTH + SBLOCK_WIDTH,
        SELECT_UNIT_NUM * 2,
        SELECT_UNIT_NUM * 10 + LBLOCK_WIDTH + SBLOCK_WIDTH + SBLOCK_WIDTH / 2,
    ];

    #[test]
    fn test_construct() {
        for &p in TEST_PROB {
            for &n in TEST_SIZE {
                let mut rng: StdRng = SeedableRng::from_seed([0; 32]);
                let mut bv = BitVector::with_odds(n, p);
                for _ in 0..n {
                    let b = rng.gen_bool(p);
                    bv.push(b);
                }
            }
        }
    }

    #[test]
    fn test_rank1() {
        for &p in TEST_PROB {
            for &n in TEST_SIZE {
                let mut rng: StdRng = SeedableRng::from_seed([0; 32]);
                let mut bv = BitVector::new();
                let mut ba = bit_arr![false; n];
                for i in 0..n {
                    let b = rng.gen_bool(p);
                    ba.set_bit(i, b);
                    bv.push(b);
                }

                let mut rank = 0;
                for i in 0..n {
                    assert_eq!(rank, bv.rank1(i));
                    rank += ba.get_bit(i) as u64;
                }
            }
        }
    }

    #[test]
    fn test_select1() {
        for &p in TEST_PROB {
            for &n in TEST_SIZE {
                let mut rng: StdRng = SeedableRng::from_seed([0; 32]);
                let mut bv = BitVector::new();
                let mut select_ans = vec![];
                for i in 0..n {
                    let b = rng.gen_bool(p);
                    bv.push(b);
                    if b {
                        select_ans.push(i);
                    }
                }

                for (i, &r) in select_ans.iter().enumerate() {
                    assert_eq!(bv.select1(i as u64), r);
                }
            }
        }
    }

    #[test]
    fn test_select0() {
        for &p in TEST_PROB {
            for &n in TEST_SIZE {
                let mut rng: StdRng = SeedableRng::from_seed([0; 32]);
                let mut bv = BitVector::new();
                let mut select_ans = vec![];
                for i in 0..n {
                    let b = rng.gen_bool(p);
                    bv.push(b);
                    if !b {
                        select_ans.push(i);
                    }
                }

                for (i, &r) in select_ans.iter().enumerate() {
                    assert_eq!(bv.select0(i as u64), r);
                }
            }
        }
    }

    #[test]
    fn test_get() {
        for &p in TEST_PROB {
            for &n in TEST_SIZE {
                let mut rng: StdRng = SeedableRng::from_seed([0; 32]);
                let mut bv = BitVector::new();
                let mut ba = bit_arr![false; n];
                for i in 0..n {
                    let b = rng.gen_bool(p);
                    ba.set_bit(i, b);
                    bv.push(b);
                }

                for i in 0..n {
                    assert_eq!(bv.get(i), ba.get_bit(i));
                }
            }
        }
    }

    #[cfg(feature = "serde")]
    #[cfg_attr(not(feature = "serde"), ignore)]
    #[test]
    fn test_serialize_rank1() {
        for &p in TEST_PROB {
            for &n in TEST_SIZE {
                let mut rng: StdRng = SeedableRng::from_seed([0; 32]);
                let mut bv = BitVector::new();
                let mut ba = bit_arr![false; n];
                for i in 0..n {
                    let b = rng.gen_bool(p);
                    ba.set_bit(i, b);
                    bv.push(b);
                }

                let encoded = bincode::serialize(&bv).unwrap();
                let bv: BitVector = bincode::deserialize(&encoded).unwrap();

                let mut rank = 0;
                for i in 0..n {
                    assert_eq!(rank, bv.rank1(i));
                    rank += ba.get_bit(i) as u64;
                }
            }
        }
    }
}
