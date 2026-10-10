//! virtio-blk backed by a host file (virtio 1.3 §5.2), or a join disk: the ranges the
//! images of containers joining the microVM were given (D119).
//!
//! Data moves directly between the file and guest memory: the device passes raw guest
//! pointers to positional reads and writes, so no Rust reference ever aliases memory the guest
//! may modify concurrently.

use std::fs::File;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use super::queue::{Chain, Descriptor, Queue};
use super::worker::Worker;
use super::{Activation, DeviceInterrupt, VirtioDevice, feature};
use crate::memory::{Access, GuestMemory};
use crate::{debug, platform};

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

/// Where a join disk's ranges start: each on a boundary of this many bytes.
const RANGE_ALIGN: u64 = 1 << 20;

/// A disk's backing store and identity.
#[derive(Debug)]
struct Backend {
    store: Store,
    read_only: bool,
    id: [u8; ID_BYTES],
}

/// What a disk's sectors are.
#[derive(Debug)]
enum Store {
    /// One host file, of a capacity fixed when it was opened, in sectors.
    File { file: File, capacity: u64 },
    /// A join disk's ranges.
    Join(&'static Join),
}

impl Backend {
    /// The disk's capacity in sectors, as the driver reads it now.
    fn capacity(&self) -> u64 {
        match &self.store {
            Store::File { capacity, .. } => *capacity,
            Store::Join(join) => join.capacity(),
        }
    }
}

/// A read-only disk whose sectors are the images of the containers that joined the
/// microVM (D119), each a range of its own from a [`RANGE_ALIGN`] boundary, in the order
/// they came; the gaps between them read as zeros. It is empty, of no capacity, until a
/// container joins, and only grows: a range let go reads as an I/O error, and its place
/// is never given again, so no range's offset ever names another image.
///
/// A VM process runs one VM: its join disk is a `static` of that process's, which its
/// device and the process's join requests both reach, with no count of holders kept.
#[derive(Debug)]
pub struct Join {
    ranges: RwLock<Vec<Range>>,
    /// The device's interrupt while the driver has it, to say the capacity grew: the
    /// transport's, which it makes for each activation, so a disk that outlives them holds
    /// it while one lasts, taken once as the driver activates the device and let go at its
    /// reset, never on a request's path.
    interrupt: Mutex<Option<Arc<DeviceInterrupt>>>,
}

impl Default for Join {
    fn default() -> Join {
        Join::new()
    }
}

#[derive(Debug)]
struct Range {
    /// Where it starts, in bytes.
    start: u64,
    /// Its length in bytes, a whole number of sectors: its file's.
    len: u64,
    /// Its image, until it is let go.
    file: Option<File>,
}

impl Join {
    /// An empty join disk, for a `static`.
    pub const fn new() -> Join {
        Join {
            ranges: RwLock::new(Vec::new()),
            interrupt: Mutex::new(None),
        }
    }

    /// Gives `file`, an image whose size is a whole number of sectors, a range after the
    /// last, and tells the driver the disk's new capacity. Returns where the range starts
    /// and its length, in bytes.
    pub fn attach(&self, file: File) -> Result<(u64, u64), String> {
        let len = file
            .metadata()
            .map_err(|e| format!("a file for the join disk: {e}"))?
            .len();
        if len == 0 || len % SECTOR != 0 {
            return Err(format!(
                "a file of {len} bytes: a join disk takes whole sectors of {SECTOR}"
            ));
        }
        let start = {
            let mut ranges = self.ranges.write().unwrap_or_else(PoisonError::into_inner);
            let end = ranges.last().map_or(Some(0), |r| r.start.checked_add(r.len));
            let start = end
                .and_then(|e| e.checked_next_multiple_of(RANGE_ALIGN))
                .filter(|s| s.checked_add(len).is_some())
                .ok_or("the join disk is full")?;
            ranges.push(Range {
                start,
                len,
                file: Some(file),
            });
            start
        };
        // Once its range is there: a driver that reads the new capacity reads the image.
        if let Some(interrupt) = &*self.interrupt.lock().unwrap_or_else(PoisonError::into_inner) {
            interrupt.config_change();
        }
        Ok((start, len))
    }

    /// Lets the range that starts at `start` go: its file is closed, and the driver's
    /// reads of it fail from here on.
    pub fn detach(&self, start: u64) -> Result<(), String> {
        let mut ranges = self.ranges.write().unwrap_or_else(PoisonError::into_inner);
        let range = ranges
            .iter_mut()
            .find(|r| r.start == start)
            .ok_or_else(|| format!("no range of the join disk starts at {start}"))?;
        range.file = None;
        Ok(())
    }

    /// Whether no container's image was ever given a range: a disk a snapshot may keep,
    /// as it keeps nothing of a range.
    pub fn is_empty(&self) -> bool {
        self.ranges
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty()
    }

    /// Its capacity in sectors: its last range's end.
    fn capacity(&self) -> u64 {
        let ranges = self.ranges.read().unwrap_or_else(PoisonError::into_inner);
        ranges
            .last()
            .and_then(|r| r.start.checked_add(r.len))
            .map_or(0, |end| end / SECTOR)
    }

    /// Reads up to `len` bytes at `offset` into the guest's memory at `ptr`, which holds
    /// that many: from the range there, or zeros between ranges. Returns how many it
    /// read, or why it read none.
    ///
    /// # Safety
    ///
    /// `ptr .. ptr + len` lies in guest RAM, as `GuestMemory::host_ptr` gives it.
    unsafe fn read_at(&self, ptr: *mut u8, len: usize, offset: u64) -> Result<usize, u8> {
        let ranges = self.ranges.read().unwrap_or_else(PoisonError::into_inner);
        // The last range that starts at or before `offset`.
        let at = ranges.partition_point(|r| r.start <= offset);
        let next = ranges.get(at).map(|r| r.start);
        if let Some(r) = at.checked_sub(1).and_then(|i| ranges.get(i))
            && offset < r.start.saturating_add(r.len)
        {
            let file = r.file.as_ref().ok_or(S_IOERR)?;
            let left = r.start.saturating_add(r.len) - offset;
            let n = usize::try_from(left).map_or(len, |left| left.min(len));
            // SAFETY: the caller's: `ptr .. ptr + n` lies in guest RAM.
            return match unsafe { platform::read_at(file, ptr, n, offset - r.start) } {
                Ok(0) | Err(_) => Err(S_IOERR),
                Ok(n) => Ok(n),
            };
        }
        // Between ranges, or before the first: zeros, up to the next range.
        let gap = next.map_or(len, |s| usize::try_from(s - offset).map_or(len, |g| g.min(len)));
        // SAFETY: the caller's: `ptr .. ptr + gap` lies in guest RAM; no Rust reference
        // to it exists, the kernel's reads aside.
        unsafe { std::ptr::write_bytes(ptr, 0, gap) };
        Ok(gap)
    }
}

/// The worker thread returns its queue when stopped, or nothing if a malformed ring
/// failed the device.
pub struct Block {
    backend: Arc<Backend>,
    /// What a worker needs besides its queue, from activation until reset.
    context: Option<(Arc<GuestMemory>, Arc<DeviceInterrupt>)>,
    worker: Option<Worker>,
    /// The queue while paused.
    paused: Option<Queue>,
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
        let file =
            crate::platform::open_input(path, !read_only).map_err(|e| format!("{}: {e}", path.display()))?;
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
        Ok(Block::of(Backend {
            store: Store::File {
                file,
                capacity: bytes / SECTOR,
            },
            read_only,
            id: serial(id),
        }))
    }

    /// A join disk (D119), its serial `id`: read-only, empty until `disk` is given a
    /// range.
    pub fn join(disk: &'static Join, id: &str) -> Block {
        Block::of(Backend {
            store: Store::Join(disk),
            read_only: true,
            id: serial(id),
        })
    }

    fn of(backend: Backend) -> Block {
        Block {
            backend: Arc::new(backend),
            context: None,
            worker: None,
            paused: None,
        }
    }

    /// Starts the worker on `queue`; if it cannot, the queue waits paused, as a snapshot's
    /// pause leaves it, for a resume or a reset.
    fn start(&mut self, queue: Queue) -> Result<(), String> {
        let Some((memory, interrupt)) = self.context.clone() else {
            self.paused = Some(queue);
            return Err("virtio-blk started before activation".into());
        };
        let (backend, mem) = (self.backend.clone(), memory.clone());
        let answer = move |chain: &Chain| handle(chain, &mem, &backend);
        match Worker::start("virtio-blk", queue, memory, interrupt, answer) {
            Ok(worker) => {
                self.worker = Some(worker);
                Ok(())
            }
            Err((e, queue)) => {
                self.paused = Some(queue);
                Err(e)
            }
        }
    }

    /// Stops the worker after the request it is executing; returns its queue.
    fn stop(&mut self) -> Option<Queue> {
        self.worker.take()?.stop()
    }

    /// A join disk's interrupt, given at activation and taken at reset: who tells the
    /// driver its capacity grew.
    fn join_interrupt(&self, interrupt: Option<Arc<DeviceInterrupt>>) {
        if let Store::Join(join) = &self.backend.store {
            *join.interrupt.lock().unwrap_or_else(PoisonError::into_inner) = interrupt;
        }
    }
}

/// `id` as a virtio-blk serial: its first [`ID_BYTES`] bytes, NUL-padded.
fn serial(id: &str) -> [u8; ID_BYTES] {
    let mut serial = [0u8; ID_BYTES];
    for (d, s) in serial.iter_mut().zip(id.bytes()) {
        *d = s;
    }
    serial
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
        config[0..8].copy_from_slice(&self.backend.capacity().to_le_bytes());
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
        self.join_interrupt(Some(interrupt.clone()));
        self.context = Some((memory, interrupt));
        // An activation that fails leaves its queue: the driver sets the device up anew.
        self.start(queue).inspect_err(|_| self.paused = None)
    }

    fn notify(&self, _queue: u16) {
        if let Some(w) = &self.worker {
            w.notify();
        }
    }

    fn reset(&mut self) {
        self.stop();
        self.paused = None;
        self.context = None;
        self.join_interrupt(None);
    }

    /// The worker stops after the request it is answering. Whatever the driver published
    /// and it did not answer stays in the ring, for the worker that resumes.
    fn pause(&mut self) -> Vec<super::QueueState> {
        if let Some(queue) = self.stop() {
            self.paused = Some(queue);
        }
        self.paused.iter().map(Queue::state).collect()
    }

    /// The new worker drains first, so requests published meanwhile are served.
    fn resume(&mut self) -> Result<(), String> {
        match self.paused.take() {
            Some(queue) => self.start(queue),
            None => Ok(()),
        }
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        self.reset();
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
    let stored = mem.access().map(|a| a.write_obj(status_at, status));
    if !matches!(stored, Ok(Ok(()))) {
        return 0;
    }
    data_written.saturating_add(1)
}

/// Returns bytes of data written into guest buffers, or a status code on failure.
fn execute(chain: &Chain, status: Descriptor, mem: &GuestMemory, backend: &Backend) -> Result<u32, u8> {
    let mut header = [0u8; 16];
    let body = gather_header(chain, &mem.access().map_err(|_| S_IOERR)?, &mut header)?;
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
        // Durable on stable storage, not just in the host page cache (platform::sync_durable).
        // A join disk is read-only: nothing to make durable.
        T_FLUSH => match &backend.store {
            Store::File { file, .. } => platform::sync_durable(file).map(|()| 0).map_err(|_| S_IOERR),
            Store::Join(_) => Ok(0),
        },
        T_GET_ID => {
            let mut written = 0u32;
            let mut id = backend.id.as_slice();
            let a = mem.access().map_err(|_| S_IOERR)?;
            for d in &out {
                let n = id.len().min(d.len as usize);
                let (chunk, rest) = id.split_at(n);
                a.write(d.addr, chunk).map_err(|_| S_IOERR)?;
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
fn gather_header(chain: &Chain, mem: &Access<'_>, header: &mut [u8; 16]) -> Result<Vec<Descriptor>, u8> {
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
    if end > backend.capacity().saturating_mul(SECTOR) {
        return Err(S_IOERR);
    }
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
                match (&backend.store, dir) {
                    (Store::File { file, .. }, Direction::Read) => {
                        platform::read_at(file, p, remaining, offset).map_err(|_| S_IOERR)
                    }
                    (Store::File { file, .. }, Direction::Write) => {
                        platform::write_at(file, p, remaining, offset).map_err(|_| S_IOERR)
                    }
                    (Store::Join(join), Direction::Read) => join.read_at(p, remaining, offset),
                    (Store::Join(_), Direction::Write) => Err(S_IOERR),
                }
            };
            match n {
                Ok(0) | Err(_) => return Err(S_IOERR), // 0: unexpected end of file
                Ok(n) => {
                    done += n;
                    offset += n as u64;
                }
            }
        }
    }
    u32::try_from(total).map_err(|_| S_IOERR)
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::super::queue::QueueConfig;
    use super::super::worker;
    use super::*;

    struct Line;
    impl crate::devices::Interrupt for Line {
        fn set_level(&self, _: bool) {}
    }

    /// An activation, and a resume after a snapshot's pause, whose worker the system
    /// refuses fail alone: the paused queue stays, and the next activation or resume runs
    /// (review 1.14). Block's and pmem's workers are one.
    #[test]
    fn a_refused_worker_leaves_the_queue_paused() {
        let path = std::env::temp_dir().join(format!("shards-blk-refused-{}", std::process::id()));
        std::fs::write(&path, vec![0u8; 8 * 512]).unwrap();
        let mut b = Block::open(&path, true, "disk0").unwrap();
        const BASE: u64 = 0x8000_0000;
        let mem = Arc::new(GuestMemory::anonymous(&[(BASE, 1 << 16)]).unwrap());
        let irq = Arc::new(DeviceInterrupt::new(Arc::new(Line)));
        let activation = || {
            let cfg = QueueConfig {
                size: 8,
                desc: BASE,
                avail: BASE + 0x1000,
                used: BASE + 0x2000,
                ready: true,
            };
            Activation {
                memory: mem.clone(),
                queues: vec![Queue::new(cfg, 8, &mem, feature::VERSION_1).unwrap()],
                interrupt: irq.clone(),
                features: feature::VERSION_1,
                restored: false,
            }
        };
        worker::REFUSE.set(true);
        let refused = b.activate(activation());
        worker::REFUSE.set(false);
        assert!(refused.unwrap_err().starts_with("spawning the virtio-blk worker"));
        // Its queue gone with it.
        assert!(b.worker.is_none() && b.paused.is_none());
        b.activate(activation()).unwrap();
        assert_eq!(b.pause().len(), 1);
        worker::REFUSE.set(true);
        assert!(b.resume().is_err());
        worker::REFUSE.set(false);
        assert!(b.worker.is_none());
        assert_eq!(b.pause().len(), 1, "paused still");
        b.resume().unwrap();
        assert!(b.worker.is_some() && b.paused.is_none());
        drop(b);
        let _ = std::fs::remove_file(&path);
    }

    /// A join disk (D119): of no capacity until an image is given a range; each range from
    /// a 1 MiB boundary, read as its file reads, the gaps as zeros, one let go as an I/O
    /// error; the driver told each time the capacity grows, while it has the device.
    #[test]
    fn a_join_disk_reads_each_image_at_its_range() {
        let image = |name: &str, len: usize, byte: u8| {
            let path = std::env::temp_dir().join(format!("shards-join-{name}-{}", std::process::id()));
            std::fs::write(&path, vec![byte; len]).unwrap();
            let file = File::open(&path).unwrap();
            std::fs::remove_file(&path).unwrap();
            file
        };
        static DISK: Join = Join::new();
        let disk = &DISK;
        let mut b = Block::join(disk, "shards-join");
        let capacity = |b: &Block| {
            let mut c = [0u8; 8];
            b.read_config(0, &mut c);
            u64::from_le_bytes(c)
        };
        assert!(disk.is_empty());
        assert_eq!(capacity(&b), 0);
        assert_ne!(b.features() & F_RO, 0);
        const BASE: u64 = 0x8000_0000;
        let mem = Arc::new(GuestMemory::anonymous(&[(BASE, 1 << 16)]).unwrap());
        let irq = Arc::new(DeviceInterrupt::new(Arc::new(Line)));
        let cfg = QueueConfig {
            size: 8,
            desc: BASE,
            avail: BASE + 0x1000,
            used: BASE + 0x2000,
            ready: true,
        };
        b.activate(Activation {
            memory: mem.clone(),
            queues: vec![Queue::new(cfg, 8, &mem, feature::VERSION_1).unwrap()],
            interrupt: irq.clone(),
            features: feature::VERSION_1,
            restored: false,
        })
        .unwrap();
        assert_eq!(disk.attach(image("a", 4096, 0xaa)).unwrap(), (0, 4096));
        assert_eq!(capacity(&b), 8);
        assert_ne!(irq.status() & super::super::INT_CONFIG_CHANGE, 0);
        irq.ack(super::super::INT_CONFIG_CHANGE);
        assert_eq!(disk.attach(image("b", 8192, 0xbb)).unwrap(), (1 << 20, 8192));
        assert_eq!(capacity(&b), ((1 << 20) + 8192) / SECTOR);
        assert_ne!(irq.status() & super::super::INT_CONFIG_CHANGE, 0);
        assert!(!disk.is_empty());
        let read = |at: u64, len: usize| {
            let mut buf = vec![0x55u8; len];
            // SAFETY: `buf` holds `len` bytes.
            let n = unsafe { disk.read_at(buf.as_mut_ptr(), len, at) };
            n.map(|n| buf[..n].to_vec())
        };
        assert_eq!(read(0, 4096).unwrap(), vec![0xaa; 4096]);
        // Past a's end, zeros up to b's start, never past it.
        assert_eq!(read(4096, 4096).unwrap(), vec![0; 4096]);
        assert_eq!(read((1 << 20) - 512, 1024).unwrap(), vec![0; 512]);
        assert_eq!(read(1 << 20, 8192).unwrap(), vec![0xbb; 8192]);
        assert_eq!(read((1 << 20) + 4096, 8192).unwrap(), vec![0xbb; 4096]);
        disk.detach(0).unwrap();
        assert_eq!(read(0, 512), Err(S_IOERR));
        assert!(disk.detach(512).is_err());
        // A range let go keeps its place: the next comes after the last.
        assert_eq!(disk.attach(image("c", 512, 0xcc)).unwrap(), (2 << 20, 512));
        for bad in [0, 100] {
            assert!(disk.attach(image("bad", bad, 0)).is_err());
        }
        // Reset: the driver no longer has the device, and is told nothing.
        b.reset();
        irq.ack(super::super::INT_CONFIG_CHANGE);
        disk.attach(image("d", 512, 0xdd)).unwrap();
        assert_eq!(irq.status() & super::super::INT_CONFIG_CHANGE, 0);
    }

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
