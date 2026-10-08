//! The bit writer of the sequences' streams (zstd/bitwriter.go): the first bit the LSB of
//! the first byte.

/// `bitMask16`: Go's array lists 26 masks, the rest zero.
const BIT_MASK16: [u16; 32] = [
    0, 1, 3, 7, 0xF, 0x1F, 0x3F, 0x7F, 0xFF, 0x1FF, 0x3FF, 0x7FF, 0xFFF, 0x1FFF, 0x3FFF, 0x7FFF, 0xFFFF,
    0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF, 0, 0, 0, 0, 0, 0,
];

/// `bitMask32`.
pub const BIT_MASK32: [u32; 32] = [
    0,
    1,
    3,
    7,
    0xF,
    0x1F,
    0x3F,
    0x7F,
    0xFF,
    0x1FF,
    0x3FF,
    0x7FF,
    0xFFF,
    0x1FFF,
    0x3FFF,
    0x7FFF,
    0xFFFF,
    0x1_FFFF,
    0x3_FFFF,
    0x7_FFFF,
    0xF_FFFF,
    0x1F_FFFF,
    0x3F_FFFF,
    0x7F_FFFF,
    0xFF_FFFF,
    0x1FF_FFFF,
    0x3FF_FFFF,
    0x7FF_FFFF,
    0xFFF_FFFF,
    0x1FFF_FFFF,
    0x3FFF_FFFF,
    0x7FFF_FFFF,
];

pub fn mask32(bits: u8) -> u32 {
    BIT_MASK32.get(usize::from(bits & 31)).copied().unwrap_or(0)
}

#[derive(Debug, Default)]
pub struct BitWriter {
    container: u64,
    n_bits: u8,
    pub out: Vec<u8>,
}

impl BitWriter {
    /// `reset`: writing on after `out`.
    pub fn new(out: Vec<u8>) -> BitWriter {
        BitWriter {
            container: 0,
            n_bits: 0,
            out,
        }
    }

    /// `addBits16NC`.
    pub fn add16(&mut self, value: u16, bits: u8) {
        let m = BIT_MASK16.get(usize::from(bits & 31)).copied().unwrap_or(0);
        self.container |= u64::from(value & m) << (self.n_bits & 63);
        self.n_bits = self.n_bits.wrapping_add(bits);
    }

    /// `addBits32NC`.
    pub fn add32(&mut self, value: u32, bits: u8) {
        self.container |= u64::from(value & mask32(bits)) << (self.n_bits & 63);
        self.n_bits = self.n_bits.wrapping_add(bits);
    }

    /// `addBits32Clean`.
    fn add32_clean(&mut self, value: u32, bits: u8) {
        self.container |= u64::from(value) << (self.n_bits & 63);
        self.n_bits = self.n_bits.wrapping_add(bits);
    }

    /// `addBits64NC`.
    pub fn add64(&mut self, value: u64, bits: u8) {
        if bits <= 31 {
            self.add32_clean(value as u32, bits);
            return;
        }
        self.add32_clean(value as u32, 32);
        self.flush32();
        self.add32_clean((value >> 32) as u32, bits - 32);
    }

    /// `flush32`.
    pub fn flush32(&mut self) {
        if self.n_bits < 32 {
            return;
        }
        self.out.extend_from_slice(&(self.container as u32).to_le_bytes());
        self.n_bits -= 32;
        self.container >>= 32;
    }

    /// `close`: the end mark, then the last bytes.
    pub fn close(&mut self) {
        self.container |= 1u64 << (self.n_bits & 63);
        self.n_bits = self.n_bits.wrapping_add(1);
        let n = (self.n_bits + 7) >> 3;
        for i in 0..n {
            self.out.push((self.container >> (u32::from(i) * 8)) as u8);
        }
        self.n_bits = 0;
        self.container = 0;
    }
}
