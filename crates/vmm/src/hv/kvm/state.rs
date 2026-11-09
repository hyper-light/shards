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
    /// The TSC's offset from the host's (KVM_VCPU_TSC_OFFSET), where KVM has it: what
    /// keeps each vCPU's TSC as far from the others' as it was.
    pub tsc_offset: Option<u64>,
}

/// The VM's state: the in-kernel interrupt controllers and kvmclock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmState {
    pub pic_master: Vec<u8>,
    pub pic_slave: Vec<u8>,
    pub ioapic: Vec<u8>,
    pub clock: Vec<u8>,
}

/// What a decoder takes, each the most KVM gives or takes. Each structure is its ioctl's
/// own size ([`sys`]'s, asserted against the ioctl numbers). The XSAVE area is at least
/// `struct kvm_xsave`'s and at most what its CPUID says ([`xsave_most`]).
/// CPUID: KVM_MAX_CPUID_ENTRIES ([`sys::MAX_CPUID_ENTRIES`]).
const MAX_CPUID: usize = sys::MAX_CPUID_ENTRIES;
/// MSRs: fewer than KVM's MAX_IO_MSRS, 256, which KVM_GET_MSRS and KVM_SET_MSRS refuse a
/// list of (arch/x86/kvm/x86.c l. 93 and 4690-4692, `msr_io`; Linux 1590cf03): a save reads
/// the list in one call, and a restore sets it in one.
const MAX_MSRS: usize = 256 - 1;

/// The most bytes a vCPU of `cpuid` has in its XSAVE area: CPUID leaf 0DH's ECX, "the size
/// required by the XSAVE/XRSTOR save area for all supported features" (Intel SDM Vol. 2A,
/// CPUID), which KVM gives its guests as the size of the components it permits
/// (arch/x86/kvm/cpuid.c l. 1543-1545) and its KVM_CAP_XSAVE2 says the same of, never
/// less than `struct kvm_xsave`'s 4096 (arch/x86/kvm/x86.c l. 4955-4959; Linux 1590cf03).
/// A CPU without the leaf has the 4096.
fn xsave_most(cpuid: &[[u32; 7]]) -> usize {
    cpuid
        .iter()
        .find(|e| e[0] == 0xd && e[1] == 0)
        .map_or(sys::XSAVE_SIZE, |e| (e[5] as usize).max(sys::XSAVE_SIZE))
}

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
        match self.tsc_offset {
            Some(offset) => {
                w.u8(1);
                w.u64(offset);
            }
            None => w.u8(0),
        }
    }

    pub fn decode(r: &mut Reader<'_>) -> codec::Result<VcpuState> {
        let cpuid = r.seq(MAX_CPUID, 28, |r| {
            let mut e = [0u32; 7];
            for v in &mut e {
                *v = r.u32()?;
            }
            Ok(e)
        })?;
        let mp_state = r.u32()?;
        let xsave_most = xsave_most(&cpuid);
        let mut blob = |least: usize, most: usize| -> codec::Result<Vec<u8>> {
            let b = r.bytes(most)?;
            if b.len() < least {
                return Err(DecodeError(format!(
                    "a state structure of {} bytes, not {least}",
                    b.len()
                )));
            }
            Ok(b.to_vec())
        };
        let (regs_len, sregs_len) = (size_of::<sys::kvm_regs>(), size_of::<sys::kvm_sregs>());
        let regs = blob(regs_len, regs_len)?;
        let sregs = blob(sregs_len, sregs_len)?;
        let xsave = blob(sys::XSAVE_SIZE, xsave_most)?;
        let xcrs = blob(sys::XCRS_SIZE, sys::XCRS_SIZE)?;
        let debugregs = blob(sys::DEBUGREGS_SIZE, sys::DEBUGREGS_SIZE)?;
        let lapic = blob(sys::LAPIC_SIZE, sys::LAPIC_SIZE)?;
        let msrs = r.seq(MAX_MSRS, 12, |r| Ok((r.u32()?, r.u64()?)))?;
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
            tsc_offset: match r.u8()? {
                0 => None,
                1 => Some(r.u64()?),
                other => return Err(DecodeError(format!("a TSC offset marked {other}"))),
            },
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
            // Leaf 0xD's ECX: AMX's XSAVE area, 11008 bytes.
            cpuid: vec![
                [1, 0, 0, 0x806f8, 0x800, 0xfffa3203, 0x178bfbff],
                [0xd, 0, 1, 0x602e7, 0x2b00, 11008, 0],
            ],
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
            tsc_offset: Some(u64::MAX - 5),
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
        // A host without TSC offsets keeps none, and a marker that is neither is damage.
        let mut old = vcpu();
        old.tsc_offset = None;
        let mut w = Writer::default();
        old.encode(&mut w);
        let mut bytes = w.into_bytes();
        assert_eq!(VcpuState::decode(&mut Reader::new(&bytes)).unwrap(), old);
        if let Some(marker) = bytes.last_mut() {
            *marker = 2;
        }
        assert!(VcpuState::decode(&mut Reader::new(&bytes)).is_err());
    }

    /// An XSAVE area is no longer than its CPUID says, and no shorter than KVM's least; a
    /// CPU without leaf 0xD has the least. MSRs are fewer than KVM takes in one call.
    #[test]
    fn a_decoders_bounds_are_what_kvm_gives() {
        let decodes = |st: &VcpuState| {
            let mut w = Writer::default();
            st.encode(&mut w);
            VcpuState::decode(&mut Reader::new(&w.into_bytes())).is_ok()
        };
        let mut st = vcpu();
        st.xsave.push(0);
        assert!(!decodes(&st));
        st.cpuid.retain(|e| e[0] != 0xd);
        assert!(!decodes(&st));
        st.xsave = vec![3; sys::XSAVE_SIZE];
        assert!(decodes(&st));
        st.xsave.pop();
        assert!(!decodes(&st));

        let mut st = vcpu();
        st.msrs = (0..MAX_MSRS as u32).map(|i| (i, 0)).collect();
        assert!(decodes(&st));
        st.msrs.push((0xc000_0080, 0));
        assert!(!decodes(&st));
    }
}
