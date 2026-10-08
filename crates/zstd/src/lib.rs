//! klauspost/compress's zstd writer, ported to make the same bytes: the layers BuildKit
//! compresses with zstd (util/compression/zstd.go: `zstd.NewWriter` at
//! `EncoderLevelFromZstd(level)`, or no level, written whole and closed) have Docker's
//! digests only if every bit of the frame is klauspost's. Ported from v1.19.2, BuildKit
//! v0.28.1's, encoder only: no dictionaries, no assembly; held to Go's own output by
//! `tests/oracle.rs`.
//!
//! The Go writer encodes blocks concurrently by default, which makes the same bytes as
//! encoding them in turn (`WithEncoderConcurrency(1)`): the oracle records both for every
//! case. This is the latter.

mod bits;
mod block;
mod enc;
mod entropy;
mod fse;
mod huff;
mod util;
mod xxhash;

use std::io::{self, Write};

use enc::{Encoder, Level, MAX_COMPRESSED_BLOCK_SIZE};
use fse::FseEncoder;

/// `frameMagic`.
const FRAME_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];
/// `MinWindowSize`.
const MIN_WINDOW_SIZE: usize = 1 << 10;

/// `frameHeader.appendTo`, with neither dictionary nor checksum off.
fn frame_header(dst: &mut Vec<u8>, content_size: u64, window_size: u32, single_segment: bool) {
    dst.extend_from_slice(&FRAME_MAGIC);
    let fcs = u8::from(content_size >= 256)
        + u8::from(content_size >= 65536 + 256)
        + u8::from(content_size >= 0xffff_ffff);
    dst.push(1 << 2 | u8::from(single_segment) << 5 | fcs << 6);
    if !single_segment {
        let log = util::len32(window_size.wrapping_sub(1)).saturating_sub(10);
        dst.push((log << 3) as u8);
    }
    let cs = content_size.to_le_bytes();
    match fcs {
        0 if single_segment => dst.push(cs[0]),
        0 => {}
        1 => dst.extend_from_slice(&(content_size.wrapping_sub(256) as u16).to_le_bytes()),
        2 => dst.extend_from_slice(&(content_size as u32).to_le_bytes()),
        _ => dst.extend_from_slice(&cs),
    }
}

fn invalid(e: block::Error) -> io::Error {
    let block::Error::Internal(s) = e;
    io::Error::other(s)
}

/// `zstd.Encoder` writing a stream: blocks as input fills them, one frame.
#[derive(Debug)]
pub struct Writer<W: Write> {
    w: W,
    enc: Encoder,
    predef: Box<[FseEncoder; 3]>,
    block_size: usize,
    all_lit_entropy: bool,
    filling: Vec<u8>,
    header_written: bool,
}

impl<W: Write> Writer<W> {
    /// `zstd.NewWriter(w)`, and with `WithEncoderLevel(EncoderLevelFromZstd(level))`
    /// where `level` is some.
    pub fn new(w: W, level: Option<i32>) -> Writer<W> {
        let (level, window, block_size, all_lit_entropy) = match level {
            None => (Level::Default, 8 << 20, MAX_COMPRESSED_BLOCK_SIZE, false),
            Some(n) if n < 3 => (Level::Fastest, 4 << 20, 1 << 16, false),
            Some(n) if n < 6 => (Level::Default, 8 << 20, MAX_COMPRESSED_BLOCK_SIZE, false),
            Some(n) if n < 10 => (Level::Better, 8 << 20, MAX_COMPRESSED_BLOCK_SIZE, true),
            Some(_) => (Level::Best, 8 << 20, MAX_COMPRESSED_BLOCK_SIZE, true),
        };
        let mut enc = Encoder::new(level, window);
        enc.reset();
        Writer {
            w,
            enc,
            predef: Box::new(enc::predef()),
            block_size,
            all_lit_entropy,
            filling: Vec::with_capacity(block_size),
            header_written: false,
        }
    }

    /// `Close`: the last block and the checksum; then the writer.
    pub fn finish(mut self) -> io::Result<W> {
        if self.next_block(true)? {
            let mut crc = Vec::with_capacity(4);
            self.enc.base.append_crc(&mut crc);
            self.w.write_all(&crc)?;
        }
        Ok(self.w)
    }

    /// `nextBlock`: the block filled, the frame's header first; whether the frame still
    /// needs its checksum.
    fn next_block(&mut self, last: bool) -> io::Result<bool> {
        if !self.header_written {
            if last && !self.filling.is_empty() {
                let frame = self.encode_all()?;
                self.w.write_all(&frame)?;
                self.filling.clear();
                self.header_written = true;
                return Ok(false);
            }
            let mut h = Vec::with_capacity(6);
            frame_header(&mut h, 0, self.enc.base.window_size(0) as u32, false);
            self.w.write_all(&h)?;
            self.header_written = true;
        }
        let blk = &mut self.enc.base.blk;
        if self.filling.is_empty() {
            if last {
                blk.reset();
                blk.last = true;
                blk.encode_raw(&[]);
                self.w.write_all(&blk.output)?;
            }
            return Ok(true);
        }
        blk.reset();
        self.enc.encode(&self.filling, &self.predef);
        let blk = &mut self.enc.base.blk;
        blk.last = last;
        blk.encode(&self.filling, false, !self.all_lit_entropy, &self.predef)
            .map_err(invalid)?;
        self.w.write_all(&blk.output)?;
        self.filling.clear();
        Ok(true)
    }

    /// `encodeAll` of what is filled, at most a block: a frame of one block, which
    /// records its size.
    fn encode_all(&mut self) -> io::Result<Vec<u8>> {
        let src = &self.filling;
        let single = src.len() <= self.enc.base.window_size(i64::MAX) as usize && src.len() > MIN_WINDOW_SIZE;
        let mut dst = Vec::with_capacity(src.len() + 32);
        let window = self.enc.base.window_size(src.len() as i64) as u32;
        frame_header(&mut dst, src.len() as u64, window, single);
        self.enc.reset();
        self.enc.base.crc.write(src);
        self.enc.base.blk.last = true;
        self.enc.encode_no_hist(src, &self.predef);
        let blk = &mut self.enc.base.blk;
        blk.encode(src, false, !self.all_lit_entropy, &self.predef)
            .map_err(invalid)?;
        dst.extend_from_slice(&blk.output);
        self.enc.base.append_crc(&mut dst);
        Ok(dst)
    }
}

impl<W: Write> Write for Writer<W> {
    /// `writeBlocks`: input fills blocks, each encoded once full.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut p = buf;
        while !p.is_empty() {
            let room = self.block_size - self.filling.len();
            let (add, rest) = p.split_at(room.min(p.len()));
            self.enc.base.crc.write(add);
            self.filling.extend_from_slice(add);
            p = rest;
            if self.filling.len() < self.block_size {
                break;
            }
            self.next_block(false)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.w.flush()
    }
}

/// `input` compressed as BuildKit's writer compresses a layer: written whole and closed.
pub fn compress(level: Option<i32>, input: &[u8]) -> io::Result<Vec<u8>> {
    let mut w = Writer::new(Vec::new(), level);
    w.write_all(input)?;
    w.finish()
}
