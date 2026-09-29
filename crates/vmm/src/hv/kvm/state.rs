//! The state a snapshot keeps of a KVM VM and its vCPUs: KVM's own structures as their
//! bytes, since a template restores only on the host that saved it, and the MSRs KVM
//! lists, by index. What each holds, and the orders to save and restore it in, are
//! Firecracker's (fc:src/vmm/src/arch/x86_64/vcpu.rs, save_state and restore_state;
//! fc:src/vmm/src/arch/x86_64/vm.rs), after KVM's api.rst.

use super::sys;
use crate::snapshot::codec::{self, DecodeError, Reader, Writer};

/// A vCPU's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VcpuState {
    /// The CPUID the vCPU had, which a restore requires again.
    pub cpuid: Vec<[u32; 7]>,
    pub mp_state: u32,
    pub regs: Vec<u8>,
    pub sregs: Vec<u8>,
    /// XSAVE area: 4096 bytes, or KVM_CAP_XSAVE2's size.
    pub xsave: Vec<u8>,
    pub xcrs: Vec<u8>,
    pub debugregs: Vec<u8>,
    pub lapic: Vec<u8>,
    pub msrs: Vec<(u32, u64)>,
    pub events: Vec<u8>,
    pub tsc_khz: u32,
}

/// The VM's state: the in-kernel interrupt controllers and kvmclock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmState {
    pub pic_master: Vec<u8>,
    pub pic_slave: Vec<u8>,
    pub ioapic: Vec<u8>,
    pub clock: Vec<u8>,
}

/// Bounds on what a decoder accepts: a state blob is at most a few pages; the XSAVE area
/// with AMX about 11 KiB.
const MAX_BLOB: usize = 64 << 10;
const MAX_MSRS: usize = 4096;
const MAX_CPUID: usize = sys::MAX_CPUID_ENTRIES;

impl VcpuState {
    pub fn encode(&self, w: &mut Writer) {
        w.seq(&self.cpuid, |w, e| e.iter().for_each(|&v| w.u32(v)));
        w.u32(self.mp_state);
        for blob in [
            &self.regs,
            &self.sregs,
            &self.xsave,
            &self.xcrs,
            &self.debugregs,
            &self.lapic,
        ] {
            w.bytes(blob);
        }
        w.seq(&self.msrs, |w, &(index, data)| {
            w.u32(index);
            w.u64(data);
        });
        w.bytes(&self.events);
        w.u32(self.tsc_khz);
    }

    pub fn decode(r: &mut Reader<'_>) -> codec::Result<VcpuState> {
        let cpuid = r.seq(MAX_CPUID, |r| {
            let mut e = [0u32; 7];
            for v in &mut e {
                *v = r.u32()?;
            }
            Ok(e)
        })?;
        let mp_state = r.u32()?;
        let mut blob = |len: Option<usize>| -> codec::Result<Vec<u8>> {
            let b = r.bytes(MAX_BLOB)?;
            match len {
                Some(len) if b.len() != len => Err(DecodeError(format!(
                    "a state structure of {} bytes, not {len}",
                    b.len()
                ))),
                _ => Ok(b.to_vec()),
            }
        };
        let regs = blob(Some(size_of::<sys::kvm_regs>()))?;
        let sregs = blob(Some(size_of::<sys::kvm_sregs>()))?;
        let xsave = blob(None)?;
        let xcrs = blob(Some(sys::XCRS_SIZE))?;
        let debugregs = blob(Some(sys::DEBUGREGS_SIZE))?;
        let lapic = blob(Some(sys::LAPIC_SIZE))?;
        if xsave.len() < sys::XSAVE_SIZE {
            return Err(DecodeError(format!("an XSAVE area of {} bytes", xsave.len())));
        }
        let msrs = r.seq(MAX_MSRS, |r| Ok((r.u32()?, r.u64()?)))?;
        let events = r.bytes(sys::VCPU_EVENTS_SIZE)?.to_vec();
        if events.len() != sys::VCPU_EVENTS_SIZE {
            return Err(DecodeError("vCPU events of the wrong size".into()));
        }
        Ok(VcpuState {
            cpuid,
            mp_state,
            regs,
            sregs,
            xsave,
            xcrs,
            debugregs,
            lapic,
            msrs,
            events,
            tsc_khz: r.u32()?,
        })
    }
}

impl VmState {
    pub fn encode(&self, w: &mut Writer) {
        for blob in [&self.pic_master, &self.pic_slave, &self.ioapic, &self.clock] {
            w.bytes(blob);
        }
    }

    pub fn decode(r: &mut Reader<'_>) -> codec::Result<VmState> {
        let mut blob = |len: usize| -> codec::Result<Vec<u8>> {
            let b = r.bytes(len)?;
            if b.len() != len {
                return Err(DecodeError(format!("{} bytes where {len} belong", b.len())));
            }
            Ok(b.to_vec())
        };
        Ok(VmState {
            pic_master: blob(sys::IRQCHIP_SIZE)?,
            pic_slave: blob(sys::IRQCHIP_SIZE)?,
            ioapic: blob(sys::IRQCHIP_SIZE)?,
            clock: blob(sys::CLOCK_SIZE)?,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn vcpu() -> VcpuState {
        VcpuState {
            cpuid: vec![[1, 0, 0, 0x806f8, 0x800, 0xfffa3203, 0x178bfbff]],
            mp_state: 0,
            regs: vec![1; size_of::<sys::kvm_regs>()],
            sregs: vec![2; size_of::<sys::kvm_sregs>()],
            xsave: vec![3; 11008],
            xcrs: vec![4; sys::XCRS_SIZE],
            debugregs: vec![5; sys::DEBUGREGS_SIZE],
            lapic: vec![6; sys::LAPIC_SIZE],
            msrs: vec![(0x10, 1 << 40), (0x6e0, 7)],
            events: vec![8; sys::VCPU_EVENTS_SIZE],
            tsc_khz: 2_400_000,
        }
    }

    #[test]
    fn states_round_trip_and_damage_is_an_error() {
        let vm = VmState {
            pic_master: vec![1; sys::IRQCHIP_SIZE],
            pic_slave: vec![2; sys::IRQCHIP_SIZE],
            ioapic: vec![3; sys::IRQCHIP_SIZE],
            clock: vec![4; sys::CLOCK_SIZE],
        };
        let mut w = Writer::default();
        vm.encode(&mut w);
        vcpu().encode(&mut w);
        let bytes = w.into_bytes();
        let mut r = Reader::new(&bytes);
        assert_eq!(VmState::decode(&mut r).unwrap(), vm);
        assert_eq!(VcpuState::decode(&mut r).unwrap(), vcpu());
        r.finish().unwrap();
        for cut in 0..bytes.len() {
            let mut r = Reader::new(&bytes[..cut]);
            let whole = VmState::decode(&mut r).and_then(|_| VcpuState::decode(&mut r));
            assert!(whole.is_err(), "cut at {cut}");
        }
        let mut short = vcpu();
        short.lapic.pop();
        let mut w = Writer::default();
        short.encode(&mut w);
        assert!(VcpuState::decode(&mut Reader::new(&w.into_bytes())).is_err());
    }
}
