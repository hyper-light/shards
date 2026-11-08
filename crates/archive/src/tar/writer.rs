//! Go's tar.Writer (go1.26.1 src/archive/tar/writer.go): a header is USTAR where USTAR
//! holds it and PAX otherwise. Go writes GNU headers only for what neither holds, which no
//! header go-archive writes reaches (its headers ask for PAX); those are refused.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};

use super::strconv::{format_octal, format_pax_record, format_string, is_ascii, to_ascii};
use super::{
    BLOCK, ERR_FIELD_TOO_LONG, Format, Header, MAX_SPECIAL, NAME_SIZE, PREFIX_SIZE, TYPE_DIR, TYPE_REG,
    TYPE_REGA, TYPE_XGLOBAL_HEADER, TYPE_XHEADER, Time, allowed_formats, block_padding, header_only,
};
use crate::error::Error;
use crate::gopath::posix;

/// Writes an archive: a header, then its data, and so on, then [`Writer::finish`].
#[derive(Debug)]
pub struct Writer<W> {
    w: W,
    /// Data still owed to the current entry.
    remaining: u64,
    /// Zeros owed after it.
    pad: u64,
}

impl<W: Write> Writer<W> {
    pub fn new(w: W) -> Writer<W> {
        Writer {
            w,
            remaining: 0,
            pad: 0,
        }
    }

    /// Flush: the last entry must be complete; its padding is written.
    fn flush(&mut self) -> Result<(), Error> {
        if self.remaining > 0 {
            return Err(Error::other(format!(
                "archive/tar: missed writing {} bytes",
                self.remaining
            )));
        }
        let pad = usize::try_from(self.pad).unwrap_or(0);
        self.w.write_all([0u8; BLOCK].get(..pad).unwrap_or_default())?;
        self.pad = 0;
        Ok(())
    }

    /// WriteHeader.
    pub fn write_header(&mut self, hdr: &Header) -> Result<(), Error> {
        self.flush()?;
        let mut h = hdr.clone();
        if h.typeflag == TYPE_REGA {
            h.typeflag = if h.name.ends_with(b"/") {
                TYPE_DIR
            } else {
                TYPE_REG
            };
        }
        // Without a format asked for, times are rounded to seconds and access and change
        // times dropped, so a nominal header stays USTAR.
        if h.format == Format::UNKNOWN {
            if !h.mtime.is_zero() && h.mtime.nsec >= 500_000_000 {
                h.mtime = Time::unix(h.mtime.sec.wrapping_add(1), 0);
            } else if !h.mtime.is_zero() {
                h.mtime = Time::unix(h.mtime.sec, 0);
            }
            h.atime = Time::ZERO;
            h.ctime = Time::ZERO;
        }
        let allowed = allowed_formats(&h)?;
        if allowed.format.has(Format::USTAR) {
            self.write_ustar(&h)
        } else if allowed.format.has(Format::PAX) {
            self.write_pax(&h, &allowed.pax)
        } else if allowed.format.has(Format::GNU) {
            Err(Error::other(
                "archive/tar: only the GNU format holds this header, which is not written",
            ))
        } else {
            Err(allowed
                .why
                .unwrap_or_else(|| Error::other("archive/tar: cannot encode header")))
        }
    }

    fn write_ustar(&mut self, h: &Header) -> Result<(), Error> {
        let (prefix, name) = split_ustar_path(&h.name).unwrap_or((&[], &h.name));
        let mut blk = template(h, name, &h.linkname);
        format_string(&mut blk[345..500], prefix);
        set_format(&mut blk);
        self.write_raw_header(&blk, h.size, h.typeflag)
    }

    fn write_pax(&mut self, h: &Header, records: &BTreeMap<Vec<u8>, Vec<u8>>) -> Result<(), Error> {
        let global = h.typeflag == TYPE_XGLOBAL_HEADER;
        if !records.is_empty() || global {
            let mut data = Vec::new();
            for (k, v) in records {
                let rec = format_pax_record(k, v).ok_or_else(super::err_header)?;
                data.extend_from_slice(&rec);
            }
            let (name, flag) = if global {
                let name = if h.name.is_empty() {
                    b"GlobalHead.0.0".to_vec()
                } else {
                    h.name.clone()
                };
                (name, TYPE_XGLOBAL_HEADER)
            } else {
                let (dir, file) = posix::split(&h.name);
                (posix::join(&[dir, b"PaxHeaders.0", file]), TYPE_XHEADER)
            };
            if data.len() > MAX_SPECIAL {
                return Err(Error::other(ERR_FIELD_TOO_LONG));
            }
            self.write_raw_file(&name, &data, flag)?;
            if global {
                return Ok(());
            }
        }
        let mut blk = template(h, &to_ascii(&h.name), &to_ascii(&h.linkname));
        let (uname, gname) = (to_ascii(&h.uname), to_ascii(&h.gname));
        format_string(&mut blk[265..297], &uname);
        format_string(&mut blk[297..329], &gname);
        set_format(&mut blk);
        self.write_raw_header(&blk, h.size, h.typeflag)
    }

    /// writeRawFile: a header and data of Go's own, a PAX header here.
    fn write_raw_file(&mut self, name: &[u8], data: &[u8], flag: u8) -> Result<(), Error> {
        let mut name = to_ascii(name);
        name.truncate(NAME_SIZE);
        while name.last() == Some(&b'/') {
            name.pop();
        }
        let mut blk = [0u8; BLOCK];
        blk[156] = flag;
        format_string(&mut blk[0..100], &name);
        format_octal(&mut blk[100..108], 0);
        format_octal(&mut blk[108..116], 0);
        format_octal(&mut blk[116..124], 0);
        let size = i64::try_from(data.len()).unwrap_or(i64::MAX);
        format_octal(&mut blk[124..136], size);
        format_octal(&mut blk[136..148], 0);
        set_format(&mut blk);
        self.write_raw_header(&blk, size, flag)?;
        self.w.write_all(data)?;
        self.remaining = 0;
        Ok(())
    }

    fn write_raw_header(&mut self, blk: &[u8; BLOCK], size: i64, flag: u8) -> Result<(), Error> {
        self.flush()?;
        self.w.write_all(blk)?;
        let size = if header_only(flag) {
            0
        } else {
            u64::try_from(size).unwrap_or(0)
        };
        self.remaining = size;
        self.pad = block_padding(size);
        Ok(())
    }

    /// Copies the current entry's data from `src`: exactly what the header owes, the rest
    /// of `src` unread, as Go's regFileWriter stops at the size. A `src` that ends short
    /// is an error: the archive cannot be finished.
    pub fn copy_from(&mut self, src: impl Read) -> Result<(), Error> {
        let owed = self.remaining;
        let n = io::copy(&mut src.take(owed), &mut self.w)?;
        self.remaining -= n;
        if self.remaining > 0 {
            return Err(Error::other(format!(
                "archive/tar: missed writing {} bytes",
                self.remaining
            )));
        }
        Ok(())
    }

    /// Close: the last entry padded, then the two zero blocks that end an archive.
    pub fn finish(mut self) -> Result<W, Error> {
        self.flush()?;
        self.w.write_all(&[0u8; 2 * BLOCK])?;
        Ok(self.w)
    }
}

impl<W: Write> Write for Writer<W> {
    /// Some of the current entry's data; past its size, Go's ErrWriteTooLong.
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        if b.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            return Err(io::Error::other("archive/tar: write too long"));
        }
        let n = usize::try_from(self.remaining).map_or(b.len(), |r| r.min(b.len()));
        let n = self.w.write(b.get(..n).unwrap_or_default())?;
        self.remaining -= n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.w.flush()
    }
}

/// templateV7Plus: the V7 and USTAR fields, with numbers in octal (0 where one does not
/// fit, as in a PAX header whose records carry it).
fn template(h: &Header, name: &[u8], link: &[u8]) -> [u8; BLOCK] {
    let mut blk = [0u8; BLOCK];
    let mtime = if h.mtime.is_zero() { 0 } else { h.mtime.sec };
    blk[156] = h.typeflag;
    format_string(&mut blk[0..100], name);
    format_string(&mut blk[157..257], link);
    format_octal(&mut blk[100..108], h.mode);
    format_octal(&mut blk[108..116], h.uid);
    format_octal(&mut blk[116..124], h.gid);
    format_octal(&mut blk[124..136], h.size);
    format_octal(&mut blk[136..148], mtime);
    format_string(&mut blk[265..297], &h.uname);
    format_string(&mut blk[297..329], &h.gname);
    format_octal(&mut blk[329..337], h.devmajor);
    format_octal(&mut blk[337..345], h.devminor);
    blk
}

/// setFormat for USTAR and PAX, which share the magic, then the checksum.
fn set_format(blk: &mut [u8; BLOCK]) {
    blk[257..263].copy_from_slice(b"ustar\0");
    blk[263..265].copy_from_slice(b"00");
    let (sum, _) = checksum(blk);
    format_octal(&mut blk[148..155], sum);
    blk[155] = b' ';
}

/// computeChecksum: unsigned and signed sums, the checksum field counted as spaces.
pub(crate) fn checksum(blk: &[u8; BLOCK]) -> (i64, i64) {
    let (mut unsigned, mut signed) = (0i64, 0i64);
    for (i, &c) in blk.iter().enumerate() {
        let c = if (148..156).contains(&i) { b' ' } else { c };
        unsigned += i64::from(c);
        signed += i64::from(c as i8);
    }
    (unsigned, signed)
}

/// splitUSTARPath: a long ASCII name split at a slash into a prefix of at most 155 bytes
/// and a name of at most 100.
pub(crate) fn split_ustar_path(name: &[u8]) -> Option<(&[u8], &[u8])> {
    let mut length = name.len();
    if length <= NAME_SIZE || !is_ascii(name) {
        return None;
    } else if length > PREFIX_SIZE + 1 {
        length = PREFIX_SIZE + 1;
    } else if name.last() == Some(&b'/') {
        length -= 1;
    }
    let i = name.get(..length)?.iter().rposition(|&c| c == b'/')?;
    let (prefix, rest) = name.split_at_checked(i)?;
    let suffix = rest.get(1..)?;
    if i == 0 || suffix.len() > NAME_SIZE || suffix.is_empty() || i > PREFIX_SIZE {
        return None;
    }
    Some((prefix, suffix))
}
