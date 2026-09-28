//! shards control page: the guest writes 32-bit boot-phase markers at offset 0 and the
//! VMM timestamps them. This is how boot time is measured end to end, in the manner of
//! Firecracker's boot-timer device (docs/research/boot-latency.md).

use std::sync::Mutex;

use super::{MmioDevice, get_le};
use crate::sync::lock;

#[derive(Debug, Default)]
pub struct Control {
    /// `(marker, microseconds since VMM start)`, in arrival order.
    markers: Mutex<Vec<(u32, u128)>>,
}

impl Control {
    pub fn markers(&self) -> Vec<(u32, u128)> {
        lock(&self.markers).clone()
    }
}

impl MmioDevice for Control {
    fn read(&self, _offset: u64, data: &mut [u8]) {
        data.fill(0);
    }

    fn write(&self, offset: u64, data: &[u8]) {
        if offset != 0 || data.len() != 4 {
            return;
        }
        let at = crate::log::uptime_us();
        let marker = get_le(data) as u32;
        crate::info!("guest marker {marker} at {at} us");
        lock(&self.markers).push((marker, at));
    }
}
