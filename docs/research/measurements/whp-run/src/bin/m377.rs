//! What a sparse file mapped takes of its disk's free space (PM M377): CI's saved.rs ran its
//! windows-2025 runner's temp disk out of its 29 GB, and on jupiter took 62 GB of its free
//! space at once, while no file under the tests' directories held more than 148 MB and the
//! page files kept their size. A run's writable layer is a sparse file of 10 GiB, mapped
//! shared and writable by its VM (D190); a restore's RAM a copy-on-write view of its
//! template's file (D196). For each: the volume's free space (GetDiskFreeSpaceExW) and what
//! the file holds (GetCompressedFileSizeW) as a sparse file of 1 GiB is made, its section
//! created, its view mapped, one page of every 1, 16 or 256 written through it, the view
//! flushed and unmapped, the section closed, and the file deleted.

use std::io::Write as _;
use std::os::windows::io::AsRawHandle as _;

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Storage::FileSystem::{GetCompressedFileSizeW, GetDiskFreeSpaceExW};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_COPY, FILE_MAP_WRITE, FlushViewOfFile, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    PAGE_READWRITE, PAGE_WRITECOPY, UnmapViewOfFile,
};

const GIB: u64 = 1 << 30;

fn free(dir: &std::path::Path) -> i64 {
    let wide: Vec<u16> = dir.as_os_str().encode_wide_nul();
    let mut avail = 0u64;
    // SAFETY: a NUL-terminated path, and one count to fill.
    unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &raw mut avail, std::ptr::null_mut(), std::ptr::null_mut()) };
    (avail / (1 << 20)) as i64
}

/// What `path` holds of its disk, in MiB (GetCompressedFileSizeW: a sparse file's holes
/// hold none).
fn held(path: &std::path::Path) -> i64 {
    let wide = path.as_os_str().encode_wide_nul();
    let mut high = 0u32;
    // SAFETY: a NUL-terminated path, and the high half to fill.
    let low = unsafe { GetCompressedFileSizeW(wide.as_ptr(), &raw mut high) };
    (((u64::from(high) << 32) | u64::from(low)) / (1 << 20)) as i64
}

/// The ranges of `file` its file system holds allocated (FSCTL_QUERY_ALLOCATED_RANGES), up to
/// 64 of them, as (offset, length) in KiB.
fn allocated(file: &std::fs::File) -> String {
    use windows_sys::Win32::System::Ioctl::{FILE_ALLOCATED_RANGE_BUFFER, FSCTL_QUERY_ALLOCATED_RANGES};
    let asked = FILE_ALLOCATED_RANGE_BUFFER {
        FileOffset: 0,
        Length: GIB as i64,
    };
    let mut found = vec![FILE_ALLOCATED_RANGE_BUFFER::default(); 64];
    let mut returned = 0u32;
    // SAFETY: a range asked of a file this process holds, and room for its answer.
    let done = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_QUERY_ALLOCATED_RANGES,
            (&raw const asked).cast(),
            size_of::<FILE_ALLOCATED_RANGE_BUFFER>() as u32,
            found.as_mut_ptr().cast(),
            (found.len() * size_of::<FILE_ALLOCATED_RANGE_BUFFER>()) as u32,
            &raw mut returned,
            std::ptr::null_mut(),
        )
    };
    let n = returned as usize / size_of::<FILE_ALLOCATED_RANGE_BUFFER>();
    let ranges: Vec<String> = found
        .iter()
        .take(n.min(4))
        .map(|r| format!("{}+{} KiB", r.FileOffset / 1024, r.Length / 1024))
        .collect();
    format!("{} range(s){}: {ranges:?}", n, if done == 0 { " (more)" } else { "" })
}

trait Wide {
    fn encode_wide_nul(&self) -> Vec<u16>;
}

impl Wide for std::ffi::OsStr {
    fn encode_wide_nul(&self) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt as _;
        self.encode_wide().chain([0]).collect()
    }
}

/// A sparse file of 1 GiB mapped shared and writable, a page written through the view, and
/// the whole file read through its handle (ReadFile, as a copy of its ranges reads it).
fn read_while_mapped(dir: &std::path::Path) -> Result<(), String> {
    use std::os::windows::fs::FileExt as _;
    let mut out = std::io::stdout();
    let start = free(dir);
    let path = dir.join(format!("m377-{}-read", std::process::id()));
    let say = |out: &mut std::io::Stdout, step: &str| {
        let _ = writeln!(
            out,
            "m377 read while mapped: {step}: free {} MiB ({:+} MiB), the file holds {} MiB",
            free(dir),
            free(dir) - start,
            held(&path)
        );
    };
    let file = std::fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let mut got = 0u32;
    // SAFETY: an FSCTL with no buffers on a file this process holds.
    unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_SET_SPARSE,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &raw mut got,
            std::ptr::null_mut(),
        )
    };
    file.set_len(GIB).map_err(|e| e.to_string())?;
    // SAFETY: a section of the whole file.
    let section =
        unsafe { CreateFileMappingW(file.as_raw_handle(), std::ptr::null(), PAGE_READWRITE, 0, 0, std::ptr::null()) };
    // SAFETY: a view of the whole section.
    let view = unsafe { MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 0) };
    if section.is_null() || view.Value.is_null() {
        return Err(format!("mapping: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: the view's first page.
    unsafe { view.Value.cast::<u8>().write_volatile(1) };
    // SAFETY: the view.
    unsafe { FlushViewOfFile(view.Value, 0) };
    say(&mut out, "mapped, a page written and flushed");
    let _ = writeln!(out, "m377 read while mapped: its allocated ranges, mapped: {}", allocated(&file));
    // Deleted while open and mapped, as a run's layer is once its VM holds it.
    std::fs::remove_file(&path).map_err(|e| e.to_string())?;
    let _ = writeln!(out, "m377 read while mapped: its allocated ranges, deleted: {}", allocated(&file));
    let mut buf = vec![0u8; 1 << 20];
    let mut at = 0u64;
    while at < GIB {
        file.seek_read(&mut buf, at).map_err(|e| e.to_string())?;
        at += buf.len() as u64;
    }
    say(&mut out, "read whole through its handle");
    let _ = writeln!(out, "m377 read while mapped: its allocated ranges, read whole: {}", allocated(&file));
    // SAFETY: the view, then the section, each let go of once.
    unsafe {
        UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: view.Value });
        CloseHandle(section);
    }
    say(&mut out, "unmapped, its section closed");
    drop(file);
    let _ = std::fs::remove_file(&path);
    say(&mut out, "deleted");
    Ok(())
}

fn run(dir: &std::path::Path, writable: bool, stride: usize) -> Result<(), String> {
    let mut out = std::io::stdout();
    let label = format!(
        "{}, a page in {stride} written",
        if writable { "shared, writable" } else { "copy-on-write" }
    );
    let start = free(dir);
    let path = dir.join(format!("m377-{}-{}-{stride}", std::process::id(), u8::from(writable)));
    let say = |out: &mut std::io::Stdout, step: &str| {
        let _ = writeln!(
            out,
            "m377 {label}: {step}: free {} MiB ({:+} MiB), the file holds {} MiB",
            free(dir),
            free(dir) - start,
            held(&path)
        );
    };
    let file = std::fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let mut got = 0u32;
    // SAFETY: an FSCTL with no buffers on a file this process holds.
    let sparse = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_SET_SPARSE,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &raw mut got,
            std::ptr::null_mut(),
        )
    };
    if sparse == 0 {
        return Err(format!("FSCTL_SET_SPARSE: {}", std::io::Error::last_os_error()));
    }
    file.set_len(GIB).map_err(|e| e.to_string())?;
    say(&mut out, "made sparse, 1 GiB long");
    let protect = if writable { PAGE_READWRITE } else { PAGE_WRITECOPY };
    // SAFETY: a section of the whole file.
    let section = unsafe { CreateFileMappingW(file.as_raw_handle(), std::ptr::null(), protect, 0, 0, std::ptr::null()) };
    if section.is_null() {
        return Err(format!("CreateFileMappingW: {}", std::io::Error::last_os_error()));
    }
    say(&mut out, "its section made");
    let access = if writable { FILE_MAP_WRITE } else { FILE_MAP_COPY };
    // SAFETY: a view of the whole section.
    let view = unsafe { MapViewOfFile(section, access, 0, 0, 0) };
    if view.Value.is_null() {
        return Err(format!("MapViewOfFile: {}", std::io::Error::last_os_error()));
    }
    say(&mut out, "its view mapped");
    let pages = (GIB / 4096) as usize;
    for page in (0..pages).step_by(stride) {
        // SAFETY: a page of the view, written.
        unsafe { view.Value.cast::<u8>().add(page * 4096).write_volatile(1) };
    }
    say(&mut out, &format!("{} pages written", pages.div_ceil(stride)));
    // SAFETY: the view.
    unsafe { FlushViewOfFile(view.Value, 0) };
    say(&mut out, "flushed");
    // SAFETY: the view, unmapped once.
    unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: view.Value }) };
    say(&mut out, "unmapped");
    // SAFETY: the section, closed once.
    unsafe { CloseHandle(section) };
    say(&mut out, "its section closed");
    drop(file);
    let _ = std::fs::remove_file(&path);
    say(&mut out, "deleted");
    Ok(())
}

fn main() {
    let dir = std::env::args_os()
        .nth(1)
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("TEMP").map(std::path::PathBuf::from))
        .unwrap_or_else(|| ".".into());
    if let Err(e) = read_while_mapped(&dir) {
        let _ = writeln!(std::io::stdout(), "m377: {e}");
    }
    for writable in [true, false] {
        for stride in [1, 16, 256] {
            if let Err(e) = run(&dir, writable, stride) {
                let _ = writeln!(std::io::stdout(), "m377: {e}");
            }
        }
    }
}
