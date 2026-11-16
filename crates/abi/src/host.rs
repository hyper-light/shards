//! Whether this host runs microVMs, typed (docs/design/architecture.md D262): the hypervisor
//! backend a build of shards drives VMs with, and why one cannot run any here. The VMM's own
//! checks (`shards_vmm::vm::check_host`) say it; `shards api-check` tells it to
//! `shards_api::host_check`; a caller matches the variants, and its text is for people.

use alloc::string::String;
use core::fmt;

/// What boots guests on a host: the hypervisor backend each build of shards has for its OS
/// and architecture (`cfg(hv)`), `shards_api::Backend` as its callers name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Backend {
    /// Linux's KVM.
    Kvm,
    /// macOS's Hypervisor.framework.
    HypervisorFramework,
    /// The Windows Hypervisor Platform.
    WindowsHypervisorPlatform,
}

impl Backend {
    /// Its name, as `shards_vmm::hv::BACKEND` gives it.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Kvm => "kvm",
            Self::HypervisorFramework => "hvf",
            Self::WindowsHypervisorPlatform => "whp",
        }
    }

    /// The backend `name` names.
    pub fn named(name: &str) -> Option<Backend> {
        [
            Self::Kvm,
            Self::HypervisorFramework,
            Self::WindowsHypervisorPlatform,
        ]
        .into_iter()
        .find(|b| b.name() == name)
    }
}

/// Why this host runs no microVM now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unusable {
    /// shards has no backend for this OS and architecture (an Intel Mac): `os`/`arch` as
    /// Rust names them (`std::env::consts`).
    Unsupported { os: String, arch: String },
    /// KVM: `/dev/kvm` does not exist: no hardware virtualization, or its module not loaded.
    KvmMissing,
    /// KVM: `/dev/kvm` is there, and this user may not open it.
    KvmDenied,
    /// Hypervisor.framework: `kern.hv_support` is not 1 on a Mac that is no VM.
    HvSupportOff,
    /// The Windows Hypervisor Platform's optional feature is off: its library is not there.
    WhpOff,
    /// Windows: no hypervisor runs (virtualization off in its firmware, or Windows started
    /// without Hyper-V).
    NoHypervisor,
    /// This host is itself a VM whose hypervisor gives it no nested virtualization: on macOS
    /// `kern.hv_vmm_present` is 1 where `kern.hv_support` is not; on x86-64 the CPU says a
    /// hypervisor runs it (CPUID.1:ECX bit 31) where KVM or WHP has none of its own.
    NoNested,
    /// The backend is there and lacks what shards needs of it, in its own words.
    Lacks(String),
    /// Asking the backend failed: the call, and the OS's words.
    Unasked(String),
}

impl fmt::Display for Unusable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported { os, arch } => write!(
                f,
                "shards cannot run VMs on {os}/{arch}: no hypervisor backend for this host"
            ),
            Self::KvmMissing => f.write_str(
                "KVM is unavailable on this host: /dev/kvm does not exist (no hardware virtualization, or the kvm module is not loaded)",
            ),
            Self::KvmDenied => {
                f.write_str("cannot open /dev/kvm: permission denied (add this user to the `kvm` group)")
            }
            Self::HvSupportOff => {
                f.write_str("Hypervisor.framework is unavailable on this host (sysctl kern.hv_support is not 1)")
            }
            Self::WhpOff => f.write_str(
                "the Windows Hypervisor Platform is not on this host: turn on the optional feature \
                 \"Windows Hypervisor Platform\" (HypervisorPlatform) and restart",
            ),
            Self::NoHypervisor => f.write_str(
                "no hypervisor runs on this host: hardware virtualization is off in its firmware, or Windows \
                 started without Hyper-V",
            ),
            Self::NoNested => f.write_str(
                "this host is a VM whose hypervisor gives it no nested virtualization: turn nested \
                 virtualization on for it",
            ),
            Self::Lacks(what) | Self::Unasked(what) => f.write_str(what),
        }
    }
}

/// Its words, where a caller says why a start failed as text.
impl From<Unusable> for String {
    fn from(why: Unusable) -> String {
        alloc::format!("{why}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_backend_is_named_as_hv_names_it() {
        for b in [
            Backend::Kvm,
            Backend::HypervisorFramework,
            Backend::WindowsHypervisorPlatform,
        ] {
            assert_eq!(Backend::named(b.name()), Some(b));
        }
        assert_eq!(Backend::named("KVM"), None);
    }
}
