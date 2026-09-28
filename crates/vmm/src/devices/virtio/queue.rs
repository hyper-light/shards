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
use std::sync::atomic::{Ordering, fence};

use super::feature;
use crate::memory::{GuestMemory, OutOfBounds, Pod};

const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;
const DESC_F_INDIRECT: u16 = 4;
const AVAIL_F_NO_INTERRUPT: u16 = 1;
const USED_F_NO_NOTIFY: u16 = 1;
/// Upper bound on descriptors in one chain, direct and indirect together. It covers
/// the largest request a driver can build from the segment limits we advertise.
pub const MAX_CHAIN: usize = 4096;
pub const MAX_QUEUE_SIZE: u16 = 32768;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueError {
    Memory(OutOfBounds),
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
            QueueError::AvailIndexJump { published, size } => {
                write!(f, "driver published {published} new buffers on a queue of {size}")
            }
            QueueError::DescriptorIndex(i) => write!(f, "descriptor index {i} out of range"),
            QueueError::ChainTooLong => write!(f, "descriptor chain loops or exceeds {MAX_CHAIN} entries"),
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
/// ordering (virtio 1.3 §2.7.4).
#[derive(Default)]
struct ChainBuilder {
    descriptors: Vec<Descriptor>,
    seen_writable: bool,
}

impl ChainBuilder {
    fn push(&mut self, d: &RawDesc) -> Result<(), QueueError> {
        if self.descriptors.len() >= MAX_CHAIN {
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

fn walk_indirect(mem: &GuestMemory, table: &RawDesc, chain: &mut ChainBuilder) -> Result<(), QueueError> {
    if table.len == 0 || !table.len.is_multiple_of(16) || table.len as usize / 16 > MAX_CHAIN {
        return Err(QueueError::IndirectLength(table.len));
    }
    let count = (table.len / 16) as u16;
    mem.host_ptr(table.addr, table.len as usize)?;
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

    fn slot(&self, index: Wrapping<u16>) -> u64 {
        u64::from(index.0 & (self.size - 1))
    }

    fn avail_idx(&self, mem: &GuestMemory) -> Result<Wrapping<u16>, QueueError> {
        Ok(Wrapping(mem.atomic_u16(self.avail + 2)?.load(Ordering::Acquire)))
    }

    /// Takes the next available request, or `None` if the ring is empty.
    pub fn pop(&mut self, mem: &GuestMemory) -> Result<Option<Chain>, QueueError> {
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

    fn walk(&self, mem: &GuestMemory, head: u16) -> Result<Chain, QueueError> {
        let mut chain = ChainBuilder::default();
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
    pub fn add_used(&mut self, mem: &GuestMemory, head: u16, len: u32) -> Result<(), QueueError> {
        let elem = UsedElem {
            id: u32::from(head),
            len,
        };
        mem.write_obj(self.used + 4 + 8 * self.slot(self.next_used), elem)?;
        self.next_used += 1;
        mem.atomic_u16(self.used + 2)?
            .store(self.next_used.0, Ordering::Release);
        Ok(())
    }

    /// Whether the driver wants an interrupt for the used buffers added since the last
    /// one (EVENT_IDX `used_event`, or the NO_INTERRUPT flag without it).
    pub fn needs_interrupt(&mut self, mem: &GuestMemory) -> Result<bool, QueueError> {
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
    pub fn disable_notification(&mut self, mem: &GuestMemory) -> Result<(), QueueError> {
        if !self.event_idx {
            mem.write_obj(self.used, USED_F_NO_NOTIFY)?;
        }
        Ok(())
    }

    /// Re-arms driver notifications. Returns true if buffers arrived meanwhile, in
    /// which case the caller must keep draining (the driver may not notify for them).
    pub fn enable_notification(&mut self, mem: &GuestMemory) -> Result<bool, QueueError> {
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
        mem: GuestMemory,
        size: u16,
        avail_idx: u16,
    }

    impl Driver {
        fn new(size: u16) -> Driver {
            let p = crate::memory::page_size().unwrap();
            let mem = GuestMemory::anonymous(&[(BASE, 8 * p)]).unwrap();
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
            self.mem.write_obj(DESC + 16 * u64::from(i), d).unwrap();
        }
        fn publish(&mut self, head: u16) {
            let slot = u64::from(self.avail_idx % self.size);
            self.mem.write_obj(AVAIL + 4 + 2 * slot, head).unwrap();
            self.avail_idx = self.avail_idx.wrapping_add(1);
            self.mem.write_obj(AVAIL + 2, self.avail_idx).unwrap();
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
        let c = q.pop(&d.mem).unwrap().unwrap();
        assert_eq!(c.head, 0);
        assert_eq!(c.readable().count(), 1);
        assert_eq!(c.writable().map(|x| x.len).collect::<Vec<_>>(), vec![512, 1]);
        assert!(q.pop(&d.mem).unwrap().is_none());
        q.add_used(&d.mem, c.head, 513).unwrap();
        assert_eq!(d.mem.read_obj::<u16>(USED + 2).unwrap(), 1);
        assert_eq!(d.mem.read_obj::<u32>(USED + 4).unwrap(), 0);
        assert_eq!(d.mem.read_obj::<u32>(USED + 8).unwrap(), 513);
    }

    #[test]
    fn rejects_hostile_rings() {
        let mut d = Driver::new(4);
        let mut q = d.queue(feature::VERSION_1);
        d.set_desc(0, DATA, 8, DESC_F_NEXT, 1);
        d.set_desc(1, DATA, 8, DESC_F_NEXT, 0); // cycle 0 -> 1 -> 0
        d.publish(0);
        assert_eq!(q.pop(&d.mem), Err(QueueError::ChainTooLong));

        d.set_desc(2, DATA, 8, DESC_F_NEXT, 9); // next out of range
        d.publish(2);
        assert_eq!(q.pop(&d.mem), Err(QueueError::DescriptorIndex(9)));

        d.publish(7); // head out of range
        assert_eq!(q.pop(&d.mem), Err(QueueError::DescriptorIndex(7)));

        d.set_desc(3, DATA, 8, DESC_F_WRITE | DESC_F_NEXT, 0);
        d.set_desc(0, DATA, 8, 0, 0);
        d.publish(3); // writable then readable
        assert_eq!(q.pop(&d.mem), Err(QueueError::ReadableAfterWritable));

        d.set_desc(0, DATA, 16, DESC_F_INDIRECT, 0);
        d.publish(0); // indirect not negotiated
        assert_eq!(q.pop(&d.mem), Err(QueueError::IndirectNotNegotiated));

        // The driver claims far more new buffers than the ring holds.
        d.mem.write_obj(AVAIL + 2, d.avail_idx.wrapping_add(100)).unwrap();
        assert!(matches!(q.pop(&d.mem), Err(QueueError::AvailIndexJump { .. })));
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
        d.mem.write_obj(table, raw(DATA, 16, DESC_F_NEXT, 1)).unwrap();
        d.mem
            .write_obj(table + 16, raw(DATA + 16, 4096, DESC_F_NEXT | DESC_F_WRITE, 2))
            .unwrap();
        d.mem
            .write_obj(table + 32, raw(DATA + 4112, 1, DESC_F_WRITE, 0))
            .unwrap();
        d.set_desc(0, table, 48, DESC_F_INDIRECT, 0);
        d.publish(0);
        let c = q.pop(&d.mem).unwrap().unwrap();
        assert_eq!(c.descriptors.len(), 3);
        assert_eq!(c.writable().count(), 2);

        d.mem
            .write_obj(table + 16, raw(table, 48, DESC_F_INDIRECT, 0))
            .unwrap();
        d.set_desc(1, table, 48, DESC_F_INDIRECT, 0);
        d.publish(1);
        assert_eq!(q.pop(&d.mem), Err(QueueError::NestedIndirect));

        d.set_desc(2, table, 20, DESC_F_INDIRECT, 0);
        d.publish(2);
        assert_eq!(q.pop(&d.mem), Err(QueueError::IndirectLength(20)));
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
        d.mem.write_obj(used_event, 2u16).unwrap(); // interrupt when used passes 2
        for _ in 0..2 {
            let c = q.pop(&d.mem).unwrap().unwrap();
            q.add_used(&d.mem, c.head, 0).unwrap();
        }
        assert!(q.needs_interrupt(&d.mem).unwrap()); // first decision always interrupts
        let c = q.pop(&d.mem).unwrap().unwrap();
        q.add_used(&d.mem, c.head, 0).unwrap();
        assert!(q.needs_interrupt(&d.mem).unwrap()); // crossed used_event (2)
        d.mem.write_obj(used_event, 10u16).unwrap();
        d.set_desc(3, DATA, 8, 0, 0);
        d.publish(3);
        let c = q.pop(&d.mem).unwrap().unwrap();
        q.add_used(&d.mem, c.head, 0).unwrap();
        assert!(!q.needs_interrupt(&d.mem).unwrap()); // driver asked for later

        // Re-arming reports buffers that raced in, and records avail_event.
        assert!(!q.enable_notification(&d.mem).unwrap());
        assert_eq!(d.mem.read_obj::<u16>(USED + 4 + 8 * 8).unwrap(), 4);
        d.set_desc(4, DATA, 8, 0, 0);
        d.publish(4);
        assert!(q.enable_notification(&d.mem).unwrap());
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
}
