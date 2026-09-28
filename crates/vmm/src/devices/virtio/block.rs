//! virtio-blk backed by a host file (virtio 1.3 §5.2).
//!
//! Data moves directly between the file and guest memory: the device passes raw guest
//! pointers to `pread`/`pwrite`, so no Rust reference ever aliases memory the guest
//! may modify concurrently.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle, Thread};

use super::queue::{Chain, Descriptor, Queue};
use super::{Activation, DeviceInterrupt, VirtioDevice, feature};
use crate::memory::GuestMemory;
use crate::{debug, warn};

pub const DEVICE_ID: u32 = 2;
const SECTOR: u64 = 512;
const QUEUE_SIZE: u16 = 256;
/// Data segments per request advertised to the driver (VIRTIO_BLK_F_SEG_MAX).
const SEG_MAX: u32 = 254;
const ID_BYTES: usize = 20;

const F_SEG_MAX: u64 = 1 << 2;
const F_RO: u64 = 1 << 5;
const F_BLK_SIZE: u64 = 1 << 6;
const F_FLUSH: u64 = 1 << 9;

const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const T_GET_ID: u32 = 8;

const S_OK: u8 = 0;
const S_IOERR: u8 = 1;
const S_UNSUPP: u8 = 2;

/// Host-file backing store.
#[derive(Debug)]
struct Backend {
    file: File,
    capacity: u64,
    read_only: bool,
    id: [u8; ID_BYTES],
}

struct Worker {
    thread: JoinHandle<()>,
    waker: Thread,
    stop: Arc<AtomicBool>,
}

pub struct Block {
    backend: Arc<Backend>,
    worker: Option<Worker>,
}

impl std::fmt::Debug for Block {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Block")
            .field("backend", &self.backend)
            .finish_non_exhaustive()
    }
}

impl Block {
    /// Opens `path` as a disk. Its size must be a whole number of 512-byte sectors.
    pub fn open(path: &Path, read_only: bool, id: &str) -> Result<Block, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(!read_only)
            .open(path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let bytes = file
            .metadata()
            .map_err(|e| format!("{}: {e}", path.display()))?
            .len();
        if bytes % SECTOR != 0 {
            return Err(format!(
                "{}: size {bytes} is not a multiple of {SECTOR}",
                path.display()
            ));
        }
        let mut serial = [0u8; ID_BYTES];
        for (d, s) in serial.iter_mut().zip(id.bytes()) {
            *d = s;
        }
        Ok(Block {
            backend: Arc::new(Backend {
                file,
                capacity: bytes / SECTOR,
                read_only,
                id: serial,
            }),
            worker: None,
        })
    }
}

impl VirtioDevice for Block {
    fn device_id(&self) -> u32 {
        DEVICE_ID
    }

    fn features(&self) -> u64 {
        let ro = if self.backend.read_only { F_RO } else { 0 };
        feature::VERSION_1
            | feature::EVENT_IDX
            | feature::INDIRECT_DESC
            | F_SEG_MAX
            | F_BLK_SIZE
            | F_FLUSH
            | ro
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &[QUEUE_SIZE]
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // struct virtio_blk_config (§5.2.4): capacity @0, size_max @8, seg_max @12,
        // geometry @16, blk_size @20; everything else reads as zero.
        let mut config = [0u8; 24];
        config[0..8].copy_from_slice(&self.backend.capacity.to_le_bytes());
        config[12..16].copy_from_slice(&SEG_MAX.to_le_bytes());
        config[20..24].copy_from_slice(&(SECTOR as u32).to_le_bytes());
        let start = offset as usize;
        for (i, b) in data.iter_mut().enumerate() {
            *b = start
                .checked_add(i)
                .and_then(|at| config.get(at))
                .copied()
                .unwrap_or(0);
        }
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}

    fn activate(&mut self, activation: Activation) -> Result<(), String> {
        let Activation {
            memory,
            mut queues,
            interrupt,
            ..
        } = activation;
        let queue = queues.pop().ok_or("virtio-blk activated without a queue")?;
        let stop = Arc::new(AtomicBool::new(false));
        let backend = self.backend.clone();
        let stop_flag = stop.clone();
        let thread = thread::Builder::new()
            .name("virtio-blk".into())
            .spawn(move || serve(queue, &memory, &interrupt, &backend, &stop_flag))
            .map_err(|e| format!("spawning virtio-blk worker: {e}"))?;
        let waker = thread.thread().clone();
        self.worker = Some(Worker { thread, waker, stop });
        Ok(())
    }

    fn notify(&self, _queue: u16) {
        if let Some(w) = &self.worker {
            w.waker.unpark();
        }
    }

    fn reset(&mut self) {
        if let Some(w) = self.worker.take() {
            w.stop.store(true, Ordering::Release);
            w.waker.unpark();
            if w.thread.join().is_err() {
                warn!("virtio-blk worker ended abnormally");
            }
        }
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        self.reset();
    }
}

/// Drains the queue whenever notified, until stopped. A malformed ring marks the device
/// as needing reset and stops processing (virtio 1.3 §2.1.2).
fn serve(mut queue: Queue, mem: &GuestMemory, irq: &DeviceInterrupt, backend: &Backend, stop: &AtomicBool) {
    while !stop.load(Ordering::Acquire) {
        if let Err(e) = drain(&mut queue, mem, irq, backend) {
            warn!("virtio-blk: {e}; device needs reset");
            irq.fail();
            return;
        }
        thread::park();
    }
}

fn drain(
    queue: &mut Queue,
    mem: &GuestMemory,
    irq: &DeviceInterrupt,
    backend: &Backend,
) -> Result<(), String> {
    let e = |e: super::queue::QueueError| e.to_string();
    loop {
        queue.disable_notification(mem).map_err(e)?;
        let mut completed = false;
        while let Some(chain) = queue.pop(mem).map_err(e)? {
            let written = handle(&chain, mem, backend);
            queue.add_used(mem, chain.head, written).map_err(e)?;
            completed = true;
        }
        if completed && queue.needs_interrupt(mem).map_err(e)? {
            irq.used_buffer();
        }
        if !queue.enable_notification(mem).map_err(e)? {
            return Ok(());
        }
    }
}

/// Executes one request; returns the bytes written into the driver's buffers.
fn handle(chain: &Chain, mem: &GuestMemory, backend: &Backend) -> u32 {
    // The status byte is the last byte of the last writable descriptor.
    let Some(last) = chain.writable().last().copied() else {
        debug!("virtio-blk: request without a status byte");
        return 0;
    };
    if last.len == 0 {
        return 0;
    }
    let Some(status_at) = last.addr.checked_add(u64::from(last.len) - 1) else {
        return 0;
    };
    let (status, data_written) = match execute(chain, last, mem, backend) {
        Ok(n) => (S_OK, n),
        Err(status) => (status, 0),
    };
    if mem.write_obj(status_at, status).is_err() {
        return 0;
    }
    data_written.saturating_add(1)
}

/// Returns bytes of data written into guest buffers, or a status code on failure.
fn execute(chain: &Chain, status: Descriptor, mem: &GuestMemory, backend: &Backend) -> Result<u32, u8> {
    let mut header = [0u8; 16];
    let body = gather_header(chain, mem, &mut header)?;
    let kind = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let sector = u64::from_le_bytes([
        header[8], header[9], header[10], header[11], header[12], header[13], header[14], header[15],
    ]);
    // Writable buffers except the trailing status byte.
    let out: Vec<Descriptor> = writable_data(chain, status);
    match kind {
        T_IN => transfer(&out, sector, mem, backend, Direction::Read),
        T_OUT if backend.read_only => Err(S_IOERR),
        T_OUT => transfer(&body, sector, mem, backend, Direction::Write).map(|_| 0),
        T_FLUSH => flush(&backend.file).map(|()| 0),
        T_GET_ID => {
            let mut written = 0u32;
            let mut id = backend.id.as_slice();
            for d in &out {
                let n = id.len().min(d.len as usize);
                let (chunk, rest) = id.split_at(n);
                mem.write(d.addr, chunk).map_err(|_| S_IOERR)?;
                written += n as u32;
                id = rest;
            }
            Ok(written)
        }
        _ => Err(S_UNSUPP),
    }
}

/// Copies the 16-byte request header out of the readable descriptors and returns the
/// readable data that follows it (framing is not assumed, per VIRTIO 1.x).
fn gather_header(chain: &Chain, mem: &GuestMemory, header: &mut [u8; 16]) -> Result<Vec<Descriptor>, u8> {
    let mut filled = 0usize;
    let mut body = Vec::new();
    for d in chain.readable() {
        let take = (header.len() - filled).min(d.len as usize);
        if take > 0 {
            let dst = header.get_mut(filled..filled + take).ok_or(S_IOERR)?;
            mem.read(d.addr, dst).map_err(|_| S_IOERR)?;
            filled += take;
        }
        let rest = d.len - take as u32;
        if rest > 0 {
            body.push(Descriptor {
                addr: d.addr.checked_add(take as u64).ok_or(S_IOERR)?,
                len: rest,
                writable: false,
            });
        }
    }
    if filled == header.len() {
        Ok(body)
    } else {
        Err(S_IOERR)
    }
}

fn writable_data(chain: &Chain, status: Descriptor) -> Vec<Descriptor> {
    let mut out: Vec<Descriptor> = chain.writable().copied().collect();
    if let Some(last) = out.last_mut()
        && *last == status
    {
        last.len -= 1;
    }
    out.retain(|d| d.len > 0);
    out
}

#[derive(Clone, Copy)]
enum Direction {
    Read,
    Write,
}

fn transfer(
    bufs: &[Descriptor],
    sector: u64,
    mem: &GuestMemory,
    backend: &Backend,
    dir: Direction,
) -> Result<u32, u8> {
    let total: u64 = bufs.iter().map(|d| u64::from(d.len)).sum();
    let start = sector.checked_mul(SECTOR).ok_or(S_IOERR)?;
    let end = start.checked_add(total).ok_or(S_IOERR)?;
    if end > backend.capacity * SECTOR {
        return Err(S_IOERR);
    }
    let fd = backend.file.as_raw_fd();
    let mut offset = start;
    for d in bufs {
        let ptr = mem.host_ptr(d.addr, d.len as usize).map_err(|_| S_IOERR)?;
        let mut done = 0usize;
        while done < d.len as usize {
            let remaining = d.len as usize - done;
            // SAFETY: `ptr + done .. ptr + len` lies inside guest RAM (host_ptr checked
            // the whole descriptor); the kernel copies to/from it without Rust aliasing.
            let n = unsafe {
                let p = ptr.add(done);
                match dir {
                    Direction::Read => libc::pread(fd, p.cast(), remaining, offset as libc::off_t),
                    Direction::Write => libc::pwrite(fd, p.cast(), remaining, offset as libc::off_t),
                }
            };
            match n {
                n if n > 0 => {
                    done += n as usize;
                    offset += n as u64;
                }
                0 => return Err(S_IOERR), // unexpected end of file
                _ if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {}
                _ => return Err(S_IOERR),
            }
        }
    }
    u32::try_from(total).map_err(|_| S_IOERR)
}

/// Makes completed writes durable. On macOS `fsync` does not flush the drive's cache;
/// `F_FULLFSYNC` does (fsync(2), fcntl(2)).
fn flush(file: &File) -> Result<(), u8> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: fcntl on an owned, open descriptor.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } == 0 {
            return Ok(());
        }
    }
    file.sync_data().map_err(|_| S_IOERR)
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn config_space_reports_capacity_segments_and_block_size() {
        let dir = std::env::temp_dir().join(format!("shards-blk-{}", std::process::id()));
        std::fs::write(&dir, vec![0u8; 8 * 512]).unwrap();
        let b = Block::open(&dir, true, "disk0").unwrap();
        let mut cap = [0u8; 8];
        b.read_config(0, &mut cap);
        assert_eq!(u64::from_le_bytes(cap), 8);
        let mut seg = [0u8; 4];
        b.read_config(12, &mut seg);
        assert_eq!(u32::from_le_bytes(seg), SEG_MAX);
        let mut beyond = [0xffu8; 4];
        b.read_config(4096, &mut beyond);
        assert_eq!(beyond, [0; 4]);
        assert_ne!(b.features() & F_RO, 0);
        std::fs::write(&dir, vec![0u8; 513]).unwrap();
        assert!(Block::open(&dir, true, "x").is_err());
        let _ = std::fs::remove_file(&dir);
    }
}
