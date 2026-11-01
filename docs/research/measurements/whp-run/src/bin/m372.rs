//! What a restore's RAM costs mapped as views of what free page reporting gives back (PM
//! M372, D151 and D196 on Windows): M371 found Windows gives back a copy-on-write view's
//! written pages only as the whole view is unmapped, and a restore maps its RAM as one view.
//! Mapped as views of 2 MiB, pageblock_order's on x86 and so the least a guest reports,
//! each range it reports is whole views, each unmapped and mapped again from its file. Here,
//! for RAM of 256 MiB, 1 GiB and 4 GiB, its file a template's: a placeholder reserved
//! whole, split into views of 2, 16 and 64 MiB (VirtualFree, MEM_RELEASE |
//! MEM_PRESERVE_PLACEHOLDER), each mapped copy-on-write into its part (MapViewOfFile3,
//! MEM_REPLACE_PLACEHOLDER); the time of the whole, and of each step, as n, p50, p90, p99
//! and max, ten times each, against the one view a restore maps now. Then one 2 MiB view
//! unmapped (UnmapViewOfFile2, MEM_PRESERVE_PLACEHOLDER) and mapped again, as a report
//! would have it, its pages written first: what that costs, and what it gives back of the
//! private working set (PROCESS_MEMORY_COUNTERS_EX2).

use std::io::Write as _;
use std::os::windows::io::AsRawHandle as _;
use std::time::Instant;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER, MEM_RESERVE,
    MEM_RESERVE_PLACEHOLDER, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile3, PAGE_NOACCESS, PAGE_READONLY,
    PAGE_WRITECOPY, UnmapViewOfFile2, VirtualAlloc2, VirtualFree,
};
use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX2};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

const MIB: usize = 1 << 20;

fn last() -> String {
    std::io::Error::last_os_error().to_string()
}

fn private_working_set_kib() -> u64 {
    let mut c = PROCESS_MEMORY_COUNTERS_EX2 {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32,
        ..Default::default()
    };
    // SAFETY: counters of the size their `cb` says, of this process.
    unsafe { GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut c).cast(), c.cb) };
    (c.PrivateWorkingSetSize / 1024) as u64
}

/// The `q`th quantile of `sorted`, nearest rank.
fn quantile(sorted: &[f64], q: f64) -> f64 {
    let n = sorted.len();
    if n == 0 {
        return 0.0;
    }
    let rank = ((q * n as f64).ceil() as usize).clamp(1, n);
    sorted[rank - 1]
}

fn stats(us: &mut [f64]) -> String {
    us.sort_by(f64::total_cmp);
    format!(
        "n {} p50 {:.1} p90 {:.1} p99 {:.1} max {:.1} us",
        us.len(),
        quantile(us, 0.5),
        quantile(us, 0.9),
        quantile(us, 0.99),
        quantile(us, 1.0)
    )
}

/// `len` bytes of `section` mapped copy-on-write as views of `view` bytes into a placeholder
/// split for them: the address, and each view's time to split and map, in µs.
fn map(section: HANDLE, len: usize, view: usize) -> Result<(*mut u8, Vec<f64>), String> {
    // SAFETY: a placeholder reservation of this process's.
    let at = unsafe {
        VirtualAlloc2(
            GetCurrentProcess(),
            std::ptr::null(),
            len,
            MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
            PAGE_NOACCESS,
            std::ptr::null_mut(),
            0,
        )
    }
    .cast::<u8>();
    if at.is_null() {
        return Err(last());
    }
    let mut each = Vec::with_capacity(len / view);
    for off in (0..len).step_by(view) {
        let t = Instant::now();
        // SAFETY: the front of the placeholder's rest split off as a placeholder of its own,
        // where there is more of it.
        if off + view < len && unsafe { VirtualFree(at.add(off).cast(), view, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER) } == 0
        {
            return Err(format!("splitting at {off}: {}", last()));
        }
        // SAFETY: the section's range mapped over the placeholder just split, which it
        // replaces whole.
        let mapped = unsafe {
            MapViewOfFile3(
                section,
                GetCurrentProcess(),
                at.add(off).cast(),
                off as u64,
                view,
                MEM_REPLACE_PLACEHOLDER,
                PAGE_WRITECOPY,
                std::ptr::null_mut(),
                0,
            )
        };
        if mapped.Value.is_null() {
            return Err(format!("mapping at {off}: {}", last()));
        }
        each.push(t.elapsed().as_secs_f64() * 1e6);
    }
    Ok((at, each))
}

fn unmap(at: *mut u8, len: usize, view: usize) {
    for off in (0..len).step_by(view) {
        // SAFETY: each view mapped by `map`, unmapped once.
        unsafe {
            UnmapViewOfFile2(
                GetCurrentProcess(),
                MEMORY_MAPPED_VIEW_ADDRESS { Value: at.add(off).cast() },
                0,
            )
        };
    }
}

fn main() {
    let mut out = std::io::stdout();
    let dir = std::path::PathBuf::from(std::env::var_os("TEMP").unwrap_or_else(|| ".".into()));
    for gib_quarters in [1usize, 4, 16] {
        let len = gib_quarters * 256 * MIB;
        let path = dir.join(format!("m372-{}-{len}", std::process::id()));
        // A template's file: its length, every page holding bytes.
        let made = std::fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .and_then(|mut f| {
                let chunk = vec![0xa5u8; 8 * MIB];
                for _ in 0..len / chunk.len() {
                    f.write_all(&chunk)?;
                }
                f.sync_all()?;
                Ok(f)
            });
        let file = match made {
            Ok(f) => f,
            Err(e) => {
                let _ = writeln!(out, "m372 its file, {}: {e}", path.display());
                return;
            }
        };
        // SAFETY: a read-only section of the file, its views copy-on-write.
        let section = unsafe {
            CreateFileMappingW(file.as_raw_handle(), std::ptr::null(), PAGE_READONLY, 0, 0, std::ptr::null())
        };
        if section.is_null() {
            let _ = writeln!(out, "m372 its section: {}", last());
            return;
        }
        for view in [len, 64 * MIB, 16 * MIB, 2 * MIB] {
            if view > len {
                continue;
            }
            let (mut wholes, mut steps) = (Vec::new(), Vec::new());
            for _ in 0..10 {
                let t = Instant::now();
                match map(section, len, view) {
                    Ok((at, each)) => {
                        wholes.push(t.elapsed().as_secs_f64() * 1e6);
                        steps.extend(each);
                        unmap(at, len, view);
                    }
                    Err(e) => {
                        let _ = writeln!(out, "m372 {} MiB as views of {} MiB: {e}", len / MIB, view / MIB);
                        break;
                    }
                }
            }
            let _ = writeln!(
                out,
                "m372 {} MiB as {} views of {} MiB: the whole {}; each view {}",
                len / MIB,
                len / view,
                view / MIB,
                stats(&mut wholes),
                stats(&mut steps)
            );
        }
        // A reported range given back: one 2 MiB view, written, unmapped and mapped again.
        if let Ok((at, _)) = map(section, len, 2 * MIB) {
            let mut taken = Vec::new();
            let mut freed = 0i64;
            for i in 0..64.min(len / (2 * MIB)) {
                let off = i * 2 * MIB;
                for p in (0..2 * MIB).step_by(4096) {
                    // SAFETY: a page of a view mapped copy-on-write, writable.
                    unsafe { at.add(off + p).write_volatile(0x5a) };
                }
                let before = private_working_set_kib();
                let t = Instant::now();
                // SAFETY: the view at `off`, unmapped to its placeholder and mapped again.
                let ok = unsafe {
                    UnmapViewOfFile2(
                        GetCurrentProcess(),
                        MEMORY_MAPPED_VIEW_ADDRESS { Value: at.add(off).cast() },
                        MEM_PRESERVE_PLACEHOLDER,
                    )
                } != 0
                    && !unsafe {
                        MapViewOfFile3(
                            section,
                            GetCurrentProcess(),
                            at.add(off).cast(),
                            off as u64,
                            2 * MIB,
                            MEM_REPLACE_PLACEHOLDER,
                            PAGE_WRITECOPY,
                            std::ptr::null_mut(),
                            0,
                        )
                    }
                    .Value
                    .is_null();
                taken.push(t.elapsed().as_secs_f64() * 1e6);
                if !ok {
                    let _ = writeln!(out, "m372 giving a view back: {}", last());
                    break;
                }
                freed += before as i64 - private_working_set_kib() as i64;
            }
            let _ = writeln!(
                out,
                "m372 {} MiB: a 2 MiB view written, unmapped and mapped again: {}; {} KiB given back of {} KiB written",
                len / MIB,
                stats(&mut taken),
                freed,
                taken.len() * 2048
            );
            unmap(at, len, 2 * MIB);
        }
        // SAFETY: the section made above, closed once.
        unsafe { CloseHandle(section) };
        drop(file);
        let _ = std::fs::remove_file(&path);
    }
}
