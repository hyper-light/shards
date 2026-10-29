//! A DAX window's mechanism on WHP (PM M341, D189), before the window is written for Windows:
//! each range a view of a section placed into a placeholder reservation (MapViewOfFile3,
//! MEM_REPLACE_PLACEHOLDER) and mapped at stage 2 (WHvMapGpaRange), and replaced by unmapping
//! it at stage 2, freeing the view back to its placeholder (UnmapViewOfFile2,
//! MEM_PRESERVE_PLACEHOLDER), and placing and mapping another.
//!
//! The sections are pagefile-backed, as the DAX objects slates hands a VM process are (its
//! A-175), and each has a second, ordinary view: the server's own, through which it writes
//! the file. The guest is one vCPU in long mode running an interpreter: at each port exit
//! the host names an operation in its registers (a load, a store, or a load of every page of
//! a range), and the guest's next port exit carries the load's result in RAX. So it asks:
//! - does the guest see a host store into the section, through the placed view and through
//!   the server's view;
//! - does the host see the guest's store, through both;
//! - with the view swapped in the host alone, no stage-2 remap (M193's rule on HVF: a range
//!   replaced in the host alone keeps its old pages at stage 2), what the guest reads, and
//!   where its store lands;
//! - what each step of a replace done right costs, and what the guest's first access to a
//!   range just mapped costs beside its next one and beside a load from its RAM;
//! - what the same replace costs with the stage-2 mapping left standing, views swapped
//!   under it, and with the range populated at stage 2 after each swap (WHvAdviseGpaRange);
//! - whether a window may be mapped at stage 2 over its placeholders, before any view;
//! - whether memory only reserved may be mapped at stage 2 and committed as its guest first
//!   touches it, a block at a time, as a DAX window's head is to be (PM M413, D197's slice
//!   4): the guest's load there exiting until the host commits its block, then reading it
//!   with no further call; and what a block decommitted again does.

// Windows' constants, matched by their own names.
#![allow(non_upper_case_globals)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use whp_run::{Aligned, Api, bind, check, reg64, segment, summary};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Hypervisor::*;
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEM_COMMIT, MEM_DECOMMIT, MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE,
    MEM_REPLACE_PLACEHOLDER, MEM_RESERVE, MEM_RESERVE_PLACEHOLDER, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    MapViewOfFile3, PAGE_NOACCESS, PAGE_READWRITE, UnmapViewOfFile, UnmapViewOfFile2, VirtualAlloc,
    VirtualAlloc2, VirtualFree,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// The guest's memory: 2 MiB from GPA 0, its code and page tables in it.
const RAM: usize = 2 << 20;
const CODE: u64 = 0x1000;
const PML4: u64 = 0x10000;
const PDPT: u64 = 0x11000;
/// A range of the window: Shards' FUSE_DAX_SZ (window.rs `RANGE`).
const RANGE: usize = 2 << 20;
/// The window's ranges: the one replaced, and one left in place beside it.
const RANGES: usize = 2;
/// Where the window lies in the guest: 1 GiB, a 1 GiB page of the guest's own tables.
const WINDOW: u64 = 1 << 30;
const PAGE: usize = 4096;
/// The port the guest's interpreter waits on, and its operations (RBX).
const PORT: u16 = 0xE0;
const LOAD: u64 = 0;
const STORE: u64 = 1;
const TOUCH: u64 = 3;
/// What a load the hypervisor could not complete answers: no section's bytes.
const POISON: u64 = 0xDEAD_DEAD_DEAD_DEAD;
/// The replaces timed.
const ROUNDS: usize = 1000;
/// How long the probe may run before its vCPU's run is canceled, and the most memory
/// exits one operation may take (more than a range's pages).
const WATCHDOG: Duration = Duration::from_secs(600);
const MOST_FAULTS: u32 = 2 * (RANGE / PAGE) as u32;
/// Where in a range each question writes.
const HOST_PLACED: usize = 0x100;
const HOST_SERVER: usize = 0x200;
const GUEST_STORE: usize = 0x300;
const AFTER_RELEASE: usize = 0x400;
const AFTER_SWAP: usize = 0x500;
const STAMP: usize = 0x800;
const ECHO: usize = 0x900;

/// The guest's code, hand-assembled (x86-64): `out dx, al` and wait for the host's word;
/// then RBX names what to do: LOAD `mov rax, [rcx]`; STORE `mov [rcx], rsi`; TOUCH a load
/// of each page of RSI bytes from RCX, summed into RAX; and back to the `out`. Every load
/// and store that may exit to the VMM is three bytes long, which the VMM steps over.
fn guest_code() -> Vec<u8> {
    let mut c = vec![0xEE]; // out dx, al
    c.extend([0x83, 0xFB, STORE as u8]); // cmp ebx, STORE
    c.extend([0x74, 0]); // je store
    let to_store = c.len() - 1;
    c.extend([0x83, 0xFB, TOUCH as u8]); // cmp ebx, TOUCH
    c.extend([0x74, 0]); // je touch
    let to_touch = c.len() - 1;
    c.extend([0x48, 0x8B, 0x01]); // mov rax, [rcx]
    c.extend([0xEB, 0]); // jmp 0
    let back_load = c.len() - 1;
    let store = c.len();
    c.extend([0x48, 0x89, 0x31]); // mov [rcx], rsi
    c.extend([0xEB, 0]); // jmp 0
    let back_store = c.len() - 1;
    let touch = c.len();
    c.extend([0x31, 0xC0]); // xor eax, eax
    c.extend([0x48, 0x89, 0xCF]); // mov rdi, rcx
    c.extend([0x4C, 0x8D, 0x04, 0x31]); // lea r8, [rcx + rsi]
    let each = c.len();
    c.extend([0x48, 0x03, 0x07]); // add rax, [rdi]
    c.extend([0x48, 0x81, 0xC7]); // add rdi, PAGE
    c.extend((PAGE as u32).to_le_bytes());
    c.extend([0x4C, 0x39, 0xC7]); // cmp rdi, r8
    c.extend([0x72, 0]); // jb each
    let to_each = c.len() - 1;
    c.extend([0xEB, 0]); // jmp 0
    let back_touch = c.len() - 1;
    for (at, target) in [
        (to_store, store),
        (to_touch, touch),
        (back_load, 0),
        (back_store, 0),
        (to_each, each),
        (back_touch, 0),
    ] {
        c[at] = (target as isize - (at as isize + 1)) as i8 as u8;
    }
    c
}

/// What the guest answered an operation with.
struct Answer {
    rax: u64,
    /// The memory exits the operation took, and the first one's GPA.
    faults: u32,
    fault_gpa: Option<u64>,
    /// From the host's word to the guest's answer.
    took: Duration,
}

struct Guest<'a> {
    api: &'a Api,
    partition: WHV_PARTITION_HANDLE,
    /// Where the vCPU resumes: past its `out`.
    rip: u64,
}

impl Guest<'_> {
    fn run(&self) -> Result<WHV_RUN_VP_EXIT_CONTEXT, String> {
        let mut exit = Aligned(WHV_RUN_VP_EXIT_CONTEXT::default());
        // SAFETY: an exit context of its own size, on the partition's one vCPU.
        let hr = unsafe {
            (self.api.run)(
                self.partition,
                0,
                (&raw mut exit).cast(),
                std::mem::size_of::<WHV_RUN_VP_EXIT_CONTEXT>() as u32,
            )
        };
        check("WHvRunVirtualProcessor", hr)?;
        Ok(exit.0)
    }

    fn set(&self, names: &[WHV_REGISTER_NAME], values: &[WHV_REGISTER_VALUE]) -> Result<(), String> {
        // SAFETY: as many values as names, 16-aligned as the caller's `Aligned` holds them.
        check("registers", unsafe {
            (self.api.set_regs)(
                self.partition,
                0,
                names.as_ptr(),
                names.len() as u32,
                values.as_ptr(),
            )
        })
    }

    /// Runs the vCPU from its first instruction to its first `out`.
    fn start(&mut self) -> Result<(), String> {
        let exit = self.run()?;
        if exit.ExitReason != WHvRunVpExitReasonX64IoPortAccess {
            return Err(format!(
                "first exit {:#x} at rip {:#x}",
                exit.ExitReason, exit.VpContext.Rip
            ));
        }
        self.rip = exit.VpContext.Rip + 1;
        Ok(())
    }

    fn ask(&mut self, op: u64, gpa: u64, value: u64) -> Result<Answer, String> {
        let started = Instant::now();
        let values = Aligned([reg64(self.rip), reg64(op), reg64(gpa), reg64(value)]);
        self.set(
            &[
                WHvX64RegisterRip,
                WHvX64RegisterRbx,
                WHvX64RegisterRcx,
                WHvX64RegisterRsi,
            ],
            &values.0,
        )?;
        let mut faults = 0u32;
        let mut fault_gpa = None;
        loop {
            let exit = self.run()?;
            match exit.ExitReason {
                // SAFETY (each union read): the member the exit reason names.
                r if r == WHvRunVpExitReasonX64IoPortAccess
                    && unsafe { exit.Anonymous.IoPortAccess.PortNumber } == PORT =>
                {
                    self.rip = exit.VpContext.Rip + 1;
                    return Ok(Answer {
                        rax: unsafe { exit.Anonymous.IoPortAccess.Rax },
                        faults,
                        fault_gpa,
                        took: started.elapsed(),
                    });
                }
                r if r == WHvRunVpExitReasonMemoryAccess => {
                    faults += 1;
                    fault_gpa.get_or_insert(unsafe { exit.Anonymous.MemoryAccess.Gpa });
                    if faults > MOST_FAULTS {
                        return Err(format!("past {MOST_FAULTS} memory exits"));
                    }
                    // Stepped over; a load answers POISON.
                    let next = Aligned([reg64(exit.VpContext.Rip + 3), reg64(POISON)]);
                    let names = [WHvX64RegisterRip, WHvX64RegisterRax];
                    let count = if op == LOAD { 2 } else { 1 };
                    self.set(&names[..count], &next.0[..count])?;
                }
                other => return Err(format!("exit reason {other:#x} at rip {:#x}", exit.VpContext.Rip)),
            }
        }
    }

    fn load(&mut self, gpa: u64) -> Result<Answer, String> {
        self.ask(LOAD, gpa, 0)
    }
}

/// A pagefile-backed section of a range, and the server's own view of it.
struct Section {
    name: char,
    handle: HANDLE,
    server: *mut u8,
    fill: u64,
}

impl Section {
    fn new(name: char, byte: u8) -> Result<Self, String> {
        // SAFETY: a pagefile-backed section of RANGE bytes, and a view of all of it.
        unsafe {
            let handle = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                RANGE as u32,
                std::ptr::null(),
            );
            if handle.is_null() {
                return Err(format!("CreateFileMappingW: {}", std::io::Error::last_os_error()));
            }
            let server = MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, RANGE)
                .Value
                .cast::<u8>();
            if server.is_null() {
                return Err(format!("MapViewOfFile: {}", std::io::Error::last_os_error()));
            }
            std::ptr::write_bytes(server, byte, RANGE);
            Ok(Section {
                name,
                handle,
                server,
                fill: u64::from_ne_bytes([byte; 8]),
            })
        }
    }

    fn write(&self, offset: usize, value: u64) {
        // SAFETY: an 8-aligned offset within the view's RANGE bytes.
        unsafe { std::ptr::write_volatile(self.server.add(offset).cast::<u64>(), value) }
    }

    fn read(&self, offset: usize) -> u64 {
        // SAFETY: as `write`.
        unsafe { std::ptr::read_volatile(self.server.add(offset).cast::<u64>()) }
    }
}

impl Drop for Section {
    fn drop(&mut self) {
        // SAFETY: the view and the handle this section made.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.server.cast(),
            });
            CloseHandle(self.handle);
        }
    }
}

/// The window: a placeholder reservation of RANGES ranges, split into one a range.
struct Window<'a> {
    api: &'a Api,
    partition: WHV_PARTITION_HANDLE,
    base: *mut u8,
}

fn last_error(what: &str) -> String {
    format!("{what}: {}", std::io::Error::last_os_error())
}

impl Window<'_> {
    fn host(&self, range: usize) -> *mut u8 {
        // SAFETY: within the reservation.
        unsafe { self.base.add(range * RANGE) }
    }

    fn gpa(range: usize) -> u64 {
        WINDOW + (range * RANGE) as u64
    }

    /// Places a view of `section` into range `range`'s placeholder.
    fn place(&self, range: usize, section: &Section) -> Result<(), String> {
        let at = self.host(range);
        // SAFETY: a placeholder of exactly RANGE bytes at `at`, replaced by a view.
        let view = unsafe {
            MapViewOfFile3(
                section.handle,
                GetCurrentProcess(),
                at.cast(),
                0,
                RANGE,
                MEM_REPLACE_PLACEHOLDER,
                PAGE_READWRITE,
                std::ptr::null_mut(),
                0,
            )
        };
        if view.Value != at.cast::<c_void>() {
            return Err(last_error("MapViewOfFile3"));
        }
        Ok(())
    }

    /// Frees range `range`'s view back to a placeholder.
    fn release(&self, range: usize) -> Result<(), String> {
        let at = MEMORY_MAPPED_VIEW_ADDRESS {
            Value: self.host(range).cast(),
        };
        // SAFETY: a view this window placed.
        if unsafe { UnmapViewOfFile2(GetCurrentProcess(), at, MEM_PRESERVE_PLACEHOLDER) } == 0 {
            return Err(last_error("UnmapViewOfFile2"));
        }
        Ok(())
    }

    fn map(&self, range: usize) -> Result<(), String> {
        // SAFETY: a placed view of RANGE bytes, kept until it is unmapped here.
        check("WHvMapGpaRange", unsafe {
            (self.api.map)(
                self.partition,
                self.host(range).cast(),
                Self::gpa(range),
                RANGE as u64,
                WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite | WHvMapGpaRangeFlagExecute,
            )
        })
    }

    /// Asks the hypervisor to populate range `range` at stage 2 now, for writes, rather
    /// than page by page as the guest touches it.
    fn populate(&self, range: usize) -> Result<(), String> {
        let Some(advise) = self.api.advise else {
            return Err("WHvAdviseGpaRange: not offered".into());
        };
        let entry = WHV_MEMORY_RANGE_ENTRY {
            GuestAddress: Self::gpa(range),
            SizeInBytes: RANGE as u64,
        };
        let advice = WHV_ADVISE_GPA_RANGE_POPULATE {
            AccessType: WHvMemoryAccessWrite,
            ..Default::default()
        };
        // SAFETY: one range entry and an advice buffer of its own size.
        check("WHvAdviseGpaRange", unsafe {
            advise(
                self.partition,
                &raw const entry,
                1,
                WHvAdviseGpaRangeCodePopulate,
                (&raw const advice).cast(),
                std::mem::size_of_val(&advice) as u32,
            )
        })
    }

    fn unmap(&self, range: usize) -> Result<(), String> {
        // SAFETY: a GPA range of the partition.
        check("WHvUnmapGpaRange", unsafe {
            (self.api.unmap)(self.partition, Self::gpa(range), RANGE as u64)
        })
    }
}

/// Which section's bytes a value is: its fill, a value written into it, or neither.
fn whose(value: u64, sections: &[&Section], written: u64) -> String {
    if value == POISON {
        return "no section's (the access exited)".into();
    }
    let mut names: Vec<String> = sections
        .iter()
        .filter(|s| s.fill == value)
        .map(|s| format!("{}'s fill", s.name))
        .collect();
    if value == written {
        names.push("the value written".into());
    }
    if names.is_empty() {
        format!("{value:#018x}, unknown")
    } else {
        names.join(", ")
    }
}

fn print_load(label: &str, a: &Answer, sections: &[&Section], written: u64) {
    println!(
        "{label}: guest read {:#018x} ({}) faults={} fault_gpa={:x?} round_trip_us={:.1}",
        a.rax,
        whose(a.rax, sections, written),
        a.faults,
        a.fault_gpa,
        a.took.as_secs_f64() * 1e6
    );
}

fn timed(f: impl FnOnce() -> Result<(), String>) -> Result<Duration, String> {
    let t = Instant::now();
    f()?;
    Ok(t.elapsed())
}

/// Questions 1 and 2: coherence each way, through the placed view and the server's.
fn coherence(guest: &mut Guest<'_>, window: &Window<'_>, a: &Section, c: &Section) -> Result<(), String> {
    let gpa = Window::gpa(0);
    let first = guest.load(gpa)?;
    print_load("fill", &first, &[a, c], 0);
    let placed = window.host(0);
    let value = 0x1111_2222_3333_4444u64;
    // SAFETY: an 8-aligned offset within the placed view.
    unsafe { std::ptr::write_volatile(placed.add(HOST_PLACED).cast::<u64>(), value) };
    let seen = guest.load(gpa + HOST_PLACED as u64)?;
    print_load("host store through the placed view", &seen, &[a, c], value);
    let value = 0x5555_6666_7777_8888u64;
    a.write(HOST_SERVER, value);
    let seen = guest.load(gpa + HOST_SERVER as u64)?;
    print_load("host store through the server's view", &seen, &[a, c], value);
    let value = 0x9999_AAAA_BBBB_CCCCu64;
    let stored = guest.ask(STORE, gpa + GUEST_STORE as u64, value)?;
    // SAFETY: as above.
    let in_placed = unsafe { std::ptr::read_volatile(placed.add(GUEST_STORE).cast::<u64>()) };
    let in_server = a.read(GUEST_STORE);
    println!(
        "guest store {value:#018x}: placed view reads {in_placed:#018x} (match={}), server's view reads {in_server:#018x} (match={}), faults={} round_trip_us={:.1}",
        in_placed == value,
        in_server == value,
        stored.faults,
        stored.took.as_secs_f64() * 1e6
    );
    Ok(())
}

/// Question 3: the view swapped in the host alone, its stage-2 mapping left as it was.
fn swap_without_remap(
    guest: &mut Guest<'_>,
    window: &Window<'_>,
    a: &Section,
    b: &Section,
) -> Result<(), String> {
    let gpa = Window::gpa(0);
    let released = window.release(0);
    println!("view freed to its placeholder, stage 2 left mapped: {released:?}");
    released?;
    let seen = guest.load(gpa + HOST_PLACED as u64)?;
    print_load(
        "after the free, a byte the host stored before it",
        &seen,
        &[a, b],
        0x1111_2222_3333_4444,
    );
    let value = 0x0123_4567_89AB_CDEFu64;
    a.write(AFTER_RELEASE, value);
    let seen = guest.load(gpa + AFTER_RELEASE as u64)?;
    print_load(
        "after the free, the server's store into the old section",
        &seen,
        &[a, b],
        value,
    );
    let placed = window.place(0, b);
    println!("another section's view placed, stage 2 left mapped: {placed:?}");
    placed?;
    let seen = guest.load(gpa)?;
    print_load("after the swap", &seen, &[a, b], 0);
    let value = 0xFEDC_BA98_7654_3210u64;
    b.write(AFTER_SWAP, value);
    let seen = guest.load(gpa + AFTER_SWAP as u64)?;
    print_load(
        "after the swap, the server's store into the new section",
        &seen,
        &[a, b],
        value,
    );
    let value = 0x0F0F_0F0F_F0F0_F0F0u64;
    let stored = guest.ask(STORE, gpa + (AFTER_SWAP + 8) as u64, value)?;
    let (old, new) = (a.read(AFTER_SWAP + 8), b.read(AFTER_SWAP + 8));
    println!(
        "after the swap, guest store {value:#018x}: old section {} holds it={}, new section {} holds it={}, faults={}",
        a.name,
        old == value,
        b.name,
        new == value,
        stored.faults
    );
    let remapped = window.unmap(0).and_then(|()| window.map(0));
    println!("then unmapped and mapped again at stage 2: {remapped:?}");
    remapped?;
    let seen = guest.load(gpa)?;
    print_load("after the stage-2 remap", &seen, &[a, b], 0);
    Ok(())
}

/// How a round replaces range 0's view.
#[derive(Clone, Copy, PartialEq)]
enum Way {
    /// Unmapped at stage 2, freed, placed and mapped again: M193's rule.
    Remap,
    /// As `Remap`, then the range populated at stage 2 (WHvAdviseGpaRange) before the
    /// guest touches it.
    RemapPopulated,
    /// Freed and placed under a stage-2 mapping left standing.
    Standing,
    /// As `Standing`, then the range populated at stage 2 (WHvAdviseGpaRange) before the
    /// guest touches it.
    Populated,
}

impl Way {
    fn label(self) -> &'static str {
        match self {
            Way::Remap => "remap",
            Way::RemapPopulated => "remap-populated",
            Way::Standing => "standing",
            Way::Populated => "populated",
        }
    }
}

/// Questions 4 and 5: a replace made `way`, ROUNDS times, alternating between two
/// sections, each step timed, coherence each way checked every round: the server's store
/// made before the replace read by the guest after it, and the guest's store read by the
/// server's view.
fn replaces(
    guest: &mut Guest<'_>,
    window: &Window<'_>,
    sections: [&Section; 2],
    ram_gpa: u64,
    way: Way,
) -> Result<(), String> {
    let (mut unmap, mut release, mut place, mut map, mut populate, mut whole) =
        (vec![], vec![], vec![], vec![], vec![], vec![]);
    let (mut first, mut next, mut ram, mut touch_first, mut touch_next) =
        (vec![], vec![], vec![], vec![], vec![]);
    let (mut stale, mut lost, mut faults) = (0usize, 0usize, 0u32);
    let gpa = Window::gpa(0);
    for round in 0..ROUNDS {
        let section = sections[(round + 1) % 2];
        let stamp = 0x5A00_0000_0000_0000u64 | round as u64;
        section.write(STAMP, stamp);
        let t = Instant::now();
        let remap = matches!(way, Way::Remap | Way::RemapPopulated);
        if remap {
            unmap.push(timed(|| window.unmap(0))?);
        }
        release.push(timed(|| window.release(0))?);
        place.push(timed(|| window.place(0, section))?);
        if remap {
            map.push(timed(|| window.map(0))?);
        }
        if matches!(way, Way::RemapPopulated | Way::Populated) {
            populate.push(timed(|| window.populate(0))?);
        }
        whole.push(t.elapsed());
        let seen = guest.load(gpa + STAMP as u64)?;
        stale += usize::from(seen.rax != stamp);
        faults += seen.faults;
        first.push(seen.took);
        let again = guest.load(gpa + STAMP as u64)?;
        next.push(again.took);
        ram.push(guest.load(ram_gpa)?.took);
        let touched = guest.ask(TOUCH, gpa, RANGE as u64)?;
        faults += touched.faults;
        touch_first.push(touched.took);
        touch_next.push(guest.ask(TOUCH, gpa, RANGE as u64)?.took);
        let echo = stamp ^ u64::MAX;
        let stored = guest.ask(STORE, gpa + ECHO as u64, echo)?;
        faults += stored.faults;
        lost += usize::from(section.read(ECHO) != echo);
    }
    let label = way.label();
    println!(
        "{label}: rounds={ROUNDS} guest_read_stale={stale} guest_store_lost={lost} memory_exits={faults}"
    );
    for (what, v) in [
        ("stage-2 unmap", &mut unmap),
        ("view freed to placeholder", &mut release),
        ("view placed", &mut place),
        ("stage-2 map", &mut map),
        ("stage-2 populate", &mut populate),
        ("whole replace", &mut whole),
        ("guest load, first after the replace", &mut first),
        ("guest load, next", &mut next),
        ("guest load from RAM", &mut ram),
        (
            "guest touch of every page, first after the replace",
            &mut touch_first,
        ),
        ("guest touch of every page, next", &mut touch_next),
    ] {
        if !v.is_empty() {
            println!("{label} {what} (us): {}", summary(v));
        }
    }
    Ok(())
}

/// Question 6: whether a window may be mapped at stage 2 before any view is placed, over
/// its placeholders, so that a replace never calls the hypervisor: range 0 unmapped and
/// freed, mapped over its placeholder, the guest's load there, then a view placed.
fn map_over_placeholder(
    guest: &mut Guest<'_>,
    window: &Window<'_>,
    a: &Section,
    b: &Section,
) -> Result<(), String> {
    window.unmap(0)?;
    window.release(0)?;
    let mapped = window.map(0);
    println!("stage-2 map over a placeholder: {mapped:?}");
    let gpa = Window::gpa(0);
    if mapped.is_ok() {
        let seen = guest.load(gpa)?;
        print_load("over the placeholder", &seen, &[a, b], 0);
    }
    window.place(0, a)?;
    let seen = guest.load(gpa)?;
    print_load("a view placed after", &seen, &[a, b], 0);
    if seen.faults > 0 {
        let remapped = window.map(0);
        println!("stage-2 map once the view is placed: {remapped:?}");
        remapped?;
        let seen = guest.load(gpa)?;
        print_load("after that map", &seen, &[a, b], 0);
    }
    Ok(())
}

/// Question 7 (PM M413): memory only reserved (VirtualAlloc, MEM_RESERVE), mapped at stage
/// 2 at once, its blocks committed as the guest first touches them, as a DAX window's head
/// is to be: what the guest's loads meet before and after a block is committed, what its
/// store there makes, a block decommitted again, and what committing a block costs.
fn commit_as_touched(guest: &mut Guest<'_>, api: &Api, protect: u32, at: u64) -> Result<(), String> {
    const BLOCK: usize = 64 << 10;
    println!("reserved as {protect:#x}:");
    // SAFETY: a fresh reservation of a range of this process's, released below.
    let host = unsafe { VirtualAlloc(std::ptr::null(), RANGE, MEM_RESERVE, protect) }.cast::<u8>();
    if host.is_null() {
        return Err(last_error("VirtualAlloc reserve"));
    }
    const AT: u64 = 0;
    let at_gpa = at + AT;
    // SAFETY: the reservation above, kept until it is unmapped below.
    let mapped = check("WHvMapGpaRange over reserved memory", unsafe {
        (api.map)(
            guest.partition,
            host.cast(),
            at_gpa,
            RANGE as u64,
            WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite | WHvMapGpaRangeFlagExecute,
        )
    });
    println!("stage-2 map over reserved memory: {mapped:?}");
    if mapped.is_ok() {
        let seen = guest.load(at_gpa)?;
        print_load("reserved, uncommitted", &seen, &[], 0);
        let mut commits = Vec::new();
        for block in 0..16 {
            let t = Instant::now();
            // SAFETY: a block of the reservation above, committed.
            let made = unsafe { VirtualAlloc(host.add(block * BLOCK).cast(), BLOCK, MEM_COMMIT, PAGE_READWRITE) };
            commits.push(t.elapsed());
            if made.is_null() {
                return Err(last_error("VirtualAlloc commit"));
            }
        }
        println!("commit a 64 KiB block: {}", summary(&mut commits));
        // SAFETY: an 8-aligned offset within the committed blocks.
        unsafe { std::ptr::write_volatile(host.cast::<u64>(), 0x5151_5151_5151_5151) };
        let seen = guest.load(at_gpa)?;
        print_load("committed, the host's store", &seen, &[], 0x5151_5151_5151_5151);
        let seen = guest.load(at_gpa + 16 * BLOCK as u64)?;
        print_load("the block after them, uncommitted", &seen, &[], 0);
        let stored = guest.ask(STORE, at_gpa + 8, 0x7272_7272_7272_7272)?;
        // SAFETY: as above.
        let landed = unsafe { std::ptr::read_volatile(host.add(8).cast::<u64>()) };
        println!("the guest's store: faults={} the host reads {landed:#x}", stored.faults);
        // SAFETY: the first block, decommitted.
        let freed = unsafe { VirtualFree(host.cast(), BLOCK, MEM_DECOMMIT) };
        println!("first block decommitted: {}", if freed == 0 { last_error("VirtualFree") } else { "ok".into() });
        let seen = guest.load(at_gpa)?;
        print_load("decommitted", &seen, &[], 0);
        // SAFETY: as above, committed again.
        let made = unsafe { VirtualAlloc(host.cast(), BLOCK, MEM_COMMIT, PAGE_READWRITE) };
        println!("committed again: {}", if made.is_null() { last_error("VirtualAlloc") } else { "ok".into() });
        let seen = guest.load(at_gpa)?;
        print_load("committed again", &seen, &[], 0);
        // SAFETY: the mapping made above.
        let _ = unsafe { (api.unmap)(guest.partition, at_gpa, RANGE as u64) };
    }
    // SAFETY: the reservation above, which nothing maps any more.
    unsafe { VirtualFree(host.cast(), 0, MEM_RELEASE) };
    Ok(())
}

fn partition(api: &Api) -> Result<(WHV_PARTITION_HANDLE, *mut u8), String> {
    let mut partition: WHV_PARTITION_HANDLE = 0;
    // SAFETY: each call on the partition this function makes, and memory it allocates.
    unsafe {
        check("WHvCreatePartition", (api.create_partition)(&raw mut partition))?;
        let count: u32 = 1;
        check(
            "ProcessorCount",
            (api.set_property)(
                partition,
                WHvPartitionPropertyCodeProcessorCount,
                (&raw const count).cast(),
                4,
            ),
        )?;
        check("WHvSetupPartition", (api.setup_partition)(partition))?;
        let ram = VirtualAlloc(std::ptr::null(), RAM, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE).cast::<u8>();
        if ram.is_null() {
            return Err(last_error("VirtualAlloc"));
        }
        let mem = std::slice::from_raw_parts_mut(ram, RAM);
        let code = guest_code();
        mem[CODE as usize..CODE as usize + code.len()].copy_from_slice(&code);
        // PML4[0] -> PDPT; PDPT[0] and [1]: 1 GiB pages at 0 (the RAM) and 1 GiB (the window).
        mem[PML4 as usize..PML4 as usize + 8].copy_from_slice(&(PDPT | 0x3).to_le_bytes());
        mem[PDPT as usize..PDPT as usize + 8].copy_from_slice(&0x83u64.to_le_bytes());
        mem[PDPT as usize + 8..PDPT as usize + 16].copy_from_slice(&(WINDOW | 0x83).to_le_bytes());
        check(
            "WHvMapGpaRange RAM",
            (api.map)(
                partition,
                ram.cast(),
                0,
                RAM as u64,
                WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite | WHvMapGpaRangeFlagExecute,
            ),
        )?;
        check("WHvCreateVirtualProcessor", (api.create_vp)(partition, 0, 0))?;
        let names = [
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
        ];
        let values = Aligned([
            reg64(0x8000_0031),
            reg64(PML4),
            reg64(0x20),
            reg64(0x500),
            segment(0x8, 0xA09B),
            segment(0x10, 0xC093),
            segment(0x10, 0xC093),
            segment(0x10, 0xC093),
            reg64(CODE),
            reg64(0x2),
            reg64(0x8000),
            reg64(u64::from(PORT)),
        ]);
        check(
            "registers",
            (api.set_regs)(
                partition,
                0,
                names.as_ptr(),
                names.len() as u32,
                values.0.as_ptr(),
            ),
        )?;
        Ok((partition, ram))
    }
}

/// The window reserved and split, A placed in range 0 and C in range 1, both mapped.
fn window<'a>(
    api: &'a Api,
    partition: WHV_PARTITION_HANDLE,
    a: &Section,
    c: &Section,
) -> Result<Window<'a>, String> {
    // SAFETY: a placeholder reservation of this process, split at its range boundary.
    let base = unsafe {
        VirtualAlloc2(
            GetCurrentProcess(),
            std::ptr::null(),
            RANGE * RANGES,
            MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
            PAGE_NOACCESS,
            std::ptr::null_mut(),
            0,
        )
    }
    .cast::<u8>();
    if base.is_null() {
        return Err(last_error("VirtualAlloc2"));
    }
    let window = Window { api, partition, base };
    for range in 0..RANGES - 1 {
        // SAFETY: the first RANGE bytes of the placeholder at this range, split from the rest.
        if unsafe {
            VirtualFree(
                window.host(range).cast(),
                RANGE,
                MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
            )
        } == 0
        {
            return Err(last_error("VirtualFree split"));
        }
    }
    let t = Instant::now();
    window.place(0, a)?;
    let placed = t.elapsed();
    let t = Instant::now();
    window.map(0)?;
    let mapped = t.elapsed();
    window.place(1, c)?;
    window.map(1)?;
    println!(
        "window: {RANGES} ranges of {RANGE} bytes at GPA {WINDOW:#x}; first place_us={:.1} first stage2_map_us={:.1}",
        placed.as_secs_f64() * 1e6,
        mapped.as_secs_f64() * 1e6
    );
    Ok(window)
}

fn probe(api: &Api, guest: &mut Guest<'_>) -> Result<(), String> {
    let a = Section::new('A', 0xA1)?;
    let b = Section::new('B', 0xB2)?;
    let c = Section::new('C', 0xC3)?;
    let window = window(api, guest.partition, &a, &c)?;
    guest.start()?;
    coherence(guest, &window, &a, &c)?;
    if let Err(e) = swap_without_remap(guest, &window, &a, &b) {
        println!("swap without remap stopped: {e}");
        // Back to a known state: B placed in range 0 and mapped.
        let _unmapped = window.unmap(0);
        let _released = window.release(0);
        window.place(0, &b)?;
        window.map(0)?;
    }
    for way in [Way::Remap, Way::RemapPopulated, Way::Standing, Way::Populated] {
        if let Err(e) = replaces(guest, &window, [&a, &b], CODE, way) {
            println!("{} stopped: {e}", way.label());
        }
    }
    map_over_placeholder(guest, &window, &a, &b)?;
    // Past the window, within the guest's own 1 GiB page there; reserved readable and
    // writable, and inaccessible, as memory to plug is (platform::reserve_withheld).
    for (protect, at) in [(PAGE_READWRITE, 64), (PAGE_NOACCESS, 66)] {
        if let Err(e) = commit_as_touched(guest, api, protect, WINDOW + at * RANGE as u64) {
            println!("commit as touched stopped: {e}");
        }
    }
    let beside = guest.load(Window::gpa(1))?;
    print_load("the range beside, after every replace", &beside, &[&a, &b, &c], 0);
    Ok(())
}

fn main() {
    let api = match bind() {
        Ok(api) => api,
        Err(e) => {
            println!("unbound: {e}");
            return;
        }
    };
    let (partition, _ram) = match partition(&api) {
        Ok(p) => p,
        Err(e) => {
            println!("partition: {e}");
            return;
        }
    };
    let mut guest = Guest {
        api: &api,
        partition,
        rip: CODE,
    };
    let done = AtomicBool::new(false);
    let cancel = api.cancel;
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let until = Instant::now() + WATCHDOG;
            while !done.load(Ordering::Relaxed) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(10));
            }
            if !done.load(Ordering::Relaxed) {
                // SAFETY: the partition, alive until this scope ends.
                unsafe { cancel(partition, 0, 0) };
            }
        });
        if let Err(e) = probe(&api, &mut guest) {
            println!("stopped: {e}");
        }
        done.store(true, Ordering::Relaxed);
    });
    // SAFETY: the partition this probe made.
    unsafe {
        (api.delete_vp)(partition, 0);
        (api.delete_partition)(partition);
    }
}
