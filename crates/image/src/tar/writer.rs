//! Writes tar archives as Go's archive/tar writes them (go1.26.1 src/archive/tar
//! writer.go, common.go, strconv.go and format.go), because BuildKit writes the layers it
//! builds with it, through containerd's ChangeWriter: a header is USTAR where USTAR can
//! hold it, and PAX otherwise. GNU headers, which Go writes only for what neither can
//! hold, are refused: no header shards writes needs them.
//!
//! Times are whole seconds, as ChangeWriter truncates them and Go rounds them when no
//! format is asked for; access and change times are never written, as both clear them.

use std::collections::BTreeMap;
use std::io::Write;

use crate::{Error, bad};

pub const REG: u8 = b'0';
pub const LINK: u8 = b'1';
pub const SYMLINK: u8 = b'2';
pub const CHAR: u8 = b'3';
pub const BLOCK_DEVICE: u8 = b'4';
pub const DIR: u8 = b'5';
pub const FIFO: u8 = b'6';
const XHEADER: u8 = b'x';

const BLOCK: usize = 512;
const NAME_SIZE: usize = 100;
const PREFIX_SIZE: usize = 155;
/// Go's maxSpecialFileSize, the largest PAX header written.
const MAX_SPECIAL: usize = 1 << 20;
/// Go's zero time, January 1 of year 1, in Unix seconds: Go writes it as 0.
const ZERO_TIME: i64 = -62_135_596_800;

/// The keys whose records Go makes from a header's own fields, so a header's
/// `pax` records under them are not copied.
const BASIC_KEYS: [&[u8]; 10] = [
    b"path",
    b"linkpath",
    b"size",
    b"uid",
    b"gid",
    b"uname",
    b"gname",
    b"mtime",
    b"atime",
    b"ctime",
];

/// The format a header asks for: Go's `Header.Format`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    /// Whichever fits, USTAR first.
    #[default]
    Unspecified,
    /// PAX, or USTAR when nothing needs PAX (Go's "PAX implies USTAR allowed too").
    Pax,
}

/// The fields of Go's `tar.Header` that shards sets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Header {
    pub name: Vec<u8>,
    pub typeflag: u8,
    pub linkname: Vec<u8>,
    pub mode: i64,
    pub uid: i64,
    pub gid: i64,
    pub size: i64,
    /// Unix seconds.
    pub mtime: i64,
    pub devmajor: i64,
    pub devminor: i64,
    /// `PAXRecords`.
    pub pax: BTreeMap<Vec<u8>, Vec<u8>>,
    pub format: Format,
}

// Go's Format bits.
const USTAR: u8 = 1;
const PAX: u8 = 2;
const GNU: u8 = 4;

/// Writes an archive: a header, then exactly its size in data, and so on, then
/// [`Writer::finish`].
#[derive(Debug)]
pub struct Writer<W> {
    out: W,
    /// Data still owed to the current member.
    remaining: u64,
    /// Zeros owed after the current member's data.
    pad: usize,
}

impl<W: Write> Writer<W> {
    /// Where the archive goes.
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.out
    }

    pub fn new(out: W) -> Writer<W> {
        Writer {
            out,
            remaining: 0,
            pad: 0,
        }
    }

    /// Writes `hdr`, after padding out the member before it, as Go's WriteHeader.
    pub fn header(&mut self, hdr: &Header) -> Result<(), Error> {
        self.flush()?;
        let (formats, pax, why) = allowed(hdr)?;
        if formats & USTAR != 0 {
            self.ustar(hdr)
        } else if formats & PAX != 0 {
            self.pax(hdr, &pax)
        } else if formats & GNU != 0 {
            bad(format!(
                "{}: only the GNU format can hold it, which shards does not write",
                show(&hdr.name)
            ))
        } else {
            bad(why)
        }
    }

    /// Writes some of the current member's data.
    pub fn write(&mut self, data: &[u8]) -> Result<(), Error> {
        if data.len() as u64 > self.remaining {
            return bad("archive/tar: write too long");
        }
        self.out.write_all(data)?;
        self.remaining -= data.len() as u64;
        Ok(())
    }

    /// Pads out the last member and writes the two zero blocks that end an archive.
    pub fn finish(mut self) -> Result<W, Error> {
        self.flush()?;
        self.out.write_all(&[0; 2 * BLOCK])?;
        Ok(self.out)
    }

    fn flush(&mut self) -> Result<(), Error> {
        if self.remaining > 0 {
            return bad(format!("archive/tar: missed writing {} bytes", self.remaining));
        }
        self.out
            .write_all([0; BLOCK].get(..self.pad).unwrap_or_default())?;
        self.pad = 0;
        Ok(())
    }

    fn raw(&mut self, blk: &[u8; BLOCK], size: i64, flag: u8) -> Result<(), Error> {
        self.flush()?;
        self.out.write_all(blk)?;
        let size = if header_only(flag) { 0 } else { size };
        let size = u64::try_from(size).map_err(|_| crate::Error("negative size".into()))?;
        self.remaining = size;
        self.pad = (size.wrapping_neg() & (BLOCK as u64 - 1)) as usize;
        Ok(())
    }

    fn ustar(&mut self, hdr: &Header) -> Result<(), Error> {
        let (prefix, name) = split_ustar(&hdr.name).unwrap_or((&[], &hdr.name));
        let mut blk = template(hdr, name, &hdr.linkname);
        put_string(&mut blk[345..500], prefix);
        set_format(&mut blk);
        self.raw(&blk, hdr.size, hdr.typeflag)
    }

    fn pax(&mut self, hdr: &Header, records: &BTreeMap<Vec<u8>, Vec<u8>>) -> Result<(), Error> {
        if !records.is_empty() {
            let mut data = Vec::new();
            for (k, v) in records {
                data.extend_from_slice(&pax_record(k, v));
            }
            if data.len() > MAX_SPECIAL {
                return bad("archive/tar: header field too long");
            }
            let (dir, file) = split(&hdr.name);
            let name = join(&[dir, b"PaxHeaders.0", file]);
            self.raw_file(&name, &data)?;
        }
        let blk = template(hdr, &to_ascii(&hdr.name), &to_ascii(&hdr.linkname));
        let mut blk = blk;
        set_format(&mut blk);
        self.raw(&blk, hdr.size, hdr.typeflag)
    }

    /// A PAX header member: Go's writeRawFile.
    fn raw_file(&mut self, name: &[u8], data: &[u8]) -> Result<(), Error> {
        let mut name = to_ascii(name);
        name.truncate(NAME_SIZE);
        while name.last() == Some(&b'/') {
            name.pop();
        }
        let mut blk = [0u8; BLOCK];
        blk[156] = XHEADER;
        put_string(&mut blk[0..100], &name);
        put_octal(&mut blk[100..108], 0);
        put_octal(&mut blk[108..116], 0);
        put_octal(&mut blk[116..124], 0);
        put_octal(&mut blk[124..136], data.len() as i64);
        put_octal(&mut blk[136..148], 0);
        set_format(&mut blk);
        self.raw(&blk, data.len() as i64, XHEADER)?;
        self.write(data)
    }
}

fn show(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn header_only(flag: u8) -> bool {
    matches!(flag, LINK | SYMLINK | CHAR | BLOCK_DEVICE | DIR | FIFO)
}

fn is_ascii(s: &[u8]) -> bool {
    s.iter().all(|&c| c < 0x80 && c != 0)
}

/// Go's toASCII: what is not ASCII, or is NUL, is dropped. A rune of several bytes is all
/// at or past 0x80, so dropping bytes drops the rune.
fn to_ascii(s: &[u8]) -> Vec<u8> {
    s.iter().copied().filter(|&c| c < 0x80 && c != 0).collect()
}

fn fits_octal(n: usize, x: i64) -> bool {
    let bits = (n as u32 - 1) * 3;
    x >= 0 && (n >= 22 || x < 1i64 << bits)
}

fn fits_base256(n: usize, x: i64) -> bool {
    let bits = (n as u32 - 1) * 8;
    n >= 9 || (x >= -(1i64 << bits) && x < 1i64 << bits)
}

/// Go's formatString: a short value is NUL-terminated, a long one cut, and a cut one's
/// trailing slash replaced by NUL.
fn put_string(b: &mut [u8], s: &[u8]) {
    for (dst, &src) in b.iter_mut().zip(s) {
        *dst = src;
    }
    if let Some(end) = b.get_mut(s.len()) {
        *end = 0;
    }
    if s.len() > b.len() && b.last() == Some(&b'/') {
        let cut = s.get(..b.len() - 1).unwrap_or_default();
        let keep = cut.iter().rposition(|&c| c != b'/').map_or(0, |i| i + 1);
        if let Some(end) = b.get_mut(keep) {
            *end = 0;
        }
    }
}

/// Go's formatOctal: zero-padded, leaving room for a NUL; 0 when it does not fit. The
/// digits go straight into the field, right to left: what Go's FormatInt, padding and
/// formatString make of a value that fits, with no string between.
fn put_octal(b: &mut [u8], x: i64) {
    let mut x = if fits_octal(b.len(), x) {
        x.unsigned_abs()
    } else {
        0
    };
    let Some((nul, digits)) = b.split_last_mut() else {
        return;
    };
    *nul = 0;
    for d in digits.iter_mut().rev() {
        *d = b'0' + (x & 7) as u8;
        x >>= 3;
    }
}

/// The V7 and USTAR fields of `hdr`, with its name and link as given.
fn template(hdr: &Header, name: &[u8], link: &[u8]) -> [u8; BLOCK] {
    let mut blk = [0u8; BLOCK];
    let mtime = if hdr.mtime == ZERO_TIME { 0 } else { hdr.mtime };
    blk[156] = hdr.typeflag;
    put_string(&mut blk[0..100], name);
    put_string(&mut blk[157..257], link);
    put_octal(&mut blk[100..108], hdr.mode);
    put_octal(&mut blk[108..116], hdr.uid);
    put_octal(&mut blk[116..124], hdr.gid);
    put_octal(&mut blk[124..136], hdr.size);
    put_octal(&mut blk[136..148], mtime);
    // uname and gname stay empty.
    put_string(&mut blk[265..297], b"");
    put_string(&mut blk[297..329], b"");
    put_octal(&mut blk[329..337], hdr.devmajor);
    put_octal(&mut blk[337..345], hdr.devminor);
    blk
}

/// USTAR's magic and version, which PAX shares, and the checksum.
fn set_format(blk: &mut [u8; BLOCK]) {
    blk[257..263].copy_from_slice(b"ustar\0");
    blk[263..265].copy_from_slice(b"00");
    let sum: i64 = blk
        .iter()
        .enumerate()
        .map(|(i, &c)| if (148..156).contains(&i) { 32 } else { i64::from(c) })
        .sum();
    put_octal(&mut blk[148..155], sum);
    blk[155] = b' ';
}

/// Go's splitUSTARPath: a long ASCII name split at a slash into a prefix of at most 155
/// bytes and a name of at most 100.
fn split_ustar(name: &[u8]) -> Option<(&[u8], &[u8])> {
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

/// Go's path.Split.
fn split(p: &[u8]) -> (&[u8], &[u8]) {
    let at = p.iter().rposition(|&c| c == b'/').map_or(0, |i| i + 1);
    p.split_at_checked(at).unwrap_or((&[], p))
}

/// Go's path.Join: the non-empty elements joined by slashes, then cleaned.
fn join(parts: &[&[u8]]) -> Vec<u8> {
    let joined: Vec<&[u8]> = parts.iter().copied().filter(|p| !p.is_empty()).collect();
    if joined.is_empty() {
        return Vec::new();
    }
    clean(&joined.join(&b'/'))
}

/// Go's path.Clean.
fn clean(p: &[u8]) -> Vec<u8> {
    if p.is_empty() {
        return b".".to_vec();
    }
    let rooted = p.first() == Some(&b'/');
    let mut out: Vec<&[u8]> = Vec::new();
    for part in p.split(|&c| c == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                if out.last().is_some_and(|l| *l != b"..") {
                    out.pop();
                } else if !rooted {
                    out.push(part);
                }
            }
            _ => out.push(part),
        }
    }
    let body = out.join(&b'/');
    match (rooted, body.is_empty()) {
        (true, _) => [b"/".as_slice(), &body].concat(),
        (false, true) => b".".to_vec(),
        (false, false) => body,
    }
}

/// Go's formatPAXRecord: `size key=value\n`, the size counting itself.
fn pax_record(k: &[u8], v: &[u8]) -> Vec<u8> {
    let mut size = k.len() + v.len() + 3;
    size += size.to_string().len();
    let mut record = [size.to_string().as_bytes(), b" ", k, b"=", v, b"\n"].concat();
    if record.len() != size {
        size = record.len();
        record = [size.to_string().as_bytes(), b" ", k, b"=", v, b"\n"].concat();
    }
    record
}

/// Go's validPAXRecord.
fn valid_record(k: &[u8], v: &[u8]) -> bool {
    if k.is_empty() || k.contains(&b'=') {
        return false;
    }
    match k {
        b"path" | b"linkpath" | b"uname" | b"gname" => !v.contains(&0),
        _ => !k.contains(&0),
    }
}

type Allowed = (u8, BTreeMap<Vec<u8>, Vec<u8>>, String);

/// Go's Header.allowedFormats, for the fields shards sets: the formats that can hold
/// `hdr`, the PAX records it needs, and why none can.
fn allowed(hdr: &Header) -> Result<Allowed, Error> {
    let mut format = USTAR | PAX | GNU;
    let mut pax: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let (mut no_ustar, mut no_pax, mut no_gnu) = (String::new(), String::new(), String::new());
    let mut prefer_pax = false;

    let mut string = |s: &[u8], size: usize, name: &str, key: &[u8], format: &mut u8| {
        let too_long = s.len() > size;
        let long_gnu = key == b"path" || key == b"linkpath";
        if s.contains(&0) || (too_long && !long_gnu) {
            no_gnu = format!("GNU cannot encode {name}={:?}", show(s));
            *format &= !GNU;
        }
        if !is_ascii(s) || too_long {
            if key != b"path" || split_ustar(s).is_none() {
                no_ustar = format!("USTAR cannot encode {name}={:?}", show(s));
                *format &= !USTAR;
            }
            pax.insert(key.to_vec(), s.to_vec());
        }
        if hdr.pax.get(key).is_some_and(|v| v == s) {
            pax.insert(key.to_vec(), s.to_vec());
        }
    };
    string(&hdr.name, NAME_SIZE, "Name", b"path", &mut format);
    string(&hdr.linkname, NAME_SIZE, "Linkname", b"linkpath", &mut format);

    let mut numeric = |n: i64, size: usize, name: &str, key: &[u8], format: &mut u8| {
        if !fits_base256(size, n) {
            no_gnu = format!("GNU cannot encode {name}={n}");
            *format &= !GNU;
        }
        if !fits_octal(size, n) {
            no_ustar = format!("USTAR cannot encode {name}={n}");
            *format &= !USTAR;
            if key.is_empty() {
                no_pax = format!("PAX cannot encode {name}={n}");
                *format &= !PAX;
            } else {
                pax.insert(key.to_vec(), n.to_string().into_bytes());
            }
        }
        if hdr.pax.get(key).is_some_and(|v| *v == n.to_string().into_bytes()) {
            pax.insert(key.to_vec(), n.to_string().into_bytes());
        }
    };
    numeric(hdr.mode, 8, "Mode", b"", &mut format);
    numeric(hdr.uid, 8, "Uid", b"uid", &mut format);
    numeric(hdr.gid, 8, "Gid", b"gid", &mut format);
    numeric(hdr.size, 12, "Size", b"size", &mut format);
    numeric(hdr.devmajor, 8, "Devmajor", b"", &mut format);
    numeric(hdr.devminor, 8, "Devminor", b"", &mut format);

    // The modification time, in whole seconds (Go's verifyTime with no nanoseconds).
    if hdr.mtime != ZERO_TIME {
        let t = hdr.mtime;
        if !fits_base256(12, t) {
            no_gnu = format!("GNU cannot encode ModTime={t}");
            format &= !GNU;
        }
        if !fits_octal(12, t) {
            no_ustar = format!("USTAR cannot encode ModTime={t}");
            format &= !USTAR;
            prefer_pax = true;
            pax.insert(b"mtime".to_vec(), t.to_string().into_bytes());
        }
        if hdr
            .pax
            .get(b"mtime".as_slice())
            .is_some_and(|v| *v == t.to_string().into_bytes())
        {
            pax.insert(b"mtime".to_vec(), t.to_string().into_bytes());
        }
    }

    let mut only_pax = String::new();
    if matches!(hdr.typeflag, REG | CHAR | BLOCK_DEVICE | FIFO) && hdr.name.ends_with(b"/") {
        return bad("archive/tar: cannot encode header: filename may not have trailing slash");
    }
    if !header_only(hdr.typeflag) && hdr.size < 0 {
        return bad("archive/tar: cannot encode header: negative size on header-only type");
    }
    if !hdr.pax.is_empty() {
        for (k, v) in &hdr.pax {
            if pax.contains_key(k) {
                continue;
            }
            if !BASIC_KEYS.contains(&k.as_slice()) && !k.starts_with(b"GNU.sparse.") {
                pax.insert(k.clone(), v.clone());
            }
        }
        only_pax = "only PAX supports PAXRecords".into();
        format &= PAX;
    }
    for (k, v) in &pax {
        if !valid_record(k, v) {
            return bad(format!(
                "archive/tar: cannot encode header: invalid PAX record: {:?}",
                format!("{} = {}", show(k), show(v))
            ));
        }
    }
    if hdr.format == Format::Pax {
        let mut want = PAX;
        if !prefer_pax {
            want |= USTAR;
        }
        format &= want;
    }
    let why = if format == 0 {
        let parts: Vec<&str> = match hdr.format {
            Format::Pax => vec!["Format specifies PAX", no_pax.as_str()],
            Format::Unspecified => vec![
                no_ustar.as_str(),
                no_pax.as_str(),
                no_gnu.as_str(),
                only_pax.as_str(),
            ],
        }
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();
        format!("archive/tar: cannot encode header: {}", parts.join("; and "))
    } else {
        String::new()
    };
    Ok((format, pax, why))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn paths_clean_and_join_as_go_does() {
        assert_eq!(join(&[b"a/b/", b"PaxHeaders.0", b""]), b"a/b/PaxHeaders.0");
        assert_eq!(join(&[b"", b"PaxHeaders.0", b"f"]), b"PaxHeaders.0/f");
        assert_eq!(clean(b"/../a//./b/.."), b"/a");
        assert_eq!(clean(b"../a/../.."), b"../..");
        assert_eq!(clean(b""), b".");
    }

    #[test]
    fn pax_records_count_their_own_length() {
        assert_eq!(pax_record(b"path", b"x"), b"9 path=x\n");
        // 3 + 4 + 89 = 96, plus 2 digits is 98: no carry.
        let v = vec![b'a'; 89];
        assert_eq!(pax_record(b"path", &v).len(), 98);
        // 3 + 4 + 91 = 98, plus 2 is 100: three digits, so 101.
        let v = vec![b'a'; 91];
        let r = pax_record(b"path", &v);
        assert_eq!(r.len(), 101);
        assert!(r.starts_with(b"101 "));
    }
}
