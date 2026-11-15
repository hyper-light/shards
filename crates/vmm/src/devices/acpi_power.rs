//! Hardware-reduced ACPI power registers, which the FADT points at: SLEEP_CONTROL_REG and
//! SLEEP_STATUS_REG (ACPI 6.5 §4.8.3.7), and RESET_REG (§4.8.3.6, D268).
//! - The guest powers off by writing SLP_TYP (from `\_S5`) with SLP_EN to the control
//!   register (drivers/acpi/acpica/hwesleep.c:69-135), and then polls the status register
//!   forever.
//! - It resets, on a restart, an emergency restart and a panic, by writing RESET_VALUE to
//!   the reset register (drivers/acpi/reboot.c:36-82, with `reboot=a`).

use std::sync::Arc;

use super::MmioDevice;
use super::power::{Power, PowerEvent};
use crate::arch::x86_64::acpi::{RESET, RESET_VALUE, S5_SLP_TYP, SLP_EN};
use crate::snapshot::codec::{self, Reader, Writer};

const CONTROL: u64 = 0;

#[derive(Debug)]
pub struct AcpiPower {
    power: Arc<Power>,
}

impl AcpiPower {
    pub fn new(power: Arc<Power>) -> AcpiPower {
        AcpiPower { power }
    }
}

impl MmioDevice for AcpiPower {
    /// WAK_STS never sets: the machine does not wake. The reset register reads as zero.
    fn read(&self, _offset: u64, data: &mut [u8]) {
        data.fill(0);
    }

    fn write(&self, offset: u64, data: &[u8]) {
        let Some(&v) = data.first() else { return };
        match offset {
            CONTROL if v & SLP_EN != 0 && (v >> 2) & 0x7 == S5_SLP_TYP => {
                self.power.request(PowerEvent::Off);
            }
            // §4.8.3.6 gives RESET_VALUE alone a meaning: another value resets nothing.
            o if o == u64::from(RESET) && v == RESET_VALUE => {
                crate::warn!("the guest reset its machine through the ACPI reset register");
                self.power.request(PowerEvent::Reset);
            }
            // SLEEP_STATUS: writing WAK_STS clears nothing that is set.
            _ => {}
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::arch::x86_64::acpi::SLEEP_STATUS;
    use std::sync::Mutex;

    fn recorded() -> (AcpiPower, Arc<Mutex<Vec<PowerEvent>>>) {
        let power = Arc::new(Power::default());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        power.on_event(Box::new(move |e| log.lock().unwrap().push(e)));
        (AcpiPower::new(power), seen)
    }

    #[test]
    fn only_s5_with_slp_en_powers_off() {
        let (dev, seen) = recorded();
        dev.write(0, &[(S5_SLP_TYP << 2)]); // no SLP_EN
        dev.write(0, &[(3 << 2) | SLP_EN]); // S3
        dev.write(1, &[(S5_SLP_TYP << 2) | SLP_EN]); // status register
        assert!(seen.lock().unwrap().is_empty());
        dev.write(0, &[(S5_SLP_TYP << 2) | SLP_EN]); // what hwesleep.c writes for S5
        assert_eq!(*seen.lock().unwrap(), vec![PowerEvent::Off]);
    }

    /// What acpi_reset writes to an I/O RESET_REG, RESET_VALUE in one byte
    /// (acpica/hwxface.c l. 60-62), is a reset; any other value there, or RESET_VALUE at
    /// another of the device's registers, is nothing.
    #[test]
    fn reset_value_at_the_reset_register_alone_resets() {
        let (dev, seen) = recorded();
        let reset = u64::from(RESET);
        for v in (0..=u8::MAX).filter(|&v| v != RESET_VALUE) {
            dev.write(reset, &[v]);
        }
        dev.write(CONTROL, &[RESET_VALUE]);
        dev.write(u64::from(SLEEP_STATUS), &[RESET_VALUE]);
        dev.write(reset, &[]);
        assert!(seen.lock().unwrap().is_empty());
        dev.write(reset, &[RESET_VALUE]);
        assert_eq!(*seen.lock().unwrap(), vec![PowerEvent::Reset]);
    }
}
