//! What gives a guest's freed memory back to Windows (PM M371, D151 under WHP): free page
//! reporting gave nothing back of a restored microVM on windows-2025 (`reported_kib` 1 GiB,
//! its private bytes 1.26 GB against 470 MB idle), whose RAM is its snapshot's file mapped
//! copy-on-write (D196), where `platform::discard` decommits and commits again, as it does
//! a cold boot's private RAM. Here 64 MiB of each, written whole, then given back by each
//! way Windows has, and after each its process's private bytes (PrivateUsage, its commit)
//! and private working set (PROCESS_MEMORY_COUNTERS_EX2), what a page reads then, and the
//! protection a written page of the view has (VirtualQuery):
//!
//! - private memory (VirtualAlloc): decommitted and committed again; DiscardVirtualMemory;
//!   MEM_RESET then VirtualUnlock (which takes pages out of the working set);
//! - a file's view, mapped copy-on-write (PAGE_WRITECOPY), its pages written: decommitted;
//!   DiscardVirtualMemory; MEM_RESET then VirtualUnlock; and the view unmapped and mapped
//!   again, its placeholder kept (UnmapViewOfFile2, MEM_PRESERVE_PLACEHOLDER;
//!   MapViewOfFile3, MEM_REPLACE_PLACEHOLDER), as a restore places it.

use std::io::Write as _;
use std::os::windows::io::AsRawHandle as _;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, DiscardVirtualMemory, MEM_COMMIT, MEM_DECOMMIT, MEM_PRESERVE_PLACEHOLDER,
    MEM_RELEASE, MEM_REPLACE_PLACEHOLDER, MEM_RESERVE, MEM_RESERVE_PLACEHOLDER, MEM_RESET, MEMORY_BASIC_INFORMATION,
    MapViewOfFile3, PAGE_NOACCESS, PAGE_READWRITE, PAGE_WRITECOPY, UnmapViewOfFile2, VirtualAlloc, VirtualAlloc2,
    VirtualFree, VirtualQuery, VirtualUnlock,
};
use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX2};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

const LEN: usize = 64 << 20;
const PAGE: usize = 4096;

/// This process's private bytes and private working set, in KiB.
fn memory() -> (u64, u64) {
    let mut c = PROCESS_MEMORY_COUNTERS_EX2 {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32,
        ..Default::default()
    };
    // SAFETY: counters of the size their `cb` says, of this process.
    unsafe { GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut c).cast(), c.cb) };
    ((c.PrivateUsage / 1024) as u64, (c.PrivateWorkingSetSize / 1024) as u64)
}

fn last() -> String {
    std::io::Error::last_os_error().to_string()
}

/// Writes every page of `at..at+LEN` with `byte`.
fn touch(at: *mut u8, byte: u8) {
    for off in (0..LEN).step_by(PAGE) {
        // SAFETY: a committed, writable page of the region.
        unsafe { at.add(off).write_volatile(byte) };
    }
}

/// The protection of the page at `at`.
fn protection(at: *mut u8) -> u32 {
    let mut info = MEMORY_BASIC_INFORMATION::default();
    // SAFETY: a query of this process's own address.
    unsafe { VirtualQuery(at.cast(), &raw mut info, size_of::<MEMORY_BASIC_INFORMATION>()) };
    info.Protect
}

/// Says how giving back the region at `at` by `how` went: what it did to private bytes and
/// the private working set, and what its first and a middle page read then.
fn report(kind: &str, how: &str, before: (u64, u64), result: Result<(), String>, at: *mut u8) {
    let after = memory();
    let read = match &result {
        // SAFETY: a page of the region, committed again or left committed by each way.
        Ok(()) => format!("reads {:#04x} and {:#04x}", unsafe { at.read_volatile() }, unsafe {
            at.add(LEN / 2).read_volatile()
        }),
        Err(_) => String::new(),
    };
    let said = match &result {
        Ok(()) => "ok".to_string(),
        Err(e) => format!("refused: {e}"),
    };
    let _ = writeln!(
        std::io::stdout(),
        "m371 {kind}, {how}: {said}; private bytes {:+} MiB, private working set {:+} MiB; {read}",
        (after.0 as i64 - before.0 as i64) / 1024,
        (after.1 as i64 - before.1 as i64) / 1024
    );
}

fn give_back(at: *mut u8, how: &str) -> Result<(), String> {
    match how {
        "decommitted and committed again" => {
            // SAFETY: the region this probe committed, decommitted and committed in place.
            if unsafe { VirtualFree(at.cast(), LEN, MEM_DECOMMIT) } == 0 {
                return Err(last());
            }
            // SAFETY: as above.
            if unsafe { VirtualAlloc(at.cast(), LEN, MEM_COMMIT, PAGE_READWRITE) }.is_null() {
                return Err(last());
            }
            Ok(())
        }
        "DiscardVirtualMemory" => {
            // SAFETY: the region this probe holds.
            match unsafe { DiscardVirtualMemory(at.cast(), LEN) } {
                0 => Ok(()),
                e => Err(std::io::Error::from_raw_os_error(e as i32).to_string()),
            }
        }
        "MEM_RESET then VirtualUnlock" => {
            // SAFETY: the region this probe holds; MEM_RESET leaves it committed.
            if unsafe { VirtualAlloc(at.cast(), LEN, MEM_RESET, PAGE_NOACCESS) }.is_null() {
                return Err(last());
            }
            // An unlocked range's VirtualUnlock takes its pages out of the working set, and
            // says it was not locked (ERROR_NOT_LOCKED), as its documentation has it.
            // SAFETY: as above.
            unsafe { VirtualUnlock(at.cast(), LEN) };
            Ok(())
        }
        _ => Err("no such way".into()),
    }
}

fn private(how: &str) {
    // SAFETY: a fresh region of this process's.
    let at = unsafe { VirtualAlloc(std::ptr::null(), LEN, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) }.cast::<u8>();
    if at.is_null() {
        let _ = writeln!(std::io::stdout(), "m371 private memory: {}", last());
        return;
    }
    touch(at, 0x5a);
    let before = memory();
    let result = give_back(at, how);
    report("private memory", how, before, result, at);
    // SAFETY: the region allocated above, released once.
    unsafe { VirtualFree(at.cast(), 0, MEM_RELEASE) };
}

/// A view of `section` mapped copy-on-write into a placeholder of its own: its address and
/// the placeholder's.
fn view(section: HANDLE) -> Result<*mut u8, String> {
    // SAFETY: a placeholder reservation of this process's.
    let place = unsafe {
        VirtualAlloc2(
            GetCurrentProcess(),
            std::ptr::null(),
            LEN,
            MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
            PAGE_NOACCESS,
            std::ptr::null_mut(),
            0,
        )
    };
    if place.is_null() {
        return Err(last());
    }
    // SAFETY: the section mapped copy-on-write over the placeholder just made, which it
    // replaces whole.
    let mapped = unsafe {
        MapViewOfFile3(
            section,
            GetCurrentProcess(),
            place,
            0,
            LEN,
            MEM_REPLACE_PLACEHOLDER,
            PAGE_WRITECOPY,
            std::ptr::null_mut(),
            0,
        )
    };
    if mapped.Value.is_null() {
        return Err(last());
    }
    Ok(mapped.Value.cast())
}

fn mapped(file: &std::fs::File, how: &str) {
    // SAFETY: a section of the file this probe made, read-only, copy-on-write views of it.
    let section = unsafe {
        CreateFileMappingW(
            file.as_raw_handle(),
            std::ptr::null(),
            windows_sys::Win32::System::Memory::PAGE_READONLY,
            0,
            0,
            std::ptr::null(),
        )
    };
    if section.is_null() {
        let _ = writeln!(std::io::stdout(), "m371 a copy-on-write view: its section: {}", last());
        return;
    }
    let at = match view(section) {
        Ok(at) => at,
        Err(e) => {
            let _ = writeln!(std::io::stdout(), "m371 a copy-on-write view: {e}");
            // SAFETY: the section made above.
            unsafe { CloseHandle(section) };
            return;
        }
    };
    let untouched = protection(at);
    touch(at, 0x5a);
    let written = protection(at);
    if how == "decommitted and committed again" {
        let _ = writeln!(
            std::io::stdout(),
            "m371 a copy-on-write view: a page's protection {untouched:#x} as mapped (PAGE_WRITECOPY {PAGE_WRITECOPY:#x}), {written:#x} once written (PAGE_READWRITE {PAGE_READWRITE:#x})"
        );
    }
    let before = memory();
    let result = if how == "unmapped and mapped again" {
        // SAFETY: the view mapped above, unmapped to its placeholder, which a view of the
        // section's same range replaces again.
        if unsafe {
            UnmapViewOfFile2(
                GetCurrentProcess(),
                windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS { Value: at.cast() },
                MEM_PRESERVE_PLACEHOLDER,
            )
        } == 0
        {
            Err(last())
        } else {
            // SAFETY: as above.
            let again = unsafe {
                MapViewOfFile3(
                    section,
                    GetCurrentProcess(),
                    at.cast(),
                    0,
                    LEN,
                    MEM_REPLACE_PLACEHOLDER,
                    PAGE_WRITECOPY,
                    std::ptr::null_mut(),
                    0,
                )
            };
            if again.Value.is_null() { Err(last()) } else { Ok(()) }
        }
    } else {
        give_back(at, how)
    };
    report("a copy-on-write view", how, before, result, at);
    // SAFETY: the view and section made above, each let go of once.
    unsafe {
        UnmapViewOfFile2(
            GetCurrentProcess(),
            windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS { Value: at.cast() },
            0,
        );
        CloseHandle(section);
    }
}

fn main() {
    let dir = std::path::PathBuf::from(std::env::var_os("TEMP").unwrap_or_else(|| ".".into()));
    let path = dir.join(format!("m371-{}", std::process::id()));
    // The file a restore maps: its bytes 0xa5, as a snapshot's are its guest's.
    let made = std::fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .and_then(|mut f| {
            f.write_all(&vec![0xa5u8; LEN])?;
            f.sync_all()?;
            Ok(f)
        });
    let file = match made {
        Ok(f) => f,
        Err(e) => {
            let _ = writeln!(std::io::stdout(), "m371 its file, {}: {e}", path.display());
            return;
        }
    };
    for how in ["decommitted and committed again", "DiscardVirtualMemory", "MEM_RESET then VirtualUnlock"] {
        private(how);
    }
    for how in [
        "decommitted and committed again",
        "DiscardVirtualMemory",
        "MEM_RESET then VirtualUnlock",
        "unmapped and mapped again",
    ] {
        mapped(&file, how);
    }
    drop(file);
    let _ = std::fs::remove_file(&path);
}
