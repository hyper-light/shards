//! Go's `compressor` (deflate.go): every level, from storing to lazy matching over hash
//! chains, as `flate.NewWriter` makes it, written to, then closed (no `Flush`, which
//! BuildKit's layers never take).

use std::io::{self, Write};

use crate::bits::{BitWriter, MAX_STORE_BLOCK_SIZE};
use crate::fast::DeflateFast;
use crate::token::{Token, literal_token, match_token};

const LOG_WINDOW_SIZE: u32 = 15;
const WINDOW_SIZE: i64 = 1 << LOG_WINDOW_SIZE;
const WINDOW_MASK: i64 = WINDOW_SIZE - 1;
const BASE_MATCH_LENGTH: i64 = 3;
const MIN_MATCH_LENGTH: i64 = 4;
const MAX_MATCH_LENGTH: i64 = 258;
const BASE_MATCH_OFFSET: i64 = 1;
const MAX_FLATE_BLOCK_TOKENS: usize = 1 << 14;
const HASH_BITS: u32 = 17;
const HASH_SIZE: usize = 1 << HASH_BITS;
const HASH_MASK: u32 = (1 << HASH_BITS) - 1;
const MAX_HASH_OFFSET: i64 = 1 << 24;
const SKIP_NEVER: i64 = i32::MAX as i64;
const HASHMUL: u32 = 0x1e35a7bd;

/// `levels`: level, good, lazy, nice, chain, fastSkipHashing.
const LEVELS: [[i64; 6]; 10] = [
    [0, 0, 0, 0, 0, 0],
    [1, 0, 0, 0, 0, 0],
    [2, 4, 0, 16, 8, 5],
    [3, 4, 0, 32, 32, 6],
    [4, 4, 4, 16, 16, SKIP_NEVER],
    [5, 8, 16, 32, 32, SKIP_NEVER],
    [6, 8, 16, 128, 128, SKIP_NEVER],
    [7, 8, 32, 128, 256, SKIP_NEVER],
    [8, 32, 128, 258, 1024, SKIP_NEVER],
    [9, 32, 258, 258, 4096, SKIP_NEVER],
];

/// What each step does with the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// NoCompression: stored blocks.
    Store,
    /// HuffmanOnly: literals alone.
    Huff,
    /// BestSpeed: deflatefast.go.
    Speed,
    /// Levels 2 to 9.
    Deflate,
}

#[derive(Debug)]
pub struct Compressor<W: Write> {
    good: i64,
    lazy: i64,
    nice: i64,
    chain: i64,
    fast_skip_hashing: i64,
    mode: Mode,
    w: BitWriter<W>,
    best_speed: Option<DeflateFast>,
    index: i64,
    window: Vec<u8>,
    window_end: i64,
    block_start: i64,
    byte_available: bool,
    sync: bool,
    tokens: Vec<Token>,
    length: i64,
    offset: i64,
    max_insert_index: i64,
    chain_head: i64,
    hash_head: Vec<u32>,
    hash_prev: Vec<u32>,
    hash_offset: i64,
    closed: bool,
}

fn hash4(b: &[u8]) -> u32 {
    let v = b.iter().take(4).fold(0u32, |v, &x| (v << 8) | u32::from(x));
    v.wrapping_mul(HASHMUL) >> (32 - HASH_BITS)
}

fn at(v: &[u8], i: i64) -> u8 {
    usize::try_from(i)
        .ok()
        .and_then(|i| v.get(i))
        .copied()
        .unwrap_or(0)
}

fn span(v: &[u8], from: i64, to: i64) -> &[u8] {
    match (usize::try_from(from), usize::try_from(to)) {
        (Ok(f), Ok(t)) => v.get(f..t).unwrap_or_default(),
        _ => &[],
    }
}

fn u32_at(v: &[u32], i: i64) -> u32 {
    usize::try_from(i)
        .ok()
        .and_then(|i| v.get(i))
        .copied()
        .unwrap_or(0)
}

fn set_u32(v: &mut [u32], i: i64, x: u32) {
    if let Some(e) = usize::try_from(i).ok().and_then(|i| v.get_mut(i)) {
        *e = x;
    }
}

/// `matchLen`: how many of `a`'s first `max` bytes `b` matches.
fn match_len(a: &[u8], b: &[u8], max: i64) -> i64 {
    let max = usize::try_from(max).unwrap_or(0);
    a.iter().zip(b).take(max).take_while(|(x, y)| x == y).count() as i64
}

impl<W: Write> Compressor<W> {
    /// `init`: a compressor for `level`, -2 (HuffmanOnly) to 9, -1 the default, 6.
    pub fn new(writer: W, level: i32) -> io::Result<Compressor<W>> {
        let mode = match level {
            0 => Mode::Store,
            -2 => Mode::Huff,
            1 => Mode::Speed,
            -1 | 2..=9 => Mode::Deflate,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("flate: invalid compression level {level}: want value in range [-2, 9]"),
                ));
            }
        };
        let params = match (mode, level) {
            (Mode::Deflate, -1) => LEVELS.get(6),
            (Mode::Deflate | Mode::Speed, l) => LEVELS.get(usize::try_from(l).unwrap_or(0)),
            _ => LEVELS.first(),
        }
        .copied()
        .unwrap_or_default();
        let [_, good, lazy, nice, chain, fast_skip_hashing] = params;
        let deflate = mode == Mode::Deflate;
        Ok(Compressor {
            good,
            lazy,
            nice,
            chain,
            fast_skip_hashing,
            mode,
            w: BitWriter::new(writer),
            best_speed: (mode == Mode::Speed).then(DeflateFast::new),
            index: 0,
            window: vec![
                0;
                if deflate {
                    2 * WINDOW_SIZE as usize
                } else {
                    MAX_STORE_BLOCK_SIZE
                }
            ],
            window_end: 0,
            block_start: 0,
            byte_available: false,
            sync: false,
            tokens: Vec::with_capacity(if deflate {
                MAX_FLATE_BLOCK_TOKENS + 1
            } else {
                MAX_STORE_BLOCK_SIZE
            }),
            length: MIN_MATCH_LENGTH - 1,
            offset: 0,
            max_insert_index: 0,
            chain_head: -1,
            hash_head: if deflate { vec![0; HASH_SIZE] } else { Vec::new() },
            hash_prev: if deflate {
                vec![0; WINDOW_SIZE as usize]
            } else {
                Vec::new()
            },
            hash_offset: 1,
            closed: false,
        })
    }

    fn failed(&mut self) -> io::Result<()> {
        match self.w.error() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn fill(&mut self, b: &[u8]) -> usize {
        if self.mode == Mode::Deflate {
            return self.fill_deflate(b);
        }
        self.copy_in(b)
    }

    fn copy_in(&mut self, b: &[u8]) -> usize {
        let start = usize::try_from(self.window_end).unwrap_or(0);
        let room = self.window.get_mut(start..).unwrap_or_default();
        let n = room.len().min(b.len());
        if let (Some(dst), Some(src)) = (room.get_mut(..n), b.get(..n)) {
            dst.copy_from_slice(src);
        }
        self.window_end += n as i64;
        n
    }

    fn fill_deflate(&mut self, b: &[u8]) -> usize {
        if self.index >= 2 * WINDOW_SIZE - (MIN_MATCH_LENGTH + MAX_MATCH_LENGTH) {
            // Shift the window by its size.
            let half = WINDOW_SIZE as usize;
            self.window.copy_within(half..2 * half, 0);
            self.index -= WINDOW_SIZE;
            self.window_end -= WINDOW_SIZE;
            if self.block_start >= WINDOW_SIZE {
                self.block_start -= WINDOW_SIZE;
            } else {
                self.block_start = i64::from(i32::MAX);
            }
            self.hash_offset += WINDOW_SIZE;
            if self.hash_offset > MAX_HASH_OFFSET {
                let delta = self.hash_offset - 1;
                self.hash_offset -= delta;
                self.chain_head -= delta;
                for v in self.hash_prev.iter_mut().chain(self.hash_head.iter_mut()) {
                    *v = if i64::from(*v) > delta {
                        (i64::from(*v) - delta) as u32
                    } else {
                        0
                    };
                }
            }
        }
        self.copy_in(b)
    }

    /// `writeBlock`: the tokens since the block began, up to `index`.
    fn write_block(&mut self, index: i64) -> io::Result<()> {
        if index > 0 {
            let window = (self.block_start <= index).then(|| span(&self.window, self.block_start, index));
            self.block_start = index;
            self.w.write_block(&self.tokens, false, window);
            return self.failed();
        }
        Ok(())
    }

    fn write_stored_block(&mut self, from: i64, to: i64) -> io::Result<()> {
        let buf = span(&self.window, from, to).to_vec();
        self.w.write_stored_header(buf.len(), false);
        if self.w.err.is_none() {
            self.w.write_bytes(&buf);
        }
        self.failed()
    }

    /// `findMatch`: a match at `pos` longer than `prev_length`, from the chain at
    /// `prev_head`, looking at no more of it than the level allows.
    fn find_match(&self, pos: i64, prev_head: i64, prev_length: i64, lookahead: i64) -> Option<(i64, i64)> {
        let min_match_look = MAX_MATCH_LENGTH.min(lookahead);
        let win = span(&self.window, 0, pos + min_match_look);
        let nice = (win.len() as i64 - pos).min(self.nice);
        let mut tries = self.chain;
        let mut length = prev_length;
        if length >= self.good {
            tries >>= 2;
        }
        let mut w_end = at(win, pos + length);
        let w_pos = span(win, pos, win.len() as i64);
        let min_index = pos - WINDOW_SIZE;
        let mut found = None;
        let mut i = prev_head;
        while tries > 0 {
            if w_end == at(win, i + length) {
                let n = match_len(span(win, i, win.len() as i64), w_pos, min_match_look);
                if n > length && (n > MIN_MATCH_LENGTH || pos - i <= 4096) {
                    length = n;
                    found = Some((n, pos - i));
                    if n >= nice {
                        break;
                    }
                    w_end = at(win, pos + n);
                }
            }
            if i == min_index {
                // Its place in hash_prev has been written over already.
                break;
            }
            i = i64::from(u32_at(&self.hash_prev, i & WINDOW_MASK)) - self.hash_offset;
            if i < min_index || i < 0 {
                break;
            }
            tries -= 1;
        }
        found
    }

    /// `encSpeed`.
    fn enc_speed(&mut self) -> io::Result<()> {
        if self.window_end < MAX_STORE_BLOCK_SIZE as i64 {
            if !self.sync {
                return Ok(());
            }
            if self.window_end < 128 {
                match self.window_end {
                    0 => return Ok(()),
                    1..=16 => self.write_stored_block(0, self.window_end)?,
                    _ => {
                        let input = span(&self.window, 0, self.window_end).to_vec();
                        self.w.write_block_huff(false, &input);
                        self.failed()?;
                    }
                }
                self.window_end = 0;
                if let Some(f) = self.best_speed.as_mut() {
                    f.reset();
                }
                return Ok(());
            }
        }
        let input = span(&self.window, 0, self.window_end).to_vec();
        self.tokens.clear();
        if let Some(f) = self.best_speed.as_mut() {
            f.encode(&mut self.tokens, &input);
        }
        let end = self.window_end;
        if self.tokens.len() as i64 > end - (end >> 4) {
            self.w.write_block_huff(false, &input);
        } else {
            self.w.write_block_dynamic(&self.tokens, false, Some(&input));
        }
        self.window_end = 0;
        self.failed()
    }

    fn insert(&mut self, index: i64) {
        let hash = hash4(span(&self.window, index, index + MIN_MATCH_LENGTH));
        let head = (hash & HASH_MASK) as i64;
        let prev = u32_at(&self.hash_head, head);
        set_u32(&mut self.hash_prev, index & WINDOW_MASK, prev);
        set_u32(&mut self.hash_head, head, (index + self.hash_offset) as u32);
    }

    /// `deflate`: tokens for what the window holds, blocks written as they fill.
    fn deflate(&mut self) -> io::Result<()> {
        if self.window_end - self.index < MIN_MATCH_LENGTH + MAX_MATCH_LENGTH && !self.sync {
            return Ok(());
        }
        self.max_insert_index = self.window_end - (MIN_MATCH_LENGTH - 1);
        let skip_never = self.fast_skip_hashing == SKIP_NEVER;
        loop {
            let lookahead = self.window_end - self.index;
            if lookahead < MIN_MATCH_LENGTH + MAX_MATCH_LENGTH {
                if !self.sync {
                    break;
                }
                if lookahead == 0 {
                    if self.byte_available {
                        // One pending literal.
                        self.tokens
                            .push(literal_token(u32::from(at(&self.window, self.index - 1))));
                        self.byte_available = false;
                    }
                    if !self.tokens.is_empty() {
                        self.write_block(self.index)?;
                        self.tokens.clear();
                    }
                    break;
                }
            }
            if self.index < self.max_insert_index {
                let hash = hash4(span(&self.window, self.index, self.index + MIN_MATCH_LENGTH));
                let head = (hash & HASH_MASK) as i64;
                self.chain_head = i64::from(u32_at(&self.hash_head, head));
                set_u32(
                    &mut self.hash_prev,
                    self.index & WINDOW_MASK,
                    self.chain_head as u32,
                );
                set_u32(&mut self.hash_head, head, (self.index + self.hash_offset) as u32);
            }
            let prev_length = self.length;
            let prev_offset = self.offset;
            self.length = MIN_MATCH_LENGTH - 1;
            self.offset = 0;
            let min_index = (self.index - WINDOW_SIZE).max(0);
            if self.chain_head - self.hash_offset >= min_index
                && (!skip_never && lookahead > MIN_MATCH_LENGTH - 1
                    || skip_never && lookahead > prev_length && prev_length < self.lazy)
                && let Some((length, offset)) = self.find_match(
                    self.index,
                    self.chain_head - self.hash_offset,
                    MIN_MATCH_LENGTH - 1,
                    lookahead,
                )
            {
                self.length = length;
                self.offset = offset;
            }
            if !skip_never && self.length >= MIN_MATCH_LENGTH
                || skip_never && prev_length >= MIN_MATCH_LENGTH && self.length <= prev_length
            {
                // A match at the step before, and none better now: the earlier one.
                let (l, o) = if skip_never {
                    (prev_length, prev_offset)
                } else {
                    (self.length, self.offset)
                };
                self.tokens.push(match_token(
                    (l - BASE_MATCH_LENGTH) as u32,
                    (o - BASE_MATCH_OFFSET) as u32,
                ));
                // Every string to the match's end hashed, but where too little follows.
                if self.length <= self.fast_skip_hashing {
                    let new_index = if skip_never {
                        self.index + prev_length - 1
                    } else {
                        self.index + self.length
                    };
                    let mut index = self.index + 1;
                    while index < new_index {
                        if index < self.max_insert_index {
                            self.insert(index);
                        }
                        index += 1;
                    }
                    self.index = index;
                    if skip_never {
                        self.byte_available = false;
                        self.length = MIN_MATCH_LENGTH - 1;
                    }
                } else {
                    // Matches this long are not hashed string by string.
                    self.index += self.length;
                }
                if self.tokens.len() == MAX_FLATE_BLOCK_TOKENS {
                    // The block includes the current character.
                    self.write_block(self.index)?;
                    self.tokens.clear();
                }
            } else {
                if !skip_never || self.byte_available {
                    let i = if skip_never { self.index - 1 } else { self.index };
                    self.tokens.push(literal_token(u32::from(at(&self.window, i))));
                    if self.tokens.len() == MAX_FLATE_BLOCK_TOKENS {
                        self.write_block(i + 1)?;
                        self.tokens.clear();
                    }
                }
                self.index += 1;
                if skip_never {
                    self.byte_available = true;
                }
            }
        }
        Ok(())
    }

    fn store(&mut self) -> io::Result<()> {
        if self.window_end > 0 && (self.window_end == MAX_STORE_BLOCK_SIZE as i64 || self.sync) {
            self.write_stored_block(0, self.window_end)?;
            self.window_end = 0;
        }
        Ok(())
    }

    fn store_huff(&mut self) -> io::Result<()> {
        if self.window_end < self.window.len() as i64 && !self.sync || self.window_end == 0 {
            return Ok(());
        }
        let input = span(&self.window, 0, self.window_end).to_vec();
        self.w.write_block_huff(false, &input);
        self.window_end = 0;
        self.failed()
    }

    fn step(&mut self) -> io::Result<()> {
        match self.mode {
            Mode::Store => self.store(),
            Mode::Huff => self.store_huff(),
            Mode::Speed => self.enc_speed(),
            Mode::Deflate => self.deflate(),
        }
    }

    /// `write`: `b` taken into the window, each window's worth compressed.
    pub fn write(&mut self, mut b: &[u8]) -> io::Result<()> {
        if self.closed {
            return Err(io::Error::other("flate: closed writer"));
        }
        self.failed()?;
        while !b.is_empty() {
            self.step()?;
            let n = self.fill(b);
            b = b.get(n..).unwrap_or_default();
            self.failed()?;
        }
        Ok(())
    }

    /// `syncFlush`: what is pending compressed, and an empty stored block, not the last,
    /// that ends on a byte (`Flush`; estargz flushes before each chunk, D82).
    pub fn sync_flush(&mut self) -> io::Result<()> {
        self.failed()?;
        self.sync = true;
        self.step()?;
        self.failed()?;
        self.w.write_stored_header(0, false);
        self.w.flush();
        self.sync = false;
        self.failed()
    }

    /// `close`: what is left compressed, and the final, empty, stored block.
    pub fn close(&mut self) -> io::Result<()> {
        if self.closed {
            return Ok(());
        }
        self.failed()?;
        self.sync = true;
        self.step()?;
        self.w.write_stored_header(0, true);
        self.failed()?;
        self.w.flush();
        self.failed()?;
        self.closed = true;
        Ok(())
    }

    pub fn into_inner(self) -> W {
        self.w.into_inner()
    }
}
