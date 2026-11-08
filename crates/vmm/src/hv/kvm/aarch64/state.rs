//! A vCPU's state as KVM holds it, for snapshots (D154): every register KVM_GET_REG_LIST
//! names, by its ID (api.rst 4.68 and 4.84: the core registers, KVM's firmware
//! pseudo-registers, the system registers, the timers among them, and the cache geometry),
//! its power state, and an SError pending in it. As Firecracker and QEMU keep one: what KVM
//! lists is what a user can read and write back (KVM's get-reg-list self-test holds it to
//! that), and a register a later KVM adds is in it unasked.

use super::super::sys::{self, SError};
use super::{Error, Result, call};
use crate::arch::aarch64::state::VcpuCodec;
use crate::snapshot::codec::{self, DecodeError, Reader, Writer};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VcpuState {
    /// Every register KVM gives a user, by ID, in its list's order, each a value of its
    /// ID's size: 4, 8 or 16 bytes (an SVE vCPU's wider ones are not, as no vCPU of
    /// shards' has SVE).
    pub regs: Vec<(u64, u128)>,
    /// PSCI's power state, which KVM's PSCI keeps (KVM_GET_MP_STATE): runnable, or stopped.
    pub mp_state: u32,
    /// A virtual SError pending, which no register shows.
    pub serror: Option<SError>,
}

/// More registers than any arm64 vCPU has (a few hundred): a state that names more is no
/// vCPU's.
const MAX_REGS: usize = 4096;

/// The counters' registers, which a restore leaves alone: it sets the virtual counter once
/// for the whole VM as its vCPUs are released, the physical one being the host's as on HVF
/// (vm::aarch64, `release_offset`). A write of either sets the VM's offset of it from the
/// host's counter, as of the write (sys_regs.c, arch_timer_set_user).
fn counter(id: u64) -> bool {
    id == sys::REG_TIMER_CNT || id == sys::REG_PTIMER_CNT
}

/// Reads `fd`'s state. Its vCPU must be out of the guest. KVM finishes a PSCI CPU_ON owed to
/// it before a register is read (arm.c, KVM_GET_ONE_REG), so a vCPU powered on and not yet
/// run reads as it will start.
pub(super) fn save(fd: &sys::VcpuFd) -> Result<VcpuState> {
    let mp_state = fd.get_mp_state().map_err(call("KVM_GET_MP_STATE"))?;
    let ids = fd.reg_list().map_err(call("KVM_GET_REG_LIST"))?;
    let mut regs = Vec::with_capacity(ids.len());
    for id in ids {
        if sys::reg_size(id) > 16 {
            return Err(Error::Guest(format!(
                "register {id:#x} is {} bytes: a vCPU with SVE is not snapshotted",
                sys::reg_size(id)
            )));
        }
        let value = fd
            .get_reg_value(id)
            .map_err(|e| Error::Guest(format!("KVM_GET_ONE_REG of {id:#x} failed: {e}")))?;
        regs.push((id, value));
    }
    let serror = fd.serror().map_err(call("KVM_GET_VCPU_EVENTS"))?;
    Ok(VcpuState {
        regs,
        mp_state,
        serror,
    })
}

/// Writes `st` into `fd`'s vCPU, initialized as the snapshot's was and not run since, or run
/// only by the prefetch loop: its registers in KVM's list order, the counters aside
/// ([`counter`]), then its power state, then a pending SError. A register this KVM does not
/// have, or a value it refuses (an ID register naming a feature this CPU lacks), fails the
/// restore: the snapshot was taken on another CPU or another KVM.
pub(super) fn restore(fd: &sys::VcpuFd, st: &VcpuState) -> Result<()> {
    for &(id, value) in st.regs.iter().filter(|(id, _)| !counter(*id)) {
        fd.set_reg_value(id, value)
            .map_err(|e| Error::Guest(format!("KVM_SET_ONE_REG of {id:#x} failed: {e}")))?;
    }
    fd.set_mp_state(st.mp_state).map_err(call("KVM_SET_MP_STATE"))?;
    if let Some(serror) = st.serror {
        fd.set_serror(serror).map_err(call("KVM_SET_VCPU_EVENTS"))?;
    }
    Ok(())
}

impl VcpuCodec for VcpuState {
    fn encode(&self, w: &mut Writer) {
        w.u32(self.mp_state);
        w.seq(&self.regs, |w, &(id, value)| {
            w.u64(id);
            match sys::reg_size(id) {
                4 => w.u32(value as u32),
                8 => w.u64(value as u64),
                _ => w.u128(value),
            }
        });
        match self.serror {
            None => w.u8(0),
            Some(SError { esr: None }) => w.u8(1),
            Some(SError { esr: Some(esr) }) => {
                w.u8(2);
                w.u64(esr);
            }
        }
    }

    fn decode(r: &mut Reader<'_>) -> codec::Result<VcpuState> {
        let mp_state = r.u32()?;
        let regs = r.seq(MAX_REGS, 12, |r| {
            let id = r.u64()?;
            let value = match sys::reg_size(id) {
                4 => u128::from(r.u32()?),
                8 => u128::from(r.u64()?),
                16 => r.u128()?,
                n => return Err(DecodeError(format!("register {id:#x} of {n} bytes"))),
            };
            Ok((id, value))
        })?;
        let serror = match r.u8()? {
            0 => None,
            1 => Some(SError { esr: None }),
            2 => Some(SError { esr: Some(r.u64()?) }),
            s => return Err(DecodeError(format!("SError state {s}"))),
        };
        Ok(VcpuState {
            regs,
            mp_state,
            serror,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn sample() -> VcpuState {
        VcpuState {
            regs: vec![
                // X0, a 64-bit core register.
                (sys::reg_x(0), 0x8000_1000),
                // FPSR, 32 bits; V0, 128.
                (0x6020_0000_0010_00d4, 0x0800_0010),
                (0x6040_0000_0010_0054, u128::MAX - 7),
                (sys::sysreg(0xc080), 0x30d0_1805),
                (sys::REG_TIMER_CNT, 12345),
            ],
            // KVM_MP_STATE_STOPPED: a vCPU PSCI has off.
            mp_state: 5,
            serror: Some(SError {
                esr: Some(0x0200_0000),
            }),
        }
    }

    #[test]
    fn a_vcpus_state_round_trips_each_register_at_its_size() {
        for st in [
            sample(),
            VcpuState {
                serror: None,
                ..sample()
            },
            VcpuState {
                serror: Some(SError { esr: None }),
                ..sample()
            },
        ] {
            let mut w = Writer::default();
            st.encode(&mut w);
            let bytes = w.into_bytes();
            // The count and mp_state, 12 bytes of IDs each, and 8 + 4 + 16 + 8 + 8 of
            // values, then the SError.
            let serror = match st.serror {
                None | Some(SError { esr: None }) => 1,
                Some(_) => 9,
            };
            assert_eq!(bytes.len(), 8 + 5 * 8 + 44 + serror);
            let mut r = Reader::new(&bytes);
            assert_eq!(VcpuState::decode(&mut r).unwrap(), st);
            r.finish().unwrap();
        }
    }

    #[test]
    fn every_truncation_and_corruption_is_an_error_or_a_value_never_a_panic() {
        let mut w = Writer::default();
        sample().encode(&mut w);
        let bytes = w.into_bytes();
        for cut in 0..bytes.len() {
            assert!(VcpuState::decode(&mut Reader::new(&bytes[..cut])).is_err());
        }
        for i in 0..bytes.len() {
            let mut b = bytes.clone();
            b[i] ^= 0xff;
            let _ = VcpuState::decode(&mut Reader::new(&b));
        }
    }

    /// A restore leaves both counters alone, and nothing else.
    #[test]
    fn the_counters_are_the_registers_a_restore_leaves() {
        assert!(counter(sys::REG_TIMER_CNT));
        assert!(counter(sys::REG_PTIMER_CNT));
        // KVM_REG_ARM_TIMER_CVAL and _CTL, the virtual timer's, are restored.
        assert!(!counter(sys::sysreg(0xdf02)));
        assert!(!counter(sys::sysreg(0xdf19)));
    }
}
