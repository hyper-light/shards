//! The I/O APIC, for a backend whose hypervisor emulates only the local APICs (WHP): the
//! guest's view of it at `layout::IOAPIC`, and its pins, which the machine's devices drive
//! (`hv::Irqs`). Each interrupt it sends is a message the backend hands its hypervisor;
//! each EOI of a level-triggered vector comes back from the hypervisor to clear its pin's
//! remote IRR (PM M198). After the Intel 82093AA I/O APIC datasheet (1996) §3, as KVM's
//! in-kernel IOAPIC presents it to guests (virt/kvm/ioapic.c): version 0x11, 24 pins,
//! polarity left to the devices, which assert their lines active-high.

use std::sync::Mutex;

use crate::sync::lock;

/// Pins, and so redirection entries.
pub const PINS: usize = 24;
/// IOAPICVER: version 0x11, entries 0..=23.
const VERSION: u32 = 0x11 | ((PINS as u32 - 1) << 16);

const IOREGSEL: u64 = 0x00;
const IOWIN: u64 = 0x10;
/// Version 0x20's EOI register, which KVM's takes from any guest and Linux writes to none of
/// version 0x11.
const EOI: u64 = 0x40;

const MASKED: u64 = 1 << 16;
const LEVEL: u64 = 1 << 15;
const REMOTE_IRR: u64 = 1 << 14;
const DELIVERY_STATUS: u64 = 1 << 12;
/// What a guest's write to an entry cannot change.
const READ_ONLY: u64 = REMOTE_IRR | DELIVERY_STATUS;

/// An interrupt the IOAPIC sends, from a redirection entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Message {
    pub vector: u8,
    /// The delivery mode, bits 10:8: 0 fixed, 1 lowest priority, 2 SMI, 4 NMI, 5 INIT,
    /// 7 ExtINT.
    pub delivery: u8,
    /// Logical destination mode, else physical.
    pub logical: bool,
    /// Level-triggered, else edge-triggered.
    pub level: bool,
    pub destination: u8,
}

impl Message {
    fn of(entry: u64) -> Message {
        Message {
            vector: entry as u8,
            delivery: ((entry >> 8) & 7) as u8,
            logical: entry & (1 << 11) != 0,
            level: entry & LEVEL != 0,
            destination: (entry >> 56) as u8,
        }
    }
}

/// The messages one change sends, at most one a pin: kept off the heap, sent once the lock
/// is let go.
#[derive(Debug)]
pub struct Sent {
    messages: [Option<Message>; PINS],
}

impl Sent {
    fn none() -> Sent {
        Sent {
            messages: [None; PINS],
        }
    }

    fn push(&mut self, pin: usize, message: Message) {
        if let Some(slot) = self.messages.get_mut(pin) {
            *slot = Some(message);
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = Message> + '_ {
        self.messages.iter().flatten().copied()
    }
}

#[derive(Debug)]
struct State {
    select: u32,
    id: u32,
    entries: [u64; PINS],
    /// Each pin's line as its device drives it.
    lines: [bool; PINS],
}

#[derive(Debug)]
pub struct Ioapic {
    state: Mutex<State>,
}

impl Default for Ioapic {
    /// As after reset: every entry masked.
    fn default() -> Ioapic {
        Ioapic {
            state: Mutex::new(State {
                select: 0,
                id: 0,
                entries: [MASKED; PINS],
                lines: [false; PINS],
            }),
        }
    }
}

impl State {
    /// Pin `pin`'s interrupt, if its line, entry and remote IRR let one go now: an edge
    /// when its line rises (`rose`), a level for as long as it is asserted and unacknowledged.
    fn service(&mut self, pin: usize, rose: bool) -> Option<Message> {
        let line = *self.lines.get(pin)?;
        let entry = self.entries.get_mut(pin)?;
        if *entry & MASKED != 0 || !line {
            return None;
        }
        if *entry & LEVEL != 0 {
            if *entry & REMOTE_IRR != 0 {
                return None;
            }
            *entry |= REMOTE_IRR;
        } else if !rose {
            return None;
        }
        Some(Message::of(*entry))
    }

    fn read(&self, register: u32) -> u32 {
        match register {
            0 => self.id,
            1 => VERSION,
            2 => self.id,
            0x10..=0x3F => {
                let pin = ((register - 0x10) / 2) as usize;
                let entry = self.entries.get(pin).copied().unwrap_or(MASKED);
                if register & 1 == 0 {
                    entry as u32
                } else {
                    (entry >> 32) as u32
                }
            }
            _ => 0,
        }
    }

    fn write(&mut self, register: u32, value: u32, sent: &mut Sent) {
        match register {
            0 => self.id = value & 0x0F00_0000,
            0x10..=0x3F => {
                let pin = ((register - 0x10) / 2) as usize;
                let Some(entry) = self.entries.get_mut(pin) else {
                    return;
                };
                let old = *entry;
                let new = if register & 1 == 0 {
                    (old & 0xFFFF_FFFF_0000_0000) | u64::from(value)
                } else {
                    (old & 0xFFFF_FFFF) | u64::from(value) << 32
                };
                *entry = (new & !READ_ONLY) | (old & READ_ONLY);
                // An entry made edge-triggered has no remote IRR (82093AA §3.2.4).
                if *entry & LEVEL == 0 {
                    *entry &= !REMOTE_IRR;
                }
                // A level-triggered line still asserted goes out as its entry is unmasked;
                // an edge that came while it was masked is lost (82093AA §3.2.4, bit 16).
                if old & MASKED != 0
                    && *entry & MASKED == 0
                    && let Some(m) = self.service(pin, false)
                {
                    sent.push(pin, m);
                }
            }
            _ => {}
        }
    }

    fn eoi(&mut self, vector: u8, sent: &mut Sent) {
        for pin in 0..PINS {
            let Some(entry) = self.entries.get_mut(pin) else {
                continue;
            };
            if *entry & LEVEL == 0 || *entry & REMOTE_IRR == 0 || *entry as u8 != vector {
                continue;
            }
            *entry &= !REMOTE_IRR;
            if let Some(m) = self.service(pin, false) {
                sent.push(pin, m);
            }
        }
    }
}

impl Ioapic {
    /// A guest's read at `offset` in the IOAPIC's page.
    pub fn read(&self, offset: u64, data: &mut [u8]) {
        let st = lock(&self.state);
        let value = match offset & !3 {
            IOREGSEL => st.select,
            IOWIN => st.read(st.select),
            _ => 0,
        };
        let shift = 8 * (offset & 3) as u32;
        let bytes = value.checked_shr(shift).unwrap_or(0).to_le_bytes();
        for (d, b) in data.iter_mut().zip(bytes.iter().chain(std::iter::repeat(&0))) {
            *d = *b;
        }
    }

    /// A guest's write at `offset` in the IOAPIC's page; what it sends.
    pub fn write(&self, offset: u64, data: &[u8]) -> Sent {
        let mut sent = Sent::none();
        let mut word = [0u8; 4];
        for (w, d) in word.iter_mut().zip(data) {
            *w = *d;
        }
        let value = u32::from_le_bytes(word);
        let mut st = lock(&self.state);
        match offset {
            IOREGSEL => st.select = value & 0xFF,
            IOWIN => {
                let register = st.select;
                st.write(register, value, &mut sent);
            }
            EOI => st.eoi(value as u8, &mut sent),
            _ => {}
        }
        sent
    }

    /// Drives pin `pin`'s line; what it sends.
    pub fn set(&self, pin: u32, level: bool) -> Sent {
        let mut sent = Sent::none();
        let pin = pin as usize;
        let mut st = lock(&self.state);
        let Some(line) = st.lines.get_mut(pin) else {
            return sent;
        };
        let rose = level && !*line;
        *line = level;
        if level && let Some(m) = st.service(pin, rose) {
            sent.push(pin, m);
        }
        sent
    }

    /// The EOI of `vector`, broadcast by a local APIC: each level-triggered pin of it
    /// acknowledged, and sent again where its line is still asserted.
    pub fn eoi(&self, vector: u8) -> Sent {
        let mut sent = Sent::none();
        lock(&self.state).eoi(vector, &mut sent);
        sent
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn write_entry(io: &Ioapic, pin: u32, entry: u64) -> Vec<Message> {
        // Linux's order: the high half, then the low (arch/x86/kernel/apic/io_apic.c).
        io.write(IOREGSEL, &(0x11 + 2 * pin).to_le_bytes());
        let mut sent: Vec<Message> = io
            .write(IOWIN, &((entry >> 32) as u32).to_le_bytes())
            .iter()
            .collect();
        io.write(IOREGSEL, &(0x10 + 2 * pin).to_le_bytes());
        sent.extend(io.write(IOWIN, &(entry as u32).to_le_bytes()).iter());
        sent
    }

    fn read_register(io: &Ioapic, register: u32) -> u32 {
        io.write(IOREGSEL, &register.to_le_bytes());
        let mut d = [0u8; 4];
        io.read(IOWIN, &mut d);
        u32::from_le_bytes(d)
    }

    /// As reset: version 0x11 with 24 entries, every entry masked; the ID as written.
    #[test]
    fn it_reads_as_an_82093aa_with_24_pins() {
        let io = Ioapic::default();
        assert_eq!(read_register(&io, 1), 0x0017_0011);
        for pin in 0..PINS as u32 {
            assert_eq!(read_register(&io, 0x10 + 2 * pin), 1 << 16, "pin {pin}");
            assert_eq!(read_register(&io, 0x11 + 2 * pin), 0);
        }
        io.write(IOREGSEL, &0u32.to_le_bytes());
        io.write(IOWIN, &0x0300_0000u32.to_le_bytes());
        assert_eq!(read_register(&io, 0), 0x0300_0000);
        let mut sel = [0u8; 4];
        io.read(IOREGSEL, &mut sel);
        assert_eq!(u32::from_le_bytes(sel), 0);
    }

    /// An edge-triggered pin sends once per rising edge, and nothing while masked, nor
    /// later for an edge that came then.
    #[test]
    fn an_edge_goes_once_per_rise_and_not_while_masked() {
        let io = Ioapic::default();
        let entry = 0x0300_0000_0000_0041u64; // vector 0x41, fixed, physical, to APIC 3
        assert!(io.set(5, true).iter().next().is_none(), "masked");
        io.set(5, false);
        assert!(
            write_entry(&io, 5, entry).is_empty(),
            "the edge while masked is lost"
        );
        let sent: Vec<Message> = io.set(5, true).iter().collect();
        assert_eq!(
            sent,
            [Message {
                vector: 0x41,
                delivery: 0,
                logical: false,
                level: false,
                destination: 3
            }]
        );
        assert!(io.set(5, true).iter().next().is_none(), "no rise, no edge");
        io.set(5, false);
        assert_eq!(io.set(5, true).iter().count(), 1);
        // An EOI of its vector does nothing to an edge-triggered pin.
        assert!(io.eoi(0x41).iter().next().is_none());
    }

    /// A level-triggered pin sends while asserted and unacknowledged: once, then again at
    /// each EOI of its vector while still asserted; its entry shows remote IRR meanwhile,
    /// which a guest cannot write; unmasking an asserted line sends it.
    #[test]
    fn a_level_goes_until_acknowledged_and_again_while_asserted() {
        let io = Ioapic::default();
        let entry = LEVEL | MASKED | 0x51;
        assert!(write_entry(&io, 4, entry).is_empty());
        assert!(io.set(4, true).iter().next().is_none(), "masked");
        let sent = write_entry(&io, 4, entry & !MASKED);
        assert_eq!(sent.len(), 1, "unmasked while asserted");
        assert!(sent.iter().all(|m| m.level && m.vector == 0x51));
        assert_ne!(read_register(&io, 0x18) & REMOTE_IRR as u32, 0);
        // Not again until acknowledged, whatever the line does meanwhile.
        assert!(io.set(4, true).iter().next().is_none());
        // A guest's write of remote IRR is ignored.
        assert!(write_entry(&io, 4, entry & !MASKED).is_empty());
        assert_ne!(read_register(&io, 0x18) & REMOTE_IRR as u32, 0);
        // Another vector's EOI is not this pin's.
        assert!(io.eoi(0x52).iter().next().is_none());
        // Its EOI, still asserted: again.
        assert_eq!(io.eoi(0x51).iter().count(), 1);
        // Its EOI, deasserted: acknowledged, and nothing sent.
        io.set(4, false);
        assert!(io.eoi(0x51).iter().next().is_none());
        assert_eq!(read_register(&io, 0x18) & REMOTE_IRR as u32, 0);
        // The next assertion goes at once.
        assert_eq!(io.set(4, true).iter().count(), 1);
        // The version 0x20 EOI register acknowledges too.
        io.set(4, false);
        assert!(io.write(EOI, &0x51u32.to_le_bytes()).iter().next().is_none());
        assert_eq!(read_register(&io, 0x18) & REMOTE_IRR as u32, 0);
    }

    /// Every field of an entry reaches its message: delivery mode, logical destination,
    /// and an 8-bit destination.
    #[test]
    fn messages_carry_their_entries_fields() {
        let io = Ioapic::default();
        let entry = 0xFF00_0000_0000_0000 | 1 << 11 | 1 << 8 | 0x60;
        write_entry(&io, 23, entry);
        let sent: Vec<Message> = io.set(23, true).iter().collect();
        assert_eq!(
            sent,
            [Message {
                vector: 0x60,
                delivery: 1,
                logical: true,
                level: false,
                destination: 0xFF
            }]
        );
        // Past the last pin, nothing.
        assert!(io.set(24, true).iter().next().is_none());
        assert_eq!(read_register(&io, 0x40), 0);
    }

    /// A byte read of a register's upper bytes, as no guest of ours makes but any may.
    #[test]
    fn narrower_reads_take_their_bytes() {
        let io = Ioapic::default();
        io.write(IOREGSEL, &1u32.to_le_bytes());
        let mut b = [0u8; 1];
        io.read(IOWIN + 2, &mut b);
        assert_eq!(b, [0x17]);
        let mut wide = [0xAAu8; 8];
        io.read(IOWIN, &mut wide);
        assert_eq!(wide, [0x11, 0, 0x17, 0, 0, 0, 0, 0]);
    }
}
