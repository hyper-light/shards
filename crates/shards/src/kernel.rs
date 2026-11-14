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
        name: "vmlinux-6.18.48-x86_64-hyperv-7f8c931141dd",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-7f8c931141dd/vmlinux-6.18.48-x86_64-hyperv",
        size: 27_789_288,
        sha256: "e40d77793e63c3f7466967420313a2009e071c771d6b4b78072622d5c1d06ec2",
    })
} else if cfg!(target_arch = "x86_64") {
    Some(Pinned {
        name: "vmlinux-6.18.48-x86_64-7f8c931141dd",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-7f8c931141dd/vmlinux-6.18.48-x86_64",
        size: 27_759_968,
        sha256: "a9e00491d3233cfab80cf1971fd5dd72f2ef3e5dc8eb52390e9521057bf1976a",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-7f8c931141dd",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-7f8c931141dd/Image-6.18.48-aarch64",
        size: 19_091_968,
        sha256: "52e369d56ff5515bdbe3c1bcb536874cb769f3f55a8139812188c6761d7e3cd7",
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
        name: "vmlinux-6.18.48-x86_64-kvm-7f8c931141dd",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-7f8c931141dd/vmlinux-6.18.48-x86_64-kvm",
        size: 27_818_664,
        sha256: "11492824539dcd5414b298fb91d90deb621d54a1a063f7b180c4c7ba0cd735a7",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-kvm-7f8c931141dd",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-7f8c931141dd/Image-6.18.48-aarch64-kvm",
        size: 20_244_992,
        sha256: "b53e264ed81d17eba6a6d19a7b8fd44b5dc64b919f3b1ea438d94cb1b5e443ed",
    })
} else {
    None
};

/// The kernel of a guest given a GPU (D144, D201): the `gpu` variant, DRM's virtio-gpu
/// driver built in (resources/kernel/gpu.config), and on a Windows host `hyperv+gpu`, which
/// keeps Hyper-V's time besides (D140). Of the pinned kernels' own release.
pub const KERNEL_GPU: Option<Pinned> = if cfg!(all(target_arch = "x86_64", windows)) {
    Some(Pinned {
        name: "vmlinux-6.18.48-x86_64-hyperv-gpu-7f8c931141dd",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-7f8c931141dd/vmlinux-6.18.48-x86_64-hyperv-gpu",
        size: 30_005_264,
        sha256: "6f14c68bf66fe7f6cffe5fc1502e867627ec9f5e707e68c44a858f4334c60ea2",
    })
} else if cfg!(target_arch = "x86_64") {
    Some(Pinned {
        name: "vmlinux-6.18.48-x86_64-gpu-7f8c931141dd",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-7f8c931141dd/vmlinux-6.18.48-x86_64-gpu",
        size: 29_984_144,
        sha256: "8f7be4e73e1a86191efcdd23df8e17989369b5fa25a5f4fc53ad27fbfef236fc",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-gpu-7f8c931141dd",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-7f8c931141dd/Image-6.18.48-aarch64-gpu",
        size: 19_692_032,
        sha256: "91a95a8c925ca8b3820c90edb947483e2e5573e1c3878e1256be37d68f02f7bf",
    })
} else {
    None
};
