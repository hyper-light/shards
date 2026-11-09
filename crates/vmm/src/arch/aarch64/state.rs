//! arm64 machine state for snapshots. Each vCPU's is its backend's own: Hypervisor.framework's
//! registers ([`VcpuState`]), system registers keyed by their op0:op1:CRn:CRm:op2 encoding,
//! which HVF's `hv_sys_reg_t` and `hv_gic_icc_reg_t` and KVM's `ARM64_SYS_REG` share
//! (ground-truth doc §5 row 16), or every register KVM lists, by its ID (hv::kvm, D154). The
//! GIC device is the backend's own serialization too: a snapshot restores only on the
//! backend that took it.

use super::{Entry, layout};
use crate::snapshot::codec::{DecodeError, Reader, Result, Writer};

/// A vCPU's state as a snapshot keeps it, in its backend's terms.
pub trait VcpuCodec: Sized {
    fn encode(&self, w: &mut Writer);
    fn decode(r: &mut Reader<'_>) -> Result<Self>;
}

/// A vCPU's PSCI power state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Power {
    Off,
    On,
    /// CPU_ON accepted; the vCPU has not yet entered at `Entry`.
    Pending(Entry),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VcpuState {
    pub x: [u64; 31],
    pub pc: u64,
    pub pstate: u64,
    pub v: [u128; 32],
    pub fpcr: u64,
    pub fpsr: u64,
    /// System registers, SP_EL0/SP_EL1/ELR_EL1/SPSR_EL1 and the EL1 timers included.
    pub sys: Vec<(u16, u64)>,
    /// GIC CPU interface registers by encoding.
    pub icc: Vec<(u16, u64)>,
    pub power: Power,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineState<V> {
    /// The guest's virtual counter (CNTVCT_EL0) when the VM paused. Restore keeps it
    /// continuous: every vCPU gets one counter offset.
    pub counter: u64,
    /// ID registers of the CPU the snapshot was taken on. Restore refuses a CPU that
    /// reports anything different (ground-truth doc §5 row 21).
    pub cpu_id: Vec<(u16, u64)>,
    /// The GIC device: distributor, redistributors and what the backend holds besides,
    /// in the backend's own serialization (HVF: hv_gic_state; KVM: its registers).
    pub gic: Vec<u8>,
    pub vcpus: Vec<V>,
}

// What a decoder takes: what an arm64 machine of any backend has.
/// vCPUs: as many as its memory map has redistributors for ([`layout::MAX_VCPUS`]).
const MAX_VCPUS: usize = layout::MAX_VCPUS as usize;
/// System registers and the CPU interface's, each keyed by its encoding, op0:op1:CRn:CRm:op2
/// of 2, 3, 4, 4 and 3 bits (Arm ARM, DDI 0487, C5.3, "A64 System instruction class
/// encoding"; the MRS and MSR forms, C6.2): a list longer than the 2^16 encodings names one
/// twice, and is no vCPU's.
const SYS_REG_ENCODINGS: usize = 1 << 16;
/// ID registers: the Feature ID space, op0 3, op1 0, 1 or 3, CRn 0, CRm 0 to 7, op2 0 to
/// 7 (DDI 0487J.a, D19.2.66, as Linux arch/arm64/include/uapi/asm/kvm.h l. 536-554 quotes it,
/// KVM_ARM_FEATURE_ID_RANGE_SIZE): 192 registers.
const FEATURE_ID_SPACE: usize = 3 * 8 * 8;
// The GIC's state is its backend's own serialization, as long as the input holds: each
// backend's restore bounds it by what that backend makes (KVM's writes of its vCPUs,
// hv::kvm::aarch64::gic; WHP's WinHvPlatformDefs.h layouts, hv::whp_arm64), and
// Hypervisor.framework refuses a state not its own (hv_gic_set_state, hv_gic_state.h).

fn put_pairs16(w: &mut Writer, v: &[(u16, u64)]) {
    w.seq(v, |w, &(k, x)| {
        w.u16(k);
        w.u64(x);
    });
}

fn get_pairs16(r: &mut Reader<'_>, max: usize) -> Result<Vec<(u16, u64)>> {
    r.seq(max, 10, |r| Ok((r.u16()?, r.u64()?)))
}

impl VcpuCodec for VcpuState {
    fn encode(&self, w: &mut Writer) {
        self.x.iter().for_each(|&v| w.u64(v));
        w.u64(self.pc);
        w.u64(self.pstate);
        self.v.iter().for_each(|&v| w.u128(v));
        w.u64(self.fpcr);
        w.u64(self.fpsr);
        put_pairs16(w, &self.sys);
        put_pairs16(w, &self.icc);
        match self.power {
            Power::Off => w.u8(0),
            Power::On => w.u8(1),
            Power::Pending(e) => {
                w.u8(2);
                w.u64(e.pc);
                w.u64(e.x0);
            }
        }
    }

    fn decode(r: &mut Reader<'_>) -> Result<VcpuState> {
        let mut x = [0u64; 31];
        for v in &mut x {
            *v = r.u64()?;
        }
        let (pc, pstate) = (r.u64()?, r.u64()?);
        let mut v = [0u128; 32];
        for q in &mut v {
            *q = r.u128()?;
        }
        let (fpcr, fpsr) = (r.u64()?, r.u64()?);
        let sys = get_pairs16(r, SYS_REG_ENCODINGS)?;
        let icc = get_pairs16(r, SYS_REG_ENCODINGS)?;
        let power = match r.u8()? {
            0 => Power::Off,
            1 => Power::On,
            2 => Power::Pending(Entry {
                pc: r.u64()?,
                x0: r.u64()?,
            }),
            p => return Err(DecodeError(format!("power state {p}"))),
        };
        Ok(VcpuState {
            x,
            pc,
            pstate,
            v,
            fpcr,
            fpsr,
            sys,
            icc,
            power,
        })
    }
}

impl<V: VcpuCodec> MachineState<V> {
    pub fn encode(&self, w: &mut Writer) {
        w.u64(self.counter);
        put_pairs16(w, &self.cpu_id);
        w.bytes(&self.gic);
        w.seq(&self.vcpus, |w, v| v.encode(w));
    }

    pub fn decode(r: &mut Reader<'_>) -> Result<MachineState<V>> {
        Ok(MachineState {
            counter: r.u64()?,
            cpu_id: get_pairs16(r, FEATURE_ID_SPACE)?,
            gic: r.bytes(usize::MAX)?.to_vec(),
            vcpus: r.seq(MAX_VCPUS, 1, V::decode)?,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn sample() -> MachineState<VcpuState> {
        let vcpu = |i: u64, power| VcpuState {
            x: std::array::from_fn(|n| i * 100 + n as u64),
            pc: 0x8000_1000 + i,
            pstate: 0x3c5,
            v: std::array::from_fn(|n| u128::MAX - n as u128),
            fpcr: 1,
            fpsr: 2,
            sys: vec![(0xc080, 0x30d0_1805), (0xdf1a, 12345)],
            icc: vec![(0xc230, 0xf0)],
            power,
        };
        MachineState {
            counter: 0x1234_5678_9abc,
            cpu_id: vec![(0xc000, 0x610f_0000)],
            gic: vec![0x13, 0, 0x61, 7],
            vcpus: vec![
                vcpu(0, Power::On),
                vcpu(1, Power::Off),
                vcpu(
                    2,
                    Power::Pending(Entry {
                        pc: 0x8000_2000,
                        x0: 7,
                    }),
                ),
            ],
        }
    }

    #[test]
    fn round_trips() {
        let s = sample();
        let mut w = Writer::default();
        s.encode(&mut w);
        let bytes = w.into_bytes();
        let mut r = Reader::new(&bytes);
        assert_eq!(MachineState::decode(&mut r).unwrap(), s);
        r.finish().unwrap();
    }

    #[test]
    fn every_truncation_and_corruption_is_an_error_or_a_value_never_a_panic() {
        let mut w = Writer::default();
        sample().encode(&mut w);
        let bytes = w.into_bytes();
        for cut in 0..bytes.len() {
            assert!(MachineState::<VcpuState>::decode(&mut Reader::new(&bytes[..cut])).is_err());
        }
        for i in 0..bytes.len() {
            let mut b = bytes.clone();
            b[i] ^= 0xff;
            let _ = MachineState::<VcpuState>::decode(&mut Reader::new(&b));
        }
    }

    /// A state is refused past what a machine has: more vCPUs than its redistributors, more
    /// ID registers than the Feature ID space, more system registers than their encodings.
    #[test]
    fn a_state_past_what_a_machine_has_is_refused() {
        let decodes = |s: &MachineState<VcpuState>| {
            let mut w = Writer::default();
            s.encode(&mut w);
            MachineState::<VcpuState>::decode(&mut Reader::new(&w.into_bytes())).is_ok()
        };
        assert_eq!(MAX_VCPUS, 123);
        let mut s = sample();
        s.vcpus = vec![s.vcpus[0].clone(); MAX_VCPUS];
        assert!(decodes(&s));
        s.vcpus.push(s.vcpus[0].clone());
        assert!(!decodes(&s));

        let mut s = sample();
        s.cpu_id = vec![(0xc000, 0); FEATURE_ID_SPACE];
        assert!(decodes(&s));
        s.cpu_id.push((0xc000, 0));
        assert!(!decodes(&s));

        let mut s = sample();
        s.vcpus.truncate(1);
        s.vcpus[0].sys = vec![(0xc080, 0); SYS_REG_ENCODINGS];
        assert!(decodes(&s));
        s.vcpus[0].sys.push((0xc080, 0));
        assert!(!decodes(&s));
    }
}
