use std::fs::File;
use std::io;
use std::ops::Range;
use std::os::windows::io::AsRawHandle;
use std::ptr::NonNull;

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::ERROR_HANDLE_EOF;
use windows_sys::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FlushFileBuffers, GetDiskFreeSpaceExW, ReadFile, WriteFile,
};
use windows_sys::Win32::System::IO::OVERLAPPED;
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FlushViewOfFile, MEM_COMMIT, MEM_DECOMMIT, MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE,
    MEM_REPLACE_PLACEHOLDER, MEM_RESERVE, MEM_RESERVE_PLACEHOLDER, MEMORY_MAPPED_VIEW_ADDRESS,
    MapViewOfFile3, PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE, PAGE_WRITECOPY, UnmapViewOfFile2,
    VirtualAlloc, VirtualAlloc2, VirtualFree, VirtualProtect,
};
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// Waits until any of `sockets` has something to read, its end, or an error (WSAPoll,
/// which reports a peer's half-close as a hangup, PM M199), or `timeout` passes: which
/// did. For the few sockets one thread waits on at a time.
pub fn wait_readable(
    sockets: &[super::RawSocket],
    timeout: Option<std::time::Duration>,
) -> io::Result<Vec<bool>> {
    use windows_sys::Win32::Networking::WinSock::{POLLRDNORM, WSAPOLLFD, WSAPoll};
    let mut polled: Vec<WSAPOLLFD> = sockets
        .iter()
        .map(|&fd| WSAPOLLFD {
            fd,
            events: POLLRDNORM,
            revents: 0,
        })
        .collect();
    // Rounded up, so that a wait never ends early.
    let ms = timeout.map_or(-1, |t| {
        i32::try_from(t.as_micros().div_ceil(1000)).unwrap_or(i32::MAX)
    });
    // SAFETY: `polled.len()` entries of sockets the caller holds.
    if unsafe { WSAPoll(polled.as_mut_ptr(), polled.len() as u32, ms) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(polled.iter().map(|p| p.revents != 0).collect())
}

/// Makes `dir` and its missing parents. Under `%LOCALAPPDATA%`, they inherit ACLs that
/// admit this user alone.
pub fn create_private_dir(dir: &std::path::Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)
}

pub fn page_size() -> io::Result<usize> {
    let mut info = SYSTEM_INFO::default();
    // SAFETY: GetSystemInfo fills the provided struct.
    unsafe { GetSystemInfo(&mut info) };
    let p = info.dwPageSize as usize;
    if p.is_power_of_two() {
        Ok(p)
    } else {
        Err(io::Error::other(format!("GetSystemInfo reported page size {p}")))
    }
}

/// Reserves and commits `len` bytes of zero-filled read/write memory. Physical pages are
/// supplied on first touch; the commit charge is taken up front.
pub fn reserve(len: usize) -> io::Result<NonNull<u8>> {
    // SAFETY: fresh allocation owned by the caller until `release`.
    let p = unsafe { VirtualAlloc(std::ptr::null(), len, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
    NonNull::new(p.cast()).ok_or_else(io::Error::last_os_error)
}

/// Reserves guest RAM, committed and zero-filled as [`reserve`]'s: a placeholder
/// (VirtualAlloc2) that committed read/write memory replaces at once, so that a restore can
/// free it back to its placeholder and place a view of its memory file at the same address,
/// leaving no moment in which another thread could take the range ([`map_file_private`], PM
/// M345). Windows backs large pages only for callers holding SeLockMemoryPrivilege, which
/// shards does not ask for.
pub fn reserve_ram(len: usize) -> io::Result<NonNull<u8>> {
    // SAFETY: the pseudo-handle of this process, which needs no closing.
    let process = unsafe { GetCurrentProcess() };
    // SAFETY: a fresh placeholder, which only this function references.
    let base = unsafe {
        VirtualAlloc2(
            process,
            std::ptr::null(),
            len,
            MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
            PAGE_NOACCESS,
            std::ptr::null_mut(),
            0,
        )
    };
    let base = NonNull::new(base.cast::<u8>()).ok_or_else(io::Error::last_os_error)?;
    // SAFETY: committed memory replacing the whole placeholder, at its base and size.
    let committed = unsafe {
        VirtualAlloc2(
            process,
            base.as_ptr().cast(),
            len,
            MEM_RESERVE | MEM_COMMIT | MEM_REPLACE_PLACEHOLDER,
            PAGE_READWRITE,
            std::ptr::null_mut(),
            0,
        )
    };
    if committed != base.as_ptr().cast() {
        let e = io::Error::last_os_error();
        // SAFETY: the placeholder made above, which nothing references.
        unsafe { VirtualFree(base.as_ptr().cast(), 0, MEM_RELEASE) };
        return Err(e);
    }
    Ok(base)
}

/// Guest RAM on base pages, as `reserve_ram`'s always is here: nothing to do.
pub fn small_pages(_ptr: NonNull<u8>, _len: usize) {}

pub fn huge_pages(_ptr: NonNull<u8>, _len: usize) {}

/// A file's mapping read without readahead, where the host gives a way (unix.rs); a
/// Windows host runs no writable layers (D190).
pub fn reads_at_random(_ptr: NonNull<u8>, _len: usize) {}

pub fn reads_ahead(_ptr: NonNull<u8>, _len: usize) {}

pub fn advised_huge_page() -> Option<usize> {
    None
}

pub fn thp_always() -> bool {
    false
}

/// Gives the pages of `ptr..ptr+len` back and leaves the range inaccessible: decommitted,
/// still reserved (VirtualFree MEM_DECOMMIT).
///
/// # Safety
/// `ptr..ptr+len` must be page-aligned, within one region from [`reserve`] or
/// [`reserve_ram`], and accessed by no host thread meanwhile.
pub unsafe fn withhold(ptr: NonNull<u8>, len: usize) -> io::Result<()> {
    // SAFETY: decommits part of the caller's own region.
    if unsafe { VirtualFree(ptr.as_ptr().cast(), len, MEM_DECOMMIT) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Reserves `len` bytes of guest RAM of which the host gives nothing yet, each part
/// [`grant`]ed as it is given out (memory to plug, D133): reserved alone (MEM_RESERVE), as
/// [`withhold`] leaves a region. Reserved and committed first, as [`reserve`] does, it took
/// its whole length's commit charge, which a host short of it refused ("The paging file is
/// too small", ERROR_COMMITMENT_LIMIT, on jupiter under load).
pub fn reserve_withheld(len: usize) -> io::Result<NonNull<u8>> {
    // SAFETY: a fresh reservation owned by the caller until `release`.
    let p = unsafe { VirtualAlloc(std::ptr::null(), len, MEM_RESERVE, PAGE_NOACCESS) };
    NonNull::new(p.cast()).ok_or_else(io::Error::last_os_error)
}

/// Commits `ptr..ptr+len`, which [`withhold`] decommitted, again: read/write, zero-filled.
///
/// # Safety
/// As for [`withhold`].
pub unsafe fn grant(ptr: NonNull<u8>, len: usize) -> io::Result<()> {
    // SAFETY: commits part of the caller's own reserved region.
    let p = unsafe { VirtualAlloc(ptr.as_ptr().cast(), len, MEM_COMMIT, PAGE_READWRITE) };
    if p.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Gives the pages of `ptr..ptr+len` back and leaves the range readable and writable,
/// zero-filled: decommitted and committed again ([`withhold`], [`grant`]). Guest RAM the
/// guest holds free (D151).
///
/// # Safety
/// As for [`withhold`].
pub unsafe fn discard(ptr: NonNull<u8>, len: usize) -> io::Result<()> {
    // SAFETY: forwarded caller contract.
    unsafe { withhold(ptr, len) }?;
    // SAFETY: as above.
    unsafe { grant(ptr, len) }
}

/// Gives back what [`reserve`] or [`reserve_ram`] made at `ptr`, `len` bytes: its memory, or
/// the views of a file [`map_file_private`] or [`map_file_views`] placed there since, which
/// are unmapped rather than freed.
///
/// # Safety
/// `ptr` must come from `reserve` or `reserve_ram` and not be used afterwards.
pub unsafe fn release(ptr: NonNull<u8>, len: usize) {
    use windows_sys::Win32::System::Memory::{MEM_FREE, MEM_MAPPED, MEMORY_BASIC_INFORMATION, VirtualQuery};
    // Each allocation of the range in turn: a reservation's memory or placeholder, or a
    // view of a file, of which a restore's RAM is many ([`map_file_views`]).
    let (mut at, end) = (ptr.as_ptr() as usize, ptr.as_ptr() as usize + len.max(1));
    while at < end {
        let mut info = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: a query of this process's own address, into a value of its own size.
        let queried = unsafe {
            VirtualQuery(
                at as *const _,
                &raw mut info,
                size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if queried == 0 || info.RegionSize == 0 {
            return;
        }
        // SAFETY: forwarded caller contract; a view is unmapped whole, and MEM_RELEASE
        // frees an allocation or a placeholder whole, given its base and size 0.
        unsafe {
            if info.State != MEM_FREE && info.Type == MEM_MAPPED {
                UnmapViewOfFile2(
                    GetCurrentProcess(),
                    MEMORY_MAPPED_VIEW_ADDRESS {
                        Value: info.AllocationBase,
                    },
                    0,
                );
            } else if info.State != MEM_FREE {
                VirtualFree(info.AllocationBase, 0, MEM_RELEASE);
            }
        }
        at += info.RegionSize;
    }
}

/// The path of the file `file` has open, links resolved (GetFinalPathNameByHandleW): what a
/// VM's spawner tells it of each file it hands it, as an AppContainer may not ask (PM M203).
pub fn final_path(file: &File) -> io::Result<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt as _;
    use windows_sys::Win32::Storage::FileSystem::{FILE_NAME_NORMALIZED, GetFinalPathNameByHandleW};
    let mut buf = vec![0u16; 1024];
    loop {
        // SAFETY: an open handle, and a buffer of the length given.
        let n = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                buf.as_mut_ptr(),
                u32::try_from(buf.len()).unwrap_or(u32::MAX),
                FILE_NAME_NORMALIZED,
            )
        } as usize;
        if n == 0 {
            return Err(io::Error::last_os_error());
        }
        // Too small: `n` is the length it needs, its NUL counted.
        if n >= buf.len() {
            buf.resize(n + 1, 0);
            continue;
        }
        buf.truncate(n);
        return Ok(std::path::PathBuf::from(std::ffi::OsString::from_wide(&buf)));
    }
}

/// The `size` bytes of `file` read-only, then zeros, `len` bytes in all at one address; and
/// how many of them are the file's view. A placeholder of `len` bytes (VirtualAlloc2), split
/// where the file's last whole page ends: its whole pages a view of the file
/// (MapViewOfFile3, MEM_REPLACE_PLACEHOLDER: page-aligned, its base and size the
/// placeholder's), whose pages every VM that maps the file shares in the cache, as on Unix;
/// the rest committed memory, the file's last partial page copied into it, then read-only.
/// A view may not run past its section's end, which is the file's size: one rounded up to
/// a page is refused (ERROR_ACCESS_DENIED, CI run 38217982469). `len` is page-aligned.
pub fn map_file_padded(file: &File, size: u64, len: usize) -> io::Result<(NonNull<u8>, usize)> {
    let page = page_size()?;
    let size = usize::try_from(size)
        .ok()
        .filter(|&s| s > 0 && s <= len && len.is_multiple_of(page))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{size} bytes of a file in {len}"),
            )
        })?;
    let whole = size - size % page;
    let tail = len - whole;
    // SAFETY: the pseudo-handle of this process, which needs no closing.
    let process = unsafe { GetCurrentProcess() };
    // SAFETY: a fresh placeholder, which only this function references.
    let base = unsafe {
        VirtualAlloc2(
            process,
            std::ptr::null(),
            len,
            MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
            PAGE_NOACCESS,
            std::ptr::null_mut(),
            0,
        )
    };
    let base = NonNull::new(base.cast::<u8>()).ok_or_else(io::Error::last_os_error)?;
    // Frees, on a failure, what is at `base` and at `base + whole`: a view, an allocation
    // or a placeholder, each released whole.
    let undo = |viewed: bool| {
        // SAFETY: placeholders, views and allocations this function made, which nothing
        // references.
        unsafe {
            if viewed {
                UnmapViewOfFile2(
                    process,
                    MEMORY_MAPPED_VIEW_ADDRESS {
                        Value: base.as_ptr().cast(),
                    },
                    0,
                );
            } else {
                VirtualFree(base.as_ptr().cast(), 0, MEM_RELEASE);
            }
            if whole > 0 && tail > 0 {
                VirtualFree(base.as_ptr().add(whole).cast(), 0, MEM_RELEASE);
            }
        }
    };
    if whole > 0 && tail > 0 {
        // SAFETY: splits our own placeholder in two at a page boundary within it.
        let split = unsafe {
            VirtualFree(
                base.as_ptr().cast(),
                whole,
                MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
            )
        };
        if split == 0 {
            let e = io::Error::last_os_error();
            // SAFETY: the placeholder, still whole.
            unsafe { VirtualFree(base.as_ptr().cast(), 0, MEM_RELEASE) };
            return Err(e);
        }
    }
    if whole > 0 {
        // SAFETY: a read-only section of the whole file, closed once its view holds it.
        let section = unsafe {
            CreateFileMappingW(
                file.as_raw_handle(),
                std::ptr::null(),
                PAGE_READONLY,
                0,
                0,
                std::ptr::null(),
            )
        };
        if section.is_null() {
            let e = io::Error::last_os_error();
            undo(false);
            return Err(e);
        }
        // SAFETY: the view replaces the first placeholder, at its base and of its size,
        // within the section.
        let view = unsafe {
            MapViewOfFile3(
                section,
                process,
                base.as_ptr().cast(),
                0,
                whole,
                MEM_REPLACE_PLACEHOLDER,
                PAGE_READONLY,
                std::ptr::null_mut(),
                0,
            )
        };
        let failed = view.Value.is_null().then(io::Error::last_os_error);
        // SAFETY: the section handle, which the view (if any) keeps alive by itself.
        unsafe { CloseHandle(section) };
        if let Some(e) = failed {
            undo(false);
            return Err(e);
        }
    }
    if tail > 0 {
        // SAFETY: committed zeros replace the last placeholder, at its base and size.
        let rest = unsafe {
            VirtualAlloc2(
                process,
                base.as_ptr().add(whole).cast(),
                tail,
                MEM_RESERVE | MEM_COMMIT | MEM_REPLACE_PLACEHOLDER,
                PAGE_READWRITE,
                std::ptr::null_mut(),
                0,
            )
        };
        if rest.is_null() {
            let e = io::Error::last_os_error();
            undo(whole > 0);
            return Err(e);
        }
        let mut copied = whole;
        while copied < size {
            // SAFETY: within the committed tail, which nothing else references yet.
            let n = match unsafe { read_at(file, base.as_ptr().add(copied), size - copied, copied as u64) } {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    undo(whole > 0);
                    return Err(e);
                }
            };
            copied += n;
        }
        let mut old = 0u32;
        // SAFETY: the committed tail, read-only from here on.
        if unsafe { VirtualProtect(base.as_ptr().add(whole).cast(), tail, PAGE_READONLY, &raw mut old) } == 0
        {
            let e = io::Error::last_os_error();
            undo(whole > 0);
            return Err(e);
        }
    }
    Ok((base, whole))
}

/// Unmaps what [`map_file_padded`] mapped: the view, then what follows it.
///
/// # Safety
/// `ptr`, `viewed` and `len` are a mapping's from [`map_file_padded`], which nothing uses
/// any more: no hypervisor maps it.
pub unsafe fn unmap_file_padded(ptr: NonNull<u8>, viewed: usize, len: usize) {
    // SAFETY: forwarded caller contract.
    unsafe {
        if viewed > 0 {
            UnmapViewOfFile2(
                GetCurrentProcess(),
                MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: ptr.as_ptr().cast(),
                },
                0,
            );
        }
        if len > viewed {
            VirtualFree(ptr.as_ptr().add(viewed).cast(), 0, MEM_RELEASE);
        }
    }
}

/// The first `len` bytes of `file`, opened for writing, mapped shared and writable: a
/// read-write section of the file and a view of it, whose writes are the file's through
/// the cache (a run's swap, D159). `len` is page-aligned and within the file.
pub fn map_file_shared(file: &File, len: usize) -> io::Result<NonNull<u8>> {
    // SAFETY: a read-write section of the whole file, closed once its view holds it.
    let section = unsafe {
        CreateFileMappingW(
            file.as_raw_handle(),
            std::ptr::null(),
            PAGE_READWRITE,
            0,
            0,
            std::ptr::null(),
        )
    };
    if section.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh view of the section, which only the caller references.
    let view = unsafe {
        MapViewOfFile3(
            section,
            GetCurrentProcess(),
            std::ptr::null(),
            0,
            len,
            0,
            PAGE_READWRITE,
            std::ptr::null_mut(),
            0,
        )
    };
    let failed = view.Value.is_null().then(io::Error::last_os_error);
    // SAFETY: the section handle, which the view (if any) keeps alive by itself.
    unsafe { CloseHandle(section) };
    if let Some(e) = failed {
        return Err(e);
    }
    NonNull::new(view.Value.cast::<u8>()).ok_or_else(|| io::Error::other("MapViewOfFile3 returned NULL"))
}

/// Writes what was written through `ptr..ptr + len`, a view [`map_file_shared`] made, to
/// its file (FlushViewOfFile).
///
/// # Safety
/// `ptr` and `len` are a view's from [`map_file_shared`], still mapped.
pub unsafe fn flush_shared(ptr: NonNull<u8>, len: usize) -> io::Result<()> {
    // SAFETY: forwarded caller contract.
    if unsafe { FlushViewOfFile(ptr.as_ptr().cast(), len) } != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Makes what was written to `file` durable, through a view of it at `ptr..ptr+len`
/// ([`map_file_shared`]) too: the view's pages written to the file (FlushViewOfFile), then
/// the file to its device (FlushFileBuffers).
///
/// # Safety
/// `ptr..ptr+len` is the view [`map_file_shared`] made of `file`, still mapped.
pub unsafe fn sync_mapped(file: &File, ptr: NonNull<u8>, len: usize) -> io::Result<()> {
    // SAFETY: forwarded caller contract.
    unsafe { flush_shared(ptr, len) }?;
    sync_durable(file)
}

/// Whether a mapping of a file hides its holes (unix.rs): its ranges are all of it here
/// ([`data_ranges`]).
pub const MAPPED_HIDES_HOLES: bool = false;

/// Maps a view of `file` at `at` again (unix.rs): never asked, as no mapping hides a
/// file's holes here ([`MAPPED_HIDES_HOLES`]).
///
/// # Safety
/// None: this never touches memory.
pub unsafe fn map_file_shared_at(_file: &File, _len: usize, _at: NonNull<u8>) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

/// The ranges of `file`'s first `len` bytes that hold data ([`allocated_ranges`]): as its
/// spawner finds them, where it stages this process's snapshots, as a confined process may
/// make no FSCTL (D141). A snapshot's copy of a writable layer copies them alone, where all
/// of a layer's length, its holes too, took past a minute (D196).
pub fn data_ranges(file: &File, len: u64) -> io::Result<Vec<(u64, u64)>> {
    use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
    match super::stager() {
        Some(by) => by.data_ranges(file, len),
        None => match allocated_ranges(file, len) {
            // A process that may make no FSCTL and has no spawner left to ask: all of it,
            // as a copy then reads its holes as zeros and writes none of them.
            Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => Ok(vec![(0, len)]),
            other => other,
        },
    }
}

/// The ranges of `file`'s first `len` bytes that its file system holds allocated
/// (FSCTL_QUERY_ALLOCATED_RANGES): a sparse file's holes left out, and a file not sparse all
/// of it, as the call's documentation says.
pub fn allocated_ranges(file: &File, len: u64) -> io::Result<Vec<(u64, u64)>> {
    use windows_sys::Win32::Foundation::ERROR_MORE_DATA;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{FILE_ALLOCATED_RANGE_BUFFER, FSCTL_QUERY_ALLOCATED_RANGES};
    let end = i64::try_from(len).map_err(io::Error::other)?;
    let size = size_of::<FILE_ALLOCATED_RANGE_BUFFER>();
    let mut found = vec![FILE_ALLOCATED_RANGE_BUFFER::default(); 64];
    let mut ranges = Vec::new();
    let mut from = 0i64;
    while from < end {
        let asked = FILE_ALLOCATED_RANGE_BUFFER {
            FileOffset: from,
            Length: end - from,
        };
        let mut returned = 0u32;
        // SAFETY: a range asked of a file this process holds, and room for what answers it.
        let done = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                FSCTL_QUERY_ALLOCATED_RANGES,
                (&raw const asked).cast(),
                u32::try_from(size).map_err(io::Error::other)?,
                found.as_mut_ptr().cast(),
                u32::try_from(found.len() * size).map_err(io::Error::other)?,
                &raw mut returned,
                std::ptr::null_mut(),
            )
        };
        let more = if done != 0 {
            false
        } else {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(ERROR_MORE_DATA as i32) {
                return Err(e);
            }
            true
        };
        let got = found.get(..returned as usize / size).unwrap_or_default();
        for r in got {
            let (at, n) = (u64::try_from(r.FileOffset), u64::try_from(r.Length));
            ranges.push((at.map_err(io::Error::other)?, n.map_err(io::Error::other)?));
        }
        match got.last() {
            Some(last) if more => from = last.FileOffset.saturating_add(last.Length),
            _ => break,
        }
    }
    Ok(ranges)
}

/// Unmaps what [`map_file_shared`] mapped.
///
/// # Safety
/// `ptr` and `len` are a mapping's from [`map_file_shared`], which nothing uses any more:
/// no hypervisor maps it.
pub unsafe fn unmap_file_shared(ptr: NonNull<u8>, _len: usize) {
    // SAFETY: forwarded caller contract.
    unsafe {
        UnmapViewOfFile2(
            GetCurrentProcess(),
            MEMORY_MAPPED_VIEW_ADDRESS {
                Value: ptr.as_ptr().cast(),
            },
            0,
        );
    }
}

/// Maps `len` bytes of `file` from `offset` copy-on-write at `at`, where that is the whole
/// of a region [`reserve_ram`] made: its memory freed back to its placeholder (VirtualFree,
/// MEM_PRESERVE_PLACEHOLDER) and a copy-on-write view of the file placed there
/// (MapViewOfFile3, PAGE_WRITECOPY, MEM_REPLACE_PLACEHOLDER), whose pages every restore of
/// the file shares in the cache until one writes a page, its own copy; the file is never
/// written (PM M345). Unsupported where `offset` is not on the allocation granularity, at
/// which every view starts, and where the range is no such region, a part of one or of a
/// [`reserve`]'s, which Windows frees back to no placeholder (ERROR_INVALID_ADDRESS): the
/// caller then reads the file in, the range untouched.
///
/// # Safety
/// `at..at + len` must be within a reservation of [`reserve`]'s or [`reserve_ram`]'s, which
/// no hypervisor maps and nothing references.
pub unsafe fn map_file_private(file: &File, offset: u64, len: usize, at: NonNull<u8>) -> io::Result<()> {
    let mut info = SYSTEM_INFO::default();
    // SAFETY: GetSystemInfo fills the provided struct.
    unsafe { GetSystemInfo(&mut info) };
    if !offset.is_multiple_of(u64::from(info.dwAllocationGranularity.max(1))) {
        return Err(io::ErrorKind::Unsupported.into());
    }
    // SAFETY: the pseudo-handle of this process, which needs no closing.
    let process = unsafe { GetCurrentProcess() };
    // Committed memory again in the placeholder, as `reserve_ram` left it, where the view
    // could not be placed.
    let recommit = || {
        // SAFETY: the placeholder this function freed the region back to.
        unsafe {
            VirtualAlloc2(
                process,
                at.as_ptr().cast(),
                len,
                MEM_RESERVE | MEM_COMMIT | MEM_REPLACE_PLACEHOLDER,
                PAGE_READWRITE,
                std::ptr::null_mut(),
                0,
            )
        };
    };
    // SAFETY: forwarded caller contract: memory of this process's reservations, freed back
    // to its placeholder only where it is the whole of an allocation that replaced one; else
    // the call frees nothing.
    if unsafe { VirtualFree(at.as_ptr().cast(), len, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER) } == 0 {
        return Err(io::ErrorKind::Unsupported.into());
    }
    // SAFETY: a copy-on-write section of the whole file, closed once its view holds it.
    let section = unsafe {
        CreateFileMappingW(
            file.as_raw_handle(),
            std::ptr::null(),
            PAGE_WRITECOPY,
            0,
            0,
            std::ptr::null(),
        )
    };
    if section.is_null() {
        let e = io::Error::last_os_error();
        recommit();
        return Err(e);
    }
    // SAFETY: the view replaces the placeholder, at its base and of its size, within the
    // section, which the caller's file holds.
    let view = unsafe {
        MapViewOfFile3(
            section,
            process,
            at.as_ptr().cast(),
            offset,
            len,
            MEM_REPLACE_PLACEHOLDER,
            PAGE_WRITECOPY,
            std::ptr::null_mut(),
            0,
        )
    };
    let failed = (view.Value != at.as_ptr().cast()).then(io::Error::last_os_error);
    // SAFETY: the section handle, which the view (if any) keeps alive by itself.
    unsafe { CloseHandle(section) };
    if let Some(e) = failed {
        recommit();
        return Err(e);
    }
    Ok(())
}

/// Guest RAM mapped from a file in parts ([`map_file_views`]): each run of the file that
/// holds data a copy-on-write view of it, and each stretch between them memory of its own,
/// zeros, as the file holds there; and the file's section, held so that a view is mapped
/// from it again as its pages are given back ([`give_back`](Self::give_back)).
#[derive(Debug)]
pub struct FileViews {
    section: Option<std::os::windows::io::OwnedHandle>,
    parts: Vec<Part>,
}

/// A part of [`FileViews`]: where it is and how long, and the offset in the file of what it
/// views, none for memory of its own.
#[derive(Debug, Clone, Copy)]
struct Part {
    at: usize,
    len: usize,
    file: Option<u64>,
}

/// The parts of `len` bytes at `base` that `data`'s runs, from its start, make: each run a
/// view of the file from `offset` on, and each stretch between them memory of its own.
fn parts_of(base: usize, offset: u64, len: usize, data: &[Range<usize>]) -> io::Result<Vec<Part>> {
    let mut parts = Vec::new();
    let mut next = 0;
    for run in data {
        if run.start < next || run.start >= run.end || run.end > len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("data at {run:?} of {len} bytes, after {next}"),
            ));
        }
        if run.start > next {
            parts.push(Part {
                at: base + next,
                len: run.start - next,
                file: None,
            });
        }
        parts.push(Part {
            at: base + run.start,
            len: run.len(),
            file: Some(offset + run.start as u64),
        });
        next = run.end;
    }
    if next < len {
        parts.push(Part {
            at: base + next,
            len: len - next,
            file: None,
        });
    }
    Ok(parts)
}

/// Committed memory in the placeholder at `at`, `len` long, as [`reserve_ram`] commits its
/// own: whether it took.
///
/// # Safety
/// `at..at + len` is a placeholder of this process, whole.
unsafe fn commit_placeholder(at: usize, len: usize) -> bool {
    // SAFETY: forwarded caller contract; the pseudo-handle of this process.
    let placed = unsafe {
        VirtualAlloc2(
            GetCurrentProcess(),
            at as *const _,
            len,
            MEM_RESERVE | MEM_COMMIT | MEM_REPLACE_PLACEHOLDER,
            PAGE_READWRITE,
            std::ptr::null_mut(),
            0,
        )
    };
    placed as usize == at
}

/// Maps `len` bytes of `file` from `offset` at `at`, where [`reserve_ram`] reserved them, as
/// `data`'s runs of them, from their start, say the file holds: each run a copy-on-write
/// view of the file, as [`map_file_private`] maps one, and each stretch between them memory
/// of its own, zeros, as the file holds there. A restore's RAM: its template's guest's data
/// viewed, shared with every restore of it until written, and the rest its guest's alone,
/// which gives its pages back at their own grain (decommitted and committed again, M371),
/// where Windows gives a view's written pages back only as the whole view is unmapped (M371,
/// M375). Its committed memory freed back to its placeholder, split into one a part, and
/// each replaced by its view or by committed memory: as many parts as runs of data and
/// stretches between them, three of a template's 280 MiB on the M5 Max, where views of every
/// 2 MiB took 307.7 µs for 256 MiB at p50 (M372). Unsupported as `map_file_private` is, the
/// range kept committed memory, for the caller to read the file into.
///
/// # Safety
/// As for [`map_file_private`].
pub unsafe fn map_file_views(
    file: &File,
    offset: u64,
    len: usize,
    at: NonNull<u8>,
    data: &[Range<usize>],
) -> io::Result<FileViews> {
    use std::os::windows::io::{FromRawHandle as _, OwnedHandle};
    let mut info = SYSTEM_INFO::default();
    // SAFETY: GetSystemInfo fills the provided struct.
    unsafe { GetSystemInfo(&mut info) };
    let granularity = u64::from(info.dwAllocationGranularity.max(1));
    let base = at.as_ptr() as usize;
    let parts = parts_of(base, offset, len, data)?;
    // MapViewOfFile3 takes a view's offset and address on the allocation granularity.
    if parts.iter().any(|p| {
        p.file
            .is_some_and(|f| !f.is_multiple_of(granularity) || !(p.at as u64).is_multiple_of(granularity))
    }) {
        return Err(io::ErrorKind::Unsupported.into());
    }
    // Nothing of the file's: its committed memory, zeros, as it is.
    if parts.iter().all(|p| p.file.is_none()) {
        return Ok(FileViews { section: None, parts });
    }
    // SAFETY: the pseudo-handle of this process, which needs no closing.
    let process = unsafe { GetCurrentProcess() };
    // SAFETY: a copy-on-write section of the whole file, held for the views mapped again.
    let section = unsafe {
        CreateFileMappingW(
            file.as_raw_handle(),
            std::ptr::null(),
            PAGE_WRITECOPY,
            0,
            0,
            std::ptr::null(),
        )
    };
    if section.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a handle this function owns, closed once as it drops.
    let section = unsafe { OwnedHandle::from_raw_handle(section) };
    // SAFETY: forwarded caller contract: the whole of an allocation that replaced a
    // placeholder, freed back to it; else the call frees nothing.
    if unsafe { VirtualFree(at.as_ptr().cast(), len, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER) } == 0 {
        return Err(io::ErrorKind::Unsupported.into());
    }
    // Split into one placeholder a part; where one cannot be, committed memory again in
    // each split off and in the rest, whole.
    for (k, p) in parts.iter().enumerate() {
        if p.at + p.len < base + len {
            // SAFETY: the placeholder's first `p.len` bytes split off as one of their own.
            if unsafe { VirtualFree(p.at as *mut _, p.len, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER) } == 0 {
                let e = io::Error::last_os_error();
                for q in parts.iter().take(k) {
                    // SAFETY: a placeholder split off above.
                    unsafe { commit_placeholder(q.at, q.len) };
                }
                // SAFETY: the placeholder left, from this part on.
                unsafe { commit_placeholder(p.at, base + len - p.at) };
                return Err(e);
            }
        }
    }
    for (i, p) in parts.iter().enumerate() {
        let placed = match p.file {
            // SAFETY: the view replaces its placeholder, at its base and of its size, within
            // the section, which the caller's file holds.
            Some(from) => unsafe {
                MapViewOfFile3(
                    section.as_raw_handle(),
                    process,
                    p.at as *const _,
                    from,
                    p.len,
                    MEM_REPLACE_PLACEHOLDER,
                    PAGE_WRITECOPY,
                    std::ptr::null_mut(),
                    0,
                )
                .Value as usize
                    == p.at
            },
            // SAFETY: the part's placeholder, split off above.
            None => unsafe { commit_placeholder(p.at, p.len) },
        };
        if !placed {
            let e = io::Error::last_os_error();
            // Committed memory again where the parts could not all be placed: each view
            // placed unmapped to its placeholder first, and the memory placed kept.
            for (j, q) in parts.iter().enumerate() {
                if j < i && q.file.is_none() {
                    continue;
                }
                // SAFETY: a view placed above, unmapped to its placeholder; then each
                // placeholder committed.
                unsafe {
                    if j < i {
                        UnmapViewOfFile2(
                            process,
                            MEMORY_MAPPED_VIEW_ADDRESS {
                                Value: q.at as *mut _,
                            },
                            MEM_PRESERVE_PLACEHOLDER,
                        );
                    }
                    commit_placeholder(q.at, q.len);
                }
            }
            return Err(e);
        }
    }
    Ok(FileViews {
        section: Some(section),
        parts,
    })
}

impl FileViews {
    /// Gives back what each part holds within `ptr..ptr + len`: memory of its own
    /// decommitted and committed again ([`discard`]), zeros, at its pages' grain; and a view
    /// lying wholly within it mapped again from the file (UnmapViewOfFile2,
    /// MEM_PRESERVE_PLACEHOLDER; MapViewOfFile3, MEM_REPLACE_PLACEHOLDER), its written pages
    /// gone and the file's bytes back (M375: a written 2 MiB view so at p50 405 µs on
    /// jupiter, under a partition that maps it). A view only part of which lies within it
    /// keeps its pages. Guest RAM its guest reports free, whose stage 2 its caller has taken
    /// away first and gives back after. A view whose mapping again fails holds committed
    /// memory, zeros: its pages were free. The bytes given back.
    ///
    /// # Safety
    /// `ptr..ptr + len` lies within the parts, whole pages, which no hypervisor maps and no
    /// thread accesses meanwhile.
    pub unsafe fn give_back(&self, ptr: NonNull<u8>, len: usize) -> io::Result<usize> {
        // SAFETY: the pseudo-handle of this process, which needs no closing.
        let process = unsafe { GetCurrentProcess() };
        let from = ptr.as_ptr() as usize;
        let to = from.saturating_add(len);
        let mut given = 0;
        for p in &self.parts {
            let (start, end) = (p.at.max(from), (p.at + p.len).min(to));
            if start >= end {
                continue;
            }
            let Some(offset) = p.file else {
                let at = NonNull::new(start as *mut u8).ok_or(io::ErrorKind::InvalidInput)?;
                // SAFETY: whole pages of the part's memory, forwarded caller contract.
                unsafe { discard(at, end - start) }?;
                given += end - start;
                continue;
            };
            if start != p.at || end != p.at + p.len {
                continue;
            }
            let section = self
                .section
                .as_ref()
                .ok_or_else(|| io::Error::other("a view without its file's section"))?;
            // SAFETY: a view `map_file_views` placed, unmapped to its placeholder.
            let unmapped = unsafe {
                UnmapViewOfFile2(
                    process,
                    MEMORY_MAPPED_VIEW_ADDRESS {
                        Value: p.at as *mut _,
                    },
                    MEM_PRESERVE_PLACEHOLDER,
                )
            };
            if unmapped == 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: the view mapped again into its placeholder, from the section.
            let mapped = unsafe {
                MapViewOfFile3(
                    section.as_raw_handle(),
                    process,
                    p.at as *const _,
                    offset,
                    p.len,
                    MEM_REPLACE_PLACEHOLDER,
                    PAGE_WRITECOPY,
                    std::ptr::null_mut(),
                    0,
                )
            };
            if mapped.Value as usize != p.at {
                let e = io::Error::last_os_error();
                // SAFETY: the view's placeholder, left by the unmap above.
                if !unsafe { commit_placeholder(p.at, p.len) } {
                    return Err(e);
                }
            }
            given += p.len;
        }
        Ok(given)
    }
}

/// File views need placeholders on Windows too (see `map_file_private`).
///
/// # Safety
/// None: this never touches memory.
pub unsafe fn map_file_readonly(_file: &File, _len: usize, _at: NonNull<u8>) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

/// Windows hosts are x86_64 or arm64 under Hyper-V, which keeps guest memory coherent.
///
/// # Safety
/// None: this never touches memory.
pub unsafe fn clean_dcache(_ptr: *const u8, _len: usize) {}

pub fn fill_random(buf: &mut [u8]) -> io::Result<()> {
    for chunk in buf.chunks_mut(u32::MAX as usize) {
        // SAFETY: writes `chunk.len()` (≤ u32::MAX) bytes into `chunk`.
        let status = unsafe {
            BCryptGenRandom(
                std::ptr::null_mut(),
                chunk.as_mut_ptr(),
                chunk.len() as u32,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        };
        if status != 0 {
            return Err(io::Error::other(format!("BCryptGenRandom: NTSTATUS {status:#x}")));
        }
    }
    Ok(())
}

fn at(offset: u64) -> OVERLAPPED {
    let mut o = OVERLAPPED::default();
    o.Anonymous.Anonymous.Offset = offset as u32;
    o.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
    o
}

/// Reads up to `len` bytes at `offset` into raw memory; 0 at end of file.
///
/// # Safety
/// `dst` must be valid for writes of `len` bytes for the duration of the call.
pub unsafe fn read_at(file: &File, dst: *mut u8, len: usize, offset: u64) -> io::Result<usize> {
    let mut o = at(offset);
    let mut n = 0u32;
    let len = len.min(u32::MAX as usize) as u32;
    // SAFETY: forwarded caller contract; the handle is synchronous, so the read completes
    // before `o` goes out of scope.
    if unsafe { ReadFile(file.as_raw_handle(), dst, len, &mut n, &mut o) } == 0 {
        let e = io::Error::last_os_error();
        // A synchronous positional read at or past end of file fails with ERROR_HANDLE_EOF.
        return match e.raw_os_error() {
            Some(code) if code == ERROR_HANDLE_EOF as i32 => Ok(0),
            _ => Err(e),
        };
    }
    Ok(n as usize)
}

/// Writes up to `len` bytes from raw memory at `offset`.
///
/// # Safety
/// `src` must be valid for reads of `len` bytes for the duration of the call.
pub unsafe fn write_at(file: &File, src: *const u8, len: usize, offset: u64) -> io::Result<usize> {
    let mut o = at(offset);
    let mut n = 0u32;
    let len = len.min(u32::MAX as usize) as u32;
    // SAFETY: forwarded caller contract; synchronous handle, as in `read_at`.
    if unsafe { WriteFile(file.as_raw_handle(), src, len, &mut n, &mut o) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

/// Opens the directory at `path`: a directory opens only for backup semantics
/// (CreateFileW).
pub fn open_dir(path: &std::path::Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

/// Which pages of anonymous memory were never touched: Windows is not asked yet, so none
/// are skipped (unix.rs `untouched`).
///
/// # Safety
/// As unix.rs `untouched`.
pub unsafe fn untouched(_ptr: *const u8, _len: usize, _page: usize) -> io::Result<Option<Vec<bool>>> {
    Ok(None)
}

/// Whether every page of a range was never touched (unix.rs `all_untouched`): Windows is not
/// asked, as [`untouched`] is not.
///
/// # Safety
/// As unix.rs `untouched`.
pub unsafe fn all_untouched(_ptr: *const u8, _len: usize, _page: usize) -> io::Result<Option<bool>> {
    Ok(None)
}

/// Faults pages in writable ahead of a restored guest (unix.rs `populate_writable`): asked
/// only for a working set's written pages, which a restore on Windows does not record yet
/// (vm::WORKING_SETS, D196).
pub fn populate_writable(_ptr: *mut u8, _len: usize) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "pages are not faulted in ahead on Windows: its restores record no working set yet",
    ))
}

/// The host's fault-around held off over ranges (unix.rs `FaultAroundOff`), so that a
/// working set recorded or mapped ahead holds what the guest touched alone: none on
/// Windows, whose restores record no working set yet (vm::WORKING_SETS, D196).
#[derive(Debug)]
pub struct FaultAroundOff;

impl FaultAroundOff {
    pub fn over(_ranges: &[(*mut u8, usize)]) -> Option<FaultAroundOff> {
        None
    }
}

/// Which file `file` is: its volume's serial number and its index on that volume
/// (GetFileInformationByHandle), as a Unix file's device and inode say. A handle opened for
/// no access may ask it, as `std::fs::metadata`'s is opened, where a confined process may
/// read nothing of the file (D141).
pub fn file_id(file: &File) -> io::Result<(u64, u64)> {
    use windows_sys::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle};
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: a buffer of its own type, for a handle this process holds.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &raw mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((
        u64::from(info.dwVolumeSerialNumber),
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}

/// The file at `path` opened for no access: what may still be asked of it where reading it
/// is denied, its metadata (CreateFileW: "query certain metadata ... even if GENERIC_READ
/// access would have been denied"), as `std::fs::metadata` opens one.
pub fn open_for_metadata(path: &std::path::Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

/// Makes `file` keep what is never written to it as holes (FSCTL_SET_SPARSE): NTFS gives
/// a file not so marked every byte up to the last it was written, zeros included, and
/// writes those zeros as it goes. A snapshot's memory file, most of it all-zero pages left
/// unwritten, so takes the disk its guest used (PM M345). One marked already asks nothing:
/// a VM process may make no FSCTL (D141's mitigations), and its spawner marks the memory
/// file it hands it as it makes it (`snapshot::prepare`).
pub fn sparse(file: &File) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt as _;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_SPARSE_FILE;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;
    if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_SPARSE_FILE != 0 {
        return Ok(());
    }
    let mut returned = 0u32;
    // SAFETY: an FSCTL that takes no buffer, on a handle this process holds.
    let set = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_SET_SPARSE,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &raw mut returned,
            std::ptr::null_mut(),
        )
    };
    if set == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Flushes file data and metadata to stable storage.
pub fn sync_durable(file: &File) -> io::Result<()> {
    // SAFETY: flushing an owned, open handle.
    if unsafe { FlushFileBuffers(file.as_raw_handle()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The volume holding `path`: the bytes this user may still write there, and its size
/// (GetDiskFreeSpaceExW's counts for the caller, which honor quotas).
pub fn disk_space(path: &std::path::Path) -> io::Result<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let (mut available, mut total) = (0u64, 0u64);
    // SAFETY: a NUL-terminated wide path, and locals for the two counts asked for.
    if unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, &mut total, std::ptr::null_mut()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((available, total))
}

/// Runs the calling thread at THREAD_PRIORITY_TIME_CRITICAL, base priority 15 in its
/// process's NORMAL_PRIORITY_CLASS against its other threads' 8: as it becomes ready it
/// preempts them, "without allowing it to finish using its time slice" (Microsoft,
/// Scheduling Priorities). For a thread that runs briefly between waits, as that page
/// reserves high priority for (PM M294).
pub fn run_time_critical() -> Result<(), String> {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
    };
    // SAFETY: the calling thread's pseudo-handle, which is not closed.
    if unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) } == 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    Ok(())
}

/// The host's monotonic clock in nanoseconds (QueryPerformanceCounter), whose results "are
/// consistent across all processors in multi-core and multi-processor systems, even when
/// measured on different threads or processes" ("Acquiring high-resolution time stamps",
/// Microsoft Learn): by which the processes that share a budget mark its time alike (a
/// group's CPU, D183).
pub fn monotonic_ns() -> io::Result<u64> {
    use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    let (mut count, mut frequency) = (0i64, 0i64);
    // SAFETY: a count to write.
    if unsafe { QueryPerformanceCounter(&raw mut count) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a frequency to write.
    if unsafe { QueryPerformanceFrequency(&raw mut frequency) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let ns = u128::from(count.unsigned_abs()).saturating_mul(1_000_000_000)
        / u128::from(frequency.unsigned_abs()).max(1);
    Ok(u64::try_from(ns).unwrap_or(u64::MAX))
}

/// A thread's CPU clock, read from any thread of the process while the thread lives
/// (D191): its kernel and user times (GetThreadTimes), which Windows counts by its clock's
/// ticks. The handle it holds is closed as it goes.
#[derive(Debug)]
pub struct ThreadClock(windows_sys::Win32::Foundation::HANDLE);

// SAFETY: a thread handle opened for querying alone, which GetThreadTimes reads from any
// thread; nothing else uses it.
unsafe impl Send for ThreadClock {}
// SAFETY: as above: reading the times through a shared handle changes nothing.
unsafe impl Sync for ThreadClock {}

impl ThreadClock {
    /// The calling thread's.
    pub fn current() -> io::Result<ThreadClock> {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThreadId, OpenThread, THREAD_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: opens the calling thread, by its id, to query it alone.
        let handle = unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, GetCurrentThreadId()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(ThreadClock(handle))
    }

    /// The CPU time its thread has used, in nanoseconds.
    pub fn read(&self) -> io::Result<u64> {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::System::Threading::GetThreadTimes;
        let zero = || FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut created, mut exited, mut kernel, mut user) = (zero(), zero(), zero(), zero());
        // SAFETY: a handle this value owns, and four times to write.
        if unsafe { GetThreadTimes(self.0, &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // In 100 ns units.
        let ns = |t: FILETIME| {
            ((u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)).saturating_mul(100)
        };
        Ok(ns(kernel).saturating_add(ns(user)))
    }
}

impl Drop for ThreadClock {
    fn drop(&mut self) {
        // SAFETY: the handle this value opened, closed once.
        unsafe { CloseHandle(self.0) };
    }
}

/// The stream of a share another server serves (D38) on Windows: an end of a duplex named
/// pipe the server made (slates' daemon) and this process's spawner duplicated into it, as
/// AF_UNIX can make no pair without a socket file and the pipe loses nothing at its
/// writer's exit (PM M340). The device reads and writes it blocking, one request at a time
/// under its slot's lock, as it does a Unix socket. So every call is an overlapped
/// ReadFile or WriteFile waited for at once (GetOverlappedResult): a handle opened for
/// overlapped I/O, as slates opens its end, requires an OVERLAPPED, and a synchronous one
/// completes the call before it returns, so the VM need not know how its server opened the
/// end. One event serves every call, since each comes through `&mut self`. A read past the
/// server's going is the end, `Ok(0)`, as a Unix socket's: every byte it wrote is read
/// first, then ERROR_BROKEN_PIPE (M340).
#[derive(Debug)]
pub struct PipeStream {
    pipe: std::os::windows::io::OwnedHandle,
    /// The manual-reset event every call's OVERLAPPED carries.
    event: std::os::windows::io::OwnedHandle,
}

impl PipeStream {
    /// Takes `pipe`, an end of a pipe, as a share's stream.
    pub fn from_handle(pipe: std::os::windows::io::OwnedHandle) -> io::Result<PipeStream> {
        use std::os::windows::io::FromRawHandle as _;
        use windows_sys::Win32::System::Threading::CreateEventW;
        // SAFETY: an unnamed manual-reset event, not signaled, which this value owns.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the event just made, owned from here on.
        let event = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(event) };
        Ok(PipeStream { pipe, event })
    }

    /// An OVERLAPPED that signals this stream's event.
    fn overlapped(&self) -> OVERLAPPED {
        OVERLAPPED {
            hEvent: self.event.as_raw_handle(),
            ..OVERLAPPED::default()
        }
    }

    /// Waits for the call `started` began on `overlapped`: the bytes it moved, or its error.
    /// A read of a message longer than its buffer, from an end in message-read mode, moves
    /// what fits and leaves the rest of the message for the next read, failing with
    /// ERROR_MORE_DATA (ReadFile's documentation). That is a read of those bytes here, so the
    /// device reads an end in either mode as it reads a byte stream.
    fn finish(&self, started: i32, overlapped: &mut OVERLAPPED) -> io::Result<usize> {
        use windows_sys::Win32::Foundation::{ERROR_IO_PENDING, ERROR_MORE_DATA, GetLastError};
        use windows_sys::Win32::System::IO::GetOverlappedResult;
        let failed = |error: u32| io::Error::from_raw_os_error(i32::try_from(error).unwrap_or(i32::MAX));
        if started == 0 {
            // SAFETY: no arguments.
            let error = unsafe { GetLastError() };
            if error != ERROR_IO_PENDING && error != ERROR_MORE_DATA {
                return Err(failed(error));
            }
        }
        let mut moved = 0u32;
        // SAFETY: the OVERLAPPED of the call just begun on this handle, waited for here (bWait),
        // so neither it nor the call's buffer is touched once this returns. A call already
        // finished, ERROR_MORE_DATA's among them, is answered at once with what it moved.
        if unsafe { GetOverlappedResult(self.pipe.as_raw_handle(), overlapped, &mut moved, 1) } == 0 {
            // SAFETY: no arguments.
            let error = unsafe { GetLastError() };
            if error != ERROR_MORE_DATA {
                return Err(failed(error));
            }
        }
        Ok(usize::try_from(moved).unwrap_or(usize::MAX))
    }
}

/// Its handle's value in this process: what a DAX window's ask names as the share's end of
/// the stream, for its server to check before it duplicates a section here (D189, PM M343).
impl std::os::windows::io::AsRawHandle for PipeStream {
    fn as_raw_handle(&self) -> std::os::windows::io::RawHandle {
        self.pipe.as_raw_handle()
    }
}

impl io::Read for PipeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        use windows_sys::Win32::Foundation::{ERROR_BROKEN_PIPE, ERROR_PIPE_NOT_CONNECTED};
        let mut overlapped = self.overlapped();
        let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        // SAFETY: a buffer of `len` bytes and an OVERLAPPED, each kept until `finish` has waited
        // for the call.
        let started = unsafe {
            ReadFile(
                self.pipe.as_raw_handle(),
                buf.as_mut_ptr(),
                len,
                std::ptr::null_mut(),
                &mut overlapped,
            )
        };
        match self.finish(started, &mut overlapped) {
            Err(e)
                if e.raw_os_error().is_some_and(|code| {
                    u32::try_from(code)
                        .is_ok_and(|code| code == ERROR_BROKEN_PIPE || code == ERROR_PIPE_NOT_CONNECTED)
                }) =>
            {
                Ok(0)
            }
            other => other,
        }
    }
}

impl io::Write for PipeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut overlapped = self.overlapped();
        let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        // SAFETY: as `read`'s.
        let started = unsafe {
            WriteFile(
                self.pipe.as_raw_handle(),
                buf.as_ptr(),
                len,
                std::ptr::null_mut(),
                &mut overlapped,
            )
        };
        self.finish(started, &mut overlapped)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
pub mod pipe_stream {
    //! A share's stream over a real pipe pair, made as a server makes one for a VM (PM M340);
    //! [`pair`] for the device's tests too.
    use std::io::{Read as _, Write as _};
    use std::os::windows::io::{FromRawHandle as _, OwnedHandle};

    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    };
    use windows_sys::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_READMODE_MESSAGE, PIPE_REJECT_REMOTE_CLIENTS,
        PIPE_TYPE_BYTE, PIPE_TYPE_MESSAGE, PIPE_WAIT,
    };

    use super::PipeStream;

    /// A connected byte-mode pair: the server's end and the VM's, each overlapped
    /// (`overlapped`) or not.
    pub fn pair(overlapped: bool) -> (PipeStream, PipeStream) {
        pair_in(overlapped, PIPE_TYPE_BYTE | PIPE_READMODE_BYTE)
    }

    /// A connected pair whose server end is made in `mode`, a pipe type and read mode.
    fn pair_in(overlapped: bool, mode: u32) -> (PipeStream, PipeStream) {
        static MADE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let name: Vec<u16> = format!(
            r"\\.\pipe\shards-vmm-test-{}-{}",
            std::process::id(),
            MADE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )
        .encode_utf16()
        .chain([0])
        .collect();
        let flag = if overlapped { FILE_FLAG_OVERLAPPED } else { 0 };
        // SAFETY: a NUL-terminated name and plain arguments; checked below.
        let server = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | flag,
                mode | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                4096,
                4096,
                0,
                std::ptr::null(),
            )
        };
        assert_ne!(server, INVALID_HANDLE_VALUE);
        // SAFETY: as above.
        let client = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                flag,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(client, INVALID_HANDLE_VALUE);
        // SAFETY: each handle just made, owned from here on.
        let (server, client) = unsafe {
            (
                OwnedHandle::from_raw_handle(server),
                OwnedHandle::from_raw_handle(client),
            )
        };
        (
            PipeStream::from_handle(server).unwrap(),
            PipeStream::from_handle(client).unwrap(),
        )
    }

    /// A message longer than a read, from an end in message-read mode, comes in pieces of a
    /// read's worth that join into it whole: ERROR_MORE_DATA is a read of what fitted, on
    /// overlapped ends and synchronous ones alike.
    #[test]
    fn a_message_longer_than_a_read_comes_in_pieces() {
        for overlapped in [true, false] {
            let (mut server, mut vm) = pair_in(overlapped, PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE);
            let message: Vec<u8> = (0..100u8).collect();
            vm.write_all(&message).unwrap();
            let mut got = Vec::new();
            let mut piece = [0u8; 16];
            while got.len() < message.len() {
                let read = server.read(&mut piece).unwrap();
                assert!(
                    read > 0 && read <= piece.len(),
                    "overlapped {overlapped}: a piece of {read}"
                );
                got.extend_from_slice(&piece[..read]);
            }
            assert_eq!(got, message, "overlapped {overlapped}: the message whole");
        }
    }

    /// Frames go each way whole, on overlapped ends and on synchronous ones alike, and a
    /// frame longer than the pipe's buffers arrives whole.
    #[test]
    fn frames_go_each_way_whole_on_either_kind_of_end() {
        for overlapped in [true, false] {
            let (mut server, mut vm) = pair(overlapped);
            let request: Vec<u8> = (0..64 * 1024).map(|i| (i % 251) as u8).collect();
            let sent = request.clone();
            let writer = std::thread::spawn(move || {
                vm.write_all(&sent).unwrap();
                vm
            });
            let mut got = vec![0u8; request.len()];
            server.read_exact(&mut got).unwrap();
            assert_eq!(got, request, "overlapped {overlapped}: the request whole");
            let mut vm = writer.join().unwrap();
            server.write_all(b"reply").unwrap();
            let mut reply = [0u8; 5];
            vm.read_exact(&mut reply).unwrap();
            assert_eq!(&reply, b"reply", "overlapped {overlapped}: the reply whole");
        }
    }

    /// A server that writes and goes is read whole, then its end: Ok(0), so read_exact's
    /// UnexpectedEof is the end of the stream, as a Unix socket's (M340).
    #[test]
    fn a_server_that_goes_is_read_whole_then_ends() {
        let (mut server, mut vm) = pair(true);
        server.write_all(b"last words").unwrap();
        drop(server);
        let mut got = [0u8; 10];
        vm.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"last words");
        let mut more = [0u8; 1];
        assert_eq!(vm.read(&mut more).unwrap(), 0, "the end, as Ok(0)");
        let error = vm.read_exact(&mut more).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    /// A write to a server that has gone is an error, as a Unix socket's.
    #[test]
    fn a_write_to_a_server_that_went_fails() {
        let (server, mut vm) = pair(true);
        drop(server);
        assert!(vm.write_all(b"anyone").is_err());
    }
}
