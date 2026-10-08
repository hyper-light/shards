//! Go's `huffmanBitWriter` (huffman_bit_writer.go): blocks written stored, with the fixed
//! codes or with dynamic ones, whichever is smallest, the bits gathered six bytes at a
//! time into a 248-byte buffer before they reach the writer.

use std::io::{self, Write};

use crate::huffman::{Encoder, Hcode, MAX_NUM_LIT};
use crate::token::{self, LENGTH_BASE, MATCH_TYPE, OFFSET_BASE, Token};

pub const OFFSET_CODE_COUNT: usize = 30;
pub const END_BLOCK_MARKER: Token = 256;
const LENGTH_CODES_START: usize = 257;
const CODEGEN_CODE_COUNT: usize = 19;
const BAD_CODE: u8 = 255;
const BUFFER_FLUSH_SIZE: usize = 240;
const BUFFER_SIZE: usize = BUFFER_FLUSH_SIZE + 8;
pub const MAX_STORE_BLOCK_SIZE: usize = 65535;

const LENGTH_EXTRA_BITS: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const OFFSET_EXTRA_BITS: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13,
];
const CODEGEN_ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

fn get<T: Copy + Default>(v: &[T], i: usize) -> T {
    v.get(i).copied().unwrap_or_default()
}

fn bump(v: &mut [i32], i: usize) {
    if let Some(x) = v.get_mut(i) {
        *x = x.wrapping_add(1);
    }
}

/// Which codes a block is written with.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Codes {
    Fixed,
    Dynamic,
}

#[derive(Debug)]
pub struct BitWriter<W: Write> {
    writer: W,
    bits: u64,
    nbits: u32,
    bytes: [u8; BUFFER_SIZE],
    codegen_freq: [i32; CODEGEN_CODE_COUNT],
    nbytes: usize,
    literal_freq: Vec<i32>,
    offset_freq: Vec<i32>,
    codegen: Vec<u8>,
    literal_encoding: Encoder,
    offset_encoding: Encoder,
    codegen_encoding: Encoder,
    fixed_literal: Encoder,
    fixed_offset: Encoder,
    /// `huffOffset`: the offset codes of a block of literals alone.
    huff_offset: Encoder,
    /// The first error writing met, which every later write meets again (Go's sticky
    /// `err`): its kind and words.
    pub err: Option<io::ErrorKind>,
    message: String,
}

impl<W: Write> BitWriter<W> {
    pub fn new(writer: W) -> BitWriter<W> {
        let mut huff_offset = Encoder::new(OFFSET_CODE_COUNT);
        let mut freq = vec![0i32; OFFSET_CODE_COUNT];
        if let Some(f) = freq.first_mut() {
            *f = 1;
        }
        huff_offset.generate(&freq, 15);
        BitWriter {
            writer,
            bits: 0,
            nbits: 0,
            bytes: [0; BUFFER_SIZE],
            codegen_freq: [0; CODEGEN_CODE_COUNT],
            nbytes: 0,
            literal_freq: vec![0; MAX_NUM_LIT],
            offset_freq: vec![0; OFFSET_CODE_COUNT],
            codegen: vec![0; MAX_NUM_LIT + OFFSET_CODE_COUNT + 1],
            literal_encoding: Encoder::new(MAX_NUM_LIT),
            offset_encoding: Encoder::new(OFFSET_CODE_COUNT),
            codegen_encoding: Encoder::new(CODEGEN_CODE_COUNT),
            fixed_literal: Encoder::fixed_literal(),
            fixed_offset: Encoder::fixed_offset(),
            huff_offset,
            err: None,
            message: String::new(),
        }
    }

    /// The first error writing met, which every later write keeps returning.
    pub fn error(&self) -> Option<io::Error> {
        self.err.map(|kind| io::Error::new(kind, self.message.clone()))
    }

    pub fn into_inner(self) -> W {
        self.writer
    }

    fn write(&mut self, b: &[u8]) {
        if self.err.is_some() {
            return;
        }
        if let Err(e) = self.writer.write_all(b) {
            self.err = Some(e.kind());
            self.message = e.to_string();
        }
    }

    fn write_buffered(&mut self, n: usize) {
        let mut out = [0u8; BUFFER_SIZE];
        let n = n.min(BUFFER_SIZE);
        if let (Some(dst), Some(src)) = (out.get_mut(..n), self.bytes.get(..n)) {
            dst.copy_from_slice(src);
        }
        if let Some(b) = out.get(..n) {
            self.write(b);
        }
    }

    pub fn flush(&mut self) {
        if self.err.is_some() {
            self.nbits = 0;
            return;
        }
        let mut n = self.nbytes;
        while self.nbits != 0 {
            if let Some(b) = self.bytes.get_mut(n) {
                *b = self.bits as u8;
            }
            self.bits >>= 8;
            self.nbits = self.nbits.saturating_sub(8);
            n += 1;
        }
        self.bits = 0;
        self.write_buffered(n);
        self.nbytes = 0;
    }

    /// Six bytes of the gathered bits into the buffer, the buffer to the writer once full.
    fn spill(&mut self) {
        let bits = self.bits;
        self.bits >>= 48;
        self.nbits -= 48;
        let n = self.nbytes;
        if let Some(bytes) = self.bytes.get_mut(n..n + 6) {
            for (i, b) in bytes.iter_mut().enumerate() {
                *b = (bits >> (8 * i)) as u8;
            }
        }
        let n = n + 6;
        if n >= BUFFER_FLUSH_SIZE {
            self.write_buffered(n);
            self.nbytes = 0;
        } else {
            self.nbytes = n;
        }
    }

    fn write_bits(&mut self, b: i32, nb: u32) {
        if self.err.is_some() {
            return;
        }
        // uint64(b): a Go conversion of an int32, sign extended.
        self.bits |= (i64::from(b) as u64) << self.nbits;
        self.nbits += nb;
        if self.nbits >= 48 {
            self.spill();
        }
    }

    pub fn write_bytes(&mut self, bytes: &[u8]) {
        if self.err.is_some() {
            return;
        }
        let mut n = self.nbytes;
        if self.nbits & 7 != 0 {
            self.err = Some(io::ErrorKind::Other);
            self.message = "flate: writeBytes with unfinished bits".into();
            return;
        }
        while self.nbits != 0 {
            if let Some(b) = self.bytes.get_mut(n) {
                *b = self.bits as u8;
            }
            self.bits >>= 8;
            self.nbits -= 8;
            n += 1;
        }
        if n != 0 {
            self.write_buffered(n);
        }
        self.nbytes = 0;
        self.write(bytes);
    }

    /// `generateCodegen`: the run-length encoding of the two alphabets' code lengths
    /// (RFC 1951 §3.2.7), and how often each of its codes is used.
    fn generate_codegen(&mut self, num_literals: usize, num_offsets: usize, huff: bool) {
        self.codegen_freq = [0; CODEGEN_CODE_COUNT];
        let lit = &self.literal_encoding;
        let off = if huff {
            &self.huff_offset
        } else {
            &self.offset_encoding
        };
        for i in 0..num_literals {
            if let Some(c) = self.codegen.get_mut(i) {
                *c = get(&lit.codes, i).len as u8;
            }
        }
        for i in 0..num_offsets {
            if let Some(c) = self.codegen.get_mut(num_literals + i) {
                *c = get(&off.codes, i).len as u8;
            }
        }
        if let Some(c) = self.codegen.get_mut(num_literals + num_offsets) {
            *c = BAD_CODE;
        }
        let codegen = &mut self.codegen;
        let freq = &mut self.codegen_freq;
        let mut size = get(codegen, 0);
        let mut count: i32 = 1;
        let mut out = 0usize;
        let put = |codegen: &mut Vec<u8>, out: &mut usize, v: u8| {
            if let Some(c) = codegen.get_mut(*out) {
                *c = v;
            }
            *out += 1;
        };
        let mut in_index = 1usize;
        while size != BAD_CODE {
            let next = get(codegen, in_index);
            in_index += 1;
            if next == size {
                count += 1;
                continue;
            }
            if size != 0 {
                put(codegen, &mut out, size);
                bump(freq, usize::from(size));
                count -= 1;
                while count >= 3 {
                    let n = count.min(6);
                    put(codegen, &mut out, 16);
                    put(codegen, &mut out, (n - 3) as u8);
                    bump(freq, 16);
                    count -= n;
                }
            } else {
                while count >= 11 {
                    let n = count.min(138);
                    put(codegen, &mut out, 18);
                    put(codegen, &mut out, (n - 11) as u8);
                    bump(freq, 18);
                    count -= n;
                }
                if count >= 3 {
                    put(codegen, &mut out, 17);
                    put(codegen, &mut out, (count - 3) as u8);
                    bump(freq, 17);
                    count = 0;
                }
            }
            count -= 1;
            while count >= 0 {
                put(codegen, &mut out, size);
                bump(freq, usize::from(size));
                count -= 1;
            }
            size = next;
            count = 1;
        }
        put(codegen, &mut out, BAD_CODE);
    }

    /// `dynamicSize`: a dynamic block's bits, and how many code length codes it writes.
    fn dynamic_size(&self, huff: bool, extra_bits: i64) -> (i64, usize) {
        let mut num_codegens = CODEGEN_CODE_COUNT;
        while num_codegens > 4 && get(&self.codegen_freq, get(&CODEGEN_ORDER, num_codegens - 1)) == 0 {
            num_codegens -= 1;
        }
        let cf = |i: usize| i64::from(get(&self.codegen_freq, i));
        let header = 3
            + 5
            + 5
            + 4
            + 3 * num_codegens as i64
            + self.codegen_encoding.bit_length(&self.codegen_freq)
            + cf(16) * 2
            + cf(17) * 3
            + cf(18) * 7;
        let off = if huff {
            &self.huff_offset
        } else {
            &self.offset_encoding
        };
        let size = header
            + self.literal_encoding.bit_length(&self.literal_freq)
            + off.bit_length(&self.offset_freq)
            + extra_bits;
        (size, num_codegens)
    }

    fn fixed_size(&self, extra_bits: i64) -> i64 {
        3 + self.fixed_literal.bit_length(&self.literal_freq)
            + self.fixed_offset.bit_length(&self.offset_freq)
            + extra_bits
    }

    /// `storedSize`: a nil input is never stored; an empty one may be.
    fn stored_size(input: Option<&[u8]>) -> Option<i64> {
        let input = input?;
        (input.len() <= MAX_STORE_BLOCK_SIZE).then(|| (input.len() as i64 + 5) * 8)
    }

    fn write_code(&mut self, c: Hcode) {
        if self.err.is_some() {
            return;
        }
        self.bits |= u64::from(c.code) << self.nbits;
        self.nbits += u32::from(c.len);
        if self.nbits >= 48 {
            self.spill();
        }
    }

    fn write_dynamic_header(
        &mut self,
        num_literals: usize,
        num_offsets: usize,
        num_codegens: usize,
        eof: bool,
    ) {
        if self.err.is_some() {
            return;
        }
        self.write_bits(if eof { 5 } else { 4 }, 3);
        self.write_bits(num_literals as i32 - 257, 5);
        self.write_bits(num_offsets as i32 - 1, 5);
        self.write_bits(num_codegens as i32 - 4, 4);
        for i in 0..num_codegens {
            let value = get(&self.codegen_encoding.codes, get(&CODEGEN_ORDER, i)).len;
            self.write_bits(i32::from(value), 3);
        }
        let mut i = 0usize;
        loop {
            let word = get(&self.codegen, i);
            i += 1;
            if word == BAD_CODE {
                break;
            }
            let code = get(&self.codegen_encoding.codes, usize::from(word));
            self.write_code(code);
            let extra = match word {
                16 => Some(2),
                17 => Some(3),
                18 => Some(7),
                _ => None,
            };
            if let Some(nb) = extra {
                self.write_bits(i32::from(get(&self.codegen, i)), nb);
                i += 1;
            }
        }
    }

    pub fn write_stored_header(&mut self, length: usize, eof: bool) {
        if self.err.is_some() {
            return;
        }
        self.write_bits(i32::from(eof), 3);
        self.flush();
        let length = length as u16;
        self.write_bits(i32::from(length), 16);
        self.write_bits(i32::from(!length), 16);
    }

    fn write_fixed_header(&mut self, eof: bool) {
        if self.err.is_some() {
            return;
        }
        self.write_bits(if eof { 3 } else { 2 }, 3);
    }

    /// `writeBlock`: `tokens` with the fewest bits, fixed, dynamic or stored from `input`.
    pub fn write_block(&mut self, tokens: &[Token], eof: bool, input: Option<&[u8]>) {
        if self.err.is_some() {
            return;
        }
        let (num_literals, num_offsets) = self.index_tokens(tokens);
        let stored = Self::stored_size(input);
        let mut extra_bits: i64 = 0;
        if stored.is_some() {
            for code in LENGTH_CODES_START + 8..num_literals {
                extra_bits += i64::from(get(&self.literal_freq, code))
                    * i64::from(get(&LENGTH_EXTRA_BITS, code - LENGTH_CODES_START));
            }
            for code in 4..num_offsets {
                extra_bits +=
                    i64::from(get(&self.offset_freq, code)) * i64::from(get(&OFFSET_EXTRA_BITS, code));
            }
        }
        let mut codes = Codes::Fixed;
        let mut size = self.fixed_size(extra_bits);
        self.generate_codegen(num_literals, num_offsets, false);
        let freq = self.codegen_freq;
        self.codegen_encoding.generate(&freq, 7);
        let (dynamic, num_codegens) = self.dynamic_size(false, extra_bits);
        if dynamic < size {
            size = dynamic;
            codes = Codes::Dynamic;
        }
        if let (Some(stored), Some(input)) = (stored, input)
            && stored < size
        {
            self.write_stored_header(input.len(), eof);
            self.write_bytes(input);
            return;
        }
        let (lit, off) = match codes {
            Codes::Fixed => {
                self.write_fixed_header(eof);
                (self.fixed_literal.codes.clone(), self.fixed_offset.codes.clone())
            }
            Codes::Dynamic => {
                self.write_dynamic_header(num_literals, num_offsets, num_codegens, eof);
                (
                    self.literal_encoding.codes.clone(),
                    self.offset_encoding.codes.clone(),
                )
            }
        };
        self.write_tokens(tokens, &lit, &off);
    }

    /// `writeBlockDynamic`: `tokens` with dynamic codes, or stored where they save less
    /// than a sixteenth.
    pub fn write_block_dynamic(&mut self, tokens: &[Token], eof: bool, input: Option<&[u8]>) {
        if self.err.is_some() {
            return;
        }
        let (num_literals, num_offsets) = self.index_tokens(tokens);
        self.generate_codegen(num_literals, num_offsets, false);
        let freq = self.codegen_freq;
        self.codegen_encoding.generate(&freq, 7);
        let (size, num_codegens) = self.dynamic_size(false, 0);
        if let (Some(stored), Some(input)) = (Self::stored_size(input), input)
            && stored < size + (size >> 4)
        {
            self.write_stored_header(input.len(), eof);
            self.write_bytes(input);
            return;
        }
        self.write_dynamic_header(num_literals, num_offsets, num_codegens, eof);
        let (lit, off) = (
            self.literal_encoding.codes.clone(),
            self.offset_encoding.codes.clone(),
        );
        self.write_tokens(tokens, &lit, &off);
    }

    /// `indexTokens`: the frequencies of `tokens` and the end of block marker after them,
    /// their codes, and how many literal and offset codes are used.
    fn index_tokens(&mut self, tokens: &[Token]) -> (usize, usize) {
        self.literal_freq.iter_mut().for_each(|f| *f = 0);
        self.offset_freq.iter_mut().for_each(|f| *f = 0);
        for &t in tokens.iter().chain(std::iter::once(&END_BLOCK_MARKER)) {
            if t < MATCH_TYPE {
                bump(&mut self.literal_freq, token::literal(t) as usize);
                continue;
            }
            bump(
                &mut self.literal_freq,
                LENGTH_CODES_START + token::length_code(token::length(t)) as usize,
            );
            bump(
                &mut self.offset_freq,
                token::offset_code(token::offset(t)) as usize,
            );
        }
        let mut num_literals = self.literal_freq.len();
        while num_literals > 0 && get(&self.literal_freq, num_literals - 1) == 0 {
            num_literals -= 1;
        }
        let mut num_offsets = self.offset_freq.len();
        while num_offsets > 0 && get(&self.offset_freq, num_offsets - 1) == 0 {
            num_offsets -= 1;
        }
        if num_offsets == 0 {
            // At least one offset, so that the offset tree can be written.
            if let Some(f) = self.offset_freq.first_mut() {
                *f = 1;
            }
            num_offsets = 1;
        }
        let lit = self.literal_freq.clone();
        self.literal_encoding.generate(&lit, 15);
        let off = self.offset_freq.clone();
        self.offset_encoding.generate(&off, 15);
        (num_literals, num_offsets)
    }

    fn write_tokens(&mut self, tokens: &[Token], lit: &[Hcode], off: &[Hcode]) {
        if self.err.is_some() {
            return;
        }
        for &t in tokens.iter().chain(std::iter::once(&END_BLOCK_MARKER)) {
            if t < MATCH_TYPE {
                self.write_code(get(lit, token::literal(t) as usize));
                continue;
            }
            let length = token::length(t);
            let lcode = token::length_code(length) as usize;
            self.write_code(get(lit, lcode + LENGTH_CODES_START));
            let extra = u32::from(get(&LENGTH_EXTRA_BITS, lcode));
            if extra > 0 {
                self.write_bits(length.wrapping_sub(get(&LENGTH_BASE, lcode)) as i32, extra);
            }
            let offset = token::offset(t);
            let ocode = token::offset_code(offset) as usize;
            self.write_code(get(off, ocode));
            let extra = u32::from(get(&OFFSET_EXTRA_BITS, ocode));
            if extra > 0 {
                self.write_bits(offset.wrapping_sub(get(&OFFSET_BASE, ocode)) as i32, extra);
            }
        }
    }

    /// `writeBlockHuff`: `input` as literals with dynamic codes, or stored where they save
    /// less than a sixteenth. The offset frequencies but the first are left as the last
    /// block left them, and count in the size, as Go's do.
    pub fn write_block_huff(&mut self, eof: bool, input: &[u8]) {
        if self.err.is_some() {
            return;
        }
        self.literal_freq.iter_mut().for_each(|f| *f = 0);
        for &b in input {
            bump(&mut self.literal_freq, usize::from(b));
        }
        if let Some(f) = self
            .literal_freq
            .get_mut(usize::try_from(END_BLOCK_MARKER).unwrap_or(256))
        {
            *f = 1;
        }
        const NUM_LITERALS: usize = 257;
        if let Some(f) = self.offset_freq.first_mut() {
            *f = 1;
        }
        const NUM_OFFSETS: usize = 1;
        let lit = self.literal_freq.clone();
        self.literal_encoding.generate(&lit, 15);
        self.generate_codegen(NUM_LITERALS, NUM_OFFSETS, true);
        let freq = self.codegen_freq;
        self.codegen_encoding.generate(&freq, 7);
        let (size, num_codegens) = self.dynamic_size(true, 0);
        if let Some(stored) = Self::stored_size(Some(input))
            && stored < size + (size >> 4)
        {
            self.write_stored_header(input.len(), eof);
            self.write_bytes(input);
            return;
        }
        self.write_dynamic_header(NUM_LITERALS, NUM_OFFSETS, num_codegens, eof);
        let codes = self.literal_encoding.codes.clone();
        for &b in input {
            self.write_code(get(&codes, usize::from(b)));
            if self.err.is_some() {
                return;
            }
        }
        self.write_code(get(&codes, 256));
    }
}
