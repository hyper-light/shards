//! Loads, hashes and match lengths as the encoders use them (zstd.go, hash.go,
//! matchlen_generic.go). Go reads past nothing here, as its encoders keep their margins;
//! a read out of range is zero, which none of them makes.

/// A byte of `b` at `i`.
pub fn byte(b: &[u8], i: i32) -> u8 {
    usize::try_from(i)
        .ok()
        .and_then(|i| b.get(i))
        .copied()
        .unwrap_or(0)
}

/// `load3232`: four bytes at `i`, little endian.
pub fn load32(b: &[u8], i: i32) -> u32 {
    let at = usize::try_from(i).ok();
    match at.and_then(|i| b.get(i..i.checked_add(4)?)) {
        Some(&[a, c, d, e]) => u32::from_le_bytes([a, c, d, e]),
        _ => (0..4).fold(0u32, |v, k| v | u32::from(byte(b, i.wrapping_add(k))) << (8 * k)),
    }
}

/// `load6432`: eight bytes at `i`, little endian.
pub fn load64(b: &[u8], i: i32) -> u64 {
    let at = usize::try_from(i).ok();
    match at.and_then(|i| b.get(i..i.checked_add(8)?)) {
        Some(s) => s.try_into().map(u64::from_le_bytes).unwrap_or_default(),
        None => (0..8).fold(0u64, |v, k| v | u64::from(byte(b, i.wrapping_add(k))) << (8 * k)),
    }
}

const PRIME3: u32 = 506_832_829;
const PRIME4: u32 = 2_654_435_761;
const PRIME5: u64 = 889_523_592_379;
const PRIME6: u64 = 227_718_039_650_203;
const PRIME7: u64 = 58_295_818_150_454_627;
const PRIME8: u64 = 0xcf1b_bcdc_b7a5_6463;

/// `hashLen`: a hash of the lowest `mls` bytes of `u` in `length` bits.
pub fn hash_len(u: u64, length: u32, mls: u32) -> u32 {
    match mls {
        3 => ((u << 8) as u32).wrapping_mul(PRIME3) >> (32 - length),
        5 => ((u << (64 - 40)).wrapping_mul(PRIME5) >> (64 - length)) as u32,
        6 => ((u << (64 - 48)).wrapping_mul(PRIME6) >> (64 - length)) as u32,
        7 => ((u << (64 - 56)).wrapping_mul(PRIME7) >> (64 - length)) as u32,
        8 => (u.wrapping_mul(PRIME8) >> (64 - length)) as u32,
        _ => (u as u32).wrapping_mul(PRIME4) >> (32 - length),
    }
}

/// `matchLen`: the length of the prefix `a` and `b` share.
pub fn match_len(a: &[u8], b: &[u8]) -> usize {
    let mut n = 0;
    for (x, y) in a.as_chunks::<8>().0.iter().zip(b.as_chunks::<8>().0) {
        let diff = u64::from_le_bytes(*x) ^ u64::from_le_bytes(*y);
        if diff != 0 {
            return n + (diff.trailing_zeros() >> 3) as usize;
        }
        n += 8;
    }
    let (a, b) = (a.get(n..).unwrap_or_default(), b.get(n..).unwrap_or_default());
    n + a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// `e.matchlen(s, t, src)`: the match at `s` against `t` in `src`.
pub fn match_at(src: &[u8], s: i32, t: i32) -> i32 {
    let at = |i: i32| {
        usize::try_from(i)
            .ok()
            .and_then(|i| src.get(i..))
            .unwrap_or_default()
    };
    i32::try_from(match_len(at(s), at(t))).unwrap_or(i32::MAX)
}

/// `bits.Len32`.
pub fn len32(v: u32) -> u32 {
    32 - v.leading_zeros()
}

/// `highBit`: the index of the highest bit set, as Go's `bits.Len32(v) - 1` wraps.
pub fn high_bit(v: u32) -> u32 {
    len32(v).wrapping_sub(1)
}
