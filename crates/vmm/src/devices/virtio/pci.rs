//! virtio over PCI (virtio 1.3 §4.1), for the devices plugged into a running machine
//! (D214): a modern, non-transitional function whose registers are those of the device's
//! MMIO transport ([`MmioTransport`]), read and written for it, so that the device's state
//! machine is the one every other virtio device of shards' runs.
//!
//! One memory BAR, 32-bit, holds the common configuration, the ISR status, the device's own
//! configuration, the notifications (each queue's at four times its index) and the MSI-X
//! table and pending bits. MSI-X has two vectors, as Linux then takes one for configuration
//! changes and one its queues share where a device has more than one queue (virtio_pci_
//! common.c, vp_find_vqs_msix); there is no INTx, as a plugged function's slot has no
//! interrupt line its firmware tables route.

use std::sync::{Arc, Mutex, PoisonError};

use super::mmio::{self, MmioTransport};
use super::{DeviceInterrupt, VirtioDevice};
use crate::devices::pci::hotplug::Plugged;
use crate::devices::pci::{
    Bar, BarKind, Header, Identity, Interrupts, Msi, MsixCap, MsixTable, PciFunction, reg,
};
use crate::devices::{Interrupt, MmioDevice, get_le, put_le};
use crate::memory::GuestMemory;

/// The BAR's length and its regions' places in it.
pub const BAR_SIZE: u64 = 0x4000;
const COMMON: u64 = 0x0000;
const COMMON_LEN: u64 = 0x40;
const ISR: u64 = 0x1000;
const DEVICE: u64 = 0x2000;
const DEVICE_LEN: u64 = 0x1000;
const NOTIFY: u64 = 0x3000;
const NOTIFY_LEN: u64 = 0x800;
const NOTIFY_MULTIPLIER: u32 = 4;
const MSIX_TABLE: u64 = 0x3800;
const MSIX_PBA: u64 = 0x3c00;
/// MSI-X vectors: one for configuration changes, one for the queues.
const VECTORS: u16 = 2;
/// A vector register's value for none (virtio 1.3 §4.1.5.1.2).
const NO_VECTOR: u16 = 0xffff;

/// virtio's PCI IDs (virtio 1.3 §4.1.2): Red Hat's vendor ID, and a modern device's ID,
/// 0x1040 plus its virtio device ID; a subsystem ID of at least 0x40.
const VENDOR: u16 = 0x1af4;
const MODERN: u16 = 0x1040;
const SUBSYSTEM: u16 = 0x40;

/// The vendor-specific capability's ID, and virtio's structures' types (§4.1.4).
const CAP_VENDOR: u8 = 0x09;
const CFG_COMMON: u8 = 1;
const CFG_NOTIFY: u8 = 2;
const CFG_ISR: u8 = 3;
const CFG_DEVICE: u8 = 4;

/// Where its capabilities are: MSI-X's, then virtio's common, notification, ISR and
/// device configuration's.
const AT_MSIX: u16 = 0x40;
const AT_COMMON: u16 = 0x50;
const AT_NOTIFY: u16 = 0x60;
const AT_ISR: u16 = 0x78;
const AT_DEVICE: u16 = 0x88;

/// The common configuration's fields (struct virtio_pci_common_cfg, §4.1.4.3).
mod common {
    pub const DEVICE_FEATURE_SELECT: u64 = 0x00;
    pub const DEVICE_FEATURE: u64 = 0x04;
    pub const DRIVER_FEATURE_SELECT: u64 = 0x08;
    pub const DRIVER_FEATURE: u64 = 0x0c;
    pub const CONFIG_MSIX_VECTOR: u64 = 0x10;
    pub const NUM_QUEUES: u64 = 0x12;
    pub const DEVICE_STATUS: u64 = 0x14;
    pub const CONFIG_GENERATION: u64 = 0x15;
    pub const QUEUE_SELECT: u64 = 0x16;
    pub const QUEUE_SIZE: u64 = 0x18;
    pub const QUEUE_MSIX_VECTOR: u64 = 0x1a;
    pub const QUEUE_ENABLE: u64 = 0x1c;
    pub const QUEUE_NOTIFY_OFF: u64 = 0x1e;
    pub const QUEUE_DESC: u64 = 0x20;
    pub const QUEUE_DRIVER: u64 = 0x28;
    pub const QUEUE_DEVICE: u64 = 0x30;
}

/// Which of a queue's rings' addresses an access at `offset` of the common configuration
/// reaches, a half or the whole: the descriptor table's, the driver's area's or the
/// device's.
fn ring(offset: u64) -> Option<usize> {
    match offset & !7 {
        common::QUEUE_DESC => Some(0),
        common::QUEUE_DRIVER => Some(1),
        common::QUEUE_DEVICE => Some(2),
        _ => None,
    }
}

/// What the common configuration keeps of its own: what the MMIO transport's registers
/// cannot read back.
#[derive(Debug, Default)]
struct Common {
    device_feature_select: u32,
    driver_feature_select: u32,
    driver_features: [u32; 2],
    queue_select: u16,
    /// Each queue's size as the driver set it, its most to start with.
    sizes: Vec<u16>,
    /// Each queue's rings' addresses as the driver wrote them.
    rings: Vec<[u64; 3]>,
}

/// Where its interrupts go: its MSI-X table, as its header enables it, and its vectors.
struct Signals {
    header: Header,
    table: Mutex<MsixTable>,
    /// The configuration's vector and each queue's.
    vectors: Mutex<(u16, Vec<u16>)>,
    msi: Arc<dyn Msi>,
}

impl Signals {
    /// Sends vector `k`, or holds it pending where it or the function is masked; nothing
    /// where MSI-X is off, as the function has no INTx.
    fn send(&self, k: u16) {
        let Some(state) = self.header.msix() else {
            return;
        };
        if !state.enabled || k == NO_VECTOR {
            return;
        }
        let mut table = self.table.lock().unwrap_or_else(PoisonError::into_inner);
        let k = usize::from(k);
        match table.message(k).filter(|_| !state.masked) {
            Some((address, data)) => {
                table.set_pending(k, false);
                drop(table);
                self.msi.send(address, data);
            }
            None => table.set_pending(k, true),
        }
    }

    /// Sends what pended of the vectors unmasked now.
    fn flush(&self) {
        let Some(state) = self.header.msix() else {
            return;
        };
        if !state.enabled || state.masked {
            return;
        }
        let mut due = Vec::new();
        {
            let mut table = self.table.lock().unwrap_or_else(PoisonError::into_inner);
            for k in 0..table.vectors() {
                if table.pending(k)
                    && let Some(message) = table.message(k)
                {
                    table.set_pending(k, false);
                    due.push(message);
                }
            }
        }
        for (address, data) in due {
            self.msi.send(address, data);
        }
    }
}

/// The queues' vector: each vector a queue names, once.
struct QueueLine(Arc<Signals>);

impl Interrupt for QueueLine {
    fn set_level(&self, level: bool) {
        if !level {
            return;
        }
        let mut sent: Vec<u16> = {
            let vectors = self.0.vectors.lock().unwrap_or_else(PoisonError::into_inner);
            vectors.1.clone()
        };
        sent.sort_unstable();
        sent.dedup();
        for k in sent {
            self.0.send(k);
        }
    }
}

/// The configuration's vector.
struct ConfigLine(Arc<Signals>);

impl Interrupt for ConfigLine {
    fn set_level(&self, level: bool) {
        if level {
            let k = self.0.vectors.lock().unwrap_or_else(PoisonError::into_inner).0;
            self.0.send(k);
        }
    }
}

/// A virtio device on PCI, plugged into a running machine.
pub struct VirtioPci {
    signals: Arc<Signals>,
    transport: MmioTransport,
    common: Mutex<Common>,
    max_sizes: Vec<u16>,
}

impl std::fmt::Debug for VirtioPci {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtioPci").finish_non_exhaustive()
    }
}

/// A virtio capability (struct virtio_pci_cap, §4.1.4): of `kind`, at `offset` of BAR 0,
/// `len` bytes; `next` the capability after it; a notification's with its multiplier.
fn capability(next: u16, kind: u8, offset: u64, len: u64, multiplier: Option<u32>) -> Vec<u8> {
    let cap_len: u8 = if multiplier.is_some() { 20 } else { 16 };
    let mut c = vec![CAP_VENDOR, next as u8, cap_len, kind, 0, 0, 0, 0];
    c.extend_from_slice(&(offset as u32).to_le_bytes());
    c.extend_from_slice(&(len as u32).to_le_bytes());
    if let Some(m) = multiplier {
        c.extend_from_slice(&m.to_le_bytes());
    }
    c
}

/// A device's PCI class (base, sub-class, interface), as QEMU gives its virtio functions:
/// a disk's mass storage (SCSI's), a file system's other mass storage.
fn class(device_id: u32) -> u32 {
    match device_id {
        super::block::DEVICE_ID => 0x01_0000,
        super::fs::DEVICE_ID => 0x01_8000,
        _ => 0xff_0000,
    }
}

impl VirtioPci {
    /// `device` on PCI, reaching `memory`, its MSIs sent through `msi`.
    pub fn new(
        device: Box<dyn VirtioDevice>,
        memory: Arc<GuestMemory>,
        msi: Arc<dyn Msi>,
    ) -> Result<VirtioPci, String> {
        let device_id = device.device_id();
        let id = Identity {
            vendor: VENDOR,
            device: MODERN
                .checked_add(u16::try_from(device_id).map_err(|_| format!("virtio device {device_id}"))?)
                .ok_or_else(|| format!("virtio device {device_id}"))?,
            class: class(device_id),
            revision: 1,
            subsystem_vendor: VENDOR,
            subsystem: SUBSYSTEM,
        };
        let bar = Bar {
            index: 0,
            addr: 0,
            size: BAR_SIZE,
            kind: BarKind::Memory32,
        };
        let interrupts = Interrupts {
            pin: 0,
            msi: None,
            msix: Some(MsixCap {
                at: AT_MSIX,
                next: AT_COMMON as u8,
                size: VECTORS,
                table: (0, MSIX_TABLE as u32),
                pba: (0, MSIX_PBA as u32),
            }),
        };
        let header = Header::with_interrupts(id, vec![bar], interrupts)?
            .movable()
            .with_capability(
                AT_COMMON,
                &capability(AT_NOTIFY, CFG_COMMON, COMMON, COMMON_LEN, None),
            )?
            .with_capability(
                AT_NOTIFY,
                &capability(AT_ISR, CFG_NOTIFY, NOTIFY, NOTIFY_LEN, Some(NOTIFY_MULTIPLIER)),
            )?
            .with_capability(AT_ISR, &capability(AT_DEVICE, CFG_ISR, ISR, 1, None))?
            .with_capability(AT_DEVICE, &capability(0, CFG_DEVICE, DEVICE, DEVICE_LEN, None))?;
        let max_sizes = device.queue_max_sizes().to_vec();
        if max_sizes.len() as u64 * u64::from(NOTIFY_MULTIPLIER) > NOTIFY_LEN {
            return Err(format!("a virtio device of {} queues", max_sizes.len()));
        }
        let signals = Arc::new(Signals {
            header,
            table: Mutex::new(MsixTable::new(VECTORS)),
            vectors: Mutex::new((NO_VECTOR, vec![NO_VECTOR; max_sizes.len()])),
            msi,
        });
        let interrupt = DeviceInterrupt::with_config_line(
            Arc::new(QueueLine(signals.clone())),
            Arc::new(ConfigLine(signals.clone())),
        );
        let transport = MmioTransport::with_interrupt(device, memory, interrupt);
        let common = Common {
            sizes: max_sizes.clone(),
            rings: vec![[0; 3]; max_sizes.len()],
            ..Common::default()
        };
        Ok(VirtioPci {
            signals,
            transport,
            common: Mutex::new(common),
            max_sizes,
        })
    }

    fn mmio_read(&self, register: u64) -> u32 {
        let mut b = [0u8; 4];
        self.transport.read(register, &mut b);
        u32::from_le_bytes(b)
    }

    fn mmio_write(&self, register: u64, value: u32) {
        self.transport.write(register, &value.to_le_bytes());
    }

    /// Its device status, as the driver last set it (§2.1).
    pub fn status(&self) -> u32 {
        self.mmio_read(mmio::STATUS)
    }

    fn common_read(&self, offset: u64, data: &mut [u8]) {
        let c = self.common.lock().unwrap_or_else(PoisonError::into_inner);
        let q = usize::from(c.queue_select);
        let vectors = || {
            self.signals
                .vectors
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
        };
        let value: u64 = match (offset, data.len()) {
            (common::DEVICE_FEATURE_SELECT, 4) => u64::from(c.device_feature_select),
            (common::DEVICE_FEATURE, 4) => u64::from(self.mmio_read(mmio::DEVICE_FEATURES)),
            (common::DRIVER_FEATURE_SELECT, 4) => u64::from(c.driver_feature_select),
            (common::DRIVER_FEATURE, 4) => c
                .driver_features
                .get(c.driver_feature_select as usize)
                .copied()
                .map_or(0, u64::from),
            (common::CONFIG_MSIX_VECTOR, 2) => u64::from(vectors().0),
            (common::NUM_QUEUES, 2) => self.max_sizes.len() as u64,
            (common::DEVICE_STATUS, 1) => u64::from(self.mmio_read(mmio::STATUS) as u8),
            (common::CONFIG_GENERATION, 1) => u64::from(self.mmio_read(mmio::CONFIG_GENERATION) as u8),
            (common::QUEUE_SELECT, 2) => u64::from(c.queue_select),
            (common::QUEUE_SIZE, 2) => c.sizes.get(q).copied().map_or(0, u64::from),
            (common::QUEUE_MSIX_VECTOR, 2) => vectors()
                .1
                .get(q)
                .copied()
                .map_or(u64::from(NO_VECTOR), u64::from),
            (common::QUEUE_ENABLE, 2) => u64::from(self.mmio_read(mmio::QUEUE_READY)),
            (common::QUEUE_NOTIFY_OFF, 2) => q as u64,
            (common::QUEUE_DESC..=0x37, 4 | 8) => {
                let field = ring(offset).unwrap_or(usize::MAX);
                let ring = c.rings.get(q).and_then(|r| r.get(field)).copied().unwrap_or(0);
                if offset % 8 == 4 { ring >> 32 } else { ring }
            }
            _ => 0,
        };
        put_le(data, value);
    }

    fn common_write(&self, offset: u64, data: &[u8]) {
        let value = get_le(data);
        let mut c = self.common.lock().unwrap_or_else(PoisonError::into_inner);
        let q = usize::from(c.queue_select);
        // A vector the table has, or none.
        let vector = |v: u64| match u16::try_from(v) {
            Ok(v) if v < VECTORS => v,
            _ => NO_VECTOR,
        };
        match (offset, data.len()) {
            (common::DEVICE_FEATURE_SELECT, 4) => {
                c.device_feature_select = value as u32;
                self.mmio_write(mmio::DEVICE_FEATURES_SEL, value as u32);
            }
            (common::DRIVER_FEATURE_SELECT, 4) => {
                c.driver_feature_select = value as u32;
                self.mmio_write(mmio::DRIVER_FEATURES_SEL, value as u32);
            }
            (common::DRIVER_FEATURE, 4) => {
                let sel = c.driver_feature_select as usize;
                if let Some(f) = c.driver_features.get_mut(sel) {
                    *f = value as u32;
                }
                self.mmio_write(mmio::DRIVER_FEATURES, value as u32);
            }
            (common::CONFIG_MSIX_VECTOR, 2) => {
                self.signals
                    .vectors
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .0 = vector(value);
            }
            (common::DEVICE_STATUS, 1) => {
                self.mmio_write(mmio::STATUS, value as u32);
                if value == 0 {
                    // A reset (§4.1.4.3.1): the transport's, and what is kept here.
                    *c = Common {
                        sizes: self.max_sizes.clone(),
                        rings: vec![[0; 3]; self.max_sizes.len()],
                        ..Common::default()
                    };
                    *self
                        .signals
                        .vectors
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner) =
                        (NO_VECTOR, vec![NO_VECTOR; self.max_sizes.len()]);
                }
            }
            (common::QUEUE_SELECT, 2) => {
                c.queue_select = value as u16;
                self.mmio_write(mmio::QUEUE_SEL, value as u32);
            }
            (common::QUEUE_SIZE, 2) => {
                let most = self.max_sizes.get(q).copied().unwrap_or(0);
                if let Some(size) = c.sizes.get_mut(q) {
                    *size = (value as u16).min(most);
                }
            }
            (common::QUEUE_MSIX_VECTOR, 2) => {
                if let Some(v) = self
                    .signals
                    .vectors
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .1
                    .get_mut(q)
                {
                    *v = vector(value);
                }
            }
            (common::QUEUE_ENABLE, 2) if value == 1 => {
                let size = c.sizes.get(q).copied().unwrap_or(0);
                let rings = c.rings.get(q).copied().unwrap_or_default();
                self.mmio_write(mmio::QUEUE_NUM, u32::from(size));
                for (register, ring) in [
                    mmio::QUEUE_DESC_LOW,
                    mmio::QUEUE_DRIVER_LOW,
                    mmio::QUEUE_DEVICE_LOW,
                ]
                .into_iter()
                .zip(rings)
                {
                    self.mmio_write(register, ring as u32);
                    self.mmio_write(register + 4, (ring >> 32) as u32);
                }
                self.mmio_write(mmio::QUEUE_READY, 1);
            }
            (common::QUEUE_DESC..=0x37, 4 | 8) => {
                let field = ring(offset).unwrap_or(usize::MAX);
                if let Some(ring) = c.rings.get_mut(q).and_then(|r| r.get_mut(field)) {
                    *ring = match (offset % 8, data.len()) {
                        (0, 8) => value,
                        (0, _) => (*ring & !0xffff_ffff) | (value & 0xffff_ffff),
                        _ => (*ring & 0xffff_ffff) | (value << 32),
                    };
                }
            }
            _ => {}
        }
    }
}

impl PciFunction for VirtioPci {
    fn config_read(&self, offset: u16, data: &mut [u8]) {
        self.signals.header.read(offset, data);
    }

    fn config_write(&self, offset: u16, data: &[u8]) {
        self.signals.header.write(offset, data);
        // MSI-X enabled or unmasked as a whole: what pended goes.
        if let Some(cap) = self.signals.header.msix_cap()
            && (cap.at..cap.at + reg::MSIX_LEN).contains(&offset)
        {
            self.signals.flush();
        }
    }
}

impl Plugged for VirtioPci {
    fn claims(&self, addr: u64, len: u64) -> Option<(usize, u64)> {
        let header = &self.signals.header;
        if header.command() & reg::COMMAND_MEMORY == 0 {
            return None;
        }
        let base = header.bar_addr(0).filter(|&b| b != 0)?;
        let offset = addr.checked_sub(base)?;
        (offset.checked_add(len)? <= BAR_SIZE).then_some((0, offset))
    }

    fn bar_read(&self, _bar: usize, offset: u64, data: &mut [u8]) {
        match offset {
            COMMON..0x40 => self.common_read(offset - COMMON, data),
            // Reading the ISR status clears it (§4.1.4.5).
            ISR => {
                let status = self.mmio_read(mmio::INTERRUPT_STATUS);
                self.mmio_write(mmio::INTERRUPT_ACK, status);
                put_le(data, u64::from(status));
            }
            DEVICE..0x3000 => self.transport.read(mmio::CONFIG + (offset - DEVICE), data),
            MSIX_TABLE..MSIX_PBA => self
                .signals
                .table
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .read(offset - MSIX_TABLE, data),
            MSIX_PBA..BAR_SIZE => self
                .signals
                .table
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .read_pending(offset - MSIX_PBA, data),
            _ => data.fill(0),
        }
    }

    fn bar_write(&self, _bar: usize, offset: u64, data: &[u8]) {
        match offset {
            COMMON..0x40 => self.common_write(offset - COMMON, data),
            DEVICE..0x3000 => self.transport.write(mmio::CONFIG + (offset - DEVICE), data),
            // A queue's notification, at its index times the multiplier (§4.1.4.4).
            NOTIFY..MSIX_TABLE => {
                let queue = (offset - NOTIFY) / u64::from(NOTIFY_MULTIPLIER);
                if (queue as usize) < self.max_sizes.len() {
                    self.mmio_write(mmio::QUEUE_NOTIFY, queue as u32);
                }
            }
            MSIX_TABLE..MSIX_PBA => {
                let written = self
                    .signals
                    .table
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .write(offset - MSIX_TABLE, data);
                if written.is_some() {
                    self.signals.flush();
                }
            }
            _ => {}
        }
    }

    fn held(&self) -> Option<String> {
        let status = self.status();
        (status != 0).then(|| {
            format!(
                "its guest's driver still drives it (device status {status:#x}): its guest lets it go first (`remove` in sysfs)"
            )
        })
    }

    fn pause(&self) {
        self.transport.pause();
    }

    fn resume(&self) -> Result<(), String> {
        self.transport.resume()
    }

    fn unplugged(&self) {
        // A reset stops its device's workers; it touches guest memory no more.
        self.mmio_write(mmio::STATUS, 0);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    use crate::devices::virtio::queue::QueueState;
    use crate::devices::virtio::{Activation, ShmRegion};

    struct Nothing;

    impl VirtioDevice for Nothing {
        fn device_id(&self) -> u32 {
            2
        }
        fn features(&self) -> u64 {
            super::super::feature::VERSION_1 | 1 << 9
        }
        fn queue_max_sizes(&self) -> &[u16] {
            &[256]
        }
        fn read_config(&self, offset: u64, data: &mut [u8]) {
            put_le(data, 0x1122_3344_5566_7788 >> (8 * offset));
        }
        fn write_config(&mut self, _: u64, _: &[u8]) {}
        fn shm_regions(&self) -> &[ShmRegion] {
            &[]
        }
        fn activate(&mut self, _: Activation) -> Result<(), String> {
            Ok(())
        }
        fn notify(&self, _: u16) {}
        fn reset(&mut self) {}
        fn pause(&mut self) -> Vec<QueueState> {
            Vec::new()
        }
        fn resume(&mut self) -> Result<(), String> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct Sent(Mutex<Vec<(u64, u32)>>);

    impl Msi for Sent {
        fn send(&self, address: u64, data: u32) {
            self.0.lock().unwrap().push((address, data));
        }
    }

    fn cfg(f: &VirtioPci, offset: u16, len: usize) -> u64 {
        let mut b = vec![0u8; len];
        f.config_read(offset, &mut b);
        get_le(&b)
    }

    fn bar(f: &VirtioPci, offset: u64, len: usize) -> u64 {
        let mut b = vec![0u8; len];
        f.bar_read(0, offset, &mut b);
        get_le(&b)
    }

    fn put(f: &VirtioPci, offset: u64, value: u64, len: usize) {
        let mut b = vec![0u8; len];
        put_le(&mut b, value);
        f.bar_write(0, offset, &b);
    }

    /// The function says it is a modern virtio disk, its BAR placed where the guest writes
    /// it, its capability list leading from MSI-X through virtio's four structures; the
    /// driver's setup reaches the device's state machine; its configuration change goes to
    /// the configuration's vector, held while masked and sent as unmasked.
    #[test]
    fn a_virtio_function_is_found_set_up_and_signals_by_msix() {
        let memory = Arc::new(GuestMemory::anonymous(&[(0x8000_0000, 1 << 20)]).unwrap());
        let sent = Arc::new(Sent::default());
        let f = VirtioPci::new(Box::new(Nothing), memory, sent.clone()).unwrap();
        assert_eq!(cfg(&f, reg::VENDOR, 4), 0x1042_1af4);
        assert_eq!(cfg(&f, reg::CAPABILITIES, 1), u64::from(AT_MSIX));
        // The list: MSI-X, common, notify, ISR, device, each its type.
        let mut at = cfg(&f, reg::CAPABILITIES, 1) as u16;
        let mut kinds = Vec::new();
        while at != 0 {
            let id = cfg(&f, at, 1) as u8;
            kinds.push(if id == CAP_VENDOR {
                cfg(&f, at + 3, 1) as u8
            } else {
                id
            });
            at = cfg(&f, at + 1, 1) as u16;
        }
        assert_eq!(
            kinds,
            [reg::CAP_MSIX, CFG_COMMON, CFG_NOTIFY, CFG_ISR, CFG_DEVICE]
        );
        // Its BAR sized, then placed by the guest; decoded once memory space is on.
        f.config_write(reg::BAR0, &u32::MAX.to_le_bytes());
        assert_eq!(cfg(&f, reg::BAR0, 4), !(BAR_SIZE - 1) & 0xffff_ffff);
        f.config_write(reg::BAR0, &0x1234_4000u32.to_le_bytes());
        assert_eq!(cfg(&f, reg::BAR0, 4), 0x1234_4000);
        assert_eq!(f.claims(0x1234_4000, 4), None, "memory space off");
        f.config_write(reg::COMMAND, &reg::COMMAND_MEMORY.to_le_bytes());
        assert_eq!(f.claims(0x1234_4010, 4), Some((0, 0x10)));
        assert_eq!(f.claims(0x1234_8000, 4), None);
        // The driver's setup (§3.1.1).
        put(&f, common::DEVICE_STATUS, 1 | 2, 1);
        put(&f, common::DEVICE_FEATURE_SELECT, 1, 4);
        assert_eq!(bar(&f, common::DEVICE_FEATURE, 4), 1, "VERSION_1");
        put(&f, common::DRIVER_FEATURE_SELECT, 1, 4);
        put(&f, common::DRIVER_FEATURE, 1, 4);
        put(&f, common::DEVICE_STATUS, 1 | 2 | 8, 1);
        assert_eq!(bar(&f, common::DEVICE_STATUS, 1), 1 | 2 | 8, "FEATURES_OK kept");
        assert_eq!(bar(&f, common::NUM_QUEUES, 2), 1);
        assert_eq!(bar(&f, common::QUEUE_SIZE, 2), 256);
        put(&f, common::QUEUE_SIZE, 128, 2);
        assert_eq!(bar(&f, common::QUEUE_SIZE, 2), 128);
        put(&f, common::CONFIG_MSIX_VECTOR, 0, 2);
        put(&f, common::QUEUE_MSIX_VECTOR, 5, 2);
        assert_eq!(
            bar(&f, common::QUEUE_MSIX_VECTOR, 2),
            u64::from(NO_VECTOR),
            "past the table"
        );
        put(&f, common::QUEUE_MSIX_VECTOR, 1, 2);
        assert_eq!(bar(&f, common::QUEUE_MSIX_VECTOR, 2), 1);
        put(&f, common::QUEUE_DESC, 0x8000_0000, 4);
        put(&f, common::QUEUE_DESC + 4, 0, 4);
        put(&f, common::QUEUE_DRIVER, 0x8000_1000, 8);
        put(&f, common::QUEUE_DEVICE, 0x8000_2000, 8);
        assert_eq!(bar(&f, common::QUEUE_DRIVER, 8), 0x8000_1000);
        put(&f, common::QUEUE_ENABLE, 1, 2);
        assert_eq!(bar(&f, common::QUEUE_ENABLE, 2), 1);
        put(&f, common::DEVICE_STATUS, 1 | 2 | 8 | 4, 1);
        assert_eq!(f.status(), 1 | 2 | 8 | 4);
        assert!(f.held().is_some(), "driven");
        assert_eq!(bar(&f, DEVICE, 4), 0x5566_7788);
        // MSI-X on, the configuration's vector programmed but masked: held, then sent.
        f.config_write(AT_MSIX + reg::MSIX_CONTROL, &reg::MSIX_ENABLE.to_le_bytes());
        put(&f, MSIX_TABLE, 0xfee0_0000, 8);
        put(&f, MSIX_TABLE + 8, 0x41, 4);
        f.transport_config_change();
        assert!(sent.0.lock().unwrap().is_empty(), "masked");
        assert_eq!(bar(&f, MSIX_PBA, 8), 1);
        put(&f, MSIX_TABLE + 12, 0, 4);
        assert_eq!(*sent.0.lock().unwrap(), [(0xfee0_0000, 0x41)]);
        assert_eq!(bar(&f, MSIX_PBA, 8), 0);
        // A reset lets it go.
        put(&f, common::DEVICE_STATUS, 0, 1);
        assert!(f.held().is_none());
        assert_eq!(bar(&f, common::QUEUE_SIZE, 2), 256);
    }

    impl VirtioPci {
        fn transport_config_change(&self) {
            ConfigLine(self.signals.clone()).set_level(true);
        }
    }
}
