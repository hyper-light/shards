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
        name: "vmlinux-6.18.48-x86_64-hyperv-426ba771634e",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-426ba771634e/vmlinux-6.18.48-x86_64-hyperv",
        size: 27_753_608,
        sha256: "4872a28d374fba8619f42035cbf26158de768c3118bce0266eb8af239fd0a102",
    })
} else if cfg!(target_arch = "x86_64") {
    Some(Pinned {
        name: "vmlinux-6.18.48-x86_64-426ba771634e",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-426ba771634e/vmlinux-6.18.48-x86_64",
        size: 27_724_288,
        sha256: "16888ea7591da397ae0c7f7df270192a6df3a3f1cecca6328fd413393c087da2",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-426ba771634e",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-426ba771634e/Image-6.18.48-aarch64",
        size: 19_016_192,
        sha256: "1fbbd508e8ef67e7a4380ca2e5c6264c78d14a36ff88605f8c3c62ad49891a61",
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
        name: "vmlinux-6.18.48-x86_64-kvm-426ba771634e",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-426ba771634e/vmlinux-6.18.48-x86_64-kvm",
        size: 27_782_984,
        sha256: "0e986fdc01a8627235d9613d7f39de98b04ee9b899713b2bbea64efd84d5a9ad",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-kvm-426ba771634e",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-426ba771634e/Image-6.18.48-aarch64-kvm",
        size: 20_167_168,
        sha256: "08d0448b2db8ed9194776863244e09faeb0c2b2be6ef5b62c232c5a2435eb256",
    })
} else {
    None
};

/// The kernel of a guest given a GPU (D144): the `gpu` variant, DRM's virtio-gpu driver
/// built in (resources/kernel/gpu.config); arm64's alone, as Hypervisor.framework's guests
/// alone have a GPU yet. Of the pinned kernels' own release.
pub const KERNEL_GPU: Option<Pinned> = if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-gpu-426ba771634e",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-426ba771634e/Image-6.18.48-aarch64-gpu",
        size: 19_614_208,
        sha256: "8521aea5c793b1899807f4fa4358b540f48ff24c6828e84b1db33d97bb2e99ca",
    })
} else {
    None
};
