//! The contract between the shards VMM and software inside the guest. Anything both
//! sides must agree on lives here, so the two can never drift apart.

#![no_std]

/// Guest-physical address of the control page. Guests share the host's architecture,
/// so the VMM and the guest build of this crate always agree.
#[cfg(target_arch = "aarch64")]
pub const CONTROL_PAGE: u64 = 0x0902_0000;
/// On x86_64, the first page of the 32-bit MMIO gap, where Firecracker's boot timer is
/// (docs/research/kvm-x86_64-ground-truth.md §6.4).
#[cfg(target_arch = "x86_64")]
pub const CONTROL_PAGE: u64 = 0xc000_0000;

/// Registers of the control page: 32-bit, little-endian.
pub mod control {
    /// Write: a boot-phase marker (see [`super::marker`]); the VMM timestamps it.
    pub const MARKER: u64 = 0x00;
    /// Write [`SNAPSHOT_NOW`]: the VM pauses right after this store, and the VMM writes a
    /// snapshot. The store completes first, so a restored guest resumes at the next
    /// instruction.
    pub const SNAPSHOT: u64 = 0x04;
    /// Read: how many restores this guest's lineage went through: 0 in the VM that
    /// booted, and one more in each VM restored from a snapshot of it.
    pub const GENERATION: u64 = 0x08;

    pub const SNAPSHOT_NOW: u32 = 1;
}

/// Boot-phase markers written to the control page.
pub mod marker {
    /// PID 1 is executing.
    pub const INIT_STARTED: u32 = 1;
    /// The guest resumed from a snapshot.
    pub const RESUMED: u32 = 2;
}
