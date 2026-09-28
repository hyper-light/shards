use std::fs::File;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::ptr::NonNull;

use windows_sys::Win32::Foundation::ERROR_HANDLE_EOF;
use windows_sys::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
use windows_sys::Win32::Storage::FileSystem::{FlushFileBuffers, ReadFile, WriteFile};
use windows_sys::Win32::System::IO::OVERLAPPED;
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
};
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};

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

/// # Safety
/// `ptr` must come from `reserve` and not be used afterwards.
pub unsafe fn release(ptr: NonNull<u8>, _len: usize) {
    // SAFETY: forwarded caller contract; MEM_RELEASE requires size 0.
    unsafe { VirtualFree(ptr.as_ptr().cast(), 0, MEM_RELEASE) };
}

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

/// Flushes file data and metadata to stable storage.
pub fn sync_durable(file: &File) -> io::Result<()> {
    // SAFETY: flushing an owned, open handle.
    if unsafe { FlushFileBuffers(file.as_raw_handle()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
