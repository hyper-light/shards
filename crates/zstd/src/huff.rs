//! The literals' Huffman coding (huff0/compress.go, huff0.go, bitwriter.go), and the FSE
//! coding of a Huffman table's weights it uses (fse/compress.go, fse.go, bitwriter.go).

use crate::fse::shl32;
use crate::util::high_bit;

/// Why a section is not Huffman coded: it does not pay (`ErrIncompressible`), it is one
/// value repeated (`ErrUseRLE`), or a table could not be made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HuffError {
    Incompressible,
    UseRle,
    Internal(&'static str),
}

const MAX_SYMBOL_VALUE: usize = 255;
const TABLE_LOG_MAX: u8 = 11;
const TABLE_LOG_DEFAULT: u8 = 11;
const MIN_TABLE_LOG: u8 = 5;
const HUFF_NODES_LEN: usize = 512;
const HUFF_NODES_MASK: u32 = (HUFF_NODES_LEN as u32) - 1;

/// `bitMask16` of the fse package: 26 masks, the rest zero.
const FSE_MASK16: [u16; 32] = [
    0, 1, 3, 7, 0xF, 0x1F, 0x3F, 0x7F, 0xFF, 0x1FF, 0x3FF, 0x7FF, 0xFFF, 0x1FFF, 0x3FFF, 0x7FFF, 0xFFFF,
    0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF, 0, 0, 0, 0, 0, 0,
];

/// The bit writers of huff0 and fse: the first bit the LSB of the first byte.
#[derive(Debug, Default)]
struct Bits {
    container: u64,
    n_bits: u8,
    out: Vec<u8>,
}

impl Bits {
    fn add16_clean(&mut self, value: u16, bits: u8) {
        self.container |= u64::from(value) << (self.n_bits & 63);
        self.n_bits = self.n_bits.wrapping_add(bits);
    }

    /// fse's `addBits16NC`.
    fn add16(&mut self, value: u16, bits: u8) {
        let m = FSE_MASK16.get(usize::from(bits & 31)).copied().unwrap_or(0);
        self.container |= u64::from(value & m) << (self.n_bits & 63);
        self.n_bits = self.n_bits.wrapping_add(bits);
    }

    /// fse's `addBits16ZeroNC`.
    fn add16_zero(&mut self, mut value: u16, bits: u8) {
        if bits == 0 {
            return;
        }
        let sh = u32::from(16u8.wrapping_sub(bits) & 15);
        value <<= sh;
        value >>= sh;
        self.container |= u64::from(value) << (self.n_bits & 63);
        self.n_bits = self.n_bits.wrapping_add(bits);
    }

    fn flush32(&mut self) {
        if self.n_bits < 32 {
            return;
        }
        self.out.extend_from_slice(&(self.container as u32).to_le_bytes());
        self.n_bits -= 32;
        self.container >>= 32;
    }

    /// fse's `flush`: the whole bytes.
    fn flush(&mut self) {
        let v = self.n_bits >> 3;
        for i in 0..v.min(8) {
            self.out.push((self.container >> (u32::from(i) * 8)) as u8);
        }
        self.container = self.container.checked_shr(u32::from(v) << 3).unwrap_or(0);
        self.n_bits &= 7;
    }

    /// `close`: the end mark, then the last bytes.
    fn close(&mut self) {
        self.add16_clean(1, 1);
        let n = (self.n_bits + 7) >> 3;
        for i in 0..n {
            self.out.push((self.container >> (u32::from(i) * 8)) as u8);
        }
        self.n_bits = 0;
        self.container = 0;
    }
}

/// fse's `symbolTransform`.
#[derive(Debug, Clone, Copy, Default)]
struct WeightTransform {
    delta_find_state: i32,
    delta_nb_bits: u32,
}

/// fse's `Scratch`, as huff0 uses it for its tables' weights.
#[derive(Debug)]
struct WeightFse {
    count: [u32; 256],
    norm: [i16; 256],
    table_symbol: Vec<u8>,
    state_table: Vec<u16>,
    symbol_tt: Vec<WeightTransform>,
    max_count: usize,
    symbol_len: u16,
    actual_table_log: u8,
    zero_bits: bool,
    clear_count: bool,
    table_log: u8,
    remain: usize,
}

impl Default for WeightFse {
    fn default() -> WeightFse {
        WeightFse {
            count: [0; 256],
            norm: [0; 256],
            table_symbol: Vec::new(),
            state_table: Vec::new(),
            symbol_tt: Vec::new(),
            max_count: 0,
            symbol_len: 0,
            actual_table_log: 0,
            zero_bits: false,
            clear_count: false,
            table_log: 0,
            remain: 0,
        }
    }
}

const FSE_MAX_TABLE_LOG: u8 = 12;
const FSE_MIN_TABLE_LOG: u8 = 5;
const FSE_DEFAULT_TABLE_LOG: u8 = 11;
const RTB_TABLE: [u32; 8] = [0, 473_195, 504_333, 520_860, 550_000, 700_000, 750_000, 830_000];

fn norm_at(n: &[i16; 256], i: usize) -> i16 {
    n.get(i).copied().unwrap_or(0)
}

#[derive(Debug, Clone, Copy)]
struct WeightState {
    state: u16,
}

impl WeightState {
    fn init(state_table: &[u16], first: WeightTransform) -> WeightState {
        let nb_bits_out = (first.delta_nb_bits.wrapping_add(1 << 15)) >> 16;
        let im = ((nb_bits_out << 16).wrapping_sub(first.delta_nb_bits)) as i32;
        let lu = (im >> nb_bits_out).wrapping_add(first.delta_find_state);
        let state = usize::try_from(lu)
            .ok()
            .and_then(|i| state_table.get(i))
            .copied()
            .unwrap_or(0);
        WeightState { state }
    }

    fn step(&mut self, state_table: &[u16], tt: WeightTransform) -> u32 {
        let nb_bits_out = (u32::from(self.state).wrapping_add(tt.delta_nb_bits)) >> 16;
        let dst = i32::from(self.state >> (nb_bits_out & 15)).wrapping_add(tt.delta_find_state);
        let out = nb_bits_out;
        let old = self.state;
        self.state = usize::try_from(dst)
            .ok()
            .and_then(|i| state_table.get(i))
            .copied()
            .unwrap_or(0);
        u32::from(old) | (out << 16)
    }

    /// `encode`.
    fn encode(&mut self, bw: &mut Bits, state_table: &[u16], tt: WeightTransform) {
        let v = self.step(state_table, tt);
        bw.add16(v as u16, (v >> 16) as u8);
    }

    /// `encodeZero`.
    fn encode_zero(&mut self, bw: &mut Bits, state_table: &[u16], tt: WeightTransform) {
        let v = self.step(state_table, tt);
        bw.add16_zero(v as u16, (v >> 16) as u8);
    }

    /// `flush`.
    fn flush(&self, bw: &mut Bits, table_log: u8) {
        bw.flush32();
        bw.add16(self.state, table_log);
        bw.flush();
    }
}

impl WeightFse {
    /// `HistogramFinished`.
    fn histogram_finished(&mut self, max_symbol: u8, max_count: usize) {
        self.max_count = max_count;
        self.symbol_len = u16::from(max_symbol) + 1;
        self.clear_count = max_count != 0;
    }

    /// `Compress`.
    fn compress(&mut self, input: &[u8]) -> Result<Vec<u8>, HuffError> {
        if input.len() <= 1 {
            return Err(HuffError::Incompressible);
        }
        if self.table_log == 0 {
            self.table_log = FSE_DEFAULT_TABLE_LOG;
        }
        if self.table_log > FSE_MAX_TABLE_LOG {
            return Err(HuffError::Internal("tableLog > maxTableLog"));
        }
        if self.clear_count && self.max_count == 0 {
            self.count = [0; 256];
            self.clear_count = false;
        }
        self.remain = input.len();
        let mut max_count = self.max_count;
        if max_count == 0 {
            max_count = self.count_simple(input);
        }
        self.clear_count = true;
        self.max_count = 0;
        if max_count == input.len() {
            return Err(HuffError::UseRle);
        }
        if max_count == 1 || max_count < (input.len() >> 7) {
            return Err(HuffError::Incompressible);
        }
        self.optimal_table_log();
        self.normalize_count()?;
        let header = self.write_count()?;
        self.build_ctable()?;
        let out = self.encode(input, header)?;
        if out.len() >= input.len() {
            return Err(HuffError::Incompressible);
        }
        Ok(out)
    }

    fn count_simple(&mut self, input: &[u8]) -> usize {
        for &v in input {
            if let Some(c) = self.count.get_mut(usize::from(v)) {
                *c += 1;
            }
        }
        let (mut m, mut symlen) = (0u32, self.symbol_len);
        for (i, &v) in self.count.iter().enumerate() {
            if v == 0 {
                continue;
            }
            m = m.max(v);
            symlen = i as u16 + 1;
        }
        self.symbol_len = symlen;
        m as usize
    }

    fn optimal_table_log(&mut self) {
        let mut table_log = self.table_log;
        let min_bits_src = high_bit((self.remain as u32).wrapping_sub(1)).wrapping_add(1);
        let min_bits_symbols = high_bit(u32::from(self.symbol_len.wrapping_sub(1))).wrapping_add(2);
        let min_bits = min_bits_src.min(min_bits_symbols) as u8;
        let max_bits_src = (high_bit((self.remain as u32).wrapping_sub(1)) as u8).wrapping_sub(2);
        if max_bits_src < table_log {
            table_log = max_bits_src;
        }
        if min_bits > table_log {
            table_log = min_bits;
        }
        table_log = table_log.clamp(FSE_MIN_TABLE_LOG, FSE_MAX_TABLE_LOG);
        self.actual_table_log = table_log;
    }

    fn normalize_count(&mut self) -> Result<(), HuffError> {
        let table_log = self.actual_table_log;
        let scale = 62 - u64::from(table_log);
        let step = (1u64 << 62) / self.remain as u64;
        let v_step = 1u64 << (scale - 20);
        let mut still = 1i16 << table_log;
        let (mut largest, mut largest_p) = (0usize, 0i16);
        let low_threshold = (self.remain >> table_log) as u32;
        for i in 0..usize::from(self.symbol_len) {
            let cnt = self.count.get(i).copied().unwrap_or(0);
            let n = if cnt == 0 {
                0
            } else if cnt <= low_threshold {
                still -= 1;
                -1
            } else {
                let mut proba = (u64::from(cnt).wrapping_mul(step) >> scale) as i16;
                if proba < 8 {
                    let rest =
                        v_step.wrapping_mul(u64::from(RTB_TABLE.get(proba as usize).copied().unwrap_or(0)));
                    let v = u64::from(cnt)
                        .wrapping_mul(step)
                        .wrapping_sub((proba as u64) << scale);
                    if v > rest {
                        proba += 1;
                    }
                }
                if proba > largest_p {
                    largest_p = proba;
                    largest = i;
                }
                still -= proba;
                proba
            };
            if let Some(x) = self.norm.get_mut(i) {
                *x = n;
            }
        }
        if -still >= (norm_at(&self.norm, largest) >> 1) {
            return self.normalize_count2();
        }
        if let Some(x) = self.norm.get_mut(largest) {
            *x += still;
        }
        Ok(())
    }

    fn normalize_count2(&mut self) -> Result<(), HuffError> {
        const NOT_YET_ASSIGNED: i16 = -2;
        let mut distributed = 0u32;
        let mut total = self.remain as u32;
        let table_log = self.actual_table_log;
        let low_threshold = total >> table_log;
        let mut low_one = total.wrapping_mul(3) >> (table_log + 1);
        let sl = usize::from(self.symbol_len);
        for i in 0..sl {
            let cnt = self.count.get(i).copied().unwrap_or(0);
            let n = if cnt == 0 {
                0
            } else if cnt <= low_threshold {
                distributed += 1;
                total = total.wrapping_sub(cnt);
                -1
            } else if cnt <= low_one {
                distributed += 1;
                total = total.wrapping_sub(cnt);
                1
            } else {
                NOT_YET_ASSIGNED
            };
            if let Some(x) = self.norm.get_mut(i) {
                *x = n;
            }
        }
        let mut to_distribute = (1u32 << table_log).wrapping_sub(distributed);
        if to_distribute != 0 && (total / to_distribute) > low_one {
            low_one = total.wrapping_mul(3) / to_distribute.wrapping_mul(2);
            for i in 0..sl {
                let cnt = self.count.get(i).copied().unwrap_or(0);
                if norm_at(&self.norm, i) == NOT_YET_ASSIGNED && cnt <= low_one {
                    if let Some(x) = self.norm.get_mut(i) {
                        *x = 1;
                    }
                    distributed += 1;
                    total = total.wrapping_sub(cnt);
                }
            }
            to_distribute = (1u32 << table_log).wrapping_sub(distributed);
        }
        if distributed == u32::from(self.symbol_len) + 1 {
            let (mut max_v, mut max_c) = (0usize, 0u32);
            for i in 0..sl {
                let cnt = self.count.get(i).copied().unwrap_or(0);
                if cnt > max_c {
                    max_v = i;
                    max_c = cnt;
                }
            }
            if let Some(x) = self.norm.get_mut(max_v) {
                *x = x.wrapping_add(to_distribute as i16);
            }
            return Ok(());
        }
        if total == 0 {
            let mut i = 0usize;
            while to_distribute > 0 {
                if let Some(x) = self.norm.get_mut(i)
                    && *x > 0
                {
                    to_distribute -= 1;
                    *x += 1;
                }
                i = (i + 1) % sl.max(1);
            }
            return Ok(());
        }
        let v_step_log = 62 - u64::from(table_log);
        let mid = (1u64 << (v_step_log - 1)) - 1;
        let r_step = ((1u64 << v_step_log)
            .wrapping_mul(u64::from(to_distribute))
            .wrapping_add(mid))
            / u64::from(total);
        let mut tmp_total = mid;
        for i in 0..sl {
            if norm_at(&self.norm, i) == NOT_YET_ASSIGNED {
                let cnt = self.count.get(i).copied().unwrap_or(0);
                let end = tmp_total.wrapping_add(u64::from(cnt).wrapping_mul(r_step));
                let weight = ((end >> v_step_log) as u32).wrapping_sub((tmp_total >> v_step_log) as u32);
                if weight < 1 {
                    return Err(HuffError::Internal("weight < 1"));
                }
                if let Some(x) = self.norm.get_mut(i) {
                    *x = weight as i16;
                }
                tmp_total = end;
            }
        }
        Ok(())
    }

    /// `writeCount`: the header, which the stream follows.
    fn write_count(&self) -> Result<Vec<u8>, HuffError> {
        let table_log = self.actual_table_log;
        let table_size = 1i32 << table_log;
        let mut previous0 = false;
        let mut charnum = 0usize;
        let max_header_size = ((usize::from(self.symbol_len) * usize::from(table_log) + 4 + 2) >> 3) + 3;
        let mut bit_stream = u32::from(table_log - FSE_MIN_TABLE_LOG);
        let mut bit_count = 4u32;
        let mut remaining = (table_size + 1) as i16;
        let mut threshold = table_size as i16;
        let mut nb_bits = u32::from(table_log) + 1;
        let mut out = vec![0u8; max_header_size + 2];
        let mut out_p = 0usize;
        let put = |out: &mut Vec<u8>, at: usize, v: u32| {
            if let Some(b) = out.get_mut(at) {
                *b = v as u8;
            }
            if let Some(b) = out.get_mut(at + 1) {
                *b = (v >> 8) as u8;
            }
        };
        while remaining > 1 {
            if previous0 {
                let mut start = charnum;
                while norm_at(&self.norm, charnum) == 0 && charnum < 256 {
                    charnum += 1;
                }
                while charnum >= start + 24 {
                    start += 24;
                    bit_stream = bit_stream.wrapping_add(shl32(0xFFFF, bit_count));
                    put(&mut out, out_p, bit_stream);
                    out_p += 2;
                    bit_stream >>= 16;
                }
                while charnum >= start + 3 {
                    start += 3;
                    bit_stream = bit_stream.wrapping_add(shl32(3, bit_count));
                    bit_count += 2;
                }
                bit_stream = bit_stream.wrapping_add(shl32((charnum - start) as u32, bit_count));
                bit_count += 2;
                if bit_count > 16 {
                    put(&mut out, out_p, bit_stream);
                    out_p += 2;
                    bit_stream >>= 16;
                    bit_count -= 16;
                }
            }
            let mut count = norm_at(&self.norm, charnum);
            charnum += 1;
            let max = (2 * threshold - 1) - remaining;
            if count < 0 {
                remaining += count;
            } else {
                remaining -= count;
            }
            count += 1;
            if count >= threshold {
                count += max;
            }
            bit_stream = bit_stream.wrapping_add(shl32(count as u32, bit_count));
            bit_count += nb_bits;
            if count < max {
                bit_count -= 1;
            }
            previous0 = count == 1;
            if remaining < 1 {
                return Err(HuffError::Internal("remaining<1"));
            }
            while remaining < threshold {
                nb_bits -= 1;
                threshold >>= 1;
            }
            if bit_count > 16 {
                put(&mut out, out_p, bit_stream);
                out_p += 2;
                bit_stream >>= 16;
                bit_count -= 16;
            }
        }
        put(&mut out, out_p, bit_stream);
        out_p += bit_count.div_ceil(8) as usize;
        if charnum > usize::from(self.symbol_len) {
            return Err(HuffError::Internal("charnum > s.symbolLen"));
        }
        out.truncate(out_p);
        Ok(out)
    }

    fn build_ctable(&mut self) -> Result<(), HuffError> {
        let table_size = 1u32 << self.actual_table_log;
        let mut high_threshold = table_size - 1;
        let mut cumul = [0i16; MAX_SYMBOL_VALUE + 2];
        self.table_symbol.resize(table_size as usize, 0);
        self.state_table.resize(table_size as usize, 0);
        if self.symbol_tt.len() < 256 {
            self.symbol_tt.resize(256, WeightTransform::default());
        }
        let sl = usize::from(self.symbol_len);
        let get = |c: &[i16; MAX_SYMBOL_VALUE + 2], i: usize| c.get(i).copied().unwrap_or(0);
        for u in 0..sl {
            let v = norm_at(&self.norm, u);
            let next = if v == -1 {
                if let Some(t) = self.table_symbol.get_mut(high_threshold as usize) {
                    *t = u as u8;
                }
                high_threshold = high_threshold.wrapping_sub(1);
                get(&cumul, u) + 1
            } else {
                get(&cumul, u) + v
            };
            if let Some(c) = cumul.get_mut(u + 1) {
                *c = next;
            }
        }
        if get(&cumul, sl) as u32 != table_size {
            return Err(HuffError::Internal("expected cumul[s.symbolLen] == tableSize"));
        }
        if let Some(c) = cumul.get_mut(sl) {
            *c = table_size as i16 + 1;
        }
        self.zero_bits = false;
        let step = (table_size >> 1) + (table_size >> 3) + 3;
        let mask = table_size - 1;
        let mut position = 0u32;
        let large_limit = 1i16 << (self.actual_table_log - 1);
        for (ui, &v) in self.norm.iter().enumerate().take(sl) {
            if v > large_limit {
                self.zero_bits = true;
            }
            for _ in 0..v.max(0) {
                if let Some(t) = self.table_symbol.get_mut(position as usize) {
                    *t = ui as u8;
                }
                position = (position + step) & mask;
                while position > high_threshold {
                    position = (position + step) & mask;
                }
            }
        }
        if position != 0 {
            return Err(HuffError::Internal("position!=0"));
        }
        for (u, &v) in self.table_symbol.iter().enumerate() {
            let at = get(&cumul, usize::from(v));
            if let Some(t) = self.state_table.get_mut(at as usize) {
                *t = (table_size as usize + u) as u16;
            }
            if let Some(c) = cumul.get_mut(usize::from(v)) {
                *c = at + 1;
            }
        }
        let mut total = 0i16;
        let table_log = self.actual_table_log;
        let tl = (u32::from(table_log) << 16).wrapping_sub(1 << table_log);
        for i in 0..sl {
            let v = norm_at(&self.norm, i);
            let Some(tt) = self.symbol_tt.get_mut(i) else {
                continue;
            };
            match v {
                0 => {}
                -1 | 1 => {
                    tt.delta_nb_bits = tl;
                    tt.delta_find_state = i32::from(total - 1);
                    total += 1;
                }
                _ => {
                    let max_bits_out = u32::from(table_log).wrapping_sub(high_bit((v - 1) as u32));
                    let min_state_plus = (v as u32) << max_bits_out;
                    tt.delta_nb_bits = (max_bits_out << 16).wrapping_sub(min_state_plus);
                    tt.delta_find_state = i32::from(total - v);
                    total += v;
                }
            }
        }
        if total as u32 != table_size {
            return Err(HuffError::Internal("total mismatch"));
        }
        Ok(())
    }

    /// `compress`: from the last byte to the first, two states each every second byte.
    fn encode(&self, src: &[u8], header: Vec<u8>) -> Result<Vec<u8>, HuffError> {
        if src.len() <= 2 {
            return Err(HuffError::Internal("compress: src too small"));
        }
        let tt = |b: u8| self.symbol_tt.get(usize::from(b)).copied().unwrap_or_default();
        let at = |i: usize| src.get(i).copied().unwrap_or(0);
        let st = &self.state_table;
        let mut bw = Bits {
            out: header,
            ..Bits::default()
        };
        let mut ip = src.len();
        let (mut c1, mut c2);
        if ip & 1 == 1 {
            c1 = WeightState::init(st, tt(at(ip - 1)));
            c2 = WeightState::init(st, tt(at(ip - 2)));
            c1.encode_zero(&mut bw, st, tt(at(ip - 3)));
            ip -= 3;
        } else {
            c2 = WeightState::init(st, tt(at(ip - 1)));
            c1 = WeightState::init(st, tt(at(ip - 2)));
            ip -= 2;
        }
        if ip & 2 != 0 {
            c2.encode_zero(&mut bw, st, tt(at(ip - 1)));
            c1.encode_zero(&mut bw, st, tt(at(ip - 2)));
            ip -= 2;
        }
        let zero = self.zero_bits;
        let big = self.actual_table_log > 8;
        while ip >= 4 {
            let (v3, v2, v1, v0) = (at(ip - 4), at(ip - 3), at(ip - 2), at(ip - 1));
            bw.flush32();
            if zero {
                c2.encode_zero(&mut bw, st, tt(v0));
                c1.encode_zero(&mut bw, st, tt(v1));
            } else {
                c2.encode(&mut bw, st, tt(v0));
                c1.encode(&mut bw, st, tt(v1));
            }
            if big {
                bw.flush32();
            }
            if zero {
                c2.encode_zero(&mut bw, st, tt(v2));
                c1.encode_zero(&mut bw, st, tt(v3));
            } else {
                c2.encode(&mut bw, st, tt(v2));
                c1.encode(&mut bw, st, tt(v3));
            }
            ip -= 4;
        }
        c2.flush(&mut bw, self.actual_table_log);
        c1.flush(&mut bw, self.actual_table_log);
        bw.close();
        Ok(bw.out)
    }
}

/// `cTableEntry`.
#[derive(Debug, Clone, Copy, Default)]
struct CEntry {
    val: u16,
    n_bits: u8,
}

/// When a block's literals may code with the table before (`ReusePolicy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reuse {
    Allow,
    None,
}

/// huff0's `Scratch`, as a block keeps it from one block to the next.
#[derive(Debug)]
pub struct Huff {
    count: [u32; 256],
    src_len: usize,
    pub reuse: Reuse,
    want_log_less: u8,
    symbol_len: u16,
    clear_count: bool,
    actual_table_log: u8,
    prev_table_log: u8,
    prev_table: Vec<CEntry>,
    c_table: Vec<CEntry>,
    /// The nodes of the tree, kept as Go keeps them across blocks.
    nodes: Vec<u64>,
    fse: WeightFse,
    huff_weight: [u8; 256],
}

impl Huff {
    /// `huff0.Scratch{WantLogLess: 4}`.
    pub fn new() -> Huff {
        Huff {
            count: [0; 256],
            src_len: 0,
            reuse: Reuse::Allow,
            want_log_less: 4,
            symbol_len: 0,
            clear_count: false,
            actual_table_log: 0,
            prev_table_log: 0,
            prev_table: Vec::new(),
            c_table: Vec::new(),
            nodes: vec![0; HUFF_NODES_LEN + 1],
            fse: WeightFse::default(),
            huff_weight: [0; 256],
        }
    }

    /// `Compress1X` (`four` false) or `Compress4X`: the coded literals, and whether they
    /// use the table before.
    pub fn compress(&mut self, input: &[u8], four: bool) -> Result<(Vec<u8>, bool), HuffError> {
        if input.len() > (1 << 18) - 1 {
            return Err(HuffError::Internal("input too big"));
        }
        if self.clear_count {
            self.count = [0; 256];
            self.clear_count = false;
        }
        self.src_len = input.len();
        if self.reuse == Reuse::None {
            self.prev_table.clear();
        }
        let (max_count, can_reuse) = self.count_simple(input);
        let mut want = input.len();
        if self.want_log_less > 0 {
            want -= want >> self.want_log_less;
        }
        self.clear_count = true;
        if max_count >= input.len() {
            if max_count > input.len() {
                return Err(HuffError::Internal("maxCount > length"));
            }
            if input.len() == 1 {
                return Err(HuffError::Incompressible);
            }
            return Err(HuffError::UseRle);
        }
        if max_count == 1 || max_count < (input.len() >> 7) {
            return Err(HuffError::Incompressible);
        }
        self.build_ctable()?;
        if self.reuse == Reuse::Allow && can_reuse {
            // `hSize` is the output so far, which is nothing yet.
            let h_size = 0usize;
            let hist = self.count.get(..usize::from(self.symbol_len)).unwrap_or_default();
            let old_size = estimate_size(&self.prev_table, hist);
            let new_size = estimate_size(&self.c_table, hist);
            if old_size <= h_size + new_size || h_size + 12 >= want {
                let mut out = Vec::new();
                compressor(&self.prev_table, self.prev_table_log, input, &mut out, four)?;
                if out.len() >= want {
                    return Err(HuffError::Incompressible);
                }
                return Ok((out, true));
            }
        }
        let mut out = self.write_table()?;
        compressor(&self.c_table, self.actual_table_log, input, &mut out, four)?;
        if out.len() >= want {
            return Err(HuffError::Incompressible);
        }
        self.prev_table = std::mem::take(&mut self.c_table);
        self.prev_table_log = self.actual_table_log;
        Ok((out, false))
    }

    /// `countSimple`: the histogram, its largest count, and whether the table before can
    /// code every symbol.
    fn count_simple(&mut self, input: &[u8]) -> (usize, bool) {
        for &v in input {
            if let Some(c) = self.count.get_mut(usize::from(v)) {
                *c += 1;
            }
        }
        let mut m = 0u32;
        let mut reuse = !self.prev_table.is_empty();
        for (i, &v) in self.count.iter().enumerate() {
            if v == 0 {
                continue;
            }
            m = m.max(v);
            self.symbol_len = i as u16 + 1;
            if self.prev_table.get(i).is_none_or(|e| e.n_bits == 0) {
                reuse = false;
            }
        }
        (m as usize, reuse)
    }

    fn optimal_table_log(&mut self) {
        let mut table_log = TABLE_LOG_DEFAULT;
        let min_bits_src = high_bit(self.src_len as u32).wrapping_add(1);
        let min_bits_symbols = high_bit(u32::from(self.symbol_len.wrapping_sub(1))).wrapping_add(2);
        let min_bits = min_bits_src.min(min_bits_symbols) as u8;
        let max_bits_src = (high_bit((self.src_len as u32).wrapping_sub(1)) as u8).wrapping_sub(1);
        if max_bits_src < table_log {
            table_log = max_bits_src;
        }
        if min_bits > table_log {
            table_log = min_bits;
        }
        self.actual_table_log = table_log.clamp(MIN_TABLE_LOG, TABLE_LOG_MAX);
    }

    fn node(&self, i: usize) -> u64 {
        self.nodes.get(i).copied().unwrap_or(0)
    }

    fn set_node(&mut self, i: usize, v: u64) {
        if let Some(n) = self.nodes.get_mut(i) {
            *n = v;
        }
    }

    /// `huffSort`: the symbols by decreasing count.
    fn huff_sort(&mut self) {
        #[derive(Clone, Copy, Default)]
        struct RankPos {
            base: u32,
            current: u32,
        }
        let mut rank = [RankPos::default(); 32];
        let sl = usize::from(self.symbol_len);
        for &v in self.count.iter().take(sl) {
            let r = (high_bit(v.wrapping_add(1)) & 31) as usize;
            if let Some(x) = rank.get_mut(r) {
                x.base += 1;
            }
        }
        const MAX_BIT_LENGTH: usize = 18 + 1;
        for n in (1..=MAX_BIT_LENGTH).rev() {
            let b = rank.get(n).map_or(0, |x| x.base);
            if let Some(x) = rank.get_mut(n - 1) {
                x.base += b;
            }
        }
        for x in rank.iter_mut().take(MAX_BIT_LENGTH) {
            x.current = x.base;
        }
        // `nodes[1:]`, its index `k` at `k + 1`.
        for n in 0..sl {
            let c = self.count.get(n).copied().unwrap_or(0);
            let r = ((high_bit(c.wrapping_add(1)).wrapping_add(1)) & 31) as usize;
            let Some(rp) = rank.get_mut(r) else { continue };
            let mut pos = rp.current;
            rp.current += 1;
            let base = rp.base;
            let at = |p: u32| (p & HUFF_NODES_MASK) as usize + 1;
            let mut prev = self.node(at(pos.wrapping_sub(1)));
            while pos > base && c > prev as u32 {
                self.set_node(at(pos), prev);
                pos -= 1;
                prev = self.node(at(pos.wrapping_sub(1)));
            }
            self.set_node(at(pos), u64::from(c) | (n as u64) << 48);
        }
    }

    /// `buildCTable`.
    fn build_ctable(&mut self) -> Result<(), HuffError> {
        self.optimal_table_log();
        self.huff_sort();
        self.c_table = vec![CEntry::default(); usize::from(self.symbol_len)];
        let count = |e: u64| e as u32;
        let parent = |e: u64| (e >> 32) as u16;
        let nb_bits = |e: u64| (e >> 56) as u8;
        let set_count = |e: u64, c: u32| (e & 0xffff_ffff_0000_0000) | u64::from(c);
        let set_parent = |e: u64, p: i16| (e & 0xffff_0000_ffff_ffff) | u64::from(p as u16) << 32;
        let set_nb_bits = |e: u64, n: u8| (e & 0x00ff_ffff_ffff_ffff) | u64::from(n) << 56;
        // huffNode[k] is nodes[k+1]; huffNode0[k] is nodes[k].
        let hn = |k: i32| usize::try_from(k + 1).unwrap_or(0);
        let start_node = self.symbol_len as i16;
        let mut non_null_rank = self.symbol_len.wrapping_sub(1);
        let mut node_nb = start_node;
        while count(self.node(hn(i32::from(non_null_rank)))) == 0 && non_null_rank > 0 {
            non_null_rank -= 1;
        }
        let mut low_s = non_null_rank as i16;
        let node_root = node_nb + low_s - 1;
        let mut low_n = node_nb;
        let sum =
            count(self.node(hn(i32::from(low_s)))).wrapping_add(count(self.node(hn(i32::from(low_s) - 1))));
        let e = set_count(self.node(hn(i32::from(node_nb))), sum);
        self.set_node(hn(i32::from(node_nb)), e);
        for k in [i32::from(low_s), i32::from(low_s) - 1] {
            let e = set_parent(self.node(hn(k)), node_nb);
            self.set_node(hn(k), e);
        }
        node_nb += 1;
        low_s -= 2;
        for n in node_nb..=node_root {
            let e = set_count(self.node(hn(i32::from(n))), 1 << 30);
            self.set_node(hn(i32::from(n)), e);
        }
        let e = set_count(self.node(0), 1 << 31);
        self.set_node(0, e);
        let n0 = |k: i16| usize::try_from(i32::from(k)).unwrap_or(0);
        while node_nb <= node_root {
            let pick = |low_s: &mut i16, low_n: &mut i16, nodes: &Vec<u64>| {
                let at = |i: i16| nodes.get(n0(i)).copied().unwrap_or(0);
                if count(at(*low_s + 1)) < count(at(*low_n + 1)) {
                    let n = *low_s;
                    *low_s -= 1;
                    n
                } else {
                    let n = *low_n;
                    *low_n += 1;
                    n
                }
            };
            let n1 = pick(&mut low_s, &mut low_n, &self.nodes);
            let n2 = pick(&mut low_s, &mut low_n, &self.nodes);
            let sum = count(self.node(n0(n1 + 1))).wrapping_add(count(self.node(n0(n2 + 1))));
            let e = set_count(self.node(hn(i32::from(node_nb))), sum);
            self.set_node(hn(i32::from(node_nb)), e);
            for k in [n1, n2] {
                let e = set_parent(self.node(n0(k + 1)), node_nb);
                self.set_node(n0(k + 1), e);
            }
            node_nb += 1;
        }
        let e = set_nb_bits(self.node(hn(i32::from(node_root))), 0);
        self.set_node(hn(i32::from(node_root)), e);
        let mut n = node_root - 1;
        while n >= start_node {
            let p = parent(self.node(hn(i32::from(n))));
            let bits = nb_bits(self.node(hn(i32::from(p)))).wrapping_add(1);
            let e = set_nb_bits(self.node(hn(i32::from(n))), bits);
            self.set_node(hn(i32::from(n)), e);
            n -= 1;
        }
        for n in 0..=non_null_rank {
            let p = parent(self.node(hn(i32::from(n))));
            let bits = nb_bits(self.node(hn(i32::from(p)))).wrapping_add(1);
            let e = set_nb_bits(self.node(hn(i32::from(n))), bits);
            self.set_node(hn(i32::from(n)), e);
        }
        self.actual_table_log = self.set_max_height(usize::from(non_null_rank));
        let max_nb_bits = self.actual_table_log;
        if max_nb_bits > TABLE_LOG_MAX {
            return Err(HuffError::Internal("maxNbBits > tableLogMax"));
        }
        let mut nb_per_rank = [0u16; TABLE_LOG_MAX as usize + 1];
        let mut val_per_rank = [0u16; 16];
        for k in 0..=usize::from(non_null_rank) {
            let b = nb_bits(self.node(k + 1));
            if let Some(x) = nb_per_rank.get_mut(usize::from(b)) {
                *x += 1;
            }
        }
        {
            let mut min = 0u16;
            for n in (1..=max_nb_bits).rev() {
                if let Some(x) = val_per_rank.get_mut(usize::from(n)) {
                    *x = min;
                }
                min = min.wrapping_add(nb_per_rank.get(usize::from(n)).copied().unwrap_or(0));
                min >>= 1;
            }
        }
        for k in 0..=usize::from(non_null_rank) {
            let e = self.node(k + 1);
            let sym = usize::from((e >> 48) as u8);
            if let Some(c) = self.c_table.get_mut(sym) {
                c.n_bits = nb_bits(e);
            }
        }
        for c in &mut self.c_table {
            let nbits = usize::from(c.n_bits & 15);
            let v = val_per_rank.get(nbits).copied().unwrap_or(0);
            c.val = v;
            if let Some(x) = val_per_rank.get_mut(nbits) {
                *x = v.wrapping_add(1);
            }
        }
        Ok(())
    }

    /// `setMaxHeight`: no code longer than the table's log.
    fn set_max_height(&mut self, last_non_null: usize) -> u8 {
        let max_nb_bits = self.actual_table_log;
        let nb = |s: &Huff, i: usize| (s.node(i + 1) >> 56) as u8;
        let cnt = |s: &Huff, i: usize| s.node(i + 1) as u32;
        let set_nb = |s: &mut Huff, i: usize, b: u8| {
            let e = (s.node(i + 1) & 0x00ff_ffff_ffff_ffff) | u64::from(b) << 56;
            s.set_node(i + 1, e);
        };
        let largest_bits = nb(self, last_non_null);
        if largest_bits <= max_nb_bits {
            return largest_bits;
        }
        let mut total_cost = 0i64;
        let base_cost = 1i64 << (largest_bits - max_nb_bits);
        let mut n = last_non_null as u32;
        while nb(self, n as usize) > max_nb_bits {
            total_cost += base_cost - (1i64 << (largest_bits - nb(self, n as usize)));
            set_nb(self, n as usize, max_nb_bits);
            n = n.wrapping_sub(1);
        }
        while nb(self, n as usize) == max_nb_bits {
            n = n.wrapping_sub(1);
        }
        total_cost >>= largest_bits - max_nb_bits;
        const NO_SYMBOL: u32 = 0xF0F0_F0F0;
        let mut rank_last = [NO_SYMBOL; TABLE_LOG_MAX as usize + 2];
        {
            let mut current = max_nb_bits;
            let mut pos = n as i64;
            while pos >= 0 {
                let b = nb(self, pos as usize);
                if b < current {
                    current = b;
                    if let Some(x) = rank_last.get_mut(usize::from(max_nb_bits.wrapping_sub(current))) {
                        *x = pos as u32;
                    }
                }
                pos -= 1;
            }
        }
        let rl = |r: &[u32; TABLE_LOG_MAX as usize + 2], i: u8| {
            r.get(usize::from(i)).copied().unwrap_or(NO_SYMBOL)
        };
        while total_cost > 0 {
            let mut nb_dec = (high_bit(total_cost as u32) as u8).wrapping_add(1);
            while nb_dec > 1 {
                let high_pos = rl(&rank_last, nb_dec);
                let low_pos = rl(&rank_last, nb_dec - 1);
                if high_pos == NO_SYMBOL {
                    nb_dec -= 1;
                    continue;
                }
                if low_pos == NO_SYMBOL {
                    break;
                }
                let high_total = cnt(self, high_pos as usize);
                let low_total = cnt(self, low_pos as usize).wrapping_mul(2);
                if high_total <= low_total {
                    break;
                }
                nb_dec -= 1;
            }
            while nb_dec <= TABLE_LOG_MAX && rl(&rank_last, nb_dec) == NO_SYMBOL {
                nb_dec += 1;
            }
            total_cost -= 1i64 << (nb_dec - 1);
            let next = rl(&rank_last, nb_dec);
            if rl(&rank_last, nb_dec - 1) == NO_SYMBOL
                && let Some(x) = rank_last.get_mut(usize::from(nb_dec - 1))
            {
                *x = next;
            }
            let at = rl(&rank_last, nb_dec) as usize;
            let b = nb(self, at).wrapping_add(1);
            set_nb(self, at, b);
            if rl(&rank_last, nb_dec) == 0 {
                if let Some(x) = rank_last.get_mut(usize::from(nb_dec)) {
                    *x = NO_SYMBOL;
                }
            } else {
                let next = rl(&rank_last, nb_dec).wrapping_sub(1);
                let empty = nb(self, next as usize) != max_nb_bits.wrapping_sub(nb_dec);
                if let Some(x) = rank_last.get_mut(usize::from(nb_dec)) {
                    *x = if empty { NO_SYMBOL } else { next };
                }
            }
        }
        while total_cost < 0 {
            if rl(&rank_last, 1) == NO_SYMBOL {
                while nb(self, n as usize) == max_nb_bits {
                    n = n.wrapping_sub(1);
                }
                let at = (n + 1) as usize;
                let b = nb(self, at).wrapping_sub(1);
                set_nb(self, at, b);
                if let Some(x) = rank_last.get_mut(1) {
                    *x = n + 1;
                }
                total_cost += 1;
                continue;
            }
            let at = (rl(&rank_last, 1) + 1) as usize;
            let b = nb(self, at).wrapping_sub(1);
            set_nb(self, at, b);
            if let Some(x) = rank_last.get_mut(1) {
                *x += 1;
            }
            total_cost += 1;
        }
        max_nb_bits
    }

    /// `cTable.write`: the table's weights, FSE coded where that is smaller.
    fn write_table(&mut self) -> Result<Vec<u8>, HuffError> {
        let huff_log = self.actual_table_log;
        let max_symbol_value = (self.symbol_len.wrapping_sub(1)) as u8;
        let mut bits_to_weight = [0u8; TABLE_LOG_MAX as usize + 1];
        for n in 1..=huff_log {
            if let Some(x) = bits_to_weight.get_mut(usize::from(n)) {
                *x = huff_log + 1 - n;
            }
        }
        for h in self.fse.count.iter_mut().take(16) {
            *h = 0;
        }
        for n in 0..usize::from(max_symbol_value) {
            let b = self.c_table.get(n).map_or(0, |c| c.n_bits);
            let v = bits_to_weight.get(usize::from(b)).copied().unwrap_or(0) & 15;
            if let Some(w) = self.huff_weight.get_mut(n) {
                *w = v;
            }
            if let Some(h) = self.fse.count.get_mut(usize::from(v)) {
                *h += 1;
            }
        }
        if max_symbol_value >= 2 {
            let (mut huff_max_cnt, mut huff_max) = (0u32, 0u8);
            for (i, &v) in self.fse.count.iter().enumerate().take(16) {
                if v == 0 {
                    continue;
                }
                huff_max = i as u8;
                huff_max_cnt = huff_max_cnt.max(v);
            }
            self.fse.histogram_finished(huff_max, huff_max_cnt as usize);
            self.fse.table_log = 6;
            let weights = self
                .huff_weight
                .get(..usize::from(max_symbol_value))
                .unwrap_or_default()
                .to_vec();
            if let Ok(b) = self.fse.compress(&weights)
                && b.len() < usize::from(self.symbol_len >> 1)
            {
                let mut out = Vec::with_capacity(b.len() + 1);
                out.push(b.len() as u8);
                out.extend_from_slice(&b);
                return Ok(out);
            }
        }
        if max_symbol_value > 128 {
            return Err(HuffError::Incompressible);
        }
        let mut out = vec![128 | max_symbol_value.wrapping_sub(1)];
        if let Some(w) = self.huff_weight.get_mut(usize::from(max_symbol_value)) {
            *w = 0;
        }
        let w = |i: usize| self.huff_weight.get(i).copied().unwrap_or(0);
        let mut n = 0usize;
        while n < usize::from(max_symbol_value) {
            out.push((w(n) << 4) | w(n + 1));
            n += 2;
        }
        Ok(out)
    }
}

/// `cTable.estimateSize`: the bytes `hist` takes with `table`.
fn estimate_size(table: &[CEntry], hist: &[u32]) -> usize {
    let mut nb_bits = 7u32;
    for (c, &h) in table.iter().zip(hist) {
        nb_bits = nb_bits.wrapping_add(u32::from(c.n_bits).wrapping_mul(h));
    }
    (nb_bits >> 3) as usize
}

/// `compress1X` or `compress4X` with `table`, after what `out` holds.
fn compressor(
    table: &[CEntry],
    table_log: u8,
    src: &[u8],
    out: &mut Vec<u8>,
    four: bool,
) -> Result<(), HuffError> {
    if !four {
        compress1x(table, table_log, src, out);
        return Ok(());
    }
    if src.len() < 12 {
        return Err(HuffError::Incompressible);
    }
    let segment = src.len().div_ceil(4);
    let offset_idx = out.len();
    out.extend_from_slice(&[0; 6]);
    let mut rest = src;
    for i in 0..4 {
        let (todo, after) = rest.split_at(rest.len().min(segment));
        rest = after;
        let idx = out.len();
        compress1x(table, table_log, todo, out);
        let length = out.len() - idx;
        if length > usize::from(u16::MAX) {
            return Err(HuffError::Incompressible);
        }
        if i < 3 {
            if let Some(b) = out.get_mut(offset_idx + i * 2) {
                *b = length as u8;
            }
            if let Some(b) = out.get_mut(offset_idx + i * 2 + 1) {
                *b = (length >> 8) as u8;
            }
        }
    }
    Ok(())
}

/// `compress1xDo`: from the last symbol to the first.
fn compress1x(table: &[CEntry], table_log: u8, src: &[u8], out: &mut Vec<u8>) {
    let mut bw = Bits {
        out: std::mem::take(out),
        ..Bits::default()
    };
    let enc = |b: u8| table.get(usize::from(b)).copied().unwrap_or_default();
    let at = |i: usize| src.get(i).copied().unwrap_or(0);
    let mut n = src.len() - (src.len() & 3);
    for i in (1..=(src.len() & 3)).rev() {
        let e = enc(at(n + i - 1));
        bw.container |= u64::from(e.val) << (bw.n_bits & 63);
        bw.n_bits = bw.n_bits.wrapping_add(e.n_bits);
    }
    while n >= 4 {
        n -= 4;
        let (a, b, c, d) = (enc(at(n + 3)), enc(at(n + 2)), enc(at(n + 1)), enc(at(n)));
        bw.flush32();
        if table_log <= 8 {
            let bits_a = a.n_bits;
            let bits_b = bits_a.wrapping_add(b.n_bits);
            let bits_c = bits_b.wrapping_add(c.n_bits);
            let bits_d = bits_c.wrapping_add(d.n_bits);
            let combined = u64::from(a.val)
                | (u64::from(b.val) << (bits_a & 63))
                | (u64::from(c.val) << (bits_b & 63))
                | (u64::from(d.val) << (bits_c & 63));
            bw.container |= combined << (bw.n_bits & 63);
            bw.n_bits = bw.n_bits.wrapping_add(bits_d);
        } else {
            let two = |bw: &mut Bits, x: CEntry, y: CEntry| {
                let combined = u64::from(x.val) | (u64::from(y.val) << (x.n_bits & 63));
                bw.container |= combined << (bw.n_bits & 63);
                bw.n_bits = bw.n_bits.wrapping_add(x.n_bits.wrapping_add(y.n_bits));
            };
            two(&mut bw, a, b);
            bw.flush32();
            two(&mut bw, c, d);
        }
    }
    bw.close();
    *out = bw.out;
}
