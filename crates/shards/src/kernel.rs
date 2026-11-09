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
        name: "vmlinux-6.18.48-x86_64-hyperv-f7cd54dd57a5",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-f7cd54dd57a5/vmlinux-6.18.48-x86_64-hyperv",
        size: 27_773_048,
        sha256: "ec00fb01d2e03517a4f81bb422e4675e652ea6ce8ed3fd35288a62694574f315",
    })
} else if cfg!(target_arch = "x86_64") {
    Some(Pinned {
        name: "vmlinux-6.18.48-x86_64-f7cd54dd57a5",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-f7cd54dd57a5/vmlinux-6.18.48-x86_64",
        size: 27_747_824,
        sha256: "cd841a353b550e369e64e9629365ac66a65c8b82a2c13dba83e1595ec9c5d68f",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-f7cd54dd57a5",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-f7cd54dd57a5/Image-6.18.48-aarch64",
        size: 19_087_872,
        sha256: "26287242cefcbc24d45ab9b61eb14a04379367da90234a64ba7181fb147529dc",
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
        name: "vmlinux-6.18.48-x86_64-kvm-f7cd54dd57a5",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-f7cd54dd57a5/vmlinux-6.18.48-x86_64-kvm",
        size: 27_802_424,
        sha256: "85c6d297eae6f8fbacb2e00d31456bda5f4c24a2642a93d764daaf30e8a8c5ec",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-kvm-f7cd54dd57a5",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-f7cd54dd57a5/Image-6.18.48-aarch64-kvm",
        size: 20_175_360,
        sha256: "9a08ae6a8897195206f82346a58bf233b8ba5a2ab3f6a5ab53e2592cd47d9a7e",
    })
} else {
    None
};

/// The kernel of a guest given a GPU (D144, D201): the `gpu` variant, DRM's virtio-gpu
/// driver built in (resources/kernel/gpu.config), and on a Windows host `hyperv+gpu`, which
/// keeps Hyper-V's time besides (D140). Of the pinned kernels' own release.
pub const KERNEL_GPU: Option<Pinned> = if cfg!(all(target_arch = "x86_64", windows)) {
    Some(Pinned {
        name: "vmlinux-6.18.48-x86_64-hyperv-gpu-f7cd54dd57a5",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-f7cd54dd57a5/vmlinux-6.18.48-x86_64-hyperv-gpu",
        size: 29_997_224,
        sha256: "2367a9ce7c2027a0ebf407d66ff91ad5f9f88523fa440c3b6c1372305c7bf62c",
    })
} else if cfg!(target_arch = "x86_64") {
    Some(Pinned {
        name: "vmlinux-6.18.48-x86_64-gpu-f7cd54dd57a5",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-f7cd54dd57a5/vmlinux-6.18.48-x86_64-gpu",
        size: 29_963_808,
        sha256: "46b9c87de668aea61d45f32da8368aedbcc2f2b2bc0e7de4b1d38a63b7336100",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-gpu-f7cd54dd57a5",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-f7cd54dd57a5/Image-6.18.48-aarch64-gpu",
        size: 19_685_888,
        sha256: "9284d3539af0bc3bf6e6d5be314908cf248932689c8de0ce70a42396510d60c2",
    })
} else {
    None
};
