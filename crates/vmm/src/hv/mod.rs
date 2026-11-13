//! Hypervisor backends. build.rs compiles exactly one per target, as `cfg(hv = "...")`
//! (plus `cfg(hv)` whenever there is one):
//!
//! | `hv` | Host |
//! |---|---|
//! | `hvf` | macOS on arm64: Hypervisor.framework |
//! | `kvm` | Linux on x86_64 and arm64: KVM |
//! | `whp` | Windows on x86_64 and arm64: the Windows Hypervisor Platform |
//!
//! Every other target in the platform matrix (docs/design/architecture.md D13) builds
//! without a backend until its backend lands, and reports that VMs cannot run there.
//!
//! Every backend presents KVM's semantics (docs/research/hvf-arm64-kvm-ground-truth.md §5).
//! Device accesses complete inside `Vcpu::run` through [`Io`]. Firmware power management
//! (PSCI on arm64) stays inside the backend, and a powered-off vCPU parks inside `run`. The
//! caller sees only an [`Exit`]. Each backend exports the same names: `Vm`, `Vcpu`, `Kicker`,
//! `GuestClock` (what a vCPU has run of its guest, read from any thread), `Stage2`, `Error`,
//! `check_host`, `max_vcpus`, plus the architecture's interrupt-controller types.

#[cfg(hv = "hvf")]
pub mod hvf;
#[cfg(hv = "hvf")]
pub use hvf::*;

#[cfg(hv = "kvm")]
pub mod kvm;
#[cfg(hv = "kvm")]
pub use kvm::*;

#[cfg(all(hv = "whp", target_arch = "x86_64"))]
pub mod whp;
/// arm64's: its own partition, GIC and exits, beside x86_64's.
#[cfg(all(hv = "whp", target_arch = "aarch64"))]
#[path = "whp_arm64/mod.rs"]
pub mod whp;
#[cfg(hv = "whp")]
pub use whp::*;

/// Whether a Windows host's hypervisor assigns a host's PCI function, as both WHP backends
/// ask it (D154).
#[cfg(any(hv = "whp", test))]
pub mod assignment;

/// The backend this build drives VMs with, if any.
pub const BACKEND: Option<&str> = if cfg!(hv = "hvf") {
    Some("hvf")
} else if cfg!(hv = "kvm") {
    Some("kvm")
} else if cfg!(hv = "whp") {
    Some("whp")
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
/// any request is known, under a budget too, whose later reads ran 1.42 to 1.48 times their
/// time with no budget at p50 without them and 1.20 to 1.22 with (D188, PM M333), so the
/// pool's VMs times this bounds what a fleet speculates. 64
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

/// Notes in `out` the pages a dirty bitmap marks (D176 part 7), as KVM_GET_DIRTY_LOG and
/// WHvQueryGpaRangeDirtyBitmap give one for `page`-byte pages from `gpa`: bit `i`, word
/// `i / 64`'s bit `i % 64`, the page at `gpa + i * page`, noted as each
/// [`live::PAGE`](crate::vm::live::PAGE) it holds, the unit a live move sends, whatever the
/// host's page (KVM's bits are the host kernel's pages, 16 KiB on some arm64 hosts).
#[cfg(any(hv = "kvm", hv = "whp", test))]
pub(crate) fn note_dirty(bitmap: &[u64], gpa: u64, page: u64, out: &mut std::collections::BTreeSet<u64>) {
    let unit = crate::vm::live::PAGE;
    let per = (page / unit).max(1);
    for (w, &word) in (0u64..).zip(bitmap) {
        let mut bits = word;
        while bits != 0 {
            let index = w * 64 + u64::from(bits.trailing_zeros());
            bits &= bits - 1;
            let at = gpa.saturating_add(index.saturating_mul(page));
            out.extend((0..per).map(|k| at.saturating_add(k * unit)));
        }
    }
}

/// Every [`live::PAGE`](crate::vm::live::PAGE) of `len` bytes at `gpa`: what a range whose
/// dirty bitmap could not be read counts as written.
#[cfg(any(hv = "kvm", hv = "whp", test))]
pub(crate) fn every_page(gpa: u64, len: u64) -> impl Iterator<Item = u64> {
    let unit = crate::vm::live::PAGE;
    (0..len.div_ceil(unit)).map(move |p| gpa.saturating_add(p * unit))
}

/// A WHP virtual processor's guest time in nanoseconds, of its runtime counters
/// (WHV_PROCESSOR_RUNTIME_COUNTERS, WinHvPlatformDefs.h, Windows SDK 10.0.26100, one struct
/// for x64 and Arm64): its run time, "a combination of the time the virtual processor
/// consumes running guest code, and the time the associated logical processor spends
/// running hypervisor code on behalf of that guest" (Hyper-V TLFS, "Virtual Processor Run
/// Time Register"), less the hypervisor's, each in 100 ns units.
#[cfg(any(hv = "whp", test))]
pub(crate) fn whp_guest_ns(total_100ns: u64, hypervisor_100ns: u64) -> u64 {
    total_100ns.saturating_sub(hypervisor_100ns).saturating_mul(100)
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
    /// A guest access faulted on memory the host could not give, at `gpa` where the
    /// hypervisor says: true where the VMM has given memory there now, and the access runs
    /// again (a share's DAX window's range past its file's end, the file cut short on the
    /// host, D189); false where the fault is the guest's own.
    fn memory_fault(&self, _gpa: Option<u64>) -> bool {
        false
    }
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

    /// A dirty bitmap's bits are its pages, by word and bit, from its range's start: each
    /// noted once as the 4 KiB units it holds, a 16 KiB host page as four, and pages
    /// noted before kept.
    #[test]
    fn a_dirty_bitmaps_bits_are_its_ranges_pages() {
        let mut out = std::collections::BTreeSet::from([0x10]);
        // Pages 0, 5 and 63 of the first word, 64 and 127 of the second.
        let bitmap = [1 | 1 << 5 | 1 << 63, 1 | 1 << 63];
        note_dirty(&bitmap, 0x4000_0000, 4096, &mut out);
        let pages: Vec<u64> = out.iter().copied().collect();
        let marked = [0u64, 5, 63, 64, 127].map(|p| 0x4000_0000 + p * 4096);
        assert_eq!(pages, [&[0x10u64][..], &marked].concat());
        let mut out = std::collections::BTreeSet::new();
        note_dirty(&[0b10], 0x8000, 16 << 10, &mut out);
        let pages: Vec<u64> = out.into_iter().collect();
        assert_eq!(pages, [0xc000, 0xd000, 0xe000, 0xf000]);
        let mut out = std::collections::BTreeSet::new();
        note_dirty(&[0, 0], 0, 4096, &mut out);
        assert!(out.is_empty(), "no bit, no page");
        let all: Vec<u64> = every_page(0x1000, 0x3000).collect();
        assert_eq!(all, [0x1000, 0x2000, 0x3000], "a range unread counts whole");
    }

    /// A WHP virtual processor's guest time is its run time less the hypervisor's, from
    /// 100 ns units; never below none, and at most u64's.
    #[test]
    fn a_whp_vcpus_guest_time_is_its_run_time_less_the_hypervisors() {
        assert_eq!(whp_guest_ns(12_345, 345), 1_200_000);
        assert_eq!(whp_guest_ns(0, 0), 0);
        assert_eq!(whp_guest_ns(7, 9), 0, "the hypervisor's past the whole");
        assert_eq!(whp_guest_ns(u64::MAX, 0), u64::MAX);
    }
}
