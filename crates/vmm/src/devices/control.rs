//! shards control page: the guest's side channel to the VMM (crates/abi `control`).
//! Markers are timestamped on arrival, which is how boot and restore time are measured
//! end to end, in the manner of Firecracker's boot-timer device
//! (docs/research/boot-latency.md). The guest also asks for snapshots here, reads how
//! many restores precede it, and says which contract its init speaks.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use shards_abi::control;

use super::{MmioDevice, get_le, put_le};
use crate::snapshot::codec::{Reader, Result, Writer};
use crate::sync::lock;

pub type SnapshotRequest = Box<dyn Fn() + Send + Sync>;

#[derive(Default)]
pub struct Control {
    /// `(marker, microseconds since VMM start)`, in arrival order.
    markers: Mutex<Vec<(u32, u128)>>,
    generation: AtomicU32,
    /// The identity the guest's init announced (`control::ABI`), or 0 before it does.
    abi: AtomicU64,
    on_snapshot: OnceLock<SnapshotRequest>,
}

impl std::fmt::Debug for Control {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Control")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl Control {
    pub fn markers(&self) -> Vec<(u32, u128)> {
        lock(&self.markers).clone()
    }

    /// Where guest snapshot requests go. Without one, requests are ignored.
    pub fn on_snapshot(&self, request: SnapshotRequest) {
        let _ = self.on_snapshot.set(request);
    }

    pub fn generation(&self) -> u32 {
        self.generation.load(Ordering::Acquire)
    }

    /// The contract the guest's init announced, if it has.
    pub fn guest_abi(&self) -> Option<u64> {
        Some(self.abi.load(Ordering::Acquire)).filter(|&abi| abi != 0)
    }
}

/// The host's wall clock in nanoseconds since the Unix epoch, or 0 if it is before it.
fn host_time_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

impl MmioDevice for Control {
    fn read(&self, offset: u64, data: &mut [u8]) {
        let value = match offset {
            control::GENERATION => u64::from(self.generation()),
            // In one access only: halves read apart could straddle a carry.
            control::HOST_TIME if data.len() == 8 => host_time_ns(),
            _ => 0,
        };
        put_le(data, value);
    }

    fn write(&self, offset: u64, data: &[u8]) {
        // In one access only, as it is read.
        if offset == control::ABI && data.len() == 8 {
            self.abi.store(get_le(data), Ordering::Release);
            return;
        }
        if data.len() != 4 {
            return;
        }
        let value = get_le(data) as u32;
        match offset {
            control::MARKER => {
                let at = crate::log::uptime_us();
                crate::info!("guest marker {value} at {at} us");
                lock(&self.markers).push((value, at));
            }
            control::SNAPSHOT if value == control::SNAPSHOT_NOW => match self.on_snapshot.get() {
                Some(request) => request(),
                None => crate::warn!("guest asked for a snapshot, but snapshots are not enabled"),
            },
            _ => {}
        }
    }

    fn pause(&self) {}

    fn resume(&self) -> std::result::Result<(), String> {
        Ok(())
    }

    /// Markers are this run's measurements, not guest state.
    fn save(&self, w: &mut Writer) {
        w.u32(self.generation());
        w.u64(self.abi.load(Ordering::Acquire));
    }

    /// A restored VM is the next generation of the one saved, and its init is the same.
    fn restore(&self, r: &mut Reader<'_>) -> Result<()> {
        let saved = r.u32()?;
        let abi = r.u64()?;
        self.generation.store(saved.saturating_add(1), Ordering::Release);
        self.abi.store(abi, Ordering::Release);
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_init_announces_its_contract_and_a_snapshot_keeps_it() {
        let c = Control::default();
        assert_eq!(c.guest_abi(), None);
        // Only in one 64-bit access.
        c.write(control::ABI, &0x1234_5678u32.to_le_bytes());
        assert_eq!(c.guest_abi(), None);
        c.write(control::ABI, &shards_abi::IDENTITY.to_le_bytes());
        assert_eq!(c.guest_abi(), Some(shards_abi::IDENTITY));

        let mut w = Writer::default();
        c.save(&mut w);
        let saved = w.into_bytes();
        let restored = Control::default();
        restored.restore(&mut Reader::new(&saved)).unwrap();
        assert_eq!(restored.guest_abi(), Some(shards_abi::IDENTITY));
        assert_eq!(restored.generation(), 1);
    }
}
