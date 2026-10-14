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
        name: "vmlinux-6.18.48-x86_64-hyperv-a6cbab8c9218",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-a6cbab8c9218/vmlinux-6.18.48-x86_64-hyperv",
        size: 27_753_608,
        sha256: "8591f237bb8b1cff263f94b6a4506ea35eb64954e0287cc2c306bc25a2c3ec62",
    })
} else if cfg!(target_arch = "x86_64") {
    Some(Pinned {
        name: "vmlinux-6.18.48-x86_64-a6cbab8c9218",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-a6cbab8c9218/vmlinux-6.18.48-x86_64",
        size: 27_724_288,
        sha256: "b47a9c50f1564d5af8a9e8d12c00324a94cd7a3d556f91651430398e8887dbac",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-a6cbab8c9218",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-a6cbab8c9218/Image-6.18.48-aarch64",
        size: 19_016_192,
        sha256: "99b434f2cbe898eb3df4c6479cc4417d848b15c51050c7034004c0f0f00e19f1",
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
        name: "vmlinux-6.18.48-x86_64-kvm-a6cbab8c9218",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-a6cbab8c9218/vmlinux-6.18.48-x86_64-kvm",
        size: 27_782_984,
        sha256: "c37b04b1c3b4f4f9062489931de7392f983ed11c6d5556626cf899f73b1dd7f9",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-kvm-a6cbab8c9218",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-a6cbab8c9218/Image-6.18.48-aarch64-kvm",
        size: 20_167_168,
        sha256: "9f4f21d0f27fb5ade9f9868ade6e34ed56acd2ea1313490be693cfdfca0050df",
    })
} else {
    None
};

/// The kernel of a guest given a GPU (D144): the `gpu` variant, DRM's virtio-gpu driver
/// built in (resources/kernel/gpu.config); arm64's alone, as Hypervisor.framework's guests
/// alone have a GPU yet. Of the pinned kernels' own release.
pub const KERNEL_GPU: Option<Pinned> = if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-gpu-a6cbab8c9218",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-a6cbab8c9218/Image-6.18.48-aarch64-gpu",
        size: 19_614_208,
        sha256: "b1b2491b479c72c6bd6b5d1fc20ec4552d5d740f605c5a48cc88658e31ba48b8",
    })
} else {
    None
};
