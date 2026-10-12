//! WinHvPlatform.dll, the Windows Hypervisor Platform's API, for the subset shards uses: its
//! types are windows-sys's (Win32_System_Hypervisor, from WinHvPlatform.h), its functions
//! looked up the first time any is called, not linked. A binary linked to it starts only
//! where the optional feature "Windows Hypervisor Platform" is on; shards' every command but
//! a VM's must start everywhere, and say why a VM cannot.

#![allow(non_snake_case)]

use std::ffi::c_void;

pub use windows_sys::Win32::System::Hypervisor::*;
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows_sys::core::HRESULT;

/// What a call returns when the DLL, or the function, is not there: E_NOTIMPL.
pub const MISSING: HRESULT = 0x8000_4001_u32 as HRESULT;

macro_rules! winhv {
    ($( fn $name:ident($($arg:ident: $ty:ty),* $(,)?); )*) => {
        struct Table {
            /// Whether the DLL loaded.
            loaded: bool,
            $( $name: Option<unsafe extern "system" fn($($ty),*) -> HRESULT>, )*
        }

        fn load() -> Table {
            let name: Vec<u16> = "WinHvPlatform.dll".encode_utf16().chain([0]).collect();
            // SAFETY: a NUL-terminated name, loaded from System32 alone and kept for the
            // process's life.
            let module = unsafe { LoadLibraryExW(name.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
            let find = |name: &[u8]| -> *const c_void {
                if module.is_null() {
                    return std::ptr::null();
                }
                // SAFETY: a live module and a NUL-terminated name.
                match unsafe { GetProcAddress(module, name.as_ptr()) } {
                    Some(f) => f as *const c_void,
                    None => std::ptr::null(),
                }
            };
            Table {
                loaded: !module.is_null(),
                $( $name: {
                    let at = find(concat!(stringify!($name), "\0").as_bytes());
                    // SAFETY: the DLL's function of this name, whose signature the macro's
                    // line for it gives, from WinHvPlatform.h.
                    (!at.is_null()).then(|| unsafe {
                        std::mem::transmute::<*const c_void, unsafe extern "system" fn($($ty),*) -> HRESULT>(at)
                    })
                }, )*
            }
        }

        $(
            /// WinHvPlatform's function of this name.
            ///
            /// # Safety
            ///
            /// As the platform's own: its arguments as its header asks.
            pub unsafe fn $name($($arg: $ty),*) -> HRESULT {
                match table().$name {
                    // SAFETY: the platform's function, called as its header asks.
                    Some(f) => unsafe { f($($arg),*) },
                    None => MISSING,
                }
            }
        )*
    };
}

fn table() -> &'static Table {
    static TABLE: std::sync::OnceLock<Table> = std::sync::OnceLock::new();
    TABLE.get_or_init(load)
}

/// Whether WinHvPlatform.dll is on this host: the optional feature is on.
pub fn loaded() -> bool {
    table().loaded
}

winhv! {
    fn WHvGetCapability(code: WHV_CAPABILITY_CODE, buffer: *mut c_void, size: u32, written: *mut u32);
    fn WHvCreatePartition(partition: *mut WHV_PARTITION_HANDLE);
    fn WHvSetupPartition(partition: WHV_PARTITION_HANDLE);
    fn WHvDeletePartition(partition: WHV_PARTITION_HANDLE);
    fn WHvSetPartitionProperty(partition: WHV_PARTITION_HANDLE, code: WHV_PARTITION_PROPERTY_CODE, value: *const c_void, size: u32);
    fn WHvMapGpaRange(partition: WHV_PARTITION_HANDLE, source: *const c_void, gpa: u64, size: u64, flags: WHV_MAP_GPA_RANGE_FLAGS);
    fn WHvUnmapGpaRange(partition: WHV_PARTITION_HANDLE, gpa: u64, size: u64);
    fn WHvCreateVirtualProcessor(partition: WHV_PARTITION_HANDLE, index: u32, flags: u32);
    fn WHvDeleteVirtualProcessor(partition: WHV_PARTITION_HANDLE, index: u32);
    fn WHvRunVirtualProcessor(partition: WHV_PARTITION_HANDLE, index: u32, exit: *mut c_void, size: u32);
    fn WHvCancelRunVirtualProcessor(partition: WHV_PARTITION_HANDLE, index: u32, flags: u32);
    fn WHvGetVirtualProcessorRegisters(partition: WHV_PARTITION_HANDLE, index: u32, names: *const WHV_REGISTER_NAME, count: u32, values: *mut WHV_REGISTER_VALUE);
    fn WHvSetVirtualProcessorRegisters(partition: WHV_PARTITION_HANDLE, index: u32, names: *const WHV_REGISTER_NAME, count: u32, values: *const WHV_REGISTER_VALUE);
    fn WHvRequestInterrupt(partition: WHV_PARTITION_HANDLE, control: *const WHV_INTERRUPT_CONTROL, size: u32);
}

/// A register value at the 16 bytes' alignment WinHvPlatform.h gives WHV_REGISTER_VALUE
/// (`DECLSPEC_ALIGN(16)`) and windows-sys's declaration does not: handed over 8-aligned,
/// arrays of them fault in the DLL (PM M197).
#[repr(C, align(16))]
#[derive(Clone, Copy, Default)]
pub struct Value(pub WHV_REGISTER_VALUE);

/// The exit context at the same alignment, which holds register values.
#[repr(C, align(16))]
#[derive(Clone, Copy, Default)]
pub struct ExitContext(pub WHV_RUN_VP_EXIT_CONTEXT);

impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Value({:#x})", self.get())
    }
}

impl std::fmt::Debug for ExitContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ExitContext({:#x})", self.0.ExitReason)
    }
}

impl Value {
    pub fn u64(v: u64) -> Value {
        let mut r = WHV_REGISTER_VALUE::default();
        r.Reg64 = v;
        Value(r)
    }

    /// Its 64 bits.
    pub fn get(&self) -> u64 {
        // SAFETY: every register value's first 8 bytes are a u64's, whatever it holds.
        unsafe { self.0.Reg64 }
    }
}
