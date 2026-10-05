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
    // What it uses first, while its files are as they will be packed.
    if let Some(used) = crate::changes::upper().and_then(|u| usage(std::path::Path::new(&u)).ok()) {
        let mut c = conn;
        c.write_all(&run::header(kind::USAGE, 8))?;
        c.write_all(&used.to_be_bytes())?;
    }
    let mut out = Sender {
        conn,
        buf: Vec::with_capacity(CHUNK),
    };
    let packed = pack(&mut out);
    out.flush()?;
    // The end, whether or not all of it was sent: the host keeps only a whole one.
    send(conn, &[])?;
    packed
}

/// The disk the tree at `root` uses, as continuity's DiskUsage counts it (containerd's
/// snapshot Usage, which dockerd's SizeRw is): each inode's blocks once, directories and
/// `root` itself among them.
pub fn usage(root: &std::path::Path) -> io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    let mut seen = std::collections::HashSet::new();
    let mut total = 0u64;
    let mut stack = vec![(root.to_path_buf(), std::fs::metadata(root)?)];
    while let Some((path, meta)) = stack.pop() {
        if seen.insert((meta.dev(), meta.ino())) {
            total = total.saturating_add(meta.blocks().saturating_mul(512));
        }
        if meta.is_dir() {
            for e in std::fs::read_dir(&path)? {
                let e = e?;
                let m = std::fs::symlink_metadata(e.path())?;
                stack.push((e.path(), m));
            }
        }
    }
    Ok(total)
}

/// Writes the container's writable layer to `out`, as an OCI layer: what init made of
/// every container left out, and the directories that hold nothing else and are as the
/// image has them (Docker's init layer is beneath a container's, not in it).
pub fn pack(out: &mut impl Write) -> io::Result<()> {
    let upper = crate::changes::upper().ok_or_else(|| io::Error::other("the writable layer was not kept"))?;
    let mut exclude: Vec<Vec<u8>> = MADE.iter().map(|p| p.as_bytes().to_vec()).collect();
    exclude.extend(made_only(&upper).into_iter().map(String::into_bytes));
    let opts = shards_archive::PackOptions {
        exclude_patterns: exclude,
        whiteout: shards_archive::WhiteoutFormat::Overlay,
        ..Default::default()
    };
    // The kept descriptor's path is a link to a directory the root hides: entered, it is
    // the directory. Where init stands matters to nothing else (a built-in's own
    // process, or init after its workload).
    std::env::set_current_dir(&upper)?;
    shards_archive::pack(std::path::Path::new("."), &opts, out)
        .map(drop)
        .map_err(|e| io::Error::other(e.to_string()))
}

/// The directories of the writable layer `upper` that hold only what init made, and whose
/// mode and owner are the image's: copied up for init's files, not changed by the
/// container.
fn made_only(upper: &str) -> Vec<String> {
    use std::os::unix::fs::MetadataExt;
    let mut dirs: Vec<&str> = MADE
        .iter()
        .filter_map(|m| m.rsplit_once('/').map(|(d, _)| d))
        .collect();
    dirs.sort_unstable();
    dirs.dedup();
    dirs.into_iter()
        .filter(|dir| {
            let Ok(entries) = std::fs::read_dir(format!("{upper}/{dir}")) else {
                return false;
            };
            let only_made = entries.flatten().all(|e| {
                let name = format!("{dir}/{}", e.file_name().to_string_lossy());
                MADE.contains(&name.as_str())
            });
            let (Ok(here), Ok(image)) = (
                std::fs::symlink_metadata(format!("{upper}/{dir}")),
                std::fs::symlink_metadata(format!("/{dir}")),
            ) else {
                return false;
            };
            only_made && here.mode() == image.mode() && here.uid() == image.uid() && here.gid() == image.gid()
        })
        .map(str::to_string)
        .collect()
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
