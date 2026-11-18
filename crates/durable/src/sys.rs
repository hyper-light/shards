//! Each OS's calls: the durable writes themselves, and what a declaration and its slot file
//! take (a file's volume, a process's start, a thread's ID, the clock, the slot file's views
//! and its growth's lock).
#![expect(
    clippy::disallowed_methods,
    reason = "the one place shards calls its OS's durable writes, each declared by its caller (D286)"
)]

#[cfg(unix)]
pub(crate) use unix::*;
#[cfg(windows)]
pub(crate) use windows::*;

/// Darwin's `EINVAL`, `ENOTTY` and `ENOTSUP` (XNU bsd/sys/errno.h:114, :117, :146), named here
/// rather than taken from libc so that [`lacks_full_fsync`] is built and tested on every OS;
/// its macOS test holds them to libc's.
#[cfg(any(target_os = "macos", test))]
const DARWIN_EINVAL: i32 = 22;
#[cfg(any(target_os = "macos", test))]
const DARWIN_ENOTTY: i32 = 25;
#[cfg(any(target_os = "macos", test))]
const DARWIN_ENOTSUP: i32 = 45;

/// Whether an `F_FULLFSYNC` refused with `errno` was refused because its file's file system
/// does not implement it, so that fsync(2), that file system's own durable write, is made in
/// its place. These are the three values XNU itself takes to mean "the underlying filesystem
/// doesn't support it" when it retries an `F_BARRIERFSYNC` as `F_FULLFSYNC`
/// (bsd/kern/kern_descrip.c:3978-3988, xnu-12377.101.15). `ENOTSUP` is what a file system
/// without an ioctl of its own answers: VNOP_IOCTL (kpi_vfs.c:4139) calls the file system's
/// default routine for every operation it left unfilled (vfs_init.c:236-240; a kext's,
/// kpi_vfs.c:1155-1159), conventionally vn_default_error (vfs_init.c:136-139), or err_ioctl
/// (vfs_support.c:279-282). `ENOTTY` and `EINVAL` are what a file system's own ioctl answers
/// to a command it does not know. Any other errno is the write's failure and is never retried
/// as fsync: a writeback that failed (`EIO`) may have dropped the pages it could not write,
/// and an fsync after it would find nothing left to write and report success.
#[cfg(any(target_os = "macos", test))]
fn lacks_full_fsync(errno: i32) -> bool {
    matches!(errno, DARWIN_ENOTSUP | DARWIN_ENOTTY | DARWIN_EINVAL)
}

#[cfg(unix)]
mod unix {
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsRawFd as _, RawFd};
    use std::ptr::NonNull;

    use crate::Target;

    /// A call's status as an error where it failed (its `errno`).
    fn said(rc: libc::c_int) -> io::Result<()> {
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// `call`, made again while a signal interrupts it (`EINTR`), as std's own syncs are
    /// (library/std/src/sys/fs/unix.rs:1413-1428, Rust 1.98.1); its error otherwise.
    fn retried(mut call: impl FnMut() -> libc::c_int) -> io::Result<()> {
        loop {
            match said(call()) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                done => return done,
            }
        }
    }

    /// `F_FULLFSYNC`, which writes the file's data and then flushes the drive's cache
    /// (fcntl(2)). Where the file system does not implement it ([`super::lacks_full_fsync`]):
    /// `first`, then fsync(2). Any other refusal is returned with its errno.
    #[cfg(target_os = "macos")]
    fn full(fd: RawFd, first: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
        // SAFETY: fcntl(2) of a descriptor the caller holds.
        let Err(refused) = retried(|| unsafe { libc::fcntl(fd, libc::F_FULLFSYNC) }) else {
            return Ok(());
        };
        if !refused.raw_os_error().is_some_and(super::lacks_full_fsync) {
            return Err(refused);
        }
        first()?;
        // SAFETY: fsync(2) of the same descriptor.
        retried(|| unsafe { libc::fsync(fd) })
    }

    fn fd(file: &impl Target) -> RawFd {
        file.as_fd().as_raw_fd()
    }

    pub(crate) fn data(file: &impl Target) -> io::Result<()> {
        #[cfg(target_os = "macos")]
        return full(fd(file), || Ok(()));
        #[cfg(not(target_os = "macos"))]
        // SAFETY: fdatasync(2) of a descriptor the caller holds.
        return retried(|| unsafe { libc::fdatasync(fd(file)) });
    }

    pub(crate) fn all(file: &impl Target) -> io::Result<()> {
        #[cfg(target_os = "macos")]
        return full(fd(file), || Ok(()));
        #[cfg(not(target_os = "macos"))]
        // SAFETY: fsync(2) of a descriptor the caller holds.
        return retried(|| unsafe { libc::fsync(fd(file)) });
    }

    /// # Safety
    /// `ptr..ptr + len` is a shared mapping, mapped for the call.
    pub(crate) unsafe fn view(ptr: NonNull<u8>, len: usize) -> io::Result<()> {
        // SAFETY: msync(2) of the caller's mapping.
        said(unsafe { libc::msync(ptr.as_ptr().cast(), len, libc::MS_SYNC) })
    }

    /// # Safety
    /// `ptr..ptr + len` is a shared mapping of `file`, mapped for the call.
    pub(crate) unsafe fn mapped(file: &impl Target, ptr: NonNull<u8>, len: usize) -> io::Result<()> {
        #[cfg(target_os = "macos")]
        // SAFETY: forwarded caller contract, for the msync(2) made before an fsync(2) where
        // F_FULLFSYNC, which writes a mapping's dirty pages back too, is not implemented.
        return full(fd(file), || unsafe { view(ptr, len) });
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (ptr, len);
            data(file)
        }
    }

    pub(crate) fn ahead(file: &File, failed: impl FnOnce(io::Error) + Send + 'static) -> io::Result<()> {
        #[cfg(target_os = "macos")]
        {
            let back = |file: &File| {
                // SAFETY: fsync(2) of the descriptor `file` holds.
                crate::table::declared(file, || retried(|| unsafe { libc::fsync(file.as_raw_fd()) }))
            };
            let own = file.try_clone()?;
            let spawned = std::thread::Builder::new()
                .name("write-back".into())
                .spawn(move || {
                    if let Err(e) = back(&own) {
                        failed(e);
                    }
                });
            if spawned.is_err() {
                return back(file);
            }
        }
        #[cfg(not(target_os = "macos"))]
        let _ = (file, failed);
        Ok(())
    }

    /// The device `file` is on (fstat(2)'s `st_dev`).
    pub(crate) fn volume(file: &impl Target) -> io::Result<u64> {
        let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: fstat(2) of a descriptor the caller holds, into a stat of ours.
        said(unsafe { libc::fstat(fd(file), st.as_mut_ptr()) })?;
        // SAFETY: fstat filled it.
        let st = unsafe { st.assume_init() };
        // dev_t is u64 on Linux, i32 on macOS: its bits, as a u64.
        #[allow(clippy::unnecessary_cast)]
        let device = st.st_dev as u64;
        Ok(device)
    }

    /// Process `pid`'s start as its OS keeps it, which with its ID names it alone: Linux
    /// `/proc/<pid>/stat` field 22 (proc(5), `starttime`); none where no process has `pid`.
    #[cfg(target_os = "linux")]
    pub(crate) fn start_of(pid: u32) -> io::Result<Option<u64>> {
        let bytes = match std::fs::read(format!("/proc/{pid}/stat")) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ESRCH) => {
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let text = String::from_utf8_lossy(&bytes);
        // The fields after its command's closing parenthesis are the 3rd on: the 22nd is the
        // 20th of them. Its command may hold any byte, ')' among them, but none past the last.
        text.rfind(')')
            .and_then(|at| text.get(at.saturating_add(1)..))
            .and_then(|rest| rest.split_whitespace().nth(19))
            .and_then(|start| start.parse().ok())
            .map(Some)
            .ok_or_else(|| io::Error::other(format!("/proc/{pid}/stat holds no start")))
    }

    /// Process `pid`'s start as its OS keeps it, which with its ID names it alone: macOS
    /// proc_pid_rusage(3)'s `ri_proc_start_abstime`; none where no process has `pid`.
    #[cfg(target_os = "macos")]
    pub(crate) fn start_of(pid: u32) -> io::Result<Option<u64>> {
        let id = libc::c_int::try_from(pid).map_err(io::Error::other)?;
        // SAFETY: a rusage_info_v0, plain data, for proc_pid_rusage to fill.
        let mut info: libc::rusage_info_v0 = unsafe { std::mem::zeroed() };
        // SAFETY: proc_pid_rusage(3) of `id` into a rusage_info_v0 of ours, which
        // RUSAGE_INFO_V0 names.
        if unsafe { libc::proc_pid_rusage(id, libc::RUSAGE_INFO_V0, (&raw mut info).cast()) } != 0 {
            let e = io::Error::last_os_error();
            return if e.raw_os_error() == Some(libc::ESRCH) {
                Ok(None)
            } else {
                Err(e)
            };
        }
        Ok(Some(info.ri_proc_start_abstime))
    }

    /// This process's start ([`start_of`]).
    pub(crate) fn start() -> io::Result<u64> {
        start_of(std::process::id())?.ok_or_else(|| io::Error::other("this process's start unread"))
    }

    /// This thread's ID as its OS's listing of a process's threads names it: Linux its task
    /// ID (gettid(2), `/proc/<pid>/task/<tid>`).
    #[cfg(target_os = "linux")]
    pub(crate) fn thread() -> io::Result<u64> {
        // SAFETY: gettid(2), which takes nothing and cannot fail.
        let tid = unsafe { libc::syscall(libc::SYS_gettid) };
        u64::try_from(tid).map_err(io::Error::other)
    }

    /// This thread's ID as its OS's listing of a process's threads names it: macOS its
    /// system-wide ID (pthread_threadid_np(3), `PROC_PIDLISTTHREADIDS`).
    #[cfg(target_os = "macos")]
    pub(crate) fn thread() -> io::Result<u64> {
        let mut id = 0u64;
        // SAFETY: the calling thread's ID into a u64 of ours: a null pthread_t (libc's
        // `uintptr_t`) names the calling thread (libpthread src/pthread.c:1119-1130,
        // libpthread-539.100.4).
        let rc = unsafe { libc::pthread_threadid_np(0, &raw mut id) };
        // It returns its error number and leaves errno alone, as pthread(3) calls do.
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc));
        }
        Ok(id)
    }

    pub(crate) fn now() -> io::Result<u64> {
        #[cfg(target_os = "linux")]
        let clock = libc::CLOCK_MONOTONIC;
        #[cfg(target_os = "macos")]
        let clock = libc::CLOCK_UPTIME_RAW;
        let mut at = std::mem::MaybeUninit::<libc::timespec>::uninit();
        // SAFETY: clock_gettime(2) into a timespec of ours.
        said(unsafe { libc::clock_gettime(clock, at.as_mut_ptr()) })?;
        // SAFETY: clock_gettime filled it.
        let at = unsafe { at.assume_init() };
        let seconds = u64::try_from(at.tv_sec).map_err(io::Error::other)?;
        let nanos = u64::try_from(at.tv_nsec).map_err(io::Error::other)?;
        seconds
            .checked_mul(1_000_000_000)
            .and_then(|n| n.checked_add(nanos))
            .ok_or_else(|| io::Error::other("the clock past 64 bits of nanoseconds"))
    }

    /// The directory `dir` made, and those above it it lacks, its user's alone (0700), as a
    /// home is made (the daemon's, shards-vmm's `platform::create_private_dir`): a home holds
    /// its user's runs, layers and keys.
    pub(crate) fn private_dir(dir: &std::path::Path) -> io::Result<()> {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
    }

    /// What a slot file grows by and is mapped in: a page (sysconf(3) `_SC_PAGESIZE`), the
    /// least a mapping's offset steps by.
    pub(crate) fn granule() -> io::Result<usize> {
        // SAFETY: sysconf(3), which reads a value.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        usize::try_from(page)
            .ok()
            .filter(|&page| page > 0)
            .ok_or_else(io::Error::last_os_error)
    }

    /// A shared mapping of a file's bytes, let go of as it drops.
    #[derive(Debug)]
    pub(crate) struct Map {
        pub(crate) ptr: NonNull<u8>,
        pub(crate) len: usize,
    }

    // SAFETY: the mapping is shared memory of no thread's own, read and written through
    // atomics alone.
    unsafe impl Send for Map {}
    // SAFETY: as above.
    unsafe impl Sync for Map {}

    impl Map {
        /// `len` bytes of `file` at `offset`, writable where `write`.
        pub(crate) fn of(file: &File, offset: u64, len: usize, write: bool) -> io::Result<Map> {
            let prot = if write {
                libc::PROT_READ | libc::PROT_WRITE
            } else {
                libc::PROT_READ
            };
            let offset = libc::off_t::try_from(offset).map_err(io::Error::other)?;
            // SAFETY: a new shared mapping of `len` bytes of a descriptor `file` holds, placed by
            // the kernel; it outlives the descriptor by itself.
            let at = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    prot,
                    libc::MAP_SHARED,
                    file.as_raw_fd(),
                    offset,
                )
            };
            if at == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            NonNull::new(at.cast())
                .map(|ptr| Map { ptr, len })
                .ok_or_else(|| io::Error::other("a mapping at null"))
        }
    }

    impl Drop for Map {
        fn drop(&mut self) {
            // SAFETY: the mapping this made, which no reference outlives (its owners say so).
            unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.len) };
        }
    }

    /// The slot file's growth lock, held as long as this lives (flock(2), `LOCK_EX`): advisory,
    /// so that its mappings are untouched.
    #[derive(Debug)]
    pub(crate) struct Locked<'a>(&'a File);

    impl<'a> Locked<'a> {
        pub(crate) fn of(file: &'a File) -> io::Result<Locked<'a>> {
            loop {
                // SAFETY: flock(2) of a descriptor `file` holds.
                if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                    return Ok(Locked(file));
                }
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::Interrupted {
                    return Err(e);
                }
            }
        }
    }

    impl Drop for Locked<'_> {
        fn drop(&mut self) {
            // SAFETY: flock(2) unlocking the descriptor this locked.
            unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::fs::File;
    use std::io;
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, RawHandle};
    use std::ptr::NonNull;

    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, FILETIME, WAIT_OBJECT_0};
    use windows_sys::Win32::Storage::FileSystem::{
        FlushFileBuffers, GetVolumeInformationByHandleW, LOCKFILE_EXCLUSIVE_LOCK, LockFileEx, UnlockFileEx,
    };
    use windows_sys::Win32::System::IO::{OVERLAPPED, OVERLAPPED_0, OVERLAPPED_0_0};
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, FlushViewOfFile, MEMORY_MAPPED_VIEW_ADDRESS,
        MapViewOfFile, PAGE_READONLY, PAGE_READWRITE, UnmapViewOfFile,
    };
    use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentThreadId, GetProcessTimes, OpenProcess,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    use crate::Target;

    /// A call's success, or its error (GetLastError).
    fn said(ok: windows_sys::core::BOOL) -> io::Result<()> {
        if ok != 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn handle(file: &impl Target) -> RawHandle {
        file.as_handle().as_raw_handle()
    }

    fn flush(file: &impl Target) -> io::Result<()> {
        // SAFETY: FlushFileBuffers of a handle the caller holds.
        said(unsafe { FlushFileBuffers(handle(file)) })
    }

    pub(crate) fn data(file: &impl Target) -> io::Result<()> {
        flush(file)
    }

    pub(crate) fn all(file: &impl Target) -> io::Result<()> {
        flush(file)
    }

    /// # Safety
    /// `ptr..ptr + len` is a view of a file, mapped for the call.
    pub(crate) unsafe fn view(ptr: NonNull<u8>, len: usize) -> io::Result<()> {
        // SAFETY: FlushViewOfFile of the caller's view.
        said(unsafe { FlushViewOfFile(ptr.as_ptr().cast_const().cast(), len) })
    }

    /// # Safety
    /// `ptr..ptr + len` is a view of `file`, mapped for the call.
    pub(crate) unsafe fn mapped(file: &impl Target, ptr: NonNull<u8>, len: usize) -> io::Result<()> {
        // SAFETY: forwarded caller contract.
        unsafe { view(ptr, len) }?;
        flush(file)
    }

    pub(crate) fn ahead(file: &File, failed: impl FnOnce(io::Error) + Send + 'static) -> io::Result<()> {
        let _ = (file, failed);
        Ok(())
    }

    /// The volume `file` is on: its serial number (GetVolumeInformationByHandleW), as
    /// GetVolumeInformationW names each volume. Its volume's information alone
    /// (FileFsVolumeInformation), which asks no access right of the handle, where the file's
    /// own (GetFileInformationByHandle, FileBasicInformation) asks FILE_READ_ATTRIBUTES, which
    /// a handle opened to write alone has not (vmm's NtCreateFile opens of a directory's files).
    pub(crate) fn volume(file: &impl Target) -> io::Result<u64> {
        let mut serial = 0u32;
        // SAFETY: a handle the caller holds; its serial alone asked, into a u32 of ours, and
        // no buffer named.
        said(unsafe {
            GetVolumeInformationByHandleW(
                handle(file),
                std::ptr::null_mut(),
                0,
                &raw mut serial,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            )
        })?;
        Ok(u64::from(serial))
    }

    fn filetime(at: FILETIME) -> u64 {
        (u64::from(at.dwHighDateTime) << 32) | u64::from(at.dwLowDateTime)
    }

    /// The creation time of the process `process` holds (GetProcessTimes), 100 ns units since
    /// 1601.
    fn created(process: RawHandle) -> io::Result<u64> {
        let [mut created, mut exit, mut kernel, mut user] = [FILETIME::default(); 4];
        // SAFETY: a process handle the caller holds, into FILETIMEs of ours.
        said(unsafe {
            GetProcessTimes(
                process,
                &raw mut created,
                &raw mut exit,
                &raw mut kernel,
                &raw mut user,
            )
        })?;
        Ok(filetime(created))
    }

    /// Process `pid`'s start as its OS keeps it, which with its ID names it alone: its creation
    /// time (GetProcessTimes); none where no process has `pid` (OpenProcess's
    /// ERROR_INVALID_PARAMETER), or the one that had it has exited and is held by another's
    /// handle alone.
    pub(crate) fn start_of(pid: u32) -> io::Result<Option<u64>> {
        // SAFETY: OpenProcess of `pid`, to read its times and wait on it; the handle owned below.
        let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, 0, pid) };
        if raw.is_null() {
            let e = io::Error::last_os_error();
            return if e.raw_os_error() == i32::try_from(ERROR_INVALID_PARAMETER).ok() {
                Ok(None)
            } else {
                Err(e)
            };
        }
        // SAFETY: the handle OpenProcess just made, owned here alone.
        let process = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(raw) };
        // SAFETY: a zero wait on the process handle held: signaled once it has exited.
        if unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } == WAIT_OBJECT_0 {
            return Ok(None);
        }
        created(process.as_raw_handle()).map(Some)
    }

    /// This process's start ([`start_of`]).
    pub(crate) fn start() -> io::Result<u64> {
        // SAFETY: the pseudo-handle of this process, which needs no closing.
        created(unsafe { GetCurrentProcess() })
    }

    /// This thread's ID (GetCurrentThreadId), as the system's listing of a process's threads
    /// names it (SYSTEM_THREAD_INFORMATION's `ClientId.UniqueThread`).
    pub(crate) fn thread() -> io::Result<u64> {
        // SAFETY: GetCurrentThreadId, which takes nothing and cannot fail.
        Ok(u64::from(unsafe { GetCurrentThreadId() }))
    }

    pub(crate) fn now() -> io::Result<u64> {
        let (mut ticks, mut hertz) = (0i64, 0i64);
        // SAFETY: the performance counter and its frequency into i64s of ours.
        said(unsafe { QueryPerformanceCounter(&raw mut ticks) })?;
        // SAFETY: as above.
        said(unsafe { QueryPerformanceFrequency(&raw mut hertz) })?;
        let (ticks, hertz) = (
            u128::try_from(ticks).map_err(io::Error::other)?,
            u128::try_from(hertz).map_err(io::Error::other)?,
        );
        ticks
            .checked_mul(1_000_000_000)
            .and_then(|n| n.checked_div(hertz))
            .and_then(|n| u64::try_from(n).ok())
            .ok_or_else(|| io::Error::other("the performance counter past 64 bits of nanoseconds"))
    }

    /// The directory `dir` made, and those above it it lacks, as a home is made on Windows
    /// (the daemon's, shards-vmm's `platform::create_private_dir`): its access its parent's,
    /// the user's own local application data (`FOLDERID_LocalAppData`), inherited.
    pub(crate) fn private_dir(dir: &std::path::Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)
    }

    /// What a slot file grows by and is mapped in: the allocation granularity (GetSystemInfo's
    /// `dwAllocationGranularity`), the least a view's offset steps by.
    pub(crate) fn granule() -> io::Result<usize> {
        let mut info = SYSTEM_INFO::default();
        // SAFETY: GetSystemInfo into a SYSTEM_INFO of ours.
        unsafe { GetSystemInfo(&raw mut info) };
        usize::try_from(info.dwAllocationGranularity)
            .ok()
            .filter(|&g| g > 0)
            .ok_or_else(|| io::Error::other("no allocation granularity"))
    }

    /// A view of a file's bytes, let go of as it drops.
    #[derive(Debug)]
    pub(crate) struct Map {
        pub(crate) ptr: NonNull<u8>,
        pub(crate) len: usize,
    }

    // SAFETY: the view is shared memory of no thread's own, read and written through atomics
    // alone.
    unsafe impl Send for Map {}
    // SAFETY: as above.
    unsafe impl Sync for Map {}

    impl Map {
        /// `len` bytes of `file` at `offset`, writable where `write`: a section as long as their
        /// end (CreateFileMappingW), its view (MapViewOfFile), the section let go of at once,
        /// as its view holds it.
        pub(crate) fn of(file: &File, offset: u64, len: usize, write: bool) -> io::Result<Map> {
            let end = u64::try_from(len)
                .ok()
                .and_then(|len| offset.checked_add(len))
                .ok_or_else(|| io::Error::other("a view past 64 bits"))?;
            let (protect, access) = if write {
                (PAGE_READWRITE, FILE_MAP_READ | FILE_MAP_WRITE)
            } else {
                (PAGE_READONLY, FILE_MAP_READ)
            };
            let high = |n: u64| u32::try_from(n >> 32).unwrap_or(u32::MAX);
            let low = |n: u64| u32::try_from(n & u64::from(u32::MAX)).unwrap_or(u32::MAX);
            // SAFETY: an unnamed section of a file handle `file` holds, as long as `end`, with
            // default security; the handle closed below.
            let section = unsafe {
                CreateFileMappingW(
                    file.as_raw_handle(),
                    std::ptr::null(),
                    protect,
                    high(end),
                    low(end),
                    std::ptr::null(),
                )
            };
            if section.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: a view of the section just made.
            let view = unsafe { MapViewOfFile(section, access, high(offset), low(offset), len) };
            let failed = view.Value.is_null().then(io::Error::last_os_error);
            // SAFETY: the section handle, which its view (if any) holds by itself.
            unsafe { CloseHandle(section) };
            if let Some(e) = failed {
                return Err(e);
            }
            NonNull::new(view.Value.cast())
                .map(|ptr| Map { ptr, len })
                .ok_or_else(|| io::Error::other("a view at null"))
        }
    }

    impl Drop for Map {
        fn drop(&mut self) {
            // SAFETY: the view this made, which no reference outlives (its owners say so).
            unsafe {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.ptr.as_ptr().cast(),
                })
            };
        }
    }

    /// Where the growth lock lies: the byte at offset 2^63 (its high word's top bit), which the
    /// slot file, grown a granule at a time only as its claims need one, never reaches; so the
    /// lock, which Windows enforces on reads and writes through other handles (LockFileEx),
    /// overlaps no byte its views read.
    const LOCKED_AT_HIGH: u32 = 1 << 31;

    /// The slot file's growth lock, held as long as this lives (LockFileEx, exclusive, of
    /// one byte at [`LOCKED_AT_HIGH`]).
    #[derive(Debug)]
    pub(crate) struct Locked<'a>(&'a File);

    fn at_lock() -> OVERLAPPED {
        OVERLAPPED {
            Anonymous: OVERLAPPED_0 {
                Anonymous: OVERLAPPED_0_0 {
                    Offset: 0,
                    OffsetHigh: LOCKED_AT_HIGH,
                },
            },
            ..OVERLAPPED::default()
        }
    }

    impl<'a> Locked<'a> {
        pub(crate) fn of(file: &'a File) -> io::Result<Locked<'a>> {
            let mut at = at_lock();
            // SAFETY: LockFileEx of one byte of a synchronous handle `file` holds, waited for.
            said(unsafe {
                LockFileEx(
                    file.as_raw_handle(),
                    LOCKFILE_EXCLUSIVE_LOCK,
                    0,
                    1,
                    0,
                    &raw mut at,
                )
            })?;
            Ok(Locked(file))
        }
    }

    impl Drop for Locked<'_> {
        fn drop(&mut self) {
            let mut at = at_lock();
            // SAFETY: UnlockFileEx of the byte this locked.
            unsafe { UnlockFileEx(self.0.as_raw_handle(), 0, 1, 0, &raw mut at) };
        }
    }
}

#[cfg(test)]
mod tests {
    /// Darwin's errno values (XNU bsd/sys/errno.h, xnu-12377.101.15) that are not
    /// [`super::lacks_full_fsync`]'s, each an `F_FULLFSYNC` that failed: `ENOENT`, `EINTR`
    /// (made again before it gets here), `EIO`, `ENXIO`, `EBADF` (a descriptor that is no
    /// vnode's, kern_descrip.c:3964-3966), `ENODEV` (a device's driver), `ENOSPC`, `EROFS`,
    /// `EDQUOT` and the kernel's `EOPNOTSUPP` (102, apart from `ENOTSUP` there).
    const FAILED: [i32; 10] = [2, 4, 5, 6, 9, 19, 28, 30, 69, 102];

    /// A file system that does not implement `F_FULLFSYNC` falls back to fsync; an
    /// `F_FULLFSYNC` refused for any other reason fails.
    #[test]
    fn only_a_file_system_without_full_fsync_falls_back() {
        for errno in [super::DARWIN_ENOTSUP, super::DARWIN_ENOTTY, super::DARWIN_EINVAL] {
            assert!(super::lacks_full_fsync(errno), "{errno} failed");
        }
        for errno in FAILED.into_iter().chain([0, -1, i32::MAX]) {
            assert!(!super::lacks_full_fsync(errno), "{errno} fell back");
        }
    }

    /// The Darwin values named for every OS are macOS's own.
    #[cfg(target_os = "macos")]
    #[test]
    fn darwins_errno_values_are_libcs() {
        assert_eq!(
            [super::DARWIN_ENOTSUP, super::DARWIN_ENOTTY, super::DARWIN_EINVAL],
            [libc::ENOTSUP, libc::ENOTTY, libc::EINVAL]
        );
        assert_eq!(
            FAILED,
            [
                libc::ENOENT,
                libc::EINTR,
                libc::EIO,
                libc::ENXIO,
                libc::EBADF,
                libc::ENODEV,
                libc::ENOSPC,
                libc::EROFS,
                libc::EDQUOT,
                libc::EOPNOTSUPP,
            ]
        );
    }

    /// Live on this host's kernel, both outcomes. devfs does not implement `F_FULLFSYNC`:
    /// its directories' ioctl is err_ioctl (XNU bsd/miscfs/devfs/devfs_vnops.c:1711, ENOTSUP)
    /// and their fsync nop_fsync (:1715), so a durable write of `/dev` is made with fsync, as
    /// one on an SMB share is (ENOTSUP, go.dev/issue/64215). `/dev/null`'s driver refuses every
    /// ioctl it does not know with ENODEV (mmioctl, bsd/dev/mem.c:150) and takes fsync
    /// (spec_fsync), so a durable write of it that succeeded would be a fallback after a
    /// failure: it fails, with that errno.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_full_fsync_refused_falls_back_only_where_unimplemented() {
        use std::fs::{File, OpenOptions};
        use std::os::fd::AsRawFd as _;
        let refused = |file: &File| {
            // SAFETY: fcntl(2) of a descriptor this test holds.
            let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) };
            assert_eq!(rc, -1, "F_FULLFSYNC taken");
            std::io::Error::last_os_error().raw_os_error()
        };
        let devfs = File::open("/dev").unwrap();
        assert_eq!(refused(&devfs), Some(libc::ENOTSUP));
        crate::entries(&devfs).unwrap();
        crate::all(&devfs).unwrap();
        crate::data(&devfs).unwrap();
        let null = OpenOptions::new().write(true).open("/dev/null").unwrap();
        assert_eq!(refused(&null), Some(libc::ENODEV));
        // SAFETY: fsync(2) of a descriptor this test holds.
        let synced = unsafe { libc::fsync(null.as_raw_fd()) };
        assert_eq!(synced, 0, "/dev/null refused fsync");
        for made in [crate::data(&null), crate::all(&null)] {
            assert_eq!(made.unwrap_err().raw_os_error(), Some(libc::ENODEV));
        }
        // An APFS directory takes F_FULLFSYNC itself (PM M46).
        let held = shards_testdir::TempDir::new("sync").unwrap();
        crate::entries(File::open(held.path()).unwrap()).unwrap();
    }
}
