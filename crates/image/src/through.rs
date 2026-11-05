//! Where a process's writes of the files a build makes go (D188): by default, written by
//! this process; once a writer is set, handed to it, which writes them to the same open
//! file, at the offset the file's description has, as this process would have. A build held
//! to a budget hands them to a process of its own outside the budget's cgroup, so that the
//! page cache they dirty is charged there, not to the budget, whose dirty limit and
//! `memory.high` otherwise pace them to the disk's speed (mm/page-writeback.c,
//! `balance_dirty_pages`; PM M442).

use std::fs::File;
use std::io::{self, Write};
use std::sync::OnceLock;

/// What writes a file's bytes for this process.
pub trait Writer: Send + Sync {
    /// Writes some of `buf` to `file` at its description's offset, as write(2) does: how
    /// many bytes were written.
    fn write(&self, file: &File, buf: &[u8]) -> io::Result<usize>;
}

static WRITER: OnceLock<Box<dyn Writer>> = OnceLock::new();

/// The bytes a buffered writer of a build's files gathers before it hands them on: as many
/// as a writer outside the budget takes in one message (`shards_ipc::MAX_PAYLOAD`, which
/// build/outside.rs holds it to), so that each of its round trips carries a whole one, not
/// a file's few bytes.
pub const CHUNK: usize = 1 << 20;

/// Hands this process's writes to `writer` from here on: once, for the process's life.
/// Whether it was set (another was, before).
pub fn set(writer: Box<dyn Writer>) -> bool {
    WRITER.set(writer).is_ok()
}

/// Writes some of `buf` to `file`, through the writer set, or else here.
pub fn write(file: &File, buf: &[u8]) -> io::Result<usize> {
    match WRITER.get() {
        Some(w) => w.write(file, buf),
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

/// A file whose writes go [`write`]'s way, for a buffered writer of it.
#[derive(Debug)]
pub struct Through(pub File);

impl Write for Through {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        write(&self.0, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl io::Seek for Through {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        io::Seek::seek(&mut self.0, pos)
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
        let mut through = Through(f.try_clone().unwrap());
        through.write_all(b"world").unwrap();
        through.seek(io::SeekFrom::Start(0)).unwrap();
        through.write_all(b"J").unwrap();
        let mut back = String::new();
        f.seek(io::SeekFrom::Start(0)).unwrap();
        f.read_to_string(&mut back).unwrap();
        assert_eq!(back, "Jello, world");
    }
}
