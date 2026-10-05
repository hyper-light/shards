use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::ptr::NonNull;

/// The filesystem holding `path`: the bytes this user may still write there, and its size
/// (statvfs(3); what a filesystem keeps for root is not available).
pub fn disk_space(path: &std::path::Path) -> io::Result<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path with NUL"))?;
    // SAFETY: statvfs is plain data, for which all zeroes is a value.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: statvfs(3) with a NUL-terminated path, into a local it fills.
    if unsafe { libc::statvfs(path.as_ptr(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    #[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
    let (frsize, avail, blocks) = (
        u64::from(st.f_frsize),
        u64::from(st.f_bavail),
        u64::from(st.f_blocks),
    );
    Ok((avail.saturating_mul(frsize), blocks.saturating_mul(frsize)))
}

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

/// MAP_NORESERVE, but for Miri, which checks the memory tests' accesses
/// (docs/research/measurements/access-guard): it maps only plain private anonymous
/// memory, and has no swap to reserve.
const fn noreserve() -> libc::c_int {
    if cfg!(miri) { 0 } else { libc::MAP_NORESERVE }
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
            libc::MAP_PRIVATE | libc::MAP_ANON | noreserve(),
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
    #[cfg(all(target_os = "linux", not(miri)))]
    if let Some(huge) = huge_page_size()
        && len >= huge
    {
        return reserve_huge(len, huge);
    }
    reserve(len)
}

/// The kernel's transparent huge page size, if it has transparent huge pages.
#[cfg(all(target_os = "linux", not(miri)))]
fn huge_page_size() -> Option<usize> {
    std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/hpage_pmd_size")
        .ok()?
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|size| size.is_power_of_two())
}

#[cfg(all(target_os = "linux", not(miri)))]
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

/// Which of the `len / page` pages at `ptr`, anonymous private memory, the process has
/// never touched: neither resident nor paged out, so reading one would get zeros, the
/// first touch having never happened. Where the OS cannot say, `None`.
///
/// macOS: `mach_vm_page_range_query`, each page's disposition (osfmk/mach/vm_statistics.h
/// `VM_PAGE_QUERY_*`). Linux: /proc/self/pagemap, each page's present and swapped bits
/// (Documentation/admin-guide/mm/pagemap.rst); a page read but never written maps the
/// shared zero page, and is read, as are any the kernel says are there.
///
/// # Safety
/// `ptr..ptr+len` must be memory this process owns, `page`-aligned.
pub unsafe fn untouched(ptr: *const u8, len: usize, page: usize) -> io::Result<Option<Vec<bool>>> {
    let pages = len / page;
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn mach_vm_page_range_query(
                target_map: libc::vm_map_t,
                address: u64,
                size: u64,
                dispositions: u64,
                dispositions_count: *mut u64,
            ) -> libc::kern_return_t;
            /// This task's port, which the SDK's `mach_task_self()` reads (mach/mach_init.h).
            static mach_task_self_: libc::mach_port_t;
            fn mach_vm_region(
                target_task: libc::vm_map_t,
                address: *mut u64,
                size: *mut u64,
                flavor: i32,
                info: *mut ExtendedInfo,
                count: *mut u32,
                object_name: *mut libc::mach_port_t,
            ) -> libc::kern_return_t;
        }
        /// `struct vm_region_extended_info` (mach/vm_region.h), flavor 13.
        #[repr(C)]
        #[derive(Default)]
        struct ExtendedInfo {
            protection: i32,
            user_tag: u32,
            pages_resident: u32,
            pages_shared_now_private: u32,
            pages_swapped_out: u32,
            pages_dirtied: u32,
            ref_count: u32,
            shadow_depth: u16,
            external_pager: u8,
            share_mode: u8,
            pages_reusable: u32,
        }
        // Whole-entry counts first: a range each of whose map entries has every page
        // resident or paged out has none untouched, and asking page by page would cost a
        // fully used RAM its save time (PM M54). XNU splits a large anonymous mapping into
        // entries of 128 MiB; each is asked in turn.
        let end = ptr as u64 + len as u64;
        let mut at = ptr as u64;
        let all_used = loop {
            if at >= end {
                break true;
            }
            let (mut address, mut size) = (at, 0u64);
            let mut info = ExtendedInfo::default();
            let mut count = (std::mem::size_of::<ExtendedInfo>() / 4) as u32;
            let mut object = 0;
            // SAFETY: mach_vm_region(2) filling `info` for the entry at or after `address`.
            let kr = unsafe {
                mach_vm_region(
                    mach_task_self_,
                    &mut address,
                    &mut size,
                    13,
                    &mut info,
                    &mut count,
                    &mut object,
                )
            };
            let inside = address == at && address.saturating_add(size) <= end;
            let used = u64::from(info.pages_resident) + u64::from(info.pages_swapped_out);
            if kr != 0 || !inside || size == 0 || used < size / page as u64 {
                break false;
            }
            at = address + size;
        };
        if all_used {
            return Ok(None);
        }
        let mut dispositions = vec![0i32; pages];
        let mut done = 0usize;
        while done < pages {
            let mut count = (pages - done) as u64;
            // SAFETY: a query of our own task's pages into the rest of `dispositions`, as
            // many as it has room for.
            let kr = unsafe {
                mach_vm_page_range_query(
                    mach_task_self_,
                    ptr as u64 + (done * page) as u64,
                    ((pages - done) * page) as u64,
                    dispositions.as_mut_ptr().add(done) as u64,
                    &mut count,
                )
            };
            if kr != 0 {
                return Err(io::Error::other(format!("mach_vm_page_range_query: {kr}")));
            }
            if count == 0 {
                return Ok(None);
            }
            done += count as usize;
        }
        let seen = libc::VM_PAGE_QUERY_PAGE_PRESENT | libc::VM_PAGE_QUERY_PAGE_PAGED_OUT;
        Ok(Some(dispositions.iter().map(|d| d & seen == 0).collect()))
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::FileExt;
        const PRESENT: u64 = 1 << 63;
        const SWAPPED: u64 = 1 << 62;
        let pagemap = File::open("/proc/self/pagemap")?;
        let mut bytes = vec![0u8; pages * 8];
        let first = (ptr as usize / page) as u64 * 8;
        pagemap.read_exact_at(&mut bytes, first)?;
        Ok(Some(
            bytes
                .as_chunks::<8>()
                .0
                .iter()
                .map(|e| u64::from_ne_bytes(*e) & (PRESENT | SWAPPED) == 0)
                .collect(),
        ))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (ptr, pages);
        Ok(None)
    }
}

/// Replaces `len` bytes at `at` with a private, copy-on-write mapping of `file` from
/// `offset`. Pages come from the page cache on first touch; a write copies only the page
/// written.
///
/// Reserving no commit, as the RAM it replaces reserved none: Linux charges a private
/// writable mapping at its whole size, so each restored VM was charged its whole RAM
/// where it had written little, and strict overcommit (mode 2) ignores the flag and keeps
/// charging it (overcommit-accounting.rst; PM M56).
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
            libc::MAP_PRIVATE | libc::MAP_FIXED | noreserve(),
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

/// Opens the directory at `path`, to open files in it wherever it goes ([`open_in`],
/// [`write_in`]).
pub fn open_dir(path: &std::path::Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(path)
}

/// Opens `name` for reading in the directory `dir` holds open: the file there, even if the
/// directory was renamed or replaced since.
pub fn open_in(dir: &File, name: &str) -> io::Result<File> {
    use std::os::fd::FromRawFd as _;
    let name = std::ffi::CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a file name with NUL"))?;
    // SAFETY: openat(2) with a NUL-terminated name, relative to a directory we hold open.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor we just opened, and nothing else owns.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Creates `name` in the directory `dir` holds open, durably: written to a temporary
/// sibling of its own, synced, renamed over `name`, and the directory synced. It lands
/// there even if the directory was renamed since it was opened, and of writers racing to
/// the same name, one replaces the file whole.
pub fn write_in(dir: &File, name: &str, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write as _;
    use std::os::fd::FromRawFd as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let invalid = |_| io::Error::new(io::ErrorKind::InvalidInput, "a file name with NUL");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let tmp = std::ffi::CString::new(format!(
        ".{name}.{nanos:x}-{:x}-{:x}.tmp",
        std::process::id(),
        COUNT.fetch_add(1, Ordering::Relaxed)
    ))
    .map_err(invalid)?;
    let target = std::ffi::CString::new(name).map_err(invalid)?;
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC;
    // SAFETY: openat(2) with a NUL-terminated name, relative to a directory we hold open.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), tmp.as_ptr(), flags, 0o644 as libc::c_uint) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor we just opened, and nothing else owns.
    let mut file = unsafe { File::from_raw_fd(fd) };
    let written = file
        .write_all(bytes)
        .and_then(|()| sync_durable(&file))
        .and_then(|()| {
            // SAFETY: renameat(2) with NUL-terminated names, relative to a directory we hold
            // open.
            if unsafe { libc::renameat(dir.as_raw_fd(), tmp.as_ptr(), dir.as_raw_fd(), target.as_ptr()) } != 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    drop(file);
    if let Err(e) = written {
        // SAFETY: unlinkat(2) of our own temporary file, relative to a directory we hold
        // open.
        unsafe { libc::unlinkat(dir.as_raw_fd(), tmp.as_ptr(), 0) };
        return Err(e);
    }
    sync_entries(dir)
}

/// Makes the entries of the directory `dir` holds open durable: the renames into it
/// survive a crash once this returns (review 1.13). Linux takes an fsync of the directory
/// itself (fsync(2)); macOS takes [`sync_durable`]'s flush, whose `F_FULLFSYNC`
/// directories accept (PM M46), and whose fsync a filesystem without it gets instead,
/// where std's `sync_all` would fail.
pub fn sync_entries(dir: &File) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    return sync_durable(dir);
    #[cfg(not(target_os = "macos"))]
    return dir.sync_all();
}

/// Makes completed writes durable on stable storage. On macOS `fsync` does not flush the
/// drive's write cache; `F_FULLFSYNC` does (fsync(2), fcntl(2)). A filesystem without
/// `F_FULLFSYNC` gets `fsync` instead, as SQLite falls back (`full_fsync`, os_unix.c):
/// std's `sync_data` would ask for `F_FULLFSYNC` again.
pub fn sync_durable(file: &File) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: fcntl(2) on an owned, open descriptor.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } == 0 {
            return Ok(());
        }
        // SAFETY: fsync(2) on the same descriptor.
        if unsafe { libc::fsync(file.as_raw_fd()) } == 0 {
            return Ok(());
        }
        Err(io::Error::last_os_error())
    }
    #[cfg(not(target_os = "macos"))]
    file.sync_data()
}

/// The names in the directory `dir` holds open, but for `.` and `..`, wherever it has gone
/// (fdopendir(3), readdir(3)).
pub fn names_in(dir: &File) -> io::Result<Vec<std::ffi::OsString>> {
    use std::os::unix::ffi::OsStrExt as _;
    // A descriptor of its own for the stream, which closedir(3) closes: the directory's
    // own stays open, and reads from its start.
    // SAFETY: openat(2) of the directory itself, relative to a directory we hold open.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            c".".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fdopendir(3) of a directory descriptor we just opened; on success the
    // stream owns it.
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let e = io::Error::last_os_error();
        // SAFETY: close(2) of the descriptor fdopendir did not take.
        unsafe { libc::close(fd) };
        return Err(e);
    }
    let mut names = Vec::new();
    let read = loop {
        // readdir(3) says the end and an error apart only by errno.
        clear_errno();
        // SAFETY: readdir(3) on the stream opened above, not yet closed.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            let e = io::Error::last_os_error();
            break if e.raw_os_error() == Some(0) {
                Ok(())
            } else {
                Err(e)
            };
        }
        // SAFETY: readdir's entry holds a NUL-terminated name, valid until the next call.
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
        if !matches!(name.to_bytes(), b"." | b"..") {
            names.push(std::ffi::OsStr::from_bytes(name.to_bytes()).to_os_string());
        }
    };
    // SAFETY: closedir(3) of the stream opened above, once.
    unsafe { libc::closedir(stream) };
    read.map(|()| names)
}

/// Sets errno to 0, for calls that say an error only by it.
fn clear_errno() {
    // SAFETY: the calling thread's errno, which is writable.
    #[cfg(target_os = "macos")]
    unsafe {
        *libc::__error() = 0;
    }
    // SAFETY: the calling thread's errno, which is writable.
    #[cfg(not(target_os = "macos"))]
    unsafe {
        *libc::__errno_location() = 0;
    }
}

/// How many threads this process may have, as far as the system says: macOS's limit for a
/// process (`kern.num_taskthreads`); on Linux the least of its user's (`RLIMIT_NPROC`,
/// which counts threads there: getrlimit(2)), the system's (`/proc/sys/kernel/threads-max`,
/// proc(5)), and its control groups' (`pids_max`). `None` where none is said.
pub fn thread_limit() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let mut per_process: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>();
        // SAFETY: sysctlbyname(3) reading one int into a local of its size.
        let read = unsafe {
            libc::sysctlbyname(
                c"kern.num_taskthreads".as_ptr(),
                (&raw mut per_process).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if read != 0 {
            return None;
        }
        u64::try_from(per_process).ok()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mut least: Option<u64> = None;
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: getrlimit(2) into a local.
        if unsafe { libc::getrlimit(libc::RLIMIT_NPROC, &mut lim) } == 0
            && lim.rlim_cur != libc::RLIM_INFINITY
        {
            least = fewer(least, Some(lim.rlim_cur));
        }
        least = fewer(least, count(std::path::Path::new("/proc/sys/kernel/threads-max")));
        if let Ok(groups) = std::fs::read_to_string("/proc/self/cgroup") {
            least = fewer(least, pids_max(std::path::Path::new("/sys/fs/cgroup"), &groups));
        }
        least
    }
}

/// The fewer of two limits, either of which may be none.
#[cfg(not(target_os = "macos"))]
fn fewer(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        _ => a.or(b),
    }
}

/// The number a file of the kernel's holds, if it holds one (`max` is none).
#[cfg(not(target_os = "macos"))]
fn count(path: &std::path::Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// The fewest tasks the control groups of a process allow it, from what its
/// `/proc/PID/cgroup` says (`groups`), with cgroupfs at `root`, where systemd and runc
/// mount it (runc libcontainer/cgroups/utils.go): the `pids.max` of its group and of each
/// above it, to the root of those it sees, included, which is where a container with a
/// cgroup namespace of its own finds its limit (Linux Documentation/admin-guide/
/// cgroup-v2.rst, "PID" and "Namespace"). cgroup v2's unified hierarchy (`0::PATH`), and
/// v1's pids controller (`N:pids:PATH`, under `root/pids`: cgroup-v1/pids.rst), both read,
/// as a host mounting both (systemd's hybrid) has its limits on v1. A group out of the
/// namespace's sight (`/../…`) has none it can read.
#[cfg(not(target_os = "macos"))]
pub(super) fn pids_max(root: &std::path::Path, groups: &str) -> Option<u64> {
    let mut least = None;
    for line in groups.lines() {
        let mut fields = line.splitn(3, ':');
        let (Some(_), Some(controllers), Some(path)) = (fields.next(), fields.next(), fields.next()) else {
            continue;
        };
        if std::path::Path::new(path)
            .components()
            .any(|c| c == std::path::Component::ParentDir)
        {
            continue;
        }
        let base = if controllers.is_empty() {
            root.to_path_buf()
        } else if controllers.split(',').any(|c| c == "pids") {
            root.join("pids")
        } else {
            continue;
        };
        let mut dir = base.join(path.trim_start_matches('/'));
        while dir.starts_with(&base) {
            least = fewer(least, count(&dir.join("pids.max")));
            if dir == base || !dir.pop() {
                break;
            }
        }
    }
    least
}

/// A child process's end, watched ([`Poller::add_exit`]): on Linux its descriptor, whose
/// closing ends the watch; on macOS nothing, kqueue dropping the event once it is told.
#[derive(Debug)]
pub struct ExitWatch {
    #[cfg(not(target_os = "macos"))]
    _pidfd: std::os::fd::OwnedFd,
}

/// Descriptors waited on together for something to read, or their end, each named by
/// a token: kqueue on macOS (kqueue(2), `EVFILT_READ`), epoll on Linux (epoll(7)). Level
/// by level: one stays ready while anything is left to read, or once it has ended. A
/// wait costs what is ready, not what is watched.
#[derive(Debug)]
pub struct Poller {
    fd: std::os::fd::OwnedFd,
}

impl Poller {
    pub fn new() -> io::Result<Poller> {
        use std::os::fd::FromRawFd as _;
        #[cfg(target_os = "macos")]
        // SAFETY: kqueue(2) takes no arguments. A kqueue is not inherited by a child.
        let raw = unsafe { libc::kqueue() };
        #[cfg(not(target_os = "macos"))]
        // SAFETY: epoll_create1(2) with a flag.
        let raw = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a descriptor just made, owned by nothing else.
        let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
        Ok(Poller { fd })
    }

    /// Watches `fd`, named `token`, until it is [`remove`](Self::remove)d or closed.
    pub fn add(&self, fd: std::os::fd::BorrowedFd<'_>, token: u64) -> io::Result<()> {
        #[cfg(target_os = "macos")]
        {
            self.change(fd, libc::EV_ADD, token)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let mut event = libc::epoll_event {
                events: (libc::EPOLLIN | libc::EPOLLRDHUP) as u32,
                u64: token,
            };
            // SAFETY: epoll_ctl(2) adding a descriptor the caller holds open.
            if unsafe {
                libc::epoll_ctl(
                    self.fd.as_raw_fd(),
                    libc::EPOLL_CTL_ADD,
                    fd.as_raw_fd(),
                    &mut event,
                )
            } < 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }

    /// Watches child process `pid` for its end, named `token`: ready once it has exited,
    /// when its status is taken without waiting. On macOS once (`EVFILT_PROC`,
    /// `NOTE_EXIT`, delivered `EV_ONESHOT`); on Linux until the watch is dropped, through
    /// the process's descriptor (pidfd_open(2), Linux 5.3), readable once it has exited.
    /// macOS attaches only what comes after (xnu-11417.101.15 bsd/kern/kern_event.c,
    /// `filt_procattach`), and refuses a child that has ended already with `ESRCH`: look at
    /// the child once its end is watched.
    pub fn add_exit(&self, pid: u32, token: u64) -> io::Result<ExitWatch> {
        let pid = libc::pid_t::try_from(pid)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a process ID out of range"))?;
        #[cfg(target_os = "macos")]
        {
            // SAFETY: kevent is plain data, for which all zeroes is a value.
            let mut change: libc::kevent = unsafe { std::mem::zeroed() };
            change.ident = pid.unsigned_abs() as libc::uintptr_t;
            change.filter = libc::EVFILT_PROC;
            change.flags = libc::EV_ADD;
            change.fflags = libc::NOTE_EXIT;
            change.udata = token as usize as *mut libc::c_void;
            // SAFETY: kevent(2) applying one change, asking for no events.
            if unsafe {
                libc::kevent(
                    self.fd.as_raw_fd(),
                    &change,
                    1,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                )
            } < 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(ExitWatch {})
        }
        #[cfg(not(target_os = "macos"))]
        {
            use std::os::fd::{AsFd as _, FromRawFd as _};
            // SAFETY: pidfd_open(2) of a process, without flags: its descriptor is
            // close-on-exec.
            let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            let raw = std::os::fd::RawFd::try_from(raw)
                .map_err(|_| io::Error::other("a descriptor out of range"))?;
            // SAFETY: a descriptor just made, owned by nothing else.
            let pidfd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
            self.add(pidfd.as_fd(), token)?;
            Ok(ExitWatch { _pidfd: pidfd })
        }
    }

    /// Stops watching `fd`.
    pub fn remove(&self, fd: std::os::fd::BorrowedFd<'_>) -> io::Result<()> {
        #[cfg(target_os = "macos")]
        {
            self.change(fd, libc::EV_DELETE, 0)
        }
        #[cfg(not(target_os = "macos"))]
        {
            // SAFETY: epoll_ctl(2) removing a descriptor; the event is ignored (and may be
            // null since Linux 2.6.9).
            if unsafe {
                libc::epoll_ctl(
                    self.fd.as_raw_fd(),
                    libc::EPOLL_CTL_DEL,
                    fd.as_raw_fd(),
                    std::ptr::null_mut(),
                )
            } < 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }

    #[cfg(target_os = "macos")]
    fn change(&self, fd: std::os::fd::BorrowedFd<'_>, flags: u16, token: u64) -> io::Result<()> {
        // SAFETY: kevent is plain data, for which all zeroes is a value.
        let mut change: libc::kevent = unsafe { std::mem::zeroed() };
        change.ident = fd.as_raw_fd() as libc::uintptr_t;
        change.filter = libc::EVFILT_READ;
        change.flags = flags;
        change.udata = token as usize as *mut libc::c_void;
        // SAFETY: kevent(2) applying one change, asking for no events.
        if unsafe {
            libc::kevent(
                self.fd.as_raw_fd(),
                &change,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Waits until some of those watched are ready, or `timeout` passes (`None`: for as
    /// long as it takes), and puts their tokens in `ready`, which it empties first. A
    /// signal's interruption is a wait that found nothing.
    pub fn wait(&self, ready: &mut Vec<u64>, timeout: Option<std::time::Duration>) -> io::Result<()> {
        ready.clear();
        const AT_ONCE: usize = 64;
        #[cfg(target_os = "macos")]
        {
            // SAFETY: kevent is plain data, for which all zeroes is a value.
            let mut events: [libc::kevent; AT_ONCE] = unsafe { std::mem::zeroed() };
            let spec = timeout.map(|t| libc::timespec {
                tv_sec: libc::time_t::try_from(t.as_secs()).unwrap_or(libc::time_t::MAX),
                tv_nsec: libc::c_long::from(t.subsec_nanos()),
            });
            // SAFETY: kevent(2) asking for up to AT_ONCE events into a buffer that size.
            let n = unsafe {
                libc::kevent(
                    self.fd.as_raw_fd(),
                    std::ptr::null(),
                    0,
                    events.as_mut_ptr(),
                    AT_ONCE as libc::c_int,
                    spec.as_ref()
                        .map_or(std::ptr::null(), |s| s as *const libc::timespec),
                )
            };
            let n = match usize::try_from(n) {
                Ok(n) => n,
                Err(_) => return interrupted_or(io::Error::last_os_error()),
            };
            ready.extend(events.iter().take(n).map(|e| e.udata as usize as u64));
        }
        #[cfg(not(target_os = "macos"))]
        {
            // SAFETY: epoll_event is plain data, for which all zeroes is a value.
            let mut events: [libc::epoll_event; AT_ONCE] = unsafe { std::mem::zeroed() };
            // Rounded up: a wait of 0 ms would return at once, and spin.
            let ms = timeout.map_or(-1, |t| {
                libc::c_int::try_from(t.as_micros().div_ceil(1000)).unwrap_or(libc::c_int::MAX)
            });
            // SAFETY: epoll_wait(2) for up to AT_ONCE events into a buffer that size.
            let n = unsafe {
                libc::epoll_wait(
                    self.fd.as_raw_fd(),
                    events.as_mut_ptr(),
                    AT_ONCE as libc::c_int,
                    ms,
                )
            };
            let n = match usize::try_from(n) {
                Ok(n) => n,
                Err(_) => return interrupted_or(io::Error::last_os_error()),
            };
            ready.extend(events.iter().take(n).map(|e| e.u64));
        }
        Ok(())
    }
}

/// A wait a signal cut short found nothing; any other failure is one.
fn interrupted_or(e: io::Error) -> io::Result<()> {
    if e.kind() == io::ErrorKind::Interrupted {
        Ok(())
    } else {
        Err(e)
    }
}

/// A watch on a directory, for other processes' changes to it: its descriptor becomes
/// readable once a name in it has come or gone, or the file of it that is watched has
/// been written, and for no other file's writes, for poll(2) to wait on beside others.
/// kqueue's `EVFILT_VNODE` on macOS (kqueue(2)), inotify on Linux (inotify(7)), each on
/// the directory and on the file watched.
#[derive(Debug)]
pub struct FileWatch {
    fd: std::os::fd::OwnedFd,
    /// What kqueue watches, open while it does.
    #[cfg(target_os = "macos")]
    _dir: File,
    #[cfg(target_os = "macos")]
    file: Option<File>,
    /// inotify's watch of the file watched.
    #[cfg(not(target_os = "macos"))]
    file: Option<libc::c_int>,
}

impl FileWatch {
    /// A watch on the directory `dir` holds open, wherever it goes.
    pub fn new(dir: &File) -> io::Result<FileWatch> {
        use std::os::fd::FromRawFd as _;
        #[cfg(target_os = "macos")]
        {
            let opened = dir.try_clone()?;
            // SAFETY: kqueue(2) takes no arguments.
            let kq = unsafe { libc::kqueue() };
            if kq < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: a descriptor just made, owned by nothing else.
            let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(kq) };
            let watch = FileWatch {
                fd,
                _dir: opened,
                file: None,
            };
            watch.register(&watch._dir)?;
            Ok(watch)
        }
        #[cfg(not(target_os = "macos"))]
        {
            // SAFETY: inotify_init1(2) with flags.
            let raw = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: a descriptor just made, owned by nothing else.
            let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
            // The directory itself, through its descriptor's link in /proc (proc(5)),
            // which inotify follows.
            let name = std::ffi::CString::new(format!("/proc/self/fd/{}", dir.as_raw_fd()))
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path with NUL"))?;
            // Names alone: a write to a file in it is not one, nor seen unless the file
            // is watched (`file`).
            let mask = libc::IN_CREATE
                | libc::IN_DELETE
                | libc::IN_MOVED_TO
                | libc::IN_MOVED_FROM
                | libc::IN_DELETE_SELF
                | libc::IN_MOVE_SELF
                | libc::IN_ONLYDIR;
            // SAFETY: inotify_add_watch(2) with a NUL-terminated path.
            if unsafe { libc::inotify_add_watch(raw, name.as_ptr(), mask) } < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(FileWatch { fd, file: None })
        }
    }

    /// Watches `file`, one in the directory, for writes, in place of the one watched
    /// before. On Linux that may wake it once: the watch replaced says it has gone
    /// (`IN_IGNORED`, inotify(7)).
    pub fn file(&mut self, file: &File) -> io::Result<()> {
        #[cfg(target_os = "macos")]
        {
            let file = file.try_clone()?;
            self.register(&file)?;
            // The one before leaves kqueue as its descriptor closes (kqueue(2)).
            self.file = Some(file);
        }
        #[cfg(not(target_os = "macos"))]
        {
            let name = std::ffi::CString::new(format!("/proc/self/fd/{}", file.as_raw_fd()))
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path with NUL"))?;
            // SAFETY: inotify_add_watch(2) with a NUL-terminated path.
            let watched =
                unsafe { libc::inotify_add_watch(self.fd.as_raw_fd(), name.as_ptr(), libc::IN_MODIFY) };
            if watched < 0 {
                return Err(io::Error::last_os_error());
            }
            // The same file again is the same watch, its mask replaced (inotify(7)); one
            // whose file is gone went with it, and its removal fails harmlessly.
            if let Some(before) = self.file.replace(watched)
                && before != watched
            {
                // SAFETY: inotify_rm_watch(2) of a watch of this descriptor's.
                unsafe { libc::inotify_rm_watch(self.fd.as_raw_fd(), before) };
            }
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn register(&self, file: &File) -> io::Result<()> {
        // SAFETY: kevent is plain data, for which all zeroes is a value.
        let mut change: libc::kevent = unsafe { std::mem::zeroed() };
        change.ident = file.as_raw_fd() as libc::uintptr_t;
        change.filter = libc::EVFILT_VNODE;
        change.flags = libc::EV_ADD | libc::EV_CLEAR;
        change.fflags = libc::NOTE_WRITE | libc::NOTE_EXTEND | libc::NOTE_DELETE | libc::NOTE_RENAME;
        // SAFETY: kevent(2) registering one change, and asking for no events.
        if unsafe {
            libc::kevent(
                self.fd.as_raw_fd(),
                &change,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// The descriptor to wait on.
    pub fn fd(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd as _;
        self.fd.as_fd()
    }

    /// Takes the changes seen, so the descriptor waits for the next.
    pub fn clear(&self) {
        #[cfg(target_os = "macos")]
        {
            // SAFETY: kevent is plain data, for which all zeroes is a value.
            let mut events: [libc::kevent; 8] = unsafe { std::mem::zeroed() };
            let none = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: kevent(2) taking up to 8 events into a local, without waiting.
            while unsafe {
                libc::kevent(
                    self.fd.as_raw_fd(),
                    std::ptr::null(),
                    0,
                    events.as_mut_ptr(),
                    8,
                    &none,
                )
            } == 8
            {}
        }
        #[cfg(not(target_os = "macos"))]
        {
            let mut buf = [0u8; 4096];
            // SAFETY: read(2) into a local buffer of its size, from a non-blocking
            // descriptor.
            while unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
        }
    }
}

/// What a process uses of the host: its resident memory, in bytes, and the CPU time it
/// has had, user and system together, in nanoseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub resident: u64,
    pub cpu_ns: u64,
}

/// [`Usage`] of process `pid`, as the kernel keeps it: macOS's proc_pidinfo
/// (PROC_PIDTASKINFO), its times in Mach absolute-time units converted by the timebase;
/// Linux's /proc/PID/stat (utime, stime, in clock ticks) and /proc/PID/statm (resident
/// pages).
pub fn process_usage(pid: u32) -> io::Result<Usage> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: a zeroed proc_taskinfo is a valid out-parameter, which proc_pidinfo
        // fills up to its size.
        let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
        let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_taskinfo>()).unwrap_or(0);
        let pid = libc::c_int::try_from(pid).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: proc_pidinfo(3) writes at most `size` bytes into `info`.
        let got = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTASKINFO, 0, (&raw mut info).cast(), size) };
        if got != size {
            return Err(io::Error::last_os_error());
        }
        // <mach/mach_time.h>'s, bound here: libc's binding is deprecated in favour of a
        // crate, the call itself is not.
        #[repr(C)]
        struct Timebase {
            numer: u32,
            denom: u32,
        }
        unsafe extern "C" {
            fn mach_timebase_info(info: *mut Timebase) -> libc::c_int;
        }
        let mut base = Timebase { numer: 0, denom: 0 };
        // SAFETY: mach_timebase_info(3) fills `base`.
        unsafe { mach_timebase_info(&raw mut base) };
        let ticks = u128::from(info.pti_total_user) + u128::from(info.pti_total_system);
        let ns = if base.denom == 0 {
            ticks
        } else {
            ticks * u128::from(base.numer) / u128::from(base.denom)
        };
        Ok(Usage {
            resident: info.pti_resident_size,
            cpu_ns: u64::try_from(ns).unwrap_or(u64::MAX),
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
        // Fields after the command, which may hold spaces, in parentheses.
        let rest = stat.rsplit_once(')').map(|(_, r)| r).unwrap_or_default();
        let fields: Vec<&str> = rest.split_whitespace().collect();
        let tick = |i: usize| fields.get(i).and_then(|f| f.parse::<u64>().ok()).unwrap_or(0);
        // utime and stime are the stat line's 14th and 15th fields: 12th and 13th after
        // the command.
        let ticks = tick(11).saturating_add(tick(12));
        // SAFETY: sysconf(3) reads a constant.
        let hz = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) })
            .unwrap_or(100)
            .max(1);
        let statm = std::fs::read_to_string(format!("/proc/{pid}/statm"))?;
        let pages: u64 = statm
            .split_whitespace()
            .nth(1)
            .and_then(|f| f.parse().ok())
            .unwrap_or(0);
        Ok(Usage {
            resident: pages.saturating_mul(page_size()? as u64),
            cpu_ns: ticks.saturating_mul(1_000_000_000 / hz),
        })
    }
}

#[cfg(test)]
mod usage_tests {
    use super::*;

    #[test]
    fn a_process_is_measured_as_it_runs() {
        let me = std::process::id();
        let before = process_usage(me).unwrap();
        // Some CPU time, spent here.
        let mut x = 0u64;
        for i in 0..50_000_000u64 {
            x = x.wrapping_add(i * i);
        }
        assert!(x > 0);
        let after = process_usage(me).unwrap();
        assert!(after.resident > 0);
        assert!(after.cpu_ns > before.cpu_ns, "{before:?} {after:?}");
    }
}
