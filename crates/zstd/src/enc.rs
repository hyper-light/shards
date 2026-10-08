//! The match finders (zstd/enc_base.go, enc_fast.go, enc_dfast.go, enc_better.go,
//! enc_best.go): each turns a block into literals and sequences against the history the
//! frame keeps, `Encode`, or against the block alone, `EncodeNoHist`.

use crate::block::{BlockEnc, ML_BITS_TABLE, Seq, ml_code, of_code};
use crate::entropy::shannon_entropy_bits;
use crate::fse::FseEncoder;
use crate::util::{byte, hash_len, load32, load64, match_at, match_len};
use crate::xxhash::Xxh64;

/// `maxCompressedBlockSize`.
pub const MAX_COMPRESSED_BLOCK_SIZE: usize = 128 << 10;
const ZSTD_MIN_MATCH: i32 = 3;
/// `maxMatchLength`, and the decoder's `maxMatchLen` the best encoder scores by.
const MAX_MATCH_LENGTH: i32 = 131_074;

/// `fastBase`: the history, its position, the checksum and the block.
#[derive(Debug)]
pub struct Base {
    pub cur: i32,
    max_match_off: i32,
    buffer_reset: i32,
    hist: Vec<u8>,
    /// `cap(e.hist)`, which decides when history moves down.
    hist_cap: usize,
    pub crc: Xxh64,
    pub blk: BlockEnc,
}

impl Base {
    fn new(window: i32) -> Base {
        Base {
            cur: 0,
            max_match_off: window,
            buffer_reset: i32::MAX - window.wrapping_mul(2),
            hist: Vec::new(),
            hist_cap: 0,
            crc: Xxh64::default(),
            blk: BlockEnc::new(),
        }
    }

    /// `AppendCRC`: the checksum's low 32 bits, little endian.
    pub fn append_crc(&self, dst: &mut Vec<u8>) {
        dst.extend_from_slice(&(self.crc.sum64() as u32).to_le_bytes());
    }

    /// `WindowSize`.
    pub fn window_size(&self, size: i64) -> i32 {
        if size > 0 && size < i64::from(self.max_match_off) {
            let bits = 64 - (size as u64).leading_zeros();
            return (1i32.checked_shl(bits).unwrap_or(i32::MAX)).max(1024);
        }
        self.max_match_off
    }

    /// `addBlock`: `src` after the history, moving the history down first where it would
    /// not fit; where `src` starts.
    fn add_block(&mut self, src: &[u8]) -> i32 {
        if self.hist.len() + src.len() > self.hist_cap {
            if self.hist_cap == 0 {
                self.ensure_hist(src.len());
            } else {
                let mmo = self.max_match_off as usize;
                let offset = self.hist.len().saturating_sub(mmo);
                self.hist.copy_within(offset.., 0);
                self.hist.truncate(mmo);
                self.cur = self.cur.wrapping_add(offset as i32);
            }
        }
        let s = self.hist.len() as i32;
        self.hist.extend_from_slice(src);
        self.hist_cap = self.hist_cap.max(self.hist.len());
        s
    }

    /// `ensureHist`: an empty history of the capacity a window needs, at least `n`.
    fn ensure_hist(&mut self, n: usize) {
        if self.hist_cap >= n {
            return;
        }
        let mmo = self.max_match_off as usize;
        let mut l = mmo
            + if mmo <= MAX_COMPRESSED_BLOCK_SIZE {
                MAX_COMPRESSED_BLOCK_SIZE
            } else {
                mmo
            };
        l = l.max(1 << 20).max(n);
        self.hist = Vec::with_capacity(l);
        self.hist_cap = l;
    }

    /// `resetBase` without a dictionary.
    fn reset(&mut self) {
        self.blk.reset();
        self.blk.init_new_encode();
        self.crc = Xxh64::default();
        if self.cur < self.buffer_reset {
            self.cur = self
                .cur
                .wrapping_add(self.max_match_off.wrapping_add(self.hist.len() as i32));
        }
        self.hist.clear();
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct TableEntry {
    val: u32,
    offset: i32,
}

#[derive(Debug, Clone, Copy, Default)]
struct PrevEntry {
    offset: i32,
    prev: i32,
}

fn at<T: Copy + Default>(t: &[T], h: u32) -> T {
    t.get(h as usize).copied().unwrap_or_default()
}

/// A new entry at `h`, the one there before its `prev`.
fn push(t: &mut [PrevEntry], h: u32, offset: i32) {
    if let Some(e) = t.get_mut(h as usize) {
        *e = PrevEntry {
            offset,
            prev: e.offset,
        };
    }
}

fn put<T>(t: &mut [T], h: u32, v: T) {
    if let Some(e) = t.get_mut(h as usize) {
        *e = v;
    }
}

/// An offset kept across a move of the history, or 0 where it falls out of the window.
fn moved(v: i32, min_off: i32, cur: i32, mmo: i32) -> i32 {
    if v < min_off {
        0
    } else {
        v.wrapping_sub(cur).wrapping_add(mmo)
    }
}

fn rebase_table(t: &mut [TableEntry], min_off: i32, cur: i32, mmo: i32) {
    for e in t {
        e.offset = moved(e.offset, min_off, cur, mmo);
    }
}

fn rebase_prev(t: &mut [PrevEntry], min_off: i32, cur: i32, mmo: i32) {
    for e in t {
        *e = if e.offset < min_off {
            PrevEntry::default()
        } else {
            PrevEntry {
                offset: moved(e.offset, min_off, cur, mmo),
                prev: moved(e.prev, min_off, cur, mmo),
            }
        };
    }
}

/// `src[a:b]`, empty where out of range.
fn span(src: &[u8], a: i32, b: i32) -> &[u8] {
    match (usize::try_from(a), usize::try_from(b)) {
        (Ok(a), Ok(b)) => src.get(a..b).unwrap_or_default(),
        _ => &[],
    }
}

/// A block too short to search: all literals.
fn literal_block(blk: &mut BlockEnc, src: &[u8]) {
    blk.extra_lits = src.len();
    blk.literals.clear();
    blk.literals.extend_from_slice(src);
}

/// The literals from `next_emit` to the end.
fn last_literals(blk: &mut BlockEnc, src: &[u8], next_emit: i32) {
    if (next_emit as usize) < src.len() {
        blk.literals
            .extend_from_slice(span(src, next_emit, src.len() as i32));
        blk.extra_lits = src.len() - next_emit as usize;
    }
}

/// `addLiterals`: the literals up to `until`, and their count.
fn add_literals(blk: &mut BlockEnc, src: &[u8], next_emit: i32, until: i32) -> u32 {
    if until == next_emit {
        return 0;
    }
    blk.literals.extend_from_slice(span(src, next_emit, until));
    (until - next_emit) as u32
}

fn seq(lit_len: u32, match_len: i32, offset: u32) -> Seq {
    Seq::new(lit_len, match_len.wrapping_sub(ZSTD_MIN_MATCH) as u32, offset)
}

/// The level's match finder.
#[derive(Debug)]
enum Kind {
    Fast(Box<Fast>),
    DFast(Box<DFast>),
    Better(Box<Better>),
    Best(Box<Best>),
}

/// An encoder: a match finder and its base.
#[derive(Debug)]
pub struct Encoder {
    pub base: Base,
    kind: Kind,
}

/// `EncoderLevel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Fastest,
    Default,
    Better,
    Best,
}

impl Encoder {
    /// `encoderOptions.encoder`.
    pub fn new(level: Level, window: i32) -> Encoder {
        let kind = match level {
            Level::Fastest => Kind::Fast(Box::new(Fast::new())),
            Level::Default => Kind::DFast(Box::new(DFast {
                fast: Fast::new(),
                long: vec![TableEntry::default(); 1 << DFAST_LONG_BITS],
            })),
            Level::Better => Kind::Better(Box::new(Better {
                table: vec![TableEntry::default(); 1 << BETTER_SHORT_BITS],
                long: vec![PrevEntry::default(); 1 << BETTER_LONG_BITS],
            })),
            Level::Best => Kind::Best(Box::new(Best {
                table: vec![PrevEntry::default(); 1 << BEST_SHORT_BITS],
                long: vec![PrevEntry::default(); 1 << BEST_LONG_BITS],
            })),
        };
        Encoder {
            base: Base::new(window),
            kind,
        }
    }

    /// `Reset` without a dictionary.
    pub fn reset(&mut self) {
        self.base.reset();
    }

    /// `Encode`: `src` against the history.
    pub fn encode(&mut self, src: &[u8], predef: &[FseEncoder; 3]) {
        let b = &mut self.base;
        match &mut self.kind {
            Kind::Fast(f) => f.encode(b, src, false),
            Kind::DFast(d) => d.encode(b, src, false),
            Kind::Better(e) => e.encode(b, src),
            Kind::Best(e) => e.encode(b, src, predef),
        }
    }

    /// `EncodeNoHist`: `src`, a frame's only block, alone.
    pub fn encode_no_hist(&mut self, src: &[u8], predef: &[FseEncoder; 3]) {
        let b = &mut self.base;
        match &mut self.kind {
            Kind::Fast(f) => f.encode(b, src, true),
            Kind::DFast(d) => d.encode(b, src, true),
            Kind::Better(e) => {
                b.ensure_hist(src.len());
                e.encode(b, src);
            }
            Kind::Best(e) => {
                b.ensure_hist(src.len());
                e.encode(b, src, predef);
            }
        }
    }
}

const TABLE_BITS: u32 = 15;
const TABLE_FAST_HASH_LEN: u32 = 6;

/// `fastEncoder`.
#[derive(Debug)]
struct Fast {
    table: Vec<TableEntry>,
}

impl Fast {
    fn new() -> Fast {
        Fast {
            table: vec![TableEntry::default(); 1 << TABLE_BITS],
        }
    }

    /// `Encode`, and `EncodeNoHist` where `no_hist`.
    fn encode(&mut self, b: &mut Base, input: &[u8], no_hist: bool) {
        const INPUT_MARGIN: i32 = 8;
        const MIN_NON_LITERAL_BLOCK_SIZE: usize = 1 + 1 + INPUT_MARGIN as usize;
        const STEP_SIZE: i32 = 2;
        const K_SEARCH_STRENGTH: i32 = 6;
        let s0 = if no_hist {
            if b.cur >= b.buffer_reset {
                self.table.fill(TableEntry::default());
                b.cur = b.max_match_off;
            }
            0
        } else {
            if b.cur >= b.buffer_reset.wrapping_sub(b.hist.len() as i32) {
                if b.hist.is_empty() {
                    self.table.fill(TableEntry::default());
                } else {
                    let min_off = b
                        .cur
                        .wrapping_add(b.hist.len() as i32)
                        .wrapping_sub(b.max_match_off);
                    rebase_table(&mut self.table, min_off, b.cur, b.max_match_off);
                }
                b.cur = b.max_match_off;
            }
            b.add_block(input)
        };
        let Base {
            cur,
            max_match_off,
            buffer_reset,
            hist,
            blk,
            ..
        } = b;
        let (e_cur, mmo) = (*cur, *max_match_off);
        blk.size = input.len();
        if input.len() < MIN_NON_LITERAL_BLOCK_SIZE {
            literal_block(blk, input);
            return;
        }
        let src: &[u8] = if no_hist { input } else { hist };
        let table = &mut self.table;
        let s_limit = src.len() as i32 - INPUT_MARGIN;
        let mut s = s0;
        let mut next_emit = s;
        let mut cv = load64(src, s);
        let mut offset1 = blk.recent_offsets[0] as i32;
        let mut offset2 = blk.recent_offsets[1] as i32;

        'encode: loop {
            let mut t: i32;
            let can_repeat = blk.sequences.len() > 2;
            loop {
                let rep_ok = if no_hist {
                    blk.sequences.len() > 2
                } else {
                    can_repeat
                };
                let next_hash = hash_len(cv, TABLE_BITS, TABLE_FAST_HASH_LEN);
                let next_hash2 = hash_len(cv >> 8, TABLE_BITS, TABLE_FAST_HASH_LEN);
                let candidate = at(table, next_hash);
                let candidate2 = at(table, next_hash2);
                let mut rep_index = s - offset1 + 2;
                put(
                    table,
                    next_hash,
                    TableEntry {
                        offset: s.wrapping_add(e_cur),
                        val: cv as u32,
                    },
                );
                put(
                    table,
                    next_hash2,
                    TableEntry {
                        offset: s.wrapping_add(e_cur).wrapping_add(1),
                        val: (cv >> 8) as u32,
                    },
                );
                if rep_ok && rep_index >= 0 && load32(src, rep_index) == (cv >> 16) as u32 {
                    let length = 4 + match_at(src, s + 6, rep_index + 4);
                    let mut match_len = (length - ZSTD_MIN_MATCH) as u32;
                    let mut start = s + 2;
                    let start_limit = next_emit + 1;
                    let s_min = (s - mmo).max(0);
                    while rep_index > s_min
                        && start > start_limit
                        && byte(src, rep_index - 1) == byte(src, start - 1)
                        && (no_hist || match_len < (MAX_MATCH_LENGTH - ZSTD_MIN_MATCH) as u32)
                    {
                        rep_index -= 1;
                        start -= 1;
                        match_len += 1;
                    }
                    let lit_len = add_literals(blk, src, next_emit, start);
                    blk.sequences.push(Seq::new(lit_len, match_len, 1));
                    s += length + 2;
                    next_emit = s;
                    if s >= s_limit {
                        break 'encode;
                    }
                    cv = load64(src, s);
                    continue;
                }
                let coffset0 = s - candidate.offset.wrapping_sub(e_cur);
                let coffset1 = s - candidate2.offset.wrapping_sub(e_cur) + 1;
                if coffset0 < mmo && cv as u32 == candidate.val {
                    t = candidate.offset.wrapping_sub(e_cur);
                    break;
                }
                if coffset1 < mmo && (cv >> 8) as u32 == candidate2.val {
                    t = candidate2.offset.wrapping_sub(e_cur);
                    s += 1;
                    break;
                }
                s += STEP_SIZE + ((s - next_emit) >> (K_SEARCH_STRENGTH - 1));
                if s >= s_limit {
                    break 'encode;
                }
                cv = load64(src, s);
            }
            offset2 = offset1;
            offset1 = s - t;
            let mut l = match_at(src, s + 4, t + 4) + 4;
            let t_min = (s - mmo).max(0);
            while t > t_min
                && s > next_emit
                && byte(src, t - 1) == byte(src, s - 1)
                && (no_hist || l < MAX_MATCH_LENGTH)
            {
                s -= 1;
                t -= 1;
                l += 1;
            }
            let lit_len = (s - next_emit) as u32;
            if lit_len > 0 {
                blk.literals.extend_from_slice(span(src, next_emit, s));
            }
            blk.sequences.push(seq(lit_len, l, (s - t) as u32 + 3));
            s += l;
            next_emit = s;
            if s >= s_limit {
                break 'encode;
            }
            cv = load64(src, s);
            let o2 = s - offset2;
            let rep_ok = if no_hist {
                blk.sequences.len() > 2
            } else {
                can_repeat
            };
            if rep_ok && load32(src, o2) == cv as u32 {
                let l = 4 + match_at(src, s + 4, o2 + 4);
                let next_hash = hash_len(cv, TABLE_BITS, TABLE_FAST_HASH_LEN);
                put(
                    table,
                    next_hash,
                    TableEntry {
                        offset: s.wrapping_add(e_cur),
                        val: cv as u32,
                    },
                );
                blk.sequences.push(seq(0, l, 1));
                s += l;
                next_emit = s;
                std::mem::swap(&mut offset1, &mut offset2);
                if s >= s_limit {
                    break 'encode;
                }
                cv = load64(src, s);
            }
        }
        last_literals(blk, src, next_emit);
        if no_hist {
            if *cur < *buffer_reset {
                *cur = cur.wrapping_add(src.len() as i32);
            }
        } else {
            blk.recent_offsets[0] = offset1 as u32;
            blk.recent_offsets[1] = offset2 as u32;
        }
    }
}

const DFAST_LONG_BITS: u32 = 17;
const DFAST_LONG_LEN: u32 = 8;
const DFAST_SHORT_BITS: u32 = TABLE_BITS;
const DFAST_SHORT_LEN: u32 = 5;

/// `doubleFastEncoder`.
#[derive(Debug)]
struct DFast {
    fast: Fast,
    long: Vec<TableEntry>,
}

impl DFast {
    /// `Encode`, and `EncodeNoHist` where `no_hist`.
    fn encode(&mut self, b: &mut Base, input: &[u8], no_hist: bool) {
        const INPUT_MARGIN: i32 = 8 + 2;
        const MIN_NON_LITERAL_BLOCK_SIZE: usize = 16;
        const STEP_SIZE: i32 = 1;
        const K_SEARCH_STRENGTH: i32 = 8;
        const REP_OFF: i32 = 1;
        const CHECK_AT: i32 = 1;
        let s0 = if no_hist {
            if b.cur >= b.buffer_reset {
                self.fast.table.fill(TableEntry::default());
                self.long.fill(TableEntry::default());
                b.cur = b.max_match_off;
            }
            0
        } else {
            if b.cur >= b.buffer_reset.wrapping_sub(b.hist.len() as i32) {
                if b.hist.is_empty() {
                    self.fast.table.fill(TableEntry::default());
                    self.long.fill(TableEntry::default());
                } else {
                    let min_off = b
                        .cur
                        .wrapping_add(b.hist.len() as i32)
                        .wrapping_sub(b.max_match_off);
                    rebase_table(&mut self.fast.table, min_off, b.cur, b.max_match_off);
                    rebase_table(&mut self.long, min_off, b.cur, b.max_match_off);
                }
                b.cur = b.max_match_off;
            }
            b.add_block(input)
        };
        let Base {
            cur,
            max_match_off,
            buffer_reset,
            hist,
            blk,
            ..
        } = b;
        let (e_cur, mmo) = (*cur, *max_match_off);
        blk.size = input.len();
        if input.len() < MIN_NON_LITERAL_BLOCK_SIZE {
            literal_block(blk, input);
            return;
        }
        let src: &[u8] = if no_hist { input } else { hist };
        let (table, long) = (&mut self.fast.table, &mut self.long);
        let s_limit = src.len() as i32 - INPUT_MARGIN;
        let mut s = s0;
        let mut next_emit = s;
        let mut cv = load64(src, s);
        let mut offset1 = blk.recent_offsets[0] as i32;
        let mut offset2 = blk.recent_offsets[1] as i32;

        'encode: loop {
            let mut t: i32;
            let can_repeat = blk.sequences.len() > 2;
            loop {
                let next_hash_l = hash_len(cv, DFAST_LONG_BITS, DFAST_LONG_LEN);
                let next_hash_s = hash_len(cv, DFAST_SHORT_BITS, DFAST_SHORT_LEN);
                let candidate_l = at(long, next_hash_l);
                let candidate_s = at(table, next_hash_s);
                let mut rep_index = s - offset1 + REP_OFF;
                let entry = TableEntry {
                    offset: s.wrapping_add(e_cur),
                    val: cv as u32,
                };
                put(long, next_hash_l, entry);
                put(table, next_hash_s, entry);
                let rep_ok = if no_hist {
                    blk.sequences.len() > 2
                } else {
                    can_repeat
                };
                if rep_ok && rep_index >= 0 && load32(src, rep_index) == (cv >> (REP_OFF * 8)) as u32 {
                    let length = 4 + match_at(src, s + 4 + REP_OFF, rep_index + 4);
                    let mut match_len = (length - ZSTD_MIN_MATCH) as u32;
                    let mut start = s + REP_OFF;
                    let start_limit = next_emit + 1;
                    let t_min = (s - mmo).max(0);
                    while rep_index > t_min
                        && start > start_limit
                        && byte(src, rep_index - 1) == byte(src, start - 1)
                        && (no_hist || match_len < (MAX_MATCH_LENGTH - ZSTD_MIN_MATCH - 1) as u32)
                    {
                        rep_index -= 1;
                        start -= 1;
                        match_len += 1;
                    }
                    let lit_len = add_literals(blk, src, next_emit, start);
                    blk.sequences.push(Seq::new(lit_len, match_len, 1));
                    s += length + REP_OFF;
                    next_emit = s;
                    if s >= s_limit {
                        break 'encode;
                    }
                    cv = load64(src, s);
                    continue;
                }
                let coffset_l = s - candidate_l.offset.wrapping_sub(e_cur);
                let coffset_s = s - candidate_s.offset.wrapping_sub(e_cur);
                if coffset_l < mmo && cv as u32 == candidate_l.val {
                    t = candidate_l.offset.wrapping_sub(e_cur);
                    break;
                }
                if coffset_s < mmo && cv as u32 == candidate_s.val {
                    let cv = load64(src, s + CHECK_AT);
                    let next_hash_l = hash_len(cv, DFAST_LONG_BITS, DFAST_LONG_LEN);
                    let candidate_l = at(long, next_hash_l);
                    let coffset_l = s - candidate_l.offset.wrapping_sub(e_cur) + CHECK_AT;
                    put(
                        long,
                        next_hash_l,
                        TableEntry {
                            offset: (s + CHECK_AT).wrapping_add(e_cur),
                            val: cv as u32,
                        },
                    );
                    if coffset_l < mmo && cv as u32 == candidate_l.val {
                        t = candidate_l.offset.wrapping_sub(e_cur);
                        s += CHECK_AT;
                        break;
                    }
                    t = candidate_s.offset.wrapping_sub(e_cur);
                    break;
                }
                s += STEP_SIZE + ((s - next_emit) >> (K_SEARCH_STRENGTH - 1));
                if s >= s_limit {
                    break 'encode;
                }
                cv = load64(src, s);
            }
            offset2 = offset1;
            offset1 = s - t;
            let mut l = match_at(src, s + 4, t + 4) + 4;
            let t_min = (s - mmo).max(0);
            while t > t_min
                && s > next_emit
                && byte(src, t - 1) == byte(src, s - 1)
                && (no_hist || l < MAX_MATCH_LENGTH)
            {
                s -= 1;
                t -= 1;
                l += 1;
            }
            let lit_len = (s - next_emit) as u32;
            if lit_len > 0 {
                blk.literals.extend_from_slice(span(src, next_emit, s));
            }
            blk.sequences.push(seq(lit_len, l, (s - t) as u32 + 3));
            s += l;
            next_emit = s;
            if s >= s_limit {
                break 'encode;
            }
            let index0 = s - l + 1;
            let index1 = s - 2;
            let mut cv0 = load64(src, index0);
            let mut cv1 = load64(src, index1);
            let mut te0 = TableEntry {
                offset: index0.wrapping_add(e_cur),
                val: cv0 as u32,
            };
            let mut te1 = TableEntry {
                offset: index1.wrapping_add(e_cur),
                val: cv1 as u32,
            };
            put(long, hash_len(cv0, DFAST_LONG_BITS, DFAST_LONG_LEN), te0);
            put(long, hash_len(cv1, DFAST_LONG_BITS, DFAST_LONG_LEN), te1);
            cv0 >>= 8;
            cv1 >>= 8;
            te0.offset = te0.offset.wrapping_add(1);
            te1.offset = te1.offset.wrapping_add(1);
            te0.val = cv0 as u32;
            te1.val = cv1 as u32;
            put(table, hash_len(cv0, DFAST_SHORT_BITS, DFAST_SHORT_LEN), te0);
            put(table, hash_len(cv1, DFAST_SHORT_BITS, DFAST_SHORT_LEN), te1);
            cv = load64(src, s);
            let rep_ok = if no_hist {
                blk.sequences.len() > 2
            } else {
                can_repeat
            };
            if !rep_ok {
                continue;
            }
            loop {
                let o2 = s - offset2;
                if load32(src, o2) != cv as u32 {
                    break;
                }
                // EncodeNoHist hashes the short entry from `cv1 >> 8`, which Go does.
                let next_hash_s = if no_hist {
                    hash_len(cv1 >> 8, DFAST_SHORT_BITS, DFAST_SHORT_LEN)
                } else {
                    hash_len(cv, DFAST_SHORT_BITS, DFAST_SHORT_LEN)
                };
                let next_hash_l = hash_len(cv, DFAST_LONG_BITS, DFAST_LONG_LEN);
                let l = 4 + match_at(src, s + 4, o2 + 4);
                let entry = TableEntry {
                    offset: s.wrapping_add(e_cur),
                    val: cv as u32,
                };
                put(long, next_hash_l, entry);
                put(table, next_hash_s, entry);
                blk.sequences.push(seq(0, l, 1));
                s += l;
                next_emit = s;
                std::mem::swap(&mut offset1, &mut offset2);
                if s >= s_limit {
                    break 'encode;
                }
                cv = load64(src, s);
            }
        }
        last_literals(blk, src, next_emit);
        if no_hist {
            if *cur < *buffer_reset {
                *cur = cur.wrapping_add(src.len() as i32);
            }
        } else {
            blk.recent_offsets[0] = offset1 as u32;
            blk.recent_offsets[1] = offset2 as u32;
        }
    }
}

/// A block of one byte repeated, as the better and best encoders code it: a literal and
/// a match at offset 1.
fn rle_block(blk: &mut BlockEnc, input: &[u8]) -> bool {
    if input.len() > ZSTD_MIN_MATCH as usize {
        let ml = match_len(input.get(1..).unwrap_or_default(), input);
        if ml == input.len() - 1 {
            blk.literals.push(input.first().copied().unwrap_or(0));
            blk.sequences.push(Seq::new(
                1,
                (input.len() - 1) as u32 - ZSTD_MIN_MATCH as u32,
                1 + 3,
            ));
            return true;
        }
    }
    false
}

/// Index from `index0` up to `s - 1` every two bytes in the better encoder's tables.
fn better_index(
    table: &mut [TableEntry],
    long: &mut [PrevEntry],
    src: &[u8],
    mut index0: i32,
    s: i32,
    e_cur: i32,
) {
    while index0 < s - 1 {
        let cv0 = load64(src, index0);
        let cv1 = cv0 >> 8;
        let h0 = hash_len(cv0, BETTER_LONG_BITS, BETTER_LONG_LEN);
        let off = index0.wrapping_add(e_cur);
        push(long, h0, off);
        put(
            table,
            hash_len(cv1, BETTER_SHORT_BITS, BETTER_SHORT_LEN),
            TableEntry {
                offset: off.wrapping_add(1),
                val: cv1 as u32,
            },
        );
        index0 += 2;
    }
}

const BETTER_LONG_BITS: u32 = 19;
const BETTER_LONG_LEN: u32 = 8;
const BETTER_SHORT_BITS: u32 = 13;
const BETTER_SHORT_LEN: u32 = 5;

/// `betterFastEncoder`.
#[derive(Debug)]
struct Better {
    table: Vec<TableEntry>,
    long: Vec<PrevEntry>,
}

impl Better {
    /// `Encode`.
    fn encode(&mut self, b: &mut Base, input: &[u8]) {
        const INPUT_MARGIN: i32 = 8 + 2;
        const MIN_NON_LITERAL_BLOCK_SIZE: usize = 16;
        const STEP_SIZE: i32 = 1;
        const K_SEARCH_STRENGTH: i32 = 9;
        const REP_OFF: i32 = 1;
        const CHECK_AT: i32 = 1;
        const SKIP_BEGINNING: i32 = 3;
        if b.cur >= b.buffer_reset.wrapping_sub(b.hist.len() as i32) {
            if b.hist.is_empty() {
                self.table.fill(TableEntry::default());
                self.long.fill(PrevEntry::default());
            } else {
                let min_off = b
                    .cur
                    .wrapping_add(b.hist.len() as i32)
                    .wrapping_sub(b.max_match_off);
                rebase_table(&mut self.table, min_off, b.cur, b.max_match_off);
                rebase_prev(&mut self.long, min_off, b.cur, b.max_match_off);
            }
            b.cur = b.max_match_off;
        }
        let s0 = b.add_block(input);
        let Base {
            cur,
            max_match_off,
            hist,
            blk,
            ..
        } = b;
        let (e_cur, mmo) = (*cur, *max_match_off);
        blk.size = input.len();
        if rle_block(blk, input) {
            return;
        }
        if input.len() < MIN_NON_LITERAL_BLOCK_SIZE {
            literal_block(blk, input);
            return;
        }
        let src: &[u8] = hist;
        let (table, long) = (&mut self.table, &mut self.long);
        let s_limit = src.len() as i32 - INPUT_MARGIN;
        let mut s = s0;
        let mut next_emit = s;
        let mut cv = load64(src, s);
        let mut offset1 = blk.recent_offsets[0] as i32;
        let mut offset2 = blk.recent_offsets[1] as i32;

        'encode: loop {
            let mut t: i32;
            let can_repeat = blk.sequences.len() > 2;
            let mut matched: i32;
            let mut index0: i32;
            loop {
                let next_hash_l = hash_len(cv, BETTER_LONG_BITS, BETTER_LONG_LEN);
                let next_hash_s = hash_len(cv, BETTER_SHORT_BITS, BETTER_SHORT_LEN);
                let candidate_l = at(long, next_hash_l);
                let candidate_s = at(table, next_hash_s);
                let mut rep_index = s - offset1 + REP_OFF;
                let off = s.wrapping_add(e_cur);
                put(
                    long,
                    next_hash_l,
                    PrevEntry {
                        offset: off,
                        prev: candidate_l.offset,
                    },
                );
                put(
                    table,
                    next_hash_s,
                    TableEntry {
                        offset: off,
                        val: cv as u32,
                    },
                );
                index0 = s + 1;
                if can_repeat && rep_index >= 0 && load32(src, rep_index) == (cv >> (REP_OFF * 8)) as u32 {
                    let length = 4 + match_at(src, s + 4 + REP_OFF, rep_index + 4);
                    let mut match_len = (length - ZSTD_MIN_MATCH) as u32;
                    let mut start = s + REP_OFF;
                    let start_limit = next_emit + 1;
                    let t_min = (s - mmo).max(0);
                    while rep_index > t_min
                        && start > start_limit
                        && byte(src, rep_index - 1) == byte(src, start - 1)
                        && match_len < (MAX_MATCH_LENGTH - ZSTD_MIN_MATCH - 1) as u32
                    {
                        rep_index -= 1;
                        start -= 1;
                        match_len += 1;
                    }
                    let lit_len = add_literals(blk, src, next_emit, start);
                    blk.sequences.push(Seq::new(lit_len, match_len, 1));
                    let index0 = s + REP_OFF;
                    s += length + REP_OFF;
                    next_emit = s;
                    if s >= s_limit {
                        break 'encode;
                    }
                    better_index(table, long, src, index0, s, e_cur);
                    cv = load64(src, s);
                    continue;
                }
                let coffset_l = candidate_l.offset.wrapping_sub(e_cur);
                let coffset_lp = candidate_l.prev.wrapping_sub(e_cur);
                if s - coffset_l < mmo && cv == load64(src, coffset_l) {
                    matched = match_at(src, s + 8, coffset_l + 8) + 8;
                    t = coffset_l;
                    if s - coffset_lp < mmo && cv == load64(src, coffset_lp) {
                        let prev_match = match_at(src, s + 8, coffset_lp + 8) + 8;
                        if prev_match > matched {
                            matched = prev_match;
                            t = coffset_lp;
                        }
                    }
                    break;
                }
                if s - coffset_lp < mmo && cv == load64(src, coffset_lp) {
                    matched = match_at(src, s + 8, coffset_lp + 8) + 8;
                    t = coffset_lp;
                    break;
                }
                let coffset_s = candidate_s.offset.wrapping_sub(e_cur);
                if s - coffset_s < mmo && cv as u32 == candidate_s.val {
                    matched = match_at(src, s + 4, coffset_s + 4) + 4;
                    let cv = load64(src, s + CHECK_AT);
                    let next_hash_l = hash_len(cv, BETTER_LONG_BITS, BETTER_LONG_LEN);
                    let candidate_l = at(long, next_hash_l);
                    let coffset_l = candidate_l.offset.wrapping_sub(e_cur);
                    put(
                        long,
                        next_hash_l,
                        PrevEntry {
                            offset: (s + CHECK_AT).wrapping_add(e_cur),
                            prev: candidate_l.offset,
                        },
                    );
                    if s - coffset_l < mmo && cv == load64(src, coffset_l) {
                        let matched_next = match_at(src, s + 8 + CHECK_AT, coffset_l + 8) + 8;
                        if matched_next > matched {
                            t = coffset_l;
                            s += CHECK_AT;
                            matched = matched_next;
                            break;
                        }
                    }
                    let coffset_l = candidate_l.prev.wrapping_sub(e_cur);
                    if s - coffset_l < mmo && cv == load64(src, coffset_l) {
                        let matched_next = match_at(src, s + 8 + CHECK_AT, coffset_l + 8) + 8;
                        if matched_next > matched {
                            t = coffset_l;
                            s += CHECK_AT;
                            matched = matched_next;
                            break;
                        }
                    }
                    t = coffset_s;
                    break;
                }
                s += STEP_SIZE + ((s - next_emit) >> (K_SEARCH_STRENGTH - 1));
                if s >= s_limit {
                    break 'encode;
                }
                cv = load64(src, s);
            }
            if s + matched < s_limit {
                let next_hash_l = hash_len(load64(src, s + matched), BETTER_LONG_BITS, BETTER_LONG_LEN);
                let s2 = s + SKIP_BEGINNING;
                let cv = load32(src, s2);
                let candidate_l = at(long, next_hash_l);
                for cand in [candidate_l.offset, candidate_l.prev] {
                    let coffset_l = cand
                        .wrapping_sub(e_cur)
                        .wrapping_sub(matched)
                        .wrapping_add(SKIP_BEGINNING);
                    if coffset_l >= 0
                        && coffset_l < s2
                        && s2 - coffset_l < mmo
                        && cv == load32(src, coffset_l)
                    {
                        let matched_next = match_at(src, s2 + 4, coffset_l + 4) + 4;
                        if matched_next > matched {
                            t = coffset_l;
                            s = s2;
                            matched = matched_next;
                        }
                    }
                }
            }
            offset2 = offset1;
            offset1 = s - t;
            let mut l = matched;
            let t_min = (s - mmo).max(0);
            while t > t_min && s > next_emit && byte(src, t - 1) == byte(src, s - 1) && l < MAX_MATCH_LENGTH {
                s -= 1;
                t -= 1;
                l += 1;
            }
            let lit_len = (s - next_emit) as u32;
            if lit_len > 0 {
                blk.literals.extend_from_slice(span(src, next_emit, s));
            }
            blk.sequences.push(seq(lit_len, l, (s - t) as u32 + 3));
            s += l;
            next_emit = s;
            if s >= s_limit {
                break 'encode;
            }
            better_index(table, long, src, index0, s, e_cur);
            cv = load64(src, s);
            if !can_repeat {
                continue;
            }
            loop {
                let o2 = s - offset2;
                if load32(src, o2) != cv as u32 {
                    break;
                }
                let next_hash_l = hash_len(cv, BETTER_LONG_BITS, BETTER_LONG_LEN);
                let next_hash_s = hash_len(cv, BETTER_SHORT_BITS, BETTER_SHORT_LEN);
                let l = 4 + match_at(src, s + 4, o2 + 4);
                push(long, next_hash_l, s.wrapping_add(e_cur));
                put(
                    table,
                    next_hash_s,
                    TableEntry {
                        offset: s.wrapping_add(e_cur),
                        val: cv as u32,
                    },
                );
                blk.sequences.push(seq(0, l, 1));
                s += l;
                next_emit = s;
                std::mem::swap(&mut offset1, &mut offset2);
                if s >= s_limit {
                    break 'encode;
                }
                cv = load64(src, s);
            }
        }
        last_literals(blk, src, next_emit);
        blk.recent_offsets[0] = offset1 as u32;
        blk.recent_offsets[1] = offset2 as u32;
    }
}

const BEST_LONG_BITS: u32 = 22;
const BEST_LONG_LEN: u32 = 8;
const BEST_SHORT_BITS: u32 = 18;
const BEST_SHORT_LEN: u32 = 4;
const HIGH_SCORE: i32 = MAX_MATCH_LENGTH * 8;

/// `match`: a candidate the best encoder scores.
#[derive(Debug, Clone, Copy, Default)]
struct Match {
    offset: i32,
    s: i32,
    length: i32,
    rep: i32,
    est: i32,
}

/// What `improve` weighs a candidate against.
struct Scorer<'a> {
    src: &'a [u8],
    mmo: i32,
    next_emit: i32,
    bits_per_byte: i32,
    predef: &'a [FseEncoder; 3],
}

impl Scorer<'_> {
    /// `estBits`: the candidate's cost in bits, less what it saves.
    fn est_bits(&self, m: &mut Match) {
        let mlc = ml_code(m.length.wrapping_sub(ZSTD_MIN_MATCH) as u32);
        let ofc = if m.rep < 0 {
            of_code((m.s.wrapping_sub(m.offset) as u32).wrapping_add(3))
        } else {
            of_code(m.rep as u32 & 3)
        };
        let [_, of_pre, ml_pre] = self.predef;
        let of_tt = of_pre
            .ct
            .symbol_tt
            .get(usize::from(ofc))
            .copied()
            .unwrap_or_default();
        let ml_tt = ml_pre
            .ct
            .symbol_tt
            .get(usize::from(mlc))
            .copied()
            .unwrap_or_default();
        m.est = i32::from(of_tt.out_bits.wrapping_add(ml_tt.out_bits));
        m.est = m
            .est
            .wrapping_add(((of_tt.delta_nb_bits >> 16).wrapping_add(ml_tt.delta_nb_bits >> 16)) as i32);
        m.est = m
            .est
            .wrapping_sub(m.length.wrapping_mul(self.bits_per_byte) >> 10);
        if m.est > 0 {
            m.length = 0;
            m.est = HIGH_SCORE;
        }
    }

    /// `improve`: `m` replaced by the match at `offset` for `s` where that scores better.
    fn improve(&self, m: &mut Match, mut offset: i32, mut s: i32, first: u32, rep: i32) {
        let src = self.src;
        let delta = s.wrapping_sub(offset);
        if delta >= self.mmo || delta <= 0 || load32(src, offset) != first {
            return;
        }
        if m.length > 16 {
            let left = src.len() as i64 - i64::from(m.s) - i64::from(m.length);
            if left <= 0 {
                return;
            }
            let check_len = m.length - (s - m.s) - 8;
            if left > 2 && check_len > 4 && load32(src, offset + check_len) != load32(src, s + check_len) {
                return;
            }
        }
        let mut l = 4 + match_at(src, s + 4, offset + 4);
        if m.rep <= 0 {
            let t_min = (s - self.mmo).max(0);
            while offset > t_min
                && s > self.next_emit
                && byte(src, offset - 1) == byte(src, s - 1)
                && l < MAX_MATCH_LENGTH
            {
                s -= 1;
                offset -= 1;
                l += 1;
            }
        }
        let mut cand = Match {
            offset,
            s,
            length: l,
            rep,
            est: 0,
        };
        self.est_bits(&mut cand);
        if m.est >= HIGH_SCORE
            || cand
                .est
                .wrapping_sub(m.est)
                .wrapping_add(cand.s.wrapping_sub(m.s).wrapping_mul(self.bits_per_byte) >> 10)
                < 0
        {
            *m = cand;
        }
    }
}

/// `bestFastEncoder`.
#[derive(Debug)]
struct Best {
    table: Vec<PrevEntry>,
    long: Vec<PrevEntry>,
}

impl Best {
    /// `Encode`.
    fn encode(&mut self, b: &mut Base, input: &[u8], predef: &[FseEncoder; 3]) {
        const INPUT_MARGIN: i32 = 8 + 4;
        const MIN_NON_LITERAL_BLOCK_SIZE: usize = 16;
        const K_SEARCH_STRENGTH: i32 = 10;
        const GOOD_ENOUGH: i32 = 250;
        const SKIP_BEGINNING: i32 = 2;
        if b.cur >= b.buffer_reset.wrapping_sub(b.hist.len() as i32) {
            if b.hist.is_empty() {
                self.table.fill(PrevEntry::default());
                self.long.fill(PrevEntry::default());
            } else {
                let min_off = b
                    .cur
                    .wrapping_add(b.hist.len() as i32)
                    .wrapping_sub(b.max_match_off);
                rebase_prev(&mut self.table, min_off, b.cur, b.max_match_off);
                rebase_prev(&mut self.long, min_off, b.cur, b.max_match_off);
            }
            b.cur = b.max_match_off;
        }
        let s0 = b.add_block(input);
        let Base {
            cur,
            max_match_off,
            hist,
            blk,
            ..
        } = b;
        let (e_cur, mmo) = (*cur, *max_match_off);
        blk.size = input.len();
        if rle_block(blk, input) {
            return;
        }
        if input.len() < MIN_NON_LITERAL_BLOCK_SIZE {
            literal_block(blk, input);
            return;
        }
        let bits_per_byte = i32::try_from((shannon_entropy_bits(input) * 1024) / input.len() as i64)
            .unwrap_or(i32::MAX)
            .max(1024);
        let src: &[u8] = hist;
        let (table, long) = (&mut self.table, &mut self.long);
        let s_limit = src.len() as i32 - INPUT_MARGIN;
        let mut s = s0;
        let mut next_emit = s;
        let mut offset1 = blk.recent_offsets[0] as i32;
        let mut offset2 = blk.recent_offsets[1] as i32;
        let mut offset3 = blk.recent_offsets[2] as i32;

        // Index from `index0` to `end` in both tables.
        let index = |table: &mut [PrevEntry], long: &mut [PrevEntry], mut index0: i32, end: i32| {
            let mut off = index0.wrapping_add(e_cur);
            while index0 < end {
                let cv0 = load64(src, index0);
                let h0 = hash_len(cv0, BEST_LONG_BITS, BEST_LONG_LEN);
                let h1 = hash_len(cv0, BEST_SHORT_BITS, BEST_SHORT_LEN);
                push(long, h0, off);
                push(table, h1, off);
                off = off.wrapping_add(1);
                index0 += 1;
            }
        };

        'encode: loop {
            let can_repeat = blk.sequences.len() > 2;
            let sc = Scorer {
                src,
                mmo,
                next_emit,
                bits_per_byte,
                predef,
            };
            let mut cv = load64(src, s);
            let next_hash_l = hash_len(cv, BEST_LONG_BITS, BEST_LONG_LEN);
            let next_hash_s = hash_len(cv, BEST_SHORT_BITS, BEST_SHORT_LEN);
            let mut candidate_l = at(long, next_hash_l);
            let mut candidate_s = at(table, next_hash_s);
            let mut best = Match {
                s,
                est: HIGH_SCORE,
                ..Match::default()
            };
            sc.improve(
                &mut best,
                candidate_l.offset.wrapping_sub(e_cur),
                s,
                cv as u32,
                -1,
            );
            sc.improve(&mut best, candidate_l.prev.wrapping_sub(e_cur), s, cv as u32, -1);
            sc.improve(
                &mut best,
                candidate_s.offset.wrapping_sub(e_cur),
                s,
                cv as u32,
                -1,
            );
            sc.improve(&mut best, candidate_s.prev.wrapping_sub(e_cur), s, cv as u32, -1);
            if can_repeat && best.length < GOOD_ENOUGH {
                if s == next_emit {
                    sc.improve(&mut best, s - offset2, s, cv as u32, 1 | 4);
                    sc.improve(&mut best, s - offset3, s, cv as u32, 2 | 4);
                    if offset1 > 1 {
                        sc.improve(&mut best, s - (offset1 - 1), s, cv as u32, 3 | 4);
                    }
                }
                if best.rep <= 0 {
                    let mut cv32 = (cv >> 8) as u32;
                    let mut spp = s + 1;
                    sc.improve(&mut best, spp - offset1, spp, cv32, 1);
                    sc.improve(&mut best, spp - offset2, spp, cv32, 2);
                    sc.improve(&mut best, spp - offset3, spp, cv32, 3);
                    if best.rep < 0 {
                        cv32 = (cv >> 24) as u32;
                        spp += 2;
                        sc.improve(&mut best, spp - offset1, spp, cv32, 1);
                        sc.improve(&mut best, spp - offset2, spp, cv32, 2);
                        sc.improve(&mut best, spp - offset3, spp, cv32, 3);
                    }
                }
            }
            put(
                long,
                next_hash_l,
                PrevEntry {
                    offset: s.wrapping_add(e_cur),
                    prev: candidate_l.offset,
                },
            );
            put(
                table,
                next_hash_s,
                PrevEntry {
                    offset: s.wrapping_add(e_cur),
                    prev: candidate_s.offset,
                },
            );
            let index0 = s + 1;
            if best.length < GOOD_ENOUGH {
                if best.length < 4 {
                    s += 1 + ((s - next_emit) >> (K_SEARCH_STRENGTH - 1));
                    if s >= s_limit {
                        break 'encode;
                    }
                    continue;
                }
                candidate_s = at(table, hash_len(cv >> 8, BEST_SHORT_BITS, BEST_SHORT_LEN));
                cv = load64(src, s + 1);
                let cv2 = load64(src, s + 2);
                candidate_l = at(long, hash_len(cv, BEST_LONG_BITS, BEST_LONG_LEN));
                let candidate_l2 = at(long, hash_len(cv2, BEST_LONG_BITS, BEST_LONG_LEN));
                sc.improve(
                    &mut best,
                    candidate_s.offset.wrapping_sub(e_cur),
                    s + 1,
                    cv as u32,
                    -1,
                );
                sc.improve(
                    &mut best,
                    candidate_l.offset.wrapping_sub(e_cur),
                    s + 1,
                    cv as u32,
                    -1,
                );
                sc.improve(
                    &mut best,
                    candidate_l.prev.wrapping_sub(e_cur),
                    s + 1,
                    cv as u32,
                    -1,
                );
                sc.improve(
                    &mut best,
                    candidate_l2.offset.wrapping_sub(e_cur),
                    s + 2,
                    cv2 as u32,
                    -1,
                );
                sc.improve(
                    &mut best,
                    candidate_l2.prev.wrapping_sub(e_cur),
                    s + 2,
                    cv2 as u32,
                    -1,
                );
                if best.s > s - SKIP_BEGINNING {
                    let s_at = best.s + best.length;
                    if s_at < s_limit {
                        let candidate_end =
                            at(long, hash_len(load64(src, s_at), BEST_LONG_BITS, BEST_LONG_LEN));
                        let off = candidate_end
                            .offset
                            .wrapping_sub(e_cur)
                            .wrapping_sub(best.length)
                            .wrapping_add(SKIP_BEGINNING);
                        let at_s = best.s + SKIP_BEGINNING;
                        if off >= 0 {
                            sc.improve(&mut best, off, at_s, load32(src, at_s), -1);
                            let off = candidate_end
                                .prev
                                .wrapping_sub(e_cur)
                                .wrapping_sub(best.length)
                                .wrapping_add(SKIP_BEGINNING);
                            // The first may have moved the best's start.
                            let at_s = best.s + SKIP_BEGINNING;
                            if off >= 0 {
                                sc.improve(&mut best, off, at_s, load32(src, at_s), -1);
                            }
                        }
                    }
                }
            }
            s = best.s;
            if best.rep > 0 {
                let lit_len = add_literals(blk, src, next_emit, best.s);
                blk.sequences.push(Seq::new(
                    lit_len,
                    best.length.wrapping_sub(ZSTD_MIN_MATCH) as u32,
                    (best.rep & 3) as u32,
                ));
                s = best.s + best.length;
                next_emit = s;
                index(table, long, index0, s.min(s_limit + 4));
                match best.rep {
                    2 | 5 => std::mem::swap(&mut offset1, &mut offset2),
                    3 | 6 => (offset1, offset2, offset3) = (offset3, offset1, offset2),
                    7 => (offset1, offset2, offset3) = (offset1 - 1, offset1, offset2),
                    _ => {}
                }
                if s >= s_limit {
                    break 'encode;
                }
                continue;
            }
            let t = best.offset;
            (offset1, offset2, offset3) = (s - t, offset1, offset2);
            let l = best.length;
            let lit_len = (s - next_emit) as u32;
            if lit_len > 0 {
                blk.literals.extend_from_slice(span(src, next_emit, s));
            }
            blk.sequences.push(seq(lit_len, l, (s - t) as u32 + 3));
            s += l;
            next_emit = s;
            index(table, long, index0, s.min(s_limit - 4));
            if s >= s_limit {
                break 'encode;
            }
        }
        last_literals(blk, src, next_emit);
        blk.recent_offsets = [offset1 as u32, offset2 as u32, offset3 as u32];
    }
}

/// The predefined encoders, for a block's sequences and the best encoder's scores.
pub fn predef() -> [FseEncoder; 3] {
    crate::fse::predefined(&crate::block::LL_BITS_TABLE, &ML_BITS_TABLE)
}
