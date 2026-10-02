//! What a `RUN` step changed, as its builder guest reports the overlay's upper layer
//! (shards_abi::changes), put into the step's snapshot: the guest's kernel made every
//! change, and this records it, so the layer written of the snapshot afterwards is the one
//! BuildKit's differ writes of the same upper layer (crate::diff).
//!
//! The stream comes from the guest, and is read as untrusted: its decoder bounds every
//! length and path, and a path is taken literally, never through a symlink. A file's bytes
//! go to a staging file of the build's, read from there as any layer's are.

use std::fs::File;
use std::io::Write;

use shards_abi::changes::{self, Decoder, Event, flag, kind};
use shards_image::erofs::{DataRef, Kind, Meta, Node};

use crate::Error;
use crate::vfs::Fs;

/// Puts a step's changes into its snapshot as they arrive.
#[derive(Debug)]
pub struct Applier<'a> {
    fs: &'a mut Fs,
    decoder: Decoder,
    /// Where the changes' file bytes are kept, as source `source` of the build's.
    staging: &'a mut File,
    source: u32,
    /// How many bytes the staging file holds.
    at: u64,
    /// Each directory put, and its metadata: set again once all is in, since putting what a
    /// directory holds stamps its time.
    dirs: Vec<(Vec<u8>, Meta)>,
    /// Bytes of the current file still to come.
    pending: u64,
}

impl<'a> Applier<'a> {
    /// An applier into `fs`, keeping file bytes at the end of `staging`, whose bytes the
    /// build reads as source `source`.
    pub fn new(fs: &'a mut Fs, staging: &'a mut File, source: u32, at: u64) -> Applier<'a> {
        Applier {
            fs,
            decoder: Decoder::new(),
            staging,
            source,
            at,
            dirs: Vec::new(),
            pending: 0,
        }
    }

    /// Takes the next bytes of the stream.
    pub fn feed(&mut self, mut input: &[u8]) -> Result<(), Error> {
        let bad = |e: changes::Error| Error(format!("the step's changes: {e:?}"));
        loop {
            let (event, n) = self.decoder.next(input).map_err(bad)?;
            match event {
                Event::Entry(e) => self.entry(e)?,
                Event::Data(bytes) => {
                    self.staging
                        .write_all(bytes)
                        .map_err(|e| Error(format!("keeping the step's files: {e}")))?;
                    self.at = self.at.saturating_add(bytes.len() as u64);
                    self.pending = self.pending.saturating_sub(bytes.len() as u64);
                }
                Event::More => return Ok(()),
                Event::End => {
                    let rest = input.get(n..).unwrap_or_default();
                    // Anything after the end is refused, as the decoder refuses it.
                    return if rest.is_empty() {
                        Ok(())
                    } else {
                        self.decoder.next(rest).map(|_| ()).map_err(bad)
                    };
                }
            }
            input = input.get(n..).unwrap_or_default();
        }
    }

    /// Whether the stream has ended.
    pub fn ended(&self) -> bool {
        self.decoder.ended()
    }

    /// Ends the stream: it must have ended whole. Every directory then takes the time its
    /// step left it. How many bytes the staging file holds.
    pub fn finish(self) -> Result<u64, Error> {
        if !self.decoder.ended() || self.pending != 0 {
            return Err(Error("the step's changes ended early".into()));
        }
        for (path, meta) in self.dirs {
            self.fs.set_meta(&path, meta).map_err(|e| Error(e.to_string()))?;
        }
        Ok(self.at)
    }

    fn entry(&mut self, e: changes::Entry) -> Result<(), Error> {
        let fail = |e: crate::vfs::PathError| Error(e.to_string());
        let meta = meta(&e);
        let node = |kind: Kind| Node {
            kind,
            meta: meta.clone(),
        };
        match e.kind {
            kind::DIR => {
                self.fs
                    .put_dir(&e.path, meta.clone(), e.flags & flag::OPAQUE != 0)
                    .map_err(fail)?;
                self.dirs.push((e.path, meta));
            }
            kind::FILE => {
                let data = DataRef {
                    source: self.source,
                    offset: self.at,
                };
                self.fs
                    .put(&e.path, node(Kind::File { size: e.size, data }))
                    .map_err(fail)?;
                self.pending = e.size;
            }
            kind::SYMLINK => {
                self.fs
                    .put(&e.path, node(Kind::Symlink(e.target.into_boxed_slice())))
                    .map_err(fail)?;
            }
            kind::LINK => self.fs.put_link(&e.path, &e.target).map_err(fail)?,
            kind::CHAR => {
                let (major, minor) = (e.major, e.minor);
                self.fs
                    .put(&e.path, node(Kind::CharDevice { major, minor }))
                    .map_err(fail)?;
            }
            kind::BLOCK => {
                let (major, minor) = (e.major, e.minor);
                self.fs
                    .put(&e.path, node(Kind::BlockDevice { major, minor }))
                    .map_err(fail)?;
            }
            kind::FIFO => {
                self.fs.put(&e.path, node(Kind::Fifo)).map_err(fail)?;
            }
            kind::SOCKET => {
                self.fs.put(&e.path, node(Kind::Socket)).map_err(fail)?;
            }
            kind::WHITEOUT => self.fs.whiteout(&e.path).map_err(fail)?,
            other => return Err(Error(format!("the step's changes: an entry of kind {other}"))),
        }
        Ok(())
    }
}

/// An entry's metadata, less overlayfs's own xattrs, which the guest's kernel keeps for
/// itself and no layer holds.
fn meta(e: &changes::Entry) -> Meta {
    let mut m = Meta {
        mode: (e.mode & 0o7777) as u16,
        uid: e.uid,
        gid: e.gid,
        mtime: e.mtime,
        mtime_nsec: e.mtime_nsec,
        ..Meta::default()
    };
    for (name, value) in &e.xattrs {
        if !name.starts_with(b"trusted.overlay.") && !name.starts_with(b"user.overlay.") {
            m.xattrs.insert(name.clone(), value.clone());
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::Sources;
    use crate::diff::write_layer;
    use shards_image::erofs::Tree;
    use shards_image::tar;

    fn empty() -> Fs {
        Fs::new(
            Tree::new(Meta {
                mode: 0o755,
                ..Meta::default()
            }),
            (1_600_000_000, 0),
        )
    }

    fn entry(k: u8, path: &str) -> changes::Entry {
        changes::Entry {
            kind: k,
            mode: if k == kind::DIR { 0o755 } else { 0o644 },
            mtime: 1_700_000_000,
            mtime_nsec: 5,
            path: path.as_bytes().to_vec(),
            ..changes::Entry::default()
        }
    }

    /// A stream of `entries`, each file followed by its bytes.
    fn stream(entries: &[(changes::Entry, &[u8])]) -> Vec<u8> {
        let mut s = Vec::new();
        for (e, data) in entries {
            e.encode_into(&mut s);
            s.extend_from_slice(data);
        }
        s.push(changes::END);
        s
    }

    /// The upper layer overlayfs leaves of `chmod 600 /etc/a` (one name of a hard link
    /// below), `rm /gone`, `rm -rf /d && mkdir /d && touch /d/new`, and a new directory
    /// holding a file, a hard link to it and a symlink: put into the snapshot, then written
    /// as the layer BuildKit's differ writes of that upper layer.
    #[test]
    fn a_steps_upper_layer_becomes_its_snapshot_and_layer() {
        let mut sources = Sources::default();
        let x = sources.bytes(b"x".to_vec()).unwrap();
        let mut lower = empty();
        lower.mkdir(b"/etc", 0o755).unwrap();
        let a = lower.create(b"/etc/a", 0o644).unwrap();
        lower.set_data(a, 1, x);
        lower.link(b"/etc/a", b"/etc/b").unwrap();
        lower.mkdir(b"/d", 0o755).unwrap();
        lower.create(b"/d/old", 0o644).unwrap();
        lower.create(b"/gone", 0o644).unwrap();
        lower.begin();
        let mut upper = lower.clone();
        upper.begin();

        let dir = std::env::temp_dir().join(format!("shards-upper-{}", std::process::id()));
        let mut staging = File::options()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&dir)
            .unwrap();
        let source = sources.archive(staging.try_clone().unwrap()).unwrap();
        let a600 = changes::Entry {
            mode: 0o600,
            size: 1,
            ..entry(kind::FILE, "etc/a")
        };
        let d = changes::Entry {
            flags: flag::OPAQUE,
            ..entry(kind::DIR, "d")
        };
        let f = changes::Entry {
            size: 3,
            ..entry(kind::FILE, "n/f")
        };
        let g = changes::Entry {
            target: b"n/f".to_vec(),
            ..entry(kind::LINK, "n/g")
        };
        let s = changes::Entry {
            mode: 0o777,
            target: b"f".to_vec(),
            ..entry(kind::SYMLINK, "n/s")
        };
        let bytes = stream(&[
            (entry(kind::DIR, "etc"), b""),
            (a600, b"x"),
            (entry(kind::WHITEOUT, "gone"), b""),
            (d, b""),
            (
                changes::Entry {
                    size: 0,
                    ..entry(kind::FILE, "d/new")
                },
                b"",
            ),
            (entry(kind::DIR, "n"), b""),
            (f, b"hi\n"),
            (g, b""),
            (s, b""),
        ]);
        let mut applier = Applier::new(&mut upper, &mut staging, source, 0);
        // In pieces, as frames arrive.
        for piece in bytes.chunks(7) {
            applier.feed(piece).unwrap();
        }
        assert_eq!(applier.finish().unwrap(), 4);

        let mode = |fs: &Fs, p: &[u8]| fs.node(fs.lstat(p).unwrap()).unwrap().meta.mode;
        assert_eq!((mode(&upper, b"/etc/a"), mode(&upper, b"/etc/b")), (0o600, 0o644));
        assert_ne!(upper.lstat(b"/etc/a").unwrap(), upper.lstat(b"/etc/b").unwrap());
        assert!(upper.lstat(b"/gone").is_err());
        assert_eq!(upper.read_dir(b"/d").unwrap(), [b"new".to_vec()]);
        assert_eq!(upper.lstat(b"/n/f").unwrap(), upper.lstat(b"/n/g").unwrap());
        assert_eq!(upper.readlink(b"/n/s").unwrap(), b"f");
        // A directory's time is its step's, though what it holds came after it.
        let n = upper.node(upper.lstat(b"/n").unwrap()).unwrap();
        assert_eq!((n.meta.mtime, n.meta.mtime_nsec), (1_700_000_000, 5));

        let mut out = Vec::new();
        write_layer(&lower, &upper, &mut sources, &mut out).unwrap();
        let mut r = tar::Reader::raw(&out[..]);
        let mut names = Vec::new();
        while let Some(e) = r.next_entry().unwrap() {
            names.push(String::from_utf8(e.path).unwrap());
        }
        assert_eq!(
            names,
            // BuildKit's differ writes a whiteout for each name a directory made again no
            // longer holds, not an opaque marker (crate::diff, held to BuildKit's output).
            [
                "d/",
                "d/new",
                "d/.wh.old",
                "etc/",
                "etc/a",
                ".wh.gone",
                "n/",
                "n/f",
                "n/g",
                "n/s"
            ]
        );
        let _ = std::fs::remove_file(&dir);
    }

    /// A stream that stops short, or names a path through a file, changes nothing it
    /// should not and fails.
    #[test]
    fn a_stream_cut_short_or_bent_fails() {
        let mut fs = empty();
        fs.create(b"/file", 0o644).unwrap();
        fs.begin();
        let path = std::env::temp_dir().join(format!("shards-upper-bad-{}", std::process::id()));
        let mut staging = File::create(&path).unwrap();
        let mut bytes = stream(&[(
            changes::Entry {
                size: 5,
                ..entry(kind::FILE, "a")
            },
            b"hello",
        )]);
        bytes.truncate(bytes.len() - 3);
        let mut applier = Applier::new(&mut fs, &mut staging, 0, 0);
        applier.feed(&bytes).unwrap();
        assert!(applier.finish().is_err());
        let bytes = stream(&[(entry(kind::FILE, "file/below"), b"")]);
        let mut applier = Applier::new(&mut fs, &mut staging, 0, 0);
        assert!(applier.feed(&bytes).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
