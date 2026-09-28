//! ARM PL031 real-time clock (DDI0224C): gives the guest wall-clock time at boot.
//!
//! Only the counter is modelled; the alarm needs an interrupt line the devicetree
//! doesn't describe, and Linux disables the alarm feature without one.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{MmioDevice, get_le, put_le};
use crate::sync::lock;

const DR: u64 = 0x000; // data (current seconds)
const MR: u64 = 0x004; // match
const LR: u64 = 0x008; // load
const CR: u64 = 0x00c; // control
const IMSC: u64 = 0x010;
const RIS: u64 = 0x014;
const MIS: u64 = 0x018;
/// PrimeCell identification: PeriphID0-3 then CellID0-3. Linux matches
/// 0x00041031 under mask 0x000fffff and requires CellID 0xB105F00D.
const ID_BASE: u64 = 0xfe0;
const ID_BYTES: [u8; 8] = [0x31, 0x10, 0x04, 0x00, 0x0d, 0xf0, 0x05, 0xb1];

#[derive(Debug, Default)]
struct State {
    /// Guest seconds = host seconds + offset.
    offset: i64,
    mr: u32,
    imsc: u32,
}

#[derive(Debug, Default)]
pub struct Pl031 {
    state: Mutex<State>,
}

fn host_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl MmioDevice for Pl031 {
    fn read(&self, offset: u64, data: &mut [u8]) {
        let s = lock(&self.state);
        let now = host_seconds().wrapping_add(s.offset) as u32;
        let v: u32 = match offset {
            DR | LR => now,
            MR => s.mr,
            CR => 1, // always enabled
            IMSC => s.imsc,
            RIS | MIS => 0,
            o if (ID_BASE..ID_BASE + 32).contains(&o) && o.is_multiple_of(4) => ID_BYTES
                .get(((o - ID_BASE) / 4) as usize)
                .copied()
                .map_or(0, u32::from),
            _ => 0,
        };
        put_le(data, u64::from(v));
    }

    fn write(&self, offset: u64, data: &[u8]) {
        let v = get_le(data) as u32;
        let mut s = lock(&self.state);
        match offset {
            LR => s.offset = i64::from(v).wrapping_sub(host_seconds()),
            MR => s.mr = v,
            IMSC => s.imsc = v & 1,
            _ => {}
        }
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn rd(r: &Pl031, off: u64) -> u32 {
        let mut b = [0u8; 4];
        r.read(off, &mut b);
        u32::from_le_bytes(b)
    }

    #[test]
    fn identifies_as_pl031_and_tracks_host_time() {
        let r = Pl031::default();
        let pid = (0..4)
            .map(|i| rd(&r, ID_BASE + 4 * i) << (8 * i))
            .fold(0, |a, b| a | b);
        let cid = (0..4)
            .map(|i| rd(&r, ID_BASE + 16 + 4 * i) << (8 * i))
            .fold(0, |a, b| a | b);
        assert_eq!(pid & 0x000f_ffff, 0x0004_1031);
        assert_eq!(cid, 0xb105_f00d);
        let now = host_seconds() as u32;
        assert!(rd(&r, DR).abs_diff(now) <= 1);
        r.write(LR, &1000u32.to_le_bytes());
        assert!(rd(&r, DR).abs_diff(1000) <= 1);
    }
}
