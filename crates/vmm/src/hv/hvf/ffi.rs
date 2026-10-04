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

/// Hypervisor.framework's functions, declared once each: name, C signature, and what the
/// call returns when the framework could not be loaded or lacks it. They are looked up
/// with `dlsym` the first time any is called, not linked: a linked framework is loaded,
/// with what it needs, by dyld as every process of the binary starts, 1.1 ms of a launch
/// on an M5 Max (PM M113), and the CLI and daemon never call one. A call through the
/// table costs what a call through dyld's stub does: one load and an indirect branch.
macro_rules! hypervisor {
    ($( $(#[$meta:meta])* fn $name:ident($($arg:ident: $ty:ty),* $(,)?) -> $ret:ty = $failed:expr; )*) => {
        #[allow(non_snake_case)]
        struct Table {
            /// The framework's handle, for symbols looked up later.
            handle: usize,
            $( $(#[$meta])* $name: Option<unsafe extern "C" fn($($ty),*) -> $ret>, )*
        }

        fn load() -> Table {
            // SAFETY: a NUL-terminated path; the handle is kept for the process's life.
            let handle = unsafe {
                libc::dlopen(
                    c"/System/Library/Frameworks/Hypervisor.framework/Hypervisor".as_ptr(),
                    libc::RTLD_LAZY | libc::RTLD_LOCAL,
                )
            };
            let find = |name: &std::ffi::CStr| -> *mut c_void {
                if handle.is_null() {
                    return std::ptr::null_mut();
                }
                // SAFETY: a live handle and a NUL-terminated name.
                unsafe { libc::dlsym(handle, name.as_ptr()) }
            };
            Table {
                handle: handle as usize,
                $( $(#[$meta])* $name: {
                    let at = find(&std::ffi::CString::new(stringify!($name)).unwrap_or_default());
                    // SAFETY: the framework's function of this name, whose signature the
                    // macro's line for it gives, from the macOS 26.4 SDK's headers.
                    (!at.is_null()).then(|| unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn($($ty),*) -> $ret>(at) })
                }, )*
            }
        }

        $(
            $(#[$meta])*
            /// Hypervisor.framework's function of this name.
            ///
            /// # Safety
            ///
            /// As the framework's own: its arguments as its header asks.
            pub unsafe fn $name($($arg: $ty),*) -> $ret {
                match table().$name {
                    // SAFETY: the framework's function, called as its header asks.
                    Some(f) => unsafe { f($($arg),*) },
                    None => $failed,
                }
            }
        )*
    };
}

fn table() -> &'static Table {
    static TABLE: std::sync::OnceLock<Table> = std::sync::OnceLock::new();
    TABLE.get_or_init(load)
}

/// What a call returns when the framework, or the function, is not there:
/// `HV_UNSUPPORTED` (hv_error.h).
const MISSING: hv_return_t = 0xfae9_400f_u32 as hv_return_t;

hypervisor! {
    fn hv_vm_get_max_vcpu_count(max: *mut u32) -> hv_return_t = MISSING;
    fn hv_vm_config_create() -> hv_vm_config_t = std::ptr::null_mut();
    fn hv_vm_config_get_max_ipa_size(bits: *mut u32) -> hv_return_t = MISSING;
    fn hv_vm_config_set_ipa_size(config: hv_vm_config_t, bits: u32) -> hv_return_t = MISSING;
    fn hv_vm_create(config: hv_vm_config_t) -> hv_return_t = MISSING;
    fn hv_vm_destroy() -> hv_return_t = MISSING;
    fn hv_vm_map(addr: *mut c_void, ipa: hv_ipa_t, size: usize, flags: hv_memory_flags_t) -> hv_return_t = MISSING;
    fn hv_vm_unmap(ipa: hv_ipa_t, size: usize) -> hv_return_t = MISSING;
    fn hv_vm_protect(ipa: hv_ipa_t, size: usize, flags: hv_memory_flags_t) -> hv_return_t = MISSING;
    fn hv_vcpu_create(vcpu: *mut hv_vcpu_t, exit: *mut *const hv_vcpu_exit_t, config: hv_vcpu_config_t) -> hv_return_t = MISSING;
    fn hv_vcpu_destroy(vcpu: hv_vcpu_t) -> hv_return_t = MISSING;
    fn hv_vcpu_run(vcpu: hv_vcpu_t) -> hv_return_t = MISSING;
    fn hv_vcpus_exit(vcpus: *const hv_vcpu_t, count: u32) -> hv_return_t = MISSING;
    fn hv_vcpu_get_reg(vcpu: hv_vcpu_t, reg: hv_reg_t, value: *mut u64) -> hv_return_t = MISSING;
    fn hv_vcpu_set_reg(vcpu: hv_vcpu_t, reg: hv_reg_t, value: u64) -> hv_return_t = MISSING;
    fn hv_vcpu_get_sys_reg(vcpu: hv_vcpu_t, reg: hv_sys_reg_t, value: *mut u64) -> hv_return_t = MISSING;
    fn hv_vcpu_set_sys_reg(vcpu: hv_vcpu_t, reg: hv_sys_reg_t, value: u64) -> hv_return_t = MISSING;
    // `value` receives an `hv_simd_fp_uchar16_t`: 16 bytes.
    fn hv_vcpu_get_simd_fp_reg(vcpu: hv_vcpu_t, reg: hv_simd_fp_reg_t, value: *mut u8) -> hv_return_t = MISSING;
    fn hv_vcpu_get_vtimer_offset(vcpu: hv_vcpu_t, offset: *mut u64) -> hv_return_t = MISSING;
    fn hv_vcpu_set_vtimer_offset(vcpu: hv_vcpu_t, offset: u64) -> hv_return_t = MISSING;
    fn hv_gic_config_create() -> hv_gic_config_t = std::ptr::null_mut();
    fn hv_gic_config_set_distributor_base(config: hv_gic_config_t, base: hv_ipa_t) -> hv_return_t = MISSING;
    fn hv_gic_config_set_redistributor_base(config: hv_gic_config_t, base: hv_ipa_t) -> hv_return_t = MISSING;
    fn hv_gic_config_set_msi_region_base(config: hv_gic_config_t, base: hv_ipa_t) -> hv_return_t = MISSING;
    fn hv_gic_config_set_msi_interrupt_range(config: hv_gic_config_t, base: u32, count: u32) -> hv_return_t = MISSING;
    fn hv_gic_create(config: hv_gic_config_t) -> hv_return_t = MISSING;
    fn hv_gic_set_spi(intid: u32, level: bool) -> hv_return_t = MISSING;
    fn hv_gic_send_msi(address: hv_ipa_t, intid: u32) -> hv_return_t = MISSING;
    fn hv_gic_get_distributor_size(size: *mut usize) -> hv_return_t = MISSING;
    fn hv_gic_get_distributor_base_alignment(align: *mut usize) -> hv_return_t = MISSING;
    fn hv_gic_get_redistributor_size(size: *mut usize) -> hv_return_t = MISSING;
    fn hv_gic_get_redistributor_base_alignment(align: *mut usize) -> hv_return_t = MISSING;
    fn hv_gic_get_msi_region_size(size: *mut usize) -> hv_return_t = MISSING;
    fn hv_gic_get_msi_region_base_alignment(align: *mut usize) -> hv_return_t = MISSING;
    fn hv_gic_get_spi_interrupt_range(base: *mut u32, count: *mut u32) -> hv_return_t = MISSING;
    fn hv_gic_get_redistributor_base(vcpu: hv_vcpu_t, base: *mut hv_ipa_t) -> hv_return_t = MISSING;
    #[cfg(test)]
    fn hv_gic_get_distributor_reg(reg: u16, value: *mut u64) -> hv_return_t = MISSING;
    #[cfg(test)]
    fn hv_gic_set_distributor_reg(reg: u16, value: u64) -> hv_return_t = MISSING;
    fn hv_gic_get_icc_reg(vcpu: hv_vcpu_t, reg: u16, value: *mut u64) -> hv_return_t = MISSING;
    fn hv_gic_set_icc_reg(vcpu: hv_vcpu_t, reg: u16, value: u64) -> hv_return_t = MISSING;
    fn hv_gic_state_create() -> hv_gic_state_t = std::ptr::null_mut();
    fn hv_gic_state_get_size(state: hv_gic_state_t, size: *mut usize) -> hv_return_t = MISSING;
    fn hv_gic_state_get_data(state: hv_gic_state_t, data: *mut c_void) -> hv_return_t = MISSING;
    fn hv_gic_set_state(data: *const c_void, size: usize) -> hv_return_t = MISSING;
}

/// `hv_vcpu_set_simd_fp_reg`, as the framework exports it: its vector by value, which
/// only C can pass (simd.c), given this pointer.
pub type SetSimdFpReg = *mut c_void;

unsafe extern "C" {
    /// Calls `set` (`hv_vcpu_set_simd_fp_reg`) with the vector passed in memory
    /// (src/hv/hvf/simd.c).
    fn shards_hv_call_set_simd_fp_reg(
        set: SetSimdFpReg,
        vcpu: hv_vcpu_t,
        reg: hv_simd_fp_reg_t,
        bytes: *const u8,
    ) -> hv_return_t;
}

/// `hv_vcpu_set_simd_fp_reg` with the vector passed in memory.
///
/// # Safety
///
/// `bytes` is 16 readable bytes; the vCPU is this thread's.
pub unsafe fn shards_hv_vcpu_set_simd_fp_reg(
    vcpu: hv_vcpu_t,
    reg: hv_simd_fp_reg_t,
    bytes: *const u8,
) -> hv_return_t {
    static SET: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let set = *SET.get_or_init(|| lookup(c"hv_vcpu_set_simd_fp_reg") as usize);
    if set == 0 {
        return MISSING;
    }
    // SAFETY: the framework's function, and 16 bytes, as the caller promises.
    unsafe { shards_hv_call_set_simd_fp_reg(set as SetSimdFpReg, vcpu, reg, bytes) }
}

/// A symbol of the framework, or null.
fn lookup(name: &std::ffi::CStr) -> *mut c_void {
    let handle = table().handle as *mut c_void;
    if handle.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: a live handle and a NUL-terminated name.
    unsafe { libc::dlsym(handle, name.as_ptr()) }
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

/// `hv_vm_config_set_ipa_granule`, from macOS 26, where macOS 15 (our floor: the in-kernel
/// GIC is 15.0) has none.
pub type SetIpaGranule = unsafe extern "C" fn(hv_vm_config_t, hv_ipa_granule_t) -> hv_return_t;

pub fn set_ipa_granule_fn() -> Option<SetIpaGranule> {
    let f = lookup(c"hv_vm_config_set_ipa_granule");
    if f.is_null() {
        return None;
    }
    // SAFETY: the symbol is the function declared in hv_vm_config.h with this signature.
    Some(unsafe { std::mem::transmute::<*mut c_void, SetIpaGranule>(f) })
}
