//! Just enough of the i8042 keyboard controller for an x86 guest to reset the machine:
//! the status port reads as idle (input buffer empty), and command 0xFE (pulse the reset
//! line) is a reset. Every guest restart, emergency restart and `panic=` path ends there
//! (arch/x86/kernel/reboot.c:522-531, 613-628; docs/research/kvm-x86_64-ground-truth.md
//! §4). No keyboard: the guest binds no i8042 driver (FADT 8042 bit clear, no PNP0303).

use std::sync::Arc;

use super::MmioDevice;
use super::power::{Power, PowerEvent};
use crate::snapshot::codec::{self, Reader, Writer};

/// The command port, relative to the data port (0x60 → 0x64).
const COMMAND: u64 = 4;
const CMD_RESET: u8 = 0xfe;

#[derive(Debug)]
pub struct I8042 {
    power: Arc<Power>,
}

impl I8042 {
    pub fn new(power: Arc<Power>) -> I8042 {
        I8042 { power }
    }
}

impl MmioDevice for I8042 {
    /// Data and status read as zero: output buffer empty, input buffer empty.
    fn read(&self, _offset: u64, data: &mut [u8]) {
        data.fill(0);
    }

    fn write(&self, offset: u64, data: &[u8]) {
        if offset == COMMAND && data.first() == Some(&CMD_RESET) {
            self.power.request(PowerEvent::Reset);
        }
    }

    fn pause(&self) {}

    fn resume(&self) -> Result<(), String> {
        Ok(())
    }

    fn save(&self, _w: &mut Writer) {}

    fn restore(&self, _r: &mut Reader<'_>) -> codec::Result<()> {
        Ok(())
    }
}
