//! WinHvPlatform.dll on arm64: its types as the Windows SDK 10.0.26100's WinHvPlatformDefs.h
//! gives them in its `_ARM64_` branches, which windows-sys 0.61 does not carry (its metadata
//! is x64's), each size held to the header's C_ASSERT by `sizes`; and its functions, bound
//! when first called.

#![allow(dead_code, non_snake_case)]

use std::ffi::c_void;
use std::sync::OnceLock;

use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};

pub type Hresult = i32;
/// WHV_PARTITION_HANDLE.
pub type Partition = *mut c_void;

// WHV_CAPABILITY_CODE
pub const CAP_HYPERVISOR_PRESENT: u32 = 0x0000_0000;
pub const CAP_FEATURES: u32 = 0x0000_0001;
pub const CAP_EXTENDED_VM_EXITS: u32 = 0x0000_0002;
pub const CAP_PROCESSOR_VENDOR: u32 = 0x0000_1000;
pub const CAP_PROCESSOR_FEATURES: u32 = 0x0000_1001;
pub const CAP_PROCESSOR_CLOCK_FREQUENCY: u32 = 0x0000_1004;
pub const CAP_SYNTHETIC_PROCESSOR_FEATURES_BANKS: u32 = 0x0000_1008;
pub const CAP_PHYSICAL_ADDRESS_WIDTH: u32 = 0x0000_100A;
pub const CAP_GIC_LPI_INT_ID_BITS: u32 = 0x0000_2011;
pub const CAP_MAX_SVE_VECTOR_LENGTH: u32 = 0x0000_2012;

/// WHV_CAPABILITY_FEATURES' Arm64Support (bit 11): the platform's arm64 API as released, which
/// QEMU asks for, where 24H2 builds before 26100.3915 shipped a pre-release one
/// (docs/system/whpx.rst).
pub const FEATURE_ARM64_SUPPORT: u64 = 1 << 11;

// WHV_PARTITION_PROPERTY_CODE
pub const PROP_EXTENDED_VM_EXITS: u32 = 0x0000_0001;
pub const PROP_ARM64_IC_PARAMETERS: u32 = 0x0000_1012;
pub const PROP_PROCESSOR_COUNT: u32 = 0x0000_1fff;

// WHV_RUN_VP_EXIT_REASON
pub const EXIT_NONE: u32 = 0x0000_0000;
pub const EXIT_UNMAPPED_GPA: u32 = 0x8000_0000;
pub const EXIT_GPA_INTERCEPT: u32 = 0x8000_0001;
pub const EXIT_INVALID_VP_REGISTER_VALUE: u32 = 0x8000_0020;
pub const EXIT_UNRECOVERABLE_EXCEPTION: u32 = 0x8000_0021;
pub const EXIT_UNSUPPORTED_FEATURE: u32 = 0x8000_0022;
pub const EXIT_HYPERCALL: u32 = 0x8000_0050;
pub const EXIT_SYNIC_SINT_DELIVERABLE: u32 = 0x8000_0062;
pub const EXIT_REGISTER_INTERCEPT: u32 = 0x8001_0006;
pub const EXIT_ARM64_RESET: u32 = 0x8001_000c;
pub const EXIT_CANCELED: u32 = 0xFFFF_FFFF;

// WHV_ARM64_RESET_TYPE
pub const RESET_POWER_OFF: u32 = 0;
pub const RESET_REBOOT: u32 = 1;

// WHV_REGISTER_NAME: X0 to X28 follow one another.
pub const X0: u32 = 0x0002_0000;
pub const FP: u32 = 0x0002_001D;
pub const LR: u32 = 0x0002_001E;
pub const SP: u32 = 0x0002_001F;
pub const SP_EL0: u32 = 0x0002_0020;
pub const SP_EL1: u32 = 0x0002_0021;
pub const PC: u32 = 0x0002_0022;
pub const PSTATE: u32 = 0x0002_0023;
pub const Q0: u32 = 0x0003_0000;
pub const MPIDR_EL1: u32 = 0x0004_0001;
pub const SCTLR_EL1: u32 = 0x0004_0002;
pub const CPACR_EL1: u32 = 0x0004_0004;
pub const TTBR0_EL1: u32 = 0x0004_0005;
pub const TTBR1_EL1: u32 = 0x0004_0006;
pub const TCR_EL1: u32 = 0x0004_0007;
pub const ESR_EL1: u32 = 0x0004_0008;
pub const FAR_EL1: u32 = 0x0004_0009;
pub const PAR_EL1: u32 = 0x0004_000A;
pub const MAIR_EL1: u32 = 0x0004_000B;
pub const VBAR_EL1: u32 = 0x0004_000C;
pub const CONTEXTIDR_EL1: u32 = 0x0004_000D;
pub const TPIDR_EL1: u32 = 0x0004_000E;
pub const TPIDRRO_EL0: u32 = 0x0004_0010;
pub const TPIDR_EL0: u32 = 0x0004_0011;
pub const FPCR: u32 = 0x0004_0012;
pub const FPSR: u32 = 0x0004_0013;
pub const SPSR_EL1: u32 = 0x0004_0014;
pub const ELR_EL1: u32 = 0x0004_0015;
pub const CSSELR_EL1: u32 = 0x0004_0035;
pub const MIDR_EL1: u32 = 0x0004_0051;
pub const ZCR_EL1: u32 = 0x0004_0071;
pub const MDSCR_EL1: u32 = 0x0005_004D;
pub const CNTKCTL_EL1: u32 = 0x0005_8008;
pub const CNTV_CTL_EL0: u32 = 0x0005_800E;
pub const CNTV_CVAL_EL0: u32 = 0x0005_800F;
pub const CNTVCT_EL0: u32 = 0x0005_8011;
pub const ID_AA64PFR0_EL1: u32 = 0x0002_2020;
pub const ID_AA64PFR1_EL1: u32 = 0x0002_2021;
pub const ID_AA64ISAR0_EL1: u32 = 0x0002_2030;
pub const ID_AA64MMFR0_EL1: u32 = 0x0002_2038;
pub const GICR_BASE_GPA: u32 = 0x0006_3000;
pub const INTERNAL_ACTIVITY_STATE: u32 = 0x0000_0004;

/// WHV_ANY_VP: a partition-wide register's, or state's, vCPU.
pub const ANY_VP: u32 = 0xFFFF_FFFF;

// WHV_MAP_GPA_RANGE_FLAGS
pub const MAP_READ: u32 = 1;
pub const MAP_WRITE: u32 = 2;
pub const MAP_EXECUTE: u32 = 4;

// WHV_VIRTUAL_PROCESSOR_STATE_TYPE on arm64
pub const STATE_PFN: u32 = 1 << 31;
pub const STATE_ANY_VP: u32 = 1 << 30;
pub const STATE_INTERRUPT_CONTROLLER: u32 = STATE_PFN;
pub const STATE_SYNIC_TIMER: u32 = 4;
pub const STATE_GLOBAL_INTERRUPT: u32 = 6 | STATE_PFN | STATE_ANY_VP;

// WHV_ARM64_IC_EMULATION_MODE, WHV_INTERRUPT_TYPE
pub const IC_EMULATION_GICV3: u32 = 1;
pub const INTERRUPT_TYPE_FIXED: u32 = 0;

/// WHV_REGISTER_VALUE: 16 bytes, at the 16 bytes' alignment the header declares.
#[repr(C, align(16))]
#[derive(Clone, Copy, Default, Debug)]
pub struct Value(pub [u64; 2]);

/// WHV_INTERCEPT_MESSAGE_HEADER.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct InterceptHeader {
    pub vp_index: u32,
    pub instruction_length: u8,
    pub intercept_access_type: u8,
    pub execution_state: u16,
    pub pc: u64,
    pub cpsr: u64,
}

/// WHV_MEMORY_ACCESS_CONTEXT.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct MemoryAccess {
    pub header: InterceptHeader,
    pub reserved0: u32,
    pub instruction_byte_count: u8,
    pub access_info: u8,
    pub reserved1: u16,
    pub instruction_bytes: [u8; 4],
    pub reserved2: u32,
    pub gva: u64,
    pub gpa: u64,
    pub syndrome: u64,
}

/// WHV_ARM64_RESET_CONTEXT.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct Reset {
    pub header: InterceptHeader,
    pub reset_type: u32,
    pub reserved: u32,
}

/// WHV_ARM64_HYPERCALL_CONTEXT.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct Hypercall {
    pub header: InterceptHeader,
    pub immediate: u16,
    pub reserved1: u16,
    pub reserved2: u32,
    pub x: [u64; 18],
}

/// WHV_RUN_VP_EXIT_CONTEXT: its reason, then the union of what each reason gives.
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug)]
pub struct ExitContext {
    pub exit_reason: u32,
    pub reserved: u32,
    pub reserved1: u64,
    pub body: [u64; 32],
}

impl Default for ExitContext {
    fn default() -> Self {
        ExitContext { exit_reason: 0, reserved: 0, reserved1: 0, body: [0; 32] }
    }
}

impl ExitContext {
    fn read<T: Copy>(&self) -> T {
        const { assert!(size_of::<T>() <= 256) };
        // SAFETY: the union's bytes, at least `T`'s many, read unaligned as the plain-data
        // member the reason says it holds.
        unsafe { std::ptr::read_unaligned(self.body.as_ptr().cast::<T>()) }
    }

    pub fn memory_access(&self) -> MemoryAccess {
        self.read()
    }

    pub fn reset(&self) -> Reset {
        self.read()
    }

    pub fn hypercall(&self) -> Hypercall {
        self.read()
    }

    pub fn header(&self) -> InterceptHeader {
        self.read()
    }
}

/// WHV_ARM64_IC_GIC_V3_PARAMETERS.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct GicV3Parameters {
    pub gicd_base: u64,
    pub gits_translater_base: u64,
    pub reserved: u32,
    pub gic_lpi_int_id_bits: u32,
    pub ppi_cntv_overflow: u32,
    pub ppi_performance_monitors: u32,
    pub reserved1: [u32; 6],
}

/// WHV_ARM64_IC_PARAMETERS.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct IcParameters {
    pub emulation_mode: u32,
    pub reserved: u32,
    pub gic_v3: GicV3Parameters,
}

/// WHV_INTERRUPT_CONTROL on arm64: `control` is WHV_INTERRUPT_CONTROL2, its type in the low
/// 32 bits and Asserted at bit 34.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct InterruptControl {
    pub target_partition: u64,
    pub control: u64,
    pub destination_address: u64,
    pub requested_vector: u32,
    pub target_vtl: u8,
    pub reserved_z0: u8,
    pub reserved_z1: u16,
}

pub const INTERRUPT_ASSERTED: u64 = 1 << 34;

macro_rules! winhv {
    ($( fn $name:ident($($arg:ident: $ty:ty),* $(,)?); )*) => {
        struct Table {
            $( $name: Option<unsafe extern "system" fn($($ty),*) -> Hresult>, )*
        }

        fn table() -> &'static Table {
            static TABLE: OnceLock<Table> = OnceLock::new();
            TABLE.get_or_init(|| {
                let name: Vec<u16> = "WinHvPlatform.dll".encode_utf16().chain([0]).collect();
                // SAFETY: a NUL-terminated name, loaded from System32 alone.
                let module = unsafe { LoadLibraryExW(name.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
                Table {
                    $( $name: if module.is_null() {
                        None
                    } else {
                        // SAFETY: a live module and a NUL-terminated name; the function's
                        // signature is its header's.
                        unsafe {
                            GetProcAddress(module, concat!(stringify!($name), "\0").as_ptr()).map(|f| {
                                std::mem::transmute::<unsafe extern "system" fn() -> isize, unsafe extern "system" fn($($ty),*) -> Hresult>(f)
                            })
                        }
                    }, )*
                }
            })
        }

        /// Whether WinHvPlatform.dll is here, with WHvGetCapability.
        pub fn loaded() -> bool {
            table().WHvGetCapability.is_some()
        }

        $(
            /// The platform's function of this name.
            ///
            /// # Safety
            ///
            /// As the platform's own: its arguments as its header asks.
            #[allow(non_snake_case)]
            pub unsafe fn $name($($arg: $ty),*) -> Hresult {
                match table().$name {
                    // SAFETY: the platform's function, called as its header asks.
                    Some(f) => unsafe { f($($arg),*) },
                    // E_NOTIMPL
                    None => 0x8000_4001_u32 as i32,
                }
            }
        )*
    };
}

winhv! {
    fn WHvGetCapability(code: u32, buffer: *mut c_void, size: u32, written: *mut u32);
    fn WHvCreatePartition(partition: *mut Partition);
    fn WHvSetupPartition(partition: Partition);
    fn WHvResetPartition(partition: Partition);
    fn WHvDeletePartition(partition: Partition);
    fn WHvSetPartitionProperty(partition: Partition, code: u32, value: *const c_void, size: u32);
    fn WHvMapGpaRange(partition: Partition, source: *const c_void, gpa: u64, size: u64, flags: u32);
    fn WHvCreateVirtualProcessor(partition: Partition, index: u32, flags: u32);
    fn WHvRunVirtualProcessor(partition: Partition, index: u32, exit: *mut c_void, size: u32);
    fn WHvCancelRunVirtualProcessor(partition: Partition, index: u32, flags: u32);
    fn WHvGetVirtualProcessorRegisters(partition: Partition, index: u32, names: *const u32, count: u32, values: *mut Value);
    fn WHvSetVirtualProcessorRegisters(partition: Partition, index: u32, names: *const u32, count: u32, values: *const Value);
    fn WHvGetVirtualProcessorState(partition: Partition, index: u32, kind: u32, buffer: *mut c_void, size: u32, written: *mut u32);
    fn WHvSetVirtualProcessorState(partition: Partition, index: u32, kind: u32, buffer: *const c_void, size: u32);
    fn WHvRequestInterrupt(partition: Partition, control: *const InterruptControl, size: u32);
}

#[cfg(test)]
mod sizes {
    use super::*;

    /// Each as WinHvPlatformDefs.h's C_ASSERT says.
    #[test]
    fn as_the_header_asserts() {
        assert_eq!(size_of::<InterceptHeader>(), 24);
        assert_eq!(size_of::<MemoryAccess>(), 64);
        assert_eq!(size_of::<Reset>(), 32);
        assert_eq!(size_of::<ExitContext>(), 272);
        assert_eq!(size_of::<GicV3Parameters>(), 56);
        assert_eq!(size_of::<IcParameters>(), 64);
        assert_eq!(size_of::<InterruptControl>(), 32);
        assert_eq!(size_of::<Value>(), 16);
        assert_eq!(std::mem::offset_of!(MemoryAccess, syndrome), 56);
        assert_eq!(std::mem::offset_of!(ExitContext, body), 16);
    }
}
