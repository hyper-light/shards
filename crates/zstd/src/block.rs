//! A block's encoding (zstd/blockenc.go, seqenc.go): its literals Huffman coded, raw or
//! RLE, and its sequences FSE coded with new, repeated or predefined tables, or the whole
//! block raw or RLE where that is smaller.

use crate::bits::{BitWriter, mask32};
use crate::fse::{CState, FseEncoder, SymbolTransform};
use crate::huff::{Huff, HuffError, Reuse};
use crate::util::{high_bit, len32};

/// A sequence (`seq`): literals, a match, its offset code, and the codes the encoder
/// looks up once.
#[derive(Debug, Clone, Copy, Default)]
pub struct Seq {
    pub lit_len: u32,
    pub match_len: u32,
    pub offset: u32,
    ll_code: u8,
    ml_code: u8,
    of_code: u8,
}

impl Seq {
    pub fn new(lit_len: u32, match_len: u32, offset: u32) -> Seq {
        Seq {
            lit_len,
            match_len,
            offset,
            ..Seq::default()
        }
    }
}

const ZSTD_MIN_MATCH: u32 = 3;

const LL_CODE_TABLE: [u8; 64] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 16, 17, 17, 18, 18, 19, 19, 20, 20, 20, 20, 21,
    21, 21, 21, 22, 22, 22, 22, 22, 22, 22, 22, 23, 23, 23, 23, 23, 23, 23, 23, 24, 24, 24, 24, 24, 24, 24,
    24, 24, 24, 24, 24, 24, 24, 24, 24,
];

/// `llBitsTable`.
pub const LL_BITS_TABLE: [u8; 36] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10, 11, 12, 13,
    14, 15, 16,
];

const ML_CODE_TABLE: [u8; 128] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28,
    29, 30, 31, 32, 32, 33, 33, 34, 34, 35, 35, 36, 36, 36, 36, 37, 37, 37, 37, 38, 38, 38, 38, 38, 38, 38,
    38, 39, 39, 39, 39, 39, 39, 39, 39, 40, 40, 40, 40, 40, 40, 40, 40, 40, 40, 40, 40, 40, 40, 40, 40, 41,
    41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42,
    42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42,
];

/// `mlBitsTable`.
pub const ML_BITS_TABLE: [u8; 53] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1,
    1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
];

/// `llCode`.
fn ll_code(lit_len: u32) -> u8 {
    if lit_len <= 63 {
        return LL_CODE_TABLE.get((lit_len & 63) as usize).copied().unwrap_or(0);
    }
    (high_bit(lit_len) as u8).wrapping_add(19)
}

/// `mlCode`: of the match length less `zstdMinMatch`.
pub fn ml_code(ml_base: u32) -> u8 {
    if ml_base <= 127 {
        return ML_CODE_TABLE.get((ml_base & 127) as usize).copied().unwrap_or(0);
    }
    (high_bit(ml_base) as u8).wrapping_add(36)
}

/// `ofCode`.
pub fn of_code(offset: u32) -> u8 {
    (len32(offset) as u8).wrapping_sub(1)
}

/// `blockHeader`.
#[derive(Debug, Clone, Copy, Default)]
struct BlockHeader(u32);

const BLOCK_TYPE_RAW: u32 = 0;
const BLOCK_TYPE_RLE: u32 = 1;
const BLOCK_TYPE_COMPRESSED: u32 = 2;

impl BlockHeader {
    fn set_last(&mut self, b: bool) {
        if b {
            self.0 |= 1;
        } else {
            self.0 &= (1 << 24) - 2;
        }
    }

    fn set_size(&mut self, v: u32) {
        self.0 = (self.0 & 7) | v.wrapping_shl(3);
    }

    fn set_type(&mut self, t: u32) {
        const MASK: u32 = 1 | (((1 << 24) - 1) ^ 7);
        self.0 = (self.0 & MASK) | (t << 1);
    }

    fn bytes(self) -> [u8; 3] {
        [self.0 as u8, (self.0 >> 8) as u8, (self.0 >> 16) as u8]
    }
}

/// `literalsHeader`.
#[derive(Debug, Clone, Copy, Default)]
struct LiteralsHeader(u64);

const LITERALS_RAW: u64 = 0;
const LITERALS_RLE: u64 = 1;
const LITERALS_COMPRESSED: u64 = 2;
const LITERALS_TREELESS: u64 = 3;

impl LiteralsHeader {
    fn set_type(&mut self, t: u64) {
        self.0 = (self.0 & (u64::MAX - 3)) | t;
    }

    /// `setSize`: one size, of raw or RLE literals.
    fn set_size(&mut self, regen: usize) -> Result<(), Error> {
        let in_bits = len32(regen as u32);
        let mut lh = self.0 & 3;
        let regen = regen as u64;
        match in_bits {
            0..=4 => lh |= (regen << 3) | (1 << 60),
            5..=11 => lh |= (1 << 2) | (regen << 4) | (2 << 60),
            12..=19 => lh |= (3 << 2) | (regen << 4) | (3 << 60),
            _ => return Err(Error::Internal("block too big")),
        }
        self.0 = lh;
        Ok(())
    }

    /// `setSizes`: the compressed size and the size it regenerates.
    fn set_sizes(&mut self, comp: usize, input: usize, single: bool) -> Result<(), Error> {
        let (comp_bits, in_bits) = (len32(comp as u32), len32(input as u32));
        let mut lh = self.0 & 3;
        let (comp, input) = (comp as u64, input as u64);
        if comp_bits <= 10 && in_bits <= 10 {
            if !single {
                lh |= 1 << 2;
            }
            lh |= (input << 4) | (comp << (10 + 4)) | (3 << 60);
        } else if comp_bits <= 14 && in_bits <= 14 {
            if single {
                return Err(Error::Internal(
                    "single stream used with more than 10 bits length",
                ));
            }
            lh |= (2 << 2) | (input << 4) | (comp << (14 + 4)) | (4 << 60);
        } else if comp_bits <= 18 && in_bits <= 18 {
            if single {
                return Err(Error::Internal(
                    "single stream used with more than 10 bits length",
                ));
            }
            lh |= (3 << 2) | (input << 4) | (comp << (18 + 4)) | (5 << 60);
        } else {
            return Err(Error::Internal("block too big"));
        }
        self.0 = lh;
        Ok(())
    }

    fn size(self) -> usize {
        (self.0 >> 60) as usize
    }

    fn append_to(self, b: &mut Vec<u8>) {
        let n = self.size().min(5);
        b.extend((0..n).map(|i| (self.0 >> (8 * i)) as u8));
    }
}

/// What failed in encoding a block: the Huffman coder's, or an internal check's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Internal(&'static str),
}

impl From<&'static str> for Error {
    fn from(s: &'static str) -> Error {
        Error::Internal(s)
    }
}

/// Which table a sequence type is coded with (`seqCompMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    /// The block's own (a new table, or RLE): `compModeFSE` or `compModeRLE`.
    Current,
    /// The table before: `compModeRepeat`.
    Previous,
    /// `compModePredefined`.
    Predefined,
}

/// `seqCoders`: each type's current encoder and the one before.
#[derive(Debug)]
struct Coders {
    ll: Box<FseEncoder>,
    of: Box<FseEncoder>,
    ml: Box<FseEncoder>,
    ll_prev: Box<FseEncoder>,
    of_prev: Box<FseEncoder>,
    ml_prev: Box<FseEncoder>,
}

/// `compareSwap`: the encoder used becomes the one before.
fn compare_swap(used: Choice, current: &mut Box<FseEncoder>, prev: &mut Box<FseEncoder>) {
    match used {
        Choice::Current => {
            std::mem::swap(current, prev);
            current.re_used = false;
            prev.re_used = true;
        }
        Choice::Previous => {}
        Choice::Predefined => prev.symbol_len = 0,
    }
}

/// `chooseComp`: the predefined table where it is no worse, else the one before, else a
/// new one.
fn choose(cur: &FseEncoder, prev: &FseEncoder, predef: &FseEncoder) -> Choice {
    let hist = cur.count.get(..usize::from(cur.symbol_len)).unwrap_or_default();
    let mut n_size = cur.approx_size(hist).wrapping_add(cur.max_header_size());
    let predef_size = predef.approx_size(hist);
    let prev_size = prev.approx_size(hist);
    n_size = n_size.wrapping_add(n_size.wrapping_add(2 * 8 * 16) >> 4);
    if predef_size <= prev_size && predef_size <= n_size {
        Choice::Predefined
    } else if prev_size <= n_size {
        Choice::Previous
    } else {
        Choice::Current
    }
}

fn mode(c: Choice, rle: bool) -> u8 {
    match (c, rle) {
        (_, true) => 1,
        (Choice::Predefined, _) => 0,
        (Choice::Current, _) => 2,
        (Choice::Previous, _) => 3,
    }
}

/// `blockEnc`.
#[derive(Debug)]
pub struct BlockEnc {
    pub size: usize,
    pub literals: Vec<u8>,
    pub sequences: Vec<Seq>,
    coders: Coders,
    lit_enc: Huff,
    pub extra_lits: usize,
    pub output: Vec<u8>,
    pub recent_offsets: [u32; 3],
    prev_recent_offsets: [u32; 3],
    pub last: bool,
}

impl BlockEnc {
    /// `init`, then `initNewEncode`.
    pub fn new() -> BlockEnc {
        let fresh = || Box::new(FseEncoder::default());
        let mut b = BlockEnc {
            size: 0,
            literals: Vec::with_capacity(128 << 10),
            sequences: Vec::with_capacity(2000),
            coders: Coders {
                ll: fresh(),
                of: fresh(),
                ml: fresh(),
                ll_prev: fresh(),
                of_prev: fresh(),
                ml_prev: fresh(),
            },
            lit_enc: Huff::new(),
            extra_lits: 0,
            output: Vec::with_capacity(128 << 10),
            recent_offsets: [0; 3],
            prev_recent_offsets: [0; 3],
            last: false,
        };
        b.init_new_encode();
        b
    }

    /// `initNewEncode`: the offsets and encoders a frame starts with.
    pub fn init_new_encode(&mut self) {
        self.recent_offsets = [1, 4, 8];
        self.lit_enc.reuse = Reuse::None;
        let c = &mut self.coders;
        for (cur, prev) in [
            (&mut c.ll, &mut c.ll_prev),
            (&mut c.ml, &mut c.ml_prev),
            (&mut c.of, &mut c.of_prev),
        ] {
            compare_swap(Choice::Predefined, cur, prev);
        }
    }

    /// `reset`: a new block of the same frame.
    pub fn reset(&mut self) {
        self.extra_lits = 0;
        self.literals.clear();
        self.size = 0;
        self.sequences.clear();
        self.output.clear();
        self.last = false;
    }

    /// `popOffsets`.
    fn pop_offsets(&mut self) {
        self.recent_offsets = self.prev_recent_offsets;
    }

    fn header(&self, size: u32, t: u32) -> BlockHeader {
        let mut bh = BlockHeader::default();
        bh.set_last(self.last);
        bh.set_size(size);
        bh.set_type(t);
        bh
    }

    /// `encodeRaw`: `a` as the whole output.
    pub fn encode_raw(&mut self, a: &[u8]) {
        let bh = self.header(a.len() as u32, BLOCK_TYPE_RAW);
        self.output.clear();
        self.output.extend_from_slice(&bh.bytes());
        self.output.extend_from_slice(a);
    }

    /// `encodeRLE`.
    fn encode_rle(&mut self, val: u8, length: u32) {
        let bh = self.header(length, BLOCK_TYPE_RLE);
        self.output.extend_from_slice(&bh.bytes());
        self.output.push(val);
    }

    /// `encodeLits`: a block of literals alone.
    fn encode_lits(&mut self, lits: &[u8], raw: bool) -> Result<(), Error> {
        let mut bh = self.header(lits.len() as u32, BLOCK_TYPE_RAW);
        let raw_block = |b: &mut BlockEnc| {
            b.output.extend_from_slice(&bh.bytes());
            b.output.extend_from_slice(lits);
            Ok(())
        };
        if lits.len() < 32 || raw {
            return raw_block(self);
        }
        let (mut single, mut r) = (false, Err(HuffError::Incompressible));
        if lits.len() >= 1024 {
            r = self.lit_enc.compress(lits, true);
        } else if lits.len() > 16 {
            single = true;
            r = self.lit_enc.compress(lits, false);
        }
        if let Ok((out, _)) = &r
            && out.len() + 5 > lits.len()
        {
            let mut lh = LiteralsHeader::default();
            lh.set_sizes(out.len(), lits.len(), single)?;
            if out.len() + lh.size() >= lits.len() {
                r = Err(HuffError::Incompressible);
            }
        }
        let (out, re_used) = match r {
            Err(HuffError::Incompressible) => return raw_block(self),
            Err(HuffError::UseRle) => {
                bh.set_type(BLOCK_TYPE_RLE);
                self.output.extend_from_slice(&bh.bytes());
                self.output.push(lits.first().copied().unwrap_or(0));
                return Ok(());
            }
            Err(HuffError::Internal(e)) => return Err(Error::Internal(e)),
            Ok(v) => v,
        };
        self.lit_enc.reuse = Reuse::Allow;
        bh.set_type(BLOCK_TYPE_COMPRESSED);
        let mut lh = LiteralsHeader::default();
        lh.set_type(if re_used {
            LITERALS_TREELESS
        } else {
            LITERALS_COMPRESSED
        });
        lh.set_sizes(out.len(), lits.len(), single)?;
        bh.set_size((out.len() + lh.size() + 1) as u32);
        self.output.extend_from_slice(&bh.bytes());
        lh.append_to(&mut self.output);
        self.output.extend_from_slice(&out);
        self.output.push(0);
        Ok(())
    }

    /// `genCodes`: each sequence's codes, and each type's histogram.
    fn gen_codes(&mut self) {
        let c = &mut self.coders;
        c.ll.count = [0; 256];
        c.of.count = [0; 256];
        c.ml.count = [0; 256];
        let (mut ll_max, mut of_max, mut ml_max) = (0u8, 0u8, 0u8);
        for seq in &mut self.sequences {
            seq.ll_code = ll_code(seq.lit_len);
            seq.of_code = of_code(seq.offset);
            seq.ml_code = ml_code(seq.match_len);
            for (code, hist, max) in [
                (seq.ll_code, &mut c.ll.count, &mut ll_max),
                (seq.of_code, &mut c.of.count, &mut of_max),
                (seq.ml_code, &mut c.ml.count, &mut ml_max),
            ] {
                if let Some(h) = hist.get_mut(usize::from(code)) {
                    *h += 1;
                }
                if code > *max {
                    *max = code;
                }
            }
        }
        let top = |h: &[u32; 256], m: u8| h.iter().take(usize::from(m) + 1).copied().max().unwrap_or(0);
        let ml_top = top(&c.ml.count, ml_max);
        c.ml.histogram_finished(ml_max, ml_top as usize);
        let of_top = top(&c.of.count, of_max);
        c.of.histogram_finished(of_max, of_top as usize);
        let ll_top = top(&c.ll.count, ll_max);
        c.ll.histogram_finished(ll_max, ll_top as usize);
    }

    /// `encode`: the block, after `output`. `org` is its input; `raw` codes literals raw,
    /// and `raw_all_lits` a block of literals alone.
    pub fn encode(
        &mut self,
        org: &[u8],
        raw: bool,
        raw_all_lits: bool,
        predef: &[FseEncoder; 3],
    ) -> Result<(), Error> {
        if self.sequences.is_empty() {
            let lits = std::mem::take(&mut self.literals);
            let r = self.encode_lits(&lits, raw_all_lits);
            self.literals = lits;
            return r;
        }
        if let ([seq], false, true) = (
            self.sequences.as_slice(),
            org.is_empty(),
            self.literals.len() <= 1,
        ) && seq.lit_len == self.literals.len() as u32
            && seq.offset.wrapping_sub(3) == 1
        {
            let length = seq
                .match_len
                .wrapping_add(ZSTD_MIN_MATCH)
                .wrapping_add(seq.lit_len);
            self.encode_rle(org.first().copied().unwrap_or(0), length);
            return Ok(());
        }
        let saved = self.size as i64 - self.literals.len() as i64 - (self.size >> 6) as i64;
        if saved < 16 {
            self.pop_offsets();
            return self.encode_lits(org, raw_all_lits);
        }
        let mut bh = BlockHeader::default();
        bh.set_last(self.last);
        bh.set_type(BLOCK_TYPE_COMPRESSED);
        let bh_offset = self.output.len();
        self.output.extend_from_slice(&bh.bytes());

        let lits = std::mem::take(&mut self.literals);
        let (mut single, mut r) = (false, Err(HuffError::Incompressible));
        if lits.len() >= 1024 && !raw {
            r = self.lit_enc.compress(&lits, true);
        } else if lits.len() > 16 && !raw {
            single = true;
            r = self.lit_enc.compress(&lits, false);
        }
        if let Ok((out, _)) = &r
            && out.len() + 5 > lits.len()
        {
            let mut lh = LiteralsHeader::default();
            lh.set_size(lits.len())?;
            let sz_raw = lh.size();
            lh.set_sizes(out.len(), lits.len(), single)?;
            if out.len() + lh.size() >= lits.len() + sz_raw {
                r = Err(HuffError::Incompressible);
            }
        }
        let mut lh = LiteralsHeader::default();
        match r {
            Err(HuffError::Incompressible) => {
                lh.set_type(LITERALS_RAW);
                lh.set_size(lits.len())?;
                lh.append_to(&mut self.output);
                self.output.extend_from_slice(&lits);
            }
            Err(HuffError::UseRle) => {
                lh.set_type(LITERALS_RLE);
                lh.set_size(lits.len())?;
                lh.append_to(&mut self.output);
                self.output.push(lits.first().copied().unwrap_or(0));
            }
            Err(HuffError::Internal(e)) => {
                self.literals = lits;
                return Err(Error::Internal(e));
            }
            Ok((out, re_used)) => {
                lh.set_type(if re_used {
                    LITERALS_TREELESS
                } else {
                    LITERALS_COMPRESSED
                });
                lh.set_sizes(out.len(), lits.len(), single)?;
                lh.append_to(&mut self.output);
                self.output.extend_from_slice(&out);
                self.lit_enc.reuse = Reuse::Allow;
            }
        }
        self.literals = lits;

        let n = self.sequences.len();
        if n < 128 {
            self.output.push(n as u8);
        } else if n < 0x7f00 {
            self.output.push(128u8.wrapping_add((n >> 8) as u8));
            self.output.push(n as u8);
        } else {
            let m = n - 0x7f00;
            self.output.extend_from_slice(&[255, m as u8, (m >> 8) as u8]);
        }
        self.gen_codes();
        self.coders.ll.normalize_count(n)?;
        self.coders.of.normalize_count(n)?;
        self.coders.ml.normalize_count(n)?;

        let [ll_pre, of_pre, ml_pre] = predef;
        let c = &mut self.coders;
        let first = self.sequences.first().copied().unwrap_or_default();
        let choose_or_rle = |cur: &mut Box<FseEncoder>, prev: &FseEncoder, pre: &FseEncoder, code: u8| {
            if cur.use_rle {
                cur.set_rle(code);
                (Choice::Current, true)
            } else {
                (choose(cur, prev, pre), false)
            }
        };
        let (ll_c, ll_rle) = choose_or_rle(&mut c.ll, &c.ll_prev, ll_pre, first.ll_code);
        let (of_c, of_rle) = choose_or_rle(&mut c.of, &c.of_prev, of_pre, first.of_code);
        let (ml_c, ml_rle) = choose_or_rle(&mut c.ml, &c.ml_prev, ml_pre, first.ml_code);
        self.output
            .push((mode(ll_c, ll_rle) << 6) | (mode(of_c, of_rle) << 4) | (mode(ml_c, ml_rle) << 2));
        for (ch, cur, prev, pre) in [
            (ll_c, &c.ll, &c.ll_prev, ll_pre),
            (of_c, &c.of, &c.of_prev, of_pre),
            (ml_c, &c.ml, &c.ml_prev, ml_pre),
        ] {
            select(ch, cur, prev, pre).write_count(&mut self.output)?;
        }
        // The output bits of the tables this block makes.
        if ll_c == Choice::Current {
            c.ll.set_bits(Some(&LL_BITS_TABLE));
            c.ll.zero_rle_state();
        }
        if ml_c == Choice::Current {
            c.ml.set_bits(Some(&ML_BITS_TABLE));
            c.ml.zero_rle_state();
        }
        if of_c == Choice::Current {
            c.of.set_bits(None);
            c.of.zero_rle_state();
        }
        let ll_enc = select(ll_c, &c.ll, &c.ll_prev, ll_pre);
        let of_enc = select(of_c, &c.of, &c.of_prev, of_pre);
        let ml_enc = select(ml_c, &c.ml, &c.ml_prev, ml_pre);

        let mut wr = BitWriter::new(std::mem::take(&mut self.output));
        let tt =
            |e: &FseEncoder, code: u8| e.ct.symbol_tt.get(usize::from(code)).copied().unwrap_or_default();
        let last = self.sequences.last().copied().unwrap_or_default();
        let (ll_b, of_b, ml_b) = (
            tt(ll_enc, last.ll_code),
            tt(of_enc, last.of_code),
            tt(ml_enc, last.ml_code),
        );
        let mut ll = CState::init(&ll_enc.ct, ll_b);
        let mut of = CState::init(&of_enc.ct, of_b);
        wr.flush32();
        let mut ml = CState::init(&ml_enc.ct, ml_b);
        wr.add32(last.lit_len, ll_b.out_bits);
        wr.add32(last.match_len, ml_b.out_bits);
        wr.flush32();
        wr.add32(last.offset, of_b.out_bits);
        for s in self.sequences.iter().rev().skip(1) {
            let of_b: SymbolTransform = tt(of_enc, s.of_code);
            wr.flush32();
            of.encode(&mut wr, &of_enc.ct.state_table, of_b);
            let out_bits = of_b.out_bits & 31;
            let mut extra = u64::from(s.offset & mask32(out_bits));
            let mut extra_n = out_bits;
            let ml_b = tt(ml_enc, s.ml_code);
            ml.encode(&mut wr, &ml_enc.ct.state_table, ml_b);
            let out_bits = ml_b.out_bits & 31;
            extra = (extra << out_bits) | u64::from(s.match_len & mask32(out_bits));
            extra_n += out_bits;
            let ll_b = tt(ll_enc, s.ll_code);
            ll.encode(&mut wr, &ll_enc.ct.state_table, ll_b);
            let out_bits = ll_b.out_bits & 31;
            extra = (extra << out_bits) | u64::from(s.lit_len & mask32(out_bits));
            extra_n += out_bits;
            wr.flush32();
            wr.add64(extra, extra_n);
        }
        ml.flush(&mut wr, ml_enc.actual_table_log);
        of.flush(&mut wr, of_enc.actual_table_log);
        ll.flush(&mut wr, ll_enc.actual_table_log);
        wr.close();
        self.output = wr.out;

        if self.output.len() - 3 - bh_offset >= self.size {
            // Raw: no smaller coded.
            self.output.truncate(bh_offset);
            let raw_bh = self.header(org.len() as u32, BLOCK_TYPE_RAW);
            self.output.extend_from_slice(&raw_bh.bytes());
            self.output.extend_from_slice(org);
            self.pop_offsets();
            self.lit_enc.reuse = Reuse::None;
            return Ok(());
        }
        bh.set_size((self.output.len() - bh_offset - 3) as u32);
        for (o, b) in self.output.iter_mut().skip(bh_offset).zip(bh.bytes()) {
            *o = b;
        }
        let c = &mut self.coders;
        compare_swap(ll_c, &mut c.ll, &mut c.ll_prev);
        compare_swap(ml_c, &mut c.ml, &mut c.ml_prev);
        compare_swap(of_c, &mut c.of, &mut c.of_prev);
        Ok(())
    }
}

/// The encoder `c` names.
fn select<'a>(c: Choice, cur: &'a FseEncoder, prev: &'a FseEncoder, pre: &'a FseEncoder) -> &'a FseEncoder {
    match c {
        Choice::Current => cur,
        Choice::Previous => prev,
        Choice::Predefined => pre,
    }
}
