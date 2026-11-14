//! shards-vm confines itself before it reads anything a guest, a snapshot or a daemon
//! sends: a seccomp filter over its whole process allowing the syscalls a VM process is
//! seen to make, over every VM test on a KVM host (docs/research/measurements/vmm-syscalls,
//! PM M52), and nothing else (docs/research/rootless-security.md R3). Where it has an
//! argument that says what it does, only the values it is made with pass: ioctl requests
//! are the KVM backend's own (hv::IOCTLS) and three a terminal and a nonblocking socket
//! need, sockets are Unix sockets, and threads are all it may create: `clone3`, whose
//! flags a filter cannot read, fails with ENOSYS, so the C library makes threads with
//! `clone`, whose flags must be those of a thread, as Firecracker's filters have it.
//! No exec, no process of its own, no network.
//!
//! Linux, where the KVM backend is, on x86_64 and arm64. arm64's system calls are the
//! generic ABI's, which has none of x86_64's that name a path or a descriptor without one
//! to start from (open, stat, access, mkdir, rename, unlink, readlink, poll, dup2): their
//! `*at` forms, which the C libraries make there, take their places (D154).

#[cfg(target_os = "linux")]
pub fn confine() -> Result<(), String> {
    confine_as(landlock_abi())
}

/// [`confine`], on a kernel whose Landlock ABI is `abi`.
#[cfg(target_os = "linux")]
fn confine_as(abi: i64) -> Result<(), String> {
    use shards_vmm::platform::seccomp::{self, Rule};

    #[cfg(target_arch = "x86_64")]
    let arch = [
        // musl's `open`, `stat`, `lstat`, `access`, `mkdir`, `rename`, `unlink`, `readlink`
        // and `poll`, where glibc's make the *at calls.
        libc::SYS_open,
        libc::SYS_stat,
        libc::SYS_lstat,
        libc::SYS_access,
        libc::SYS_mkdir,
        libc::SYS_rename,
        libc::SYS_unlink,
        libc::SYS_readlink,
        libc::SYS_poll,
        libc::SYS_dup2,
        libc::SYS_arch_prctl,
    ];
    #[cfg(target_arch = "aarch64")]
    let arch = [libc::SYS_faccessat, libc::SYS_mkdirat, libc::SYS_readlinkat];
    let any = [
        libc::SYS_read,
        libc::SYS_write,
        libc::SYS_readv,
        libc::SYS_writev,
        libc::SYS_pread64,
        libc::SYS_pwrite64,
        libc::SYS_lseek,
        libc::SYS_close,
        libc::SYS_fstat,
        libc::SYS_newfstatat,
        libc::SYS_statx,
        libc::SYS_openat,
        libc::SYS_faccessat2,
        libc::SYS_getdents64,
        libc::SYS_renameat,
        libc::SYS_unlinkat,
        libc::SYS_getcwd,
        libc::SYS_flock,
        libc::SYS_fsync,
        libc::SYS_fdatasync,
        libc::SYS_ftruncate,
        libc::SYS_mmap,
        libc::SYS_mprotect,
        libc::SYS_munmap,
        libc::SYS_mremap,
        libc::SYS_madvise,
        // A run's swap, flushed to its file as a save copies it (D176).
        libc::SYS_msync,
        // Which of a DAX window's pages its host holds, recorded for its guest as each range
        // is mapped (D189, `Window::record_held`).
        libc::SYS_mincore,
        libc::SYS_brk,
        libc::SYS_futex,
        libc::SYS_clock_nanosleep,
        libc::SYS_nanosleep,
        libc::SYS_clock_gettime,
        libc::SYS_sched_yield,
        libc::SYS_sched_getaffinity,
        libc::SYS_getrandom,
        // The C library's sysconf(_SC_PHYS_PAGES), the host's memory, which a snapshot's
        // state is read within (vmm snapshot `StateRefused::PastHost`, migrate-04), as
        // /proc/meminfo is past Landlock's rules: glibc's and musl's ask sysinfo(2).
        libc::SYS_sysinfo,
        libc::SYS_getpid,
        libc::SYS_gettid,
        libc::SYS_getppid,
        libc::SYS_getuid,
        libc::SYS_geteuid,
        libc::SYS_getgid,
        libc::SYS_getegid,
        libc::SYS_rt_sigaction,
        libc::SYS_rt_sigprocmask,
        libc::SYS_rt_sigreturn,
        libc::SYS_rt_sigtimedwait,
        libc::SYS_sigaltstack,
        libc::SYS_restart_syscall,
        libc::SYS_set_robust_list,
        libc::SYS_set_tid_address,
        libc::SYS_rseq,
        libc::SYS_exit,
        libc::SYS_exit_group,
        libc::SYS_ppoll,
        libc::SYS_pipe2,
        // The eventfds a passed-through device's interrupts reach KVM by (D154).
        libc::SYS_eventfd2,
        libc::SYS_dup,
        libc::SYS_dup3,
        libc::SYS_accept4,
        libc::SYS_bind,
        libc::SYS_listen,
        libc::SYS_connect,
        libc::SYS_sendto,
        libc::SYS_recvfrom,
        libc::SYS_sendmsg,
        libc::SYS_recvmsg,
        libc::SYS_shutdown,
        // Landlock, applied after the filter: it can only take rights away.
        libc::SYS_landlock_create_ruleset,
        libc::SYS_landlock_add_rule,
        libc::SYS_landlock_restrict_self,
    ];
    let mut rules: Vec<Rule> = any.into_iter().chain(arch).map(Rule::any).collect();
    // The requests' low 32 bits: the kernel's `cmd` is an unsigned int.
    let mut requests: Vec<u32> = shards_vmm::hv::IOCTLS.iter().map(|&r| r as u32).collect();
    // VFIO's, for a passed-through PCI function (D154): only Landlock's rules give a
    // process a VFIO descriptor to make them of.
    requests.extend(shards_vmm::devices::pci::vfio::IOCTLS.iter().map(|&r| r as u32));
    // A userfaultfd's handshake and its ranges' registration, which hold the host's
    // fault-around off over a restored guest's memory for its machine's life, as it
    // records its working set and maps one ahead (platform::FaultAroundOff); none of its
    // faults is the userfaultfd's.
    requests.extend([
        shards_vmm::platform::uffd::IOCTL_API,
        shards_vmm::platform::uffd::IOCTL_REGISTER,
    ]);
    // What a terminal is asked: whether it is one, its settings, and its settings set
    // (raw mode, terminal.rs). `isatty` asks TIOCGWINSZ in musl, TCGETS in older glibc and
    // TCGETS2 in glibc 2.43, whose tcgetattr asks TCGETS2 and tcsetattr sets with TCSETS2
    // (their code on Fedora 44, where a filter without them killed `run --kernel`'s VM).
    requests.extend([
        libc::FIONBIO as u32,
        libc::TCGETS as u32,
        libc::TCSETS as u32,
        libc::TCGETS2 as u32,
        libc::TCSETS2 as u32,
        libc::TIOCGWINSZ as u32,
    ]);
    rules.push(Rule::with(libc::SYS_ioctl, 1, &requests));
    // The GPU's VM process runs its host's GPU driver (D201): the requests of its drivers'
    // devices, by their types, whatever their numbers and sizes, as each driver's version
    // makes them, Landlock's rules giving it those devices alone to make them of; and the
    // syscalls the drivers and the renderer make besides (PM M382).
    if shards_vmm::devices::virtio::gpu::RENDERS {
        rules.push(Rule::masked(
            libc::SYS_ioctl,
            1,
            0xff00,
            &GPU_IOCTL_TYPES.map(|t| u32::from(t) << 8),
        ));
        rules.extend(GPU_SYSCALLS.map(Rule::any));
        // The render server's and its client's check of their connection's type
        // (render_socket.c, proxy_socket.c).
        rules.push(Rule::with(libc::SYS_getsockopt, 2, &[libc::SO_TYPE as u32]));
        // NVIDIA's driver links /dev/char's names to its devices where udev has not
        // (/dev/char/195:0 to ../nvidia0), which no user but root may: refused as its host
        // refuses them, it goes on without (PM M382).
        #[cfg(target_arch = "x86_64")]
        rules.push(Rule::fails(libc::SYS_symlink, libc::EACCES as u16));
        rules.push(Rule::fails(libc::SYS_symlinkat, libc::EACCES as u16));
        // The renderer seals the shared memory its server hands it, and checks the seals
        // (proxy_context.c, F_ADD_SEALS and F_GET_SEALS), and its memory files are given
        // their room (fallocate(2), mode 0).
        rules.push(Rule::with(
            libc::SYS_fcntl,
            1,
            &[libc::F_ADD_SEALS as u32, libc::F_GET_SEALS as u32],
        ));
        rules.push(Rule::with(libc::SYS_fallocate, 1, &[0]));
        // RADV, Mesa's AMD driver, sets a thread's CPUs as it renders (sched_setaffinity,
        // from the renderer's ring thread) and renders on where it may not: refused, as a
        // VM process sets no thread's CPUs, which could be another of its user's
        // processes' (PM M384).
        rules.push(Rule::fails(libc::SYS_sched_setaffinity, libc::EPERM as u16));
        // So are a thread's priority and policy, which Venus's ring thread sets as its
        // guest asks (virglrenderer's vkr_ring.c, setpriority) and Mesa's sets for the
        // queues it starts at their least (u_queue.c: SCHED_BATCH by pthread_setschedparam,
        // nice 19): refused with EPERM, which both go on past, as the VM process's threads'
        // scheduling is shards' (Mesa 26.1's Intel driver, from musl, ended the process
        // with them, its ring thread blocking the filter's signal).
        rules.push(Rule::fails(libc::SYS_setpriority, libc::EPERM as u16));
        rules.push(Rule::fails(libc::SYS_sched_setscheduler, libc::EPERM as u16));
        // Venus makes a dma-buf of the renderer's memory files through udmabuf where a
        // driver exports no host memory it can map, as Mesa's Intel driver on Intel's UHD
        // Graphics (vkr_physical_device.c): UDMABUF_CREATE alone, _IOW('u', 0x42, struct
        // udmabuf_create) of linux/udmabuf.h, Landlock's rules giving it /dev/udmabuf.
        rules.push(Rule::with(libc::SYS_ioctl, 1, &[UDMABUF_CREATE]));
        // A dma-buf's fences as a sync file, out and in (linux/dma-buf.h,
        // DMA_BUF_IOCTL_EXPORT_SYNC_FILE and _IMPORT_SYNC_FILE), the implicit sync a driver
        // keeps with what else uses its memory: Mesa 26.1's Intel driver exported one,
        // Alpine's, and refused, its VM process ended as its guest's vulkaninfo ran.
        rules.push(Rule::with(
            libc::SYS_ioctl,
            1,
            &[DMA_BUF_IOCTL_EXPORT_SYNC_FILE, DMA_BUF_IOCTL_IMPORT_SYNC_FILE],
        ));
        // musl's GPU's VM process, linked dynamically: musl registers for private expedited
        // memory barriers as a process starts its first thread, and its loader takes one as
        // a library it opens brings thread-local storage, the drivers' (musl's
        // src/linux/membarrier.c and ldso/dynlink.c, install_new_tls). Refused, the first
        // ended the process before it was ready.
        if cfg!(target_env = "musl") {
            rules.push(Rule::with(
                libc::SYS_membarrier,
                0,
                &[
                    MEMBARRIER_CMD_PRIVATE_EXPEDITED,
                    MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED,
                ],
            ));
        }
    }
    let commands = [
        libc::F_DUPFD,
        libc::F_DUPFD_CLOEXEC,
        libc::F_GETFD,
        libc::F_SETFD,
        libc::F_GETFL,
        libc::F_SETFL,
    ];
    rules.push(Rule::with(libc::SYS_fcntl, 1, &commands.map(|c| c as u32)));
    // musl 1.2.6's pwrite(3) is pwritev2 of its one buffer with RWF_NOAPPEND, which writes
    // at the offset as pwrite64 does, an O_APPEND file's too (musl's src/unistd/pwrite.c):
    // the GPU's VM process, linked to a musl host's musl (D208), makes it as it saves a
    // snapshot; the other helpers, static with Rust's own musl, make pwrite64 as they
    // save (CI's musl jobs' snapshot tests).
    if cfg!(target_env = "musl") {
        rules.push(Rule::with(libc::SYS_pwritev2, 5, &[libc::RWF_NOAPPEND as u32]));
    }
    // Signals to this process's own threads alone: a vCPU's kick, and glibc's raise(3).
    // SAFETY: getpid(2) has no preconditions.
    let pid = unsafe { libc::getpid() };
    rules.push(Rule::with(libc::SYS_tgkill, 0, &[pid as u32]));
    // This process's own limits alone, as its C library reads them (pid 0: glibc's and
    // musl's getrlimit, as pthread_create sizes a stack): another process's, its spawner's
    // among them, are its user's to change, irreversibly for a hard limit
    // (getrlimit(2), "prlimit"; kernel/sys.c, `check_prlimit_permission`), and Landlock
    // has no hook on them.
    rules.push(Rule::with(libc::SYS_prlimit64, 0, &[0, pid as u32]));
    // A thread's own scheduling slice, read and set (pid 0, the calling thread alone): a
    // vCPU's, a device worker's and the hold's governor's, which run as they wake on a
    // busy host (platform `prioritize_vcpu_thread`, PM M440). Unprivileged, the kernel
    // refuses it a higher priority or a real-time policy.
    rules.push(Rule::with(libc::SYS_sched_getattr, 0, &[0]));
    rules.push(Rule::with(libc::SYS_sched_setattr, 0, &[0]));
    // A pidfd of this process alone: what a run's client waits on for its VM process's end
    // where the VM holds a host's devices (warm.rs, `finish`).
    rules.push(Rule::with(libc::SYS_pidfd_open, 0, &[pid as u32]));
    // A thread's pidfd (PIDFD_THREAD, include/uapi/linux/pidfd.h: O_EXCL), whose end wakes
    // a Landlock layer's wait for its answer where the kernel's ABI asks each thread
    // (`asked`), before v8: so its rules allow a pidfd of any thread on its host, which
    // says whether that thread lives, and nothing else a VM process may use (it may make
    // no pidfd_send_signal(2), pidfd_getfd(2), setns(2) or waitid(2)).
    if abi < TSYNC_ABI {
        rules.push(Rule::with(libc::SYS_pidfd_open, 1, &[libc::O_EXCL as u32]));
    }
    // That userfaultfd, of user faults alone (UFFD_USER_MODE_ONLY), which takes no
    // privilege.
    rules.push(Rule::with(
        libc::SYS_userfaultfd,
        0,
        &[shards_vmm::platform::uffd::FLAGS as u32],
    ));
    // A seccomp layer over this filter ([`sockets`]). Unprivileged, a filter installs only
    // under no_new_privs, and takes syscalls away alone (seccomp(2): every filter a thread
    // has runs, and the most restrictive answer wins).
    rules.push(Rule::with(libc::SYS_seccomp, 0, &[libc::SECCOMP_SET_MODE_FILTER]));
    rules.push(Rule::with(libc::SYS_socket, 0, &[libc::AF_UNIX as u32]));
    rules.push(Rule::with(libc::SYS_socketpair, 0, &[libc::AF_UNIX as u32]));
    // Naming threads; and no_new_privs, which Landlock needs set and which only takes
    // privilege away.
    rules.push(Rule::with(
        libc::SYS_prctl,
        0,
        &[
            libc::PR_SET_NAME as u32,
            libc::PR_GET_NAME as u32,
            libc::PR_SET_NO_NEW_PRIVS as u32,
        ],
    ));
    // A thread's flags, glibc's (nptl `create_thread`) and musl's (`pthread_create`),
    // which adds CLONE_DETACHED.
    let thread = libc::CLONE_VM
        | libc::CLONE_FS
        | libc::CLONE_FILES
        | libc::CLONE_SIGHAND
        | libc::CLONE_THREAD
        | libc::CLONE_SYSVSEM
        | libc::CLONE_SETTLS
        | libc::CLONE_PARENT_SETTID
        | libc::CLONE_CHILD_CLEARTID;
    rules.push(Rule::with(
        libc::SYS_clone,
        0,
        &[thread as u32, (thread | libc::CLONE_DETACHED) as u32],
    ));
    rules.push(Rule::fails(libc::SYS_clone3, libc::ENOSYS as u16));
    let filter = seccomp::compile(&rules)?;
    seccomp::name_refusals().map_err(|e| format!("SIGSYS: {e}"))?;
    seccomp::install(&filter, true).map_err(|e| format!("installing the seccomp filter: {e}"))
}

#[cfg(not(target_os = "linux"))]
pub fn confine() -> Result<(), String> {
    Ok(())
}

/// Narrows this process's Unix sockets on every thread: a seccomp layer over [`confine`]'s
/// filter (`seccomp::layer`), which every kernel with seccomp's TSYNC has (Linux 3.17),
/// installed as the process confines itself, beside [`landlock`], whose ABI v9 governs a
/// connect to a socket file too (`RESOLVE_UNIX`), an extra layer.
///
/// - A socket is a stream or a sequenced-packet one, of either's flags: one of the
///   datagram type could send to any socket file it names (sendto(2), sendmsg(2) with an
///   address; net/unix/af_unix.c, `unix_dgram_sendmsg`), where a stream refuses an address
///   (`unix_stream_sendmsg`, EOPNOTSUPP or EISCONN) and a sequenced-packet socket ignores
///   it (`unix_seqpacket_sendmsg`). A VM process makes no datagram socket: Rust's own and
///   the vsock muxer's sockets are streams, the GPU's renderer's sequenced packets
///   (virgl.rs).
/// - A VM process dials nothing: connect(2) fails with EACCES, as Landlock would refuse it
///   (Documentation/userspace-api/landlock.rst, `LANDLOCK_ACCESS_FS_RESOLVE_UNIX`), on
///   every kernel. Its user's sockets, its daemon's Engine API among them, cannot be
///   dialed. A daemon's VMs dial none (`--warm` takes no `--vsock`): their host ports are
///   served in their process. One given a vsock path has its spawner dial the host ports
///   beside it, and bind the path, as on macOS and Windows (`grant_ask::listen_at`, `dial`;
///   `kind::DIAL`).
#[cfg(target_os = "linux")]
pub fn sockets() -> Result<(), String> {
    use shards_vmm::platform::seccomp::{self, Rule};
    let types = [libc::SOCK_STREAM, libc::SOCK_SEQPACKET]
        .into_iter()
        .flat_map(|t| {
            [
                t,
                t | libc::SOCK_CLOEXEC,
                t | libc::SOCK_NONBLOCK,
                t | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            ]
            .map(|t| t as u32)
        });
    let types: Vec<u32> = types.collect();
    let rules = [
        Rule::with(libc::SYS_socket, 1, &types),
        Rule::with(libc::SYS_socketpair, 1, &types),
        Rule::fails(libc::SYS_connect, libc::EACCES as u16),
    ];
    let layer = seccomp::layer(&rules)?;
    seccomp::install(&layer, true).map_err(|e| format!("installing the sockets' seccomp layer: {e}"))
}

/// The ioctl types of the GPU drivers' devices (include/uapi/asm-generic/ioctl.h, bits 8
/// to 15 of a request), as NVIDIA's and Mesa's RADV make them through Venus (PM M382):
/// NVIDIA's `'F'` (NV_IOCTL_MAGIC, its control and GPU nodes) and `'m'` (NVKMS_IOCTL_MAGIC,
/// nvidia-modeset's), and DRM's `'d'` (DRM_IOCTL_BASE, every render node's).
#[cfg(target_os = "linux")]
const GPU_IOCTL_TYPES: [u8; 3] = *b"Fmd";

/// udmabuf's one request a GPU's VM process makes (linux/udmabuf.h): _IOW('u', 0x42,
/// struct udmabuf_create), its 24 bytes a descriptor, flags, an offset and a size.
#[cfg(target_os = "linux")]
const UDMABUF_CREATE: u32 = (1 << 30) | (24 << 16) | ((b'u' as u32) << 8) | 0x42;

/// A dma-buf's sync-file requests (linux/dma-buf.h): _IOWR('b', 2, struct
/// dma_buf_export_sync_file) and _IOW('b', 3, struct dma_buf_import_sync_file), each 8
/// bytes, flags and a descriptor.
#[cfg(target_os = "linux")]
const DMA_BUF_IOCTL_EXPORT_SYNC_FILE: u32 = (3 << 30) | (8 << 16) | ((b'b' as u32) << 8) | 2;
#[cfg(target_os = "linux")]
const DMA_BUF_IOCTL_IMPORT_SYNC_FILE: u32 = (1 << 30) | (8 << 16) | ((b'b' as u32) << 8) | 3;

/// membarrier(2)'s commands musl makes (linux/membarrier.h), which the libc crate names on
/// Android alone.
#[cfg(target_os = "linux")]
const MEMBARRIER_CMD_PRIVATE_EXPEDITED: u32 = 1 << 3;
#[cfg(target_os = "linux")]
const MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED: u32 = 1 << 4;

/// What else the GPU's VM process's drivers make, every argument allowed (PM M382):
/// NVIDIA's `statfs` of where it keeps files and `sched_get_priority_max` and `_min` of the
/// threads it starts, the address of a socket it was given (`getsockname`, libGLX_nvidia),
/// the C library's `getresuid` and `getresgid` as a driver asks whether it runs set-uid or
/// set-gid, anonymous files of memory (`memfd_create`): Mesa's, NVIDIA's and the renderer's
/// own shared memory. (`sysinfo`, by which the C library's `sysconf(_SC_PHYS_PAGES)` says
/// the host's memory to Mesa's Intel driver, which sizes an integrated GPU's heap by it, is
/// every VM process's now: Mesa's os_get_total_physical_memory; refused, ANV on Intel's UHD
/// Graphics ended its VM process before it was ready.)
#[cfg(target_os = "linux")]
const GPU_SYSCALLS: [libc::c_long; 7] = [
    libc::SYS_statfs,
    libc::SYS_sched_get_priority_max,
    libc::SYS_sched_get_priority_min,
    libc::SYS_getsockname,
    libc::SYS_getresuid,
    libc::SYS_getresgid,
    libc::SYS_memfd_create,
];

/// What a GPU's VM process reads and controls besides a VM process's, on Linux (D201), as
/// its host's Vulkan loader and drivers reach them, measured with NVIDIA's and Mesa's
/// (PM M382): the loader's manifests and the libraries they name, the system's libraries
/// and the drivers' data, the host's devices and processors as sysfs and procfs describe
/// them, and the GPUs' device nodes, each where the host has it. Nothing is written: the
/// drivers' shader caches stay off.
#[cfg(target_os = "linux")]
pub fn gpu_paths(paths: &mut Paths) {
    use std::path::{Path, PathBuf};
    let there = |p: &str| Path::new(p).exists().then(|| PathBuf::from(p));
    // The loader's manifests, where its search finds them (Vulkan-Loader's
    // LoaderInterfaceArchitecture.md, Linux's search paths), which a process given no
    // home's variables searches.
    let manifests = [
        "/etc/xdg/vulkan",
        "/etc/vulkan",
        "/usr/local/share/vulkan",
        "/usr/share/vulkan",
    ];
    // The system's libraries, which a manifest's library names and its own dependencies
    // are found among, and the drivers' data: NVIDIA's application profiles, libdrm's
    // device names, Mesa's options (drirc), EGL's and GLVND's vendors, which NVIDIA's
    // and Mesa's drivers read as they start.
    let data = [
        "/usr/lib",
        "/usr/lib64",
        "/usr/local/lib",
        "/lib",
        "/lib64",
        "/usr/share/nvidia",
        "/etc/nvidia",
        "/usr/share/libdrm",
        "/usr/share/drirc.d",
        "/usr/share/egl",
        "/usr/share/glvnd",
        "/sys",
        "/proc/driver",
    ];
    paths
        .read_under
        .extend(manifests.iter().chain(&data).filter_map(|p| there(p)));
    // Each library a manifest names by its path, where it is.
    for dir in manifests
        .iter()
        .flat_map(|m| ["icd.d", "implicit_layer.d", "explicit_layer.d"].map(|d| Path::new(m).join(d)))
    {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            for library in library_paths(&text) {
                let library = Path::new(&library);
                if library.components().count() < 2 {
                    continue;
                }
                let library = if library.is_absolute() {
                    library.to_path_buf()
                } else {
                    dir.join(library)
                };
                if let Some(parent) = library.parent().filter(|p| p.exists()) {
                    paths.read_under.push(parent.to_path_buf());
                }
            }
        }
    }
    paths.read.extend(
        [
            "/etc/ld.so.cache",
            "/etc/drirc",
            "/proc/stat",
            "/proc/meminfo",
            "/proc/cpuinfo",
            "/proc/filesystems",
        ]
        .iter()
        .filter_map(|p| there(p)),
    );
    // musl's loader's search path, where a musl host names one (musl's ldso/dynlink.c).
    paths
        .read
        .extend(there(&format!("/etc/ld-musl-{}.path", std::env::consts::ARCH)));
    // /dev/dri listed, its render nodes used; NVIDIA's control, GPU, modesetting and
    // unified memory nodes; udmabuf, which Venus makes host memory of where a driver
    // exports none (vkr_physical_device.c).
    paths.read_under.extend(there("/dev/dri"));
    let named = |dir: &str, keep: &dyn Fn(&str) -> bool| -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| e.file_name().to_str().is_some_and(keep))
                    .map(|e| e.path())
                    .collect()
            })
            .unwrap_or_default()
    };
    paths
        .devices
        .extend(named("/dev/dri", &|n| n.starts_with("renderD")));
    paths.devices.extend(named("/dev", &|n| {
        n == "nvidiactl"
            || n == "nvidia-modeset"
            || n == "nvidia-uvm"
            || n == "nvidia-uvm-tools"
            || n.strip_prefix("nvidia")
                .is_some_and(|i| !i.is_empty() && i.bytes().all(|b| b.is_ascii_digit()))
    }));
    paths.devices.extend(there("/dev/udmabuf"));
}

/// The `library_path`s a Vulkan manifest names (LoaderInterfaceArchitecture.md, "Driver
/// Manifest File Format" and the layers' own): each value of that key, read as a JSON
/// string.
#[cfg(target_os = "linux")]
fn library_paths(manifest: &str) -> Vec<String> {
    const KEY: &str = "\"library_path\"";
    let mut found = Vec::new();
    let mut rest = manifest;
    while let Some(at) = rest.find(KEY) {
        rest = rest.get(at + KEY.len()..).unwrap_or_default();
        let Some(value) = rest
            .trim_start()
            .strip_prefix(':')
            .and_then(|v| v.trim_start().strip_prefix('"'))
        else {
            continue;
        };
        let mut out = String::new();
        let mut chars = value.chars();
        let mut closed = false;
        while let Some(c) = chars.next() {
            match c {
                '"' => {
                    closed = true;
                    break;
                }
                '\\' => match chars.next() {
                    Some(c @ ('/' | '\\' | '"')) => out.push(c),
                    _ => break,
                },
                c => out.push(c),
            }
        }
        if closed {
            found.push(out);
        }
        rest = chars.as_str();
    }
    found
}

/// The files and sockets a VM process may use, for the grants its spawner gives it in App
/// Sandbox on macOS and in an AppContainer on Windows (`grant`), and its Landlock rules on
/// Linux (D30): all it is denied besides.
#[derive(Debug, Default, Clone)]
pub struct Paths {
    /// Files it reads.
    pub read: Vec<std::path::PathBuf>,
    /// Directories whose files it reads.
    pub read_under: Vec<std::path::PathBuf>,
    /// Files it reads and writes.
    pub write: Vec<std::path::PathBuf>,
    /// Directories it may make, and in which it reads and writes anything.
    pub write_under: Vec<std::path::PathBuf>,
    /// Device files it reads, writes and controls: a passed-through PCI function's VFIO
    /// container and group (D154).
    pub devices: Vec<std::path::PathBuf>,
    /// Directories it removes Unix sockets in (Linux): that of a vsock socket path it was
    /// given (`--vsock`), which its spawner binds for it. The ports its own process serves
    /// need none.
    pub sockets_under: Vec<std::path::PathBuf>,
    /// The vsock socket path it was given, which its spawner binds for it, and beside
    /// which it dials for it (grant).
    pub vsock: Option<std::path::PathBuf>,
}

/// Applies `paths` to every thread of this process with Landlock
/// (https://docs.kernel.org/userspace-api/landlock.html), as App Sandbox's grants do on
/// macOS (D30). Every filesystem access Landlock's ABI v5 knows is handled, and only
/// `paths` are allowed, with `/dev/kvm`, `/dev/null`, this process's `/proc` entry and the
/// kernel's transparent huge page settings. TCP is refused, and where the ABI knows them
/// (v6), signals to other processes and abstract Unix sockets outside this one, and (v9)
/// every connection to a socket file, which its spawner dials for it.
///
/// A layer restricts the thread that makes it and the threads it starts after, not the
/// ones already running (landlock_restrict_self(2)); a VM process has some by then (its
/// exec port's, its rates', its stdin's), and a seal's layer comes with every vCPU and
/// device thread running. So each layer reaches every thread ([`every_thread`]): at once
/// where the ABI has `TSYNC` (v8), else by each thread making it itself.
///
/// It fails closed (PM M66): a kernel without Landlock, or whose ABI is older than v5, the
/// first that governs a device's ioctls (/dev/kvm's), starts no VM, and neither does a path
/// it must allow that is not there. Each path gets only the rights its use takes: no
/// directory may take a file from another (`REFER`), and none makes a socket: its spawner
/// binds its vsock path for it. The version is asked of the kernel, never inferred from its release, as
/// Landlock's maintainer asks (firecracker-microvm/firecracker#5771).
///
/// Rules hold inodes, not names: a directory keeps its rule when renamed, as a template's
/// is when the daemon settles it. What is written with a template after that, its working
/// set, the daemon writes: no VM may write a template.
#[cfg(target_os = "linux")]
pub fn landlock(paths: &Paths) -> Result<(), String> {
    let (ruleset, abi) = ruleset(paths)?;
    // SAFETY: prctl(2) with integer arguments.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(format!("no_new_privs: {}", std::io::Error::last_os_error()));
    }
    let reach = if abi >= TSYNC_ABI {
        Reach::AtOnce
    } else {
        Reach::Asked
    };
    every_thread(&ruleset, reach).map_err(|e| format!("Landlock: {e}"))?;
    shards_vmm::debug!("confined by Landlock ABI v{abi}, {reach:?}");
    Ok(())
}

/// [`landlock`]'s ruleset for `paths`, and the kernel's Landlock ABI.
#[cfg(target_os = "linux")]
fn ruleset(paths: &Paths) -> Result<(std::os::fd::OwnedFd, i64), String> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;

    // include/uapi/linux/landlock.h
    const CREATE_RULESET_VERSION: u32 = 1;
    /// The oldest ABI a VM runs under: v5 (Linux 6.10), whose rules govern device ioctls.
    const MIN_ABI: i64 = 5;
    const RULE_PATH_BENEATH: libc::c_long = 1;
    const EXECUTE: u64 = 1 << 0;
    const WRITE_FILE: u64 = 1 << 1;
    const READ_FILE: u64 = 1 << 2;
    const READ_DIR: u64 = 1 << 3;
    const REMOVE_DIR: u64 = 1 << 4;
    const REMOVE_FILE: u64 = 1 << 5;
    const MAKE_DIR: u64 = 1 << 7;
    const MAKE_REG: u64 = 1 << 8;
    const REFER: u64 = 1 << 13;
    const TRUNCATE: u64 = 1 << 14;
    const IOCTL_DEV: u64 = 1 << 15;
    const RESOLVE_UNIX: u64 = 1 << 16;
    /// The first ABI whose rules govern connections to socket files (`RESOLVE_UNIX`).
    const RESOLVE_UNIX_ABI: i64 = 9;
    const NET_BIND_TCP: u64 = 1 << 0;
    const NET_CONNECT_TCP: u64 = 1 << 1;
    const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
    const SCOPE_SIGNAL: u64 = 1 << 1;
    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
        handled_access_net: u64,
        scoped: u64,
    }
    #[repr(C, packed)]
    struct PathBeneath {
        allowed_access: u64,
        parent_fd: i32,
    }

    // SAFETY: landlock_create_ruleset(2) with no attribute asks only the ABI's version.
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            CREATE_RULESET_VERSION,
        )
    };
    if abi < 0 {
        let e = std::io::Error::last_os_error();
        return Err(match e.raw_os_error() {
            Some(libc::ENOSYS | libc::EOPNOTSUPP) => format!(
                "this kernel has no Landlock ({e}): shards confines each VM process with it, and \
                 needs Linux 6.10 or later with Landlock enabled (lsm=...,landlock)"
            ),
            _ => format!("Landlock's ABI: {e}"),
        });
    }
    if abi < MIN_ABI {
        return Err(format!(
            "this kernel's Landlock is ABI v{abi}: shards needs v{MIN_ABI} (Linux 6.10) or \
             later, whose rules govern /dev/kvm's ioctls"
        ));
    }
    // Every filesystem right v5 knows: v1's, EXECUTE through MAKE_SYM, then REFER (v2),
    // TRUNCATE (v3) and IOCTL_DEV (v5); v6 to v8 add none; and RESOLVE_UNIX where the
    // kernel has it (v9), granted nowhere, as [`sockets`] refuses every connect on every
    // kernel. All of them are handled, so all are enforced: nothing asked for is left to a
    // kernel that cannot.
    let resolve_unix = if abi >= RESOLVE_UNIX_ABI { RESOLVE_UNIX } else { 0 };
    let fs = ((1u64 << 13) - 1) | REFER | TRUNCATE | IOCTL_DEV | resolve_unix;
    let attr = RulesetAttr {
        handled_access_fs: fs,
        handled_access_net: NET_BIND_TCP | NET_CONNECT_TCP,
        scoped: if abi >= 6 {
            SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL
        } else {
            0
        },
    };
    // The attribute's size as this ABI knows it: a v5 kernel refuses a longer one.
    let size = if abi >= 6 {
        std::mem::size_of::<RulesetAttr>()
    } else {
        16
    };
    // SAFETY: landlock_create_ruleset(2) reads `size` bytes of `attr`.
    let ruleset = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, &raw const attr, size, 0u32) };
    if ruleset < 0 {
        return Err(format!("Landlock's ruleset: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: a fresh descriptor nothing else owns.
    let ruleset = unsafe { OwnedFd::from_raw_fd(ruleset as i32) };
    let file_rights = EXECUTE | WRITE_FILE | READ_FILE | TRUNCATE | IOCTL_DEV;
    let allow = |path: &std::path::Path, rights: u64| -> Result<(), String> {
        // As opening it would report it: a missing kernel reads as one.
        let at = |e: std::io::Error| format!("{}: {e}", path.display());
        let name =
            std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| "a path with NUL".to_string())?;
        // SAFETY: open(2) of a NUL-terminated path, for a handle on its inode alone.
        let fd = unsafe { libc::open(name.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(at(std::io::Error::last_os_error()));
        }
        // SAFETY: a fresh descriptor nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let dir = std::fs::metadata(path).map_err(at)?.is_dir();
        let rule = PathBeneath {
            // A file takes only the rights a file has.
            allowed_access: if dir { rights } else { rights & file_rights },
            parent_fd: fd.as_raw_fd(),
        };
        // SAFETY: landlock_add_rule(2) reads the rule, whose descriptor is open.
        let r = unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                ruleset.as_raw_fd(),
                RULE_PATH_BENEATH,
                &raw const rule,
                0u32,
            )
        };
        if r < 0 {
            return Err(at(std::io::Error::last_os_error()));
        }
        Ok(())
    };
    let read = READ_FILE;
    let read_dir = READ_FILE | READ_DIR;
    let write = READ_FILE | WRITE_FILE | TRUNCATE;
    let write_dir = read_dir | WRITE_FILE | TRUNCATE | REMOVE_DIR | REMOVE_FILE | MAKE_DIR | MAKE_REG;
    // Its vsock socket, which its spawner binds for it, removed as its device goes.
    let sockets_dir = REMOVE_FILE;
    // The host's own files, where it has them: a host without /dev/kvm runs no VM, which
    // the KVM backend says itself, and a kernel built without transparent huge pages has
    // no settings for them. Nothing absent can be reached, rule or not.
    for (host, rights) in [
        ("/dev/kvm", write | IOCTL_DEV),
        ("/sys/kernel/mm/transparent_hugepage", read_dir),
    ] {
        let host = std::path::Path::new(host);
        if host.exists() {
            allow(host, rights)?;
        }
    }
    allow(std::path::Path::new("/dev/null"), write)?;
    // This process's /proc directory stays looked up while it lives, its rule's inode with
    // it: procfs makes a new inode for /proc/PID each time its dentry is made afresh
    // (fs/proc/base.c, proc_pid_lookup → proc_pid_make_inode, "We need a new inode"), a
    // live process's dentry unused goes as the host reclaims memory (pid_delete_dentry),
    // and Landlock holds a rule's inode, not its dentry (security/landlock/fs.c,
    // get_inode_object). So a rule made on the first inode no longer met the path: a
    // template's save, reading /proc/self/pagemap, was refused (EACCES) on a busy host, and
    // its pool never filled (Fedora, 6 of 55 rounds of the daemon tests). A descriptor of
    // it held for the process's life holds its dentry, from the first layer on.
    static PROC_SELF: std::sync::OnceLock<OwnedFd> = std::sync::OnceLock::new();
    if PROC_SELF.get().is_none() {
        let name = c"/proc/self";
        // SAFETY: open(2) of a NUL-terminated path, for a handle on its inode alone.
        let fd = unsafe { libc::open(name.as_ptr(), libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(format!("/proc/self: {}", std::io::Error::last_os_error()));
        }
        // SAFETY: a fresh descriptor nothing else owns.
        let _ = PROC_SELF.set(unsafe { OwnedFd::from_raw_fd(fd) });
    }
    allow(std::path::Path::new("/proc/self"), read_dir)?;
    for f in &paths.read {
        allow(f, read)?;
    }
    for d in &paths.read_under {
        allow(d, read_dir)?;
    }
    for f in &paths.write {
        allow(f, write)?;
    }
    for d in &paths.write_under {
        allow(d, write_dir)?;
    }
    for d in &paths.sockets_under {
        allow(d, sockets_dir)?;
    }
    for d in &paths.devices {
        allow(d, write | IOCTL_DEV)?;
    }
    Ok((ruleset, abi))
}

/// The kernel's Landlock ABI, as it says it (landlock_create_ruleset(2),
/// LANDLOCK_CREATE_RULESET_VERSION); below 0 where it has none.
#[cfg(target_os = "linux")]
fn landlock_abi() -> i64 {
    // SAFETY: landlock_create_ruleset(2) with no attribute asks only the ABI's version.
    unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<u8>(),
            0usize,
            1u32,
        )
    }
}

/// The first Landlock ABI that restricts every thread of a process at once
/// (`LANDLOCK_RESTRICT_SELF_TSYNC`, include/uapi/linux/landlock.h).
#[cfg(target_os = "linux")]
const TSYNC_ABI: i64 = 8;
/// landlock_restrict_self(2)'s flag for it.
#[cfg(target_os = "linux")]
const RESTRICT_SELF_TSYNC: u32 = 1 << 3;

/// How a Landlock layer reaches every thread of this process.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// One call with `TSYNC` (ABI v8), which gives every thread the caller's new domain
    /// atomically ("overrides the Landlock configuration of sibling threads").
    AtOnce,
    /// Each thread restricting itself, asked in turn ([`asked`]): what every ABI from v5
    /// can, as glibc makes a setuid(2) every thread's (nptl's `__nptl_setxid`, its
    /// SIGSETXID) and as Landlock's documentation says to before v8.
    Asked,
}

/// Restricts every thread of this process with `ruleset`, as `reach` says.
#[cfg(target_os = "linux")]
fn every_thread(ruleset: &std::os::fd::OwnedFd, reach: Reach) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    match reach {
        Reach::AtOnce => restrict(ruleset.as_raw_fd(), RESTRICT_SELF_TSYNC).map_err(|e| e.to_string()),
        Reach::Asked => asked::every_thread(ruleset.as_raw_fd()).map_err(|e| e.to_string()),
    }
}

/// landlock_restrict_self(2) of the calling thread (with `TSYNC`, of every thread).
/// Async-signal-safe: one syscall, its error read from errno.
#[cfg(target_os = "linux")]
fn restrict(ruleset: std::os::fd::RawFd, flags: u32) -> std::io::Result<()> {
    // SAFETY: landlock_restrict_self(2) on a descriptor the caller holds open.
    if unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset, flags) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Every thread of this process asked, one at a time, to restrict itself with a ruleset:
/// by a signal whose handler makes landlock_restrict_self(2) in the thread it interrupts,
/// and answers on an eventfd.
///
/// - The calling thread is restricted first. Then, until a listing of the process's threads
///   (`/proc/self/task`, which its own `/proc` rule lets it read) names none not settled,
///   each one named is asked. A thread started after its creator was restricted is
///   restricted (landlock_restrict_self(2): "and its future children"), and one started
///   before is named by the next listing, as its creator was by this one: when a listing
///   names no thread not settled, none is left unrestricted.
/// - Whether a thread can answer is read, not guessed: a thread that blocks the signal, or
///   a process that ignores it (`/proc/self/task/TID/status`, SigBlk and SigIgn), cannot,
///   and the layer fails at once, closed ([`Unasked::Deaf`]). One gone or exiting
///   (`stat`'s flags, PF_EXITING, include/linux/sched.h) is settled: it runs no more of
///   this process's code, and drops a signal sent to it alone (kernel/signal.c,
///   `exit_signals`).
/// - Any other is waited for as long as it lives, with no timeout: its answer, or its end,
///   which its thread pidfd says (pidfd_open(2), PIDFD_THREAD, Linux 6.9, older than the
///   Landlock ABI v5 a VM needs), wakes the wait. A thread that entered its exit as it was
///   asked never answers, and ends.
/// - A thread id is settled once: one reused by a new thread within one pass would need
///   the kernel's thread ids to wrap (pid_max) while the pass runs.
#[cfg(target_os = "linux")]
mod asked {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};

    /// The ruleset the asked thread restricts itself with.
    static RULESET: AtomicI32 = AtomicI32::new(-1);
    /// The eventfd its handler answers on.
    static ANSWERS: AtomicI32 = AtomicI32::new(-1);
    /// The thread asked: the only one the handler acts in, once.
    static ASKED: AtomicI32 = AtomicI32::new(0);
    /// Its answer: [`UNANSWERED`], then [`RESTRICTED`] or [`FAILED`] plus the error.
    static ANSWER: AtomicU32 = AtomicU32::new(UNANSWERED);
    const UNANSWERED: u32 = 0;
    const RESTRICTED: u32 = 1;
    const FAILED: u32 = 2;
    /// One pass at a time: its statics are one pass's.
    static PASS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Why a pass left a thread unrestricted, and so failed.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) enum Unasked {
        /// It blocks the signal, or the process ignores it: it cannot answer.
        Deaf { tid: i32, signal: libc::c_int },
        /// Its restriction failed with this error.
        Refused { tid: i32, errno: i32 },
        /// What the pass needed of the kernel failed.
        System { what: &'static str, error: String },
    }

    impl std::fmt::Display for Unasked {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Unasked::Deaf { tid, signal } => write!(
                    f,
                    "thread {tid} blocks or ignores signal {signal}: it cannot be asked to restrict itself"
                ),
                Unasked::Refused { tid, errno } => {
                    write!(f, "thread {tid}: {}", std::io::Error::from_raw_os_error(*errno))
                }
                Unasked::System { what, error } => write!(f, "{what}: {error}"),
            }
        }
    }

    fn system(what: &'static str) -> impl Fn(std::io::Error) -> Unasked {
        move |e| Unasked::System {
            what,
            error: e.to_string(),
        }
    }

    /// The signal that asks: SIGSYS, the one every thread of a filtered process takes, as
    /// its filter's trap is reported by it (`seccomp::name_refusals`), and the one Mesa's
    /// threads, which block every other for their lives, leave unblocked
    /// (src/util/u_thread.c, `u_thread_create`, at mirror/mesa 8c427aae9): a GPU's VM
    /// process has those threads by its seal. A SIGSYS this process sends one of its own
    /// threads (SI_TKILL) runs [`restrict_here`] there (`seccomp::on_sent`).
    pub(super) fn signal() -> libc::c_int {
        libc::SIGSYS
    }

    /// A thread, as `/proc/self/task/TID` shows it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Seen {
        /// Gone, or exiting: settled.
        Ended,
        /// It blocks the signal, or the process ignores it: it cannot answer.
        Deaf,
        /// It takes the signal: asked, and waited for.
        Hears,
    }

    /// What asking a thread came to.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Asked {
        Restricted,
        /// It ended before it could.
        Ended,
    }

    /// What ended a wait for an answer.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum Woken {
        Answered,
        /// The thread waited for ended.
        Ended,
    }

    /// What a SIGSYS this process sent a thread of its own runs there
    /// (`seccomp::on_sent`): in the asked thread alone, the restriction and its answer;
    /// errno kept for the code it interrupted. Async-signal-safe: atomics, prctl(2),
    /// landlock_restrict_self(2), write(2).
    extern "C" fn restrict_here() {
        // SAFETY: __errno_location(3) is this thread's errno, and gettid(2) has no
        // preconditions.
        let (errno, me) = unsafe { (*libc::__errno_location(), libc::syscall(libc::SYS_gettid) as i32) };
        if ASKED
            .compare_exchange(me, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            // SAFETY: prctl(2) with integer arguments: no_new_privs, which
            // landlock_restrict_self(2) needs of the thread, and which only takes privilege away.
            let nnp = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } == 0;
            let answer = match nnp.then(|| super::restrict(RULESET.load(Ordering::SeqCst), 0)) {
                Some(Ok(())) => RESTRICTED,
                Some(Err(e)) => FAILED.saturating_add(e.raw_os_error().unwrap_or(0).unsigned_abs()),
                None => FAILED.saturating_add(libc::EPERM.unsigned_abs()),
            };
            ANSWER.store(answer, Ordering::SeqCst);
            let one = 1u64;
            // SAFETY: write(2) of eight bytes of ours to the pass's eventfd.
            unsafe {
                libc::write(
                    ANSWERS.load(Ordering::SeqCst),
                    (&raw const one).cast(),
                    std::mem::size_of::<u64>(),
                )
            };
        }
        // SAFETY: as above.
        unsafe { *libc::__errno_location() = errno };
    }

    /// Has a SIGSYS this process sends run [`restrict_here`], its refusals' report
    /// installed for it where it is not yet: restarting what it interrupts where the kernel
    /// restarts it (SA_RESTART); every wait a VM process makes that it does not goes on
    /// past EINTR, as it does past the vCPUs' kicks.
    fn install() -> Result<(), Unasked> {
        shards_vmm::platform::seccomp::name_refusals().map_err(system("SIGSYS's handler"))?;
        shards_vmm::platform::seccomp::on_sent(restrict_here);
        Ok(())
    }

    /// The process's threads, by id.
    fn threads() -> Result<Vec<i32>, Unasked> {
        let at = system("/proc/self/task");
        let mut found = Vec::new();
        for entry in std::fs::read_dir("/proc/self/task").map_err(&at)? {
            if let Some(tid) = entry
                .map_err(&at)?
                .file_name()
                .to_str()
                .and_then(|n| n.parse().ok())
            {
                found.push(tid);
            }
        }
        Ok(found)
    }

    /// Thread `tid` as its `/proc` entry shows it: see [`Seen`].
    fn seen(tid: i32) -> Result<Seen, Unasked> {
        /// PF_EXITING (include/linux/sched.h).
        const EXITING: u64 = 0x4;
        let read = |file: &str| match std::fs::read_to_string(format!("/proc/self/task/{tid}/{file}")) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ESRCH) => {
                Ok(None)
            }
            Err(e) => Err(Unasked::System {
                what: "a thread's /proc entry",
                error: format!("{tid}/{file}: {e}"),
            }),
        };
        let unread = |what: &str| Unasked::System {
            what: "a thread's /proc entry",
            error: format!("{tid}: no {what}"),
        };
        let Some(stat) = read("stat")? else {
            return Ok(Seen::Ended);
        };
        // Its fields after the name, whose parentheses close last (proc_pid_stat(5)):
        // the state, then the parent, group, session, terminal, its group, and the flags.
        let flags = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(6))
            .and_then(|f| f.parse::<u64>().ok())
            .ok_or_else(|| unread("flags"))?;
        if flags & EXITING != 0 {
            return Ok(Seen::Ended);
        }
        let Some(status) = read("status")? else {
            return Ok(Seen::Ended);
        };
        let mask = |name: &str| {
            status
                .lines()
                .find_map(|l| l.strip_prefix(name))
                .and_then(|m| u64::from_str_radix(m.trim(), 16).ok())
                .ok_or_else(|| unread(name))
        };
        let bit = u32::try_from(signal() - 1)
            .ok()
            .and_then(|b| 1u64.checked_shl(b))
            .unwrap_or(0);
        let deaf = (mask("SigBlk:")? | mask("SigIgn:")?) & bit != 0;
        Ok(if deaf { Seen::Deaf } else { Seen::Hears })
    }

    /// Waits, with no timeout, until `answers` has an answer or `thread`, a thread's pidfd,
    /// says it ended.
    pub(super) fn wait(answers: &OwnedFd, thread: &OwnedFd) -> Result<Woken, Unasked> {
        loop {
            let mut polled = [answers.as_raw_fd(), thread.as_raw_fd()].map(|fd| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            });
            // SAFETY: ppoll(2) on two pollfds of descriptors held open, with no timeout and
            // no mask.
            let n = unsafe { libc::ppoll(polled.as_mut_ptr(), 2, std::ptr::null(), std::ptr::null()) };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(system("waiting for an answer")(e));
            }
            let [answered, ended] = polled;
            if answered.revents != 0 {
                let mut count = 0u64;
                // SAFETY: read(2) of an eventfd's eight-byte count into a u64 of ours.
                unsafe {
                    libc::read(
                        answers.as_raw_fd(),
                        (&raw mut count).cast(),
                        std::mem::size_of::<u64>(),
                    )
                };
                return Ok(Woken::Answered);
            }
            if ended.revents != 0 {
                return Ok(Woken::Ended);
            }
        }
    }

    /// Thread `tid`'s pidfd, or none where it is gone.
    pub(super) fn pidfd(tid: i32) -> Result<Option<OwnedFd>, Unasked> {
        // SAFETY: pidfd_open(2) of a thread (PIDFD_THREAD, include/uapi/linux/pidfd.h:
        // O_EXCL).
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, tid, libc::O_EXCL) };
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            return match e.raw_os_error() {
                Some(libc::ESRCH) => Ok(None),
                _ => Err(system("a thread's pidfd")(e)),
            };
        }
        // SAFETY: a fresh descriptor nothing else owns.
        Ok(Some(unsafe { OwnedFd::from_raw_fd(fd as i32) }))
    }

    /// Asks thread `tid` of process `pid` to restrict itself, answering on `answers`.
    fn ask(pid: i32, tid: i32, answers: &OwnedFd) -> Result<Asked, Unasked> {
        match seen(tid)? {
            Seen::Ended => return Ok(Asked::Ended),
            Seen::Deaf => {
                return Err(Unasked::Deaf {
                    tid,
                    signal: signal(),
                });
            }
            Seen::Hears => {}
        }
        let Some(thread) = pidfd(tid)? else {
            return Ok(Asked::Ended);
        };
        ANSWER.store(UNANSWERED, Ordering::SeqCst);
        ASKED.store(tid, Ordering::SeqCst);
        // SAFETY: tgkill(2) to a thread of this process.
        if unsafe { libc::syscall(libc::SYS_tgkill, pid, tid, signal()) } != 0 {
            let e = std::io::Error::last_os_error();
            ASKED.store(0, Ordering::SeqCst);
            return match e.raw_os_error() {
                Some(libc::ESRCH) => Ok(Asked::Ended),
                _ => Err(system("asking a thread")(e)),
            };
        }
        let woken = wait(answers, &thread)?;
        ASKED.store(0, Ordering::SeqCst);
        match (woken, ANSWER.load(Ordering::SeqCst)) {
            (_, RESTRICTED) => Ok(Asked::Restricted),
            (Woken::Ended, UNANSWERED) => Ok(Asked::Ended),
            (_, failed) => Err(Unasked::Refused {
                tid,
                errno: i32::try_from(failed.saturating_sub(FAILED)).unwrap_or(0),
            }),
        }
    }

    /// Restricts every thread of this process with `ruleset`.
    pub(super) fn every_thread(ruleset: std::os::fd::RawFd) -> Result<(), Unasked> {
        let _pass = PASS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        install()?;
        // SAFETY: eventfd(2) with no flags but close-on-exec.
        let answers = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        if answers < 0 {
            return Err(system("the answers' eventfd")(std::io::Error::last_os_error()));
        }
        // SAFETY: a fresh descriptor nothing else owns.
        let answers = unsafe { OwnedFd::from_raw_fd(answers) };
        // SAFETY: getpid(2) and gettid(2) have no preconditions.
        let (pid, me) = unsafe { (libc::getpid(), libc::syscall(libc::SYS_gettid) as i32) };
        super::restrict(ruleset, 0).map_err(system("landlock_restrict_self"))?;
        RULESET.store(ruleset, Ordering::SeqCst);
        ANSWERS.store(answers.as_raw_fd(), Ordering::SeqCst);
        let passed = settle(pid, me, &answers);
        ANSWERS.store(-1, Ordering::SeqCst);
        RULESET.store(-1, Ordering::SeqCst);
        passed
    }

    /// Asks every thread but `me` in turn until a listing names none not settled.
    fn settle(pid: i32, me: i32, answers: &OwnedFd) -> Result<(), Unasked> {
        let mut settled = vec![me];
        loop {
            let left: Vec<i32> = threads()?.into_iter().filter(|t| !settled.contains(t)).collect();
            if left.is_empty() {
                return Ok(());
            }
            for tid in left {
                ask(pid, tid, answers)?;
                settled.push(tid);
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A Vulkan manifest's libraries, by the `library_path` keys it has, each read as a JSON
    /// string: NVIDIA's driver manifest and Mesa's, a layers manifest of two, escapes, space
    /// around the colon; a key whose value is no string, and one cut short, name none.
    #[test]
    fn a_manifest_names_its_libraries() {
        let nvidia = r#"{"file_format_version" : "1.0.1", "ICD": {"library_path": "libGLX_nvidia.so.0", "api_version" : "1.4.351"}}"#;
        assert_eq!(library_paths(nvidia), ["libGLX_nvidia.so.0"]);
        let radeon = r#"{"ICD":{"api_version":"1.4.354","library_arch":"64","library_path":"\/usr\/lib\/libvulkan_radeon.so"},"file_format_version":"1.0.1"}"#;
        assert_eq!(library_paths(radeon), ["/usr/lib/libvulkan_radeon.so"]);
        let layers = r#"{"layers": [{"name": "A", "library_path" :  "./a.so"}, {"name": "B", "library_path": "C:\\b \"x\".so"}]}"#;
        assert_eq!(library_paths(layers), ["./a.so", r#"C:\b "x".so"#]);
        assert_eq!(
            library_paths(r#"{"library_path": 7, "x": "library_path"}"#),
            Vec::<String>::new()
        );
        assert_eq!(library_paths(r#"{"library_path": "lib.so"#), Vec::<String>::new());
        assert_eq!(library_paths("{}"), Vec::<String>::new());
    }

    /// udmabuf's request as Venus made it under Mesa's Intel driver, which the filter
    /// refused before it was allowed (seccomp's record: ioctl 0x40187542).
    #[test]
    fn udmabuf_create_is_the_request_venus_makes() {
        assert_eq!(UDMABUF_CREATE, 0x4018_7542);
    }

    /// A dma-buf's sync-file requests as linux/dma-buf.h defines them, the export as Mesa's
    /// Intel driver made it (seccomp's record: 0xffffffffc0086202, musl's `int` request
    /// sign-extended, whose low 32 bits the filter compares).
    #[test]
    fn dma_buf_sync_file_requests_are_the_kernels() {
        assert_eq!(DMA_BUF_IOCTL_EXPORT_SYNC_FILE, 0xc008_6202);
        assert_eq!(DMA_BUF_IOCTL_IMPORT_SYNC_FILE, 0x4008_6203);
    }

    /// In a child process (this test binary, told by `LANDLOCK_CHILD` where its files
    /// are): confines itself to one file to read, one directory to write in and one to make
    /// sockets in, then tries what a VM might be made to. Prints each outcome, and the ABI.
    fn landlock_child(dir: &str) -> ! {
        use std::io::Write as _;
        let dir = std::path::Path::new(dir);
        // SAFETY: landlock_create_ruleset(2) asking only the ABI's version.
        let abi = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<u8>(),
                0usize,
                1u32,
            )
        };
        let mut out = format!("abi: {abi}\n");
        let applied = landlock(&Paths {
            read: vec![dir.join("granted")],
            write_under: vec![dir.join("out")],
            sockets_under: vec![dir.join("socks")],
            ..Paths::default()
        });
        out.push_str(&format!("applied: {applied:?}\n"));
        let ok = |r: bool| if r { "ok" } else { "refused" };
        let tries = [
            ("read granted", std::fs::read(dir.join("granted")).is_ok()),
            ("read other", std::fs::read(dir.join("other")).is_ok()),
            (
                "write under",
                std::fs::write(dir.join("out").join("f"), b"x").is_ok(),
            ),
            (
                "make under",
                std::fs::create_dir(dir.join("out").join("d")).is_ok(),
            ),
            ("write other", std::fs::write(dir.join("other2"), b"x").is_ok()),
            ("read etc", std::fs::read("/etc/passwd").is_ok()),
            ("tcp", std::net::TcpListener::bind("127.0.0.1:0").is_ok()),
            (
                "socket in socks",
                std::os::unix::net::UnixListener::bind(dir.join("socks").join("s")).is_ok(),
            ),
            (
                "socket in out",
                std::os::unix::net::UnixListener::bind(dir.join("out").join("s")).is_ok(),
            ),
            (
                "file in socks",
                std::fs::write(dir.join("socks").join("f"), b"x").is_ok(),
            ),
            (
                "remove in socks",
                std::fs::remove_file(dir.join("socks").join("v")).is_ok(),
            ),
            (
                "move out of out",
                std::fs::rename(dir.join("out").join("f"), dir.join("socks").join("f")).is_ok(),
            ),
            (
                "missing path",
                landlock(&Paths {
                    read: vec![dir.join("not-there")],
                    ..Paths::default()
                })
                .is_ok(),
            ),
        ];
        for (what, r) in tries {
            out.push_str(&format!("{what}: {}\n", ok(r)));
        }
        let _ = std::io::stdout().write_all(out.as_bytes());
        std::process::exit(0)
    }

    /// In a child process: confined with two directories to write in, then sealed, as a VM
    /// that saved a snapshot seals it (vm_run.rs, `seal`): a second layer with the same
    /// paths but the snapshot's. Prints each outcome after.
    fn seal_child(dir: &str) -> ! {
        use std::io::Write as _;
        let dir = std::path::Path::new(dir);
        let first = Paths {
            read: vec![dir.join("granted")],
            write_under: vec![dir.join("out"), dir.join("snap")],
            ..Paths::default()
        };
        let mut kept = first.clone();
        kept.write_under.retain(|d| d != &dir.join("snap"));
        let mut out = format!(
            "first: {:?}\nbefore: {}\n",
            landlock(&first),
            std::fs::write(dir.join("snap").join("before"), b"x").is_ok()
        );
        out.push_str(&format!("sealed: {:?}\n", landlock(&kept)));
        let ok = |r: bool| if r { "ok" } else { "refused" };
        for (what, r) in [
            (
                "write snap",
                std::fs::write(dir.join("snap").join("after"), b"x").is_ok(),
            ),
            (
                "make in snap",
                std::fs::create_dir(dir.join("snap").join("d")).is_ok(),
            ),
            (
                "remove from snap",
                std::fs::remove_file(dir.join("snap").join("before")).is_ok(),
            ),
            (
                "read snap",
                std::fs::read(dir.join("snap").join("before")).is_ok(),
            ),
            (
                "write out",
                std::fs::write(dir.join("out").join("f"), b"x").is_ok(),
            ),
            ("read granted", std::fs::read(dir.join("granted")).is_ok()),
        ] {
            out.push_str(&format!("{what}: {}\n", ok(r)));
        }
        let _ = std::io::stdout().write_all(out.as_bytes());
        std::process::exit(0)
    }

    /// A second layer takes a directory away, reads and writes, and leaves the rest, as Landlock's
    /// layers each restrict the process further (Documentation/userspace-api/landlock.rst,
    /// "Layers of file path access rights"): what a VM that saved a template gives up
    /// before its run's workload starts (review 8.2).
    #[test]
    fn a_second_layer_seals_a_directory_and_keeps_the_rest() {
        if let Ok(dir) = std::env::var("SEAL_CHILD") {
            seal_child(&dir);
        }
        let dir_guard = shards_testdir::TempDir::new("seal").unwrap();
        let dir = dir_guard.join("seal");
        let _ = std::fs::remove_dir_all(&dir);
        for d in ["out", "snap"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        std::fs::write(dir.join("granted"), b"g").unwrap();
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "confine::tests::a_second_layer_seals_a_directory_and_keeps_the_rest",
                "--nocapture",
            ])
            .env("SEAL_CHILD", &dir)
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stdout).into_owned();
        let _ = std::fs::remove_dir_all(&dir);
        for line in [
            "first: Ok(())",
            "before: true",
            "sealed: Ok(())",
            "write snap: refused",
            "make in snap: refused",
            "remove from snap: refused",
            "read snap: refused",
            "write out: ok",
            "read granted: ok",
        ] {
            assert!(
                said.contains(line),
                "{line}\n{said}\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    /// A VM process confined by Landlock reads and writes its paths and nothing else, makes
    /// no socket and removes its vsock socket alone, moves nothing from one directory to
    /// another, has no TCP,
    /// and is refused a rule for a path not there (D30, PM M66). shards runs no VM on a
    /// kernel without ABI v5, and this test fails on one.
    /// In a child process (this test binary, told by `CONFINED_TERMINAL_CHILD`): a
    /// terminal's pair opened, then confined as a VM process is, it asks of the terminal
    /// what a VM asks of its own. Exits 0 where its filter allowed it all; 1 where a call
    /// failed, 2 where no terminal opened, 3 where it could not be confined. A call refused
    /// ends it by SIGSYS, the call named.
    fn terminal_child() -> ! {
        let (mut leader, mut follower) = (0, 0);
        // SAFETY: openpty(3) writing two descriptors into ints of ours, with no name,
        // settings or size given.
        let opened = unsafe {
            libc::openpty(
                &mut leader,
                &mut follower,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        } == 0;
        let status = if !opened {
            2
        } else if confine().is_err() {
            3
        } else {
            // SAFETY: isatty(3), tcgetattr(3) and tcsetattr(3) of the terminal just opened,
            // its settings read into, and set from, a termios of ours.
            let asked = unsafe {
                let mut settings: libc::termios = std::mem::zeroed();
                libc::isatty(follower) == 1
                    && libc::tcgetattr(follower, &mut settings) == 0
                    && libc::tcsetattr(follower, libc::TCSANOW, &settings) == 0
            };
            i32::from(!asked)
        };
        // SAFETY: exit_group(2), which the filter allows.
        unsafe { libc::syscall(libc::SYS_exit_group, status) };
        loop {
            std::hint::spin_loop();
        }
    }

    /// A VM process asks its terminal what its C library asks with, which its filter
    /// allows: TCGETS2 and TCSETS2 in glibc 2.43, TCGETS and TCSETS in older glibc,
    /// TIOCGWINSZ in musl's isatty (a filter without the first killed `run --kernel`'s VM on
    /// Fedora 44, glibc 2.43).
    #[test]
    fn a_confined_vm_asks_its_terminal_as_its_c_library_does() {
        if std::env::var_os("CONFINED_TERMINAL_CHILD").is_some() {
            terminal_child();
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "confine::tests::a_confined_vm_asks_its_terminal_as_its_c_library_does",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("CONFINED_TERMINAL_CHILD", "1")
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn landlock_confines_the_process_to_its_paths() {
        if let Ok(dir) = std::env::var("LANDLOCK_CHILD") {
            landlock_child(&dir);
        }
        let dir_guard = shards_testdir::TempDir::new("landlock").unwrap();
        let dir = dir_guard.join("landlock");
        let _ = std::fs::remove_dir_all(&dir);
        for d in ["out", "socks"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        std::fs::write(dir.join("granted"), b"g").unwrap();
        std::fs::write(dir.join("other"), b"o").unwrap();
        // A vsock socket its spawner bound, left for it to remove.
        drop(std::os::unix::net::UnixListener::bind(dir.join("socks").join("v")).unwrap());
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "confine::tests::landlock_confines_the_process_to_its_paths",
                "--nocapture",
            ])
            .env("LANDLOCK_CHILD", &dir)
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stdout).into_owned();
        let _ = std::fs::remove_dir_all(&dir);
        let why = || format!("{said}\n{}", String::from_utf8_lossy(&out.stderr));
        let abi: i64 = said
            .lines()
            .find_map(|l| l.strip_prefix("abi: "))
            .and_then(|a| a.trim().parse().ok())
            .unwrap_or_else(|| panic!("{}", why()));
        assert!(abi >= 5, "Landlock ABI {abi}: shards needs v5\n{}", why());
        for line in [
            "applied: Ok(())",
            "read granted: ok",
            "read other: refused",
            "write under: ok",
            "make under: ok",
            "write other: refused",
            "read etc: refused",
            "tcp: refused",
            "socket in socks: refused",
            "remove in socks: ok",
            "socket in out: refused",
            "file in socks: refused",
            "move out of out: refused",
            "missing path: refused",
        ] {
            assert!(said.contains(line), "{line}\n{}", why());
        }
    }

    /// Runs this test binary's `test` in a child process with `env` set, and gives its
    /// exit code, stdout and stderr.
    fn child_of(test: &str, env: &[(&str, &str)]) -> (Option<i32>, String, String) {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture", "--test-threads=1"])
            .envs(env.iter().copied())
            .output()
            .unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// Ends a child as a confined VM ends, by exit_group(2), which every filter allows.
    fn end(status: i32) -> ! {
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
        // SAFETY: exit_group(2).
        unsafe { libc::syscall(libc::SYS_exit_group, status) };
        loop {
            std::hint::spin_loop();
        }
    }

    /// The signals a test's thread blocks for its life.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Blocks {
        Nothing,
        /// Every one but SIGSYS and SIGSEGV, as Mesa's threads (src/util/u_thread.c).
        AsMesa,
        Everything,
    }

    /// A thread started before its process is confined, which on each word it is sent
    /// tries what it names and says whether it could. `deaf` blocks every signal first, as
    /// Mesa's threads do (u_thread_create).
    fn sibling(
        dir: std::path::PathBuf,
        blocks: Blocks,
    ) -> (
        std::sync::mpsc::Sender<&'static str>,
        std::sync::mpsc::Receiver<bool>,
    ) {
        let (ask, asked) = std::sync::mpsc::channel::<&'static str>();
        let (tell, told) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if blocks != Blocks::Nothing {
                // SAFETY: sigset operations on a local set; pthread_sigmask on this thread.
                unsafe {
                    let mut set: libc::sigset_t = std::mem::zeroed();
                    libc::sigfillset(&mut set);
                    if blocks == Blocks::AsMesa {
                        libc::sigdelset(&mut set, libc::SIGSYS);
                        libc::sigdelset(&mut set, libc::SIGSEGV);
                    }
                    libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
                }
                let _ = tell.send(true);
            }
            for what in asked {
                let could = match what {
                    "read other" => std::fs::read(dir.join("other")).is_ok(),
                    "write snap" => std::fs::write(
                        dir.join("snap")
                            .join(format!("{:?}", std::thread::current().id())),
                        b"x",
                    )
                    .is_ok(),
                    "write out" => std::fs::write(
                        dir.join("out").join(format!("{:?}", std::thread::current().id())),
                        b"x",
                    )
                    .is_ok(),
                    _ => false,
                };
                let _ = tell.send(could);
            }
        });
        (ask, told)
    }

    /// In a child process (`THREADS_CHILD` names its directory, `THREADS_REACH` how its
    /// layers reach its threads): three threads started before it is confined, and one
    /// between its first layer and its seal, as a VM's are (its exec port's, its vCPUs');
    /// then each tries what it must not reach. Prints each outcome.
    fn threads_child(dir: &str, reach: &str) -> ! {
        use std::io::Write as _;
        let dir = std::path::PathBuf::from(dir);
        let first = Paths {
            read: vec![dir.join("granted")],
            write_under: vec![dir.join("out"), dir.join("snap")],
            ..Paths::default()
        };
        let mut kept = first.clone();
        kept.write_under.retain(|d| d != &dir.join("snap"));
        let apply = |paths: &Paths| -> Result<(), String> {
            match reach {
                "asked" => {
                    let (ruleset, _) = ruleset(paths)?;
                    // SAFETY: prctl(2) with integer arguments.
                    unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
                    every_thread(&ruleset, Reach::Asked)
                }
                _ => landlock(paths),
            }
        };
        let mut siblings: Vec<_> = (0..3).map(|_| sibling(dir.clone(), Blocks::Nothing)).collect();
        // Under the filter of a host whose ABI asks each thread (v5), as a VM there is.
        if reach == "asked" {
            confine_as(5).unwrap();
        }
        let mut out = format!("first: {:?}\n", apply(&first));
        let tries = |siblings: &[(
            std::sync::mpsc::Sender<&'static str>,
            std::sync::mpsc::Receiver<bool>,
        )],
                     what: &'static str| {
            siblings
                .iter()
                .map(|(ask, told)| {
                    ask.send(what).unwrap();
                    if told.recv().unwrap() { "ok" } else { "refused" }
                })
                .collect::<Vec<_>>()
                .join(",")
        };
        out.push_str(&format!(
            "siblings read other: {}\n",
            tries(&siblings, "read other")
        ));
        siblings.push(sibling(dir.clone(), Blocks::Nothing));
        // And one as Mesa's start, before the seal: every signal blocked but SIGSYS and
        // SIGSEGV (u_thread_create), as a GPU's VM process has.
        let (ask, told) = sibling(dir.clone(), Blocks::AsMesa);
        told.recv().unwrap();
        siblings.push((ask, told));
        out.push_str(&format!(
            "siblings write snap: {}\n",
            tries(&siblings, "write snap")
        ));
        out.push_str(&format!("sealed: {:?}\n", apply(&kept)));
        out.push_str(&format!(
            "sealed siblings write snap: {}\n",
            tries(&siblings, "write snap")
        ));
        out.push_str(&format!(
            "sealed siblings write out: {}\n",
            tries(&siblings, "write out")
        ));
        let _ = std::io::stdout().write_all(out.as_bytes());
        end(0)
    }

    /// In a child process: a thread that blocks every signal and sleeps so, started before
    /// it is confined by asking each thread: the layer fails, closed, naming the thread,
    /// where one made at once (TSYNC) reaches it.
    fn deaf_child(dir: &str, reach: &str) -> ! {
        use std::io::Write as _;
        let dir = std::path::PathBuf::from(dir);
        let paths = Paths {
            read: vec![dir.join("granted")],
            ..Paths::default()
        };
        let (ask, told) = sibling(dir.clone(), Blocks::Everything);
        // Its signals blocked.
        told.recv().unwrap();
        // SAFETY: prctl(2) with integer arguments.
        unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
        let applied = match reach {
            "asked" => ruleset(&paths).and_then(|(ruleset, _)| every_thread(&ruleset, Reach::Asked)),
            _ => ruleset(&paths).and_then(|(ruleset, _)| every_thread(&ruleset, Reach::AtOnce)),
        };
        ask.send("read other").unwrap();
        let read = told.recv().unwrap();
        let _ = std::io::stdout()
            .write_all(format!("applied: {applied:?}\ndeaf read other: {read}\n").as_bytes());
        end(0)
    }

    /// Every thread of a confined process is confined, the ones it had before its layer
    /// among them, and a seal reaches every thread too: at once where the ABI has TSYNC
    /// (v8), and on every ABI by each thread asked (platform-01).
    #[test]
    fn every_thread_of_a_confined_process_is_confined() {
        if let (Ok(dir), Ok(reach)) = (std::env::var("THREADS_CHILD"), std::env::var("THREADS_REACH")) {
            threads_child(&dir, &reach);
        }
        let abi = {
            // SAFETY: landlock_create_ruleset(2) asking only the ABI's version.
            unsafe {
                libc::syscall(
                    libc::SYS_landlock_create_ruleset,
                    std::ptr::null::<u8>(),
                    0usize,
                    1u32,
                )
            }
        };
        let mut reaches = vec!["asked"];
        if abi >= TSYNC_ABI {
            reaches.push("landlock");
        }
        for reach in reaches {
            let dir_guard = shards_testdir::TempDir::new("threads").unwrap();
            let dir = dir_guard.join("threads");
            for d in ["out", "snap"] {
                std::fs::create_dir_all(dir.join(d)).unwrap();
            }
            std::fs::write(dir.join("granted"), b"g").unwrap();
            std::fs::write(dir.join("other"), b"o").unwrap();
            let (code, said, err) = child_of(
                "confine::tests::every_thread_of_a_confined_process_is_confined",
                &[("THREADS_CHILD", dir.to_str().unwrap()), ("THREADS_REACH", reach)],
            );
            assert_eq!(code, Some(0), "{reach}: {said}\n{err}");
            for line in [
                "first: Ok(())",
                "siblings read other: refused,refused,refused\n",
                "siblings write snap: ok,ok,ok,ok,ok\n",
                "sealed: Ok(())",
                "sealed siblings write snap: refused,refused,refused,refused,refused\n",
                "sealed siblings write out: ok,ok,ok,ok,ok\n",
            ] {
                assert!(said.contains(line), "{reach}: {line}\n{said}\n{err}");
            }
        }
    }

    /// A thread that blocks the asking signal as it waits cannot be asked: the layer fails,
    /// closed, rather than leave it unrestricted or wait on it for ever; TSYNC reaches it.
    #[test]
    fn a_thread_that_cannot_be_asked_fails_the_layer() {
        if let (Ok(dir), Ok(reach)) = (std::env::var("DEAF_CHILD"), std::env::var("DEAF_REACH")) {
            deaf_child(&dir, &reach);
        }
        let abi = {
            // SAFETY: landlock_create_ruleset(2) asking only the ABI's version.
            unsafe {
                libc::syscall(
                    libc::SYS_landlock_create_ruleset,
                    std::ptr::null::<u8>(),
                    0usize,
                    1u32,
                )
            }
        };
        let dir_guard = shards_testdir::TempDir::new("deaf").unwrap();
        let dir = dir_guard.join("deaf");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("granted"), b"g").unwrap();
        std::fs::write(dir.join("other"), b"o").unwrap();
        let test = "confine::tests::a_thread_that_cannot_be_asked_fails_the_layer";
        let (code, said, err) = child_of(
            test,
            &[("DEAF_CHILD", dir.to_str().unwrap()), ("DEAF_REACH", "asked")],
        );
        assert_eq!(code, Some(0), "{said}\n{err}");
        assert!(
            said.contains("applied: Err(") && said.contains("blocks or ignores signal"),
            "{said}\n{err}"
        );
        if abi >= TSYNC_ABI {
            let (code, said, err) = child_of(
                test,
                &[("DEAF_CHILD", dir.to_str().unwrap()), ("DEAF_REACH", "tsync")],
            );
            assert_eq!(code, Some(0), "{said}\n{err}");
            assert!(
                said.contains("applied: Ok(())\ndeaf read other: false"),
                "{said}\n{err}"
            );
        }
    }

    /// In a child process (`SOCKETS_CHILD` names its directory, `SOCKETS_LAYERS` what
    /// confines it): two listeners its user made before, one beside the vsock path it is
    /// given and one elsewhere; it is confined, then dials each, and makes a datagram
    /// socket last, which its filter refuses by ending it.
    fn sockets_child(dir: &str, layers: &str) -> ! {
        use std::io::Write as _;
        let dir = std::path::PathBuf::from(dir);
        let paths = Paths {
            sockets_under: if layers.contains("socks") {
                vec![dir.join("socks")]
            } else {
                Vec::new()
            },
            ..Paths::default()
        };
        let _outside = std::os::unix::net::UnixListener::bind(dir.join("engine.sock")).unwrap();
        let _beside = std::os::unix::net::UnixListener::bind(dir.join("socks").join("v_5")).unwrap();
        let mut out = format!("filter: {:?}\n", confine());
        if layers.contains("seccomp") {
            out.push_str(&format!("seccomp: {:?}\n", sockets()));
        }
        if layers.contains("landlock") {
            out.push_str(&format!("landlock: {:?}\n", landlock(&paths)));
        }
        let dial = |at: std::path::PathBuf| match std::os::unix::net::UnixStream::connect(at) {
            Ok(_) => "ok".to_string(),
            Err(e) => format!("refused {}", e.raw_os_error().unwrap_or(0)),
        };
        out.push_str(&format!("dial outside: {}\n", dial(dir.join("engine.sock"))));
        out.push_str(&format!("dial beside: {}\n", dial(dir.join("socks").join("v_5"))));
        let _ = std::io::stdout().write_all(out.as_bytes());
        let _ = std::io::stdout().flush();
        // SAFETY: socket(2) with integer arguments.
        let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM, 0) };
        let _ = std::io::stdout().write_all(format!("datagram: {fd}\n").as_bytes());
        end(0)
    }

    /// A confined VM dials no socket file of its user's, its daemon's Engine API among
    /// them, whether or not it was given a vsock path, beside which its spawner dials for
    /// it: refused by its seccomp layer on every kernel, and by Landlock's v9 rules too; a
    /// datagram socket, which could send to any, is never made (platform-02).
    #[test]
    fn a_confined_vm_dials_only_beside_its_vsock_path() {
        if let (Ok(dir), Ok(layers)) = (std::env::var("SOCKETS_CHILD"), std::env::var("SOCKETS_LAYERS")) {
            sockets_child(&dir, &layers);
        }
        let refused = format!("refused {}", libc::EACCES);
        let refused = Some(refused.as_str());
        // The seccomp layer alone, as on a kernel before v9, and everything, as a VM is
        // confined: a datagram socket ends it.
        let mut cases = vec![
            ("seccomp", refused, refused, true),
            ("seccomp socks", refused, refused, true),
            ("seccomp landlock", refused, refused, true),
            ("seccomp landlock socks", refused, refused, true),
        ];
        if landlock_abi() >= 9 {
            // Landlock's rules alone (v9), its extra layer.
            cases.push(("landlock", refused, refused, false));
            cases.push(("landlock socks", refused, refused, false));
        }
        let test = "confine::tests::a_confined_vm_dials_only_beside_its_vsock_path";
        for (layers, outside, beside, datagram_refused) in cases {
            let dir_guard = shards_testdir::TempDir::new("socks").unwrap();
            let dir = dir_guard.join("s");
            std::fs::create_dir_all(dir.join("socks")).unwrap();
            let (code, said, err) = child_of(
                test,
                &[
                    ("SOCKETS_CHILD", dir.to_str().unwrap()),
                    ("SOCKETS_LAYERS", layers),
                ],
            );
            let why = format!("{layers}: {said}\n{err}");
            assert!(!said.contains("Err("), "{why}");
            if let Some(outside) = outside {
                assert!(said.contains(&format!("dial outside: {outside}\n")), "{why}");
            }
            if let Some(beside) = beside {
                assert!(said.contains(&format!("dial beside: {beside}\n")), "{why}");
            }
            if datagram_refused {
                assert_eq!(code, Some(shards_vmm::platform::seccomp::REFUSED), "{why}");
                assert!(err.contains(&format!("syscall {}", libc::SYS_socket)), "{why}");
            } else {
                assert_eq!(code, Some(0), "{why}");
            }
        }
    }

    /// The wait for a thread's answer ends with its answer, or with its end where it ends
    /// unanswered (its thread pidfd): it has no timeout, and never outwaits a thread.
    #[test]
    fn a_wait_for_an_answer_ends_with_the_answer_or_the_thread() {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        let eventfd = || {
            // SAFETY: eventfd(2), close-on-exec; a fresh descriptor owned once.
            unsafe { OwnedFd::from_raw_fd(libc::eventfd(0, libc::EFD_CLOEXEC)) }
        };
        for answered in [false, true] {
            let (go, gone) = std::sync::mpsc::channel::<()>();
            let (said, tid) = std::sync::mpsc::channel();
            let thread = std::thread::spawn(move || {
                // SAFETY: gettid(2).
                said.send(unsafe { libc::syscall(libc::SYS_gettid) } as i32)
                    .unwrap();
                let _ = gone.recv();
            });
            let tid = tid.recv().unwrap();
            let pidfd = asked::pidfd(tid).unwrap().unwrap();
            let answers = eventfd();
            let (woke, woken) = std::sync::mpsc::channel();
            let waiting = {
                let answers = answers.try_clone().unwrap();
                std::thread::spawn(move || woke.send(asked::wait(&answers, &pidfd)).unwrap())
            };
            if answered {
                let one = 1u64;
                // SAFETY: write(2) of eight bytes of ours to the eventfd.
                unsafe { libc::write(answers.as_raw_fd(), (&raw const one).cast(), 8) };
            } else {
                go.send(()).unwrap();
            }
            let got = woken.recv_timeout(std::time::Duration::from_secs(60));
            let want = if answered {
                asked::Woken::Answered
            } else {
                asked::Woken::Ended
            };
            assert_eq!(got, Ok(Ok(want)), "answered: {answered}");
            drop(go);
            thread.join().unwrap();
            waiting.join().unwrap();
        }
    }

    /// Its own `RLIMIT_NOFILE`, as /proc/PID/limits says it: soft and hard.
    fn open_files_limit(pid: u32) -> String {
        std::fs::read_to_string(format!("/proc/{pid}/limits"))
            .unwrap()
            .lines()
            .find(|l| l.starts_with("Max open files"))
            .unwrap()
            .to_string()
    }

    /// In a child process (`PRLIMIT_CHILD` names a disposable process of its user's):
    /// confined, it reads and sets its own limits, then tries to take its target's.
    fn prlimit_child(target: &str) -> ! {
        use std::io::Write as _;
        let target: i32 = target.parse().unwrap();
        let confined = confine();
        let mut own = libc::rlimit64 {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: prlimit64(2) reading this process's limit into a local.
        let read = unsafe {
            libc::syscall(
                libc::SYS_prlimit64,
                0,
                libc::RLIMIT_NOFILE,
                std::ptr::null::<libc::rlimit64>(),
                &mut own,
            )
        };
        // SAFETY: prlimit64(2) setting this process's limit to what it is.
        let set = unsafe {
            libc::syscall(
                libc::SYS_prlimit64,
                libc::getpid(),
                libc::RLIMIT_NOFILE,
                &own,
                std::ptr::null_mut::<libc::rlimit64>(),
            )
        };
        let _ =
            std::io::stdout().write_all(format!("confined: {confined:?}\nown: {read} {set}\n").as_bytes());
        let _ = std::io::stdout().flush();
        let zero = libc::rlimit64 {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: prlimit64(2) of another process, from a local: what the filter must refuse.
        let other = unsafe {
            libc::syscall(
                libc::SYS_prlimit64,
                target,
                libc::RLIMIT_NOFILE,
                &zero,
                std::ptr::null_mut::<libc::rlimit64>(),
            )
        };
        let _ = std::io::stdout().write_all(format!("other: {other}\n").as_bytes());
        end(0)
    }

    /// A confined VM reads and sets its own limits, as its C library does, and cannot
    /// change another process's: its filter refuses prlimit64(2) of any pid but 0 and its
    /// own, and its target keeps its limits (platform-04).
    #[test]
    fn a_confined_vm_sets_only_its_own_limits() {
        if std::env::var_os("PRLIMIT_TARGET").is_some() {
            let _ = std::io::Read::read_to_end(&mut std::io::stdin(), &mut Vec::new());
            std::process::exit(0);
        }
        if let Ok(target) = std::env::var("PRLIMIT_CHILD") {
            prlimit_child(&target);
        }
        let test = "confine::tests::a_confined_vm_sets_only_its_own_limits";
        let mut target = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env("PRLIMIT_TARGET", "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let before = open_files_limit(target.id());
        let (code, said, err) = child_of(test, &[("PRLIMIT_CHILD", &target.id().to_string())]);
        let after = open_files_limit(target.id());
        drop(target.stdin.take());
        let _ = target.wait();
        let why = format!("{said}\n{err}");
        assert!(said.contains("confined: Ok(())\nown: 0 0\n"), "{why}");
        assert_eq!(before, after, "its target's limits changed\n{why}");
        assert_eq!(code, Some(shards_vmm::platform::seccomp::REFUSED), "{why}");
        assert!(err.contains(&format!("syscall {}", libc::SYS_prlimit64)), "{why}");
    }
}
