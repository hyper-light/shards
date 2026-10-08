//! Go's `BestSpeed` encoder (deflatefast.go): Snappy-like matching in each block of up to
//! 64 KiB against a table of 4-byte hashes, matches reaching into the block before it.

use crate::bits::MAX_STORE_BLOCK_SIZE;
use crate::token::{Token, literal_token, match_token};

const TABLE_BITS: u32 = 14;
const TABLE_SIZE: usize = 1 << TABLE_BITS;
const TABLE_MASK: u32 = (TABLE_SIZE as u32) - 1;
const TABLE_SHIFT: u32 = 32 - TABLE_BITS;
const BUFFER_RESET: i32 = i32::MAX - (MAX_STORE_BLOCK_SIZE as i32) * 2;
const INPUT_MARGIN: i32 = 16 - 1;
const MIN_NON_LITERAL_BLOCK_SIZE: usize = 1 + 1 + INPUT_MARGIN as usize;
const MAX_MATCH_OFFSET: i32 = 1 << 15;
const MAX_MATCH_LENGTH: i32 = 258;
const BASE_MATCH_LENGTH: i32 = 3;
const BASE_MATCH_OFFSET: i32 = 1;

fn byte(b: &[u8], i: i32) -> u8 {
    usize::try_from(i)
        .ok()
        .and_then(|i| b.get(i))
        .copied()
        .unwrap_or(0)
}

fn load32(b: &[u8], i: i32) -> u32 {
    (0..4).fold(0u32, |v, k| v | u32::from(byte(b, i + k)) << (8 * k))
}

fn load64(b: &[u8], i: i32) -> u64 {
    (0..8).fold(0u64, |v, k| v | u64::from(byte(b, i + k)) << (8 * k))
}

fn hash(u: u32) -> u32 {
    u.wrapping_mul(0x1e35a7bd) >> TABLE_SHIFT
}

#[derive(Debug, Clone, Copy, Default)]
struct TableEntry {
    val: u32,
    offset: i32,
}

#[derive(Debug)]
pub struct DeflateFast {
    table: Vec<TableEntry>,
    /// The previous block, empty if unknown.
    prev: Vec<u8>,
    /// The current match offset.
    cur: i32,
}

impl DeflateFast {
    pub fn new() -> DeflateFast {
        DeflateFast {
            table: vec![TableEntry::default(); TABLE_SIZE],
            prev: Vec::with_capacity(MAX_STORE_BLOCK_SIZE),
            cur: MAX_STORE_BLOCK_SIZE as i32,
        }
    }

    fn entry(&self, h: u32) -> TableEntry {
        self.table
            .get((h & TABLE_MASK) as usize)
            .copied()
            .unwrap_or_default()
    }

    fn set(&mut self, h: u32, e: TableEntry) {
        if let Some(x) = self.table.get_mut((h & TABLE_MASK) as usize) {
            *x = e;
        }
    }

    /// `encode`: `src`'s tokens after `dst`'s.
    pub fn encode(&mut self, dst: &mut Vec<Token>, src: &[u8]) {
        if self.cur >= BUFFER_RESET {
            self.shift_offsets();
        }
        if src.len() < MIN_NON_LITERAL_BLOCK_SIZE {
            self.cur = self.cur.wrapping_add(MAX_STORE_BLOCK_SIZE as i32);
            self.prev.clear();
            emit_literal(dst, src);
            return;
        }
        let len = i32::try_from(src.len()).unwrap_or(i32::MAX);
        let s_limit = len - INPUT_MARGIN;
        let mut next_emit: i32 = 0;
        let mut s: i32 = 0;
        let mut cv = load32(src, s);
        let mut next_hash = hash(cv);
        'outer: loop {
            let mut skip: i32 = 32;
            let mut next_s = s;
            let mut candidate;
            loop {
                s = next_s;
                let between = skip >> 5;
                next_s = s + between;
                skip += between;
                if next_s > s_limit {
                    break 'outer;
                }
                candidate = self.entry(next_hash);
                let now = load32(src, next_s);
                self.set(
                    next_hash,
                    TableEntry {
                        offset: s.wrapping_add(self.cur),
                        val: cv,
                    },
                );
                next_hash = hash(now);
                let offset = s.wrapping_sub(candidate.offset.wrapping_sub(self.cur));
                if offset > MAX_MATCH_OFFSET || cv != candidate.val {
                    cv = now;
                    continue;
                }
                break;
            }
            emit_literal(dst, slice(src, next_emit, s));
            loop {
                s += 4;
                let t = candidate.offset.wrapping_sub(self.cur) + 4;
                let l = self.match_len(s, t, src);
                dst.push(match_token(
                    (l + 4 - BASE_MATCH_LENGTH) as u32,
                    (s - t - BASE_MATCH_OFFSET) as u32,
                ));
                s += l;
                next_emit = s;
                if s >= s_limit {
                    break 'outer;
                }
                let mut x = load64(src, s - 1);
                let prev_hash = hash(x as u32);
                self.set(
                    prev_hash,
                    TableEntry {
                        offset: self.cur.wrapping_add(s - 1),
                        val: x as u32,
                    },
                );
                x >>= 8;
                let curr_hash = hash(x as u32);
                candidate = self.entry(curr_hash);
                self.set(
                    curr_hash,
                    TableEntry {
                        offset: self.cur.wrapping_add(s),
                        val: x as u32,
                    },
                );
                let offset = s.wrapping_sub(candidate.offset.wrapping_sub(self.cur));
                if offset > MAX_MATCH_OFFSET || x as u32 != candidate.val {
                    cv = (x >> 8) as u32;
                    next_hash = hash(cv);
                    s += 1;
                    break;
                }
            }
        }
        if next_emit < len {
            emit_literal(dst, slice(src, next_emit, len));
        }
        self.cur = self.cur.wrapping_add(len);
        self.prev.clear();
        self.prev.extend_from_slice(src);
    }

    /// `matchLen`: how far `src` from `s` matches from `t`, which may start in the
    /// previous block.
    fn match_len(&self, s: i32, t: i32, src: &[u8]) -> i32 {
        let len = i32::try_from(src.len()).unwrap_or(i32::MAX);
        let s1 = (s + MAX_MATCH_LENGTH - 4).min(len);
        if t >= 0 {
            let a = slice(src, s, s1);
            let b = slice(src, t, len);
            return a.iter().zip(b).take_while(|(x, y)| x == y).count() as i32;
        }
        let tp = i32::try_from(self.prev.len()).unwrap_or(i32::MAX) + t;
        if tp < 0 {
            return 0;
        }
        let a = slice(src, s, s1);
        let b = slice(&self.prev, tp, i32::try_from(self.prev.len()).unwrap_or(i32::MAX));
        let b = b.get(..b.len().min(a.len())).unwrap_or_default();
        if let Some(i) = a.iter().zip(b).position(|(x, y)| x != y) {
            return i as i32;
        }
        let n = b.len() as i32;
        if s + n == s1 {
            return n;
        }
        let a = slice(src, s + n, s1);
        let b = slice(src, 0, len);
        n + a.iter().zip(b).take_while(|(x, y)| x == y).count() as i32
    }

    /// `reset`: the previous block forgotten, the offset moved past it.
    pub fn reset(&mut self) {
        self.prev.clear();
        self.cur = self.cur.wrapping_add(MAX_MATCH_OFFSET);
        if self.cur >= BUFFER_RESET {
            self.shift_offsets();
        }
    }

    fn shift_offsets(&mut self) {
        if self.prev.is_empty() {
            self.table.iter_mut().for_each(|e| *e = TableEntry::default());
            self.cur = MAX_MATCH_OFFSET + 1;
            return;
        }
        for e in &mut self.table {
            e.offset = (e.offset.wrapping_sub(self.cur) + MAX_MATCH_OFFSET + 1).max(0);
        }
        self.cur = MAX_MATCH_OFFSET + 1;
    }
}

fn slice(b: &[u8], from: i32, to: i32) -> &[u8] {
    match (usize::try_from(from), usize::try_from(to)) {
        (Ok(f), Ok(t)) => b.get(f..t.min(b.len())).unwrap_or_default(),
        _ => &[],
    }
}

fn emit_literal(dst: &mut Vec<Token>, lit: &[u8]) {
    dst.extend(lit.iter().map(|&v| literal_token(u32::from(v))));
}
