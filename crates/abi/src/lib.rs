//! The contract between the shards VMM and software inside the guest. Anything both
//! sides must agree on lives here, so the two can never drift apart.

#![no_std]

extern crate alloc;

/// The first agent's or harness's uid and gid in a microVM (D59): the n-th domain, agents
/// first in the order the Agentfile declares them and then harnesses, runs as this plus
/// n, and the build checks that no file outside a domain is its user's (§9.2).
pub const DOMAIN_FIRST_ID: u32 = 200_000;

/// The first in-VM server instance's uid and gid (D60): the n-th domain's instance runs as
/// this plus n. Past every domain's: each domain has a process, so there are no more
/// domains than a 64-bit kernel's PIDs, `PID_MAX_LIMIT` (include/linux/threads.h,
/// 4 * 1024 * 1024).
pub const SERVER_FIRST_ID: u32 = DOMAIN_FIRST_ID + 4 * 1024 * 1024;

/// The join disk's virtio-blk serial (D119), which init finds it by: the read-only disk
/// whose ranges are the images of containers joining the microVM's network.
pub const JOIN_DISK_SERIAL: &str = "shards-join";

pub mod build;
pub mod changes;
pub mod run;

include!(concat!(env!("OUT_DIR"), "/identity.rs"));

/// Guest-physical address of the control page ([`control`]). Guests share the host's
/// architecture, so the VMM and the guest build of this crate always agree. On aarch64,
/// the 64 KiB slot after the RTC's.
#[cfg(target_arch = "aarch64")]
pub const CONTROL_PAGE: u64 = 0x0902_0000;
/// Guest-physical address of the control page ([`control`]). Guests share the host's
/// architecture, so the VMM and the guest build of this crate always agree. On x86_64,
/// the first page of the 32-bit MMIO gap, where Firecracker's boot timer is
/// (docs/research/kvm-x86_64-ground-truth.md §6.4).
#[cfg(target_arch = "x86_64")]
pub const CONTROL_PAGE: u64 = 0xc000_0000;

/// Registers of the control page, little-endian: 32 bits, or 64 where one says so.
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
    /// Read, 64 bits in one access: the host's wall-clock time when read, in nanoseconds
    /// since the Unix epoch. A booted guest's clock comes from the RTC in whole seconds, and
    /// a restored guest's is its snapshot's; shards-init sets CLOCK_REALTIME from this.
    pub const HOST_TIME: u64 = 0x10;
    /// Write, 64 bits in one access: the [`IDENTITY`](super::IDENTITY) of the contract the
    /// guest's init was built with. shards-init writes it as it starts, a snapshot keeps it,
    /// and the host hands a workload only to a guest whose init wrote the host's own.
    pub const ABI: u64 = 0x18;

    pub const SNAPSHOT_NOW: u32 = 1;
}

/// Boot-phase markers written to the control page.
pub mod marker {
    /// PID 1 is executing.
    pub const INIT_STARTED: u32 = 1;
    /// The guest resumed from a snapshot.
    pub const RESUMED: u32 = 2;
    /// shards-init connected to the host for its workload.
    pub const CONNECTED: u32 = 3;
    /// The workload's command is executing.
    pub const WORKLOAD_STARTED: u32 = 4;
    /// The workload's main process exited.
    pub const WORKLOAD_EXITED: u32 = 5;
    /// shards-init is powering the VM off.
    pub const POWERING_OFF: u32 = 6;
}
