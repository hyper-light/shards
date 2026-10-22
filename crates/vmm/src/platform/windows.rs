use std::fs::File;
use std::io;
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
    MapViewOfFile3, PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE, UnmapViewOfFile2, VirtualAlloc,
    VirtualAlloc2, VirtualFree, VirtualProtect,
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

/// Reserves guest RAM. Windows backs large pages only for callers holding
/// SeLockMemoryPrivilege, which shards does not ask for, so this is `reserve`.
pub fn reserve_ram(len: usize) -> io::Result<NonNull<u8>> {
    reserve(len)
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

/// # Safety
/// `ptr` must come from `reserve` or `reserve_ram` and not be used afterwards.
pub unsafe fn release(ptr: NonNull<u8>, _len: usize) {
    // SAFETY: forwarded caller contract; MEM_RELEASE requires size 0.
    unsafe { VirtualFree(ptr.as_ptr().cast(), 0, MEM_RELEASE) };
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

/// The ranges of `file`'s first `len` bytes that hold data: all of it, as no Windows
/// backend takes snapshots, whose saves alone ask (`vm::SNAPSHOTS`).
pub fn data_ranges(_file: &File, len: u64) -> io::Result<Vec<(u64, u64)>> {
    Ok(vec![(0, len)])
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

/// A file view cannot replace part of a `VirtualAlloc` region without placeholders
/// (VirtualAlloc2 + MapViewOfFile3), so callers fall back to reading.
///
/// # Safety
/// None: this never touches memory.
pub unsafe fn map_file_private(_file: &File, _offset: u64, _len: usize, _at: NonNull<u8>) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
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

/// A snapshot's file beneath `root` (unix.rs `open_beneath`). No snapshots are read on
/// Windows.
pub fn open_beneath_input(_root: &std::path::Path, _rel: &[&str]) -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "a snapshot's files are not read on Windows",
    ))
}

/// A snapshot's directories beneath `root` (unix.rs `open_dir_beneath`). No snapshots are
/// written on Windows.
pub fn open_dir_beneath(_root: &std::path::Path, _rel: &[&str]) -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "a snapshot's directories are not opened on Windows",
    ))
}

/// Opens `name` in the directory `dir` holds open. No snapshots are read on Windows.
pub fn open_in(_dir: &File, _name: &str) -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "opening a file in an open directory is not supported on Windows",
    ))
}

/// Which pages of anonymous memory were never touched: Windows is not asked yet, so none
/// are skipped (unix.rs `untouched`).
///
/// # Safety
/// As unix.rs `untouched`.
pub unsafe fn untouched(_ptr: *const u8, _len: usize, _page: usize) -> io::Result<Option<Vec<bool>>> {
    Ok(None)
}

/// Faults pages in writable ahead of a restored guest (unix.rs `populate_writable`): no
/// snapshot is restored on Windows yet (vm::SNAPSHOTS).
pub fn populate_writable(_ptr: *mut u8, _len: usize) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "pages are not faulted in ahead on Windows: it restores no snapshot yet",
    ))
}

/// The pages mapped of a range, for a restore's working set (unix.rs `mapped_pages`): no
/// snapshot is restored on Windows yet (vm::SNAPSHOTS).
pub fn mapped_pages(_ptr: *const u8, _len: usize) -> io::Result<Vec<(usize, bool)>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "working sets are not recorded on Windows: it restores no snapshot yet",
    ))
}

/// The names in the directory `dir` holds open. No container logs are kept on Windows.
pub fn names_in(_dir: &File) -> io::Result<Vec<std::ffi::OsString>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "listing an open directory is not supported on Windows",
    ))
}

/// Creates `name` in the directory `dir` holds open. No snapshots are written on Windows.
pub fn write_in(_dir: &File, _name: &str, _bytes: &[u8]) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "writing into an open directory is not supported on Windows",
    ))
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
