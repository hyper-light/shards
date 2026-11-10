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
        name: "vmlinux-6.18.48-x86_64-hyperv-17c3ba205400",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-17c3ba205400/vmlinux-6.18.48-x86_64-hyperv",
        size: 27_788_128,
        sha256: "43219b3751e5f60c29a12fb31518b7a11606d7a3796f68a152740c41c7bd81f6",
    })
} else if cfg!(target_arch = "x86_64") {
    Some(Pinned {
        name: "vmlinux-6.18.48-x86_64-17c3ba205400",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-17c3ba205400/vmlinux-6.18.48-x86_64",
        size: 27_758_808,
        sha256: "48a3824b7467dee7d82f36401fed99626e91c5dc5fefc432bc354a69ec616aa7",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-17c3ba205400",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-17c3ba205400/Image-6.18.48-aarch64",
        size: 19_091_968,
        sha256: "1b178851998d27b4c6c346ae391c26d675f81461fbaa19cbe514500291fe472d",
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
        name: "vmlinux-6.18.48-x86_64-kvm-17c3ba205400",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-17c3ba205400/vmlinux-6.18.48-x86_64-kvm",
        size: 27_817_504,
        sha256: "0d7e6b69d43a3ad1373c68164243f633c12c87a3f3782917a741d7703f337b60",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-kvm-17c3ba205400",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-17c3ba205400/Image-6.18.48-aarch64-kvm",
        size: 20_244_992,
        sha256: "eb06ef6171a207fe7739687342d0ec50204ff4f7a86a1ee2fcc385e8871ad6e6",
    })
} else {
    None
};

/// The kernel of a guest given a GPU (D144, D201): the `gpu` variant, DRM's virtio-gpu
/// driver built in (resources/kernel/gpu.config), and on a Windows host `hyperv+gpu`, which
/// keeps Hyper-V's time besides (D140). Of the pinned kernels' own release.
pub const KERNEL_GPU: Option<Pinned> = if cfg!(all(target_arch = "x86_64", windows)) {
    Some(Pinned {
        name: "vmlinux-6.18.48-x86_64-hyperv-gpu-17c3ba205400",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-17c3ba205400/vmlinux-6.18.48-x86_64-hyperv-gpu",
        size: 30_004_104,
        sha256: "16115b5f11f6b9b455813c3bae65af69f7c3ea678c046c14f0f666a716080973",
    })
} else if cfg!(target_arch = "x86_64") {
    Some(Pinned {
        name: "vmlinux-6.18.48-x86_64-gpu-17c3ba205400",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-17c3ba205400/vmlinux-6.18.48-x86_64-gpu",
        size: 29_982_984,
        sha256: "8cc711a1e099201dbc46d8e2a5b4f915dda1243d13fdfa50fd411cd7be1d6d18",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-gpu-17c3ba205400",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-17c3ba205400/Image-6.18.48-aarch64-gpu",
        size: 19_689_984,
        sha256: "35ba3a7ba40c8f72c10905e1b2b67eeedbe8109c69362ebfd0b5945777dd4719",
    })
} else {
    None
};
