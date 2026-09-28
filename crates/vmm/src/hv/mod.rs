//! Hypervisor backends. build.rs compiles exactly one per target, as `cfg(hv = "...")`
//! (plus `cfg(hv)` whenever there is one):
//!
//! | `hv` | Host |
//! |---|---|
//! | `hvf` | macOS on arm64: Hypervisor.framework |
//! | `kvm` | Linux on x86_64: KVM |
//!
//! Every other target in the platform matrix (docs/design/architecture.md D13) builds
//! without a backend until its backend lands, and reports that VMs cannot run there.
//!
//! Every backend presents KVM's semantics (docs/research/hvf-arm64-kvm-ground-truth.md §5).
//! Device accesses complete inside `Vcpu::run` through [`Io`]. Firmware power management
//! (PSCI on arm64) stays inside the backend, and a powered-off vCPU parks inside `run`. The
//! caller sees only an [`Exit`]. Each backend exports the same names: `Vm`, `Vcpu`, `Kicker`,
//! `Error`, `check_host`, `max_vcpus`, plus the architecture's interrupt-controller types.

#[cfg(hv = "hvf")]
pub mod hvf;
#[cfg(hv = "hvf")]
pub use hvf::*;

#[cfg(hv = "kvm")]
pub mod kvm;
#[cfg(hv = "kvm")]
pub use kvm::*;

/// The backend this build drives VMs with, if any.
pub const BACKEND: Option<&str> = if cfg!(hv = "hvf") {
    Some("hvf")
} else if cfg!(hv = "kvm") {
    Some("kvm")
} else {
    None
};

/// The guest's device accesses, as the VMM's buses serve them. Backends call these from
/// vCPU threads, concurrently.
pub trait Io: Sync {
    /// Fills `data` (1, 2, 4 or 8 bytes, little-endian) from guest-physical `addr`.
    fn mmio_read(&self, addr: u64, data: &mut [u8]);
    fn mmio_write(&self, addr: u64, data: &[u8]);
    /// x86 port I/O (IN): fills `data` (1, 2 or 4 bytes). Other architectures have none.
    fn pio_read(&self, _port: u16, data: &mut [u8]) {
        data.fill(0);
    }
    /// x86 port I/O (OUT).
    fn pio_write(&self, _port: u16, _data: &[u8]) {}
}

/// Why `Vcpu::run` returned to the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// A `Kicker` interrupted the vCPU (or a kick raced with its last exit).
    Canceled,
    /// The guest powered the machine off.
    Shutdown,
    /// The guest asked for a reset.
    Reset,
}
