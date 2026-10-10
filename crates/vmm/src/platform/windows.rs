use std::fs::File;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::ptr::NonNull;

use windows_sys::Win32::Foundation::ERROR_HANDLE_EOF;
use windows_sys::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FlushFileBuffers, GetDiskFreeSpaceExW, ReadFile, WriteFile,
};
use windows_sys::Win32::System::IO::OVERLAPPED;
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
};
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};

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

/// # Safety
/// `ptr` must come from `reserve` or `reserve_ram` and not be used afterwards.
pub unsafe fn release(ptr: NonNull<u8>, _len: usize) {
    // SAFETY: forwarded caller contract; MEM_RELEASE requires size 0.
    unsafe { VirtualFree(ptr.as_ptr().cast(), 0, MEM_RELEASE) };
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
