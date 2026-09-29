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

/// The kernel for guests of this host's architecture, which is theirs.
pub const KERNEL: Option<Pinned> = if cfg!(target_arch = "x86_64") {
    Some(Pinned {
        name: "vmlinux-6.18.48-x86_64-296d2de54137",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-296d2de54137/vmlinux-6.18.48-x86_64",
        size: 27_717_320,
        sha256: "dd464d2076713e58ae4b57f02171779fa358907d6bc7d7cc2d9f14c39209a8dc",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-296d2de54137",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-296d2de54137/Image-6.18.48-aarch64",
        size: 18_950_656,
        sha256: "5cc14e7758925368367175486516b2e338f89d18b185d9c67daaa33f893a8a96",
    })
} else {
    None
};
