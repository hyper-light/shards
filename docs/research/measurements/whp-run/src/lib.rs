//! WinHvPlatform and WinHvEmulation, bound at run time (LoadLibraryExW, GetProcAddress) as
//! shards binds them, so that a probe starts where they are not; and what the probes share.
//! The probes: `main.rs` (PM M197) and `bin/m198.rs` (PM M198).

pub mod confine;

use std::ffi::c_void;
use std::time::Duration;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Hypervisor::*;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
use windows_sys::core::HRESULT;

pub type GetCapability = unsafe extern "system" fn(WHV_CAPABILITY_CODE, *mut c_void, u32, *mut u32) -> HRESULT;
pub type CreatePartition = unsafe extern "system" fn(*mut WHV_PARTITION_HANDLE) -> HRESULT;
pub type SetPartitionProperty =
    unsafe extern "system" fn(WHV_PARTITION_HANDLE, WHV_PARTITION_PROPERTY_CODE, *const c_void, u32) -> HRESULT;
pub type SetupPartition = unsafe extern "system" fn(WHV_PARTITION_HANDLE) -> HRESULT;
pub type DeletePartition = unsafe extern "system" fn(WHV_PARTITION_HANDLE) -> HRESULT;
pub type MapGpaRange =
    unsafe extern "system" fn(WHV_PARTITION_HANDLE, *const c_void, u64, u64, WHV_MAP_GPA_RANGE_FLAGS) -> HRESULT;
pub type CreateVp = unsafe extern "system" fn(WHV_PARTITION_HANDLE, u32, u32) -> HRESULT;
pub type DeleteVp = unsafe extern "system" fn(WHV_PARTITION_HANDLE, u32) -> HRESULT;
pub type SetRegs = unsafe extern "system" fn(
    WHV_PARTITION_HANDLE,
    u32,
    *const WHV_REGISTER_NAME,
    u32,
    *const WHV_REGISTER_VALUE,
) -> HRESULT;
pub type GetRegs =
    unsafe extern "system" fn(WHV_PARTITION_HANDLE, u32, *const WHV_REGISTER_NAME, u32, *mut WHV_REGISTER_VALUE) -> HRESULT;
pub type RunVp = unsafe extern "system" fn(WHV_PARTITION_HANDLE, u32, *mut c_void, u32) -> HRESULT;
pub type TranslateGva = unsafe extern "system" fn(
    WHV_PARTITION_HANDLE,
    u32,
    u64,
    WHV_TRANSLATE_GVA_FLAGS,
    *mut WHV_TRANSLATE_GVA_RESULT,
    *mut u64,
) -> HRESULT;
pub type EmuCreate = unsafe extern "system" fn(*const WHV_EMULATOR_CALLBACKS, *mut *mut c_void) -> HRESULT;
pub type EmuMmio = unsafe extern "system" fn(
    *const c_void,
    *const c_void,
    *const WHV_VP_EXIT_CONTEXT,
    *const WHV_MEMORY_ACCESS_CONTEXT,
    *mut WHV_EMULATOR_STATUS,
) -> HRESULT;
pub type EmuIo = unsafe extern "system" fn(
    *const c_void,
    *const c_void,
    *const WHV_VP_EXIT_CONTEXT,
    *const WHV_X64_IO_PORT_ACCESS_CONTEXT,
    *mut WHV_EMULATOR_STATUS,
) -> HRESULT;
pub type EmuDestroy = unsafe extern "system" fn(*const c_void) -> HRESULT;
pub type CancelRun = unsafe extern "system" fn(WHV_PARTITION_HANDLE, u32, u32) -> HRESULT;
pub type RequestInterrupt = unsafe extern "system" fn(WHV_PARTITION_HANDLE, *const WHV_INTERRUPT_CONTROL, u32) -> HRESULT;
pub type CreatePort = unsafe extern "system" fn(
    WHV_PARTITION_HANDLE,
    *const WHV_NOTIFICATION_PORT_PARAMETERS,
    HANDLE,
    *mut *mut c_void,
) -> HRESULT;
pub type DeletePort = unsafe extern "system" fn(WHV_PARTITION_HANDLE, *const c_void) -> HRESULT;

/// `T` at 16 bytes' alignment, which WinHvPlatform.h gives its register values
/// (`DECLSPEC_ALIGN(16)` on WHV_REGISTER_VALUE and WHV_UINT128) and windows-sys's
/// declarations do not: an array of them handed over 8-aligned may fault in the DLL.
#[repr(C, align(16))]
pub struct Aligned<T>(pub T);

/// WinHvPlatform's and WinHvEmulation's functions, bound.
pub struct Api {
    pub get_capability: GetCapability,
    pub create_partition: CreatePartition,
    pub set_property: SetPartitionProperty,
    pub setup_partition: SetupPartition,
    pub delete_partition: DeletePartition,
    pub map: MapGpaRange,
    pub create_vp: CreateVp,
    pub delete_vp: DeleteVp,
    pub set_regs: SetRegs,
    pub get_regs: GetRegs,
    pub run: RunVp,
    pub translate: TranslateGva,
    pub emu_create: EmuCreate,
    pub emu_mmio: EmuMmio,
    pub emu_io: EmuIo,
    pub emu_destroy: EmuDestroy,
    pub cancel: CancelRun,
    pub request_interrupt: RequestInterrupt,
    pub create_port: CreatePort,
    pub delete_port: DeletePort,
}

/// A system DLL, loaded from System32 alone.
fn library(name: &str) -> Result<*mut c_void, String> {
    let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
    // SAFETY: a NUL-terminated wide string.
    let lib = unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
    if lib.is_null() {
        return Err(format!("{name}: {}", std::io::Error::last_os_error()));
    }
    Ok(lib)
}

/// Function `name` of `lib`, as type `T`, a function pointer of its signature.
unsafe fn sym<T: Copy>(lib: *mut c_void, name: &str) -> Result<T, String> {
    let c: Vec<u8> = name.bytes().chain([0]).collect();
    // SAFETY: a module handle LoadLibraryExW gave, and a NUL-terminated name.
    match unsafe { GetProcAddress(lib, c.as_ptr()) } {
        // SAFETY: the caller names `T` as the function's own signature, a pointer's size.
        Some(f) => Ok(unsafe { std::mem::transmute_copy(&f) }),
        None => Err(format!("{name}: {}", std::io::Error::last_os_error())),
    }
}

pub fn bind() -> Result<Api, String> {
    let p = library("WinHvPlatform.dll")?;
    let e = library("WinHvEmulation.dll")?;
    // SAFETY: each name with its own signature, from WinHvPlatform.h and WinHvEmulation.h.
    unsafe {
        Ok(Api {
            get_capability: sym(p, "WHvGetCapability")?,
            create_partition: sym(p, "WHvCreatePartition")?,
            set_property: sym(p, "WHvSetPartitionProperty")?,
            setup_partition: sym(p, "WHvSetupPartition")?,
            delete_partition: sym(p, "WHvDeletePartition")?,
            map: sym(p, "WHvMapGpaRange")?,
            create_vp: sym(p, "WHvCreateVirtualProcessor")?,
            delete_vp: sym(p, "WHvDeleteVirtualProcessor")?,
            set_regs: sym(p, "WHvSetVirtualProcessorRegisters")?,
            get_regs: sym(p, "WHvGetVirtualProcessorRegisters")?,
            run: sym(p, "WHvRunVirtualProcessor")?,
            translate: sym(p, "WHvTranslateGva")?,
            emu_create: sym(e, "WHvEmulatorCreateEmulator")?,
            emu_mmio: sym(e, "WHvEmulatorTryMmioEmulation")?,
            emu_io: sym(e, "WHvEmulatorTryIoEmulation")?,
            emu_destroy: sym(e, "WHvEmulatorDestroyEmulator")?,
            cancel: sym(p, "WHvCancelRunVirtualProcessor")?,
            request_interrupt: sym(p, "WHvRequestInterrupt")?,
            create_port: sym(p, "WHvCreateNotificationPort")?,
            delete_port: sym(p, "WHvDeleteNotificationPort")?,
        })
    }
}

pub fn check(what: &str, hr: HRESULT) -> Result<(), String> {
    if hr < 0 {
        return Err(format!("{what}: HRESULT {:#010x}", hr as u32));
    }
    Ok(())
}

/// Capability `code` as its union, and whether the host answered it.
pub fn capability(api: &Api, code: WHV_CAPABILITY_CODE) -> Option<WHV_CAPABILITY> {
    let mut cap = WHV_CAPABILITY::default();
    let mut written = 0u32;
    // SAFETY: a buffer of the union's size, which the call fills.
    let hr = unsafe {
        (api.get_capability)(
            code,
            (&raw mut cap).cast(),
            std::mem::size_of::<WHV_CAPABILITY>() as u32,
            &raw mut written,
        )
    };
    (hr >= 0).then_some(cap)
}

/// `values`, sorted: n, and the nearest-rank p50, p90, p99 and max, in µs.
pub fn summary(values: &mut [Duration]) -> String {
    values.sort();
    let n = values.len();
    if n == 0 {
        return "n=0".into();
    }
    let rank = |p: usize| values[(n * p).div_ceil(100).clamp(1, n) - 1].as_secs_f64() * 1e6;
    format!(
        "n={n} p50={:.2} p90={:.2} p99={:.2} max={:.2}",
        rank(50),
        rank(90),
        rank(99),
        rank(100)
    )
}

pub fn segment(selector: u16, attributes: u16) -> WHV_REGISTER_VALUE {
    let mut v = WHV_REGISTER_VALUE::default();
    v.Segment = WHV_X64_SEGMENT_REGISTER {
        Base: 0,
        Limit: 0xFFFF_FFFF,
        Selector: selector,
        Anonymous: WHV_X64_SEGMENT_REGISTER_0 {
            Attributes: attributes,
        },
    };
    v
}

pub fn reg64(x: u64) -> WHV_REGISTER_VALUE {
    let mut v = WHV_REGISTER_VALUE::default();
    v.Reg64 = x;
    v
}
