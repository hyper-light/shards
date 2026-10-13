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
        name: "vmlinux-6.18.48-x86_64-hyperv-b3f3b948641f",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-b3f3b948641f/vmlinux-6.18.48-x86_64-hyperv",
        size: 27_753_168,
        sha256: "ce779d68a3a0e2e84ed81302514b034c8030b60f6b8bcd8cfc7eb4a72daa63ee",
    })
} else if cfg!(target_arch = "x86_64") {
    Some(Pinned {
        name: "vmlinux-6.18.48-x86_64-b3f3b948641f",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-b3f3b948641f/vmlinux-6.18.48-x86_64",
        size: 27_723_840,
        sha256: "e92155ab4331c222679870cbce8ede7baf4967925ae5a2d836f9362d194c181e",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-b3f3b948641f",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-b3f3b948641f/Image-6.18.48-aarch64",
        size: 19_016_192,
        sha256: "afeffb94428d96a0026b75fa1957f18f51697258848f7b0ecf411c562a9c5e34",
    })
} else {
    None
};

/// The kernel of a guest given a GPU (D144): the `gpu` variant, DRM's virtio-gpu driver
/// built in (resources/kernel/gpu.config); arm64's alone, as Hypervisor.framework's guests
/// alone have a GPU yet. Of the pinned kernels' own release.
pub const KERNEL_GPU: Option<Pinned> = if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-gpu-b3f3b948641f",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-b3f3b948641f/Image-6.18.48-aarch64-gpu",
        size: 19_614_208,
        sha256: "4152a240366c80db7840e95a1cf6fb193b0444dc81ab5db9b5aa604f356d0fa1",
    })
} else {
    None
};
