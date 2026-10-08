//! Go's `compress/flate` and `compress/gzip` writers, ported to make the same bytes: the
//! layers BuildKit gzips (util/compression, `gzip.NewWriterLevel`, written whole and
//! closed) have the digests Docker's do only if every bit of the deflate stream is Go's.
//! Ported from Go 1.26.1, whose compress/flate and compress/gzip writers are Go 1.25.8's
//! (Docker 29.3.1's) but for the order of a struct's fields; held to Go's own output by
//! `tests/oracle.rs`.

mod bits;
mod deflate;
mod fast;
mod huffman;
mod token;

use std::io::{self, Write};

use deflate::Compressor;

/// The levels Go names (flate's constants).
pub const NO_COMPRESSION: i32 = 0;
pub const BEST_SPEED: i32 = 1;
pub const BEST_COMPRESSION: i32 = 9;
pub const DEFAULT_COMPRESSION: i32 = -1;
pub const HUFFMAN_ONLY: i32 = -2;

/// `flate.Writer`: a raw deflate stream (RFC 1951).
#[derive(Debug)]
pub struct DeflateWriter<W: Write> {
    d: Compressor<W>,
}

impl<W: Write> DeflateWriter<W> {
    /// `flate.NewWriter`: `level` from -2 (HuffmanOnly) to 9, -1 for the default.
    pub fn new(writer: W, level: i32) -> io::Result<DeflateWriter<W>> {
        Ok(DeflateWriter {
            d: Compressor::new(writer, level)?,
        })
    }

    /// `Close`, then the writer.
    pub fn finish(mut self) -> io::Result<W> {
        self.d.close()?;
        Ok(self.d.into_inner())
    }
}

impl<W: Write> Write for DeflateWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.d.write(buf)?;
        Ok(buf.len())
    }

    /// Nothing: Go's `Flush` writes a sync marker, which BuildKit's layers never carry.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// `gzip.Writer` as BuildKit makes it: no name, comment, extra field or time; the OS
/// unknown (255); XFL 2 at level 9, 4 at level 1, else 0 (RFC 1952).
#[derive(Debug)]
pub struct GzipWriter<W: Write> {
    level: i32,
    out: Option<W>,
    d: Option<Compressor<W>>,
    crc: u32,
    size: u32,
}

impl<W: Write> GzipWriter<W> {
    /// `gzip.NewWriterLevel`.
    pub fn new(writer: W, level: i32) -> io::Result<GzipWriter<W>> {
        if !(HUFFMAN_ONLY..=BEST_COMPRESSION).contains(&level) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("gzip: invalid compression level: {level}"),
            ));
        }
        Ok(GzipWriter {
            level,
            out: Some(writer),
            d: None,
            crc: 0,
            size: 0,
        })
    }

    /// The header, written once, before the first bytes (Go writes it lazily).
    fn started(&mut self) -> io::Result<&mut Compressor<W>> {
        if self.d.is_none() {
            let mut w = self
                .out
                .take()
                .ok_or_else(|| io::Error::other("gzip: closed writer"))?;
            let xfl = match self.level {
                BEST_COMPRESSION => 2,
                BEST_SPEED => 4,
                _ => 0,
            };
            w.write_all(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, xfl, 255])?;
            self.d = Some(Compressor::new(w, self.level)?);
        }
        self.d
            .as_mut()
            .ok_or_else(|| io::Error::other("gzip: closed writer"))
    }

    /// `Close`: the deflate stream ended, then its CRC-32 and size; the writer.
    pub fn finish(mut self) -> io::Result<W> {
        let (crc, size) = (self.crc, self.size);
        let d = self.started()?;
        d.close()?;
        let mut w = self
            .d
            .take()
            .map(Compressor::into_inner)
            .ok_or_else(|| io::Error::other("gzip: closed writer"))?;
        w.write_all(&crc.to_le_bytes())?;
        w.write_all(&size.to_le_bytes())?;
        Ok(w)
    }
}

impl<W: Write> Write for GzipWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.started()?.write(buf)?;
        self.size = self.size.wrapping_add(buf.len() as u32);
        self.crc = crc32_update(self.crc, buf);
        Ok(buf.len())
    }

    /// Nothing, as [`DeflateWriter::flush`].
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The IEEE CRC-32 table (`crc32.IEEETable`).
const CRC_TABLE: [u32; 256] = crc_table();

// Evaluated at compile time only: an index out of range fails the build, never a run.
#[allow(clippy::indexing_slicing)]
const fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 == 1 { 0xedb88320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

/// `crc32.Update` with the IEEE table.
fn crc32_update(crc: u32, p: &[u8]) -> u32 {
    !p.iter().fold(!crc, |c, &b| {
        let i = ((c ^ u32::from(b)) & 0xff) as usize;
        CRC_TABLE.get(i).copied().unwrap_or(0) ^ (c >> 8)
    })
}
