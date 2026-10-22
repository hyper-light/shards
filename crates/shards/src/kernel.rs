//! shards' guest kernel, pinned: Linux 6.18.48 with Firecracker's microVM configuration and
//! shards' (resources/kernel), built reproducibly by CI and published as a release. Runs
//! boot it unless told otherwise (docs/design/architecture.md D28). The tests pin theirs
//! here too (tests/common/mod.rs).

/// A kernel release asset, and what it must be.
#[derive(Debug, Clone, Copy)]
pub struct Pinned {
    /// Its name in reports and caches: the asset's, then the release's input hash.
    pub name: &'static str,
    pub url: &'static str,
    pub size: u64,
    pub sha256: &'static str,
}

/// The kernel for guests of this host's architecture, which is theirs: on a Windows host,
/// its `hyperv` variant, which keeps time by the clocks a WHP partition offers
/// (resources/kernel/hyperv.config, D140).
pub const KERNEL: Option<Pinned> = if cfg!(all(target_arch = "x86_64", windows)) {
    Some(Pinned {
        name: "vmlinux-6.18.48-x86_64-hyperv-8df0015cca35",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-8df0015cca35/vmlinux-6.18.48-x86_64-hyperv",
        size: 27_753_776,
        sha256: "5c5dcf8ba357473cc38c97eb4ecf71a44c691bb22d810ba21de78e74906e6397",
    })
} else if cfg!(target_arch = "x86_64") {
    Some(Pinned {
        name: "vmlinux-6.18.48-x86_64-8df0015cca35",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-8df0015cca35/vmlinux-6.18.48-x86_64",
        size: 27_724_448,
        sha256: "78faced873347085a5cbc4e803f03cab26b83dda0b626d20621fc5cb882a3354",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-8df0015cca35",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-8df0015cca35/Image-6.18.48-aarch64",
        size: 19_016_192,
        sha256: "60d4b287c22a16efa78957296708e8e167e72e3e8cf45d6cd239dea7ca0a89ab",
    })
} else {
    None
};

/// The kernel of a guest given a host's VFIO group (`--device /dev/vfio/N`, D154): the
/// `kvm` variant, VFIO and virtio-iommu built in (resources/kernel/kvm.config). Of the
/// pinned kernels' own release; none on Windows, whose hosts have no VFIO.
pub const KERNEL_KVM: Option<Pinned> = if cfg!(windows) {
    None
} else if cfg!(target_arch = "x86_64") {
    Some(Pinned {
        name: "vmlinux-6.18.48-x86_64-kvm-8df0015cca35",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-8df0015cca35/vmlinux-6.18.48-x86_64-kvm",
        size: 27_783_144,
        sha256: "8e629b6bde41b47a414d25440868edfe7c5f8b8e3a5318e9d38538175e848731",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-kvm-8df0015cca35",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-8df0015cca35/Image-6.18.48-aarch64-kvm",
        size: 20_167_168,
        sha256: "5ee52780b3bb94674e6e859f70e96d25695f225343d78238451eee42c5d35958",
    })
} else {
    None
};

/// The kernel of a guest given a GPU (D144): the `gpu` variant, DRM's virtio-gpu driver
/// built in (resources/kernel/gpu.config); arm64's alone, as Hypervisor.framework's guests
/// alone have a GPU yet. Of the pinned kernels' own release.
pub const KERNEL_GPU: Option<Pinned> = if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-gpu-8df0015cca35",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-8df0015cca35/Image-6.18.48-aarch64-gpu",
        size: 19_614_208,
        sha256: "ce62457443a034e2f3f7446db54aa7f9b8cf84a9d0775d07b6b84c085756711d",
    })
} else {
    None
};
