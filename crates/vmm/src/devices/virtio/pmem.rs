//! virtio-pmem (virtio 1.3 §5.19): a host file mapped into guest physical memory. The
//! guest sees `/dev/pmemN` and can mount it with DAX, so file data never enters guest RAM
//! or its snapshots (docs/research/image-storage.md R2).
//!
//! Read-only: images are immutable, so a flush has nothing to write back, and every VM
//! that maps one file, restored copies included, shares one copy in the host page cache.
//! A guest's swap window ([`SwapWindow`]) is the one writable kind: a run's swap.

use std::fs::File;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::queue::{Chain, Queue};
use super::worker::Worker;
use super::{Activation, DeviceInterrupt, VirtioDevice, feature};
use crate::memory::{Access, GuestMemory};
use crate::platform;
use crate::snapshot::codec::{self, Reader, Writer};
use crate::sync::lock;

pub const DEVICE_ID: u32 = 27;
const QUEUE_SIZE: u16 = 64;
/// Regions are 2 MiB aligned and sized: the size the guest's memory hotplug and DAX
/// work in (Firecracker docs/pmem.md).
pub const ALIGN: u64 = 2 << 20;
/// VIRTIO_PMEM_REQ_TYPE_FLUSH (include/uapi/linux/virtio_pmem.h).
const REQ_FLUSH: u32 = 0;

/// A file mapped read-only into this process for a guest. The length is the file's size
/// rounded up to `ALIGN`; past the end of the file the region reads as zeros.
pub struct Region {
    host: NonNull<u8>,
    /// The bytes of it that are a mapping of the file: past them, zeros, and on Windows the
    /// file's last partial page copied (platform::map_file_padded).
    viewed: usize,
    len: usize,
    _file: File,
}

// SAFETY: the mapping is read-only and owned by this value; it is only read, by the
// hypervisor and the guest.
unsafe impl Send for Region {}
// SAFETY: as above.
unsafe impl Sync for Region {}

impl std::fmt::Debug for Region {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Region")
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

impl Region {
    pub fn open(path: &Path) -> Result<Region, String> {
        let err = |e: std::io::Error| format!("{}: {e}", path.display());
        let file = crate::platform::open_input(path, false).map_err(err)?;
        let size = file.metadata().map_err(err)?.len();
        if size == 0 {
            return Err(format!("{}: an empty file cannot back pmem", path.display()));
        }
        let len = size
            .checked_next_multiple_of(ALIGN)
            .and_then(|l| usize::try_from(l).ok())
            .ok_or_else(|| format!("{}: {size} bytes is too large", path.display()))?;
        let (host, viewed) = platform::map_file_padded(&file, size, len).map_err(err)?;
        Ok(Region {
            host,
            viewed,
            len,
            _file: file,
        })
    }

    pub fn host(&self) -> *mut u8 {
        self.host.as_ptr()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: the mapping `open` made; the VM that mapped it is gone (`Running` drops
        // the VM before the devices).
        unsafe { platform::unmap_file_padded(self.host, self.viewed, self.len) };
    }
}

/// What maps a swap window's memory into the guest while the VM lives (D159): slots of
/// its own, each mapped or unmapped by its index, as KVM numbers them (other hypervisors
/// map by address).
pub trait Slots: Send + Sync {
    /// Maps `len` bytes at `host` at `gpa`, as slot `i`, writable by the guest only if
    /// `writable`.
    ///
    /// # Safety
    /// `host..host+len` must stay mapped in this process until slot `i` is unmapped or the
    /// VM is destroyed.
    unsafe fn map(&self, i: u32, host: *mut u8, gpa: u64, len: usize, writable: bool) -> Result<(), String>;

    /// Unmaps slot `i`, mapped at `gpa` for `len` bytes.
    fn unmap(&self, i: u32, gpa: u64, len: usize) -> Result<(), String>;
}

/// The slots of a swap window: its head's zeros, and a run's file.
pub const SWAP_SLOTS: u32 = 2;
const HEAD_SLOT: u32 = 0;
const RUN_SLOT: u32 = 1;
/// The window's head, zeros until a run's file is mapped over it: what the guest's kernel
/// reads of the disk as it adds it, its partition scan reading the first sectors
/// (block/partitions/core.c, read_part_sector), whose reads of nothing would fail.
const HEAD: usize = ALIGN as usize;

/// A guest's swap window (D159): a virtio-pmem device each run's swap file is mapped
/// into at its start, as long as the most swap a run of the VM may have. The guest's CPUs
/// read and write it as memory, so its kernel completes each swap write before reclaim
/// goes on, and frees what it wrote at once (pmem is BLK_FEAT_SYNCHRONOUS; mm/vmscan.c,
/// shrink_folio_list, "a synchronous write"): reclaim under a memory limit never waits on
/// a host thread. A virtio-blk device's writes complete on the VMM's worker, and reclaim
/// under a cgroup's limit does not wait for them: anonymous folios under writeback are
/// passed over, and 16 rounds without progress call the OOM killer (mm/vmscan.c,
/// folio_check_dirty_writeback; mm/memcontrol.c, try_charge_memcg), which killed a
/// workload within its swap (PM M230).
///
/// Its head reads as zeros until a run's file is mapped; past it nothing is mapped, and
/// nothing reaches there: swap writes no further than its area's header says.
pub struct SwapWindow {
    gpa: u64,
    len: u64,
    /// Whether the guest has been shown the device (`control::SHOW_SWAP`).
    shown: AtomicBool,
    slots: Box<dyn Slots>,
    /// The head's zeros, mapped while no run's file is over them: the window's for its
    /// life, a reservation never touched.
    head: Zeros,
    /// A run's file, mapped at the window's start, once given.
    run: Mutex<Option<Shared>>,
}

impl std::fmt::Debug for SwapWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SwapWindow")
            .field("gpa", &self.gpa)
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

impl SwapWindow {
    /// A window of `len` bytes at `gpa`, whole multiples of [`ALIGN`], its head mapped
    /// read-only zeros through `slots`, [`SWAP_SLOTS`] of them.
    pub fn new(gpa: u64, len: u64, slots: Box<dyn Slots>) -> Result<SwapWindow, String> {
        if len < HEAD as u64 || !len.is_multiple_of(ALIGN) || !gpa.is_multiple_of(ALIGN) {
            return Err(format!(
                "a swap window of {len} bytes at {gpa:#x}: whole {} MiB at a {} MiB boundary",
                ALIGN >> 20,
                ALIGN >> 20
            ));
        }
        let head = Zeros::new(HEAD).map_err(|e| format!("a swap window's head: {e}"))?;
        // SAFETY: the zeros stay mapped until unmapped here, or until the window drops
        // after the VM (`Running` drops the VM before the devices).
        unsafe { slots.map(HEAD_SLOT, head.host(), gpa, HEAD, false) }?;
        Ok(SwapWindow {
            gpa,
            len,
            shown: AtomicBool::new(false),
            slots,
            head,
            run: Mutex::new(None),
        })
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Shows the guest the device: its transport has one from here on.
    pub fn show(&self) {
        self.shown.store(true, Ordering::Release);
    }

    pub fn shown(&self) -> bool {
        self.shown.load(Ordering::Acquire)
    }

    /// Maps `file`, a run's swap, at the window's start in place of its head, writable by
    /// the guest; returns its length. It is whole multiples of [`ALIGN`], at least the head;
    /// one longer than the window is cut to it, all the guest can reach (`vm::swap_layout`).
    /// Where a run's is mapped already, nothing: the microVM's members share its swap, as a
    /// host's containers share its own, each held to its allowance by its cgroup.
    pub fn attach(&self, file: File) -> Result<Option<u64>, String> {
        let mut run = lock(&self.run);
        if run.is_some() {
            return Ok(None);
        }
        let mut size = file.metadata().map_err(|e| format!("its swap file: {e}"))?.len();
        if size > self.len {
            file.set_len(self.len)
                .map_err(|e| format!("its swap file: {e}"))?;
            size = self.len;
        }
        if size < HEAD as u64 || !size.is_multiple_of(ALIGN) {
            return Err(format!(
                "a swap file of {size} bytes for a window of {}: whole {} MiB, from {} MiB",
                self.len,
                ALIGN >> 20,
                HEAD >> 20
            ));
        }
        let len = usize::try_from(size).map_err(|_| format!("a swap file of {size} bytes"))?;
        // Its first page written here, through the file's own path, where the guest writes
        // the swap area's header: written first through the mapping, a page of the sparse
        // file took the host's fault path to be made, 3.4 ms at p90 and 80 ms at worst (PM
        // M230). 64 KiB, the largest page a guest has.
        write_zeros(&file, FIRST_PAGE).map_err(|e| format!("its swap file: {e}"))?;
        let mapped = Shared::map(file, len).map_err(|e| format!("its swap file: {e}"))?;
        self.slots.unmap(HEAD_SLOT, self.gpa, HEAD)?;
        // SAFETY: the mapping stays until the window drops, after the VM.
        if let Err(e) = unsafe { self.slots.map(RUN_SLOT, mapped.host(), self.gpa, len, true) } {
            // The head again, so that the disk reads as it did.
            // SAFETY: as in `new`.
            let _ = unsafe { self.slots.map(HEAD_SLOT, self.head.host(), self.gpa, HEAD, false) };
            return Err(e);
        }
        *run = Some(mapped);
        Ok(Some(size))
    }

    /// Whether a run's file is mapped: a snapshot keeps no run's swap.
    fn attached(&self) -> bool {
        lock(&self.run).is_some()
    }
}

/// The most of a run's swap file written before it is mapped: the largest page a guest
/// kernel has (arm64's 64 KiB), which holds its swap area's header.
const FIRST_PAGE: usize = 64 << 10;

/// Writes `len` zero bytes at the start of `file`, through its own path.
fn write_zeros(file: &File, len: usize) -> std::io::Result<()> {
    let zeros = vec![0u8; len];
    let mut at = 0;
    while at < len {
        let rest = zeros.get(at..).unwrap_or_default();
        // SAFETY: a buffer of `rest.len()` bytes, read alone.
        let n = unsafe { platform::write_at(file, rest.as_ptr(), rest.len(), at as u64) }?;
        if n == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::WriteZero));
        }
        at += n;
    }
    Ok(())
}

/// Read-only zeros for a window's head: an anonymous reservation never written.
struct Zeros {
    host: NonNull<u8>,
    len: usize,
}

// SAFETY: the reservation is owned by this value and only ever read, by the hypervisor.
unsafe impl Send for Zeros {}
// SAFETY: as above.
unsafe impl Sync for Zeros {}

impl Zeros {
    fn new(len: usize) -> std::io::Result<Zeros> {
        Ok(Zeros {
            host: platform::reserve(len)?,
            len,
        })
    }

    fn host(&self) -> *mut u8 {
        self.host.as_ptr()
    }
}

impl Drop for Zeros {
    fn drop(&mut self) {
        // SAFETY: the reservation `new` made; the VM that mapped it is gone.
        unsafe { platform::release(self.host, self.len) };
    }
}

/// A run's swap file, mapped shared and writable into this process.
struct Shared {
    host: NonNull<u8>,
    len: usize,
    _file: File,
}

// SAFETY: the mapping is owned by this value; the guest writes it, through the
// hypervisor, and this process never touches it.
unsafe impl Send for Shared {}
// SAFETY: as above.
unsafe impl Sync for Shared {}

impl Shared {
    fn map(file: File, len: usize) -> std::io::Result<Shared> {
        let host = platform::map_file_shared(&file, len)?;
        Ok(Shared {
            host,
            len,
            _file: file,
        })
    }

    fn host(&self) -> *mut u8 {
        self.host.as_ptr()
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // SAFETY: the mapping `map` made; the VM that mapped it is gone.
        unsafe { platform::unmap_file_shared(self.host, self.len) };
    }
}

/// What a virtio-pmem device is backed by.
enum Backing {
    /// An image file, read-only.
    File(Arc<Region>),
    /// A guest's swap window (D159).
    Swap(Arc<SwapWindow>),
}

pub struct Pmem {
    backing: Backing,
    /// Where the guest sees the region.
    gpa: u64,
    /// What a worker needs besides its queue, from activation until reset.
    context: Option<(Arc<GuestMemory>, Arc<DeviceInterrupt>)>,
    worker: Option<Worker>,
    /// The queue while paused.
    paused: Option<Queue>,
}

impl std::fmt::Debug for Pmem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("Pmem");
        d.field("gpa", &self.gpa);
        match &self.backing {
            Backing::File(region) => d.field("region", region),
            Backing::Swap(window) => d.field("swap", window),
        };
        d.finish_non_exhaustive()
    }
}

impl Pmem {
    /// A device for `region`, which the machine has mapped at `gpa`.
    pub fn new(region: Arc<Region>, gpa: u64) -> Pmem {
        Pmem {
            backing: Backing::File(region),
            gpa,
            context: None,
            worker: None,
            paused: None,
        }
    }

    /// The device of a guest's swap window (D159).
    pub fn swap(window: Arc<SwapWindow>) -> Pmem {
        Pmem {
            gpa: window.gpa,
            backing: Backing::Swap(window),
            context: None,
            worker: None,
            paused: None,
        }
    }

    /// The length the guest sees.
    fn len(&self) -> u64 {
        match &self.backing {
            Backing::File(region) => region.len() as u64,
            Backing::Swap(window) => window.len(),
        }
    }

    /// Starts the worker on `queue`; if it cannot, the queue waits paused, as a snapshot's
    /// pause leaves it, for a resume or a reset.
    fn start(&mut self, queue: Queue) -> Result<(), String> {
        let Some((memory, interrupt)) = self.context.clone() else {
            self.paused = Some(queue);
            return Err("virtio-pmem started before activation".into());
        };
        let mem = memory.clone();
        let answering = move |chain: &Chain| mem.access().map_or(0, |a| answer(chain, &a));
        match Worker::start("virtio-pmem", queue, memory, interrupt, answering) {
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

    /// Stops the worker after the request it is answering; returns its queue.
    fn stop(&mut self) -> Option<Queue> {
        self.worker.take()?.stop()
    }
}

impl VirtioDevice for Pmem {
    /// A swap window's is no device (0) until the guest is shown it (`control::SHOW_SWAP`).
    fn device_id(&self) -> u32 {
        match &self.backing {
            Backing::Swap(window) if !window.shown() => 0,
            _ => DEVICE_ID,
        }
    }

    fn features(&self) -> u64 {
        feature::VERSION_1
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &[QUEUE_SIZE]
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // struct virtio_pmem_config: le64 start, le64 size.
        let mut config = [0u8; 16];
        let (start, size) = config.split_at_mut(8);
        start.copy_from_slice(&self.gpa.to_le_bytes());
        size.copy_from_slice(&self.len().to_le_bytes());
        let at = offset as usize;
        for (i, b) in data.iter_mut().enumerate() {
            *b = at
                .checked_add(i)
                .and_then(|i| config.get(i))
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
        let queue = queues.pop().ok_or("virtio-pmem activated without a queue")?;
        self.context = Some((memory, interrupt));
        // An activation that fails leaves its queue: the driver sets the device up anew.
        self.start(queue).inspect_err(|_| self.paused = None)
    }

    /// Flushes are answered on the device's worker, never on the notifying vCPU's thread,
    /// which a driver refilling the queue from another CPU could otherwise keep from the
    /// guest and from stopping (audit A09).
    fn notify(&self, _queue: u16) {
        if let Some(w) = &self.worker {
            w.notify();
        }
    }

    fn reset(&mut self) {
        self.stop();
        self.paused = None;
        self.context = None;
    }

    /// As block's: the worker stops after the request it is answering, and what it left
    /// stays in the ring for the worker that resumes.
    fn pause(&mut self) -> Vec<super::QueueState> {
        if let Some(queue) = self.stop() {
            self.paused = Some(queue);
        }
        self.paused.iter().map(Queue::state).collect()
    }

    fn resume(&mut self) -> Result<(), String> {
        match self.paused.take() {
            Some(queue) => self.start(queue),
            None => Ok(()),
        }
    }

    /// A swap window's: whether the guest was shown it.
    fn save(&self, w: &mut Writer) {
        if let Backing::Swap(window) = &self.backing {
            w.bool(window.shown());
        }
    }

    fn restore(&mut self, r: &mut Reader<'_>) -> codec::Result<()> {
        if let Backing::Swap(window) = &self.backing
            && r.bool()?
        {
            window.show();
        }
        Ok(())
    }

    /// A run's swap is the run's alone: no template or snapshot holds it.
    fn unsaved(&self) -> Option<String> {
        match &self.backing {
            Backing::Swap(window) if window.attached() => {
                Some("a run's swap is mapped into the microVM's swap window".into())
            }
            _ => None,
        }
    }
}

impl Drop for Pmem {
    fn drop(&mut self) {
        self.reset();
    }
}

/// Reads `struct virtio_pmem_req { le32 type; }` and writes `struct virtio_pmem_resp
/// { le32 ret; }`: 0 for a flush, 1 for anything else. Returns the bytes written.
fn answer(chain: &Chain, mem: &Access<'_>) -> u32 {
    let (Some(req), Some(resp)) = (chain.readable().next(), chain.writable().next()) else {
        return 0;
    };
    if req.len < 4 || resp.len < 4 {
        return 0;
    }
    let ret: u32 = match mem.read_obj::<u32>(req.addr) {
        Ok(t) if u32::from_le(t) == REQ_FLUSH => 0,
        _ => 1,
    };
    match mem.write_obj(resp.addr, ret.to_le()) {
        Ok(()) => 4,
        Err(_) => 0,
    }
}
