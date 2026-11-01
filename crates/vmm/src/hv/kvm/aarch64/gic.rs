//! KVM's GICv3 state, for snapshots (D154): its registers as KVM gives them a user
//! (Documentation/virt/kvm/devices/arm-vgic-v3.rst), kept as the writes that make a GIC of
//! a new VM, initialized and with every vCPU, the one saved: in QEMU's order
//! (hw/intc/arm_gicv3_kvm.c, kvm_arm_gicv3_put), the distributor's implementation and
//! control first, then each redistributor's, the private interrupts', then the
//! distributor's of the SPIs, each interrupt's configuration before its line's level, its
//! routing and enable before its pending state, then each CPU interface's. A set-enable
//! and a set-active register come after all-ones written to its clear twin, as a new GIC's
//! SGIs are enabled. The pending registers set the latched state, and their clear twins
//! read and write nothing (arm-vgic-v3.rst), so a write of a pending register is all of its
//! state. LPIs have none here: a machine with an ITS saves no snapshot.

use super::super::sys;
use super::{Error, NR_IRQS, Result};
use crate::snapshot::codec::{DecodeError, Reader, Writer};

/// A redistributor's SGI frame, after its RD frame (arm-vgic-v3.rst; GICv3 §12.10).
const SGI: u64 = 64 << 10;

/// The distributor's registers by offset (GICv3 §12.9).
const GICD_CTLR: u64 = 0x0000;
const GICD_IIDR: u64 = 0x0008;
const GICD_IGROUPR: u64 = 0x0080;
const GICD_ISENABLER: u64 = 0x0100;
const GICD_ICENABLER: u64 = 0x0180;
const GICD_ISPENDR: u64 = 0x0200;
const GICD_ISACTIVER: u64 = 0x0300;
const GICD_ICACTIVER: u64 = 0x0380;
const GICD_IPRIORITYR: u64 = 0x0400;
const GICD_ICFGR: u64 = 0x0c00;
const GICD_IROUTER: u64 = 0x6000;

/// The CPU interface's registers by encoding (op0:op1:CRn:CRm:op2).
const ICC_SRE_EL1: u64 = 0xc665;
const ICC_CTLR_EL1: u64 = 0xc664;
const ICC_IGRPEN0_EL1: u64 = 0xc666;
const ICC_IGRPEN1_EL1: u64 = 0xc667;
const ICC_PMR_EL1: u64 = 0xc230;
const ICC_BPR0_EL1: u64 = 0xc643;
const ICC_BPR1_EL1: u64 = 0xc663;
/// ICC_AP0R<n>_EL1 and ICC_AP1R<n>_EL1, n from 0.
const ICC_AP0R0_EL1: u64 = 0xc644;
const ICC_AP1R0_EL1: u64 = 0xc648;

/// More writes than a GIC of the most vCPUs a machine has makes.
const MAX_WRITES: usize = 1 << 16;

/// One write of the state: a register's attribute group and ID, and its value, 32 bits
/// but for a CPU interface register's 64.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Write {
    group: u32,
    attr: u64,
    value: u64,
}

/// A redistributor's or CPU interface's attribute for the vCPU of MPIDR `mpidr`: its
/// affinity, Aff3 to Aff0, in bits 63:32 (arm-vgic-v3.rst).
fn cpu(mpidr: u64) -> u64 {
    (((mpidr >> 8) & 0xff00_0000) | (mpidr & 0xff_ffff)) << 32
}

/// The input levels of the 32 interrupts from `intid`, of the vCPU `cpu` names where they
/// are its own (KVM_DEV_ARM_VGIC_GRP_LEVEL_INFO, VGIC_LEVEL_INFO_LINE_LEVEL = 0).
fn levels(cpu: u64, intid: u32) -> u64 {
    cpu | u64::from(intid & 0x3ff)
}

/// How many of each kind of active priority register a CPU interface has, by the priority
/// bits its ICC_CTLR_EL1 says (bits 10:8, less one): 1, 2 at 6 bits, 4 at 7 (GICv3
/// §12.2.1, ICC_AP0R<n>_EL1).
fn active_priority_registers(ctlr: u64) -> u64 {
    match ((ctlr >> 8) & 7) + 1 {
        7 => 4,
        6 => 2,
        _ => 1,
    }
}

/// The writes that make a new GIC this one, read from `gic` of a VM whose vCPUs are those
/// of `mpidrs`, every one out of the guest.
pub(super) fn save(gic: &sys::DeviceFd, mpidrs: &[u64]) -> Result<Vec<u8>> {
    let get = |group: u32, attr: u64| -> Result<Write> {
        let value = if group == sys::VGIC_GRP_CPU_SYSREGS {
            gic.get_attr::<u64>(group, attr)
        } else {
            gic.get_attr::<u32>(group, attr).map(u64::from)
        };
        let value = value.map_err(|e| {
            Error::Guest(format!(
                "KVM_GET_DEVICE_ATTR of the GIC's {group}/{attr:#x} failed: {e}"
            ))
        })?;
        Ok(Write { group, attr, value })
    };
    let clear = |group: u32, attr: u64| Write {
        group,
        attr,
        value: u64::from(u32::MAX),
    };
    let dist = |offset: u64| get(sys::VGIC_GRP_DIST_REGS, offset);
    let mut writes = vec![dist(GICD_IIDR)?, dist(GICD_CTLR)?];
    for &mpidr in mpidrs {
        let cpu = cpu(mpidr);
        let redist = |offset: u64| get(sys::VGIC_GRP_REDIST_REGS, cpu | (SGI + offset));
        writes.extend([
            redist(GICD_IGROUPR)?,
            clear(sys::VGIC_GRP_REDIST_REGS, cpu | (SGI + GICD_ICENABLER)),
            redist(GICD_ISENABLER)?,
            redist(GICD_ICFGR)?,
            redist(GICD_ICFGR + 4)?,
            get(sys::VGIC_GRP_LEVEL_INFO, levels(cpu, 0))?,
            redist(GICD_ISPENDR)?,
            clear(sys::VGIC_GRP_REDIST_REGS, cpu | (SGI + GICD_ICACTIVER)),
            redist(GICD_ISACTIVER)?,
        ]);
        for n in 0..8 {
            writes.push(redist(GICD_IPRIORITYR + 4 * n)?);
        }
    }
    // The SPIs': each register's words past the private interrupts', 32 interrupts to a
    // word of a bit each, 16 of two bits, 4 of eight, and the routing's two of one.
    let words = |bits: u32| (32 * bits / 32)..(NR_IRQS * bits / 32);
    for w in words(1) {
        writes.push(dist(GICD_IGROUPR + 4 * u64::from(w))?);
    }
    for w in words(1) {
        writes.push(clear(sys::VGIC_GRP_DIST_REGS, GICD_ICENABLER + 4 * u64::from(w)));
        writes.push(dist(GICD_ISENABLER + 4 * u64::from(w))?);
    }
    for w in words(64) {
        writes.push(dist(GICD_IROUTER + 4 * u64::from(w))?);
    }
    for w in words(2) {
        writes.push(dist(GICD_ICFGR + 4 * u64::from(w))?);
    }
    for w in words(1) {
        writes.push(get(sys::VGIC_GRP_LEVEL_INFO, levels(0, 32 * w))?);
    }
    for w in words(1) {
        writes.push(dist(GICD_ISPENDR + 4 * u64::from(w))?);
    }
    for w in words(1) {
        writes.push(clear(sys::VGIC_GRP_DIST_REGS, GICD_ICACTIVER + 4 * u64::from(w)));
        writes.push(dist(GICD_ISACTIVER + 4 * u64::from(w))?);
    }
    for w in words(8) {
        writes.push(dist(GICD_IPRIORITYR + 4 * u64::from(w))?);
    }
    for &mpidr in mpidrs {
        let cpu = cpu(mpidr);
        let icc = |encoding: u64| get(sys::VGIC_GRP_CPU_SYSREGS, cpu | encoding);
        let ctlr = icc(ICC_CTLR_EL1)?;
        writes.extend([
            icc(ICC_SRE_EL1)?,
            ctlr,
            icc(ICC_IGRPEN0_EL1)?,
            icc(ICC_IGRPEN1_EL1)?,
            icc(ICC_PMR_EL1)?,
            icc(ICC_BPR0_EL1)?,
            icc(ICC_BPR1_EL1)?,
        ]);
        for n in 0..active_priority_registers(ctlr.value) {
            writes.push(icc(ICC_AP0R0_EL1 + n)?);
            writes.push(icc(ICC_AP1R0_EL1 + n)?);
        }
    }
    let mut w = Writer::default();
    w.seq(&writes, |w, write| {
        w.u32(write.group);
        w.u64(write.attr);
        w.u64(write.value);
    });
    Ok(w.into_bytes())
}

/// Makes `gic`, initialized, every vCPU of its VM made and none in the guest nor in any
/// other call of KVM's, the GIC [`save`] read.
pub(super) fn restore(gic: &sys::DeviceFd, state: &[u8]) -> Result<()> {
    let mut r = Reader::new(state);
    let writes = r
        .seq(MAX_WRITES, 20, |r| {
            Ok(Write {
                group: r.u32()?,
                attr: r.u64()?,
                value: r.u64()?,
            })
        })
        .and_then(|writes| r.finish().map(|()| writes))
        .map_err(|DecodeError(e)| Error::Guest(format!("the snapshot's GIC: {e}")))?;
    for Write { group, attr, value } in writes {
        let set = match group {
            sys::VGIC_GRP_CPU_SYSREGS => gic.set_attr(group, attr, &value),
            sys::VGIC_GRP_DIST_REGS | sys::VGIC_GRP_REDIST_REGS | sys::VGIC_GRP_LEVEL_INFO => {
                let value = u32::try_from(value).map_err(|_| {
                    Error::Guest(format!("the snapshot's GIC: {value:#x} for {group}/{attr:#x}"))
                })?;
                gic.set_attr(group, attr, &value)
            }
            other => {
                return Err(Error::Guest(format!(
                    "the snapshot's GIC: attribute group {other}"
                )));
            }
        };
        set.map_err(|e| {
            Error::Guest(format!(
                "KVM_SET_DEVICE_ATTR of the GIC's {group}/{attr:#x} failed: {e}"
            ))
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A redistributor's attribute names its vCPU by affinity, Aff3 in bits 63:56 where
    /// MPIDR_EL1 has it in 39:32; and the levels' attribute its first INTID.
    #[test]
    fn attributes_name_a_vcpu_by_its_affinity() {
        assert_eq!(cpu(0), 0);
        assert_eq!(cpu(crate::arch::aarch64::mpidr(17)), 0x0000_0101_0000_0000);
        assert_eq!(cpu(0x12_0034_5678), 0x1234_5678_0000_0000);
        assert_eq!(levels(cpu(1), 32), 0x0000_0001_0000_0020);
    }

    /// One active priority register of each kind at 5 bits of priority, 2 at 6, 4 at 7.
    #[test]
    fn a_cpu_interface_has_the_active_priority_registers_its_priority_bits_need() {
        assert_eq!(active_priority_registers(4 << 8), 1);
        assert_eq!(active_priority_registers(5 << 8), 2);
        assert_eq!(active_priority_registers(6 << 8), 4);
    }

    /// The SPIs' words of each register, for the machine's interrupts.
    #[test]
    fn the_spis_registers_are_the_words_past_the_private_interrupts() {
        let words = |bits: u32| (32 * bits / 32)..(NR_IRQS * bits / 32);
        assert_eq!(words(1), 1..NR_IRQS / 32);
        assert_eq!(words(2), 2..NR_IRQS / 16);
        assert_eq!(words(8), 8..NR_IRQS / 4);
        assert_eq!(words(64).len() as u32, 2 * (NR_IRQS - 32));
    }
}
