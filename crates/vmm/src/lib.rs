//! The shards virtual machine monitor.

// Guest RAM, file offsets and device windows are 64-bit quantities throughout, and every
// target in the platform matrix (docs/design/architecture.md D13) is 64-bit.
#[cfg(not(target_pointer_width = "64"))]
compile_error!("shards-vmm requires a 64-bit host");

pub mod arch;
pub mod devices;
pub mod fdt;
pub mod hv;
pub mod initramfs;
pub mod log;
pub mod memory;
pub mod platform;
pub mod snapshot;
pub mod sync;
pub mod vm;

/// Diagnostic (branch kvm-ws-diag): appends `line` to the file SHARDS_KVM_STATS names,
/// with this process's id and uptime, whatever its stderr has become.
pub fn diag(line: &str) {
    use std::io::Write;
    if let Some(path) = std::env::var_os("SHARDS_KVM_STATS")
        && let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path)
    {
        let _ = writeln!(f, "{} {}us {line}", std::process::id(), crate::log::uptime_us());
    }
}

/// Diagnostic (branch kvm-ws-diag): KVM's counters now, labeled, where the backend is KVM.
pub fn diag_phase(label: &str) {
    #[cfg(hv = "kvm")]
    hv::kvm::diag_phase(label);
    #[cfg(not(hv = "kvm"))]
    let _ = label;
}
