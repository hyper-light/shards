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
        name: "vmlinux-6.18.48-x86_64-1bff175d35cb",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-1bff175d35cb/vmlinux-6.18.48-x86_64",
        size: 27_708_976,
        sha256: "136a182b7013fa32d852a7f227b91f6c113d9ad9dbe7a9b9d4baac7153ddd59c",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-1bff175d35cb",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-1bff175d35cb/Image-6.18.48-aarch64",
        size: 18_883_072,
        sha256: "ed7fb50d27b59e29e9e6c9f57f02c4bb82f8c3f5ecd51bd8083741f77597913b",
    })
} else {
    None
};
