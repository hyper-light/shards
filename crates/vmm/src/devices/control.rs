//! shards control page: the guest's side channel to the VMM (crates/abi `control`).
//! Markers are timestamped on arrival, which is how boot and restore time are measured
//! end to end, in the manner of Firecracker's boot-timer device
//! (docs/research/boot-latency.md). The guest also asks for snapshots here, reads how
//! many restores precede it, and says which contract its init speaks.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use shards_abi::{control, marker};

use super::{MmioDevice, get_le, put_le};
use crate::snapshot::codec::{Reader, Result, Writer};

pub type SnapshotRequest = Box<dyn Fn() + Send + Sync>;

/// The markers a guest may send: shards_abi::marker's, 1 to POWERING_OFF.
const MARKERS: usize = marker::POWERING_OFF as usize + 1;

#[derive(Default)]
pub struct Control {
    /// Each marker's first arrival, in microseconds since the VMM started, plus one; 0 for
    /// none yet. A table, not a list: a guest writing markers without end costs nothing.
    first: [AtomicU64; MARKERS],
    /// Whether a request for a snapshot no policy serves has been said.
    said_unserved: AtomicBool,
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
    /// Each marker that came, `(marker, µs since the VMM started)`, at its first
    /// arrival, in the order they came.
    pub fn markers(&self) -> Vec<(u32, u128)> {
        let mut came: Vec<(u32, u128)> = (0..MARKERS)
            .filter_map(|m| {
                let at = self.first.get(m)?.load(Ordering::Acquire);
                Some((u32::try_from(m).ok()?, u128::from(at.checked_sub(1)?)))
            })
            .collect();
        came.sort_by_key(|&(_, at)| at);
        came
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
                let stamp = u64::try_from(at).unwrap_or(u64::MAX - 1).saturating_add(1);
                if let Some(first) = usize::try_from(value).ok().and_then(|m| self.first.get(m))
                    && first
                        .compare_exchange(0, stamp, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                {
                    crate::info!("guest marker {value} at {at} us");
                }
            }
            control::SNAPSHOT if value == control::SNAPSHOT_NOW => match self.on_snapshot.get() {
                Some(request) => request(),
                None if !self.said_unserved.swap(true, Ordering::Relaxed) => {
                    crate::warn!("guest asked for a snapshot, but snapshots are not enabled");
                }
                None => {}
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

    /// A marker counts at its first arrival; one past the guest's markers, and every
    /// repeat, cost nothing, however many come.
    #[test]
    fn markers_count_once_and_hold_nothing_more() {
        let c = Control::default();
        c.write(control::MARKER, &marker::CONNECTED.to_le_bytes());
        c.write(control::MARKER, &marker::INIT_STARTED.to_le_bytes());
        for _ in 0..100_000 {
            c.write(control::MARKER, &marker::CONNECTED.to_le_bytes());
            c.write(control::MARKER, &u32::MAX.to_le_bytes());
            c.write(control::MARKER, &(marker::POWERING_OFF + 1).to_le_bytes());
        }
        // In the order of their times; two in one microsecond, in either.
        let came = c.markers();
        assert!(came.windows(2).all(|w| w[0].1 <= w[1].1), "{came:?}");
        let mut seen: Vec<u32> = came.iter().map(|&(m, _)| m).collect();
        seen.sort_unstable();
        assert_eq!(seen, [marker::INIT_STARTED, marker::CONNECTED]);
    }

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
