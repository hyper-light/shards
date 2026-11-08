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
        name: "vmlinux-6.18.48-x86_64-hyperv-fa4edcc82303",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-fa4edcc82303/vmlinux-6.18.48-x86_64-hyperv",
        size: 27_773_048,
        sha256: "975846f3633fe1e94b1a507f7dfa8688494636feac9e02dfc872a8b90be8a8d2",
    })
} else if cfg!(target_arch = "x86_64") {
    Some(Pinned {
        name: "vmlinux-6.18.48-x86_64-fa4edcc82303",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-fa4edcc82303/vmlinux-6.18.48-x86_64",
        size: 27_747_824,
        sha256: "85682e108bca1e3fd0d90ee649a939e344f507af703aacca1b266511f1052c08",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-fa4edcc82303",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-fa4edcc82303/Image-6.18.48-aarch64",
        size: 19_087_872,
        sha256: "88fe0db804983c20158cd2fecdd5c6f90655ad5f33fa3f0cd0b22bf8c841391e",
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
        name: "vmlinux-6.18.48-x86_64-kvm-fa4edcc82303",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-fa4edcc82303/vmlinux-6.18.48-x86_64-kvm",
        size: 27_802_424,
        sha256: "91ae2d5ca2230be3c16bb2b0c2b50d2f1513a43cdeddf7719a73476c8f8c7848",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-kvm-fa4edcc82303",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-fa4edcc82303/Image-6.18.48-aarch64-kvm",
        size: 20_175_360,
        sha256: "825ece94b6371289a8820deed951324335fbb054b907e337116d1424de7188d8",
    })
} else {
    None
};

/// The kernel of a guest given a GPU (D144, D201): the `gpu` variant, DRM's virtio-gpu
/// driver built in (resources/kernel/gpu.config), and on a Windows host `hyperv+gpu`, which
/// keeps Hyper-V's time besides (D140). Of the pinned kernels' own release.
pub const KERNEL_GPU: Option<Pinned> = if cfg!(all(target_arch = "x86_64", windows)) {
    Some(Pinned {
        name: "vmlinux-6.18.48-x86_64-hyperv-gpu-fa4edcc82303",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-fa4edcc82303/vmlinux-6.18.48-x86_64-hyperv-gpu",
        size: 29_997_224,
        sha256: "e3df6ed6e82a756d896f27099e60c6e004031a6ac4af3226c3450d27a99051e3",
    })
} else if cfg!(target_arch = "x86_64") {
    Some(Pinned {
        name: "vmlinux-6.18.48-x86_64-gpu-fa4edcc82303",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-fa4edcc82303/vmlinux-6.18.48-x86_64-gpu",
        size: 29_963_808,
        sha256: "2fa9cd8fbd98b8cee07951b5d054927d62d3e89abf7cfa9ce6ad2db07c7cbd8b",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-gpu-fa4edcc82303",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-fa4edcc82303/Image-6.18.48-aarch64-gpu",
        size: 19_685_888,
        sha256: "e702b0218c16960c2a4c399e3349cc59de570cb2c1aabc1a3f33dfcdced54f74",
    })
} else {
    None
};
