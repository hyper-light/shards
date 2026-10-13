//! A test function with the identification and liveness registers of QEMU's edu device
//! (QEMU docs/specs/edu.rst), its IDs 1234:11e8: what a guest reads through the PCI bus,
//! with no driver of its own, to show that its configuration space and its BAR reach the
//! VMM (D153). Nothing else of edu's: no factorial, no interrupt, no DMA.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use super::{Bar, BarKind, Header, Identity, PciFunction};
use crate::devices::{MmioDevice, get_le, put_le};
use crate::snapshot::codec::{self, Reader, Writer};

pub const VENDOR: u16 = 0x1234;
pub const DEVICE: u16 = 0x11e8;
/// edu's BAR 0: 1 MiB of memory.
pub const BAR_SIZE: u64 = 1 << 20;
/// The identification register: major version 1, minor 0, and 0xed.
pub const IDENTIFICATION: u32 = 0x0100_00ed;

/// Register offsets in BAR 0.
const REG_IDENTIFICATION: u64 = 0x00;
const REG_LIVENESS: u64 = 0x04;

/// The function: its configuration header, BAR 0 placed at `bar` by the VMM.
#[derive(Debug)]
pub struct Edu {
    header: Header,
}

impl Edu {
    /// The function and its BAR 0's registers, which the machine maps at `bar`.
    pub fn new(bar: u64) -> Result<(Arc<Edu>, Arc<EduRegisters>), String> {
        let header = Header::new(
            Identity {
                vendor: VENDOR,
                device: DEVICE,
                // Base class 0xff, "device does not fit any defined class", as edu's.
                class: 0xff_00_00,
                revision: 0x10,
                subsystem_vendor: 0,
                subsystem: 0,
            },
            vec![Bar {
                index: 0,
                addr: bar,
                size: BAR_SIZE,
                kind: BarKind::Memory32,
            }],
        )?;
        Ok((
            Arc::new(Edu { header }),
            Arc::new(EduRegisters {
                liveness: AtomicU32::new(0),
            }),
        ))
    }
}

impl PciFunction for Edu {
    fn config_read(&self, offset: u16, data: &mut [u8]) {
        self.header.read(offset, data);
    }

    fn config_write(&self, offset: u16, data: &[u8]) {
        self.header.write(offset, data);
    }
}

/// BAR 0: the identification register, and the liveness check, which reads back the
/// inverse of what was written (edu.rst: "a simple value inversion"); the rest reads 0.
#[derive(Debug)]
pub struct EduRegisters {
    liveness: AtomicU32,
}

impl MmioDevice for EduRegisters {
    fn read(&self, offset: u64, data: &mut [u8]) {
        let value = match offset {
            REG_IDENTIFICATION => IDENTIFICATION,
            REG_LIVENESS => !self.liveness.load(Ordering::Relaxed),
            _ => 0,
        };
        put_le(data, u64::from(value));
    }

    fn write(&self, offset: u64, data: &[u8]) {
        if offset == REG_LIVENESS {
            self.liveness.store(get_le(data) as u32, Ordering::Relaxed);
        }
    }

    fn pause(&self) {}

    fn resume(&self) -> Result<(), String> {
        Ok(())
    }

    fn save(&self, _: &mut Writer) {}

    fn restore(&self, _: &mut Reader<'_>) -> codec::Result<()> {
        Ok(())
    }

    fn unsaved(&self) -> Option<String> {
        Some("its PCI functions' state is their devices' own".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// edu's identification and liveness registers, through its BAR; its IDs and BAR
    /// through its configuration space.
    #[test]
    fn edu_says_what_it_is_and_that_it_lives() {
        let (edu, regs) = Edu::new(0x1010_0000).unwrap();
        let mut b = [0u8; 4];
        edu.config_read(0, &mut b);
        assert_eq!(u32::from_le_bytes(b), 0x11e8_1234);
        edu.config_read(0x10, &mut b);
        assert_eq!(u32::from_le_bytes(b), 0x1010_0000);
        regs.read(REG_IDENTIFICATION, &mut b);
        assert_eq!(u32::from_le_bytes(b), IDENTIFICATION);
        regs.write(REG_LIVENESS, &0x1234_5678u32.to_le_bytes());
        regs.read(REG_LIVENESS, &mut b);
        assert_eq!(u32::from_le_bytes(b), !0x1234_5678);
    }
}
