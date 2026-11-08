//! Go's archive/tar (go1.26.1 src/archive/tar): headers as Go models them, a reader of
//! USTAR, PAX, GNU (long names, sparse files) and STAR archives, and a writer of USTAR and
//! PAX, byte for byte as Go writes them. go-archive reads and writes every archive through
//! it, so Docker's archives are what these make.

use std::collections::BTreeMap;

use crate::error::{Error, Kind, quote};

mod reader;
mod strconv;
mod writer;

pub use reader::Reader;
pub use writer::Writer;

pub(crate) use strconv::format_pax_time;

/// Type flags (common.go).
pub const TYPE_REG: u8 = b'0';
/// Go's deprecated TypeRegA, read as TYPE_REG (or TYPE_DIR for a name ending in `/`).
pub const TYPE_REGA: u8 = 0;
pub const TYPE_LINK: u8 = b'1';
pub const TYPE_SYMLINK: u8 = b'2';
pub const TYPE_CHAR: u8 = b'3';
pub const TYPE_BLOCK: u8 = b'4';
pub const TYPE_DIR: u8 = b'5';
pub const TYPE_FIFO: u8 = b'6';
pub const TYPE_CONT: u8 = b'7';
pub const TYPE_XHEADER: u8 = b'x';
pub const TYPE_XGLOBAL_HEADER: u8 = b'g';
pub const TYPE_GNU_SPARSE: u8 = b'S';
pub const TYPE_GNU_LONGNAME: u8 = b'L';
pub const TYPE_GNU_LONGLINK: u8 = b'K';

/// The PAX record prefix of extended attributes.
pub const PAX_SCHILY_XATTR: &[u8] = b"SCHILY.xattr.";

pub(crate) const BLOCK: usize = 512;
pub(crate) const NAME_SIZE: usize = 100;
pub(crate) const PREFIX_SIZE: usize = 155;
/// maxSpecialFileSize: the largest PAX header or GNU long name.
pub(crate) const MAX_SPECIAL: usize = 1 << 20;

pub(crate) const ERR_HEADER: &str = "archive/tar: invalid tar header";
pub(crate) const ERR_FIELD_TOO_LONG: &str = "archive/tar: header field too long";

/// Whether the writer can write `h`: Go's allowedFormats leaves USTAR or PAX.
pub(crate) fn allowed(h: &Header) -> Option<()> {
    let a = allowed_formats(h).ok()?;
    (a.format.has(Format::USTAR) || a.format.has(Format::PAX)).then_some(())
}

/// The archive's error a read of an entry's data gave, or the I/O error as Go prints it.
pub(crate) fn read_error(e: &std::io::Error) -> Error {
    match e.get_ref().and_then(|i| i.downcast_ref::<Error>()) {
        Some(inner) => inner.clone(),
        None => Error::io(e),
    }
}

pub(crate) fn err_header() -> Error {
    Error::new(Kind::Header, ERR_HEADER)
}

/// The keys of records Go makes from a header's own fields (basicKeys).
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

/// Go's tar.Format: a set of formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Format(pub u8);

impl Format {
    pub const UNKNOWN: Format = Format(0);
    pub const V7: Format = Format(1);
    pub const USTAR: Format = Format(2);
    pub const PAX: Format = Format(4);
    pub const GNU: Format = Format(8);
    pub const STAR: Format = Format(16);

    pub fn has(self, f: Format) -> bool {
        self.0 & f.0 != 0
    }

    fn may_only_be(&mut self, f: Format) {
        self.0 &= f.0;
    }

    fn must_not_be(&mut self, f: Format) {
        self.0 &= !f.0;
    }

    fn may_be(&mut self, f: Format) {
        self.0 |= f.0;
    }
}

/// Go's time.Time, as archives carry it: Unix seconds and nanoseconds in 0..1e9. Go's
/// zero time is January 1 of year 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Time {
    pub sec: i64,
    pub nsec: u32,
}

impl Time {
    pub const ZERO: Time = Time {
        sec: -62_135_596_800,
        nsec: 0,
    };

    /// time.Unix: the nanoseconds folded into the seconds. Seconds wrap as Go's do.
    pub fn unix(sec: i64, nsec: i64) -> Time {
        let (mut sec, mut nsec) = (sec, nsec);
        if !(0..1_000_000_000).contains(&nsec) {
            let n = nsec / 1_000_000_000;
            sec = sec.wrapping_add(n);
            nsec -= n * 1_000_000_000;
            if nsec < 0 {
                nsec += 1_000_000_000;
                sec = sec.wrapping_sub(1);
            }
        }
        Time {
            sec,
            nsec: u32::try_from(nsec).unwrap_or(0),
        }
    }

    pub fn is_zero(self) -> bool {
        self == Time::ZERO
    }
}

/// Go's tar.Header. Names and values are bytes, as Go's strings are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub typeflag: u8,
    pub name: Vec<u8>,
    pub linkname: Vec<u8>,
    pub size: i64,
    pub mode: i64,
    pub uid: i64,
    pub gid: i64,
    pub uname: Vec<u8>,
    pub gname: Vec<u8>,
    pub mtime: Time,
    pub atime: Time,
    pub ctime: Time,
    pub devmajor: i64,
    pub devminor: i64,
    /// PAXRecords. Go's Header.Xattrs, which mirrors the `SCHILY.xattr.` records, is not
    /// kept apart: the writer makes the same records of either.
    pub pax: BTreeMap<Vec<u8>, Vec<u8>>,
    pub format: Format,
}

impl Default for Header {
    fn default() -> Header {
        Header {
            typeflag: 0,
            name: Vec::new(),
            linkname: Vec::new(),
            size: 0,
            mode: 0,
            uid: 0,
            gid: 0,
            uname: Vec::new(),
            gname: Vec::new(),
            mtime: Time::ZERO,
            atime: Time::ZERO,
            ctime: Time::ZERO,
            devmajor: 0,
            devminor: 0,
            pax: BTreeMap::new(),
            format: Format::UNKNOWN,
        }
    }
}

/// isHeaderOnlyType: types whose size is ignored.
pub(crate) fn header_only(flag: u8) -> bool {
    matches!(
        flag,
        TYPE_LINK | TYPE_SYMLINK | TYPE_CHAR | TYPE_BLOCK | TYPE_DIR | TYPE_FIFO
    )
}

/// blockPadding.
pub(crate) fn block_padding(n: u64) -> u64 {
    n.wrapping_neg() & (BLOCK as u64 - 1)
}

/// What Header.allowedFormats decides: the formats that can hold a header, the PAX
/// records it needs, and why none can.
pub(crate) struct Allowed {
    pub(crate) format: Format,
    pub(crate) pax: BTreeMap<Vec<u8>, Vec<u8>>,
    pub(crate) why: Option<Error>,
}

/// headerError's text.
fn header_error(parts: &[&str]) -> Error {
    let parts: Vec<&str> = parts.iter().copied().filter(|s| !s.is_empty()).collect();
    let prefix = "archive/tar: cannot encode header";
    if parts.is_empty() {
        return Error::other(prefix);
    }
    Error::other(format!("{prefix}: {}", parts.join("; and ")))
}

/// A time as `%v` prints it, in seconds: only error texts no shards archive reaches use it.
fn show_time(t: Time) -> String {
    String::from_utf8_lossy(&format_pax_time(t)).into_owned()
}

/// Header.allowedFormats (common.go).
pub(crate) fn allowed_formats(h: &Header) -> Result<Allowed, Error> {
    let mut format = Format(Format::USTAR.0 | Format::PAX.0 | Format::GNU.0);
    let mut pax: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let (mut no_ustar, mut no_pax, mut no_gnu) = (String::new(), String::new(), String::new());
    let mut prefer_pax = false;

    let mut verify_string = |s: &[u8], size: usize, name: &str, key: &[u8], format: &mut Format| {
        let too_long = s.len() > size;
        let long_gnu = key == b"path" || key == b"linkpath";
        if s.contains(&0) || (too_long && !long_gnu) {
            no_gnu = format!("GNU cannot encode {name}={}", quote(s));
            format.must_not_be(Format::GNU);
        }
        if !strconv::is_ascii(s) || too_long {
            let can_split = key == b"path";
            if !can_split || writer::split_ustar_path(s).is_none() {
                no_ustar = format!("USTAR cannot encode {name}={}", quote(s));
                format.must_not_be(Format::USTAR);
            }
            if key.is_empty() {
                no_pax = format!("PAX cannot encode {name}={}", quote(s));
                format.must_not_be(Format::PAX);
            } else {
                pax.insert(key.to_vec(), s.to_vec());
            }
        }
        if h.pax.get(key).is_some_and(|v| v == s) {
            pax.insert(key.to_vec(), s.to_vec());
        }
    };
    verify_string(&h.name, NAME_SIZE, "Name", b"path", &mut format);
    verify_string(&h.linkname, NAME_SIZE, "Linkname", b"linkpath", &mut format);
    verify_string(&h.uname, 32, "Uname", b"uname", &mut format);
    verify_string(&h.gname, 32, "Gname", b"gname", &mut format);

    let mut verify_numeric = |n: i64, size: usize, name: &str, key: &[u8], format: &mut Format| {
        if !strconv::fits_base256(size, n) {
            no_gnu = format!("GNU cannot encode {name}={n}");
            format.must_not_be(Format::GNU);
        }
        if !strconv::fits_octal(size, n) {
            no_ustar = format!("USTAR cannot encode {name}={n}");
            format.must_not_be(Format::USTAR);
            if key.is_empty() {
                no_pax = format!("PAX cannot encode {name}={n}");
                format.must_not_be(Format::PAX);
            } else {
                pax.insert(key.to_vec(), n.to_string().into_bytes());
            }
        }
        if h.pax.get(key).is_some_and(|v| *v == n.to_string().into_bytes()) {
            pax.insert(key.to_vec(), n.to_string().into_bytes());
        }
    };
    verify_numeric(h.mode, 8, "Mode", b"", &mut format);
    verify_numeric(h.uid, 8, "Uid", b"uid", &mut format);
    verify_numeric(h.gid, 8, "Gid", b"gid", &mut format);
    verify_numeric(h.size, 12, "Size", b"size", &mut format);
    verify_numeric(h.devmajor, 8, "Devmajor", b"", &mut format);
    verify_numeric(h.devminor, 8, "Devminor", b"", &mut format);

    let mut verify_time = |t: Time, size: usize, name: &str, key: &[u8], format: &mut Format| {
        if t.is_zero() {
            return;
        }
        if !strconv::fits_base256(size, t.sec) {
            no_gnu = format!("GNU cannot encode {name}={}", show_time(t));
            format.must_not_be(Format::GNU);
        }
        let is_mtime = key == b"mtime";
        let fits = strconv::fits_octal(size, t.sec);
        if (is_mtime && !fits) || !is_mtime {
            no_ustar = format!("USTAR cannot encode {name}={}", show_time(t));
            format.must_not_be(Format::USTAR);
        }
        if !is_mtime || !fits || t.nsec != 0 {
            prefer_pax = true;
            pax.insert(key.to_vec(), format_pax_time(t));
        }
        if h.pax.get(key).is_some_and(|v| *v == format_pax_time(t)) {
            pax.insert(key.to_vec(), format_pax_time(t));
        }
    };
    verify_time(h.mtime, 12, "ModTime", b"mtime", &mut format);
    verify_time(h.atime, 12, "AccessTime", b"atime", &mut format);
    verify_time(h.ctime, 12, "ChangeTime", b"ctime", &mut format);

    let mut only_pax = "";
    match h.typeflag {
        TYPE_REG | TYPE_CHAR | TYPE_BLOCK | TYPE_FIFO | TYPE_GNU_SPARSE => {
            if h.name.ends_with(b"/") {
                return Err(header_error(&["filename may not have trailing slash"]));
            }
        }
        TYPE_XHEADER | TYPE_GNU_LONGNAME | TYPE_GNU_LONGLINK => {
            return Err(header_error(&[
                "cannot manually encode TypeXHeader, TypeGNULongName, or TypeGNULongLink headers",
            ]));
        }
        TYPE_XGLOBAL_HEADER => {
            let only = Header {
                name: h.name.clone(),
                typeflag: h.typeflag,
                pax: h.pax.clone(),
                format: h.format,
                ..Header::default()
            };
            if *h != only {
                return Err(header_error(&[
                    "only PAXRecords should be set for TypeXGlobalHeader",
                ]));
            }
            only_pax = "only PAX supports TypeXGlobalHeader";
            format.may_only_be(Format::PAX);
        }
        _ => {}
    }
    if !header_only(h.typeflag) && h.size < 0 {
        return Err(header_error(&["negative size on header-only type"]));
    }
    if !h.pax.is_empty() {
        for (k, v) in &h.pax {
            if pax.contains_key(k) {
                continue;
            }
            if h.typeflag == TYPE_XGLOBAL_HEADER
                || (!BASIC_KEYS.contains(&k.as_slice()) && !k.starts_with(b"GNU.sparse."))
            {
                pax.insert(k.clone(), v.clone());
            }
        }
        only_pax = "only PAX supports PAXRecords";
        format.may_only_be(Format::PAX);
    }
    for (k, v) in &pax {
        if !strconv::valid_pax_record(k, v) {
            let rec = [k.as_slice(), b" = ", v.as_slice()].concat();
            return Err(header_error(&[&format!("invalid PAX record: {}", quote(&rec))]));
        }
    }
    if h.format != Format::UNKNOWN {
        let mut want = h.format;
        if want.has(Format::PAX) && !prefer_pax {
            want.may_be(Format::USTAR);
        }
        format.may_only_be(want);
    }
    let why = (format == Format::UNKNOWN).then(|| match h.format {
        Format::USTAR => header_error(&["Format specifies USTAR", &no_ustar, only_pax]),
        Format::PAX => header_error(&["Format specifies PAX", &no_pax]),
        Format::GNU => header_error(&["Format specifies GNU", &no_gnu, only_pax]),
        _ => header_error(&[&no_ustar, &no_pax, &no_gnu, only_pax]),
    });
    Ok(Allowed { format, pax, why })
}
