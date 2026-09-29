//! Host-OS services behind one API: guest-memory reservation, positional I/O into guest
//! memory, durable flushes, entropy, the console, and vCPU thread policy. The rest of the
//! VMM never calls the OS directly.

use std::fs::File;
use std::io;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(target_os = "macos")]
mod macos_thread;

/// Applies the scheduling policy measured to give vCPUs precise timer wake-ups. Only
/// macOS has a measured policy (docs/research/platform-measurements.md M8, M10); on other
/// hosts this is a no-op until one is measured.
pub fn prioritize_vcpu_thread() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    return macos_thread::make_current_realtime();
    #[cfg(not(target_os = "macos"))]
    Ok(())
}

/// The bytes of `name` in the directory `dir` holds open, or `None` if there is no such
/// file. A file longer than `max` bytes is an error, found before it is read.
pub fn read_in(dir: &File, name: &str, max: u64) -> io::Result<Option<Vec<u8>>> {
    let file = match open_in(dir, name) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let len = file.metadata()?.len();
    if len > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{name}: {len} bytes, past the limit of {max}"),
        ));
    }
    let len = usize::try_from(len).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len)
        .map_err(|e| io::Error::new(io::ErrorKind::OutOfMemory, e.to_string()))?;
    bytes.resize(len, 0);
    read_exact_at(&file, &mut bytes, 0)?;
    Ok(Some(bytes))
}

/// Makes the entries of the directory at `path` durable: the renames into it survive a
/// crash once this returns.
pub fn sync_dir(path: &std::path::Path) -> io::Result<()> {
    #[cfg(unix)]
    return File::open(path)?.sync_all();
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Fills `buf` from `file` at `offset`, failing on a short file.
pub fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    let mut done = 0;
    while let Some(rest) = buf.get_mut(done..).filter(|r| !r.is_empty()) {
        let at = offset
            .checked_add(done as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file offset overflows"))?;
        // SAFETY: `rest` is a live, exclusively borrowed buffer of `rest.len()` bytes.
        let n = unsafe { read_at(file, rest.as_mut_ptr(), rest.len(), at)? };
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        done += n;
    }
    Ok(())
}

/// An unbuffered handle to this process's standard output, independent of `std::io::stdout`'s
/// lock and line buffering.
pub fn stdout_file() -> io::Result<File> {
    #[cfg(unix)]
    let owned = std::os::fd::AsFd::as_fd(&io::stdout()).try_clone_to_owned()?;
    #[cfg(windows)]
    let owned = std::os::windows::io::AsHandle::as_handle(&io::stdout()).try_clone_to_owned()?;
    Ok(File::from(owned))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_file(tag: &str, contents: &[u8]) -> (std::path::PathBuf, File) {
        let path = std::env::temp_dir().join(format!("shards-platform-{tag}-{}", std::process::id()));
        std::fs::File::create(&path).unwrap().write_all(contents).unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        (path, file)
    }

    #[test]
    fn reservations_are_zeroed_writable_and_page_aligned() {
        let page = page_size().unwrap();
        let len = 64 * page;
        let p = reserve(len).unwrap();
        assert!((p.as_ptr() as usize).is_multiple_of(page));
        // SAFETY: `p` is a fresh reservation of `len` bytes, released below.
        unsafe {
            let s = std::slice::from_raw_parts_mut(p.as_ptr(), len);
            assert!(s.iter().all(|&b| b == 0));
            s[0] = 1;
            s[len - 1] = 2;
            assert_eq!((s[0], s[len - 1]), (1, 2));
            release(p, len);
        }
    }

    #[test]
    fn positional_io_round_trips_and_reports_end_of_file() {
        let (path, file) = temp_file("io", b"0123456789");
        let mut buf = [0u8; 4];
        // SAFETY: `buf` is a live 4-byte buffer.
        assert_eq!(unsafe { read_at(&file, buf.as_mut_ptr(), 4, 3) }.unwrap(), 4);
        assert_eq!(&buf, b"3456");
        // SAFETY: as above; reading at end of file returns 0 bytes.
        assert_eq!(unsafe { read_at(&file, buf.as_mut_ptr(), 4, 10) }.unwrap(), 0);
        // SAFETY: the source is a live 3-byte buffer; writing past the end extends the file.
        assert_eq!(unsafe { write_at(&file, b"abc".as_ptr(), 3, 12) }.unwrap(), 3);
        sync_durable(&file).unwrap();
        let mut whole = [0u8; 15];
        read_exact_at(&file, &mut whole, 0).unwrap();
        assert_eq!(&whole, b"0123456789\0\0abc");
        let short = read_exact_at(&file, &mut [0u8; 4], 13).unwrap_err();
        assert_eq!(short.kind(), io::ErrorKind::UnexpectedEof);
        drop(file);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn random_bytes_differ_between_calls() {
        // 600 bytes spans several getentropy(2) chunks.
        let (mut a, mut b) = ([0u8; 600], [0u8; 600]);
        fill_random(&mut a).unwrap();
        fill_random(&mut b).unwrap();
        assert_ne!(a, b);
        assert!(a[256..].iter().any(|&x| x != 0));
    }

    #[test]
    fn stdout_handle_is_writable() {
        let mut out = stdout_file().unwrap();
        out.write_all(b"").unwrap();
    }
}
