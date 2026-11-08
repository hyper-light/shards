//! Go's tar.Reader (go1.26.1 src/archive/tar/reader.go and format.go): V7, USTAR, PAX,
//! GNU and STAR headers, GNU long names and links, PAX records, and GNU sparse files in
//! their old, 0.0, 0.1 and 1.0 forms, read as Go reads them: holes come back as zeros.
//!
//! Archives are untrusted. Numbers are parsed with overflow checks, metadata is bounded at
//! 1 MiB as Go bounds it, and sparse maps are validated before use.

use std::collections::BTreeMap;
use std::io::{self, Read};

use super::strconv::{parse_int, parse_numeric, parse_octal, parse_pax_record, parse_pax_time, parse_string};
use super::writer::checksum;
use super::{
    BLOCK, ERR_FIELD_TOO_LONG, Format, Header, MAX_SPECIAL, TYPE_DIR, TYPE_GNU_LONGLINK, TYPE_GNU_LONGNAME,
    TYPE_GNU_SPARSE, TYPE_REG, TYPE_REGA, TYPE_XGLOBAL_HEADER, TYPE_XHEADER, Time, block_padding, err_header,
    header_only,
};
use crate::error::{Error, Kind};

const ERR_MISS_DATA: &str = "archive/tar: sparse file references non-existent data";
const ERR_UNREF_DATA: &str = "archive/tar: sparse file contains unreferenced data";
const ERR_SPARSE_TOO_LONG: &str = "archive/tar: sparse map too long";

/// A run of a sparse file: Go's sparseEntry.
#[derive(Debug, Clone, Copy, Default)]
struct Run {
    offset: i64,
    length: i64,
}

impl Run {
    fn end(self) -> i64 {
        self.offset.saturating_add(self.length)
    }
}

/// sparseFileReader's state over the entry's physical data: the holes, from `next` on,
/// and the logical position.
#[derive(Debug, Default)]
struct Sparse {
    holes: Vec<Run>,
    next: usize,
    pos: i64,
}

/// Reads an archive: [`Reader::next_header`] gives each entry's header, and the reader itself
/// reads that entry's data.
#[derive(Debug)]
pub struct Reader<R> {
    r: R,
    pad: u64,
    /// Physical data left in the current entry: regFileReader's nb.
    nb: u64,
    /// Set when the entry is a sparse file.
    sparse: Option<Sparse>,
    /// The error that stopped the archive: Go's sticky tr.err.
    err: Option<Error>,
    /// Whether the archive ended cleanly.
    eof: bool,
}

/// An error that ends reading.
fn eof_error(e: io::Error) -> Error {
    Error::io(&e)
}

fn unexpected_eof() -> Error {
    Error::new(Kind::Header, "unexpected EOF")
}

impl<R: Read> Reader<R> {
    pub fn new(r: R) -> Reader<R> {
        Reader {
            r,
            pad: 0,
            nb: 0,
            sparse: None,
            err: None,
            eof: false,
        }
    }

    /// What it reads from, as `RawAccounting` needs it: what `next_header` reads, and only
    /// that, is a header's raw bytes and the padding before it.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.r
    }

    /// The next entry's header, or None at the end of the archive (Go's io.EOF).
    pub fn next_header(&mut self) -> Result<Option<Header>, Error> {
        if let Some(e) = &self.err {
            return Err(e.clone());
        }
        if self.eof {
            return Ok(None);
        }
        match self.read_next() {
            Ok(Some(h)) => Ok(Some(h)),
            Ok(None) => {
                self.eof = true;
                Ok(None)
            }
            Err(e) => {
                self.err = Some(e.clone());
                Err(e)
            }
        }
    }

    fn read_next(&mut self) -> Result<Option<Header>, Error> {
        let mut pax: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let (mut long_name, mut long_link) = (Vec::new(), Vec::new());
        let mut format = Format(Format::USTAR.0 | Format::PAX.0 | Format::GNU.0);
        loop {
            // Whatever is left of the last entry, then its padding.
            let left = self.nb;
            self.discard(left)?;
            let pad = usize::try_from(self.pad).unwrap_or(0);
            let mut skip = [0u8; BLOCK];
            let (n, end) = try_read_full(&mut self.r, skip.get_mut(..pad).unwrap_or_default())?;
            if end && n < pad {
                return Ok(None);
            }
            self.pad = 0;

            let Some((mut hdr, raw)) = self.read_header()? else {
                return Ok(None);
            };
            self.handle_regular_file(&hdr)?;
            format.may_only_be(hdr.format);

            match hdr.typeflag {
                TYPE_XHEADER | TYPE_XGLOBAL_HEADER => {
                    format.may_only_be(Format::PAX);
                    pax = self.parse_pax()?;
                    if hdr.typeflag == TYPE_XGLOBAL_HEADER {
                        merge_pax(&mut hdr, &pax)?;
                        return Ok(Some(Header {
                            name: hdr.name,
                            typeflag: hdr.typeflag,
                            pax: hdr.pax,
                            format,
                            ..Header::default()
                        }));
                    }
                }
                TYPE_GNU_LONGNAME | TYPE_GNU_LONGLINK => {
                    format.may_only_be(Format::GNU);
                    let real = self.read_special()?;
                    let value = parse_string(&real).to_vec();
                    if hdr.typeflag == TYPE_GNU_LONGNAME {
                        long_name = value;
                    } else {
                        long_link = value;
                    }
                }
                _ => {
                    merge_pax(&mut hdr, &pax)?;
                    if !long_name.is_empty() {
                        hdr.name = long_name;
                    }
                    if !long_link.is_empty() {
                        hdr.linkname = long_link;
                    }
                    if hdr.typeflag == TYPE_REGA {
                        hdr.typeflag = if hdr.name.ends_with(b"/") {
                            TYPE_DIR
                        } else {
                            TYPE_REG
                        };
                    }
                    self.handle_regular_file(&hdr)?;
                    self.handle_sparse_file(&mut hdr, &raw)?;
                    if format.has(Format::USTAR) && format.has(Format::PAX) {
                        format.may_only_be(Format::USTAR);
                    }
                    hdr.format = format;
                    return Ok(Some(hdr));
                }
            }
        }
    }

    /// discard: skips `n` bytes; an archive that ends first is unexpected.
    fn discard(&mut self, n: u64) -> Result<(), Error> {
        let skipped = io::copy(&mut (&mut self.r).take(n), &mut io::sink()).map_err(eof_error)?;
        if skipped < n {
            return Err(unexpected_eof());
        }
        self.nb = 0;
        Ok(())
    }

    /// handleRegularFile: the entry's data is its size, none for header-only types.
    fn handle_regular_file(&mut self, hdr: &Header) -> Result<(), Error> {
        let nb = if header_only(hdr.typeflag) { 0 } else { hdr.size };
        let nb = u64::try_from(nb).map_err(|_| err_header())?;
        self.pad = block_padding(nb);
        self.nb = nb;
        self.sparse = None;
        Ok(())
    }

    /// readSpecialFile: a metadata entry's data, at most 1 MiB.
    fn read_special(&mut self) -> Result<Vec<u8>, Error> {
        let mut buf = Vec::new();
        let limit = MAX_SPECIAL as u64 + 1;
        (&mut *self)
            .take(limit)
            .read_to_end(&mut buf)
            .map_err(|e| inner_error(&e))?;
        if buf.len() > MAX_SPECIAL {
            return Err(Error::other(ERR_FIELD_TOO_LONG));
        }
        Ok(buf)
    }

    /// parsePAX: the records, with GNU sparse 0.0's repeated offset and numbytes records
    /// gathered into one GNU.sparse.map.
    fn parse_pax(&mut self) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, Error> {
        let buf = self.read_special()?;
        let mut rest = buf.as_slice();
        let mut sparse: Vec<Vec<u8>> = Vec::new();
        let mut records = BTreeMap::new();
        while !rest.is_empty() {
            let (k, v, r) = parse_pax_record(rest).ok_or_else(err_header)?;
            rest = r;
            match k {
                b"GNU.sparse.offset" | b"GNU.sparse.numbytes" => {
                    if (sparse.len().is_multiple_of(2) && k != b"GNU.sparse.offset")
                        || (sparse.len() % 2 == 1 && k != b"GNU.sparse.numbytes")
                        || v.contains(&b',')
                    {
                        return Err(err_header());
                    }
                    sparse.push(v.to_vec());
                }
                _ => {
                    records.insert(k.to_vec(), v.to_vec());
                }
            }
        }
        if !sparse.is_empty() {
            records.insert(b"GNU.sparse.map".to_vec(), sparse.join(&b','));
        }
        Ok(records)
    }

    /// readHeader: the next header block, or None at the end: no more data, or two zero
    /// blocks, or one followed by nothing.
    fn read_header(&mut self) -> Result<Option<(Header, [u8; BLOCK])>, Error> {
        let mut blk = [0u8; BLOCK];
        if !read_full(&mut self.r, &mut blk)? {
            return Ok(None);
        }
        if blk == [0u8; BLOCK] {
            if !read_full(&mut self.r, &mut blk)? {
                return Ok(None);
            }
            if blk == [0u8; BLOCK] {
                return Ok(None);
            }
            return Err(err_header());
        }
        let format = get_format(&blk);
        if format == Format::UNKNOWN {
            return Err(err_header());
        }
        let mut ok = true;
        let mut num = |b: &[u8]| {
            parse_numeric(b).unwrap_or_else(|| {
                ok = false;
                0
            })
        };
        let mut hdr = Header {
            typeflag: blk[156],
            name: parse_string(&blk[0..100]).to_vec(),
            linkname: parse_string(&blk[157..257]).to_vec(),
            size: num(&blk[124..136]),
            mode: num(&blk[100..108]),
            uid: num(&blk[108..116]),
            gid: num(&blk[116..124]),
            mtime: Time::unix(num(&blk[136..148]), 0),
            ..Header::default()
        };
        if format.0 > Format::V7.0 {
            hdr.uname = parse_string(&blk[265..297]).to_vec();
            hdr.gname = parse_string(&blk[297..329]).to_vec();
            hdr.devmajor = num(&blk[329..337]);
            hdr.devminor = num(&blk[337..345]);
            let mut prefix: Vec<u8> = Vec::new();
            if format.has(Format(Format::USTAR.0 | Format::PAX.0)) {
                hdr.format = format;
                prefix = parse_string(&blk[345..500]).to_vec();
                if blk.iter().any(|&c| c >= 0x80) {
                    hdr.format = Format::UNKNOWN;
                }
                let nul = |r: std::ops::Range<usize>| blk.get(r.end - 1) == Some(&0);
                if !(nul(124..136)
                    && nul(100..108)
                    && nul(108..116)
                    && nul(116..124)
                    && nul(136..148)
                    && nul(329..337)
                    && nul(337..345))
                {
                    hdr.format = Format::UNKNOWN;
                }
            } else if format.has(Format::STAR) {
                prefix = parse_string(&blk[345..476]).to_vec();
                hdr.atime = Time::unix(num(&blk[476..488]), 0);
                hdr.ctime = Time::unix(num(&blk[488..500]), 0);
            } else if format.has(Format::GNU) {
                hdr.format = format;
                let mut times_ok = true;
                let mut time = |b: &[u8]| -> Time {
                    if b.first() == Some(&0) {
                        return Time::ZERO;
                    }
                    match parse_numeric(b) {
                        Some(n) => Time::unix(n, 0),
                        None => {
                            times_ok = false;
                            Time::unix(0, 0)
                        }
                    }
                };
                hdr.atime = time(&blk[345..357]);
                hdr.ctime = time(&blk[357..369]);
                if !times_ok {
                    // Some old GNU writers put a USTAR prefix where GNU keeps times.
                    hdr.atime = Time::ZERO;
                    hdr.ctime = Time::ZERO;
                    let p = parse_string(&blk[345..500]);
                    if super::strconv::is_ascii(p) {
                        prefix = p.to_vec();
                    }
                    hdr.format = Format::UNKNOWN;
                }
            }
            if !prefix.is_empty() {
                hdr.name = [prefix.as_slice(), b"/", &hdr.name].concat();
            }
        }
        if !ok {
            return Err(err_header());
        }
        Ok(Some((hdr, blk)))
    }

    /// handleSparseFile: an old GNU sparse header or GNU's PAX records make the entry's
    /// data a sparse file's.
    fn handle_sparse_file(&mut self, hdr: &mut Header, raw: &[u8; BLOCK]) -> Result<(), Error> {
        let runs = if hdr.typeflag == TYPE_GNU_SPARSE {
            Some(self.read_old_gnu_sparse_map(hdr, raw)?)
        } else {
            self.read_gnu_sparse_pax(hdr)?
        };
        if let Some(runs) = runs {
            if header_only(hdr.typeflag) || !valid_runs(&runs, hdr.size) {
                return Err(err_header());
            }
            self.sparse = Some(Sparse {
                holes: invert(&runs, hdr.size),
                next: 0,
                pos: 0,
            });
        }
        Ok(())
    }

    /// readGNUSparsePAXHeaders: GNU sparse 0.0 and 0.1 keep the map in records; 1.0 keeps
    /// it at the start of the data.
    fn read_gnu_sparse_pax(&mut self, hdr: &mut Header) -> Result<Option<Vec<Run>>, Error> {
        let major = hdr.pax.get(b"GNU.sparse.major".as_slice()).map(Vec::as_slice);
        let minor = hdr.pax.get(b"GNU.sparse.minor".as_slice()).map(Vec::as_slice);
        let map = hdr.pax.get(b"GNU.sparse.map".as_slice()).map(Vec::as_slice);
        let is1x0 = match (major.unwrap_or_default(), minor.unwrap_or_default()) {
            (b"0", b"0" | b"1") => false,
            (b"1", b"0") => true,
            (a, b) if !a.is_empty() || !b.is_empty() => return Ok(None),
            _ if map.is_some_and(|m| !m.is_empty()) => false,
            _ => return Ok(None),
        };
        hdr.format.may_only_be(Format::PAX);
        if let Some(name) = hdr
            .pax
            .get(b"GNU.sparse.name".as_slice())
            .filter(|n| !n.is_empty())
        {
            hdr.name = name.clone();
        }
        let size = hdr
            .pax
            .get(b"GNU.sparse.size".as_slice())
            .filter(|s| !s.is_empty())
            .or_else(|| hdr.pax.get(b"GNU.sparse.realsize".as_slice()));
        if let Some(size) = size.filter(|s| !s.is_empty()) {
            hdr.size = parse_int(size).ok_or_else(err_header)?;
        }
        if is1x0 {
            return self.read_gnu_sparse_map_1x0().map(Some);
        }
        read_gnu_sparse_map_0x1(&hdr.pax).map(Some)
    }

    /// readOldGNUSparseMap: the runs in the header, then in extension blocks.
    fn read_old_gnu_sparse_map(&mut self, hdr: &mut Header, raw: &[u8; BLOCK]) -> Result<Vec<Run>, Error> {
        if get_format(raw) != Format::GNU {
            return Err(err_header());
        }
        hdr.format.may_only_be(Format::GNU);
        hdr.size = parse_numeric(&raw[483..495]).ok_or_else(err_header)?;
        let mut runs = Vec::new();
        // The header's 4 entries (offset 386), its extension flag at 482; an extension
        // block has 21 and its flag at 504.
        let mut blk = *raw;
        let (mut start, mut count, mut flag) = (386, 4, 482);
        // Go reads extension blocks without end, so an archive could grow the map with
        // its length; they are held to the 1 MiB Go allows any other metadata
        // (readGNUSparseMap1x0's errSparseTooLong).
        let mut extended = 0usize;
        loop {
            for i in 0..count {
                let at = start + i * 24;
                let entry = blk.get(at..at + 24).unwrap_or_default();
                if entry.first().is_none_or(|&c| c == 0) {
                    break;
                }
                let offset = parse_numeric(entry.get(..12).unwrap_or_default()).ok_or_else(err_header)?;
                let length = parse_numeric(entry.get(12..).unwrap_or_default()).ok_or_else(err_header)?;
                runs.push(Run { offset, length });
            }
            if blk.get(flag).is_some_and(|&c| c > 0) {
                extended += BLOCK;
                if extended > MAX_SPECIAL {
                    return Err(Error::other(ERR_SPARSE_TOO_LONG));
                }
                if !read_full(&mut self.r, &mut blk)? {
                    return Err(unexpected_eof());
                }
                (start, count, flag) = (0, 21, 504);
                continue;
            }
            return Ok(runs);
        }
    }

    /// readGNUSparseMap1x0: newline-separated decimals at the start of the data, in whole
    /// blocks: the count, then offset and length pairs.
    fn read_gnu_sparse_map_1x0(&mut self) -> Result<Vec<Run>, Error> {
        let mut buf: Vec<u8> = Vec::new();
        let mut read_pos = 0usize;
        let mut newlines: i64 = 0;
        let mut total = 0usize;
        let mut feed =
            |me: &mut Self, buf: &mut Vec<u8>, newlines: &mut i64, want: i64| -> Result<(), Error> {
                while *newlines < want {
                    total += BLOCK;
                    if total > MAX_SPECIAL {
                        return Err(Error::other(ERR_SPARSE_TOO_LONG));
                    }
                    let mut blk = [0u8; BLOCK];
                    let mut got = 0;
                    while got < BLOCK {
                        let n = me
                            .read(blk.get_mut(got..).unwrap_or_default())
                            .map_err(|e| inner_error(&e))?;
                        if n == 0 {
                            return Err(unexpected_eof());
                        }
                        got += n;
                    }
                    buf.extend_from_slice(&blk);
                    *newlines += blk.iter().filter(|&&c| c == b'\n').count() as i64;
                }
                Ok(())
            };
        fn token(buf: &[u8], newlines: &mut i64, read_pos: &mut usize) -> Vec<u8> {
            *newlines -= 1;
            let rest = buf.get(*read_pos..).unwrap_or_default();
            let end = rest
                .iter()
                .position(|&c| c == b'\n')
                .map_or(rest.len(), |i| i + 1);
            *read_pos += end;
            let tok = rest.get(..end).unwrap_or_default();
            tok.strip_suffix(b"\n").unwrap_or(tok).to_vec()
        }
        feed(self, &mut buf, &mut newlines, 1)?;
        let entries = parse_int(&token(&buf, &mut newlines, &mut read_pos)).ok_or_else(err_header)?;
        let want = entries
            .checked_mul(2)
            .filter(|_| entries >= 0)
            .ok_or_else(err_header)?;
        feed(self, &mut buf, &mut newlines, want)?;
        let mut runs = Vec::new();
        for _ in 0..entries {
            let offset = parse_int(&token(&buf, &mut newlines, &mut read_pos));
            let length = parse_int(&token(&buf, &mut newlines, &mut read_pos));
            match (offset, length) {
                (Some(offset), Some(length)) => runs.push(Run { offset, length }),
                _ => return Err(err_header()),
            }
        }
        Ok(runs)
    }

    /// regFileReader.Read on the physical data: 0 once it is all read; a stream that
    /// ends first is unexpected.
    fn read_physical(&mut self, b: &mut [u8]) -> io::Result<usize> {
        let want = usize::try_from(self.nb).map_or(b.len(), |nb| nb.min(b.len()));
        if want == 0 {
            return Ok(0);
        }
        let n = self.r.read(b.get_mut(..want).unwrap_or_default())?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, unexpected_eof()));
        }
        self.nb -= n as u64;
        Ok(n)
    }

    /// sparseFileReader.Read: data runs from the physical data, holes as zeros.
    fn read_sparse(&mut self, sp: &mut Sparse, b: &mut [u8]) -> io::Result<usize> {
        let last_end = sp.holes.last().map_or(0, |h| h.end());
        let logical = |pos: i64| u64::try_from(last_end.saturating_sub(pos)).unwrap_or(0);
        let len = usize::try_from(logical(sp.pos)).map_or(b.len(), |l| l.min(b.len()));
        let mut done = 0;
        let mut miss = false;
        while done < len && !miss {
            let Some(hole) = sp.holes.get(sp.next).copied() else {
                break;
            };
            let chunk = b.get_mut(done..len).unwrap_or_default();
            let room = chunk.len();
            let nf = if sp.pos < hole.offset {
                let want = usize::try_from(hole.offset - sp.pos).map_or(room, |w| w.min(room));
                let part = chunk.get_mut(..want).unwrap_or_default();
                let mut got = 0;
                while got < part.len() {
                    let n = self.read_physical(part.get_mut(got..).unwrap_or_default())?;
                    if n == 0 {
                        miss = true;
                        break;
                    }
                    got += n;
                }
                got
            } else {
                let want = usize::try_from(hole.end() - sp.pos).map_or(room, |w| w.min(room));
                chunk.get_mut(..want).unwrap_or_default().fill(0);
                want
            };
            done += nf;
            sp.pos += nf as i64;
            if sp.pos >= hole.end() && sp.next + 1 < sp.holes.len() {
                sp.next += 1;
            }
        }
        if miss {
            return Err(io::Error::other(Error::other(ERR_MISS_DATA)));
        }
        if logical(sp.pos) == 0 && self.nb > 0 {
            return Err(io::Error::other(Error::other(ERR_UNREF_DATA)));
        }
        Ok(done)
    }
}

impl<R: Read> Read for Reader<R> {
    /// The current entry's data.
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        if let Some(e) = &self.err {
            return Err(io::Error::other(e.clone()));
        }
        let r = match self.sparse.take() {
            None => self.read_physical(b),
            Some(mut sp) => {
                let r = self.read_sparse(&mut sp, b);
                self.sparse = Some(sp);
                r
            }
        };
        if let Err(e) = &r
            && e.kind() != io::ErrorKind::Interrupted
        {
            self.err = Some(inner_error(e));
        }
        r
    }
}

/// The archive error inside an I/O error, or the I/O error as Go prints it.
fn inner_error(e: &io::Error) -> Error {
    match e.get_ref().and_then(|i| i.downcast_ref::<Error>()) {
        Some(inner) => inner.clone(),
        None => Error::io(e),
    }
}

/// Reads all of `b`: false for nothing at all (a clean end), an error for part of it.
fn read_full(r: &mut impl Read, b: &mut [u8]) -> Result<bool, Error> {
    let (n, _) = try_read_full(r, b)?;
    if n == 0 && !b.is_empty() {
        return Ok(false);
    }
    if n < b.len() {
        return Err(unexpected_eof());
    }
    Ok(true)
}

/// tryReadFull: as much of `b` as the reader gives, and whether it ended.
fn try_read_full(r: &mut impl Read, b: &mut [u8]) -> Result<(usize, bool), Error> {
    let mut n = 0;
    while n < b.len() {
        match r.read(b.get_mut(n..).unwrap_or_default()) {
            Ok(0) => return Ok((n, true)),
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(eof_error(e)),
        }
    }
    Ok((n, false))
}

/// getFormat: the checksum must match, signed or unsigned; then the magic decides.
fn get_format(blk: &[u8; BLOCK]) -> Format {
    let Some(value) = parse_octal(&blk[148..156]) else {
        return Format::UNKNOWN;
    };
    let (unsigned, signed) = checksum(blk);
    if value != unsigned && value != signed {
        return Format::UNKNOWN;
    }
    let magic = &blk[257..263];
    let version = &blk[263..265];
    let trailer = &blk[508..512];
    if magic == b"ustar\0" && trailer == b"tar\0" {
        Format::STAR
    } else if magic == b"ustar\0" {
        Format(Format::USTAR.0 | Format::PAX.0)
    } else if magic == b"ustar " && version == b" \0" {
        Format::GNU
    } else {
        Format::V7
    }
}

/// mergePAX: records override the header's fields; an empty record keeps the field.
fn merge_pax(hdr: &mut Header, pax: &BTreeMap<Vec<u8>, Vec<u8>>) -> Result<(), Error> {
    for (k, v) in pax {
        if v.is_empty() {
            continue;
        }
        match k.as_slice() {
            b"path" => hdr.name = v.clone(),
            b"linkpath" => hdr.linkname = v.clone(),
            b"uname" => hdr.uname = v.clone(),
            b"gname" => hdr.gname = v.clone(),
            b"uid" => hdr.uid = parse_int(v).ok_or_else(err_header)?,
            b"gid" => hdr.gid = parse_int(v).ok_or_else(err_header)?,
            b"atime" => hdr.atime = parse_pax_time(v)?,
            b"mtime" => hdr.mtime = parse_pax_time(v)?,
            b"ctime" => hdr.ctime = parse_pax_time(v)?,
            b"size" => hdr.size = parse_int(v).ok_or_else(err_header)?,
            _ => {}
        }
    }
    hdr.pax = pax.clone();
    Ok(())
}

/// readGNUSparseMap0x1: GNU.sparse.numblocks pairs in GNU.sparse.map.
fn read_gnu_sparse_map_0x1(pax: &BTreeMap<Vec<u8>, Vec<u8>>) -> Result<Vec<Run>, Error> {
    let count = pax
        .get(b"GNU.sparse.numblocks".as_slice())
        .map(Vec::as_slice)
        .unwrap_or_default();
    let count = parse_int(count).filter(|&n| n >= 0).ok_or_else(err_header)?;
    let want = count.checked_mul(2).ok_or_else(err_header)?;
    let map = pax
        .get(b"GNU.sparse.map".as_slice())
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut fields: Vec<&[u8]> = map.split(|&c| c == b',').collect();
    if fields.len() == 1 && fields.first().is_some_and(|f| f.is_empty()) {
        fields.clear();
    }
    if fields.len() as i64 != want {
        return Err(err_header());
    }
    fields
        .chunks(2)
        .map(|pair| match pair {
            [o, l] => match (parse_int(o), parse_int(l)) {
                (Some(offset), Some(length)) => Ok(Run { offset, length }),
                _ => Err(err_header()),
            },
            _ => Err(err_header()),
        })
        .collect()
}

/// validateSparseEntries: in order, not overlapping, within the size, no overflow.
fn valid_runs(runs: &[Run], size: i64) -> bool {
    if size < 0 {
        return false;
    }
    let mut pre = Run::default();
    for &cur in runs {
        if cur.offset < 0 || cur.length < 0 {
            return false;
        }
        if cur.offset > i64::MAX - cur.length {
            return false;
        }
        if cur.end() > size || pre.end() > cur.offset {
            return false;
        }
        pre = cur;
    }
    true
}

/// invertSparseEntries: the holes between data runs, ending with the (maybe empty) last.
fn invert(runs: &[Run], size: i64) -> Vec<Run> {
    let mut out = Vec::new();
    let mut pre = Run::default();
    for &cur in runs {
        if cur.length == 0 {
            continue;
        }
        pre.length = cur.offset - pre.offset;
        if pre.length > 0 {
            out.push(pre);
        }
        pre.offset = cur.end();
    }
    pre.length = size - pre.offset;
    out.push(pre);
    out
}
