//! Hardware-reduced ACPI sleep registers (ACPI 6.5 §4.8.3.7): SLEEP_CONTROL_REG and
//! SLEEP_STATUS_REG, which the FADT points at. The guest powers off by writing
//! SLP_TYP (from `\_S5`) with SLP_EN to the control register (drivers/acpi/acpica/
//! hwesleep.c:69-135), and then polls the status register forever.

use std::sync::Arc;

use super::MmioDevice;
use super::power::{Power, PowerEvent};
use crate::arch::x86_64::acpi::{S5_SLP_TYP, SLP_EN};
use crate::snapshot::codec::{self, Reader, Writer};

const CONTROL: u64 = 0;

#[derive(Debug)]
pub struct AcpiSleep {
    power: Arc<Power>,
}

impl AcpiSleep {
    pub fn new(power: Arc<Power>) -> AcpiSleep {
        AcpiSleep { power }
    }
}

impl MmioDevice for AcpiSleep {
    /// WAK_STS never sets: the machine does not wake.
    fn read(&self, _offset: u64, data: &mut [u8]) {
        data.fill(0);
    }

    fn write(&self, offset: u64, data: &[u8]) {
        if offset != CONTROL {
            return; // SLEEP_STATUS: writing WAK_STS clears nothing that is set
        }
        if let Some(&v) = data.first()
            && v & SLP_EN != 0
            && (v >> 2) & 0x7 == S5_SLP_TYP
        {
            self.power.request(PowerEvent::Off);
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
    use std::sync::Mutex;

    #[test]
    fn only_s5_with_slp_en_powers_off() {
        let power = Arc::new(Power::default());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        power.on_event(Box::new(move |e| log.lock().unwrap().push(e)));
        let dev = AcpiSleep::new(power);
        dev.write(0, &[(S5_SLP_TYP << 2)]); // no SLP_EN
        dev.write(0, &[(3 << 2) | SLP_EN]); // S3
        dev.write(1, &[(S5_SLP_TYP << 2) | SLP_EN]); // status register
        assert!(seen.lock().unwrap().is_empty());
        dev.write(0, &[(S5_SLP_TYP << 2) | SLP_EN]); // what hwesleep.c writes for S5
        assert_eq!(*seen.lock().unwrap(), vec![PowerEvent::Off]);
    }
}
