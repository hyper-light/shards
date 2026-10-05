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
        name: "vmlinux-6.18.48-x86_64-98788948976a",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-98788948976a/vmlinux-6.18.48-x86_64",
        size: 27_722_344,
        sha256: "f54407e582583eb357f3e0b575558b9d9cfecaf3f77973ce12fae5e2cdd6cef4",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-98788948976a",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-98788948976a/Image-6.18.48-aarch64",
        size: 19_081_728,
        sha256: "39647fe81509e1cb012278bcda68f28c0370081c9f0125b024bc8b2d5e9ddf96",
    })
} else {
    None
};
