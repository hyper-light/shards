//! Emulated devices and the MMIO bus that routes trapped guest accesses to them.

pub mod acpi_sleep;
pub mod control;
pub mod i8042;
pub mod power;
pub mod rtc;
pub mod serial;
pub mod virtio;
pub mod vmgenid;

use std::sync::Arc;

use crate::snapshot::codec::{self, Reader, Writer};

/// A device occupying a guest-physical MMIO window. Accesses arrive from vCPU threads
/// concurrently, so devices synchronize internally.
///
/// Every device takes part in snapshots, so none can silently lose state.
pub trait MmioDevice: Send + Sync {
    /// Fills `data` (1, 2, 4 or 8 bytes, little-endian) from register `offset`.
    fn read(&self, offset: u64, data: &mut [u8]);
    fn write(&self, offset: u64, data: &[u8]);
    /// Stops background work at a clean boundary, with vCPUs already stopped, so the
    /// device's state and the guest memory it touches hold still for a snapshot.
    fn pause(&self);
    /// Continues after `pause`.
    fn resume(&self) -> Result<(), String>;
    /// Appends the device's state. The device is paused.
    fn save(&self, w: &mut Writer);
    /// Loads state that `save` wrote into a device built from the same configuration,
    /// then continues as if resumed.
    fn restore(&self, r: &mut Reader<'_>) -> codec::Result<()>;
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

/// Snapshots cover every device, in address order: the order a restored machine, built
/// from the same configuration, has too.
impl MmioBus {
    pub fn pause(&self) {
        self.devices.iter().for_each(|(_, _, d)| d.pause());
    }

    pub fn resume(&self) -> Result<(), String> {
        self.devices.iter().try_for_each(|(_, _, d)| d.resume())
    }

    pub fn save(&self, w: &mut Writer) {
        w.u32(self.devices.len() as u32);
        for (base, _, device) in &self.devices {
            w.u64(*base);
            device.save(w);
        }
    }

    pub fn restore(&self, r: &mut Reader<'_>) -> codec::Result<()> {
        let count = r.u32()? as usize;
        if count != self.devices.len() {
            return Err(codec::DecodeError(format!(
                "snapshot has {count} devices; this machine has {}",
                self.devices.len()
            )));
        }
        for (base, _, device) in &self.devices {
            let saved = r.u64()?;
            if saved != *base {
                return Err(codec::DecodeError(format!(
                    "snapshot device at {saved:#x}; this machine has one at {base:#x}"
                )));
            }
            device.restore(r)?;
        }
        Ok(())
    }
}

/// The guest's MMIO accesses. Accesses no device claims read as zero and are logged.
impl crate::hv::Io for MmioBus {
    fn mmio_read(&self, addr: u64, data: &mut [u8]) {
        if !self.read(addr, data) {
            crate::debug!("unclaimed MMIO read {addr:#x} ({} bytes)", data.len());
        }
    }

    fn mmio_write(&self, addr: u64, data: &[u8]) {
        if !self.write(addr, data) {
            crate::debug!(
                "unclaimed MMIO write {addr:#x} <- {:#x} ({} bytes)",
                get_le(data),
                data.len()
            );
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
        fn pause(&self) {}
        fn resume(&self) -> Result<(), String> {
            Ok(())
        }
        fn save(&self, w: &mut Writer) {
            let writes = self.0.lock().unwrap();
            w.seq(&writes, |w, (offset, data)| {
                w.u64(*offset);
                w.bytes(data);
            });
        }
        fn restore(&self, r: &mut Reader<'_>) -> codec::Result<()> {
            let writes = r.seq(64, 12, |r| Ok((r.u64()?, r.bytes(8)?.to_vec())))?;
            *self.0.lock().unwrap() = writes;
            Ok(())
        }
    }

    #[test]
    fn snapshots_restore_into_the_same_layout_only() {
        let bus = |bases: &[u64]| {
            let mut bus = MmioBus::default();
            let devices: Vec<_> = bases.iter().map(|_| Arc::new(Recorder::default())).collect();
            for (&base, d) in bases.iter().zip(&devices) {
                bus.insert(base, 0x100, d.clone()).unwrap();
            }
            (bus, devices)
        };
        let (a, a_devs) = bus(&[0x2000, 0x1000]);
        a.write(0x1004, &[1, 2]);
        a.write(0x2008, &[3]);
        let mut w = Writer::default();
        a.save(&mut w);
        let saved = w.into_bytes();

        let (b, b_devs) = bus(&[0x1000, 0x2000]);
        let mut r = Reader::new(&saved);
        b.restore(&mut r).unwrap();
        r.finish().unwrap();
        // Address order, whatever the insertion order.
        assert_eq!(*b_devs[0].0.lock().unwrap(), *a_devs[1].0.lock().unwrap());
        assert_eq!(*b_devs[1].0.lock().unwrap(), *a_devs[0].0.lock().unwrap());

        let (c, _) = bus(&[0x1000, 0x3000]);
        assert!(c.restore(&mut Reader::new(&saved)).is_err());
        let (d, _) = bus(&[0x1000]);
        assert!(d.restore(&mut Reader::new(&saved)).is_err());
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
