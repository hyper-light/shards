//! Emulated devices and the MMIO bus that routes trapped guest accesses to them.

pub mod control;
pub mod rtc;
pub mod serial;

use std::sync::Arc;

/// A device occupying a guest-physical MMIO window. Accesses arrive from vCPU threads
/// concurrently, so devices synchronize internally.
pub trait MmioDevice: Send + Sync {
    /// Fills `data` (1, 2, 4 or 8 bytes, little-endian) from register `offset`.
    fn read(&self, offset: u64, data: &mut [u8]);
    fn write(&self, offset: u64, data: &[u8]);
}

/// Stores `value` little-endian into `data` (an access of 1-8 bytes); bytes past the
/// value's width read as zero.
pub fn put_le(data: &mut [u8], value: u64) {
    let bytes = value.to_le_bytes();
    for (i, b) in data.iter_mut().enumerate() {
        *b = bytes.get(i).copied().unwrap_or(0);
    }
}

/// Reads a little-endian value from an access of 1-8 bytes (extra bytes are ignored).
pub fn get_le(data: &[u8]) -> u64 {
    data.iter()
        .take(8)
        .enumerate()
        .fold(0, |v, (i, &b)| v | (u64::from(b) << (8 * i)))
}

/// Drives a level-sensitive or edge-triggered interrupt line into the guest.
pub trait Interrupt: Send + Sync {
    /// Sets the line level. For edge-triggered lines, `true` produces one edge.
    fn set_level(&self, level: bool);
}

#[derive(Default)]
pub struct MmioBus {
    /// Sorted by base address; windows never overlap.
    devices: Vec<(u64, u64, Arc<dyn MmioDevice>)>,
}

impl std::fmt::Debug for MmioBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.devices.iter().map(|(b, s, _)| (b, s)))
            .finish()
    }
}

impl MmioBus {
    pub fn insert(&mut self, base: u64, size: u64, device: Arc<dyn MmioDevice>) -> Result<(), String> {
        let end = base.checked_add(size).ok_or("MMIO window wraps")?;
        if self.devices.iter().any(|&(b, s, _)| base < b + s && b < end) {
            return Err(format!(
                "MMIO window {base:#x}+{size:#x} overlaps an existing device"
            ));
        }
        self.devices.push((base, size, device));
        self.devices.sort_by_key(|d| d.0);
        Ok(())
    }

    fn find(&self, addr: u64, len: usize) -> Option<(&dyn MmioDevice, u64)> {
        let i = self.devices.partition_point(|d| d.0 <= addr).checked_sub(1)?;
        let (base, size, dev) = self.devices.get(i)?;
        let off = addr.checked_sub(*base)?;
        (off.checked_add(len as u64)? <= *size).then_some((dev.as_ref(), off))
    }

    /// Returns false if no device claims the access (reads then yield zeros).
    pub fn read(&self, addr: u64, data: &mut [u8]) -> bool {
        match self.find(addr, data.len()) {
            Some((dev, off)) => {
                dev.read(off, data);
                true
            }
            None => {
                data.fill(0);
                false
            }
        }
    }

    pub fn write(&self, addr: u64, data: &[u8]) -> bool {
        match self.find(addr, data.len()) {
            Some((dev, off)) => {
                dev.write(off, data);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<(u64, Vec<u8>)>>);
    impl MmioDevice for Recorder {
        fn read(&self, offset: u64, data: &mut [u8]) {
            data.fill(offset as u8);
        }
        fn write(&self, offset: u64, data: &[u8]) {
            self.0.lock().unwrap().push((offset, data.to_vec()));
        }
    }

    #[test]
    fn routes_by_window_and_rejects_overlap() {
        let a = Arc::new(Recorder::default());
        let mut bus = MmioBus::default();
        bus.insert(0x1000, 0x100, a.clone()).unwrap();
        bus.insert(0x2000, 0x100, Arc::new(Recorder::default())).unwrap();
        assert!(bus.insert(0x10f0, 0x20, Arc::new(Recorder::default())).is_err());
        assert!(
            bus.insert(u64::MAX - 1, 4, Arc::new(Recorder::default()))
                .is_err()
        );

        let mut b = [0u8; 4];
        assert!(bus.read(0x1010, &mut b));
        assert_eq!(b, [0x10; 4]);
        assert!(bus.write(0x10fc, &[1, 2, 3, 4]));
        assert!(!bus.write(0x10fe, &[1, 2, 3, 4])); // straddles the end of the window
        assert!(!bus.read(0x0fff, &mut b));
        assert_eq!(b, [0; 4]);
        assert_eq!(a.0.lock().unwrap()[0], (0xfc, vec![1, 2, 3, 4]));
    }
}
