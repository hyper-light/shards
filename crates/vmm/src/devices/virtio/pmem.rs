//! virtio-pmem (virtio 1.3 §5.19): a host file mapped into guest physical memory. The
//! guest sees `/dev/pmemN` and can mount it with DAX, so file data never enters guest RAM
//! or its snapshots (docs/research/image-storage.md R2).
//!
//! Read-only: images are immutable, so a flush has nothing to write back, and every VM
//! that maps one file, restored copies included, shares one copy in the host page cache.

use std::fs::File;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use super::queue::{Chain, Queue, QueueError};
use super::{Activation, DeviceInterrupt, VirtioDevice, feature};
use crate::memory::GuestMemory;
use crate::sync::lock;
use crate::{platform, warn};

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

struct Active {
    queue: Queue,
    memory: Arc<GuestMemory>,
    interrupt: Arc<DeviceInterrupt>,
}

pub struct Pmem {
    region: Arc<Region>,
    /// Where the guest sees the region.
    gpa: u64,
    active: Mutex<Option<Active>>,
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
            active: Mutex::new(None),
        }
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
        *lock(&self.active) = Some(Active {
            queue,
            memory,
            interrupt,
        });
        Ok(())
    }

    /// Flushes of a read-only region complete at once, on the notifying vCPU thread:
    /// there is nothing to write back, so nothing can block.
    fn notify(&self, _queue: u16) {
        let mut active = lock(&self.active);
        let Some(a) = active.as_mut() else {
            return;
        };
        if let Err(e) = drain(&mut a.queue, &a.memory, &a.interrupt) {
            warn!("virtio-pmem: {e}; device needs reset");
            a.interrupt.fail();
            *active = None;
        }
    }

    fn reset(&mut self) {
        *lock(&self.active) = None;
    }

    fn pause(&mut self) -> Vec<super::QueueState> {
        lock(&self.active).iter().map(|a| a.queue.state()).collect()
    }

    fn resume(&mut self) -> Result<(), String> {
        Ok(())
    }
}

fn drain(queue: &mut Queue, mem: &GuestMemory, irq: &DeviceInterrupt) -> Result<(), QueueError> {
    let mut used = false;
    while let Some(chain) = queue.pop(mem)? {
        let written = answer(&chain, mem);
        queue.add_used(mem, chain.head, written)?;
        used = true;
    }
    if used && queue.needs_interrupt(mem)? {
        irq.used_buffer();
    }
    Ok(())
}

/// Reads `struct virtio_pmem_req { le32 type; }` and writes `struct virtio_pmem_resp
/// { le32 ret; }`: 0 for a flush, 1 for anything else. Returns the bytes written.
fn answer(chain: &Chain, mem: &GuestMemory) -> u32 {
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
