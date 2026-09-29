//! arm64 machine state for snapshots, in backend-neutral terms. System registers are keyed
//! by their op0:op1:CRn:CRm:op2 encoding and GIC registers by their GICv3 offset. HVF's
//! `hv_sys_reg_t`/`hv_gic_*_reg_t` and KVM's `ARM64_SYS_REG`/vGIC attributes use the same keys
//! (ground-truth doc §5 row 16).

use super::Entry;
use crate::snapshot::codec::{DecodeError, Reader, Result, Writer};

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
    /// GIC redistributor registers (SGI/PPI state) by GICR offset.
    pub redist: Vec<(u32, u64)>,
    /// GIC CPU interface registers by encoding.
    pub icc: Vec<(u16, u64)>,
    pub power: Power,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineState {
    /// The guest's virtual counter (CNTVCT_EL0) when the VM paused. Restore keeps it
    /// continuous: every vCPU gets one counter offset.
    pub counter: u64,
    /// ID registers of the CPU the snapshot was taken on. Restore refuses a CPU that
    /// reports anything different (ground-truth doc §5 row 21).
    pub cpu_id: Vec<(u16, u64)>,
    /// GIC distributor registers by GICD offset.
    pub dist: Vec<(u32, u64)>,
    pub vcpus: Vec<VcpuState>,
}

// Decoding limits, well above what any backend produces (HVF: ≤ 64 vCPUs, ~1500
// distributor registers).
const MAX_VCPUS: usize = 1024;
const MAX_SYS: usize = 512;
const MAX_REDIST: usize = 64;
const MAX_ICC: usize = 32;
const MAX_DIST: usize = 8192;
const MAX_ID: usize = 64;

fn put_pairs16(w: &mut Writer, v: &[(u16, u64)]) {
    w.seq(v, |w, &(k, x)| {
        w.u16(k);
        w.u64(x);
    });
}

fn put_pairs32(w: &mut Writer, v: &[(u32, u64)]) {
    w.seq(v, |w, &(k, x)| {
        w.u32(k);
        w.u64(x);
    });
}

fn get_pairs16(r: &mut Reader<'_>, max: usize) -> Result<Vec<(u16, u64)>> {
    r.seq(max, 10, |r| Ok((r.u16()?, r.u64()?)))
}

fn get_pairs32(r: &mut Reader<'_>, max: usize) -> Result<Vec<(u32, u64)>> {
    r.seq(max, 12, |r| Ok((r.u32()?, r.u64()?)))
}

impl VcpuState {
    fn encode(&self, w: &mut Writer) {
        self.x.iter().for_each(|&v| w.u64(v));
        w.u64(self.pc);
        w.u64(self.pstate);
        self.v.iter().for_each(|&v| w.u128(v));
        w.u64(self.fpcr);
        w.u64(self.fpsr);
        put_pairs16(w, &self.sys);
        put_pairs32(w, &self.redist);
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
        let sys = get_pairs16(r, MAX_SYS)?;
        let redist = get_pairs32(r, MAX_REDIST)?;
        let icc = get_pairs16(r, MAX_ICC)?;
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
            redist,
            icc,
            power,
        })
    }
}

impl MachineState {
    pub fn encode(&self, w: &mut Writer) {
        w.u64(self.counter);
        put_pairs16(w, &self.cpu_id);
        put_pairs32(w, &self.dist);
        w.seq(&self.vcpus, |w, v| v.encode(w));
    }

    pub fn decode(r: &mut Reader<'_>) -> Result<MachineState> {
        Ok(MachineState {
            counter: r.u64()?,
            cpu_id: get_pairs16(r, MAX_ID)?,
            dist: get_pairs32(r, MAX_DIST)?,
            vcpus: r.seq(MAX_VCPUS, 1, VcpuState::decode)?,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn sample() -> MachineState {
        let vcpu = |i: u64, power| VcpuState {
            x: std::array::from_fn(|n| i * 100 + n as u64),
            pc: 0x8000_1000 + i,
            pstate: 0x3c5,
            v: std::array::from_fn(|n| u128::MAX - n as u128),
            fpcr: 1,
            fpsr: 2,
            sys: vec![(0xc080, 0x30d0_1805), (0xdf1a, 12345)],
            redist: vec![(0x1_0100, 0xffff)],
            icc: vec![(0xc230, 0xf0)],
            power,
        };
        MachineState {
            counter: 0x1234_5678_9abc,
            cpu_id: vec![(0xc000, 0x610f_0000)],
            dist: vec![(0x0, 0x13), (0x6100, 0)],
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
            assert!(MachineState::decode(&mut Reader::new(&bytes[..cut])).is_err());
        }
        for i in 0..bytes.len() {
            let mut b = bytes.clone();
            b[i] ^= 0xff;
            let _ = MachineState::decode(&mut Reader::new(&b));
        }
    }
}
