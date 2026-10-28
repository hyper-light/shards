//! Whether a WHP partition's RAM, mapped as copy-on-write views of a template's file, can give
//! a view's written pages back while the guest runs (PM M375, D151 and D196 on Windows): M371
//! found Windows gives back a copy-on-write view's written pages only as the view is unmapped,
//! and M372 what views of 2 MiB cost to map. Here a guest of one vCPU whose RAM is 4 MiB, two
//! views of 2 MiB of a file (PAGE_WRITECOPY into a placeholder split for them), mapped at
//! stage 2 whole (WHvMapGpaRange). The guest stores into the second view; then the host gives
//! that view's pages back by unmapping it to its placeholder and mapping it again
//! (UnmapViewOfFile2, MEM_PRESERVE_PLACEHOLDER; MapViewOfFile3, MEM_REPLACE_PLACEHOLDER):
//!
//! - A: with stage 2 left as it is: does the unmap succeed, and does the guest's next load
//!   read the file's bytes, its own store, or fail; does its next store reach the host's new
//!   view;
//! - B: with stage 2 unmapped first and mapped again after (WHvUnmapGpaRange,
//!   WHvMapGpaRange): the same.
//!
//! And for each, what a give-back so costs, as n, p50, p90, p99 and max, the view's pages
//! written by the guest first each time; what it gives back of the private working set; and
//! whether, after the last, every page the guest loads is the file's again.
//!
//! The guest runs an interpreter, as M345's: at each port exit the host names an operation
//! in RBX (0 load, 1 store), its address in RCX and a value in RSI; a load's value is in RAX
//! at the next exit.

// Windows' constants, matched by their own names.
#![allow(non_upper_case_globals)]

use std::io::Write as _;
use std::os::windows::io::AsRawHandle as _;
use std::time::Instant;

use whp_run::{Aligned, Api, bind, check, reg64, segment, summary};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Hypervisor::*;
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER, MEM_RESERVE,
    MEM_RESERVE_PLACEHOLDER, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile3, PAGE_NOACCESS, PAGE_WRITECOPY,
    UnmapViewOfFile2, VirtualAlloc2, VirtualFree,
};
use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX2};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

const MIB: usize = 1 << 20;
/// Guest RAM: two views of 2 MiB.
const RAM: usize = 4 * MIB;
const VIEW: usize = 2 * MIB;
const CODE: u64 = 0x1000;
const STACK: u64 = 0x8000;
const PML4: u64 = 0x10000;
const PDPT: u64 = 0x11000;
/// The second view's first page, and the file's bytes there.
const DATA: u64 = VIEW as u64;
const FILE_BYTE: u8 = 0xA5;
const PORT: u16 = 0xE0;
const LOAD: u64 = 0;
const STORE: u64 = 1;
const ROUNDS: usize = 200;

#[repr(C, align(16))]
#[derive(Clone, Copy, Default)]
struct Value(WHV_REGISTER_VALUE);

fn last(what: &str) -> String {
    format!("{what}: {}", std::io::Error::last_os_error())
}

fn private_working_set_kib() -> i64 {
    let mut c = PROCESS_MEMORY_COUNTERS_EX2 {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32,
        ..Default::default()
    };
    // SAFETY: counters of the size their `cb` says, of this process.
    unsafe { GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut c).cast(), c.cb) };
    (c.PrivateWorkingSetSize / 1024) as i64
}

/// The interpreter: `out`, then a load or a store, then `out` again.
fn guest_code() -> Vec<u8> {
    vec![
        0xEE, // top: out dx, al
        0x83, 0xFB, 0x01, // cmp ebx, 1
        0x74, 0x05, // je store
        0x48, 0x8B, 0x01, // mov rax, [rcx]
        0xEB, 0xF5, // jmp top
        0x48, 0x89, 0x31, // store: mov [rcx], rsi
        0xEB, 0xF0, // jmp top
    ]
}

struct Vm<'a> {
    api: &'a Api,
    p: WHV_PARTITION_HANDLE,
}

impl Vm<'_> {
    fn set(&self, names: &[WHV_REGISTER_NAME], values: &[Value]) -> Result<(), String> {
        // SAFETY: as many 16-aligned values as names.
        check("WHvSetVirtualProcessorRegisters", unsafe {
            (self.api.set_regs)(self.p, 0, names.as_ptr(), names.len() as u32, values.as_ptr().cast())
        })
    }

    fn rax(&self) -> Result<u64, String> {
        let mut v = [Value::default()];
        // SAFETY: one 16-aligned value.
        check("WHvGetVirtualProcessorRegisters", unsafe {
            (self.api.get_regs)(self.p, 0, [WHvX64RegisterRax].as_ptr(), 1, v.as_mut_ptr().cast())
        })?;
        // SAFETY: RAX's value is its Reg64.
        Ok(unsafe { v[0].0.Reg64 })
    }

    /// Runs to the next exit, which must be the interpreter's port's.
    fn run(&self) -> Result<(), String> {
        let mut exit = Aligned(WHV_RUN_VP_EXIT_CONTEXT::default());
        // SAFETY: an exit context of its own size, on vCPU 0.
        check("WHvRunVirtualProcessor", unsafe {
            (self.api.run)(self.p, 0, (&raw mut exit).cast(), size_of::<WHV_RUN_VP_EXIT_CONTEXT>() as u32)
        })?;
        if exit.0.ExitReason != WHvRunVpExitReasonX64IoPortAccess {
            // SAFETY: a memory access exit's union member, where it is one.
            let gpa = unsafe { exit.0.Anonymous.MemoryAccess.Gpa };
            return Err(format!("exit reason {:#x} (a memory access's GPA {gpa:#x})", exit.0.ExitReason));
        }
        Ok(())
    }

    /// One operation of the interpreter, from its `out`: RAX after it.
    fn ask(&self, op: u64, gpa: u64, value: u64) -> Result<u64, String> {
        self.set(
            &[WHvX64RegisterRbx, WHvX64RegisterRcx, WHvX64RegisterRsi, WHvX64RegisterRip],
            &[Value(reg64(op)), Value(reg64(gpa)), Value(reg64(value)), Value(reg64(CODE + 1))],
        )?;
        self.run()?;
        self.rax()
    }
}

/// A partition of one vCPU in long mode, at its interpreter's first `out`, its RAM the two
/// views at `ram`.
fn partition(api: &Api, ram: *mut u8) -> Result<WHV_PARTITION_HANDLE, String> {
    let mut p: WHV_PARTITION_HANDLE = 0;
    // SAFETY: each call on the partition made here.
    unsafe {
        check("WHvCreatePartition", (api.create_partition)(&raw mut p))?;
        let count: u32 = 1;
        check(
            "ProcessorCount",
            (api.set_property)(p, WHvPartitionPropertyCodeProcessorCount, (&raw const count).cast(), 4),
        )?;
        check("WHvSetupPartition", (api.setup_partition)(p))?;
        check(
            "WHvMapGpaRange",
            (api.map)(
                p,
                ram.cast(),
                0,
                RAM as u64,
                WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite | WHvMapGpaRangeFlagExecute,
            ),
        )?;
        check("WHvCreateVirtualProcessor", (api.create_vp)(p, 0, 0))?;
    }
    let vm = Vm { api, p };
    vm.set(
        &[
            WHvX64RegisterCr0,
            WHvX64RegisterCr3,
            WHvX64RegisterCr4,
            WHvX64RegisterEfer,
            WHvX64RegisterCs,
            WHvX64RegisterDs,
            WHvX64RegisterEs,
            WHvX64RegisterSs,
            WHvX64RegisterRip,
            WHvX64RegisterRflags,
            WHvX64RegisterRsp,
            WHvX64RegisterRdx,
        ],
        &[
            Value(reg64(0x8000_0031)),
            Value(reg64(PML4)),
            Value(reg64(0x20)),
            Value(reg64(0x500)),
            Value(segment(0x8, 0xA09B)),
            Value(segment(0x10, 0xC093)),
            Value(segment(0x10, 0xC093)),
            Value(segment(0x10, 0xC093)),
            Value(reg64(CODE)),
            Value(reg64(0x2)),
            Value(reg64(STACK)),
            Value(reg64(u64::from(PORT))),
        ],
    )?;
    vm.run()?;
    Ok(p)
}

/// The file's view `i` of 2 MiB, copy-on-write, into its placeholder at `at`.
fn map_view(section: HANDLE, at: *mut u8, i: usize) -> Result<(), String> {
    // SAFETY: the section's range over the placeholder at `at`, which it replaces whole.
    let view = unsafe {
        MapViewOfFile3(
            section,
            GetCurrentProcess(),
            at.cast(),
            (i * VIEW) as u64,
            VIEW,
            MEM_REPLACE_PLACEHOLDER,
            PAGE_WRITECOPY,
            std::ptr::null_mut(),
            0,
        )
    };
    if view.Value.is_null() {
        return Err(last(&format!("MapViewOfFile3 view {i}")));
    }
    Ok(())
}

fn unmap_view(at: *mut u8) -> Result<(), String> {
    // SAFETY: a view this probe mapped at `at`, unmapped to its placeholder.
    if unsafe {
        UnmapViewOfFile2(
            GetCurrentProcess(),
            MEMORY_MAPPED_VIEW_ADDRESS { Value: at.cast() },
            MEM_PRESERVE_PLACEHOLDER,
        )
    } == 0
    {
        return Err(last("UnmapViewOfFile2"));
    }
    Ok(())
}

fn run() -> Result<(), String> {
    let mut out = std::io::stdout();
    let api = bind()?;
    // The template's file: view 0 the guest's code and tables, view 1 its data, every byte
    // FILE_BYTE.
    let dir = std::path::PathBuf::from(std::env::var_os("TEMP").unwrap_or_else(|| ".".into()));
    let path = dir.join(format!("m375-{}", std::process::id()));
    let mut bytes = vec![0u8; RAM];
    let code = guest_code();
    bytes[CODE as usize..CODE as usize + code.len()].copy_from_slice(&code);
    bytes[PML4 as usize..PML4 as usize + 8].copy_from_slice(&(PDPT | 0x3).to_le_bytes());
    bytes[PDPT as usize..PDPT as usize + 8].copy_from_slice(&0x83u64.to_le_bytes());
    bytes[VIEW..].fill(FILE_BYTE);
    let mut file = std::fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    file.write_all(&bytes).and_then(|()| file.sync_all()).map_err(|e| e.to_string())?;
    // SAFETY: a read-only section of the file, its views copy-on-write.
    let section = unsafe {
        CreateFileMappingW(file.as_raw_handle(), std::ptr::null(), PAGE_WRITECOPY, 0, 0, std::ptr::null())
    };
    if section.is_null() {
        return Err(last("CreateFileMappingW"));
    }
    let filled = u64::from_le_bytes([FILE_BYTE; 8]);
    for with_stage2 in [false, true] {
        let label = if with_stage2 { "B, stage 2 unmapped and mapped again" } else { "A, stage 2 as it is" };
        // SAFETY: a placeholder of this process's, split into two and replaced by views.
        let ram = unsafe {
            VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                RAM,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            )
        }
        .cast::<u8>();
        if ram.is_null() {
            return Err(last("VirtualAlloc2"));
        }
        // SAFETY: the placeholder's first 2 MiB split off as one of its own.
        if unsafe { VirtualFree(ram.cast(), VIEW, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER) } == 0 {
            return Err(last("splitting the placeholder"));
        }
        map_view(section, ram, 0)?;
        // SAFETY: the second placeholder, within the reservation.
        let second = unsafe { ram.add(VIEW) };
        map_view(section, second, 1)?;
        let p = partition(&api, ram)?;
        let vm = Vm { api: &api, p };
        let stored = 0x5A5A_5A5A_5A5A_5A5Au64;
        vm.ask(STORE, DATA, stored)?;
        // SAFETY: the second view's first page, which the guest stored into.
        let host_saw = unsafe { second.cast::<u64>().read_volatile() };
        let before = private_working_set_kib();
        let gave = (|| -> Result<(), String> {
            if with_stage2 {
                // SAFETY: the partition's own range.
                check("WHvUnmapGpaRange", unsafe { (api.unmap)(p, DATA, VIEW as u64) })?;
            }
            unmap_view(second)?;
            map_view(section, second, 1)?;
            if with_stage2 {
                // SAFETY: the view just mapped, at the range just unmapped.
                check("WHvMapGpaRange", unsafe {
                    (api.map)(
                        p,
                        second.cast(),
                        DATA,
                        VIEW as u64,
                        WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite | WHvMapGpaRangeFlagExecute,
                    )
                })?;
            }
            Ok(())
        })();
        let freed = before - private_working_set_kib();
        let _ = writeln!(
            out,
            "m375 {label}: the guest stored {stored:#x}, the host read {host_saw:#x}; the give-back: {}; {freed} KiB of the private working set given back",
            gave.as_ref().map_or_else(|e| format!("refused, {e}"), |()| "done".into())
        );
        if gave.is_ok() {
            // SAFETY: the second view's first page, mapped again.
            let host_now = unsafe { second.cast::<u64>().read_volatile() };
            let loaded = vm.ask(LOAD, DATA, 0);
            let _ = writeln!(
                out,
                "m375 {label}: the host reads {host_now:#x} (the file's {filled:#x}); the guest loads {}",
                loaded.as_ref().map_or_else(|e| format!("nothing: {e}"), |v| format!("{v:#x}"))
            );
            if loaded.is_ok() {
                let again = 0x1234_5678_9ABC_DEF0u64;
                let stored_again = vm.ask(STORE, DATA, again);
                // SAFETY: as above.
                let host_after = unsafe { second.cast::<u64>().read_volatile() };
                let _ = writeln!(
                    out,
                    "m375 {label}: the guest stores {again:#x} ({}); the host's view then reads {host_after:#x}",
                    stored_again.map_or_else(|e| format!("failed: {e}"), |_| "done".into())
                );
            }
        }
        if gave.is_ok() {
            // What a give-back costs, its view's pages written by the guest each time.
            let mut took = Vec::with_capacity(ROUNDS);
            let mut given_kib = Vec::with_capacity(ROUNDS);
            for round in 0..ROUNDS {
                for page in (0..VIEW).step_by(4096) {
                    vm.ask(STORE, DATA + page as u64, round as u64)?;
                }
                let before = private_working_set_kib();
                let t = Instant::now();
                if with_stage2 {
                    // SAFETY: as above.
                    check("WHvUnmapGpaRange", unsafe { (api.unmap)(p, DATA, VIEW as u64) })?;
                }
                unmap_view(second)?;
                map_view(section, second, 1)?;
                if with_stage2 {
                    // SAFETY: as above.
                    check("WHvMapGpaRange", unsafe {
                        (api.map)(
                            p,
                            second.cast(),
                            DATA,
                            VIEW as u64,
                            WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite | WHvMapGpaRangeFlagExecute,
                        )
                    })?;
                }
                took.push(t.elapsed());
                given_kib.push(before - private_working_set_kib());
            }
            given_kib.sort_unstable();
            let _ = writeln!(
                out,
                "m375 {label}: a give-back of a written view of 2 MiB: {} us; the private working set given back each time: min {} KiB, median {} KiB, max {} KiB",
                summary(&mut took),
                given_kib.first().copied().unwrap_or(0),
                given_kib.get(given_kib.len() / 2).copied().unwrap_or(0),
                given_kib.last().copied().unwrap_or(0),
            );
            // After the last, each page the guest loads is the file's again.
            let mut stale = 0;
            for page in (0..VIEW).step_by(4096) {
                if vm.ask(LOAD, DATA + page as u64, 0)? != filled {
                    stale += 1;
                }
            }
            let _ = writeln!(out, "m375 {label}: after the last give-back, {stale} of {} pages load other than the file's bytes", VIEW / 4096);
        }
        // SAFETY: the partition and the views this round made, each let go of once.
        unsafe {
            (api.delete_partition)(p);
            for at in [ram, second] {
                UnmapViewOfFile2(GetCurrentProcess(), MEMORY_MAPPED_VIEW_ADDRESS { Value: at.cast() }, 0);
            }
        }
    }
    // SAFETY: the section made above.
    unsafe { CloseHandle(section) };
    drop(file);
    let _ = std::fs::remove_file(&path);
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        let _ = writeln!(std::io::stdout(), "m375: {e}");
    }
}
