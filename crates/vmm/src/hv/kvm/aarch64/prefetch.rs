//! The prefetch program (arch::aarch64::prefetch) under KVM (D154). arm64's KVM has no
//! KVM_PRE_FAULT_MEMORY (x86's and s390's alone, virt/kvm/Kconfig, to Linux 7.2), so a
//! vCPU touches the pages, as under Hypervisor.framework. KVM runs no vCPU of a VM whose GIC
//! is not yet initialized (vgic-v3.c, vgic_v3_map_resources: EBUSY), nor makes one after
//! (arm.c, kvm_arch_vcpu_precreate), so vCPU 0 runs it once every vCPU exists and the GIC
//! is initialized, from the thread that finishes the machine, while its own waits; its
//! state is restored again after it ([`super::Vm::prefetch`]).
//!
//! KVM takes a guest's HVC for itself (SMCCC), so the program ends with a write to
//! [`DONE`], an address no memory slot holds, which KVM returns as MMIO. Its exception
//! vectors write [`FAULT`] the same way: a loop that faulted would otherwise take its
//! exception through a vector its table does not map, and fault for ever in the guest.

use std::sync::atomic::Ordering;

use super::super::sys;
use super::{Error, PAGE, Result, Vm};
use crate::arch::aarch64::{layout, prefetch, sysreg};
use crate::hv::Touch;

/// Where the program ends, and where its vectors say it faulted: in the 1 GiB block its
/// table maps for its code, past its memory slot.
const DONE: u64 = layout::PREFETCH + 0x2000_0000;
const FAULT: u64 = layout::PREFETCH + 0x3000_0000;

/// `str xzr, [x10]`: the write to [`DONE`], X10 holding it; then `b .`.
const END: [u32; 2] = [0xf900_015f, 0x1400_0000];
/// Each exception vector's: `str xzr, [x11]`, X11 holding [`FAULT`]; then `b .`.
const VECTOR: [u32; 2] = [0xf900_017f, 0x1400_0000];
/// The vectors' place in the program's memory: 2 KiB aligned, as VBAR_EL1 asks, sixteen
/// of 0x80 bytes.
const VECTORS: u64 = 0x800;
/// The list's place, after the table.
const LIST: u64 = prefetch::TABLE + (4 << 10);

const VBAR_EL1: u16 = sysreg::enc(3, 0, 12, 0, 0);
const ESR_EL1: u16 = sysreg::enc(3, 0, 5, 2, 0);
const FAR_EL1: u16 = sysreg::enc(3, 0, 6, 0, 0);
const ELR_EL1: u16 = sysreg::enc(3, 0, 4, 0, 1);

/// The program's memory: anonymous, unmapped on drop.
struct Scratch {
    host: *mut u8,
    len: usize,
}

impl Scratch {
    fn new(len: usize) -> Result<Scratch> {
        // SAFETY: a fresh private anonymous mapping, owned by the Scratch.
        let host = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if host == libc::MAP_FAILED {
            return Err(Error::Guest(format!(
                "memory for the prefetch program: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(Scratch {
            host: host.cast(),
            len,
        })
    }

    /// Copies `words`' bytes in at `offset`, as far as they fit.
    fn put<const N: usize>(&self, offset: u64, words: impl IntoIterator<Item = [u8; N]>) {
        let bytes: Vec<u8> = words.into_iter().flatten().collect();
        let Ok(offset) = usize::try_from(offset) else {
            return;
        };
        let n = bytes.len().min(self.len.saturating_sub(offset));
        if n == 0 {
            return;
        }
        // SAFETY: `offset + n` is within the mapping, which nothing else references yet.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.host.add(offset), n) };
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // SAFETY: our own mapping, which no memory slot holds any longer.
        unsafe { libc::munmap(self.host.cast(), self.len) };
    }
}

/// Runs the program on `fd`, vCPU 0's, over `pages`, `memory` saying of each whether it is
/// guest memory, and writable; returns how many pages it touched. `fd`'s registers are the
/// program's after it: the caller restores the vCPU's state.
pub(super) fn run(
    vm: &Vm,
    fd: &mut sys::VcpuFd,
    pages: &[Touch],
    memory: impl Fn(u64) -> Option<bool>,
) -> Result<usize> {
    let reg = |enc: u16| {
        fd.get_reg(sys::sysreg(enc))
            .map_err(|e| Error::Guest(format!("KVM_GET_ONE_REG of {enc:#x} failed: {e}")))
    };
    let lse = prefetch::lse(reg(sysreg::ID_AA64ISAR0_EL1)?);
    let parange = reg(sysreg::ID_AA64MMFR0_EL1)?;
    let list = prefetch::entries(pages, PAGE, lse, memory);
    if list.is_empty() {
        return Ok(0);
    }
    let host_page =
        crate::platform::page_size().map_err(|e| Error::Guest(format!("the host's page size: {e}")))?;
    let len = usize::try_from(LIST)
        .ok()
        .and_then(|list_at| list_at.checked_add(list.len().checked_mul(8)?))
        .and_then(|end| end.checked_next_multiple_of(host_page))
        .ok_or_else(|| Error::Guest(format!("a prefetch list of {} pages", list.len())))?;
    let scratch = Scratch::new(len)?;
    scratch.put(0, prefetch::LOOP.iter().chain(&END).map(|w| w.to_le_bytes()));
    for v in 0..16u64 {
        scratch.put(VECTORS + 0x80 * v, VECTOR.iter().map(|w| w.to_le_bytes()));
    }
    scratch.put(
        prefetch::TABLE,
        prefetch::table(&list).iter().map(|e| e.to_le_bytes()),
    );
    scratch.put(LIST, list.iter().map(|e| e.to_le_bytes()));
    let slots = vm.device_slots(1);
    // SAFETY: `scratch` outlives the slot, deleted below before it drops; read-only to the
    // guest, whose writes there would exit as MMIO.
    unsafe { slots.map(0, scratch.host, layout::PREFETCH, len, false) }?;
    let ran = run_program(fd, parange, list.len());
    // Deleting the slot unmaps it at stage 2, which flushes the VM's stage 1 translations
    // too (arm64 kvm/hyp, __kvm_tlb_flush_vmid_range): none of the program's map outlives
    // it, even where the program stopped short of its `tlbi`.
    let deleted = slots.unmap(0, layout::PREFETCH, len);
    if deleted.is_err() {
        // The guest may still reach it: it must outlive the VM.
        std::mem::forget(scratch);
    }
    ran?;
    deleted?;
    Ok(list.len())
}

/// Sets `fd`'s registers for the program, its list `n` entries long, runs it to its end,
/// and retires the end's write, so that the vCPU's next run does not complete it over the
/// state restored meanwhile (KVM_RUN completes the last exit's MMIO first, mmio.c,
/// kvm_handle_mmio_return, even with `immediate_exit`).
fn run_program(fd: &mut sys::VcpuFd, parange: u64, n: usize) -> Result<()> {
    let set = |id: u64, value: u64| {
        fd.set_reg(id, value)
            .map_err(|e| Error::Guest(format!("KVM_SET_ONE_REG of {id:#x} failed: {e}")))
    };
    for (enc, value) in prefetch::registers(parange, layout::PREFETCH + prefetch::TABLE) {
        set(sys::sysreg(enc), value)?;
    }
    set(sys::sysreg(VBAR_EL1), layout::PREFETCH + VECTORS)?;
    set(sys::REG_PSTATE, sysreg::PSTATE_EL1H_DAIF)?;
    set(sys::reg_x(0), layout::PREFETCH + LIST)?;
    set(sys::reg_x(1), n as u64)?;
    set(sys::reg_x(10), DONE)?;
    set(sys::reg_x(11), FAULT)?;
    set(sys::REG_PC, layout::PREFETCH)?;
    // A vCPU 0 the guest powered off runs nothing until powered on.
    fd.set_mp_state(sys::MP_STATE_RUNNABLE)
        .map_err(super::call("KVM_SET_MP_STATE"))?;
    loop {
        let faulted = match fd.run().map_err(super::call("KVM_RUN"))? {
            sys::RunExit::MmioWrite { addr: DONE, .. } => break,
            sys::RunExit::MmioWrite { addr: FAULT, .. } => true,
            sys::RunExit::Interrupted | sys::RunExit::Again => false,
            other => return Err(Error::Guest(format!("the prefetch program stopped: {other:?}"))),
        };
        if faulted {
            let at = |enc: u16| fd.get_reg(sys::sysreg(enc)).unwrap_or(u64::MAX);
            return Err(Error::Guest(format!(
                "the prefetch program took an exception: ESR_EL1 {:#x} at ELR_EL1 {:#x}, FAR_EL1 {:#x}",
                at(ESR_EL1),
                at(ELR_EL1),
                at(FAR_EL1)
            )));
        }
        // A signal of the thread's own goes on; a kick, which sets `immediate_exit` too, is
        // the VM stopping.
        if fd.run.immediate_exit().load(Ordering::SeqCst) != 0 {
            return Err(Error::Guest("the VM stopped as it prefetched".into()));
        }
    }
    let kicked = fd.run.immediate_exit().swap(1, Ordering::SeqCst);
    let retired = match fd.run() {
        Ok(sys::RunExit::Interrupted) => Ok(()),
        Ok(other) => Err(Error::Guest(format!(
            "retiring the prefetch program's end: {other:?}"
        ))),
        Err(e) => Err(super::call("KVM_RUN")(e)),
    };
    fd.run.immediate_exit().store(kicked, Ordering::SeqCst);
    retired
}
