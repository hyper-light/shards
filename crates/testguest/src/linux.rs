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
            "work" => work(),
            "kmsg" => kmsg(),
            "beat" => beat(),
            "vsock" => vsock(),
            "vsock_snapshot" => vsock_snapshot(),
            "vsock_snapshot_held" => vsock_snapshot_held(),
            "pmem" => pmem(),
            "erofs" => erofs(),
            other => Err(format!("unknown test {other:?}")),
        },
    );
    let line = match result {
        Ok(()) => "SHARDS-TEST PASS".to_string(),
        Err(e) => format!("SHARDS-TEST FAIL {e}"),
    };
    let _ = writeln!(io::stdout(), "{line}");
    // SAFETY: PID 1 flushing and powering off.
    unsafe {
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
                    let now = monotonic_ns();
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
    let _ = writeln!(io::stdout(), "SHARDS-TEST INFO generation={generation}");

    let _ = writeln!(
        io::stdout(),
        "SHARDS-TEST INFO longest gaps before: {}; longest turns taken: {}",
        gaps(&storm, true),
        spread(&storm.late, true)
    );
    // Every CPU's clock goes on from the snapshot in step with the others'.
    clock_in_step(cpus, "after the snapshot")?;
    let seen: Vec<u64> = storm.progress.iter().map(|p| p.load(Ordering::Relaxed)).collect();
    // Long enough for 64 vCPUs sharing a smaller host's cores, short of what a lost
    // completion or timer would cost: those never end.
    thread::sleep(Duration::from_secs(1));
    if let Some(e) = storm
        .failure
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    {
        return Err(e);
    }
    for (i, (p, before)) in storm.progress.iter().zip(&seen).enumerate() {
        if p.load(Ordering::Relaxed) == *before {
            let at = match storm.writer_at.load(Ordering::Relaxed) {
                1 => "reading",
                2 => "writing",
                _ => "between requests",
            };
            let interrupts = std::fs::read_to_string("/proc/interrupts").unwrap_or_default();
            let virtio: Vec<&str> = interrupts.lines().filter(|l| l.contains("virtio")).collect();
            return Err(format!(
                "worker {i} of {workers} made no progress in 1 s after the snapshot; longest gaps since: {}; the writer is {at} slot {}; in flight (reads writes): vda {}, vdb {}; {}",
                gaps(&storm, false),
                storm.written.load(Ordering::Relaxed),
                sysfs("/sys/block/vda/inflight").unwrap_or_default(),
                sysfs("/sys/block/vdb/inflight").unwrap_or_default(),
                virtio.join(" | ")
            ));
        }
    }
    stop(&storm);
    // A lost completion never ends, a slow one does: only the host's bound tells them
    // apart, so the guest waits, saying which workers are still in their rounds.
    waiting("the workers' last rounds", || {
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
    // host streamed ended with the guest alive.
    let _ = writeln!(io::stdout(), "SHARDS-TEST INFO storm checked");
    // The host's side of the connection is up only once it has read its OK, after this
    // side's accept: its byte, or its close, says so.
    let mut host = accept_within(&done, Duration::from_secs(60))?;
    let mut byte = [0u8; 1];
    std::io::Read::read(&mut host, &mut byte)
        .map(drop)
        .map_err(|e| format!("waiting for the host's word: {e}"))
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

/// A booted guest at rest, for measuring any VMM that holds it (the Firecracker
/// comparison): prints `SHARDS-TEST READY` and waits for the host to end the VM. It uses
/// nothing shards-specific, so every VMM runs the same guest.
fn idle() -> Result<(), String> {
    let _ = writeln!(io::stdout(), "SHARDS-TEST READY");
    loop {
        // SAFETY: pause(2) takes no arguments; it returns only after a signal handler ran.
        unsafe { libc::pause() };
    }
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
        return Err(format!("mount erofs: {}", io::Error::last_os_error()));
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
