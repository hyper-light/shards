//! XXH64 with a seed of 0 (zstd/internal/xxhash): the frame's checksum, of the content.

const P1: u64 = 11_400_714_785_074_694_791;
const P2: u64 = 14_029_467_366_897_019_727;
const P3: u64 = 1_609_587_929_392_839_161;
const P4: u64 = 9_650_029_242_287_828_579;
const P5: u64 = 2_870_177_450_012_600_261;

fn round(acc: u64, input: u64) -> u64 {
    acc.wrapping_add(input.wrapping_mul(P2))
        .rotate_left(31)
        .wrapping_mul(P1)
}

fn merge_round(acc: u64, val: u64) -> u64 {
    (acc ^ round(0, val)).wrapping_mul(P1).wrapping_add(P4)
}

fn le64(b: &[u8]) -> u64 {
    b.get(..8)
        .and_then(|s| <[u8; 8]>::try_from(s).ok())
        .map_or(0, u64::from_le_bytes)
}

fn le32(b: &[u8]) -> u32 {
    b.get(..4)
        .and_then(|s| <[u8; 4]>::try_from(s).ok())
        .map_or(0, u32::from_le_bytes)
}

/// `xxhash.Digest`.
#[derive(Debug, Clone)]
pub struct Xxh64 {
    v: [u64; 4],
    total: u64,
    mem: [u8; 32],
    n: usize,
}

impl Default for Xxh64 {
    fn default() -> Xxh64 {
        Xxh64 {
            v: [P1.wrapping_add(P2), P2, 0, 0u64.wrapping_sub(P1)],
            total: 0,
            mem: [0; 32],
            n: 0,
        }
    }
}

impl Xxh64 {
    fn stripe(&mut self, b: &[u8]) {
        for (i, v) in self.v.iter_mut().enumerate() {
            *v = round(*v, le64(b.get(i * 8..).unwrap_or_default()));
        }
    }

    pub fn write(&mut self, mut b: &[u8]) {
        self.total = self.total.wrapping_add(b.len() as u64);
        if self.n + b.len() < 32 {
            if let Some(dst) = self.mem.get_mut(self.n..self.n + b.len()) {
                dst.copy_from_slice(b);
            }
            self.n += b.len();
            return;
        }
        if self.n > 0 {
            let take = 32 - self.n;
            let (head, rest) = b.split_at(take.min(b.len()));
            if let Some(dst) = self.mem.get_mut(self.n..) {
                dst.copy_from_slice(head);
            }
            let mem = self.mem;
            self.stripe(&mem);
            b = rest;
            self.n = 0;
        }
        let (chunks, rest) = b.as_chunks::<32>();
        for c in chunks {
            self.stripe(c);
        }
        if let Some(dst) = self.mem.get_mut(..rest.len()) {
            dst.copy_from_slice(rest);
        }
        self.n = rest.len();
    }

    pub fn sum64(&self) -> u64 {
        let [v1, v2, v3, v4] = self.v;
        let mut h = if self.total >= 32 {
            let mut h = v1
                .rotate_left(1)
                .wrapping_add(v2.rotate_left(7))
                .wrapping_add(v3.rotate_left(12))
                .wrapping_add(v4.rotate_left(18));
            h = merge_round(h, v1);
            h = merge_round(h, v2);
            h = merge_round(h, v3);
            merge_round(h, v4)
        } else {
            v3.wrapping_add(P5)
        };
        h = h.wrapping_add(self.total);
        let mut b = self.mem.get(..self.n).unwrap_or_default();
        while b.len() >= 8 {
            h ^= round(0, le64(b));
            h = h.rotate_left(27).wrapping_mul(P1).wrapping_add(P4);
            b = b.get(8..).unwrap_or_default();
        }
        if b.len() >= 4 {
            h ^= u64::from(le32(b)).wrapping_mul(P1);
            h = h.rotate_left(23).wrapping_mul(P2).wrapping_add(P3);
            b = b.get(4..).unwrap_or_default();
        }
        for &c in b {
            h ^= u64::from(c).wrapping_mul(P5);
            h = h.rotate_left(11).wrapping_mul(P1);
        }
        h ^= h >> 33;
        h = h.wrapping_mul(P2);
        h ^= h >> 29;
        h = h.wrapping_mul(P3);
        h ^ (h >> 32)
    }
}

#[cfg(test)]
mod tests {
    /// XXH64's published values, seed 0.
    #[test]
    fn digests_are_xxh64s() {
        let sum = |b: &[u8]| {
            let mut d = super::Xxh64::default();
            d.write(b);
            d.sum64()
        };
        assert_eq!(sum(b""), 0xef46_db37_51d8_e999);
        assert_eq!(sum(b"a"), 0xd24e_c4f1_a98c_6e5b);
        assert_eq!(sum(b"abc"), 0x44bc_2cf5_ad77_0999);
        assert_eq!(
            sum(b"The quick brown fox jumps over the lazy dog"),
            0x0b24_2d36_1fda_71bc
        );
        // In pieces, across stripes, as a stream writes it.
        let long: Vec<u8> = (0..1000u32).map(|i| (i * 7 % 251) as u8).collect();
        let mut d = super::Xxh64::default();
        for piece in long.chunks(13) {
            d.write(piece);
        }
        assert_eq!(d.sum64(), sum(&long));
    }
}
