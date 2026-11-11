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

#[cfg(all(test, target_os = "macos", target_arch = "aarch64"))]
#[path = "../../../docs/research/measurements/managed-audit-wire-adversarial/native.rs"]
mod managed_audit_wire_adversarial;
