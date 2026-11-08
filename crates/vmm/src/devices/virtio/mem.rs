//! virtio-mem (VIRTIO 1.3 §5.15): memory the guest has only as its device plugs it, in
//! blocks, up to the size the device asks for (D133). The guest asks for that size itself,
//! through the control page (shards_abi::control::HOTPLUG_REQUEST), up to what the host
//! gave it: a builder boots small and grows as its build needs, where memory it had from
//! boot cost it its page structures whether it used the memory or not (PM M82).
//!
//! Unplugged memory is no one's: the host withholds its pages (platform::withhold), the
//! host's devices are refused it (memory::GuestMemory::host_ptr), and where the
//! hypervisor's stage 2 holds what it maps, it is unmapped from the guest ([`Mapper`]).
//! The driver must accept VIRTIO_MEM_F_UNPLUGGED_INACCESSIBLE, its promise never to touch
//! it (§5.15.6.1).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::queue::{Chain, Queue};
use super::worker::Worker;
use super::{Activation, DeviceInterrupt, QueueState, VirtioDevice, feature};
use crate::memory::{Access, GuestMemory};
use crate::snapshot::codec::{self, Reader, Writer};
use crate::sync::lock;

pub const DEVICE_ID: u32 = 24;
/// The blocks memory is plugged in: 2 MiB, Linux's pageblock with 4 KiB pages, below which
/// its driver plugs nothing (drivers/virtio/virtio_mem.c, `virtio_mem_init_hotplug`), a
/// huge page of an x86_64 host, and whole 16 KiB pages of a Mac's.
pub const BLOCK: u64 = 2 << 20;
/// What the memory to plug starts at and is rounded up to: Linux's memory block with 4 KiB
/// pages on both architectures (arm64 SECTION_SIZE_BITS 27, x86_64 MIN_MEMORY_BLOCK_SIZE),
/// the unit it adds memory in; memory outside whole blocks of it Linux cannot use
/// (`virtio_mem_init_hotplug`).
pub const ALIGN: u64 = 128 << 20;
/// Linux's driver waits for each request's answer before it sends the next.
const QUEUE_SIZE: u16 = 64;
/// VIRTIO_MEM_F_UNPLUGGED_INACCESSIBLE (include/uapi/linux/virtio_mem.h).
const UNPLUGGED_INACCESSIBLE: u64 = 1 << 1;

/// Request types (§5.15.6).
const PLUG: u16 = 0;
const UNPLUG: u16 = 1;
const UNPLUG_ALL: u16 = 2;
const STATE: u16 = 3;
/// Response types.
const ACK: u16 = 0;
const NACK: u16 = 1;
const BUSY: u16 = 2;
const ERROR: u16 = 3;
/// What a STATE request finds.
const PLUGGED: u16 = 0;
const UNPLUGGED: u16 = 1;
const MIXED: u16 = 2;
/// `struct virtio_mem_req` and `struct virtio_mem_resp`.
const REQ_LEN: usize = 24;
const RESP_LEN: usize = 10;
/// `struct virtio_mem_config`.
const CONFIG_LEN: usize = 56;

/// Maps host memory into the guest at the hypervisor's stage 2 and unmaps it, while the VM
/// lives, readable, writable and executable: plugged memory, and a writable layer's head as
/// it is mapped back (pmem `Layer::remap_head`). The hypervisor's (`hv::Stage2`), or a
/// test's.
pub trait Mapper: Send + Sync {
    /// # Safety
    /// `host..host+len` must stay mapped in this process until unmapped here or the VM is
    /// destroyed.
    unsafe fn map(&self, host: *mut u8, gpa: u64, len: usize) -> Result<(), String>;
    fn unmap(&self, gpa: u64, len: usize) -> Result<(), String>;
}

/// The device's sizes, which the guest's requests through the control page change: shared
/// by the device, its worker, and the vCPU threads that read its configuration or write
/// the control page.
#[derive(Debug)]
pub struct Sizes {
    gpa: u64,
    region: u64,
    /// The most the guest may have plugged: whole blocks, at most `region`. A run's own,
    /// set as a VM restored from a template takes it (D148).
    most: AtomicU64,
    state: Mutex<State>,
    /// Whether a plug or unplug the host could not do has been said: once, as a guest
    /// asking again and again could otherwise fill the host's log.
    said: AtomicBool,
}

#[derive(Debug)]
struct State {
    requested: u64,
    plugged: u64,
    /// Changes with every size, for the transport's ConfigGeneration (§4.2.2.1): the
    /// driver reads a 64-bit field as two 32-bit halves, and again where this changed
    /// between them.
    generation: u32,
    /// The driver's interrupt, while it is active: what tells it of a new request.
    interrupt: Option<Arc<DeviceInterrupt>>,
}

impl Sizes {
    /// `region` bytes at `gpa`, both multiples of [`BLOCK`], of which the guest may have at
    /// most `most` plugged; none asked for yet.
    pub fn new(gpa: u64, region: u64, most: u64) -> Sizes {
        Sizes {
            gpa,
            region,
            most: AtomicU64::new(most.min(region) / BLOCK * BLOCK),
            state: Mutex::new(State {
                requested: 0,
                plugged: 0,
                generation: 0,
                interrupt: None,
            }),
            said: AtomicBool::new(false),
        }
    }

    /// The most the guest may have plugged (shards_abi::control::HOTPLUG_SIZE).
    pub fn most(&self) -> u64 {
        self.most.load(Ordering::Relaxed)
    }

    /// Sets the most the guest may have plugged: a run's own, as it is given to a VM
    /// restored from a template (D148). Whole blocks, at most the region; a request past it
    /// is lowered to it, as the guest may have no more.
    pub fn set_most(&self, bytes: u64) {
        let most = bytes.min(self.region) / BLOCK * BLOCK;
        self.most.store(most, Ordering::Relaxed);
        if self.requested() > most {
            self.request(most);
        }
    }

    /// What the guest asks to have plugged now (shards_abi::control::HOTPLUG_REQUEST).
    pub fn requested(&self) -> u64 {
        lock(&self.state).requested
    }

    /// The guest asks to have `bytes` plugged: whole blocks, at most [`most`](Self::most).
    /// The device asks its driver for it (`requested_size`), which hears through a
    /// configuration interrupt and plugs or unplugs to match (§5.15.6). Asked again for
    /// what it asked already, it tells the driver again: Linux's driver tries a request it
    /// could not meet (ENOMEM, BUSY) again on a configuration change at once, or else 50
    /// s later, doubling to 300 s (drivers/virtio/virtio_mem.c, `virtio_mem_config_changed`,
    /// `VIRTIO_MEM_RETRY_TIMER_MIN_MS`), which a build held at its memory waits out (D133).
    /// The configuration's generation moves only as it changes.
    pub fn request(&self, bytes: u64) {
        // `most` is whole blocks, so rounding up within it stays within it.
        let most = self.most();
        let want = bytes.min(most).next_multiple_of(BLOCK).min(most);
        let interrupt = {
            let mut s = lock(&self.state);
            if s.requested != want {
                s.requested = want;
                s.generation = s.generation.wrapping_add(1);
            }
            s.interrupt.clone()
        };
        if let Some(i) = interrupt {
            i.config_change();
        }
    }

    /// `struct virtio_mem_config` as it stands. The usable region is the whole region,
    /// always: never less than the size asked for, and never shrunk (§5.15.4.2).
    fn config(&self) -> [u8; CONFIG_LEN] {
        let s = lock(&self.state);
        let mut c = [0u8; CONFIG_LEN];
        for (at, value) in [
            (0, BLOCK),
            (16, self.gpa),
            (24, self.region),
            (32, self.region),
            (40, s.plugged),
            (48, s.requested),
        ] {
            if let Some(field) = c.get_mut(at..at + 8) {
                field.copy_from_slice(&value.to_le_bytes());
            }
        }
        c
    }

    fn generation(&self) -> u32 {
        lock(&self.state).generation
    }

    /// Counts `len` bytes more plugged, or `len` fewer.
    fn count(&self, len: u64, plugged: bool) {
        let mut s = lock(&self.state);
        s.plugged = if plugged {
            s.plugged.saturating_add(len)
        } else {
            s.plugged.saturating_sub(len)
        };
        s.generation = s.generation.wrapping_add(1);
    }

    /// The bytes `blocks` blocks from `addr` are, where a request may name them: aligned,
    /// at least one, and all within the usable region (§5.15.6.2-4).
    fn range(&self, addr: u64, blocks: u16) -> Option<u64> {
        if blocks == 0 || !addr.is_multiple_of(BLOCK) {
            return None;
        }
        let len = u64::from(blocks).checked_mul(BLOCK)?;
        let end = addr.checked_add(len)?;
        (addr >= self.gpa && end <= self.gpa.checked_add(self.region)?).then_some(len)
    }

    /// PLUG: blocks all unplugged, within what the device asks for (NACK past it), plugged
    /// whole or not at all (BUSY where the host could not).
    fn plug(&self, access: &Access<'_>, addr: u64, blocks: u16, mapper: &impl Mapper) -> u16 {
        let Some(len) = self.range(addr, blocks) else {
            return ERROR;
        };
        if access
            .memory()
            .plugged(addr, len)
            .is_none_or(|(plugged, _)| plugged > 0)
        {
            return ERROR;
        }
        {
            let s = lock(&self.state);
            if s.plugged.checked_add(len).is_none_or(|after| after > s.requested) {
                return NACK;
            }
        }
        let map = |host, gpa, size| {
            // SAFETY: the memory to plug stays mapped in this process as long as the guest
            // memory does, which outlives the VM.
            unsafe { mapper.map(host, gpa, size) }
        };
        match access.plug(addr, len, map) {
            Ok(()) => {
                self.count(len, true);
                ACK
            }
            Err(e) => self.busy(&e),
        }
    }

    /// UNPLUG: blocks all plugged, unplugged whole or not at all.
    fn unplug(&self, access: &Access<'_>, addr: u64, blocks: u16, mapper: &impl Mapper) -> u16 {
        let Some(len) = self.range(addr, blocks) else {
            return ERROR;
        };
        if access
            .memory()
            .plugged(addr, len)
            .is_none_or(|(plugged, total)| plugged != total)
        {
            return ERROR;
        }
        match access.unplug(addr, len, |gpa, size| mapper.unmap(gpa, size)) {
            Ok(()) => {
                self.count(len, false);
                ACK
            }
            Err(e) => self.busy(&e),
        }
    }

    /// UNPLUG ALL: every plugged block unplugged; the usable region kept, as it may be
    /// (§5.15.6.4).
    fn unplug_all(&self, access: &Access<'_>, mapper: &impl Mapper) -> u16 {
        for (gpa, len) in access.memory().plugged_runs() {
            if let Err(e) = access.unplug(gpa, len, |gpa, size| mapper.unmap(gpa, size)) {
                return self.busy(&e);
            }
            self.count(len, false);
        }
        ACK
    }

    /// STATE: whether the blocks are all plugged, all unplugged, or some of each.
    fn query(&self, access: &Access<'_>, addr: u64, blocks: u16) -> (u16, u16) {
        let Some(len) = self.range(addr, blocks) else {
            return (ERROR, 0);
        };
        match access.memory().plugged(addr, len) {
            Some((plugged, total)) if plugged == total => (ACK, PLUGGED),
            Some((0, _)) => (ACK, UNPLUGGED),
            Some(_) => (ACK, MIXED),
            None => (ERROR, 0),
        }
    }

    /// What the host could not do, said once: the driver retries a BUSY request later.
    fn busy(&self, why: &str) -> u16 {
        if !self.said.swap(true, Ordering::Relaxed) {
            crate::warn!("virtio-mem: {why}; the driver is told to retry");
        }
        BUSY
    }
}

pub struct Mem<M> {
    sizes: Arc<Sizes>,
    mapper: M,
    /// What a worker needs besides its queue, from activation until reset.
    context: Option<(Arc<GuestMemory>, Arc<DeviceInterrupt>)>,
    worker: Option<Worker>,
    /// The queue while paused.
    paused: Option<Queue>,
}

impl<M> std::fmt::Debug for Mem<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mem")
            .field("sizes", &self.sizes)
            .finish_non_exhaustive()
    }
}

impl<M: Mapper + Clone + 'static> Mem<M> {
    /// A device for the memory `sizes` describes, which guest memory holds
    /// (`GuestMemory::with_hotplug`), mapped into the guest by `mapper`.
    pub fn new(sizes: Arc<Sizes>, mapper: M) -> Mem<M> {
        Mem {
            sizes,
            mapper,
            context: None,
            worker: None,
            paused: None,
        }
    }

    /// Starts the worker on `queue`; if it cannot, the queue waits paused, as a pause
    /// leaves it, for a resume or a reset.
    fn start(&mut self, queue: Queue) -> Result<(), String> {
        let Some((memory, interrupt)) = self.context.clone() else {
            self.paused = Some(queue);
            return Err("virtio-mem started before activation".into());
        };
        let (mem, sizes, mapper) = (memory.clone(), self.sizes.clone(), self.mapper.clone());
        let answering = move |chain: &Chain| answer(chain, &mem, &sizes, &mapper);
        match Worker::start("virtio-mem", queue, memory, interrupt, answering) {
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

impl<M: Mapper + Clone + 'static> VirtioDevice for Mem<M> {
    fn device_id(&self) -> u32 {
        DEVICE_ID
    }

    /// What it asks for, what is plugged, and its configuration's generation: a restored
    /// machine's memory has the same blocks plugged again (`vm::replug`, D176), and its
    /// driver reads the sizes it left.
    fn save(&self, w: &mut Writer) {
        let s = lock(&self.sizes.state);
        w.u64(s.requested);
        w.u64(s.plugged);
        w.u32(s.generation);
    }

    fn restore(&mut self, r: &mut Reader<'_>) -> codec::Result<()> {
        let (requested, plugged, generation) = (r.u64()?, r.u64()?, r.u32()?);
        if requested > self.sizes.region || plugged > self.sizes.region {
            return Err(codec::DecodeError(format!(
                "virtio-mem: {requested} bytes asked for and {plugged} plugged of a region of {}",
                self.sizes.region
            )));
        }
        let mut s = lock(&self.sizes.state);
        s.requested = requested;
        s.plugged = plugged;
        s.generation = generation;
        Ok(())
    }

    fn features(&self) -> u64 {
        feature::VERSION_1 | UNPLUGGED_INACCESSIBLE
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &[QUEUE_SIZE]
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let config = self.sizes.config();
        let at = usize::try_from(offset).ok();
        for (i, b) in data.iter_mut().enumerate() {
            *b = at
                .and_then(|at| at.checked_add(i))
                .and_then(|i| config.get(i))
                .copied()
                .unwrap_or(0);
        }
    }

    /// The configuration is the device's to change alone (§5.15.4.1).
    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}

    fn config_generation(&self) -> u32 {
        self.sizes.generation()
    }

    fn activate(&mut self, activation: Activation) -> Result<(), String> {
        let Activation {
            memory,
            mut queues,
            interrupt,
            features,
            ..
        } = activation;
        if features & UNPLUGGED_INACCESSIBLE == 0 {
            return Err("virtio-mem: the driver did not accept VIRTIO_MEM_F_UNPLUGGED_INACCESSIBLE, its promise never to touch unplugged memory, which the host withholds".into());
        }
        let queue = queues.pop().ok_or("virtio-mem activated without a queue")?;
        self.context = Some((memory, interrupt.clone()));
        lock(&self.sizes.state).interrupt = Some(interrupt);
        // An activation that fails leaves its queue: the driver sets the device up anew.
        self.start(queue).inspect_err(|_| self.paused = None)
    }

    /// Requests are answered on the device's worker, which plugs and unplugs with guest
    /// memory held: never on the notifying vCPU's thread.
    fn notify(&self, _queue: u16) {
        if let Some(w) = &self.worker {
            w.notify();
        }
    }

    /// Stops the worker. Plugged blocks stay plugged, and their memory as it is (§5.15.5.2).
    fn reset(&mut self) {
        self.stop();
        self.paused = None;
        self.context = None;
        lock(&self.sizes.state).interrupt = None;
    }

    /// As pmem's: the worker stops after the request it is answering, and what it left
    /// stays in the ring for the worker that resumes.
    fn pause(&mut self) -> Vec<QueueState> {
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

impl<M> Drop for Mem<M> {
    fn drop(&mut self) {
        if let Some(w) = self.worker.take() {
            w.stop();
        }
        lock(&self.sizes.state).interrupt = None;
    }
}

fn le16(b: &[u8]) -> u16 {
    b.first_chunk::<2>().map_or(0, |c| u16::from_le_bytes(*c))
}

fn le64(b: &[u8]) -> u64 {
    b.first_chunk::<8>().map_or(0, |c| u64::from_le_bytes(*c))
}

/// Answers one request: reads `struct virtio_mem_req` (le16 type; le64 addr and le16
/// nb_blocks at 8 and 16), does what it asks with guest memory held, and writes `struct
/// virtio_mem_resp` (le16 type; le16 state at 8). Returns the bytes written.
fn answer(chain: &Chain, memory: &GuestMemory, sizes: &Sizes, mapper: &impl Mapper) -> u32 {
    let (Some(req), Some(resp)) = (chain.readable().next(), chain.writable().next()) else {
        return 0;
    };
    if (req.len as usize) < REQ_LEN || (resp.len as usize) < RESP_LEN {
        return 0;
    }
    let Ok(access) = memory.access() else {
        return 0;
    };
    let mut raw = [0u8; REQ_LEN];
    if access.read(req.addr, &mut raw).is_err() {
        return 0;
    }
    let addr = raw.get(8..).map_or(0, le64);
    let blocks = raw.get(16..).map_or(0, le16);
    let (kind, state) = match le16(&raw) {
        PLUG => (sizes.plug(&access, addr, blocks, mapper), 0),
        UNPLUG => (sizes.unplug(&access, addr, blocks, mapper), 0),
        UNPLUG_ALL => (sizes.unplug_all(&access, mapper), 0),
        STATE => sizes.query(&access, addr, blocks),
        _ => (ERROR, 0),
    };
    let [k0, k1] = kind.to_le_bytes();
    let [s0, s1] = state.to_le_bytes();
    let response = [k0, k1, 0, 0, 0, 0, 0, 0, s0, s1];
    match access.write(resp.addr, &response) {
        Ok(()) => RESP_LEN as u32,
        Err(_) => 0,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::devices::virtio::queue::Descriptor;
    use crate::platform::page_size;

    const RAM: u64 = 0x8000_0000;
    const HOTPLUG: u64 = 0x1_0000_0000;
    const REGION: u64 = 64 * BLOCK;

    /// Records which blocks it has mapped, as Hypervisor.framework's stage 2 would hold
    /// them; refuses maps once told to.
    #[derive(Debug, Clone, Default)]
    struct Fake {
        mapped: Arc<Mutex<std::collections::BTreeSet<u64>>>,
        refuse: Arc<AtomicBool>,
    }

    impl Fake {
        fn blocks(gpa: u64, len: usize) -> impl Iterator<Item = u64> {
            (gpa..gpa + len as u64).step_by(BLOCK as usize)
        }

        fn mapped(&self) -> Vec<u64> {
            self.mapped.lock().unwrap().iter().copied().collect()
        }
    }

    impl Mapper for Fake {
        unsafe fn map(&self, _host: *mut u8, gpa: u64, len: usize) -> Result<(), String> {
            if self.refuse.load(Ordering::Relaxed) {
                return Err("refused".into());
            }
            for b in Fake::blocks(gpa, len) {
                if !self.mapped.lock().unwrap().insert(b) {
                    return Err(format!("{b:#x} mapped twice"));
                }
            }
            Ok(())
        }

        fn unmap(&self, gpa: u64, len: usize) -> Result<(), String> {
            for b in Fake::blocks(gpa, len) {
                if !self.mapped.lock().unwrap().remove(&b) {
                    return Err(format!("{b:#x} unmapped but not mapped"));
                }
            }
            Ok(())
        }
    }

    /// An interrupt line that counts its edges.
    #[derive(Default)]
    struct Edges(std::sync::atomic::AtomicUsize);

    impl crate::devices::Interrupt for Edges {
        fn set_level(&self, level: bool) {
            if level {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    struct Rig {
        memory: GuestMemory,
        sizes: Sizes,
        fake: Fake,
    }

    impl Rig {
        fn new() -> Rig {
            let p = page_size().unwrap();
            Rig {
                memory: GuestMemory::with_hotplug(&[(RAM, 4 * p)], HOTPLUG, REGION as usize, BLOCK).unwrap(),
                sizes: Sizes::new(HOTPLUG, REGION, REGION),
                fake: Fake::default(),
            }
        }

        /// Sends one request through guest RAM, as the driver lays it out; the response's
        /// type and state.
        fn ask(&self, kind: u16, addr: u64, blocks: u16) -> (u16, u16) {
            let mut req = [0u8; REQ_LEN];
            req[..2].copy_from_slice(&kind.to_le_bytes());
            req[8..16].copy_from_slice(&addr.to_le_bytes());
            req[16..18].copy_from_slice(&blocks.to_le_bytes());
            self.memory.access().unwrap().write(RAM, &req).unwrap();
            self.memory
                .access()
                .unwrap()
                .write(RAM + 64, &[0xee; RESP_LEN])
                .unwrap();
            let chain = Chain {
                head: 0,
                descriptors: vec![
                    Descriptor {
                        addr: RAM,
                        len: REQ_LEN as u32,
                        writable: false,
                    },
                    Descriptor {
                        addr: RAM + 64,
                        len: RESP_LEN as u32,
                        writable: true,
                    },
                ],
            };
            assert_eq!(
                answer(&chain, &self.memory, &self.sizes, &self.fake),
                RESP_LEN as u32
            );
            let mut resp = [0u8; RESP_LEN];
            self.memory.access().unwrap().read(RAM + 64, &mut resp).unwrap();
            assert_eq!(resp[2..8], [0; 6], "padding");
            (le16(&resp), le16(&resp[8..]))
        }

        fn config(&self, at: usize) -> u64 {
            le64(&self.sizes.config()[at..])
        }
    }

    /// A run's own bound, set as a restored VM takes it (D148): whole blocks, at most the
    /// region, and what was asked for past it lowered to it.
    #[test]
    fn the_most_is_a_runs_own_and_lowers_what_was_asked() {
        let s = Sizes::new(HOTPLUG, REGION, REGION);
        s.request(REGION);
        assert_eq!(s.requested(), REGION);
        s.set_most(3 * BLOCK + 5);
        assert_eq!((s.most(), s.requested()), (3 * BLOCK, 3 * BLOCK));
        s.request(REGION);
        assert_eq!(s.requested(), 3 * BLOCK, "asked for past it, given it");
        s.set_most(2 * REGION);
        assert_eq!(s.most(), REGION, "at most the region");
        assert_eq!(s.requested(), 3 * BLOCK, "a higher bound asks for nothing more");
    }

    /// What a device asks for and has plugged is saved, and a restored device says it as
    /// it was (D176): its driver reads the sizes it left. Sizes past its region are no
    /// snapshot of it.
    #[test]
    fn its_sizes_come_back_with_a_snapshot() {
        let sizes = Arc::new(Sizes::new(HOTPLUG, REGION, REGION));
        let device = Mem::new(sizes.clone(), Fake::default());
        sizes.request(3 * BLOCK);
        sizes.count(2 * BLOCK, true);
        assert_eq!(device.unsaved(), None, "plugged memory is saved");
        let mut w = Writer::default();
        device.save(&mut w);
        let bytes = w.into_bytes();
        let back = Arc::new(Sizes::new(HOTPLUG, REGION, REGION));
        let mut restored = Mem::new(back.clone(), Fake::default());
        let mut r = Reader::new(&bytes);
        restored.restore(&mut r).unwrap();
        r.finish().unwrap();
        assert_eq!(back.config(), sizes.config());
        assert_eq!(back.generation(), sizes.generation());
        let small = Arc::new(Sizes::new(HOTPLUG, BLOCK, BLOCK));
        assert!(
            Mem::new(small, Fake::default())
                .restore(&mut Reader::new(&bytes))
                .is_err()
        );
    }

    /// A plug within the size asked for is the guest's: the host's devices reach it, and
    /// it reads as zeros; past the size asked for, NACK.
    #[test]
    fn a_plug_within_the_request_is_the_guests() {
        let r = Rig::new();
        assert_eq!(r.ask(PLUG, HOTPLUG, 2), (NACK, 0), "nothing asked for yet");
        r.sizes.request(3 * BLOCK);
        assert_eq!(r.ask(PLUG, HOTPLUG, 2), (ACK, 0));
        assert_eq!(r.config(40), 2 * BLOCK, "plugged_size");
        assert_eq!(r.fake.mapped(), [HOTPLUG, HOTPLUG + BLOCK]);
        let a = r.memory.access().unwrap();
        assert_eq!(a.read_obj::<u64>(HOTPLUG + BLOCK).unwrap(), 0);
        a.write_obj(HOTPLUG + 2 * BLOCK - 8, 0x5au64).unwrap();
        assert!(
            a.read_obj::<u8>(HOTPLUG + 2 * BLOCK).is_err(),
            "the next block is not plugged"
        );
        drop(a);
        // Two more blocks would be four, past the three asked for.
        assert_eq!(r.ask(PLUG, HOTPLUG + 2 * BLOCK, 2), (NACK, 0));
        assert_eq!(r.ask(PLUG, HOTPLUG + 2 * BLOCK, 1), (ACK, 0));
        assert_eq!(r.config(40), 3 * BLOCK);
    }

    /// A request naming blocks it may not is an ERROR (§5.15.6.2-4): unaligned, none, past
    /// the usable region, or before it; a plug of plugged blocks, an unplug of unplugged.
    #[test]
    fn requests_out_of_bounds_or_state_are_errors() {
        let r = Rig::new();
        r.sizes.request(REGION);
        for kind in [PLUG, UNPLUG, STATE] {
            assert_eq!(r.ask(kind, HOTPLUG + 4096, 1).0, ERROR);
            assert_eq!(r.ask(kind, HOTPLUG, 0).0, ERROR);
            assert_eq!(r.ask(kind, HOTPLUG + REGION - BLOCK, 2).0, ERROR);
            assert_eq!(r.ask(kind, HOTPLUG - BLOCK, 1).0, ERROR);
            assert_eq!(r.ask(kind, u64::MAX - BLOCK + 1, 1).0, ERROR);
        }
        assert_eq!(r.ask(UNPLUG, HOTPLUG, 1), (ERROR, 0), "not plugged");
        assert_eq!(r.ask(PLUG, HOTPLUG, 2), (ACK, 0));
        assert_eq!(r.ask(PLUG, HOTPLUG + BLOCK, 2), (ERROR, 0), "partly plugged");
        assert_eq!(r.ask(UNPLUG, HOTPLUG + BLOCK, 2), (ERROR, 0), "partly unplugged");
        assert_eq!(r.ask(9, HOTPLUG, 1), (ERROR, 0), "no such request");
        assert_eq!(r.config(40), 2 * BLOCK, "nothing refused changed what is plugged");
    }

    /// An unplug takes the blocks from the guest and the host's devices, and a block
    /// plugged again holds zeros, not what it held: its pages went back to the host.
    #[test]
    fn an_unplugged_block_is_no_ones_and_comes_back_empty() {
        let r = Rig::new();
        r.sizes.request(4 * BLOCK);
        assert_eq!(r.ask(PLUG, HOTPLUG, 4), (ACK, 0));
        r.memory
            .access()
            .unwrap()
            .write_obj(HOTPLUG + BLOCK + 16, 0xfeed_u64)
            .unwrap();
        assert_eq!(r.ask(UNPLUG, HOTPLUG + BLOCK, 1), (ACK, 0));
        assert_eq!(r.config(40), 3 * BLOCK);
        assert!(r.memory.host_ptr(HOTPLUG + BLOCK, 1).is_err());
        assert!(
            r.memory.host_ptr(HOTPLUG, 2 * BLOCK as usize).is_err(),
            "a range across it"
        );
        assert!(r.memory.host_ptr(HOTPLUG, BLOCK as usize).is_ok());
        assert_eq!(r.ask(STATE, HOTPLUG, 4), (ACK, MIXED));
        assert_eq!(r.ask(STATE, HOTPLUG + BLOCK, 1), (ACK, UNPLUGGED));
        assert_eq!(r.ask(STATE, HOTPLUG + 2 * BLOCK, 2), (ACK, PLUGGED));
        assert_eq!(r.ask(PLUG, HOTPLUG + BLOCK, 1), (ACK, 0));
        assert_eq!(
            r.memory
                .access()
                .unwrap()
                .read_obj::<u64>(HOTPLUG + BLOCK + 16)
                .unwrap(),
            0
        );
        // Unplug all, across the runs it is plugged in.
        assert_eq!(r.ask(UNPLUG, HOTPLUG + 2 * BLOCK, 1), (ACK, 0));
        assert_eq!(r.ask(UNPLUG_ALL, 0, 0), (ACK, 0));
        assert_eq!(r.config(40), 0);
        assert!(r.memory.plugged_runs().is_empty());
        assert!(r.fake.mapped().is_empty(), "everything unmapped");
        assert_eq!(r.ask(STATE, HOTPLUG, 64), (ACK, UNPLUGGED));
    }

    /// A plug the hypervisor refuses is BUSY, and leaves the blocks unplugged and withheld.
    #[test]
    fn a_plug_the_host_cannot_make_is_busy_and_undone() {
        let r = Rig::new();
        r.sizes.request(REGION);
        r.fake.refuse.store(true, Ordering::Relaxed);
        assert_eq!(r.ask(PLUG, HOTPLUG, 1), (BUSY, 0));
        assert_eq!(r.config(40), 0);
        assert!(r.memory.host_ptr(HOTPLUG, 1).is_err());
        r.fake.refuse.store(false, Ordering::Relaxed);
        assert_eq!(r.ask(PLUG, HOTPLUG, 1), (ACK, 0));
    }

    /// A request may name only blocks of the usable region, aligned, at least one
    /// (§5.15.6.2-4), on its own word: guest memory's bounds are a second check, not this.
    #[test]
    fn a_request_names_whole_blocks_of_the_usable_region() {
        let s = Sizes::new(HOTPLUG, REGION, REGION);
        assert_eq!(s.range(HOTPLUG, 1), Some(BLOCK));
        assert_eq!(s.range(HOTPLUG + REGION - BLOCK, 1), Some(BLOCK));
        assert_eq!(s.range(HOTPLUG, 64), Some(REGION));
        assert_eq!(s.range(HOTPLUG - BLOCK, 1), None, "before the region");
        assert_eq!(s.range(HOTPLUG - BLOCK, 2), None, "across its start");
        assert_eq!(s.range(HOTPLUG + REGION - BLOCK, 2), None, "across its end");
        assert_eq!(s.range(HOTPLUG + REGION, 1), None, "after it");
        assert_eq!(s.range(HOTPLUG + 4096, 1), None, "unaligned");
        assert_eq!(s.range(HOTPLUG, 0), None, "no blocks");
        assert_eq!(
            s.range(u64::MAX - BLOCK + 1, u16::MAX),
            None,
            "past the address space"
        );
    }

    /// The size asked for is whole blocks within the most the guest may have; each change
    /// moves the configuration's generation, and the configuration says the region whole
    /// as usable.
    #[test]
    fn requests_are_whole_blocks_within_the_most() {
        let s = Sizes::new(HOTPLUG, REGION, 10 * BLOCK + 5);
        assert_eq!(s.most(), 10 * BLOCK, "the most is whole blocks");
        let g = s.generation();
        s.request(BLOCK + 1);
        assert_eq!(s.requested(), 2 * BLOCK);
        assert_ne!(s.generation(), g);
        let g = s.generation();
        s.request(2 * BLOCK);
        assert_eq!(s.generation(), g, "no change, no new generation");
        // But the driver is told again: it tries what it could not plug at once.
        let edges = Arc::new(Edges::default());
        lock(&s.state).interrupt = Some(Arc::new(DeviceInterrupt::new(edges.clone())));
        s.request(2 * BLOCK);
        s.request(2 * BLOCK);
        assert_eq!(edges.0.load(Ordering::SeqCst), 2, "each request tells the driver");
        assert_eq!(s.generation(), g);
        s.request(u64::MAX);
        assert_eq!(s.requested(), 10 * BLOCK);
        let c = s.config();
        assert_eq!(le64(&c), BLOCK);
        assert_eq!(le16(&c[8..]), 0, "node_id");
        assert_eq!(le64(&c[16..]), HOTPLUG);
        assert_eq!(le64(&c[24..]), REGION);
        assert_eq!(le64(&c[32..]), REGION, "usable_region_size");
        assert_eq!(le64(&c[48..]), 10 * BLOCK);
    }
}
