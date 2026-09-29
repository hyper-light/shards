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

/// Faults in the `len` bytes at `ptr` writable, copying each page a private mapping
/// still shares with its file: the copies the first writes would make, made ahead
/// (madvise(2) MADV_POPULATE_WRITE, Linux 5.14: a write fault on each page, mm/madvise.c
/// madvise_populate).
#[cfg(target_os = "linux")]
pub fn populate_writable(ptr: *mut u8, len: usize) -> io::Result<()> {
    loop {
        // SAFETY: MADV_POPULATE_WRITE only faults pages in: every byte keeps its value,
        // and a range that is not all mapped is an error, not an access.
        if unsafe { libc::madvise(ptr.cast(), len, libc::MADV_POPULATE_WRITE) } == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// The pages of the `len` bytes at `ptr` that are mapped now, each as its index and
/// whether it is a private copy, written since it was mapped, rather than a page of the
/// mapped file: the present (63) and file-page (61) bits of /proc/self/pagemap, which
/// need no privilege, unlike its frame numbers (Documentation/admin-guide/mm/pagemap.rst).
#[cfg(target_os = "linux")]
pub fn mapped_pages(ptr: *const u8, len: usize) -> io::Result<Vec<(usize, bool)>> {
    use std::os::unix::fs::FileExt;
    const PRESENT: u64 = 1 << 63;
    const FILE_PAGE: u64 = 1 << 61;
    let page = page_size()?;
    if !(ptr as usize).is_multiple_of(page) || !len.is_multiple_of(page) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{ptr:p}+{len:#x} is not page-aligned"),
        ));
    }
    let (start, pages) = (ptr as usize / page, len / page);
    let pagemap = File::open("/proc/self/pagemap")?;
    let mut entries = vec![0u8; 8 << 13];
    let mut mapped = Vec::new();
    let mut done = 0;
    while done < pages {
        let n = (pages - done).min(entries.len() / 8);
        let chunk = entries
            .get_mut(..n * 8)
            .ok_or_else(|| io::Error::other("pagemap buffer"))?;
        pagemap.read_exact_at(chunk, (start + done) as u64 * 8)?;
        let (words, _) = chunk.as_chunks::<8>();
        for (i, word) in words.iter().enumerate() {
            let entry = u64::from_ne_bytes(*word);
            if entry & PRESENT != 0 {
                mapped.push((done + i, entry & FILE_PAGE == 0));
            }
        }
        done += n;
    }
    Ok(mapped)
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

/// The most one `read_at` or `write_at` asks for. XNU fails a read or write of more than
/// `INT_MAX` bytes with EINVAL (PM M35); Linux moves at most `MAX_RW_COUNT` and says so.
const RW_MAX: usize = if cfg!(target_vendor = "apple") {
    libc::c_int::MAX as usize
} else {
    isize::MAX as usize
};

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
        let n = unsafe { libc::pread(file.as_raw_fd(), dst.cast(), len.min(RW_MAX), offset) };
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
        let n = unsafe { libc::pwrite(file.as_raw_fd(), src.cast(), len.min(RW_MAX), offset) };
        if n >= 0 {
            return Ok(n.unsigned_abs());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Creates `name` in the directory `dir` holds open, durably: written to a temporary
/// sibling, synced, renamed over `name`, and the directory synced. It lands there even if
/// the directory was renamed since it was opened.
pub fn write_in(dir: &File, name: &str, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write as _;
    use std::os::fd::FromRawFd as _;
    let invalid = |_| io::Error::new(io::ErrorKind::InvalidInput, "a file name with NUL");
    let tmp = std::ffi::CString::new(format!(".{name}.tmp")).map_err(invalid)?;
    let target = std::ffi::CString::new(name).map_err(invalid)?;
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_CLOEXEC;
    // SAFETY: openat(2) with a NUL-terminated name, relative to a directory we hold open.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), tmp.as_ptr(), flags, 0o644 as libc::c_uint) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor we just opened, and nothing else owns.
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(bytes)?;
    sync_durable(&file)?;
    drop(file);
    // SAFETY: renameat(2) with NUL-terminated names, relative to a directory we hold open.
    if unsafe { libc::renameat(dir.as_raw_fd(), tmp.as_ptr(), dir.as_raw_fd(), target.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    dir.sync_all()
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
