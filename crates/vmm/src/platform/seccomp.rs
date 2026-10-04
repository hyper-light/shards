//! Seccomp filters: allow-lists of syscalls, some with the values an argument may take,
//! compiled to classic BPF over `struct seccomp_data` and installed with
//! `seccomp(SECCOMP_SET_MODE_FILTER)` after `PR_SET_NO_NEW_PRIVS`
//! (linux: Documentation/userspace-api/seccomp_filter.rst; seccomp(2)). The design is
//! Firecracker's: allow-lists checked against the architecture first, refusing by a trap
//! whose handler names what was refused (docs/research/rootless-security.md §2.4, R3).
//!
//! A program checks the architecture, loads the syscall number, and finds the syscall's
//! rule by a binary search over the rules' numbers, as libseccomp's binary tree does
//! (seccomp_attr_set(3), SCMP_FLTATR_CTL_OPTIMIZE): a syscall runs a few instructions
//! however many rules there are, and wherever its own is (review 1.7: checking each rule
//! in turn ran `KVM_RUN`, the hottest, past 83 others first). A rule allows its syscall,
//! fails it, or loads its argument, found among its values by a search of its own.
//! Arguments are compared in their low 32 bits: the ones filtered are `int` or `unsigned
//! int` to the kernel (ioctl's `cmd`, socket's `domain`, fcntl's `cmd`, prctl's
//! `option`), and a C library may pass them sign-extended (musl's `ioctl` takes an `int`
//! request).
//!
//! Its instructions are the ones the kernel's cache of syscalls a filter always allows
//! can follow (kernel/seccomp.c, `seccomp_is_const_allow`, Linux 5.11): loads of the
//! number and the architecture, `JEQ`, `JGT`, `JA` and `RET`. A syscall its rule allows
//! whatever its arguments never runs the filter there; the loads of arguments are what
//! keep the others from the cache.

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
const JGT_K: u16 = (libc::BPF_JMP | libc::BPF_JGT | libc::BPF_K) as u16;
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
/// syscall add up: one allowing it whatever its argument allows it so, and the others allow
/// every value they list, of one argument. A syscall's rules that fail it must all fail it
/// with one error.
pub fn compile(rules: &[Rule]) -> Result<Filter, String> {
    let mut sorted: Vec<&Rule> = rules.iter().collect();
    sorted.sort_by_key(|r| r.syscall);
    let mut syscalls = Vec::with_capacity(sorted.len());
    for group in sorted.chunk_by(|a, b| a.syscall == b.syscall) {
        let syscall = group.first().map_or(0, |r| r.syscall);
        let nr = u32::try_from(syscall).map_err(|_| format!("syscall {syscall}: not a number"))?;
        syscalls.push((nr, verdict(syscall, group)?));
    }
    let mut prog = vec![
        op(LD_W_ABS, 0, 0, ARCH),
        op(JEQ_K, 1, 0, AUDIT_ARCH),
        op(RET_K, 0, 0, libc::SECCOMP_RET_KILL_PROCESS),
        op(LD_W_ABS, 0, 0, NR),
    ];
    let allow = |_: &u32, prog: &mut Vec<libc::sock_filter>| {
        prog.push(op(RET_K, 0, 0, libc::SECCOMP_RET_ALLOW));
        Ok(())
    };
    let block = |(_, verdict): &(u32, Verdict), prog: &mut Vec<libc::sock_filter>| match verdict {
        Verdict::Return(action) => {
            prog.push(op(RET_K, 0, 0, *action));
            Ok(())
        }
        // The argument over the number in A, which no other block needs: each ends in a
        // RET.
        Verdict::Check(index, values) => {
            prog.push(op(LD_W_ABS, 0, 0, arg_low(*index)));
            search(values, |v| *v, &allow, prog)
        }
    };
    search(&syscalls, |(nr, _)| *nr, &block, &mut prog)?;
    // The kernel refuses a longer one (kernel/seccomp.c, seccomp_prepare_filter).
    if prog.len() > libc::BPF_MAXINSNS as usize {
        return Err(format!(
            "a filter of {} instructions, past the {} a filter may have",
            prog.len(),
            libc::BPF_MAXINSNS
        ));
    }
    Ok(Filter(prog))
}

/// What a syscall's search ends in: a return, or a check of its argument `.0` against the
/// values `.1`, sorted, which allows them and traps anything else.
enum Verdict {
    Return(u32),
    Check(u32, Vec<u32>),
}

/// The arguments `struct seccomp_data` holds.
const ARGS: u32 = 6;

/// What `rules`, all for `syscall`, make of it.
fn verdict(syscall: libc::c_long, rules: &[&Rule]) -> Result<Verdict, String> {
    if let Some((index, _)) = rules
        .iter()
        .filter_map(|r| r.arg.as_ref())
        .find(|(i, _)| *i >= ARGS)
    {
        return Err(format!("syscall {syscall}: argument {index}, of {ARGS}"));
    }
    if let Some(errno) = rules.iter().find_map(|r| r.errno) {
        if rules.iter().any(|r| r.errno != Some(errno)) {
            return Err(format!(
                "syscall {syscall}: failed by a rule, and not so by another"
            ));
        }
        return Ok(Verdict::Return(libc::SECCOMP_RET_ERRNO | u32::from(errno)));
    }
    if rules.iter().any(|r| r.arg.is_none()) {
        return Ok(Verdict::Return(libc::SECCOMP_RET_ALLOW));
    }
    let mut checked = rules.iter().filter_map(|r| r.arg.as_ref());
    let Some((index, first)) = checked.next() else {
        return Ok(Verdict::Return(libc::SECCOMP_RET_TRAP));
    };
    let mut values = first.clone();
    for (other, more) in checked {
        if other != index {
            return Err(format!("syscall {syscall}: rules on two arguments"));
        }
        values.extend_from_slice(more);
    }
    values.sort_unstable();
    values.dedup();
    Ok(Verdict::Check(*index, values))
}

/// Entries a search compares one by one, at most. A halving costs one comparison and
/// leaves half: for five entries or more it takes fewer comparisons on average, for four
/// as many, for three more.
const LEAF: usize = 4;

/// Emits into `prog` the search for the number in A among `entries`, sorted by `key` and
/// distinct: halved while more than [`LEAF`] are left, then compared one by one. Each
/// entry's search ends in what `block` emits for it, which must end every path in a RET;
/// a number none of them is ends in a trap. Whichever it is, a number takes at most
/// ⌈log2(n / LEAF)⌉ halvings and LEAF comparisons, each one instruction, and one more, a
/// JA, where what it skips is past 255 instructions.
fn search<E>(
    entries: &[E],
    key: fn(&E) -> u32,
    block: &impl Fn(&E, &mut Vec<libc::sock_filter>) -> Result<(), String>,
    prog: &mut Vec<libc::sock_filter>,
) -> Result<(), String> {
    if entries.len() <= LEAF {
        for entry in entries {
            let at = prog.len();
            prog.push(op(JEQ_K, 0, 0, key(entry)));
            block(entry, prog)?;
            // Not this one: over its block.
            skip(prog, at, false)?;
        }
        prog.push(op(RET_K, 0, 0, libc::SECCOMP_RET_TRAP));
        return Ok(());
    }
    let (low, high) = entries.split_at(entries.len() / 2);
    let at = prog.len();
    prog.push(op(JGT_K, 0, 0, low.last().map_or(0, key)));
    search(low, key, block, prog)?;
    // Above the low half: past it.
    skip(prog, at, true)?;
    search(high, key, block, prog)
}

/// Points the jump at `at`, which goes on to the code after it, over that code: when its
/// condition holds if `when`, else when it fails. A jump's 8 bits reach 255 instructions;
/// past that it goes through a JA of 32 bits inserted after it, which the other way skips.
fn skip(prog: &mut Vec<libc::sock_filter>, at: usize, when: bool) -> Result<(), String> {
    let over = prog.len().saturating_sub(at + 1);
    let Some(jump) = prog.get_mut(at) else {
        return Err("a jump past the filter's end".to_string());
    };
    match u8::try_from(over) {
        Ok(over) if when => jump.jt = over,
        Ok(over) => jump.jf = over,
        Err(_) => {
            let far = u32::try_from(over).map_err(|_| "a filter too long".to_string())?;
            if when {
                jump.jf = 1;
            } else {
                jump.jt = 1;
            }
            prog.insert(at + 1, op(JA, 0, 0, far));
        }
    }
    Ok(())
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
    /// filter and makes the syscall the test names; exits 0 if it was allowed. A `large`
    /// filter allows every other syscall below 512 too, and fcntl 300 more commands: its
    /// search's jumps over its halves and over fcntl's check are past 255 instructions,
    /// and the kernel runs them.
    fn child(case: &str, large: bool) -> ! {
        name_refusals().unwrap();
        let mut rules = base();
        rules.push(Rule::any(libc::SYS_getpid));
        let mut commands = vec![libc::F_GETFD as u32];
        if large {
            rules.extend(
                (0..512)
                    .filter(|&n| n != libc::SYS_getppid && n != libc::SYS_fcntl)
                    .map(Rule::any),
            );
            commands.extend(0x1_0000..0x1_0000 + 300);
        }
        rules.push(Rule::with(libc::SYS_fcntl, 1, &commands));
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
            child(&case, std::env::var_os("SECCOMP_LARGE").is_some());
        }
        for (case, large, status, said) in [false, true].into_iter().flat_map(|large| {
            [
                ("allowed", large, 0, None),
                (
                    "refused",
                    large,
                    REFUSED,
                    Some(format!("syscall {}", libc::SYS_getppid)),
                ),
                (
                    "argument",
                    large,
                    REFUSED,
                    Some(format!("syscall {}", libc::SYS_fcntl)),
                ),
            ]
        }) {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "platform::seccomp::tests::a_filter_allows_what_it_lists_and_names_what_it_refuses",
                    "--nocapture",
                    "--test-threads",
                    "1",
                ])
                .env("SECCOMP_CHILD", case)
                .envs(large.then_some(("SECCOMP_LARGE", "1")))
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert_eq!(out.status.code(), Some(status), "{case}, large {large}: {stderr}");
            if let Some(said) = said {
                assert!(stderr.contains(&said), "{case}, large {large}: {stderr}");
                assert!(stderr.contains("in thread "), "{case}, large {large}: {stderr}");
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

    /// `struct seccomp_data` (include/uapi/linux/seccomp.h) for `nr` of `arch` with `args`.
    fn data(nr: u32, arch: u32, args: [u64; 6]) -> [u8; 64] {
        let mut d = [0u8; 64];
        d[0..4].copy_from_slice(&nr.to_ne_bytes());
        d[4..8].copy_from_slice(&arch.to_ne_bytes());
        for (i, a) in args.iter().enumerate() {
            d[16 + 8 * i..24 + 8 * i].copy_from_slice(&a.to_ne_bytes());
        }
        d
    }

    /// What the kernel checks of a program before it takes it (net/core/filter.c,
    /// bpf_check_classic; kernel/seccomp.c, seccomp_check_filter and
    /// seccomp_prepare_filter): a length it allows, jumps that land inside it, loads of
    /// `seccomp_data`'s words, and a RET last.
    fn check(prog: &[libc::sock_filter]) {
        let len = prog.len();
        assert!(
            (1..=libc::BPF_MAXINSNS as usize).contains(&len),
            "{len} instructions"
        );
        for (pc, i) in prog.iter().enumerate() {
            match i.code {
                LD_W_ABS => assert!(i.k < 64 && i.k % 4 == 0, "a load of {} at {pc}", i.k),
                JEQ_K | JGT_K => assert!(
                    pc + 1 + usize::from(i.jt.max(i.jf)) < len,
                    "a jump past the end at {pc}"
                ),
                JA => assert!(pc + 1 + (i.k as usize) < len, "a jump past the end at {pc}"),
                RET_K => {}
                code => panic!("instruction {code:#x} at {pc}"),
            }
        }
        assert_eq!(prog[len - 1].code, RET_K, "a program ends in a RET");
    }

    /// Runs `prog` over `data` as the kernel runs a classic BPF program: what it returns,
    /// and how many instructions it ran.
    fn run(prog: &[libc::sock_filter], data: &[u8; 64]) -> (u32, usize) {
        let (mut a, mut pc, mut ran) = (0u32, 0usize, 0usize);
        loop {
            let i = prog[pc];
            ran += 1;
            pc += 1;
            match i.code {
                LD_W_ABS => {
                    let k = i.k as usize;
                    a = u32::from_ne_bytes(data[k..k + 4].try_into().unwrap());
                }
                JEQ_K => pc += usize::from(if a == i.k { i.jt } else { i.jf }),
                JGT_K => pc += usize::from(if a > i.k { i.jt } else { i.jf }),
                JA => pc += i.k as usize,
                RET_K => return (i.k, ran),
                code => panic!("instruction {code:#x}"),
            }
        }
    }

    /// What `rules` make of `nr` with `args`, read from the rules alone.
    fn model(rules: &[Rule], nr: u32, args: [u64; 6]) -> u32 {
        let mine: Vec<&Rule> = rules
            .iter()
            .filter(|r| r.syscall == libc::c_long::from(nr))
            .collect();
        if let Some(errno) = mine.iter().find_map(|r| r.errno) {
            libc::SECCOMP_RET_ERRNO | u32::from(errno)
        } else if mine.iter().any(|r| r.arg.is_none())
            || mine.iter().any(|r| {
                let (index, values) = r.arg.as_ref().unwrap();
                values.contains(&(args[*index as usize] as u32))
            })
        {
            libc::SECCOMP_RET_ALLOW
        } else {
            libc::SECCOMP_RET_TRAP
        }
    }

    /// Every number up to past the highest rule's, and the x32 ABI's and the highest:
    /// for each, no arguments, and every value its rules list, with its neighbours, with
    /// high bits set (which a check reads past), and the extremes. The filter answers each
    /// as its rules say; another architecture is killed at once. Returns the most
    /// instructions an allowed syscall ran.
    fn answers_as_its_rules_say(rules: &[Rule]) -> usize {
        let prog = compile(rules).unwrap().0;
        check(&prog);
        let top = rules.iter().map(|r| r.syscall as u32).max().unwrap_or(0);
        let mut most = 0;
        for nr in (0..=top + 64).chain([0x4000_0000 | 1, u32::MAX]) {
            let mut cases = vec![[0u64; 6]];
            for r in rules.iter().filter(|r| r.syscall == libc::c_long::from(nr)) {
                if let Some((index, values)) = &r.arg {
                    for &v in values.iter().chain(&[0, u32::MAX]) {
                        for x in [
                            u64::from(v),
                            u64::from(v.wrapping_sub(1)),
                            u64::from(v.wrapping_add(1)),
                            u64::from(v) | 1 << 32,
                            u64::from(v) | 0xffff_ffff << 32,
                        ] {
                            let mut args = [0u64; 6];
                            args[*index as usize] = x;
                            cases.push(args);
                        }
                    }
                }
            }
            for args in cases {
                let (verdict, ran) = run(&prog, &data(nr, AUDIT_ARCH, args));
                assert_eq!(verdict, model(rules, nr, args), "syscall {nr}, {args:x?}");
                if verdict == libc::SECCOMP_RET_ALLOW {
                    most = most.max(ran);
                }
            }
            assert_eq!(
                run(&prog, &data(nr, AUDIT_ARCH ^ 1, [0; 6])),
                (libc::SECCOMP_RET_KILL_PROCESS, 3)
            );
        }
        most
    }

    /// Rules shaped as shards-vm's (crates/shards/src/confine.rs): 83 syscalls allowed
    /// whatever their arguments, ioctl with 47 requests, a few with fewer, one failed.
    fn shaped() -> Vec<Rule> {
        let mut rules: Vec<Rule> = (0..300).step_by(3).take(83).map(Rule::any).collect();
        let requests: Vec<u32> = (0..43u32)
            .map(|i| 0xAE00 + i * 5 + if i % 3 == 0 { 0x4008_0000 } else { 0 })
            .chain([0x5401, 0x5402, 0x5413, 0x5421])
            .collect();
        rules.push(Rule::with(16, 1, &requests));
        rules.push(Rule::with(73, 1, &[0, 1, 2, 3, 4, 1030]));
        rules.push(Rule::with(200, 0, &[1234]));
        rules.push(Rule::with(301, 0, &[1]));
        rules.push(Rule::with(157, 0, &[15, 16, 38]));
        rules.push(Rule::with(56, 0, &[0x3d0f00, 0x7d0f00]));
        rules.push(Rule::fails(305, libc::ENOSYS as u16));
        rules
    }

    #[test]
    fn a_filter_answers_every_syscall_and_value_as_its_rules_say() {
        answers_as_its_rules_say(&shaped());
        answers_as_its_rules_say(&base());
        answers_as_its_rules_say(&[]);
        // Jumps past 255 instructions: a check of 300 values, and a tree over 600 syscalls
        // whose halves are longer than that.
        let mut wide: Vec<Rule> = (0..600).map(|n| Rule::any(n * 2)).collect();
        wide.push(Rule::with(301, 2, &(0..300).map(|v| v * 7).collect::<Vec<_>>()));
        wide.push(Rule::with(303, 0, &(0..300).map(|v| v * 3).collect::<Vec<_>>()));
        answers_as_its_rules_say(&wide);
    }

    /// The halvings a search over `n` entries takes: ⌈log2(⌈n / LEAF⌉)⌉.
    fn halvings(n: usize) -> usize {
        (usize::BITS - (n.div_ceil(LEAF).max(1) - 1).leading_zeros()) as usize
    }

    /// A syscall runs no more instructions than its search's halvings and comparisons,
    /// however many rules there are and wherever its own is: under rules shaped as
    /// shards-vm's, every syscall allowed in 19 or fewer, where checking each rule in turn
    /// ran ioctl's past 84 others first, `KVM_RUN` in 185.
    #[test]
    fn a_syscall_runs_a_few_instructions_however_many_rules_there_are() {
        for k in 0..=10 {
            let n = 1usize << k;
            let rules: Vec<Rule> = (0..n as libc::c_long).map(|i| Rule::any(i * 2 + 1)).collect();
            let prog = compile(&rules).unwrap().0;
            // The architecture's check and the number's load; the search; its RET.
            let bound = 3 + 2 * halvings(n) + LEAF + 1;
            for nr in 0..=2 * n as u32 + 2 {
                let (_, ran) = run(&prog, &data(nr, AUDIT_ARCH, [0; 6]));
                assert!(
                    ran <= bound,
                    "{n} rules, syscall {nr}: {ran} instructions, past {bound}"
                );
            }
            let values: Vec<u32> = (0..n as u32).map(|v| v * 2 + 1).collect();
            let prog = compile(&[Rule::with(7, 0, &values)]).unwrap().0;
            // Its syscall's comparison, the argument's load, the values' search.
            let bound = 3 + 1 + 1 + 2 * halvings(n) + LEAF + 1;
            for v in 0..=2 * n as u64 + 2 {
                let (_, ran) = run(&prog, &data(7, AUDIT_ARCH, [v, 0, 0, 0, 0, 0]));
                assert!(
                    ran <= bound,
                    "{n} values, value {v}: {ran} instructions, past {bound}"
                );
            }
        }
        let most = answers_as_its_rules_say(&shaped());
        assert!(most <= 19, "{most} instructions");
    }

    #[test]
    fn rules_for_one_syscall_add_up() {
        let both = [
            Rule::with(libc::SYS_ioctl, 1, &[1, 2]),
            Rule::with(libc::SYS_ioctl, 1, &[2, 3]),
        ];
        answers_as_its_rules_say(&both);
        let prog = compile(&both).unwrap().0;
        let ioctl = libc::SYS_ioctl as u32;
        for (request, verdict) in [
            (1, libc::SECCOMP_RET_ALLOW),
            (3, libc::SECCOMP_RET_ALLOW),
            (4, libc::SECCOMP_RET_TRAP),
        ] {
            assert_eq!(
                run(&prog, &data(ioctl, AUDIT_ARCH, [0, request, 0, 0, 0, 0])).0,
                verdict
            );
        }
        // Allowed whatever its argument by one: by all, whatever the others check.
        for rules in [
            [Rule::with(libc::SYS_ioctl, 1, &[1]), Rule::any(libc::SYS_ioctl)],
            [Rule::any(libc::SYS_ioctl), Rule::with(libc::SYS_ioctl, 2, &[1])],
        ] {
            answers_as_its_rules_say(&rules);
            let prog = compile(&rules).unwrap().0;
            assert_eq!(
                run(&prog, &data(ioctl, AUDIT_ARCH, [0, 9, 9, 0, 0, 0])).0,
                libc::SECCOMP_RET_ALLOW
            );
        }
        // Failed alike by two: failed so.
        answers_as_its_rules_say(&[Rule::fails(1, 38), Rule::fails(1, 38)]);
        for wrong in [
            vec![
                Rule::with(libc::SYS_ioctl, 1, &[1]),
                Rule::with(libc::SYS_ioctl, 2, &[1]),
            ],
            vec![Rule::fails(1, 38), Rule::any(1)],
            vec![Rule::with(1, 0, &[1]), Rule::fails(1, 38)],
            vec![Rule::fails(1, 38), Rule::fails(1, 1)],
            vec![Rule::with(1, 6, &[1])],
            vec![Rule::with(1, u32::MAX, &[1])],
            vec![Rule::any(-1)],
            vec![Rule::any(1 << 32)],
            // Past the instructions a filter may have.
            (0..3000).map(Rule::any).collect(),
        ] {
            assert!(compile(&wrong).is_err(), "{wrong:?}");
        }
    }
}
