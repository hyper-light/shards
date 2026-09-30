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

/// A guest page in a working set: what a run touched, in the order it first did, so that
/// later copies can have it in place before they run (REAP, Ustiugov et al., ASPLOS 2021;
/// platform-measurements M30).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Touch {
    /// The page's guest-physical address, aligned to the backend's stage-2 page.
    pub gpa: u64,
    /// Whether the guest wrote it.
    pub written: bool,
}

/// The private bytes a restore may spend, ahead of its request, copying the pages its
/// working set says the guest wrote (audit D02, PM M62). Every warm VM spends them before
/// any request is known, so the pool's VMs times this bounds what a fleet speculates. 64
/// MiB is above the largest working set measured, 3,900 pages of 16 KiB (M33), were every
/// page of it written. Past it, written pages are prefetched as reads: the guest's first
/// write copies each, as it would without a working set.
pub const PREFETCH_PRIVATE: u64 = 64 << 20;

/// Keeps the written mark of `touches`, pages of `page` bytes, in their first-touch order,
/// until `budget` bytes of them are marked, and clears it past that. Returns how many
/// marks were cleared.
pub fn budget_writes(touches: &mut [Touch], page: u64, budget: u64) -> usize {
    let mut left = budget / page.max(1);
    let mut cleared = 0;
    for t in touches.iter_mut().filter(|t| t.written) {
        if left == 0 {
            t.written = false;
            cleared += 1;
        } else {
            left -= 1;
        }
    }
    cleared
}

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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Written marks are kept in first-touch order up to the budget, whole pages of it,
    /// and cleared past it; reads are left alone.
    #[test]
    fn a_working_sets_writes_are_kept_within_the_budget() {
        let page = 16 << 10;
        let mut touches: Vec<Touch> = (0..10)
            .map(|i| Touch {
                gpa: i * page,
                written: i % 2 == 0,
            })
            .collect();
        // Room for two written pages and part of a third.
        assert_eq!(budget_writes(&mut touches, page, 2 * page + 1), 3);
        let written: Vec<u64> = touches
            .iter()
            .filter(|t| t.written)
            .map(|t| t.gpa / page)
            .collect();
        assert_eq!(written, [0, 2]);
        assert_eq!(touches.len(), 10, "every page is still prefetched");
        // A budget that covers them all clears none.
        let mut all = vec![
            Touch {
                gpa: 0,
                written: true
            };
            4
        ];
        assert_eq!(budget_writes(&mut all, page, PREFETCH_PRIVATE), 0);
    }
}
