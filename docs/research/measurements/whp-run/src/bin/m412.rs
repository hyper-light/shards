//! What a DAX window's range of a host file's own section takes on Windows (PM M412, D197's
//! slice 4), before shards' share server answers a mapping with one: a file's section
//! reaches no further than the file, where a range is 2 MiB (FUSE_DAX_SZ). So it asks:
//! - can a range's placeholder be split at a page that is no multiple of the allocation
//!   granularity, the file's view placed in the first part and committed zeros in the rest;
//! - what the view reads past the file's end in its last page, and whether a store there
//!   reaches the file;
//! - whether the file can be cut short, or grown, while a view of it is mapped, or while a
//!   section of it is held with no view;
//! - whether a view sees a write made through another handle of the file (WriteFile, cached),
//!   and a read through such a handle sees a store made through the view;
//! - what a section of a file opened to read alone, made to read and write, answers;
//! - whether a file a view of which is mapped can have a hole punched in it, be renamed, and
//!   be removed with POSIX semantics, as a share's server does each for its guest.
//!
//! `cargo run --release --bin m412`, on the host to measure; it prints what each step
//! answered and exits 0 whatever it found.

#![allow(non_upper_case_globals)]

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt as _;

use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_ALWAYS, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_BEGIN, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile, SetEndOfFile, SetFilePointerEx, WriteFile,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MEM_COMMIT, MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER, MEM_RESERVE,
    MEM_RESERVE_PLACEHOLDER, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile3, PAGE_NOACCESS, PAGE_READONLY,
    PAGE_READWRITE, UnmapViewOfFile2, VirtualAlloc2, VirtualFree,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

const RANGE: usize = 2 << 20;
const PAGE: usize = 4096;
/// The file's size: a MiB and 100 bytes, its end within a range, in no page's last byte,
/// and past no multiple of the allocation granularity.
const SIZE: usize = (1 << 20) + 100;

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().chain([0]).collect()
}

fn last() -> String {
    // SAFETY: GetLastError reads this thread's last error.
    let code = unsafe { GetLastError() };
    format!("{} ({})", code, std::io::Error::from_raw_os_error(code as i32))
}

fn open(path: &str, access: u32, disposition: u32) -> HANDLE {
    let path = wide(path);
    // SAFETY: a NUL-terminated path; no security attributes or template.
    unsafe {
        CreateFileW(
            path.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            disposition,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    }
}

fn write_at(h: HANDLE, offset: i64, bytes: &[u8]) -> Result<(), String> {
    // SAFETY: a handle this holds; the file pointer moved, then `bytes` written.
    unsafe {
        if SetFilePointerEx(h, offset, std::ptr::null_mut(), FILE_BEGIN) == 0 {
            return Err(last());
        }
        let mut wrote = 0u32;
        if WriteFile(h, bytes.as_ptr(), bytes.len() as u32, &mut wrote, std::ptr::null_mut()) == 0 {
            return Err(last());
        }
    }
    Ok(())
}

fn read_at(h: HANDLE, offset: i64, len: usize) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; len];
    // SAFETY: as above, into a buffer of `len` bytes.
    unsafe {
        if SetFilePointerEx(h, offset, std::ptr::null_mut(), FILE_BEGIN) == 0 {
            return Err(last());
        }
        let mut got = 0u32;
        if ReadFile(h, buf.as_mut_ptr(), len as u32, &mut got, std::ptr::null_mut()) == 0 {
            return Err(last());
        }
        buf.truncate(got as usize);
    }
    Ok(buf)
}

fn set_size(h: HANDLE, size: i64) -> Result<(), String> {
    // SAFETY: a handle this holds, opened to write.
    unsafe {
        if SetFilePointerEx(h, size, std::ptr::null_mut(), FILE_BEGIN) == 0 || SetEndOfFile(h) == 0 {
            return Err(last());
        }
    }
    Ok(())
}

fn size_of(path: &str) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(u64::MAX)
}

fn section(file: HANDLE, protect: u32) -> Result<HANDLE, String> {
    // SAFETY: a file handle this holds; the section as long as the file (sizes 0).
    let s = unsafe { CreateFileMappingW(file, std::ptr::null(), protect, 0, 0, std::ptr::null()) };
    if s.is_null() { Err(last()) } else { Ok(s) }
}

fn main() {
    let dir = std::env::temp_dir().join(format!("m412-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("f").to_string_lossy().into_owned();
    let file = open(&path, GENERIC_READ | GENERIC_WRITE, CREATE_ALWAYS);
    if file == INVALID_HANDLE_VALUE {
        println!("open: {}", last());
        return;
    }
    let data: Vec<u8> = (0..SIZE).map(|i| (i % 251) as u8).collect();
    println!("write {SIZE} bytes: {:?}", write_at(file, 0, &data));
    let sec = match section(file, PAGE_READWRITE) {
        Ok(s) => s,
        Err(e) => {
            println!("section: {e}");
            return;
        }
    };
    // SAFETY: a placeholder of a range, reserved for this probe alone.
    let base = unsafe {
        VirtualAlloc2(
            GetCurrentProcess(),
            std::ptr::null(),
            RANGE,
            MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
            PAGE_NOACCESS,
            std::ptr::null_mut(),
            0,
        )
    };
    println!("placeholder of {RANGE}: {}", if base.is_null() { last() } else { "ok".into() });
    if base.is_null() {
        return;
    }
    let head = SIZE.div_ceil(PAGE) * PAGE;
    // SAFETY: the placeholder above, split at `head`.
    let split = unsafe { VirtualFree(base, head, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER) };
    println!(
        "split at {head} ({} KiB, {} a multiple of 64 KiB): {}",
        head / 1024,
        if head % (64 << 10) == 0 { "" } else { "not" },
        if split == 0 { last() } else { "ok".into() }
    );
    // The section's view into the placeholder's first part: asked as long as that part, as
    // long as the file, and to the section's end (0).
    let mut view = MEMORY_MAPPED_VIEW_ADDRESS { Value: std::ptr::null_mut() };
    for asked in [head, SIZE, 0] {
        // SAFETY: a view of the section into the placeholder's first part.
        view = unsafe {
            MapViewOfFile3(sec, GetCurrentProcess(), base, 0, asked, MEM_REPLACE_PLACEHOLDER, PAGE_READWRITE, std::ptr::null_mut(), 0)
        };
        println!("view asked {asked} bytes into it: {}", if view.Value.is_null() { last() } else { "ok".into() });
        if !view.Value.is_null() {
            break;
        }
    }
    let tail = unsafe { base.cast::<u8>().add(head) }.cast::<c_void>();
    // SAFETY: the placeholder's second part, committed in its place.
    let zeros = unsafe {
        VirtualAlloc2(
            GetCurrentProcess(),
            tail,
            RANGE - head,
            MEM_RESERVE | MEM_COMMIT | MEM_REPLACE_PLACEHOLDER,
            PAGE_READWRITE,
            std::ptr::null_mut(),
            0,
        )
    };
    println!("zeros of {} after it: {}", RANGE - head, if zeros.is_null() { last() } else { "ok".into() });
    if view.Value.is_null() {
        return;
    }
    let v = view.Value.cast::<u8>();
    // SAFETY: reads and writes within the view's `head` bytes.
    unsafe {
        let same = (0..SIZE).all(|i| *v.add(i) == data[i]);
        let past: Vec<u8> = (SIZE..head).map(|i| *v.add(i)).collect();
        println!("view reads the file: {same}; past its end, in its last page, all zero: {}", past.iter().all(|&b| b == 0));
        *v.add(SIZE + 10) = 0xAB;
        *v.add(10) = 0xCD;
    }
    // Coherence: a write through another handle, read through the view; a store through
    // the view, read through that handle.
    let other = open(&path, GENERIC_READ | GENERIC_WRITE, OPEN_EXISTING);
    println!("another handle: {}", if other == INVALID_HANDLE_VALUE { last() } else { "ok".into() });
    println!("WriteFile at 20: {:?}", write_at(other, 20, &[0xEE]));
    // SAFETY: within the view.
    println!("the view at 20 after it: {:#x}", unsafe { *v.add(20) });
    println!("ReadFile at 10 after the view's store: {:?}", read_at(other, 10, 1));
    println!("ReadFile past the end, at {}: {:?}", SIZE + 10, read_at(other, (SIZE + 10) as i64, 1));
    // Cut short and grown with the view mapped, and with the section held but no view.
    println!("cut to 4096 with a view mapped: {:?}, size {}", set_size(other, 4096), size_of(&path));
    println!("grow to {} with a view mapped: {:?}, size {}", SIZE + 8192, set_size(other, (SIZE + 8192) as i64), size_of(&path));
    // SAFETY: the view above, back to its placeholder.
    let unmapped = unsafe { UnmapViewOfFile2(GetCurrentProcess(), view, MEM_PRESERVE_PLACEHOLDER) };
    println!("view unmapped: {}", if unmapped == 0 { last() } else { "ok".into() });
    println!("the store past the end reached the file: {:?}", read_at(other, (SIZE + 10) as i64, 1));
    println!("cut to 4096, the section held, no view: {:?}, size {}", set_size(other, 4096), size_of(&path));
    // SAFETY: the section this made.
    unsafe { CloseHandle(sec) };
    println!("cut to 4096, no section: {:?}, size {}", set_size(other, 4096), size_of(&path));
    // A section to write of a file opened to read alone.
    let ro = open(&path, GENERIC_READ, OPEN_EXISTING);
    println!("PAGE_READWRITE section of a read-only handle: {:?}", section(ro, PAGE_READWRITE).map(|s| unsafe { CloseHandle(s) }));
    println!("PAGE_READONLY section of it: {:?}", section(ro, PAGE_READONLY).map(|s| unsafe { CloseHandle(s) }));
    // SAFETY: handles this opened.
    unsafe {
        CloseHandle(ro);
    }
    mapped_file_operations(&dir, other);
    // SAFETY: handles this opened.
    unsafe {
        CloseHandle(other);
        CloseHandle(file);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A hole punched, a rename and a POSIX removal of a file a view of which is mapped, its
/// section's handle closed once the view is made, as a VMM keeps only its views.
fn mapped_file_operations(dir: &std::path::Path, other: HANDLE) {
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO_EX,
        FileDispositionInfoEx, MoveFileExW, SetFileInformationByHandle,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{FILE_ZERO_DATA_INFORMATION, FSCTL_SET_ZERO_DATA};
    println!("grow back to {SIZE}: {:?}", set_size(other, SIZE as i64));
    let path = dir.join("f").to_string_lossy().into_owned();
    let Ok(sec) = section(other, PAGE_READWRITE) else {
        println!("a second section: {}", last());
        return;
    };
    // SAFETY: a view of the section anywhere, then the section's handle closed.
    let view = unsafe {
        MapViewOfFile3(sec, GetCurrentProcess(), std::ptr::null(), 0, SIZE, 0, PAGE_READWRITE, std::ptr::null_mut(), 0)
    };
    unsafe { CloseHandle(sec) };
    println!("a view, its section's handle closed: {}", if view.Value.is_null() { last() } else { "ok".into() });
    let zero = FILE_ZERO_DATA_INFORMATION { FileOffset: 0, BeyondFinalZero: 65536 };
    let mut got = 0u32;
    // SAFETY: FSCTL_SET_ZERO_DATA of its own structure on a handle opened to write.
    let punched = unsafe {
        DeviceIoControl(
            other,
            FSCTL_SET_ZERO_DATA,
            (&raw const zero).cast(),
            std::mem::size_of::<FILE_ZERO_DATA_INFORMATION>() as u32,
            std::ptr::null_mut(),
            0,
            &mut got,
            std::ptr::null_mut(),
        )
    };
    println!("a hole punched with the view mapped: {}", if punched == 0 { last() } else { "ok".into() });
    let moved = dir.join("g").to_string_lossy().into_owned();
    // SAFETY: two NUL-terminated paths.
    let renamed = unsafe { MoveFileExW(wide(&path).as_ptr(), wide(&moved).as_ptr(), 0) };
    println!("renamed with the view mapped: {}", if renamed == 0 { last() } else { "ok".into() });
    let doomed = open(&moved, DELETE, OPEN_EXISTING);
    let info = FILE_DISPOSITION_INFO_EX { Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS };
    // SAFETY: a handle opened with DELETE, and its structure.
    let removed = unsafe {
        SetFileInformationByHandle(doomed, FileDispositionInfoEx, (&raw const info).cast(), std::mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32)
    };
    println!("removed (POSIX) with the view mapped: {}", if removed == 0 { last() } else { "ok".into() });
    unsafe { CloseHandle(doomed) };
    println!("its name there after: {}", std::path::Path::new(&moved).exists());
    // SAFETY: the view made above, still mapped.
    if !view.Value.is_null() {
        println!("the view still reads: {:#x}", unsafe { *view.Value.cast::<u8>().add(70000) });
        unsafe { UnmapViewOfFile2(GetCurrentProcess(), view, 0) };
    }
}
