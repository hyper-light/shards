//! A watch on a directory, for other processes' changes to it, on Windows (unix.rs
//! `FileWatch`): its event is set once a name in the directory has come or gone, or the file
//! of it that is watched has been written, and for no other file's writes, for
//! WaitForMultipleObjects to wait on beside others.
//!
//! - **Names** come and go as ReadDirectoryChangesW says, read into the process's port.
//! - **Writes** to the file watched break a read oplock (OPLOCK_LEVEL_CACHE_READ) held on a
//!   handle of the watch's own, requested again after each break: NTFS says a write through
//!   any other handle so. ReadDirectoryChangesW says a file's size or last write only as
//!   NTFS updates its directory entry, which for a writer that holds the file open came not
//!   within 5 s of an append on jupiter (Windows 10.0.26200); a read oplock was granted beside
//!   that writer, and each of its appends broke it, 5.6 to 111 µs on (2026-10-27). Where none
//!   is granted (a file system without oplocks), the directory's records of the file's name
//!   stand in.
//! - **One port and one thread** serve every watch of the process, as Linux reads one
//!   inotify instance for them (PM M257), each setting a watch's own event for the changes
//!   it watches alone. A watch's reads and requests write into its own boxed buffers until
//!   their completions come back, so it is freed only then: dropped while one is out, it is
//!   cancelled, and freed as the cancellation comes back.

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::os::windows::io::{
    AsHandle as _, AsRawHandle as _, BorrowedHandle, FromRawHandle as _, OwnedHandle,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use windows_sys::Win32::Foundation::{ERROR_IO_PENDING, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_REMOVED, FILE_ACTION_RENAMED_NEW_NAME,
    FILE_ACTION_RENAMED_OLD_NAME, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_DIR_NAME,
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
    FILE_NOTIFY_INFORMATION, FILE_READ_ATTRIBUTES, ReadDirectoryChangesW,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, CreateIoCompletionPort, DeviceIoControl, GetQueuedCompletionStatus, OVERLAPPED,
};
use windows_sys::Win32::System::Ioctl::{
    FSCTL_REQUEST_OPLOCK, OPLOCK_LEVEL_CACHE_READ, REQUEST_OPLOCK_CURRENT_VERSION,
    REQUEST_OPLOCK_INPUT_BUFFER, REQUEST_OPLOCK_INPUT_FLAG_REQUEST, REQUEST_OPLOCK_OUTPUT_BUFFER,
};
use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, ResetEvent, SetEvent};

/// What a watch's directory is read for: names coming and going, and writes, which wake it
/// only for its file's name where its file holds no oplock.
const CHANGES: u32 = FILE_NOTIFY_CHANGE_FILE_NAME
    | FILE_NOTIFY_CHANGE_DIR_NAME
    | FILE_NOTIFY_CHANGE_LAST_WRITE
    | FILE_NOTIFY_CHANGE_SIZE;

/// The bytes of a watch's records read at once; past them its read says none (no bytes), a
/// change all the same.
const RECORDS: usize = 16 << 10;

/// A watch on a directory (the module's).
#[derive(Debug)]
pub struct FileWatch {
    key: usize,
    event: OwnedHandle,
}

/// The process's watches, their port, and the next key one is given.
struct Watcher {
    port: OwnedHandle,
    watches: Mutex<HashMap<usize, Box<Watch>>>,
    next: AtomicUsize,
}

/// One watch's side of the process's: its directory's handle and read, the oplocks of the
/// files it has watched (the last its own, those before it waiting for their cancellations
/// to come back), and its event.
struct Watch {
    dir: OwnedHandle,
    overlapped: OVERLAPPED,
    records: Vec<u32>,
    #[expect(
        clippy::vec_box,
        reason = "the kernel writes each request's OVERLAPPED and output where it was asked until its completion comes back, so each stays where it is as the Vec grows and shrinks"
    )]
    oplocks: Vec<Box<Oplock>>,
    /// The name of the file watched, where its file holds no oplock: its records stand in.
    named: Option<Vec<u16>>,
    event: OwnedHandle,
    /// Its read is out: its completion is yet to come.
    reading: bool,
    /// Its watch was dropped: freed once nothing of it is out.
    ending: bool,
}

/// A read oplock held on a file watched, through a handle of its own, its request out until
/// another handle writes to the file.
struct Oplock {
    file: OwnedHandle,
    overlapped: OVERLAPPED,
    input: REQUEST_OPLOCK_INPUT_BUFFER,
    output: REQUEST_OPLOCK_OUTPUT_BUFFER,
    /// Its request is out.
    out: bool,
    /// The file watched now, not one before it.
    current: bool,
}

// SAFETY: a Watch and its Oplocks are touched under their watcher's lock alone, and their
// OVERLAPPEDs' pointers are their own buffers' and handles', which no other thread holds.
unsafe impl Send for Watch {}

static WATCHER: OnceLock<Option<Watcher>> = OnceLock::new();
/// Whether the watcher's thread started: told once, by the first watch, and kept, so that
/// a watcher no thread serves is never handed out.
static SERVED: OnceLock<bool> = OnceLock::new();

/// The process's watcher, made with its first watch and served by a thread of its own from
/// then on; none where its port could not be made, or its thread started.
fn watcher() -> Option<&'static Watcher> {
    let watcher = WATCHER
        .get_or_init(|| {
            // SAFETY: a new completion port, associated with no handle yet, served by one
            // thread.
            let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1) };
            if port.is_null() {
                return None;
            }
            Some(Watcher {
                // SAFETY: the port just made, owned by nothing else.
                port: unsafe { OwnedHandle::from_raw_handle(port) },
                watches: Mutex::new(HashMap::new()),
                next: AtomicUsize::new(1),
            })
        })
        .as_ref()?;
    let served = *SERVED.get_or_init(|| {
        std::thread::Builder::new()
            .name("file watches".into())
            .spawn(move || watcher.serve())
            .is_ok()
    });
    served.then_some(watcher)
}

impl Watcher {
    fn watches(&self) -> MutexGuard<'_, HashMap<usize, Box<Watch>>> {
        self.watches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Hears each completion: a read's changes checked and its directory read again, an
    /// oplock's break said and its oplock asked for again, each where its watch is held; a
    /// watch dropped freed once nothing of it is out.
    fn serve(&self) {
        loop {
            let mut bytes = 0u32;
            let mut key = 0usize;
            let mut overlapped: *mut OVERLAPPED = std::ptr::null_mut();
            // SAFETY: the process's port, and locals for what its next packet says.
            let ok = unsafe {
                GetQueuedCompletionStatus(
                    self.port.as_raw_handle(),
                    &mut bytes,
                    &mut key,
                    &mut overlapped,
                    INFINITE,
                )
            };
            if overlapped.is_null() {
                // No packet: the port itself failed, which a port the process holds does not.
                if ok == 0 {
                    return;
                }
                continue;
            }
            let mut watches = self.watches();
            let Some(watch) = watches.get_mut(&key) else {
                continue;
            };
            let woke = if std::ptr::eq(overlapped, &raw const watch.overlapped) {
                watch.read_done(ok != 0, usize::try_from(bytes).unwrap_or(usize::MAX))
            } else {
                watch.oplock_done(overlapped, ok != 0)
            };
            if woke {
                // SAFETY: the event this watch holds.
                unsafe { SetEvent(watch.event.as_raw_handle()) };
            }
            if watch.ending && watch.idle() {
                watches.remove(&key);
            }
        }
    }

    /// Lets go of watch `key`: at once where nothing of it is out, else once what is out
    /// comes back, cancelled, to [`serve`](Self::serve).
    fn end(&self, key: usize) {
        let mut watches = self.watches();
        let Some(watch) = watches.get_mut(&key) else {
            return;
        };
        watch.ending = true;
        if watch.reading {
            // A read done already, whose cancel fails, has its completion on its way all the
            // same: either frees it.
            // SAFETY: the read this watch has out, on its own directory handle.
            let _ = unsafe { CancelIoEx(watch.dir.as_raw_handle(), &watch.overlapped) };
        }
        for oplock in &mut watch.oplocks {
            oplock.retire();
        }
        if watch.idle() {
            watches.remove(&key);
        }
    }
}

impl Watch {
    /// Nothing of it out: its read, nor any oplock's request.
    fn idle(&self) -> bool {
        !self.reading && self.oplocks.iter().all(|o| !o.out)
    }

    /// Reads its directory's next changes into the port.
    fn read(&mut self) -> io::Result<()> {
        self.overlapped = OVERLAPPED::default();
        let len = u32::try_from(RECORDS).map_err(io::Error::other)?;
        // SAFETY: the directory handle this watch holds, opened for overlapped I/O and bound
        // to the port; its records' RECORDS bytes and its OVERLAPPED, boxed, which stay
        // until the read's completion comes back ([`Watcher::end`]).
        let started = unsafe {
            ReadDirectoryChangesW(
                self.dir.as_raw_handle(),
                self.records.as_mut_ptr().cast(),
                len,
                0,
                CHANGES,
                std::ptr::null_mut(),
                &mut self.overlapped,
                None,
            )
        };
        if started == 0 {
            return Err(io::Error::last_os_error());
        }
        self.reading = true;
        Ok(())
    }

    /// Its read's completion, `ok` and of `bytes`: whether it holds a change watched. Read
    /// again unless it is ending or its read failed: a directory gone fails the read
    /// (ERROR_ACCESS_DENIED), which is a change, and would fail again.
    fn read_done(&mut self, ok: bool, bytes: usize) -> bool {
        self.reading = false;
        if self.ending {
            return false;
        }
        let woke = !ok || bytes == 0 || self.changed(bytes);
        if ok && self.read().is_err() {
            return true;
        }
        woke
    }

    /// The completion of the oplock request at `overlapped`, `ok`: a break, which is a write
    /// to its file, whose oplock is asked for again; a request no longer its file's freed.
    fn oplock_done(&mut self, overlapped: *mut OVERLAPPED, ok: bool) -> bool {
        let Some(at) = self
            .oplocks
            .iter()
            .position(|o| std::ptr::eq(overlapped, &raw const o.overlapped))
        else {
            return false;
        };
        let Some(oplock) = self.oplocks.get_mut(at) else {
            return false;
        };
        oplock.out = false;
        if !oplock.current || self.ending {
            self.oplocks.remove(at);
            return false;
        }
        // Broken, or failed (its file gone): a change. Asked for again where it broke; one
        // not granted again leaves the file to its name in the records.
        if ok && oplock.request().is_err() {
            self.oplocks.remove(at);
        }
        true
    }

    /// Whether the `bytes` of records its read wrote hold a change it watches: a name come
    /// or gone, or a write to its file where its file holds no oplock.
    fn changed(&self, bytes: usize) -> bool {
        let bytes = bytes.min(RECORDS);
        let head = std::mem::offset_of!(FILE_NOTIFY_INFORMATION, FileName);
        let base = self.records.as_ptr().cast::<u8>();
        let mut at = 0usize;
        loop {
            let Some(name_at) = at.checked_add(head).filter(|&n| n <= bytes) else {
                // A record past what was read: something changed, though not said whole.
                return true;
            };
            // SAFETY: `at..name_at` lies within the `bytes` the read wrote, just checked.
            let record = unsafe { std::ptr::read_unaligned(base.add(at).cast::<FILE_NOTIFY_INFORMATION>()) };
            match record.Action {
                FILE_ACTION_ADDED
                | FILE_ACTION_REMOVED
                | FILE_ACTION_RENAMED_OLD_NAME
                | FILE_ACTION_RENAMED_NEW_NAME => {
                    return true;
                }
                FILE_ACTION_MODIFIED if self.oplocks.iter().all(|o| !o.current) => {
                    let units = usize::try_from(record.FileNameLength).unwrap_or(usize::MAX) / 2;
                    let Some(end) = units
                        .checked_mul(2)
                        .and_then(|n| name_at.checked_add(n))
                        .filter(|&e| e <= bytes)
                    else {
                        return true;
                    };
                    let mut name = vec![0u16; units];
                    // SAFETY: `name_at..end` lies within what was read, just checked, and
                    // `name` has room for its units.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            base.add(name_at),
                            name.as_mut_ptr().cast::<u8>(),
                            end - name_at,
                        );
                    }
                    if self.named.as_deref().is_some_and(|file| same_name(file, &name)) {
                        return true;
                    }
                }
                _ => {}
            }
            if record.NextEntryOffset == 0 {
                return false;
            }
            at = match at.checked_add(usize::try_from(record.NextEntryOffset).unwrap_or(usize::MAX)) {
                Some(next) if next < bytes => next,
                _ => return true,
            };
        }
    }
}

impl Oplock {
    /// Asks for a read oplock on its file, which another handle's write breaks: out until then.
    fn request(&mut self) -> io::Result<()> {
        self.overlapped = OVERLAPPED::default();
        self.output = REQUEST_OPLOCK_OUTPUT_BUFFER::default();
        let mut returned = 0u32;
        let (input, output) = (
            u32::try_from(size_of::<REQUEST_OPLOCK_INPUT_BUFFER>()).map_err(io::Error::other)?,
            u32::try_from(size_of::<REQUEST_OPLOCK_OUTPUT_BUFFER>()).map_err(io::Error::other)?,
        );
        // SAFETY: the file handle this oplock holds, opened for overlapped I/O and bound to
        // the port; its input, output and OVERLAPPED, boxed, which stay until the request's
        // completion comes back.
        let asked = unsafe {
            DeviceIoControl(
                self.file.as_raw_handle(),
                FSCTL_REQUEST_OPLOCK,
                (&raw const self.input).cast(),
                input,
                (&raw mut self.output).cast(),
                output,
                &mut returned,
                &mut self.overlapped,
            )
        };
        let e = io::Error::last_os_error();
        if asked == 0 && e.raw_os_error() != i32::try_from(ERROR_IO_PENDING).ok() {
            return Err(e);
        }
        // Granted and out; or done at once, its completion queued all the same.
        self.out = true;
        Ok(())
    }

    /// No longer the file watched: its request cancelled, its completion freeing it.
    fn retire(&mut self) {
        self.current = false;
        if self.out {
            // SAFETY: the request this oplock has out, on its own handle.
            let _ = unsafe { CancelIoEx(self.file.as_raw_handle(), &self.overlapped) };
        }
    }
}

/// Whether two names of a directory are one, as Windows' file systems compare them: without
/// regard to case, by the system's own table (CompareStringOrdinal).
fn same_name(a: &[u16], b: &[u16]) -> bool {
    let (Ok(la), Ok(lb)) = (i32::try_from(a.len()), i32::try_from(b.len())) else {
        return false;
    };
    // SAFETY: two buffers of their lengths, read alone.
    unsafe { CompareStringOrdinal(a.as_ptr(), la, b.as_ptr(), lb, 1) == CSTR_EQUAL }
}

impl FileWatch {
    /// A watch on the directory `dir` holds open, wherever it goes.
    pub fn new(dir: &File) -> io::Result<FileWatch> {
        let watcher = watcher()
            .ok_or_else(|| io::Error::other("the process's watch of its files could not be made"))?;
        // A handle of its own, for overlapped reads: asynchronous, no synchronous I/O asked.
        let watched = OwnedHandle::from(super::windows_files::reopen_dir(dir, FILE_LIST_DIRECTORY, 0)?);
        let event = new_event()?;
        let key = watcher.next.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the directory's handle, bound to the process's port under `key`.
        if unsafe { CreateIoCompletionPort(watched.as_raw_handle(), watcher.port.as_raw_handle(), key, 0) }
            .is_null()
        {
            return Err(io::Error::last_os_error());
        }
        let mut watch = Box::new(Watch {
            dir: watched,
            overlapped: OVERLAPPED::default(),
            records: vec![0u32; RECORDS / 4],
            oplocks: Vec::new(),
            named: None,
            event: event.try_clone()?,
            reading: false,
            ending: false,
        });
        // Under the lock: its first completion finds it there.
        let mut watches = watcher.watches();
        watch.read()?;
        watches.insert(key, watch);
        Ok(FileWatch { key, event })
    }

    /// Watches `file`, one in the directory, for writes, in place of the one watched before
    /// (unix.rs `FileWatch::file`): by a read oplock on a handle of its own, else by its name
    /// in the directory's records.
    pub fn file(&mut self, file: &File) -> io::Result<()> {
        let path = super::final_path(file)?;
        let name: Vec<u16> = path
            .file_name()
            .map(|n| std::os::windows::ffi::OsStrExt::encode_wide(n).collect())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "a file with no name"))?;
        let watcher =
            watcher().ok_or_else(|| io::Error::other("the process's watch of its files is gone"))?;
        let own = OwnedHandle::from(super::windows_files::reopen_file(file, FILE_READ_ATTRIBUTES, 0)?);
        let mut watches = watcher.watches();
        let Some(watch) = watches.get_mut(&self.key) else {
            return Ok(());
        };
        for oplock in &mut watch.oplocks {
            oplock.retire();
        }
        watch.oplocks.retain(|o| o.out);
        watch.named = Some(name);
        // SAFETY: the file's own handle, bound to the process's port under this watch's key.
        if unsafe { CreateIoCompletionPort(own.as_raw_handle(), watcher.port.as_raw_handle(), self.key, 0) }
            .is_null()
        {
            return Err(io::Error::last_os_error());
        }
        let mut oplock = Box::new(Oplock {
            file: own,
            overlapped: OVERLAPPED::default(),
            input: REQUEST_OPLOCK_INPUT_BUFFER {
                StructureVersion: u16::try_from(REQUEST_OPLOCK_CURRENT_VERSION).unwrap_or(1),
                StructureLength: u16::try_from(size_of::<REQUEST_OPLOCK_INPUT_BUFFER>()).unwrap_or(u16::MAX),
                RequestedOplockLevel: OPLOCK_LEVEL_CACHE_READ,
                Flags: REQUEST_OPLOCK_INPUT_FLAG_REQUEST,
            },
            output: REQUEST_OPLOCK_OUTPUT_BUFFER::default(),
            out: false,
            current: true,
        });
        // Not granted (a file system without oplocks): its name in the records stands in.
        if oplock.request().is_ok() {
            watch.oplocks.push(oplock);
        }
        Ok(())
    }

    /// The handle to wait on: an event, set until [`clear`](Self::clear).
    pub fn fd(&self) -> BorrowedHandle<'_> {
        self.event.as_handle()
    }

    /// Takes the changes seen, so the handle waits for the next.
    pub fn clear(&self) {
        // SAFETY: the event this watch holds.
        unsafe { ResetEvent(self.event.as_raw_handle()) };
    }
}

impl Drop for FileWatch {
    fn drop(&mut self) {
        if let Some(watcher) = WATCHER.get().and_then(Option::as_ref) {
            watcher.end(self.key);
        }
    }
}

/// A manual-reset event, not set.
fn new_event() -> io::Result<OwnedHandle> {
    // SAFETY: an unnamed event, manual-reset, not signaled: a handle or null.
    let raw: HANDLE = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the event just made, owned by nothing else.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    /// Whether `watch` is set within `ms`.
    fn woke(watch: &FileWatch, ms: u32) -> bool {
        // SAFETY: the watch's event, waited on.
        unsafe { WaitForSingleObject(watch.fd().as_raw_handle(), ms) == WAIT_OBJECT_0 }
    }

    /// A name's coming and going wakes the watch; a write to the file watched wakes it, from
    /// a writer that holds the file open, writing on; a write to another file does not, nor,
    /// once another file is watched, a write to the one before.
    #[test]
    fn a_watch_wakes_for_names_and_its_own_files_writes() {
        let dir = shards_testdir::TempDir::new("watch").unwrap();
        std::fs::write(dir.join("a"), b"").unwrap();
        std::fs::write(dir.join("b"), b"").unwrap();
        let opened = super::super::open_dir(dir.path()).unwrap();
        let mut watch = FileWatch::new(&opened).unwrap();
        assert!(!woke(&watch, 50), "nothing yet");
        std::fs::write(dir.join("c"), b"").unwrap();
        assert!(woke(&watch, 5000), "a name came");
        watch.clear();
        assert!(!woke(&watch, 50), "cleared");
        let mut writer = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join("a"))
            .unwrap();
        watch.file(&File::open(dir.join("a")).unwrap()).unwrap();
        let mut other = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join("b"))
            .unwrap();
        other.write_all(b"other").unwrap();
        assert!(!woke(&watch, 500), "another file's write");
        let mut waited = Vec::new();
        for n in 0..5 {
            let start = Instant::now();
            writer.write_all(format!("line {n}\n").as_bytes()).unwrap();
            assert!(woke(&watch, 5000), "write {n} to the file watched, held open");
            waited.push(start.elapsed());
            watch.clear();
        }
        eprintln!("woken after each write, its file held open: {waited:?}");
        watch.file(&File::open(dir.join("b")).unwrap()).unwrap();
        writer.write_all(b"no longer watched\n").unwrap();
        assert!(!woke(&watch, 500), "the file watched before");
        other.write_all(b"watched now").unwrap();
        assert!(woke(&watch, 5000), "the file watched now");
        watch.clear();
        std::fs::rename(dir.join("c"), dir.join("c2")).unwrap();
        assert!(woke(&watch, 5000), "a rename");
        watch.clear();
        drop(writer);
        drop(other);
        std::fs::remove_file(dir.join("c2")).unwrap();
        assert!(woke(&watch, 5000), "a name went");
    }

    /// Watches made and dropped, many of them, their reads and oplocks out, are each freed:
    /// their handles, events and buffers let go of as what was out comes back, cancelled.
    #[test]
    fn dropped_watches_are_freed() {
        let dir = shards_testdir::TempDir::new("watches").unwrap();
        std::fs::write(dir.join("f"), b"").unwrap();
        let opened = super::super::open_dir(dir.path()).unwrap();
        let file = File::open(dir.join("f")).unwrap();
        let keys: Vec<usize> = (0..500)
            .map(|n| {
                let mut watch = FileWatch::new(&opened).unwrap();
                if n % 2 == 0 {
                    watch.file(&file).unwrap();
                    watch.file(&file).unwrap();
                }
                let key = watch.key;
                drop(watch);
                key
            })
            .collect();
        let held = || {
            let watches = super::watcher().unwrap().watches();
            keys.iter().filter(|k| watches.contains_key(k)).count()
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while held() > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(held(), 0, "watches dropped and still held");
    }
}
