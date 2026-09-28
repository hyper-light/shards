//! Data patterns shared by the host-side E2E tests and the guest agent, so both sides
//! derive expected disk contents from one definition.

#![no_std]

/// The byte at `offset` of a disk filled with pattern `salt`: varies with every byte
/// and every sector, so misplaced, torn, or stale transfers are all detected.
pub fn pattern_byte(salt: u64, offset: u64) -> u8 {
    let sector = offset / 512;
    let mix = sector.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ salt.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    (mix >> 29) as u8 ^ offset as u8
}

/// Fills `buf` with pattern `salt` as if it were read from disk offset `offset`.
pub fn fill(salt: u64, offset: u64, buf: &mut [u8]) {
    for (i, b) in buf.iter_mut().enumerate() {
        *b = pattern_byte(salt, offset.wrapping_add(i as u64));
    }
}

/// Index of the first byte in `buf` that does not match pattern `salt` at `offset`.
pub fn first_mismatch(salt: u64, offset: u64, buf: &[u8]) -> Option<usize> {
    buf.iter()
        .enumerate()
        .position(|(i, &b)| b != pattern_byte(salt, offset.wrapping_add(i as u64)))
}
