//! Seccomp filters: allow-lists of syscalls, some with the values an argument may take,
//! compiled to classic BPF over `struct seccomp_data` and installed with
//! `seccomp(SECCOMP_SET_MODE_FILTER)` after `PR_SET_NO_NEW_PRIVS`
//! (linux: Documentation/userspace-api/seccomp_filter.rst; seccomp(2)). The design is
//! Firecracker's: allow-lists checked against the architecture first, refusing by a trap
//! whose handler names what was refused (docs/research/rootless-security.md §2.4, R3).
//!
//! A program checks the architecture, loads the syscall number, and compares it with each
//! rule in turn; a rule's block allows it, or loads the argument and compares it with each
//! value. Arguments are compared in their low 32 bits: the ones filtered are `int` or
//! `unsigned int` to the kernel (ioctl's `cmd`, socket's `domain`, fcntl's `cmd`,
//! prctl's `option`), and a C library may pass them sign-extended (musl's `ioctl` takes
//! an `int` request).

use std::io;

/// `struct seccomp_data`'s fields (include/uapi/linux/seccomp.h).
const NR: u32 = 0;
const ARCH: u32 = 4;
const fn arg_low(index: u32) -> u32 {
    16 + 8 * index
}

/// `AUDIT_ARCH_*`: the ELF machine, 64-bit, little-endian (include/uapi/linux/audit.h).
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 62 | 0x8000_0000 | 0x4000_0000;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 183 | 0x8000_0000 | 0x4000_0000;

const LD_W_ABS: u16 = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
const JEQ_K: u16 = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
const JA: u16 = (libc::BPF_JMP | libc::BPF_JA) as u16;
const RET_K: u16 = (libc::BPF_RET | libc::BPF_K) as u16;

/// The exit status of a process a filter refused: 128 plus SIGSYS, as a shell reports a
/// process SIGSYS ended.
pub const REFUSED: i32 = 128 + libc::SIGSYS;

/// A syscall a filter allows: always, or when its argument `arg` is one of `values`; or,
/// with `errno`, one it answers with that error instead of trapping, as Firecracker
/// answers `clone3` with ENOSYS so that the C library falls back to `clone`, whose flags a
/// filter can read.
#[derive(Debug, Clone)]
pub struct Rule {
    pub syscall: libc::c_long,
    pub arg: Allowed,
    pub errno: Option<u16>,
}

impl Rule {
    pub fn any(syscall: libc::c_long) -> Rule {
        Rule {
            syscall,
            arg: None,
            errno: None,
        }
    }

    pub fn with(syscall: libc::c_long, arg: u32, values: &[u32]) -> Rule {
        Rule {
            syscall,
            arg: Some((arg, values.to_vec())),
            errno: None,
        }
    }

    pub fn fails(syscall: libc::c_long, errno: u16) -> Rule {
        Rule {
            syscall,
            arg: None,
            errno: Some(errno),
        }
    }
}

/// Values a rule allows for its syscall's argument: which argument, and the values.
type Allowed = Option<(u32, Vec<u32>)>;

/// A compiled filter.
#[derive(Debug, Clone)]
pub struct Filter(Vec<libc::sock_filter>);

fn op(code: u16, jt: u8, jf: u8, k: u32) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

/// Compiles `rules`: what they allow is allowed, and anything else traps. Rules for one
/// syscall add up. A syscall's values are at most 253, what one block's forward jumps
/// reach.
pub fn compile(rules: &[Rule]) -> Result<Filter, String> {
    let mut merged: Vec<(libc::c_long, Allowed)> = Vec::new();
    let mut failing: Vec<(libc::c_long, u16)> = Vec::new();
    for rule in rules {
        if let Some(errno) = rule.errno {
            if merged.iter().any(|(s, _)| *s == rule.syscall)
                || failing.iter().any(|(s, _)| *s == rule.syscall)
            {
                return Err(format!("syscall {}: allowed and failed", rule.syscall));
            }
            failing.push((rule.syscall, errno));
            continue;
        }
        if failing.iter().any(|(s, _)| *s == rule.syscall) {
            return Err(format!("syscall {}: allowed and failed", rule.syscall));
        }
        match merged.iter_mut().find(|(s, _)| *s == rule.syscall) {
            None => merged.push((rule.syscall, rule.arg.clone())),
            Some((_, have)) => match (have.as_mut(), &rule.arg) {
                // Allowed whatever its argument, by either.
                (None, _) => {}
                (Some(_), None) => *have = None,
                (Some((a, values)), Some((b, more))) if a == b => values.extend(more),
                (Some(_), Some(_)) => {
                    return Err(format!("syscall {}: rules on two arguments", rule.syscall));
                }
            },
        }
    }
    let mut prog = vec![
        op(LD_W_ABS, 0, 0, ARCH),
        op(JEQ_K, 1, 0, AUDIT_ARCH),
        op(RET_K, 0, 0, libc::SECCOMP_RET_KILL_PROCESS),
        op(LD_W_ABS, 0, 0, NR),
    ];
    for (syscall, errno) in &failing {
        let nr = u32::try_from(*syscall).map_err(|_| format!("syscall {syscall}: not a number"))?;
        prog.push(op(JEQ_K, 0, 1, nr));
        prog.push(op(RET_K, 0, 0, libc::SECCOMP_RET_ERRNO | u32::from(*errno)));
    }
    for (syscall, arg) in &merged {
        let nr = u32::try_from(*syscall).map_err(|_| format!("syscall {syscall}: not a number"))?;
        let block: Vec<libc::sock_filter> = match arg {
            None => vec![op(RET_K, 0, 0, libc::SECCOMP_RET_ALLOW)],
            Some((index, values)) => {
                let mut values = values.clone();
                values.sort_unstable();
                values.dedup();
                let n = values.len();
                if n > 253 {
                    return Err(format!("syscall {syscall}: {n} values, more than 253"));
                }
                let mut block = vec![op(LD_W_ABS, 0, 0, arg_low(*index))];
                for (i, v) in values.iter().enumerate() {
                    // To the block's ALLOW, past the values after this one and its RET.
                    let to_allow = u8::try_from(n - i).map_err(|_| "too many values".to_string())?;
                    block.push(op(JEQ_K, to_allow, 0, *v));
                }
                block.push(op(RET_K, 0, 0, libc::SECCOMP_RET_TRAP));
                block.push(op(RET_K, 0, 0, libc::SECCOMP_RET_ALLOW));
                block
            }
        };
        let skip = u32::try_from(block.len()).map_err(|_| "a block too long".to_string())?;
        // Not this syscall: over its block. The syscall's number stays in A, since a
        // block that loads an argument ends in a RET.
        prog.push(op(JEQ_K, 1, 0, nr));
        prog.push(op(JA, 0, 0, skip));
        prog.extend(block);
    }
    prog.push(op(RET_K, 0, 0, libc::SECCOMP_RET_TRAP));
    if prog.len() > usize::from(u16::MAX) {
        return Err(format!("a filter of {} instructions", prog.len()));
    }
    Ok(Filter(prog))
}

/// Installs `filter` on the calling thread, or with `all_threads` on every thread of the
/// process (`SECCOMP_FILTER_FLAG_TSYNC`), after `PR_SET_NO_NEW_PRIVS`, which lets an
/// unprivileged process install one. Filters add up: a thread keeps every one it has, and
/// a new thread starts with its creator's. A refused syscall raises SIGSYS, which
/// [`name_refusals`] reports.
pub fn install(filter: &Filter, all_threads: bool) -> io::Result<()> {
    let prog = libc::sock_fprog {
        len: u16::try_from(filter.0.len()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?,
        filter: filter.0.as_ptr().cast_mut(),
    };
    // SAFETY: prctl(2) with integer arguments.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let tsync = if all_threads {
        libc::SECCOMP_FILTER_FLAG_TSYNC
    } else {
        0
    };
    let set = |flags: libc::c_ulong| {
        // SAFETY: seccomp(2) reading the program `prog` points at, which `filter` keeps
        // alive for the call; the kernel copies it.
        unsafe {
            libc::syscall(
                libc::SYS_seccomp,
                libc::SECCOMP_SET_MODE_FILTER,
                flags,
                &raw const prog,
            )
        }
    };
    // LOG: the kernel logs each refusal, the syscall's number with it (seccomp(2)). A
    // refusal while SIGSYS is blocked, as in a thread's exit, ends the process with the
    // signal's default action before any handler can name it; the log still does. A
    // kernel before 4.14 knows no LOG, and refuses it with EINVAL: the filter goes on
    // without it.
    let mut r = set(libc::SECCOMP_FILTER_FLAG_LOG | tsync);
    if r < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL) {
        r = set(tsync);
    }
    match r {
        0 => Ok(()),
        // With TSYNC, a positive result is a thread that could not take the filter.
        r if r > 0 => Err(io::Error::other(format!("thread {r} could not take the filter"))),
        _ => Err(io::Error::last_os_error()),
    }
}

/// Reports a syscall a filter refused: on SIGSYS, writes `shards: seccomp refused syscall
/// N in thread NAME` to stderr and exits with [`REFUSED`]. Only write(2), prctl(2) and
/// exit_group(2) are made, which every filter allows.
pub fn name_refusals() -> io::Result<()> {
    // SAFETY: an all-zero sigaction is a valid value to fill.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    let handler: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) = refused;
    action.sa_sigaction = handler as libc::sighandler_t;
    action.sa_flags = libc::SA_SIGINFO;
    // SAFETY: sigaction(2) installing a handler for SIGSYS from a valid struct.
    if unsafe { libc::sigaction(libc::SIGSYS, &action, std::ptr::null_mut()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The SIGSYS handler: async-signal-safe, formatting into a stack buffer.
extern "C" fn refused(_: libc::c_int, info: *mut libc::siginfo_t, _: *mut libc::c_void) {
    // `si_syscall`, after `si_call_addr` in the union after three ints and padding
    // (include/uapi/asm-generic/siginfo.h, _sigsys), at 24 on 64-bit Linux.
    // SAFETY: the kernel passes a valid siginfo_t for SIGSYS, whose _sigsys it filled.
    let syscall = unsafe { info.cast::<u8>().add(24).cast::<i32>().read_unaligned() };
    let mut buf = [0u8; 128];
    let mut len = 0;
    let mut put = |bytes: &[u8]| {
        for &b in bytes {
            if let Some(slot) = buf.get_mut(len) {
                *slot = b;
                len += 1;
            }
        }
    };
    put(b"shards: seccomp refused syscall ");
    let mut digits = [0u8; 12];
    let mut n = syscall.unsigned_abs();
    let mut at = digits.len();
    loop {
        at -= 1;
        if let Some(d) = digits.get_mut(at) {
            *d = b'0' + (n % 10) as u8;
        }
        n /= 10;
        if n == 0 || at == 0 {
            break;
        }
    }
    put(digits.get(at..).unwrap_or_default());
    put(b" in thread ");
    let mut name = [0u8; 16];
    // SAFETY: prctl(2) writing at most 16 bytes of the thread's name into `name`.
    unsafe { libc::prctl(libc::PR_GET_NAME, name.as_mut_ptr(), 0, 0, 0) };
    let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
    put(name.get(..end).unwrap_or_default());
    put(b"\n");
    // SAFETY: write(2) of our stack buffer, then exit_group(2).
    unsafe {
        libc::write(2, buf.as_ptr().cast(), len);
        libc::syscall(libc::SYS_exit_group, REFUSED);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A filter over what a test's child needs to reach its end, and to report a refusal.
    fn base() -> Vec<Rule> {
        let mut rules: Vec<Rule> = [
            libc::SYS_write,
            libc::SYS_exit_group,
            libc::SYS_exit,
            libc::SYS_rt_sigreturn,
            libc::SYS_rt_sigprocmask,
            libc::SYS_sigaltstack,
            libc::SYS_munmap,
            libc::SYS_madvise,
            libc::SYS_futex,
        ]
        .into_iter()
        .map(Rule::any)
        .collect();
        rules.push(Rule::with(libc::SYS_prctl, 0, &[libc::PR_GET_NAME as u32]));
        rules
    }

    /// Run in a child process (this test binary, told by `SECCOMP_CHILD`): installs a
    /// filter and makes the syscall the test names; exits 0 if it was allowed.
    fn child(case: &str) -> ! {
        name_refusals().unwrap();
        let mut rules = base();
        rules.push(Rule::any(libc::SYS_getpid));
        rules.push(Rule::with(libc::SYS_fcntl, 1, &[libc::F_GETFD as u32]));
        install(&compile(&rules).unwrap(), true).unwrap();
        // SAFETY: syscalls with integer arguments.
        unsafe {
            match case {
                "allowed" => {
                    libc::getpid();
                    libc::fcntl(2, libc::F_GETFD);
                }
                "refused" => {
                    libc::getppid();
                }
                "argument" => {
                    libc::fcntl(2, libc::F_GETFL);
                }
                _ => {}
            }
            libc::syscall(libc::SYS_exit_group, 0);
        }
        // exit_group(2) does not return.
        loop {
            std::hint::spin_loop();
        }
    }

    /// A filtered process makes what its filter allows, and a syscall or an argument it
    /// does not allow ends it with SIGSYS's status, the syscall and thread named.
    #[test]
    fn a_filter_allows_what_it_lists_and_names_what_it_refuses() {
        if let Ok(case) = std::env::var("SECCOMP_CHILD") {
            child(&case);
        }
        for (case, status, said) in [
            ("allowed", 0, None),
            ("refused", REFUSED, Some(format!("syscall {}", libc::SYS_getppid))),
            ("argument", REFUSED, Some(format!("syscall {}", libc::SYS_fcntl))),
        ] {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "platform::seccomp::tests::a_filter_allows_what_it_lists_and_names_what_it_refuses",
                    "--nocapture",
                    "--test-threads",
                    "1",
                ])
                .env("SECCOMP_CHILD", case)
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert_eq!(out.status.code(), Some(status), "{case}: {stderr}");
            if let Some(said) = said {
                assert!(stderr.contains(&said), "{case}: {stderr}");
                assert!(stderr.contains("in thread "), "{case}: {stderr}");
            }
        }
    }

    /// A syscall a rule fails returns its error to the caller, and the process goes on.
    fn failing_child() -> ! {
        name_refusals().unwrap();
        let mut rules = base();
        rules.push(Rule::fails(libc::SYS_getppid, libc::ENOSYS as u16));
        install(&compile(&rules).unwrap(), true).unwrap();
        // SAFETY: syscalls with integer arguments.
        unsafe {
            let r = libc::syscall(libc::SYS_getppid);
            let errno = *libc::__errno_location();
            libc::syscall(
                libc::SYS_exit_group,
                if r == -1 && errno == libc::ENOSYS { 0 } else { 1 },
            );
        }
        loop {
            std::hint::spin_loop();
        }
    }

    #[test]
    fn a_failed_syscall_returns_its_error() {
        if std::env::var_os("SECCOMP_FAILING_CHILD").is_some() {
            failing_child();
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "platform::seccomp::tests::a_failed_syscall_returns_its_error",
                "--nocapture",
                "--test-threads",
                "1",
            ])
            .env("SECCOMP_FAILING_CHILD", "1")
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(compile(&[Rule::fails(1, 38), Rule::any(1)]).is_err());
    }

    #[test]
    fn rules_for_one_syscall_add_up() {
        let f = compile(&[
            Rule::with(libc::SYS_ioctl, 1, &[1, 2]),
            Rule::with(libc::SYS_ioctl, 1, &[2, 3]),
        ])
        .unwrap();
        // Arch check (3), the number (1), the syscall's jump pair (2), its load, three
        // values, TRAP and ALLOW (6), and the final TRAP.
        assert_eq!(f.0.len(), 3 + 1 + 2 + 6 + 1);
        let any = compile(&[Rule::with(libc::SYS_ioctl, 1, &[1]), Rule::any(libc::SYS_ioctl)]).unwrap();
        assert_eq!(any.0.len(), 3 + 1 + 2 + 1 + 1);
        assert!(
            compile(&[
                Rule::with(libc::SYS_ioctl, 1, &[1]),
                Rule::with(libc::SYS_ioctl, 2, &[1])
            ])
            .is_err()
        );
    }
}
