//! Snappy blocks decoded as github.com/golang/snappy v1.0.0's Decode decodes them, for the
//! `.json.sn` bundles GitHub's attestations point to: the same output, and the same error
//! for every input. Go allocates the length the header claims, up to 4 GiB, before it
//! finds the input short; a claim no input of its length could meet is refused here first,
//! with the error Go would reach.

/// ErrCorrupt.
pub const CORRUPT: &str = "snappy: corrupt input";
/// The most a tag writes for the bytes it takes: a 2-byte-offset copy writes 64 for 3.
const MOST_PER_BYTE: usize = 22;

/// binary.Uvarint: the value and the bytes it took; none where it is cut short or
/// overflows 64 bits.
fn uvarint(b: &[u8]) -> Option<(u64, usize)> {
    let mut x: u64 = 0;
    let mut s: u32 = 0;
    for (i, &c) in b.iter().enumerate() {
        if i == 10 {
            return None;
        }
        if c < 0x80 {
            if i == 9 && c > 1 {
                return None;
            }
            return Some((x | (u64::from(c) << s), i + 1));
        }
        x |= u64::from(c & 0x7f) << s;
        s += 7;
    }
    None
}

/// Decode(nil, src).
pub fn decode(src: &[u8]) -> Result<Vec<u8>, &'static str> {
    let (len, header) = uvarint(src).ok_or(CORRUPT)?;
    if len > 0xffff_ffff {
        return Err(CORRUPT);
    }
    let len = usize::try_from(len).map_err(|_| CORRUPT)?;
    let src = src.get(header..).ok_or(CORRUPT)?;
    if len > src.len().saturating_mul(MOST_PER_BYTE) {
        return Err(CORRUPT);
    }
    let mut dst = vec![0u8; len];
    decode_block(&mut dst, src)?;
    Ok(dst)
}

/// decode (decode_other.go): `src`'s tags into all of `dst`.
fn decode_block(dst: &mut [u8], src: &[u8]) -> Result<(), &'static str> {
    let byte = |i: usize| src.get(i).copied().map(usize::from).ok_or(CORRUPT);
    let (mut d, mut s) = (0usize, 0usize);
    while s < src.len() {
        let tag = byte(s)?;
        let (length, offset) = match tag & 0x03 {
            // tagLiteral
            0 => {
                let x = tag >> 2;
                let (x, took) = match x {
                    0..=59 => (x, 1),
                    60 => (byte(s + 1)?, 2),
                    61 => (byte(s + 1)? | byte(s + 2)? << 8, 3),
                    62 => (byte(s + 1)? | byte(s + 2)? << 8 | byte(s + 3)? << 16, 4),
                    _ => (
                        byte(s + 1)? | byte(s + 2)? << 8 | byte(s + 3)? << 16 | byte(s + 4)? << 24,
                        5,
                    ),
                };
                s += took;
                // int(x) + 1, never less than one where int has 64 bits, as on every
                // target shards builds for.
                let length = x + 1;
                if length > dst.len() - d || length > src.len() - s {
                    return Err(CORRUPT);
                }
                let (to, from) = (dst.get_mut(d..d + length), src.get(s..s + length));
                to.ok_or(CORRUPT)?.copy_from_slice(from.ok_or(CORRUPT)?);
                d += length;
                s += length;
                continue;
            }
            // tagCopy1
            1 => {
                let (a, b) = (byte(s)?, byte(s + 1)?);
                s += 2;
                (4 + ((a >> 2) & 0x7), ((a & 0xe0) << 3) | b)
            }
            // tagCopy2
            2 => {
                let (a, lo, hi) = (byte(s)?, byte(s + 1)?, byte(s + 2)?);
                s += 3;
                (1 + (a >> 2), lo | hi << 8)
            }
            // tagCopy4
            _ => {
                let a = byte(s)?;
                let o = byte(s + 1)? | byte(s + 2)? << 8 | byte(s + 3)? << 16 | byte(s + 4)? << 24;
                s += 5;
                (1 + (a >> 2), o)
            }
        };
        if offset == 0 || d < offset || length > dst.len() - d {
            return Err(CORRUPT);
        }
        // Byte by byte, as the copy may overlap what it writes.
        for i in d..d + length {
            let b = *dst.get(i - offset).ok_or(CORRUPT)?;
            *dst.get_mut(i).ok_or(CORRUPT)? = b;
        }
        d += length;
    }
    if d != dst.len() {
        return Err(CORRUPT);
    }
    Ok(())
}
