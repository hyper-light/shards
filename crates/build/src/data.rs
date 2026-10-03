//! Where a build's file contents are: layers' archives, files of the build context on the
//! host, and bytes the build made. Snapshots refer to them by [`DataRef`] and nothing is
//! copied until a layer or image is written.

use std::fs::File;
use std::io;
use std::path::PathBuf;

use shards_image::erofs::{DataRef, Source};

#[derive(Debug)]
enum Data {
    /// An uncompressed layer archive; a file's offset is where its data starts.
    Archive(File),
    /// A file of the build context, and the size it had when the context was read.
    Host(PathBuf, u64),
    Bytes(Vec<u8>),
}

/// Every source of file data a build has, by number.
#[derive(Debug, Default)]
pub struct Sources {
    list: Vec<Data>,
    /// The host file read last, kept open for the reads that follow.
    open: Option<(u32, File)>,
}

impl Sources {
    fn push(&mut self, d: Data) -> Result<u32, io::Error> {
        let id = u32::try_from(self.list.len())
            .ok()
            .filter(|&id| id != u32::MAX)
            .ok_or_else(|| io::Error::other("too many file sources"))?;
        self.list.push(d);
        Ok(id)
    }

    /// An archive whose files [`shards_image::layer::apply`] gives this source number.
    pub fn archive(&mut self, file: File) -> Result<u32, io::Error> {
        self.push(Data::Archive(file))
    }

    /// A host file of `size` bytes.
    pub fn host(&mut self, path: PathBuf, size: u64) -> Result<DataRef, io::Error> {
        Ok(DataRef {
            source: self.push(Data::Host(path, size))?,
            offset: 0,
        })
    }

    pub fn bytes(&mut self, data: Vec<u8>) -> Result<DataRef, io::Error> {
        Ok(DataRef {
            source: self.push(Data::Bytes(data))?,
            offset: 0,
        })
    }
}

impl Source for Sources {
    fn read_at(&mut self, data: DataRef, at: u64, buf: &mut [u8]) -> io::Result<()> {
        let pos = data
            .offset
            .checked_add(at)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "offset overflows"))?;
        let missing = || io::Error::new(io::ErrorKind::NotFound, "no such file source");
        match self.list.get_mut(data.source as usize).ok_or_else(missing)? {
            Data::Archive(f) => read_exact_at(f, buf, pos),
            Data::Bytes(b) => {
                let start = usize::try_from(pos).map_err(|_| missing())?;
                let src = b
                    .get(start..start.saturating_add(buf.len()))
                    .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "past the data"))?;
                buf.copy_from_slice(src);
                Ok(())
            }
            Data::Host(path, size) => {
                // A context file must still be what it was when the context was read.
                if pos.saturating_add(buf.len() as u64) > *size {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "past the file"));
                }
                if self.open.as_ref().map(|(id, _)| *id) != Some(data.source) {
                    let f = File::open(&*path)?;
                    if f.metadata()?.len() != *size {
                        return Err(io::Error::other(format!(
                            "{}: changed while the build read it",
                            path.display()
                        )));
                    }
                    self.open = Some((data.source, f));
                }
                let (_, f) = self.open.as_ref().ok_or_else(missing)?;
                read_exact_at(f, buf, pos)
            }
        }
    }
}

/// Fills `buf` from `file` at `pos` by positional reads: one call where a seek and a read
/// would be two.
#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], pos: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, pos)
}

/// Fills `buf` from `file` at `pos` by positional reads, which on Windows may read less
/// than asked.
#[cfg(windows)]
fn read_exact_at(file: &File, mut buf: &mut [u8], mut pos: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, pos) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "past the file")),
            Ok(n) => {
                buf = std::mem::take(&mut buf).get_mut(n..).unwrap_or_default();
                pos = pos.saturating_add(n as u64);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
