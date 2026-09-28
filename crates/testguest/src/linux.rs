use std::ffi::CString;
use std::io::{self, Write};
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
            "resume" => resume(),
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

/// The benchmark guest: asks for a snapshot; a restored clone marks that it runs again
/// and powers off at once, so restore timings contain no guest work.
fn resume() -> Result<(), String> {
    let control = ControlPage::map()?;
    control.write(shards_abi::control::SNAPSHOT, shards_abi::control::SNAPSHOT_NOW);
    control.write(shards_abi::control::MARKER, shards_abi::marker::RESUMED);
    Ok(())
}
