//! Data patterns shared by the host-side E2E tests and the guest agent, so both sides
//! derive expected disk contents from one definition.

#![no_std]

/// The byte at `offset` of a disk filled with pattern `salt`: varies with every byte
/// and every sector, so misplaced, torn, or stale transfers are all detected.
pub fn pattern_byte(salt: u64, offset: u64) -> u8 {
    let sector = offset / 512;
    let mix = sector.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ salt.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    (mix >> 29) as u8 ^ offset as u8
}

/// Fills `buf` with pattern `salt` as if it were read from disk offset `offset`.
pub fn fill(salt: u64, offset: u64, buf: &mut [u8]) {
    for (i, b) in buf.iter_mut().enumerate() {
        *b = pattern_byte(salt, offset.wrapping_add(i as u64));
    }
}

/// Index of the first byte in `buf` that does not match pattern `salt` at `offset`.
pub fn first_mismatch(salt: u64, offset: u64, buf: &[u8]) -> Option<usize> {
    buf.iter()
        .enumerate()
        .position(|(i, &b)| b != pattern_byte(salt, offset.wrapping_add(i as u64)))
}

/// FNV-1a over `bytes`: what a test's guest says of what it was given (`digest`) where
/// saying the bytes would put them where they must not be, such as a secret in a log.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &b| {
        (hash ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// FNV-1a over `len` bytes of pattern `salt` from offset 0: CPU work whose answer the
/// host can check (benches/workloads.rs).
pub fn pattern_hash(salt: u64, len: u64) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut offset = 0;
    while offset < len {
        hash ^= u64::from(pattern_byte(salt, offset));
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        offset += 1;
    }
    hash
}

/// The sysctls a Linux kernel sizes by its memory at boot (kernel/fork.c `set_max_threads`
/// and `init_user_ns`'s maxima, fs/file_table.c `files_maxfiles_init`, net/ipv4/tcp.c
/// `tcp_init`, net/ipv4/udp.c `udp_init`, fs/notify/inotify/inotify_user.c,
/// fs/eventpoll.c): what a builder whose memory is plugged as its build needs it raises to
/// all of it (D133), each said `SHARDS-TEST LIMIT NAME VALUE`, its values joined by
/// commas, by the guest booted with all of it and by a step in the builder alike.
pub const LIMITS: &[&str] = &[
    "kernel.threads-max",
    "user.max_user_namespaces",
    "user.max_pid_namespaces",
    "user.max_uts_namespaces",
    "user.max_ipc_namespaces",
    "user.max_net_namespaces",
    "user.max_mnt_namespaces",
    "user.max_cgroup_namespaces",
    "user.max_time_namespaces",
    "user.max_inotify_watches",
    "fs.file-max",
    "fs.inotify.max_user_watches",
    "fs.epoll.max_user_watches",
    "net.ipv4.tcp_mem",
    "net.ipv4.udp_mem",
    "net.ipv4.tcp_rmem",
    "net.ipv4.tcp_wmem",
    "net.ipv4.tcp_max_orphans",
    "net.ipv4.tcp_max_tw_buckets",
];
