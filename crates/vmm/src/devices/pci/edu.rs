//! A test function with the identification, liveness and interrupt registers of QEMU's edu
//! device (QEMU docs/specs/edu.rst), its IDs 1234:11e8: what a guest reads and raises
//! through the PCI bus, with no driver of its own, to show that its configuration space,
//! its BARs and its interrupts reach the VMM, and through VFIO a guest's own VMs (D153,
//! D154). Its interrupts are INTA#, asserted while its interrupt status is not zero, or one
//! MSI for each raise where the guest enabled MSI, as edu's; and, of shards' own, as edu
//! has none, an MSI-X capability of two vectors, a raise of bit k sending vector k, its
//! table and pending bits in BAR 2. Its DMA engine is edu's: its buffer on one side, guest
//! memory on the other, by its requester ID's domain where the machine has an IOMMU (D154).
//! Nothing else of edu's: no factorial.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use super::{
    Bar, BarKind, Header, Identity, Interrupts, Lines, MsiCap, MsixCap, MsixTable, PciFunction, reg,
};
use crate::devices::virtio::iommu::Shared;
use crate::devices::{MmioDevice, get_le, put_le};
use crate::memory::GuestMemory;
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
/// Its MSI-X capability, after MSI's: two vectors, their table at the start of BAR 2 and
/// their pending bits halfway into it.
pub const MSIX_CAP: u16 = 0x50;
pub const MSIX_VECTORS: u16 = 2;
pub const MSIX_BAR_SIZE: u64 = 4096;
const MSIX_BAR: u8 = 2;
const MSIX_PBA: u64 = 0x800;
/// Its BAR 4, of shards' own as its MSI-X is: 1 MiB of 64-bit memory, which the machine
/// places in its 64-bit window, its first page a scratch the guest reads back.
pub const SCRATCH_BAR: usize = 4;
pub const SCRATCH_BAR_SIZE: u64 = 1 << 20;
const SCRATCH_DWORDS: usize = 1024;
/// Its INTx pin: INTA#.
pub const PIN: u8 = 1;

/// Register offsets in BAR 0.
const REG_IDENTIFICATION: u64 = 0x00;
const REG_LIVENESS: u64 = 0x04;
const REG_INTERRUPT_STATUS: u64 = 0x24;
const REG_INTERRUPT_RAISE: u64 = 0x60;
const REG_INTERRUPT_ACK: u64 = 0x64;
/// Its DMA engine's registers, 64 bits each (edu.rst): the transfer's source, destination
/// and length, and its command: run (bit 0, cleared once done), from its buffer to guest
/// memory rather than to it (bit 1), and raise interrupt 0x100 once done (bit 2).
const REG_DMA: u64 = 0x80;
const DMA_REGISTERS: usize = 4;
const DMA_COMMAND: usize = 3;
const DMA_RUN: u64 = 1 << 0;
const DMA_TO_MEMORY: u64 = 1 << 1;
const DMA_RAISE: u64 = 1 << 2;
const DMA_INTERRUPT: u32 = 0x100;
/// Its buffer, the edu side of a transfer: 4 KiB at 0x40000 of its DMA addresses.
pub const DMA_BUFFER: u64 = 0x40000;
const DMA_BUFFER_SIZE: usize = 4096;

/// How the function's DMA reaches guest memory: the memory, and the IOMMU its requester ID
/// is translated by, with that ID, where the machine has one (D154).
#[derive(Debug, Clone)]
pub struct Dma {
    pub memory: Arc<GuestMemory>,
    pub iommu: Option<(Shared, u32)>,
}

/// Its DMA engine: its registers and its buffer.
#[derive(Debug)]
struct Engine {
    registers: [u64; DMA_REGISTERS],
    buffer: Vec<u8>,
}

/// The function: its configuration header, BAR 0 placed at `bar` by the VMM, and its
/// interrupt status, which its BAR's registers raise and acknowledge; its MSI-X table and
/// pending bits.
#[derive(Debug)]
pub struct Edu {
    header: Header,
    status: Mutex<u32>,
    msix: Mutex<MsixTable>,
    lines: Option<Lines>,
    /// Whether it has an INTx pin: of its device's functions, its function 0 alone, as
    /// they would share its device's line.
    intx: bool,
    engine: Mutex<Engine>,
    dma: Option<Dma>,
}

/// What of a device a function is: one of several, a multi-function device's (its
/// header's bit 7); and whether it has the device's INTx pin.
#[derive(Debug, Clone, Copy)]
pub struct Role {
    pub multi_function: bool,
    pub intx: bool,
}

impl Role {
    /// A device's only function.
    pub const ALONE: Role = Role {
        multi_function: false,
        intx: true,
    };
}

/// Where the machine maps the function's BARs: BAR 0's registers, BAR 2's MSI-X table and
/// BAR 4's scratch, in its 64-bit window.
#[derive(Debug, Clone, Copy)]
pub struct Places {
    pub registers: u64,
    pub msix: u64,
    pub scratch: u64,
}

/// The function's BARs, for the machine to map where [`Places`] says.
#[derive(Debug)]
pub struct Bars {
    pub registers: Arc<EduRegisters>,
    pub msix: Arc<EduMsix>,
    pub scratch: Arc<EduScratch>,
}

impl Edu {
    /// The function and its BARs, which the machine maps at `at`; its interrupts go to
    /// `lines`, and where a machine takes none it has none; its DMA goes as `dma` says,
    /// and without it nowhere.
    pub fn new(at: Places, lines: Option<Lines>, dma: Option<Dma>) -> Result<(Arc<Edu>, Bars), String> {
        Edu::of(at, lines, dma, Role::ALONE)
    }

    /// [`Edu::new`], the function of its device that `role` says.
    pub fn of(
        at: Places,
        lines: Option<Lines>,
        dma: Option<Dma>,
        role: Role,
    ) -> Result<(Arc<Edu>, Bars), String> {
        let interrupts = match lines {
            Some(_) => Interrupts {
                pin: if role.intx { PIN } else { 0 },
                msi: Some(MsiCap {
                    at: MSI_CAP,
                    next: MSIX_CAP as u8,
                    is64: true,
                }),
                msix: Some(MsixCap {
                    at: MSIX_CAP,
                    next: 0,
                    size: MSIX_VECTORS,
                    table: (MSIX_BAR, 0),
                    pba: (MSIX_BAR, MSIX_PBA as u32),
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
            vec![
                Bar {
                    index: 0,
                    addr: at.registers,
                    size: BAR_SIZE,
                    kind: BarKind::Memory32,
                },
                Bar {
                    index: usize::from(MSIX_BAR),
                    addr: at.msix,
                    size: MSIX_BAR_SIZE,
                    kind: BarKind::Memory32,
                },
                Bar {
                    index: SCRATCH_BAR,
                    addr: at.scratch,
                    size: SCRATCH_BAR_SIZE,
                    kind: BarKind::Memory64,
                },
            ],
            interrupts,
        )?;
        let header = if role.multi_function {
            header.multi_function()
        } else {
            header
        };
        let edu = Arc::new(Edu {
            header,
            status: Mutex::new(0),
            msix: Mutex::new(MsixTable::new(MSIX_VECTORS)),
            lines,
            intx: role.intx,
            engine: Mutex::new(Engine {
                registers: [0; DMA_REGISTERS],
                buffer: vec![0; DMA_BUFFER_SIZE],
            }),
            dma,
        });
        let bars = Bars {
            registers: Arc::new(EduRegisters {
                edu: edu.clone(),
                liveness: AtomicU32::new(0),
            }),
            msix: Arc::new(EduMsix { edu: edu.clone() }),
            scratch: Arc::new(EduScratch {
                edu: edu.clone(),
                scratch: Mutex::new(vec![0; SCRATCH_DWORDS]),
            }),
        };
        Ok((edu, bars))
    }

    /// Changes the interrupt status as `change` says, and gives the guest what the status
    /// asks of the function's interrupts: where MSI-X is on, vector k for each bit k of
    /// `raised`, held pending while masked (§6.8.2); where MSI is, a message for each
    /// raise, as edu's `edu_raise_irq`; else INTx pending while the status is not zero,
    /// which the status register's Interrupt Status says whether or not INTx is disabled,
    /// and INTA# asserted where it is not (§6.2.2, §6.2.3; QEMU's pci_update_irq_status
    /// and pci_irq_disabled).
    fn interrupt(&self, change: impl FnOnce(u32) -> u32, raised: u32) {
        let mut status = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        *status = change(*status);
        let Some(lines) = &self.lines else {
            return;
        };
        // Its device's line, where it has the pin.
        let level = |on: bool| {
            if self.intx {
                lines.intx.set_level(on);
            }
        };
        if self.header.msix().is_some_and(|m| m.enabled) {
            self.header.set_interrupt_status(false);
            level(false);
            let mut table = self.msix.lock().unwrap_or_else(PoisonError::into_inner);
            for k in 0..table.vectors() {
                if raised & (1 << k) != 0 {
                    table.set_pending(k, true);
                }
            }
            self.send_unmasked(lines, &mut table);
            return;
        }
        match self.header.msi() {
            Some((address, data)) => {
                self.header.set_interrupt_status(false);
                level(false);
                if raised != 0 && *status != 0 {
                    self.send_msi(lines, address, data);
                }
            }
            None => {
                let pending = *status != 0;
                self.header.set_interrupt_status(pending);
                level(pending && self.header.command() & reg::COMMAND_INTX_OFF == 0);
            }
        }
    }

    /// Runs the transfer `engine`'s registers describe, as edu's `edu_dma_timer` does but
    /// at once: `length` bytes between its buffer, at an address in it, and guest memory,
    /// at the address its endpoint's domain maps where the machine has an IOMMU. A transfer
    /// past the buffer, or to what the domain does not map for it, moves nothing, as an
    /// IOMMU's fault ends a device's DMA. Its run bit clears as it ends, and where asked,
    /// interrupt 0x100 is raised.
    fn transfer(&self) {
        let mut engine = self.engine.lock().unwrap_or_else(PoisonError::into_inner);
        let [source, destination, length, command] = engine.registers;
        let to_memory = command & DMA_TO_MEMORY != 0;
        let (on_edu, in_memory) = if to_memory {
            (source, destination)
        } else {
            (destination, source)
        };
        let moved = (|| {
            let at = usize::try_from(on_edu.checked_sub(DMA_BUFFER)?).ok()?;
            let len = usize::try_from(length).ok()?;
            at.checked_add(len).filter(|&end| end <= DMA_BUFFER_SIZE)?;
            let dma = self.dma.as_ref()?;
            let parts = match &dma.iommu {
                Some((translation, endpoint)) => translation
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .translate(*endpoint, in_memory, length, to_memory)?,
                None => vec![(in_memory, length)],
            };
            let memory = dma.memory.access().ok()?;
            let mut at = at;
            for (gpa, part) in parts {
                let part = usize::try_from(part).ok()?;
                let bytes = engine.buffer.get_mut(at..at + part)?;
                if to_memory {
                    memory.write(gpa, bytes).ok()?;
                } else {
                    memory.read(gpa, bytes).ok()?;
                }
                at += part;
            }
            Some(())
        })();
        if moved.is_none() {
            crate::debug!("edu: a DMA of {length:#x} bytes at {in_memory:#x} moved nothing");
        }
        if let Some(c) = engine.registers.get_mut(DMA_COMMAND) {
            *c &= !DMA_RUN;
        }
        drop(engine);
        if command & DMA_RAISE != 0 {
            self.interrupt(|status| status | DMA_INTERRUPT, DMA_INTERRUPT);
        }
    }

    /// Whether it decodes its BARs' addresses: memory space enabled in its command register
    /// (§6.2.2). It does not where the guest disabled it, and an access there ends as no
    /// function claims it, a read all ones (§2.3.2).
    fn decodes(&self) -> bool {
        self.header.command() & reg::COMMAND_MEMORY != 0
    }

    /// Sends a message: a write of `data` to `address`, which goes through the function's
    /// IOMMU domain where the machine has an IOMMU, as its DMA does (D154): to the
    /// guest-physical address the domain maps it to, and nowhere where it maps none.
    fn send_msi(&self, lines: &Lines, address: u64, data: u32) {
        let to = match self.dma.as_ref().and_then(|d| d.iommu.as_ref()) {
            Some((translation, endpoint)) => translation
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .msi_target(*endpoint, address),
            None => Some(address),
        };
        match to {
            Some(gpa) => lines.msi.send(gpa, data),
            None => crate::debug!("edu: an MSI to {address:#x} its domain does not map"),
        }
    }

    /// Sends each pending MSI-X vector the function and the vector leave unmasked.
    fn send_unmasked(&self, lines: &Lines, table: &mut MsixTable) {
        if !self.header.msix().is_some_and(|m| m.enabled && !m.masked) {
            return;
        }
        for k in 0..table.vectors() {
            if table.pending(k)
                && let Some((address, data)) = table.message(k)
            {
                self.send_msi(lines, address, data);
                table.set_pending(k, false);
            }
        }
    }
}

impl PciFunction for Edu {
    fn config_read(&self, offset: u16, data: &mut [u8]) {
        self.header.read(offset, data);
    }

    /// A write, and then INTx as the command register and MSI's enable now say, and any
    /// MSI-X vector pending sent where the function is no longer masked.
    fn config_write(&self, offset: u16, data: &[u8]) {
        self.header.write(offset, data);
        self.interrupt(|status| status, 0);
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

/// Which of the DMA engine's registers `offset` of BAR 0 is in, where it is one.
fn dma_register(offset: u64) -> Option<usize> {
    let i = usize::try_from(offset.checked_sub(REG_DMA)? / 8).ok()?;
    (i < DMA_REGISTERS).then_some(i)
}

/// BAR 4: its first page what the guest last wrote there, a dword or a qword at a time; the
/// rest reads zero and keeps nothing.
#[derive(Debug)]
pub struct EduScratch {
    edu: Arc<Edu>,
    scratch: Mutex<Vec<u32>>,
}

impl MmioDevice for EduScratch {
    fn read(&self, offset: u64, data: &mut [u8]) {
        if !self.edu.decodes() {
            data.fill(0xff);
            return;
        }
        let scratch = self.scratch.lock().unwrap_or_else(PoisonError::into_inner);
        super::read_dwords(offset, data, |at| {
            usize::try_from(at / 4)
                .ok()
                .and_then(|i| scratch.get(i))
                .copied()
                .unwrap_or(0)
        });
    }

    fn write(&self, offset: u64, data: &[u8]) {
        let len = data.len() as u64;
        if !self.edu.decodes() || !matches!(len, 4 | 8) || !offset.is_multiple_of(len) {
            return;
        }
        let value = get_le(data);
        let mut scratch = self.scratch.lock().unwrap_or_else(PoisonError::into_inner);
        let dwords = [value as u32, (value >> 32) as u32];
        for (i, dword) in (offset / 4..).zip(dwords.into_iter().take(data.len() / 4)) {
            if let Some(slot) = usize::try_from(i).ok().and_then(|i| scratch.get_mut(i)) {
                *slot = dword;
            }
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

/// BAR 2: the MSI-X table, from its start, and the pending bits, read-only, at 0x800.
#[derive(Debug)]
pub struct EduMsix {
    edu: Arc<Edu>,
}

impl MmioDevice for EduMsix {
    fn read(&self, offset: u64, data: &mut [u8]) {
        if !self.edu.decodes() {
            data.fill(0xff);
            return;
        }
        let table = self.edu.msix.lock().unwrap_or_else(PoisonError::into_inner);
        match offset.checked_sub(MSIX_PBA) {
            Some(at) => table.read_pending(at, data),
            None => table.read(offset, data),
        }
    }

    /// A write of the table, the pending bits being read-only; a vector unmasked sends
    /// what it held pending.
    fn write(&self, offset: u64, data: &[u8]) {
        if offset >= MSIX_PBA || !self.edu.decodes() {
            return;
        }
        let mut table = self.edu.msix.lock().unwrap_or_else(PoisonError::into_inner);
        if table.write(offset, data).is_some()
            && let Some(lines) = &self.edu.lines
        {
            self.edu.send_unmasked(lines, &mut table);
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

impl MmioDevice for EduRegisters {
    fn read(&self, offset: u64, data: &mut [u8]) {
        if !self.edu.decodes() {
            data.fill(0xff);
            return;
        }
        if let Some(i) = dma_register(offset) {
            let engine = self.edu.engine.lock().unwrap_or_else(PoisonError::into_inner);
            let value = engine.registers.get(i).copied().unwrap_or(0);
            put_le(data, value >> (8 * (offset % 8)));
            return;
        }
        let value = match offset {
            REG_IDENTIFICATION => IDENTIFICATION,
            REG_LIVENESS => !self.liveness.load(Ordering::Relaxed),
            REG_INTERRUPT_STATUS => *self.edu.status.lock().unwrap_or_else(PoisonError::into_inner),
            _ => 0,
        };
        put_le(data, u64::from(value));
    }

    fn write(&self, offset: u64, data: &[u8]) {
        if !self.edu.decodes() {
            return;
        }
        if let Some(i) = dma_register(offset) {
            let mut engine = self.edu.engine.lock().unwrap_or_else(PoisonError::into_inner);
            let shift = 8 * (offset % 8);
            let mask = match data.len() {
                8 => u64::MAX,
                n => ((1u64 << (8 * n)) - 1) << shift,
            };
            let run = match engine.registers.get_mut(i) {
                Some(r) => {
                    *r = (*r & !mask) | ((get_le(data) << shift) & mask);
                    i == DMA_COMMAND && *r & DMA_RUN != 0
                }
                None => false,
            };
            drop(engine);
            if run {
                self.edu.transfer();
            }
            return;
        }
        let value = get_le(data) as u32;
        match offset {
            REG_LIVENESS => self.liveness.store(value, Ordering::Relaxed),
            REG_INTERRUPT_RAISE => self.edu.interrupt(|status| status | value, value),
            REG_INTERRUPT_ACK => self.edu.interrupt(|status| status & !value, 0),
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
        let (edu, regs, _, recorded) = edu_with_msix();
        (edu, regs, recorded)
    }

    fn edu_with_msix() -> (Arc<Edu>, Arc<EduRegisters>, Arc<EduMsix>, Arc<Recorded>) {
        let recorded = Arc::new(Recorded::default());
        let lines = Lines {
            intx: Arc::new(Line(recorded.clone())),
            msi: Arc::new(Line(recorded.clone())),
        };
        let (edu, bars) = Edu::new(
            Places {
                registers: 0x1010_0000,
                msix: 0x1020_0000,
                scratch: 0x1_0010_0000,
            },
            Some(lines),
            None,
        )
        .unwrap();
        let (regs, msix) = (bars.registers, bars.msix);
        // Decoding its BARs, as a driver enables it.
        edu.config_write(0x04, &reg::COMMAND_MEMORY.to_le_bytes());
        (edu, regs, msix, recorded)
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

    /// BAR 4, of 64 bits, is placed past 4 GiB, its address in two registers; its first
    /// page reads back what was written, a dword or a qword at a time.
    #[test]
    fn edus_scratch_is_64_bit_memory() {
        let recorded = Arc::new(Recorded::default());
        let lines = Lines {
            intx: Arc::new(Line(recorded.clone())),
            msi: Arc::new(Line(recorded)),
        };
        let (edu, bars) = Edu::new(
            Places {
                registers: 0x1010_0000,
                msix: 0x1020_0000,
                scratch: 0x1_0010_0000,
            },
            Some(lines),
            None,
        )
        .unwrap();
        edu.config_write(0x04, &reg::COMMAND_MEMORY.to_le_bytes());
        assert_eq!(word(&*edu, 0x20), 0x0010_0004, "BAR 4: its low half, 64-bit");
        assert_eq!(word(&*edu, 0x24), 1, "BAR 5: its high half");
        let scratch = bars.scratch;
        scratch.write(8, &0x1122_3344_5566_7788u64.to_le_bytes());
        scratch.write(0x10, &0xdead_beefu32.to_le_bytes());
        let mut q = [0u8; 8];
        scratch.read(8, &mut q);
        assert_eq!(u64::from_le_bytes(q), 0x1122_3344_5566_7788);
        let mut d = [0u8; 4];
        scratch.read(0x10, &mut d);
        assert_eq!(u32::from_le_bytes(d), 0xdead_beef);
        scratch.read(0x8_0000, &mut d);
        assert_eq!(u32::from_le_bytes(d), 0, "past the page");
    }

    /// Its DMA engine moves guest memory into its buffer and back: by guest-physical
    /// address while its endpoint bypasses the IOMMU, from reset, and by I/O virtual
    /// address once its domain maps one; to what the domain does not map, or past its
    /// buffer, it moves nothing. Its run bit clears as each ends.
    #[test]
    fn edus_dma_goes_through_its_iommu() {
        use crate::devices::virtio::iommu::{MAP_F_READ, MAP_F_WRITE, Translation};
        let memory = Arc::new(GuestMemory::anonymous(&[(0x8000_0000, 1 << 20)]).unwrap());
        let translation = Arc::new(Mutex::new(Translation::new(vec![8], None)));
        let (edu, bars) = Edu::new(
            Places {
                registers: 0x1010_0000,
                msix: 0x1020_0000,
                scratch: 0x1_0010_0000,
            },
            None,
            Some(Dma {
                memory: memory.clone(),
                iommu: Some((translation.clone(), 8)),
            }),
        )
        .unwrap();
        edu.config_write(0x04, &reg::COMMAND_MEMORY.to_le_bytes());
        let regs = bars.registers;
        let transfer = |source: u64, destination: u64, len: u64, command: u64| {
            for (i, v) in [source, destination, len, command].into_iter().enumerate() {
                regs.write(REG_DMA + 8 * i as u64, &v.to_le_bytes());
            }
        };
        let read = |gpa: u64, len: usize| {
            let mut b = vec![0u8; len];
            memory.access().unwrap().read(gpa, &mut b).unwrap();
            b
        };
        memory
            .access()
            .unwrap()
            .write(0x8000_1000, b"through edu")
            .unwrap();
        transfer(0x8000_1000, DMA_BUFFER, 11, DMA_RUN);
        transfer(DMA_BUFFER, 0x8000_2000, 11, DMA_RUN | DMA_TO_MEMORY);
        assert_eq!(read(0x8000_2000, 11), b"through edu", "by guest-physical address");
        let mut c = [0u8; 8];
        regs.read(REG_DMA + 24, &mut c);
        assert_eq!(u64::from_le_bytes(c) & DMA_RUN, 0, "done");
        // Its domain maps I/O address 0x10_0000 to 0x8000_3000.
        {
            let mut t = translation.lock().unwrap();
            assert_eq!(t.attach(1, 8, 0), 0);
            assert_eq!(
                t.map(1, 0x10_0000, 0x10_0fff, 0x8000_3000, MAP_F_READ | MAP_F_WRITE),
                0
            );
        }
        transfer(DMA_BUFFER, 0x10_0000, 11, DMA_RUN | DMA_TO_MEMORY);
        assert_eq!(read(0x8000_3000, 11), b"through edu", "by I/O virtual address");
        transfer(DMA_BUFFER, 0x8000_4000, 11, DMA_RUN | DMA_TO_MEMORY);
        assert_eq!(read(0x8000_4000, 11), vec![0; 11], "unmapped: nothing moved");
        transfer(0x10_0000, DMA_BUFFER + 4090, 11, DMA_RUN);
        regs.read(REG_DMA + 24, &mut c);
        assert_eq!(u64::from_le_bytes(c) & DMA_RUN, 0, "past the buffer, ended");
    }

    /// With its memory space disabled, edu decodes neither BAR: reads are all ones, as an
    /// access no function claims, and writes go nowhere.
    #[test]
    fn edu_decodes_its_bars_only_with_memory_space_on() {
        let (edu, regs, msix, rec) = edu_with_msix();
        edu.config_write(0x04, &0u16.to_le_bytes());
        let mut b = [0u8; 4];
        regs.read(REG_IDENTIFICATION, &mut b);
        assert_eq!(u32::from_le_bytes(b), u32::MAX);
        msix.read(12, &mut b);
        assert_eq!(u32::from_le_bytes(b), u32::MAX);
        regs.write(REG_INTERRUPT_RAISE, &1u32.to_le_bytes());
        edu.config_write(0x04, &reg::COMMAND_MEMORY.to_le_bytes());
        regs.read(REG_IDENTIFICATION, &mut b);
        assert_eq!(u32::from_le_bytes(b), IDENTIFICATION);
        regs.read(REG_INTERRUPT_STATUS, &mut b);
        assert_eq!(u32::from_le_bytes(b), 0, "the raise went nowhere");
        assert!(!rec.levels.lock().unwrap().contains(&true));
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
        edu.config_write(0x04, &(reg::COMMAND_MEMORY | reg::COMMAND_INTX_OFF).to_le_bytes());
        regs.write(REG_INTERRUPT_RAISE, &0x1u32.to_le_bytes());
        assert_eq!(rec.levels.lock().unwrap().last(), Some(&false));
        assert_ne!(word(&*edu, 0x04) >> 16 & u32::from(reg::STATUS_INTERRUPT), 0);
        edu.config_write(0x04, &reg::COMMAND_MEMORY.to_le_bytes());
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

    /// MSI-X (shards' own, beside edu's MSI): its capability after MSI's, its table and
    /// pending bits in BAR 2. Vectors start masked, so a raise is held pending, and sent
    /// as its vector is unmasked; the function's mask holds them all; a raise of bit k
    /// sends vector k; and INTx and MSI stay quiet while MSI-X is on.
    #[test]
    fn edu_raises_its_msix_vectors() {
        let (edu, regs, msix, rec) = edu_with_msix();
        assert_eq!(
            word(&*edu, MSI_CAP) >> 8 & 0xff,
            u32::from(MSIX_CAP),
            "MSI's next"
        );
        assert_eq!(word(&*edu, MSIX_CAP) & 0xff, u32::from(reg::CAP_MSIX));
        assert_eq!(word(&*edu, MSIX_CAP) >> 16 & 0x7ff, u32::from(MSIX_VECTORS) - 1);
        assert_eq!(
            word(&*edu, MSIX_CAP + reg::MSIX_TABLE),
            2,
            "table at BAR 2's start"
        );
        assert_eq!(word(&*edu, MSIX_CAP + reg::MSIX_PBA), 0x802);
        assert_eq!(word(&*edu, 0x18), 0x1020_0000, "BAR 2");
        let dword = |offset: u64| {
            let mut b = [0u8; 4];
            msix.read(offset, &mut b);
            u32::from_le_bytes(b)
        };
        assert_eq!(dword(12), reg::MSIX_ENTRY_MASKED, "masked from reset");
        // Vector 1's message, written as a qword and a dword.
        msix.write(16, &0x0802_0040u64.to_le_bytes());
        msix.write(24, &81u32.to_le_bytes());
        edu.config_write(MSIX_CAP + reg::MSIX_CONTROL, &reg::MSIX_ENABLE.to_le_bytes());
        regs.write(REG_INTERRUPT_RAISE, &0b10u32.to_le_bytes());
        assert!(rec.msis.lock().unwrap().is_empty(), "masked: held");
        assert_eq!(dword(MSIX_PBA), 0b10, "pending");
        msix.write(28, &0u32.to_le_bytes());
        assert_eq!(
            *rec.msis.lock().unwrap(),
            vec![(0x0802_0040, 81)],
            "sent as unmasked"
        );
        assert_eq!(dword(MSIX_PBA), 0);
        // The function masked: held, then sent as it is unmasked.
        edu.config_write(
            MSIX_CAP + reg::MSIX_CONTROL,
            &(reg::MSIX_ENABLE | reg::MSIX_MASK).to_le_bytes(),
        );
        regs.write(REG_INTERRUPT_RAISE, &0b10u32.to_le_bytes());
        assert_eq!(rec.msis.lock().unwrap().len(), 1);
        edu.config_write(MSIX_CAP + reg::MSIX_CONTROL, &reg::MSIX_ENABLE.to_le_bytes());
        assert_eq!(rec.msis.lock().unwrap().len(), 2);
        assert!(rec.levels.lock().unwrap().iter().all(|&l| !l), "INTx stays low");
    }
}
