//! What a snapshot keeps of a WHP VM and its vCPUs (D196, PM M345):
//! - each vCPU's registers as WHP names them, and its local APIC and XSAVE area as
//!   WHvGetVirtualProcessorState gives them, not the functions it supersedes
//!   (WinHvPlatform's reference, "WHvGetVirtualProcessorState");
//! - the partition's reference time, the Hyper-V MSRs its guest writes once for all its
//!   vCPUs, every vCPU's TSC read at one instant with the partition's time held, and the
//!   VMM's own IOAPIC.
//!
//! A restore applies them in OpenVMM's order (virt_whp's vp_state.rs and apic.rs): the APIC
//! base before the APIC's page, which its mode governs; the XSAVE area before XCR0; then,
//! once every vCPU has its own state, with the partition's time held until a vCPU first
//! runs, the reference time, each vCPU's TSC and the Hyper-V MSRs.

use std::sync::OnceLock;

use super::ffi::*;
use super::{Error, Result, Vcpu, Vm, host};
use crate::arch::x86_64::ioapic;
use crate::snapshot::codec::{self, DecodeError, Reader, Writer};

/// The registers each vCPU's snapshot keeps, every one of which each host measured reads
/// (PM M345): what the vCPU runs from, the MSRs a Linux guest writes, its APIC base, the VP
/// assist page its Hyper-V setup gives it, and what is pending of its events and its
/// activity (an AP still held for its SIPI among it). Not its TSC, the VM's to read for
/// every vCPU at once ([`Vm::save_state`]).
const REGISTERS: &[WHV_REGISTER_NAME] = &[
    WHvX64RegisterRax,
    WHvX64RegisterRcx,
    WHvX64RegisterRdx,
    WHvX64RegisterRbx,
    WHvX64RegisterRsp,
    WHvX64RegisterRbp,
    WHvX64RegisterRsi,
    WHvX64RegisterRdi,
    WHvX64RegisterR8,
    WHvX64RegisterR9,
    WHvX64RegisterR10,
    WHvX64RegisterR11,
    WHvX64RegisterR12,
    WHvX64RegisterR13,
    WHvX64RegisterR14,
    WHvX64RegisterR15,
    WHvX64RegisterRip,
    WHvX64RegisterRflags,
    WHvX64RegisterEs,
    WHvX64RegisterCs,
    WHvX64RegisterSs,
    WHvX64RegisterDs,
    WHvX64RegisterFs,
    WHvX64RegisterGs,
    WHvX64RegisterLdtr,
    WHvX64RegisterTr,
    WHvX64RegisterIdtr,
    WHvX64RegisterGdtr,
    WHvX64RegisterCr0,
    WHvX64RegisterCr2,
    WHvX64RegisterCr3,
    WHvX64RegisterCr4,
    WHvX64RegisterCr8,
    WHvX64RegisterDr0,
    WHvX64RegisterDr1,
    WHvX64RegisterDr2,
    WHvX64RegisterDr3,
    WHvX64RegisterDr6,
    WHvX64RegisterDr7,
    WHvX64RegisterXCr0,
    WHvX64RegisterEfer,
    WHvX64RegisterKernelGsBase,
    WHvX64RegisterApicBase,
    WHvX64RegisterPat,
    WHvX64RegisterSysenterCs,
    WHvX64RegisterSysenterEip,
    WHvX64RegisterSysenterEsp,
    WHvX64RegisterStar,
    WHvX64RegisterLstar,
    WHvX64RegisterCstar,
    WHvX64RegisterSfmask,
    WHvX64RegisterTscAux,
    WHvX64RegisterMsrMtrrDefType,
    WHvX64RegisterMsrMtrrFix64k00000,
    WHvX64RegisterMsrMtrrFix16k80000,
    WHvX64RegisterMsrMtrrFix16kA0000,
    WHvX64RegisterMsrMtrrFix4kC0000,
    WHvX64RegisterMsrMtrrFix4kC8000,
    WHvX64RegisterMsrMtrrFix4kD0000,
    WHvX64RegisterMsrMtrrFix4kD8000,
    WHvX64RegisterMsrMtrrFix4kE0000,
    WHvX64RegisterMsrMtrrFix4kE8000,
    WHvX64RegisterMsrMtrrFix4kF0000,
    WHvX64RegisterMsrMtrrFix4kF8000,
    WHvRegisterVpAssistPage,
    WHvRegisterPendingInterruption,
    WHvRegisterInterruptState,
    WHvRegisterPendingEvent,
    WHvRegisterInternalActivityState,
];

/// Registers a host reads or not, by its processor and its Windows, kept where the saving
/// host reads them, and refused where a restoring host does not take them (PM M345:
/// jupiter read Xss and the CET registers, and refused SpecCtrl, TsxCtrl, TscAdjust,
/// UmwaitControl, Xfd and XfdErr). Besides these, the variable-range MTRRs the vCPU has
/// (MtrrCap's count). Not TSC-deadline, which no partition of shards' offers its guest (D140)
/// and which would have to follow the TSC.
const OPTIONAL: &[WHV_REGISTER_NAME] = &[
    WHvX64RegisterXss,
    WHvX64RegisterSpecCtrl,
    WHvX64RegisterTsxCtrl,
    WHvX64RegisterTscAdjust,
    WHvX64RegisterUmwaitControl,
    WHvX64RegisterXfd,
    WHvX64RegisterXfdErr,
    WHvX64RegisterUCet,
    WHvX64RegisterSCet,
    WHvX64RegisterSsp,
    WHvX64RegisterPl0Ssp,
    WHvX64RegisterPl1Ssp,
    WHvX64RegisterPl2Ssp,
    WHvX64RegisterPl3Ssp,
    WHvX64RegisterInterruptSspTableAddr,
    WHvX64RegisterPendingDebugException,
];

/// The Hyper-V MSRs a guest writes once for all its vCPUs, which the partition keeps:
/// its OS ID before its hypercall page, which the hypervisor enables only for a guest that
/// named itself (Hyper-V TLFS, "Establishing the Hypercall Interface").
const HYPERV: [WHV_REGISTER_NAME; 3] = [
    WHvRegisterGuestOsId,
    WHvX64RegisterHypercall,
    WHvRegisterReferenceTsc,
];

/// The CPUID leaves that say which CPU a snapshot was taken on, as WHP describes it to a
/// partition's guest: its vendor and family, its features, its XSAVE components and its
/// address sizes. Not its caches, which a hybrid host's cores describe apart (PM M258).
pub(super) const IDENTITY: &[(u32, u32)] = &[
    (0, 0),
    (1, 0),
    (7, 0),
    (7, 1),
    (0xD, 0),
    (0xD, 1),
    (0x8000_0000, 0),
    (0x8000_0001, 0),
    (0x8000_0007, 0),
    (0x8000_0008, 0),
];

/// Bounds on what a decoder takes: a state is a few pages; a host's register list is
/// shorter than this.
/// ERROR_MORE_DATA (234) as an HRESULT (FACILITY_WIN32).
const ERROR_MORE_DATA_HRESULT: u32 = 0x8007_00EA;

const MAX_BLOB: usize = 64 << 10;
const MAX_REGISTERS: usize = 256;
const MAX_LEAVES: usize = 64;
const MAX_VCPUS: usize = 254;

/// A vCPU's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VcpuState {
    /// The CPU it was saved on ([`IDENTITY`]), which a restore requires again.
    cpuid: Vec<[u32; 7]>,
    /// Each register by its WHP name, with its value's 16 bytes.
    registers: Vec<(WHV_REGISTER_NAME, [u8; 16])>,
    /// Its local APIC (InterruptControllerState2): its registers' page.
    apic: Vec<u8>,
    /// Its XSAVE area (XsaveState), as large as its partition's.
    xsave: Vec<u8>,
}

/// The VM's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmState {
    /// The partition's reference time, in 100 ns units, which its guest keeps time by
    /// (D140).
    reference_time: u64,
    /// [`HYPERV`], as vCPU 0 has them.
    hyperv: [u64; 3],
    /// Each vCPU's TSC, all read with the partition's time held.
    tscs: Vec<u64>,
    ioapic: ioapic::Saved,
}

const _: () = assert!(size_of::<WHV_REGISTER_VALUE>() == 16);

fn bytes_of(value: &Value) -> [u8; 16] {
    // SAFETY: a 16-byte union of plain integers, read as its bytes.
    unsafe { std::mem::transmute::<WHV_REGISTER_VALUE, [u8; 16]>(value.0) }
}

fn value_of(bytes: [u8; 16]) -> Value {
    // SAFETY: any 16 bytes are a register value: a union of plain integers.
    Value(unsafe { std::mem::transmute::<[u8; 16], WHV_REGISTER_VALUE>(bytes) })
}

/// The registers this host reads of [`OPTIONAL`], and its variable-range MTRRs, each asked
/// alone of `vcpu`: once a process, as its host's are its every VM's.
fn optional(vcpu: &Vcpu) -> Result<&'static [WHV_REGISTER_NAME]> {
    static OPTIONAL_READ: OnceLock<Vec<WHV_REGISTER_NAME>> = OnceLock::new();
    if let Some(names) = OPTIONAL_READ.get() {
        return Ok(names);
    }
    // MtrrCap's count of variable ranges (bits 7:0), each a base and a mask.
    let ranges = (vcpu.get(WHvX64RegisterMsrMtrrCap)? & 0xFF).min(16) as i32;
    let mtrrs = (0..ranges).flat_map(|i| {
        [
            WHvX64RegisterMsrMtrrPhysBase0 + i,
            WHvX64RegisterMsrMtrrPhysMask0 + i,
        ]
    });
    let names = mtrrs
        .chain(OPTIONAL.iter().copied())
        .filter(|&name| vcpu.get(name).is_ok())
        .collect();
    Ok(OPTIONAL_READ.get_or_init(|| names))
}

/// Whether `name` is one a snapshot keeps: what a restore takes from a state file, which
/// it treats as untrusted.
fn kept(name: WHV_REGISTER_NAME) -> bool {
    REGISTERS.contains(&name)
        || OPTIONAL.contains(&name)
        || (WHvX64RegisterMsrMtrrPhysBase0..=WHvX64RegisterMsrMtrrPhysBaseF).contains(&name)
        || (WHvX64RegisterMsrMtrrPhysMask0..=WHvX64RegisterMsrMtrrPhysMaskF).contains(&name)
}

impl Vcpu {
    /// Captures the vCPU's state: its registers, its local APIC's page and its XSAVE area.
    /// With the vCPU out of the guest; from any thread.
    pub fn save_state(&self) -> Result<VcpuState> {
        let names: Vec<WHV_REGISTER_NAME> = REGISTERS.iter().chain(optional(self)?).copied().collect();
        let values = self.partition.registers(self.index, &names)?;
        Ok(VcpuState {
            cpuid: host()?.identity.clone(),
            registers: names.into_iter().zip(values.iter().map(bytes_of)).collect(),
            apic: self
                .partition
                .state(self.index, WHvVirtualProcessorStateTypeInterruptControllerState2)?,
            xsave: self
                .partition
                .state(self.index, WHvVirtualProcessorStateTypeXsaveState)?,
        })
    }

    /// Loads `st` into this new vCPU: its APIC base, then its APIC's page, then its XSAVE
    /// area, then every register. A vCPU of another CPU refuses: the snapshot's registers
    /// and XSAVE area are its CPU's.
    pub fn restore_state(&mut self, st: &VcpuState) -> Result<()> {
        if st.cpuid != host()?.identity {
            return Err(Error::Guest(
                "this CPU is not the one the snapshot was taken on (its CPUID differs)".into(),
            ));
        }
        let (names, values): (Vec<WHV_REGISTER_NAME>, Vec<Value>) = st
            .registers
            .iter()
            .map(|&(name, bytes)| (name, value_of(bytes)))
            .unzip();
        let base = names
            .iter()
            .position(|&n| n == WHvX64RegisterApicBase)
            .and_then(|i| values.get(i))
            .ok_or_else(|| Error::Guest("a vCPU's state without its APIC base".into()))?;
        self.set(
            &[WHvX64RegisterApicBase],
            &[*base],
            "WHvSetVirtualProcessorRegisters(ApicBase)",
        )?;
        self.partition
            .set_state(
                self.index,
                WHvVirtualProcessorStateTypeInterruptControllerState2,
                &st.apic,
                "WHvSetVirtualProcessorState(InterruptControllerState2)",
            )
            .map_err(|e| match e {
                // What Windows Server 2022 (build 20348) answers a page set back as read, and
                // E_INVALIDARG to a part of it, where builds 26100 and 26200 take it (PM M345).
                Error::Call { hr, .. } if hr as u32 == ERROR_MORE_DATA_HRESULT => Error::Guest(format!(
                    "{e}: this Windows sets no vCPU's APIC page back, so no snapshot restores on it \
                     (Windows Server 2022, build 20348, refuses it; builds 26100 and 26200 take it)"
                )),
                e => e,
            })?;
        self.partition.set_state(
            self.index,
            WHvVirtualProcessorStateTypeXsaveState,
            &st.xsave,
            "WHvSetVirtualProcessorState(XsaveState)",
        )?;
        self.partition.set_each(self.index, &names, &values)
    }

    /// No restore holds a TSC back: a restored partition's time is held until a vCPU of it
    /// first runs ([`Vm::restore_state`]).
    pub fn tsc(&self) -> Option<Tsc> {
        None
    }

    pub fn resume_tsc(&mut self, _: u64) -> Result<()> {
        Ok(())
    }
}

impl Vm {
    /// The partition's reference time, each vCPU's TSC, the Hyper-V MSRs and the IOAPIC.
    /// Every vCPU must be stopped. The TSCs are read with the partition's time held, at
    /// one instant, which a restore sets back with its own time held: no vCPU's TSC comes
    /// back apart from another's by however far apart their captures were. The time goes
    /// on once they are read, as KVM's guests' does while their snapshot is written.
    pub fn save_state(&self) -> Result<VmState> {
        self.partition.suspend_time()?;
        let read = (|| -> Result<VmState> {
            let reference_time = self.partition.reference_time()?;
            let tscs = (0..self.vcpus)
                .map(|vp| {
                    self.partition
                        .registers(vp, &[WHvX64RegisterTsc])
                        .map(|v| v.first().map_or(0, Value::get))
                })
                .collect::<Result<Vec<u64>>>()?;
            let msrs = self.partition.registers(0, &HYPERV)?;
            let mut hyperv = [0u64; 3];
            for (to, from) in hyperv.iter_mut().zip(&msrs) {
                *to = from.get();
            }
            Ok(VmState {
                reference_time,
                hyperv,
                tscs,
                ioapic: self.ioapic.save(),
            })
        })();
        let resumed = self.partition.resume_time();
        let st = read?;
        resumed?;
        Ok(st)
    }

    /// Restores `st` once every vCPU exists and holds its own state, with the partition's
    /// time held, which a vCPU's first run resumes (WHvSuspendPartitionTime's reference): a
    /// partition never run keeps its time where it was made (PM M345), and from here it
    /// stays where the snapshot left it, the guest's clocks going on from there whenever
    /// its vCPUs are released. Then the IOAPIC, before the devices drive its pins again.
    pub fn restore_state(&self, st: &VmState) -> Result<()> {
        if st.tscs.len() != self.vcpus as usize {
            return Err(Error::Guest(format!(
                "the snapshot has {} vCPUs' TSCs for {} vCPUs",
                st.tscs.len(),
                self.vcpus
            )));
        }
        self.partition.suspend_time()?;
        self.partition.set_reference_time(st.reference_time)?;
        for (vp, tsc) in (0u32..).zip(&st.tscs) {
            self.partition
                .set_each(vp, &[WHvX64RegisterTsc], &[Value::u64(*tsc)])?;
        }
        let msrs = st.hyperv.map(Value::u64);
        self.partition.set_each(0, &HYPERV, &msrs)?;
        self.ioapic.restore(&st.ioapic);
        Ok(())
    }
}

/// A restored vCPU's TSC: none, as the partition's time is held instead.
#[derive(Debug, Clone)]
pub struct Tsc(());

/// Where a restored TSC starts: nowhere of its own.
#[derive(Debug, Clone, Copy)]
pub struct TscStart(());

impl Tsc {
    pub fn restart(&self, _: TscStart) -> Result<Option<u64>> {
        Ok(None)
    }
}

impl VcpuState {
    /// None: a restore holds the partition's time, not a TSC.
    pub fn tsc_start(&self) -> Option<TscStart> {
        None
    }

    pub fn encode(&self, w: &mut Writer) {
        w.seq(&self.cpuid, |w, leaf| leaf.iter().for_each(|&v| w.u32(v)));
        w.seq(&self.registers, |w, (name, value)| {
            w.u32(*name as u32);
            w.u128(u128::from_le_bytes(*value));
        });
        w.bytes(&self.apic);
        w.bytes(&self.xsave);
    }

    pub fn decode(r: &mut Reader<'_>) -> codec::Result<VcpuState> {
        let cpuid = r.seq(MAX_LEAVES, 28, |r| {
            let mut leaf = [0u32; 7];
            for v in &mut leaf {
                *v = r.u32()?;
            }
            Ok(leaf)
        })?;
        let registers = r.seq(MAX_REGISTERS, 20, |r| {
            let name = r.u32()? as WHV_REGISTER_NAME;
            if !kept(name) {
                return Err(DecodeError(format!(
                    "register {name:#x}, which no snapshot keeps"
                )));
            }
            Ok((name, r.u128()?.to_le_bytes()))
        })?;
        if !registers.iter().any(|&(n, _)| n == WHvX64RegisterApicBase) {
            return Err(DecodeError("a vCPU's state without its APIC base".into()));
        }
        let apic = r.bytes(MAX_BLOB)?.to_vec();
        let xsave = r.bytes(MAX_BLOB)?.to_vec();
        Ok(VcpuState {
            cpuid,
            registers,
            apic,
            xsave,
        })
    }
}

impl VmState {
    pub fn encode(&self, w: &mut Writer) {
        w.u64(self.reference_time);
        self.hyperv.iter().for_each(|&v| w.u64(v));
        w.seq(&self.tscs, |w, &tsc| w.u64(tsc));
        w.u32(self.ioapic.select);
        w.u32(self.ioapic.id);
        self.ioapic.entries.iter().for_each(|&e| w.u64(e));
        w.u32(self.ioapic.lines);
    }

    pub fn decode(r: &mut Reader<'_>) -> codec::Result<VmState> {
        let reference_time = r.u64()?;
        let mut hyperv = [0u64; 3];
        for v in &mut hyperv {
            *v = r.u64()?;
        }
        let tscs = r.seq(MAX_VCPUS, 8, |r| r.u64())?;
        let select = r.u32()?;
        let id = r.u32()?;
        let mut entries = [0u64; ioapic::PINS];
        for e in &mut entries {
            *e = r.u64()?;
        }
        Ok(VmState {
            reference_time,
            hyperv,
            tscs,
            ioapic: ioapic::Saved {
                select,
                id,
                entries,
                lines: r.u32()?,
            },
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn vcpu() -> VcpuState {
        VcpuState {
            cpuid: vec![[1, 0, 0, 0x00a2_0f12, 0x0080_0800, 0xfef8_3203, 0x178b_fbff]],
            registers: vec![
                (WHvX64RegisterRip, [1; 16]),
                (WHvX64RegisterApicBase, [2; 16]),
                (WHvX64RegisterMsrMtrrPhysMask0 + 7, [3; 16]),
                (WHvX64RegisterXss, [4; 16]),
            ],
            apic: vec![5; 4096],
            xsave: vec![6; 2472],
        }
    }

    fn vm() -> VmState {
        VmState {
            reference_time: 11_525_134,
            hyperv: [0x8100_0006_1200_0000, 0x1D001, 0x1E001],
            tscs: vec![0x1ba4_3628, 0x1ba4_3628],
            ioapic: ioapic::Saved {
                select: 0x17,
                id: 0x0200_0000,
                entries: [0x0001_0000; ioapic::PINS],
                lines: 1 << 4,
            },
        }
    }

    /// A VM's state and its vCPUs' come back whole, and any part of it cut off is damage.
    #[test]
    fn states_round_trip_and_damage_is_an_error() {
        let mut w = Writer::default();
        vm().encode(&mut w);
        vcpu().encode(&mut w);
        let bytes = w.into_bytes();
        let mut r = Reader::new(&bytes);
        assert_eq!(VmState::decode(&mut r).unwrap(), vm());
        assert_eq!(VcpuState::decode(&mut r).unwrap(), vcpu());
        r.finish().unwrap();
        for cut in 0..bytes.len() {
            let mut r = Reader::new(&bytes[..cut]);
            let whole = VmState::decode(&mut r).and_then(|_| VcpuState::decode(&mut r));
            assert!(whole.is_err(), "cut at {cut}");
        }
    }

    /// A state file names only registers a snapshot keeps, its APIC base among them: one
    /// that would set any other register, or none, is refused.
    #[test]
    fn a_state_naming_registers_no_snapshot_keeps_is_refused() {
        for (name, kept) in [
            (WHvX64RegisterVirtualCr0, false),
            (WHvX64RegisterTscDeadline, false),
            (WHvX64RegisterTsc, false),
            (WHvX64RegisterMsrMtrrPhysBase0, true),
            (WHvX64RegisterMsrMtrrPhysMaskF, true),
            (WHvX64RegisterPl3Ssp, true),
        ] {
            let mut st = vcpu();
            st.registers.push((name, [0; 16]));
            let mut w = Writer::default();
            st.encode(&mut w);
            let decoded = VcpuState::decode(&mut Reader::new(&w.into_bytes()));
            assert_eq!(decoded.is_ok(), kept, "register {name:#x}");
        }
        let mut st = vcpu();
        st.registers.retain(|&(n, _)| n != WHvX64RegisterApicBase);
        let mut w = Writer::default();
        st.encode(&mut w);
        assert!(VcpuState::decode(&mut Reader::new(&w.into_bytes())).is_err());
    }
}
