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
        name: "vmlinux-6.18.48-x86_64-hyperv-4970e0190ebe",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-4970e0190ebe/vmlinux-6.18.48-x86_64-hyperv",
        size: 27_753_776,
        sha256: "5c5dcf8ba357473cc38c97eb4ecf71a44c691bb22d810ba21de78e74906e6397",
    })
} else if cfg!(target_arch = "x86_64") {
    Some(Pinned {
        name: "vmlinux-6.18.48-x86_64-4970e0190ebe",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-4970e0190ebe/vmlinux-6.18.48-x86_64",
        size: 27_724_448,
        sha256: "78faced873347085a5cbc4e803f03cab26b83dda0b626d20621fc5cb882a3354",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-4970e0190ebe",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-4970e0190ebe/Image-6.18.48-aarch64",
        size: 19_016_192,
        sha256: "d233d5327920b633003038baac78427abad59451c996d005a6221650a4339976",
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
        name: "vmlinux-6.18.48-x86_64-kvm-4970e0190ebe",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-4970e0190ebe/vmlinux-6.18.48-x86_64-kvm",
        size: 27_783_144,
        sha256: "8e629b6bde41b47a414d25440868edfe7c5f8b8e3a5318e9d38538175e848731",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-kvm-4970e0190ebe",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-4970e0190ebe/Image-6.18.48-aarch64-kvm",
        size: 20_167_168,
        sha256: "547e67d53f54e5b581e0f2cc50a6f1d6651f53d8a18213fdf58c12d9b7b868a1",
    })
} else {
    None
};

/// The kernel of a guest given a GPU (D144): the `gpu` variant, DRM's virtio-gpu driver
/// built in (resources/kernel/gpu.config); arm64's alone, as Hypervisor.framework's guests
/// alone have a GPU yet. Of the pinned kernels' own release.
pub const KERNEL_GPU: Option<Pinned> = if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-gpu-4970e0190ebe",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-4970e0190ebe/Image-6.18.48-aarch64-gpu",
        size: 19_614_208,
        sha256: "0ba4b502a99a7c87c068736e2fabe4717f88f61c2ff68650f06796d525c8d185",
    })
} else {
    None
};
