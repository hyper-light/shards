
/// Probe (docs/research/measurements/musl-opath-v1): appended to crates/init/src/changes.rs
/// for one run, then taken away. Each test runs in a process of its own (`--exact`): `keep`
/// keeps its layers once a process.
#[cfg(test)]
mod probe {
    #![allow(clippy::unwrap_used)]
    use std::os::fd::AsRawFd as _;

    /// What `keep` keeps, as the kernel holds it: F_GETFL of each layer's descriptor.
    #[test]
    fn kept_layers_are_path_descriptors() {
        let lower = shards_testdir::TempDir::new("kept-lower").unwrap();
        let upper = shards_testdir::TempDir::new("kept-upper").unwrap();
        super::keep(lower.to_str().unwrap(), upper.to_str().unwrap()).unwrap();
        let fds = [super::lower().unwrap().as_raw_fd(), super::upper_fd().unwrap()];
        // SAFETY: fcntl(2) F_GETFL of descriptors this process holds.
        let flags = fds.map(|fd| unsafe { libc::fcntl(fd, libc::F_GETFL) });
        let path = flags.iter().all(|f| f & libc::O_PATH != 0);
        eprintln!("KEPT lower={:#o} upper={:#o} O_PATH={path}", flags[0], flags[1]);
        assert!(path);
    }

    /// A layer its keeper may not read (mode 0), kept all the same where a path is kept.
    #[test]
    fn a_layer_none_may_read_is_kept() {
        use std::os::unix::fs::PermissionsExt as _;
        let lower = shards_testdir::TempDir::new("kept-unread").unwrap();
        let upper = shards_testdir::TempDir::new("kept-upper").unwrap();
        std::fs::set_permissions(&lower, std::fs::Permissions::from_mode(0o000)).unwrap();
        let kept = super::keep(lower.to_str().unwrap(), upper.to_str().unwrap());
        std::fs::set_permissions(&lower, std::fs::Permissions::from_mode(0o700)).unwrap();
        // SAFETY: geteuid(2) takes nothing.
        let euid = unsafe { libc::geteuid() };
        eprintln!("KEPT unreadable: {kept:?} (euid {euid})");
        kept.unwrap();
    }
}
