//! A coded response body decoded as it comes (D242, creds-04): a response to a request with a
//! value filled in asks `identity`, and one that comes coded anyway (RFC 9110 §12.5.3 lets a
//! server code it) is decoded here, so that the echo mask reads the bytes its guest decodes, and
//! it goes on as `identity`. gzip and deflate are decoded by flate2 (zlib-rs), zstd by ruzstd:
//! the decoders the image store and the registry's client already use. Any other coding, br
//! among them, is none this side decodes.
//!
//! Bounds (R03): what one turn of the host's bytes decodes to is at most what the connection
//! holds toward the guest (`tcp::TO_GUEST`), so that a body that decodes past it (a ratio no
//! spec bounds) is refused, never held; a zstd frame's window at most 8 MiB, the most RFC 9659
//! §3 lets a zstd content coding's encoder use; a zstd block at most 128 KiB (RFC 8878
//! §3.1.1.2.3's Block_Maximum_Size), so that what waits for a block's end is bounded.

use std::fmt;
use std::io::{self, Write};

use flate2::{Decompress, FlushDecompress, Status};
use ruzstd::decoding::{BlockDecodingStrategy, FrameDecoder};
use zeroize::Zeroizing;

/// RFC 9659 §3: a zstd content coding's Window_Size is at most 8 MiB.
const ZSTD_WINDOW: u64 = 8 << 20;
/// RFC 8878 §3.1.1.2.3: no block's content is past 128 KiB.
const ZSTD_BLOCK: usize = 128 << 10;
/// RFC 8878 §3.1.1: a Zstandard frame's magic number, and the range skippable frames take
/// (§3.1.2).
const ZSTD_MAGIC: u32 = 0xFD2F_B528;
const SKIPPABLE: std::ops::RangeInclusive<u32> = 0x184D_2A50..=0x184D_2A5F;

/// A content coding this side decodes (RFC 9110 §8.4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Coding {
    Gzip,
    Deflate,
    Zstd,
}

impl Coding {
    /// The coding `name` names (case-insensitive, RFC 9110 §8.4.1; `x-gzip` is gzip's alias,
    /// §8.4.1.3); none for one this side does not decode.
    pub(crate) fn named(name: &[u8]) -> Option<Coding> {
        match name.to_ascii_lowercase().as_slice() {
            b"gzip" | b"x-gzip" => Some(Coding::Gzip),
            b"deflate" => Some(Coding::Deflate),
            b"zstd" => Some(Coding::Zstd),
            _ => None,
        }
    }
}

/// Why a coded body could not be decoded (R04).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Undecodable {
    /// Its bytes are no stream of its coding.
    Corrupt,
    /// It ended before its coding's stream did.
    Truncated,
    /// One turn of it decodes past what the connection holds toward the guest.
    PastBudget,
    /// A zstd frame whose window is past RFC 9659's 8 MiB, or that needs a dictionary.
    Unsupported,
}

impl Undecodable {
    pub(crate) fn why(self) -> &'static str {
        match self {
            Undecodable::Corrupt => "a coded response that is no stream of its coding",
            Undecodable::Truncated => "a coded response that ended before its coding's stream",
            Undecodable::PastBudget => "a coded response that decodes past what the connection holds",
            Undecodable::Unsupported => "a zstd response whose window or dictionary no content coding takes",
        }
    }
}

impl fmt::Display for Undecodable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.why())
    }
}

/// What a decoder writes into: its output, refused past `left` octets (`over`).
struct Budget {
    out: Zeroizing<Vec<u8>>,
    left: usize,
    over: bool,
}

impl Write for Budget {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        let Some(left) = self.left.checked_sub(b.len()) else {
            self.over = true;
            return Err(io::Error::other("past its budget"));
        };
        self.left = left;
        self.out.extend_from_slice(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A body coded `Coding`, decoded as its bytes come.
pub(crate) struct Decoder {
    how: How,
}

enum How {
    /// gzip (RFC 1952), each member's CRC and length checked as it ends.
    Gzip(Box<flate2::write::MultiGzDecoder<Budget>>),
    /// deflate: a zlib stream (RFC 1950), as RFC 9110 §8.4.1.2 defines it, or a raw deflate one
    /// (RFC 1951), which some servers send in its place; told apart by its first two octets.
    Deflate {
        inflate: Option<Box<Decompress>>,
        first: Vec<u8>,
        ended: bool,
    },
    Zstd(Box<Zstd>),
}

impl fmt::Debug for Decoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.how {
            How::Gzip(_) => "Decoder(gzip)",
            How::Deflate { .. } => "Decoder(deflate)",
            How::Zstd(_) => "Decoder(zstd)",
        })
    }
}

impl Decoder {
    pub(crate) fn new(coding: Coding) -> Decoder {
        let how = match coding {
            Coding::Gzip => How::Gzip(Box::new(flate2::write::MultiGzDecoder::new(Budget {
                out: Zeroizing::new(Vec::new()),
                left: 0,
                over: false,
            }))),
            Coding::Deflate => How::Deflate {
                inflate: None,
                first: Vec::new(),
                ended: false,
            },
            Coding::Zstd => How::Zstd(Box::default()),
        };
        Decoder { how }
    }

    /// `coded`'s bytes decoded, appended to `out`: at most `most` octets of them.
    pub(crate) fn feed(&mut self, coded: &[u8], out: &mut Vec<u8>, most: usize) -> Result<(), Undecodable> {
        match &mut self.how {
            How::Gzip(gz) => {
                gz.get_mut().left = most;
                let wrote = gz.write_all(coded).and_then(|()| gz.flush());
                take(gz.get_mut(), out, wrote)
            }
            How::Deflate { inflate, first, ended } => match inflate {
                Some(inflate) => inflated(inflate, ended, coded, out, most),
                None => {
                    // Its first two octets, held until both have come, tell its framing.
                    first.extend_from_slice(coded);
                    if first.len() < 2 {
                        return Ok(());
                    }
                    let mut made = Box::new(Decompress::new(zlib(first)));
                    let start = Zeroizing::new(std::mem::take(first));
                    let went = inflated(&mut made, ended, &start, out, most);
                    *inflate = Some(made);
                    went
                }
            },
            How::Zstd(z) => z.feed(coded, out, most),
        }
    }

    /// The body's end: what its decoder still holds, appended to `out`, at most `most`; refused
    /// where its coding's stream has not ended.
    pub(crate) fn finish(&mut self, out: &mut Vec<u8>, most: usize) -> Result<(), Undecodable> {
        match &mut self.how {
            How::Gzip(gz) => {
                gz.get_mut().left = most;
                let finished = gz.try_finish();
                take(gz.get_mut(), out, finished).map_err(|e| match e {
                    Undecodable::Corrupt => Undecodable::Truncated,
                    e => e,
                })
            }
            How::Deflate { ended, .. } => ended.then_some(()).ok_or(Undecodable::Truncated),
            How::Zstd(z) => z.finish(),
        }
    }
}

/// What a gzip decoder wrote, moved to `out`; its write's failure told as its cause.
fn take(budget: &mut Budget, out: &mut Vec<u8>, wrote: io::Result<()>) -> Result<(), Undecodable> {
    out.extend_from_slice(&budget.out);
    budget.out.clear();
    match wrote {
        Ok(()) => Ok(()),
        Err(_) if budget.over => Err(Undecodable::PastBudget),
        Err(_) => Err(Undecodable::Corrupt),
    }
}

/// Whether a deflate body's first two octets are a zlib header (RFC 1950 §2.2: CM 8, a window
/// of at most 32 KiB, and CMF·256 + FLG a multiple of 31), which a raw deflate stream's first
/// block header never is together.
fn zlib(first: &[u8]) -> bool {
    let (Some(&cmf), Some(&flg)) = (first.first(), first.get(1)) else {
        return false;
    };
    cmf & 0x0f == 8 && cmf >> 4 <= 7 && (u16::from(cmf) << 8 | u16::from(flg)) % 31 == 0
}

/// `coded` through `inflate`, its output appended to `out`, at most `most` in all.
fn inflated(
    inflate: &mut Decompress,
    ended: &mut bool,
    mut coded: &[u8],
    out: &mut Vec<u8>,
    most: usize,
) -> Result<(), Undecodable> {
    let mut buf = Zeroizing::new(vec![0u8; 32 << 10]);
    let mut left = most;
    while !coded.is_empty() {
        if *ended {
            return Err(Undecodable::Corrupt);
        }
        let (before_in, before_out) = (inflate.total_in(), inflate.total_out());
        let status = inflate
            .decompress(coded, &mut buf, FlushDecompress::None)
            .map_err(|_| Undecodable::Corrupt)?;
        let read = usize::try_from(inflate.total_in().saturating_sub(before_in)).unwrap_or(usize::MAX);
        let wrote = usize::try_from(inflate.total_out().saturating_sub(before_out)).unwrap_or(usize::MAX);
        left = left.checked_sub(wrote).ok_or(Undecodable::PastBudget)?;
        out.extend_from_slice(buf.get(..wrote).ok_or(Undecodable::Corrupt)?);
        coded = coded.get(read..).unwrap_or_default();
        match status {
            Status::StreamEnd => *ended = true,
            Status::Ok => {}
            Status::BufError if read == 0 && wrote == 0 => return Err(Undecodable::Corrupt),
            Status::BufError => {}
        }
    }
    Ok(())
}

/// A zstd body (RFC 8878), one or more frames: each frame's header and each block read once it
/// has come whole, so that the decoder never meets the end of what came part way through one.
#[derive(Default)]
struct Zstd {
    decoder: FrameDecoder,
    pending: Zeroizing<Vec<u8>>,
    /// Within a frame, its blocks coming, and whether it ends in a checksum; none between frames.
    frame: Option<bool>,
}

impl Zstd {
    fn feed(&mut self, coded: &[u8], out: &mut Vec<u8>, most: usize) -> Result<(), Undecodable> {
        self.pending.extend_from_slice(coded);
        let mut left = most;
        loop {
            let took = match self.frame {
                None => self.header()?,
                Some(checksum) => self.block(checksum)?,
            };
            let Some(took) = took else {
                return Ok(());
            };
            let took = took.min(self.pending.len());
            self.pending.drain(..took);
            if let Some(decoded) = self.decoder.collect() {
                left = left.checked_sub(decoded.len()).ok_or(Undecodable::PastBudget)?;
                out.extend_from_slice(&decoded);
                drop(Zeroizing::new(decoded));
            }
            if self.frame.is_some() && self.decoder.is_finished() {
                let sum = self.decoder.get_checksum_from_data();
                if sum.is_some() && sum != self.decoder.get_calculated_checksum() {
                    return Err(Undecodable::Corrupt);
                }
                self.frame = None;
            }
        }
    }

    fn finish(&self) -> Result<(), Undecodable> {
        if self.frame.is_some() || !self.pending.is_empty() {
            return Err(Undecodable::Truncated);
        }
        Ok(())
    }

    /// A frame's header, or a skippable frame (RFC 8878 §3.1.2), once whole: how many octets it
    /// took; none while it has not come.
    fn header(&mut self) -> Result<Option<usize>, Undecodable> {
        let p = &self.pending;
        let Some(magic) = p.get(..4).and_then(|m| <[u8; 4]>::try_from(m).ok()) else {
            return Ok(None);
        };
        let magic = u32::from_le_bytes(magic);
        if SKIPPABLE.contains(&magic) {
            let Some(size) = p.get(4..8).and_then(|s| <[u8; 4]>::try_from(s).ok()) else {
                return Ok(None);
            };
            let whole = usize::try_from(u32::from_le_bytes(size))
                .ok()
                .and_then(|n| n.checked_add(8))
                .ok_or(Undecodable::Corrupt)?;
            return Ok((p.len() >= whole).then_some(whole));
        }
        if magic != ZSTD_MAGIC {
            return Err(Undecodable::Corrupt);
        }
        let Some(&fhd) = p.get(4) else {
            return Ok(None);
        };
        // RFC 8878 §3.1.1.1: Frame_Header_Descriptor's fields, which say the header's length.
        let single = fhd & 0x20 != 0;
        let checksum = fhd & 0x04 != 0;
        let dict = fhd & 0x03;
        let fcs = match fhd >> 6 {
            0 => usize::from(single),
            1 => 2,
            2 => 4,
            _ => 8,
        };
        if dict != 0 {
            return Err(Undecodable::Unsupported);
        }
        let len = 5 + usize::from(!single) + fcs;
        let Some(header) = p.get(..len) else {
            return Ok(None);
        };
        self.decoder.set_max_window_size(ZSTD_WINDOW);
        self.decoder.reset(header).map_err(|e| match e {
            ruzstd::decoding::errors::FrameDecoderError::WindowSizeTooBig { .. } => Undecodable::Unsupported,
            _ => Undecodable::Corrupt,
        })?;
        self.frame = Some(checksum);
        Ok(Some(len))
    }

    /// The frame's next block (RFC 8878 §3.1.1.2), its checksum with it where it is the last
    /// and the frame has one, once whole and decoded: how many octets it took; none while it has
    /// not come.
    fn block(&mut self, checksum: bool) -> Result<Option<usize>, Undecodable> {
        let p = &self.pending;
        let Some(h) = p.get(..3) else {
            return Ok(None);
        };
        let h = h.iter().rev().fold(0u32, |h, &b| h << 8 | u32::from(b));
        let last = h & 1 != 0;
        let size = usize::try_from(h >> 3).map_err(|_| Undecodable::Corrupt)?;
        if size > ZSTD_BLOCK {
            return Err(Undecodable::Corrupt);
        }
        let body = match (h >> 1) & 3 {
            0 | 2 => size,
            1 => 1,
            _ => return Err(Undecodable::Corrupt),
        };
        let whole = 3 + body + if last && checksum { 4 } else { 0 };
        let Some(block) = p.get(..whole) else {
            return Ok(None);
        };
        self.decoder
            .decode_blocks(block, BlockDecodingStrategy::UptoBlocks(1))
            .map_err(|_| Undecodable::Corrupt)?;
        Ok(Some(whole))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn decoded(coding: Coding, coded: &[u8], piece: usize) -> Result<Vec<u8>, Undecodable> {
        let mut d = Decoder::new(coding);
        let mut out = Vec::new();
        for p in coded.chunks(piece) {
            d.feed(p, &mut out, usize::MAX)?;
        }
        d.finish(&mut out, usize::MAX)?;
        Ok(out)
    }

    fn text() -> Vec<u8> {
        (0..200_000u32).flat_map(|i| format!("{{\"n\":{}}}", i % 977).into_bytes()).collect()
    }

    /// Each coding, fed whole and a byte at a time, decodes to what was coded; a truncated one is
    /// refused, and one past its budget.
    #[test]
    fn each_coding_decodes_as_it_comes_and_ends_whole() {
        let text = text();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&text).unwrap();
        let gz = gz.finish().unwrap();
        let mut zl = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        zl.write_all(&text).unwrap();
        let zl = zl.finish().unwrap();
        let mut raw = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        raw.write_all(&text).unwrap();
        let raw = raw.finish().unwrap();
        let zs = ruzstd::encoding::compress_to_vec(&text[..], ruzstd::encoding::CompressionLevel::Fastest);
        for (coding, coded) in [
            (Coding::Gzip, &gz),
            (Coding::Deflate, &zl),
            (Coding::Deflate, &raw),
            (Coding::Zstd, &zs),
        ] {
            for piece in [coded.len(), 1, 4093] {
                assert!(decoded(coding, coded, piece).unwrap() == text, "{coding:?} in pieces of {piece}");
            }
            assert_eq!(
                decoded(coding, &coded[..coded.len() - 3], 4093),
                Err(Undecodable::Truncated),
                "{coding:?}"
            );
            let mut d = Decoder::new(coding);
            assert_eq!(
                d.feed(coded, &mut Vec::new(), text.len() / 2),
                Err(Undecodable::PastBudget),
                "{coding:?}"
            );
        }
        assert_eq!(
            decoded(Coding::Gzip, b"not gzip at all", 4),
            Err(Undecodable::Corrupt)
        );
        assert_eq!(decoded(Coding::Zstd, b"not zstd", 4), Err(Undecodable::Corrupt));
    }

    /// A zstd frame whose window is past RFC 9659's 8 MiB is refused before it is decoded.
    #[test]
    fn a_zstd_window_past_8_mib_is_refused() {
        // Magic, a descriptor of no single segment and no checksum, a window descriptor of
        // exponent 14 (2^24 = 16 MiB, RFC 8878 §3.1.1.1.2), and an empty last raw block.
        let frame = [0x28, 0xb5, 0x2f, 0xfd, 0x00, 14 << 3, 0x01, 0x00, 0x00];
        assert_eq!(decoded(Coding::Zstd, &frame, 9), Err(Undecodable::Unsupported));
    }
}
