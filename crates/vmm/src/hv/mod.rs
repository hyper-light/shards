//! Hypervisor backends. build.rs compiles exactly one per target, as `cfg(hv = "...")`:
//!
//! | `hv` | Host |
//! |---|---|
//! | `hvf` | macOS on arm64: Hypervisor.framework |
//!
//! Every other target in the platform matrix (docs/design/architecture.md D13) builds
//! without a backend until its backend lands, and reports that VMs cannot run there.

#[cfg(hv = "hvf")]
pub mod hvf;

/// The backend this build drives VMs with, if any.
pub const BACKEND: Option<&str> = if cfg!(hv = "hvf") { Some("hvf") } else { None };
