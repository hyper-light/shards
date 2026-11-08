//! The sequences' FSE encoders (zstd/fse_encoder.go, fse_predefined.go): each block's
//! literal-length, offset and match-length codes, their normalized counts and tables.

use crate::bits::BitWriter;
use crate::util::high_bit;

const MAX_ENC_TABLE_LOG: u8 = 8;
const MIN_ENC_TABLE_LOG: u8 = 5;

/// `symbolTransform`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SymbolTransform {
    pub delta_nb_bits: u32,
    pub delta_find_state: i16,
    pub out_bits: u8,
}

/// `cTable`.
#[derive(Debug, Clone, Default)]
pub struct CTable {
    table_symbol: Vec<u8>,
    pub state_table: Vec<u16>,
    pub symbol_tt: Vec<SymbolTransform>,
}

/// `fseEncoder`.
#[derive(Debug, Clone)]
pub struct FseEncoder {
    pub symbol_len: u16,
    pub actual_table_log: u8,
    pub ct: CTable,
    max_count: usize,
    zero_bits: bool,
    clear_count: bool,
    pub use_rle: bool,
    pre_defined: bool,
    pub re_used: bool,
    rle_val: u8,
    max_bits: u8,
    pub count: [u32; 256],
    norm: [i16; 256],
}

impl Default for FseEncoder {
    fn default() -> FseEncoder {
        FseEncoder {
            symbol_len: 0,
            actual_table_log: 0,
            ct: CTable::default(),
            max_count: 0,
            zero_bits: false,
            clear_count: false,
            use_rle: false,
            pre_defined: false,
            re_used: false,
            rle_val: 0,
            max_bits: 0,
            count: [0; 256],
            norm: [0; 256],
        }
    }
}

const RTB_TABLE: [u32; 8] = [0, 473_195, 504_333, 520_860, 550_000, 700_000, 750_000, 830_000];

/// Go's `uint32 << n`: zero once `n` reaches 32.
pub fn shl32(v: u32, n: u32) -> u32 {
    v.checked_shl(n).unwrap_or(0)
}

/// `tableStep`.
fn table_step(table_size: u32) -> u32 {
    (table_size >> 1) + (table_size >> 3) + 3
}

fn norm_at(n: &[i16; 256], i: usize) -> i16 {
    n.get(i).copied().unwrap_or(0)
}

impl FseEncoder {
    /// `HistogramFinished`.
    pub fn histogram_finished(&mut self, max_symbol: u8, max_count: usize) {
        self.max_count = max_count;
        self.symbol_len = u16::from(max_symbol) + 1;
        self.clear_count = max_count != 0;
    }

    /// `allocCtable`.
    fn alloc_ctable(&mut self) {
        let table_size = 1usize << self.actual_table_log;
        self.ct.table_symbol.resize(table_size, 0);
        self.ct.state_table.resize(table_size, 0);
        if self.ct.symbol_tt.len() < 256 {
            self.ct.symbol_tt.resize(256, SymbolTransform::default());
        }
    }

    fn tt_mut(&mut self, i: usize) -> Option<&mut SymbolTransform> {
        self.ct.symbol_tt.get_mut(i)
    }

    /// `buildCTable`.
    pub fn build_ctable(&mut self) -> Result<(), &'static str> {
        let table_size = 1u32 << self.actual_table_log;
        let mut high_threshold = table_size - 1;
        let mut cumul = [0i16; 256];
        self.alloc_ctable();
        let sl = usize::from(self.symbol_len);
        let set_cumul = |cumul: &mut [i16; 256], i: usize, v: i16| {
            if let Some(c) = cumul.get_mut(i) {
                *c = v;
            }
        };
        let get_cumul = |cumul: &[i16; 256], i: usize| cumul.get(i).copied().unwrap_or(0);
        {
            for u in 0..sl {
                let v = norm_at(&self.norm, u);
                if v == -1 {
                    let c = get_cumul(&cumul, u) + 1;
                    set_cumul(&mut cumul, u + 1, c);
                    if let Some(t) = self.ct.table_symbol.get_mut(high_threshold as usize) {
                        *t = u as u8;
                    }
                    high_threshold = high_threshold.wrapping_sub(1);
                } else {
                    let c = get_cumul(&cumul, u) + v;
                    set_cumul(&mut cumul, u + 1, c);
                }
            }
            if get_cumul(&cumul, sl) as u32 != table_size {
                return Err("internal error: expected cumul[s.symbolLen] == tableSize");
            }
            set_cumul(&mut cumul, sl, table_size as i16 + 1);
        }
        self.zero_bits = false;
        {
            let step = table_step(table_size);
            let table_mask = table_size - 1;
            let mut position = 0u32;
            let large_limit = 1i16 << (self.actual_table_log - 1);
            for (ui, &v) in self.norm.iter().enumerate().take(sl) {
                if v > large_limit {
                    self.zero_bits = true;
                }
                for _ in 0..v.max(0) {
                    if let Some(t) = self.ct.table_symbol.get_mut(position as usize) {
                        *t = ui as u8;
                    }
                    position = (position + step) & table_mask;
                    while position > high_threshold {
                        position = (position + step) & table_mask;
                    }
                }
            }
            if position != 0 {
                return Err("position!=0");
            }
        }
        {
            let tsi = table_size as usize;
            for (u, &v) in self.ct.table_symbol.iter().enumerate() {
                let at = get_cumul(&cumul, usize::from(v));
                if let Some(t) = self.ct.state_table.get_mut(at as usize) {
                    *t = (tsi + u) as u16;
                }
                set_cumul(&mut cumul, usize::from(v), at + 1);
            }
        }
        {
            let mut total = 0i16;
            let table_log = self.actual_table_log;
            let tl = (u32::from(table_log) << 16).wrapping_sub(1 << table_log);
            for i in 0..sl {
                let v = norm_at(&self.norm, i);
                match v {
                    0 => {}
                    -1 | 1 => {
                        if let Some(t) = self.tt_mut(i) {
                            t.delta_nb_bits = tl;
                            t.delta_find_state = total - 1;
                        }
                        total += 1;
                    }
                    _ => {
                        let max_bits_out = u32::from(table_log).wrapping_sub(high_bit((v - 1) as u32));
                        let min_state_plus = (v as u32) << max_bits_out;
                        if let Some(t) = self.tt_mut(i) {
                            t.delta_nb_bits = (max_bits_out << 16).wrapping_sub(min_state_plus);
                            t.delta_find_state = total - v;
                        }
                        total += v;
                    }
                }
            }
            if total as u32 != table_size {
                return Err("total mismatch");
            }
        }
        Ok(())
    }

    /// `setRLE`.
    pub fn set_rle(&mut self, val: u8) {
        self.alloc_ctable();
        self.actual_table_log = 0;
        self.ct.state_table.truncate(1);
        if let Some(t) = self.tt_mut(usize::from(val)) {
            *t = SymbolTransform::default();
        }
        self.rle_val = val;
        self.use_rle = true;
    }

    /// `setBits`: the output bits of each symbol, the index itself where `transform` is
    /// none.
    pub fn set_bits(&mut self, transform: Option<&[u8]>) {
        if self.re_used || self.pre_defined {
            return;
        }
        if self.use_rle {
            let rle = usize::from(self.rle_val);
            let bits = match transform {
                None => self.rle_val,
                Some(t) => t.get(rle).copied().unwrap_or(0),
            };
            self.max_bits = bits;
            if let Some(t) = self.tt_mut(rle) {
                t.out_bits = bits;
            }
            return;
        }
        let sl = usize::from(self.symbol_len);
        match transform {
            None => {
                for (i, t) in self.ct.symbol_tt.iter_mut().enumerate().take(sl) {
                    t.out_bits = i as u8;
                }
                self.max_bits = (self.symbol_len.wrapping_sub(1)) as u8;
            }
            Some(tr) => {
                self.max_bits = 0;
                for (i, &v) in tr.iter().enumerate().take(sl) {
                    if let Some(t) = self.ct.symbol_tt.get_mut(i) {
                        t.out_bits = v;
                    }
                    if v > self.max_bits {
                        self.max_bits = v;
                    }
                }
            }
        }
    }

    /// `normalizeCount`: the counts normalized to the table's size, and the tables made.
    pub fn normalize_count(&mut self, length: usize) -> Result<(), &'static str> {
        if self.re_used {
            return Ok(());
        }
        self.optimal_table_log(length);
        let table_log = self.actual_table_log;
        let scale = 62 - u64::from(table_log);
        let step = (1u64 << 62) / length as u64;
        let v_step = 1u64 << (scale - 20);
        let mut still_to_distribute = 1i16 << table_log;
        let mut largest = 0usize;
        let mut largest_p = 0i16;
        let low_threshold = (length >> table_log) as u32;
        if self.max_count == length {
            self.use_rle = true;
            return Ok(());
        }
        self.use_rle = false;
        for i in 0..usize::from(self.symbol_len) {
            let cnt = self.count.get(i).copied().unwrap_or(0);
            let norm = if cnt == 0 {
                0
            } else if cnt <= low_threshold {
                still_to_distribute -= 1;
                -1
            } else {
                let mut proba = ((u64::from(cnt).wrapping_mul(step)) >> scale) as i16;
                if proba < 8 {
                    let rest_to_beat =
                        v_step.wrapping_mul(u64::from(RTB_TABLE.get(proba as usize).copied().unwrap_or(0)));
                    let v = u64::from(cnt)
                        .wrapping_mul(step)
                        .wrapping_sub((proba as u64) << scale);
                    if v > rest_to_beat {
                        proba += 1;
                    }
                }
                if proba > largest_p {
                    largest_p = proba;
                    largest = i;
                }
                still_to_distribute -= proba;
                proba
            };
            if let Some(n) = self.norm.get_mut(i) {
                *n = norm;
            }
        }
        if -still_to_distribute >= (norm_at(&self.norm, largest) >> 1) {
            self.normalize_count2(length)?;
            return self.build_ctable();
        }
        if let Some(n) = self.norm.get_mut(largest) {
            *n += still_to_distribute;
        }
        self.build_ctable()
    }

    /// `normalizeCount2`.
    fn normalize_count2(&mut self, length: usize) -> Result<(), &'static str> {
        const NOT_YET_ASSIGNED: i16 = -2;
        let mut distributed = 0u32;
        let mut total = length as u32;
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
                let s_start = (tmp_total >> v_step_log) as u32;
                let s_end = (end >> v_step_log) as u32;
                let weight = s_end.wrapping_sub(s_start);
                if weight < 1 {
                    return Err("weight < 1");
                }
                if let Some(x) = self.norm.get_mut(i) {
                    *x = weight as i16;
                }
                tmp_total = end;
            }
        }
        Ok(())
    }

    /// `optimalTableLog`.
    fn optimal_table_log(&mut self, length: usize) {
        let mut table_log = MAX_ENC_TABLE_LOG;
        let min_bits_src = high_bit(length as u32).wrapping_add(1);
        let min_bits_symbols = high_bit(u32::from(self.symbol_len.wrapping_sub(1))).wrapping_add(2);
        let mut min_bits = min_bits_symbols as u8;
        if min_bits_src < min_bits_symbols {
            min_bits = min_bits_src as u8;
        }
        let max_bits_src = (high_bit(length.wrapping_sub(1) as u32) as u8).wrapping_sub(2);
        if max_bits_src < table_log {
            table_log = max_bits_src;
        }
        if min_bits > table_log {
            table_log = min_bits;
        }
        self.actual_table_log = table_log.clamp(MIN_ENC_TABLE_LOG, MAX_ENC_TABLE_LOG);
    }

    /// `writeCount`: the normalized counts, as `readNCount` reads them.
    pub fn write_count(&self, out: &mut Vec<u8>) -> Result<(), &'static str> {
        if self.use_rle {
            out.push(self.rle_val);
            return Ok(());
        }
        if self.pre_defined || self.re_used {
            return Ok(());
        }
        let table_log = self.actual_table_log;
        let table_size = 1i32 << table_log;
        let mut previous0 = false;
        let mut charnum = 0usize;
        let max_header_size = ((usize::from(self.symbol_len) * usize::from(table_log)) >> 3) + 3 + 2;
        let mut bit_stream = u32::from(table_log - MIN_ENC_TABLE_LOG);
        let mut bit_count = 4u32;
        let mut remaining = (table_size + 1) as i16;
        let mut threshold = table_size as i16;
        let mut nb_bits = u32::from(table_log) + 1;
        let start_len = out.len();
        let mut buf: Vec<u8> = vec![0; max_header_size];
        let mut out_p = 0usize;
        let put = |buf: &mut Vec<u8>, at: usize, v: u32| {
            if let Some(b) = buf.get_mut(at) {
                *b = v as u8;
            }
            if let Some(b) = buf.get_mut(at + 1) {
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
                    put(&mut buf, out_p, bit_stream);
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
                    put(&mut buf, out_p, bit_stream);
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
                return Err("internal error: remaining < 1");
            }
            while remaining < threshold {
                nb_bits -= 1;
                threshold >>= 1;
            }
            if bit_count > 16 {
                put(&mut buf, out_p, bit_stream);
                out_p += 2;
                bit_stream >>= 16;
                bit_count -= 16;
            }
        }
        if out_p + 2 > buf.len() {
            return Err("internal error: header too big");
        }
        put(&mut buf, out_p, bit_stream);
        out_p += bit_count.div_ceil(8) as usize;
        if charnum > usize::from(self.symbol_len) {
            return Err("internal error: charnum > s.symbolLen");
        }
        out.truncate(start_len);
        out.extend_from_slice(buf.get(..out_p).unwrap_or_default());
        Ok(())
    }

    /// `bitCost`.
    fn bit_cost(&self, symbol: usize, accuracy_log: u32) -> u32 {
        let tt = self.ct.symbol_tt.get(symbol).copied().unwrap_or_default();
        let min_nb_bits = tt.delta_nb_bits >> 16;
        let threshold = (min_nb_bits + 1) << 16;
        let table_size = 1u32 << self.actual_table_log;
        let delta = threshold.wrapping_sub(tt.delta_nb_bits.wrapping_add(table_size));
        let normalized = (delta << accuracy_log) >> self.actual_table_log;
        let multiplier = 1u32 << accuracy_log;
        (min_nb_bits + 1)
            .wrapping_mul(multiplier)
            .wrapping_sub(normalized)
    }

    /// `approxSize`: the bits `hist` costs with this table, or the most where it cannot be
    /// represented.
    pub fn approx_size(&self, hist: &[u32]) -> u32 {
        if usize::from(self.symbol_len) < hist.len() || self.use_rle {
            return u32::MAX;
        }
        const K_ACCURACY_LOG: u32 = 8;
        let bad_cost = (u32::from(self.actual_table_log) + 1) << K_ACCURACY_LOG;
        let mut cost = 0u32;
        for (i, &v) in hist.iter().enumerate() {
            if v == 0 {
                continue;
            }
            if norm_at(&self.norm, i) == 0 {
                return u32::MAX;
            }
            let bit_cost = self.bit_cost(i, K_ACCURACY_LOG);
            if bit_cost > bad_cost {
                return u32::MAX;
            }
            cost = cost.wrapping_add(v.wrapping_mul(bit_cost));
        }
        cost >> K_ACCURACY_LOG
    }

    /// `maxHeaderSize`, in bits.
    pub fn max_header_size(&self) -> u32 {
        if self.pre_defined {
            return 0;
        }
        if self.use_rle {
            return 8;
        }
        (((u32::from(self.symbol_len) * u32::from(self.actual_table_log)) >> 3) + 3) * 8
    }

    /// `cState.init`'s write of an RLE table's one state.
    pub fn zero_rle_state(&mut self) {
        if self.ct.state_table.len() == 1
            && let Some(s) = self.ct.state_table.get_mut(0)
        {
            *s = 0;
        }
    }
}

/// `cState`: a stream's compression state.
#[derive(Debug, Clone, Copy)]
pub struct CState {
    pub state: u16,
}

impl CState {
    /// `init`: the state of the first symbol; an RLE table's (one state, which the
    /// block has set to 0) is 0.
    pub fn init(ct: &CTable, first: SymbolTransform) -> CState {
        if ct.state_table.len() == 1 {
            return CState { state: 0 };
        }
        let nb_bits_out = (first.delta_nb_bits.wrapping_add(1 << 15)) >> 16;
        let im = ((nb_bits_out << 16).wrapping_sub(first.delta_nb_bits)) as i32;
        let lu = (im >> nb_bits_out) + i32::from(first.delta_find_state);
        let state = usize::try_from(lu)
            .ok()
            .and_then(|i| ct.state_table.get(i))
            .copied()
            .unwrap_or(0);
        CState { state }
    }

    /// `encode`: a symbol's bits, and the next state.
    pub fn encode(&mut self, bw: &mut BitWriter, state_table: &[u16], tt: SymbolTransform) {
        let nb_bits_out = (u32::from(self.state).wrapping_add(tt.delta_nb_bits)) >> 16;
        let dst = i32::from(self.state >> (nb_bits_out & 15)) + i32::from(tt.delta_find_state);
        bw.add16(self.state, nb_bits_out as u8);
        self.state = usize::try_from(dst)
            .ok()
            .and_then(|i| state_table.get(i))
            .copied()
            .unwrap_or(0);
    }

    /// `flush`.
    pub fn flush(&self, bw: &mut BitWriter, table_log: u8) {
        bw.flush32();
        bw.add16(self.state, table_log);
    }
}

/// The predefined distributions (fse_predefined.go), as encoders: literal lengths,
/// offsets, match lengths.
pub fn predefined(ll_bits: &[u8], ml_bits: &[u8]) -> [FseEncoder; 3] {
    let make = |table_log: u8, norm: &[i16], bits: Option<&[u8]>| {
        let mut e = FseEncoder {
            actual_table_log: table_log,
            symbol_len: norm.len() as u16,
            ..FseEncoder::default()
        };
        for (d, &s) in e.norm.iter_mut().zip(norm) {
            *d = s;
        }
        // The distributions are the format's; they build.
        let _ = e.build_ctable();
        e.set_bits(bits);
        e.pre_defined = true;
        e
    };
    [
        make(
            6,
            &[
                4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1,
                1, -1, -1, -1, -1,
            ],
            Some(ll_bits),
        ),
        make(
            5,
            &[
                1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
            ],
            None,
        ),
        make(
            6,
            &[
                1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
                1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
            ],
            Some(ml_bits),
        ),
    ]
}
