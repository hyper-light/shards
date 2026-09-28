//! The shards virtual machine monitor.

pub mod arch;
pub mod devices;
pub mod fdt;
pub mod initramfs;
pub mod log;
pub mod memory;
pub mod sync;

#[cfg(target_os = "macos")]
pub mod hvf;
#[cfg(target_os = "macos")]
mod thread;
#[cfg(target_os = "macos")]
pub mod vm;
