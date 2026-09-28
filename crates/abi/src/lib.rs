//! The contract between the shards VMM and software inside the guest. Anything both
//! sides must agree on lives here, so the two can never drift apart.

#![no_std]

/// Guest-physical address of the control page on arm64 guests. The guest writes
/// 32-bit markers at offset 0; the VMM timestamps each write.
pub const CONTROL_PAGE_AARCH64: u64 = 0x0902_0000;

/// Boot-phase markers written to the control page.
pub mod marker {
    /// PID 1 is executing.
    pub const INIT_STARTED: u32 = 1;
}
