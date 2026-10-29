//! shards control page: the guest's side channel to the VMM (crates/abi `control`).
//! Markers are timestamped on arrival, which is how boot and restore time are measured
//! end to end, in the manner of Firecracker's boot-timer device
//! (docs/research/boot-latency.md). The guest also asks for snapshots here, reads how
//! many restores precede it, says which contract its init speaks, and asks for memory to
//! be plugged (D133).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use shards_abi::{control, marker};

use super::virtio::head::WindowHeads;
use super::virtio::mem::Sizes;
use super::virtio::pmem::{Layer, SwapWindow};
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
    /// The virtio-mem device's sizes, where the guest has memory to plug.
    hotplug: OnceLock<std::sync::Arc<Sizes>>,
    /// The MAC the VMM was given for the guest's network device (`control::MAC`), or 0.
    mac: AtomicU64,
    /// The guest's swap window, where it has one (D159).
    swap: OnceLock<std::sync::Arc<SwapWindow>>,
    /// The run's writable layer and its guest address, where it has one (D190).
    layer: OnceLock<(std::sync::Arc<Layer>, u64)>,
    /// Its DAX windows' heads, where it has windows (D189).
    window_heads: OnceLock<std::sync::Arc<WindowHeads>>,
    /// What runs as the guest first says its PID 1 runs (`marker::INIT_STARTED`).
    on_init_started: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
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

    /// Where the guest's requests for memory go: its virtio-mem device's sizes (D133).
    /// Without them, the guest has none to plug.
    pub fn on_hotplug(&self, sizes: std::sync::Arc<Sizes>) {
        let _ = self.hotplug.set(sizes);
    }

    /// Sets the most memory the guest may have plugged, in bytes: a run's own, as a VM
    /// restored from a template takes it (D148). False where it has none to plug.
    pub fn set_plug_most(&self, bytes: u64) -> bool {
        self.hotplug.get().map(|s| s.set_most(bytes)).is_some()
    }

    /// The guest's swap window (D159), which the guest is shown here and a run's swap is
    /// mapped into.
    pub fn on_swap(&self, window: std::sync::Arc<SwapWindow>) {
        let _ = self.swap.set(window);
    }

    /// The guest's swap window, where it has one.
    pub fn swap(&self) -> Option<&std::sync::Arc<SwapWindow>> {
        self.swap.get()
    }

    /// The run's writable layer (D190), at `gpa`: which the guest finds here, and a
    /// snapshot keeps.
    pub fn on_layer(&self, layer: std::sync::Arc<Layer>, gpa: u64) {
        let _ = self.layer.set((layer, gpa));
    }

    /// The run's writable layer, where it has one.
    pub fn layer(&self) -> Option<&std::sync::Arc<Layer>> {
        self.layer.get().map(|(layer, _)| layer)
    }

    /// Its DAX windows' heads (D189), which a snapshot keeps.
    pub fn on_window_heads(&self, heads: std::sync::Arc<WindowHeads>) {
        let _ = self.window_heads.set(heads);
    }

    /// Its DAX windows' heads, where it has windows.
    pub fn window_heads(&self) -> Option<&std::sync::Arc<WindowHeads>> {
        self.window_heads.get()
    }

    /// Whether the guest has lost its writable layer, its run's swap or a DAX window's head:
    /// a mapping taken from it for a save that could not be given back, so that its machine
    /// cannot go on.
    pub fn lost(&self) -> bool {
        self.layer().is_some_and(|l| l.lost())
            || self.swap().is_some_and(|w| w.lost())
            || self.window_heads().is_some_and(|h| h.lost())
    }

    /// Runs `hook` as the guest first says its PID 1 runs (`marker::INIT_STARTED`), on
    /// the vCPU that says so: its kernel's boot is done.
    pub fn on_init_started(&self, hook: Box<dyn FnOnce() + Send>) {
        *self
            .on_init_started
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(hook);
    }

    /// The MAC the VMM was given for the guest's network device, which the guest reads
    /// (`control::MAC`): a restored VM's own, where its guest still has its template's.
    pub fn on_mac(&self, mac: [u8; 6]) {
        let [a, b, c, d, e, f] = mac;
        self.mac
            .store(u64::from_le_bytes([a, b, c, d, e, f, 0, 0]), Ordering::Release);
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
            control::HOTPLUG_SIZE if data.len() == 8 => self.hotplug.get().map_or(0, |s| s.most()),
            control::HOTPLUG_REQUEST if data.len() == 8 => self.hotplug.get().map_or(0, |s| s.requested()),
            control::MAC if data.len() == 8 => self.mac.load(Ordering::Acquire),
            control::SWAP if data.len() == 8 => self.swap.get().map_or(0, |w| w.len()),
            control::LAYER if data.len() == 8 => self.layer.get().map_or(0, |&(_, gpa)| gpa),
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
        if offset == control::HOTPLUG_REQUEST && data.len() == 8 {
            if let Some(sizes) = self.hotplug.get() {
                sizes.request(get_le(data));
            }
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
                    if value == marker::INIT_STARTED {
                        let hook = self
                            .on_init_started
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .take();
                        if let Some(hook) = hook {
                            hook();
                        }
                    }
                }
            }
            control::SWAP if value == control::SHOW_SWAP => {
                if let Some(window) = self.swap.get() {
                    window.show();
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

    /// Memory of a DAX window's head the host commits as its guest first touches it (D189,
    /// PM M413): given, and the access runs again.
    fn memory_fault(&self, gpa: Option<u64>) -> bool {
        gpa.zip(self.window_heads.get())
            .is_some_and(|(gpa, heads)| heads.memory_fault(gpa))
    }

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

    /// The guest asks for memory here, 64 bits in one access: rounded up to the device's
    /// blocks, at most what it may have; without a device it may have none.
    #[test]
    fn the_guest_asks_for_memory_within_what_it_may_have() {
        use crate::devices::virtio::mem::BLOCK;
        let read = |c: &Control, at| {
            let mut v = [0u8; 8];
            c.read(at, &mut v);
            u64::from_le_bytes(v)
        };
        let c = Control::default();
        c.write(control::HOTPLUG_REQUEST, &(64 * BLOCK).to_le_bytes());
        assert_eq!(
            (
                read(&c, control::HOTPLUG_SIZE),
                read(&c, control::HOTPLUG_REQUEST)
            ),
            (0, 0)
        );

        let sizes = std::sync::Arc::new(Sizes::new(1 << 32, 64 * BLOCK, 48 * BLOCK));
        c.on_hotplug(sizes.clone());
        assert_eq!(read(&c, control::HOTPLUG_SIZE), 48 * BLOCK);
        c.write(control::HOTPLUG_REQUEST, &(BLOCK + 1).to_le_bytes());
        assert_eq!(read(&c, control::HOTPLUG_REQUEST), 2 * BLOCK);
        // Only in one 64-bit access.
        c.write(control::HOTPLUG_REQUEST, &(8 * BLOCK as u32).to_le_bytes());
        assert_eq!(sizes.requested(), 2 * BLOCK);
        let mut half = [0u8; 4];
        c.read(control::HOTPLUG_SIZE, &mut half);
        assert_eq!(half, [0; 4]);
        c.write(control::HOTPLUG_REQUEST, &u64::MAX.to_le_bytes());
        assert_eq!(read(&c, control::HOTPLUG_REQUEST), 48 * BLOCK);
    }

    /// The MAC the VMM was given reads whole, in one 64-bit access, its first octet lowest;
    /// none, 0; a half of it, nothing (D31, E1).
    #[test]
    fn the_guest_reads_the_mac_its_vm_was_given() {
        let c = Control::default();
        let mut word = [0u8; 8];
        c.read(control::MAC, &mut word);
        assert_eq!(u64::from_le_bytes(word), 0);
        c.on_mac([0x02, 0x42, 0xac, 0x11, 0x00, 0x05]);
        c.read(control::MAC, &mut word);
        assert_eq!(word, [0x02, 0x42, 0xac, 0x11, 0x00, 0x05, 0, 0]);
        let mut half = [0u8; 4];
        c.read(control::MAC, &mut half);
        assert_eq!(half, [0; 4]);
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
