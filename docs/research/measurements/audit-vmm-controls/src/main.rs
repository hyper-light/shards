//! Actual public control/serial APIs and macOS HVF handle-lifetime probes.
use std::alloc::{GlobalAlloc, Layout, System};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

use shards_vmm::devices::control::Control;
use shards_vmm::devices::serial::Serial;
use shards_vmm::devices::{Interrupt, MmioDevice};

static COUNT: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static REALLOCS: AtomicUsize = AtomicUsize::new(0);
static REQUESTED: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
struct Allocator;

fn add(n: usize) {
    REQUESTED.fetch_add(n, Relaxed);
    let live = LIVE.fetch_add(n, Relaxed) + n;
    PEAK.fetch_max(live, Relaxed);
}

// SAFETY: System receives the original allocator arguments. Counters allocate nothing.
// Counted phases own all the allocations they free; no preexisting storage is freed.
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        // SAFETY: forwarded allocator contract.
        let p = unsafe { System.alloc(l) };
        if !p.is_null() && COUNT.load(Relaxed) {
            ALLOCS.fetch_add(1, Relaxed);
            add(l.size());
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        // SAFETY: forwarded allocator contract.
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() && COUNT.load(Relaxed) {
            ALLOCS.fetch_add(1, Relaxed);
            add(l.size());
        }
        p
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        // SAFETY: forwarded allocator contract; failure leaves p alive.
        let next = unsafe { System.realloc(p, l, n) };
        if !next.is_null() && COUNT.load(Relaxed) {
            REALLOCS.fetch_add(1, Relaxed);
            LIVE.fetch_sub(l.size(), Relaxed);
            add(n);
        }
        next
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        if COUNT.load(Relaxed) {
            LIVE.fetch_sub(l.size(), Relaxed);
        }
        // SAFETY: forwarded allocator contract.
        unsafe { System.dealloc(p, l) };
    }
}
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

fn begin(count: bool) {
    for c in [&ALLOCS, &REALLOCS, &REQUESTED, &LIVE, &PEAK] {
        c.store(0, Relaxed);
    }
    COUNT.store(count, Relaxed);
}
fn counters() -> [usize; 5] {
    COUNT.store(false, Relaxed);
    [
        ALLOCS.load(Relaxed),
        REALLOCS.load(Relaxed),
        REQUESTED.load(Relaxed),
        LIVE.load(Relaxed),
        PEAK.load(Relaxed),
    ]
}

fn markers(n: usize, count: bool) {
    shards_vmm::log::init();
    assert!(!shards_vmm::log::enabled(shards_vmm::log::Level::Info));
    let c = Control::default();
    let _ = shards_vmm::log::uptime_us();
    begin(count);
    let at = Instant::now();
    for marker in 0..n {
        c.write(shards_abi::control::MARKER, &(marker as u32).to_le_bytes());
    }
    let ns = at.elapsed().as_nanos();
    let [allocs, reallocs, requested, live, peak] = counters();
    begin(count);
    let copy = c.markers();
    let [clone_allocs, _, clone_requested, clone_live, _] = counters();
    assert_eq!(copy.len(), n);
    assert!(copy.iter().enumerate().all(|(i, (m, _))| *m == i as u32));
    println!(
        "{{\"case\":\"markers\",\"n_markers\":{n},\"ns\":{ns},\"allocs\":{allocs},\"reallocs\":{reallocs},\"requested\":{requested},\"live\":{live},\"peak\":{peak},\"clone_allocs\":{clone_allocs},\"clone_requested\":{clone_requested},\"clone_live\":{clone_live},\"entry_bytes\":{}}}",
        size_of::<(u32, u128)>()
    );
}

#[derive(Debug)]
struct NoIrq;
impl Interrupt for NoIrq {
    fn set_level(&self, _: bool) {}
}

#[cfg(unix)]
fn serial_block() -> io::Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::sync::mpsc;
    let mut fds = [-1; 2];
    // SAFETY: two writable descriptor outputs.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: each successful pipe descriptor is transferred once to File ownership.
    let (mut reader, writer) = unsafe {
        (
            std::fs::File::from_raw_fd(fds[0]),
            std::fs::File::from_raw_fd(fds[1]),
        )
    };
    let fd = writer.as_raw_fd();
    // SAFETY: valid descriptor, ordinary descriptor flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let fill = [0xa5u8; 4096];
    let mut filled = 0;
    loop {
        // SAFETY: fill contains 4096 initialized readable bytes.
        let n = unsafe { libc::write(fd, fill.as_ptr().cast(), fill.len()) };
        if n >= 0 {
            filled += n as usize;
            continue;
        }
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if e.kind() == io::ErrorKind::WouldBlock {
            break;
        }
        return Err(e);
    }
    // SAFETY: valid descriptor, restore the original blocking policy.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let serial = Arc::new(Serial::new(Box::new(writer), Arc::new(NoIrq)));
    let (entered, entering) = mpsc::channel();
    let (done, finished) = mpsc::channel();
    let s = serial.clone();
    let thread = std::thread::spawn(move || {
        entered.send(()).unwrap();
        s.write(0, b"X");
        done.send(()).unwrap();
    });
    entering.recv().unwrap();
    let blocked = finished.recv_timeout(Duration::from_millis(200)).is_err();
    // Drain to release the thread; no VM or guest is started.
    use std::io::Read;
    let mut drained = vec![0u8; filled];
    reader.read_exact(&mut drained)?;
    if blocked {
        finished.recv_timeout(Duration::from_secs(2)).unwrap();
    }
    thread.join().unwrap();
    let mut byte = [0];
    reader.read_exact(&mut byte)?;
    assert_eq!(byte, *b"X");
    println!(
        "{{\"case\":\"serial_full_pipe\",\"pipe_bytes\":{filled},\"blocked_for_200ms\":{blocked},\"completed_after_drain\":true}}"
    );
    Ok(())
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod mac {
    use shards_vmm::hv::{self, GicLayout, Io, VmConfig};
    #[link(name = "Hypervisor", kind = "framework")]
    unsafe extern "C" {
        fn hv_gic_get_distributor_reg(reg: u16, value: *mut u64) -> i32;
        fn hv_gic_set_distributor_reg(reg: u16, value: u64) -> i32;
    }
    fn read(reg: u16) -> u64 {
        let mut value = 0;
        // SAFETY: a live GIC and a writable u64. Observation uses the same SDK API as tests.
        assert_eq!(unsafe { hv_gic_get_distributor_reg(reg, &mut value) }, 0);
        value
    }
    fn write(reg: u16, value: u64) {
        // SAFETY: a live GIC; no host memory is passed.
        assert_eq!(unsafe { hv_gic_set_distributor_reg(reg, value) }, 0);
    }
    fn machine() -> (hv::Vm, hv::Gic) {
        let vm = hv::Vm::new(VmConfig {
            ipa_bits: 36,
            mpidrs: vec![0],
        })
        .unwrap();
        let gic = vm
            .create_gic(&GicLayout {
                dist_base: 0x0800_0000,
                redist_base: 0x080a_0000,
                msi: None,
            })
            .unwrap();
        (vm, gic)
    }
    pub fn stale_gic() {
        let (a, old) = machine();
        drop(a);
        let without_vm = old.set_spi(33, true).is_ok();
        let (b, _new) = machine();
        write(0x104, 2); // enable SPI33
        write(0xc08, 8); // SPI33 edge-triggered
        let before = read(0x204);
        let accepted = old.set_spi(33, true).is_ok();
        let after = read(0x204);
        println!(
            "{{\"case\":\"stale_gic\",\"accepted_without_vm\":{without_vm},\"accepted_with_replacement\":{accepted},\"pending_before\":{before},\"pending_after\":{after},\"changed_replacement\":{}}}",
            before != after
        );
        drop(b);
    }
    struct EmptyIo;
    impl Io for EmptyIo {
        fn mmio_read(&self, _: u64, data: &mut [u8]) {
            data.fill(0);
        }
        fn mmio_write(&self, _: u64, _: &[u8]) {}
    }
    pub fn stale_kicker() {
        let (a, _gic) = machine();
        let cpu = a.create_vcpu(0).unwrap();
        let old = cpu.kicker();
        drop(cpu);
        drop(a);
        let memory = shards_vmm::memory::GuestMemory::anonymous(&[(0x8000_0000, 16384)]).unwrap();
        memory
            .access()
            .unwrap()
            .write(0x8000_0000, &0xd4200000u32.to_le_bytes())
            .unwrap(); // BRK #0
        let (b, _gic) = machine();
        for (gpa, host, len) in memory.regions() {
            // SAFETY: memory is declared before b, and remains mapped until b drops.
            unsafe { b.map_ram(host, gpa, len) }.unwrap();
        }
        let mut cpu = b.create_vcpu(0).unwrap();
        cpu.boot(shards_vmm::arch::aarch64::Entry {
            pc: 0x8000_0000,
            x0: 0,
        });
        let baseline_traps = cpu.run(&EmptyIo).is_err();
        assert!(baseline_traps, "BRK must trap before the stale kick");
        old.kick();
        let canceled = cpu.run(&EmptyIo) == Ok(hv::Exit::Canceled);
        println!(
            "{{\"case\":\"stale_kicker\",\"baseline_brk_traps\":{baseline_traps},\"old_kicker_cancels_replacement\":{canceled}}}"
        );
        drop(cpu);
        drop(b);
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("markers") => markers(
            args.get(2).ok_or("marker count")?.parse()?,
            args.iter().any(|s| s == "--count"),
        ),
        #[cfg(unix)]
        Some("serial") => serial_block()?,
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        Some("stale-gic") => mac::stale_gic(),
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        Some("stale-kicker") => mac::stale_kicker(),
        _ => {
            return Err(
                "usage: audit-vmm-controls markers N [--count] | serial | stale-gic | stale-kicker".into(),
            );
        }
    }
    Ok(())
}
