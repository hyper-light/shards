//! Split virtqueue, device side (virtio 1.3 §2.7).
//!
//! Everything in the rings is guest-controlled and changes concurrently:
//! - each descriptor is fetched exactly once, into a local copy
//! - every index is checked against the queue size
//! - chains are bounded, so a cyclic chain cannot hang the device
//! - an available index that runs ahead by more than the queue size is an error
//!
//! These rules come from docs/research/rootless-security.md.

use std::fmt;
use std::num::Wrapping;
use std::sync::atomic::{AtomicBool, Ordering, fence};

use super::{DeviceInterrupt, feature};
use crate::memory::{Access, GuestMemory, OutOfBounds, Pod, Reentered};

const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;
const DESC_F_INDIRECT: u16 = 4;
const AVAIL_F_NO_INTERRUPT: u16 = 1;
const USED_F_NO_NOTIFY: u16 = 1;
pub const MAX_QUEUE_SIZE: u16 = 32768;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueError {
    Memory(OutOfBounds),
    /// The thread already held guest memory (a bug here, reported rather than awaited).
    Reentered,
    /// The driver published more new buffers than the queue can hold.
    AvailIndexJump {
        published: u16,
        size: u16,
    },
    DescriptorIndex(u16),
    ChainTooLong,
    NestedIndirect,
    IndirectNotNegotiated,
    IndirectLength(u32),
    ReadableAfterWritable,
    Config(&'static str),
}

impl fmt::Display for QueueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueueError::Memory(e) => write!(f, "{e}"),
            QueueError::Reentered => write!(f, "{Reentered}"),
            QueueError::AvailIndexJump { published, size } => {
                write!(f, "driver published {published} new buffers on a queue of {size}")
            }
            QueueError::DescriptorIndex(i) => write!(f, "descriptor index {i} out of range"),
            QueueError::ChainTooLong => write!(f, "descriptor chain loops or is longer than its queue"),
            QueueError::NestedIndirect => write!(f, "indirect descriptor inside an indirect table"),
            QueueError::IndirectNotNegotiated => {
                write!(f, "indirect descriptor without VIRTIO_F_INDIRECT_DESC")
            }
            QueueError::IndirectLength(l) => {
                write!(f, "indirect table length {l} is not a positive multiple of 16")
            }
            QueueError::ReadableAfterWritable => write!(f, "device-readable descriptor after a writable one"),
            QueueError::Config(what) => write!(f, "invalid queue configuration: {what}"),
        }
    }
}

impl std::error::Error for QueueError {}

impl From<OutOfBounds> for QueueError {
    fn from(e: OutOfBounds) -> Self {
        QueueError::Memory(e)
    }
}

impl From<Reentered> for QueueError {
    fn from(_: Reentered) -> Self {
        QueueError::Reentered
    }
}

/// Runs `f` with an access to `mem` held for it alone: one step of a device's queue work,
/// between the system calls that move its data.
pub fn with<R>(
    mem: &GuestMemory,
    f: impl FnOnce(&Access<'_>) -> Result<R, QueueError>,
) -> Result<R, QueueError> {
    f(&mem.access()?)
}

/// What a round of serving a queue left ([`serve_round`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Round {
    /// The queue is empty and the driver's notifications are re-armed: the driver notifies
    /// for what it adds next.
    Idle,
    /// Requests may remain, with the driver's notifications still off: the device comes
    /// back for them without waiting to be notified, which the driver, told not to, will
    /// not do.
    More,
}

/// Serves one round of `queue`: answers its requests with `answer`, which returns the bytes
/// it wrote into each, a ring's worth at most and none once `stop` is set, then interrupts
/// the driver if it wants to hear of them. A driver refilling the ring from another CPU
/// cannot keep the device from its other work, from stopping, or itself from hearing of
/// completions (audit A09).
pub fn serve_round(
    queue: &mut Queue,
    mem: &GuestMemory,
    irq: &DeviceInterrupt,
    stop: &AtomicBool,
    answer: &mut dyn FnMut(&Chain) -> u32,
) -> Result<Round, QueueError> {
    let budget = usize::from(queue.size);
    let mut served = 0;
    let round = 'round: loop {
        with(mem, |a| queue.disable_notification(a))?;
        loop {
            if served == budget || stop.load(Ordering::Acquire) {
                break 'round Round::More;
            }
            let Some(chain) = with(mem, |a| queue.pop(a))? else {
                break;
            };
            let written = answer(&chain);
            with(mem, |a| queue.add_used(a, chain.head, written))?;
            served += 1;
        }
        if !with(mem, |a| queue.enable_notification(a))? {
            break Round::Idle;
        }
    };
    if served > 0 && with(mem, |a| queue.needs_interrupt(a))? {
        irq.used_buffer();
    }
    Ok(round)
}

/// A queue as the driver programmed it through the transport.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueConfig {
    pub size: u16,
    pub desc: u64,
    pub avail: u64,
    pub used: u64,
    pub ready: bool,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct RawDesc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

// SAFETY: repr(C), 8 + 4 + 2 + 2 bytes with no padding; every bit pattern is valid.
unsafe impl Pod for RawDesc {}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct UsedElem {
    id: u32,
    len: u32,
}

// SAFETY: repr(C), two u32 with no padding.
unsafe impl Pod for UsedElem {}

/// One guest buffer of a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Descriptor {
    pub addr: u64,
    pub len: u32,
    pub writable: bool,
}

/// A request popped from the available ring: device-readable buffers first, then
/// device-writable ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chain {
    pub head: u16,
    pub descriptors: Vec<Descriptor>,
}

impl Chain {
    pub fn readable(&self) -> impl Iterator<Item = &Descriptor> {
        self.descriptors.iter().filter(|d| !d.writable)
    }

    pub fn writable(&self) -> impl Iterator<Item = &Descriptor> {
        self.descriptors.iter().filter(|d| d.writable)
    }
}

/// `(new - event - 1) < (new - old)` in u16 arithmetic (virtio 1.3 §2.7.10).
pub fn need_event(event: u16, new: u16, old: u16) -> bool {
    new.wrapping_sub(event).wrapping_sub(1) < new.wrapping_sub(old)
}

/// Accumulates a chain while enforcing the length bound and readable-then-writable
/// ordering (virtio 1.3 §2.7.4). A chain, its indirect descriptors included, is no longer
/// than its queue: a driver must not make one longer (§2.7.5.3.1), and the devices
/// advertise no more segments than fit (block's `seg_max` is its queue's size less two).
struct ChainBuilder {
    descriptors: Vec<Descriptor>,
    seen_writable: bool,
    limit: usize,
}

impl ChainBuilder {
    fn new(limit: u16) -> ChainBuilder {
        ChainBuilder {
            descriptors: Vec::new(),
            seen_writable: false,
            limit: usize::from(limit),
        }
    }

    /// Room left in the chain.
    fn room(&self) -> usize {
        self.limit.saturating_sub(self.descriptors.len())
    }

    fn push(&mut self, d: &RawDesc) -> Result<(), QueueError> {
        if self.descriptors.len() >= self.limit {
            return Err(QueueError::ChainTooLong);
        }
        let writable = d.flags & DESC_F_WRITE != 0;
        if self.seen_writable && !writable {
            return Err(QueueError::ReadableAfterWritable);
        }
        self.seen_writable |= writable;
        self.descriptors.push(Descriptor {
            addr: d.addr,
            len: d.len,
            writable,
        });
        Ok(())
    }

    fn finish(self, head: u16) -> Chain {
        Chain {
            head,
            descriptors: self.descriptors,
        }
    }
}

fn walk_indirect(mem: &Access<'_>, table: &RawDesc, chain: &mut ChainBuilder) -> Result<(), QueueError> {
    // A table longer than the chain has room for is refused before it is read.
    if table.len == 0 || !table.len.is_multiple_of(16) || table.len as usize / 16 > chain.room() {
        return Err(QueueError::IndirectLength(table.len));
    }
    let count = (table.len / 16) as u16;
    mem.memory().host_ptr(table.addr, table.len as usize)?;
    let mut index = 0u16;
    for _ in 0..count {
        if index >= count {
            return Err(QueueError::DescriptorIndex(index));
        }
        let d: RawDesc = mem.read_obj(table.addr + 16 * u64::from(index))?;
        if d.flags & DESC_F_INDIRECT != 0 {
            return Err(QueueError::NestedIndirect);
        }
        chain.push(&d)?;
        if d.flags & DESC_F_NEXT == 0 {
            return Ok(());
        }
        index = d.next;
    }
    Err(QueueError::ChainTooLong)
}

/// How far the device has got through a queue: what a snapshot records beyond the
/// driver's configuration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueState {
    pub next_avail: u16,
    pub next_used: u16,
    pub signalled_used: Option<u16>,
}

#[derive(Debug)]
pub struct Queue {
    size: u16,
    desc: u64,
    avail: u64,
    used: u64,
    next_avail: Wrapping<u16>,
    next_used: Wrapping<u16>,
    /// Used index when the driver was last interrupted (None: never).
    signalled_used: Option<Wrapping<u16>>,
    event_idx: bool,
    indirect: bool,
}

impl Queue {
    /// Validates the driver's configuration against `max_size` and guest memory.
    pub fn new(
        cfg: QueueConfig,
        max_size: u16,
        mem: &GuestMemory,
        features: u64,
    ) -> Result<Queue, QueueError> {
        let size = cfg.size;
        if size == 0 || size > max_size || size > MAX_QUEUE_SIZE || !size.is_power_of_two() {
            return Err(QueueError::Config(
                "size must be a power of two within the maximum",
            ));
        }
        if !cfg.desc.is_multiple_of(16) || !cfg.avail.is_multiple_of(2) || !cfg.used.is_multiple_of(4) {
            return Err(QueueError::Config("ring alignment (desc 16, avail 2, used 4)"));
        }
        let n = u64::from(size);
        // The trailing event fields exist only with EVENT_IDX, but reserving them always
        // keeps the check simple and the layout unambiguous.
        mem.host_ptr(cfg.desc, (16 * n) as usize)?;
        mem.host_ptr(cfg.avail, (6 + 2 * n) as usize)?;
        mem.host_ptr(cfg.used, (6 + 8 * n) as usize)?;
        Ok(Queue {
            size,
            desc: cfg.desc,
            avail: cfg.avail,
            used: cfg.used,
            next_avail: Wrapping(0),
            next_used: Wrapping(0),
            signalled_used: None,
            event_idx: features & feature::EVENT_IDX != 0,
            indirect: features & feature::INDIRECT_DESC != 0,
        })
    }

    pub fn size(&self) -> u16 {
        self.size
    }

    pub fn state(&self) -> QueueState {
        QueueState {
            next_avail: self.next_avail.0,
            next_used: self.next_used.0,
            signalled_used: self.signalled_used.map(|w| w.0),
        }
    }

    /// Continues from where a snapshot of this queue left off.
    pub fn set_state(&mut self, st: QueueState) {
        self.next_avail = Wrapping(st.next_avail);
        self.next_used = Wrapping(st.next_used);
        self.signalled_used = st.signalled_used.map(Wrapping);
    }

    fn slot(&self, index: Wrapping<u16>) -> u64 {
        u64::from(index.0 & (self.size - 1))
    }

    fn avail_idx(&self, mem: &Access<'_>) -> Result<Wrapping<u16>, QueueError> {
        Ok(Wrapping(mem.load_u16(self.avail + 2, Ordering::Acquire)?))
    }

    /// Takes the next available request, or `None` if the ring is empty.
    pub fn pop(&mut self, mem: &Access<'_>) -> Result<Option<Chain>, QueueError> {
        let published = (self.avail_idx(mem)? - self.next_avail).0;
        if published == 0 {
            return Ok(None);
        }
        if published > self.size {
            return Err(QueueError::AvailIndexJump {
                published,
                size: self.size,
            });
        }
        let head: u16 = mem.read_obj(self.avail + 4 + 2 * self.slot(self.next_avail))?;
        self.next_avail += 1;
        self.walk(mem, head).map(Some)
    }

    fn walk(&self, mem: &Access<'_>, head: u16) -> Result<Chain, QueueError> {
        let mut chain = ChainBuilder::new(self.size);
        let mut index = head;
        // A direct chain visits each table entry at most once.
        for _ in 0..self.size {
            if index >= self.size {
                return Err(QueueError::DescriptorIndex(index));
            }
            let d: RawDesc = mem.read_obj(self.desc + 16 * u64::from(index))?;
            if d.flags & DESC_F_INDIRECT != 0 {
                if !self.indirect {
                    return Err(QueueError::IndirectNotNegotiated);
                }
                // An indirect descriptor ends the chain (INDIRECT with NEXT is invalid).
                walk_indirect(mem, &d, &mut chain)?;
                return Ok(chain.finish(head));
            }
            chain.push(&d)?;
            if d.flags & DESC_F_NEXT == 0 {
                return Ok(chain.finish(head));
            }
            index = d.next;
        }
        Err(QueueError::ChainTooLong)
    }

    /// Returns a request to the driver with `len` bytes written into its buffers.
    pub fn add_used(&mut self, mem: &Access<'_>, head: u16, len: u32) -> Result<(), QueueError> {
        let elem = UsedElem {
            id: u32::from(head),
            len,
        };
        mem.write_obj(self.used + 4 + 8 * self.slot(self.next_used), elem)?;
        self.next_used += 1;
        mem.store_u16(self.used + 2, self.next_used.0, Ordering::Release)?;
        Ok(())
    }

    /// Whether the driver wants an interrupt for the used buffers added since the last
    /// one (EVENT_IDX `used_event`, or the NO_INTERRUPT flag without it).
    pub fn needs_interrupt(&mut self, mem: &Access<'_>) -> Result<bool, QueueError> {
        // The used index must be visible before the driver's event field is read.
        fence(Ordering::SeqCst);
        let new = self.next_used;
        let old = self.signalled_used.replace(new);
        if !self.event_idx {
            let flags: u16 = mem.read_obj(self.avail)?;
            return Ok(flags & AVAIL_F_NO_INTERRUPT == 0);
        }
        let Some(old) = old else {
            return Ok(true);
        };
        let used_event: u16 = mem.read_obj(self.avail + 4 + 2 * u64::from(self.size))?;
        Ok(need_event(used_event, new.0, old.0))
    }

    /// Asks the driver not to notify while the device is draining the queue.
    pub fn disable_notification(&mut self, mem: &Access<'_>) -> Result<(), QueueError> {
        if !self.event_idx {
            mem.write_obj(self.used, USED_F_NO_NOTIFY)?;
        }
        Ok(())
    }

    /// Re-arms driver notifications. Returns true if buffers arrived meanwhile, in
    /// which case the caller must keep draining (the driver may not notify for them).
    pub fn enable_notification(&mut self, mem: &Access<'_>) -> Result<bool, QueueError> {
        if self.event_idx {
            mem.write_obj(self.used + 4 + 8 * u64::from(self.size), self.next_avail.0)?;
        } else {
            mem.write_obj(self.used, 0u16)?;
        }
        // The re-arm must be visible before the available index is re-read.
        fence(Ordering::SeqCst);
        Ok(self.avail_idx(mem)? != self.next_avail)
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;

    const BASE: u64 = 0x8000_0000;
    const DESC: u64 = BASE;
    const AVAIL: u64 = BASE + 0x1000;
    const USED: u64 = BASE + 0x2000;
    const DATA: u64 = BASE + 0x4000;

    /// A minimal, deliberately hostile-capable driver.
    struct Driver {
        mem: std::sync::Arc<GuestMemory>,
        size: u16,
        avail_idx: u16,
    }

    impl Driver {
        fn new(size: u16) -> Driver {
            let p = crate::platform::page_size().unwrap();
            let mem = std::sync::Arc::new(GuestMemory::anonymous(&[(BASE, 8 * p)]).unwrap());
            Driver {
                mem,
                size,
                avail_idx: 0,
            }
        }
        fn queue(&self, features: u64) -> Queue {
            let cfg = QueueConfig {
                size: self.size,
                desc: DESC,
                avail: AVAIL,
                used: USED,
                ready: true,
            };
            Queue::new(cfg, self.size, &self.mem, features).unwrap()
        }
        fn set_desc(&self, i: u16, addr: u64, len: u32, flags: u16, next: u16) {
            let d = RawDesc {
                addr,
                len,
                flags,
                next,
            };
            self.mem
                .access()
                .unwrap()
                .write_obj(DESC + 16 * u64::from(i), d)
                .unwrap();
        }
        fn publish(&mut self, head: u16) {
            let slot = u64::from(self.avail_idx % self.size);
            self.mem
                .access()
                .unwrap()
                .write_obj(AVAIL + 4 + 2 * slot, head)
                .unwrap();
            self.avail_idx = self.avail_idx.wrapping_add(1);
            self.mem
                .access()
                .unwrap()
                .write_obj(AVAIL + 2, self.avail_idx)
                .unwrap();
        }
    }

    #[test]
    fn pops_chains_and_returns_used() {
        let mut d = Driver::new(8);
        let mut q = d.queue(feature::VERSION_1);
        d.set_desc(0, DATA, 16, DESC_F_NEXT, 1);
        d.set_desc(1, DATA + 16, 512, DESC_F_NEXT | DESC_F_WRITE, 2);
        d.set_desc(2, DATA + 528, 1, DESC_F_WRITE, 0);
        d.publish(0);
        let c = q.pop(&d.mem.access().unwrap()).unwrap().unwrap();
        assert_eq!(c.head, 0);
        assert_eq!(c.readable().count(), 1);
        assert_eq!(c.writable().map(|x| x.len).collect::<Vec<_>>(), vec![512, 1]);
        assert!(q.pop(&d.mem.access().unwrap()).unwrap().is_none());
        q.add_used(&d.mem.access().unwrap(), c.head, 513).unwrap();
        assert_eq!(d.mem.access().unwrap().read_obj::<u16>(USED + 2).unwrap(), 1);
        assert_eq!(d.mem.access().unwrap().read_obj::<u32>(USED + 4).unwrap(), 0);
        assert_eq!(d.mem.access().unwrap().read_obj::<u32>(USED + 8).unwrap(), 513);
    }

    #[test]
    fn rejects_hostile_rings() {
        let mut d = Driver::new(4);
        let mut q = d.queue(feature::VERSION_1);
        d.set_desc(0, DATA, 8, DESC_F_NEXT, 1);
        d.set_desc(1, DATA, 8, DESC_F_NEXT, 0); // cycle 0 -> 1 -> 0
        d.publish(0);
        assert_eq!(q.pop(&d.mem.access().unwrap()), Err(QueueError::ChainTooLong));

        d.set_desc(2, DATA, 8, DESC_F_NEXT, 9); // next out of range
        d.publish(2);
        assert_eq!(
            q.pop(&d.mem.access().unwrap()),
            Err(QueueError::DescriptorIndex(9))
        );

        d.publish(7); // head out of range
        assert_eq!(
            q.pop(&d.mem.access().unwrap()),
            Err(QueueError::DescriptorIndex(7))
        );

        d.set_desc(3, DATA, 8, DESC_F_WRITE | DESC_F_NEXT, 0);
        d.set_desc(0, DATA, 8, 0, 0);
        d.publish(3); // writable then readable
        assert_eq!(
            q.pop(&d.mem.access().unwrap()),
            Err(QueueError::ReadableAfterWritable)
        );

        d.set_desc(0, DATA, 16, DESC_F_INDIRECT, 0);
        d.publish(0); // indirect not negotiated
        assert_eq!(
            q.pop(&d.mem.access().unwrap()),
            Err(QueueError::IndirectNotNegotiated)
        );

        // The driver claims far more new buffers than the ring holds.
        d.mem
            .access()
            .unwrap()
            .write_obj(AVAIL + 2, d.avail_idx.wrapping_add(100))
            .unwrap();
        assert!(matches!(
            q.pop(&d.mem.access().unwrap()),
            Err(QueueError::AvailIndexJump { .. })
        ));
    }

    /// A chain is no longer than its queue, indirect descriptors included (virtio 1.3
    /// §2.7.5.3.1): one as long is taken; one longer, direct and indirect together, is
    /// refused, an indirect table longer than the room left before any of it is read
    /// (audit D09).
    #[test]
    fn chains_are_no_longer_than_their_queue() {
        let mut d = Driver::new(8);
        let mut q = d.queue(feature::VERSION_1 | feature::INDIRECT_DESC);
        let table = DATA + 0x800;
        let write_table = |d: &Driver, entries: u16| {
            for i in 0..entries {
                let last = i + 1 == entries;
                let raw = RawDesc {
                    addr: DATA + 64 * u64::from(i),
                    len: 1,
                    flags: if last { 0 } else { DESC_F_NEXT },
                    next: i + 1,
                };
                d.mem
                    .access()
                    .unwrap()
                    .write_obj(table + 16 * u64::from(i), raw)
                    .unwrap();
            }
        };
        // Two direct descriptors, then a table of six: eight, the queue's size.
        write_table(&d, 6);
        d.set_desc(0, DATA, 1, DESC_F_NEXT, 1);
        d.set_desc(1, DATA + 1, 1, DESC_F_NEXT, 2);
        d.set_desc(2, table, 6 * 16, DESC_F_INDIRECT, 0);
        d.publish(0);
        assert_eq!(
            q.pop(&d.mem.access().unwrap())
                .unwrap()
                .unwrap()
                .descriptors
                .len(),
            8
        );
        // The same with a table of seven: nine.
        d.set_desc(2, table, 7 * 16, DESC_F_INDIRECT, 0);
        d.publish(0);
        assert_eq!(
            q.pop(&d.mem.access().unwrap()),
            Err(QueueError::IndirectLength(7 * 16))
        );
        // A table alone longer than the queue, its entries never written: refused unread.
        d.set_desc(3, DATA + 0x10_0000, 9 * 16, DESC_F_INDIRECT, 0);
        d.publish(3);
        assert_eq!(
            q.pop(&d.mem.access().unwrap()),
            Err(QueueError::IndirectLength(9 * 16))
        );
    }

    #[test]
    fn follows_indirect_tables_and_rejects_nesting() {
        let mut d = Driver::new(4);
        let mut q = d.queue(feature::VERSION_1 | feature::INDIRECT_DESC);
        let table = DATA + 0x800;
        let raw = |addr, len, flags, next| RawDesc {
            addr,
            len,
            flags,
            next,
        };
        d.mem
            .access()
            .unwrap()
            .write_obj(table, raw(DATA, 16, DESC_F_NEXT, 1))
            .unwrap();
        d.mem
            .access()
            .unwrap()
            .write_obj(table + 16, raw(DATA + 16, 4096, DESC_F_NEXT | DESC_F_WRITE, 2))
            .unwrap();
        d.mem
            .access()
            .unwrap()
            .write_obj(table + 32, raw(DATA + 4112, 1, DESC_F_WRITE, 0))
            .unwrap();
        d.set_desc(0, table, 48, DESC_F_INDIRECT, 0);
        d.publish(0);
        let c = q.pop(&d.mem.access().unwrap()).unwrap().unwrap();
        assert_eq!(c.descriptors.len(), 3);
        assert_eq!(c.writable().count(), 2);

        d.mem
            .access()
            .unwrap()
            .write_obj(table + 16, raw(table, 48, DESC_F_INDIRECT, 0))
            .unwrap();
        d.set_desc(1, table, 48, DESC_F_INDIRECT, 0);
        d.publish(1);
        assert_eq!(q.pop(&d.mem.access().unwrap()), Err(QueueError::NestedIndirect));

        d.set_desc(2, table, 20, DESC_F_INDIRECT, 0);
        d.publish(2);
        assert_eq!(
            q.pop(&d.mem.access().unwrap()),
            Err(QueueError::IndirectLength(20))
        );
    }

    #[test]
    fn event_idx_suppresses_interrupts_and_notifications() {
        assert!(need_event(0, 1, 0));
        assert!(!need_event(5, 3, 2)); // driver asked to hear only after index 5
        assert!(need_event(u16::MAX, 0, u16::MAX)); // wrap-around

        let mut d = Driver::new(8);
        let mut q = d.queue(feature::VERSION_1 | feature::EVENT_IDX);
        for i in 0..3 {
            d.set_desc(i, DATA, 8, 0, 0);
            d.publish(i);
        }
        let used_event = AVAIL + 4 + 2 * 8;
        d.mem.access().unwrap().write_obj(used_event, 2u16).unwrap(); // interrupt when used passes 2
        for _ in 0..2 {
            let c = q.pop(&d.mem.access().unwrap()).unwrap().unwrap();
            q.add_used(&d.mem.access().unwrap(), c.head, 0).unwrap();
        }
        assert!(q.needs_interrupt(&d.mem.access().unwrap()).unwrap()); // first decision always interrupts
        let c = q.pop(&d.mem.access().unwrap()).unwrap().unwrap();
        q.add_used(&d.mem.access().unwrap(), c.head, 0).unwrap();
        assert!(q.needs_interrupt(&d.mem.access().unwrap()).unwrap()); // crossed used_event (2)
        d.mem.access().unwrap().write_obj(used_event, 10u16).unwrap();
        d.set_desc(3, DATA, 8, 0, 0);
        d.publish(3);
        let c = q.pop(&d.mem.access().unwrap()).unwrap().unwrap();
        q.add_used(&d.mem.access().unwrap(), c.head, 0).unwrap();
        assert!(!q.needs_interrupt(&d.mem.access().unwrap()).unwrap()); // driver asked for later

        // Re-arming reports buffers that raced in, and records avail_event.
        assert!(!q.enable_notification(&d.mem.access().unwrap()).unwrap());
        assert_eq!(
            d.mem.access().unwrap().read_obj::<u16>(USED + 4 + 8 * 8).unwrap(),
            4
        );
        d.set_desc(4, DATA, 8, 0, 0);
        d.publish(4);
        assert!(q.enable_notification(&d.mem.access().unwrap()).unwrap());
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

    fn interrupt() -> (std::sync::Arc<DeviceInterrupt>, std::sync::Arc<Edges>) {
        let edges = std::sync::Arc::new(Edges::default());
        let irq = std::sync::Arc::new(DeviceInterrupt::new(edges.clone()));
        (irq, edges)
    }

    /// A round answers a ring's worth of requests at most, then says whether requests may
    /// remain, with the driver's notifications left off only then; the driver hears of
    /// each round's completions (audit A09).
    #[test]
    fn a_round_answers_a_ring_at_most() {
        let mut d = Driver::new(8);
        let mut q = d.queue(feature::VERSION_1);
        let (irq, edges) = interrupt();
        let stop = AtomicBool::new(false);
        for i in 0..8 {
            d.set_desc(i, DATA + 64 * u64::from(i), 8, DESC_F_WRITE, 0);
            d.publish(i);
        }
        let answered = std::cell::RefCell::new(Vec::new());
        let mut answer = |c: &Chain| {
            answered.borrow_mut().push(c.head);
            1
        };
        let flags = |d: &Driver| d.mem.access().unwrap().read_obj::<u16>(USED).unwrap();
        assert_eq!(
            serve_round(&mut q, &d.mem, &irq, &stop, &mut answer).unwrap(),
            Round::More
        );
        assert_eq!(flags(&d), USED_F_NO_NOTIFY);
        assert_eq!(edges.0.load(Ordering::SeqCst), 1);
        // Four more, answered in the next round, which finds the queue empty after them.
        for i in 0..4 {
            d.publish(i);
        }
        assert_eq!(
            serve_round(&mut q, &d.mem, &irq, &stop, &mut answer).unwrap(),
            Round::Idle
        );
        assert_eq!(
            flags(&d),
            0,
            "notifications left off with nothing to come back for"
        );
        assert_eq!(edges.0.load(Ordering::SeqCst), 2);
        assert_eq!(*answered.borrow(), [0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 2, 3]);
        // An idle round interrupts nobody.
        assert_eq!(
            serve_round(&mut q, &d.mem, &irq, &stop, &mut answer).unwrap(),
            Round::Idle
        );
        assert_eq!(edges.0.load(Ordering::SeqCst), 2);
    }

    /// A stop ends a round between requests: what it did not answer stays in the ring, and
    /// the round after the stop answers it, each request once.
    #[test]
    fn a_stop_ends_a_round_between_requests() {
        let mut d = Driver::new(8);
        let mut q = d.queue(feature::VERSION_1 | feature::EVENT_IDX);
        let (irq, _) = interrupt();
        let stop = AtomicBool::new(false);
        for i in 0..6 {
            d.set_desc(i, DATA + 64 * u64::from(i), 8, DESC_F_WRITE, 0);
            d.publish(i);
        }
        let mut answered = Vec::new();
        let mut answer = |c: &Chain| {
            answered.push(c.head);
            // Asked to stop as it answers the second.
            if answered.len() == 2 {
                stop.store(true, Ordering::Release);
            }
            0
        };
        assert_eq!(
            serve_round(&mut q, &d.mem, &irq, &stop, &mut answer).unwrap(),
            Round::More
        );
        assert_eq!(answered, [0, 1]);
        stop.store(false, Ordering::Release);
        let mut rest = Vec::new();
        let mut answer = |c: &Chain| {
            rest.push(c.head);
            0
        };
        assert_eq!(
            serve_round(&mut q, &d.mem, &irq, &stop, &mut answer).unwrap(),
            Round::Idle
        );
        assert_eq!(rest, [2, 3, 4, 5]);
        assert_eq!(d.mem.access().unwrap().read_obj::<u16>(USED + 2).unwrap(), 6);
    }

    /// A driver that recycles every request the moment it completes, from another thread,
    /// holds the worker neither from stopping nor from telling it of its completions, past
    /// the 16-bit indices' wrap, with EVENT_IDX. No request is answered twice, and a
    /// resumed worker re-arms the driver's notifications once it has caught up, so what
    /// the driver adds next is heard of (audit A09). The indices start 40 short of the
    /// wrap, as a restored queue's may, so that Miri, which interprets every step, crosses
    /// it too (access-guard/check.sh).
    #[test]
    fn a_driver_recycling_the_ring_cannot_hold_its_worker() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;
        use std::time::{Duration, Instant};

        use super::super::worker::Worker;

        const SIZE: u16 = 16;
        const START: u16 = u16::MAX - 40;
        let mut d = Driver::new(SIZE);
        let mut q = d.queue(feature::VERSION_1 | feature::EVENT_IDX);
        q.set_state(QueueState {
            next_avail: START,
            next_used: START,
            signalled_used: None,
        });
        d.avail_idx = START;
        d.mem
            .access()
            .unwrap()
            .store_u16(USED + 2, START, Ordering::Release)
            .unwrap();
        for i in 0..SIZE {
            d.set_desc(i, DATA + 64 * u64::from(i), 8, DESC_F_WRITE, 0);
            d.publish(i);
        }
        let (irq, edges) = interrupt();
        let answered = Arc::new(AtomicUsize::new(0));
        let counted = answered.clone();
        let worker = Worker::start("test-queue", q, d.mem.clone(), irq.clone(), move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            0
        })
        .unwrap();
        worker.notify();
        let mut driver = Recycler {
            avail: START.wrapping_add(SIZE),
            seen: START,
            outstanding: [true; SIZE as usize],
        };
        let waker = worker.waker();
        let recycling = AtomicBool::new(true);
        let mut worker = Some(worker);
        std::thread::scope(|s| {
            let (mem, recycling, waker) = (&d.mem, &recycling, waker.clone());
            let recycler = s.spawn(move || {
                while recycling.load(Ordering::Acquire) {
                    driver.recycle(mem, &|| waker.unpark());
                    std::thread::yield_now();
                }
                driver
            });
            // Past the indices' wrap, then a stop in the middle of it. Miri's clock is its
            // own, and slow.
            let (enough, patience) = if cfg!(miri) { (100, 3600) } else { (5_000, 20) };
            let deadline = Instant::now() + Duration::from_secs(patience);
            while answered.load(Ordering::SeqCst) < enough {
                assert!(Instant::now() < deadline, "the worker stalled at {answered:?}");
                std::thread::sleep(Duration::from_millis(1));
            }
            let t0 = Instant::now();
            let stopped = worker.take().unwrap().stop();
            let took = t0.elapsed();
            recycling.store(false, Ordering::Release);
            let mut driver = recycler.join().unwrap();
            assert!(
                cfg!(miri) || took < Duration::from_millis(500),
                "stopping took {took:?}"
            );
            assert!(edges.0.load(Ordering::SeqCst) > 0, "no completion was signalled");
            // Resumed with what the stop left: it catches up, re-arms, and hears of the
            // next request.
            let q = stopped.expect("a queue back from the stop");
            let counted = answered.clone();
            let resumed = Worker::start("test-queue", q, d.mem.clone(), irq.clone(), move |_| {
                counted.fetch_add(1, Ordering::SeqCst);
                0
            })
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while d
                .mem
                .access()
                .unwrap()
                .load_u16(USED + 2, Ordering::Acquire)
                .unwrap()
                != driver.avail
            {
                assert!(Instant::now() < deadline, "the resumed worker never caught up");
                std::thread::sleep(Duration::from_millis(1));
            }
            let before = answered.load(Ordering::SeqCst);
            let waker = resumed.waker();
            driver.recycle(&d.mem, &|| waker.unpark());
            let deadline = Instant::now() + Duration::from_secs(10);
            while answered.load(Ordering::SeqCst) == before {
                assert!(
                    Instant::now() < deadline,
                    "a request after the catch-up went unheard"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            assert!(!irq.failed(), "the ring was taken for malformed");
            let _ = resumed.stop();
        });
    }

    /// A driver that publishes every request again as soon as it is used, and notifies as
    /// EVENT_IDX tells it to.
    struct Recycler {
        avail: u16,
        seen: u16,
        /// Per request, whether it is published and not yet used.
        outstanding: [bool; 16],
    }

    impl Recycler {
        fn recycle(&mut self, mem: &GuestMemory, notify: &dyn Fn()) {
            const SIZE: u16 = 16;
            let avail_event = USED + 4 + 8 * u64::from(SIZE);
            let used_event = AVAIL + 4 + 2 * u64::from(SIZE);
            let a = mem.access().unwrap();
            let used = a.load_u16(USED + 2, Ordering::Acquire).unwrap();
            while self.seen != used {
                let slot = u64::from(self.seen % SIZE);
                let head = a.read_obj::<u32>(USED + 4 + 8 * slot).unwrap() as u16;
                assert!(
                    std::mem::replace(&mut self.outstanding[usize::from(head)], false),
                    "request {head} answered twice"
                );
                self.seen = self.seen.wrapping_add(1);
                a.write_obj(AVAIL + 4 + 2 * u64::from(self.avail % SIZE), head)
                    .unwrap();
                let old = self.avail;
                self.avail = self.avail.wrapping_add(1);
                a.store_u16(AVAIL + 2, self.avail, Ordering::Release).unwrap();
                self.outstanding[usize::from(head)] = true;
                // Interrupt me after the next completion.
                a.write_obj(used_event, self.seen).unwrap();
                fence(Ordering::SeqCst);
                let event = a.read_obj::<u16>(avail_event).unwrap();
                if need_event(event, self.avail, old) {
                    notify();
                }
            }
        }
    }

    #[test]
    fn validates_configuration() {
        let d = Driver::new(8);
        let base = QueueConfig {
            size: 8,
            desc: DESC,
            avail: AVAIL,
            used: USED,
            ready: true,
        };
        let bad = [
            QueueConfig { size: 0, ..base },
            QueueConfig { size: 6, ..base },
            QueueConfig { size: 16, ..base }, // above max
            QueueConfig {
                desc: DESC + 8,
                ..base
            },
            QueueConfig {
                used: USED + 2,
                ..base
            },
            QueueConfig { used: 0x10, ..base }, // not guest RAM
        ];
        for cfg in bad {
            assert!(Queue::new(cfg, 8, &d.mem, 0).is_err(), "{cfg:?}");
        }
        assert!(Queue::new(base, 8, &d.mem, 0).is_ok());
    }

    /// Two devices' threads working queues a driver laid over each other, each queue's
    /// rings on the other's descriptors and rings, never race in the host: each index store
    /// of one lands on the other's descriptors, and what they read is the guest's garbage,
    /// returned as errors (audit A01). ThreadSanitizer and Miri check this test
    /// (docs/research/measurements/access-guard).
    #[test]
    fn queues_laid_over_each_other_work_on_two_threads() {
        let d = Driver::new(8);
        let cfg = |desc, avail, used| QueueConfig {
            size: 8,
            desc,
            avail,
            used,
            ready: true,
        };
        let configs = [cfg(DESC, AVAIL, USED), cfg(USED, DESC, AVAIL)];
        std::thread::scope(|s| {
            for c in configs {
                let mem = &d.mem;
                s.spawn(move || {
                    let mut q = Queue::new(c, 8, mem, feature::EVENT_IDX).unwrap();
                    for i in 0..2000u16 {
                        // Each thread plays its queue's driver too, publishing a buffer.
                        let _ = with(mem, |a| {
                            a.store_u16(c.avail + 2, i.wrapping_add(1), Ordering::Release)?;
                            if let Some(chain) = q.pop(a)? {
                                q.add_used(a, chain.head, 1)?;
                            }
                            q.needs_interrupt(a)?;
                            q.enable_notification(a)
                        });
                    }
                });
            }
        });
    }
}
