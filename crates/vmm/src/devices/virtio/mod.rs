//! virtio devices (OASIS VIRTIO 1.3) over the MMIO transport.
//!
//! Every exit is a userspace exit on HVF (~0.8 µs), so devices negotiate
//! VIRTIO_RING_F_EVENT_IDX and a queue notify only hands work to a device thread
//! (docs/research/virtio-io-exits.md R2).

pub mod block;
#[cfg(unix)]
pub mod fs;
pub mod mmio;
#[cfg(unix)]
pub mod net;
pub mod pmem;
pub mod queue;
#[cfg(unix)]
pub mod vsock;
mod worker;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::Interrupt;
use crate::memory::GuestMemory;
use crate::snapshot::codec::{self, Reader, Writer};
use queue::{Queue, QueueState};

/// Feature bits (virtio 1.3 §6).
pub mod feature {
    pub const INDIRECT_DESC: u64 = 1 << 28;
    pub const EVENT_IDX: u64 = 1 << 29;
    pub const VERSION_1: u64 = 1 << 32;
}

/// Device status bits (virtio 1.3 §2.1).
pub mod status {
    pub const ACKNOWLEDGE: u32 = 1;
    pub const DRIVER: u32 = 2;
    pub const DRIVER_OK: u32 = 4;
    pub const FEATURES_OK: u32 = 8;
    pub const DEVICE_NEEDS_RESET: u32 = 64;
    pub const FAILED: u32 = 128;
}

/// InterruptStatus bits (virtio 1.3 §4.2.2).
const INT_USED_BUFFER: u32 = 1;
const INT_CONFIG_CHANGE: u32 = 2;

/// The interrupt line a device raises, with the MMIO InterruptStatus it reports.
pub struct DeviceInterrupt {
    status: AtomicU32,
    /// Set by a device that met a malformed ring; reported as DEVICE_NEEDS_RESET.
    failed: AtomicBool,
    line: Arc<dyn Interrupt>,
}

impl std::fmt::Debug for DeviceInterrupt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceInterrupt")
            .field("status", &self.status.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl DeviceInterrupt {
    pub fn new(line: Arc<dyn Interrupt>) -> DeviceInterrupt {
        DeviceInterrupt {
            status: AtomicU32::new(0),
            failed: AtomicBool::new(false),
            line,
        }
    }

    /// Signals that used buffers are available (the line is edge-triggered).
    pub fn used_buffer(&self) {
        self.status.fetch_or(INT_USED_BUFFER, Ordering::AcqRel);
        self.line.set_level(true);
    }

    pub fn config_change(&self) {
        self.status.fetch_or(INT_CONFIG_CHANGE, Ordering::AcqRel);
        self.line.set_level(true);
    }

    /// Stops the device from being used until the driver resets it (§2.1.2).
    pub fn fail(&self) {
        self.failed.store(true, Ordering::Release);
        self.config_change();
    }

    fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    fn status(&self) -> u32 {
        self.status.load(Ordering::Acquire)
    }

    /// Restores the InterruptStatus bits and the failed flag without raising the line:
    /// the restored GIC already holds any pending edge.
    fn set_state(&self, status: u32, failed: bool) {
        self.status
            .store(status & (INT_USED_BUFFER | INT_CONFIG_CHANGE), Ordering::Release);
        self.failed.store(failed, Ordering::Release);
    }

    fn ack(&self, bits: u32) {
        self.status.fetch_and(!bits, Ordering::AcqRel);
    }

    fn clear(&self) {
        self.status.store(0, Ordering::Release);
        self.failed.store(false, Ordering::Release);
    }
}

/// What a device receives when the driver sets DRIVER_OK.
#[derive(Debug)]
pub struct Activation {
    pub memory: Arc<GuestMemory>,
    pub queues: Vec<Queue>,
    pub interrupt: Arc<DeviceInterrupt>,
    /// The feature set the driver accepted.
    pub features: u64,
    /// The device is resuming from a snapshot, in a VM whose host side is new.
    pub restored: bool,
}

/// A virtio device behind a transport.
pub trait VirtioDevice: Send {
    fn device_id(&self) -> u32;
    /// Offered feature bits; the transport adds nothing and removes nothing.
    fn features(&self) -> u64;
    /// Maximum size of each virtqueue (their count is the queue count).
    fn queue_max_sizes(&self) -> &[u16];
    fn read_config(&self, offset: u64, data: &mut [u8]);
    fn write_config(&mut self, offset: u64, data: &[u8]);
    /// Starts processing. On error the transport reports DEVICE_NEEDS_RESET.
    fn activate(&mut self, activation: Activation) -> Result<(), String>;
    /// Called on a vCPU thread when the driver notifies `queue`. Must not block: work that
    /// can wait goes to a device thread.
    fn notify(&self, queue: u16);
    /// Stops all processing; when this returns the device no longer touches guest
    /// memory, so the driver may reuse it.
    fn reset(&mut self);
    /// Stops processing at a request boundary; nothing touches guest memory until
    /// `resume`. Returns each queue's progress, or nothing if the device is not active.
    fn pause(&mut self) -> Vec<QueueState>;
    fn resume(&mut self) -> Result<(), String>;
    /// Writes what a snapshot keeps of the device beyond its queues. Called while paused.
    fn save(&self, _w: &mut Writer) {}
    /// Reads what `save` wrote, before a restored device is activated.
    fn restore(&mut self, _r: &mut Reader<'_>) -> codec::Result<()> {
        Ok(())
    }
}
