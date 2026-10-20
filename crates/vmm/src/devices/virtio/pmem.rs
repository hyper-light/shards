//! virtio-pmem (virtio 1.3 §5.19): a host file mapped into guest physical memory. The
//! guest sees `/dev/pmemN` and can mount it with DAX, so file data never enters guest RAM
//! or its snapshots (docs/research/image-storage.md R2).
//!
//! Read-only: images are immutable, so a flush has nothing to write back, and every VM
//! that maps one file, restored copies included, shares one copy in the host page cache.
//! Two kinds are writable: a guest's swap window ([`SwapWindow`]), a run's swap, and a
//! run's writable layer ([`Layer`]).

use std::fs::File;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

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
    /// The length of the run's file a snapshot of a run kept (D176), which its restore maps
    /// back before any vCPU runs; 0 for none.
    kept: AtomicU64,
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
            kept: AtomicU64::new(0),
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
        let size = self.fit(&file)?;
        // Its first page written here, through the file's own path, where the guest writes
        // the swap area's header: written first through the mapping, a page of the sparse
        // file took the host's fault path to be made, 3.4 ms at p90 and 80 ms at worst (PM
        // M230). 64 KiB, the largest page a guest has.
        platform::write_all_at(&file, &vec![0; FIRST_PAGE], 0).map_err(|e| format!("its swap file: {e}"))?;
        self.map_run(&mut run, file, size).map(Some)
    }

    /// Maps `file`, the run's swap a snapshot of the run kept (D176), back where its guest
    /// had it, before any vCPU runs: the length the snapshot kept, exactly, whose every
    /// page the guest may have written. A snapshot that kept none takes none.
    pub fn restore_run(&self, file: Option<File>) -> Result<(), String> {
        let kept = self.kept.load(Ordering::Acquire);
        let file = match (kept, file) {
            (0, None) => return Ok(()),
            (0, Some(_)) => return Err("a run's swap for a snapshot that kept none".into()),
            (_, None) => {
                return Err(format!(
                    "the snapshot kept {kept} bytes of its run's swap, which the restore was not given"
                ));
            }
            (_, Some(file)) => file,
        };
        let size = file.metadata().map_err(|e| format!("its swap file: {e}"))?.len();
        if size != kept {
            return Err(format!(
                "a swap file of {size} bytes for a snapshot that kept {kept}"
            ));
        }
        let size = self.fit(&file)?;
        let mut run = lock(&self.run);
        if run.is_some() {
            return Err("a restored swap window with a run's file mapped already".into());
        }
        self.map_run(&mut run, file, size).map(|_| ())
    }

    /// Copies the run's swap into `to` (D176), the guest's every write to it flushed to its
    /// file first: the ranges the file holds data in, of them the blocks not all zeros, and
    /// `to` as long as it after. Returns its length, 0 where no run's is mapped. The machine
    /// is paused: nothing writes it meanwhile.
    pub fn save_run(&self, to: &File) -> Result<u64, String> {
        let run = lock(&self.run);
        let at = |e: std::io::Error| format!("its swap: {e}");
        let Some(mapped) = run.as_ref() else {
            to.set_len(0).map_err(at)?;
            return Ok(0);
        };
        // Flushed first. APFS and Linux's own file systems count a page written through
        // the mapping as data before it is written back (APFS on M5 Max, the test below
        // without this; ext4 and XFS reserve its block as it is dirtied,
        // ext4_da_get_block_prep, xfs_buffered_write_iomap_begin), but one that gives it a
        // block only as it writes it back would count none, and the copy would lose it.
        // SAFETY: the mapping `mapped` holds, which stays while its lock is held.
        unsafe { platform::flush_shared(mapped.host, mapped.len) }.map_err(at)?;
        mapped.copy_to(to, mapped.len as u64).map_err(at)?;
        Ok(mapped.len as u64)
    }

    /// Whether its snapshot kept a run's swap that is not mapped back yet (D176).
    pub fn wants_run(&self) -> bool {
        self.kept.load(Ordering::Acquire) > 0 && !self.attached()
    }

    /// The length of the run's file mapped, which a snapshot keeps; 0 for none.
    fn mapped_len(&self) -> u64 {
        lock(&self.run).as_ref().map_or(0, |mapped| mapped.len as u64)
    }

    /// The length of `file` the window maps: whole multiples of [`ALIGN`], at least the
    /// head; one longer than the window is cut to it, all the guest can reach.
    fn fit(&self, file: &File) -> Result<u64, String> {
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
        Ok(size)
    }

    /// Maps `file`, `size` bytes of it ([`fit`](Self::fit)), in place of the head; returns
    /// its length.
    fn map_run(&self, run: &mut Option<Shared>, file: File, size: u64) -> Result<u64, String> {
        let len = usize::try_from(size).map_err(|_| format!("a swap file of {size} bytes"))?;
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
        Ok(size)
    }

    /// Whether a run's file is mapped: a template keeps no run's swap.
    pub fn attached(&self) -> bool {
        lock(&self.run).is_some()
    }
}

/// The most of a run's swap file written before it is mapped: the largest page a guest
/// kernel has (arm64's 64 KiB), which holds its swap area's header.
const FIRST_PAGE: usize = 64 << 10;

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
    /// Its file, until [`release`](Self::release) lets go of it.
    file: Mutex<Option<File>>,
    /// Unmapped before it drops ([`release`](Self::release)).
    released: AtomicBool,
}

// SAFETY: the mapping is owned by this value; the guest writes it, through the
// hypervisor, and this process only flushes it to its file (`save_run`).
unsafe impl Send for Shared {}
// SAFETY: as above.
unsafe impl Sync for Shared {}

impl Shared {
    fn map(file: File, len: usize) -> std::io::Result<Shared> {
        let host = platform::map_file_shared(&file, len)?;
        Ok(Shared {
            host,
            len,
            file: Mutex::new(Some(file)),
            released: AtomicBool::new(false),
        })
    }

    fn host(&self) -> *mut u8 {
        self.host.as_ptr()
    }

    /// Copies its file's first `len` bytes into `to` ([`platform::copy_used`]).
    fn copy_to(&self, to: &File, len: u64) -> std::io::Result<()> {
        let file = self.file.lock().unwrap_or_else(PoisonError::into_inner);
        let file = file.as_ref().ok_or_else(|| std::io::Error::other(RELEASED))?;
        platform::copy_used(file, to, len)
    }

    /// Unmaps it and closes its file now, rather than as it drops; once.
    ///
    /// # Safety
    /// No VM maps it any more, and nothing uses [`host`](Self::host) after.
    unsafe fn release(&self) {
        if !self.released.swap(true, Ordering::AcqRel) {
            // SAFETY: the mapping `map` made, which the caller says nothing uses.
            unsafe { platform::unmap_file_shared(self.host, self.len) };
            drop(self.file.lock().unwrap_or_else(PoisonError::into_inner).take());
        }
    }

    fn released(&self) -> bool {
        self.released.load(Ordering::Acquire)
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        if !*self.released.get_mut() {
            // SAFETY: the mapping `map` made; the VM that mapped it is gone.
            unsafe { platform::unmap_file_shared(self.host, self.len) };
        }
    }
}

/// A Linux memory section (SECTION_SIZE_BITS 27 on x86_64, and on arm64 with 4 KiB pages):
/// a layer starts at one, so that each of its whole sections' page structures is one 2 MiB
/// block.
pub const SECTION: u64 = 128 << 20;

/// The bytes of a layer's head before its file system, for one of `data` bytes, and the
/// NVDIMM pfn info block it holds at 4 KiB (drivers/nvdimm/pfn.h, struct nd_pfn_sb, Linux
/// 6.18): its kernel's libnvdimm finds the block as it probes the device, and keeps the
/// device's page structures in the device itself (PFN_MODE_PMEM, a vmem_altmap), in the
/// head, which the block says ends where the file system starts (`dataoff`).
///
/// Sized as the kernel fills it, for a device at a section's start: each section of the
/// device takes a 2 MiB block of page structures (64 bytes for each 4 KiB page), placed at
/// a 2 MiB boundary of the head (mm/sparse-vmemmap.c, vmemmap_populate_hugepages and
/// altmap_alloc_block_buf), the first past the info block's 8 KiB; a section the device
/// ends inside is taken as whole, the most it may take. For a device a whole number of
/// sections long, that is the head libnvdimm gives one it makes itself (pfn_devs.c,
/// nd_pfn_init).
pub fn layer_head(data: u64) -> Result<(u64, [u8; 4096]), String> {
    const PAGE: u64 = 4 << 10;
    /// MAX_STRUCT_PAGE_SIZE.
    const STRUCT_PAGE: u64 = 64;
    /// PFN_MODE_PMEM.
    const MODE_PMEM: u32 = 2;
    if data == 0 || !data.is_multiple_of(ALIGN) {
        return Err(format!("a layer of {data} bytes: whole {} MiB", ALIGN >> 20));
    }
    let too_large = || format!("a layer of {data} bytes is past any guest's addresses");
    // The head is a function of the whole device's size, its own included: from none, the
    // least that holds its own sections' structures too.
    let mut head = 0u64;
    loop {
        let size = head.checked_add(data).ok_or_else(too_large)?;
        let want = size
            .div_ceil(SECTION)
            .checked_add(1)
            .and_then(|blocks| blocks.checked_mul(ALIGN))
            .ok_or_else(too_large)?;
        if want == head {
            break;
        }
        head = want;
    }
    fn put(block: &mut [u8; 4096], at: usize, bytes: &[u8]) {
        if let Some(field) = block.get_mut(at..at + bytes.len()) {
            field.copy_from_slice(bytes);
        }
    }
    let mut block = [0u8; 4096];
    put(&mut block, 0, b"NVDIMM_PFN_INFO\0");
    // Its own UUID, any but none (nd_pfn_validate keeps the one it finds); its parent's is
    // none, the UUID of an io namespace, which a virtio-pmem device's is
    // (nd_dev_to_uuid).
    put(&mut block, 16, b"shards-layer-pfn");
    put(&mut block, 52, &1u16.to_le_bytes()); // version_major
    put(&mut block, 54, &4u16.to_le_bytes()); // version_minor: page and struct page sizes recorded
    put(&mut block, 56, &head.to_le_bytes()); // dataoff
    put(&mut block, 64, &(data / PAGE).to_le_bytes()); // npfns
    put(&mut block, 72, &MODE_PMEM.to_le_bytes());
    put(&mut block, 84, &(ALIGN as u32).to_le_bytes()); // align: 2 MiB, for DAX's PMD mappings
    put(&mut block, 88, &(PAGE as u32).to_le_bytes()); // page_size
    put(&mut block, 92, &(STRUCT_PAGE as u16).to_le_bytes()); // page_struct_size
    // nd_sb_checksum: Fletcher-64 over all the block's little-endian 32-bit words, its own
    // field, not yet written, as zeros (nd_fletcher64).
    let (mut lo, mut hi) = (0u32, 0u64);
    for word in block.as_chunks::<4>().0 {
        lo = lo.wrapping_add(u32::from_le_bytes(*word));
        hi = hi.wrapping_add(u64::from(lo));
    }
    let checksum = (hi << 32) | u64::from(lo);
    put(&mut block, 4088, &checksum.to_le_bytes());
    Ok((head, block))
}

/// A run's writable layer (D190): an ext4 file system its guest mounts with DAX, so that
/// what the run writes is the host's page cache of the run's file, which the host writes
/// back and takes back as it would any file's, and the guest keeps metadata alone. The
/// device is a head and the file system after it: the head holds the pfn info block
/// ([`layer_head`]) and the page structures the guest's kernel writes there as it probes the
/// device, which a snapshot keeps as it keeps memory; the file system is the run's file,
/// mapped shared and writable.
pub struct Layer {
    head: Head,
    data: Shared,
}

/// Why a layer's mapping is not there to use: let go of as its machine stopped.
const RELEASED: &str = "its writable layer was let go of as its machine stopped";

/// A layer's head: memory of the VM's own, or a snapshot's copy of it, mapped privately, as
/// guest RAM is restored (its writes the restored VM's alone).
struct Head {
    host: NonNull<u8>,
    len: usize,
    /// The snapshot's copy, where the head was restored from one.
    _file: Option<File>,
}

// SAFETY: the head's memory is owned by this value; the guest writes it, through the
// hypervisor, and this process reads it only for a snapshot, of a paused machine.
unsafe impl Send for Head {}
// SAFETY: as above.
unsafe impl Sync for Head {}

impl Drop for Head {
    fn drop(&mut self) {
        // SAFETY: the reservation `Layer` made (a snapshot's file mapped over it); the VM
        // that mapped it is gone (`Running` drops the VM before the devices).
        unsafe { platform::release(self.host, self.len) };
    }
}

impl std::fmt::Debug for Layer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Layer")
            .field("head", &self.head.len)
            .field("data", &self.data.len)
            .finish_non_exhaustive()
    }
}

impl Layer {
    /// A layer over `file`, the run's ext4 file system, whole multiples of [`ALIGN`], as a
    /// boot gives it: its head the VM's own memory, the info block written.
    pub fn boot(file: File) -> Result<Layer, String> {
        let data = file.metadata().map_err(|e| format!("its layer: {e}"))?.len();
        let (head_len, block) = layer_head(data)?;
        let len = usize::try_from(head_len).map_err(|_| format!("a layer head of {head_len} bytes"))?;
        let host = platform::reserve_ram(len).map_err(|e| format!("its layer's head: {e}"))?;
        let head = Head {
            host,
            len,
            _file: None,
        };
        // SAFETY: the reservation is `len` bytes, more than 8 KiB, and only this value
        // holds it yet.
        unsafe {
            std::ptr::copy_nonoverlapping(block.as_ptr(), host.as_ptr().add(4096), block.len());
        }
        Layer::over(head, file, data)
    }

    /// A layer as a snapshot kept it: its head mapped privately from `kept`, the
    /// snapshot's copy ([`save`](Self::save)), each 2 MiB block where the head had it, and
    /// `file`, the run's copy of its file system as the snapshot kept it, mapped shared.
    pub fn restore(kept: File, file: File) -> Result<Layer, String> {
        let at = |e: std::io::Error| format!("its layer's head: {e}");
        let data = file.metadata().map_err(|e| format!("its layer: {e}"))?.len();
        let (head_len, _) = layer_head(data)?;
        let len = usize::try_from(head_len).map_err(|_| format!("a layer head of {head_len} bytes"))?;
        let blocks = len / HEAD_BLOCK;
        let index = read_head_index(&kept, blocks).map_err(|e| format!("its layer's head: {e}"))?;
        let host = platform::reserve(len).map_err(at)?;
        // Held from here, so that a failure releases the reservation.
        let mut head = Head {
            host,
            len,
            _file: None,
        };
        let first = head_index_len(blocks);
        for (i, kept_at) in index.iter().enumerate() {
            let Some(n) = kept_at else {
                continue;
            };
            let offset = (*n as u64)
                .checked_mul(HEAD_BLOCK as u64)
                .and_then(|o| o.checked_add(first))
                .ok_or("a layer head's block past any file")?;
            // SAFETY: a whole block of the reservation, which nothing references yet.
            match unsafe { platform::map_file_private(&kept, offset, HEAD_BLOCK, host.add(i * HEAD_BLOCK)) } {
                Ok(()) => {}
                // Where no file maps over part of a reservation (Windows), read into it, as
                // guest RAM is restored there (`GuestMemory::from_file`).
                Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
                    // SAFETY: a whole block of the reservation, committed memory that
                    // nothing references yet.
                    let block = unsafe {
                        std::slice::from_raw_parts_mut(host.as_ptr().add(i * HEAD_BLOCK), HEAD_BLOCK)
                    };
                    platform::read_exact_at(&kept, block, offset).map_err(at)?;
                }
                Err(e) => return Err(at(e)),
            }
        }
        head._file = Some(kept);
        Layer::over(head, file, data)
    }

    fn over(head: Head, file: File, data: u64) -> Result<Layer, String> {
        let len = usize::try_from(data).map_err(|_| format!("a layer of {data} bytes"))?;
        let data = Shared::map(file, len).map_err(|e| format!("its layer: {e}"))?;
        Ok(Layer { head, data })
    }

    /// The device's length: its head and its file system.
    pub fn len(&self) -> u64 {
        (self.head.len + self.data.len) as u64
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The file system's length, the run's file's.
    pub fn data_len(&self) -> u64 {
        self.data.len as u64
    }

    /// Its two mappings for a guest that sees it at `gpa`, `(gpa, host, len)`: the head,
    /// then the file system, both writable by the guest. `gpa` must be a section's start
    /// ([`SECTION`]), where the head is sized for.
    pub fn mappings(&self, gpa: u64) -> Result<[(u64, *mut u8, usize); 2], String> {
        if self.data.released() {
            return Err(RELEASED.into());
        }
        if !gpa.is_multiple_of(SECTION) {
            return Err(format!("a layer at {gpa:#x}: a {} MiB boundary", SECTION >> 20));
        }
        let data_gpa = gpa
            .checked_add(self.head.len as u64)
            .filter(|at| at.checked_add(self.data.len as u64).is_some())
            .ok_or_else(|| format!("a layer at {gpa:#x} overflows the address space"))?;
        Ok([
            (gpa, self.head.host.as_ptr(), self.head.len),
            (data_gpa, self.data.host(), self.data.len),
        ])
    }

    /// Writes the head into `head` and copies the file system into `data`, as a paused
    /// machine's guest left them, for a snapshot (D190). The head, its 2 MiB blocks each
    /// once: all but a few hold the page structures of a section none of whose pages is in
    /// use, which are alike, the 64 bytes of each page the same (mm_init.c,
    /// __init_zone_device_page), so that a 10 GiB layer's head of 85 blocks is kept in
    /// five (PM M277). An index first, block by block: which of those it is, or none for
    /// zeros.
    /// The file system, flushed to its file first: the blocks not all zeros of the ranges
    /// its file holds data in.
    pub fn save(&self, head: &File, data: &File) -> Result<(), String> {
        if self.data.released() {
            return Err(RELEASED.into());
        }
        let at = |e: std::io::Error| format!("its layer: {e}");
        save_head(head, self.head.host, self.head.len).map_err(at)?;
        // SAFETY: the mapping `data` holds, for the layer's life.
        unsafe { platform::flush_shared(self.data.host, self.data.len) }.map_err(at)?;
        self.data.copy_to(data, self.data.len as u64).map_err(at)
    }

    /// Lets go of its file system's mapping and file once its machine is gone, rather than
    /// as the process ends: the file holds what the guest wrote, and its host reads it
    /// with nothing of this process's still at it. On macOS the last writer to let go of a
    /// file written through a shared mapping writes it back before it returns, and a reader
    /// of the file meanwhile waits behind it (PM M278).
    ///
    /// # Safety
    /// No VM maps it any more (`Running::teardown` destroys the VM first).
    pub unsafe fn release(&self) {
        // SAFETY: forwarded caller contract; `mappings` and `save` refuse it after.
        unsafe { self.data.release() }
    }
}

/// A layer head's blocks as a snapshot keeps them: what the guest's kernel fills each of a
/// whole 2 MiB, a section's page structures (vmemmap_populate_hugepages).
const HEAD_BLOCK: usize = ALIGN as usize;
/// A kept head's first bytes (D190).
const HEAD_MAGIC: [u8; 8] = *b"SHRDHEAD";
/// The most blocks a kept head's index compares each block with before it keeps it as one
/// more: a head's alike blocks are a few kinds.
const HEAD_KINDS: usize = 64;

/// The bytes of a kept head's index for `blocks` blocks: its magic, its count, and a 32-bit
/// number for each, whole blocks of it, so that what follows lies at block boundaries.
fn head_index_len(blocks: usize) -> u64 {
    let bytes = 12 + 4 * blocks as u64;
    bytes.next_multiple_of(HEAD_BLOCK as u64)
}

/// Writes the `len` bytes of a head at `host`, whole [`HEAD_BLOCK`]s, into `file`: an index
/// of its blocks, each a number among those kept after it, or `u32::MAX` for all zeros;
/// then each kept block once, in the order first met.
fn save_head(file: &File, host: NonNull<u8>, len: usize) -> std::io::Result<()> {
    let invalid = || std::io::Error::from(std::io::ErrorKind::InvalidInput);
    if !len.is_multiple_of(HEAD_BLOCK) {
        return Err(invalid());
    }
    let blocks = len / HEAD_BLOCK;
    let block = |i: usize| host.as_ptr().wrapping_add(i * HEAD_BLOCK).cast_const();
    // The head blocks kept, by their first place in the head.
    let mut kept: Vec<usize> = Vec::new();
    let mut index: Vec<u32> = Vec::with_capacity(blocks);
    for i in 0..blocks {
        // SAFETY: a whole block within the head, which the paused machine leaves alone.
        if unsafe { words_zero(block(i), HEAD_BLOCK) } {
            index.push(u32::MAX);
            continue;
        }
        // Compared with the first kinds kept alone: past them, each block is kept as it is.
        // SAFETY: as above, for both blocks.
        let alike = kept
            .iter()
            .take(HEAD_KINDS)
            .position(|&k| unsafe { words_equal(block(i), block(k), HEAD_BLOCK) });
        let n = match alike {
            Some(n) => n,
            None => {
                kept.push(i);
                kept.len() - 1
            }
        };
        index.push(u32::try_from(n).map_err(|_| invalid())?);
    }
    let first = head_index_len(blocks);
    let mut header = Vec::with_capacity(12 + 4 * blocks);
    header.extend_from_slice(&HEAD_MAGIC);
    header.extend_from_slice(&u32::try_from(blocks).map_err(|_| invalid())?.to_le_bytes());
    for n in &index {
        header.extend_from_slice(&n.to_le_bytes());
    }
    file.set_len(0)?;
    platform::write_all_at(file, &header, 0)?;
    for (n, &i) in kept.iter().enumerate() {
        let at = first + (n * HEAD_BLOCK) as u64;
        let mut done = 0;
        while done < HEAD_BLOCK {
            // SAFETY: the rest of block `i` of the head, which the kernel only reads.
            let wrote =
                unsafe { platform::write_at(file, block(i).add(done), HEAD_BLOCK - done, at + done as u64) }?;
            if wrote == 0 {
                return Err(std::io::ErrorKind::WriteZero.into());
            }
            done += wrote;
        }
    }
    file.set_len(first + (kept.len() * HEAD_BLOCK) as u64)
}

/// The index of a head [`save_head`] kept in `file`, of `blocks` blocks: each block's
/// number among those kept, or none for zeros; checked against the file's length.
fn read_head_index(file: &File, blocks: usize) -> Result<Vec<Option<u32>>, String> {
    let mut magic = [0u8; 12];
    platform::read_exact_at(file, &mut magic, 0).map_err(|e| e.to_string())?;
    if magic.get(..8) != Some(&HEAD_MAGIC[..]) {
        return Err("not a layer head shards kept".into());
    }
    let count = magic
        .get(8..12)
        .and_then(|b| <[u8; 4]>::try_from(b).ok())
        .map(u32::from_le_bytes);
    if count != u32::try_from(blocks).ok() {
        return Err(format!("a head of {count:?} blocks kept for one of {blocks}"));
    }
    let mut raw = vec![0u8; 4 * blocks];
    platform::read_exact_at(file, &mut raw, 12).map_err(|e| e.to_string())?;
    let len = file.metadata().map_err(|e| e.to_string())?.len();
    let kept = len.saturating_sub(head_index_len(blocks)) / HEAD_BLOCK as u64;
    raw.as_chunks::<4>()
        .0
        .iter()
        .map(|b| {
            let n = u32::from_le_bytes(*b);
            match n {
                u32::MAX => Ok(None),
                n if u64::from(n) < kept => Ok(Some(n)),
                n => Err(format!("a head's block {n} past the {kept} it keeps")),
            }
        })
        .collect()
}

/// Whether the `len` bytes at `p`, word-aligned, are all zeros, read word by word as the
/// guest's memory they are.
///
/// # Safety
/// `p` must be valid for reads of `len` bytes, word-aligned, a whole number of words.
unsafe fn words_zero(p: *const u8, len: usize) -> bool {
    let words = p.cast::<u64>();
    // SAFETY: each word lies within the caller's range.
    (0..len / 8).all(|k| unsafe { words.add(k).read_volatile() } == 0)
}

/// Whether the `len` bytes at `a` and at `b` are alike, read word by word.
///
/// # Safety
/// As [`words_zero`], for both.
unsafe fn words_equal(a: *const u8, b: *const u8, len: usize) -> bool {
    let (a, b) = (a.cast::<u64>(), b.cast::<u64>());
    // SAFETY: each word lies within the caller's ranges.
    (0..len / 8).all(|k| unsafe { a.add(k).read_volatile() == b.add(k).read_volatile() })
}

/// What a virtio-pmem device is backed by.
enum Backing {
    /// An image file, read-only.
    File(Arc<Region>),
    /// A guest's swap window (D159).
    Swap(Arc<SwapWindow>),
    /// A run's writable layer (D190).
    Layer(Arc<Layer>),
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
            Backing::Layer(layer) => d.field("layer", layer),
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

    /// The device of a run's writable layer (D190), which the machine has mapped at `gpa`.
    pub fn layer(layer: Arc<Layer>, gpa: u64) -> Pmem {
        Pmem {
            gpa,
            backing: Backing::Layer(layer),
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
            Backing::Layer(layer) => layer.len(),
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

    /// A swap window's: whether the guest was shown it, and the length of the run's swap
    /// mapped, whose bytes a snapshot of the run keeps beside it (D176).
    fn save(&self, w: &mut Writer) {
        if let Backing::Swap(window) = &self.backing {
            w.bool(window.shown());
            w.u64(window.mapped_len());
        }
    }

    fn restore(&mut self, r: &mut Reader<'_>) -> codec::Result<()> {
        if let Backing::Swap(window) = &self.backing {
            if r.bool()? {
                window.show();
            }
            let kept = r.u64()?;
            if kept > window.len() || !kept.is_multiple_of(ALIGN) {
                return Err(codec::DecodeError(format!(
                    "a run's swap of {kept} bytes kept for a window of {}",
                    window.len()
                )));
            }
            window.kept.store(kept, Ordering::Release);
        }
        Ok(())
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// Slots no hypervisor maps: whatever a window asks of them is done.
    struct Unmapped;

    impl Slots for Unmapped {
        unsafe fn map(&self, _: u32, _: *mut u8, _: u64, _: usize, _: bool) -> Result<(), String> {
            Ok(())
        }

        fn unmap(&self, _: u32, _: u64, _: usize) -> Result<(), String> {
            Ok(())
        }
    }

    fn window() -> Arc<SwapWindow> {
        Arc::new(SwapWindow::new(1 << 32, 8 * ALIGN, Box::new(Unmapped)).unwrap())
    }

    fn file(dir: &Path, name: &str, len: u64) -> File {
        let f = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(dir.join(name))
            .unwrap();
        f.set_len(len).unwrap();
        f
    }

    /// A layer's head holds a 2 MiB block of page structures for each section of the
    /// device, its own included, past the info block's first 2 MiB: for a device of whole
    /// sections, libnvdimm's own (nd_pfn_init: ALIGN(8 KiB + 64 bytes a page past 8 KiB,
    /// 2 MiB)); and its info block is one libnvdimm takes (nd_pfn_validate).
    #[test]
    fn a_layers_head_holds_its_sections_page_structures() {
        let mib = 1u64 << 20;
        let (head, _) = layer_head(10 << 30).unwrap();
        assert_eq!(head, 166 * mib);
        for sections in [1u64, 8, 82, 512] {
            let size = sections * SECTION;
            let kernels = (8192 + 64 * ((size - 8192) / 4096)).next_multiple_of(ALIGN);
            let (head, _) = layer_head(size - kernels).unwrap();
            assert_eq!(head, kernels, "{sections} sections");
        }
        // A device ending inside a section takes that section's block whole.
        let (head, _) = layer_head(SECTION - 4 * mib + 2 * mib).unwrap();
        assert_eq!(head, 6 * mib);
        assert!(layer_head(0).is_err());
        assert!(layer_head(ALIGN + 4096).is_err());
        assert!(layer_head(u64::MAX - ALIGN + 1).is_err());

        let data = 10u64 << 30;
        let (head, block) = layer_head(data).unwrap();
        assert_eq!(&block[..16], b"NVDIMM_PFN_INFO\0");
        assert_eq!(&block[32..48], &[0; 16], "an io namespace's parent UUID");
        let u64_at = |at: usize| u64::from_le_bytes(block[at..at + 8].try_into().unwrap());
        let u32_at = |at: usize| u32::from_le_bytes(block[at..at + 4].try_into().unwrap());
        assert_eq!((u64_at(56), u64_at(64)), (head, data / 4096));
        assert_eq!((u32_at(72), u32_at(84), u32_at(88)), (2, ALIGN as u32, 4096));
        // nd_fletcher64 of the block, its checksum zeroed, is its checksum.
        let mut zeroed = block;
        zeroed[4088..].fill(0);
        let (mut lo, mut hi) = (0u64, 0u64);
        for w in zeroed.chunks(4) {
            lo = (lo + u64::from(u32::from_le_bytes(w.try_into().unwrap()))) & 0xFFFF_FFFF;
            hi = hi.wrapping_add(lo);
        }
        assert_eq!(u64_at(4088), (hi << 32) | lo);
    }

    /// What the guest wrote to a layer's head and file system a save keeps, the head as
    /// memory, holes where it is zeros; a restore maps the head back privately, its writes
    /// its own, and the run's copy of the file system shared (D190).
    #[test]
    fn a_layer_is_kept_by_a_save_and_mapped_back_by_its_restore() {
        let dir = shards_testdir::TempDir::new("layer-save").unwrap();
        let data_len = 8 * ALIGN;
        let booted = Layer::boot(file(&dir, "fs", data_len)).unwrap();
        let [(gpa, head, head_len), (data_gpa, data, len)] = booted.mappings(SECTION).unwrap();
        assert_eq!((gpa, head_len as u64, len as u64), (SECTION, 2 * ALIGN, data_len));
        assert_eq!(data_gpa, SECTION + 2 * ALIGN);
        assert_eq!(booted.len(), 2 * ALIGN + data_len);
        // SAFETY: within the mappings, which the layer holds.
        unsafe {
            assert_eq!(
                std::slice::from_raw_parts(head.add(4096), 16),
                b"NVDIMM_PFN_INFO\0"
            );
            head.add(ALIGN as usize).write_bytes(0x5a, 8192);
            data.add(3 << 20).write_bytes(0xa5, 4096);
        }
        let (kept_head, kept_data) = (file(&dir, "head", 0), file(&dir, "data", 0));
        booted.save(&kept_head, &kept_data).unwrap();
        // Its index's block, then its two blocks, unlike.
        assert_eq!(kept_head.metadata().unwrap().len(), 3 * ALIGN);
        assert_eq!(kept_data.metadata().unwrap().len(), data_len);

        let copy = file(&dir, "data2", 0);
        platform::copy_used(&kept_data, &copy, data_len).unwrap();
        let restored = Layer::restore(kept_head.try_clone().unwrap(), kept_data).unwrap();
        let [(_, head, _), (_, data, _)] = restored.mappings(SECTION).unwrap();
        // SAFETY: within the mappings, which the layer holds.
        unsafe {
            assert_eq!(
                std::slice::from_raw_parts(head.add(4096), 16),
                b"NVDIMM_PFN_INFO\0"
            );
            assert_eq!(*head.add(ALIGN as usize + 8191), 0x5a);
            assert_eq!(*head.add(ALIGN as usize + 8192), 0);
            assert_eq!(*data.add((3 << 20) + 4095), 0xa5);
            assert_eq!(*data.add(4 << 20), 0);
            // Its head's writes are its own: the snapshot's copy keeps what it held.
            head.add(ALIGN as usize).write_bytes(0x11, 4096);
        }
        let again = Layer::restore(kept_head.try_clone().unwrap(), copy).unwrap();
        let [(_, head, _), _] = again.mappings(SECTION).unwrap();
        // SAFETY: within the mapping, which the layer holds.
        unsafe { assert_eq!(*head.add(ALIGN as usize), 0x5a) };

        // What is no kept head is refused.
        let e = Layer::restore(file(&dir, "short", ALIGN), file(&dir, "fs2", data_len)).unwrap_err();
        assert!(e.contains("not a layer head"), "{e}");
        let e = restored.mappings(ALIGN).unwrap_err();
        assert!(e.contains("boundary"), "{e}");
    }

    /// A head's alike blocks are kept once each, and every place that had one has it back:
    /// a head of nine sections' blocks, eight alike, is kept in an index and three blocks.
    #[test]
    fn a_heads_alike_blocks_are_kept_once() {
        let dir = shards_testdir::TempDir::new("layer-head").unwrap();
        let data_len = 1 << 30;
        let booted = Layer::boot(file(&dir, "fs", data_len)).unwrap();
        let [(_, head, head_len), _] = booted.mappings(SECTION).unwrap();
        let blocks = head_len / HEAD_BLOCK;
        assert_eq!(blocks, 10);
        // SAFETY: within the head, which the layer holds.
        unsafe {
            for i in 1..blocks - 1 {
                head.add(i * HEAD_BLOCK).write_bytes(0x40, HEAD_BLOCK);
            }
            head.add((blocks - 1) * HEAD_BLOCK + 64).write_bytes(0x41, 64);
        }
        let kept = file(&dir, "head", 0);
        save_head(&kept, NonNull::new(head).unwrap(), head_len).unwrap();
        assert_eq!(kept.metadata().unwrap().len(), ALIGN + 3 * ALIGN);
        let index = read_head_index(&kept, blocks).unwrap();
        assert_eq!(
            index,
            [
                Some(0),
                Some(1),
                Some(1),
                Some(1),
                Some(1),
                Some(1),
                Some(1),
                Some(1),
                Some(1),
                Some(2)
            ]
        );
        let restored = Layer::restore(kept, file(&dir, "fs2", data_len)).unwrap();
        let [(_, back, _), _] = restored.mappings(SECTION).unwrap();
        // SAFETY: within both heads, which their layers hold.
        unsafe {
            for i in 0..blocks {
                assert!(
                    words_equal(head.add(i * HEAD_BLOCK), back.add(i * HEAD_BLOCK), HEAD_BLOCK),
                    "block {i}"
                );
            }
        }
        // A kept head of another count of blocks, or naming a block it does not keep, is refused.
        let short = file(&dir, "short", 0);
        save_head(&short, NonNull::new(head).unwrap(), 2 * HEAD_BLOCK).unwrap();
        assert!(
            read_head_index(&short, blocks)
                .unwrap_err()
                .contains("blocks kept")
        );
        short.set_len(ALIGN + ALIGN).unwrap();
        platform::write_all_at(&short, &[9, 0, 0, 0], 12 + 4).unwrap();
        assert!(read_head_index(&short, 2).unwrap_err().contains("past the"));
    }

    /// What the guest wrote to a run's swap through its mapping, never flushed, a save
    /// copies into the snapshot's swap, holes where it wrote nothing; the device's state
    /// keeps its length, and a restore maps the copy back at that length alone, before any
    /// vCPU runs (D176).
    #[test]
    fn a_runs_swap_is_kept_by_a_save_and_mapped_back_by_its_restore() {
        let dir = shards_testdir::TempDir::new("swap-save").unwrap();
        let len = 4 * ALIGN;
        let saved = window();
        assert_eq!(saved.attach(file(&dir, "run", len)).unwrap(), Some(len));
        let host = lock(&saved.run).as_ref().unwrap().host();
        // SAFETY: within the mapping, which the window holds.
        unsafe {
            host.add(4096).write_bytes(7, 4096);
            host.add(3 << 20).write_bytes(9, 100);
        }
        let device = Pmem::swap(saved.clone());
        let mut w = Writer::default();
        device.save(&mut w);
        let kept = file(&dir, "kept", 0);
        assert_eq!(saved.save_run(&kept).unwrap(), len);
        assert_eq!(kept.metadata().unwrap().len(), len);
        let mut bytes = vec![0u8; len as usize];
        platform::read_exact_at(&kept, &mut bytes, 0).unwrap();
        assert!(bytes[4096..8192].iter().all(|&b| b == 7));
        assert!(bytes[3 << 20..(3 << 20) + 100].iter().all(|&b| b == 9));
        assert_eq!(bytes.iter().filter(|&&b| b != 0).count(), 4096 + 100);
        // No more of a disk than what was written: a block or two of its 8 MiB.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let on_disk = kept.metadata().unwrap().blocks() * 512;
            assert!(on_disk <= 1 << 20, "{on_disk} bytes on disk");
        }

        let restored = window();
        let mut device = Pmem::swap(restored.clone());
        let state = w.into_bytes();
        device.restore(&mut Reader::new(&state)).unwrap();
        let e = restored.restore_run(None).unwrap_err();
        assert!(e.contains("which the restore was not given"), "{e}");
        let e = restored
            .restore_run(Some(file(&dir, "short", 2 * ALIGN)))
            .unwrap_err();
        assert!(e.contains("for a snapshot that kept"), "{e}");
        restored.restore_run(Some(kept)).unwrap();
        let host = lock(&restored.run).as_ref().unwrap().host();
        // SAFETY: within the mapping, which the window holds.
        unsafe {
            assert_eq!(*host.add(4096), 7);
            assert_eq!(*host.add((3 << 20) + 99), 9);
            assert_eq!(*host.add(1 << 20), 0);
        }

        // A window that kept none takes none.
        let none = window();
        none.restore_run(None).unwrap();
        let e = none.restore_run(Some(file(&dir, "stray", len))).unwrap_err();
        assert!(e.contains("kept none"), "{e}");
    }
}
