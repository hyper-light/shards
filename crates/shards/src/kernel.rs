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
        name: "vmlinux-6.18.48-x86_64-hyperv-836568f8ad2c",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-836568f8ad2c/vmlinux-6.18.48-x86_64-hyperv",
        size: 27_789_288,
        sha256: "71386c7fa15acbb652f922c4508df565e6d5b20b59353b802eda2d8e33d3a895",
    })
} else if cfg!(target_arch = "x86_64") {
    Some(Pinned {
        name: "vmlinux-6.18.48-x86_64-836568f8ad2c",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-836568f8ad2c/vmlinux-6.18.48-x86_64",
        size: 27_759_968,
        sha256: "7a36f8858f3db357728540139c8b37ea70eb5d3274f0c8aaf10b2c9cc5ba9e46",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-836568f8ad2c",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-836568f8ad2c/Image-6.18.48-aarch64",
        size: 19_091_968,
        sha256: "c9dd57ba93276d9fc3842a6014b05111dec4ff42cfc0b23ea18b73438743e755",
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
        name: "vmlinux-6.18.48-x86_64-kvm-836568f8ad2c",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-836568f8ad2c/vmlinux-6.18.48-x86_64-kvm",
        size: 27_818_664,
        sha256: "759273d6df9d6b3800ab1ba8a792be8c3bf8f956d5a72d7ea850b831bc6ecf76",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-kvm-836568f8ad2c",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-836568f8ad2c/Image-6.18.48-aarch64-kvm",
        size: 20_244_992,
        sha256: "ff68fc5ede1ef64f9f8c9323c9429dcfbec065e579aa98a4028bb8a269b55d08",
    })
} else {
    None
};

/// The kernel of a guest given a GPU (D144, D201): the `gpu` variant, DRM's virtio-gpu
/// driver built in (resources/kernel/gpu.config), and on a Windows host `hyperv+gpu`, which
/// keeps Hyper-V's time besides (D140). Of the pinned kernels' own release.
pub const KERNEL_GPU: Option<Pinned> = if cfg!(all(target_arch = "x86_64", windows)) {
    Some(Pinned {
        name: "vmlinux-6.18.48-x86_64-hyperv-gpu-836568f8ad2c",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-836568f8ad2c/vmlinux-6.18.48-x86_64-hyperv-gpu",
        size: 30_005_264,
        sha256: "9e7edd84a217f84ff9991800489296386973f831c09fb37a4a1c6f6e647f0b87",
    })
} else if cfg!(target_arch = "x86_64") {
    Some(Pinned {
        name: "vmlinux-6.18.48-x86_64-gpu-836568f8ad2c",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-836568f8ad2c/vmlinux-6.18.48-x86_64-gpu",
        size: 29_984_144,
        sha256: "3c17fda89634d2e8f045593f3b2a8b6c3f11476c2c86bcf5e6987c8b7291c1aa",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-gpu-836568f8ad2c",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-836568f8ad2c/Image-6.18.48-aarch64-gpu",
        size: 19_692_032,
        sha256: "a0995c0d9051156f9c18d524516559cc6b023f64158d671322f9003638f931be",
    })
} else {
    None
};
