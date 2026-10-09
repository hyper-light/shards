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
        name: "vmlinux-6.18.48-x86_64-9199fc6fad00",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-9199fc6fad00/vmlinux-6.18.48-x86_64",
        size: 27_723_408,
        sha256: "69405cd3e6e20432d076deac84c0414797b031de5dbadd30d1d1510eb9ad5070",
    })
} else if cfg!(target_arch = "aarch64") {
    Some(Pinned {
        name: "Image-6.18.48-aarch64-9199fc6fad00",
        url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-9199fc6fad00/Image-6.18.48-aarch64",
        size: 19_081_728,
        sha256: "9193e849396621f788bb18f48ac6b581c2b5ef99b51bc6063c21e4f774b4fa27",
    })
} else {
    None
};
