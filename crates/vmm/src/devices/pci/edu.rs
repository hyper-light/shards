//! A test function with the identification, liveness and interrupt registers of QEMU's edu
//! device (QEMU docs/specs/edu.rst), its IDs 1234:11e8: what a guest reads and raises
//! through the PCI bus, with no driver of its own, to show that its configuration space,
//! its BAR and its interrupts reach the VMM, and through VFIO a guest's own VMs (D153,
//! D154). Its interrupts are INTA#, asserted while its interrupt status is not zero, or one
//! MSI for each raise where the guest enabled MSI, as edu's. Nothing else of edu's: no
//! factorial, no DMA.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use super::{Bar, BarKind, Header, Identity, Interrupts, Lines, MsiCap, PciFunction, reg};
use crate::devices::{MmioDevice, get_le, put_le};
use crate::snapshot::codec::{self, Reader, Writer};

pub const VENDOR: u16 = 0x1234;
pub const DEVICE: u16 = 0x11e8;
/// edu's BAR 0: 1 MiB of memory.
pub const BAR_SIZE: u64 = 1 << 20;
/// The identification register: major version 1, minor 0, and 0xed.
pub const IDENTIFICATION: u32 = 0x0100_00ed;
/// Where its MSI capability is: the first place after the header (§6.7), as QEMU's
/// `msi_init` finds for edu.
pub const MSI_CAP: u16 = 0x40;
/// Its INTx pin: INTA#.
pub const PIN: u8 = 1;

/// Register offsets in BAR 0.
const REG_IDENTIFICATION: u64 = 0x00;
const REG_LIVENESS: u64 = 0x04;
const REG_INTERRUPT_STATUS: u64 = 0x24;
const REG_INTERRUPT_RAISE: u64 = 0x60;
const REG_INTERRUPT_ACK: u64 = 0x64;

/// The function: its configuration header, BAR 0 placed at `bar` by the VMM, and its
/// interrupt status, which its BAR's registers raise and acknowledge.
#[derive(Debug)]
pub struct Edu {
    header: Header,
    status: Mutex<u32>,
    lines: Option<Lines>,
}

impl Edu {
    /// The function and its BAR 0's registers, which the machine maps at `bar`; its
    /// interrupts go to `lines`, and where a machine takes none it has none.
    pub fn new(bar: u64, lines: Option<Lines>) -> Result<(Arc<Edu>, Arc<EduRegisters>), String> {
        let interrupts = match lines {
            Some(_) => Interrupts {
                pin: PIN,
                msi: Some(MsiCap {
                    at: MSI_CAP,
                    next: 0,
                    is64: true,
                }),
            },
            None => Interrupts::default(),
        };
        let header = Header::with_interrupts(
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
            interrupts,
        )?;
        let edu = Arc::new(Edu {
            header,
            status: Mutex::new(0),
            lines,
        });
        Ok((
            edu.clone(),
            Arc::new(EduRegisters {
                edu,
                liveness: AtomicU32::new(0),
            }),
        ))
    }

    /// Changes the interrupt status as `change` says, and gives the guest what the status
    /// asks of the function's interrupts, as edu's `edu_raise_irq` and `edu_lower_irq`: a
    /// message for each `raise` where MSI is on; else INTx pending while the status is not
    /// zero, which the status register's Interrupt Status says whether or not INTx is
    /// disabled, and INTA# asserted where it is not (§6.2.2, §6.2.3; QEMU's
    /// pci_update_irq_status and pci_irq_disabled).
    fn interrupt(&self, change: impl FnOnce(u32) -> u32, raise: bool) {
        let mut status = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        *status = change(*status);
        let Some(lines) = &self.lines else {
            return;
        };
        match self.header.msi() {
            Some((address, data)) => {
                self.header.set_interrupt_status(false);
                lines.intx.set_level(false);
                if raise && *status != 0 {
                    lines.msi.send(address, data);
                }
            }
            None => {
                let pending = *status != 0;
                self.header.set_interrupt_status(pending);
                lines
                    .intx
                    .set_level(pending && self.header.command() & reg::COMMAND_INTX_OFF == 0);
            }
        }
    }
}

impl PciFunction for Edu {
    fn config_read(&self, offset: u16, data: &mut [u8]) {
        self.header.read(offset, data);
    }

    /// A write, and then INTx as the command register and MSI's enable now say.
    fn config_write(&self, offset: u16, data: &[u8]) {
        self.header.write(offset, data);
        self.interrupt(|status| status, false);
    }
}

/// BAR 0: the identification register; the liveness check, which reads back the inverse
/// of what was written (edu.rst: "a simple value inversion"); and the interrupt status,
/// raised and acknowledged; the rest reads 0.
#[derive(Debug)]
pub struct EduRegisters {
    edu: Arc<Edu>,
    liveness: AtomicU32,
}

impl MmioDevice for EduRegisters {
    fn read(&self, offset: u64, data: &mut [u8]) {
        let value = match offset {
            REG_IDENTIFICATION => IDENTIFICATION,
            REG_LIVENESS => !self.liveness.load(Ordering::Relaxed),
            REG_INTERRUPT_STATUS => *self.edu.status.lock().unwrap_or_else(PoisonError::into_inner),
            _ => 0,
        };
        put_le(data, u64::from(value));
    }

    fn write(&self, offset: u64, data: &[u8]) {
        let value = get_le(data) as u32;
        match offset {
            REG_LIVENESS => self.liveness.store(value, Ordering::Relaxed),
            REG_INTERRUPT_RAISE => self.edu.interrupt(|status| status | value, true),
            REG_INTERRUPT_ACK => self.edu.interrupt(|status| status & !value, false),
            _ => {}
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
    use crate::devices::Interrupt;
    use crate::devices::pci::Msi;

    #[derive(Default)]
    struct Recorded {
        levels: Mutex<Vec<bool>>,
        msis: Mutex<Vec<(u64, u32)>>,
    }

    struct Line(Arc<Recorded>);

    impl Interrupt for Line {
        fn set_level(&self, level: bool) {
            self.0.levels.lock().unwrap().push(level);
        }
    }

    impl Msi for Line {
        fn send(&self, address: u64, data: u32) {
            self.0.msis.lock().unwrap().push((address, data));
        }
    }

    fn edu() -> (Arc<Edu>, Arc<EduRegisters>, Arc<Recorded>) {
        let recorded = Arc::new(Recorded::default());
        let lines = Lines {
            intx: Arc::new(Line(recorded.clone())),
            msi: Arc::new(Line(recorded.clone())),
        };
        let (edu, regs) = Edu::new(0x1010_0000, Some(lines)).unwrap();
        (edu, regs, recorded)
    }

    fn word(f: &dyn PciFunction, offset: u16) -> u32 {
        let mut b = [0u8; 4];
        f.config_read(offset, &mut b);
        u32::from_le_bytes(b)
    }

    /// edu's identification and liveness registers, through its BAR; its IDs, BAR, INTx
    /// pin and MSI capability through its configuration space.
    #[test]
    fn edu_says_what_it_is_and_that_it_lives() {
        let (edu, regs, _) = edu();
        assert_eq!(word(&*edu, 0), 0x11e8_1234);
        assert_eq!(word(&*edu, 0x10), 0x1010_0000);
        assert_eq!(word(&*edu, 0x3c) >> 8 & 0xff, 1, "INTA#");
        assert_ne!(word(&*edu, 0x04) >> 16 & u32::from(reg::STATUS_CAPABILITIES), 0);
        assert_eq!(word(&*edu, 0x34) & 0xff, u32::from(MSI_CAP));
        assert_eq!(word(&*edu, MSI_CAP) & 0xff, u32::from(reg::CAP_MSI));
        let mut b = [0u8; 4];
        regs.read(REG_IDENTIFICATION, &mut b);
        assert_eq!(u32::from_le_bytes(b), IDENTIFICATION);
        regs.write(REG_LIVENESS, &0x1234_5678u32.to_le_bytes());
        regs.read(REG_LIVENESS, &mut b);
        assert_eq!(u32::from_le_bytes(b), !0x1234_5678);
    }

    /// A raise asserts INTA# until it is acknowledged, unless the guest disabled INTx; with
    /// MSI enabled, each raise sends its message instead, as edu's, and INTA# stays low.
    #[test]
    fn edu_raises_intx_or_its_msi() {
        let (edu, regs, rec) = edu();
        let status = |regs: &EduRegisters| {
            let mut b = [0u8; 4];
            regs.read(REG_INTERRUPT_STATUS, &mut b);
            u32::from_le_bytes(b)
        };
        regs.write(REG_INTERRUPT_RAISE, &0x5u32.to_le_bytes());
        assert_eq!(status(&regs), 0x5);
        assert_eq!(rec.levels.lock().unwrap().last(), Some(&true));
        assert_ne!(word(&*edu, 0x04) >> 16 & u32::from(reg::STATUS_INTERRUPT), 0);
        regs.write(REG_INTERRUPT_ACK, &0x1u32.to_le_bytes());
        assert_eq!(rec.levels.lock().unwrap().last(), Some(&true), "0x4 still raised");
        regs.write(REG_INTERRUPT_ACK, &0x4u32.to_le_bytes());
        assert_eq!(rec.levels.lock().unwrap().last(), Some(&false));
        // INTx disabled: a raise leaves the line low, and the status register still says
        // the interrupt is pending, as a driver masking INTx asks of it (vfio-pci's
        // pci_check_and_mask_intx); enabled again, the line rises.
        edu.config_write(0x04, &reg::COMMAND_INTX_OFF.to_le_bytes());
        regs.write(REG_INTERRUPT_RAISE, &0x1u32.to_le_bytes());
        assert_eq!(rec.levels.lock().unwrap().last(), Some(&false));
        assert_ne!(word(&*edu, 0x04) >> 16 & u32::from(reg::STATUS_INTERRUPT), 0);
        edu.config_write(0x04, &0u16.to_le_bytes());
        assert_eq!(rec.levels.lock().unwrap().last(), Some(&true));
        regs.write(REG_INTERRUPT_ACK, &0x1u32.to_le_bytes());
        assert_eq!(word(&*edu, 0x04) >> 16 & u32::from(reg::STATUS_INTERRUPT), 0);
        // MSI: address and data written, then enabled; each raise sends one message.
        edu.config_write(MSI_CAP + reg::MSI_ADDRESS, &0x0802_0040u32.to_le_bytes());
        edu.config_write(MSI_CAP + reg::MSI_ADDRESS_HIGH, &0u32.to_le_bytes());
        edu.config_write(MSI_CAP + reg::MSI_DATA, &64u16.to_le_bytes());
        edu.config_write(MSI_CAP + reg::MSI_CONTROL, &reg::MSI_ENABLE.to_le_bytes());
        regs.write(REG_INTERRUPT_RAISE, &0x2u32.to_le_bytes());
        regs.write(REG_INTERRUPT_RAISE, &0x2u32.to_le_bytes());
        regs.write(REG_INTERRUPT_RAISE, &0x8u32.to_le_bytes());
        assert_eq!(*rec.msis.lock().unwrap(), vec![(0x0802_0040, 64); 3]);
        assert_eq!(rec.levels.lock().unwrap().last(), Some(&false));
    }
}
