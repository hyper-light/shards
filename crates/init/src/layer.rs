//! A container's writable layer across the microVMs it runs in (docs/design/
//! architecture.md D37): saved as the container stops, as the OCI layer of its changes
//! (go-archive's tar of an overlay upper directory, whiteouts made `.wh.` entries), and
//! put back over a fresh image root before its command runs again (go-archive's
//! ApplyLayer), as dockerd keeps a stopped container's writable layer for `docker start`.
//! Both travel as [`kind::LAYER`] frames on the workload's connection.

use std::fs::File;
use std::io::{self, Read, Write};

use shards_abi::run::{self, CHUNK, kind};

/// What init makes of every container, which is not its change: its mounts and its
/// files in `/etc`, as Docker's init layer holds them (changes.rs, MADE).
const MADE: [&str; 7] = [
    "proc",
    "sys",
    "dev",
    "etc/hosts",
    "etc/hostname",
    "etc/resolv.conf",
    "etc/mtab",
];

/// Applies the layer that arrives as [`kind::LAYER`] frames on `conn`, the first of
/// which said it is `first` bytes long, over the root.
pub fn apply(conn: &File, first: u32) -> io::Result<()> {
    let mut frames = Frames {
        conn,
        buf: Vec::new(),
        at: 0,
        next: Some(first),
        ended: false,
    };
    shards_archive::apply_layer(
        &mut frames,
        std::path::Path::new("/"),
        &shards_archive::UnpackOptions::default(),
    )
    .map(drop)
    .map_err(|e| io::Error::other(e.to_string()))?;
    // What the archive did not read of its frames, to their end.
    io::copy(&mut frames, &mut io::sink()).map(drop)
}

/// Sends the container's writable layer on `conn` as [`kind::LAYER`] frames, then an
/// empty one.
pub fn save(conn: &File) -> io::Result<()> {
    let upper = crate::changes::upper().ok_or_else(|| io::Error::other("the writable layer was not kept"))?;
    let opts = shards_archive::PackOptions {
        exclude_patterns: MADE.iter().map(|p| p.as_bytes().to_vec()).collect(),
        whiteout: shards_archive::WhiteoutFormat::Overlay,
        ..Default::default()
    };
    // The kept descriptor's path is a link to a directory the root hides: entered, it is
    // the directory. Init's work is done by now, so where it stands matters to nothing.
    std::env::set_current_dir(&upper)?;
    let mut out = Sender {
        conn,
        buf: Vec::with_capacity(CHUNK),
    };
    let packed = shards_archive::pack(std::path::Path::new("."), &opts, &mut out)
        .map(drop)
        .map_err(|e| io::Error::other(e.to_string()));
    out.flush()?;
    // The end, whether or not all of it was sent: the host keeps only a whole one.
    send(conn, &[])?;
    packed
}

/// The layer's bytes from its frames.
struct Frames<'a> {
    conn: &'a File,
    buf: Vec<u8>,
    at: usize,
    /// The length of a frame whose header was read and whose payload was not.
    next: Option<u32>,
    ended: bool,
}

impl Read for Frames<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.at >= self.buf.len() {
            if self.ended {
                return Ok(0);
            }
            let len = match self.next.take() {
                Some(len) => len,
                None => {
                    let mut h = [0u8; run::HEADER];
                    (&*self.conn).read_exact(&mut h)?;
                    match run::parse_header(h) {
                        Some((kind::LAYER, len)) => len,
                        _ => return Err(io::Error::other("a layer broken off by another frame")),
                    }
                }
            };
            if len == 0 {
                self.ended = true;
                return Ok(0);
            }
            self.buf.resize(len as usize, 0);
            (&*self.conn).read_exact(&mut self.buf)?;
            self.at = 0;
        }
        let rest = self.buf.get(self.at..).unwrap_or_default();
        let n = rest.len().min(out.len());
        out.get_mut(..n)
            .unwrap_or_default()
            .copy_from_slice(rest.get(..n).unwrap_or_default());
        self.at += n;
        Ok(n)
    }
}

/// The layer's bytes into frames of at most [`CHUNK`] bytes.
struct Sender<'a> {
    conn: &'a File,
    buf: Vec<u8>,
}

impl Write for Sender<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let room = CHUNK.saturating_sub(self.buf.len());
        let n = room.min(bytes.len());
        self.buf.extend_from_slice(bytes.get(..n).unwrap_or_default());
        if self.buf.len() >= CHUNK {
            self.flush()?;
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            send(self.conn, &self.buf)?;
            self.buf.clear();
        }
        Ok(())
    }
}

/// One [`kind::LAYER`] frame of `bytes`.
fn send(conn: &File, bytes: &[u8]) -> io::Result<()> {
    let len = u32::try_from(bytes.len()).map_err(|_| io::Error::other("a frame past its size"))?;
    let mut c = conn;
    c.write_all(&run::header(kind::LAYER, len))?;
    c.write_all(bytes)
}
