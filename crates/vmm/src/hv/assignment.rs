//! Whether a Windows host's hypervisor gives a partition a host's PCI function (D154), as it
//! answers when asked, typed: what it refused, and whose limit that refusal is, by the
//! edition of Windows it runs. Both WHP backends ask it their own way
//! (`device_assignment_refused`); their answer and its words are written once, here.

use std::fmt;

/// winnt.h's product types, OSVERSIONINFOEXW's wProductType.
const VER_NT_WORKSTATION: u8 = 1;
const VER_NT_DOMAIN_CONTROLLER: u8 = 2;
const VER_NT_SERVER: u8 = 3;

/// A Windows edition, as its kernel says it (RtlGetVersion's wProductType).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edition {
    /// VER_NT_WORKSTATION: a client edition, Windows 10 and 11 among them (PM M401's Windows
    /// 11 Pro, M402's Windows 11 Enterprise).
    Client,
    /// VER_NT_SERVER or VER_NT_DOMAIN_CONTROLLER: Windows Server (M402's 2022 and 2025).
    Server,
    /// A product type winnt.h names none of.
    Other(u8),
}

impl Edition {
    pub fn of(product_type: u8) -> Edition {
        match product_type {
            VER_NT_WORKSTATION => Edition::Client,
            VER_NT_SERVER | VER_NT_DOMAIN_CONTROLLER => Edition::Server,
            other => Edition::Other(other),
        }
    }
}

/// Why this host's hypervisor gives no partition a host's PCI function, as it answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// The Windows Hypervisor Platform is not on this host.
    NoPlatform,
    /// It could not be asked, in its words: its capabilities, or a partition of its own,
    /// refused, as where no hypervisor runs (windows-11-arm's WHvCreatePartition, 0xC0351000,
    /// PM M402).
    Unasked(String),
    /// It makes no virtual PCI device of a host's function (WHvCapabilityCodeFeatures:
    /// VirtualPciDeviceSupport 0).
    NoVirtualPci,
    /// It allows no partition device assignment (WHvPartitionPropertyCodeAllowDeviceAssignment),
    /// in its words (0x80070032 on every Windows host here, M401, M402); whether it says it has
    /// IOMMU support; and the edition it runs, where its kernel said, which is whose limit
    /// that is. Microsoft supports Discrete Device Assignment on Windows Server alone: "DDA
    /// and GPU-P aren't supported on desktop-class hardware or client operating systems such
    /// as Windows 10 or 11 Pro" (Microsoft Learn, "Troubleshooting guidance for Hyper-V GPU
    /// assignment, partitioning, and passthrough", 2026-02-12), its host "running Windows
    /// Server 2016 or later" ("Deploy graphics devices by using Discrete Device Assignment",
    /// 2026-01-30). On Windows Server it "also requires server class hardware that's capable
    /// of granting the operating system control over configuring the PCIe fabric (Native PCI
    /// Express Control)", and a PCIe root complex with Access Control Services, "which enables
    /// Hyper-V to force all PCIe traffic through the Input-Output Memory Management Unit"
    /// ("Deploy devices by using Discrete Device Assignment", 2025-02-19).
    NotAllowed {
        refused: String,
        iommu: bool,
        edition: Option<Edition>,
    },
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::NoPlatform => f.write_str("the Windows Hypervisor Platform is not on this host"),
            Refused::Unasked(said) => write!(f, "asking the Windows Hypervisor Platform: {said}"),
            Refused::NoVirtualPci => f.write_str(
                "its Windows Hypervisor Platform makes no virtual PCI device of a host's (WHvCapabilityCodeFeatures: VirtualPciDeviceSupport 0)",
            ),
            Refused::NotAllowed {
                refused,
                iommu,
                edition,
            } => {
                write!(f, "its hypervisor assigns no device to a partition: {refused}")?;
                if !iommu {
                    f.write_str(", and says it has no IOMMU support (WHvCapabilityCodeFeatures: IommuSupport 0)")?;
                }
                match edition {
                    Some(Edition::Client) => f.write_str(
                        "; this Windows is a client edition, and Microsoft supports Discrete Device Assignment on Windows Server alone, not on client editions such as Windows 10 or 11 Pro (D154)",
                    ),
                    Some(Edition::Server) => f.write_str(
                        "; Windows Server assigns a host's function on server-class hardware alone, which grants it control of the PCIe fabric (Native PCI Express Control) and whose PCIe root complex has Access Control Services, forcing the function's traffic through the IOMMU (D154)",
                    ),
                    Some(Edition::Other(product)) => write!(
                        f,
                        "; this Windows' product type, {product}, is neither a client's nor a server's, and Microsoft supports Discrete Device Assignment on Windows Server on server-class hardware alone (D154)"
                    ),
                    None => f.write_str(
                        "; Microsoft supports Discrete Device Assignment on Windows Server on server-class hardware alone (D154)",
                    ),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The product types of winnt.h, as M401's and M402's probes read them (ProductType 1 on
    /// Windows 11 Pro and Enterprise, 3 on Windows Server 2022 and 2025 Datacenter).
    #[test]
    fn a_product_type_is_its_edition() {
        assert_eq!(Edition::of(1), Edition::Client);
        assert_eq!(Edition::of(2), Edition::Server);
        assert_eq!(Edition::of(3), Edition::Server);
        assert_eq!(Edition::of(0), Edition::Other(0));
        assert_eq!(Edition::of(4), Edition::Other(4));
        assert_eq!(Edition::of(u8::MAX), Edition::Other(u8::MAX));
    }

    /// windows-sys's are winnt.h's.
    #[cfg(windows)]
    #[test]
    fn its_product_types_are_windows_own() {
        use windows_sys::Win32::System::SystemServices as s;
        assert_eq!(u32::from(VER_NT_WORKSTATION), s::VER_NT_WORKSTATION);
        assert_eq!(u32::from(VER_NT_DOMAIN_CONTROLLER), s::VER_NT_DOMAIN_CONTROLLER);
        assert_eq!(u32::from(VER_NT_SERVER), s::VER_NT_SERVER);
    }

    fn refused(edition: Option<Edition>, iommu: bool) -> String {
        Refused::NotAllowed {
            refused: "WHvSetPartitionProperty(AllowDeviceAssignment) failed: HRESULT 0x80070032".into(),
            iommu,
            edition,
        }
        .to_string()
    }

    /// A refusal names its edition's limit and no other: a client edition's, that DDA is
    /// Windows Server's; a server's, the hardware DDA needs there (D154); the hypervisor's own
    /// words first, and its IOMMU where it says it has none.
    #[test]
    fn a_refusal_names_its_editions_limit() {
        let client = refused(Some(Edition::Client), false);
        assert!(
            client.starts_with(
                "its hypervisor assigns no device to a partition: WHvSetPartitionProperty(AllowDeviceAssignment) failed: HRESULT 0x80070032, and says it has no IOMMU support (WHvCapabilityCodeFeatures: IommuSupport 0); "
            ),
            "{client}"
        );
        assert!(
            client.contains("client edition") && client.contains("Windows Server alone"),
            "{client}"
        );
        assert!(!client.contains("Access Control Services"), "{client}");

        let server = refused(Some(Edition::Server), true);
        assert!(!server.contains("IommuSupport"), "{server}");
        assert!(
            server.contains("Native PCI Express Control") && server.contains("Access Control Services"),
            "{server}"
        );
        assert!(!server.contains("client edition"), "{server}");

        let other = refused(Some(Edition::Other(9)), true);
        assert!(
            other.contains("product type, 9,") && other.contains("Windows Server"),
            "{other}"
        );
        let unread = refused(None, true);
        assert!(
            unread.contains("Windows Server") && !unread.contains("client edition"),
            "{unread}"
        );
    }

    /// What it says where it was never asked: its platform missing, a refusal of its own, or
    /// no virtual PCI device.
    #[test]
    fn a_refusal_before_the_asking_says_what_stopped_it() {
        assert_eq!(
            Refused::NoPlatform.to_string(),
            "the Windows Hypervisor Platform is not on this host"
        );
        assert_eq!(
            Refused::Unasked("WHvCreatePartition failed: HRESULT 0xc0351000".into()).to_string(),
            "asking the Windows Hypervisor Platform: WHvCreatePartition failed: HRESULT 0xc0351000"
        );
        assert!(
            Refused::NoVirtualPci
                .to_string()
                .contains("VirtualPciDeviceSupport 0")
        );
    }
}
