use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::ptr::NonNull;

/// Makes `dir`, and its missing parents, readable by this user alone (0700). An existing
/// `dir` is made 0700 too, as containerd makes its root.
pub fn create_private_dir(dir: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

pub fn page_size() -> io::Result<usize> {
    // SAFETY: sysconf has no preconditions.
    let n = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    match usize::try_from(n) {
        Ok(p) if p.is_power_of_two() => Ok(p),
        _ => Err(io::Error::other(format!("sysconf(_SC_PAGESIZE) returned {n}"))),
    }
}

/// Reserves `len` bytes of zero-filled read/write memory. Pages are materialized on
/// first touch, so reserved-but-untouched guest RAM costs no host memory.
pub fn reserve(len: usize) -> io::Result<NonNull<u8>> {
    // SAFETY: fresh private anonymous mapping; the caller owns it until `release`.
    let p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    NonNull::new(p.cast()).ok_or_else(|| io::Error::other("mmap returned NULL"))
}

/// Reserves `len` bytes of guest RAM, like `reserve`. On Linux the mapping is aligned to
/// the transparent huge page size and advised `MADV_HUGEPAGE`, so the guest's first touch
/// of each huge page is one fault and KVM maps it with one stage-2 entry instead of 512.
/// Huge pages are the standard remedy for the cost of nested paging's two-dimensional
/// page walks (Bhargava et al., ASPLOS 2008; Gandhi et al., MICRO 2014).
pub fn reserve_ram(len: usize) -> io::Result<NonNull<u8>> {
    #[cfg(target_os = "linux")]
    if let Some(huge) = huge_page_size()
        && len >= huge
    {
        return reserve_huge(len, huge);
    }
    reserve(len)
}

/// The kernel's transparent huge page size, if it has transparent huge pages.
#[cfg(target_os = "linux")]
fn huge_page_size() -> Option<usize> {
    std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/hpage_pmd_size")
        .ok()?
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|size| size.is_power_of_two())
}

#[cfg(target_os = "linux")]
fn reserve_huge(len: usize, huge: usize) -> io::Result<NonNull<u8>> {
    let span = len
        .checked_add(huge)
        .ok_or_else(|| io::Error::other(format!("{len} bytes of guest RAM")))?;
    let base = reserve(span)?.as_ptr() as usize;
    let aligned = base.next_multiple_of(huge);
    let (head, tail) = (aligned - base, span - (aligned - base) - len);
    // SAFETY: both ranges lie inside the fresh `span` mapping and outside the aligned
    // `len` bytes this function returns.
    unsafe {
        if head > 0 {
            libc::munmap(base as *mut libc::c_void, head);
        }
        if tail > 0 {
            libc::munmap((aligned + len) as *mut libc::c_void, tail);
        }
    }
    // Advice only: without it (or with THP disabled) the memory is ordinary pages.
    // SAFETY: `aligned..aligned+len` is our own mapping.
    if unsafe { libc::madvise(aligned as *mut libc::c_void, len, libc::MADV_HUGEPAGE) } != 0 {
        crate::debug!("MADV_HUGEPAGE: {}", io::Error::last_os_error());
    }
    NonNull::new(aligned as *mut u8).ok_or_else(|| io::Error::other("mmap returned NULL"))
}

/// # Safety
/// `ptr..ptr+len` must be a mapping returned by `reserve` or `reserve_ram` and not used
/// afterwards.
pub unsafe fn release(ptr: NonNull<u8>, len: usize) {
    // SAFETY: forwarded caller contract.
    unsafe { libc::munmap(ptr.as_ptr().cast(), len) };
}

/// Replaces `len` bytes at `at` with a private, copy-on-write mapping of `file` from
/// `offset`. Pages come from the page cache on first touch; a write copies only the page
/// written.
///
/// # Safety
/// `at..at+len` must be page-aligned, inside a reservation from [`reserve`] that no
/// hypervisor maps and nothing references yet; `offset` must be page-aligned.
pub unsafe fn map_file_private(file: &File, offset: u64, len: usize, at: NonNull<u8>) -> io::Result<()> {
    let offset = file_offset(offset)?;
    // SAFETY: MAP_FIXED over memory the caller owns and nothing references.
    let p = unsafe {
        libc::mmap(
            at.as_ptr().cast(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_FIXED,
            file.as_raw_fd(),
            offset,
        )
    };
    if p == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    if p != at.as_ptr().cast() {
        return Err(io::Error::other("mmap(MAP_FIXED) placed the mapping elsewhere"));
    }
    Ok(())
}

/// Replaces `len` bytes at `at` with a read-only mapping of the start of `file`. It is
/// private, yet every VM mapping the same file shares one copy in the page cache: pages
/// are copied only when written, and nothing writes them (the host maps them read-only,
/// the guest's stage 2 too). Private, because HVF refuses shared read-only file mappings
/// (platform-measurements M17).
///
/// # Safety
/// `at..at+len` must be page-aligned, inside a reservation from [`reserve`] that no
/// hypervisor maps and nothing references yet.
pub unsafe fn map_file_readonly(file: &File, len: usize, at: NonNull<u8>) -> io::Result<()> {
    // SAFETY: MAP_FIXED over memory the caller owns and nothing references.
    let p = unsafe {
        libc::mmap(
            at.as_ptr().cast(),
            len,
            libc::PROT_READ,
            libc::MAP_PRIVATE | libc::MAP_FIXED,
            file.as_raw_fd(),
            0,
        )
    };
    if p == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    if p != at.as_ptr().cast() {
        return Err(io::Error::other("mmap(MAP_FIXED) placed the mapping elsewhere"));
    }
    Ok(())
}

/// Writes back `len` bytes at `ptr` from the data cache to memory, so a guest that maps
/// them as a device (non-cacheable) reads what the VMM wrote. On macOS: sys_dcache_flush
/// (libkern/OSCacheControl.h). Whether HVF's stage 2 already makes such accesses coherent
/// is unverified (ground-truth doc §5 row 25). Linux hosts need nothing: x86 is coherent,
/// and arm64 KVM forces write-back memory with FEAT_S2FWB.
///
/// # Safety
/// `ptr..ptr+len` must be mapped.
pub unsafe fn clean_dcache(ptr: *const u8, len: usize) {
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn sys_dcache_flush(start: *mut libc::c_void, len: usize);
        }
        // SAFETY: forwarded caller contract; cache maintenance does not change memory.
        unsafe { sys_dcache_flush(ptr.cast_mut().cast(), len) };
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (ptr, len);
}

/// Fills `buf` from the kernel CSPRNG, blocking only until it is first seeded.
#[cfg(target_os = "linux")]
pub fn fill_random(buf: &mut [u8]) -> io::Result<()> {
    let mut done = 0;
    while let Some(rest) = buf.get_mut(done..).filter(|r| !r.is_empty()) {
        // SAFETY: getrandom(2) writes at most `rest.len()` bytes into `rest`.
        let n = unsafe { libc::getrandom(rest.as_mut_ptr().cast(), rest.len(), 0) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
            continue;
        }
        done += n.unsigned_abs();
    }
    Ok(())
}

/// Fills `buf` from the kernel CSPRNG.
#[cfg(not(target_os = "linux"))]
pub fn fill_random(buf: &mut [u8]) -> io::Result<()> {
    // getentropy(2) returns at most 256 bytes per call.
    for chunk in buf.chunks_mut(256) {
        // SAFETY: writes exactly `chunk.len()` (≤ 256) bytes into `chunk`.
        if unsafe { libc::getentropy(chunk.as_mut_ptr().cast(), chunk.len()) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn file_offset(offset: u64) -> io::Result<libc::off_t> {
    libc::off_t::try_from(offset)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("file offset {offset:#x}")))
}

/// Reads up to `len` bytes at `offset` into raw memory, retrying on EINTR; 0 at end of file.
///
/// # Safety
/// `dst` must be valid for writes of `len` bytes for the duration of the call.
pub unsafe fn read_at(file: &File, dst: *mut u8, len: usize, offset: u64) -> io::Result<usize> {
    let offset = file_offset(offset)?;
    loop {
        // SAFETY: forwarded caller contract; the kernel writes into `dst`.
        let n = unsafe { libc::pread(file.as_raw_fd(), dst.cast(), len, offset) };
        if n >= 0 {
            return Ok(n.unsigned_abs());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Writes up to `len` bytes from raw memory at `offset`, retrying on EINTR.
///
/// # Safety
/// `src` must be valid for reads of `len` bytes for the duration of the call.
pub unsafe fn write_at(file: &File, src: *const u8, len: usize, offset: u64) -> io::Result<usize> {
    let offset = file_offset(offset)?;
    loop {
        // SAFETY: forwarded caller contract; the kernel reads from `src`.
        let n = unsafe { libc::pwrite(file.as_raw_fd(), src.cast(), len, offset) };
        if n >= 0 {
            return Ok(n.unsigned_abs());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Makes completed writes durable on stable storage. On macOS `fsync` does not flush the
/// drive's write cache; `F_FULLFSYNC` does (fsync(2), fcntl(2)).
pub fn sync_durable(file: &File) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: fcntl on an owned, open descriptor.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } == 0 {
            return Ok(());
        }
        // Filesystems without F_FULLFSYNC support fall back to fsync semantics.
    }
    file.sync_data()
}
