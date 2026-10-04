//! SipHash-2-4 (Aumasson and Bernstein, "SipHash: a fast short-input PRF", INDOCRYPT
//! 2012): a keyed function of short inputs whose outputs no one without the key can
//! predict, which Linux keys its initial sequence numbers with (net/core/secure_seq.c,
//! `secure_tcp_seq`).

/// SipHash-2-4 of `msg` under `key`.
pub fn siphash24(key: &[u8; 16], msg: &[u8]) -> u64 {
    let (k0, k1) = key.split_at(8);
    let word = |b: &[u8]| {
        let mut w = [0u8; 8];
        w.get_mut(..b.len()).unwrap_or_default().copy_from_slice(b);
        u64::from_le_bytes(w)
    };
    let (k0, k1) = (word(k0), word(k1));
    let mut v = [
        k0 ^ 0x736f_6d65_7073_6575,
        k1 ^ 0x646f_7261_6e64_6f6d,
        k0 ^ 0x6c79_6765_6e65_7261,
        k1 ^ 0x7465_6462_7974_6573,
    ];
    let round = |v: &mut [u64; 4]| {
        v[0] = v[0].wrapping_add(v[1]);
        v[1] = v[1].rotate_left(13) ^ v[0];
        v[0] = v[0].rotate_left(32);
        v[2] = v[2].wrapping_add(v[3]);
        v[3] = v[3].rotate_left(16) ^ v[2];
        v[0] = v[0].wrapping_add(v[3]);
        v[3] = v[3].rotate_left(21) ^ v[0];
        v[2] = v[2].wrapping_add(v[1]);
        v[1] = v[1].rotate_left(17) ^ v[2];
        v[2] = v[2].rotate_left(32);
    };
    let (blocks, tail) = msg.as_chunks::<8>();
    for block in blocks {
        let m = u64::from_le_bytes(*block);
        v[3] ^= m;
        round(&mut v);
        round(&mut v);
        v[0] ^= m;
    }
    // The last block: the bytes left, and the message's length in its top byte.
    let m = word(tail) | ((msg.len() as u64) << 56);
    v[3] ^= m;
    round(&mut v);
    round(&mut v);
    v[0] ^= m;
    v[2] ^= 0xff;
    for _ in 0..4 {
        round(&mut v);
    }
    v[0] ^ v[1] ^ v[2] ^ v[3]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The paper's own example (Appendix A): key 00..0f, message 00..0e.
    #[test]
    fn the_papers_example() {
        let key: [u8; 16] = std::array::from_fn(|i| i as u8);
        let msg: Vec<u8> = (0..15).collect();
        assert_eq!(siphash24(&key, &msg), 0xa129_ca61_49be_45e5);
    }

    /// The reference implementation's vectors at the edges of a block (vectors.h,
    /// vectors_sip64): the empty message, and one of 8 bytes.
    #[test]
    fn the_reference_vectors_at_a_blocks_edges() {
        let key: [u8; 16] = std::array::from_fn(|i| i as u8);
        assert_eq!(siphash24(&key, &[]), 0x726f_db47_dd0e_0e31);
        let eight: Vec<u8> = (0..8).collect();
        assert_eq!(siphash24(&key, &eight), 0x93f5_f579_9a93_2462);
    }
}
