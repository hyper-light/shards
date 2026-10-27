//! PM M344: what a Windows directory watch hears of a file's writes, beside what a read oplock
//! hears, for FileWatch on Windows (vmm platform/windows_watch.rs).
//!
//! 1. ReadDirectoryChangesW, asked for size and last-write changes: of 20 appends by a writer
//!    that holds its file open, each waited for 5 s, how many it says, and how soon.
//! 2. ReadDirectoryChangesW, asked for names: each of 1,000 files made, how soon it says so.
//! 3. A read oplock (OPLOCK_LEVEL_CACHE_READ) on a handle of the watcher's own, beside a writer
//!    that holds the file open: granted or not, and each of 1,000 appends' break, how soon.
//! 4. The same oplock asked before a writer opens: whether the writer's open breaks it, and
//!    then its append.
//! 5. A directory's handle opened again: ReOpenFile with each of five ways asked, and
//!    NtCreateFile of no name relative to the handle.
//! 6. A rename over a file another handle holds open, as FileWatch's sibling write_in renames:
//!    the holder sharing delete or not, with and without FILE_RENAME_POSIX_SEMANTICS.
//!
//! Times are from just before the write (or create) to the wait's return, in µs: n, p50,
//! p90, p99 and max.

use std::io::Write as _;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::time::Instant;

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_DIRECTORY_FILE, FILE_OPEN, FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
};
use windows_sys::Win32::Foundation::{
    ERROR_IO_PENDING, GENERIC_READ, GetLastError, INVALID_HANDLE_VALUE, UNICODE_STRING, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_GENERIC_WRITE, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReOpenFile,
    ReadDirectoryChangesW, SYNCHRONIZE,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, DeviceIoControl, GetOverlappedResult, IO_STATUS_BLOCK, OVERLAPPED,
};
use windows_sys::Win32::System::Ioctl::{
    FSCTL_REQUEST_OPLOCK, OPLOCK_LEVEL_CACHE_READ, REQUEST_OPLOCK_CURRENT_VERSION,
    REQUEST_OPLOCK_INPUT_BUFFER, REQUEST_OPLOCK_INPUT_FLAG_REQUEST, REQUEST_OPLOCK_OUTPUT_BUFFER,
};
use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};

const SHARE: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

fn wide(path: &std::path::Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn event() -> OwnedHandle {
    // SAFETY: an unnamed manual-reset event, not set.
    let raw = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    assert!(!raw.is_null());
    // SAFETY: the event just made.
    unsafe { OwnedHandle::from_raw_handle(raw) }
}

fn open(path: &std::path::Path, access: u32, flags: u32) -> OwnedHandle {
    let name = wide(path);
    // SAFETY: a NUL-terminated wide path.
    let raw = unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            SHARE,
            std::ptr::null(),
            OPEN_EXISTING,
            flags,
            std::ptr::null_mut(),
        )
    };
    assert!(
        raw != INVALID_HANDLE_VALUE,
        "{}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
    // SAFETY: the handle just opened.
    unsafe { OwnedHandle::from_raw_handle(raw) }
}

fn waited(event: &OwnedHandle, ms: u32) -> bool {
    // SAFETY: an event this holds.
    unsafe { WaitForSingleObject(event.as_raw_handle(), ms) == WAIT_OBJECT_0 }
}

fn stats(us: &mut [f64]) -> String {
    if us.is_empty() {
        return "n=0".into();
    }
    us.sort_by(f64::total_cmp);
    let at = |q: f64| us[((q * us.len() as f64) as usize).min(us.len() - 1)];
    format!(
        "n={} p50={:.1} p90={:.1} p99={:.1} max={:.1}",
        us.len(),
        at(0.5),
        at(0.9),
        at(0.99),
        us[us.len() - 1]
    )
}

/// A directory read for `filter` changes into `buf`, signalling `ev`.
fn read_changes(dir: &OwnedHandle, buf: &mut [u32], filter: u32, ov: &mut OVERLAPPED, ev: &OwnedHandle) {
    *ov = OVERLAPPED {
        hEvent: ev.as_raw_handle(),
        ..OVERLAPPED::default()
    };
    // SAFETY: an overlapped directory handle, `buf` and `ov` kept until the read is waited for.
    let ok = unsafe {
        ReadDirectoryChangesW(
            dir.as_raw_handle(),
            buf.as_mut_ptr().cast(),
            (buf.len() * 4) as u32,
            0,
            filter,
            std::ptr::null_mut(),
            ov,
            None,
        )
    };
    assert!(
        ok != 0,
        "ReadDirectoryChangesW: {}",
        std::io::Error::last_os_error()
    );
}

fn finish(handle: &OwnedHandle, ov: &OVERLAPPED) {
    let mut moved = 0u32;
    // SAFETY: the call begun on `ov`, waited for.
    unsafe { GetOverlappedResult(handle.as_raw_handle(), ov, &mut moved, 1) };
}

fn cancel(handle: &OwnedHandle, ov: &OVERLAPPED) {
    // SAFETY: the call begun on `ov`, cancelled and then waited for.
    unsafe { CancelIoEx(handle.as_raw_handle(), ov) };
    finish(handle, ov);
}

fn directory_writes(base: &std::path::Path) {
    let dir = base.join("writes");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("f"), b"").unwrap();
    let watched = open(
        &dir,
        FILE_LIST_DIRECTORY,
        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
    );
    let ev = event();
    let mut buf = vec![0u32; 16 << 8];
    let mut ov = OVERLAPPED::default();
    let mut writer = std::fs::OpenOptions::new()
        .append(true)
        .open(dir.join("f"))
        .unwrap();
    read_changes(
        &watched,
        &mut buf,
        FILE_NOTIFY_CHANGE_SIZE | FILE_NOTIFY_CHANGE_LAST_WRITE,
        &mut ov,
        &ev,
    );
    let (mut said, mut us) = (0, Vec::new());
    for _ in 0..20 {
        let start = Instant::now();
        writer.write_all(b"0123456789").unwrap();
        if waited(&ev, 5000) {
            us.push(start.elapsed().as_secs_f64() * 1e6);
            said += 1;
            finish(&watched, &ov);
            // SAFETY: the event this holds.
            unsafe { ResetEvent(ev.as_raw_handle()) };
            read_changes(
                &watched,
                &mut buf,
                FILE_NOTIFY_CHANGE_SIZE | FILE_NOTIFY_CHANGE_LAST_WRITE,
                &mut ov,
                &ev,
            );
        }
    }
    cancel(&watched, &ov);
    println!(
        "1. directory changes, size and last write, writer holding the file open: {said} of 20 appends said within 5 s; {}",
        stats(&mut us)
    );
}

fn directory_names(base: &std::path::Path) {
    let dir = base.join("names");
    std::fs::create_dir(&dir).unwrap();
    let watched = open(
        &dir,
        FILE_LIST_DIRECTORY,
        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
    );
    let ev = event();
    let mut buf = vec![0u32; 16 << 8];
    let mut ov = OVERLAPPED::default();
    let mut us = Vec::new();
    for n in 0..1000 {
        read_changes(&watched, &mut buf, FILE_NOTIFY_CHANGE_FILE_NAME, &mut ov, &ev);
        let start = Instant::now();
        std::fs::write(dir.join(format!("n{n}")), b"").unwrap();
        assert!(waited(&ev, 5000), "name {n} not said");
        us.push(start.elapsed().as_secs_f64() * 1e6);
        finish(&watched, &ov);
        // SAFETY: the event this holds.
        unsafe { ResetEvent(ev.as_raw_handle()) };
    }
    println!("2. directory changes, a name made: {}", stats(&mut us));
}

/// Asks for a read oplock on `file`: whether granted (pending).
fn request(
    file: &OwnedHandle,
    ov: &mut OVERLAPPED,
    ev: &OwnedHandle,
    out: &mut REQUEST_OPLOCK_OUTPUT_BUFFER,
) -> bool {
    let input = REQUEST_OPLOCK_INPUT_BUFFER {
        StructureVersion: REQUEST_OPLOCK_CURRENT_VERSION as u16,
        StructureLength: size_of::<REQUEST_OPLOCK_INPUT_BUFFER>() as u16,
        RequestedOplockLevel: OPLOCK_LEVEL_CACHE_READ,
        Flags: REQUEST_OPLOCK_INPUT_FLAG_REQUEST,
    };
    *ov = OVERLAPPED {
        hEvent: ev.as_raw_handle(),
        ..OVERLAPPED::default()
    };
    let mut returned = 0u32;
    // SAFETY: an overlapped file handle, `input`, `out` and `ov` kept until the request is
    // waited for.
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_REQUEST_OPLOCK,
            (&raw const input).cast(),
            size_of::<REQUEST_OPLOCK_INPUT_BUFFER>() as u32,
            (out as *mut REQUEST_OPLOCK_OUTPUT_BUFFER).cast(),
            size_of::<REQUEST_OPLOCK_OUTPUT_BUFFER>() as u32,
            &mut returned,
            ov,
        )
    };
    // SAFETY: no arguments.
    ok == 0 && unsafe { GetLastError() } == ERROR_IO_PENDING
}

fn oplocks(base: &std::path::Path) {
    let dir = base.join("oplock");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("f"), b"").unwrap();
    let mut writer = std::fs::OpenOptions::new()
        .append(true)
        .open(dir.join("f"))
        .unwrap();
    let own = open(&dir.join("f"), FILE_READ_ATTRIBUTES, FILE_FLAG_OVERLAPPED);
    let ev = event();
    let mut ov = OVERLAPPED::default();
    let mut out = REQUEST_OPLOCK_OUTPUT_BUFFER::default();
    let (mut granted, mut us, mut levels) = (0, Vec::new(), std::collections::BTreeSet::new());
    for n in 0..1000 {
        if !request(&own, &mut ov, &ev, &mut out) {
            continue;
        }
        granted += 1;
        let start = Instant::now();
        writer.write_all(b"0123456789").unwrap();
        assert!(waited(&ev, 5000), "append {n} broke nothing");
        us.push(start.elapsed().as_secs_f64() * 1e6);
        finish(&own, &ov);
        levels.insert((out.NewOplockLevel, out.Flags));
        // SAFETY: the event this holds.
        unsafe { ResetEvent(ev.as_raw_handle()) };
    }
    println!(
        "3. read oplock beside a writer holding the file open: granted {granted} of 1000; broken by each append: {}; new level and flags {levels:?}",
        stats(&mut us)
    );
    drop(writer);
    let granted = request(&own, &mut ov, &ev, &mut out);
    let mut writer = std::fs::OpenOptions::new()
        .append(true)
        .open(dir.join("f"))
        .unwrap();
    let open_broke = waited(&ev, 200);
    let start = Instant::now();
    writer.write_all(b"0123456789").unwrap();
    let write_broke = open_broke || waited(&ev, 5000);
    let after = start.elapsed();
    if granted {
        if write_broke {
            finish(&own, &ov);
        } else {
            cancel(&own, &ov);
        }
    }
    println!(
        "4. read oplock asked before a writer opens: granted {granted}; the open broke it {open_broke}; the append {}{:.1} µs",
        if write_broke && !open_broke {
            "broke it after "
        } else {
            "after "
        },
        after.as_secs_f64() * 1e6
    );
}

fn reopen(base: &std::path::Path) {
    let dir = base.join("reopen");
    std::fs::create_dir(&dir).unwrap();
    let held = open(&dir, GENERIC_READ, FILE_FLAG_BACKUP_SEMANTICS);
    for (label, access, flags) in [
        (
            "FILE_LIST_DIRECTORY|SYNCHRONIZE, backup semantics",
            FILE_LIST_DIRECTORY | SYNCHRONIZE,
            FILE_FLAG_BACKUP_SEMANTICS,
        ),
        (
            "GENERIC_READ, backup semantics",
            GENERIC_READ,
            FILE_FLAG_BACKUP_SEMANTICS,
        ),
        (
            "FILE_LIST_DIRECTORY, backup semantics, overlapped",
            FILE_LIST_DIRECTORY,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
        ),
        (
            "FILE_GENERIC_WRITE, backup semantics",
            FILE_GENERIC_WRITE,
            FILE_FLAG_BACKUP_SEMANTICS,
        ),
        ("FILE_LIST_DIRECTORY, no flags", FILE_LIST_DIRECTORY, 0),
    ] {
        // SAFETY: ReOpenFile of a handle this holds.
        let raw = unsafe { ReOpenFile(held.as_raw_handle(), access, SHARE, flags) };
        let result = if raw == INVALID_HANDLE_VALUE || raw.is_null() {
            std::io::Error::last_os_error().to_string()
        } else {
            // SAFETY: the handle just opened.
            drop(unsafe { OwnedHandle::from_raw_handle(raw) });
            "opened".into()
        };
        println!("5. ReOpenFile of a directory's handle, {label}: {result}");
    }
    let object = UNICODE_STRING {
        Length: 0,
        MaximumLength: 0,
        Buffer: std::ptr::null_mut(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: held.as_raw_handle(),
        ObjectName: &object,
        Attributes: 0,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut status_block = IO_STATUS_BLOCK::default();
    let mut handle = std::ptr::null_mut();
    // SAFETY: NtCreateFile of no name relative to a directory handle this holds.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            FILE_LIST_DIRECTORY | SYNCHRONIZE,
            &attributes,
            &mut status_block,
            std::ptr::null(),
            0,
            SHARE,
            FILE_OPEN,
            FILE_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT,
            std::ptr::null(),
            0,
        )
    };
    if status >= 0 {
        // SAFETY: the handle just opened.
        drop(unsafe { OwnedHandle::from_raw_handle(handle) });
    }
    println!("5. NtCreateFile of no name relative to the directory's handle: status {status:#x}");
}

/// A rename over a file another handle holds open, by NtSetInformationFile relative to the
/// directory's handle, as write_in renames: the holder sharing delete or not, with and
/// without FILE_RENAME_POSIX_SEMANTICS; whether it went, and what the holder and the name
/// then read.
fn renames(base: &std::path::Path) {
    use std::io::Read as _;
    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_RENAME_INFORMATION, FILE_RENAME_POSIX_SEMANTICS, FILE_RENAME_REPLACE_IF_EXISTS,
        FileRenameInformationEx, NtSetInformationFile,
    };
    use windows_sys::Win32::Storage::FileSystem::{CREATE_ALWAYS, DELETE, FILE_GENERIC_READ};
    let dir = base.join("renames");
    std::fs::create_dir(&dir).unwrap();
    let dir_handle = open(&dir, FILE_LIST_DIRECTORY, FILE_FLAG_BACKUP_SEMANTICS);
    for (holder_label, holder_share) in [
        (
            "read, write and delete",
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        ),
        ("read and write, not delete", FILE_SHARE_READ | FILE_SHARE_WRITE),
    ] {
        for (flags_label, flags) in [
            (
                "REPLACE_IF_EXISTS | POSIX_SEMANTICS",
                FILE_RENAME_REPLACE_IF_EXISTS | FILE_RENAME_POSIX_SEMANTICS,
            ),
            ("REPLACE_IF_EXISTS", FILE_RENAME_REPLACE_IF_EXISTS),
        ] {
            std::fs::write(dir.join("t"), b"old").unwrap();
            let target = wide(&dir.join("t"));
            // SAFETY: a NUL-terminated wide path.
            let raw = unsafe {
                CreateFileW(
                    target.as_ptr(),
                    FILE_GENERIC_READ,
                    holder_share,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            assert!(raw != INVALID_HANDLE_VALUE);
            // SAFETY: the handle just opened.
            let holder = std::fs::File::from(unsafe { OwnedHandle::from_raw_handle(raw) });
            let source = wide(&dir.join("s"));
            // SAFETY: a NUL-terminated wide path.
            let raw = unsafe {
                CreateFileW(
                    source.as_ptr(),
                    DELETE | FILE_GENERIC_WRITE,
                    SHARE,
                    std::ptr::null(),
                    CREATE_ALWAYS,
                    0,
                    std::ptr::null_mut(),
                )
            };
            assert!(raw != INVALID_HANDLE_VALUE);
            // SAFETY: the handle just opened.
            let mut new = std::fs::File::from(unsafe { OwnedHandle::from_raw_handle(raw) });
            new.write_all(b"new").unwrap();
            let name: Vec<u16> = "t".encode_utf16().collect();
            let size = size_of::<FILE_RENAME_INFORMATION>() + name.len() * 2;
            let mut buf = vec![0u64; size.div_ceil(8)];
            let info = buf.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
            // SAFETY: `buf` holds the structure and the name after its head.
            unsafe {
                (*info).Anonymous.Flags = flags;
                (*info).RootDirectory = dir_handle.as_raw_handle();
                (*info).FileNameLength = (name.len() * 2) as u32;
                std::ptr::copy_nonoverlapping(
                    name.as_ptr(),
                    (&raw mut (*info).FileName).cast::<u16>(),
                    name.len(),
                );
            }
            let mut status_block = IO_STATUS_BLOCK::default();
            // SAFETY: a handle opened with DELETE, and the structure `buf` holds.
            let status = unsafe {
                NtSetInformationFile(
                    new.as_raw_handle(),
                    &mut status_block,
                    info.cast(),
                    size as u32,
                    FileRenameInformationEx,
                )
            };
            drop(new);
            let mut held = String::new();
            let read_held = (&holder).read_to_string(&mut held).map(|_| held.clone());
            let named = std::fs::read_to_string(dir.join("t"));
            println!(
                "6. a rename over a file held open sharing {holder_label}, {flags_label}: status {status:#x}; the holder reads {read_held:?}; the name reads {named:?}"
            );
            drop(holder);
            let _ = std::fs::remove_file(dir.join("t"));
            let _ = std::fs::remove_file(dir.join("s"));
        }
    }
}

fn main() {
    #[expect(
        clippy::disallowed_methods,
        reason = "a standalone harness's files, removed as it ends, on the volume a user's temporary files are"
    )]
    let base = std::env::temp_dir().join(format!("windows-watch-{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    println!(
        "host: {} logical CPUs; {}",
        std::thread::available_parallelism().map_or(0, |n| n.get()),
        std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_default()
    );
    directory_writes(&base);
    directory_names(&base);
    oplocks(&base);
    reopen(&base);
    renames(&base);
    let _ = std::fs::remove_dir_all(&base);
}
