//! In-memory initramfs construction (cpio "newc", Linux Documentation/driver-api/
//! early-userspace/buffer-format.rst).
//!
//! Building the archive in the VMM lets it contain device nodes and root-owned files
//! without host privileges.

const MAGIC: &str = "070701";

#[derive(Debug, Default)]
pub struct Cpio {
    buf: Vec<u8>,
    ino: u32,
}

const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const S_IFCHR: u32 = 0o020000;

/// `v` as the eight hex digits a newc field is.
fn hex8(v: u32) -> [u8; 8] {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; 8];
    for (i, o) in out.iter_mut().enumerate() {
        // A nibble always names a digit; `get` says so without a bounds panic.
        *o = DIGITS
            .get(((v >> (28 - 4 * i)) & 0xf) as usize)
            .copied()
            .unwrap_or(b'0');
    }
    out
}

/// `n` rounded up to newc's 4-byte alignment.
const fn pad4(n: usize) -> usize {
    n.next_multiple_of(4)
}

/// The bytes an entry named `name` holding `data_len` bytes takes: its 110-byte header,
/// its name and NUL, and its data, each padded.
const fn entry_len(name: &str, data_len: usize) -> usize {
    pad4(110 + name.len() + 1) + pad4(data_len)
}

impl Cpio {
    pub fn new() -> Cpio {
        Cpio::default()
    }

    /// A builder whose archive will take `len` bytes, held from the start.
    fn with_capacity(len: usize) -> Cpio {
        Cpio {
            buf: Vec::with_capacity(len),
            ino: 0,
        }
    }

    fn entry(&mut self, name: &str, mode: u32, data: &[u8], rdev: (u32, u32)) {
        self.header(name, mode, data.len(), rdev);
        self.buf.extend_from_slice(data);
        self.pad();
    }

    /// An entry's header and name, padded, for `data_len` bytes of data to follow.
    fn header(&mut self, name: &str, mode: u32, data_len: usize, rdev: (u32, u32)) {
        self.ino += 1;
        let namesize = name.len() + 1;
        let fields = [
            self.ino,
            mode,
            0, // uid: root
            0, // gid: root
            if mode & S_IFDIR != 0 { 2 } else { 1 },
            0, // mtime: fixed, so archives are reproducible
            data_len as u32,
            0,
            0,
            rdev.0,
            rdev.1,
            namesize as u32,
            0,
        ];
        self.buf.extend_from_slice(MAGIC.as_bytes());
        for f in fields {
            self.buf.extend_from_slice(&hex8(f));
        }
        self.buf.extend_from_slice(name.as_bytes());
        self.buf.push(0);
        self.pad();
    }

    fn pad(&mut self) {
        while !self.buf.len().is_multiple_of(4) {
            self.buf.push(0);
        }
    }

    pub fn dir(&mut self, name: &str, perm: u32) -> &mut Self {
        self.entry(name, S_IFDIR | perm, &[], (0, 0));
        self
    }

    pub fn file(&mut self, name: &str, perm: u32, data: &[u8]) -> &mut Self {
        self.entry(name, S_IFREG | perm, data, (0, 0));
        self
    }

    pub fn char_dev(&mut self, name: &str, perm: u32, major: u32, minor: u32) -> &mut Self {
        self.entry(name, S_IFCHR | perm, &[], (major, minor));
        self
    }

    pub fn finish(mut self) -> Vec<u8> {
        self.entry("TRAILER!!!", 0, &[], (0, 0));
        self.buf
    }
}

/// The entries before `/init` in [`with_init`]'s archive.
fn root(c: &mut Cpio) {
    c.dir("dev", 0o755)
        .char_dev("dev/console", 0o600, 5, 1)
        .dir("proc", 0o555)
        .dir("sys", 0o555);
}

/// The bytes [`with_init`]'s archive takes for an init of `init_len` bytes.
pub const fn archive_len(init_len: usize) -> usize {
    entry_len("dev", 0)
        + entry_len("dev/console", 0)
        + entry_len("proc", 0)
        + entry_len("sys", 0)
        + entry_len("init", init_len)
        + entry_len("TRAILER!!!", 0)
}

/// The minimal root an init binary needs: `/dev/console` for its stdio, and `/init`.
pub fn with_init(init: &[u8]) -> Vec<u8> {
    let mut c = Cpio::with_capacity(archive_len(init.len()));
    root(&mut c);
    c.file("init", 0o755, init);
    c.finish()
}

/// [`with_init`] of the init binary `file` holds, `len` bytes of it, read straight into
/// its place in the archive (audit D13).
pub fn with_init_from(file: &mut impl std::io::Read, len: usize) -> std::io::Result<Vec<u8>> {
    let mut c = Cpio::with_capacity(archive_len(len));
    root(&mut c);
    c.header("init", S_IFREG | 0o755, len, (0, 0));
    let at = c.buf.len();
    c.buf.resize(at + len, 0);
    file.read_exact(c.buf.get_mut(at..).unwrap_or_default())?;
    c.pad();
    Ok(c.finish())
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;

    type Entry = (String, u32, (u32, u32), Vec<u8>);

    /// The archive is exactly as long as `archive_len` says, held in one allocation of that
    /// size, and an init read from a file makes the same bytes as one given whole (audit
    /// D13).
    #[test]
    fn archives_are_sized_once_and_read_in_place() {
        for len in [0usize, 1, 3, 4, 1 << 20, (1 << 20) + 3] {
            let init: Vec<u8> = (0..len).map(|i| (i * 7) as u8).collect();
            let whole = with_init(&init);
            assert_eq!(whole.len(), archive_len(len), "{len}");
            assert_eq!(whole.capacity(), archive_len(len), "{len}: room past its length");
            let read = with_init_from(&mut &init[..], len).unwrap();
            assert_eq!(read, whole, "{len}");
            assert_eq!(read.capacity(), archive_len(len), "{len}");
            // A file shorter than it said is an error, not a short archive.
            if len > 0 {
                assert!(with_init_from(&mut &init[..len - 1], len).is_err());
            }
        }
        assert_eq!(&hex8(0x0123_abcd), b"0123abcd");
    }

    /// Parses newc entries back: (name, mode, rdev, data).
    fn parse(mut b: &[u8]) -> Vec<Entry> {
        let hex = |s: &[u8]| u32::from_str_radix(std::str::from_utf8(s).unwrap(), 16).unwrap();
        let mut out = Vec::new();
        let total = b.len();
        loop {
            assert_eq!(&b[..6], MAGIC.as_bytes());
            let f: Vec<u32> = (0..13).map(|i| hex(&b[6 + 8 * i..14 + 8 * i])).collect();
            let (mode, filesize, rdev, namesize) = (f[1], f[6] as usize, (f[9], f[10]), f[11] as usize);
            let name = std::str::from_utf8(&b[110..110 + namesize - 1])
                .unwrap()
                .to_string();
            let data_off = (110 + namesize).next_multiple_of(4);
            let data = b[data_off..data_off + filesize].to_vec();
            let next = (data_off + filesize).next_multiple_of(4);
            let done = name == "TRAILER!!!";
            out.push((name, mode, rdev, data));
            if done {
                assert_eq!(next, b.len(), "archive ends after the trailer");
                assert_eq!(total % 4, 0);
                return out;
            }
            b = &b[next..];
        }
    }

    #[test]
    fn archive_roundtrips() {
        let entries = parse(&with_init(b"\x7fELF-ish"));
        let names: Vec<&str> = entries.iter().map(|e| e.0.as_str()).collect();
        assert_eq!(names, ["dev", "dev/console", "proc", "sys", "init", "TRAILER!!!"]);
        assert_eq!(entries[1].1, S_IFCHR | 0o600);
        assert_eq!(entries[1].2, (5, 1));
        assert_eq!(entries[4].1, S_IFREG | 0o755);
        assert_eq!(entries[4].3, b"\x7fELF-ish");
    }
}
