//! TEST ONLY: opened-file preferred-I/O initial reserve, not an adopted reader.
#[cfg(not(target_os = "linux"))]
compile_error!("this experiment uses Linux file metadata semantics");

use std::fs::File;
use std::io::{self, Read as _};
use std::os::unix::fs::MetadataExt as _;

fn initial_buffer(preferred: u64) -> io::Result<Vec<u8>> {
    let preferred = usize::try_from(preferred).map_err(|_| io::ErrorKind::InvalidData)?;
    if preferred == 0 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(preferred)
        .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
    Ok(bytes)
}

/// Experimental supported inputs require a positive, usize-representable I/O hint.
/// Query this held File, with no pathname reopen, fallback, cap or cached hint.
pub fn read_preferred(mut file: File) -> io::Result<Vec<u8>> {
    let mut bytes = initial_buffer(file.metadata()?.blksize())?;
    loop {
        // Existing f26 returning reader: one further byte to observe data or EOF.
        bytes
            .try_reserve(1)
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        let used = bytes.len();
        // Current capacity is already owned; resize cannot allocate beyond it.
        bytes.resize(bytes.capacity(), 0);
        let count = loop {
            match file.read(bytes.get_mut(used..).ok_or(io::ErrorKind::InvalidData)?) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                result => break result?,
            }
        };
        let filled = used
            .checked_add(count)
            .filter(|filled| *filled <= bytes.len())
            .ok_or(io::ErrorKind::InvalidData)?;
        bytes.truncate(filled);
        if count == 0 {
            return Ok(bytes);
        }
    }
}

#[cfg(test)]
#[path = "../../../managed-borrowed/allocation.rs"]
mod allocation;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_bytes_from_open_file_after_unlink() {
        let dir = shards_testdir::TempDir::new("preferred-bytes").unwrap();
        let path = dir.join("input");
        let content = b"\0\xffown ) )\xfe\n";
        std::fs::write(&path, content).unwrap();
        let file = File::open(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(read_preferred(file).unwrap(), content);
    }

    #[test]
    fn empty_file_returns_empty_and_directory_returns_error() {
        let dir = shards_testdir::TempDir::new("preferred-empty").unwrap();
        let path = dir.join("empty");
        std::fs::write(&path, b"").unwrap();
        assert!(read_preferred(File::open(&path).unwrap()).unwrap().is_empty());
        assert_eq!(
            read_preferred(File::open(&dir).unwrap()).unwrap_err().kind(),
            io::ErrorKind::IsADirectory
        );
    }

    #[test]
    fn zero_and_capacity_overflow_are_returned_without_error_allocation() {
        let zero = allocation::fail_after(0, || initial_buffer(0));
        assert_eq!(zero.unwrap_err().kind(), io::ErrorKind::InvalidData);
        let huge = allocation::fail_after(0, || initial_buffer(u64::MAX));
        assert_eq!(
            huge.unwrap_err().kind(),
            if usize::BITS < u64::BITS {
                io::ErrorKind::InvalidData
            } else {
                io::ErrorKind::OutOfMemory
            }
        );
    }

    #[test]
    fn initial_and_later_growth_oom_return_errors() {
        let dir = shards_testdir::TempDir::new("preferred-oom").unwrap();
        let path = dir.join("growth");
        std::fs::write(&path, b"").unwrap();
        let preferred = File::open(&path).unwrap().metadata().unwrap().blksize();
        // Fixture input exceeds the initial capacity requested from this same file's
        // actual hint; no measured stat length/page/default is copied into the method.
        let length = usize::try_from(preferred).unwrap().checked_add(1).unwrap();
        let mut content = Vec::new();
        content.try_reserve_exact(length).unwrap();
        content.resize(length, 0xff);
        std::fs::write(&path, &content).unwrap();
        assert_eq!(read_preferred(File::open(&path).unwrap()).unwrap(), content);
        for successful in [0, 1] {
            let file = File::open(&path).unwrap();
            let result = allocation::fail_after(successful, || read_preferred(file));
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::OutOfMemory);
            println!(
                "successful_allocations_before_refusal={successful} actual_reader_returned_OutOfMemory=true"
            );
        }
    }
}
