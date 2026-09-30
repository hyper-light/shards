//! virtio-pmem (virtio 1.3 §5.19): a host file mapped into guest physical memory. The
//! guest sees `/dev/pmemN` and can mount it with DAX, so file data never enters guest RAM
//! or its snapshots (docs/research/image-storage.md R2).
//!
//! Read-only: images are immutable, so a flush has nothing to write back, and every VM
//! that maps one file, restored copies included, shares one copy in the host page cache.

use std::fs::File;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::Arc;

use super::queue::{Chain, Queue};
use super::worker::Worker;
use super::{Activation, DeviceInterrupt, VirtioDevice, feature};
use crate::memory::{Access, GuestMemory};
use crate::platform;

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
        let file = File::open(path).map_err(err)?;
        let size = file.metadata().map_err(err)?.len();
        if size == 0 {
            return Err(format!("{}: an empty file cannot back pmem", path.display()));
        }
        let len = size
            .checked_next_multiple_of(ALIGN)
            .and_then(|l| usize::try_from(l).ok())
            .ok_or_else(|| format!("{}: {size} bytes is too large", path.display()))?;
        let page = platform::page_size().map_err(err)? as u64;
        let mapped = size.next_multiple_of(page) as usize;
        let host = platform::reserve(len).map_err(err)?;
        // SAFETY: `host..host+len` is a fresh reservation nothing references; `mapped`
        // is page-aligned and within it.
        if let Err(e) = unsafe { platform::map_file_readonly(&file, mapped, host) } {
            // SAFETY: our own reservation, never handed out.
            unsafe { platform::release(host, len) };
            return Err(err(e));
        }
        Ok(Region {
            host,
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
        // SAFETY: the reservation `open` made; the VM that mapped it is gone
        // (`Running` drops the VM before the devices).
        unsafe { platform::release(self.host, self.len) };
    }
}

pub struct Pmem {
    region: Arc<Region>,
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
        f.debug_struct("Pmem")
            .field("gpa", &self.gpa)
            .field("region", &self.region)
            .finish_non_exhaustive()
    }
}

impl Pmem {
    /// A device for `region`, which the machine has mapped at `gpa`.
    pub fn new(region: Arc<Region>, gpa: u64) -> Pmem {
        Pmem {
            region,
            gpa,
            context: None,
            worker: None,
            paused: None,
        }
    }

    fn start(&mut self, queue: Queue) -> Result<(), String> {
        let (memory, interrupt) = self
            .context
            .clone()
            .ok_or("virtio-pmem started before activation")?;
        let mem = memory.clone();
        let answering = move |chain: &Chain| mem.access().map_or(0, |a| answer(chain, &a));
        self.worker = Some(Worker::start("virtio-pmem", queue, memory, interrupt, answering)?);
        Ok(())
    }

    /// Stops the worker after the request it is answering; returns its queue.
    fn stop(&mut self) -> Option<Queue> {
        self.worker.take()?.stop()
    }
}

impl VirtioDevice for Pmem {
    fn device_id(&self) -> u32 {
        DEVICE_ID
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
        size.copy_from_slice(&(self.region.len() as u64).to_le_bytes());
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
        self.start(queue)
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
