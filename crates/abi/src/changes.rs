//! Changes to a tree, as a byte stream between shards and its builder guests: what a build
//! step's process changed (the overlay's upper layer, from the guest), and what the host's
//! own steps changed (for the guest to stack as a layer). Not tar: the stream never leaves
//! shards, so it keeps what a layer's tar loses (nanoseconds, every xattr, overlay's own
//! markers) and none of tar's framing. The layers shards publishes are written apart, as
//! BuildKit writes them (crates/build).
//!
//! A stream is a sequence of entries, each a header and, for a regular file, its bytes,
//! and ends with [`END`]. A directory comes before what it holds. Integers are
//! little-endian. The host reads what a guest sends as untrusted: [`Decoder`] bounds every
//! length before it trusts it, and refuses a path that is not relative and normal.

use alloc::vec::Vec;

/// Entry kinds; `END` ends the stream.
pub mod kind {
    pub const END: u8 = 0;
    pub const DIR: u8 = 1;
    pub const FILE: u8 = 2;
    pub const SYMLINK: u8 = 3;
    /// Another name of a file earlier in the stream, whose path is the entry's target.
    pub const LINK: u8 = 4;
    pub const CHAR: u8 = 5;
    pub const BLOCK: u8 = 6;
    pub const FIFO: u8 = 7;
    pub const SOCKET: u8 = 8;
    /// The path is gone: what the layers below hold there is removed.
    pub const WHITEOUT: u8 = 9;
}

pub use kind::END;

/// The longest path accepted, Linux's PATH_MAX.
pub const MAX_PATH: usize = 4096;
/// The longest xattr name and value, Linux's XATTR_NAME_MAX and XATTR_SIZE_MAX.
pub const MAX_XATTR_NAME: usize = 255;
pub const MAX_XATTR_VALUE: usize = 65536;
/// The most xattrs one entry may carry: Linux bounds a file's list at XATTR_LIST_MAX,
/// 64 KiB of names each at least two bytes with its NUL.
pub const MAX_XATTRS: usize = 65536 / 2;

/// The longest header accepted, xattrs and all, as long as the run protocol's largest
/// frame: each limit above alone would let a header reach gigabytes, which the host would
/// hold before it could refuse them. A file whose xattrs need more fails its step.
pub const MAX_HEADER: usize = 1 << 20;

/// The header's fixed part: kind, flags, mode, uid, gid, seconds, nanoseconds, the device's
/// major and minor, the file's size, then the lengths of the path, the target, and the
/// xattr list.
const FIXED: usize = 1 + 1 + 4 + 4 + 4 + 8 + 4 + 4 + 4 + 8 + 4 + 4 + 4;

/// Flags.
pub mod flag {
    /// A directory that hides what the layers below hold in it (overlayfs's opaque
    /// directory): only what the stream holds under it remains.
    pub const OPAQUE: u8 = 1;
}

/// One entry's header.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Entry {
    pub kind: u8,
    pub flags: u8,
    /// Permission and special bits (0o7777); the kind gives the type.
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: i64,
    pub mtime_nsec: u32,
    pub major: u32,
    pub minor: u32,
    /// A regular file's length: that many bytes follow the header.
    pub size: u64,
    pub path: Vec<u8>,
    /// A symlink's target, or a link's other name.
    pub target: Vec<u8>,
    pub xattrs: Vec<(Vec<u8>, Vec<u8>)>,
}

impl Entry {
    /// Appends the header to `out`. A regular file's `size` bytes are to follow it.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        out.push(self.kind);
        out.push(self.flags);
        out.extend_from_slice(&self.mode.to_le_bytes());
        out.extend_from_slice(&self.uid.to_le_bytes());
        out.extend_from_slice(&self.gid.to_le_bytes());
        out.extend_from_slice(&self.mtime.to_le_bytes());
        out.extend_from_slice(&self.mtime_nsec.to_le_bytes());
        out.extend_from_slice(&self.major.to_le_bytes());
        out.extend_from_slice(&self.minor.to_le_bytes());
        out.extend_from_slice(&self.size.to_le_bytes());
        out.extend_from_slice(&len32(self.path.len()).to_le_bytes());
        out.extend_from_slice(&len32(self.target.len()).to_le_bytes());
        out.extend_from_slice(&len32(self.xattrs.len()).to_le_bytes());
        out.extend_from_slice(&self.path);
        out.extend_from_slice(&self.target);
        for (name, value) in &self.xattrs {
            out.extend_from_slice(&len32(name.len()).to_le_bytes());
            out.extend_from_slice(name);
            out.extend_from_slice(&len32(value.len()).to_le_bytes());
            out.extend_from_slice(value);
        }
    }
}

/// A length as the stream holds it. Every length the stream carries is bounded far below
/// u32's range, which a valid entry never exceeds; a longer one saturates, and the reader
/// refuses it.
fn len32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// What [`Decoder::next`] found.
#[derive(Debug, PartialEq, Eq)]
pub enum Event<'a> {
    /// An entry's header. A regular file's bytes follow as [`Event::Data`].
    Entry(Entry),
    /// Some of the current file's bytes, in order.
    Data(&'a [u8]),
    /// The stream ended, as it should, with [`END`].
    End,
    /// More bytes are needed.
    More,
}

/// Why a stream was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Kind(u8),
    Flags(u8),
    TooLong(&'static str),
    Path,
    /// Bytes after [`END`].
    Trailing,
}

/// Reads a stream fed in pieces, as they arrive. It holds at most one header's bytes; a
/// file's bytes pass through as they are fed.
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
    /// Bytes of the current file still to come.
    data_left: u64,
    ended: bool,
}

impl Decoder {
    pub fn new() -> Decoder {
        Decoder::default()
    }

    /// The next event from `input`, and how many of its bytes it took. Feed what it did
    /// not take back, with whatever arrives next.
    pub fn next<'a>(&mut self, input: &'a [u8]) -> Result<(Event<'a>, usize), Error> {
        if self.ended {
            return if input.is_empty() {
                Ok((Event::End, 0))
            } else {
                Err(Error::Trailing)
            };
        }
        if self.data_left > 0 {
            if input.is_empty() {
                return Ok((Event::More, 0));
            }
            let n = usize::try_from(self.data_left).map_or(input.len(), |left| left.min(input.len()));
            self.data_left -= n as u64;
            return Ok((Event::Data(input.get(..n).unwrap_or_default()), n));
        }
        // A header: gather it whole before reading it, taking only what it needs, so
        // that a file's bytes behind it stay the caller's.
        if self.buf.is_empty() && input.first() == Some(&END) {
            self.ended = true;
            return Ok((Event::End, 1));
        }
        let mut taken = 0;
        loop {
            let (whole, need) = header_len(&self.buf)?;
            if whole && self.buf.len() >= need {
                break;
            }
            let rest = input.get(taken..).unwrap_or_default();
            if rest.is_empty() {
                return Ok((Event::More, taken));
            }
            let n = need.saturating_sub(self.buf.len()).max(1).min(rest.len());
            self.buf.extend_from_slice(rest.get(..n).unwrap_or_default());
            taken += n;
        }
        let entry = parse(&self.buf)?;
        self.buf.clear();
        self.data_left = if entry.kind == kind::FILE { entry.size } else { 0 };
        Ok((Event::Entry(entry), taken))
    }

    /// Whether the stream has ended, with nothing left over.
    pub fn ended(&self) -> bool {
        self.ended
    }
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    let mut a = [0u8; 4];
    if let Some(s) = b.get(at..at + 4) {
        a.copy_from_slice(s);
    }
    u32::from_le_bytes(a)
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    if let Some(s) = b.get(at..at + 8) {
        a.copy_from_slice(s);
    }
    u64::from_le_bytes(a)
}

/// How long the header in `buf` is: `(true, n)` once its fixed part and its xattrs'
/// lengths are there, else `(false, n)`, `n` being as much as must be there before more is
/// known. Every length is bounded before it is added.
fn header_len(buf: &[u8]) -> Result<(bool, usize), Error> {
    if buf.len() < FIXED {
        return Ok((false, FIXED));
    }
    let kind = buf.first().copied().unwrap_or(0);
    if !(kind::DIR..=kind::WHITEOUT).contains(&kind) {
        return Err(Error::Kind(kind));
    }
    let flags = buf.get(1).copied().unwrap_or(0);
    if flags & !flag::OPAQUE != 0 || (flags & flag::OPAQUE != 0 && kind != kind::DIR) {
        return Err(Error::Flags(flags));
    }
    let path = u32_at(buf, FIXED - 12) as usize;
    let target = u32_at(buf, FIXED - 8) as usize;
    let xattrs = u32_at(buf, FIXED - 4) as usize;
    if path == 0 || path > MAX_PATH {
        return Err(Error::Path);
    }
    if target > MAX_PATH {
        return Err(Error::TooLong("target"));
    }
    if xattrs > MAX_XATTRS {
        return Err(Error::TooLong("xattrs"));
    }
    let mut len = FIXED + path + target;
    for _ in 0..xattrs {
        // Each xattr's name length, its name, its value's length and its value.
        if len > MAX_HEADER {
            return Err(Error::TooLong("header"));
        }
        if buf.len() < len + 4 {
            return Ok((false, len + 4));
        }
        let name = u32_at(buf, len) as usize;
        if name == 0 || name > MAX_XATTR_NAME {
            return Err(Error::TooLong("xattr name"));
        }
        if buf.len() < len + 4 + name + 4 {
            return Ok((false, len + 4 + name + 4));
        }
        let value = u32_at(buf, len + 4 + name) as usize;
        if value > MAX_XATTR_VALUE {
            return Err(Error::TooLong("xattr value"));
        }
        len += 4 + name + 4 + value;
    }
    if len > MAX_HEADER {
        return Err(Error::TooLong("header"));
    }
    Ok((true, len))
}

fn parse(buf: &[u8]) -> Result<Entry, Error> {
    let at = |a: usize, n: usize| buf.get(a..a + n).unwrap_or_default().to_vec();
    let path_len = u32_at(buf, FIXED - 12) as usize;
    let target_len = u32_at(buf, FIXED - 8) as usize;
    let n = u32_at(buf, FIXED - 4) as usize;
    let path = at(FIXED, path_len);
    if !normal(&path) {
        return Err(Error::Path);
    }
    let target = at(FIXED + path_len, target_len);
    let kind = buf.first().copied().unwrap_or(0);
    if kind == kind::LINK && !normal(&target) {
        return Err(Error::Path);
    }
    let mut xattrs = Vec::with_capacity(n.min(64));
    let mut off = FIXED + path_len + target_len;
    for _ in 0..n {
        let name_len = u32_at(buf, off) as usize;
        let name = at(off + 4, name_len);
        let value_len = u32_at(buf, off + 4 + name_len) as usize;
        let value = at(off + 8 + name_len, value_len);
        off += 8 + name_len + value_len;
        xattrs.push((name, value));
    }
    Ok(Entry {
        kind,
        flags: buf.get(1).copied().unwrap_or(0),
        mode: u32_at(buf, 2),
        uid: u32_at(buf, 6),
        gid: u32_at(buf, 10),
        mtime: u64_at(buf, 14) as i64,
        mtime_nsec: u32_at(buf, 22),
        major: u32_at(buf, 26),
        minor: u32_at(buf, 30),
        size: u64_at(buf, 34),
        path,
        target,
        xattrs,
    })
}

/// A relative path of named components alone: no leading or doubled `/`, no `.` or `..`,
/// no NUL.
pub fn normal(path: &[u8]) -> bool {
    !path.is_empty()
        && !path.contains(&0)
        && path
            .split(|&b| b == b'/')
            .all(|c| !c.is_empty() && c != b"." && c != b"..")
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn file(path: &[u8], data: &[u8]) -> Entry {
        Entry {
            kind: kind::FILE,
            mode: 0o644,
            mtime: 1_700_000_000,
            mtime_nsec: 123_456_789,
            size: data.len() as u64,
            path: path.to_vec(),
            xattrs: vec![(b"user.k".to_vec(), b"v".to_vec())],
            ..Entry::default()
        }
    }

    /// Everything a stream says, read back fed one byte at a time and all at once.
    fn decode(stream: &[u8], step: usize) -> Result<Vec<(Entry, Vec<u8>)>, Error> {
        let mut d = Decoder::new();
        let mut out: Vec<(Entry, Vec<u8>)> = Vec::new();
        let mut pending: Vec<u8> = Vec::new();
        let mut fed = 0;
        loop {
            if fed < stream.len() {
                let end = (fed + step).min(stream.len());
                pending.extend_from_slice(&stream[fed..end]);
                fed = end;
            }
            loop {
                let (event, n) = d.next(&pending)?;
                let ev = match event {
                    Event::Entry(e) => Some(Ok(e)),
                    Event::Data(b) => Some(Err(b.to_vec())),
                    Event::End => {
                        pending.drain(..n);
                        pending.extend_from_slice(&stream[fed..]);
                        if !pending.is_empty() {
                            d.next(&pending)?;
                        }
                        return Ok(out);
                    }
                    Event::More => None,
                };
                pending.drain(..n);
                match ev {
                    Some(Ok(e)) => out.push((e, Vec::new())),
                    Some(Err(b)) => {
                        if let Some(last) = out.last_mut() {
                            last.1.extend_from_slice(&b);
                        }
                    }
                    None => break,
                }
            }
            if fed == stream.len() && !d.ended() {
                return match d.next(&pending)? {
                    (Event::More, _) => Err(Error::TooLong("truncated")),
                    _ => Err(Error::Trailing),
                };
            }
        }
    }

    #[test]
    fn a_stream_reads_back_whatever_pieces_it_arrives_in() {
        let entries = vec![
            (
                Entry {
                    kind: kind::DIR,
                    flags: flag::OPAQUE,
                    mode: 0o755,
                    path: b"etc".to_vec(),
                    ..Entry::default()
                },
                vec![],
            ),
            (file(b"etc/a", b"hello"), b"hello".to_vec()),
            (file(b"etc/empty", b""), vec![]),
            (
                Entry {
                    kind: kind::LINK,
                    path: b"etc/b".to_vec(),
                    target: b"etc/a".to_vec(),
                    ..Entry::default()
                },
                vec![],
            ),
            (
                Entry {
                    kind: kind::WHITEOUT,
                    path: b"etc/gone".to_vec(),
                    ..Entry::default()
                },
                vec![],
            ),
            (
                Entry {
                    kind: kind::CHAR,
                    major: 1,
                    minor: 3,
                    path: b"dev/null".to_vec(),
                    ..Entry::default()
                },
                vec![],
            ),
        ];
        let mut stream = Vec::new();
        for (e, data) in &entries {
            e.encode_into(&mut stream);
            stream.extend_from_slice(data);
        }
        stream.push(END);
        for step in [1, 3, 7, 64, stream.len()] {
            assert_eq!(decode(&stream, step).unwrap(), entries, "fed {step} at a time");
        }
    }

    #[test]
    fn a_stream_that_says_too_much_is_refused() {
        let mut bad = Vec::new();
        let mut e = file(b"a", b"");
        e.path = b"../x".to_vec();
        e.encode_into(&mut bad);
        bad.push(END);
        assert_eq!(decode(&bad, 1000), Err(Error::Path));
        for path in [&b"/abs"[..], b"a//b", b"a/./b", b"", b"a\0b"] {
            let mut s = Vec::new();
            let mut e = file(b"a", b"");
            e.path = path.to_vec();
            e.encode_into(&mut s);
            s.push(END);
            assert_eq!(decode(&s, 1000), Err(Error::Path), "{path:?}");
        }
        let mut s = Vec::new();
        let mut e = file(b"a", b"");
        e.xattrs = vec![(vec![b'n'; MAX_XATTR_NAME + 1], vec![])];
        e.encode_into(&mut s);
        assert!(matches!(decode(&s, 1000), Err(Error::TooLong(_))));
        let mut s = Vec::new();
        file(b"a", b"").encode_into(&mut s);
        s[0] = 42;
        assert_eq!(decode(&s, 1000), Err(Error::Kind(42)));
        let mut s = Vec::new();
        let mut e = file(b"a", b"");
        e.flags = flag::OPAQUE;
        e.encode_into(&mut s);
        assert_eq!(decode(&s, 1000), Err(Error::Flags(flag::OPAQUE)));
        let s = vec![END, 0];
        assert_eq!(decode(&s, 1000), Err(Error::Trailing));
        // A link must name a path, as an entry must.
        let mut s = Vec::new();
        Entry {
            kind: kind::LINK,
            path: b"a".to_vec(),
            target: b"/etc/shadow".to_vec(),
            ..Entry::default()
        }
        .encode_into(&mut s);
        s.push(END);
        assert_eq!(decode(&s, 1000), Err(Error::Path));
    }

    /// Whatever bytes arrive, the decoder answers without panicking and holds no more
    /// than one header's worth.
    #[test]
    fn any_bytes_are_answered_within_bounds() {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for round in 0..2000 {
            // Mostly well-formed headers, bent at random, and pure noise.
            let mut s = Vec::new();
            let mut e = file(b"a/b", b"xy");
            e.xattrs.push((
                vec![b'k'; 1 + (next() % 300) as usize],
                vec![0; (next() % 70_000) as usize],
            ));
            e.encode_into(&mut s);
            for _ in 0..(next() % 8) {
                let i = (next() as usize) % s.len();
                s[i] = next() as u8;
            }
            if round % 3 == 0 {
                s = (0..(next() % 4096)).map(|_| next() as u8).collect();
            }
            let mut d = Decoder::new();
            let mut at = 0;
            while at < s.len() {
                let end = (at + 1 + (next() % 997) as usize).min(s.len());
                match d.next(&s[at..end]) {
                    Ok((_, 0)) | Err(_) => break,
                    Ok((_, n)) => at += n,
                }
                assert!(d.buf.len() <= MAX_HEADER);
            }
        }
    }

    #[test]
    fn a_stream_cut_short_is_not_taken_for_whole() {
        let mut s = Vec::new();
        file(b"a", b"hello").encode_into(&mut s);
        s.extend_from_slice(b"hel");
        assert!(decode(&s, 1000).is_err());
    }
}
