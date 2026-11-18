//! Every write shards makes durable, declared (D286). One helper, whose calls are each OS's
//! own: Linux fdatasync(2) or fsync(2); macOS `F_FULLFSYNC`, the one call whose data is on the
//! drive's media when it returns, and fsync(2) where a file system lacks it, as SQLite's
//! `full_fsync` falls back (os_unix.c); Windows FlushFileBuffers, after FlushViewOfFile for a
//! view. Each is bracketed by a declaration in the slot file of its process's home
//! ([`table`]), which whoever follows the process reads to judge a wait on it (D215): a thread
//! in a durable write is its host's while the write's volume works. A process that installed
//! no slot file, nor recorded its home ([`install_at`]), declares nothing: the guest's, whose
//! writes no host judge follows (what they cost the host is its VMM's own durable write, which
//! declares), and a tool that writes outside any home.

mod sys;
pub mod table;

use std::io;
use std::ptr::NonNull;

pub use table::{Declared, FILE, Reader, home, install, install_at, install_file, installed, open};

/// What a durable write is made of: a file's descriptor on Unix, its handle on Windows,
/// borrowed, as a `&File` and a raw one borrowed both are.
#[cfg(unix)]
pub trait Target: std::os::fd::AsFd {}
#[cfg(unix)]
impl<T: std::os::fd::AsFd> Target for T {}
/// What a durable write is made of: a file's descriptor on Unix, its handle on Windows,
/// borrowed, as a `&File` and a raw one borrowed both are.
#[cfg(windows)]
pub trait Target: std::os::windows::io::AsHandle {}
#[cfg(windows)]
impl<T: std::os::windows::io::AsHandle> Target for T {}

/// Makes what was written to `file` durable on stable storage, its data and what reading it
/// back needs: Linux fdatasync(2), macOS `F_FULLFSYNC` (fsync(2) where its file system lacks
/// it), Windows FlushFileBuffers. Declared.
pub fn data(file: impl Target) -> io::Result<()> {
    table::declared(&file, || sys::data(&file))
}

/// Makes what was written to `file` durable on stable storage, its metadata all too: Linux
/// fsync(2), macOS `F_FULLFSYNC` (fsync(2) where its file system lacks it), Windows
/// FlushFileBuffers. Declared.
pub fn all(file: impl Target) -> io::Result<()> {
    table::declared(&file, || sys::all(&file))
}

/// Makes the names in the directory `dir` holds open durable: the renames into it survive a
/// crash once this returns. Linux takes an fsync(2) of the directory itself; macOS its
/// `F_FULLFSYNC`, which directories accept (PM M46), fsync(2) where its file system lacks it;
/// Windows FlushFileBuffers, which wants a handle that may write (its documentation: "must
/// have the GENERIC_WRITE access right"), so that `dir` is one opened to write there.
/// Declared.
pub fn entries(dir: impl Target) -> io::Result<()> {
    table::declared(&dir, || sys::all(&dir))
}

/// Writes what was written through `ptr..ptr + len`, a shared view of `file`, to `file`, and
/// waits until it has (msync(2) `MS_SYNC`; Windows FlushViewOfFile): the file's blocks then
/// hold it. Not durable past the drive's cache: [`mapped`] is. Declared.
///
/// # Safety
/// `ptr..ptr + len` is a shared view of `file`, mapped for the call.
pub unsafe fn view(file: impl Target, ptr: NonNull<u8>, len: usize) -> io::Result<()> {
    // SAFETY: forwarded caller contract.
    table::declared(&file, || unsafe { sys::view(ptr, len) })
}

/// Makes what was written to `file`, through write(2) or through `ptr..ptr + len`, a shared
/// view of it, durable. Linux's fdatasync(2) writes back the pages a view dirtied, as it does
/// any (Btrfs: 2.5 to 3.6 ms for 16 pages of a 10 GiB file). macOS's `F_FULLFSYNC` writes
/// them back too, and flushes the drive's cache, where a plain fsync(2) leaves them dirty
/// (PM M286); a file system without it gets msync(2), then fsync(2). Windows writes the view
/// (FlushViewOfFile), then the file (FlushFileBuffers). Declared.
///
/// # Safety
/// `ptr..ptr + len` is a shared view of `file`, mapped for the call.
pub unsafe fn mapped(file: impl Target, ptr: NonNull<u8>, len: usize) -> io::Result<()> {
    // SAFETY: forwarded caller contract.
    table::declared(&file, || unsafe { sys::mapped(&file, ptr, len) })
}

/// Has what was written to `file` written back now, on a thread of its own, where its OS
/// would make whoever lets go of the file last wait for it: macOS writes a file's dirty pages
/// back as its last writable descriptor or mapping goes, and whoever reads the file meanwhile
/// waits too (APFS, PM M278). fsync(2) alone, which writes them to the drive, not through its
/// cache as `F_FULLFSYNC` does: nothing here asks to be durable. Its own descriptor of the
/// file, which its thread closes once written back, declared while it writes; `failed` takes
/// what failed there. Where no thread starts, the caller waits for the write itself.
/// Elsewhere nothing: Linux writes back in its own time, and a close never waits for it.
pub fn ahead(file: &std::fs::File, failed: impl FnOnce(io::Error) + Send + 'static) -> io::Result<()> {
    sys::ahead(file, failed)
}

/// The system's monotonic clock now, in nanoseconds, as a declaration's `since` reads it:
/// Linux `CLOCK_MONOTONIC`, macOS `CLOCK_UPTIME_RAW` (what std's `Instant` reads), Windows
/// the performance counter. One clock for every process of the host, so that a follower
/// reads how long a write it did not make has gone on.
pub fn now() -> io::Result<u64> {
    sys::now()
}
