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

/// Waits until any of `sockets` has something to read, its end, or an error (poll(2)), or
/// `timeout` passes: which did. For the few sockets one thread waits on at a time.
pub fn wait_readable(
    sockets: &[super::RawSocket],
    timeout: Option<std::time::Duration>,
) -> io::Result<Vec<bool>> {
    let mut polled: Vec<libc::pollfd> = sockets
        .iter()
        .map(|&fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    // Rounded up, so that a wait never ends early.
    let ms = timeout.map_or(-1, |t| {
        libc::c_int::try_from(t.as_micros().div_ceil(1000)).unwrap_or(libc::c_int::MAX)
    });
    loop {
        // SAFETY: poll(2) on `polled.len()` entries of descriptors the caller holds.
        if unsafe { libc::poll(polled.as_mut_ptr(), polled.len() as libc::nfds_t, ms) } >= 0 {
            return Ok(polled.iter().map(|p| p.revents != 0).collect());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
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

/// Keeps `ptr..ptr+len`, guest RAM from [`reserve_ram`] or a file mapped over it, on the
/// host's base pages: memory the guest touches sparsely, where each huge page would hold 2
/// MiB resident for the few KiB it uses (x86_64's first 2 MiB: 78 of 512 pages, PM M157),
/// and a file's page cache folios, which a mapping without it maps whole (PM M287). Advice
/// only, given before the first touch: where it fails, or the kernel has no THP, the memory
/// stays as it was.
pub fn small_pages(ptr: NonNull<u8>, len: usize) {
    #[cfg(all(target_os = "linux", not(miri)))]
    // SAFETY: madvise(2) of guest RAM, which the caller maps.
    if unsafe { libc::madvise(ptr.as_ptr().cast(), len, libc::MADV_NOHUGEPAGE) } != 0 {
        crate::debug!("MADV_NOHUGEPAGE: {}", io::Error::last_os_error());
    }
    #[cfg(not(all(target_os = "linux", not(miri))))]
    let _ = (ptr, len);
}

/// Puts `ptr..ptr+len`, guest RAM from [`reserve_ram`], on huge pages as the guest next
/// touches it, as [`reserve_ram`] advises it, where [`small_pages`] kept it off them.
/// Advice only: where it fails, or the kernel has no THP, the memory stays as it was.
pub fn huge_pages(ptr: NonNull<u8>, len: usize) {
    #[cfg(all(target_os = "linux", not(miri)))]
    // SAFETY: madvise(2) of guest RAM, which the caller maps.
    if unsafe { libc::madvise(ptr.as_ptr().cast(), len, libc::MADV_HUGEPAGE) } != 0 {
        crate::debug!("MADV_HUGEPAGE: {}", io::Error::last_os_error());
    }
    #[cfg(not(all(target_os = "linux", not(miri))))]
    let _ = (ptr, len);
}

/// Has the kernel read only the page each fault on `ptr..ptr+len`, a file's shared
/// mapping, asks for: no readahead around it (madvise(2) `MADV_RANDOM`), where the first
/// fault in a hole of a file Btrfs keeps fills its whole 4 MiB window (PM M297). Advice
/// only: where it fails, the mapping reads ahead as before.
pub fn reads_at_random(ptr: NonNull<u8>, len: usize) {
    #[cfg(not(miri))]
    // SAFETY: madvise(2) of a mapping the caller holds.
    if unsafe { libc::madvise(ptr.as_ptr().cast(), len, libc::MADV_RANDOM) } != 0 {
        crate::debug!("MADV_RANDOM: {}", io::Error::last_os_error());
    }
    #[cfg(miri)]
    let _ = (ptr, len);
}

/// Takes [`reads_at_random`] back: the faults on `ptr..ptr+len` read ahead again
/// (`MADV_NORMAL`), as a sequential read of what the file holds needs them to (PM M297).
pub fn reads_ahead(ptr: NonNull<u8>, len: usize) {
    #[cfg(not(miri))]
    // SAFETY: madvise(2) of a mapping the caller holds.
    if unsafe { libc::madvise(ptr.as_ptr().cast(), len, libc::MADV_NORMAL) } != 0 {
        crate::debug!("MADV_NORMAL: {}", io::Error::last_os_error());
    }
    #[cfg(miri)]
    let _ = (ptr, len);
}

/// The transparent huge page size guest RAM is advised for, where the host has them and
/// the advice is not left out (SHARDS_THP_ADVICE=never, PM M157, M225): Linux's.
pub fn advised_huge_page() -> Option<usize> {
    #[cfg(all(target_os = "linux", not(miri)))]
    if std::env::var_os("SHARDS_THP_ADVICE").is_none_or(|a| a != "never") {
        return huge_page_size();
    }
    None
}

/// Whether the host puts every process's anonymous memory on transparent huge pages,
/// advised or not (`always` in /sys/kernel/mm/transparent_hugepage/enabled), as against
/// only what is advised (`madvise`): Linux's.
pub fn thp_always() -> bool {
    #[cfg(all(target_os = "linux", not(miri)))]
    {
        std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled")
            .is_ok_and(|mode| mode.split_whitespace().any(|m| m == "[always]"))
    }
    #[cfg(not(all(target_os = "linux", not(miri))))]
    {
        false
    }
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
    // SHARDS_THP_ADVICE=never leaves it out, for measuring what it buys (PM M157, M225).
    let advise = std::env::var_os("SHARDS_THP_ADVICE").is_none_or(|a| a != "never");
    // SAFETY: `aligned..aligned+len` is our own mapping.
    if advise && unsafe { libc::madvise(aligned as *mut libc::c_void, len, libc::MADV_HUGEPAGE) } != 0 {
        crate::debug!("MADV_HUGEPAGE: {}", io::Error::last_os_error());
    }
    NonNull::new(aligned as *mut u8).ok_or_else(|| io::Error::other("mmap returned NULL"))
}

/// Gives the pages of `ptr..ptr+len` back to the host and leaves the range inaccessible:
/// fresh anonymous memory, PROT_NONE, mapped over it (mmap(2) MAP_FIXED), which frees the
/// pages it held, where MADV_FREE_REUSABLE and MADV_FREE gave nothing back while
/// Hypervisor.framework mapped them (PM M193). On a hypervisor whose stage 2 holds the
/// pages it maps (`hv::Stage2`), the range must be unmapped from the VM first: replaced in
/// the host alone, it left the guest the old pages, and the guest's writes no longer
/// reached the host's (M193).
///
/// # Safety
/// `ptr..ptr+len` must be page-aligned, within one mapping from [`reserve`] or
/// [`reserve_ram`], or a mapping [`map_file_shared`] made, and accessed by no host thread
/// meanwhile.
pub unsafe fn withhold(ptr: NonNull<u8>, len: usize) -> io::Result<()> {
    // SAFETY: MAP_FIXED over the caller's own mapping, which nothing accesses meanwhile.
    let p = unsafe {
        libc::mmap(
            ptr.as_ptr().cast(),
            len,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_FIXED | noreserve(),
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Gives the pages of `ptr..ptr+len` back to the host and leaves the range readable and
/// writable, zero-filled as it is next touched: guest RAM the guest holds free (D151).
/// Linux: MADV_DONTNEED, which keeps the mapping and its advice, and takes what it held
/// of anonymous memory, or its copies of a file mapped private (madvise(2)). macOS: fresh
/// anonymous memory mapped over it (mmap(2) MAP_FIXED), as MADV_FREE_REUSABLE and
/// MADV_FREE gave nothing back (PM M193); the hypervisor must not map it meanwhile.
///
/// # Safety
/// As for [`withhold`].
pub unsafe fn discard(ptr: NonNull<u8>, len: usize) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    // SAFETY: madvise(2) of the caller's own mapping, which nothing accesses meanwhile.
    let done = unsafe { libc::madvise(ptr.as_ptr().cast(), len, libc::MADV_DONTNEED) } == 0;
    #[cfg(not(target_os = "linux"))]
    // SAFETY: MAP_FIXED over the caller's own mapping, which nothing accesses meanwhile.
    let done = unsafe {
        libc::mmap(
            ptr.as_ptr().cast(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_FIXED | noreserve(),
            -1,
            0,
        )
    } != libc::MAP_FAILED;
    if !done {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Makes `ptr..ptr+len`, which [`withhold`] left inaccessible, readable and writable again,
/// zero-filled as it is first touched; on Linux advised as [`reserve_ram`] advises guest
/// RAM, MADV_HUGEPAGE, which the mapping `withhold` made does not inherit.
///
/// # Safety
/// As for [`withhold`].
pub unsafe fn grant(ptr: NonNull<u8>, len: usize) -> io::Result<()> {
    // SAFETY: mprotect(2) of the caller's own mapping.
    if unsafe { libc::mprotect(ptr.as_ptr().cast(), len, libc::PROT_READ | libc::PROT_WRITE) } != 0 {
        return Err(io::Error::last_os_error());
    }
    #[cfg(all(target_os = "linux", not(miri)))]
    if huge_page_size().is_some_and(|huge| len >= huge)
        // SAFETY: madvise(2) of the caller's own mapping; advice only.
        && unsafe { libc::madvise(ptr.as_ptr().cast(), len, libc::MADV_HUGEPAGE) } != 0
    {
        crate::debug!("MADV_HUGEPAGE: {}", io::Error::last_os_error());
    }
    Ok(())
}

/// Reserves `len` bytes of guest RAM of which the host gives nothing yet: [`reserve_ram`]'s,
/// [`withhold`]n at once, each part [`grant`]ed as it is given out (memory to plug, D133).
pub fn reserve_withheld(len: usize) -> io::Result<NonNull<u8>> {
    let p = reserve_ram(len)?;
    // SAFETY: the mapping just made, which nothing else references.
    if let Err(e) = unsafe { withhold(p, len) } {
        // SAFETY: as above, released as it goes unused.
        unsafe { release(p, len) };
        return Err(e);
    }
    Ok(p)
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
                info: *mut i32,
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
                    (&raw mut info).cast(),
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

/// Whether every one of the `len / page` pages at `ptr` is [`untouched`], asked without
/// taking memory: a device asks it of what its guest reports free while it holds guest
/// memory's lock, and memory taken there, charged to a cgroup past its `memory.high`, had
/// its kernel stop the device 2 s at each return to user mode (mm/memcontrol.c,
/// `mem_cgroup_handle_over_high`), every other device's queue waiting on the lock meanwhile
/// and its guest's give-back with them (PM M391). Linux reads /proc/self/pagemap, opened
/// once, a page of entries at a time into the stack, and stops at the first page the host
/// holds; macOS asks [`untouched`]. Where the OS cannot say, `None`.
///
/// # Safety
/// As for [`untouched`].
pub unsafe fn all_untouched(ptr: *const u8, len: usize, page: usize) -> io::Result<Option<bool>> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::FileExt;
        const PRESENT: u64 = 1 << 63;
        const SWAPPED: u64 = 1 << 62;
        static PAGEMAP: std::sync::OnceLock<File> = std::sync::OnceLock::new();
        let pagemap = match PAGEMAP.get() {
            Some(pagemap) => pagemap,
            None => {
                let opened = File::open("/proc/self/pagemap")?;
                PAGEMAP.get_or_init(|| opened)
            }
        };
        let (first, pages) = (ptr as usize / page, len / page);
        let mut entries = [0u8; 4096];
        let mut done = 0;
        while done < pages {
            let n = (pages - done).min(entries.len() / 8);
            let read = entries.get_mut(..n * 8).unwrap_or_default();
            pagemap.read_exact_at(read, ((first + done) * 8) as u64)?;
            if read
                .as_chunks::<8>()
                .0
                .iter()
                .any(|e| u64::from_ne_bytes(*e) & (PRESENT | SWAPPED) != 0)
            {
                return Ok(Some(false));
            }
            done += n;
        }
        Ok(Some(true))
    }
    #[cfg(not(target_os = "linux"))]
    {
        // SAFETY: forwarded caller contract.
        Ok(unsafe { untouched(ptr, len, page) }?.map(|pages| pages.iter().all(|u| *u)))
    }
}

/// Guest RAM mapped privately from a file ([`map_file_views`]): nothing kept here, as a
/// private mapping's written pages go back to its file's at [`discard`] (Linux's
/// MADV_DONTNEED frees a private mapping's copies; macOS maps fresh memory over them).
#[derive(Debug)]
pub struct FileViews;

impl FileViews {
    /// Gives back the pages of `ptr..ptr + len` ([`discard`]): the bytes given back.
    ///
    /// # Safety
    /// As for [`discard`].
    pub unsafe fn give_back(&self, ptr: NonNull<u8>, len: usize) -> io::Result<usize> {
        // SAFETY: forwarded caller contract.
        unsafe { discard(ptr, len) }.map(|()| len)
    }
}

/// Maps `len` bytes of `file` from `offset` copy-on-write at `at` ([`map_file_private`]), as
/// a restore maps its RAM, whose pages its guest gives back through the [`FileViews`]:
/// whole, whatever runs of it hold data (windows.rs maps those alone from the file), as a
/// private mapping gives a page back to its file's however much of the file it maps.
///
/// # Safety
/// As for [`map_file_private`].
pub unsafe fn map_file_views(
    file: &File,
    offset: u64,
    len: usize,
    at: NonNull<u8>,
    _data: &[std::ops::Range<usize>],
) -> io::Result<FileViews> {
    // SAFETY: forwarded caller contract.
    unsafe { map_file_private(file, offset, len, at) }.map(|()| FileViews)
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
/// `at..at+len` must be page-aligned, inside a reservation from [`reserve`] or
/// [`reserve_ram`] that nothing references meanwhile and no hypervisor maps, but KVM, whose
/// guest sees what the host maps there from then on (KVM_SET_USER_MEMORY_REGION,
/// Documentation/virt/kvm/api.rst); `offset` must be page-aligned.
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

/// Replaces `len` bytes at `at` with a mapping of `file` from `offset` that a hypervisor
/// maps into a guest as the file's own memory: a share's DAX window's (D189). Shared, so
/// that the guest and the host have one file between them, and writable where `writable`.
///
/// # Safety
/// `at..at+len` must be page-aligned, inside a reservation of the caller's that nothing
/// references meanwhile; `offset` must be page-aligned.
pub unsafe fn map_file_range(
    file: std::os::fd::BorrowedFd<'_>,
    offset: u64,
    len: usize,
    at: NonNull<u8>,
    writable: bool,
) -> io::Result<()> {
    let offset = file_offset(offset)?;
    let prot = libc::PROT_READ | if writable { libc::PROT_WRITE } else { 0 };
    // SAFETY: MAP_FIXED over memory the caller owns and nothing references.
    let p = unsafe {
        libc::mmap(
            at.as_ptr().cast(),
            len,
            prot,
            libc::MAP_SHARED | libc::MAP_FIXED,
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

/// The `size` bytes of `file` read-only, then zeros, `len` bytes in all at one address; and
/// how many of them are the file's mapping: a reservation with the file mapped over its
/// start, to the page its last byte is in ([`map_file_readonly`]), whose rest the kernel
/// fills with zeros. `len` is page-aligned.
pub fn map_file_padded(file: &File, size: u64, len: usize) -> io::Result<(NonNull<u8>, usize)> {
    let page = page_size()?;
    let mapped = usize::try_from(size)
        .ok()
        .and_then(|s| s.checked_next_multiple_of(page))
        .filter(|&m| m <= len && m > 0)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{size} bytes of a file in {len}"),
            )
        })?;
    let host = reserve(len)?;
    // SAFETY: `host..host+len` is a fresh reservation nothing references; `mapped` is
    // page-aligned and within it.
    if let Err(e) = unsafe { map_file_readonly(file, mapped, host) } {
        // SAFETY: our own reservation, never handed out.
        unsafe { release(host, len) };
        return Err(e);
    }
    Ok((host, mapped))
}

/// Unmaps what [`map_file_padded`] mapped.
///
/// # Safety
/// `ptr`, `viewed` and `len` are a mapping's from [`map_file_padded`], which nothing uses
/// any more: no hypervisor maps it.
pub unsafe fn unmap_file_padded(ptr: NonNull<u8>, _viewed: usize, len: usize) {
    // SAFETY: forwarded caller contract; one munmap undoes the reservation and the file
    // mapped over it.
    unsafe { release(ptr, len) };
}

/// The first `len` bytes of `file`, opened for writing, mapped shared and writable: what
/// is written there is the file's, through the page cache, which writes it back as it
/// would any file's (a run's swap, D159). `len` is page-aligned and within the file.
pub fn map_file_shared(file: &File, len: usize) -> io::Result<NonNull<u8>> {
    // SAFETY: a fresh shared mapping of the caller's file; the caller owns it until
    // `unmap_file_shared`.
    let p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    if p == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    NonNull::new(p.cast()).ok_or_else(|| io::Error::other("mmap returned NULL"))
}

/// Maps `len` bytes of `file`, opened for writing, shared and writable at `at` again, as
/// [`map_file_shared`] mapped them: a mapping of it [`withhold`] took away for a while.
///
/// # Safety
/// `at..at+len` must be page-aligned, the range of a mapping of this process's own that
/// nothing references meanwhile and no hypervisor maps.
pub unsafe fn map_file_shared_at(file: &File, len: usize, at: NonNull<u8>) -> io::Result<()> {
    // SAFETY: MAP_FIXED over the caller's own range, which nothing references.
    let p = unsafe {
        libc::mmap(
            at.as_ptr().cast(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_FIXED,
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

/// Makes what was written to `file` durable, through write(2) or through `ptr..ptr+len`,
/// a shared mapping of it ([`map_file_shared`]). Linux's fdatasync(2) writes back the pages a
/// mapping dirtied, as it does any (Btrfs: 2.5 to 3.6 ms for 16 pages of a 10 GiB file).
/// macOS's F_FULLFSYNC writes them back too, and flushes the drive's cache (3 to 6 ms at
/// any size), where a plain fsync leaves them dirty (mincore(2)'s MINCORE_MODIFIED) and an
/// msync(2) of a 10 GiB mapping took 12 to 32 ms (PM M286); a file system without
/// F_FULLFSYNC gets msync, then fsync.
///
/// # Safety
/// `ptr..ptr+len` is the mapping [`map_file_shared`] made of `file`, still mapped.
pub unsafe fn sync_mapped(file: &File, ptr: NonNull<u8>, len: usize) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: fcntl(2) on an owned, open descriptor.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } == 0 {
            return Ok(());
        }
        // SAFETY: forwarded caller contract.
        unsafe { flush_shared(ptr, len) }?;
        // SAFETY: fsync(2) on the same descriptor.
        if unsafe { libc::fsync(file.as_raw_fd()) } == 0 {
            return Ok(());
        }
        Err(io::Error::last_os_error())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (ptr, len);
        file.sync_data()
    }
}

/// Which host pages of `ptr..ptr+len`, a mapping of a file, the host's page cache holds,
/// one for each page in order: mincore(2), whose byte for a page has its low bit set where
/// the page is in core (Linux mm/mincore.c, a file mapping's pages as the page cache holds
/// them; macOS MINCORE_INCORE, sys/mman.h).
///
/// # Safety
/// `ptr..ptr+len` must be mapped in this process, `ptr` page-aligned.
pub unsafe fn in_core(ptr: *const u8, len: usize) -> io::Result<Vec<bool>> {
    let page = page_size()?;
    let mut held = vec![0; len.div_ceil(page)];
    // SAFETY: the caller's range; mincore(2) writes one byte for each of its pages into
    // `held`, which has that many.
    #[cfg(target_os = "linux")]
    let r = unsafe { libc::mincore(ptr.cast_mut().cast(), len, held.as_mut_ptr()) };
    // SAFETY: as above.
    #[cfg(target_os = "macos")]
    let r = unsafe { libc::mincore(ptr.cast(), len, held.as_mut_ptr()) };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(held.into_iter().map(|b| b & 1 != 0).collect())
}

/// Whether a mapping of a file hides its holes: lseek(2)'s SEEK_DATA and SEEK_HOLE then
/// say that all of it holds data. APFS's do, for a file mapped shared and writable, by
/// this process or by a guest through Hypervisor.framework's mapping of it, until neither
/// is left (macOS 26.4.1, PM M285); Linux's Btrfs and tmpfs answer as they would unmapped,
/// pages written through a mapping and not yet written back counted as data (Linux
/// 6.19).
pub const MAPPED_HIDES_HOLES: bool = cfg!(target_os = "macos");

/// Writes what was written through `ptr..ptr + len`, a mapping [`map_file_shared`] made,
/// to its file, and waits until it has (msync(2) MS_SYNC): the file's blocks then hold it,
/// which [`data_ranges`] says.
///
/// # Safety
/// `ptr` and `len` are a mapping's from [`map_file_shared`], still mapped.
pub unsafe fn flush_shared(ptr: NonNull<u8>, len: usize) -> io::Result<()> {
    // SAFETY: forwarded caller contract.
    if unsafe { libc::msync(ptr.as_ptr().cast(), len, libc::MS_SYNC) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// The ranges of `file`'s first `len` bytes that hold data, `(offset, length)` in order:
/// what lseek(2)'s SEEK_DATA and SEEK_HOLE say of it. A file system that keeps no holes
/// answers that all of it does, and one that cannot be asked (EINVAL) is taken to, as one
/// is of a file a mapping hides the holes of ([`MAPPED_HIDES_HOLES`]).
pub fn data_ranges(file: &File, len: u64) -> io::Result<Vec<(u64, u64)>> {
    let end = libc::off_t::try_from(len).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let seek = |at: libc::off_t, whence: libc::c_int| {
        // SAFETY: lseek(2) on an owned, open descriptor; it moves only its offset, which
        // nothing here reads by.
        let to = unsafe { libc::lseek(file.as_raw_fd(), at, whence) };
        if to < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(to)
        }
    };
    let mut ranges = Vec::new();
    let mut at = 0;
    while at < end {
        let data = match seek(at, libc::SEEK_DATA) {
            Ok(data) => data,
            // No data past `at`.
            Err(e) if e.raw_os_error() == Some(libc::ENXIO) => break,
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) && at == 0 => return Ok(vec![(0, len)]),
            Err(e) => return Err(e),
        };
        if data >= end {
            break;
        }
        let hole = seek(data, libc::SEEK_HOLE)?.min(end);
        if hole <= data {
            return Err(io::Error::other(format!(
                "a hole at {hole} before data at {data}"
            )));
        }
        ranges.push((data.unsigned_abs(), (hole - data).unsigned_abs()));
        at = hole;
    }
    Ok(ranges)
}

/// Unmaps what [`map_file_shared`] mapped.
///
/// # Safety
/// `ptr` and `len` are a mapping's from [`map_file_shared`], which nothing uses any more:
/// no hypervisor maps it.
pub unsafe fn unmap_file_shared(ptr: NonNull<u8>, len: usize) {
    // SAFETY: forwarded caller contract.
    unsafe { libc::munmap(ptr.as_ptr().cast(), len) };
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

/// Holds the host's fault-around off over ranges of this process's memory while it lives
/// (Linux 6.7 and later): each range registered with a userfaultfd in asynchronous
/// write-protect mode (UFFD_FEATURE_WP_ASYNC, which any memory may be), which nothing ever
/// write-protects, so that a fault maps the page it faulted on and none of those around it
/// its page cache holds (mm/memory.c, `should_fault_around`: `uffd_disable_fault_around`).
/// What the host then maps of a restored guest's memory is what its guest touched: a
/// working set recorded with fault-around on held four times the pages its guest faulted
/// on CI's runners, which the next restore mapped ahead (PM M312). No fault is ever the
/// userfaultfd's to handle, so it takes user faults alone (UFFD_USER_MODE_ONLY), which needs
/// no privilege. Dropped, its descriptor closes, which lets the ranges go
/// (mm/userfaultfd.c, `userfaultfd_release`), and resets their page tables
/// (`userfaultfd_clear_vma`: `uffd_wp_range`), which a hypervisor following them through
/// MMU notifiers (KVM) takes for its stage 2 gone: held for as long as its guest runs.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct FaultAroundOff {
    /// The userfaultfd, which holds the ranges until it closes.
    _fd: std::os::fd::OwnedFd,
}

#[cfg(target_os = "linux")]
impl FaultAroundOff {
    /// Over `ranges`, each page-aligned; None where the host cannot hold it off: no
    /// userfaultfd, a kernel before 6.7, or a range it refuses (then over none of them).
    pub fn over(ranges: &[(*mut u8, usize)]) -> Option<FaultAroundOff> {
        match Self::try_over(ranges) {
            Ok(off) => Some(off),
            Err(e) => {
                crate::debug!("the host's fault-around stays on: {e}");
                None
            }
        }
    }

    fn try_over(ranges: &[(*mut u8, usize)]) -> io::Result<FaultAroundOff> {
        use std::os::fd::FromRawFd as _;
        // SAFETY: userfaultfd(2) takes its flags alone.
        let fd = unsafe { libc::syscall(libc::SYS_userfaultfd, uffd::FLAGS) };
        let fd = i32::try_from(fd).map_err(|_| io::Error::other("userfaultfd(2): no descriptor"))?;
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the descriptor the call just made, owned here alone.
        let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
        let mut api = uffd::Api {
            api: uffd::API,
            features: uffd::FEATURE_WP_ASYNC,
            ioctls: 0,
        };
        // SAFETY: UFFDIO_API, with the struct it reads and writes.
        if unsafe { libc::ioctl(fd.as_raw_fd(), uffd::IOCTL_API as libc::Ioctl, &mut api) } != 0 {
            return Err(io::Error::last_os_error());
        }
        for &(start, len) in ranges {
            let mut register = uffd::Register {
                start: start as u64,
                len: len as u64,
                mode: uffd::REGISTER_MODE_WP,
                ioctls: 0,
            };
            // SAFETY: UFFDIO_REGISTER, with the struct it reads and writes, over memory of
            // this process's own: it changes no page of it.
            if unsafe { libc::ioctl(fd.as_raw_fd(), uffd::IOCTL_REGISTER as libc::Ioctl, &mut register) } != 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(FaultAroundOff { _fd: fd })
    }
}

/// userfaultfd(2)'s interface as include/uapi/linux/userfaultfd.h has it, for
/// [`FaultAroundOff`] and the VM process's seccomp filter, which allows it.
#[cfg(target_os = "linux")]
pub mod uffd {
    /// O_CLOEXEC, and UFFD_USER_MODE_ONLY.
    pub const FLAGS: libc::c_long = libc::O_CLOEXEC as libc::c_long | 1;
    pub const API: u64 = 0xAA;
    pub const FEATURE_WP_ASYNC: u64 = 1 << 15;
    pub const REGISTER_MODE_WP: u64 = 1 << 1;
    /// _IOWR(0xAA, 0x3F, struct uffdio_api).
    pub const IOCTL_API: u32 = 0xC018_AA3F;
    /// _IOWR(0xAA, 0x00, struct uffdio_register).
    pub const IOCTL_REGISTER: u32 = 0xC020_AA00;

    /// struct uffdio_api.
    #[derive(Debug)]
    #[repr(C)]
    pub struct Api {
        pub api: u64,
        pub features: u64,
        pub ioctls: u64,
    }

    /// struct uffdio_register: its range, then its mode and the ioctls it allows.
    #[derive(Debug)]
    #[repr(C)]
    pub struct Register {
        pub start: u64,
        pub len: u64,
        pub mode: u64,
        pub ioctls: u64,
    }

    // The sizes the requests carry.
    const _: () = assert!(size_of::<Api>() == 24 && size_of::<Register>() == 32);
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

/// `root` joined with `rel`'s directories, each checked to be one (`lstat`), not a link to
/// one elsewhere, and its names plain. A snapshot's directory is written by a VM process,
/// which may leave links and FIFOs there before it gives the directory up (PM M165); it is
/// read only once given up, so what is there no longer changes, and a check of each name
/// holds for the open that follows. `root`, its reader's own, is followed as given.
fn dirs_beneath(root: &std::path::Path, rel: &[&str]) -> io::Result<std::path::PathBuf> {
    let mut path = root.to_path_buf();
    for name in rel {
        plain_name(name)?;
        path.push(name);
        if !std::fs::symlink_metadata(&path)?.is_dir() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not a directory"));
        }
    }
    Ok(path)
}

/// The directory `rel` names beneath the directory at `root`, opened: no component below
/// `root` a link ([`dirs_beneath`]).
pub fn open_dir_beneath(root: &std::path::Path, rel: &[&str]) -> io::Result<File> {
    open_dir(&dirs_beneath(root, rel)?)
}

/// The regular file `rel` names beneath the directory at `root`, opened for reading: no
/// component below `root` a link, and a file that is not a regular one refused unopened
/// (`lstat`), so that no FIFO is waited on, then checked again once open (PM M165).
/// [`dirs_beneath`] says why a check of each name is enough.
pub fn open_beneath(root: &std::path::Path, rel: &[&str]) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let (name, dirs) = rel
        .split_last()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file named"))?;
    plain_name(name)?;
    let path = dirs_beneath(root, dirs)?.join(name);
    let regular = |m: std::fs::Metadata| {
        if m.file_type().is_file() {
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::InvalidData, "not a regular file"))
        }
    };
    regular(std::fs::symlink_metadata(&path)?)?;
    // O_NONBLOCK leaves a regular file's reads as they are.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)?;
    regular(file.metadata()?)?;
    Ok(file)
}

/// `name`, if it names an entry of a directory and no more: not empty, `.` or `..`, and
/// holding no `/` or NUL.
fn plain_name(name: &str) -> io::Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name:?}: not a plain name"),
        ));
    }
    Ok(())
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

/// `name` in the directory `dir` holds open, opened with `flags` beside close-on-exec, made
/// this user's alone where it is made.
fn make_in(dir: &File, name: &str, flags: libc::c_int) -> io::Result<File> {
    use std::os::fd::FromRawFd as _;
    let name = std::ffi::CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a file name with NUL"))?;
    // SAFETY: openat(2) with a NUL-terminated name, relative to a directory we hold open.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor we just opened, and nothing else owns.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Makes `name` in the directory `dir` holds open, this user's alone, to write anew: one
/// there is emptied.
pub fn create_in(dir: &File, name: &str) -> io::Result<File> {
    make_in(dir, name, libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC)
}

/// Makes `name` in the directory `dir` holds open, this user's alone, to append to;
/// refused where it is there.
pub fn append_new_in(dir: &File, name: &str) -> io::Result<File> {
    make_in(
        dir,
        name,
        libc::O_WRONLY | libc::O_APPEND | libc::O_CREAT | libc::O_EXCL,
    )
}

/// What `entry`, a name a directory lists, is a version of: itself, as a write here replaces
/// the file at its name (windows_files.rs `version_of`, whose writes replace none).
pub fn version_of(entry: &str) -> &str {
    entry
}

/// The name `name`'s newest version goes by in the directory `dir` holds open: `name`, as a
/// write here replaces the file there (windows_files.rs `newest_in`).
pub fn newest_in(_dir: &File, name: &str) -> io::Result<String> {
    Ok(name.to_string())
}

/// Opens `name`'s newest version for reading in the directory `dir` holds open: the file at
/// `name` ([`open_in`]; windows_files.rs `open_newest_in`).
pub fn open_newest_in(dir: &File, name: &str) -> io::Result<File> {
    open_in(dir, name)
}

/// The regular file `rel` names beneath `root`, its newest version, opened for reading: the
/// file itself ([`open_beneath`]; windows_files.rs `open_newest_beneath`).
pub fn open_newest_beneath(root: &std::path::Path, rel: &[&str]) -> io::Result<File> {
    open_beneath(root, rel)
}

/// Removes `name` from the directory `dir` holds open, where it is there (windows_files.rs
/// `remove_in`, which removes its every version).
pub fn remove_in(dir: &File, name: &str) -> io::Result<()> {
    match unlink_in(dir, name) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        removed => removed,
    }
}

/// Removes `name` from the directory `dir` holds open.
pub fn unlink_in(dir: &File, name: &str) -> io::Result<()> {
    let name = std::ffi::CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a file name with NUL"))?;
    // SAFETY: unlinkat(2) with a NUL-terminated name, relative to a directory we hold open.
    if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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

/// Makes `file` keep what is never written to it as holes: nothing to do, as ext4, btrfs,
/// XFS and APFS leave a file's unwritten ranges holes by themselves (windows.rs must ask).
pub fn sparse(_file: &File) -> io::Result<()> {
    Ok(())
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

/// Makes `file` sparse, so that what it holds past what is written takes none of its disk:
/// a Unix file extended past what is written (ftruncate(2)) is already, its holes taking no
/// blocks (windows.rs makes an NTFS file so, PM M349).
pub fn make_sparse(_file: &File) -> io::Result<()> {
    Ok(())
}

/// Gives back the file system's blocks under `len` bytes of `file` from `offset`, whole 4
/// KiB pages, which read as zeros after (fcntl(2) F_PUNCHHOLE): APFS fills the holes a
/// small file's writes skip as the file is synced (PM M307: a 4 MiB file of 8 KiB written,
/// 4 MiB after fsync), where Linux's file systems keep them (ext4: 8 KiB after), so
/// elsewhere it does nothing.
pub fn punch_hole(file: &File, offset: u64, len: u64) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    if len > 0 {
        let hole = libc::fpunchhole_t {
            fp_flags: 0,
            reserved: 0,
            fp_offset: file_offset(offset)?,
            fp_length: file_offset(len)?,
        };
        // SAFETY: fcntl(2) F_PUNCHHOLE on an owned, open descriptor, reading the structure.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PUNCHHOLE, &raw const hole) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (file, offset, len);
    Ok(())
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
        sysctl_count(c"kern.num_taskthreads")
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

/// Raises this process's soft limit on open descriptors to its hard limit, as Go's runtime
/// raises its own (go1.25.0 src/syscall/rlimit.go, after go.dev/issue/46279): macOS starts
/// a process with a soft limit of 256, and refuses more than `kern.maxfilesperproc`
/// (src/syscall/rlimit_darwin.go). The hard limit is for whoever starts the process to
/// set. Returns the limit it has.
pub fn raise_descriptor_limit() -> io::Result<u64> {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit(2) into a local.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let raised = lim.rlim_max;
    #[cfg(target_os = "macos")]
    let raised = raised.min(sysctl_count(c"kern.maxfilesperproc").unwrap_or(raised));
    if raised > lim.rlim_cur {
        lim.rlim_cur = raised;
        // SAFETY: setrlimit(2) from a local.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(descriptor_limit().unwrap_or(lim.rlim_cur))
}

/// How many descriptors this process may have open: its soft limit, and on macOS no more
/// than `kern.maxfilesperproc`, which the kernel holds a process to whatever its limit.
pub fn descriptor_limit() -> Option<u64> {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit(2) into a local.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        return None;
    }
    #[cfg(target_os = "macos")]
    let lim = libc::rlimit {
        rlim_cur: lim
            .rlim_cur
            .min(sysctl_count(c"kern.maxfilesperproc").unwrap_or(lim.rlim_cur)),
        ..lim
    };
    Some(lim.rlim_cur)
}

/// How many descriptors this process has open: the entries of /dev/fd on macOS (fdescfs)
/// and of /proc/self/fd on Linux, less the one that lists them. Linux's /dev/fd is a link
/// a host's init or udev makes, which a host without them has not (a guest whose PID 1 is
/// its own, D154's test path): a share then failed to start.
pub fn open_descriptors() -> Option<u64> {
    let dir = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    let count = std::fs::read_dir(dir).ok()?.count();
    u64::try_from(count.checked_sub(1)?).ok()
}

/// The system's table of open files: how many it holds, and how many are open, as macOS's
/// `kern.maxfiles` and `kern.num_files` say, or Linux's /proc/sys/fs/file-max and file-nr
/// (proc_sys_fs(5): file-nr is the files allocated, those free, and the most).
pub fn file_table() -> Option<(u64, u64)> {
    #[cfg(target_os = "macos")]
    {
        Some((sysctl_count(c"kern.maxfiles")?, sysctl_count(c"kern.num_files")?))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let size = count(std::path::Path::new("/proc/sys/fs/file-max"))?;
        let nr = std::fs::read_to_string("/proc/sys/fs/file-nr").ok()?;
        let mut fields = nr.split_whitespace().map(str::parse::<u64>);
        let (Some(Ok(allocated)), Some(Ok(free))) = (fields.next(), fields.next()) else {
            return None;
        };
        Some((size, allocated.saturating_sub(free)))
    }
}

/// How many processes this process's user may have: macOS's `kern.maxprocperuid`; on Linux
/// as many as the threads it may have ([`thread_limit`]), each process being one at least.
pub fn user_processes() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        sysctl_count(c"kern.maxprocperuid")
    }
    #[cfg(not(target_os = "macos"))]
    {
        thread_limit()
    }
}

/// A count the kernel keeps as an int under `name` (sysctlbyname(3)).
#[cfg(target_os = "macos")]
fn sysctl_count(name: &std::ffi::CStr) -> Option<u64> {
    let mut value: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>();
    // SAFETY: sysctlbyname(3) reading one int into a local of its size.
    let read = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&raw mut value).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if read != 0 {
        return None;
    }
    u64::try_from(value).ok()
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
    pub fn add(&self, fd: &impl std::os::fd::AsFd, token: u64) -> io::Result<()> {
        let fd = fd.as_fd();
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
            use std::os::fd::FromRawFd as _;
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
            self.add(&pidfd, token)?;
            Ok(ExitWatch { _pidfd: pidfd })
        }
    }

    /// Stops watching `fd`.
    pub fn remove(&self, fd: &impl std::os::fd::AsFd) -> io::Result<()> {
        let fd = fd.as_fd();
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
/// the directory and on the file watched. On Linux every watch of the process is a watch of
/// one inotify instance ([`notify`]), its own descriptor an eventfd its events are counted
/// into: a user may have few instances.
#[derive(Debug)]
pub struct FileWatch {
    fd: std::os::fd::OwnedFd,
    /// What kqueue watches, open while it does.
    #[cfg(target_os = "macos")]
    _dir: File,
    #[cfg(target_os = "macos")]
    file: Option<File>,
    /// The process's instance's watches of the directory and of the file watched, where it
    /// has one ([`notify::shared`]); else `fd` is an instance of this watch's own.
    #[cfg(not(target_os = "macos"))]
    shared: Option<&'static notify::Shared>,
    #[cfg(not(target_os = "macos"))]
    dir: Option<libc::c_int>,
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
            // Names alone: a write to a file in it is not one, nor seen unless the file
            // is watched (`file`).
            let mask = libc::IN_CREATE
                | libc::IN_DELETE
                | libc::IN_MOVED_TO
                | libc::IN_MOVED_FROM
                | libc::IN_DELETE_SELF
                | libc::IN_MOVE_SELF
                | libc::IN_ONLYDIR;
            if let Some(shared) = notify::shared() {
                // SAFETY: eventfd(2) with flags.
                let raw = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
                if raw < 0 {
                    return Err(io::Error::last_os_error());
                }
                // SAFETY: a descriptor just made, owned by nothing else.
                let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
                let watched = shared.watch(dir, mask, fd.as_raw_fd())?;
                return Ok(FileWatch {
                    fd,
                    shared: Some(shared),
                    dir: Some(watched),
                    file: None,
                });
            }
            // SAFETY: inotify_init1(2) with flags.
            let raw = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: a descriptor just made, owned by nothing else.
            let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
            // SAFETY: inotify_add_watch(2) with a NUL-terminated path.
            if unsafe { libc::inotify_add_watch(raw, notify::proc_path(dir)?.as_ptr(), mask) } < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(FileWatch {
                fd,
                shared: None,
                dir: None,
                file: None,
            })
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
            if let Some(shared) = self.shared {
                let watched = shared.watch(file, libc::IN_MODIFY, self.fd.as_raw_fd())?;
                // The one before let go of, past the new one's coming: a write to the file
                // watched before wakes it no more, nor does its watch's end (`IN_IGNORED`).
                if let Some(before) = self.file.replace(watched)
                    && before != watched
                {
                    shared.unwatch(before, self.fd.as_raw_fd());
                }
                return Ok(());
            }
            // SAFETY: inotify_add_watch(2) with a NUL-terminated path.
            let watched = unsafe {
                libc::inotify_add_watch(
                    self.fd.as_raw_fd(),
                    notify::proc_path(file)?.as_ptr(),
                    libc::IN_MODIFY,
                )
            };
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
            // An eventfd's count, or an instance's events: either read until none is left.
            let mut buf = [0u8; 4096];
            // SAFETY: read(2) into a local buffer of its size, from a non-blocking
            // descriptor.
            while unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
        }
    }
}

#[cfg(not(target_os = "macos"))]
impl Drop for FileWatch {
    fn drop(&mut self) {
        if let Some(shared) = self.shared {
            for watched in [self.dir.take(), self.file.take()].into_iter().flatten() {
                shared.unwatch(watched, self.fd.as_raw_fd());
            }
        }
    }
}

/// Watches `file` for writes, its truncations among them (inotify(7)'s `IN_MODIFY`), with
/// the process's one inotify instance ([`notify`]), calling `call` with the watch's number
/// on each, on the thread that reads the instance and under its lock: quick, and asking
/// nothing of the watch. A file watched twice is one watch, which calls each. The daemon's
/// watch on the files its microVMs' DAX windows map (D189), each the process's watch of
/// one instance, as a microVM a process each would take an instance each (PM M257).
#[cfg(target_os = "linux")]
pub fn watch_writes(file: &File, call: fn(libc::c_int)) -> io::Result<libc::c_int> {
    notify::shared()
        .ok_or_else(|| io::Error::other("the process's inotify instance could not be made"))?
        .watch_calling(file, libc::IN_MODIFY, call)
}

/// One of `watched`'s calls of `call` ([`watch_writes`]) made no more: the watch ends with
/// the last.
#[cfg(target_os = "linux")]
pub fn unwatch_writes(watched: libc::c_int, call: fn(libc::c_int)) {
    if let Some(shared) = notify::shared() {
        shared.unwatch_calling(watched, call);
    }
}

/// One inotify instance for the whole process (Linux). A user may have few: 128 by
/// default (`fs.inotify.max_user_instances`), shared with every program the user runs,
/// of which a KDE Plasma desktop's, a few apps open, held 101 (PM M257); and many watches
/// (`max_user_watches`, half a million there). With an instance each, a daemon took one
/// for each directory it watched and each follower of a log, and the followers past the
/// user's last instance failed. One thread reads the instance and counts each event into
/// the eventfd of each [`FileWatch`] that watches what it is of.
#[cfg(not(target_os = "macos"))]
mod notify {
    use std::collections::HashMap;
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
    use std::sync::{Mutex, OnceLock, PoisonError};

    #[derive(Debug)]
    pub struct Shared {
        fd: OwnedFd,
        /// What each watch wakes, by its descriptor: two watches of one file share one,
        /// and it goes with the last of what it wakes.
        waking: Mutex<HashMap<libc::c_int, Waking>>,
    }

    /// What a watch wakes: eventfds it counts its events into, and functions it calls with
    /// its descriptor ([`super::watch_writes`]).
    #[derive(Debug, Default)]
    struct Waking {
        fds: Vec<RawFd>,
        calls: Vec<fn(libc::c_int)>,
    }

    impl Waking {
        fn is_empty(&self) -> bool {
            self.fds.is_empty() && self.calls.is_empty()
        }
    }

    static SHARED: OnceLock<Shared> = OnceLock::new();
    static READER: OnceLock<bool> = OnceLock::new();

    /// The process's instance, its reader started the first time: none where either could
    /// not be made, and each watch then has an instance of its own, as before. An instance
    /// the user has none left for is asked again by the next watch: instances free up.
    pub fn shared() -> Option<&'static Shared> {
        let shared = match SHARED.get() {
            Some(shared) => shared,
            None => {
                // SAFETY: inotify_init1(2) with flags: its reader's reads wait.
                let raw = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
                if raw < 0 {
                    return None;
                }
                // SAFETY: a descriptor just made, owned by nothing else.
                let fd = unsafe { OwnedFd::from_raw_fd(raw) };
                // Another thread's, where it made one first: this one goes.
                let _ = SHARED.set(Shared {
                    fd,
                    waking: Mutex::new(HashMap::new()),
                });
                SHARED.get()?
            }
        };
        let started = *READER.get_or_init(|| {
            std::thread::Builder::new()
                .name("inotify".into())
                .spawn(move || shared.read())
                .is_ok()
        });
        started.then_some(shared)
    }

    /// `file`'s path through its descriptor's link in /proc (proc(5)), which inotify
    /// follows: the file itself, wherever it has gone.
    pub fn proc_path(file: &File) -> io::Result<std::ffi::CString> {
        std::ffi::CString::new(format!("/proc/self/fd/{}", file.as_raw_fd()))
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path with NUL"))
    }

    impl Shared {
        /// Watches `file` for `mask`, waking `wakes`: the watch's descriptor. Made under
        /// the lock the reader takes to count an event, so that none of the watch's comes
        /// before it is counted. A watch of a file watched already is that watch, its mask
        /// joined (`IN_MASK_ADD`).
        pub fn watch(&self, file: &File, mask: u32, wakes: RawFd) -> io::Result<libc::c_int> {
            let path = proc_path(file)?;
            let mut waking = self.waking.lock().unwrap_or_else(PoisonError::into_inner);
            // SAFETY: inotify_add_watch(2) with a NUL-terminated path.
            let watched = unsafe {
                libc::inotify_add_watch(self.fd.as_raw_fd(), path.as_ptr(), mask | libc::IN_MASK_ADD)
            };
            if watched < 0 {
                return Err(io::Error::last_os_error());
            }
            waking.entry(watched).or_default().fds.push(wakes);
            Ok(watched)
        }

        /// [`watch`](Self::watch), calling `call` with the watch's descriptor on each event
        /// instead, on the reader's thread and under its lock: it must be quick, and ask
        /// nothing of this instance.
        pub fn watch_calling(
            &self,
            file: &File,
            mask: u32,
            call: fn(libc::c_int),
        ) -> io::Result<libc::c_int> {
            let path = proc_path(file)?;
            let mut waking = self.waking.lock().unwrap_or_else(PoisonError::into_inner);
            // SAFETY: inotify_add_watch(2) with a NUL-terminated path.
            let watched = unsafe {
                libc::inotify_add_watch(self.fd.as_raw_fd(), path.as_ptr(), mask | libc::IN_MASK_ADD)
            };
            if watched < 0 {
                return Err(io::Error::last_os_error());
            }
            waking.entry(watched).or_default().calls.push(call);
            Ok(watched)
        }

        /// `wakes` woken by `watched` no more; the watch removed with the last it wakes.
        pub fn unwatch(&self, watched: libc::c_int, wakes: RawFd) {
            self.let_go(watched, |w| {
                w.fds.retain(|fd| *fd != wakes);
            });
        }

        /// One of the calls of `call` [`watch_calling`](Self::watch_calling) made, made no
        /// more; the watch removed with the last it wakes.
        pub fn unwatch_calling(&self, watched: libc::c_int, call: fn(libc::c_int)) {
            self.let_go(watched, |w| {
                if let Some(i) = w.calls.iter().position(|c| std::ptr::fn_addr_eq(*c, call)) {
                    w.calls.remove(i);
                }
            });
        }

        fn let_go(&self, watched: libc::c_int, what: impl FnOnce(&mut Waking)) {
            let mut waking = self.waking.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(w) = waking.get_mut(&watched) else {
                return;
            };
            what(w);
            if w.is_empty() {
                waking.remove(&watched);
                // SAFETY: inotify_rm_watch(2) of a watch of this instance's; one whose
                // file has gone went with it, and its removal fails harmlessly.
                unsafe { libc::inotify_rm_watch(self.fd.as_raw_fd(), watched) };
            }
        }

        /// The reader: each event counted into the eventfds its watch wakes; a watch that
        /// has ended (`IN_IGNORED`) let go of, its file gone. Its reads wait, and end only
        /// with the process.
        fn read(&self) {
            // Events are whole in a read: a header of 16 bytes, then a name of its length.
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                // SAFETY: read(2) into a buffer of its length.
                let n = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                let Ok(n) = usize::try_from(n) else {
                    if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return;
                };
                let mut waking = self.waking.lock().unwrap_or_else(PoisonError::into_inner);
                let mut at = 0;
                while let Some(header) = buf.get(at..at + 16).filter(|_| at + 16 <= n) {
                    let word = |i: usize| {
                        header
                            .get(i..i + 4)
                            .and_then(|b| <[u8; 4]>::try_from(b).ok())
                            .unwrap_or_default()
                    };
                    let watched = i32::from_ne_bytes(word(0));
                    let mask = u32::from_ne_bytes(word(4));
                    let len = u32::from_ne_bytes(word(12)) as usize;
                    if let Some(w) = waking.get(&watched) {
                        for fd in &w.fds {
                            let one = 1u64.to_ne_bytes();
                            // SAFETY: write(2) of 8 bytes to an eventfd, which never waits
                            // (EFD_NONBLOCK): a count past its most is one already pending.
                            unsafe { libc::write(*fd, one.as_ptr().cast(), one.len()) };
                        }
                        for call in &w.calls {
                            call(watched);
                        }
                    }
                    if mask & libc::IN_IGNORED != 0 {
                        waking.remove(&watched);
                    }
                    at = at.saturating_add(16).saturating_add(len);
                }
            }
        }
    }
}

/// What a process uses of the host: its resident memory and its own, in bytes, and the
/// CPU time it has had, user and system together, in nanoseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub resident: u64,
    /// Its own memory, as memory per VM is counted (PM M30): macOS's physical footprint,
    /// which counts a page a guest touched as long as the process holds it, mapped into the
    /// guest or not (PM M193); Linux's anonymous resident pages (`RssAnon`, proc(5)).
    pub footprint: u64,
    pub cpu_ns: u64,
}

/// Whether process `pid` has run anything of its own yet: `Some(false)` for one the host
/// has made and not yet let run, which has made no system call and spent no user time.
/// macOS admits each launch of an executable after it has assessed it, and the
/// executables launched before it (PM M166): a process waiting there runs nothing, and
/// what its stacks would show is nothing. `None` where it cannot be told, and on hosts
/// without such a wait.
pub fn launched(pid: u32) -> Option<bool> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: a zeroed proc_taskinfo is a valid out-parameter, which proc_pidinfo
        // fills up to its size.
        let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
        let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_taskinfo>()).ok()?;
        let pid = libc::c_int::try_from(pid).ok()?;
        // SAFETY: proc_pidinfo(3) writes at most `size` bytes into `info`.
        let got = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTASKINFO, 0, (&raw mut info).cast(), size) };
        if got != size {
            return None;
        }
        Some(info.pti_total_user > 0 || info.pti_syscalls_unix > 0 || info.pti_syscalls_mach > 0)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pid;
        None
    }
}

/// [`Usage`] of process `pid`, as the kernel keeps it: macOS's proc_pidinfo
/// (PROC_PIDTASKINFO), its times in Mach absolute-time units converted by the timebase,
/// and proc_pid_rusage (RUSAGE_INFO_V2); Linux's /proc/PID/stat (utime, stime, in clock
/// ticks), /proc/PID/statm (resident pages) and /proc/PID/status (`RssAnon`).
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
        // SAFETY: a zeroed rusage_info_v2 is a valid out-parameter for its flavor, which
        // proc_pid_rusage(2) fills.
        let mut rusage: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        if unsafe {
            libc::proc_pid_rusage(
                pid,
                libc::RUSAGE_INFO_V2,
                (&raw mut rusage).cast::<libc::rusage_info_t>(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Usage {
            resident: info.pti_resident_size,
            footprint: rusage.ri_phys_footprint,
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
        let anon_kib: u64 = std::fs::read_to_string(format!("/proc/{pid}/status"))?
            .lines()
            .find_map(|l| l.strip_prefix("RssAnon:"))
            .and_then(|v| v.trim().strip_suffix("kB"))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        Ok(Usage {
            resident: pages.saturating_mul(page_size()? as u64),
            footprint: anon_kib.saturating_mul(1024),
            cpu_ns: ticks.saturating_mul(1_000_000_000 / hz),
        })
    }
}

/// A thread's CPU clock, read from any thread of the process while the thread lives
/// (D191): on Linux its CPU-time clock (pthread_getcpuclockid(3)), its task's runtime,
/// which counts its time in a guest as any other (kernel/sched/core.c,
/// `task_sched_runtime`); on macOS its Mach port's thread_info(THREAD_EXTENDED_INFO),
/// which counts a vCPU's time in its guest too (PM M288).
#[derive(Debug, Clone, Copy)]
pub struct ThreadClock(
    #[cfg(target_os = "macos")] libc::mach_port_t,
    #[cfg(not(target_os = "macos"))] libc::clockid_t,
);

impl ThreadClock {
    /// The calling thread's.
    pub fn current() -> io::Result<ThreadClock> {
        #[cfg(target_os = "macos")]
        {
            // SAFETY: the calling thread's own port, which lives as long as it does.
            Ok(ThreadClock(unsafe {
                libc::pthread_mach_thread_np(libc::pthread_self())
            }))
        }
        #[cfg(not(target_os = "macos"))]
        {
            let mut id: libc::clockid_t = 0;
            // SAFETY: the calling thread's own handle, and a clock id to write.
            match unsafe { libc::pthread_getcpuclockid(libc::pthread_self(), &raw mut id) } {
                0 => Ok(ThreadClock(id)),
                e => Err(io::Error::from_raw_os_error(e)),
            }
        }
    }

    /// The CPU time its thread has used, in nanoseconds; an error once the thread is gone.
    pub fn read(&self) -> io::Result<u64> {
        #[cfg(target_os = "macos")]
        {
            // SAFETY: zeroed plain data, which thread_info fills.
            let mut info: libc::thread_extended_info = unsafe { std::mem::zeroed() };
            let mut count = libc::THREAD_EXTENDED_INFO_COUNT;
            // SAFETY: a port of a thread of this process; a buffer of `count` words.
            let r = unsafe {
                libc::thread_info(
                    self.0,
                    libc::THREAD_EXTENDED_INFO as libc::thread_flavor_t,
                    (&raw mut info).cast(),
                    &raw mut count,
                )
            };
            if r != libc::KERN_SUCCESS {
                return Err(io::Error::other(format!("thread_info: {r}")));
            }
            Ok(info.pth_user_time.saturating_add(info.pth_system_time))
        }
        #[cfg(not(target_os = "macos"))]
        {
            let mut t = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: a clock of a thread of this process, and a timespec to write.
            if unsafe { libc::clock_gettime(self.0, &raw mut t) } != 0 {
                return Err(io::Error::last_os_error());
            }
            let secs = u64::try_from(t.tv_sec).unwrap_or(0);
            let nanos = u64::try_from(t.tv_nsec).unwrap_or(0);
            Ok(secs.saturating_mul(1_000_000_000).saturating_add(nanos))
        }
    }
}

/// The host's monotonic clock in nanoseconds: "the monotonic clock for the system"
/// (POSIX, `clock_getres`, CLOCK_MONOTONIC), one reading in every process of the host, by
/// which the processes that share a budget mark its time alike (a group's CPU, D183).
pub fn monotonic_ns() -> io::Result<u64> {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: the system's monotonic clock, and a timespec to write.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let secs = u64::try_from(t.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(t.tv_nsec).unwrap_or(0);
    Ok(secs.saturating_mul(1_000_000_000).saturating_add(nanos))
}

/// This process's page faults, page-ins from files, copy-on-write faults and context
/// switches so far, every thread's (task_info(TASK_EVENTS_INFO), mach/task_info.h), each
/// a counter that wraps: what a stretch of a thread's work took from its host, told
/// apart, the disk, the faults themselves, or waits.
#[cfg(target_os = "macos")]
pub fn task_events() -> Option<[u64; 4]> {
    unsafe extern "C" {
        /// This task's port, which the SDK's `mach_task_self()` reads (mach/mach_init.h).
        static mach_task_self_: libc::mach_port_t;
    }
    /// `struct task_events_info` (mach/task_info.h), flavor TASK_EVENTS_INFO (2).
    #[repr(C)]
    #[derive(Default)]
    struct EventsInfo {
        faults: i32,
        pageins: i32,
        cow_faults: i32,
        messages_sent: i32,
        messages_received: i32,
        syscalls_mach: i32,
        syscalls_unix: i32,
        csw: i32,
    }
    let mut info = EventsInfo::default();
    let mut count = (std::mem::size_of::<EventsInfo>() / 4) as libc::mach_msg_type_number_t;
    // SAFETY: task_info(2) of our own task filling `info`, `count` words long.
    let kr = unsafe { libc::task_info(mach_task_self_, 2, (&raw mut info).cast(), &mut count) };
    let n = |v: i32| u64::from(v.cast_unsigned());
    (kr == libc::KERN_SUCCESS).then(|| [n(info.faults), n(info.pageins), n(info.cow_faults), n(info.csw)])
}

#[cfg(test)]
mod usage_tests {
    use super::*;

    #[test]
    fn a_process_is_measured_as_it_runs() {
        let me = std::process::id();
        let before = process_usage(me).unwrap();
        // CPU time, spent here until it shows (10 s at most): Linux counts it in clock
        // ticks (proc(5), `utime`), so work shorter than one may not; the work is opaque to
        // the optimizer, which would otherwise fold the loop away.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut after = process_usage(me).unwrap();
        let mut x = 0u64;
        while after.cpu_ns <= before.cpu_ns && std::time::Instant::now() < deadline {
            for i in 0..1_000_000u64 {
                x = std::hint::black_box(x.wrapping_add(i.wrapping_mul(i)));
            }
            after = process_usage(me).unwrap();
        }
        std::hint::black_box(x);
        assert!(after.resident > 0);
        assert!(after.footprint > 0);
        assert!(after.cpu_ns > before.cpu_ns, "{before:?} {after:?}");
    }

    /// A thread's clock, read from another thread, counts what it spins: about no more
    /// than the wall time, and on a host that lets it run, a share of it.
    #[test]
    fn a_threads_clock_counts_what_it_runs() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let stop = AtomicBool::new(false);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|s| {
            s.spawn(|| {
                tx.send(ThreadClock::current().unwrap()).unwrap();
                while !stop.load(Ordering::Relaxed) {
                    std::hint::spin_loop();
                }
            });
            let clock = rx.recv().unwrap();
            let (t0, before) = (std::time::Instant::now(), clock.read().unwrap());
            std::thread::sleep(std::time::Duration::from_millis(300));
            let spun = clock.read().unwrap() - before;
            let wall = u64::try_from(t0.elapsed().as_nanos()).unwrap();
            stop.store(true, Ordering::Relaxed);
            // A read may lag what its thread has run by what it ran since its last tick.
            assert!(spun <= wall + wall / 10, "{spun} ns of CPU in {wall} ns");
            assert!(spun >= wall / 10, "{spun} ns of CPU in {wall} ns");
        });
    }
}

#[cfg(all(test, target_os = "macos"))]
#[allow(clippy::unwrap_used)]
mod launched_tests {
    use super::*;

    /// A process made and not yet let run, as one waiting in its launch is (PM M166),
    /// has run nothing; this one, running, has.
    #[test]
    fn a_process_not_yet_run_is_told_from_one_running() {
        let path = std::ffi::CString::new("/usr/bin/true").unwrap();
        let argv = [path.as_ptr(), std::ptr::null()];
        let env: [*const libc::c_char; 1] = [std::ptr::null()];
        // SAFETY: an attribute object initialized, set and destroyed here; posix_spawn(2)
        // of a NUL-terminated path with NULL-terminated argv and envp.
        let pid = unsafe {
            let mut attr: libc::posix_spawnattr_t = std::ptr::null_mut();
            assert_eq!(libc::posix_spawnattr_init(&mut attr), 0);
            let flags = libc::c_short::try_from(libc::POSIX_SPAWN_START_SUSPENDED).unwrap();
            assert_eq!(libc::posix_spawnattr_setflags(&mut attr, flags), 0);
            let mut pid: libc::pid_t = 0;
            let rc = libc::posix_spawn(
                &mut pid,
                path.as_ptr(),
                std::ptr::null(),
                &attr,
                argv.as_ptr().cast_mut().cast(),
                env.as_ptr().cast_mut().cast(),
            );
            libc::posix_spawnattr_destroy(&mut attr);
            assert_eq!(rc, 0);
            pid
        };
        let suspended = launched(u32::try_from(pid).unwrap());
        // SAFETY: kill(2) and waitpid(2) of the child made above.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            libc::waitpid(pid, std::ptr::null_mut(), 0);
        }
        assert_eq!(suspended, Some(false));
        assert_eq!(launched(std::process::id()), Some(true));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod holes_tests {
    use super::*;

    /// A shared writable mapping of a file hides its holes from lseek(2) where
    /// [`MAPPED_HIDES_HOLES`] says this host's file system does (PM M285), and they come
    /// back once the mapping is gone.
    #[test]
    fn a_mapping_hides_a_files_holes_where_the_platform_says() {
        let dir = shards_testdir::TempDir::new("holes").unwrap();
        let len = 64usize << 20;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(dir.join("sparse"))
            .unwrap();
        file.set_len(len as u64).unwrap();
        crate::platform::write_all_at(&file, &[1; 16 << 10], 32 << 20).unwrap();
        file.sync_all().unwrap();
        let alone = data_ranges(&file, len as u64).unwrap();
        assert_eq!(alone, [(32 << 20, 16 << 10)]);
        let mapped = map_file_shared(&file, len).unwrap();
        let hidden = data_ranges(&file, len as u64).unwrap();
        // SAFETY: the mapping just made, which nothing else uses.
        unsafe { unmap_file_shared(mapped, len) };
        assert_eq!(hidden == [(0, len as u64)], MAPPED_HIDES_HOLES, "{hidden:?}");
        assert_eq!(data_ranges(&file, len as u64).unwrap(), alone);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod beneath_tests {
    use super::*;
    use std::io::Read as _;

    /// What a VM process can leave in the directory of a snapshot it saves, before it
    /// gives the directory up (PM M165), opened beneath it as a snapshot's files are: a
    /// regular file is read; a link to a file outside, a link to a directory on the way
    /// to one, a FIFO and a name that leaves are each refused, the FIFO unopened (opened,
    /// it would wait here for a writer); a file not there is not found.
    #[test]
    fn a_snapshots_files_are_opened_beneath_it_alone() {
        let root_dir = shards_testdir::TempDir::new("beneath").unwrap();
        let root = root_dir.join("beneath");
        let _ = std::fs::remove_dir_all(&root);
        let outside = root.with_extension("outside");
        std::fs::create_dir_all(root.join("g-1")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("g-1/state"), b"state").unwrap();
        std::fs::write(outside.join("memory"), b"secret").unwrap();
        std::os::unix::fs::symlink(outside.join("memory"), root.join("g-1/memory")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("g-2")).unwrap();
        let fifo = std::ffi::CString::new(root.join("g-1/working-set").into_os_string().into_encoded_bytes())
            .unwrap();
        // SAFETY: mkfifo(2) of a NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);

        let mut read = String::new();
        open_beneath(&root, &["g-1", "state"])
            .unwrap()
            .read_to_string(&mut read)
            .unwrap();
        assert_eq!(read, "state");
        // A writer waits on the FIFO until a reader opens it: the FIFO refused, it waits on.
        let (tx, rx) = std::sync::mpsc::channel();
        let writer_path = root.join("g-1/working-set");
        let writer = std::thread::spawn(move || {
            let opened = std::fs::OpenOptions::new().write(true).open(&writer_path);
            let _ = tx.send(opened.is_ok());
        });
        assert_eq!(
            open_beneath(&root, &["g-1", "working-set"]).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(200)).is_err(),
            "the FIFO was opened for reading"
        );
        // Let the writer go.
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            let _reader = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(root.join("g-1/working-set"))
                .unwrap();
            assert!(rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap());
        }
        writer.join().unwrap();
        for (rel, kind) in [
            (&["g-1", "memory"][..], io::ErrorKind::InvalidData),
            (&["g-1", "working-set"][..], io::ErrorKind::InvalidData),
            (&["g-1", "absent"][..], io::ErrorKind::NotFound),
            (&["..", "outside"][..], io::ErrorKind::InvalidInput),
            (&["g-1/state"][..], io::ErrorKind::InvalidInput),
        ] {
            assert_eq!(open_beneath(&root, rel).unwrap_err().kind(), kind, "{rel:?}");
        }
        // A link to a directory on the way: refused by the open itself (ELOOP).
        assert!(open_beneath(&root, &["g-2", "memory"]).is_err());
        assert!(open_dir_beneath(&root, &["g-2"]).is_err());
        assert!(open_dir_beneath(&root, &["g-1"]).is_ok());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// What opening a snapshot's four files beneath its directory costs a restore, against
    /// opening them by path as it did (PM M165): the pointer, state, memory and working
    /// set, alternately one way and the other, ROUNDS times (`BENEATH_ROUNDS`, default
    /// 20,000); nanoseconds a round.
    #[test]
    #[ignore = "a measurement: cargo test -p shards-vmm --release --lib -- --ignored --nocapture what_opening"]
    fn what_opening_beneath_costs() {
        let root_dir = shards_testdir::TempDir::new("beneath-cost").unwrap();
        let root = root_dir.join("beneath-cost");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("g-1")).unwrap();
        std::fs::write(root.join("current"), b"g-1\n").unwrap();
        for name in ["state", "memory", "working-set"] {
            std::fs::write(root.join("g-1").join(name), b"x").unwrap();
        }
        let rounds: usize = std::env::var("BENEATH_ROUNDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(20_000);
        let files: [&[&str]; 4] = [
            &["current"],
            &["g-1", "state"],
            &["g-1", "memory"],
            &["g-1", "working-set"],
        ];
        let (mut by_path, mut beneath) = (Vec::with_capacity(rounds), Vec::with_capacity(rounds));
        for i in 0..rounds {
            let path_round = || {
                let t = std::time::Instant::now();
                for rel in files {
                    let p = rel.iter().fold(root.clone(), |p, n| p.join(n));
                    drop(std::fs::File::open(p).unwrap());
                }
                t.elapsed().as_nanos()
            };
            let beneath_round = || {
                let t = std::time::Instant::now();
                for rel in files {
                    drop(open_beneath(&root, rel).unwrap());
                }
                t.elapsed().as_nanos()
            };
            if i % 2 == 0 {
                by_path.push(path_round());
                beneath.push(beneath_round());
            } else {
                beneath.push(beneath_round());
                by_path.push(path_round());
            }
        }
        let stats = |v: &mut Vec<u128>| {
            v.sort_unstable();
            let q = |p: f64| v[((v.len() - 1) as f64 * p) as usize];
            (q(0.5), q(0.9), q(0.99), v[v.len() - 1])
        };
        println!("n {rounds}, ns a round of four files: p50, p90, p99, max");
        println!("by path:  {:?}", stats(&mut by_path));
        println!("beneath:  {:?}", stats(&mut beneath));
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
#[cfg(target_os = "linux")]
mod fault_around_tests {
    use std::os::unix::fs::FileExt as _;

    use super::*;

    /// The kernel's version, major and minor, as uname(2) gives its release.
    fn kernel() -> (u32, u32) {
        // SAFETY: uname(2) fills the struct it is given.
        let mut name: libc::utsname = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        assert_eq!(unsafe { libc::uname(&mut name) }, 0);
        // SAFETY: uname(2) leaves the release NUL-terminated.
        let release = unsafe { std::ffi::CStr::from_ptr(name.release.as_ptr()) }.to_string_lossy();
        let mut parts = release
            .split(|c: char| !c.is_ascii_digit())
            .filter_map(|p| p.parse().ok());
        (parts.next().unwrap(), parts.next().unwrap())
    }

    /// Held off, the host's fault-around maps the page of a file a read touches alone,
    /// where on it maps those around it its page cache holds; let go, it maps them again
    /// (PM M312). A host that cannot hold it off is one before 6.7, or one whose process
    /// is filtered already (a container's seccomp profile refuses userfaultfd).
    #[test]
    fn fault_around_held_off_maps_the_touched_page_alone() {
        let dir = shards_testdir::TempDir::new("fault-around").unwrap();
        let page = page_size().unwrap();
        let len = 64 * page;
        let path = dir.join("file");
        std::fs::write(&path, vec![0x5a; len]).unwrap();
        let file = File::open(&path).unwrap();
        // Its pages in the page cache, as a snapshot's just written are.
        file.read_exact_at(&mut vec![0; len], 0).unwrap();
        // SAFETY: a fresh private mapping of the file, read here alone.
        let host = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(host, libc::MAP_FAILED);
        let host = host.cast::<u8>();
        let mapped = || mapped_pages(host, len).unwrap().len();
        // SAFETY: within the mapping.
        let touch = |n: usize| unsafe { std::ptr::read_volatile(host.add(n * page)) };
        let Some(off) = FaultAroundOff::over(&[(host, len)]) else {
            let filtered = std::fs::read_to_string("/proc/self/status")
                .unwrap()
                .lines()
                .any(|l| l.starts_with("Seccomp:") && l.trim_end() != "Seccomp:\t0");
            assert!(
                kernel() < (6, 7) || filtered,
                "Linux {:?} with no filter of its own would not hold fault-around off",
                kernel()
            );
            eprintln!("SKIP: this host cannot hold its fault-around off");
            return;
        };
        let before = mapped();
        assert_eq!(touch(40), 0x5a);
        assert_eq!(mapped(), before + 1, "held off, a read mapped more than its page");
        drop(off);
        let before = mapped();
        assert_eq!(touch(8), 0x5a);
        let on = mapped() - before;
        // SAFETY: the mapping made above, which nothing uses any more.
        assert_eq!(unsafe { libc::munmap(host.cast(), len) }, 0);
        eprintln!("fault-around on again mapped {on} pages for one read");
        assert!(on >= 1);
    }
}
