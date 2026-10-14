//! A PCI bus for the devices a machine is given (D153): segment 0, bus 0, each function's
//! configuration space reached through an ECAM window (PCI Express Base Specification 6.0
//! §7.2.2), as Linux's pci-host-generic finds it in an arm64 guest's devicetree and its
//! ACPI host bridge, by MCFG, in an x86_64 guest's.

pub mod edu;
#[cfg(target_os = "linux")]
pub mod vfio;

use std::sync::{Arc, Mutex, PoisonError};

use super::{MmioDevice, get_le, put_le};
use crate::snapshot::codec::{self, Reader, Writer};

/// ECAM's window for bus 0: 32 devices of 8 functions, 4 KiB of configuration space each.
pub const ECAM_SIZE: u64 = 1 << 20;

/// A function on the bus, by its configuration space: accesses of 1, 2 or 4 bytes,
/// aligned to their size, within its 4 KiB.
pub trait PciFunction: Send + Sync {
    fn config_read(&self, offset: u16, data: &mut [u8]);
    fn config_write(&self, offset: u16, data: &[u8]);
}

/// A function on the bus and its device and function number (`devfn`: device << 3 |
/// function).
pub type Function = (u8, Arc<dyn PciFunction>);

/// Sends a message-signalled interrupt (PCI Local Bus 3.0 §6.8): a write of `data` to
/// `address`, which the machine delivers as its interrupt controller takes MSIs.
pub trait Msi: Send + Sync {
    fn send(&self, address: u64, data: u32);
}

impl std::fmt::Debug for dyn Msi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Msi")
    }
}

/// Where a function the VMM emulates sends its interrupts: its INTx pin's line, and the
/// machine's MSIs.
#[derive(Clone)]
pub struct Lines {
    pub intx: Arc<dyn super::Interrupt>,
    pub msi: Arc<dyn Msi>,
}

impl std::fmt::Debug for Lines {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Lines")
    }
}

/// The ECAM window: an access's offset holds its bus (27:20), device (19:15), function
/// (14:12) and register (11:0) (§7.2.2, Table 7-1). Only bus 0 is there; a function not
/// there reads as all ones, as an access that no function claims completes (§2.3.2), and
/// a write to it goes nowhere.
pub struct Ecam {
    functions: Vec<Function>,
}

impl std::fmt::Debug for Ecam {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.functions.iter().map(|(devfn, _)| (devfn >> 3, devfn & 7)))
            .finish()
    }
}

impl Ecam {
    pub fn new(functions: Vec<Function>) -> Ecam {
        Ecam { functions }
    }

    /// The function and register an access at `offset` of `len` bytes reaches, where it is
    /// one of the bus's functions and the access is whole and aligned.
    fn decode(&self, offset: u64, len: usize) -> Option<(&dyn PciFunction, u16)> {
        let bus = offset >> 20;
        let devfn = u8::try_from((offset >> 12) & 0xff).ok()?;
        let register = u16::try_from(offset & 0xfff).ok()?;
        if bus != 0 || !matches!(len, 1 | 2 | 4) || !(register as usize).is_multiple_of(len) {
            return None;
        }
        self.functions
            .iter()
            .find(|(f, _)| *f == devfn)
            .map(|(_, function)| (function.as_ref(), register))
    }
}

impl MmioDevice for Ecam {
    fn read(&self, offset: u64, data: &mut [u8]) {
        match self.decode(offset, data.len()) {
            Some((function, register)) => function.config_read(register, data),
            None => data.fill(0xff),
        }
    }

    fn write(&self, offset: u64, data: &[u8]) {
        if let Some((function, register)) = self.decode(offset, data.len()) {
            function.config_write(register, data);
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

    /// A function's state, its device's own (a passed-through device's above all), is not
    /// in a snapshot: a machine with a PCI bus saves none.
    fn unsaved(&self) -> Option<String> {
        Some("its PCI functions' state is their devices' own".into())
    }
}

/// What a BAR maps (PCI Local Bus 3.0 §6.2.5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarKind {
    /// Memory below 4 GiB, its BAR one register.
    Memory32,
    /// Memory anywhere, its BAR two registers, this and the next.
    Memory64,
    /// I/O space.
    Io,
}

/// A BAR as the VMM placed it: where, how much (a power of two) and of what kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bar {
    pub index: usize,
    pub addr: u64,
    pub size: u64,
    pub kind: BarKind,
}

/// The configuration header of a type 0 function (PCI Local Bus 3.0 §6.1), as the VMM
/// emulates it: its identification and the rest read-only, the command register's
/// enables and the interrupt line written, and each BAR sized as §6.2.5.1 asks (all ones
/// written, its size's mask read back) and otherwise kept where the VMM placed it: a
/// guest writing another address there reads back its place, which a guest told to keep
/// the firmware's placement (arm64's `linux,pci-probe-only`, x86_64's ACPI) never does.
#[derive(Debug)]
pub struct Header {
    /// The 4 KiB of configuration space, as reads see it.
    space: Mutex<Vec<u8>>,
    bars: Vec<Bar>,
    /// Its MSI capability, if it has one.
    msi: Option<MsiCap>,
    /// Its MSI-X capability, if it has one.
    msix: Option<MsixCap>,
}

/// Configuration space registers (§6.1, §6.2).
pub mod reg {
    pub const VENDOR: u16 = 0x00;
    pub const DEVICE: u16 = 0x02;
    pub const COMMAND: u16 = 0x04;
    pub const STATUS: u16 = 0x06;
    pub const REVISION: u16 = 0x08;
    pub const CLASS: u16 = 0x09;
    pub const HEADER_TYPE: u16 = 0x0e;
    pub const BAR0: u16 = 0x10;
    pub const SUBSYSTEM_VENDOR: u16 = 0x2c;
    pub const SUBSYSTEM: u16 = 0x2e;
    pub const CAPABILITIES: u16 = 0x34;
    pub const INTERRUPT_LINE: u16 = 0x3c;
    pub const INTERRUPT_PIN: u16 = 0x3d;
    /// The status register's interrupt status (an INTx asserted) and capabilities list.
    pub const STATUS_INTERRUPT: u16 = 1 << 3;
    pub const STATUS_CAPABILITIES: u16 = 1 << 4;
    /// MSI's capability ID (§6.8.1), and its registers from the capability's start: its
    /// message control (enable, bit 0; 64-bit addresses, bit 7; per-vector masking, bit
    /// 8), address, upper address where 64-bit, and data, after the address's 32 or 64
    /// bits.
    pub const CAP_MSI: u8 = 0x05;
    pub const MSI_CONTROL: u16 = 2;
    pub const MSI_ADDRESS: u16 = 4;
    pub const MSI_ADDRESS_HIGH: u16 = 8;
    pub const MSI_DATA: u16 = 12;
    pub const MSI_DATA_32: u16 = 8;
    pub const MSI_ENABLE: u16 = 1 << 0;
    pub const MSI_64BIT: u16 = 1 << 7;
    pub const MSI_MASKING: u16 = 1 << 8;
    /// MSI-X's capability ID (§6.8.2), and its registers from the capability's start: its
    /// message control (the table's size less one, bits 10:0; Function Mask, bit 14;
    /// MSI-X Enable, bit 15), and its table's and its pending bits' BAR (bits 2:0) and
    /// offset in it.
    pub const CAP_MSIX: u8 = 0x11;
    pub const MSIX_CONTROL: u16 = 2;
    pub const MSIX_TABLE: u16 = 4;
    pub const MSIX_PBA: u16 = 8;
    pub const MSIX_LEN: u16 = 12;
    pub const MSIX_MASK: u16 = 1 << 14;
    pub const MSIX_ENABLE: u16 = 1 << 15;
    /// A table entry's registers (§6.8.2.6-9): its message address, upper address, data,
    /// and vector control, whose bit 0 masks it.
    pub const MSIX_ENTRY: u64 = 16;
    pub const MSIX_ENTRY_MASKED: u32 = 1 << 0;
    /// The command register's I/O space, memory space and bus master enables.
    pub const COMMAND_IO: u16 = 1 << 0;
    pub const COMMAND_MEMORY: u16 = 1 << 1;
    pub const COMMAND_MASTER: u16 = 1 << 2;
    /// INTx disabled.
    pub const COMMAND_INTX_OFF: u16 = 1 << 10;
}

/// The interrupts a function says it has (§6.2.4, §6.8): its INTx pin, 1 to 4 for INTA#
/// to INTD# and 0 for none, and its MSI and MSI-X capabilities.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Interrupts {
    pub pin: u8,
    pub msi: Option<MsiCap>,
    pub msix: Option<MsixCap>,
}

/// An MSI-X capability the VMM keeps (§6.8.2): where it is, the capability after it in
/// the list, its table's size, and where its table and pending bits are, each a BAR's
/// index and an offset in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsixCap {
    pub at: u16,
    pub next: u8,
    pub size: u16,
    pub table: (u8, u32),
    pub pba: (u8, u32),
}

/// Where a function's MSI-X stands: enabled, and the whole function masked.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MsixState {
    pub enabled: bool,
    pub masked: bool,
}

/// An MSI-X table and its pending bits (§6.8.2), which a function keeps in its BARs:
/// each vector's message address, upper address, data and vector control, every vector
/// masked from reset (§6.8.2.9); and the vectors pending, each set as its vector would
/// have been sent masked and cleared as it is sent unmasked. They are reached a dword
/// or a qword at a time ("software must use aligned full DWORD or aligned full QWORD
/// transactions"): a narrower read gets its dword's bytes, and a narrower write is
/// dropped.
#[derive(Debug)]
pub struct MsixTable {
    entries: Vec<[u32; 4]>,
    pending: Vec<u64>,
}

impl MsixTable {
    /// A table of `size` vectors.
    pub fn new(size: u16) -> MsixTable {
        MsixTable {
            entries: vec![[0, 0, 0, reg::MSIX_ENTRY_MASKED]; usize::from(size)],
            pending: vec![0; usize::from(size).div_ceil(64)],
        }
    }

    pub fn vectors(&self) -> usize {
        self.entries.len()
    }

    /// Vector `k`'s message, its address and data, where the vector is unmasked.
    pub fn message(&self, k: usize) -> Option<(u64, u32)> {
        let [low, high, data, control] = *self.entries.get(k)?;
        (control & reg::MSIX_ENTRY_MASKED == 0).then_some((u64::from(low) | (u64::from(high) << 32), data))
    }

    pub fn pending(&self, k: usize) -> bool {
        self.pending.get(k / 64).is_some_and(|w| w & (1 << (k % 64)) != 0)
    }

    pub fn set_pending(&mut self, k: usize, pending: bool) {
        if let Some(w) = self.pending.get_mut(k / 64) {
            let bit = 1u64 << (k % 64);
            *w = if pending { *w | bit } else { *w & !bit };
        }
    }

    /// Reads the table at `offset`; past its vectors, zeros.
    pub fn read(&self, offset: u64, data: &mut [u8]) {
        read_dwords(offset, data, |at| {
            let k = usize::try_from(at / reg::MSIX_ENTRY).unwrap_or(usize::MAX);
            let field = ((at % reg::MSIX_ENTRY) / 4) as usize;
            self.entries
                .get(k)
                .and_then(|e| e.get(field))
                .copied()
                .unwrap_or(0)
        });
    }

    /// Reads the pending bits at `offset`; past them, zeros.
    pub fn read_pending(&self, offset: u64, data: &mut [u8]) {
        read_dwords(offset, data, |at| {
            let w = usize::try_from(at / 8).unwrap_or(usize::MAX);
            self.pending
                .get(w)
                .map_or(0, |w| (w >> (32 * ((at / 4) % 2))) as u32)
        });
    }

    /// Writes the table at `offset`: a vector's message, or its vector control's mask,
    /// the one bit of it a guest writes; the vector written, where one was.
    pub fn write(&mut self, offset: u64, data: &[u8]) -> Option<usize> {
        let len = data.len() as u64;
        if !matches!(len, 4 | 8) || !offset.is_multiple_of(len) {
            return None;
        }
        let value = get_le(data);
        let k = usize::try_from(offset / reg::MSIX_ENTRY).ok()?;
        let entry = self.entries.get_mut(k)?;
        let first = ((offset % reg::MSIX_ENTRY) / 4) as usize;
        for (field, dword) in (first..).zip(
            [value as u32, (value >> 32) as u32]
                .into_iter()
                .take(data.len() / 4),
        ) {
            if let Some(slot) = entry.get_mut(field) {
                *slot = if field == 3 {
                    dword & reg::MSIX_ENTRY_MASKED
                } else {
                    dword
                };
            }
        }
        Some(k)
    }
}

/// Reads `data` at `offset` of registers that `dword` gives a dword at a time: a qword
/// as two, and a narrower read as its dword's bytes.
pub(crate) fn read_dwords(offset: u64, data: &mut [u8], dword: impl Fn(u64) -> u32) {
    let at = offset & !3;
    let value = if data.len() == 8 {
        u64::from(dword(at)) | (u64::from(dword(at + 4)) << 32)
    } else {
        u64::from(dword(at)) >> (8 * (offset & 3))
    };
    put_le(data, value);
}

/// An MSI capability the VMM keeps (§6.8.1): where it is, the capability after it in the
/// list (0 for none), and whether its addresses are 64-bit; one message, without
/// per-vector masking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsiCap {
    pub at: u16,
    pub next: u8,
    pub is64: bool,
}

impl MsiCap {
    /// Where its data register is.
    fn data(&self) -> u16 {
        self.at
            + if self.is64 {
                reg::MSI_DATA
            } else {
                reg::MSI_DATA_32
            }
    }
}

/// What a function says it is.
#[derive(Debug, Clone, Copy)]
pub struct Identity {
    pub vendor: u16,
    pub device: u16,
    /// Base class, sub-class and programming interface, as three bytes.
    pub class: u32,
    pub revision: u8,
    pub subsystem_vendor: u16,
    pub subsystem: u16,
}

impl Header {
    /// The header of a function of a multi-function device: its header type's bit 7
    /// (PCIe 6.0 §7.5.1.1.9), by which a guest looks for the device's other functions.
    pub fn multi_function(mut self) -> Header {
        let space = self.space.get_mut().unwrap_or_else(PoisonError::into_inner);
        if let Some(t) = space.get_mut(usize::from(reg::HEADER_TYPE)) {
            *t |= 0x80;
        }
        self
    }

    /// A function's header with `bars`, each placed where its address is a multiple of its
    /// size (§6.2.5.1), sizes powers of two of at least 16 bytes (memory) or 4 (I/O), and
    /// no interrupt.
    pub fn new(id: Identity, bars: Vec<Bar>) -> Result<Header, String> {
        Header::with_interrupts(id, bars, Interrupts::default())
    }

    /// [`Header::new`]'s, with `interrupts`: its INTx pin, and its MSI capability, the
    /// first in its list, in the header's device-specific part (§6.7).
    pub fn with_interrupts(id: Identity, bars: Vec<Bar>, interrupts: Interrupts) -> Result<Header, String> {
        if interrupts.pin > 4 {
            return Err(format!("INTx pin {}: not one of INTA# to INTD#", interrupts.pin));
        }
        if let Some(cap) = interrupts.msi
            && (cap.at < 0x40 || !cap.at.is_multiple_of(4) || cap.data() + 2 > 0x100)
        {
            return Err(format!(
                "an MSI capability at {:#x}: past the header, in it, or unaligned",
                cap.at
            ));
        }
        if let Some(cap) = interrupts.msix
            && (cap.at < 0x40
                || !cap.at.is_multiple_of(4)
                || cap.at + reg::MSIX_LEN > 0x100
                || cap.size == 0
                || cap.size > 2048
                || cap.table.0 > 5
                || cap.pba.0 > 5
                || !cap.table.1.is_multiple_of(8)
                || !cap.pba.1.is_multiple_of(8))
        {
            return Err(format!(
                "an MSI-X capability at {:#x}: past the header, in it, unaligned, or its table not one",
                cap.at
            ));
        }
        let mut space = vec![0u8; 4096];
        let mut put = |at: u16, bytes: &[u8]| {
            if let Some(d) = space.get_mut(usize::from(at)..usize::from(at) + bytes.len()) {
                d.copy_from_slice(bytes);
            }
        };
        put(reg::VENDOR, &id.vendor.to_le_bytes());
        put(reg::DEVICE, &id.device.to_le_bytes());
        put(reg::REVISION, &[id.revision]);
        put(reg::CLASS, &id.class.to_le_bytes()[..3]);
        put(reg::HEADER_TYPE, &[0]);
        put(reg::SUBSYSTEM_VENDOR, &id.subsystem_vendor.to_le_bytes());
        put(reg::SUBSYSTEM, &id.subsystem.to_le_bytes());
        put(reg::INTERRUPT_PIN, &[interrupts.pin]);
        // The list starts at the first capability it holds.
        let first = [interrupts.msi.map(|c| c.at), interrupts.msix.map(|c| c.at)]
            .into_iter()
            .flatten()
            .min();
        if let Some(first) = first {
            put(reg::STATUS, &reg::STATUS_CAPABILITIES.to_le_bytes());
            put(reg::CAPABILITIES, &[first as u8]);
        }
        if let Some(cap) = interrupts.msi {
            put(cap.at, &[reg::CAP_MSI, cap.next]);
            let control = if cap.is64 { reg::MSI_64BIT } else { 0 };
            put(cap.at + reg::MSI_CONTROL, &control.to_le_bytes());
        }
        if let Some(cap) = interrupts.msix {
            put(cap.at, &[reg::CAP_MSIX, cap.next]);
            put(cap.at + reg::MSIX_CONTROL, &(cap.size - 1).to_le_bytes());
            let place = |(bir, offset): (u8, u32)| offset | u32::from(bir);
            put(cap.at + reg::MSIX_TABLE, &place(cap.table).to_le_bytes());
            put(cap.at + reg::MSIX_PBA, &place(cap.pba).to_le_bytes());
        }
        for bar in &bars {
            let least = if bar.kind == BarKind::Io { 4 } else { 16 };
            let registers = if bar.kind == BarKind::Memory64 { 2 } else { 1 };
            if bar.index + registers > 6
                || !bar.size.is_power_of_two()
                || bar.size < least
                || !bar.addr.is_multiple_of(bar.size)
                || (bar.kind == BarKind::Memory32 && bar.addr.saturating_add(bar.size) > 1 << 32)
            {
                return Err(format!(
                    "BAR {} at {:#x}+{:#x}: not a BAR",
                    bar.index, bar.addr, bar.size
                ));
            }
            let at = reg::BAR0 + 4 * bar.index as u16;
            let value = bar.addr | kind_bits(bar.kind);
            put(at, &(value as u32).to_le_bytes());
            if bar.kind == BarKind::Memory64 {
                put(at + 4, &((value >> 32) as u32).to_le_bytes());
            }
        }
        Ok(Header {
            space: Mutex::new(space),
            bars,
            msi: interrupts.msi,
            msix: interrupts.msix,
        })
    }

    /// Its MSI-X capability, if it has one.
    pub fn msix_cap(&self) -> Option<MsixCap> {
        self.msix
    }

    /// Where its MSI-X stands, as the guest set its message control; none without MSI-X.
    pub fn msix(&self) -> Option<MsixState> {
        let cap = self.msix?;
        let mut control = [0u8; 2];
        self.read(cap.at + reg::MSIX_CONTROL, &mut control);
        let control = u16::from_le_bytes(control);
        Some(MsixState {
            enabled: control & reg::MSIX_ENABLE != 0,
            masked: control & reg::MSIX_MASK != 0,
        })
    }

    pub fn bars(&self) -> &[Bar] {
        &self.bars
    }

    /// The command register as the guest last wrote it.
    pub fn command(&self) -> u16 {
        let space = self.space.lock().unwrap_or_else(PoisonError::into_inner);
        u16::from_le_bytes([
            space.get(usize::from(reg::COMMAND)).copied().unwrap_or(0),
            space.get(usize::from(reg::COMMAND) + 1).copied().unwrap_or(0),
        ])
    }

    /// The MSI the function sends, its address and data, where the guest enabled MSI.
    pub fn msi(&self) -> Option<(u64, u32)> {
        let cap = self.msi?;
        let space = self.space.lock().unwrap_or_else(PoisonError::into_inner);
        let field = |at: u16, len: usize| {
            space
                .get(usize::from(at)..usize::from(at) + len)
                .map(get_le)
                .unwrap_or(0)
        };
        if field(cap.at + reg::MSI_CONTROL, 2) & u64::from(reg::MSI_ENABLE) == 0 {
            return None;
        }
        let high = if cap.is64 {
            field(cap.at + reg::MSI_ADDRESS_HIGH, 4)
        } else {
            0
        };
        let address = field(cap.at + reg::MSI_ADDRESS, 4) | (high << 32);
        Some((address, field(cap.data(), 2) as u32))
    }

    /// Sets the status register's interrupt status: whether the function asserts INTx.
    pub fn set_interrupt_status(&self, asserted: bool) {
        let mut space = self.space.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(b) = space.get_mut(usize::from(reg::STATUS)) {
            let bit = reg::STATUS_INTERRUPT as u8;
            *b = if asserted { *b | bit } else { *b & !bit };
        }
    }

    pub fn read(&self, offset: u16, data: &mut [u8]) {
        let space = self.space.lock().unwrap_or_else(PoisonError::into_inner);
        match space.get(usize::from(offset)..usize::from(offset) + data.len()) {
            Some(bytes) => data.copy_from_slice(bytes),
            None => data.fill(0xff),
        }
    }

    /// A write: the command register's enables, the interrupt line, MSI's enable, address
    /// and data, and the BARs as their sizing asks; every other register is read-only.
    pub fn write(&self, offset: u16, data: &[u8]) {
        let mut space = self.space.lock().unwrap_or_else(PoisonError::into_inner);
        let (msi, msix) = (self.msi, self.msix);
        let writable = |at: u16| -> Option<u8> {
            // MSI-X: its message control's Function Mask and Enable, bits 14 and 15.
            if msix.is_some_and(|cap| at == cap.at + reg::MSIX_CONTROL + 1) {
                return Some(0xc0);
            }
            match at {
                // Command: I/O, memory, bus master and INTx disable.
                0x04 => Some(0x07),
                0x05 => Some(0x04),
                0x3c => Some(0xff),
                _ => {
                    // MSI: its enable alone of its control (one message, so no more to
                    // enable), its address but the two bits it keeps clear, its upper
                    // address where 64-bit, and its data.
                    let cap = msi?;
                    let from = at.checked_sub(cap.at)?;
                    let data = cap.data() - cap.at;
                    match from {
                        2 => Some(0x01),
                        4 => Some(0xfc),
                        5..=7 => Some(0xff),
                        8..=11 if cap.is64 => Some(0xff),
                        _ if from == data || from == data + 1 => Some(0xff),
                        _ => None,
                    }
                }
            }
        };
        for (i, &byte) in data.iter().enumerate() {
            let at = offset.saturating_add(i as u16);
            if let Some(mask) = writable(at)
                && let Some(b) = space.get_mut(usize::from(at))
            {
                *b = (*b & !mask) | (byte & mask);
            }
        }
        // A BAR register written whole: its size's mask where all ones were written, as
        // sizing asks, else its place.
        if data.len() == 4 && (reg::BAR0..reg::BAR0 + 24).contains(&offset) {
            let register = usize::from((offset - reg::BAR0) / 4);
            let written = get_le(data) as u32;
            if let Some(value) = self.bar_register(register, written == u32::MAX)
                && let Some(d) = space.get_mut(usize::from(offset)..usize::from(offset) + 4)
            {
                d.copy_from_slice(&value.to_le_bytes());
            }
        }
    }

    /// BAR register `register`'s value: its size's mask (`sizing`) or its place, with its
    /// kind's bits; zero where no BAR is.
    fn bar_register(&self, register: usize, sizing: bool) -> Option<u32> {
        for bar in &self.bars {
            let value = if sizing { !(bar.size - 1) } else { bar.addr };
            if bar.index == register {
                return Some((value as u32 & kind_mask(bar.kind)) | kind_bits(bar.kind) as u32);
            }
            if bar.kind == BarKind::Memory64 && bar.index + 1 == register {
                return Some((value >> 32) as u32);
            }
        }
        Some(0)
    }
}

/// A BAR register's low bits of its kind: I/O (bit 0), or memory of 32 or 64 bits (2:1).
fn kind_bits(kind: BarKind) -> u64 {
    match kind {
        BarKind::Memory32 => 0,
        BarKind::Memory64 => 0b100,
        BarKind::Io => 1,
    }
}

/// The address bits of a BAR register of `kind`.
fn kind_mask(kind: BarKind) -> u32 {
    match kind {
        BarKind::Io => !0x3,
        _ => !0xf,
    }
}

/// Puts `value` in `data` as a little-endian access of its length (configuration reads).
pub fn put(data: &mut [u8], value: u64) {
    put_le(data, value);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> Identity {
        Identity {
            vendor: 0x1234,
            device: 0x11e8,
            class: 0x00ff00,
            revision: 0x10,
            subsystem_vendor: 0x1af4,
            subsystem: 0x1100,
        }
    }

    fn read(h: &Header, offset: u16, len: usize) -> u64 {
        let mut b = vec![0u8; len];
        h.read(offset, &mut b);
        get_le(&b)
    }

    /// A header says what its function is, and its BARs are sized as the spec asks and
    /// kept where they were placed.
    #[test]
    fn a_header_is_read_and_its_bars_sized() {
        let bars = vec![
            Bar {
                index: 0,
                addr: 0x1010_0000,
                size: 1 << 20,
                kind: BarKind::Memory32,
            },
            Bar {
                index: 2,
                addr: 0x80_0000_0000,
                size: 1 << 30,
                kind: BarKind::Memory64,
            },
            Bar {
                index: 4,
                addr: 0x1000,
                size: 0x100,
                kind: BarKind::Io,
            },
        ];
        let h = Header::new(id(), bars).unwrap();
        assert_eq!(read(&h, reg::VENDOR, 4), 0x11e8_1234);
        assert_eq!(read(&h, reg::REVISION, 4), 0x00ff_0010);
        assert_eq!(read(&h, reg::BAR0, 4), 0x1010_0000);
        assert_eq!(read(&h, reg::BAR0 + 8, 4), 0b100);
        assert_eq!(read(&h, reg::BAR0 + 12, 4), 0x80);
        assert_eq!(read(&h, reg::BAR0 + 16, 4), 0x1001);
        // Sizing: all ones written, the size's mask read back; the place again after.
        h.write(reg::BAR0, &u32::MAX.to_le_bytes());
        assert_eq!(read(&h, reg::BAR0, 4), 0xfff0_0000);
        h.write(reg::BAR0, &0x1010_0000u32.to_le_bytes());
        assert_eq!(read(&h, reg::BAR0, 4), 0x1010_0000);
        h.write(reg::BAR0 + 8, &u32::MAX.to_le_bytes());
        h.write(reg::BAR0 + 12, &u32::MAX.to_le_bytes());
        assert_eq!(read(&h, reg::BAR0 + 8, 4), 0xc000_0004);
        assert_eq!(read(&h, reg::BAR0 + 12, 4), 0xffff_ffff);
        h.write(reg::BAR0 + 16, &u32::MAX.to_le_bytes());
        assert_eq!(read(&h, reg::BAR0 + 16, 4), 0xffff_ff01);
        // A BAR moved reads back its place; one not there reads zero, sized or not.
        h.write(reg::BAR0, &0x2000_0000u32.to_le_bytes());
        assert_eq!(read(&h, reg::BAR0, 4), 0x1010_0000);
        h.write(reg::BAR0 + 4, &u32::MAX.to_le_bytes());
        assert_eq!(read(&h, reg::BAR0 + 4, 4), 0);
        // The command register's enables are written; the vendor is not.
        h.write(reg::COMMAND, &0xffffu16.to_le_bytes());
        assert_eq!(h.command(), 0x0407);
        h.write(reg::VENDOR, &0u16.to_le_bytes());
        assert_eq!(read(&h, reg::VENDOR, 2), 0x1234);
    }

    /// A BAR that is not one is refused: misaligned, not a power of two, past 4 GiB for 32
    /// bits, or past the sixth register.
    #[test]
    fn a_bar_that_is_not_one_is_refused() {
        for (index, addr, size, kind) in [
            (0, 0x1000_0800, 0x1000, BarKind::Memory32),
            (0, 0x1000_0000, 0x3000, BarKind::Memory32),
            (0, 0xffff_f000, 0x2000, BarKind::Memory32),
            (5, 0x1_0000_0000, 0x1000, BarKind::Memory64),
            (0, 0x1000, 2, BarKind::Io),
        ] {
            let bar = Bar {
                index,
                addr,
                size,
                kind,
            };
            assert!(Header::new(id(), vec![bar]).is_err(), "{bar:?}");
        }
    }

    /// An MSI-X table starts with every vector masked. A guest writes an entry a dword or
    /// an aligned qword at a time, and of its vector control only the mask; a narrower or
    /// unaligned write is dropped. The pending bits read as qwords of 64 vectors each.
    #[test]
    fn an_msix_table_is_written_and_read_as_dwords_and_qwords() {
        let mut t = MsixTable::new(70);
        let dword = |t: &MsixTable, at: u64| {
            let mut b = [0u8; 4];
            t.read(at, &mut b);
            u32::from_le_bytes(b)
        };
        assert_eq!(dword(&t, 12), reg::MSIX_ENTRY_MASKED);
        assert_eq!(t.message(0), None);
        assert_eq!(t.write(16, &0x1_0802_0040u64.to_le_bytes()), Some(1));
        assert_eq!(t.write(24, &81u32.to_le_bytes()), Some(1));
        assert_eq!(t.write(28, &u32::MAX.to_le_bytes()), Some(1));
        assert_eq!(
            dword(&t, 28),
            reg::MSIX_ENTRY_MASKED,
            "of vector control, the mask"
        );
        assert_eq!(t.message(1), None, "masked");
        // Data and vector control as one qword: unmasked.
        assert_eq!(t.write(24, &81u64.to_le_bytes()), Some(1));
        assert_eq!(t.message(1), Some((0x1_0802_0040, 81)));
        assert_eq!(t.write(16, &0u16.to_le_bytes()), None, "narrower");
        assert_eq!(t.write(20, &0u64.to_le_bytes()), None, "unaligned");
        assert_eq!(t.write(70 * 16, &0u32.to_le_bytes()), None, "past the table");
        assert_eq!(t.message(1), Some((0x1_0802_0040, 81)));
        let mut q = [0u8; 8];
        t.read(16, &mut q);
        assert_eq!(u64::from_le_bytes(q), 0x1_0802_0040);
        assert_eq!(dword(&t, 70 * 16), 0, "past the table");
        // Vector 69 pends: bit 5 of the second qword.
        t.set_pending(69, true);
        assert!(t.pending(69));
        t.read_pending(8, &mut q);
        assert_eq!(u64::from_le_bytes(q), 1 << 5);
        let mut d = [0u8; 4];
        t.read_pending(12, &mut d);
        assert_eq!(u32::from_le_bytes(d), 0);
        t.set_pending(69, false);
        t.read_pending(8, &mut q);
        assert_eq!(u64::from_le_bytes(q), 0);
    }

    struct Fixed(Header);

    impl PciFunction for Fixed {
        fn config_read(&self, offset: u16, data: &mut [u8]) {
            self.0.read(offset, data);
        }
        fn config_write(&self, offset: u16, data: &[u8]) {
            self.0.write(offset, data);
        }
    }

    /// ECAM reaches a function by its device and function number; the rest of the bus,
    /// other buses and accesses not whole read as all ones.
    #[test]
    fn ecam_reaches_the_functions_there() {
        let f: Arc<dyn PciFunction> = Arc::new(Fixed(Header::new(id(), Vec::new()).unwrap()));
        let ecam = Ecam::new(vec![(1 << 3, f)]);
        let at = |device: u64, function: u64, register: u64| (device << 15) | (function << 12) | register;
        let mut b = [0u8; 4];
        ecam.read(at(1, 0, 0), &mut b);
        assert_eq!(u32::from_le_bytes(b), 0x11e8_1234);
        let mut h = [0u8; 2];
        ecam.read(at(1, 0, 2), &mut h);
        assert_eq!(u16::from_le_bytes(h), 0x11e8);
        for (addr, len) in [
            (at(0, 0, 0), 4),
            (at(1, 1, 0), 4),
            ((1 << 20) | at(1, 0, 0), 4),
            (at(1, 0, 1), 2),
        ] {
            let mut b = vec![0u8; len];
            ecam.read(addr, &mut b);
            assert!(b.iter().all(|&x| x == 0xff), "{addr:#x}+{len}: {b:?}");
        }
        assert!(ecam.unsaved().is_some());
    }
}
