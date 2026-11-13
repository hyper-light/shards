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
        name: "vmlinux-6.18.48-x86_64-hyperv-54fba99afaee",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-54fba99afaee/vmlinux-6.18.48-x86_64-hyperv",
        size: 27_788_128,
        sha256: "576c3e25e543b8678b3f48de215a998d8685385f1f5de9170d7e101dbc60f43c",
    })
} else if cfg!(target_arch = "x86_64") {
    Some(Pinned {
        name: "vmlinux-6.18.48-x86_64-54fba99afaee",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-54fba99afaee/vmlinux-6.18.48-x86_64",
        size: 27_758_808,
        sha256: "f21e65704b3062000bb5d25450872066b22fbe661c5bb795c424de2a90435449",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-54fba99afaee",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-54fba99afaee/Image-6.18.48-aarch64",
        size: 19_091_968,
        sha256: "27890635bb25d52fcbcde41f5077a7161a70c7bd6fcb4fa57221c934b4c4befb",
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
        name: "vmlinux-6.18.48-x86_64-kvm-54fba99afaee",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-54fba99afaee/vmlinux-6.18.48-x86_64-kvm",
        size: 27_817_504,
        sha256: "71b5de5f7c9b4abfc543e230ad09bc1736e2ca15f7e76cbd28efbc546e6fa89e",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-kvm-54fba99afaee",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-54fba99afaee/Image-6.18.48-aarch64-kvm",
        size: 20_244_992,
        sha256: "ed1e6dabf2f4088b37e3405a40b12962eb5eff76afe4303730e9d76ee1a7a3ba",
    })
} else {
    None
};

/// The kernel of a guest given a GPU (D144, D201): the `gpu` variant, DRM's virtio-gpu
/// driver built in (resources/kernel/gpu.config), and on a Windows host `hyperv+gpu`, which
/// keeps Hyper-V's time besides (D140). Of the pinned kernels' own release.
pub const KERNEL_GPU: Option<Pinned> = if cfg!(all(target_arch = "x86_64", windows)) {
    Some(Pinned {
        name: "vmlinux-6.18.48-x86_64-hyperv-gpu-54fba99afaee",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-54fba99afaee/vmlinux-6.18.48-x86_64-hyperv-gpu",
        size: 30_004_104,
        sha256: "5bbefce7fbd7f4ae1a2962c0e95c7a72536b3e0320605aeff150e044a506f224",
    })
} else if cfg!(target_arch = "x86_64") {
    Some(Pinned {
        name: "vmlinux-6.18.48-x86_64-gpu-54fba99afaee",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-54fba99afaee/vmlinux-6.18.48-x86_64-gpu",
        size: 29_982_984,
        sha256: "000708c81d124a6eb6924c146bf0b50744a664a78b97d73ca626055886fadbab",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-gpu-54fba99afaee",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-54fba99afaee/Image-6.18.48-aarch64-gpu",
        size: 19_689_984,
        sha256: "1ca72281a0323391b782763d0a3e5fb0a6af455ecf1301c97be61a146013c576",
    })
} else {
    None
};
