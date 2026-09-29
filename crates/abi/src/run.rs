//! The run protocol: how shards hands a workload to shards-init and gets back its output
//! and exit status (docs/design/architecture.md D16). The guest dials the host on
//! [`PORT`] once the image is mounted, and the one connection carries everything.
//!
//! Frames have the header of Docker's attach streams (moby api/pkg/stdcopy): a kind byte,
//! three zero bytes, and the payload's length as a big-endian u32. So output reaches the
//! host framed as Docker's Engine API sends it.

use alloc::vec::Vec;

/// The host port the guest dials for its workload.
pub const PORT: u32 = 1024;
/// The host port the guest dials, once the workload runs, for [`kind::SIGNAL`] frames.
/// They travel apart from the run connection, so that stdin a workload leaves unread
/// cannot hold them up, as Docker sends them apart from its attach stream.
pub const SIGNAL_PORT: u32 = 1025;
pub const HEADER: usize = 8;
/// The largest payload either side accepts.
pub const MAX_PAYLOAD: u32 = 1 << 20;

/// Frame kinds. The first four are stdcopy's streams.
pub mod kind {
    /// Host to guest: bytes for the workload's stdin. An empty frame closes it.
    pub const STDIN: u8 = 0;
    /// Guest to host: the workload's output.
    pub const STDOUT: u8 = 1;
    pub const STDERR: u8 = 2;
    /// Guest to host: why the workload did not start, before [`EXIT`].
    pub const SYSTEM_ERR: u8 = 3;
    /// Host to guest, first and once: the [`Spec`](super::Spec).
    pub const SPEC: u8 = 16;
    /// Guest to host, last: the exit status, a big-endian u32 as `docker run` reports
    /// it: the workload's code, 128 plus a fatal signal's number, or 125 to 127 when the
    /// command never ran.
    pub const EXIT: u8 = 17;
    /// Host to guest, on the [`SIGNAL_PORT`](super::SIGNAL_PORT) connection: a signal for
    /// the workload's main process, as a big-endian u32 in Linux's numbering.
    pub const SIGNAL: u8 = 18;
    /// Guest to host: the command is executing. It comes before any of its output; a
    /// command that could not start sends [`SYSTEM_ERR`] instead.
    pub const STARTED: u8 = 19;
    /// Host to guest, on the [`SIGNAL_PORT`](super::SIGNAL_PORT) connection: the size of
    /// a terminal workload's pty, as a [`Size`](super::Size) encodes it. The kernel sends
    /// the terminal's foreground process group SIGWINCH if it changed.
    pub const RESIZE: u8 = 20;
}

pub fn header(kind: u8, len: u32) -> [u8; HEADER] {
    let [a, b, c, d] = len.to_be_bytes();
    [kind, 0, 0, 0, a, b, c, d]
}

/// A header's kind and payload length, or `None` if it is malformed or too long.
pub fn parse_header(h: [u8; HEADER]) -> Option<(u8, u32)> {
    let [kind, z0, z1, z2, a, b, c, d] = h;
    let len = u32::from_be_bytes([a, b, c, d]);
    (z0 == 0 && z1 == 0 && z2 == 0 && len <= MAX_PAYLOAD).then_some((kind, len))
}

/// What to run, after the host has applied the image's and the command line's settings.
/// Strings are bytes, as Linux takes them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Spec {
    pub argv: Vec<Vec<u8>>,
    /// `KEY=value` entries.
    pub env: Vec<Vec<u8>>,
    /// The working directory, or empty for `/`.
    pub cwd: Vec<u8>,
    /// Docker's `--user`: `user` or `user:group`, each a name or a number; empty for root.
    pub user: Vec<u8>,
    pub hostname: Vec<u8>,
    /// Docker's `--tty`: the workload's stdio is a pty of this size, whose output reaches
    /// the host as [`kind::STDOUT`] alone; `None` for pipes.
    pub tty: Option<Size>,
}

/// A terminal's size in character cells. Zero in either leaves the pty's size alone, as
/// runc leaves it (libcontainer/utils_linux.go, setupConsole).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Size {
    pub rows: u16,
    pub cols: u16,
}

impl Size {
    /// Rows, then columns, each a big-endian u16.
    pub fn encode(self) -> [u8; 4] {
        let ([a, b], [c, d]) = (self.rows.to_be_bytes(), self.cols.to_be_bytes());
        [a, b, c, d]
    }

    pub fn decode(bytes: &[u8]) -> Option<Size> {
        let [a, b, c, d] = <[u8; 4]>::try_from(bytes).ok()?;
        Some(Size {
            rows: u16::from_be_bytes([a, b]),
            cols: u16::from_be_bytes([c, d]),
        })
    }
}

impl Spec {
    /// Lists are a big-endian u32 count, then their strings; each string is a big-endian
    /// u32 length, then its bytes. A terminal follows as a 1 and its size; without one,
    /// nothing follows, so an init from before terminals still reads the spec.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for list in [&self.argv, &self.env] {
            put(&mut out, list.len());
            for s in list {
                put_bytes(&mut out, s);
            }
        }
        for s in [&self.cwd, &self.user, &self.hostname] {
            put_bytes(&mut out, s);
        }
        if let Some(size) = self.tty {
            out.push(1);
            out.extend_from_slice(&size.encode());
        }
        out
    }

    /// The spec in `bytes`, or `None` unless they hold exactly one.
    pub fn decode(bytes: &[u8]) -> Option<Spec> {
        let mut r = Cursor(bytes);
        let spec = Spec {
            argv: r.list()?,
            env: r.list()?,
            cwd: r.bytes()?,
            user: r.bytes()?,
            hostname: r.bytes()?,
            tty: match r.take(1) {
                None => None,
                Some([1]) => Some(Size::decode(r.take(4)?)?),
                Some(_) => return None,
            },
        };
        r.0.is_empty().then_some(spec)
    }
}

fn put(out: &mut Vec<u8>, n: usize) {
    out.extend_from_slice(&u32::try_from(n).unwrap_or(u32::MAX).to_be_bytes());
}

fn put_bytes(out: &mut Vec<u8>, s: &[u8]) {
    put(out, s.len());
    out.extend_from_slice(s);
}

struct Cursor<'a>(&'a [u8]);

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let (head, rest) = self.0.split_at_checked(n)?;
        self.0 = rest;
        Some(head)
    }

    fn len(&mut self) -> Option<usize> {
        let b: [u8; 4] = self.take(4)?.try_into().ok()?;
        usize::try_from(u32::from_be_bytes(b)).ok()
    }

    fn bytes(&mut self) -> Option<Vec<u8>> {
        let n = self.len()?;
        self.take(n).map(<[u8]>::to_vec)
    }

    fn list(&mut self) -> Option<Vec<Vec<u8>>> {
        let n = self.len()?;
        // Each entry takes at least its 4-byte length: a count beyond that is a lie.
        if n > self.0.len() / 4 {
            return None;
        }
        (0..n).map(|_| self.bytes()).collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use alloc::vec;

    use super::*;

    #[test]
    fn specs_round_trip_and_bad_ones_are_refused() {
        let spec = Spec {
            argv: vec![b"/bin/sh".to_vec(), b"-c".to_vec(), b"echo \xff".to_vec()],
            env: vec![b"PATH=/bin".to_vec(), b"EMPTY=".to_vec()],
            cwd: b"/work".to_vec(),
            user: b"app:staff".to_vec(),
            hostname: b"box".to_vec(),
            tty: Some(Size { rows: 24, cols: 300 }),
        };
        let bytes = spec.encode();
        assert_eq!(Spec::decode(&bytes), Some(spec.clone()));
        let piped = Spec { tty: None, ..spec };
        let without = piped.encode();
        assert_eq!(Spec::decode(&without), Some(piped.clone()));
        assert_eq!(Spec::decode(&Spec::default().encode()), Some(Spec::default()));
        // Frames carry their length, so only a spec cut before its terminal reads as one
        // without.
        for cut in 0..bytes.len() {
            let expected = (cut == without.len()).then(|| piped.clone());
            assert_eq!(Spec::decode(&bytes[..cut]), expected, "cut at {cut}");
        }
        let mut long = bytes.clone();
        long.push(0);
        assert_eq!(Spec::decode(&long), None, "trailing bytes");
        assert_eq!(Spec::decode(&[0xff, 0xff, 0xff, 0xff]), None, "a huge count");
        let mut bad_tty = Spec::default().encode();
        bad_tty.push(2);
        assert_eq!(Spec::decode(&bad_tty), None, "not a terminal");
    }

    #[test]
    fn sizes_are_rows_then_columns() {
        let size = Size {
            rows: 0x0102,
            cols: 0x0304,
        };
        assert_eq!(size.encode(), [1, 2, 3, 4]);
        assert_eq!(Size::decode(&size.encode()), Some(size));
        assert_eq!(Size::decode(&[1, 2, 3]), None);
    }

    #[test]
    fn headers_are_stdcopy_headers() {
        assert_eq!(header(kind::STDERR, 0x0102_0304), [2, 0, 0, 0, 1, 2, 3, 4]);
        assert_eq!(parse_header([1, 0, 0, 0, 0, 0, 1, 0]), Some((kind::STDOUT, 256)));
        assert_eq!(parse_header([1, 0, 1, 0, 0, 0, 1, 0]), None);
        assert_eq!(parse_header(header(kind::STDOUT, MAX_PAYLOAD + 1)), None);
    }
}
