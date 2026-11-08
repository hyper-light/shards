//! Go's deflate tokens (token.go): a literal, or a match's length and offset, in 32 bits.

pub type Token = u32;

const LENGTH_SHIFT: u32 = 22;
const OFFSET_MASK: u32 = (1 << LENGTH_SHIFT) - 1;
pub const MATCH_TYPE: u32 = 1 << 30;

/// The length base of each length code (huffman_bit_writer.go `lengthBase`).
pub const LENGTH_BASE: [u32; 29] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 20, 24, 28, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192,
    224, 255,
];

/// The offset base of each offset code (`offsetBase`).
pub const OFFSET_BASE: [u32; 30] = [
    0x000000, 0x000001, 0x000002, 0x000003, 0x000004, 0x000006, 0x000008, 0x00000c, 0x000010, 0x000018,
    0x000020, 0x000030, 0x000040, 0x000060, 0x000080, 0x0000c0, 0x000100, 0x000180, 0x000200, 0x000300,
    0x000400, 0x000600, 0x000800, 0x000c00, 0x001000, 0x001800, 0x002000, 0x003000, 0x004000, 0x006000,
];

/// `lengthCodes`: each length's code, the last whose base is no more than it.
const LENGTH_CODES: [u8; 256] = codes_of(&LENGTH_BASE);

/// `offsetCodes`: each offset under 256's code, the last whose base is no more than it.
const OFFSET_CODES: [u8; 256] = codes_of(&OFFSET_BASE);

// Evaluated at compile time only: an index out of range fails the build, never a run.
#[allow(clippy::indexing_slicing)]
const fn codes_of<const N: usize>(base: &[u32; N]) -> [u8; 256] {
    let mut out = [0u8; 256];
    let mut v = 0usize;
    while v < 256 {
        let mut c = 0usize;
        while c + 1 < N && base[c + 1] <= v as u32 {
            c += 1;
        }
        out[v] = c as u8;
        v += 1;
    }
    out
}

pub fn literal_token(literal: u32) -> Token {
    literal
}

pub fn match_token(xlength: u32, xoffset: u32) -> Token {
    MATCH_TYPE
        .wrapping_add(xlength << LENGTH_SHIFT)
        .wrapping_add(xoffset)
}

pub fn literal(t: Token) -> u32 {
    t
}

pub fn offset(t: Token) -> u32 {
    t & OFFSET_MASK
}

pub fn length(t: Token) -> u32 {
    t.wrapping_sub(MATCH_TYPE) >> LENGTH_SHIFT
}

pub fn length_code(len: u32) -> u32 {
    u32::from(LENGTH_CODES.get(len as usize).copied().unwrap_or(0))
}

pub fn offset_code(off: u32) -> u32 {
    let code = |i: u32| u32::from(OFFSET_CODES.get(i as usize).copied().unwrap_or(0));
    if off < 256 {
        return code(off);
    }
    if off >> 7 < 256 {
        return code(off >> 7) + 14;
    }
    code(off >> 14) + 28
}
