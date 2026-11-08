//! Where a process's writes of the files a build makes go (D188): by default, written by
//! this process; once a writer is set, handed to it, which writes them to the same open
//! file, at the offset the file's description has, as this process would have. A build held
//! to a budget hands them to a process of its own outside the budget's cgroup, so that the
//! page cache they dirty is charged there, not to the budget, whose dirty limit and
//! `memory.high` otherwise pace them to the disk's speed (mm/page-writeback.c,
//! `balance_dirty_pages`; PM M442).
//!
//! A [`Through`] hands its file's writes on and goes on while the writer makes them, as
//! write(2) goes on while the kernel writes the page cache back: each write is made in
//! order, after every one handed before it, and what became of them is told when the file
//! is next used otherwise, flushed, sought in or taken back ([`Through::into_file`]), as a
//! buffered writer tells its own. So a build's work and its writer's overlap, where waiting
//! for each write made the writer's every write the build's time too (PM M442, rust:1-slim's
//! export 0.22 to 0.29 s slower held on each of CI's four runners). [`write`] waits for its
//! own.

use std::fs::File;
use std::io::{self, Write};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// Whose writes a writer is handed: a [`Through`]'s, or one [`write`]'s. Its writes are made
/// in the order they are handed, and settled together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Owner(u64);

impl Owner {
    /// One no other has been.
    pub fn unique() -> Owner {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Owner(NEXT.fetch_add(1, Ordering::Relaxed))
    }

    /// Its number, for a writer to tell owners apart by.
    pub fn id(self) -> u64 {
        self.0
    }
}

/// What writes a file's bytes for this process.
pub trait Writer: Send + Sync {
    /// Takes some of `buf` to write to `file` at its description's offset, after every
    /// write handed before it: how many bytes it took. It may return before they are
    /// written; a write of `owner`'s that failed is told here, or by [`Writer::settle`].
    fn hand(&self, owner: Owner, file: &File, buf: &[u8]) -> io::Result<usize>;

    /// Waits until every write handed for `owner` is made: why one failed, if one did.
    fn settle(&self, owner: Owner) -> io::Result<()>;
}

static WRITER: OnceLock<Box<dyn Writer>> = OnceLock::new();

/// The bytes a buffered writer of a build's files gathers before it hands them on, and the
/// most a writer is handed at once: what the EROFS writer reads of a file at a time
/// (shards_erofs `write_with`'s buffer), so that an image's files reach the writer whole
/// chunks at a time, not each file's few bytes.
pub const CHUNK: usize = 1 << 20;

/// Hands this process's writes to `writer` from here on: once, for the process's life.
/// Whether it was set (another was, before).
pub fn set(writer: Box<dyn Writer>) -> bool {
    WRITER.set(writer).is_ok()
}

/// Writes some of `buf` to `file`, through the writer set, or else here, and returns once
/// they are written.
pub fn write(file: &File, buf: &[u8]) -> io::Result<usize> {
    match WRITER.get() {
        Some(w) => {
            let owner = Owner::unique();
            let handed = w.hand(owner, file, buf);
            let settled = w.settle(owner);
            let n = handed?;
            settled.map(|()| n)
        }
        None => {
            let mut here: &File = file;
            here.write(buf)
        }
    }
}

/// Writes all of `buf` to `file` ([`write`]).
pub fn write_all(file: &File, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        match write(file, buf) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => buf = buf.get(n..).unwrap_or_default(),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// A file whose writes go [`write`]'s way, for a buffered writer of it, handed on without
/// waiting for them: they are settled before it is flushed, sought in, taken back or
/// dropped, so that whatever is done with the file next finds them made.
#[derive(Debug)]
pub struct Through {
    file: Option<File>,
    owner: Owner,
}

impl Through {
    /// `file`, its writes to go [`write`]'s way from here on.
    pub fn new(file: File) -> Through {
        Through {
            file: Some(file),
            owner: Owner::unique(),
        }
    }

    /// Waits for every write handed on: why one failed, if one did.
    fn settle(&self) -> io::Result<()> {
        match WRITER.get() {
            Some(w) => w.settle(self.owner),
            None => Ok(()),
        }
    }

    fn file(&self) -> io::Result<&File> {
        self.file
            .as_ref()
            .ok_or_else(|| io::Error::other("a file already taken back"))
    }

    /// The file, once every write handed on is made: why one failed, if one did.
    pub fn into_file(mut self) -> io::Result<File> {
        self.settle()?;
        self.file
            .take()
            .ok_or_else(|| io::Error::other("a file already taken back"))
    }
}

impl Write for Through {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let file = self.file()?;
        match WRITER.get() {
            Some(w) => w.hand(self.owner, file, buf),
            None => {
                let mut here: &File = file;
                here.write(buf)
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.settle()
    }
}

impl io::Seek for Through {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        // The offset its writes are made at is the description's.
        self.settle()?;
        let mut file: &File = self.file()?;
        io::Seek::seek(&mut file, pos)
    }
}

impl Drop for Through {
    fn drop(&mut self) {
        // Made before the file is closed here and used elsewhere (a clone of it, its name);
        // dropped unflushed, as a buffered writer dropped, why one failed goes unsaid.
        if self.file.is_some() {
            let _ = self.settle();
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::{Read as _, Seek as _};

    /// With no writer set, a file is written here, at its offset, as write(2) writes it.
    #[test]
    fn with_no_writer_a_file_is_written_here() {
        let dir = shards_testdir::TempDir::new("through").unwrap();
        let path = dir.path().join("f");
        let mut f = File::options()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        write_all(&f, b"hello, ").unwrap();
        let mut through = Through::new(f.try_clone().unwrap());
        through.write_all(b"world").unwrap();
        through.seek(io::SeekFrom::Start(0)).unwrap();
        through.write_all(b"J").unwrap();
        through.into_file().unwrap();
        let mut back = String::new();
        f.seek(io::SeekFrom::Start(0)).unwrap();
        f.read_to_string(&mut back).unwrap();
        assert_eq!(back, "Jello, world");
    }
}
