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
        name: "vmlinux-6.18.48-x86_64-hyperv-64a9427a8997",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-64a9427a8997/vmlinux-6.18.48-x86_64-hyperv",
        size: 27_753_712,
        sha256: "d04b68b98b5ca6f1f451954a82d2fcfc275af1e8e6e3200ff9b27985b57508b3",
    })
} else if cfg!(target_arch = "x86_64") {
    Some(Pinned {
        name: "vmlinux-6.18.48-x86_64-64a9427a8997",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-64a9427a8997/vmlinux-6.18.48-x86_64",
        size: 27_724_392,
        sha256: "e23c31e89580f2fb8524cb0f560cbd356c22f2de6ae82081352d809151434261",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-64a9427a8997",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-64a9427a8997/Image-6.18.48-aarch64",
        size: 19_016_192,
        sha256: "87fbe8c0114522cdeb80686050911db0c1b6cf62b5b8544857b373e2f53571a6",
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
        name: "vmlinux-6.18.48-x86_64-kvm-64a9427a8997",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-64a9427a8997/vmlinux-6.18.48-x86_64-kvm",
        size: 27_783_080,
        sha256: "fb1a14009a675a39d61a5d2c19bd62129266bd4bbbb04038bdbefa419c0b6c2d",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-kvm-64a9427a8997",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-64a9427a8997/Image-6.18.48-aarch64-kvm",
        size: 20_167_168,
        sha256: "b3834451dd3504010f9ac398d8602f37030cda3225ecb2ef2d8f60eebc5d14a6",
    })
} else {
    None
};

/// The kernel of a guest given a GPU (D144, D201): the `gpu` variant, DRM's virtio-gpu
/// driver built in (resources/kernel/gpu.config), and on a Windows host `hyperv+gpu`, which
/// keeps Hyper-V's time besides (D140). Of the pinned kernels' own release.
pub const KERNEL_GPU: Option<Pinned> = if cfg!(all(target_arch = "x86_64", windows)) {
    Some(Pinned {
        name: "vmlinux-6.18.48-x86_64-hyperv-gpu-64a9427a8997",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-64a9427a8997/vmlinux-6.18.48-x86_64-hyperv-gpu",
        size: 29_969_696,
        sha256: "0ac2e566b75878e7f7dc35ec6c7a085cefea8540adb2543e42bdd99255e34a98",
    })
} else if cfg!(target_arch = "x86_64") {
    Some(Pinned {
        name: "vmlinux-6.18.48-x86_64-gpu-64a9427a8997",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-64a9427a8997/vmlinux-6.18.48-x86_64-gpu",
        size: 29_948_560,
        sha256: "253881ed055417b63ffc99ec5d0157d3b62ab13d84f4f91f34f429d9597b8b2f",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-gpu-64a9427a8997",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-64a9427a8997/Image-6.18.48-aarch64-gpu",
        size: 19_614_208,
        sha256: "91353b5734a840ba5ff4ecb1be73a63ff5b30b49f444f17713161314441bdb4b",
    })
} else {
    None
};
