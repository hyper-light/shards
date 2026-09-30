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
        // musl's `open`, `stat` and `lstat`, where glibc's make the *at calls.
        libc::SYS_open,
        libc::SYS_stat,
        libc::SYS_lstat,
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
    // `isatty` asks TCGETS in glibc and TIOCGWINSZ in musl.
    requests.extend([
        libc::FIONBIO as u32,
        libc::TCGETS as u32,
        libc::TCSETS as u32,
        libc::TIOCGWINSZ as u32,
    ]);
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

/// The files and sockets a VM process may use on macOS, for its Seatbelt profile (D30):
/// all it is denied besides. Paths are resolved (`/tmp` is `/private/tmp` to Seatbelt,
/// which matches the paths of the files themselves); one not there yet, by its parent.
#[derive(Debug, Default)]
pub struct Paths {
    /// Files it reads.
    pub read: Vec<std::path::PathBuf>,
    /// Directories whose files it reads.
    pub read_under: Vec<std::path::PathBuf>,
    /// Files it reads and writes.
    pub write: Vec<std::path::PathBuf>,
    /// Directories it may make, and in which it reads and writes anything.
    pub write_under: Vec<std::path::PathBuf>,
}

/// `path` as Seatbelt sees it: resolved, or its parent resolved if it is not there yet.
// macOS applies it; the others are confined without paths.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn resolved(path: &std::path::Path) -> std::path::PathBuf {
    if let Ok(real) = std::fs::canonicalize(path) {
        return real;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => resolved(parent).join(name),
        _ => path.to_path_buf(),
    }
}

/// An SBPL string literal: its backslashes and quotes escaped.
// macOS applies it; the others are confined without paths.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn literal(path: &std::path::Path) -> String {
    let text = path.to_string_lossy();
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        if matches!(c, '"' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// The profile: nothing but sysctl reads, signals to itself, `/dev/null`, files'
/// metadata, and `paths` (docs/research/platform-measurements.md M52). Metadata, since
/// resolving a path stats each directory above it: it says which files are there, not
/// what they hold. Unix sockets are files to Seatbelt: a VM binds and dials its vsock
/// sockets in a directory it writes in.
// macOS applies it; the others are confined without paths.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn profile(paths: &Paths) -> String {
    let mut p = String::from(
        "(version 1)\n(deny default)\n(allow sysctl-read)\n(allow signal (target self))\n\
         (allow file-read-metadata)\n(allow file-read* file-write* (literal \"/dev/null\"))\n\
         (allow mach-lookup (global-name \"com.apple.diagnosticd\"))\n",
    );
    let rule = |allow: &str, filter: &str, path: &std::path::Path| {
        format!("(allow {allow} ({filter} {}))\n", literal(&resolved(path)))
    };
    for f in &paths.read {
        p.push_str(&rule("file-read*", "literal", f));
    }
    for d in &paths.read_under {
        p.push_str(&rule("file-read*", "subpath", d));
    }
    for f in &paths.write {
        p.push_str(&rule("file-read* file-write*", "literal", f));
    }
    for d in &paths.write_under {
        p.push_str(&rule("file-read* file-write*", "subpath", d));
        p.push_str(&rule(
            "network-bind network-outbound network-inbound",
            "subpath",
            d,
        ));
        // Its ancestors not there yet, which making it makes first: made, nothing more.
        let mut above = d.parent();
        while let Some(a) = above.filter(|a| !a.as_os_str().is_empty() && !a.exists()) {
            p.push_str(&rule("file-write-create", "literal", a));
            above = a.parent();
        }
    }
    p
}

/// Applies `paths`' profile to this process, for good (sandbox_init(3)).
#[cfg(target_os = "macos")]
pub fn seatbelt(paths: &Paths) -> Result<(), String> {
    unsafe extern "C" {
        fn sandbox_init(profile: *const std::ffi::c_char, flags: u64, err: *mut *mut std::ffi::c_char)
        -> i32;
        fn sandbox_free_error(err: *mut std::ffi::c_char);
    }
    shards_vmm::debug!("its sandbox:\n{}", profile(paths));
    let profile = std::ffi::CString::new(profile(paths)).map_err(|_| "a path with NUL".to_string())?;
    let mut err: *mut std::ffi::c_char = std::ptr::null_mut();
    // SAFETY: sandbox_init(3) reads a NUL-terminated profile and may set `err` to a
    // message it allocated, which is freed below.
    let r = unsafe { sandbox_init(profile.as_ptr(), 0, &mut err) };
    if r == 0 {
        return Ok(());
    }
    let why = if err.is_null() {
        String::from("no reason given")
    } else {
        // SAFETY: a NUL-terminated message sandbox_init allocated, freed once read.
        let why = unsafe { std::ffi::CStr::from_ptr(err) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: as above.
        unsafe { sandbox_free_error(err) };
        why
    };
    Err(format!("the sandbox: {why}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In a child process (this test binary, told by `SEATBELT_CHILD` where its files
    /// are): applies a profile granting one file to read and one directory to write, then
    /// tries what a VM might be made to. Prints each outcome and how long the profile took.
    #[cfg(target_os = "macos")]
    fn seatbelt_child(dir: &str) -> ! {
        use std::io::Write as _;
        let dir = std::path::Path::new(dir);
        let t0 = std::time::Instant::now();
        seatbelt(&Paths {
            read: vec![dir.join("granted")],
            write_under: vec![dir.join("out")],
            ..Paths::default()
        })
        .unwrap();
        let took = t0.elapsed();
        let ok = |r: bool| if r { "ok" } else { "refused" };
        let mut out = String::new();
        out.push_str(&format!(
            "read granted: {}\n",
            ok(std::fs::read(dir.join("granted")).is_ok())
        ));
        out.push_str(&format!(
            "read other: {}\n",
            ok(std::fs::read(dir.join("other")).is_ok())
        ));
        out.push_str(&format!(
            "write under: {}\n",
            ok(std::fs::write(dir.join("out").join("f"), b"x").is_ok())
        ));
        out.push_str(&format!(
            "write other: {}\n",
            ok(std::fs::write(dir.join("other2"), b"x").is_ok())
        ));
        let listener = std::net::TcpListener::bind("127.0.0.1:0");
        out.push_str(&format!("tcp: {}\n", ok(listener.is_ok())));
        out.push_str(&format!("took_us: {}\n", took.as_micros()));
        let _ = std::io::stdout().write_all(out.as_bytes());
        std::process::exit(0)
    }

    /// A VM process confined to its paths reads and writes those, and nothing else, and
    /// has no network (D30).
    #[cfg(target_os = "macos")]
    #[test]
    fn a_profile_confines_the_process_to_its_paths() {
        if let Ok(dir) = std::env::var("SEATBELT_CHILD") {
            seatbelt_child(&dir);
        }
        let dir = std::env::temp_dir().join(format!("shards-seatbelt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("out")).unwrap();
        std::fs::write(dir.join("granted"), b"g").unwrap();
        std::fs::write(dir.join("other"), b"o").unwrap();
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "confine::tests::a_profile_confines_the_process_to_its_paths",
                "--nocapture",
            ])
            .env("SEATBELT_CHILD", &dir)
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stdout);
        let _ = std::fs::remove_dir_all(&dir);
        for line in [
            "read granted: ok",
            "read other: refused",
            "write under: ok",
            "write other: refused",
            "tcp: refused",
        ] {
            assert!(
                said.contains(line),
                "{line}\n{said}\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let took = said
            .lines()
            .find_map(|l| l.strip_prefix("took_us: "))
            .unwrap_or("?");
        let _ = std::io::Write::write_all(
            &mut std::io::stderr(),
            format!("sandbox_init took {took} µs\n").as_bytes(),
        );
    }

    #[test]
    fn paths_are_quoted_and_resolved_as_seatbelt_matches_them() {
        assert_eq!(literal(std::path::Path::new(r#"/a "b"\c"#)), r#""/a \"b\"\\c""#);
        let tmp = std::env::temp_dir();
        let real = std::fs::canonicalize(&tmp).unwrap_or(tmp.clone());
        assert_eq!(resolved(&tmp.join("not-there-yet")), real.join("not-there-yet"));
        let p = profile(&Paths {
            read: vec![tmp.join("k")],
            write_under: vec![tmp.join("d")],
            ..Paths::default()
        });
        assert!(p.starts_with("(version 1)\n(deny default)\n"), "{p}");
        assert!(
            p.contains(&format!(
                "(allow file-read* (literal {}))",
                literal(&real.join("k"))
            )),
            "{p}"
        );
        assert!(
            p.contains(&format!(
                "(allow file-read* file-write* (subpath {}))",
                literal(&real.join("d"))
            )),
            "{p}"
        );
        // A directory whose ancestors are not there yet: they may be made, nothing more.
        let deep = profile(&Paths {
            write_under: vec![tmp.join("not-there").join("nor-this").join("d")],
            ..Paths::default()
        });
        for missing in [real.join("not-there"), real.join("not-there").join("nor-this")] {
            let rule = format!("(allow file-write-create (literal {}))", literal(&missing));
            assert!(deep.contains(&rule), "{deep}");
        }
        assert!(
            !deep.contains(&format!("(literal {})", literal(&real))),
            "an ancestor that is there granted: {deep}"
        );
    }
}
