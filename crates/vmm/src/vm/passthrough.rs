//! Why a host off Linux passes no PCI function, or this one, through to a microVM (D154):
//! its OS's own limit, typed, each with its evidence. Linux passes one through by VFIO.

use std::fmt;

/// Why this host passes no PCI function, or this one, through to a microVM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PassthroughRefusal {
    /// macOS: Hypervisor.framework assigns no device to a VM. "Hypervisor maps the physical
    /// memory in the guest to virtual memory of the host process", and has the VMM "Emulate
    /// memory-mapped hardware by emulating the memory access on exit"
    /// (developer.apple.com/documentation/hypervisor); none of the 162 functions of its
    /// macOS 26.4 SDK headers takes a device (PM M403); Virtualization.framework's devices
    /// are ones it emulates.
    #[cfg(target_os = "macos")]
    HypervisorFramework,
    /// Windows, whose hypervisor assigns no device: its own answer (PM M401, M402).
    #[cfg(hv = "whp")]
    Hypervisor(crate::hv::assignment::Refused),
    /// Windows, whose hypervisor assigns devices, of a function behind the guest's IOMMU (a
    /// workload's, at its start or plugged in): WHP remaps an assigned function's DMA to all
    /// of the partition's memory as it makes the device (WHvCreateVpciDevice's remarks) and
    /// gives the guest no I/O address space of its own, which a guest's VFIO needs to give a
    /// process the function. The VM's kernel is given it instead.
    #[cfg(hv = "whp")]
    NoGuestIommu,
    /// Windows, whose hypervisor assigns devices, of a function plugged into a running
    /// microVM outside its IOMMU (D214). A partition is allowed device assignment before
    /// WHvSetupPartition alone (E_INVALIDARG after, M401); Microsoft documents Discrete
    /// Device Assignment for a VM given its device before it starts ("After a device is
    /// successfully mounted in a VM, you're now able to start that VM") and giving it back
    /// stopped ("you must stop the VM and issue this command"; Microsoft Learn, "Deploy
    /// graphics devices by using Discrete Device Assignment", 2026-01-30); and WHP's pages
    /// (WHvCreateVpciDevice and its virtual PCI functions', 2026-06-08) say nothing of a
    /// device made in a partition that runs.
    #[cfg(hv = "whp")]
    NoPlug,
    /// A host with no hypervisor backend: why it runs no VM.
    #[cfg(not(any(target_os = "macos", hv = "whp")))]
    NoBackend(String),
}

impl fmt::Display for PassthroughRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const NONE: &str = "this host passes no PCI function through to a microVM";
        match self {
            #[cfg(target_os = "macos")]
            PassthroughRefusal::HypervisorFramework => write!(
                f,
                "{NONE}: macOS's Hypervisor.framework assigns no device to a VM, whose every device its VMM emulates (D154)"
            ),
            #[cfg(hv = "whp")]
            PassthroughRefusal::Hypervisor(refused) => write!(f, "{NONE}: {refused}"),
            #[cfg(hv = "whp")]
            PassthroughRefusal::NoGuestIommu => f.write_str(
                "this host passes a PCI function through to a microVM's kernel alone: its hypervisor remaps an assigned function's DMA to all of its VM's memory as it assigns it (WHvCreateVpciDevice), with no I/O address space of the guest's own, which a microVM's own VFIO needs to give a workload the function: give it to the VM's kernel instead (`shards run vm --kernel ... --vfio PCIP\\...`; D154)",
            ),
            #[cfg(hv = "whp")]
            PassthroughRefusal::NoPlug => f.write_str(
                "Windows gives a microVM a host's function only as it starts: its hypervisor allows a partition device assignment before it is set up alone, and Microsoft documents Discrete Device Assignment for a VM given its device before it starts and giving it back once stopped, its Windows Hypervisor Platform's pages saying nothing of a virtual PCI device made in a partition that runs (WHvCreateVpciDevice): give the function as the microVM starts (`shards run vm --kernel ... --vfio PCIP\\...`; D154, D214)",
            ),
            #[cfg(not(any(target_os = "macos", hv = "whp")))]
            PassthroughRefusal::NoBackend(why) => write!(f, "{NONE}: {why}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each refusal names its host's limit, and says what a host that assigns devices gives
    /// instead of what it refuses.
    #[test]
    fn each_refusal_names_its_hosts_limit() {
        #[cfg(target_os = "macos")]
        {
            let said = PassthroughRefusal::HypervisorFramework.to_string();
            assert!(
                said.starts_with("this host passes no PCI function through to a microVM: ")
                    && said.contains("Hypervisor.framework assigns no device to a VM"),
                "{said}"
            );
        }
        #[cfg(hv = "whp")]
        {
            let refused = crate::hv::assignment::Refused::NoVirtualPci;
            assert_eq!(
                PassthroughRefusal::Hypervisor(refused.clone()).to_string(),
                format!("this host passes no PCI function through to a microVM: {refused}")
            );
            let iommu = PassthroughRefusal::NoGuestIommu.to_string();
            assert!(
                iommu.contains("kernel alone")
                    && iommu.contains("no I/O address space")
                    && iommu.contains("--vfio PCIP"),
                "{iommu}"
            );
            let plug = PassthroughRefusal::NoPlug.to_string();
            assert!(
                plug.contains("only as it starts") && plug.contains("--vfio PCIP"),
                "{plug}"
            );
        }
        #[cfg(not(any(target_os = "macos", hv = "whp")))]
        assert_eq!(
            PassthroughRefusal::NoBackend("no hypervisor backend for this host".into()).to_string(),
            "this host passes no PCI function through to a microVM: no hypervisor backend for this host"
        );
    }
}
