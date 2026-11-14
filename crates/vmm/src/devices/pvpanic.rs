//! pvpanic (QEMU docs/specs/pvpanic.rst; Linux drivers/misc/pvpanic): a one-byte register
//! through which the guest kernel tells the host that it panicked, from its panic notifier,
//! which runs first of all (`.priority = INT_MAX`, pvpanic.c), before a `panic=-1` resets
//! it. A read says which events the device takes; a write is an event. The host learns of
//! a panic as a panic, where a reset alone says nothing of why. What it was told is the
//! console's to report with what the guest said ([`Port::panicked`]).

use std::sync::Arc;

use super::virtio::console::Port;
use super::{MmioDevice, put_le};
use crate::snapshot::codec::{self, Reader, Writer};

/// PVPANIC_PANICKED (include/uapi/misc/pvpanic.h): the one event this device takes.
/// CRASH_LOADED is a kdump kernel's, which shards' guests have none of (`CONFIG_CRASH_DUMP`
/// is not set), and SHUTDOWN is told by the power-off a guest asks for anyway (ACPI's sleep
/// register, PSCI's SYSTEM_OFF).
pub const PANICKED: u8 = 1 << 0;

#[derive(Debug)]
pub struct PvPanic {
    port: Arc<Port>,
}

impl PvPanic {
    /// A device that tells `port`, its machine's console, of a panic.
    pub fn new(port: Arc<Port>) -> PvPanic {
        PvPanic { port }
    }
}

impl MmioDevice for PvPanic {
    fn read(&self, offset: u64, data: &mut [u8]) {
        put_le(data, if offset == 0 { u64::from(PANICKED) } else { 0 });
    }

    fn write(&self, offset: u64, data: &[u8]) {
        if offset == 0 && data.first().is_some_and(|v| v & PANICKED != 0) && !self.port.tell_panicked(true) {
            crate::warn!("the guest's kernel panicked");
        }
    }

    fn pause(&self) {}

    fn resume(&self) -> Result<(), String> {
        Ok(())
    }

    fn save(&self, w: &mut Writer) {
        w.bool(self.port.panicked());
    }

    fn restore(&self, r: &mut Reader<'_>) -> codec::Result<()> {
        self.port.tell_panicked(r.bool()?);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::virtio::console::Console;

    /// The device says it takes panics alone; a write of PANICKED is told, and nothing
    /// else is.
    #[test]
    fn a_panic_written_is_told_and_nothing_else() {
        let (_console, port) = Console::new(Box::new(std::io::sink()), 0).unwrap();
        let p = PvPanic::new(port.clone());
        let mut b = [0xffu8];
        p.read(0, &mut b);
        assert_eq!(b, [PANICKED]);
        p.write(0, &[1 << 1 | 1 << 2]);
        p.write(1, &[PANICKED]);
        assert!(
            !port.panicked(),
            "a kdump's or shutdown's event, or another offset, is no panic"
        );
        p.write(0, &[PANICKED]);
        assert!(port.panicked());
    }
}
