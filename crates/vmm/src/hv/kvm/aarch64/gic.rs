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
//! state. Where the machine has an ITS (D214: an empty PCI bus a template keeps), each
//! redistributor's LPI tables' places and its LPIs' enable come before its private
//! interrupts', the tables' places first, as KVM takes them only while LPIs are off
//! (vgic-mmio-v3.c, vgic_mmio_write_propbase).

use super::super::sys;
use super::{Error, NR_IRQS, Result};
use crate::snapshot::codec::{DecodeError, Reader, Writer};

/// A redistributor's SGI frame, after its RD frame (arm-vgic-v3.rst; GICv3 §12.10).
const SGI: u64 = 64 << 10;

/// A redistributor's RD frame's LPI registers (GICv3 §12.11): its control (EnableLPIs, bit
/// 0) and its LPI configuration and pending tables' places, 64 bits each, reached 32 at a
/// time.
const GICR_CTLR: u64 = 0x0000;
const GICR_PROPBASER: u64 = 0x0070;
const GICR_PENDBASER: u64 = 0x0078;

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

/// What [`save`] writes of each redistributor: where the machine has an ITS, its LPI
/// registers (PROPBASER's and PENDBASER's two words each, then its control); then its
/// private interrupts' group, enables (the clear and the set), configuration (two words),
/// levels, pending state, activity (the clear and the set), and eight priority words.
const LPI_WRITES: usize = 5;
const PRIVATE_WRITES: usize = 9 + 8;
/// Of each CPU interface: its seven registers, and as many active priority registers of
/// each kind as its priority bits need, at most ICC_AP0R0 to ICC_AP0R3 and ICC_AP1R0 to
/// ICC_AP1R3 (GICv3 §12.2.1-12.2.4).
const ICC_WRITES_MOST: usize = 7 + 2 * 4;
/// Of the distributor: its implementation and control, then the SPIs' registers' words,
/// each register's ([`spi_words`]) as `save` reads them: a group, two enables, a routing,
/// a configuration, a level, a pending, two activities and a priority word.
const SPI_WRITES: usize =
    (7 * spi_words(1).len() + spi_words(64).len() + spi_words(2).len() + spi_words(8).len()) as usize;

/// The words of a distributor register of `bits` bits an interrupt that hold the SPIs':
/// those past the private interrupts', 32 interrupts to a word of a bit each, 16 of two
/// bits, 4 of eight, and the routing's two of one.
const fn spi_words(bits: u32) -> SpiWords {
    SpiWords {
        start: 32 * bits / 32,
        end: NR_IRQS * bits / 32,
    }
}

/// A run of register words, `start..end`.
#[derive(Debug, Clone, Copy)]
struct SpiWords {
    start: u32,
    end: u32,
}

impl SpiWords {
    const fn len(self) -> u32 {
        self.end.saturating_sub(self.start)
    }

    fn words(self) -> std::ops::Range<u32> {
        self.start..self.end
    }
}

/// The most writes [`save`] makes of a GIC of `vcpus` vCPUs, with LPIs where `lpis`: what
/// [`restore`] takes of a snapshot's.
fn most_writes(vcpus: usize, lpis: bool) -> Option<usize> {
    let lpi = if lpis { LPI_WRITES } else { 0 };
    vcpus
        .checked_mul(lpi + PRIVATE_WRITES + ICC_WRITES_MOST)?
        .checked_add(2 + SPI_WRITES)
}

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
pub(super) fn save(gic: &sys::DeviceFd, mpidrs: &[u64], lpis: bool) -> Result<Vec<u8>> {
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
        if lpis {
            let rd = |offset: u64| get(sys::VGIC_GRP_REDIST_REGS, cpu | offset);
            writes.extend([
                rd(GICR_PROPBASER)?,
                rd(GICR_PROPBASER + 4)?,
                rd(GICR_PENDBASER)?,
                rd(GICR_PENDBASER + 4)?,
                rd(GICR_CTLR)?,
            ]);
        }
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
    // The SPIs': each register's words past the private interrupts'.
    let words = |bits: u32| spi_words(bits).words();
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
/// other call of KVM's, the GIC [`save`] read of a VM of as many vCPUs, with LPIs where
/// `lpis`: no more writes than such a save makes ([`most_writes`]).
pub(super) fn restore(gic: &sys::DeviceFd, state: &[u8], vcpus: usize, lpis: bool) -> Result<()> {
    let most =
        most_writes(vcpus, lpis).ok_or_else(|| Error::Guest(format!("the snapshot's GIC: {vcpus} vCPUs")))?;
    let mut r = Reader::new(state);
    let writes = r
        .seq(most, 20, |r| {
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

/// The ITS's registers (GICv3 §12.19) a restore writes, in arm-vgic-its.rst's order: its
/// implementation first, which says the ABI of its tables; its command queue's place,
/// which resets where it is read and written; its tables' places; where its queue is read
/// and written; and, after its tables are read back, its control, which enables it.
const GITS_CTLR: u64 = 0x0000;
const GITS_IIDR: u64 = 0x0004;
const GITS_CBASER: u64 = 0x0080;
const GITS_CWRITER: u64 = 0x0088;
const GITS_CREADR: u64 = 0x0090;
const GITS_BASER: u64 = 0x0100;
/// GITS_BASER<n>, n 0 to 7.
const ITS_BASERS: u64 = 8;
/// The registers [`its_save`] keeps: the implementation, the command queue's place, each
/// table's place, where the queue is read and written, and the control.
const ITS_SAVED: usize = 2 + ITS_BASERS as usize + 3;

/// The ITS's state (arm-vgic-its.rst, "ITS Save Sequence"): its tables written to guest
/// memory, before the memory is saved, and its registers, each a 64-bit value by offset in
/// [`its_restore`]'s order but its control's, which comes last.
pub(super) fn its_save(its: &sys::DeviceFd) -> Result<Vec<u8>> {
    let failed = |what: &str, e: std::io::Error| Error::Guest(format!("the ITS's {what}: {e}"));
    its.control(sys::VGIC_GRP_CTRL, sys::ITS_SAVE_TABLES)
        .map_err(|e| failed("tables", e))?;
    let get = |offset: u64| {
        its.get_attr::<u64>(sys::VGIC_GRP_ITS_REGS, offset)
            .map(|value| (offset, value))
            .map_err(|e| failed(&format!("register {offset:#x}"), e))
    };
    let mut regs = vec![get(GITS_IIDR)?, get(GITS_CBASER)?];
    for n in 0..ITS_BASERS {
        regs.push(get(GITS_BASER + 8 * n)?);
    }
    regs.extend([get(GITS_CREADR)?, get(GITS_CWRITER)?, get(GITS_CTLR)?]);
    let mut w = Writer::default();
    w.seq(&regs, |w, (offset, value)| {
        w.u64(*offset);
        w.u64(*value);
    });
    Ok(w.into_bytes())
}

/// Makes `its`, of a VM whose memory and redistributors are restored, the ITS
/// [`its_save`] read ("ITS Restore Sequence"): reset, its registers but its control, its
/// tables read back from guest memory, its control.
pub(super) fn its_restore(its: &sys::DeviceFd, state: &[u8]) -> Result<()> {
    let failed = |what: &str, e: std::io::Error| Error::Guest(format!("the ITS's {what}: {e}"));
    let mut r = Reader::new(state);
    let regs = r
        .seq(ITS_SAVED, 16, |r| Ok((r.u64()?, r.u64()?)))
        .and_then(|regs| r.finish().map(|()| regs))
        .map_err(|DecodeError(e)| Error::Guest(format!("the snapshot's ITS: {e}")))?;
    let (ctlr, rest): (Vec<_>, Vec<_>) = regs.into_iter().partition(|(offset, _)| *offset == GITS_CTLR);
    its.control(sys::VGIC_GRP_CTRL, sys::ITS_CTRL_RESET)
        .map_err(|e| failed("reset", e))?;
    for (offset, value) in rest {
        its.set_attr(sys::VGIC_GRP_ITS_REGS, offset, &value)
            .map_err(|e| failed(&format!("register {offset:#x}"), e))?;
    }
    its.control(sys::VGIC_GRP_CTRL, sys::ITS_RESTORE_TABLES)
        .map_err(|e| failed("tables", e))?;
    for (offset, value) in ctlr {
        its.set_attr(sys::VGIC_GRP_ITS_REGS, offset, &value)
            .map_err(|e| failed("control", e))?;
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
        let words = |bits: u32| spi_words(bits).words();
        assert_eq!(words(1), 1..NR_IRQS / 32);
        assert_eq!(words(2), 2..NR_IRQS / 16);
        assert_eq!(words(8), 8..NR_IRQS / 4);
        assert_eq!(words(64).len() as u32, 2 * (NR_IRQS - 32));
        assert_eq!(spi_words(64).len(), 2 * (NR_IRQS - 32));
    }

    /// A restore takes no more writes than a save of as many vCPUs makes: 2 of the
    /// distributor's own, 81 for each 32 SPIs, and each vCPU's 17 private, 5 of LPIs where
    /// the machine has an ITS, and up to 15 of its CPU interface's.
    #[test]
    fn a_restore_takes_the_writes_a_save_of_its_vcpus_makes() {
        let spis = (NR_IRQS / 32 - 1) as usize;
        assert_eq!(most_writes(0, false), Some(2 + 81 * spis));
        assert_eq!(most_writes(1, false), Some(2 + 81 * spis + 17 + 15));
        assert_eq!(most_writes(3, true), Some(2 + 81 * spis + 3 * (5 + 17 + 15)));
        assert_eq!(most_writes(usize::MAX, false), None);
        assert_eq!(ITS_SAVED, 13);
    }
}
