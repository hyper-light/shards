use std::collections::BTreeSet;
use std::ffi::CString;
use std::io::{self, Write};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use shards_testguest::{fill, first_mismatch};

const RO_SALT: u64 = 1;
const RW_SALT: u64 = 2;
const HEAP_SALT: u64 = 3;

pub fn main() {
    let result = setup().and_then(
        |()| match std::env::var("shards_test").unwrap_or_default().as_str() {
            "blk" => blk(),
            "blk_stress" => blk_stress(),
            "snapshot" => snapshot(),
            "storm" => storm(),
            "resume" => resume(),
            "idle" => idle(),
            "walk" => memory_walk(),
            "work" => work(),
            "kmsg" => kmsg(),
            "beat" => beat(),
            "vsock" => vsock(),
            "vsock_snapshot" => vsock_snapshot(),
            "vsock_snapshot_held" => vsock_snapshot_held(),
            "pmem" => pmem(),
            "erofs" => erofs(),
            "layer" => layer(),
            "hotplug" => hotplug(),
            "snapshot-plug" => snapshot_plug(),
            "plug-snapshot" => plug_snapshot(),
            "unplug-cost" => unplug_cost(),
            "memory" => memory_report(),
            "limits" => limits(),
            "pci" => pci(),
            "pci-irq" => pci_irq(),
            "pci-dma" => pci_dma(),
            "host" => host(),
            "daemon" => daemon(),
            other => Err(format!("unknown test {other:?}")),
        },
    );
    let line = match result {
        Ok(()) => "SHARDS-TEST PASS".to_string(),
        Err(e) => format!("SHARDS-TEST FAIL {e}"),
    };
    let _ = writeln!(io::stdout(), "{line}");
    // SAFETY: PID 1 waiting for its console to send what it wrote, flushing and powering
    // off. The console's tty holds a write until its UART has sent it, and the kernel's
    // own lines as it powers off go past it: powering off at once can drop the last line.
    unsafe {
        libc::tcdrain(1);
        libc::sync();
        libc::reboot(libc::RB_POWER_OFF);
    }
}

fn setup() -> Result<(), String> {
    for (src, target, fs) in [
        ("devtmpfs", "/dev", "devtmpfs"),
        ("proc", "/proc", "proc"),
        ("sysfs", "/sys", "sysfs"),
    ] {
        let c = |s: &str| CString::new(s).map_err(|e| e.to_string());
        let (src, target, fs) = (c(src)?, c(target)?, c(fs)?);
        // SAFETY: NUL-terminated strings, no data argument.
        if unsafe { libc::mount(src.as_ptr(), target.as_ptr(), fs.as_ptr(), 0, std::ptr::null()) } != 0 {
            return Err(format!("mount {target:?}: {}", io::Error::last_os_error()));
        }
    }
    Ok(())
}

fn env_u64(name: &str) -> Result<u64, String> {
    std::env::var(name)
        .map_err(|_| format!("{name} not set"))?
        .parse()
        .map_err(|e| format!("{name}: {e}"))
}

/// Waits until `done` is `Ok`, with no deadline of its own (the host bounds the test),
/// reporting every 5 s what is awaited and `done`'s account of where it stands.
fn waiting(what: &str, mut done: impl FnMut() -> Result<(), String>) {
    let mut report = Instant::now() + Duration::from_secs(5);
    while let Err(state) = done() {
        if Instant::now() >= report {
            let _ = writeln!(io::stdout(), "SHARDS-TEST INFO waiting for {what}: {state}");
            report += Duration::from_secs(5);
        }
        thread::sleep(Duration::from_millis(1));
    }
}

fn sysfs(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .map_err(|e| format!("{path}: {e}"))
}

/// A page-aligned buffer for O_DIRECT transfers.
struct Aligned {
    ptr: *mut u8,
    len: usize,
}

impl Aligned {
    fn new(len: usize) -> Result<Aligned, String> {
        // SAFETY: anonymous private mapping owned by the returned value.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(format!("mmap: {}", io::Error::last_os_error()));
        }
        Ok(Aligned { ptr: p.cast(), len })
    }
    fn slice(&mut self, len: usize) -> &mut [u8] {
        // SAFETY: private mapping of `self.len` bytes, exclusively borrowed.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, len.min(self.len)) }
    }
}

// SAFETY: the mapping belongs to this value alone, so it may move to another thread.
unsafe impl Send for Aligned {}

impl Drop for Aligned {
    fn drop(&mut self) {
        // SAFETY: unmapping our own mapping.
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

fn open(path: &str, flags: libc::c_int) -> Result<libc::c_int, String> {
    let p = CString::new(path).map_err(|e| e.to_string())?;
    // SAFETY: NUL-terminated path.
    let fd = unsafe { libc::open(p.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        Err(format!("open {path}: {}", io::Error::last_os_error()))
    } else {
        Ok(fd)
    }
}

fn pread_exact(fd: libc::c_int, buf: &mut [u8], offset: u64) -> Result<(), String> {
    // SAFETY: buf is valid for writes of its length.
    let n = unsafe { libc::pread(fd, buf.as_mut_ptr().cast(), buf.len(), offset as libc::off_t) };
    if n as usize == buf.len() {
        Ok(())
    } else {
        Err(format!(
            "pread {} @{offset}: {n} ({})",
            buf.len(),
            io::Error::last_os_error()
        ))
    }
}

fn pwrite_exact(fd: libc::c_int, buf: &[u8], offset: u64) -> Result<(), String> {
    // SAFETY: buf is valid for reads of its length.
    let n = unsafe { libc::pwrite(fd, buf.as_ptr().cast(), buf.len(), offset as libc::off_t) };
    if n as usize == buf.len() {
        Ok(())
    } else {
        Err(format!(
            "pwrite {} @{offset}: {n} ({})",
            buf.len(),
            io::Error::last_os_error()
        ))
    }
}

fn verify(fd: libc::c_int, salt: u64, offset: u64, len: usize, buf: &mut Aligned) -> Result<(), String> {
    let b = buf.slice(len);
    pread_exact(fd, b, offset)?;
    match first_mismatch(salt, offset, b) {
        None => Ok(()),
        Some(i) => Err(format!("data mismatch at byte {}", offset + i as u64)),
    }
}

/// xorshift64*: deterministic offsets so failures reproduce.
fn next(state: &mut u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_f491_4f6c_dd1d)
}

fn blk() -> Result<(), String> {
    let (ro_bytes, rw_bytes) = (env_u64("shards_vda_bytes")?, env_u64("shards_vdb_bytes")?);
    if sysfs("/sys/block/vda/size")? != (ro_bytes / 512).to_string() {
        return Err(format!(
            "vda size {} != {}",
            sysfs("/sys/block/vda/size")?,
            ro_bytes / 512
        ));
    }
    if sysfs("/sys/block/vda/ro")? != "1" || sysfs("/sys/block/vdb/ro")? != "0" {
        return Err("read-only flags not reported".into());
    }
    if sysfs("/sys/block/vda/serial")? != "shards-disk0" {
        return Err(format!("serial {:?}", sysfs("/sys/block/vda/serial")?));
    }

    let mut buf = Aligned::new(4 << 20)?;
    // Whole read-only disk in 1 MiB O_DIRECT reads.
    let fd = open("/dev/vda", libc::O_RDONLY | libc::O_DIRECT)?;
    let mut off = 0;
    while off < ro_bytes {
        let len = (1 << 20).min(ro_bytes - off) as usize;
        verify(fd, RO_SALT, off, len, &mut buf)?;
        off += len as u64;
    }
    // Random reads at exact request sizes.
    let mut rng = 0x9e37_79b9_7f4a_7c15u64;
    for size in [512usize, 4096, 65536, 1 << 20, 4 << 20] {
        for _ in 0..16 {
            let sectors = (ro_bytes - size as u64) / 512;
            let offset = next(&mut rng) % (sectors + 1) * 512;
            verify(fd, RO_SALT, offset, size, &mut buf)?;
        }
    }
    // SAFETY: closing our descriptor.
    unsafe { libc::close(fd) };
    // Linux lets userspace open a read-only disk for writing but fails the write itself
    // with EPERM (block/fops.c blkdev_write_iter); the host checks the image is untouched.
    let fd = open("/dev/vda", libc::O_RDWR)?;
    let zero = [0u8; 512];
    // SAFETY: buffer valid for 512 bytes.
    let n = unsafe { libc::pwrite(fd, zero.as_ptr().cast(), zero.len(), 0) };
    let err = io::Error::last_os_error().raw_os_error();
    // SAFETY: closing our descriptor.
    unsafe { libc::close(fd) };
    if n >= 0 || err != Some(libc::EPERM) {
        return Err(format!(
            "write to read-only disk returned {n} ({err:?}), expected EPERM"
        ));
    }

    // Write the whole writable disk with mixed request sizes, flush, read back.
    let fd = open("/dev/vdb", libc::O_RDWR | libc::O_DIRECT)?;
    let mut sizes = [4096usize, 512, 65536, 1 << 20, 3 * 512].into_iter().cycle();
    let mut off = 0;
    while off < rw_bytes {
        let len = sizes.next().unwrap_or(512).min((rw_bytes - off) as usize);
        let b = buf.slice(len);
        fill(RW_SALT, off, b);
        pwrite_exact(fd, b, off)?;
        off += len as u64;
    }
    // SAFETY: fsync on our descriptor; issues VIRTIO_BLK_T_FLUSH.
    if unsafe { libc::fsync(fd) } != 0 {
        return Err(format!("fsync: {}", io::Error::last_os_error()));
    }
    let mut off = 0;
    while off < rw_bytes {
        let len = (1 << 20).min(rw_bytes - off) as usize;
        verify(fd, RW_SALT, off, len, &mut buf)?;
        off += len as u64;
    }
    // Beyond the end of the device must fail, not wrap.
    let b = buf.slice(4096);
    // SAFETY: buffer valid for 4096 bytes.
    let n = unsafe { libc::pread(fd, b.as_mut_ptr().cast(), 4096, rw_bytes as libc::off_t) };
    if n != 0 {
        return Err(format!("read past end returned {n}"));
    }
    // SAFETY: closing our descriptor.
    unsafe { libc::close(fd) };
    Ok(())
}

/// Many concurrent O_DIRECT readers: exercises queue depth > 1, request batching and
/// EVENT_IDX interrupt suppression under contention.
fn blk_stress() -> Result<(), String> {
    let ro_bytes = env_u64("shards_vda_bytes")?;
    let seconds = env_u64("shards_seconds").unwrap_or(2);
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let workers: Vec<_> = (0..8u64)
        .map(|t| {
            thread::Builder::new()
                .spawn(move || -> Result<u64, String> {
                    let fd = open("/dev/vda", libc::O_RDONLY | libc::O_DIRECT)?;
                    let mut buf = Aligned::new(256 << 10)?;
                    let mut rng = 0x1234_5678_9abc_def0 ^ (t + 1);
                    let mut ops = 0u64;
                    while Instant::now() < deadline {
                        let size = 512usize << (next(&mut rng) % 10); // 512 B .. 256 KiB
                        let offset = next(&mut rng) % ((ro_bytes - size as u64) / 512 + 1) * 512;
                        verify(fd, RO_SALT, offset, size, &mut buf)?;
                        ops += 1;
                    }
                    // SAFETY: closing our descriptor.
                    unsafe { libc::close(fd) };
                    Ok(ops)
                })
                .map_err(|e| e.to_string())
        })
        .collect::<Result<_, _>>()?;
    let mut total = 0;
    for w in workers {
        total += w.join().map_err(|_| "worker panicked".to_string())??;
    }
    let _ = writeln!(io::stdout(), "SHARDS-TEST INFO ops={total} seconds={seconds}");
    Ok(())
}

/// The VMM's control page, mapped through /dev/mem.
struct ControlPage {
    regs: *mut u32,
}

impl ControlPage {
    const LEN: usize = 4096;

    fn map() -> Result<ControlPage, String> {
        let fd = open("/dev/mem", libc::O_RDWR | libc::O_SYNC)?;
        // SAFETY: maps one page of the control device; the fd is closed after mapping.
        let p = unsafe {
            let p = libc::mmap(
                std::ptr::null_mut(),
                Self::LEN,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                shards_abi::CONTROL_PAGE as libc::off_t,
            );
            libc::close(fd);
            p
        };
        if p == libc::MAP_FAILED {
            return Err(format!(
                "mapping the control page: {}",
                io::Error::last_os_error()
            ));
        }
        Ok(ControlPage { regs: p.cast() })
    }

    fn write(&self, offset: u64, value: u32) {
        // SAFETY: an aligned register inside the mapped page.
        unsafe { self.regs.add(offset as usize / 4).write_volatile(value) };
    }

    fn read(&self, offset: u64) -> u32 {
        // SAFETY: an aligned register inside the mapped page.
        unsafe { self.regs.add(offset as usize / 4).read_volatile() }
    }

    /// A 64-bit register, in one access.
    fn write64(&self, offset: u64, value: u64) {
        // SAFETY: an aligned 64-bit register inside the mapped page.
        unsafe {
            self.regs
                .add(offset as usize / 4)
                .cast::<u64>()
                .write_volatile(value)
        };
    }

    /// A 64-bit register, in one access.
    fn read64(&self, offset: u64) -> u64 {
        // SAFETY: an aligned 64-bit register inside the mapped page.
        unsafe { self.regs.add(offset as usize / 4).cast::<u64>().read_volatile() }
    }
}

impl Drop for ControlPage {
    fn drop(&mut self) {
        // SAFETY: the mapping made in `map`.
        unsafe { libc::munmap(self.regs.cast(), Self::LEN) };
    }
}

fn monotonic_ns() -> u128 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: writes one timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u128 * 1_000_000_000 + ts.tv_nsec as u128
}

/// Builds state worth checking, asks the VMM for a snapshot, and then, in the VM that
/// booted and in every VM restored from it, checks that the state survived and that
/// the machine still works: memory, files, a thread on another CPU, the clock, disks.
fn snapshot() -> Result<(), String> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    let ro_bytes = env_u64("shards_vda_bytes")?;
    let mut heap = vec![0u8; 32 << 20];
    fill(HEAP_SALT, 0, &mut heap);
    std::fs::write("/snapshot-probe", b"written before the snapshot").map_err(|e| e.to_string())?;
    let counter = Arc::new(AtomicU64::new(0));
    let running = Arc::new(AtomicBool::new(true));
    let spinner = {
        let (counter, running) = (counter.clone(), running.clone());
        thread::spawn(move || {
            while running.load(Ordering::Relaxed) {
                counter.fetch_add(1, Ordering::Relaxed);
            }
        })
    };
    while counter.load(Ordering::Relaxed) == 0 {
        thread::yield_now();
    }
    let control = ControlPage::map()?;
    let before = monotonic_ns();

    control.write(shards_abi::control::SNAPSHOT, shards_abi::control::SNAPSHOT_NOW);
    // A restored guest continues here.
    control.write(shards_abi::control::MARKER, shards_abi::marker::RESUMED);
    let generation = control.read(shards_abi::control::GENERATION);
    let after = monotonic_ns();
    let _ = writeln!(io::stdout(), "SHARDS-TEST INFO generation={generation}");
    // A second request is passed by: a VM serves one snapshot, so a clone of this one
    // still begins above, and says generation=1.
    control.write(shards_abi::control::SNAPSHOT, shards_abi::control::SNAPSHOT_NOW);

    if after < before {
        return Err(format!("CLOCK_MONOTONIC went backwards: {before} -> {after}"));
    }
    if let Some(i) = first_mismatch(HEAP_SALT, 0, &heap) {
        return Err(format!("heap differs at byte {i}"));
    }
    let probe = std::fs::read("/snapshot-probe").map_err(|e| e.to_string())?;
    if probe != b"written before the snapshot" {
        return Err("rootfs file changed across the snapshot".into());
    }
    let seen = counter.load(Ordering::Relaxed);
    let deadline = Instant::now() + Duration::from_secs(5);
    while counter.load(Ordering::Relaxed) == seen {
        if Instant::now() > deadline {
            return Err("the second CPU's thread stopped running".into());
        }
        thread::yield_now();
    }
    running.store(false, Ordering::Relaxed);
    spinner
        .join()
        .map_err(|_| "spinner thread panicked".to_string())?;

    let fd = open("/dev/vda", libc::O_RDONLY | libc::O_DIRECT)?;
    let mut buf = Aligned::new(1 << 20)?;
    let len = (1 << 20).min(ro_bytes as usize);
    let checked = verify(fd, RO_SALT, 0, len, &mut buf);
    // SAFETY: closing our own descriptor.
    unsafe { libc::close(fd) };
    checked?;

    let mut entropy = [0u8; 16];
    // SAFETY: getrandom writes at most 16 bytes into `entropy`.
    if unsafe { libc::getrandom(entropy.as_mut_ptr().cast(), 16, 0) } != 16 {
        return Err(format!("getrandom: {}", io::Error::last_os_error()));
    }
    let hex: String = entropy.iter().map(|b| format!("{b:02x}")).collect();
    let _ = writeln!(io::stdout(), "SHARDS-TEST INFO random={hex}");
    Ok(())
}

/// What a storm's threads share.
struct Storm {
    stop: std::sync::atomic::AtomicBool,
    failure: std::sync::Mutex<Option<String>>,
    /// Per worker, the rounds it has finished.
    progress: Vec<std::sync::atomic::AtomicU64>,
    /// Per worker, the longest time between two of its rounds, in µs.
    longest: Vec<std::sync::atomic::AtomicU64>,
    /// Per paired worker, the longest it took to take its turn once the other gave it, in
    /// µs: a wakeup between CPUs, from the notify to the waiter running.
    late: Vec<std::sync::atomic::AtomicU64>,
    /// Records the writer has written, each completed.
    written: std::sync::atomic::AtomicU64,
    /// Where the writer is: 1 reading slot `written`, 2 writing it, 0 between.
    writer_at: std::sync::atomic::AtomicU64,
}

impl Storm {
    fn fail(&self, what: String) {
        let mut f = self
            .failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if f.is_none() {
            *f = Some(what);
        }
    }

    fn stopping(&self) -> bool {
        self.stop.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// One round of a storm's worker; an error stops the storm.
type Round = Box<dyn FnMut(&Storm) -> Result<(), String> + Send>;

const RECORD: usize = 4096;

/// The workers' longest gaps between rounds: the median and the three longest, with their
/// workers, in µs. `reset` starts them over.
fn gaps(storm: &Storm, reset: bool) -> String {
    spread(&storm.longest, reset)
}

/// The median of `values`, and the three largest, by worker; zeroed with `reset`.
fn spread(values: &[std::sync::atomic::AtomicU64], reset: bool) -> String {
    use std::sync::atomic::Ordering;
    let mut all: Vec<(u64, usize)> = values
        .iter()
        .enumerate()
        .map(|(i, l)| {
            (
                if reset {
                    l.swap(0, Ordering::Relaxed)
                } else {
                    l.load(Ordering::Relaxed)
                },
                i,
            )
        })
        .collect();
    all.sort_unstable();
    let median = all.get(all.len() / 2).map_or(0, |g| g.0);
    let top: Vec<String> = all
        .iter()
        .rev()
        .take(3)
        .map(|(g, i)| format!("{g} (worker {i})"))
        .collect();
    format!("median {median}, longest {}", top.join(", "))
}

/// The record the writer puts in slot `k`: its number, then the writable disk's pattern.
fn record(k: u64, buf: &mut [u8]) {
    fill(RW_SALT, k * RECORD as u64, buf);
    for (b, n) in buf.iter_mut().zip(k.to_le_bytes()) {
        *b = n;
    }
}

fn pin(cpu: usize) -> Result<(), String> {
    // SAFETY: a zeroed cpu_set_t is the empty set.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: CPU_SET ignores a CPU past the set's end.
    unsafe { libc::CPU_SET(cpu, &mut set) };
    // SAFETY: sched_setaffinity(2) for this thread, with a set of its size.
    if unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) } != 0 {
        return Err(format!("pinning to CPU {cpu}: {}", io::Error::last_os_error()));
    }
    Ok(())
}

/// Reads CLOCK_MONOTONIC on every CPU for `window`, one read at a time under one lock, so
/// that the reads are ordered in real time, as the kernel orders its own TSC warp check
/// (arch/x86/kernel/tsc_sync.c, check_tsc_warp): a read below the one before it, from
/// whichever CPU, is the clock going backwards between CPUs. How many reads there were,
/// and how far back the worst went, in nanoseconds.
fn clock_warps(cpus: usize, window: Duration) -> Result<(u64, u64), String> {
    warps(cpus, window, monotonic_ns)
}

/// The time-stamp counter of the CPU it runs on.
#[cfg(target_arch = "x86_64")]
fn tsc() -> u128 {
    // SAFETY: RDTSC reads the time-stamp counter, which the guest's user mode may read.
    u128::from(unsafe { core::arch::x86_64::_rdtsc() })
}

/// [`clock_warps`] of any counter `read` gives: how many reads, and the worst step back in
/// its own units.
fn warps(cpus: usize, window: Duration, read: fn() -> u128) -> Result<(u64, u64), String> {
    use std::sync::{Arc, Mutex, PoisonError};
    // The last read, how many there were, and the worst step back.
    let last = Arc::new(Mutex::new((0u128, 0u64, 0u64)));
    // The window opens once every reader is on its CPU: dozens of vCPUs sharing a few
    // host cores take a while to start them all.
    let ready = Arc::new(std::sync::Barrier::new(cpus));
    let readers: Vec<_> = (0..cpus)
        .map(|cpu| {
            let (last, ready) = (last.clone(), ready.clone());
            thread::spawn(move || {
                let pinned = pin(cpu);
                ready.wait();
                pinned?;
                let until = Instant::now() + window;
                while Instant::now() < until {
                    let mut l = last.lock().unwrap_or_else(PoisonError::into_inner);
                    let now = read();
                    if now < l.0 {
                        l.2 = l.2.max(u64::try_from(l.0 - now).unwrap_or(u64::MAX));
                    }
                    l.0 = now;
                    l.1 += 1;
                }
                Ok::<(), String>(())
            })
        })
        .collect();
    for r in readers {
        r.join().map_err(|_| "a clock reader panicked".to_string())??;
    }
    let (_, reads, worst) = *last.lock().unwrap_or_else(PoisonError::into_inner);
    Ok((reads, worst))
}

/// [`clock_warps`], as a verdict: `when` names the moment for the log.
fn clock_in_step(cpus: usize, when: &str) -> Result<(), String> {
    let (reads, worst) = clock_warps(cpus, Duration::from_millis(100))?;
    let _ = writeln!(
        io::stdout(),
        "SHARDS-TEST INFO clock {when}: {reads} reads in turn on {cpus} CPUs, worst step back {worst} ns"
    );
    if worst > 0 {
        // What keeps the guest's time, and whether the CPUs' own counters disagree: a TSC
        // restored out of step with the others, or a clock's parameters out of step.
        let source = sysfs("/sys/devices/system/clocksource/clocksource0/current_clocksource")
            .unwrap_or_else(|e| format!("unread: {e}"));
        let _ = writeln!(io::stdout(), "SHARDS-TEST INFO clocksource {when}: {source}");
        #[cfg(target_arch = "x86_64")]
        if let Ok((reads, back)) = warps(cpus, Duration::from_millis(100), tsc) {
            let _ = writeln!(
                io::stdout(),
                "SHARDS-TEST INFO tsc {when}: {reads} reads in turn on {cpus} CPUs, worst step back {back} cycles"
            );
        }
        return Err(format!(
            "CLOCK_MONOTONIC went back {worst} ns from one CPU to another {when}"
        ));
    }
    Ok(())
}

/// A machine busy in every way at once when it asks for a snapshot (audit A02): pairs of
/// threads on neighbouring CPUs wake each other in turn (interrupts between CPUs), one
/// thread sleeps in short timer ticks, one reads the read-only disk and checks it, one
/// numbers 4 KiB records onto the writable disk, and the vsock echo serves the host, which
/// streams through it. The guest asks for the snapshot once the storm is up and the echo
/// has carried a MiB.
///
/// Whatever continues past the snapshot, the original or a restore, must see every worker
/// keep going: a lost interrupt stalls one. A pair's threads wait for their turns without
/// a timeout, which would wake a CPU whose wakeup was lost and hide the loss; a lost
/// wakeup leaves its waiter asleep, and the progress check sees the stall. How long each
/// took to take its turn is reported: after a restore, the time until every vCPU runs
/// again. The writer reads each slot before it writes it, and finds it empty unless some
/// request of its own was carried out without its completion reaching the guest: every
/// record is written exactly once.
///
/// Its checks done, it says `storm checked`, and gives its verdict once the host has
/// connected to port 1235 and sent a byte: the host stops streaming first, so every round
/// ends with the guest alive.
fn storm() -> Result<(), String> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    let ro_bytes = env_u64("shards_vda_bytes")?;
    let rw_bytes = env_u64("shards_vdb_bytes")?;
    // SAFETY: sysconf(3) takes no pointers.
    let cpus = usize::try_from(unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) }).unwrap_or(1);
    if cpus < 2 {
        return Err(format!("a storm needs two CPUs; this guest has {cpus}"));
    }
    let pairs: Vec<(usize, usize)> = (0..cpus).step_by(2).map(|a| (a, (a + 1) % cpus)).collect();
    // Workers: both threads of each pair, then the timer, the reader and the writer.
    let workers = 2 * pairs.len() + 3;
    let storm = Arc::new(Storm {
        stop: AtomicBool::new(false),
        failure: std::sync::Mutex::new(None),
        progress: (0..workers).map(|_| AtomicU64::new(0)).collect(),
        longest: (0..workers).map(|_| AtomicU64::new(0)).collect(),
        late: (0..2 * pairs.len()).map(|_| AtomicU64::new(0)).collect(),
        written: AtomicU64::new(0),
        writer_at: AtomicU64::new(0),
    });
    let mut threads = Vec::new();
    let mut spawn = |slot: usize, cpu: Option<usize>, name: String, mut round: Round| {
        let storm = storm.clone();
        threads.push(thread::spawn(move || {
            if let Some(cpu) = cpu
                && let Err(e) = pin(cpu)
            {
                return storm.fail(e);
            }
            let mut last = monotonic_ns();
            while !storm.stopping() {
                match round(&storm) {
                    Ok(()) => {
                        let now = monotonic_ns();
                        let gap = u64::try_from(now.saturating_sub(last) / 1000).unwrap_or(u64::MAX);
                        last = now;
                        if let Some(l) = storm.longest.get(slot) {
                            l.fetch_max(gap, Ordering::Relaxed);
                        }
                        if let Some(p) = storm.progress.get(slot) {
                            p.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(e) => return storm.fail(format!("{name}: {e}")),
                }
            }
        }));
    };

    // Whose turn it is in each pair, and since when.
    let mut balls = Vec::new();
    for (i, &(a, b)) in pairs.iter().enumerate() {
        let ball = Arc::new((
            std::sync::Mutex::new((0u8, monotonic_ns())),
            std::sync::Condvar::new(),
        ));
        balls.push(ball.clone());
        for (side, cpu, other) in [(0u8, a, b), (1u8, b, a)] {
            let ball = ball.clone();
            let slot = 2 * i + usize::from(side);
            spawn(
                slot,
                Some(cpu),
                format!("CPU {cpu}'s turn with CPU {other}"),
                Box::new(move |storm| {
                    let (turn, cv) = &*ball;
                    let mut t = turn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    while t.0 != side {
                        if storm.stopping() {
                            return Ok(());
                        }
                        t = cv.wait(t).unwrap_or_else(std::sync::PoisonError::into_inner);
                    }
                    let now = monotonic_ns();
                    if let Some(l) = storm.late.get(slot) {
                        let late = u64::try_from(now.saturating_sub(t.1) / 1000).unwrap_or(u64::MAX);
                        l.fetch_max(late, Ordering::Relaxed);
                    }
                    *t = (1 - side, now);
                    cv.notify_all();
                    Ok(())
                }),
            );
        }
    }
    // Waiters without a timeout hear of the end only this way; the lock orders it after
    // any waiter's look at `stop`.
    let stop = |storm: &Storm| {
        storm.stop.store(true, Ordering::Relaxed);
        for ball in &balls {
            let _held = ball.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            ball.1.notify_all();
        }
    };
    let timer = 2 * pairs.len();
    let mut last = monotonic_ns();
    spawn(
        timer,
        None,
        "the timer".into(),
        Box::new(move |_| {
            thread::sleep(Duration::from_micros(200));
            let now = monotonic_ns();
            if now < last {
                return Err(format!("CLOCK_MONOTONIC went backwards: {last} -> {now}"));
            }
            last = now;
            Ok(())
        }),
    );
    let ro = open("/dev/vda", libc::O_RDONLY | libc::O_DIRECT)?;
    let mut read_buf = Aligned::new(64 << 10)?;
    let mut seed = 0x5eed_u64;
    spawn(
        timer + 1,
        None,
        "the reader".into(),
        Box::new(move |_| {
            let blocks = (ro_bytes / 4096).saturating_sub(16).max(1);
            let offset = next(&mut seed) % blocks * 4096;
            verify(ro, RO_SALT, offset, 64 << 10, &mut read_buf)
        }),
    );
    let rw = open("/dev/vdb", libc::O_RDWR | libc::O_DIRECT)?;
    let mut write_buf = Aligned::new(RECORD)?;
    let slots = rw_bytes / RECORD as u64;
    spawn(
        timer + 2,
        None,
        "the writer".into(),
        Box::new(move |storm| {
            let k = storm.written.load(Ordering::Relaxed);
            if k >= slots {
                // The disk is full: rest, still counting.
                thread::sleep(Duration::from_millis(1));
                return Ok(());
            }
            let b = write_buf.slice(RECORD);
            storm.writer_at.store(1, Ordering::Relaxed);
            pread_exact(rw, b, k * RECORD as u64)?;
            if b.iter().any(|&x| x != 0) {
                return Err(format!(
                    "slot {k} was written already, by a request whose completion never reached the guest"
                ));
            }
            record(k, b);
            storm.writer_at.store(2, Ordering::Relaxed);
            pwrite_exact(rw, b, k * RECORD as u64)?;
            storm.written.store(k + 1, Ordering::Relaxed);
            storm.writer_at.store(0, Ordering::Relaxed);
            Ok(())
        }),
    );
    let listener = vsock_listen(ECHO_PORT)?;
    thread::spawn(move || serve_echo(&listener));
    let done = vsock_listen(DONE_PORT)?;
    let _ = writeln!(io::stdout(), "SHARDS-TEST READY");

    // The snapshot lands in the thick of it: after the storm is up and the host's stream is.
    // No deadline of the guest's own: a busy host may be slow to stream, and the host's
    // wait for the verdict bounds the test. What it waits on is reported as it waits.
    thread::sleep(Duration::from_millis(50));
    waiting("the host's stream through the echo", || {
        (ECHOED.load(Ordering::Relaxed) >= 1 << 20)
            .then_some(())
            .ok_or_else(|| format!("{} bytes echoed", ECHOED.load(Ordering::Relaxed)))
    });
    // The storm in its steady state, for `shards_storm_ms`, with nothing paused.
    gaps(&storm, true);
    thread::sleep(Duration::from_millis(env_u64("shards_storm_ms").unwrap_or(0)));
    let _ = writeln!(
        io::stdout(),
        "SHARDS-TEST INFO longest gaps steady: {}; longest turns taken: {}",
        gaps(&storm, true),
        spread(&storm.late, true)
    );
    clock_in_step(cpus, "before the snapshot")?;
    let control = ControlPage::map()?;
    control.write(shards_abi::control::SNAPSHOT, shards_abi::control::SNAPSHOT_NOW);
    // A restored copy continues here, as does the original, if it resumed.
    let generation = control.read(shards_abi::control::GENERATION);
    let echoed_at = ECHOED.load(Ordering::Relaxed);
    let beats = watch_main_thread(cpus);
    let _ = writeln!(io::stdout(), "SHARDS-TEST INFO generation={generation}");

    let _ = writeln!(
        io::stdout(),
        "SHARDS-TEST INFO longest gaps before: {}; longest turns taken: {}",
        gaps(&storm, true),
        spread(&storm.late, true)
    );
    // Every CPU's clock goes on from the snapshot in step with the others'.
    beats.fetch_add(1, Ordering::Relaxed);
    clock_in_step(cpus, "after the snapshot")?;
    beats.fetch_add(1, Ordering::Relaxed);
    let seen: Vec<u64> = storm.progress.iter().map(|p| p.load(Ordering::Relaxed)).collect();
    // Every worker goes on from the snapshot. A lost completion or timer never ends; a
    // worker a loaded host gave no turn does (one waited 0.98 s on a Mac at a load of 20,
    // and a verdict at 1 s failed the test): only the host's bound tells them apart, so the
    // guest waits, saying which have not gone on and what is in flight, read once a second.
    let mut failed = None;
    let mut said = (Instant::now(), String::new());
    waiting("every worker's progress after the snapshot", || {
        beats.fetch_add(1, Ordering::Relaxed);
        if let Some(e) = storm
            .failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            failed = Some(e);
            return Ok(());
        }
        let still: Vec<usize> = storm
            .progress
            .iter()
            .zip(&seen)
            .enumerate()
            .filter(|(_, (p, before))| p.load(Ordering::Relaxed) == **before)
            .map(|(i, _)| i)
            .collect();
        if still.is_empty() {
            return Ok(());
        }
        if said.1.is_empty() || said.0.elapsed() >= Duration::from_secs(1) {
            let at = match storm.writer_at.load(Ordering::Relaxed) {
                1 => "reading",
                2 => "writing",
                _ => "between requests",
            };
            let interrupts = std::fs::read_to_string("/proc/interrupts").unwrap_or_default();
            let virtio: Vec<&str> = interrupts.lines().filter(|l| l.contains("virtio")).collect();
            said = (
                Instant::now(),
                format!(
                    "longest gaps since: {}; the writer is {at} slot {}; in flight (reads writes): vda {}, vdb {}; {}",
                    gaps(&storm, false),
                    storm.written.load(Ordering::Relaxed),
                    sysfs("/sys/block/vda/inflight").unwrap_or_default(),
                    sysfs("/sys/block/vdb/inflight").unwrap_or_default(),
                    virtio.join(" | ")
                ),
            );
        }
        Err(format!("workers {still:?} of {workers} have not; {}", said.1))
    });
    if let Some(e) = failed {
        return Err(e);
    }
    stop(&storm);
    // A lost completion never ends, a slow one does: only the host's bound tells them
    // apart, so the guest waits, saying which workers are still in their rounds.
    waiting("the workers' last rounds", || {
        beats.fetch_add(1, Ordering::Relaxed);
        let left: Vec<usize> = threads
            .iter()
            .enumerate()
            .filter(|(_, t)| !t.is_finished())
            .map(|(i, _)| i)
            .collect();
        if left.is_empty() {
            return Ok(());
        }
        Err(format!(
            "workers {left:?} still in theirs; in flight (reads writes): vda {}, vdb {}",
            sysfs("/sys/block/vda/inflight").unwrap_or_default(),
            sysfs("/sys/block/vdb/inflight").unwrap_or_default()
        ))
    });
    if let Some(e) = storm
        .failure
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    {
        return Err(e);
    }
    // Exactly the records the writer completed, each whole, and nothing after them.
    beats.fetch_add(1, Ordering::Relaxed);
    let written = storm.written.load(Ordering::Relaxed);
    let mut got = Aligned::new(RECORD)?;
    let mut want = vec![0u8; RECORD];
    for k in 0..written.saturating_add(1).min(slots) {
        let b = got.slice(RECORD);
        pread_exact(rw, b, k * RECORD as u64)?;
        if k < written {
            record(k, &mut want);
            if b != want.as_slice() {
                return Err(format!("record {k} of {written} is not what the writer wrote"));
            }
        } else if b.iter().any(|&x| x != 0) {
            return Err(format!("slot {k}, after the {written} written, holds data"));
        }
    }
    let _ = writeln!(io::stdout(), "SHARDS-TEST INFO records={written}");
    let _ = writeln!(
        io::stdout(),
        "SHARDS-TEST INFO longest gaps after: {}; longest turns taken: {}",
        gaps(&storm, false),
        spread(&storm.late, false)
    );
    // A machine that went on carries the host's stream on past its snapshot: two rounds'
    // worth (the host's rounds are 4 MiB), so the round the snapshot cut through ends and
    // another runs whole after it, however slowly a loaded host streams.
    if generation == 0 {
        let deadline = Instant::now() + Duration::from_secs(60);
        while ECHOED.load(Ordering::Relaxed).saturating_sub(echoed_at) < 8 << 20 {
            beats.fetch_add(1, Ordering::Relaxed);
            if Instant::now() > deadline {
                return Err(format!(
                    "the host streamed {} bytes after the snapshot in 60 s",
                    ECHOED.load(Ordering::Relaxed).saturating_sub(echoed_at)
                ));
            }
            thread::sleep(Duration::from_millis(1));
        }
    }
    // The verdict waits for the host to say it has stopped streaming, so every round the
    // host streamed ended with the guest alive: its main thread watched no more.
    beats.store(u64::MAX, Ordering::Relaxed);
    let _ = writeln!(io::stdout(), "SHARDS-TEST INFO storm checked");
    // The host's side of the connection is up only once it has read its OK, after this
    // side's accept: its byte, or its close, says so.
    let mut host = accept_within(&done, Duration::from_secs(60))?;
    let mut byte = [0u8; 1];
    std::io::Read::read(&mut host, &mut byte)
        .map(drop)
        .map_err(|e| format!("waiting for the host's word: {e}"))
}

/// Watches the storm's main thread from its snapshot on, by the count it adds to at each
/// step and each look at what it waits for: once the count has stood still 15 s, has the
/// kernel say what it can of it ([`stood_still`]). What stops the main thread past the
/// snapshot stops what it says too, a lost timer and a stuck console alike; printk's
/// console polls the UART, needing none of its interrupts, so the kernel's word reaches
/// the host either way. Four watchers, each pinned to its own of the guest's `cpus`, spread
/// across them: a CPU whose timer stopped stops the one there, whose sleeps never end, and
/// not the rest. The first to see it says it, once; none does after the count is
/// `u64::MAX`, which ends the watch.
fn watch_main_thread(cpus: usize) -> std::sync::Arc<std::sync::atomic::AtomicU64> {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    let beats = std::sync::Arc::new(AtomicU64::new(0));
    let said = std::sync::Arc::new(AtomicBool::new(false));
    let watchers = cpus.clamp(1, 4);
    for w in 0..watchers {
        let (watched, said) = (beats.clone(), said.clone());
        thread::spawn(move || {
            if pin(w * cpus / watchers).is_err() {
                return;
            }
            let (mut last, mut since) = (0, Instant::now());
            loop {
                thread::sleep(Duration::from_secs(1));
                let now = watched.load(Ordering::Relaxed);
                if now == u64::MAX || said.load(Ordering::Relaxed) {
                    return;
                }
                if now != last {
                    (last, since) = (now, Instant::now());
                } else if since.elapsed() >= Duration::from_secs(15) {
                    if !said.swap(true, Ordering::Relaxed) {
                        stood_still(now);
                    }
                    return;
                }
            }
        });
    }
    beats
}

/// What the kernel can say of a main thread that stood still ([`watch_main_thread`]), on
/// its console at a log level that shows it: what it logged that the console's level kept
/// off it (notices and worse), the CPU the main thread last ran on, the CPUs whose local
/// timer took no interrupt in a second, each CPU's timers and clock event device (sysrq
/// `q`), and every task's stack (sysrq `t`).
fn stood_still(beat: u64) {
    let say = |line: &str| {
        let _ = std::fs::write("/dev/kmsg", format!("storm: {line}\n"));
    };
    let _ = std::fs::write("/proc/sys/kernel/printk", "7");
    let logged = kernel_log(5);
    // SAFETY: sched_getcpu(3) takes no arguments.
    let here = unsafe { libc::sched_getcpu() };
    say(&format!(
        "the main thread stood still 15 s at beat {beat} (seen from CPU {here})"
    ));
    for line in logged {
        say(&format!("log: {line}"));
    }
    // /proc/PID/stat's field 39, the CPU it last ran on: the 37th after its command's ")".
    let cpu = std::fs::read_to_string("/proc/1/stat")
        .ok()
        .and_then(|s| {
            s.rsplit(')')
                .next()
                .and_then(|rest| rest.split_whitespace().nth(36))
                .map(str::to_string)
        })
        .unwrap_or_default();
    say(&format!("the main thread last ran on CPU {cpu}"));
    if let Some(before) = local_timer_interrupts() {
        thread::sleep(Duration::from_secs(1));
        if let Some(after) = local_timer_interrupts() {
            let none: Vec<String> = before
                .iter()
                .zip(&after)
                .enumerate()
                .filter(|(_, (b, a))| a <= b)
                .map(|(cpu, _)| cpu.to_string())
                .collect();
            say(&format!(
                "CPUs whose local timer took no interrupt in 1 s: [{}]",
                none.join(" ")
            ));
        }
    }
    say("each CPU's timers follow, then every task's stack");
    let _ = std::fs::write("/proc/sysrq-trigger", "q");
    let _ = std::fs::write("/proc/sysrq-trigger", "t");
}

/// Each CPU's count of local timer interrupts, /proc/interrupts' `LOC` line.
fn local_timer_interrupts() -> Option<Vec<u64>> {
    let text = std::fs::read_to_string("/proc/interrupts").ok()?;
    let cpus = text.lines().next()?.split_whitespace().count();
    let line = text.lines().find_map(|l| l.trim_start().strip_prefix("LOC:"))?;
    Some(
        line.split_whitespace()
            .take(cpus)
            .map(|n| n.parse().unwrap_or(0))
            .collect(),
    )
}

/// The messages the kernel has logged at `at_most` or more urgent (syslog levels: 5 is a
/// notice), read from /dev/kmsg's start without waiting for more.
fn kernel_log(at_most: u32) -> Vec<String> {
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let Ok(mut kmsg) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open("/dev/kmsg")
    else {
        return Vec::new();
    };
    let mut said = Vec::new();
    let mut buf = vec![0u8; 8192];
    loop {
        match kmsg.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                // "PRIORITY,SEQUENCE,MICROSECONDS,FLAGS;MESSAGE": the level is the
                // priority's low three bits.
                let record = String::from_utf8_lossy(buf.get(..n).unwrap_or_default()).into_owned();
                let Some((head, message)) = record.split_once(';') else {
                    continue;
                };
                let level = head
                    .split(',')
                    .next()
                    .and_then(|p| p.parse::<u32>().ok())
                    .map(|p| p & 7);
                if level.is_some_and(|l| l <= at_most) {
                    said.push(message.lines().next().unwrap_or_default().to_string());
                }
            }
            // Records overwritten before they were read: the next are there.
            Err(e) if e.raw_os_error() == Some(libc::EPIPE) => {}
            Err(_) => break,
        }
    }
    said
}

/// The next connection to `listener`, or an error once `limit` passes without one.
fn accept_within(listener: &std::fs::File, limit: Duration) -> Result<std::fs::File, String> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let wait = libc::timeval {
        tv_sec: limit.as_secs().try_into().unwrap_or(60),
        tv_usec: 0,
    };
    // SAFETY: setsockopt(2) on our own socket, with a timeval of its size: accept(2) gives
    // up after it.
    let set = unsafe {
        libc::setsockopt(
            listener.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            (&raw const wait).cast(),
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        )
    };
    if set != 0 {
        return Err(format!("SO_RCVTIMEO: {}", io::Error::last_os_error()));
    }
    // SAFETY: accept(2) on our listening socket, without the peer address.
    let fd = unsafe {
        libc::accept4(
            listener.as_raw_fd(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            libc::SOCK_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(format!(
            "no host connection in {limit:?}: {}",
            io::Error::last_os_error()
        ));
    }
    // SAFETY: a fresh descriptor nothing else owns.
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

/// The benchmark guest: asks for a snapshot; a restored clone marks that it runs again
/// and powers off at once, so restore timings contain no guest work.
fn resume() -> Result<(), String> {
    let control = ControlPage::map()?;
    control.write(shards_abi::control::SNAPSHOT, shards_abi::control::SNAPSHOT_NOW);
    control.write(shards_abi::control::MARKER, shards_abi::marker::RESUMED);
    Ok(())
}

/// Prints the kernel's log, each record as /dev/kmsg gives it (`level,seq,usecs,flags;text`,
/// Documentation/ABI/testing/dev-kmsg): with `quiet`, a boot's log costs it no console
/// output, and is read only once the boot has been measured.
fn kmsg() -> Result<(), String> {
    let fd = open("/dev/kmsg", libc::O_RDONLY | libc::O_NONBLOCK)?;
    let mut record = vec![0u8; 8192];
    let mut out = io::stdout().lock();
    loop {
        // SAFETY: `record` is valid for writes of its length; each read is one record.
        let n = unsafe { libc::read(fd, record.as_mut_ptr().cast(), record.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::EAGAIN) => break,
                // A record overwritten before it was read.
                Some(libc::EPIPE) => continue,
                _ => return Err(format!("reading /dev/kmsg: {e}")),
            }
        }
        let _ = out.write_all(record.get(..n as usize).unwrap_or_default());
    }
    // SAFETY: closing our own descriptor.
    unsafe { libc::close(fd) };
    Ok(())
}

/// A fixed amount of work: `shards_work_mib` MiB of the test pattern hashed, on the boot
/// CPU, then the hash printed. Its host CPU time, beside a run with none, is what the work
/// cost, ticks included (PM M66).
fn work() -> Result<(), String> {
    let mib: u64 = std::env::var("shards_work_mib")
        .unwrap_or_else(|_| "0".into())
        .parse()
        .map_err(|e| format!("shards_work_mib: {e}"))?;
    let hash = shards_testguest::pattern_hash(1, mib << 20);
    let _ = writeln!(io::stdout(), "SHARDS-TEST HASH {hash:016x}");
    Ok(())
}

/// A PCI bus's test function (D153): QEMU edu's IDs at 0000:00:01.0, and through its BAR
/// 0, mapped from sysfs as a driver in user space maps one, its identification register
/// and its liveness check, which reads back the inverse of what was written. With its
/// memory space disabled through its configuration space, the function decodes nothing,
/// so a read of BAR 0 is all ones, as an access no function claims (PCI 3.0 §6.2.2); enabled
/// again, BAR 0 reads as before. Passed through, its BAR leaves the guest's memory and
/// comes back (D154). Its BAR 4, 64-bit, lies past 4 GiB, its first page a scratch that
/// reads back what was written.
fn pci() -> Result<(), String> {
    const DIR: &str = "/sys/bus/pci/devices/0000:00:01.0";
    let read = |f: &str| {
        std::fs::read_to_string(format!("{DIR}/{f}"))
            .map(|s| s.trim().to_string())
            .map_err(|e| format!("{DIR}/{f}: {e}"))
    };
    let (vendor, device) = (read("vendor")?, read("device")?);
    if (vendor.as_str(), device.as_str()) != ("0x1234", "0x11e8") {
        return Err(format!("00:01.0 is {vendor}:{device}"));
    }
    std::fs::write(format!("{DIR}/enable"), "1").map_err(|e| format!("{DIR}/enable: {e}"))?;
    let bar = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!("{DIR}/resource0"))
        .map_err(|e| format!("{DIR}/resource0: {e}"))?;
    use std::os::fd::AsRawFd as _;
    // SAFETY: a fresh shared mapping of the BAR's first page, which nothing else maps.
    let regs = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            bar.as_raw_fd(),
            0,
        )
    };
    if regs == libc::MAP_FAILED {
        return Err(format!("mapping BAR 0: {}", io::Error::last_os_error()));
    }
    let regs = regs.cast::<u32>();
    // SAFETY: volatile accesses of the mapping's first two registers, a device's.
    let (identification, live) = unsafe {
        let identification = regs.read_volatile();
        regs.add(1).write_volatile(0x1234_5678);
        (identification, regs.add(1).read_volatile())
    };
    let decoding = |on: bool| -> Result<u32, String> {
        let config = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("{DIR}/config"))
            .map_err(|e| format!("{DIR}/config: {e}"))?;
        let mut command = [0u8; 2];
        pread_exact(config.as_raw_fd(), &mut command, 4)?;
        let command = u16::from_le_bytes(command);
        let command = if on { command | 2 } else { command & !2 };
        pwrite_exact(config.as_raw_fd(), &command.to_le_bytes(), 4)?;
        // SAFETY: a volatile read of the mapping's first register.
        Ok(unsafe { regs.read_volatile() })
    };
    let (off, on) = (decoding(false), decoding(true));
    // SAFETY: the mapping made above.
    unsafe { libc::munmap(regs.cast(), 4096) };
    if identification != 0x0100_00ed || live != !0x1234_5678 {
        return Err(format!(
            "BAR 0 said identification {identification:#x}, liveness {live:#x}"
        ));
    }
    let (off, on) = (off?, on?);
    if off != u32::MAX || on != 0x0100_00ed {
        return Err(format!(
            "BAR 0 read {off:#x} with memory space off, {on:#x} with it on again"
        ));
    }
    // BAR 4, 64-bit, in the bus's 64-bit window past 4 GiB (D154): its first page reads
    // back what was written to it.
    let resources = read("resource")?;
    let bar4 = resources
        .lines()
        .nth(4)
        .and_then(|l| l.split_whitespace().next())
        .and_then(|a| u64::from_str_radix(a.trim_start_matches("0x"), 16).ok())
        .ok_or_else(|| format!("{DIR}/resource: no BAR 4 in {resources:?}"))?;
    if bar4 < 1 << 32 {
        return Err(format!("BAR 4 at {bar4:#x}, under 4 GiB"));
    }
    let bar = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!("{DIR}/resource4"))
        .map_err(|e| format!("{DIR}/resource4: {e}"))?;
    // SAFETY: a fresh shared mapping of BAR 4's first page, which nothing else maps.
    let scratch = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            bar.as_raw_fd(),
            0,
        )
    };
    if scratch == libc::MAP_FAILED {
        return Err(format!("mapping BAR 4: {}", io::Error::last_os_error()));
    }
    let scratch = scratch.cast::<u32>();
    // SAFETY: volatile accesses of the mapping's registers, in its first page.
    let (first, last) = unsafe {
        scratch.write_volatile(0x5ca7_c400);
        scratch.add(1023).write_volatile(0x0dd_ba11);
        (scratch.read_volatile(), scratch.add(1023).read_volatile())
    };
    // SAFETY: the mapping made above.
    unsafe { libc::munmap(scratch.cast(), 4096) };
    if (first, last) != (0x5ca7_c400, 0x0dd_ba11) {
        return Err(format!("BAR 4 read back {first:#x} and {last:#x}"));
    }
    Ok(())
}

/// The test function's interrupts (D154): bound to vfio-pci in a no-IOMMU group (the
/// kernel's `vfio.enable_unsafe_noiommu_mode=1`), it raises INTA# through its BAR's raise
/// register, which VFIO signals on an eventfd, twice, the second after acknowledging and
/// unmasking the first; then, INTx's eventfd taken away, one MSI; then MSI-X, raise bit k
/// sending vector k. Its two vectors start with no eventfd, so the kernel leaves them
/// masked: a raise of vector 0 is held, its pending bit set, and sent as VFIO is given
/// its eventfd, which unmasks it; vector 1 is sent at once, and again held, masked as
/// VFIO frees it, and sent as it is given its eventfd again.
fn pci_irq() -> Result<(), String> {
    const NAME: &str = "0000:00:01.0";
    let (device, _) = vfio::take(NAME)?;
    let bar = vfio::region(device, 0)?;
    let raise = |value: u32| pwrite_exact(device, &value.to_le_bytes(), bar + 0x60);
    let ack = |value: u32| pwrite_exact(device, &value.to_le_bytes(), bar + 0x64);
    let status = || {
        let mut b = [0u8; 4];
        pread_exact(device, &mut b, bar + 0x24).map(|()| u32::from_le_bytes(b))
    };
    let intx = vfio::eventfd()?;
    vfio::trigger(device, vfio::INTX, &[intx]).map_err(|e| format!("INTx on: {e}"))?;
    for round in 1..=2u32 {
        raise(round)?;
        vfio::wait(intx).map_err(|e| format!("INTx {round}: {e}"))?;
        if status()? != round {
            return Err(format!("INTx {round}: status {:#x}", status()?));
        }
        ack(round)?;
        vfio::unmask_intx(device).map_err(|e| format!("INTx unmasked: {e}"))?;
    }
    vfio::trigger(device, vfio::INTX, &[]).map_err(|e| format!("INTx off: {e}"))?;
    let msi = vfio::eventfd()?;
    vfio::trigger(device, vfio::MSI, &[msi]).map_err(|e| format!("MSI on: {e}"))?;
    raise(4)?;
    vfio::wait(msi).map_err(|e| format!("MSI: {e}"))?;
    ack(4)?;
    vfio::trigger(device, vfio::MSI, &[])?;
    // MSI-X: its table and pending bits in BAR 2, the pending bits at 0x800.
    let pba = vfio::region(device, 2)? + 0x800;
    let pending = |vector: u32| {
        let mut b = [0u8; 4];
        pread_exact(device, &mut b, pba).map(|()| u32::from_le_bytes(b) & (1 << vector) != 0)
    };
    // Held while masked: its pending bit set as the host has the raise (VFIO's host
    // takes it as an interrupt first, where the function is passed through again).
    let held = |vector: u32| -> Result<(), String> {
        raise(1 << vector)?;
        let start = std::time::Instant::now();
        while !pending(vector)? {
            if start.elapsed() > std::time::Duration::from_secs(10) {
                return Err(format!("MSI-X vector {vector}: masked, nothing pending in 10 s"));
            }
            std::thread::yield_now();
        }
        Ok(())
    };
    let vectors = [vfio::eventfd()?, vfio::eventfd()?];
    vfio::msix(device, 0, &[-1, -1]).map_err(|e| format!("MSI-X on: {e}"))?;
    held(0)?;
    vfio::msix(device, 0, &vectors[..1])?;
    vfio::wait(vectors[0]).map_err(|e| format!("MSI-X vector 0, held: {e}"))?;
    if pending(0)? {
        return Err("MSI-X vector 0: still pending, sent".into());
    }
    vfio::msix(device, 1, &vectors[1..])?;
    raise(2)?;
    vfio::wait(vectors[1]).map_err(|e| format!("MSI-X vector 1: {e}"))?;
    vfio::msix(device, 1, &[-1])?;
    held(1)?;
    vfio::msix(device, 1, &vectors[1..])?;
    vfio::wait(vectors[1]).map_err(|e| format!("MSI-X vector 1, held: {e}"))?;
    ack(3)?;
    vfio::trigger(device, vfio::MSIX, &[])?;
    let _ = writeln!(
        io::stdout(),
        "SHARDS-TEST INFO interrupts: INTx 2, MSI 1, MSI-X 3, 2 of them held"
    );
    Ok(())
}

/// The test function's DMA (D154): taken through vfio-pci, its DMA engine (edu's: source,
/// destination, length and command at 0x80 of BAR 0, its buffer at 0x40000 of its DMA
/// addresses) moves a page of a pattern from memory into its buffer, and back to another
/// page. Where the function's group has an IOMMU, the pages are mapped for it at I/O
/// addresses of the test's own (VFIO_IOMMU_MAP_DMA), and a transfer to one unmapped moves
/// nothing; where it has none, the function is given the pages' guest-physical addresses
/// (/proc/self/pagemap).
fn pci_dma() -> Result<(), String> {
    const NAME: &str = "0000:00:01.0";
    let (device, iommu) = vfio::take(NAME)?;
    let map = iommu.map(|c| move |vaddr, iova, len| vfio::map_dma(c, vaddr, iova, len));
    let through = edu_dma(device, map.as_ref().map(|m| m as &Map), 0x10_0000)?;
    let _ = writeln!(io::stdout(), "{}", dma_line(through));
    Ok(())
}

/// What a test of the function's DMA says it moved, and through what.
fn dma_line(through: &str) -> String {
    format!("SHARDS-TEST INFO dma: 4096 bytes into the function and back, through {through}")
}

/// A workload's DMA through each function at `names` it was given by their group,
/// `--device /dev/vfio/N` (D154): their group found as DPDK finds a
/// function's, by its address in sysfs (/sys/bus/pci/devices/NAME/iommu_group; DPDK's
/// rte_vfio_get_group_num), its node the one at that number in `/dev/vfio`, which the
/// workload must have been given there; the functions bound to vfio-pci already, as a
/// host's are. One container, the group's, maps each function's pages at I/O addresses of
/// its own.
pub(crate) fn vfio_dma(names: &[String]) -> Result<(), String> {
    let first = names.first().map(String::as_str).ok_or("no function named")?;
    let group = vfio::group_of(first)?;
    let (container, fd) = vfio::open(first)?;
    for (k, name) in (0u64..).zip(names) {
        let theirs = vfio::group_of(name)?;
        if theirs != group {
            return Err(format!("{name} is in group {theirs}, not {first}'s {group}"));
        }
        let device = vfio::device(fd, name)?;
        let map = container.map(|c| move |vaddr, iova, len| vfio::map_dma(c, vaddr, iova, len));
        let through = edu_dma(
            device,
            map.as_ref().map(|m| m as &Map),
            0x10_0000 + k * 0x100_0000,
        )?;
        let _ = writeln!(io::stdout(), "{}", dma_line(through));
    }
    Ok(())
}

/// A workload's DMA through each function at `names` it was given by its VFIO device
/// node and iommufd (`/dev/vfio/devices/vfioN`, `/dev/iommu`; D154): each node found by
/// its address in sysfs (vfio-dev), all of them bound to one iommufd and attached to one
/// I/O address space, as their group's must be, which maps each one's pages at addresses
/// of its own (vfio.rst, "Device cdev Example").
pub(crate) fn vfio_dma_cdev(names: &[String]) -> Result<(), String> {
    let iommufd = open("/dev/iommu", libc::O_RDWR)?;
    let mut ioas = None;
    for (k, name) in (0u64..).zip(names) {
        let node = vfio::cdev_of(name)?;
        let device = open(&node, libc::O_RDWR)?;
        vfio::bind_iommufd(device, iommufd)?;
        let id = match ioas {
            Some(id) => id,
            None => *ioas.insert(vfio::ioas_alloc(iommufd)?),
        };
        vfio::attach(device, id)?;
        let map = move |vaddr, iova, len| vfio::ioas_map(iommufd, id, vaddr, iova, len);
        let through = edu_dma(device, Some(&map), 0x10_0000 + k * 0x100_0000)?;
        let _ = writeln!(io::stdout(), "{}", dma_line(through));
    }
    Ok(())
}

/// How a page of this process is mapped for a function's DMA: at an I/O address, `len`
/// bytes at `vaddr`.
type Map = dyn Fn(u64, u64, u64) -> Result<(), String>;

/// edu's DMA engine at `device`, its pages mapped by `map` from I/O address `base` where
/// its group has an IOMMU, else given it by their guest-physical addresses: a page into its
/// buffer and back, through what it was reached by.
fn edu_dma(device: libc::c_int, map: Option<&Map>, base: u64) -> Result<&'static str, String> {
    const PAGE: usize = 4096;
    const BUFFER: u64 = 0x40000;
    const RUN: u64 = 1;
    const TO_MEMORY: u64 = 2;
    let bar = vfio::region(device, 0)?;
    let page = || -> Result<*mut u8, String> {
        // SAFETY: a fresh anonymous mapping of a page, locked in memory: its address is the
        // function's for as long as the test runs.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                PAGE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_LOCKED | libc::MAP_POPULATE,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(format!("mmap: {}", io::Error::last_os_error()));
        }
        Ok(p.cast())
    };
    let (from, to) = (page()?, page()?);
    // Where the function reaches each page: an I/O address mapped for it, or its own.
    let addresses = match map {
        Some(map) => {
            map(from as u64, base, PAGE as u64)?;
            map(to as u64, base + 0x10_0000, PAGE as u64)?;
            (base, base + 0x10_0000)
        }
        None => (physical(from)?, physical(to)?),
    };
    for i in 0..PAGE {
        // SAFETY: within the page mapped above.
        unsafe { from.add(i).write_volatile((i * 7 + 3) as u8) };
    }
    let transfer = |source: u64, destination: u64, command: u64| -> Result<(), String> {
        for (offset, value) in [
            (0x80, source),
            (0x88, destination),
            (0x90, PAGE as u64),
            (0x98, command),
        ] {
            pwrite_exact(device, &value.to_le_bytes(), bar + offset)?;
        }
        let start = std::time::Instant::now();
        loop {
            let mut c = [0u8; 8];
            pread_exact(device, &mut c, bar + 0x98)?;
            if u64::from_le_bytes(c) & RUN == 0 {
                return Ok(());
            }
            if start.elapsed() > std::time::Duration::from_secs(10) {
                return Err("a DMA still running after 10 s".into());
            }
            std::thread::yield_now();
        }
    };
    transfer(addresses.0, BUFFER, RUN)?;
    transfer(BUFFER, addresses.1, RUN | TO_MEMORY)?;
    for i in 0..PAGE {
        // SAFETY: within the page mapped above.
        let b = unsafe { to.add(i).read_volatile() };
        if b != (i * 7 + 3) as u8 {
            return Err(format!("byte {i} came back {b:#x}"));
        }
    }
    if map.is_some() {
        // To an I/O address nothing is mapped at: nothing moves, and the transfer ends.
        transfer(BUFFER, base + 0x20_0000, RUN | TO_MEMORY)?;
        Ok("its IOMMU")
    } else {
        Ok("guest-physical addresses")
    }
}

/// The guest-physical address of the page at `virt`, resident and locked: its frame in
/// /proc/self/pagemap (Documentation/admin-guide/mm/pagemap.rst: bits 0-54, bit 63
/// present).
fn physical(virt: *mut u8) -> Result<u64, String> {
    let fd = open("/proc/self/pagemap", libc::O_RDONLY)?;
    let mut entry = [0u8; 8];
    let read = pread_exact(fd, &mut entry, (virt as u64 / 4096) * 8);
    // SAFETY: closing our own descriptor.
    unsafe { libc::close(fd) };
    read?;
    let entry = u64::from_le_bytes(entry);
    if entry & (1 << 63) == 0 || entry & ((1 << 55) - 1) == 0 {
        return Err(format!("page {virt:?}: no frame in pagemap ({entry:#x})"));
    }
    Ok((entry & ((1 << 55) - 1)) * 4096)
}

/// The VFIO a test guest makes of its PCI function (include/uapi/linux/vfio.h).
mod vfio {
    use std::io;

    const fn io(n: u64) -> u64 {
        ((b';' as u64) << 8) | (100 + n)
    }
    const GROUP_SET_CONTAINER: u64 = io(4);
    const SET_IOMMU: u64 = io(2);
    const GROUP_GET_DEVICE_FD: u64 = io(6);
    const DEVICE_GET_REGION_INFO: u64 = io(8);
    const DEVICE_SET_IRQS: u64 = io(10);
    const NOIOMMU_IOMMU: u64 = 8;
    const TYPE1V2_IOMMU: u64 = 3;
    const IOMMU_MAP_DMA: u64 = io(13);
    const DEVICE_BIND_IOMMUFD: u64 = io(18);
    const DEVICE_ATTACH_IOMMUFD_PT: u64 = io(19);
    /// iommufd's (iommufd.h): `_IO(';', n)`.
    const IOAS_ALLOC: u64 = ((b';' as u64) << 8) | 0x81;
    const IOAS_MAP: u64 = ((b';' as u64) << 8) | 0x85;
    pub const INTX: u32 = 0;
    pub const MSI: u32 = 1;
    pub const MSIX: u32 = 2;
    const DATA_NONE: u32 = 1 << 0;
    const DATA_EVENTFD: u32 = 1 << 2;
    const ACTION_UNMASK: u32 = 1 << 4;
    const ACTION_TRIGGER: u32 = 1 << 5;

    fn ioctl(fd: libc::c_int, request: u64, arg: usize) -> Result<libc::c_int, String> {
        // SAFETY: each caller passes what its request reads and writes.
        let r = unsafe { libc::ioctl(fd, request as _, arg) };
        if r < 0 {
            return Err(format!("ioctl {request:#x}: {}", io::Error::last_os_error()));
        }
        Ok(r)
    }

    /// The PCI function `name`, bound to vfio-pci, and the container of its group where
    /// the group has an IOMMU (type 1), none where it has not (no-IOMMU).
    pub fn take(name: &str) -> Result<(libc::c_int, Option<libc::c_int>), String> {
        bind(name)?;
        open_group(name)
    }

    /// Binds the PCI function `name` to vfio-pci.
    pub fn bind(name: &str) -> Result<(), String> {
        let dir = format!("/sys/bus/pci/devices/{name}");
        std::fs::write(format!("{dir}/driver_override"), "vfio-pci")
            .map_err(|e| format!("{dir}/driver_override: {e}"))?;
        std::fs::write("/sys/bus/pci/drivers_probe", name).map_err(|e| format!("binding {name}: {e}"))
    }

    /// The number of the IOMMU group of the PCI function `name`, as sysfs gives it.
    pub fn group_of(name: &str) -> Result<String, String> {
        let dir = format!("/sys/bus/pci/devices/{name}");
        let group = std::fs::read_link(format!("{dir}/iommu_group"))
            .map_err(|e| format!("{dir}/iommu_group: {e}"))?;
        group
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
            .ok_or_else(|| "an IOMMU group without a number".into())
    }

    /// The PCI function `name`, bound to vfio-pci already, through its group: and the
    /// group's container where it has an IOMMU (type 1), none where it has not.
    pub fn open_group(name: &str) -> Result<(libc::c_int, Option<libc::c_int>), String> {
        let (container, group) = open(name)?;
        Ok((device(group, name)?, container))
    }

    /// The group of the PCI function `name`, bound to vfio-pci already, set in a container
    /// of its own: the container where the group has an IOMMU (type 1), none where it has
    /// not, and the group.
    pub fn open(name: &str) -> Result<(Option<libc::c_int>, libc::c_int), String> {
        let group = group_of(name)?;
        let plain = format!("/dev/vfio/{group}");
        let (path, iommu) = if std::path::Path::new(&plain).exists() {
            (plain, TYPE1V2_IOMMU)
        } else {
            (format!("/dev/vfio/noiommu-{group}"), NOIOMMU_IOMMU)
        };
        let container = super::open("/dev/vfio/vfio", libc::O_RDWR)?;
        let group = super::open(&path, libc::O_RDWR)?;
        ioctl(group, GROUP_SET_CONTAINER, (&raw const container) as usize)?;
        ioctl(container, SET_IOMMU, iommu as usize)?;
        Ok(((iommu == TYPE1V2_IOMMU).then_some(container), group))
    }

    /// The device of the group `group`'s function `name`.
    pub fn device(group: libc::c_int, name: &str) -> Result<libc::c_int, String> {
        let cname = std::ffi::CString::new(name).map_err(|e| e.to_string())?;
        ioctl(group, GROUP_GET_DEVICE_FD, cname.as_ptr() as usize)
    }

    /// The VFIO device node of the PCI function `name`, bound to vfio-pci already
    /// (/sys/bus/pci/devices/NAME/vfio-dev/vfioN).
    pub fn cdev_of(name: &str) -> Result<String, String> {
        let dir = format!("/sys/bus/pci/devices/{name}/vfio-dev");
        let mut nodes: Vec<String> = std::fs::read_dir(&dir)
            .map_err(|e| format!("{dir}: {e}"))?
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .collect();
        match (nodes.pop(), nodes.is_empty()) {
            (Some(node), true) => Ok(format!("/dev/vfio/devices/{node}")),
            _ => Err(format!("{dir}: not one VFIO device node")),
        }
    }

    /// Binds the VFIO device `device`, opened by its node, to `iommufd`
    /// (struct vfio_device_bind_iommufd, as Linux 6.6 has it).
    pub fn bind_iommufd(device: libc::c_int, iommufd: libc::c_int) -> Result<(), String> {
        let mut bind = [0u32; 4];
        bind[0] = 16;
        bind[2] = iommufd as u32;
        ioctl(device, DEVICE_BIND_IOMMUFD, bind.as_mut_ptr() as usize).map(drop)
    }

    /// A new I/O address space of `iommufd`'s, its ID (struct iommu_ioas_alloc).
    pub fn ioas_alloc(iommufd: libc::c_int) -> Result<u32, String> {
        let mut alloc = [12u32, 0, 0];
        ioctl(iommufd, IOAS_ALLOC, alloc.as_mut_ptr() as usize)?;
        Ok(alloc[2])
    }

    /// Attaches `device`, bound to iommufd, to its I/O address space `ioas`
    /// (struct vfio_device_attach_iommufd_pt, as Linux 6.6 has it).
    pub fn attach(device: libc::c_int, ioas: u32) -> Result<(), String> {
        let mut attach = [12u32, 0, ioas];
        ioctl(device, DEVICE_ATTACH_IOMMUFD_PT, attach.as_mut_ptr() as usize).map(drop)
    }

    /// Maps `len` bytes of this process at `vaddr` at I/O address `iova` of iommufd's I/O
    /// address space `ioas`, for the function to read and write (struct iommu_ioas_map:
    /// FIXED_IOVA, WRITEABLE, READABLE).
    pub fn ioas_map(iommufd: libc::c_int, ioas: u32, vaddr: u64, iova: u64, len: u64) -> Result<(), String> {
        let mut map = [0u8; 40];
        map[0..4].copy_from_slice(&40u32.to_le_bytes());
        map[4..8].copy_from_slice(&7u32.to_le_bytes());
        map[8..12].copy_from_slice(&ioas.to_le_bytes());
        map[16..24].copy_from_slice(&vaddr.to_le_bytes());
        map[24..32].copy_from_slice(&len.to_le_bytes());
        map[32..40].copy_from_slice(&iova.to_le_bytes());
        ioctl(iommufd, IOAS_MAP, map.as_mut_ptr() as usize).map(drop)
    }

    /// Maps `len` bytes of this process at `vaddr` at I/O address `iova` of the
    /// container's IOMMU, for the function to read and write (struct
    /// vfio_iommu_type1_dma_map).
    pub fn map_dma(container: libc::c_int, vaddr: u64, iova: u64, len: u64) -> Result<(), String> {
        let mut map = [0u8; 32];
        map[0..4].copy_from_slice(&32u32.to_le_bytes());
        map[4..8].copy_from_slice(&3u32.to_le_bytes());
        map[8..16].copy_from_slice(&vaddr.to_le_bytes());
        map[16..24].copy_from_slice(&iova.to_le_bytes());
        map[24..32].copy_from_slice(&len.to_le_bytes());
        ioctl(container, IOMMU_MAP_DMA, map.as_ptr() as usize).map(drop)
    }

    /// Where region `index` (a BAR) is in the device's descriptor.
    pub fn region(device: libc::c_int, index: u32) -> Result<u64, String> {
        // struct vfio_region_info: argsz, flags, index, cap_offset, size, offset.
        let mut info = [0u32; 8];
        info[0] = 32;
        info[2] = index;
        ioctl(device, DEVICE_GET_REGION_INFO, info.as_mut_ptr() as usize)?;
        Ok(u64::from(info[6]) | (u64::from(info[7]) << 32))
    }

    /// struct vfio_irq_set: argsz, flags, index, start, count, then the data.
    fn set_irqs(
        device: libc::c_int,
        flags: u32,
        index: u32,
        start: u32,
        fds: &[libc::c_int],
    ) -> Result<(), String> {
        let mut set: Vec<u32> = vec![(20 + 4 * fds.len()) as u32, flags, index, start, fds.len() as u32];
        set.extend(fds.iter().map(|&fd| fd as u32));
        match ioctl(device, DEVICE_SET_IRQS, set.as_ptr() as usize)? {
            0 => Ok(()),
            could => Err(format!("{could} of {} interrupts", fds.len())),
        }
    }

    /// Signals `fds` for interrupts `0..` of `index`; none, its interrupts off.
    pub fn trigger(device: libc::c_int, index: u32, fds: &[libc::c_int]) -> Result<(), String> {
        match fds {
            [] => set_irqs(device, ACTION_TRIGGER | DATA_NONE, index, 0, &[]),
            fds => set_irqs(device, ACTION_TRIGGER | DATA_EVENTFD, index, 0, fds),
        }
    }

    /// Signals MSI-X vectors `start..` on `fds`, -1 for a vector signalled on nothing,
    /// which the kernel leaves masked.
    pub fn msix(device: libc::c_int, start: u32, fds: &[libc::c_int]) -> Result<(), String> {
        set_irqs(device, ACTION_TRIGGER | DATA_EVENTFD, MSIX, start, fds)
    }

    /// Unmasks INTx, which VFIO masked as it signalled it.
    pub fn unmask_intx(device: libc::c_int) -> Result<(), String> {
        // One interrupt, no data: count 1.
        let set: [u32; 5] = [20, ACTION_UNMASK | DATA_NONE, INTX, 0, 1];
        ioctl(device, DEVICE_SET_IRQS, set.as_ptr() as usize).map(drop)
    }

    pub fn eventfd() -> Result<libc::c_int, String> {
        // SAFETY: eventfd(2) with flags.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        if fd < 0 {
            return Err(format!("eventfd: {}", io::Error::last_os_error()));
        }
        Ok(fd)
    }

    /// Waits up to 10 s for `fd` to be signalled, and takes its count.
    pub fn wait(fd: libc::c_int) -> Result<u64, String> {
        let mut p = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one pollfd of ours.
        match unsafe { libc::poll(&raw mut p, 1, 10_000) } {
            1 => {}
            0 => return Err("no interrupt in 10 s".into()),
            _ => return Err(format!("poll: {}", io::Error::last_os_error())),
        }
        let mut count = [0u8; 8];
        // SAFETY: eight bytes of ours.
        if unsafe { libc::read(fd, count.as_mut_ptr().cast(), 8) } != 8 {
            return Err(format!("reading the eventfd: {}", io::Error::last_os_error()));
        }
        Ok(u64::from_ne_bytes(count))
    }
}

/// A guest that is itself a host (D132): it boots a VM of its own with shards' VM process
/// for this architecture's Linux, `/vm` in its initramfs, the kernel `/l2` and itself as
/// that VM's init, whose test is `shards_l2`, and `shards_l2_args`' arguments (comma
/// separated) besides, having given vfio-pci the PCI function `shards_l1_vfio` names, if
/// any. Each line of the inner VM's console is printed after `L2: `, and
/// the test passes where the inner VM ended well and its guest passed.
fn host() -> Result<(), String> {
    let test = std::env::var("shards_l2").map_err(|_| "shards_l2 not set".to_string())?;
    let args = std::env::var("shards_l2_args").unwrap_or_default();
    if !Path::new("/dev/kvm").exists() {
        return Err("no /dev/kvm: this guest's kernel is no KVM host".into());
    }
    // A PCI function of this guest's own, given to vfio-pci (`shards_l1_vfio`), for the
    // inner VM to be given in turn (D154).
    if let Ok(name) = std::env::var("shards_l1_vfio") {
        let dir = format!("/sys/bus/pci/devices/{name}");
        std::fs::write(format!("{dir}/driver_override"), "vfio-pci")
            .map_err(|e| format!("{dir}/driver_override: {e}"))?;
        std::fs::write("/sys/bus/pci/drivers_probe", &name)
            .map_err(|e| format!("binding {name} to vfio-pci: {e}"))?;
    }
    let mut vm = std::process::Command::new("/vm");
    // The inner VM process's log, at `shards_l1_log`'s level where one is asked.
    if let Ok(level) = std::env::var("shards_l1_log") {
        vm.env("SHARDS_LOG", level);
    }
    let out = vm
        .args(["run", "--kernel", "/l2", "--init", "/init", "--cpus", "2"])
        .args(["--memory", "256", "--cmdline"])
        // A function the inner guest is given it takes through VFIO: in a no-IOMMU group,
        // or behind the inner machine's IOMMU, KVM's, whose ITS isolates its MSIs, which
        // VFIO's type 1 then needs no leave to take.
        .arg(format!(
            "console=ttyS0 panic=-1 vfio.enable_unsafe_noiommu_mode=1 shards_test={test}"
        ))
        .args(args.split(',').filter(|a| !a.is_empty()))
        .output()
        .map_err(|e| format!("/vm: {e}"))?;
    let mut stdout = io::stdout().lock();
    for (stream, bytes) in [("L2", &out.stdout), ("L2 stderr", &out.stderr)] {
        for line in String::from_utf8_lossy(bytes).lines() {
            let _ = writeln!(stdout, "{stream}: {}", line.trim_end());
        }
    }
    if !out.status.success() {
        return Err(format!("the inner VM ended with {}", out.status));
    }
    let passed = String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|l| l.trim_end() == "SHARDS-TEST PASS");
    if !passed {
        return Err("the inner guest did not pass".into());
    }
    Ok(())
}

/// A guest that is a KVM host running shards (D154): its PCI functions
/// `shards_l1_vfio` (comma-separated) given vfio-pci, as a host gives one a run is to be
/// given, the image in /image.tar loaded, and a run of it given their group, by its node
/// (`--device /dev/vfio/N`) and then by a CDI device whose node it is (D146), its command
/// `shards_l2` (comma-separated), by shards' daemon, which the first run starts: what each
/// says, as `L1: ` lines, and the runs' as `L2: ` lines.
fn daemon() -> Result<(), String> {
    if !Path::new("/dev/kvm").exists() {
        return Err("no /dev/kvm: this guest's kernel is no KVM host".into());
    }
    let functions = std::env::var("shards_l1_vfio").map_err(|_| "shards_l1_vfio not set".to_string())?;
    let command = std::env::var("shards_l2").map_err(|_| "shards_l2 not set".to_string())?;
    let mut group = None;
    for name in functions.split(',') {
        vfio::bind(name)?;
        let this = vfio::group_of(name)?;
        if group.get_or_insert_with(|| this.clone()) != &this {
            return Err(format!("{functions}: in groups {group:?} and {this}"));
        }
    }
    let group = group.ok_or("shards_l1_vfio names no function")?;
    for dir in ["/tmp/home", "/tmp/cache"] {
        std::fs::create_dir_all(dir).map_err(|e| format!("{dir}: {e}"))?;
    }
    // The daemon's and its VM processes' log at `shards_l1_log`'s level where one is asked.
    let level = std::env::var("shards_l1_log").ok();
    let shards = |args: &[&str]| {
        let mut command = std::process::Command::new("/shards");
        command
            .args(args)
            .env("HOME", "/tmp/home")
            .env("SHARDS_HOME", "/tmp/shards")
            .env("XDG_CACHE_HOME", "/tmp/cache")
            .env("SHARDS_KERNEL", "/l2")
            .env("SHARDS_INIT", "/shards-init")
            .stdin(std::process::Stdio::null());
        if let Some(level) = &level {
            command.env("SHARDS_LOG", level);
        }
        command.output().map_err(|e| format!("/shards: {e}"))
    };
    let said = |who: &str, out: &std::process::Output| {
        let mut stdout = io::stdout().lock();
        for (stream, bytes) in [
            (who.to_string(), &out.stdout),
            (format!("{who} stderr"), &out.stderr),
        ] {
            for line in String::from_utf8_lossy(bytes).lines() {
                let _ = writeln!(stdout, "{stream}: {}", line.trim_end());
            }
        }
    };
    let loaded = shards(&["load", "-i", "/image.tar"])?;
    said("L1", &loaded);
    if !loaded.status.success() {
        return Err(format!("shards load ended with {}", loaded.status));
    }
    let node = format!("/dev/vfio/{group}");
    let spec = format!(
        r#"{{"cdiVersion":"0.6.0","kind":"shards.test/vfio","devices":[{{"name":"group","containerEdits":{{"deviceNodes":[{{"path":"{node}"}}]}}}}]}}"#
    );
    std::fs::create_dir_all("/etc/cdi").map_err(|e| format!("/etc/cdi: {e}"))?;
    std::fs::write("/etc/cdi/shards-test-vfio.json", spec).map_err(|e| format!("the CDI spec: {e}"))?;
    let words: Vec<&str> = command.split(',').filter(|a| !a.is_empty()).collect();
    // The runs: given the group by its node, then by a CDI device, then by the directory
    // of VFIO's nodes, as dockerd walks one; then, where this guest's kernel makes them
    // (VFIO_DEVICE_CDEV), each function by its device node and iommufd, and by the
    // directory of the device nodes, its workload taking them so too.
    let cdevs: Vec<String> = functions
        .split(',')
        .filter_map(|n| vfio::cdev_of(n).ok())
        .collect();
    let mut runs: Vec<(Vec<String>, Vec<&str>)> = vec![
        (vec!["/dev/vfio/vfio".into(), node.clone()], words.clone()),
        (
            vec!["/dev/vfio/vfio".into(), "shards.test/vfio=group".into()],
            words.clone(),
        ),
        (vec!["/dev/vfio".into()], words.clone()),
    ];
    if !cdevs.is_empty() {
        let mut by_node = words.clone();
        if let Some(first) = by_node.first_mut() {
            *first = "vfio-dma-cdev";
        }
        runs.push((
            std::iter::once("/dev/iommu".to_string()).chain(cdevs).collect(),
            by_node.clone(),
        ));
        runs.push((vec!["/dev/iommu".into(), "/dev/vfio/devices".into()], by_node));
    } else {
        let _ = writeln!(
            io::stdout(),
            "L1: no VFIO device nodes: this kernel lacks VFIO_DEVICE_CDEV"
        );
    }
    for (devices, workload) in &runs {
        let mut args = vec!["run", "--rm", "--network", "none", "--user", "root"];
        for d in devices {
            args.extend(["--device", d.as_str()]);
        }
        args.push("vfio-test");
        args.extend(workload.iter().copied());
        let ran = shards(&args)?;
        said("L2", &ran);
        if ran.status.success() {
            continue;
        }
        let device = devices.join(" ");
        // Where the daemon says why, and the VM processes it ran, once all have ended.
        said("L1", &shards(&["stop", "daemon"])?);
        if let Ok(log) = std::fs::read_to_string("/tmp/shards/daemon.log") {
            let mut stdout = io::stdout().lock();
            for line in log.lines() {
                let _ = writeln!(stdout, "L1 daemon: {line}");
            }
        }
        return Err(format!("the run given {device} ended with {}", ran.status));
    }
    Ok(())
}

/// A booted guest at rest, for measuring any VMM that holds it (the Firecracker
/// comparison): prints `SHARDS-TEST READY` and its monotonic clock in microseconds, its
/// time since its kernel started keeping time, and waits for the host to end the VM. It
/// uses nothing shards-specific, so every VMM runs the same guest.
fn idle() -> Result<(), String> {
    let _ = writeln!(io::stdout(), "SHARDS-TEST READY {}", monotonic_ns() / 1000);
    loop {
        // SAFETY: pause(2) takes no arguments; it returns only after a signal handler ran.
        unsafe { libc::pause() };
    }
}

/// A guest at work over its memory, for measuring what the host's pages cost it (THP's
/// A/B, PM M157): prints `SHARDS-TEST READY`, writes a byte to each page of
/// `shards_walk_mib` MiB, reads a byte at each of `shards_walk_reads` places in it, spread
/// by an LCG (a TLB miss nearly every read), then prints `SHARDS-TEST WALK TOUCH_NS
/// READ_NS` and idles.
fn memory_walk() -> Result<(), String> {
    let number = |name: &str, default: u64| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(default)
    };
    let mib = usize::try_from(number("shards_walk_mib", 512)).map_err(|e| e.to_string())?;
    let reads = number("shards_walk_reads", 20_000_000);
    // Its PID 1 runs, as shards-init says first thing: the VMM puts what the boot left
    // untouched on huge pages for what comes after it (D29, PM M279). Shards' alone runs
    // this test, whose control page Firecracker has not.
    ControlPage::map()?.write(shards_abi::control::MARKER, shards_abi::marker::INIT_STARTED);
    let _ = writeln!(io::stdout(), "SHARDS-TEST READY");
    let began = Instant::now();
    let mut buf = vec![0u8; mib << 20];
    for page in buf.chunks_mut(4096) {
        if let Some(b) = page.first_mut() {
            *b = 1;
        }
    }
    let touched = began.elapsed();
    let began = Instant::now();
    let (mut x, mut sum) = (0x9e37_79b9_7f4a_7c15u64, 0u64);
    let len = buf.len() as u64;
    for _ in 0..reads {
        x = x
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let at = usize::try_from((x >> 11) % len.max(1)).unwrap_or(0);
        sum = sum.wrapping_add(u64::from(buf.get(at).copied().unwrap_or(0)));
    }
    let read = began.elapsed();
    std::hint::black_box(sum);
    let _ = writeln!(
        io::stdout(),
        "SHARDS-TEST WALK {} {}",
        touched.as_nanos(),
        read.as_nanos()
    );
    idle()
}

/// `MemTotal` of /proc/meminfo, in bytes: the RAM the kernel has, plugged memory as it
/// comes online included.
fn mem_total() -> Result<u64, String> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").map_err(|e| format!("/proc/meminfo: {e}"))?;
    meminfo
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))
        .and_then(|v| v.trim().strip_suffix("kB"))
        .and_then(|kb| kb.trim().parse::<u64>().ok())
        .map(|kb| kb << 10)
        .ok_or_else(|| "/proc/meminfo: no MemTotal".to_string())
}

/// Waits for a line from the host on the console: its turn, at a point the guest names.
fn host_turn() -> Result<(), String> {
    let mut byte = [0u8; 1];
    loop {
        // SAFETY: read(2) of one byte from the console, init's stdin.
        match unsafe { libc::read(0, byte.as_mut_ptr().cast(), 1) } {
            1 if byte == *b"\n" => return Ok(()),
            1 => {}
            0 => return Err("the console closed".into()),
            _ if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
            _ => return Err(format!("the console: {}", io::Error::last_os_error())),
        }
    }
}

/// Memory to plug across a snapshot (D148): asks the VMM for a snapshot with none of it
/// plugged; then, in the VM that booted and in every VM restored from it, asks for
/// `shards_plug_mib` MiB, waits until the kernel has it all (`MemTotal`), writes and checks
/// seven eighths of it, and says `SHARDS-TEST PLUGGED <ms>`, the time from asking to having.
fn snapshot_plug() -> Result<(), String> {
    use shards_abi::control::{HOTPLUG_REQUEST, HOTPLUG_SIZE};
    let control = ControlPage::map()?;
    control.write(shards_abi::control::SNAPSHOT, shards_abi::control::SNAPSHOT_NOW);
    // A restored guest continues here.
    control.write(shards_abi::control::MARKER, shards_abi::marker::RESUMED);
    let most = control.read64(HOTPLUG_SIZE);
    let want = env_u64("shards_plug_mib")? << 20;
    if most < want {
        return Err(format!("{want} bytes asked for; the guest may plug {most}"));
    }
    let before = mem_total()?;
    let began = Instant::now();
    control.write64(HOTPLUG_REQUEST, want);
    waiting("the memory asked for", || match mem_total()? {
        now if now >= before + want => Ok(()),
        now => Err(format!("MemTotal {now}, {before} before")),
    });
    let plugged = began.elapsed();
    let used = usize::try_from(want - want / 8).map_err(|e| e.to_string())?;
    let mut buf = Aligned::new(used)?;
    for (i, page) in buf.slice(used).chunks_mut(4096).enumerate() {
        page.fill(i as u8);
    }
    if let Some(i) = buf
        .slice(used)
        .chunks(4096)
        .enumerate()
        .find_map(|(i, page)| page.iter().any(|&b| b != i as u8).then_some(i))
    {
        return Err(format!("page {i} of the plugged memory lost what was written"));
    }
    let _ = writeln!(io::stdout(), "SHARDS-TEST PLUGGED {}", plugged.as_millis());
    Ok(())
}

/// Memory plugged before a snapshot (D176): asks for `shards_plug_mib` MiB, waits until the
/// kernel has it all, writes seven eighths of it, more than the guest booted with, and only
/// then asks for a snapshot. In the VM that booted and in every VM restored from it, it
/// checks what it wrote, and says `SHARDS-TEST KEPT <MiB>`.
fn plug_snapshot() -> Result<(), String> {
    use shards_abi::control::{HOTPLUG_REQUEST, HOTPLUG_SIZE};
    let control = ControlPage::map()?;
    let most = control.read64(HOTPLUG_SIZE);
    let want = env_u64("shards_plug_mib")? << 20;
    if most < want {
        return Err(format!("{want} bytes asked for; the guest may plug {most}"));
    }
    let before = mem_total()?;
    control.write64(HOTPLUG_REQUEST, want);
    waiting("the memory asked for", || match mem_total()? {
        now if now >= before + want => Ok(()),
        now => Err(format!("MemTotal {now}, {before} before")),
    });
    let used = usize::try_from(want - want / 8).map_err(|e| e.to_string())?;
    let mut buf = Aligned::new(used)?;
    for (i, page) in buf.slice(used).chunks_mut(4096).enumerate() {
        page.fill(i as u8);
    }
    control.write(shards_abi::control::SNAPSHOT, shards_abi::control::SNAPSHOT_NOW);
    // A restored guest continues here, with what it wrote.
    control.write(shards_abi::control::MARKER, shards_abi::marker::RESUMED);
    if let Some(i) = buf
        .slice(used)
        .chunks(4096)
        .enumerate()
        .find_map(|(i, page)| page.iter().any(|&b| b != i as u8).then_some(i))
    {
        return Err(format!(
            "page {i} of what was written lost it across the snapshot"
        ));
    }
    let _ = writeln!(io::stdout(), "SHARDS-TEST KEPT {}", used >> 20);
    Ok(())
}

/// Memory plugged as the guest asks for it (D133): asks the control page for
/// `shards_plug_mib` MiB, waits until the kernel has it all (`MemTotal`), writes and checks
/// seven eighths of it, more than the guest booted with, and says `SHARDS-TEST PLUGGED
/// <ms>`, the time from asking to having; the host looks at its memory and sends a line.
/// Then frees it, asks for none, waits until the kernel has given it all back, says
/// `SHARDS-TEST UNPLUGGED <ms>`, and waits for the host again. Last, it asks for more than
/// it may have, is given what it may, and asks for none again, until it has none.
fn hotplug() -> Result<(), String> {
    use shards_abi::control::{HOTPLUG_REQUEST, HOTPLUG_SIZE};
    let control = ControlPage::map()?;
    let most = control.read64(HOTPLUG_SIZE);
    let want = env_u64("shards_plug_mib")? << 20;
    if most < want {
        return Err(format!("{want} bytes asked for; the guest may plug {most}"));
    }
    let before = mem_total()?;
    let began = Instant::now();
    control.write64(HOTPLUG_REQUEST, want);
    waiting("the memory asked for", || match mem_total()? {
        now if now >= before + want => Ok(()),
        now => Err(format!("MemTotal {now}, {before} before")),
    });
    let plugged = began.elapsed();
    let used = usize::try_from(want - want / 8).map_err(|e| e.to_string())?;
    let mut buf = Aligned::new(used)?;
    for (i, page) in buf.slice(used).chunks_mut(4096).enumerate() {
        page.fill(i as u8);
    }
    if let Some(i) = buf
        .slice(used)
        .chunks(4096)
        .enumerate()
        .find_map(|(i, page)| page.iter().any(|&b| b != i as u8).then_some(i))
    {
        return Err(format!("page {i} of the plugged memory lost what was written"));
    }
    let _ = writeln!(io::stdout(), "SHARDS-TEST PLUGGED {}", plugged.as_millis());
    host_turn()?;
    drop(buf);
    let began = Instant::now();
    control.write64(HOTPLUG_REQUEST, 0);
    waiting("the memory to go back", || match mem_total()? {
        now if now <= before => Ok(()),
        now => Err(format!("MemTotal {now}, {before} before")),
    });
    let _ = writeln!(
        io::stdout(),
        "SHARDS-TEST UNPLUGGED {}",
        began.elapsed().as_millis()
    );
    host_turn()?;
    control.write64(HOTPLUG_REQUEST, u64::MAX);
    let given = control.read64(HOTPLUG_REQUEST);
    control.write64(HOTPLUG_REQUEST, 0);
    if given != most {
        return Err(format!("asked for everything, given {given} of {most}"));
    }
    waiting("the memory to go back again", || match mem_total()? {
        now if now <= before => Ok(()),
        now => Err(format!("MemTotal {now}, {before} before")),
    });
    Ok(())
}

/// What the virtio-mem driver gives back of memory a guest plugged, used and freed, as that
/// memory came online (its command line's `memhp_default_state` and
/// `memory_hotplug.online_policy`): plugs `shards_plug_mib` MiB; fills seven eighths of it
/// with its own pages and, between them, the kernel's (`shards_files` files of a page each
/// in a tmpfs: their pages, inodes and dentries), as a session's are mixed; frees all of it
/// and drops the kernel's caches; then asks for none, and says how much is still plugged
/// 0.1, 0.5, 1, 2, 5, 10 and 30 s after, and how many memory blocks of it are still
/// online: `SHARDS-TEST UNPLUG <plugged> <ms>:<left>... blocks:<n>`.
fn unplug_cost() -> Result<(), String> {
    use shards_abi::control::{HOTPLUG_REQUEST, HOTPLUG_SIZE};
    let control = ControlPage::map()?;
    let most = control.read64(HOTPLUG_SIZE);
    let want = env_u64("shards_plug_mib")? << 20;
    if most < want {
        return Err(format!("{want} bytes asked for; the guest may plug {most}"));
    }
    let files = env_u64("shards_files").unwrap_or(65_536);
    let before = mem_total()?;
    let blocks_before = blocks_online()?;
    control.write64(HOTPLUG_REQUEST, want);
    waiting("the memory asked for", || match mem_total()? {
        now if now >= before + want => Ok(()),
        now => Err(format!("MemTotal {now}, {before} before")),
    });
    std::fs::create_dir_all("/scratch").map_err(|e| format!("/scratch: {e}"))?;
    // SAFETY: mount(2) of NUL-terminated literals.
    if unsafe {
        libc::mount(
            c"tmpfs".as_ptr(),
            c"/scratch".as_ptr(),
            c"tmpfs".as_ptr(),
            0,
            std::ptr::null(),
        )
    } != 0
    {
        return Err(format!("/scratch: {}", io::Error::last_os_error()));
    }
    let used = usize::try_from(want - want / 8).map_err(|e| e.to_string())?;
    let rounds = 16usize;
    let mut held = Vec::with_capacity(rounds);
    let page = [7u8; 4096];
    for r in 0..rounds {
        let mut buf = Aligned::new(used / rounds)?;
        for p in buf.slice(used / rounds).chunks_mut(4096) {
            p.fill(1);
        }
        held.push(buf);
        for f in 0..files / rounds as u64 {
            let path = format!("/scratch/{r}-{f}");
            std::fs::write(&path, page).map_err(|e| format!("{path}: {e}"))?;
        }
    }
    drop(held);
    for r in 0..rounds {
        for f in 0..files / rounds as u64 {
            let _ = std::fs::remove_file(format!("/scratch/{r}-{f}"));
        }
    }
    let _ = std::fs::write("/proc/sys/vm/drop_caches", "3");
    let plugged = mem_total()?.saturating_sub(before);
    let began = Instant::now();
    control.write64(HOTPLUG_REQUEST, 0);
    let mut said = format!("SHARDS-TEST UNPLUG {plugged}");
    for at in [100u64, 500, 1_000, 2_000, 5_000, 10_000, 30_000] {
        let due = Duration::from_millis(at);
        if let Some(wait) = due.checked_sub(began.elapsed()) {
            std::thread::sleep(wait);
        }
        said.push_str(&format!(" {at}:{}", mem_total()?.saturating_sub(before)));
    }
    // The memory blocks still online of what was plugged: each keeps its page structures.
    said.push_str(&format!(
        " blocks:{}",
        blocks_online()?.saturating_sub(blocks_before)
    ));
    let _ = writeln!(io::stdout(), "{said}");
    Ok(())
}

/// The memory the kernel has as PID 1 begins, as /proc/meminfo says it, in bytes: what
/// devices took of it as they were probed, a DAX window's page structures among them
/// (D189), and how long the kernel took to come to it (/proc/uptime, in seconds):
/// `SHARDS-TEST MEMORY <MemTotal> <MemFree> <MemAvailable> <uptime>`.
fn memory_report() -> Result<(), String> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").map_err(|e| format!("/proc/meminfo: {e}"))?;
    let field = |name: &str| -> Result<u64, String> {
        meminfo
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.trim().strip_suffix("kB"))
            .and_then(|kb| kb.trim().parse::<u64>().ok())
            .map(|kb| kb << 10)
            .ok_or_else(|| format!("/proc/meminfo: no {name}"))
    };
    let uptime = std::fs::read_to_string("/proc/uptime").map_err(|e| format!("/proc/uptime: {e}"))?;
    let _ = writeln!(
        io::stdout(),
        "SHARDS-TEST MEMORY {} {} {} {}",
        field("MemTotal:")?,
        field("MemFree:")?,
        field("MemAvailable:")?,
        uptime.split_whitespace().next().unwrap_or("?")
    );
    Ok(())
}

/// How many memory blocks the kernel has online (/sys/devices/system/memory, its
/// `memoryN/online`).
fn blocks_online() -> Result<u64, String> {
    let dir = "/sys/devices/system/memory";
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{dir}: {e}"))?;
    Ok(entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("memory"))
        .filter(|e| std::fs::read_to_string(e.path().join("online")).is_ok_and(|v| v.trim() == "1"))
        .count() as u64)
}

/// The limits the kernel sized by the memory it booted with (shards_testguest::LIMITS)
/// and its MemTotal: what a builder's init raises to the memory it may have (D133),
/// checked against a guest booted with all of it.
fn limits() -> Result<(), String> {
    crate::workload::limits()?;
    let _ = writeln!(io::stdout(), "SHARDS-TEST LIMIT MemTotal {}", mem_total()?);
    Ok(())
}

/// A running guest, for measuring any VMM that restores it (the Firecracker comparison):
/// prints `SHARDS-TEST READY`, then a `.` every millisecond, so the first `.` a restore
/// prints is the guest running again. It uses nothing shards-specific but, with
/// `shards_snapshot=N`, the control page, to ask shards for a snapshot after N beats, as
/// init asks for a template's; other VMMs snapshot it from the host.
fn beat() -> Result<(), String> {
    let snapshot_after: Option<u64> = std::env::var("shards_snapshot")
        .ok()
        .map(|n| n.parse())
        .transpose()
        .map_err(|e| format!("shards_snapshot: {e}"))?;
    let control = snapshot_after.map(|_| ControlPage::map()).transpose()?;
    // A quiet guest, as shards-init leaves a template: the kernel's crypto self-tests run
    // in threads after boot, and a snapshot taken before they end hands the rest to every
    // restore (docs/research/platform-measurements.md M21, M38).
    await_crypto_selftests()?;
    let _ = writeln!(io::stdout(), "SHARDS-TEST READY");
    let mut beats = 0u64;
    loop {
        thread::sleep(Duration::from_millis(1));
        let mut out = io::stdout().lock();
        let _ = out.write_all(b".").and_then(|()| out.flush());
        beats += 1;
        if let (Some(control), Some(after)) = (&control, snapshot_after)
            && beats == after
        {
            control.write(shards_abi::control::SNAPSHOT, shards_abi::control::SNAPSHOT_NOW);
        }
    }
}

/// Waits, for up to 2 s, until /proc/crypto lists no algorithm under test (a larval) or not
/// yet tested (crypto/proc.c, `c_show`), as shards-init's `await_crypto_selftests` does.
fn await_crypto_selftests() -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let text = std::fs::read_to_string("/proc/crypto").map_err(|e| format!("/proc/crypto: {e}"))?;
        let running = text.lines().any(|line| {
            let mut field = line.splitn(2, ':').map(str::trim);
            matches!(
                (field.next(), field.next()),
                (Some("selftest"), Some("unknown")) | (Some("type"), Some("larval"))
            )
        });
        if !running {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("crypto self-tests still running after 2 s".into());
        }
        thread::sleep(Duration::from_millis(1));
    }
}

/// The vsock port the guest serves an echo on.
const ECHO_PORT: u32 = 1234;
/// Where the host tells a storm it has stopped streaming.
const DONE_PORT: u32 = 1235;
/// Bytes the echo has sent back, over every connection.
static ECHOED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// The host port the guest dials first.
const HOST_PORT: u32 = 5000;

fn vsock_addr(cid: u32, port: u32) -> libc::sockaddr_vm {
    // SAFETY: an all-zero sockaddr_vm is a valid value.
    let mut a: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    a.svm_family = libc::AF_VSOCK as libc::sa_family_t;
    a.svm_cid = cid;
    a.svm_port = port;
    a
}

fn vsock_socket() -> Result<std::fs::File, String> {
    // SAFETY: socket(2) with constant arguments.
    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(format!("vsock socket: {}", io::Error::last_os_error()));
    }
    // SAFETY: a fresh descriptor nothing else owns.
    Ok(unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd) })
}

fn vsock_connect(cid: u32, port: u32) -> Result<std::fs::File, String> {
    use std::os::fd::AsRawFd;
    let s = vsock_socket()?;
    let a = vsock_addr(cid, port);
    let len = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;
    // SAFETY: `a` is a valid sockaddr_vm of `len` bytes.
    if unsafe { libc::connect(s.as_raw_fd(), (&raw const a).cast(), len) } != 0 {
        return Err(format!(
            "vsock connect {cid}:{port}: {}",
            io::Error::last_os_error()
        ));
    }
    Ok(s)
}

fn vsock_listen(port: u32) -> Result<std::fs::File, String> {
    use std::os::fd::AsRawFd;
    let s = vsock_socket()?;
    let a = vsock_addr(libc::VMADDR_CID_ANY, port);
    let len = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;
    // SAFETY: `a` is a valid sockaddr_vm of `len` bytes.
    if unsafe { libc::bind(s.as_raw_fd(), (&raw const a).cast(), len) } != 0 {
        return Err(format!("vsock bind {port}: {}", io::Error::last_os_error()));
    }
    // SAFETY: listen(2) on our own socket.
    if unsafe { libc::listen(s.as_raw_fd(), 64) } != 0 {
        return Err(format!("vsock listen: {}", io::Error::last_os_error()));
    }
    Ok(s)
}

/// Echoes each connection on `listener` until its EOF, then half-closes it, forever.
fn serve_echo(listener: &std::fs::File) -> Result<(), String> {
    use std::io::Read as _;
    use std::os::fd::{AsRawFd, FromRawFd};
    loop {
        // SAFETY: accept(2) on our listening socket, without the peer address.
        let fd = unsafe {
            libc::accept4(
                listener.as_raw_fd(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                libc::SOCK_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(format!("vsock accept: {}", io::Error::last_os_error()));
        }
        // SAFETY: a fresh descriptor nothing else owns.
        let mut conn = unsafe { std::fs::File::from_raw_fd(fd) };
        thread::spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match conn.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if conn.write_all(buf.get(..n).unwrap_or_default()).is_err() {
                            return;
                        }
                        ECHOED.fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }
            // SAFETY: shutdown(2) on our own socket: the host reads EOF after the echo.
            unsafe { libc::shutdown(conn.as_raw_fd(), libc::SHUT_WR) };
        });
    }
}

/// Dials the host, then serves the echo: host and guest each open a connection.
fn vsock() -> Result<(), String> {
    use std::io::Read as _;
    let listener = vsock_listen(ECHO_PORT)?;
    let mut host = vsock_connect(libc::VMADDR_CID_HOST, HOST_PORT)?;
    host.write_all(b"hello from the guest\n")
        .map_err(|e| format!("writing to the host: {e}"))?;
    let mut reply = Vec::new();
    host.read_to_end(&mut reply)
        .map_err(|e| format!("reading from the host: {e}"))?;
    if reply != b"hello from the host\n" {
        return Err(format!("the host said {:?}", String::from_utf8_lossy(&reply)));
    }
    drop(host);
    let _ = writeln!(io::stdout(), "SHARDS-TEST READY");
    serve_echo(&listener)
}

/// Listens, asks for a snapshot, and serves the echo in every restored copy: its
/// listener outlives the restore.
fn vsock_snapshot() -> Result<(), String> {
    let listener = vsock_listen(ECHO_PORT)?;
    let control = ControlPage::map()?;
    control.write(shards_abi::control::SNAPSHOT, shards_abi::control::SNAPSHOT_NOW);
    let _ = writeln!(io::stdout(), "SHARDS-TEST READY");
    serve_echo(&listener)
}

/// Sends the guest's line on `conn` and reads the host's, leaving the connection open.
fn greet(conn: &mut std::fs::File) -> Result<(), String> {
    use std::io::Read as _;
    conn.write_all(b"hello from the guest\n")
        .map_err(|e| format!("writing to the host: {e}"))?;
    let mut reply = Vec::new();
    let mut byte = [0u8; 1];
    while reply.last() != Some(&b'\n') {
        match conn.read(&mut byte) {
            Ok(1) => reply.extend_from_slice(&byte),
            Ok(_) => return Err("the host hung up before answering".into()),
            Err(e) => return Err(format!("reading from the host: {e}")),
        }
    }
    if reply != b"hello from the host\n" {
        return Err(format!("the host said {:?}", String::from_utf8_lossy(&reply)));
    }
    Ok(())
}

/// Holds a connection to the host across a snapshot. Each restored copy must find it
/// closed, since its host is gone, then dials its own host and serves the echo.
fn vsock_snapshot_held() -> Result<(), String> {
    use std::io::Read as _;
    use std::os::fd::AsRawFd;
    let listener = vsock_listen(ECHO_PORT)?;
    let mut held = vsock_connect(libc::VMADDR_CID_HOST, HOST_PORT)?;
    greet(&mut held)?;
    let control = ControlPage::map()?;
    control.write(shards_abi::control::SNAPSHOT, shards_abi::control::SNAPSHOT_NOW);
    // A restored copy continues here. A reset that never comes fails the test, not hangs.
    let wait = libc::timeval {
        tv_sec: 10,
        tv_usec: 0,
    };
    // SAFETY: setsockopt(2) on our own socket, with a timeval of its size.
    let set = unsafe {
        libc::setsockopt(
            held.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            (&raw const wait).cast(),
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        )
    };
    if set != 0 {
        return Err(format!("SO_RCVTIMEO: {}", io::Error::last_os_error()));
    }
    match held.read(&mut [0u8; 1]) {
        Ok(0) => {}
        Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
        Ok(_) => return Err("the held connection carried data after the restore".into()),
        Err(e) => return Err(format!("the held connection was not closed: {e}")),
    }
    drop(held);
    let mut fresh = vsock_connect(libc::VMADDR_CID_HOST, HOST_PORT)?;
    greet(&mut fresh)?;
    drop(fresh);
    let _ = writeln!(io::stdout(), "SHARDS-TEST READY");
    serve_echo(&listener)
}

/// The pmem region size the VMM gives a file of `len` bytes.
const PMEM_ALIGN: u64 = 2 << 20;

/// Checks every pmem device against `shards_pmem=<bytes>:<salt>,...`: its size is the
/// file's rounded up to 2 MiB, the file's bytes match pattern `salt`, and the rest reads as
/// zeros. Reads bypass the page cache, so they come from the region itself. With
/// `shards_snapshot=1`, asks for a snapshot and checks again in the restored copy.
fn pmem() -> Result<(), String> {
    let spec = std::env::var("shards_pmem").map_err(|_| "shards_pmem not set".to_string())?;
    let files: Vec<(u64, u64)> = spec
        .split(',')
        .map(|f| {
            let (len, salt) = f.split_once(':').ok_or(format!("bad shards_pmem entry {f:?}"))?;
            Ok((
                len.parse().map_err(|e| format!("{len}: {e}"))?,
                salt.parse().map_err(|e| format!("{salt}: {e}"))?,
            ))
        })
        .collect::<Result<_, String>>()?;
    let check = || -> Result<(), String> {
        for (i, &(len, salt)) in files.iter().enumerate() {
            check_pmem(i, len, salt)?;
        }
        Ok(())
    };
    check()?;
    if std::env::var("shards_snapshot").is_ok_and(|v| v == "1") {
        let control = ControlPage::map()?;
        control.write(shards_abi::control::SNAPSHOT, shards_abi::control::SNAPSHOT_NOW);
        check()?;
    }
    Ok(())
}

fn check_pmem(i: usize, len: u64, salt: u64) -> Result<(), String> {
    let sectors: u64 = sysfs(&format!("/sys/block/pmem{i}/size"))?
        .parse()
        .map_err(|e| format!("pmem{i} size: {e}"))?;
    let size = sectors * 512;
    let want = len.next_multiple_of(PMEM_ALIGN);
    if size != want {
        return Err(format!(
            "pmem{i} holds {size} bytes; its file rounds up to {want}"
        ));
    }
    let fd = open(&format!("/dev/pmem{i}"), libc::O_RDONLY | libc::O_DIRECT)?;
    let mut buf = Aligned::new(1 << 20)?;
    let mut at = 0u64;
    while at < size {
        let n = (size - at).min(1 << 20) as usize;
        let chunk = buf.slice(n);
        pread_exact(fd, chunk, at)?;
        let file_end = len.saturating_sub(at).min(n as u64) as usize;
        let (data, pad) = chunk.split_at(file_end);
        if let Some(bad) = first_mismatch(salt, at, data) {
            return Err(format!("pmem{i} byte {} differs from its file", at + bad as u64));
        }
        if let Some(bad) = pad.iter().position(|&b| b != 0) {
            return Err(format!(
                "pmem{i} byte {} past the file is not zero",
                at + file_end as u64 + bad as u64
            ));
        }
        at += n as u64;
    }
    // SAFETY: closing our own descriptor.
    unsafe { libc::close(fd) };
    Ok(())
}

/// A writable layer's costs (D190, PM M277): `/dev/pmem0` the ext4 file system `run vm
/// --layer` gives it, its page structures in the device's head. Says, one `LAYER` line
/// each: MemFree as it starts and once the NVDIMM buses' probes are done, and when they
/// were; a DAX mount's time, an overlay's over it, 64 MiB written and synced, 1,000 small
/// files, 8 MiB written through a mapping; and three mounts again. Without the device,
/// MemFree alone, for a boot without one to set beside it.
fn layer() -> Result<(), String> {
    let now = || {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: a valid timespec to fill.
        unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &raw mut ts) };
        ts.tv_sec as f64 * 1e3 + ts.tv_nsec as f64 / 1e6
    };
    let free = || -> Result<u64, String> {
        let info = std::fs::read_to_string("/proc/meminfo").map_err(|e| format!("/proc/meminfo: {e}"))?;
        info.lines()
            .find_map(|l| l.strip_prefix("MemFree:"))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
            .ok_or_else(|| "no MemFree".to_string())
    };
    let mut out = io::stdout().lock();
    let started = now();
    let _ = writeln!(out, "LAYER start_ms={started:.3} free_kib={}", free()?);
    for bus in std::fs::read_dir("/sys/bus/nd/devices")
        .into_iter()
        .flatten()
        .flatten()
    {
        if bus.file_name().as_encoded_bytes().starts_with(b"ndbus") {
            let _ = std::fs::read(bus.path().join("wait_probe"));
        }
    }
    let _ = writeln!(out, "LAYER probed_ms={:.3} free_kib={}", now(), free()?);
    if !std::path::Path::new("/dev/pmem0").exists() {
        return Ok(());
    }
    let pfn = std::fs::read_dir("/sys/bus/nd/devices")
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| {
            let name = e.file_name();
            name.as_encoded_bytes().starts_with(b"pfn") && e.path().join("block").exists()
        });
    let _ = writeln!(
        out,
        "LAYER pfn={pfn} dax={}",
        sysfs("/sys/block/pmem0/queue/dax").unwrap_or_default()
    );
    let c = |s: &str| CString::new(s).map_err(|e| e.to_string());
    let mount = |src: &str, target: &str, fs: &str, data: &str| -> Result<f64, String> {
        std::fs::create_dir_all(target).map_err(|e| format!("{target}: {e}"))?;
        let (src, target_c, fs, data) = (c(src)?, c(target)?, c(fs)?, c(data)?);
        let t = now();
        // SAFETY: NUL-terminated strings.
        if unsafe {
            libc::mount(
                src.as_ptr(),
                target_c.as_ptr(),
                fs.as_ptr(),
                0,
                data.as_ptr().cast(),
            )
        } != 0
        {
            return Err(format!("mount {target:?}: {}", io::Error::last_os_error()));
        }
        Ok((now() - t) * 1e3)
    };
    let umount = |target: &str| -> Result<(), String> {
        let target = c(target)?;
        // SAFETY: a NUL-terminated path.
        if unsafe { libc::umount(target.as_ptr()) } != 0 {
            return Err(format!("umount: {}", io::Error::last_os_error()));
        }
        Ok(())
    };
    const OPTS: &str = "dax=always,noinit_itable";
    const OVERLAY: &str = "lowerdir=/lower,upperdir=/l/upper,workdir=/l/work,volatile,xino=on";
    let first = mount("/dev/pmem0", "/l", "ext4", OPTS)?;
    for d in ["/l/upper", "/l/work", "/lower"] {
        std::fs::create_dir_all(d).map_err(|e| format!("{d}: {e}"))?;
    }
    let overlay = mount("overlay", "/merged", "overlay", OVERLAY)?;
    let mounts = std::fs::read_to_string("/proc/mounts").map_err(|e| e.to_string())?;
    let dax = mounts
        .lines()
        .any(|l| l.contains(" /l ext4 ") && l.contains("dax=always"));
    let _ = writeln!(
        out,
        "LAYER mount_us={first:.1} overlay_us={overlay:.1} mounted_dax={dax}"
    );
    let before = free()?;
    let chunk = vec![0x5au8; 1 << 20];
    let t = now();
    let mut big = std::fs::File::create("/merged/big").map_err(|e| e.to_string())?;
    for _ in 0..64 {
        big.write_all(&chunk).map_err(|e| e.to_string())?;
    }
    big.sync_all().map_err(|e| e.to_string())?;
    let wrote = now() - t;
    drop(big);
    let t = now();
    for i in 0..1000 {
        std::fs::write(format!("/merged/small{i}"), b"hello").map_err(|e| e.to_string())?;
    }
    let small = now() - t;
    let t = now();
    {
        use std::os::fd::AsRawFd as _;
        let len = 8 << 20;
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open("/l/mapped")
            .map_err(|e| e.to_string())?;
        f.set_len(len as u64).map_err(|e| e.to_string())?;
        // SAFETY: a fresh shared mapping of our own file, unmapped below.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                f.as_raw_fd(),
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(format!("mmap: {}", io::Error::last_os_error()));
        }
        for page in (0..len).step_by(4096) {
            // SAFETY: within the mapping.
            unsafe { *p.cast::<u8>().add(page) = 1 };
        }
        // SAFETY: the mapping made above.
        unsafe {
            libc::msync(p, len, libc::MS_SYNC);
            libc::munmap(p, len);
        }
    }
    let mapped = now() - t;
    let _ = writeln!(
        out,
        "LAYER write_64mib_ms={wrote:.3} small_1000_ms={small:.3} mapped_8mib_ms={mapped:.3} free_kib_spent={}",
        before.saturating_sub(free()?)
    );
    // What keeping the layer costs as a run ends (D190), each after 100 more files, as a
    // run leaves its last changes: its journal written in place as init's KEEP writes it
    // (sync, syncfs, then EXT4_IOC_CHECKPOINT), the file system frozen (FIFREEZE, then
    // FITHAW), or remounted read-only (then writable again). The first round's are the
    // first of each in the microVM, as a run's KEEP is.
    {
        use std::os::fd::AsRawFd as _;
        /// FIFREEZE and FITHAW, `_IOWR('X', 119, int)` and `_IOWR('X', 120, int)`.
        const FIFREEZE: u32 = 0xC004_5877;
        const FITHAW: u32 = 0xC004_5878;
        /// EXT4_IOC_CHECKPOINT, `_IOW('f', 43, __u32)` (include/uapi/linux/ext4.h).
        const CHECKPOINT: u32 = 0x4004_662B;
        let root = std::fs::File::open("/l").map_err(|e| format!("/l: {e}"))?;
        let fd = root.as_raw_fd();
        let layer = c("/l")?;
        let ioctl = |name: &str, request: u32, arg: *const u32| -> Result<f64, String> {
            let t = now();
            // SAFETY: an ioctl of a descriptor this holds, its argument null or a u32 that
            // outlives the call.
            if unsafe { libc::ioctl(fd, request as _, arg) } != 0 {
                return Err(format!("{name}: {}", io::Error::last_os_error()));
            }
            Ok((now() - t) * 1e3)
        };
        let remount = |flags: libc::c_ulong| -> Result<f64, String> {
            let t = now();
            // SAFETY: a NUL-terminated mount point; a remount takes no source, type or data.
            let r = unsafe {
                libc::mount(
                    std::ptr::null(),
                    layer.as_ptr(),
                    std::ptr::null(),
                    libc::MS_REMOUNT | flags,
                    std::ptr::null(),
                )
            };
            if r != 0 {
                return Err(format!("remount /l {flags:#x}: {}", io::Error::last_os_error()));
            }
            Ok((now() - t) * 1e3)
        };
        for round in 0..3 {
            let more = |tag: &str| -> Result<(), String> {
                for i in 0..100 {
                    std::fs::write(format!("/merged/{tag}{round}-{i}"), b"more")
                        .map_err(|e| e.to_string())?;
                }
                Ok(())
            };
            more("k")?;
            let t = now();
            // SAFETY: sync(2) takes nothing.
            unsafe { libc::sync() };
            let all = (now() - t) * 1e3;
            let t = now();
            // SAFETY: syncfs(2) of a descriptor this holds.
            if unsafe { libc::syncfs(fd) } != 0 {
                return Err(format!("syncfs: {}", io::Error::last_os_error()));
            }
            let synced = (now() - t) * 1e3;
            let flags: u32 = 0;
            let checkpointed = ioctl("EXT4_IOC_CHECKPOINT", CHECKPOINT, &raw const flags)?;
            more("f")?;
            let frozen = ioctl("FIFREEZE", FIFREEZE, std::ptr::null())?;
            let thawed = ioctl("FITHAW", FITHAW, std::ptr::null())?;
            more("r")?;
            let read_only = remount(libc::MS_RDONLY)?;
            let writable = remount(0)?;
            let _ = writeln!(
                out,
                "LAYER keep{round} sync_us={all:.1} syncfs_us={synced:.1} checkpoint_us={checkpointed:.1} keep_us={:.1} freeze_us={frozen:.1} thaw_us={thawed:.1} remount_ro_us={read_only:.1} remount_rw_us={writable:.1}",
                all + synced + checkpointed
            );
        }
    }
    umount("/merged")?;
    umount("/l")?;
    for i in 0..3 {
        let again = mount("/dev/pmem0", "/l", "ext4", OPTS)?;
        // A volatile overlay's mark refuses the next mount of its directories (overlayfs.rst).
        let _ = std::fs::remove_dir_all("/l/work/work");
        let overlay = mount("overlay", "/merged", "overlay", OVERLAY)?;
        umount("/merged")?;
        umount("/l")?;
        let _ = writeln!(out, "LAYER again{i} mount_us={again:.1} overlay_us={overlay:.1}");
    }
    Ok(())
}

fn cstr(s: &str) -> Result<CString, String> {
    CString::new(s).map_err(|e| e.to_string())
}

/// Mounts /dev/pmem0 as EROFS with DAX and checks it against its own /MANIFEST: one line
/// per entry, written by the host test (crates/shards/tests/erofs.rs and layers.rs). The
/// image must hold exactly the manifest's entries and the manifest.
fn erofs() -> Result<(), String> {
    std::fs::create_dir_all("/mnt").map_err(|e| format!("/mnt: {e}"))?;
    let (src, target, fs, opts) = (
        cstr("/dev/pmem0")?,
        cstr("/mnt")?,
        cstr("erofs")?,
        cstr("dax=always")?,
    );
    let mount = || {
        // SAFETY: NUL-terminated strings.
        if unsafe {
            libc::mount(
                src.as_ptr(),
                target.as_ptr(),
                fs.as_ptr(),
                libc::MS_RDONLY,
                opts.as_ptr().cast(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    };
    // libnvdimm makes a pmem device's node asynchronously, in a domain the kernel does not
    // wait for before init runs (crates/init/src/pmem.rs): where it is not there yet, once
    // its buses' probing is done.
    let mounted = mount().or_else(|e| match e.raw_os_error() {
        Some(libc::ENOENT | libc::ENXIO) => {
            for bus in std::fs::read_dir("/sys/bus/nd/devices")
                .into_iter()
                .flatten()
                .flatten()
            {
                if bus.file_name().as_encoded_bytes().starts_with(b"ndbus") {
                    let _ = std::fs::read(bus.path().join("wait_probe"));
                }
            }
            mount()
        }
        _ => Err(e),
    });
    if let Err(e) = mounted {
        return Err(format!("mount erofs: {e}"));
    }
    let mounts = std::fs::read_to_string("/proc/mounts").map_err(|e| e.to_string())?;
    let line = mounts.lines().find(|l| l.contains(" /mnt ")).unwrap_or_default();
    if !line.contains("dax=always") {
        return Err(format!("EROFS is not using DAX: {line}"));
    }
    let manifest = std::fs::read_to_string("/mnt/MANIFEST").map_err(|e| format!("MANIFEST: {e}"))?;
    let mut checked = 0;
    let mut want = BTreeSet::from(["/MANIFEST".to_string()]);
    for line in manifest.lines() {
        check_entry(line).map_err(|e| format!("{line:?}: {e}"))?;
        checked += 1;
        let mut fields = line.split(' ');
        if let (Some(kind), Some(path)) = (fields.next(), fields.next())
            && kind != "x"
        {
            want.insert(path.to_string());
        }
    }
    let mut found = BTreeSet::new();
    walk(Path::new("/mnt"), "", &mut found)?;
    if found != want {
        let extra: Vec<_> = found.difference(&want).take(5).collect();
        let missing: Vec<_> = want.difference(&found).take(5).collect();
        return Err(format!("unexpected entries {extra:?}, missing {missing:?}"));
    }
    let _ = writeln!(io::stdout(), "SHARDS-TEST INFO checked {checked} entries");
    Ok(())
}

/// Every path under `dir`, as `prefix/name`, without following symlinks.
fn walk(dir: &Path, prefix: &str, found: &mut BTreeSet<String>) -> Result<(), String> {
    for entry in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = format!("{prefix}/{}", entry.file_name().to_string_lossy());
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            walk(&entry.path(), &path, found)?;
        }
        found.insert(path);
    }
    Ok(())
}

fn lstat(path: &str) -> Result<libc::stat, String> {
    let p = cstr(path)?;
    // SAFETY: an all-zero stat is a valid out-parameter; `p` is NUL-terminated.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    if unsafe { libc::lstat(p.as_ptr(), &mut st) } != 0 {
        return Err(format!("lstat {path}: {}", io::Error::last_os_error()));
    }
    Ok(st)
}

fn num<T: std::str::FromStr>(field: Option<&str>) -> Result<T, String> {
    field
        .ok_or("missing field")?
        .parse()
        .map_err(|_| "bad number".to_string())
}

/// Whether the kernel serves `path` with DAX: `STATX_ATTR_DAX` in statx(2)'s attributes
/// (include/uapi/linux/stat.h), which it must also report it can say.
fn is_dax(path: &str) -> Result<bool, String> {
    const STATX_BASIC_STATS: u32 = 0x07ff;
    const STATX_ATTR_DAX: u64 = 0x0020_0000;
    let c = cstr(path)?;
    // struct statx: 256 bytes, with stx_attributes at 0x08 and stx_attributes_mask at 0x38.
    let mut buf = [0u64; 32];
    // SAFETY: a NUL-terminated path, and a buffer of the size and alignment statx writes.
    let r = unsafe {
        libc::syscall(
            libc::SYS_statx,
            libc::AT_FDCWD,
            c.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
            STATX_BASIC_STATS,
            buf.as_mut_ptr(),
        )
    };
    if r != 0 {
        return Err(format!("statx: {}", io::Error::last_os_error()));
    }
    let [_, attributes, _, _, _, _, _, mask, ..] = buf;
    if mask & STATX_ATTR_DAX == 0 {
        return Err("statx does not report DAX".into());
    }
    Ok(attributes & STATX_ATTR_DAX != 0)
}

/// One manifest line: `<kind> <path> ...` (see crates/shards/tests/erofs.rs).
fn check_entry(line: &str) -> Result<(), String> {
    let mut f = line.split(' ');
    let kind = f.next().ok_or("empty line")?;
    let path = format!("/mnt{}", f.next().ok_or("no path")?);
    let st = lstat(&path)?;
    let file_type = st.st_mode & libc::S_IFMT;
    let expect_meta = |f: &mut std::str::Split<'_, char>| -> Result<(), String> {
        let mode = u32::from_str_radix(f.next().ok_or("no mode")?, 8).map_err(|e| e.to_string())?;
        let (uid, gid): (u32, u32) = (num(f.next())?, num(f.next())?);
        if (st.st_mode & 0o7777, st.st_uid, st.st_gid) != (mode, uid, gid) {
            return Err(format!(
                "mode {:o} uid {} gid {}",
                st.st_mode & 0o7777,
                st.st_uid,
                st.st_gid
            ));
        }
        Ok(())
    };
    match kind {
        "d" => {
            if file_type != libc::S_IFDIR {
                return Err("not a directory".into());
            }
            expect_meta(&mut f)
        }
        "f" => {
            if file_type != libc::S_IFREG {
                return Err("not a regular file".into());
            }
            expect_meta(&mut f)?;
            let (size, salt): (u64, u64) = (num(f.next())?, num(f.next())?);
            let data = std::fs::read(&path).map_err(|e| e.to_string())?;
            if data.len() as u64 != size {
                return Err(format!("{} bytes", data.len()));
            }
            if let Some(at) = first_mismatch(salt, 0, &data) {
                return Err(format!("byte {at} differs"));
            }
            // Mapped straight from pmem, not copied through the page cache.
            if !is_dax(&path)? {
                return Err("not served with DAX".into());
            }
            Ok(())
        }
        "l" => {
            let target = std::fs::read_link(&path).map_err(|e| e.to_string())?;
            let want = f.next().ok_or("no target")?;
            if target.as_os_str().as_encoded_bytes() != want.as_bytes() {
                return Err(format!("points to {}", target.display()));
            }
            Ok(())
        }
        "c" | "b" => {
            let want = if kind == "c" { libc::S_IFCHR } else { libc::S_IFBLK };
            let (major, minor): (u32, u32) = (num(f.next())?, num(f.next())?);
            if file_type != want || (libc::major(st.st_rdev), libc::minor(st.st_rdev)) != (major, minor) {
                return Err(format!(
                    "type {file_type:o} rdev {}:{}",
                    libc::major(st.st_rdev),
                    libc::minor(st.st_rdev)
                ));
            }
            Ok(())
        }
        "p" if file_type == libc::S_IFIFO => Ok(()),
        "s" if file_type == libc::S_IFSOCK => Ok(()),
        "h" => {
            let other = lstat(&format!("/mnt{}", f.next().ok_or("no other path")?))?;
            if (st.st_ino, st.st_nlink) != (other.st_ino, 2) {
                return Err(format!(
                    "inode {} links {}, other inode {}",
                    st.st_ino, st.st_nlink, other.st_ino
                ));
            }
            Ok(())
        }
        "x" => {
            let name = cstr(f.next().ok_or("no name")?)?;
            let hex = f.next().unwrap_or_default();
            let want: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(hex.get(i..i + 2).unwrap_or("zz"), 16).map_err(|e| e.to_string()))
                .collect::<Result<_, _>>()?;
            let p = cstr(&path)?;
            let mut buf = vec![0u8; 4096];
            // SAFETY: NUL-terminated strings; `buf` is valid for its length.
            let n = unsafe { libc::lgetxattr(p.as_ptr(), name.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
            let n = usize::try_from(n).map_err(|_| format!("getxattr: {}", io::Error::last_os_error()))?;
            if buf.get(..n) != Some(&want[..]) {
                return Err(format!("xattr is {:?}", buf.get(..n)));
            }
            Ok(())
        }
        _ => Err(format!("kind {kind} with type {file_type:o}")),
    }
}
