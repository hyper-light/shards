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
        name: "vmlinux-6.18.48-x86_64-hyperv-301acf1c45d0",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-301acf1c45d0/vmlinux-6.18.48-x86_64-hyperv",
        size: 27_753_600,
        sha256: "3dc88e064d05048071dd4222424ebbadce45bf99893370808dce331138c20516",
    })
} else if cfg!(target_arch = "x86_64") {
    Some(Pinned {
        name: "vmlinux-6.18.48-x86_64-301acf1c45d0",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-301acf1c45d0/vmlinux-6.18.48-x86_64",
        size: 27_724_280,
        sha256: "124458c95cda39dee2434c541dcc510c356a501354b4543f472fd4343da70bf4",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-301acf1c45d0",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-301acf1c45d0/Image-6.18.48-aarch64",
        size: 19_016_192,
        sha256: "48a55b7554d668bffc06f8119fe47d1a7bb7ab6e44c873f62bc4b97feaef11a3",
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
        name: "vmlinux-6.18.48-x86_64-kvm-301acf1c45d0",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-301acf1c45d0/vmlinux-6.18.48-x86_64-kvm",
        size: 27_782_968,
        sha256: "5c8a46ccd383856ebae9dbfd6c88694d64096f2db26d9a82194fcbec934946dc",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-kvm-301acf1c45d0",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-301acf1c45d0/Image-6.18.48-aarch64-kvm",
        size: 20_167_168,
        sha256: "9b5ff9f43c2bffdae955938c84830ba30b7e8e465a6a3d2dd6ffe95787792ba0",
    })
} else {
    None
};

/// The kernel of a guest given a GPU (D144): the `gpu` variant, DRM's virtio-gpu driver
/// built in (resources/kernel/gpu.config); arm64's alone, as Hypervisor.framework's guests
/// alone have a GPU yet. Of the pinned kernels' own release.
pub const KERNEL_GPU: Option<Pinned> = if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-gpu-301acf1c45d0",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-301acf1c45d0/Image-6.18.48-aarch64-gpu",
        size: 19_614_208,
        sha256: "363a5467b4a814fcdd936e9fbcc01f632ec1a2c4d35bfe20f961281345ff38b4",
    })
} else {
    None
};
