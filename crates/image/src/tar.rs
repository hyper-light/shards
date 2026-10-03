//! Reads tar archives as Go's archive/tar reads them (go1.27.1 src/archive/tar/reader.go),
//! because Docker, containerd and BuildKit read and write OCI layers with it: V7, ustar,
//! star and GNU headers, base-256 numbers, GNU long names and links, and local PAX headers
//! (path, linkpath, uid, gid, mtime, size, and the `SCHILY.xattr.*` records containerd
//! applies). Global PAX headers are parsed and then ignored, as containerd ignores them.
//! Sparse files are refused: layers SHOULD NOT use them (image-spec layer.md), and a
//! file's data must be one run of the archive.
//!
//! Archives are untrusted: numbers are checked, metadata members are bounded at 1 MiB, and
//! paths are cleaned lexically and must stay inside the root. testdata/go-tar holds Go's
//! own test archives and what Go reads from each; the tests hold this reader to it.

use std::collections::BTreeMap;
use std::io::{self, Read};

use crate::{Error, bad};

pub mod writer;

const BLOCK: usize = 512;
/// Go's maxSpecialFileSize: the largest PAX header or GNU long name read.
const MAX_SPECIAL: u64 = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type {
    File,
    HardLink,
    Symlink,
    CharDevice,
    BlockDevice,
    Dir,
    Fifo,
}

/// One archive member, with its metadata resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Cleaned and relative: no empty, `.` or `..` components. Empty for the root.
    pub path: Vec<u8>,
    pub kind: Type,
    /// A symlink's target as written, or a hard link's cleaned target path.
    pub link: Vec<u8>,
    /// Permission, set-id and sticky bits.
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: i64,
    pub mtime_nsec: u32,
    /// Bytes of data: a file's size, and 0 for every other type.
    pub size: u64,
    /// Device numbers for devices, and 0 for every other type.
    pub devmajor: u32,
    pub devminor: u32,
    pub xattrs: BTreeMap<Vec<u8>, Vec<u8>>,
    /// Where the data starts in the archive.
    pub offset: u64,
}

/// A header's fields as Go's readHeader unpacks them, before PAX and GNU records apply.
struct Header {
    flag: u8,
    name: Vec<u8>,
    link: Vec<u8>,
    size: i64,
    mode: i64,
    uid: i64,
    gid: i64,
    mtime: i64,
    devmajor: i64,
    devminor: i64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    V7,
    Ustar,
    Star,
    Gnu,
}

/// Reads entries one after another, skipping file data.
#[derive(Debug)]
pub struct Reader<R> {
    inner: R,
    pos: u64,
    done: bool,
    /// Names and hard links as the archive holds them, uncleaned and unchecked.
    raw: bool,
    /// The last entry's data, skipped when the next is asked for, as Go's Reader skips
    /// it: an entry's header is read without its data being reached.
    owed: Option<u64>,
}

impl<R: Read> Reader<R> {
    pub fn new(inner: R) -> Reader<R> {
        Reader {
            inner,
            pos: 0,
            done: false,
            raw: false,
            owed: None,
        }
    }

    /// A reader that leaves names and hard-link targets as the archive holds them, for
    /// an unpacker that applies its own rules to them (moby's, for ADD).
    pub fn raw(inner: R) -> Reader<R> {
        let mut r = Reader::new(inner);
        r.raw = true;
        r
    }

    /// The next entry, or `None` at the end of the archive.
    pub fn next_entry(&mut self) -> Result<Option<Entry>, Error> {
        if let Some(size) = self.owed.take() {
            self.skip(size)?;
            self.pad(size)?;
        }
        let mut pax = BTreeMap::new();
        let mut long_name = Vec::new();
        let mut long_link = Vec::new();
        loop {
            if self.done {
                return Ok(None);
            }
            let Some(block) = self.block()? else {
                self.done = true;
                return Ok(None);
            };
            if block.iter().all(|&b| b == 0) {
                // Two zero blocks end the archive, and so does the stream's end after one.
                if self.block()?.is_some_and(|b| b.iter().any(|&c| c != 0)) {
                    return bad("a zero block followed by a header");
                }
                self.done = true;
                return Ok(None);
            }
            let h = header(&block)?;
            if !header_only(h.flag) && h.size < 0 {
                return bad(format!("negative size {}", h.size));
            }
            match h.flag {
                b'x' => pax = parse_pax(&self.special(h.size)?)?,
                // In Go a global header is an entry of its own, so it also ends the one
                // being assembled.
                b'g' => {
                    parse_pax(&self.special(h.size)?)?;
                    pax.clear();
                    long_name.clear();
                    long_link.clear();
                }
                b'L' => long_name = c_str(&self.special(h.size)?).to_vec(),
                b'K' => long_link = c_str(&self.special(h.size)?).to_vec(),
                _ => return self.entry(h, &pax, long_name, long_link).map(Some),
            }
        }
    }

    fn entry(
        &mut self,
        h: Header,
        pax: &BTreeMap<Vec<u8>, Vec<u8>>,
        long_name: Vec<u8>,
        long_link: Vec<u8>,
    ) -> Result<Entry, Error> {
        let Header {
            flag,
            mut name,
            mut link,
            mut size,
            mode,
            mut uid,
            mut gid,
            mut mtime,
            devmajor,
            devminor,
        } = h;
        let mut mtime_nsec = 0;
        let mut xattrs = BTreeMap::new();
        // Go's mergePAX: an empty value keeps the header's. containerd applies xattrs from
        // every record, empty or not.
        for (key, value) in pax {
            if let Some(xattr) = key.strip_prefix(b"SCHILY.xattr.") {
                xattrs.insert(xattr.to_vec(), value.clone());
            } else if key.starts_with(b"GNU.sparse.") {
                return bad("sparse file");
            } else if !value.is_empty() {
                match key.as_slice() {
                    b"path" => name.clone_from(value),
                    b"linkpath" => link.clone_from(value),
                    b"uid" => uid = decimal(value)?,
                    b"gid" => gid = decimal(value)?,
                    b"size" => size = decimal(value)?,
                    b"mtime" => (mtime, mtime_nsec) = pax_time(value)?,
                    b"atime" | b"ctime" => {
                        pax_time(value)?;
                    }
                    _ => {}
                }
            }
        }
        if !long_name.is_empty() {
            name = long_name;
        }
        if !long_link.is_empty() {
            link = long_link;
        }
        let kind = match flag {
            // Legacy archives mark directories with a trailing slash.
            0 if name.ends_with(b"/") => Type::Dir,
            0 | b'0' | b'7' => Type::File,
            b'1' => Type::HardLink,
            b'2' => Type::Symlink,
            b'3' => Type::CharDevice,
            b'4' => Type::BlockDevice,
            b'5' => Type::Dir,
            b'6' => Type::Fifo,
            b'S' => return bad("sparse file"),
            other => return bad(format!("unsupported tar entry type {:?}", char::from(other))),
        };
        // Go reads data only for files; other types' sizes are ignored.
        let size = match kind {
            Type::File => u64::try_from(size).map_err(|_| Error(format!("negative size {size}")))?,
            _ => 0,
        };
        let (devmajor, devminor) = match kind {
            Type::CharDevice | Type::BlockDevice => {
                (id(devmajor, "device major")?, id(devminor, "device minor")?)
            }
            _ => (0, 0),
        };
        let path = if self.raw { name } else { clean(&name)? };
        let link = match kind {
            Type::HardLink if !self.raw => clean(&link)?,
            _ => link,
        };
        let entry = Entry {
            path,
            kind,
            link,
            // Go's FileInfo().Mode(): the permission, set-id and sticky bits.
            mode: (mode & 0o7777) as u32,
            uid: id(uid, "uid")?,
            gid: id(gid, "gid")?,
            mtime,
            mtime_nsec,
            size,
            devmajor,
            devminor,
            xattrs,
            offset: self.pos,
        };
        self.owed = Some(size);
        Ok(entry)
    }

    /// The last entry's data, copied to `out` instead of skipped: for a reader of a stream,
    /// which cannot come back for it. Nothing is copied twice, and nothing when the entry
    /// has no data.
    pub fn copy_data(&mut self, out: &mut dyn io::Write) -> Result<u64, Error> {
        let Some(size) = self.owed.take() else {
            return Ok(0);
        };
        let n = io::copy(&mut (&mut self.inner).take(size), out)?;
        self.pos += n;
        if n != size {
            return bad("archive ends inside a member");
        }
        self.pad(size)?;
        Ok(n)
    }

    /// When the newest of what Go's `Reader.Next` returns that `FileInfo().Mode()` calls
    /// regular was modified, as BuildKit takes a source's time from an archive
    /// (dockerfile/1.27.1 epoch.go, archiveMaxTimeFromRef): `None` for an archive of none.
    /// Go returns every member but metadata ones, hard links, sparse files and types it
    /// knows nothing of included, and each global header as one of no time (year 1) and
    /// no mode, which it calls regular.
    pub fn newest_regular(mut self) -> Result<Option<(i64, u32)>, Error> {
        let mut newest = None;
        let mut pax = BTreeMap::new();
        let mut long_name = Vec::new();
        while !self.done {
            let Some(block) = self.block()? else {
                break;
            };
            if block.iter().all(|&b| b == 0) {
                if self.block()?.is_some_and(|b| b.iter().any(|&c| c != 0)) {
                    return bad("a zero block followed by a header");
                }
                break;
            }
            let h = header(&block)?;
            if !header_only(h.flag) && h.size < 0 {
                return bad(format!("negative size {}", h.size));
            }
            let (time, regular) = match h.flag {
                b'x' => {
                    pax = parse_pax(&self.special(h.size)?)?;
                    continue;
                }
                b'L' => {
                    long_name = c_str(&self.special(h.size)?).to_vec();
                    continue;
                }
                b'K' => {
                    self.special(h.size)?;
                    continue;
                }
                // Its records are no member's: Go does not check them.
                b'g' => {
                    parse_pax(&self.special(h.size)?)?;
                    ((GO_ZERO_TIME, 0), true)
                }
                flag => {
                    let (mut name, mut time, mut size) = (h.name, (h.mtime, 0), h.size);
                    // mergePAX: an empty value keeps the header's.
                    for (key, value) in pax.iter().filter(|(_, v)| !v.is_empty()) {
                        match key.as_slice() {
                            b"path" => name.clone_from(value),
                            b"mtime" => time = pax_time(value)?,
                            b"size" => size = decimal(value)?,
                            b"uid" | b"gid" => {
                                decimal(value)?;
                            }
                            b"atime" | b"ctime" => {
                                pax_time(value)?;
                            }
                            _ => {}
                        }
                    }
                    if !long_name.is_empty() {
                        name = std::mem::take(&mut long_name);
                    }
                    // An old GNU sparse file's extension blocks, each saying whether
                    // another follows.
                    let mut extended = flag == b'S' && block.get(482).is_some_and(|&b| b != 0);
                    while extended {
                        let Some(next) = self.block()? else {
                            return bad("archive ends inside a header");
                        };
                        extended = next.get(504).is_some_and(|&b| b != 0);
                    }
                    let data = if header_only(flag) {
                        0
                    } else {
                        u64::try_from(size).map_err(|_| Error(format!("negative size {size}")))?
                    };
                    self.skip(data)?;
                    self.pad(data)?;
                    (time, go_regular(flag, h.mode, &name))
                }
            };
            pax.clear();
            long_name.clear();
            if regular && newest.is_none_or(|n| time > n) {
                newest = Some(time);
            }
        }
        Ok(newest)
    }

    /// The next block: `None` at the end of the stream, an error if it ends inside one.
    fn block(&mut self) -> Result<Option<[u8; BLOCK]>, Error> {
        let mut b = [0u8; BLOCK];
        let mut n = 0;
        while let Some(rest) = b.get_mut(n..)
            && !rest.is_empty()
        {
            match self.inner.read(rest) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        self.pos += n as u64;
        match n {
            0 => Ok(None),
            BLOCK => Ok(Some(b)),
            _ => bad("archive ends inside a header"),
        }
    }

    /// A PAX header's or GNU long name's data.
    fn special(&mut self, size: i64) -> Result<Vec<u8>, Error> {
        let size = u64::try_from(size)
            .ok()
            .filter(|&s| s <= MAX_SPECIAL)
            .ok_or_else(|| Error(format!("{size}-byte metadata member")))?;
        let mut data = Vec::new();
        (&mut self.inner).take(size).read_to_end(&mut data)?;
        self.pos += data.len() as u64;
        if data.len() as u64 != size {
            return bad("archive ends inside a member");
        }
        self.pad(size)?;
        Ok(data)
    }

    fn skip(&mut self, n: u64) -> Result<(), Error> {
        let skipped = io::copy(&mut (&mut self.inner).take(n), &mut io::sink())?;
        self.pos += skipped;
        if skipped != n {
            return bad("archive ends inside a member");
        }
        Ok(())
    }

    /// Skips the padding after `size` bytes of data. Go reads a stream that ends inside it
    /// as a complete archive.
    fn pad(&mut self, size: u64) -> Result<(), Error> {
        let n = (BLOCK as u64 - size % BLOCK as u64) % BLOCK as u64;
        let skipped = io::copy(&mut (&mut self.inner).take(n), &mut io::sink())?;
        self.pos += skipped;
        if skipped != n {
            self.done = true;
        }
        Ok(())
    }
}

/// Go's zero Time, January 1 of year 1, in seconds since 1970.
const GO_ZERO_TIME: i64 = -62_135_596_800;

/// Whether Go's `FileInfo().Mode()` of a member is a regular file's: no file type in the
/// type bits of its mode, which Go keeps 32 of, nor in its type flag, NUL with a name
/// ending in a slash being a directory's (`Reader.next`, `headerFileInfo.Mode`).
fn go_regular(flag: u8, mode: i64, name: &[u8]) -> bool {
    let typed_mode = matches!(
        mode & 0xffff_ffff & !0o7777,
        0o40000 | 0o10000 | 0o120000 | 0o60000 | 0o20000 | 0o140000
    );
    let typed_flag = matches!(flag, b'2'..=b'6') || (flag == 0 && name.ends_with(b"/"));
    !typed_mode && !typed_flag
}

/// Go's isHeaderOnlyType: types whose size is not followed by data.
fn header_only(flag: u8) -> bool {
    matches!(flag, b'1' | b'2' | b'3' | b'4' | b'5' | b'6')
}

fn id(v: i64, what: &str) -> Result<u32, Error> {
    u32::try_from(v).map_err(|_| Error(format!("{what} {v} is out of range")))
}

fn field(b: &[u8; BLOCK], at: usize, len: usize) -> &[u8] {
    b.get(at..at + len).unwrap_or_default()
}

/// Go's parseString: the bytes before the first NUL.
fn c_str(b: &[u8]) -> &[u8] {
    b.split(|&c| c == 0).next().unwrap_or_default()
}

/// Go's getFormat: the checksum must match, summed unsigned or (for old writers) signed,
/// and then the magic numbers name the format.
fn format(b: &[u8; BLOCK]) -> Result<Format, Error> {
    let want = octal(field(b, 148, 8))?;
    let (mut unsigned, mut signed) = (0i64, 0i64);
    for (i, &c) in b.iter().enumerate() {
        let c = if (148..156).contains(&i) { b' ' } else { c };
        unsigned += i64::from(c);
        signed += i64::from(c as i8);
    }
    if want != unsigned && want != signed {
        return bad("tar header checksum mismatch");
    }
    let magic = field(b, 257, 6);
    Ok(if magic == b"ustar\0" && field(b, 508, 4) == b"tar\0" {
        Format::Star
    } else if magic == b"ustar\0" {
        Format::Ustar
    } else if magic == b"ustar " && field(b, 263, 2) == b" \0" {
        Format::Gnu
    } else {
        Format::V7
    })
}

/// Go's readHeader: every numeric field must parse, and ustar and star headers prefix
/// long names.
fn header(b: &[u8; BLOCK]) -> Result<Header, Error> {
    let format = format(b)?;
    let f = |at, len| field(b, at, len);
    let mut h = Header {
        flag: f(156, 1).first().copied().unwrap_or(0),
        name: c_str(f(0, 100)).to_vec(),
        link: c_str(f(157, 100)).to_vec(),
        size: numeric(f(124, 12))?,
        mode: numeric(f(100, 8))?,
        uid: numeric(f(108, 8))?,
        gid: numeric(f(116, 8))?,
        mtime: numeric(f(136, 12))?,
        devmajor: 0,
        devminor: 0,
    };
    if format == Format::V7 {
        return Ok(h);
    }
    h.devmajor = numeric(f(329, 8))?;
    h.devminor = numeric(f(337, 8))?;
    let prefix = match format {
        Format::Star => {
            // The access and change times.
            numeric(f(476, 12))?;
            numeric(f(488, 12))?;
            c_str(f(345, 131))
        }
        // Go before 1.8 wrote a ustar prefix into GNU headers, over the access and change
        // times; Go reads it back as a prefix when those fields are not numbers.
        Format::Gnu => {
            let times_valid = [f(345, 12), f(357, 12)]
                .iter()
                .all(|t| t.first() == Some(&0) || numeric(t).is_ok());
            let prefix = c_str(f(345, 155));
            if times_valid || !prefix.is_ascii() {
                &[]
            } else {
                prefix
            }
        }
        _ => c_str(f(345, 155)),
    };
    if !prefix.is_empty() {
        h.name = [prefix, b"/", &h.name].concat();
    }
    Ok(h)
}

/// Go's parseNumeric: base-256 two's complement when the top bit is set, else octal.
fn numeric(b: &[u8]) -> Result<i64, Error> {
    let Some(&first) = b.first() else {
        return Ok(0);
    };
    if first & 0x80 == 0 {
        return octal(b);
    }
    let inv = if first & 0x40 != 0 { 0xff } else { 0 };
    let mut x: u64 = 0;
    for (i, &c) in b.iter().enumerate() {
        let c = if i == 0 { (c ^ inv) & 0x7f } else { c ^ inv };
        if x >> 56 != 0 {
            return bad("base-256 number overflows");
        }
        x = x << 8 | u64::from(c);
    }
    let x = i64::try_from(x).map_err(|_| Error("base-256 number overflows".into()))?;
    Ok(if inv == 0xff { !x } else { x })
}

/// Go's parseOctal: spaces and NULs trimmed from both ends, cut at a NUL, then octal.
fn octal(b: &[u8]) -> Result<i64, Error> {
    let start = b.iter().position(|&c| c != b' ' && c != 0).unwrap_or(b.len());
    let end = b
        .iter()
        .rposition(|&c| c != b' ' && c != 0)
        .map_or(start, |e| e + 1);
    let mut v: i64 = 0;
    for &c in c_str(b.get(start..end).unwrap_or_default()) {
        if !(b'0'..=b'7').contains(&c) {
            return bad(format!("bad octal digit {:?} in a tar header", char::from(c)));
        }
        v = v
            .checked_mul(8)
            .and_then(|v| v.checked_add(i64::from(c - b'0')))
            .ok_or_else(|| Error("octal number overflows".into()))?;
    }
    Ok(v)
}

/// Go's parsePAX: records of `<length> <key>=<value>\n`, the length counting the whole
/// record; keys are not empty, and neither they nor path values hold NULs.
fn parse_pax(data: &[u8]) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, Error> {
    let malformed = || Error("malformed PAX record".into());
    let mut records = BTreeMap::new();
    let mut s = data;
    while !s.is_empty() {
        let space = s.iter().position(|&c| c == b' ').ok_or_else(malformed)?;
        let len = std::str::from_utf8(s.get(..space).unwrap_or_default())
            .ok()
            .and_then(|n| n.parse::<i64>().ok())
            .and_then(|n| usize::try_from(n).ok())
            .filter(|&n| n >= 5 && n <= s.len() && n > space + 1)
            .ok_or_else(malformed)?;
        if s.get(len - 1) != Some(&b'\n') {
            return Err(malformed());
        }
        let record = s.get(space + 1..len - 1).ok_or_else(malformed)?;
        let eq = record.iter().position(|&c| c == b'=').ok_or_else(malformed)?;
        let key = record.get(..eq).unwrap_or_default();
        let value = record.get(eq + 1..).unwrap_or_default();
        let nul = match key {
            b"path" | b"linkpath" | b"uname" | b"gname" => value.contains(&0),
            _ => key.contains(&0),
        };
        if key.is_empty() || nul {
            return Err(malformed());
        }
        records.insert(key.to_vec(), value.to_vec());
        s = s.get(len..).unwrap_or_default();
    }
    Ok(records)
}

/// Go's strconv.ParseInt(v, 10, 64).
fn decimal(v: &[u8]) -> Result<i64, Error> {
    std::str::from_utf8(v)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| Error(format!("bad PAX number {:?}", String::from_utf8_lossy(v))))
}

/// Go's parsePAXTime: decimal seconds, then optionally a fraction, of which nine digits
/// count. A negative time's fraction counts down from its seconds, as time.Unix
/// normalizes it.
fn pax_time(v: &[u8]) -> Result<(i64, u32), Error> {
    let malformed = || Error(format!("bad PAX time {:?}", String::from_utf8_lossy(v)));
    let (secs, frac) = match v.iter().position(|&c| c == b'.') {
        Some(dot) => (
            v.get(..dot).unwrap_or_default(),
            v.get(dot + 1..).unwrap_or_default(),
        ),
        None => (v, &[][..]),
    };
    let secs = decimal(secs).map_err(|_| malformed())?;
    if !frac.iter().all(u8::is_ascii_digit) {
        return Err(malformed());
    }
    let nsec = (0..9).fold(0u32, |n, i| {
        n * 10 + frac.get(i).map_or(0, |&c| u32::from(c - b'0'))
    });
    if v.first() == Some(&b'-') && nsec > 0 {
        return Ok((secs.saturating_sub(1), 1_000_000_000 - nsec));
    }
    Ok((secs, nsec))
}

/// Cleans a member path lexically, as containerd's filepath.Clean does before it joins the
/// path to the root: empty and `.` components go, and `..` removes the component before
/// it. A `..` at the root of an absolute path stays at the root; one that climbs out of a
/// relative path is refused, as moby refuses it.
fn clean(path: &[u8]) -> Result<Vec<u8>, Error> {
    let rooted = path.first() == Some(&b'/');
    let mut parts: Vec<&[u8]> = Vec::new();
    for part in path.split(|&c| c == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                if parts.pop().is_none() && !rooted {
                    return bad(format!(
                        "path {:?} leaves the root",
                        String::from_utf8_lossy(path)
                    ));
                }
            }
            p => parts.push(p),
        }
    }
    Ok(parts.join(&b'/'))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
pub(crate) mod tests {
    use std::path::Path;

    use super::*;

    /// Writes ustar archives for tests.
    #[derive(Default)]
    pub(crate) struct Writer {
        out: Vec<u8>,
    }

    #[derive(Clone, Copy)]
    pub(crate) struct Member<'a> {
        pub(crate) name: &'a [u8],
        pub(crate) flag: u8,
        pub(crate) data: &'a [u8],
        pub(crate) link: &'a [u8],
        pub(crate) mode: u32,
        pub(crate) uid: u32,
        pub(crate) gid: u32,
        pub(crate) mtime: u64,
        pub(crate) dev: (u32, u32),
        /// The size field, when it is not the data's length.
        pub(crate) size: Option<u64>,
    }

    impl Default for Member<'_> {
        fn default() -> Self {
            Member {
                name: b"",
                flag: b'0',
                data: b"",
                link: b"",
                mode: 0o644,
                uid: 0,
                gid: 0,
                mtime: 1_700_000_000,
                dev: (0, 0),
                size: None,
            }
        }
    }

    fn octal_field(dst: &mut [u8], v: u64) {
        let s = format!("{:0width$o}\0", v, width = dst.len() - 1);
        dst.copy_from_slice(&s.as_bytes()[s.len() - dst.len()..]);
    }

    impl Writer {
        pub(crate) fn member(&mut self, m: Member<'_>) -> &mut Self {
            let mut h = [0u8; BLOCK];
            h[..m.name.len()].copy_from_slice(m.name);
            octal_field(&mut h[100..108], u64::from(m.mode));
            octal_field(&mut h[108..116], u64::from(m.uid));
            octal_field(&mut h[116..124], u64::from(m.gid));
            octal_field(&mut h[124..136], m.size.unwrap_or(m.data.len() as u64));
            octal_field(&mut h[136..148], m.mtime);
            h[156] = m.flag;
            h[157..157 + m.link.len()].copy_from_slice(m.link);
            h[257..263].copy_from_slice(b"ustar\0");
            h[263..265].copy_from_slice(b"00");
            octal_field(&mut h[329..337], u64::from(m.dev.0));
            octal_field(&mut h[337..345], u64::from(m.dev.1));
            h[148..156].fill(b' ');
            let sum: u64 = h.iter().map(|&b| u64::from(b)).sum();
            octal_field(&mut h[148..155], sum);
            self.out.extend_from_slice(&h);
            self.out.extend_from_slice(m.data);
            self.out.resize(self.out.len().next_multiple_of(BLOCK), 0);
            self
        }

        /// A PAX extended header for the next member.
        pub(crate) fn pax(&mut self, records: &[(&str, &[u8])]) -> &mut Self {
            let mut body = Vec::new();
            for (k, v) in records {
                let payload = [b" ".as_slice(), k.as_bytes(), b"=", v, b"\n"].concat();
                let mut len = payload.len() + 1;
                while format!("{len}").len() + payload.len() != len {
                    len += 1;
                }
                body.extend_from_slice(format!("{len}").as_bytes());
                body.extend_from_slice(&payload);
            }
            self.member(Member {
                name: b"PaxHeader",
                flag: b'x',
                data: &body,
                ..Member::default()
            })
        }

        pub(crate) fn finish(&mut self) -> Vec<u8> {
            let mut out = std::mem::take(&mut self.out);
            out.extend_from_slice(&[0u8; 2 * BLOCK]);
            out
        }
    }

    fn entries(archive: &[u8]) -> Result<Vec<Entry>, Error> {
        let mut r = Reader::new(archive);
        let mut out = Vec::new();
        while let Some(e) = r.next_entry()? {
            out.push(e);
        }
        Ok(out)
    }

    fn fnv(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
        })
    }

    fn unhex(s: &str) -> Vec<u8> {
        s.as_bytes()
            .chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    /// testdata/go-tar holds Go's archive/tar test archives (go1.27.1) and, in expect.txt,
    /// what Go reads from each (expect.go). Where Go returns an entry we support, we return
    /// the same one; where Go returns one we refuse (sparse files, GNU dumpdirs) or fails,
    /// we fail; global headers, which Go returns as entries, we skip.
    /// testdata/tar-newest/newest.txt holds when Go's archive/tar (go1.27.1) finds the
    /// newest regular member of each of Go's test archives and of the ones newest.go
    /// crafts was modified, as BuildKit takes a source's time; where Go fails, we fail.
    #[test]
    fn finds_the_newest_regular_member_as_go_does() {
        let testdata = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
        let newest = std::fs::read_to_string(testdata.join("tar-newest/newest.txt")).unwrap();
        let mut lines = newest.lines();
        let mut checked = 0;
        while let Some(line) = lines.next() {
            let name = line.strip_prefix("archive ").unwrap();
            let want = lines.next().unwrap();
            let archive = std::fs::read(testdata.join(name)).unwrap();
            let got = Reader::new(&archive[..]).newest_regular();
            match want.split(' ').collect::<Vec<_>>().as_slice() {
                ["newest", secs, nsec] => {
                    assert_eq!(
                        got.unwrap(),
                        Some((secs.parse().unwrap(), nsec.parse().unwrap())),
                        "{name}"
                    );
                }
                ["none"] => assert_eq!(got.unwrap(), None, "{name}"),
                ["error", ..] => assert!(got.is_err(), "{name}: {got:?}"),
                _ => panic!("{want}"),
            }
            checked += 1;
        }
        assert_eq!(checked, 53);
    }

    #[test]
    fn reads_what_go_reads() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/go-tar");
        let expect = std::fs::read_to_string(dir.join("expect.txt")).unwrap();
        let mut archives: Vec<(&str, Vec<&str>)> = Vec::new();
        for line in expect.lines() {
            match line.strip_prefix("archive ") {
                Some(name) => archives.push((name, Vec::new())),
                None => archives.last_mut().unwrap().1.push(line),
            }
        }
        assert_eq!(archives.len(), 38);
        let (mut compared, mut refused) = (0, 0);
        for (name, lines) in &archives {
            let bytes = std::fs::read(dir.join(name)).unwrap();
            let mut r = Reader::new(bytes.as_slice());
            for line in lines {
                if *line == "end" {
                    assert_eq!(r.next_entry().unwrap(), None, "{name}");
                    break;
                }
                if line.starts_with("error") {
                    assert!(r.next_entry().is_err(), "{name}: Go fails");
                    break;
                }
                let go: BTreeMap<&str, &str> = line
                    .strip_prefix("entry ")
                    .unwrap()
                    .split(' ')
                    .map(|kv| kv.split_once('=').unwrap())
                    .collect();
                let num = |k: &str| go[k].parse::<i64>().unwrap();
                let kind = match (num("flag") as u8, go["sparse"]) {
                    (b'g', _) => continue,
                    (b'0' | b'7', "0") => Type::File,
                    (b'1', _) => Type::HardLink,
                    (b'2', _) => Type::Symlink,
                    (b'3', _) => Type::CharDevice,
                    (b'4', _) => Type::BlockDevice,
                    (b'5', _) => Type::Dir,
                    (b'6', _) => Type::Fifo,
                    _ => {
                        assert!(r.next_entry().is_err(), "{name}: we refuse {line}");
                        refused += 1;
                        break;
                    }
                };
                let e = r.next_entry().unwrap().unwrap();
                let link = unhex(go["link"]);
                let (data_len, data_hash) = go["data"].split_once(':').unwrap();
                let file = kind == Type::File;
                let device = matches!(kind, Type::CharDevice | Type::BlockDevice);
                let xattrs: BTreeMap<Vec<u8>, Vec<u8>> = go["xattrs"]
                    .split(',')
                    .filter(|x| !x.is_empty())
                    .map(|x| x.split_once(':').unwrap())
                    .map(|(k, v)| (unhex(k), unhex(v)))
                    .collect();
                let want = Entry {
                    path: clean(&unhex(go["name"])).unwrap(),
                    kind,
                    link: if kind == Type::HardLink {
                        clean(&link).unwrap()
                    } else {
                        link
                    },
                    mode: (num("mode") & 0o7777) as u32,
                    uid: num("uid") as u32,
                    gid: num("gid") as u32,
                    mtime: num("mtime"),
                    mtime_nsec: num("nsec") as u32,
                    size: if file { num("size") as u64 } else { 0 },
                    devmajor: if device { num("devmajor") as u32 } else { 0 },
                    devminor: if device { num("devminor") as u32 } else { 0 },
                    xattrs,
                    offset: e.offset,
                };
                assert_eq!(e, want, "{name}");
                let data = &bytes[e.offset as usize..(e.offset + e.size) as usize];
                assert_eq!(data.len().to_string(), data_len, "{name}");
                assert_eq!(format!("{:016x}", fnv(data)), data_hash, "{name}");
                compared += 1;
            }
        }
        assert_eq!((compared, refused), (50, 6));
    }

    #[test]
    fn pax_records_override_the_header() {
        let archive = Writer::default()
            .pax(&[
                ("path", b"from/pax"),
                ("uid", b"+100000"),
                ("gid", b""),
                ("mtime", b"-1.25"),
                ("SCHILY.xattr.user.empty", b""),
                ("SCHILY.xattr.security.capability", &[1, 0, 0, 2]),
            ])
            .member(Member {
                name: b"from/header",
                gid: 7,
                data: b"x",
                ..Member::default()
            })
            .member(Member {
                name: b"./dir/../link",
                flag: b'1',
                link: b"/from/./pax",
                ..Member::default()
            })
            .member(Member {
                name: b"contiguous",
                flag: b'7',
                data: b"c",
                ..Member::default()
            })
            .finish();
        let e = entries(&archive).unwrap();
        assert_eq!(e[0].path, b"from/pax");
        assert_eq!(
            (e[0].uid, e[0].gid),
            (100_000, 7),
            "an empty value keeps the header's"
        );
        assert_eq!((e[0].mtime, e[0].mtime_nsec), (-2, 750_000_000));
        assert_eq!(e[0].xattrs.get(&b"user.empty"[..]), Some(&Vec::new()));
        assert_eq!(e[0].xattrs.len(), 2);
        assert_eq!(
            (e[1].path.as_slice(), e[1].link.as_slice()),
            (&b"link"[..], &b"from/pax"[..])
        );
        assert!(e[1].xattrs.is_empty(), "PAX records apply to one member");
        assert_eq!((e[2].kind, e[2].size), (Type::File, 1));
    }

    #[test]
    fn archives_may_end_early() {
        let mut w = Writer::default();
        w.member(Member {
            name: b"a",
            data: b"abc",
            ..Member::default()
        });
        let full = w.finish();
        // No end blocks, one end block, and no padding after the last member's data.
        for len in [BLOCK + 3, 2 * BLOCK, 3 * BLOCK] {
            let e = entries(&full[..len]).unwrap();
            assert_eq!((e.len(), e[0].size), (1, 3), "{len} bytes");
        }
        for len in [BLOCK + 2, BLOCK / 2, 2 * BLOCK + 100] {
            assert!(entries(&full[..len]).is_err(), "{len} bytes");
        }
        let mut next = full.clone();
        next[3 * BLOCK] = 1;
        assert!(entries(&next).is_err(), "a zero block followed by a header");
    }

    #[test]
    fn hostile_archives_are_refused() {
        let one = |m: Member<'_>| Writer::default().member(m).finish();
        let refused = [
            one(Member {
                name: b"../etc/passwd",
                ..Member::default()
            }),
            one(Member {
                name: b"a",
                flag: b'1',
                link: b"a/../../b",
                ..Member::default()
            }),
            one(Member {
                name: b"PaxHeader",
                flag: b'x',
                data: b"99 path=x\n",
                ..Member::default()
            }),
            one(Member {
                name: b"PaxHeader",
                flag: b'x',
                data: b"3 x\n",
                ..Member::default()
            }),
            one(Member {
                name: b"PaxHeader",
                flag: b'x',
                data: b"12 path=a\0b\n",
                ..Member::default()
            }),
            one(Member {
                name: b"PaxHeader",
                flag: b'x',
                data: &vec![b'x'; MAX_SPECIAL as usize + 1],
                ..Member::default()
            }),
            one(Member {
                name: b"dev",
                flag: b'V',
                ..Member::default()
            }),
            Writer::default()
                .pax(&[("GNU.sparse.major", b"1"), ("GNU.sparse.minor", b"0")])
                .member(Member {
                    name: b"sparse",
                    ..Member::default()
                })
                .finish(),
        ];
        for (i, archive) in refused.iter().enumerate() {
            assert!(entries(archive).is_err(), "archive {i}");
        }
        let mut corrupt = one(Member {
            name: b"file",
            ..Member::default()
        });
        corrupt[0] ^= 1;
        assert!(entries(&corrupt).is_err(), "a bad checksum");
    }

    #[test]
    fn paths_are_cleaned() {
        assert_eq!(clean(b"./a//b/./c/").unwrap(), b"a/b/c");
        assert_eq!(clean(b"/abs/x").unwrap(), b"abs/x");
        assert_eq!(clean(b"a/../b").unwrap(), b"b");
        assert_eq!(clean(b"/../x").unwrap(), b"x");
        assert_eq!(clean(b"./").unwrap(), b"");
        assert!(clean(b"../x").is_err());
        assert!(clean(b"a/../../b").is_err());
    }

    #[test]
    fn numbers_parse_as_go_parses_them() {
        assert_eq!(numeric(b"0000644\0").unwrap(), 0o644);
        assert_eq!(numeric(b"\0 644 \0").unwrap(), 0o644, "leading NULs and spaces");
        assert_eq!(numeric(b"\0\0\0\0\0\0\0\0").unwrap(), 0);
        assert_eq!(numeric(&[0x80, 0, 0, 0, 0, 0x4c, 0x4b, 0x40]).unwrap(), 5_000_000);
        assert_eq!(numeric(&[0xff; 8]).unwrap(), -1, "base-256 two's complement");
        assert!(numeric(b"+0644\0").is_err());
        assert!(numeric(b"0689\0").is_err());
        assert!(
            numeric(&[
                0x80, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff
            ])
            .is_err()
        );
        assert_eq!(pax_time(b"1.5").unwrap(), (1, 500_000_000));
        assert_eq!(pax_time(b"-0.5").unwrap(), (-1, 500_000_000));
        assert_eq!(pax_time(b"7.1234567899").unwrap(), (7, 123_456_789));
        assert!(pax_time(b".5").is_err());
        assert!(pax_time(b"1.5x").is_err());
    }
}
