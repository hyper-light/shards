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
//! Linux on x86_64, where the KVM backend is; elsewhere nothing is installed yet.

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub fn confine() -> Result<(), String> {
    use shards_vmm::platform::seccomp::{self, Rule};

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
        libc::SYS_access,
        libc::SYS_faccessat2,
        libc::SYS_getdents64,
        libc::SYS_mkdir,
        libc::SYS_rename,
        libc::SYS_renameat,
        libc::SYS_unlink,
        libc::SYS_unlinkat,
        libc::SYS_readlink,
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
        libc::SYS_brk,
        libc::SYS_futex,
        libc::SYS_clock_nanosleep,
        libc::SYS_nanosleep,
        libc::SYS_clock_gettime,
        libc::SYS_sched_yield,
        libc::SYS_sched_getaffinity,
        libc::SYS_getrandom,
        libc::SYS_getpid,
        libc::SYS_gettid,
        libc::SYS_getppid,
        libc::SYS_getuid,
        libc::SYS_geteuid,
        libc::SYS_getgid,
        libc::SYS_getegid,
        libc::SYS_prlimit64,
        libc::SYS_rt_sigaction,
        libc::SYS_rt_sigprocmask,
        libc::SYS_rt_sigreturn,
        libc::SYS_rt_sigtimedwait,
        libc::SYS_sigaltstack,
        libc::SYS_restart_syscall,
        libc::SYS_set_robust_list,
        libc::SYS_set_tid_address,
        libc::SYS_rseq,
        libc::SYS_arch_prctl,
        libc::SYS_exit,
        libc::SYS_exit_group,
        libc::SYS_tgkill,
        libc::SYS_poll,
        libc::SYS_ppoll,
        libc::SYS_pipe2,
        libc::SYS_dup,
        libc::SYS_dup2,
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
    ];
    let mut rules: Vec<Rule> = any.into_iter().map(Rule::any).collect();
    // The requests' low 32 bits: the kernel's `cmd` is an unsigned int.
    let mut requests: Vec<u32> = shards_vmm::hv::IOCTLS.iter().map(|&r| r as u32).collect();
    requests.extend([libc::FIONBIO as u32, libc::TCGETS as u32, libc::TCSETS as u32]);
    rules.push(Rule::with(libc::SYS_ioctl, 1, &requests));
    let commands = [
        libc::F_DUPFD,
        libc::F_DUPFD_CLOEXEC,
        libc::F_GETFD,
        libc::F_SETFD,
        libc::F_GETFL,
        libc::F_SETFL,
    ];
    rules.push(Rule::with(libc::SYS_fcntl, 1, &commands.map(|c| c as u32)));
    rules.push(Rule::with(libc::SYS_socket, 0, &[libc::AF_UNIX as u32]));
    rules.push(Rule::with(libc::SYS_socketpair, 0, &[libc::AF_UNIX as u32]));
    rules.push(Rule::with(
        libc::SYS_prctl,
        0,
        &[libc::PR_SET_NAME as u32, libc::PR_GET_NAME as u32],
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

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub fn confine() -> Result<(), String> {
    Ok(())
}
