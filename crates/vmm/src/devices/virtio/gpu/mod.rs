//! virtio-gpu (virtio 1.3 §5.7) for Vulkan through Venus (D144): no scanout and no virgl
//! GL; contexts of the capability sets its renderer offers, blob resources, and the host
//! memory of mappable blobs mapped into its shared memory window (region 1, host-visible),
//! where the guest maps them in turn.
//!
//! The renderer runs on the device's worker. A request that asks for a fence is answered
//! once the renderer signals it, in order with the fenced requests before it on its
//! timeline (a context's ring, or the device's own): the driver signals every fence of a
//! timeline before one it is answered (virtgpu_fence.c), so none is answered ahead of one
//! still running.

pub mod protocol;
#[cfg(all(feature = "gpu", target_os = "macos"))]
pub mod virgl;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle, Thread};

use super::queue::{Chain, Queue, QueueError, QueueState, Round, serve_round, with};
use super::worker::spawn;
use super::{Activation, DeviceInterrupt, ShmRegion, VirtioDevice, feature as virtio};
use crate::memory::GuestMemory;
use crate::{debug, warn};
use protocol::{Command, Header, Refused, resp};

pub const DEVICE_ID: u32 = 16;

/// Its feature bits (include/uapi/linux/virtio_gpu.h).
pub mod feature {
    /// 3D: the driver's execbuffer and context ioctls are offered only with it.
    pub const VIRGL: u64 = 1 << 0;
    pub const RESOURCE_BLOB: u64 = 1 << 3;
    pub const CONTEXT_INIT: u64 = 1 << 4;
    /// Blobs' sizes, and their places in the window, a multiple of `blob_alignment`
    /// (Linux 7.2).
    pub const BLOB_ALIGNMENT: u64 = 1 << 5;
}

/// The host-visible memory region's id (VIRTIO_GPU_SHM_ID_HOST_VISIBLE).
pub const HOST_VISIBLE: u8 = 1;

/// The control queue's size and the cursor queue's, QEMU's and libkrun's: no scanout, so
/// no cursor is ever sent.
const QUEUE_SIZES: [u16; 2] = [256, 16];

/// A capability set the renderer offers (`struct virtio_gpu_resp_capset_info`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capset {
    pub id: u32,
    pub max_version: u32,
    pub max_size: u32,
}

/// A blob resource as the driver asks for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Blob {
    pub resource: u32,
    pub mem: u32,
    pub flags: u32,
    pub id: u64,
    pub size: u64,
}

/// Host memory of a blob that may be mapped into the window: `len` bytes at `host`, whole
/// host pages that hold nothing but the blob's, and how the guest is to cache it
/// (VIRTIO_GPU_MAP_CACHE_*).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostMemory {
    pub host: *mut u8,
    pub len: usize,
    pub cache: u32,
}

/// A fence the renderer has signalled: everything submitted before it on `ring` of `ctx`
/// is done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signal {
    pub ctx: u32,
    pub ring: u8,
    pub fence: u64,
}

/// What renders for the device (virglrenderer's Venus, or a test's). Called on the
/// device's worker alone.
pub trait Renderer: Send {
    /// The capability sets it offers, in the order the driver asks for them by index.
    fn capsets(&self) -> &[Capset];
    fn capset(&mut self, id: u32, version: u32) -> Result<Vec<u8>, Refused>;
    fn create_context(&mut self, ctx: u32, capset: u32, name: &[u8]) -> Result<(), Refused>;
    fn destroy_context(&mut self, ctx: u32);
    fn attach(&mut self, ctx: u32, resource: u32);
    fn detach(&mut self, ctx: u32, resource: u32);
    /// Creates `blob` for `ctx`, backed by the guest memory at `guest` where it has any:
    /// each a host address of guest RAM and its length, which live as long as the VM's
    /// memory does.
    fn create_blob(&mut self, ctx: u32, blob: &Blob, guest: &[(*mut u8, usize)]) -> Result<(), Refused>;
    fn unref(&mut self, resource: u32);
    fn submit(&mut self, ctx: u32, commands: &mut [u8]) -> Result<(), Refused>;
    /// Asks for `fence` on `ring` of `ctx` once what was submitted there before it is
    /// done, which `signals` then says.
    fn fence(&mut self, ctx: u32, ring: u8, fence: u64) -> Result<(), Refused>;
    /// The fences signalled since it was last asked, in `into`.
    fn signals(&mut self, into: &mut Vec<Signal>);
    /// Has `worker` woken as a fence is signalled.
    fn wake(&mut self, worker: Thread);
    /// The host memory of mappable resource `resource`.
    fn map(&mut self, resource: u32) -> Result<HostMemory, Refused>;
    fn unmap(&mut self, resource: u32);
    /// Destroys every context and resource: the driver reset the device.
    fn reset(&mut self);
}

/// Maps host memory into the guest's physical address space and unmaps it: the
/// hypervisor's stage 2, or a test's.
pub trait Window: Send + Sync {
    /// # Safety
    /// `host..host+len` must stay mapped in this process until unmapped here or the VM is
    /// destroyed.
    unsafe fn map(&self, host: *mut u8, gpa: u64, len: usize) -> Result<(), String>;
    fn unmap(&self, gpa: u64, len: usize) -> Result<(), String>;
}

/// Where a fenced answer waits: a context's ring, or the device's own timeline for those
/// that name no ring, whose work is done as its command returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Timeline {
    Device,
    Ring { ctx: u32, ring: u8 },
}

/// An answer written, its request not yet used: until `fence` is signalled, if it waits
/// for one, and until every answer before it on its timeline is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Waiting {
    head: u16,
    len: u32,
    fence: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Resource {
    size: u64,
    /// Where it is in the window, and how much of it, while mapped.
    mapped: Option<(u64, u64)>,
}

/// The device's side of the guest's GPU work: its renderer and window, the contexts and
/// resources the guest made, what of them is mapped, and the answers that wait for fences.
/// Served by one thread at a time: the device's worker, or the device while none runs.
pub struct Served {
    renderer: Box<dyn Renderer>,
    window: Box<dyn Window>,
    /// The window, where the guest sees it.
    region: ShmRegion,
    /// What a blob's place and length in the window are a multiple of: the host's page.
    granule: u64,
    /// The largest request read: no legitimate command outsizes the memory it comes from.
    max_request: usize,
    contexts: HashMap<u32, u32>,
    resources: HashMap<u32, Resource>,
    /// The window's mapped ranges, by offset: each one's length.
    mapped: BTreeMap<u64, u64>,
    waiting: HashMap<Timeline, VecDeque<Waiting>>,
    /// The requests whose answers wait, by head: no more than the queue's size, as a driver
    /// makes none available again before it is used (else the ring is malformed).
    held: HashSet<u16>,
    /// Answers let go by a command, not by their fences: a destroyed context's.
    released: Vec<(u16, u32)>,
    /// Whether a refused map of a place in the window the host cannot map has been said.
    said_unaligned: bool,
}

impl std::fmt::Debug for Served {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Served")
            .field("region", &self.region)
            .field("contexts", &self.contexts.len())
            .field("resources", &self.resources.len())
            .field("mapped", &self.mapped.len())
            .finish_non_exhaustive()
    }
}

/// An answer: its kind and the fields after its header.
type Answer = (u32, Vec<u8>);

fn nodata() -> Result<Answer, Refused> {
    Ok((resp::OK_NODATA, Vec::new()))
}

/// `struct virtio_gpu_resp_display_info`'s fields: 16 scanouts, none enabled.
const DISPLAY_INFO: usize = 16 * 24;

impl Served {
    pub fn new(
        renderer: Box<dyn Renderer>,
        window: Box<dyn Window>,
        region: ShmRegion,
        granule: u64,
        max_request: usize,
    ) -> Served {
        Served {
            renderer,
            window,
            region,
            granule,
            max_request,
            contexts: HashMap::new(),
            resources: HashMap::new(),
            mapped: BTreeMap::new(),
            waiting: HashMap::new(),
            held: HashSet::new(),
            released: Vec::new(),
            said_unaligned: false,
        }
    }

    /// Serves a control queue request: its answer written into it, and the requests to use
    /// now, each with the bytes written into it: none of this one while its answer waits
    /// for its fence.
    fn serve(&mut self, chain: &Chain, memory: &GuestMemory) -> Result<Vec<(u16, u32)>, QueueError> {
        if self.held.contains(&chain.head) {
            return Err(QueueError::InUse(chain.head));
        }
        let read = match memory.access() {
            Ok(a) => protocol::read(chain, &a, self.max_request),
            Err(_) => None,
        };
        // Too short for a header: nothing to answer, nor any fence to say.
        let Some((header, command)) = read else {
            return Ok(vec![(chain.head, 0)]);
        };
        let answered = command.and_then(|c| self.command(&header, c, memory));
        // A fence asked for is created once its command is done, and signalled with what
        // was submitted before it on its ring.
        let (kind, fields, fence) = match (answered, header.fence()) {
            (Ok((kind, fields)), Some((fence, Some(ring)))) => {
                match self.renderer.fence(header.ctx_id, ring, fence) {
                    Ok(()) => (kind, fields, Some(fence)),
                    Err(Refused(e)) => (e, Vec::new(), None),
                }
            }
            (Ok((kind, fields)), _) => (kind, fields, None),
            (Err(Refused(e)), _) => (e, Vec::new(), None),
        };
        let len = match memory.access() {
            Ok(a) => protocol::answer(chain, &a, &header, kind, &fields),
            Err(_) => 0,
        };
        let Some((_, ring)) = header.fence() else {
            let mut used = std::mem::take(&mut self.released);
            used.push((chain.head, len));
            return Ok(used);
        };
        let timeline = match ring {
            Some(ring) => Timeline::Ring {
                ctx: header.ctx_id,
                ring,
            },
            None => Timeline::Device,
        };
        self.waiting.entry(timeline).or_default().push_back(Waiting {
            head: chain.head,
            len,
            fence,
        });
        self.held.insert(chain.head);
        let mut used = std::mem::take(&mut self.released);
        used.extend(self.let_go(timeline, None));
        Ok(used)
    }

    /// The answers a fence's signal lets go, in order.
    fn signalled(&mut self, s: Signal) -> Vec<(u16, u32)> {
        let timeline = Timeline::Ring {
            ctx: s.ctx,
            ring: s.ring,
        };
        self.let_go(timeline, Some(s.fence))
    }

    /// Lets go of what waits at the front of `timeline`: those waiting for no fence, and
    /// those for `signalled` or a fence before it, which a signal stands for (MERGEABLE
    /// fences).
    fn let_go(&mut self, timeline: Timeline, signalled: Option<u64>) -> Vec<(u16, u32)> {
        let mut used = Vec::new();
        let Some(queue) = self.waiting.get_mut(&timeline) else {
            return used;
        };
        while let Some(w) = queue.front() {
            let done = match (w.fence, signalled) {
                (None, _) => true,
                (Some(f), Some(s)) => f <= s,
                (Some(_), None) => false,
            };
            if !done {
                break;
            }
            used.push((w.head, w.len));
            self.held.remove(&w.head);
            queue.pop_front();
        }
        used
    }

    fn command(
        &mut self,
        header: &Header,
        command: Command,
        memory: &GuestMemory,
    ) -> Result<Answer, Refused> {
        let ctx = header.ctx_id;
        let invalid = Refused(resp::ERR_INVALID_PARAMETER);
        match command {
            Command::GetDisplayInfo => Ok((resp::OK_DISPLAY_INFO, vec![0; DISPLAY_INFO])),
            Command::GetCapsetInfo { index } => {
                let c = usize::try_from(index)
                    .ok()
                    .and_then(|i| self.renderer.capsets().get(i).copied())
                    .ok_or(invalid)?;
                let mut fields = Vec::with_capacity(16);
                for v in [c.id, c.max_version, c.max_size, 0] {
                    fields.extend(v.to_le_bytes());
                }
                Ok((resp::OK_CAPSET_INFO, fields))
            }
            Command::GetCapset { id, version } => {
                let offered = self
                    .renderer
                    .capsets()
                    .iter()
                    .any(|c| c.id == id && version <= c.max_version);
                if !offered {
                    return Err(invalid);
                }
                Ok((resp::OK_CAPSET, self.renderer.capset(id, version)?))
            }
            Command::CtxCreate { context_init, name } => {
                let capset = context_init & 0xff;
                if ctx == 0 || self.contexts.contains_key(&ctx) {
                    return Err(Refused(resp::ERR_INVALID_CONTEXT_ID));
                }
                if !self.renderer.capsets().iter().any(|c| c.id == capset) {
                    return Err(invalid);
                }
                self.renderer.create_context(ctx, capset, &name)?;
                self.contexts.insert(ctx, capset);
                nodata()
            }
            Command::CtxDestroy => {
                if self.contexts.remove(&ctx).is_none() {
                    return Err(Refused(resp::ERR_INVALID_CONTEXT_ID));
                }
                self.renderer.destroy_context(ctx);
                // What waited for its fences waits for nothing now: the driver destroys a
                // context once nothing of it is used.
                let rings: Vec<Timeline> = self
                    .waiting
                    .keys()
                    .filter(|t| matches!(t, Timeline::Ring { ctx: c, .. } if *c == ctx))
                    .copied()
                    .collect();
                for t in rings {
                    for w in self.waiting.remove(&t).unwrap_or_default() {
                        self.held.remove(&w.head);
                        self.released.push((w.head, w.len));
                    }
                }
                nodata()
            }
            Command::CtxAttachResource { resource } => {
                self.known(ctx, resource)?;
                self.renderer.attach(ctx, resource);
                nodata()
            }
            Command::CtxDetachResource { resource } => {
                self.known(ctx, resource)?;
                self.renderer.detach(ctx, resource);
                nodata()
            }
            Command::ResourceCreateBlob {
                resource,
                mem,
                flags,
                id,
                size,
                entries,
            } => {
                if resource == 0 || self.resources.contains_key(&resource) {
                    return Err(Refused(resp::ERR_INVALID_RESOURCE_ID));
                }
                // Each entry guest RAM, reached at its host address.
                let mut guest = Vec::with_capacity(entries.len());
                for e in &entries {
                    let len = usize::try_from(e.len).map_err(|_| invalid)?;
                    let host = memory.host_ptr(e.addr, len).map_err(|_| invalid)?;
                    guest.push((host, len));
                }
                let blob = Blob {
                    resource,
                    mem,
                    flags,
                    id,
                    size,
                };
                self.renderer.create_blob(ctx, &blob, &guest)?;
                self.resources.insert(resource, Resource { size, mapped: None });
                nodata()
            }
            Command::ResourceUnref { resource } => {
                let r = self
                    .resources
                    .remove(&resource)
                    .ok_or(Refused(resp::ERR_INVALID_RESOURCE_ID))?;
                if r.mapped.is_some() {
                    self.unmap(resource, r);
                }
                self.renderer.unref(resource);
                nodata()
            }
            Command::MapBlob { resource, offset } => {
                let r = *self
                    .resources
                    .get(&resource)
                    .ok_or(Refused(resp::ERR_INVALID_RESOURCE_ID))?;
                if r.mapped.is_some() {
                    return Err(invalid);
                }
                if !offset.is_multiple_of(self.granule) {
                    if !self.said_unaligned {
                        self.said_unaligned = true;
                        warn!(
                            "virtio-gpu: the guest mapped a blob {offset:#x} into its window, which the host maps in pages of {} bytes: its driver does not align blobs (VIRTIO_GPU_F_BLOB_ALIGNMENT, Linux 7.2)",
                            self.granule
                        );
                    }
                    return Err(invalid);
                }
                let host = self.renderer.map(resource)?;
                let len = host.len as u64;
                let fits = offset
                    .checked_add(len)
                    .is_some_and(|end| end <= self.region.len && self.free(offset, end));
                if !fits || !len.is_multiple_of(self.granule) || len == 0 {
                    self.renderer.unmap(resource);
                    return Err(invalid);
                }
                let gpa = self.region.base + offset;
                // SAFETY: the renderer's memory of the resource, which stays mapped in this
                // process until the window lets it go (`unmap`) or the VM goes.
                if let Err(e) = unsafe { self.window.map(host.host, gpa, host.len) } {
                    debug!("virtio-gpu: mapping resource {resource} at {gpa:#x}: {e}");
                    self.renderer.unmap(resource);
                    return Err(Refused(resp::ERR_UNSPEC));
                }
                self.mapped.insert(offset, len);
                if let Some(r) = self.resources.get_mut(&resource) {
                    r.mapped = Some((offset, len));
                }
                let mut fields = host.cache.to_le_bytes().to_vec();
                fields.extend([0u8; 4]);
                Ok((resp::OK_MAP_INFO, fields))
            }
            Command::UnmapBlob { resource } => {
                let r = *self
                    .resources
                    .get(&resource)
                    .ok_or(Refused(resp::ERR_INVALID_RESOURCE_ID))?;
                if r.mapped.is_none() {
                    return Err(invalid);
                }
                self.unmap(resource, r);
                nodata()
            }
            Command::Submit3d { mut commands } => {
                if !self.contexts.contains_key(&ctx) {
                    return Err(Refused(resp::ERR_INVALID_CONTEXT_ID));
                }
                if !commands.len().is_multiple_of(4) {
                    return Err(invalid);
                }
                self.renderer.submit(ctx, &mut commands)?;
                nodata()
            }
            // Resources of the guest's own memory are blobs alone here.
            Command::AttachBacking { .. } | Command::DetachBacking { .. } | Command::Unsupported(_) => {
                Err(Refused(resp::ERR_UNSPEC))
            }
        }
    }

    /// Refuses a context or resource the guest has not made.
    fn known(&self, ctx: u32, resource: u32) -> Result<(), Refused> {
        if !self.contexts.contains_key(&ctx) {
            return Err(Refused(resp::ERR_INVALID_CONTEXT_ID));
        }
        if !self.resources.contains_key(&resource) {
            return Err(Refused(resp::ERR_INVALID_RESOURCE_ID));
        }
        Ok(())
    }

    /// Whether `start..end` of the window is free of every mapping.
    fn free(&self, start: u64, end: u64) -> bool {
        let before = self
            .mapped
            .range(..start)
            .next_back()
            .is_none_or(|(at, len)| at + len <= start);
        let after = self.mapped.range(start..).next().is_none_or(|(at, _)| *at >= end);
        before && after
    }

    /// Takes `resource`'s memory out of the window.
    fn unmap(&mut self, resource: u32, r: Resource) {
        let Some((offset, len)) = r.mapped else { return };
        if let Err(e) = self.window.unmap(self.region.base + offset, len as usize) {
            warn!("virtio-gpu: unmapping resource {resource}: {e}");
        }
        self.mapped.remove(&offset);
        self.renderer.unmap(resource);
        if let Some(r) = self.resources.get_mut(&resource) {
            r.mapped = None;
        }
    }

    /// Forgets everything the guest made: the driver reset the device.
    fn reset(&mut self) {
        for (offset, len) in std::mem::take(&mut self.mapped) {
            if let Err(e) = self.window.unmap(self.region.base + offset, len as usize) {
                warn!("virtio-gpu: unmapping the window at {offset:#x}: {e}");
            }
        }
        self.renderer.reset();
        self.contexts.clear();
        self.resources.clear();
        self.waiting.clear();
        self.held.clear();
        self.released.clear();
    }
}

/// One round of the device's work: the answers fences let go, then the control queue's
/// requests, a ring's worth at most and none once `stop` is set, then the cursor queue's;
/// the driver interrupted if it wants to hear of what was used.
fn round(
    queues: &mut [Queue; 2],
    served: &mut Served,
    memory: &GuestMemory,
    irq: &DeviceInterrupt,
    stop: &AtomicBool,
    signals: &mut Vec<Signal>,
) -> Result<Round, QueueError> {
    let [control, cursor] = queues;
    let mut used = Vec::new();
    served.renderer.signals(signals);
    for s in signals.drain(..) {
        used.extend(served.signalled(s));
    }
    let budget = usize::from(control.size());
    let mut taken = 0;
    let mut chain = Chain::default();
    let idle = 'round: loop {
        with(memory, |a| control.disable_notification(a))?;
        loop {
            if taken == budget || stop.load(Ordering::Acquire) {
                break 'round false;
            }
            if !with(memory, |a| control.pop_into(a, &mut chain))? {
                break;
            }
            taken += 1;
            used.extend(served.serve(&chain, memory)?);
        }
        if !with(memory, |a| control.enable_notification(a))? {
            break true;
        }
    };
    if !used.is_empty() {
        with(memory, |a| control.add_used_all(a, used.iter().copied()))?;
        if with(memory, |a| control.needs_interrupt(a))? {
            irq.used_buffer();
        }
    }
    // No scanout shows a cursor: what the cursor queue is sent is used as it comes.
    let cursors = serve_round(cursor, memory, irq, stop, &mut |_| 0)?;
    Ok(if idle && cursors == Round::Idle {
        Round::Idle
    } else {
        Round::More
    })
}

/// What a stopped worker gives back: its queues, unless their ring was malformed, and what
/// it served with.
type Stopped = (Option<[Queue; 2]>, Box<Served>);

/// The device's worker, which has its queues and what it serves with until stopped.
struct Running {
    thread: JoinHandle<Option<Stopped>>,
    waker: Thread,
    stop: Arc<AtomicBool>,
}

impl Running {
    /// Starts the worker on `queues` with `served`; a worker the system refuses gives them
    /// back.
    fn start(
        queues: [Queue; 2],
        served: Box<Served>,
        memory: Arc<GuestMemory>,
        interrupt: Arc<DeviceInterrupt>,
    ) -> Result<Running, (String, [Queue; 2], Box<Served>)> {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = spawn("virtio-gpu", (queues, served), move |(mut queues, mut served)| {
            served.renderer.wake(thread::current());
            let mut signals = Vec::new();
            let whole = loop {
                if stopping.load(Ordering::Acquire) {
                    break true;
                }
                match round(
                    &mut queues,
                    &mut served,
                    &memory,
                    &interrupt,
                    &stopping,
                    &mut signals,
                ) {
                    Ok(Round::Idle) => thread::park(),
                    Ok(Round::More) => {}
                    // A malformed ring: the device needs reset (virtio 1.3 §2.1.2).
                    Err(e) => {
                        if interrupt.fail() {
                            warn!("virtio-gpu: {e}; device needs reset");
                        } else {
                            debug!("virtio-gpu: {e}; device needs reset");
                        }
                        break false;
                    }
                }
            };
            Some((whole.then_some(queues), served))
        })
        .map_err(|(e, (queues, served))| (e, queues, served))?;
        let waker = thread.thread().clone();
        Ok(Running { thread, waker, stop })
    }

    /// Stops the worker after the request it is serving: its queues, unless the ring was
    /// malformed, and what it served with.
    fn stop(self) -> Option<Stopped> {
        self.stop.store(true, Ordering::Release);
        self.waker.unpark();
        match self.thread.join() {
            Ok(back) => back,
            Err(_) => {
                warn!("the virtio-gpu worker ended abnormally");
                None
            }
        }
    }
}

/// `struct virtio_gpu_config`: events_read, events_clear, num_scanouts, num_capsets,
/// blob_alignment.
const CONFIG_LEN: usize = 20;

/// The virtio-gpu device.
pub struct Gpu {
    capsets: u32,
    blob_alignment: u32,
    regions: [ShmRegion; 1],
    /// What it serves with, while no worker has it.
    served: Option<Box<Served>>,
    /// What a worker needs besides its queues, from activation until reset.
    context: Option<(Arc<GuestMemory>, Arc<DeviceInterrupt>)>,
    running: Option<Running>,
    /// The queues while paused.
    paused: Option<[Queue; 2]>,
}

impl std::fmt::Debug for Gpu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gpu")
            .field("capsets", &self.capsets)
            .field("regions", &self.regions)
            .finish_non_exhaustive()
    }
}

impl Gpu {
    /// A device rendering with `served`'s renderer, its window `served`'s region.
    pub fn new(served: Box<Served>) -> Gpu {
        Gpu {
            capsets: u32::try_from(served.renderer.capsets().len()).unwrap_or(u32::MAX),
            blob_alignment: u32::try_from(served.granule).unwrap_or(u32::MAX),
            regions: [served.region],
            served: Some(served),
            context: None,
            running: None,
            paused: None,
        }
    }

    /// Starts the worker on `queues`; if it cannot, they wait paused, as a pause leaves
    /// them, for a resume or a reset.
    fn start(&mut self, queues: [Queue; 2]) -> Result<(), String> {
        let (Some((memory, interrupt)), Some(served)) = (self.context.clone(), self.served.take()) else {
            self.paused = Some(queues);
            return Err("virtio-gpu started before activation".into());
        };
        match Running::start(queues, served, memory, interrupt) {
            Ok(running) => {
                self.running = Some(running);
                Ok(())
            }
            Err((e, queues, served)) => {
                self.served = Some(served);
                self.paused = Some(queues);
                Err(e)
            }
        }
    }

    /// Stops the worker after the request it is serving; its queues come back.
    fn stop(&mut self) -> Option<[Queue; 2]> {
        let (queues, served) = self.running.take()?.stop()?;
        self.served = Some(served);
        queues
    }
}

impl VirtioDevice for Gpu {
    fn device_id(&self) -> u32 {
        DEVICE_ID
    }

    fn features(&self) -> u64 {
        virtio::VERSION_1
            | virtio::EVENT_IDX
            | virtio::INDIRECT_DESC
            | feature::VIRGL
            | feature::RESOURCE_BLOB
            | feature::CONTEXT_INIT
            | feature::BLOB_ALIGNMENT
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &QUEUE_SIZES
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let mut config = [0u8; CONFIG_LEN];
        protocol::put(&mut config, 12, &self.capsets.to_le_bytes());
        protocol::put(&mut config, 16, &self.blob_alignment.to_le_bytes());
        for (i, b) in data.iter_mut().enumerate() {
            *b = usize::try_from(offset)
                .ok()
                .and_then(|at| at.checked_add(i))
                .and_then(|i| config.get(i))
                .copied()
                .unwrap_or(0);
        }
    }

    /// events_clear: no display, so no event to clear.
    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}

    fn shm_regions(&self) -> &[ShmRegion] {
        &self.regions
    }

    fn activate(&mut self, activation: Activation) -> Result<(), String> {
        let Activation {
            memory,
            queues,
            interrupt,
            ..
        } = activation;
        let queues: [Queue; 2] = queues
            .try_into()
            .map_err(|q: Vec<Queue>| format!("virtio-gpu activated with {} queues", q.len()))?;
        self.context = Some((memory, interrupt));
        // An activation that fails leaves its queues: the driver sets the device up anew.
        self.start(queues).inspect_err(|_| self.paused = None)
    }

    /// Requests are served on the device's worker, never on the notifying vCPU's thread.
    fn notify(&self, _queue: u16) {
        if let Some(r) = &self.running {
            r.waker.unpark();
        }
    }

    fn reset(&mut self) {
        self.stop();
        self.paused = None;
        self.context = None;
        if let Some(served) = self.served.as_mut() {
            served.reset();
        }
    }

    fn pause(&mut self) -> Vec<QueueState> {
        if let Some(queues) = self.stop() {
            self.paused = Some(queues);
        }
        self.paused.iter().flatten().map(Queue::state).collect()
    }

    fn resume(&mut self) -> Result<(), String> {
        match self.paused.take() {
            Some(queues) => self.start(queues),
            None => Ok(()),
        }
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        self.reset();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::devices::virtio::queue::{Descriptor, QueueConfig};
    use protocol::{FLAG_FENCE, FLAG_INFO_RING_IDX, HEADER, cmd};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    const RAM: u64 = 0x8000_0000;
    const RAM_LEN: usize = 1 << 20;
    const WINDOW: u64 = 0x10_0000_0000;
    const WINDOW_LEN: u64 = 1 << 30;
    const GRANULE: u64 = 16384;
    const VENUS: Capset = Capset {
        id: 4,
        max_version: 0,
        max_size: 156,
    };

    /// What the fake renderer and window were asked, and the fences the test signals.
    #[derive(Default)]
    struct Log {
        calls: Vec<String>,
        signals: Vec<Signal>,
        worker: Option<Thread>,
    }

    impl Log {
        fn signal(log: &Mutex<Log>, ctx: u32, ring: u8, fence: u64) {
            let mut l = log.lock().unwrap();
            l.signals.push(Signal { ctx, ring, fence });
            if let Some(w) = &l.worker {
                w.unpark();
            }
        }
    }

    struct Fake {
        log: Arc<Mutex<Log>>,
        /// Host memory its blobs' maps are of: whole granules, as a renderer's are.
        memory: Vec<u8>,
        sizes: HashMap<u32, u64>,
    }

    impl Fake {
        fn note(&self, call: String) {
            self.log.lock().unwrap().calls.push(call);
        }
    }

    impl Renderer for Fake {
        fn capsets(&self) -> &[Capset] {
            std::slice::from_ref(&VENUS)
        }
        fn capset(&mut self, id: u32, version: u32) -> Result<Vec<u8>, Refused> {
            self.note(format!("capset {id} {version}"));
            Ok(vec![7; VENUS.max_size as usize])
        }
        fn create_context(&mut self, ctx: u32, capset: u32, name: &[u8]) -> Result<(), Refused> {
            self.note(format!("create {ctx} {capset} {}", String::from_utf8_lossy(name)));
            Ok(())
        }
        fn destroy_context(&mut self, ctx: u32) {
            self.note(format!("destroy {ctx}"));
        }
        fn attach(&mut self, ctx: u32, resource: u32) {
            self.note(format!("attach {ctx} {resource}"));
        }
        fn detach(&mut self, ctx: u32, resource: u32) {
            self.note(format!("detach {ctx} {resource}"));
        }
        fn create_blob(&mut self, ctx: u32, blob: &Blob, guest: &[(*mut u8, usize)]) -> Result<(), Refused> {
            let at: Vec<String> = guest
                .iter()
                .map(|(p, l)| format!("{:#x}+{l}", *p as usize))
                .collect();
            self.note(format!(
                "blob {ctx} {} {} [{}]",
                blob.resource,
                blob.size,
                at.join(" ")
            ));
            self.sizes.insert(blob.resource, blob.size);
            Ok(())
        }
        fn unref(&mut self, resource: u32) {
            self.note(format!("unref {resource}"));
        }
        fn submit(&mut self, ctx: u32, commands: &mut [u8]) -> Result<(), Refused> {
            self.note(format!("submit {ctx} {}", commands.len()));
            Ok(())
        }
        fn fence(&mut self, ctx: u32, ring: u8, fence: u64) -> Result<(), Refused> {
            self.note(format!("fence {ctx} {ring} {fence}"));
            Ok(())
        }
        fn signals(&mut self, into: &mut Vec<Signal>) {
            into.append(&mut self.log.lock().unwrap().signals);
        }
        fn wake(&mut self, worker: Thread) {
            self.log.lock().unwrap().worker = Some(worker);
        }
        fn map(&mut self, resource: u32) -> Result<HostMemory, Refused> {
            let size = *self
                .sizes
                .get(&resource)
                .ok_or(Refused(resp::ERR_INVALID_RESOURCE_ID))?;
            Ok(HostMemory {
                host: self.memory.as_mut_ptr(),
                len: usize::try_from(size.next_multiple_of(GRANULE)).unwrap(),
                cache: 1,
            })
        }
        fn unmap(&mut self, resource: u32) {
            self.note(format!("unmap {resource}"));
        }
        fn reset(&mut self) {
            self.note("reset".into());
        }
    }

    struct Pane {
        log: Arc<Mutex<Log>>,
    }

    impl Window for Pane {
        unsafe fn map(&self, _host: *mut u8, gpa: u64, len: usize) -> Result<(), String> {
            self.log
                .lock()
                .unwrap()
                .calls
                .push(format!("window map {gpa:#x}+{len:#x}"));
            Ok(())
        }
        fn unmap(&self, gpa: u64, len: usize) -> Result<(), String> {
            self.log
                .lock()
                .unwrap()
                .calls
                .push(format!("window unmap {gpa:#x}+{len:#x}"));
            Ok(())
        }
    }

    fn served() -> (Box<Served>, Arc<Mutex<Log>>, GuestMemory) {
        let log = Arc::new(Mutex::new(Log::default()));
        let fake = Fake {
            log: log.clone(),
            memory: vec![0; 4 << 20],
            sizes: HashMap::new(),
        };
        let region = ShmRegion {
            id: HOST_VISIBLE,
            base: WINDOW,
            len: WINDOW_LEN,
        };
        let served = Box::new(Served::new(
            Box::new(fake),
            Box::new(Pane { log: log.clone() }),
            region,
            GRANULE,
            RAM_LEN,
        ));
        (served, log, GuestMemory::anonymous(&[(RAM, RAM_LEN)]).unwrap())
    }

    fn calls(log: &Mutex<Log>) -> Vec<String> {
        std::mem::take(&mut log.lock().unwrap().calls)
    }

    /// A request of `kind` with `fields`, fenced as `flags` ask, in a chain whose answer
    /// buffer follows: the chain, and where its answer is.
    fn request(mem: &GuestMemory, head: u16, header: Header, fields: &[u8]) -> (Chain, u64) {
        let at = RAM + u64::from(head) * 0x2000;
        let mut b = header_bytes(&header).to_vec();
        b.extend_from_slice(fields);
        mem.access().unwrap().write(at, &b).unwrap();
        let chain = Chain {
            head,
            descriptors: vec![
                Descriptor {
                    addr: at,
                    len: u32::try_from(b.len()).unwrap(),
                    writable: false,
                },
                Descriptor {
                    addr: at + 0x1000,
                    len: 0x1000,
                    writable: true,
                },
            ],
        };
        (chain, at + 0x1000)
    }

    fn header_bytes(h: &Header) -> [u8; HEADER] {
        let mut b = [0u8; HEADER];
        protocol::put(&mut b, 0, &h.kind.to_le_bytes());
        protocol::put(&mut b, 4, &h.flags.to_le_bytes());
        protocol::put(&mut b, 8, &h.fence_id.to_le_bytes());
        protocol::put(&mut b, 16, &h.ctx_id.to_le_bytes());
        protocol::put(&mut b, 20, &[h.ring_idx]);
        b
    }

    /// The answer's kind, and its bytes after the header.
    fn answer(mem: &GuestMemory, at: u64, len: usize) -> (u32, Vec<u8>) {
        let mut b = vec![0u8; len];
        mem.access().unwrap().read(at, &mut b).unwrap();
        (
            u32::from_le_bytes(b[..4].try_into().unwrap()),
            b[HEADER..].to_vec(),
        )
    }

    fn plain(kind: u32, ctx: u32) -> Header {
        Header {
            kind,
            ctx_id: ctx,
            ..Header::default()
        }
    }

    fn le(values: &[u32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    /// Serves one request: what it used, and its answer's kind and fields.
    fn ask(
        s: &mut Served,
        mem: &GuestMemory,
        head: u16,
        h: Header,
        fields: &[u8],
    ) -> (Vec<(u16, u32)>, u32, Vec<u8>) {
        let (chain, at) = request(mem, head, h, fields);
        let used = s.serve(&chain, mem).unwrap();
        let len = used.iter().find(|u| u.0 == head).map_or(HEADER, |u| u.1 as usize);
        let (kind, rest) = answer(mem, at, len.max(HEADER));
        (used, kind, rest)
    }

    fn context(s: &mut Served, mem: &GuestMemory, ctx: u32, capset: u32) -> u32 {
        let mut fields = le(&[5, capset]);
        fields.extend(b"probe");
        fields.resize(8 + 64, 0);
        ask(s, mem, 0, plain(cmd::CTX_CREATE, ctx), &fields).1
    }

    fn blob(s: &mut Served, mem: &GuestMemory, ctx: u32, resource: u32, size: u64) -> u32 {
        let mut fields = le(&[resource, 2, 1, 0]);
        fields.extend(9u64.to_le_bytes());
        fields.extend(size.to_le_bytes());
        ask(s, mem, 1, plain(cmd::RESOURCE_CREATE_BLOB, ctx), &fields).1
    }

    fn map(s: &mut Served, mem: &GuestMemory, resource: u32, offset: u64) -> (u32, Vec<u8>) {
        let mut fields = le(&[resource, 0]);
        fields.extend(offset.to_le_bytes());
        let (_, kind, rest) = ask(s, mem, 2, plain(cmd::RESOURCE_MAP_BLOB, 1), &fields);
        (kind, rest)
    }

    /// The renderer's capability sets are offered by index, each asked for by its id and a
    /// version it has.
    #[test]
    fn capsets_are_offered_by_index() {
        let (mut s, _log, mem) = served();
        let (used, kind, info) = ask(&mut s, &mem, 3, plain(cmd::GET_CAPSET_INFO, 0), &le(&[0]));
        assert_eq!((used.len(), kind), (1, resp::OK_CAPSET_INFO));
        assert_eq!(&info[..16], le(&[4, 0, 156, 0]).as_slice());
        assert_eq!(
            ask(&mut s, &mem, 3, plain(cmd::GET_CAPSET_INFO, 0), &le(&[1])).1,
            resp::ERR_INVALID_PARAMETER
        );
        let (_, kind, caps) = ask(&mut s, &mem, 3, plain(cmd::GET_CAPSET, 0), &le(&[4, 0]));
        assert_eq!(kind, resp::OK_CAPSET);
        assert_eq!(&caps[..156], [7u8; 156].as_slice());
        for asked in [[5, 0], [4, 1]] {
            assert_eq!(
                ask(&mut s, &mem, 3, plain(cmd::GET_CAPSET, 0), &le(&asked)).1,
                resp::ERR_INVALID_PARAMETER
            );
        }
    }

    /// A context is of a capability set offered, under an id not in use; one destroyed is
    /// gone, and so is what waited for its fences.
    #[test]
    fn contexts_are_of_the_capsets_offered() {
        let (mut s, log, mem) = served();
        assert_eq!(context(&mut s, &mem, 1, 4), resp::OK_NODATA);
        assert_eq!(calls(&log), ["create 1 4 probe"]);
        assert_eq!(context(&mut s, &mem, 1, 4), resp::ERR_INVALID_CONTEXT_ID);
        assert_eq!(context(&mut s, &mem, 0, 4), resp::ERR_INVALID_CONTEXT_ID);
        assert_eq!(context(&mut s, &mem, 2, 9), resp::ERR_INVALID_PARAMETER);
        assert_eq!(calls(&log), Vec::<String>::new());
        let fenced = Header {
            kind: cmd::SUBMIT_3D,
            flags: FLAG_FENCE | FLAG_INFO_RING_IDX,
            fence_id: 3,
            ctx_id: 1,
            ring_idx: 1,
        };
        let (used, ..) = ask(&mut s, &mem, 4, fenced, &le(&[0, 0]));
        assert_eq!(used, []);
        let (used, kind, _) = ask(&mut s, &mem, 5, plain(cmd::CTX_DESTROY, 1), &[]);
        assert_eq!(kind, resp::OK_NODATA);
        assert_eq!(used, [(4, HEADER as u32), (5, HEADER as u32)]);
        assert_eq!(
            ask(&mut s, &mem, 5, plain(cmd::CTX_DESTROY, 1), &[]).1,
            resp::ERR_INVALID_CONTEXT_ID
        );
    }

    /// A blob's guest memory is the guest's RAM, given to the renderer at its host address;
    /// an entry past it is refused.
    #[test]
    fn a_blobs_entries_are_guest_ram() {
        let (mut s, log, mem) = served();
        context(&mut s, &mem, 1, 4);
        calls(&log);
        let entry = |addr: u64, len: u32| {
            let mut fields = le(&[3, 1, 1, 1]);
            fields.extend(0u64.to_le_bytes());
            fields.extend(4096u64.to_le_bytes());
            fields.extend(addr.to_le_bytes());
            fields.extend(le(&[len, 0]));
            fields
        };
        let outside = entry(RAM + RAM_LEN as u64 - 2048, 4096);
        assert_eq!(
            ask(&mut s, &mem, 1, plain(cmd::RESOURCE_CREATE_BLOB, 1), &outside).1,
            resp::ERR_INVALID_PARAMETER
        );
        let inside = entry(RAM + 0x40000, 4096);
        assert_eq!(
            ask(&mut s, &mem, 1, plain(cmd::RESOURCE_CREATE_BLOB, 1), &inside).1,
            resp::OK_NODATA
        );
        let host = mem.host_ptr(RAM + 0x40000, 4096).unwrap() as usize;
        assert_eq!(calls(&log), [format!("blob 1 3 4096 [{host:#x}+4096]")]);
        assert_eq!(
            ask(&mut s, &mem, 1, plain(cmd::RESOURCE_CREATE_BLOB, 1), &inside).1,
            resp::ERR_INVALID_RESOURCE_ID
        );
    }

    /// A blob maps into the window at a place of whole host pages, over nothing mapped,
    /// within the window; it leaves as it is unmapped or unreferenced, and with a reset.
    #[test]
    fn blobs_map_where_the_window_is_free() {
        let (mut s, log, mem) = served();
        context(&mut s, &mem, 1, 4);
        assert_eq!(blob(&mut s, &mem, 1, 5, 20000), resp::OK_NODATA);
        assert_eq!(blob(&mut s, &mem, 1, 6, 16384), resp::OK_NODATA);
        calls(&log);
        assert_eq!(map(&mut s, &mem, 5, 0x2000).0, resp::ERR_INVALID_PARAMETER);
        assert_eq!(calls(&log), Vec::<String>::new());
        let (kind, info) = map(&mut s, &mem, 5, 0);
        assert_eq!((kind, &info[..8]), (resp::OK_MAP_INFO, le(&[1, 0]).as_slice()));
        assert_eq!(calls(&log), [format!("window map {WINDOW:#x}+0x8000")]);
        assert_eq!(map(&mut s, &mem, 5, 0x10000).0, resp::ERR_INVALID_PARAMETER);
        // Over resource 5's second page, or past the window: refused, and let go of.
        assert_eq!(map(&mut s, &mem, 6, 0x4000).0, resp::ERR_INVALID_PARAMETER);
        assert_eq!(map(&mut s, &mem, 6, WINDOW_LEN).0, resp::ERR_INVALID_PARAMETER);
        assert_eq!(calls(&log), ["unmap 6", "unmap 6"]);
        assert_eq!(map(&mut s, &mem, 6, 0x8000).0, resp::OK_MAP_INFO);
        assert_eq!(map(&mut s, &mem, 9, 0x10000).0, resp::ERR_INVALID_RESOURCE_ID);
        calls(&log);
        let (_, kind, _) = ask(&mut s, &mem, 3, plain(cmd::RESOURCE_UNMAP_BLOB, 1), &le(&[5, 0]));
        assert_eq!(kind, resp::OK_NODATA);
        assert_eq!(
            calls(&log),
            [format!("window unmap {WINDOW:#x}+0x8000"), "unmap 5".into()]
        );
        let (_, kind, _) = ask(&mut s, &mem, 3, plain(cmd::RESOURCE_UNREF, 1), &le(&[6, 0]));
        assert_eq!(kind, resp::OK_NODATA);
        assert_eq!(
            calls(&log),
            [
                format!("window unmap {:#x}+0x4000", WINDOW + 0x8000),
                "unmap 6".into(),
                "unref 6".into()
            ]
        );
        assert_eq!(map(&mut s, &mem, 5, 0x4000).0, resp::OK_MAP_INFO);
        calls(&log);
        s.reset();
        assert_eq!(
            calls(&log),
            [
                format!("window unmap {:#x}+0x8000", WINDOW + 0x4000),
                "reset".into()
            ]
        );
        assert_eq!(map(&mut s, &mem, 5, 0).0, resp::ERR_INVALID_RESOURCE_ID);
    }

    /// A fenced request is answered once its fence is signalled, after those before it on
    /// its ring, which a later fence's signal stands for; one refused waits only for those
    /// before it; a fence of no ring, and one of another ring, wait for nothing of these.
    #[test]
    fn fenced_answers_wait_their_turn() {
        let (mut s, log, mem) = served();
        context(&mut s, &mem, 1, 4);
        calls(&log);
        let fenced = |fence: u64, ring: u8| Header {
            kind: cmd::SUBMIT_3D,
            flags: FLAG_FENCE | FLAG_INFO_RING_IDX,
            fence_id: fence,
            ctx_id: 1,
            ring_idx: ring,
        };
        let n = HEADER as u32;
        assert_eq!(ask(&mut s, &mem, 10, fenced(10, 1), &le(&[4, 0, 0])).0, []);
        assert_eq!(ask(&mut s, &mem, 11, fenced(11, 1), &le(&[4, 0, 0])).0, []);
        // Refused (a length of no whole words), answered at its turn.
        let (used, kind, _) = ask(&mut s, &mem, 12, fenced(12, 1), &le(&[3, 0, 0]));
        assert_eq!((used, kind), (vec![], resp::ERR_INVALID_PARAMETER));
        assert_eq!(ask(&mut s, &mem, 13, fenced(13, 2), &le(&[4, 0, 0])).0, []);
        let device = Header {
            flags: FLAG_FENCE,
            ring_idx: 0,
            ..fenced(14, 0)
        };
        assert_eq!(ask(&mut s, &mem, 14, device, &le(&[4, 0, 0])).0, [(14, n)]);
        assert_eq!(
            calls(&log),
            [
                "submit 1 4",
                "fence 1 1 10",
                "submit 1 4",
                "fence 1 1 11",
                "submit 1 4",
                "fence 1 2 13",
                "submit 1 4"
            ]
        );
        // Made available again while held: the ring is malformed.
        let (again, _) = request(&mem, 11, fenced(15, 1), &le(&[4, 0, 0]));
        assert_eq!(s.serve(&again, &mem), Err(QueueError::InUse(11)));
        assert_eq!(
            s.signalled(Signal {
                ctx: 1,
                ring: 1,
                fence: 10
            }),
            [(10, n)]
        );
        assert_eq!(
            s.signalled(Signal {
                ctx: 1,
                ring: 2,
                fence: 99
            }),
            [(13, n)]
        );
        assert_eq!(
            s.signalled(Signal {
                ctx: 2,
                ring: 1,
                fence: 11
            }),
            []
        );
        assert_eq!(
            s.signalled(Signal {
                ctx: 1,
                ring: 1,
                fence: 11
            }),
            [(11, n), (12, n)]
        );
    }

    /// A queue in guest memory, its driver's side.
    struct Ring {
        size: u16,
        desc: u64,
        avail: u64,
        used: u64,
        next: u16,
    }

    impl Ring {
        fn new(at: u64, size: u16) -> Ring {
            Ring {
                size,
                desc: at,
                avail: at + 0x1000,
                used: at + 0x2000,
                next: 0,
            }
        }
        fn queue(&self, mem: &GuestMemory) -> Queue {
            let cfg = QueueConfig {
                size: self.size,
                desc: self.desc,
                avail: self.avail,
                used: self.used,
                ready: true,
            };
            Queue::new(cfg, self.size, mem, virtio::VERSION_1).unwrap()
        }
        /// Publishes `chain` as the next request.
        fn publish(&mut self, mem: &GuestMemory, chain: &Chain) {
            let a = mem.access().unwrap();
            let n = chain.descriptors.len();
            for (i, d) in chain.descriptors.iter().enumerate() {
                let index = chain.head + i as u16;
                let next = if i + 1 < n { 1u16 } else { 0 };
                let mut raw = d.addr.to_le_bytes().to_vec();
                raw.extend(d.len.to_le_bytes());
                raw.extend((next | if d.writable { 2 } else { 0 }).to_le_bytes());
                raw.extend((index + 1).to_le_bytes());
                a.write(self.desc + 16 * u64::from(index), &raw).unwrap();
            }
            let slot = u64::from(self.next % self.size);
            a.write(self.avail + 4 + 2 * slot, &chain.head.to_le_bytes())
                .unwrap();
            self.next = self.next.wrapping_add(1);
            a.write(self.avail + 2, &self.next.to_le_bytes()).unwrap();
        }
        fn used_idx(&self, mem: &GuestMemory) -> u16 {
            let mut b = [0u8; 2];
            mem.access().unwrap().read(self.used + 2, &mut b).unwrap();
            u16::from_le_bytes(b)
        }
    }

    struct Line;

    impl crate::devices::Interrupt for Line {
        fn set_level(&self, _: bool) {}
    }

    /// On the device's worker, a fenced request is used once its fence is signalled from
    /// another thread, which wakes the worker; one not fenced is used at once.
    #[test]
    fn the_worker_uses_a_fenced_request_once_signalled() {
        let (served, log, mem) = served();
        let mem = Arc::new(mem);
        let mut gpu = Gpu::new(served);
        let mut control = Ring::new(RAM + 0x80000, 16);
        let cursor = Ring::new(RAM + 0x90000, 16);
        let irq = Arc::new(DeviceInterrupt::new(Arc::new(Line)));
        gpu.activate(Activation {
            memory: mem.clone(),
            queues: vec![control.queue(&mem), cursor.queue(&mem)],
            interrupt: irq,
            features: gpu.features(),
            restored: false,
        })
        .unwrap();
        let until = |want: u16, control: &Ring| {
            let end = Instant::now() + Duration::from_secs(5);
            while control.used_idx(&mem) != want && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(1));
            }
            control.used_idx(&mem)
        };
        let (create, _) = request(&mem, 0, plain(cmd::CTX_CREATE, 1), &{
            let mut f = le(&[0, 4]);
            f.resize(8 + 64, 0);
            f
        });
        control.publish(&mem, &create);
        gpu.notify(0);
        assert_eq!(until(1, &control), 1);
        let fenced = Header {
            kind: cmd::SUBMIT_3D,
            flags: FLAG_FENCE | FLAG_INFO_RING_IDX,
            fence_id: 1,
            ctx_id: 1,
            ring_idx: 1,
        };
        let (submit, _) = request(&mem, 2, fenced, &le(&[4, 0, 0]));
        control.publish(&mem, &submit);
        gpu.notify(0);
        let end = Instant::now() + Duration::from_secs(5);
        while !calls(&log).iter().any(|c| c == "fence 1 1 1") {
            assert!(Instant::now() < end, "the fence was never asked for");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(control.used_idx(&mem), 1, "used before its fence");
        Log::signal(&log, 1, 1, 1);
        assert_eq!(until(2, &control), 2);
        gpu.reset();
    }
}
