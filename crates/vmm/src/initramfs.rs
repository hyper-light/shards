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

impl Cpio {
    pub fn new() -> Cpio {
        Cpio::default()
    }

    fn entry(&mut self, name: &str, mode: u32, data: &[u8], rdev: (u32, u32)) {
        self.ino += 1;
        let namesize = name.len() + 1;
        let fields = [
            self.ino,
            mode,
            0, // uid: root
            0, // gid: root
            if mode & S_IFDIR != 0 { 2 } else { 1 },
            0, // mtime: fixed, so archives are reproducible
            data.len() as u32,
            0,
            0,
            rdev.0,
            rdev.1,
            namesize as u32,
            0,
        ];
        self.buf.extend_from_slice(MAGIC.as_bytes());
        for f in fields {
            self.buf.extend_from_slice(format!("{f:08x}").as_bytes());
        }
        self.buf.extend_from_slice(name.as_bytes());
        self.buf.push(0);
        self.pad();
        self.buf.extend_from_slice(data);
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

/// The minimal root an init binary needs: `/dev/console` for its stdio, and `/init`.
pub fn with_init(init: &[u8]) -> Vec<u8> {
    let mut c = Cpio::new();
    c.dir("dev", 0o755)
        .char_dev("dev/console", 0o600, 5, 1)
        .dir("proc", 0o555)
        .dir("sys", 0o555);
    c.file("init", 0o755, init);
    c.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses newc entries back: (name, mode, rdev, data).
    fn parse(mut b: &[u8]) -> Vec<(String, u32, (u32, u32), Vec<u8>)> {
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
