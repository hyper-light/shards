//! What a seccomp filter's layout costs the syscalls it checks (review 1.7, PM M107).
//!
//! shards-vm's rules (crates/shards/src/confine.rs, those of them this architecture has),
//! compiled three ways: `linear`, each rule in turn, as shards did before (frozen here from
//! 57c5eff); `searched`, the binary search of 7a0d8a0, each comparison followed by its
//! target (frozen here too); and `search`, shards' own `compile` now, its comparisons
//! jumping to targets pooled after them. Each variant runs in a process of
//! its own, which installs it as shards-vm does (no_new_privs, then the filter over every
//! thread), and the variants take turns, round by round, in alternating order:
//!
//! - `ioctl(/dev/null, KVM_RUN)`, which the filter allows by its argument, as it does a
//!   vCPU's every run; /dev/null answers ENOTTY at once. Samples of 64 calls, as ns a call.
//! - `getpid`, which the filter allows whatever its arguments: since Linux 5.11 the
//!   kernel's cache of such syscalls skips the filter (kernel/seccomp.c,
//!   `seccomp_cache_check_allow`), so it costs the same under each, and controls for the
//!   process.
//! - The install: no_new_privs and seccomp(2), one sample a process, as µs; and of
//!   `allow`, a filter of one instruction that allows everything, which the cache lets
//!   every syscall skip: what any install costs. What that is made of (review 1.22):
//!   `allow-bare`, the same without the LOG and TSYNC flags shards-vm asks for;
//!   `allow-again`, the same installed a second time in one process; and, for each filter
//!   this program installs itself, no_new_privs apart from seccomp(2). `trace.sh` shows
//!   the kernel's part of each, by function.
//! - The compile, in this process, as µs.
//!
//! The instructions each layout runs to allow KVM_RUN are counted by running its program,
//! read from shards' `Filter` by its Debug form, the one place it shows them.
//!
//!     run.sh [ROUNDS [REV]]

use shards_vmm::platform::seccomp::{self, Rule};
use std::io::Write as _;
use std::process::Command;
use std::time::Instant;

const KVM_RUN: u32 = 0xAE80;
const BATCH: u32 = 64;
const SAMPLES: usize = 500;

/// shards-vm's rules, as confine.rs has them at 57c5eff, less the syscalls this
/// architecture does not have.
fn rules() -> Vec<Rule> {
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
        libc::SYS_exit,
        libc::SYS_exit_group,
        libc::SYS_ppoll,
        libc::SYS_pipe2,
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
        libc::SYS_landlock_create_ruleset,
        libc::SYS_landlock_add_rule,
        libc::SYS_landlock_restrict_self,
    ];
    #[cfg(target_arch = "x86_64")]
    let only: &[libc::c_long] = &[
        libc::SYS_open,
        libc::SYS_stat,
        libc::SYS_lstat,
        libc::SYS_access,
        libc::SYS_mkdir,
        libc::SYS_rename,
        libc::SYS_unlink,
        libc::SYS_readlink,
        libc::SYS_arch_prctl,
        libc::SYS_poll,
        libc::SYS_dup2,
    ];
    #[cfg(not(target_arch = "x86_64"))]
    let only: &[libc::c_long] = &[];
    let mut rules: Vec<Rule> = any.iter().chain(only).copied().map(Rule::any).collect();
    // hv::IOCTLS at 57c5eff, and the terminal's and FIONBIO.
    let mut requests: Vec<u32> = vec![
        0xAE00,
        0xAE01,
        0xAE03,
        0xAE04,
        0xC008_AE05,
        0xAE41,
        0x4020_AE46,
        0xAE47,
        0x4008_AE48,
        0xAE60,
        0x4008_AE61,
        KVM_RUN,
        0x4090_AE82,
        0x8138_AE83,
        0x4138_AE84,
        0x4008_AE90,
        0xC004_AE02,
        0xC208_AE62,
        0x8208_AE63,
        0x4030_AE7B,
        0x8030_AE7C,
        0x8090_AE81,
        0xC008_AE88,
        0x4008_AE89,
        0x8400_AE8E,
        0x4400_AE8F,
        0x8004_AE98,
        0x4004_AE99,
        0x8040_AE9F,
        0x4040_AEA0,
        0x8080_AEA1,
        0x4080_AEA2,
        0xAEA2,
        0xAEA3,
        0x9000_AEA4,
        0x5000_AEA5,
        0x8188_AEA6,
        0x4188_AEA7,
        0xAEAD,
        0x9000_AECF,
        0xC040_AED5,
        0x4018_AEE1,
        0x4018_AEE2,
    ];
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
    // SAFETY: getpid(2) has no preconditions.
    let pid = unsafe { libc::getpid() };
    rules.push(Rule::with(libc::SYS_tgkill, 0, &[pid as u32]));
    rules.push(Rule::with(libc::SYS_socket, 0, &[libc::AF_UNIX as u32]));
    rules.push(Rule::with(libc::SYS_socketpair, 0, &[libc::AF_UNIX as u32]));
    rules.push(Rule::with(
        libc::SYS_prctl,
        0,
        &[
            libc::PR_SET_NAME as u32,
            libc::PR_GET_NAME as u32,
            libc::PR_SET_NO_NEW_PRIVS as u32,
        ],
    ));
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
    rules
}

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 62 | 0x8000_0000 | 0x4000_0000;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 183 | 0x8000_0000 | 0x4000_0000;
const LD_W_ABS: u16 = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
const JEQ_K: u16 = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
const JGT_K: u16 = (libc::BPF_JMP | libc::BPF_JGT | libc::BPF_K) as u16;
const JA: u16 = (libc::BPF_JMP | libc::BPF_JA) as u16;
const RET_K: u16 = (libc::BPF_RET | libc::BPF_K) as u16;

fn op(code: u16, jt: u8, jf: u8, k: u32) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

/// shards' compile at 57c5eff: errors first, then each rule in turn, as given.
fn linear(rules: &[Rule]) -> Vec<libc::sock_filter> {
    let mut merged: Vec<(libc::c_long, Option<(u32, Vec<u32>)>)> = Vec::new();
    let mut failing: Vec<(libc::c_long, u16)> = Vec::new();
    for rule in rules {
        if let Some(errno) = rule.errno {
            failing.push((rule.syscall, errno));
            continue;
        }
        match merged.iter_mut().find(|(s, _)| *s == rule.syscall) {
            None => merged.push((rule.syscall, rule.arg.clone())),
            Some((_, have)) => match (have.as_mut(), &rule.arg) {
                (None, _) => {}
                (Some(_), None) => *have = None,
                (Some((_, values)), Some((_, more))) => values.extend(more),
            },
        }
    }
    let mut prog = vec![
        op(LD_W_ABS, 0, 0, 4),
        op(JEQ_K, 1, 0, AUDIT_ARCH),
        op(RET_K, 0, 0, libc::SECCOMP_RET_KILL_PROCESS),
        op(LD_W_ABS, 0, 0, 0),
    ];
    for (syscall, errno) in &failing {
        prog.push(op(JEQ_K, 0, 1, *syscall as u32));
        prog.push(op(RET_K, 0, 0, libc::SECCOMP_RET_ERRNO | u32::from(*errno)));
    }
    for (syscall, arg) in &merged {
        let block: Vec<libc::sock_filter> = match arg {
            None => vec![op(RET_K, 0, 0, libc::SECCOMP_RET_ALLOW)],
            Some((index, values)) => {
                let mut values = values.clone();
                values.sort_unstable();
                values.dedup();
                let n = values.len();
                let mut block = vec![op(LD_W_ABS, 0, 0, 16 + 8 * index)];
                for (i, v) in values.iter().enumerate() {
                    block.push(op(JEQ_K, (n - i) as u8, 0, *v));
                }
                block.push(op(RET_K, 0, 0, libc::SECCOMP_RET_TRAP));
                block.push(op(RET_K, 0, 0, libc::SECCOMP_RET_ALLOW));
                block
            }
        };
        prog.push(op(JEQ_K, 1, 0, *syscall as u32));
        prog.push(op(JA, 0, 0, block.len() as u32));
        prog.extend(block);
    }
    prog.push(op(RET_K, 0, 0, libc::SECCOMP_RET_TRAP));
    prog
}

/// shards' compile at 7a0d8a0: a binary search over the syscalls' numbers and over an
/// argument's values, each comparison followed by its target, a return or a check, and
/// each leaf's comparisons by a trap.
fn searched(rules: &[Rule]) -> Vec<libc::sock_filter> {
    enum Verdict {
        Return(u32),
        Check(u32, Vec<u32>),
    }
    fn search<E>(
        entries: &[E],
        key: fn(&E) -> u32,
        block: &impl Fn(&E, &mut Vec<libc::sock_filter>),
        prog: &mut Vec<libc::sock_filter>,
    ) {
        if entries.len() <= 4 {
            for entry in entries {
                let at = prog.len();
                prog.push(op(JEQ_K, 0, 0, key(entry)));
                block(entry, prog);
                skip(prog, at, false);
            }
            prog.push(op(RET_K, 0, 0, libc::SECCOMP_RET_TRAP));
            return;
        }
        let (low, high) = entries.split_at(entries.len() / 2);
        let at = prog.len();
        prog.push(op(JGT_K, 0, 0, key(low.last().expect("a low half"))));
        search(low, key, block, prog);
        skip(prog, at, true);
        search(high, key, block, prog);
    }
    fn skip(prog: &mut Vec<libc::sock_filter>, at: usize, when: bool) {
        let over = prog.len() - (at + 1);
        match u8::try_from(over) {
            Ok(over) if when => prog[at].jt = over,
            Ok(over) => prog[at].jf = over,
            Err(_) => {
                if when {
                    prog[at].jf = 1;
                } else {
                    prog[at].jt = 1;
                }
                prog.insert(at + 1, op(JA, 0, 0, over as u32));
            }
        }
    }
    let mut sorted: Vec<&Rule> = rules.iter().collect();
    sorted.sort_by_key(|r| r.syscall);
    let mut syscalls: Vec<(u32, Verdict)> = Vec::new();
    for group in sorted.chunk_by(|a, b| a.syscall == b.syscall) {
        let verdict = if let Some(errno) = group.iter().find_map(|r| r.errno) {
            Verdict::Return(libc::SECCOMP_RET_ERRNO | u32::from(errno))
        } else if group.iter().any(|r| r.arg.is_none()) {
            Verdict::Return(libc::SECCOMP_RET_ALLOW)
        } else {
            let index = group[0].arg.as_ref().expect("a check").0;
            let mut values: Vec<u32> = group
                .iter()
                .flat_map(|r| r.arg.as_ref().expect("a check").1.clone())
                .collect();
            values.sort_unstable();
            values.dedup();
            Verdict::Check(index, values)
        };
        syscalls.push((group[0].syscall as u32, verdict));
    }
    let mut prog = vec![
        op(LD_W_ABS, 0, 0, 4),
        op(JEQ_K, 1, 0, AUDIT_ARCH),
        op(RET_K, 0, 0, libc::SECCOMP_RET_KILL_PROCESS),
        op(LD_W_ABS, 0, 0, 0),
    ];
    let allow = |_: &u32, prog: &mut Vec<libc::sock_filter>| {
        prog.push(op(RET_K, 0, 0, libc::SECCOMP_RET_ALLOW));
    };
    let block = |(_, verdict): &(u32, Verdict), prog: &mut Vec<libc::sock_filter>| match verdict {
        Verdict::Return(action) => prog.push(op(RET_K, 0, 0, *action)),
        Verdict::Check(index, values) => {
            prog.push(op(LD_W_ABS, 0, 0, 16 + 8 * index));
            search(values, |v| *v, &allow, prog);
        }
    };
    search(&syscalls, |(nr, _)| *nr, &block, &mut prog);
    prog
}

/// The instructions of a filter shards compiled, from its Debug form.
fn instructions(filter: &seccomp::Filter) -> Vec<libc::sock_filter> {
    let text = format!("{filter:?}");
    text.split("sock_filter {")
        .skip(1)
        .map(|insn| {
            let field = |name: &str| -> u32 {
                let at = insn.find(&format!("{name}: ")).expect("a field") + name.len() + 2;
                let digits: String = insn[at..].chars().take_while(char::is_ascii_digit).collect();
                digits.parse().expect("a number")
            };
            op(
                field("code") as u16,
                field("jt") as u8,
                field("jf") as u8,
                field("k"),
            )
        })
        .collect()
}

/// The instructions `prog` runs for syscall `nr` with `args`, and what it returns.
fn run(prog: &[libc::sock_filter], nr: u32, args: [u64; 6]) -> (u32, usize) {
    let mut data = [0u8; 64];
    data[0..4].copy_from_slice(&nr.to_ne_bytes());
    data[4..8].copy_from_slice(&AUDIT_ARCH.to_ne_bytes());
    for (i, a) in args.iter().enumerate() {
        data[16 + 8 * i..24 + 8 * i].copy_from_slice(&a.to_ne_bytes());
    }
    let (mut a, mut pc, mut ran) = (0u32, 0usize, 0usize);
    loop {
        let i = prog[pc];
        ran += 1;
        pc += 1;
        match i.code {
            LD_W_ABS => {
                let k = i.k as usize;
                a = u32::from_ne_bytes(data[k..k + 4].try_into().expect("a word"));
            }
            JEQ_K => pc += usize::from(if a == i.k { i.jt } else { i.jf }),
            JGT_K => pc += usize::from(if a > i.k { i.jt } else { i.jf }),
            JA => pc += i.k as usize,
            RET_K => return (i.k, ran),
            code => panic!("instruction {code:#x}"),
        }
    }
}

/// The flags shards' install asks for.
const FLAGS: libc::c_ulong = libc::SECCOMP_FILTER_FLAG_LOG | libc::SECCOMP_FILTER_FLAG_TSYNC;

/// no_new_privs and the filter, with `flags`: how long each took, in µs.
fn install_raw(prog: &[libc::sock_filter], flags: libc::c_ulong) -> (f64, f64) {
    let fprog = libc::sock_fprog {
        len: prog.len() as u16,
        filter: prog.as_ptr().cast_mut(),
    };
    let start = Instant::now();
    // SAFETY: prctl(2) with integer arguments.
    assert_eq!(unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) }, 0);
    let nnp = start.elapsed();
    let start = Instant::now();
    // SAFETY: seccomp(2) with a program that outlives the call.
    let r = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            flags,
            &raw const fprog,
        )
    };
    let filter = start.elapsed();
    assert_eq!(r, 0, "{}", std::io::Error::last_os_error());
    (nnp.as_secs_f64() * 1e6, filter.as_secs_f64() * 1e6)
}

/// Installs `variant` as the child would, once, and nothing else: for `trace.sh`.
fn install(variant: &str) {
    let rules = rules();
    let allow = [op(RET_K, 0, 0, libc::SECCOMP_RET_ALLOW)];
    match variant {
        "linear" => drop(install_raw(&linear(&rules), FLAGS)),
        "searched" => drop(install_raw(&searched(&rules), FLAGS)),
        "search" => seccomp::install(&seccomp::compile(&rules).expect("compiles"), true).expect("installs"),
        "allow" => drop(install_raw(&allow, FLAGS)),
        "allow-bare" => drop(install_raw(&allow, 0)),
        other => panic!("no variant {other}"),
    }
}

/// A child's part: installs `variant` and prints its install's µs, then its ioctl and
/// getpid samples, as ns a call.
fn child(variant: &str) {
    let rules = rules();
    let devnull = std::fs::File::open("/dev/null").expect("/dev/null");
    let fd = std::os::fd::AsRawFd::as_raw_fd(&devnull);
    let allow = [op(RET_K, 0, 0, libc::SECCOMP_RET_ALLOW)];
    let mut out = String::new();
    let parts = |out: &mut String, (nnp, filter): (f64, f64)| {
        out.push_str(&format!("nnp {nnp}\nfilter {filter}\ninstall {}\n", nnp + filter));
    };
    // The variant's compile, its process's first, as a VM process's own is.
    let cold = |out: &mut String, start: Instant| {
        out.push_str(&format!("cold {}\n", start.elapsed().as_secs_f64() * 1e6));
    };
    match variant {
        "linear" => {
            let start = Instant::now();
            let prog = linear(&rules);
            cold(&mut out, start);
            parts(&mut out, install_raw(&prog, FLAGS));
        }
        "searched" => {
            let start = Instant::now();
            let prog = searched(&rules);
            cold(&mut out, start);
            parts(&mut out, install_raw(&prog, FLAGS));
        }
        "search" => {
            let start = Instant::now();
            let filter = seccomp::compile(&rules).expect("compiles");
            cold(&mut out, start);
            let start = Instant::now();
            seccomp::install(&filter, true).expect("installs");
            out.push_str(&format!("install {}\n", start.elapsed().as_secs_f64() * 1e6));
        }
        "allow" => parts(&mut out, install_raw(&allow, FLAGS)),
        "allow-bare" => parts(&mut out, install_raw(&allow, 0)),
        "allow-again" => {
            install_raw(&allow, FLAGS);
            parts(&mut out, install_raw(&allow, FLAGS));
        }
        _ => {}
    }
    for _ in 0..SAMPLES {
        let start = Instant::now();
        for _ in 0..BATCH {
            // SAFETY: an ioctl /dev/null refuses, on a descriptor this process holds.
            let r = unsafe { libc::ioctl(fd, KVM_RUN as _) };
            assert_eq!(r, -1);
        }
        out.push_str(&format!(
            "ioctl {}\n",
            start.elapsed().as_nanos() as f64 / f64::from(BATCH)
        ));
        let start = Instant::now();
        for _ in 0..BATCH {
            // SAFETY: getpid(2) has no preconditions.
            unsafe { libc::syscall(libc::SYS_getpid) };
        }
        out.push_str(&format!(
            "getpid {}\n",
            start.elapsed().as_nanos() as f64 / f64::from(BATCH)
        ));
    }
    let _ = std::io::stdout().write_all(out.as_bytes());
}

fn pct(xs: &mut [f64], p: usize) -> f64 {
    xs.sort_by(f64::total_cmp);
    xs[(p * xs.len() / 100).min(xs.len() - 1)]
}

fn report(what: &str, unit: &str, variant: &str, xs: &mut [f64]) {
    let max = xs.iter().copied().fold(0.0, f64::max);
    println!(
        "{what}, {variant}: n {}, p50 {:.1} {unit}, p90 {:.1}, p99 {:.1}, max {:.1}",
        xs.len(),
        pct(xs, 50),
        pct(xs, 90),
        pct(xs, 99),
        max
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("child") => return child(&args[2]),
        Some("install") => return install(&args[2]),
        _ => {}
    }
    let rounds: usize = args.get(1).map_or(20, |r| r.parse().expect("ROUNDS"));
    let rev = args.get(2).cloned().unwrap_or_else(|| "unknown".into());
    let uname = Command::new("uname").arg("-srm").output().expect("uname").stdout;
    let jit = std::fs::read_to_string("/proc/sys/net/core/bpf_jit_enable")
        .unwrap_or_else(|e| format!("unread ({e})"));
    println!(
        "host {}, revision {rev}, bpf_jit_enable {}",
        String::from_utf8_lossy(&uname).trim(),
        jit.trim()
    );
    let rules = rules();
    let request = [0, u64::from(KVM_RUN), 0, 0, 0, 0];
    let most = |prog: &[libc::sock_filter]| {
        rules
            .iter()
            .flat_map(|r| {
                match &r.arg {
                    None => vec![[0u64; 6]],
                    Some((i, values)) => values
                        .iter()
                        .map(|v| {
                            let mut a = [0u64; 6];
                            a[*i as usize] = u64::from(*v);
                            a
                        })
                        .collect(),
                }
                .into_iter()
                .map(move |a| (r.syscall as u32, a))
            })
            .map(|(nr, a)| run(prog, nr, a).1)
            .max()
            .unwrap_or(0)
    };
    print!("{} rules;", rules.len());
    for (name, prog) in [
        ("linear", linear(&rules)),
        ("searched", searched(&rules)),
        (
            "search",
            instructions(&seccomp::compile(&rules).expect("compiles")),
        ),
    ] {
        let (verdict, ran) = run(&prog, libc::SYS_ioctl as u32, request);
        assert_eq!(verdict, libc::SECCOMP_RET_ALLOW, "{name}");
        print!(
            " {name}: {} instructions, KVM_RUN runs {ran}, the most any allowed runs {};",
            prog.len(),
            most(&prog)
        );
    }
    println!();
    let mut compile = [Vec::new(), Vec::new(), Vec::new()];
    for i in 0..2000 {
        for k in 0..3 {
            let which = (i + k) % 3;
            let start = Instant::now();
            match which {
                0 => drop(std::hint::black_box(linear(std::hint::black_box(&rules)))),
                1 => drop(std::hint::black_box(searched(std::hint::black_box(&rules)))),
                _ => drop(std::hint::black_box(
                    seccomp::compile(std::hint::black_box(&rules)).expect("compiles"),
                )),
            }
            compile[which].push(start.elapsed().as_secs_f64() * 1e6);
        }
    }
    report("compile", "µs", "linear", &mut compile[0]);
    report("compile", "µs", "searched", &mut compile[1]);
    report("compile", "µs", "search", &mut compile[2]);
    let variants = [
        "none",
        "allow",
        "linear",
        "searched",
        "search",
        "allow-bare",
        "allow-again",
    ];
    let mut install: Vec<Vec<f64>> = vec![Vec::new(); variants.len()];
    let mut nnp: Vec<Vec<f64>> = vec![Vec::new(); variants.len()];
    let mut filter: Vec<Vec<f64>> = vec![Vec::new(); variants.len()];
    let mut ioctl: Vec<Vec<f64>> = vec![Vec::new(); variants.len()];
    let mut getpid: Vec<Vec<f64>> = vec![Vec::new(); variants.len()];
    let mut cold: Vec<Vec<f64>> = vec![Vec::new(); variants.len()];
    // Each variant's slowest install, and the round it came in.
    let mut slowest: Vec<(f64, usize)> = vec![(0.0, 0); variants.len()];
    let exe = std::env::current_exe().expect("this program");
    for round in 0..rounds {
        let mut order: Vec<usize> = (0..variants.len()).collect();
        if round % 2 == 1 {
            order.reverse();
        }
        for v in order {
            let out = Command::new(&exe)
                .args(["child", variants[v]])
                .output()
                .expect("a child");
            assert!(
                out.status.success(),
                "{}: {}",
                variants[v],
                String::from_utf8_lossy(&out.stderr)
            );
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let (what, x) = line.split_once(' ').expect("a sample");
                let x: f64 = x.parse().expect("a number");
                match what {
                    "install" => {
                        install[v].push(x);
                        if x > slowest[v].0 {
                            slowest[v] = (x, round);
                        }
                    }
                    "nnp" => nnp[v].push(x),
                    "filter" => filter[v].push(x),
                    "cold" => cold[v].push(x),
                    "ioctl" => ioctl[v].push(x),
                    _ => getpid[v].push(x),
                }
            }
        }
    }
    // The calls under the first five; the others' filters are allow's.
    for v in 0..5 {
        report("ioctl(KVM_RUN)", "ns", variants[v], &mut ioctl[v]);
    }
    for v in 0..5 {
        report("getpid", "ns", variants[v], &mut getpid[v]);
    }
    for v in 2..5 {
        report("compile, a process's first", "µs", variants[v], &mut cold[v]);
    }
    for v in 1..variants.len() {
        report("install", "µs", variants[v], &mut install[v]);
    }
    for v in 1..variants.len() {
        let (us, round) = slowest[v];
        println!(
            "slowest install, {}: {us:.1} µs, in round {round} of {rounds}",
            variants[v]
        );
    }
    for v in 1..variants.len() {
        if !nnp[v].is_empty() {
            report("  no_new_privs", "µs", variants[v], &mut nnp[v]);
            report("  seccomp(2)", "µs", variants[v], &mut filter[v]);
        }
    }
}
