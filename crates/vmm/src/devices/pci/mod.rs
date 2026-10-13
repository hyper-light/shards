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
    /// The command register's I/O space, memory space and bus master enables.
    pub const COMMAND_IO: u16 = 1 << 0;
    pub const COMMAND_MEMORY: u16 = 1 << 1;
    pub const COMMAND_MASTER: u16 = 1 << 2;
    /// INTx disabled.
    pub const COMMAND_INTX_OFF: u16 = 1 << 10;
}

/// The interrupts a function says it has (§6.2.4, §6.8.1): its INTx pin, 1 to 4 for
/// INTA# to INTD# and 0 for none, and its MSI capability.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Interrupts {
    pub pin: u8,
    pub msi: Option<MsiCap>,
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
        if let Some(cap) = interrupts.msi {
            put(reg::STATUS, &reg::STATUS_CAPABILITIES.to_le_bytes());
            put(reg::CAPABILITIES, &[cap.at as u8]);
            put(cap.at, &[reg::CAP_MSI, cap.next]);
            let control = if cap.is64 { reg::MSI_64BIT } else { 0 };
            put(cap.at + reg::MSI_CONTROL, &control.to_le_bytes());
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
        let msi = self.msi;
        let writable = |at: u16| -> Option<u8> {
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
