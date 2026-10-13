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
        name: "vmlinux-6.18.48-x86_64-hyperv-4062e616c93a",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-4062e616c93a/vmlinux-6.18.48-x86_64-hyperv",
        size: 27_752_824,
        sha256: "f85e53a6dd02600f1370d1185a49ae0a4862f5678987b20614fbb95f489ec89b",
    })
} else if cfg!(target_arch = "x86_64") {
    Some(Pinned {
        name: "vmlinux-6.18.48-x86_64-4b8900bdffdd",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-4b8900bdffdd/vmlinux-6.18.48-x86_64",
        size: 27_723_496,
        sha256: "9dcf7906e6e154ac187dc6e0e469f4723d2f82672410609a33a341805496e265",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-4b8900bdffdd",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-4b8900bdffdd/Image-6.18.48-aarch64",
        size: 19_081_728,
        sha256: "684c7f5dc8889ca417ed593b3ce8efca0d9fcc03f494d9b72b58a1bb37e99d58",
    })
} else {
    None
};

/// The kernel of a guest given a GPU (D144): the `gpu` variant, DRM's virtio-gpu driver
/// built in (resources/kernel/gpu.config). None until a release publishes it.
pub const KERNEL_GPU: Option<Pinned> = None;
