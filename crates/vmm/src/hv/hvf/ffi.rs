//! Raw Hypervisor.framework (arm64) bindings for the subset shards uses.
//!
//! Hand-written from the macOS 26.4 SDK headers; enum widths and values follow
//! `hv_*_types.h` exactly (see docs/research/hvf-arm64-kvm-ground-truth.md §1).

#![allow(non_camel_case_types)]

use std::ffi::c_void;

pub type hv_return_t = i32;
pub type hv_vcpu_t = u64;
pub type hv_ipa_t = u64;
pub type hv_memory_flags_t = u64;
pub type hv_vm_config_t = *mut c_void;
pub type hv_vcpu_config_t = *mut c_void;
pub type hv_gic_config_t = *mut c_void;
pub type hv_gic_state_t = *mut c_void;
pub type hv_reg_t = u32;
pub type hv_sys_reg_t = u16;
pub type hv_simd_fp_reg_t = u32;
pub type hv_ipa_granule_t = u32;

pub const HV_SUCCESS: hv_return_t = 0;
pub const HV_EXISTS: hv_return_t = 0xfae9_4008_u32 as hv_return_t;
pub const HV_ERROR: hv_return_t = 0xfae9_4001_u32 as hv_return_t;
pub const HV_BAD_ARGUMENT: hv_return_t = 0xfae9_4003_u32 as hv_return_t;
pub const HV_NO_RESOURCES: hv_return_t = 0xfae9_4005_u32 as hv_return_t;

pub const HV_MEMORY_READ: hv_memory_flags_t = 1 << 0;
pub const HV_MEMORY_WRITE: hv_memory_flags_t = 1 << 1;
pub const HV_MEMORY_EXEC: hv_memory_flags_t = 1 << 2;

pub const HV_IPA_GRANULE_16KB: hv_ipa_granule_t = 1;

pub const HV_EXIT_REASON_CANCELED: u32 = 0;
pub const HV_EXIT_REASON_EXCEPTION: u32 = 1;
pub const HV_EXIT_REASON_VTIMER_ACTIVATED: u32 = 2;

pub const HV_REG_X0: hv_reg_t = 0;
pub const HV_REG_PC: hv_reg_t = 31;
pub const HV_REG_FPCR: hv_reg_t = 32;
pub const HV_REG_FPSR: hv_reg_t = 33;
pub const HV_REG_CPSR: hv_reg_t = 34;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct hv_vcpu_exit_exception_t {
    pub syndrome: u64,
    pub virtual_address: u64,
    pub physical_address: hv_ipa_t,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct hv_vcpu_exit_t {
    pub reason: u32,
    pub exception: hv_vcpu_exit_exception_t,
}

#[link(name = "Hypervisor", kind = "framework")]
unsafe extern "C" {
    pub fn hv_vm_get_max_vcpu_count(max: *mut u32) -> hv_return_t;
    pub fn hv_vm_config_create() -> hv_vm_config_t;
    pub fn hv_vm_config_get_max_ipa_size(bits: *mut u32) -> hv_return_t;
    pub fn hv_vm_config_set_ipa_size(config: hv_vm_config_t, bits: u32) -> hv_return_t;
    pub fn hv_vm_create(config: hv_vm_config_t) -> hv_return_t;
    pub fn hv_vm_destroy() -> hv_return_t;
    pub fn hv_vm_map(addr: *mut c_void, ipa: hv_ipa_t, size: usize, flags: hv_memory_flags_t) -> hv_return_t;
    pub fn hv_vm_unmap(ipa: hv_ipa_t, size: usize) -> hv_return_t;
    pub fn hv_vm_protect(ipa: hv_ipa_t, size: usize, flags: hv_memory_flags_t) -> hv_return_t;

    pub fn hv_vcpu_create(
        vcpu: *mut hv_vcpu_t,
        exit: *mut *const hv_vcpu_exit_t,
        config: hv_vcpu_config_t,
    ) -> hv_return_t;
    pub fn hv_vcpu_destroy(vcpu: hv_vcpu_t) -> hv_return_t;
    pub fn hv_vcpu_run(vcpu: hv_vcpu_t) -> hv_return_t;
    pub fn hv_vcpus_exit(vcpus: *const hv_vcpu_t, count: u32) -> hv_return_t;
    pub fn hv_vcpu_get_reg(vcpu: hv_vcpu_t, reg: hv_reg_t, value: *mut u64) -> hv_return_t;
    pub fn hv_vcpu_set_reg(vcpu: hv_vcpu_t, reg: hv_reg_t, value: u64) -> hv_return_t;
    pub fn hv_vcpu_get_sys_reg(vcpu: hv_vcpu_t, reg: hv_sys_reg_t, value: *mut u64) -> hv_return_t;
    pub fn hv_vcpu_set_sys_reg(vcpu: hv_vcpu_t, reg: hv_sys_reg_t, value: u64) -> hv_return_t;
    /// `value` receives an `hv_simd_fp_uchar16_t`: 16 bytes.
    pub fn hv_vcpu_get_simd_fp_reg(vcpu: hv_vcpu_t, reg: hv_simd_fp_reg_t, value: *mut u8) -> hv_return_t;
    pub fn hv_vcpu_get_vtimer_offset(vcpu: hv_vcpu_t, offset: *mut u64) -> hv_return_t;
    pub fn hv_vcpu_set_vtimer_offset(vcpu: hv_vcpu_t, offset: u64) -> hv_return_t;

    pub fn hv_gic_config_create() -> hv_gic_config_t;
    pub fn hv_gic_config_set_distributor_base(config: hv_gic_config_t, base: hv_ipa_t) -> hv_return_t;
    pub fn hv_gic_config_set_redistributor_base(config: hv_gic_config_t, base: hv_ipa_t) -> hv_return_t;
    pub fn hv_gic_config_set_msi_region_base(config: hv_gic_config_t, base: hv_ipa_t) -> hv_return_t;
    pub fn hv_gic_config_set_msi_interrupt_range(
        config: hv_gic_config_t,
        base: u32,
        count: u32,
    ) -> hv_return_t;
    pub fn hv_gic_create(config: hv_gic_config_t) -> hv_return_t;
    pub fn hv_gic_set_spi(intid: u32, level: bool) -> hv_return_t;
    pub fn hv_gic_send_msi(address: hv_ipa_t, intid: u32) -> hv_return_t;
    pub fn hv_gic_get_distributor_size(size: *mut usize) -> hv_return_t;
    pub fn hv_gic_get_distributor_base_alignment(align: *mut usize) -> hv_return_t;
    pub fn hv_gic_get_redistributor_size(size: *mut usize) -> hv_return_t;
    pub fn hv_gic_get_redistributor_base_alignment(align: *mut usize) -> hv_return_t;
    pub fn hv_gic_get_msi_region_size(size: *mut usize) -> hv_return_t;
    pub fn hv_gic_get_msi_region_base_alignment(align: *mut usize) -> hv_return_t;
    pub fn hv_gic_get_spi_interrupt_range(base: *mut u32, count: *mut u32) -> hv_return_t;
    pub fn hv_gic_get_redistributor_base(vcpu: hv_vcpu_t, base: *mut hv_ipa_t) -> hv_return_t;
    #[cfg(test)]
    pub fn hv_gic_get_distributor_reg(reg: u16, value: *mut u64) -> hv_return_t;
    #[cfg(test)]
    pub fn hv_gic_set_distributor_reg(reg: u16, value: u64) -> hv_return_t;
    pub fn hv_gic_get_icc_reg(vcpu: hv_vcpu_t, reg: u16, value: *mut u64) -> hv_return_t;
    pub fn hv_gic_set_icc_reg(vcpu: hv_vcpu_t, reg: u16, value: u64) -> hv_return_t;
    pub fn hv_gic_state_create() -> hv_gic_state_t;
    pub fn hv_gic_state_get_size(state: hv_gic_state_t, size: *mut usize) -> hv_return_t;
    pub fn hv_gic_state_get_data(state: hv_gic_state_t, data: *mut c_void) -> hv_return_t;
    pub fn hv_gic_set_state(data: *const c_void, size: usize) -> hv_return_t;
}

unsafe extern "C" {
    /// `hv_vcpu_set_simd_fp_reg` with the vector passed in memory (src/hv/hvf/simd.c).
    pub fn shards_hv_vcpu_set_simd_fp_reg(
        vcpu: hv_vcpu_t,
        reg: hv_simd_fp_reg_t,
        bytes: *const u8,
    ) -> hv_return_t;
}

unsafe extern "C" {
    /// The host counter that `CNTVCT_EL0 = mach_absolute_time() - vtimer_offset` is
    /// relative to (hv_vcpu.h:438-449).
    pub fn mach_absolute_time() -> u64;
}

unsafe extern "C" {
    /// libSystem; releases `OS_OBJECT_DECL` objects (configs) created above.
    pub fn os_release(object: *mut c_void);
}

/// `hv_vm_config_set_ipa_granule`, from macOS 26. Linking it directly would stop the binary
/// from loading on macOS 15, our floor (the in-kernel GIC is 15.0), so it is looked up at
/// run time.
pub type SetIpaGranule = unsafe extern "C" fn(hv_vm_config_t, hv_ipa_granule_t) -> hv_return_t;

pub fn set_ipa_granule_fn() -> Option<SetIpaGranule> {
    // SAFETY: dlsym with RTLD_DEFAULT and a NUL-terminated name; Hypervisor.framework is
    // linked, so it is loaded before main runs.
    let f = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"hv_vm_config_set_ipa_granule".as_ptr()) };
    if f.is_null() {
        return None;
    }
    // SAFETY: the symbol is the function declared in hv_vm_config.h with this signature.
    Some(unsafe { std::mem::transmute::<*mut c_void, SetIpaGranule>(f) })
}
