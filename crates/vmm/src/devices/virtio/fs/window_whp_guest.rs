//! The window's three edge cases through real guests on WHP: a view shorter than its range, a
//! section's 64 KiB granules, and several vCPUs at a range while it is replaced (PM M421).
//!
//! Each test makes a partition, a real [`Window`] over WHP's stage 2, and vCPUs in long mode
//! run by [`crate::hv::Vcpu::run`], whose memory exits ask the window as a VM's do. A vCPU's
//! program polls a mailbox in its RAM, so an access that needs no exit takes none, and does
//! what the host names: a load, a store, or the storm of the third case. A fault the window
//! does not take as a replace's falls through to [`Probe::mmio_read`] or
//! [`Probe::mmio_write`], as an access no device answers does in a VM, and every test holds
//! those to none but the ones it names.

use std::os::windows::io::OwnedHandle;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, UnmapViewOfFile,
};

use super::*;
use crate::arch::x86_64::{Boot, Segment};
use crate::hv;

const MIB: u64 = 1 << 20;
const PAGE: u64 = 4096;
/// The allocation granularity on every Windows the window runs on (asserted by each test
/// that leans on it, against the window's own reading).
const GRANULE: u64 = 64 << 10;
/// Where the window lies in the guest: its own 1 GiB page of the guest's tables.
const GPA: u64 = 1 << 32;
/// The guest's RAM from GPA 0: its code, tables and mailboxes.
const RAM: usize = (2 * MIB) as usize;
const CODE: u64 = 0x1000;
const GDT: u64 = 0x2000;
const IDT: u64 = 0x2100;
const EPOCH: u64 = 0x3000;
const MAILBOXES: u64 = 0x4000;
const MAILBOX: u64 = 0x100;
const PML4: u64 = 0x10000;
const PDPT: u64 = 0x11000;

/// A mailbox's words, by byte offset: the guest's program below reads them by these numbers.
const SEQ: u64 = 0;
const DONE: u64 = 8;
const KIND: u64 = 16;
const ADDRESS: u64 = 24;
const VALUE: u64 = 32;
const RESULT: u64 = 40;
const EPOCH_AT: u64 = 48;
const STOP: u64 = 56;
const STALE_EPOCH: u64 = 64;
const STALE_VALUE: u64 = 72;
const STALE: u64 = 80;
const LOOPS: u64 = 88;

const LOAD: u64 = 0;
const STORE: u64 = 1;
const STORM: u64 = 2;

std::arch::global_asm!(
    ".text",
    ".globl window_whp_guest_start",
    ".globl window_whp_guest_end",
    "window_whp_guest_start:",
    "2:",
    "mov rax, [rsi]",
    "cmp rax, [rsi + 8]",
    "jne 3f",
    "pause",
    "jmp 2b",
    "3:",
    "mov rbx, [rsi + 16]",
    "mov rcx, [rsi + 24]",
    "mov rdx, [rsi + 32]",
    "cmp rbx, 1",
    "je 4f",
    "cmp rbx, 2",
    "je 5f",
    "mov rdx, [rcx]",
    "mov [rsi + 40], rdx",
    "jmp 9f",
    "4:",
    "mov [rcx], rdx",
    "jmp 9f",
    // The storm: until told to stop, read the host's epoch, then the stamp through the
    // window at RCX, which must be at least that epoch, then store the epoch through the
    // window at RDX, where it is not zero.
    "5:",
    "mov r9, [rsi + 48]",
    "6:",
    "cmp qword ptr [rsi + 56], 0",
    "jne 9f",
    "mov r8, [r9]",
    "mov r10, [rcx]",
    "cmp r10, r8",
    "jae 7f",
    "mov [rsi + 64], r8",
    "mov [rsi + 72], r10",
    "inc qword ptr [rsi + 80]",
    "7:",
    "test rdx, rdx",
    "jz 8f",
    "mov [rdx], r8",
    "8:",
    "inc qword ptr [rsi + 88]",
    "jmp 6b",
    "9:",
    "mov [rsi + 8], rax",
    "jmp 2b",
    "window_whp_guest_end:",
);

unsafe extern "C" {
    static window_whp_guest_start: u8;
    static window_whp_guest_end: u8;
}

/// The guest's program, position-independent, as bytes.
fn program() -> &'static [u8] {
    let start = &raw const window_whp_guest_start;
    let end = &raw const window_whp_guest_end;
    // SAFETY: the bytes between two labels of the one block of code above, read alone.
    unsafe { std::slice::from_raw_parts(start, end as usize - start as usize) }
}

/// What the VM's bus does on a vCPU's memory exit: the window asked first, as
/// `Device::memory_fault` asks it, and an access it does not take counted as one no device
/// answers.
struct Probe {
    window: Arc<Window>,
    /// The faults the window told a vCPU to run again.
    told: AtomicU64,
    /// Each access the window did not take: its address, and whether a write.
    fell: Mutex<Vec<(u64, bool)>>,
}

/// What a load no device answers reads, as a bus reads: never a stamp.
const NO_DEVICE: u8 = 0xff;

impl hv::Io for Probe {
    fn mmio_read(&self, addr: u64, data: &mut [u8]) {
        self.fell.lock().unwrap().push((addr, false));
        data.fill(NO_DEVICE);
    }
    fn mmio_write(&self, addr: u64, _data: &[u8]) {
        self.fell.lock().unwrap().push((addr, true));
    }
    fn memory_fault(&self, gpa: Option<u64>) -> bool {
        let given = self.window.memory_fault(gpa);
        if given {
            self.told.fetch_add(1, Ordering::Relaxed);
        }
        given
    }
}

/// WHP's stage 2, for the window.
struct Real(hv::Stage2);

impl Stage2 for Real {
    unsafe fn map(&self, host: *mut u8, gpa: u64, len: usize, writable: bool) -> Result<(), String> {
        // SAFETY: forwarded caller contract.
        unsafe { self.0.map_file(host, gpa, len, writable) }.map_err(|e| e.to_string())
    }
    fn unmap(&self, gpa: u64, len: usize) -> Result<(), String> {
        self.0.unmap(gpa, len).map_err(|e| e.to_string())
    }
}

/// A partition with a window and `vcpus` vCPUs, each on its thread, polling its mailbox.
struct Machine {
    probe: Option<Arc<Probe>>,
    ram: *mut u8,
    stop: Arc<AtomicBool>,
    kickers: Vec<hv::Kicker>,
    threads: Vec<std::thread::JoinHandle<()>>,
    vm: Option<Arc<hv::Vm>>,
}

// SAFETY: the guest's RAM, committed until the machine drops, read and written atomically.
unsafe impl Sync for Machine {}

fn segments() -> (Segment, Segment, Segment) {
    let code = Segment {
        base: 0,
        limit: 0xffff_ffff,
        selector: 0x10,
        kind: 0xb,
        code_or_data: true,
        present: true,
        db: false,
        long: true,
        granular: true,
    };
    let data = Segment {
        selector: 0x18,
        kind: 3,
        db: true,
        long: false,
        ..code
    };
    let tr = Segment {
        base: 0,
        limit: 0x67,
        selector: 0x20,
        kind: 11,
        code_or_data: false,
        present: true,
        db: false,
        long: false,
        granular: false,
    };
    (code, data, tr)
}

impl Machine {
    /// None, said aloud, where the host offers no WHP.
    fn new(window_len: u64, vcpus: u32) -> Option<Machine> {
        if let Err(why) = hv::check_host() {
            eprintln!("SKIP: no WHP here ({why})");
            return None;
        }
        // SAFETY: fresh committed memory, released as the machine drops.
        let ram = unsafe { VirtualAlloc(std::ptr::null(), RAM, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) }
            .cast::<u8>();
        assert!(!ram.is_null(), "{}", std::io::Error::last_os_error());
        let code = program();
        // SAFETY: within the RAM just committed, nothing running yet.
        unsafe {
            std::ptr::copy_nonoverlapping(code.as_ptr(), ram.add(CODE as usize), code.len());
            ram.add(PML4 as usize).cast::<u64>().write(PDPT | 3);
            // 1 GiB pages: the RAM's, and the window's.
            ram.add(PDPT as usize).cast::<u64>().write(0x83);
            ram.add(PDPT as usize + 8 * (GPA >> 30) as usize)
                .cast::<u64>()
                .write(GPA | 0x83);
        }
        let vm = Arc::new(hv::Vm::new(hv::VmConfig { vcpus }).unwrap());
        // SAFETY: committed until the machine drops, after the VM.
        unsafe { vm.map_ram(ram, 0, RAM) }.unwrap();
        let window = Window::new(GPA, window_len, |host| {
            // SAFETY: the window's reservation stays until the window drops, before the VM.
            let stage2 =
                unsafe { vm.add_hotplug(host, GPA, window_len as usize) }.map_err(|e| e.to_string())?;
            Ok(Box::new(Real(stage2)) as Box<dyn Stage2>)
        })
        .unwrap();
        assert_eq!(window.granularity, GRANULE);
        let probe = Arc::new(Probe {
            window: Arc::new(window),
            told: AtomicU64::new(0),
            fell: Mutex::new(Vec::new()),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let (kicked, kickers) = mpsc::channel();
        let (cs, ds, tr) = segments();
        let mut threads = Vec::new();
        let all = Arc::new(std::sync::Barrier::new(vcpus as usize));
        // Made in index order, each on the thread that runs it, none run before all exist.
        for index in 0..vcpus {
            let vcpu_of = vm.clone();
            let (probe, stop, kicked, all) = (probe.clone(), stop.clone(), kicked.clone(), all.clone());
            let (ready, wait) = mpsc::channel();
            threads.push(
                std::thread::Builder::new()
                    .name(format!("vcpu{index}"))
                    .spawn(move || {
                        let mut vcpu = vcpu_of.create_vcpu(index as usize).unwrap();
                        vcpu.boot(&Boot {
                            rip: CODE,
                            rsi: MAILBOXES + u64::from(index) * MAILBOX,
                            cr0: 0x8000_0031,
                            cr3: PML4,
                            cr4: 0x20,
                            efer: 0x500,
                            cs,
                            data: ds,
                            tr,
                            gdt: (GDT, 39),
                            idt: (IDT, 7),
                        })
                        .unwrap();
                        vcpu.leave_wait_for_sipi().unwrap();
                        kicked.send(vcpu.kicker()).unwrap();
                        ready.send(()).unwrap();
                        all.wait();
                        loop {
                            match vcpu.run(&*probe) {
                                Ok(hv::Exit::Canceled) if stop.load(Ordering::Acquire) => return,
                                Ok(hv::Exit::Canceled) => {}
                                other => panic!("vCPU {index}: {other:?}"),
                            }
                        }
                    })
                    .unwrap(),
            );
            wait.recv().unwrap();
        }
        let kickers = kickers.iter().take(vcpus as usize).collect();
        Some(Machine {
            probe: Some(probe),
            ram,
            stop,
            kickers,
            threads,
            vm: Some(vm),
        })
    }

    fn probe(&self) -> &Probe {
        self.probe.as_ref().unwrap()
    }

    fn window(&self) -> &Window {
        &self.probe().window
    }

    /// RAM's word at `gpa`.
    fn word(&self, gpa: u64) -> &AtomicU64 {
        assert!(gpa < RAM as u64 && gpa.is_multiple_of(8));
        // SAFETY: within the guest's RAM, committed for the machine's life, 8-aligned, and read
        // and written by this test atomically and by the guest by whole aligned stores.
        unsafe { &*self.ram.add(gpa as usize).cast::<AtomicU64>() }
    }

    fn mail(&self, vcpu: u32, word: u64) -> &AtomicU64 {
        self.word(MAILBOXES + u64::from(vcpu) * MAILBOX + word)
    }

    /// Asks vCPU `vcpu` for an operation, not waiting.
    fn ask(&self, vcpu: u32, kind: u64, address: u64, value: u64) {
        self.mail(vcpu, KIND).store(kind, Ordering::Relaxed);
        self.mail(vcpu, ADDRESS).store(address, Ordering::Relaxed);
        self.mail(vcpu, VALUE).store(value, Ordering::Relaxed);
        self.mail(vcpu, SEQ).fetch_add(1, Ordering::Release);
    }

    /// Waits for vCPU `vcpu`'s last operation: whether it finished within the bound.
    fn answered_within(&self, vcpu: u32, bound: Duration) -> bool {
        let started = Instant::now();
        while self.mail(vcpu, DONE).load(Ordering::Acquire) != self.mail(vcpu, SEQ).load(Ordering::Relaxed) {
            if started.elapsed() > bound {
                return false;
            }
            std::thread::yield_now();
        }
        true
    }

    fn answered(&self, vcpu: u32) {
        assert!(
            self.answered_within(vcpu, Duration::from_secs(30)),
            "vCPU {vcpu} did not finish its operation"
        );
    }

    /// The guest's load at window offset `at`, by vCPU 0.
    fn load(&self, at: u64) -> u64 {
        self.ask(0, LOAD, GPA + at, 0);
        self.answered(0);
        self.mail(0, RESULT).load(Ordering::Acquire)
    }

    /// The guest's store at window offset `at`, by vCPU 0.
    fn store(&self, at: u64, value: u64) {
        self.ask(0, STORE, GPA + at, value);
        self.answered(0);
    }

    /// The accesses the window did not take, taken.
    fn fell(&self) -> Vec<(u64, bool)> {
        std::mem::take(&mut *self.probe().fell.lock().unwrap())
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for thread in self.threads.drain(..) {
            while !thread.is_finished() {
                self.kickers.iter().for_each(hv::Kicker::kick);
                std::thread::yield_now();
            }
            let joined = thread.join();
            if !std::thread::panicking() {
                joined.unwrap();
            }
        }
        // The window before the VM: it unmaps its standing ranges.
        drop(self.probe.take());
        drop(self.vm.take());
        // SAFETY: the allocation this value made, the VM that mapped it gone.
        unsafe { VirtualFree(self.ram.cast(), 0, MEM_RELEASE) };
    }
}

/// A pagefile-backed section, as slates' daemon makes one, and the server's own view.
struct Section {
    handle: OwnedHandle,
    view: *mut u8,
    len: u64,
}

// SAFETY: a view for the section's life, read and written atomically by the storm's test.
unsafe impl Sync for Section {}

impl Section {
    fn new(len: u64, word: impl Fn(u64) -> u64) -> Section {
        // SAFETY: a fresh section of the pagefile's, checked below.
        let handle = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                (len >> 32) as u32,
                len as u32,
                std::ptr::null(),
            )
        };
        assert!(!handle.is_null(), "{}", std::io::Error::last_os_error());
        // SAFETY: the section just made, owned from here on.
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        // SAFETY: a view of the whole section, unmapped as this drops.
        let view = unsafe { MapViewOfFile(handle.as_raw_handle(), FILE_MAP_ALL_ACCESS, 0, 0, len as usize) }
            .Value
            .cast::<u8>();
        assert!(!view.is_null(), "{}", std::io::Error::last_os_error());
        let section = Section { handle, view, len };
        for at in (0..len).step_by(8) {
            section.word(at).store(word(at), Ordering::Relaxed);
        }
        section
    }

    fn give(&self) -> OwnedHandle {
        self.handle.try_clone().unwrap()
    }

    fn word(&self, at: u64) -> &AtomicU64 {
        assert!(at < self.len && at.is_multiple_of(8));
        // SAFETY: within the view, 8-aligned, read and written atomically here and by whole
        // aligned stores in the guest.
        unsafe { &*self.view.add(at as usize).cast::<AtomicU64>() }
    }
}

impl Drop for Section {
    fn drop(&mut self) {
        // SAFETY: the view this value made.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.view.cast(),
            })
        };
    }
}

/// What a file's word at `at` holds in these tests: never zero, and its own offset's.
fn file_word(at: u64) -> u64 {
    0x5a5a_0000_0000_0000 | at
}

/// A file of `size` bytes of [`file_word`], open to read and write.
fn file_of(dir: &shards_testdir::TempDir, name: &str, size: u64) -> std::fs::File {
    let path = dir.join(name);
    let bytes: Vec<u8> = (0..size.div_ceil(8))
        .flat_map(|w| file_word(w * 8).to_le_bytes())
        .take(size as usize)
        .collect();
    std::fs::write(&path, &bytes).unwrap();
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap()
}

fn handle_of(file: &std::fs::File) -> OwnedHandle {
    OwnedHandle::from(file.try_clone().unwrap())
}

/// PM M421, the first case. Do, as a real guest, for a file's end at each place a range can
/// hold it (one byte in; a page; a page and a byte; a granule; a granule and a byte; the page
/// before the last; a byte short of the range; the whole range): map 4 MiB of it and load its
/// first word, its last whole word, a word past its end in its last page, a page past
/// that, and the second range. Expect the file's bytes, then zeros, with no access falling
/// through and stage 2 told of each range once. Then store past the end and within, and
/// expect the store within to be the file's and the file's length unchanged; grow it and
/// expect the guest's next load, once the window follows it, the file's new bytes.
#[test]
fn a_view_shorter_than_its_range_is_its_files_bytes_then_zeros_to_a_guest() {
    let Some(machine) = Machine::new(8 * MIB, 1) else {
        return;
    };
    let dir = shards_testdir::TempDir::new("whp-short").unwrap();
    let w = machine.window();
    for (case, size) in [
        1,
        PAGE,
        PAGE + 1,
        GRANULE,
        GRANULE + 1,
        RANGE - PAGE,
        RANGE - 1,
        RANGE,
    ]
    .into_iter()
    .enumerate()
    {
        let file = file_of(&dir, &format!("f{case}"), size);
        assert_eq!(
            w.map(handle_of(&file), 1, 0, 0, 4 * MIB, true),
            Ok(size),
            "size {size}"
        );
        let first = machine.load(0);
        if size >= 8 {
            assert_eq!(first, file_word(0), "size {size}: its first word");
            let last = (size - 8) / 8 * 8;
            assert_eq!(
                machine.load(last),
                file_word(last),
                "size {size}: its last whole word"
            );
        } else {
            assert_eq!(first & 0xff, file_word(0) & 0xff, "size {size}: its one byte");
            assert_eq!(first >> 8, 0, "size {size}: zeros past its end in its page");
        }
        // Past its end, in the page that holds it, then a page on, then the second range.
        let page_end = size.next_multiple_of(PAGE);
        if page_end - size >= 16 {
            assert_eq!(
                machine.load(page_end - 8),
                0,
                "size {size}: past its end, its page"
            );
        }
        if page_end < RANGE {
            assert_eq!(machine.load(page_end), 0, "size {size}: the page after");
            assert_eq!(machine.load(RANGE - 8), 0, "size {size}: the range's last word");
        }
        assert_eq!(machine.load(RANGE), 0, "size {size}: the second range");
        // A store into the committed zeros stays the window's; one within is the file's.
        if page_end < RANGE {
            machine.store(page_end, 0x77);
            assert_eq!(
                machine.load(page_end),
                0x77,
                "size {size}: its store into the zeros"
            );
        }
        machine.store(RANGE, 0x88);
        assert_eq!(
            machine.load(RANGE),
            0x88,
            "size {size}: its store into the second range"
        );
        machine.store(0, 0x99);
        let read = std::fs::read(dir.join(format!("f{case}"))).unwrap();
        assert_eq!(read.len() as u64, size, "no store past its end grew it");
        assert_eq!(read[0], 0x99, "size {size}: its store is the file's");
        // Grown past the range's first page and past it whole: the guest's as it is followed.
        if size < RANGE {
            std::os::windows::fs::FileExt::seek_write(&file, &[0x6b; 8], RANGE + PAGE).unwrap();
            assert_eq!(w.reconcile(&[1]), Ok(()));
            assert_eq!(
                machine.load(RANGE + PAGE),
                0x6b6b_6b6b_6b6b_6b6b,
                "size {size}: grown"
            );
            assert_eq!(machine.load(RANGE + 2 * PAGE), 0, "size {size}: past its new end");
        }
        w.unmap(0, 4 * MIB).unwrap();
        assert!(!w.maps(1));
    }
    assert_eq!(machine.fell(), [], "every access was memory");
    assert_eq!(
        machine.probe().told.load(Ordering::Relaxed),
        0,
        "with one vCPU and no replace under way, nothing faulted"
    );
}

/// PM M421, the first case as a server's section meets it. Do: map a section that ends within
/// its range, one that ends within a mapping's second range, and a section that ends a page
/// past a range. Expect EIO for the first two (a section has no zeros after it, and a view
/// past its bytes is refused: PM M412), the first range freed again, each range taking a
/// whole section's view after, a guest's load finding it, and the third mapped whole.
#[test]
fn a_section_that_ends_within_its_range_is_refused_and_the_range_stays_whole() {
    let Some(machine) = Machine::new(8 * MIB, 1) else {
        return;
    };
    let w = machine.window();
    let short = Section::new(MIB, file_word);
    assert_eq!(w.map(short.give(), 1, 0, 0, RANGE, true), Err(proto::EIO));
    assert!(w.mappings().is_empty());
    let one_and_a_half = Section::new(3 * MIB, file_word);
    assert_eq!(
        w.map(one_and_a_half.give(), 2, 0, 0, 4 * MIB, true),
        Err(proto::EIO),
        "its second range ends short"
    );
    assert!(w.mappings().is_empty());
    let whole = Section::new(RANGE + PAGE, file_word);
    assert_eq!(w.map(whole.give(), 3, 0, 0, RANGE, true), Ok(RANGE));
    assert_eq!(machine.load(RANGE - 8), file_word(RANGE - 8));
    assert_eq!(
        w.map(whole.give(), 3, 0, RANGE, RANGE, true),
        Ok(RANGE),
        "the range the refused mapping freed takes a whole view"
    );
    assert_eq!(machine.load(RANGE + PAGE), file_word(PAGE));
    assert_eq!(machine.fell(), []);
}

/// PM M421, the second case. Do, as a real guest: map a range of a section from each of a
/// range's 32 granules, the first and the last of a two-range mapping among them; and from a
/// page inside a granule, and from a granule's half. Expect each view to begin at its offset
/// to the guest, each way (a store through it is the section's at that offset, and its last
/// word is the section's), and each offset off a granule refused EINVAL with what was mapped
/// there still the guest's.
#[test]
fn a_section_is_viewed_from_each_granule_and_refused_between_them() {
    let Some(machine) = Machine::new(16 * MIB, 1) else {
        return;
    };
    let w = machine.window();
    let s = Section::new(8 * MIB, file_word);
    for granule in 0..RANGE / GRANULE {
        let foffset = granule * GRANULE;
        assert_eq!(w.map(s.give(), 1, foffset, 0, 4 * MIB, true), Ok(4 * MIB));
        assert_eq!(machine.load(0), file_word(foffset), "from {foffset:#x}");
        assert_eq!(
            machine.load(RANGE),
            file_word(foffset + RANGE),
            "from {foffset:#x}: its second range"
        );
        assert_eq!(
            machine.load(4 * MIB - 8),
            file_word(foffset + 4 * MIB - 8),
            "from {foffset:#x}: its last word"
        );
        machine.store(GRANULE, 0xabc0 + granule);
        assert_eq!(
            s.word(foffset + GRANULE).load(Ordering::Acquire),
            0xabc0 + granule,
            "from {foffset:#x}: the guest's store"
        );
        s.word(foffset + GRANULE)
            .store(file_word(foffset + GRANULE), Ordering::Release);
    }
    let foffset = GRANULE;
    assert_eq!(w.map(s.give(), 1, foffset, 0, RANGE, true), Ok(RANGE));
    for off_granule in [PAGE, GRANULE - PAGE, GRANULE / 2, GRANULE + PAGE, 1, RANGE - PAGE] {
        assert_eq!(
            w.map(s.give(), 2, off_granule, 0, RANGE, true),
            Err(proto::EINVAL),
            "from {off_granule:#x}"
        );
        assert!(w.maps(1), "a refused mapping leaves what was there");
        assert_eq!(machine.load(0), file_word(foffset), "from {off_granule:#x}");
    }
    assert_eq!(machine.fell(), []);
    assert_eq!(machine.probe().told.load(Ordering::Relaxed), 0);
}

/// The vCPUs a storm's test runs: two whatever the host, and more up to its cores, at most
/// eight.
fn storm_widths() -> Vec<u32> {
    let cores = std::thread::available_parallelism().map_or(2, |n| n.get() as u32);
    [2, 4, 8].into_iter().filter(|&n| n == 2 || n <= cores).collect()
}

/// Where each vCPU's store goes in a range: its own word.
const SLOTS: u64 = PAGE;
/// Where each range holds the host's stamp.
const STAMP: u64 = 0x800;
/// The replaces a storm makes where its host has a core for each of its threads: a paced run
/// took 1.0 s at 4 vCPUs on jupiter, 5.2 s at 2 on windows-2025 (PM M421).
const STORM_REPLACES: u64 = 20_000;

/// The replaces at `vcpus` vCPUs: fewer where they, the vCPU beside them and the host's thread
/// outnumber the host's cores, where a paced replace took about 24 ms against 0.26 ms
/// (6 runnable on windows-2025's 4, PM M421).
fn storm_replaces(vcpus: u32) -> u64 {
    let cores = std::thread::available_parallelism().map_or(2, |n| n.get() as u32);
    if vcpus + 2 > cores {
        STORM_REPLACES / 20
    } else {
        STORM_REPLACES
    }
}

/// What a storm found.
#[derive(Debug, Default)]
struct Storm {
    loads_after_a_replace_that_saw_the_old_section: u64,
    stores_after_a_replace_that_reached_the_old_section: u64,
    fell: usize,
    told: u64,
    loops: u64,
    stores_seen: u64,
    took: Duration,
}

/// `vcpus` vCPUs in a storm over a two-range mapping that the host replaces `replaces` times,
/// alternating two sections, while a last vCPU's range beside it is left alone: `paced`, each
/// replace waits until every vCPU has been through the last one's section, so that each is
/// replaced under vCPUs mapped at it; else as fast as the host replaces, faster than a vCPU's
/// first access to a range (20,000 in 321 ms on jupiter), so that most faults meet another.
///
/// Each round the host stamps the section to come with the round's number, replaces the
/// mapping with it, and only then publishes the round as the epoch in the guest's RAM. A
/// vCPU's storm reads the epoch, loads the stamp, and stores the epoch back through the
/// window. So a load that began after a replace returned and read less than that epoch read
/// the old section through a stale stage-2 mapping; and a store carrying the epoch of the
/// replace that took a section away, found in that section, reached it after the replace
/// returned. An access the window did not take is a fault it gave up on.
fn storm(vcpus: u32, replaces: u64, paced: bool) -> Option<Storm> {
    let beside = vcpus;
    let machine = Machine::new(8 * MIB, vcpus + 1)?;
    let w = machine.window();
    let sections = [Section::new(4 * MIB, |_| 0), Section::new(4 * MIB, |_| 0)];
    let alone = Section::new(RANGE, |_| u64::MAX);
    assert_eq!(w.map(alone.give(), 9, 0, 4 * MIB, RANGE, false), Ok(RANGE));
    assert_eq!(w.map(sections[0].give(), 1, 0, 0, 4 * MIB, true), Ok(4 * MIB));
    for vcpu in 0..=vcpus {
        machine.mail(vcpu, EPOCH_AT).store(EPOCH, Ordering::Release);
    }
    // Each vCPU at one range of the mapping, the odd ones at the second.
    for vcpu in 0..vcpus {
        let range = u64::from(vcpu % 2) * RANGE;
        machine.ask(
            vcpu,
            STORM,
            GPA + range + STAMP,
            GPA + range + SLOTS + u64::from(vcpu) * 8,
        );
    }
    machine.ask(beside, STORM, GPA + 4 * MIB + STAMP, 0);
    while (0..=vcpus).any(|v| machine.mail(v, LOOPS).load(Ordering::Acquire) == 0) {
        std::thread::yield_now();
    }
    let mut found = Storm::default();
    let started = Instant::now();
    for round in 1..=replaces {
        let incoming = &sections[(round % 2) as usize];
        // A store found here carrying the epoch of the replace that took this section away
        // (the last round's), or a later one, reached it after that replace returned.
        for vcpu in 0..vcpus {
            let range = u64::from(vcpu % 2) * RANGE;
            let tag = incoming
                .word(range + SLOTS + u64::from(vcpu) * 8)
                .load(Ordering::Acquire);
            if round >= 2 && tag >= round - 1 {
                found.stores_after_a_replace_that_reached_the_old_section += 1;
            }
            if round >= 2 && tag + 2 == round {
                found.stores_seen += 1;
            }
        }
        incoming.word(STAMP).store(round, Ordering::Release);
        incoming.word(RANGE + STAMP).store(round, Ordering::Release);
        assert_eq!(w.map(incoming.give(), 1, 0, 0, 4 * MIB, true), Ok(4 * MIB));
        machine.word(EPOCH).store(round, Ordering::Release);
        if paced {
            // Every vCPU through a whole turn of its loop begun after the epoch: a load and a
            // store through this very section, whatever its faults cost.
            let from: Vec<u64> = (0..vcpus)
                .map(|v| machine.mail(v, LOOPS).load(Ordering::Acquire))
                .collect();
            let began = Instant::now();
            for (vcpu, from) in (0..vcpus).zip(from) {
                while machine.mail(vcpu, LOOPS).load(Ordering::Acquire) < from + 2 {
                    assert!(
                        began.elapsed() < Duration::from_secs(30),
                        "vCPU {vcpu} stopped at round {round}"
                    );
                    std::hint::spin_loop();
                }
            }
        }
    }
    found.took = started.elapsed();
    for vcpu in 0..=vcpus {
        machine.mail(vcpu, STOP).store(1, Ordering::Release);
    }
    for vcpu in 0..=vcpus {
        machine.answered(vcpu);
        found.loads_after_a_replace_that_saw_the_old_section +=
            machine.mail(vcpu, STALE).load(Ordering::Acquire);
        found.loops += machine.mail(vcpu, LOOPS).load(Ordering::Acquire);
        let stale = machine.mail(vcpu, STALE).load(Ordering::Acquire);
        if stale > 0 {
            eprintln!(
                "vCPU {vcpu}: {stale} stale, the last read {:#x} at epoch {:#x}",
                machine.mail(vcpu, STALE_VALUE).load(Ordering::Acquire),
                machine.mail(vcpu, STALE_EPOCH).load(Ordering::Acquire)
            );
        }
    }
    found.told = machine.probe().told.load(Ordering::Relaxed);
    let fell = machine.fell();
    if let Some(first) = fell.first() {
        eprintln!("the first of {} that fell: {first:x?}", fell.len());
    }
    found.fell = fell.len();
    assert_eq!(
        machine.mail(beside, STALE).load(Ordering::Acquire),
        0,
        "a range nothing replaced stays what it is"
    );
    Some(found)
}

/// PM M421, the third case. Do, on a real partition, at each width, paced and not: several vCPUs
/// load and store through a two-range mapping while another thread replaces it ([`storm`]).
/// Expect every fault the window took to be run again and to find memory, none to fall through
/// as the guest's, no load begun after a replace returned to see the old section, and no store
/// begun after it to reach the old one. Paced, expect each vCPU's store of every epoch found in
/// its section, so that no store was lost; and, so that none of this is vacuous, faults that
/// met a replace.
#[test]
fn several_vcpus_at_a_range_being_replaced_lose_nothing_and_see_nothing_stale() {
    for (vcpus, paced) in storm_widths().into_iter().flat_map(|v| [(v, true), (v, false)]) {
        let replaces = storm_replaces(vcpus);
        let Some(found) = storm(vcpus, replaces, paced) else {
            return;
        };
        eprintln!("PM M421: {vcpus} vCPUs, {replaces} replaces, paced {paced}: {found:?}");
        assert_eq!(found.fell, 0, "{vcpus} vCPUs: {found:?}");
        assert_eq!(
            found.loads_after_a_replace_that_saw_the_old_section, 0,
            "{found:?}"
        );
        assert_eq!(
            found.stores_after_a_replace_that_reached_the_old_section, 0,
            "{found:?}"
        );
        assert!(found.told > 0, "{vcpus} vCPUs: no fault met a replace: {found:?}");
        if paced {
            assert_eq!(
                found.stores_seen,
                u64::from(vcpus) * (replaces - 1),
                "every vCPU's store of every epoch: {found:?}"
            );
        }
    }
}

/// Do: follow a growing file with [`Window::reconcile`] while another thread takes the window's
/// lock over and over, reading what range the file's views now reach and the count of changes.
/// Expect no change seen with the count not yet moved past it: a vCPU's fault that gets the lock
/// there reads the old count, and if it asked at that count before, [`Window::memory_fault`]
/// takes a live range's fault for the guest's own (PM M421). This needs no partition.
#[test]
fn a_range_followed_is_counted_before_the_lock_is_let_go() {
    struct Silent;
    impl Stage2 for Silent {
        unsafe fn map(&self, _host: *mut u8, _gpa: u64, _len: usize, _writable: bool) -> Result<(), String> {
            Ok(())
        }
        fn unmap(&self, _gpa: u64, _len: usize) -> Result<(), String> {
            Ok(())
        }
    }
    const GROWTHS: u64 = 500;
    const ROUNDS: u64 = 40;
    let dir = shards_testdir::TempDir::new("whp-count").unwrap();
    let w = Window::new(GPA, 4 * MIB, |_| Ok(Box::new(Silent) as Box<dyn Stage2>)).unwrap();
    let file = file_of(&dir, "f", PAGE);
    assert_eq!(w.map(handle_of(&file), 1, 0, 0, RANGE, true), Ok(PAGE));
    let done = AtomicBool::new(false);
    let uncounted = std::thread::scope(|scope| {
        let watcher = scope.spawn(|| {
            let mut uncounted = 0u64;
            let mut last = (PAGE, w.changes.load(Ordering::Acquire));
            while !done.load(Ordering::Acquire) {
                let table = w.lock();
                let to = table.ranges.get(&0).and_then(|e| e.file_to).unwrap_or(0);
                let changes = w.changes.load(Ordering::Acquire);
                drop(table);
                if to != last.0 && changes == last.1 {
                    uncounted += 1;
                }
                last = (to, changes);
            }
            uncounted
        });
        for _ in 0..ROUNDS {
            for growth in 1..GROWTHS {
                file.set_len(PAGE + growth * PAGE).unwrap();
                assert_eq!(w.reconcile(&[1]), Ok(()));
            }
            w.evict(1);
            file.set_len(PAGE).unwrap();
            assert_eq!(w.reconcile(&[1]), Ok(()));
        }
        done.store(true, Ordering::Release);
        watcher.join().unwrap()
    });
    assert_eq!(
        uncounted, 0,
        "changes seen with the count not yet moved past them"
    );
}

/// PM M421, the first and third cases together. Do, on a real partition: several vCPUs load and
/// store in a file's first page, which every view holds, while the host grows the file a page at
/// a time, following it (its view longer each time, the split moved), and cuts it short again
/// (let go of, cut, made again). Expect every access to find memory, none taken for the guest's;
/// the file's own bytes at each load; and each vCPU's last store in the file.
#[test]
fn several_vcpus_at_a_view_followed_and_cut_lose_nothing() {
    const GROWTHS: u64 = 64;
    const ROUNDS: u64 = 100;
    for vcpus in storm_widths() {
        let Some(machine) = Machine::new(4 * MIB, vcpus) else {
            return;
        };
        let w = machine.window();
        let dir = shards_testdir::TempDir::new("whp-follow").unwrap();
        let file = file_of(&dir, "f", PAGE);
        assert_eq!(w.map(handle_of(&file), 1, 0, 0, RANGE, true), Ok(PAGE));
        for vcpu in 0..vcpus {
            machine.mail(vcpu, EPOCH_AT).store(EPOCH, Ordering::Release);
            machine.ask(vcpu, STORM, GPA + 2048, GPA + 8 * u64::from(vcpu));
        }
        let started = Instant::now();
        for _ in 0..ROUNDS {
            for growth in 1..GROWTHS {
                let grown = PAGE + growth * PAGE + growth % 3;
                file.set_len(grown).unwrap();
                assert_eq!(w.reconcile(&[1]), Ok(()));
            }
            w.evict(1);
            file.set_len(PAGE).unwrap();
            assert_eq!(w.reconcile(&[1]), Ok(()));
        }
        let took = started.elapsed();
        for vcpu in 0..vcpus {
            machine.mail(vcpu, STOP).store(1, Ordering::Release);
        }
        for vcpu in 0..vcpus {
            machine.answered(vcpu);
        }
        let fell = machine.fell();
        let told = machine.probe().told.load(Ordering::Relaxed);
        let loops: u64 = (0..vcpus)
            .map(|v| machine.mail(v, LOOPS).load(Ordering::Acquire))
            .sum();
        eprintln!(
            "PM M421: {vcpus} vCPUs, {} followings: fell {}, told {told}, loops {loops}, took {took:?}",
            ROUNDS * GROWTHS,
            fell.len()
        );
        assert_eq!(fell, [], "{vcpus} vCPUs");
        assert!(told > 0, "{vcpus} vCPUs: no fault met a following");
        assert_eq!(machine.load(2048), file_word(2048), "the file's own");
        w.unmap(0, RANGE).unwrap();
    }
}
