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
    let prog = vec![
        op(LD_W_ABS, 0, 0, ARCH),
        op(JEQ_K, 1, 0, AUDIT_ARCH),
        op(RET_K, 0, 0, libc::SECCOMP_RET_KILL_PROCESS),
        op(LD_W_ABS, 0, 0, NR),
    ];
    let keys: Vec<u32> = syscalls.iter().map(|(nr, _)| *nr).collect();
    let target = |i: usize| match syscalls.get(i) {
        Some((_, Verdict::Check(..))) => Target::Block(i),
        Some((_, Verdict::Return(action))) => Target::Ret(*action),
        None => Target::Ret(libc::SECCOMP_RET_TRAP),
    };
    // A syscall's check: its argument loaded over the number in A, which nothing after it
    // needs, and found among its values by a search of their own, each value allowed.
    let check = |i: usize| -> Result<Vec<libc::sock_filter>, String> {
        let Some((_, Verdict::Check(index, values))) = syscalls.get(i) else {
            return Err("a check of no syscall's argument".to_string());
        };
        search(
            vec![op(LD_W_ABS, 0, 0, arg_low(*index))],
            values,
            &|_| Target::Ret(libc::SECCOMP_RET_ALLOW),
            &|_| Err("a value's check".to_string()),
        )
    };
    let prog = search(prog, &keys, &target, &check)?;
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

/// Where a comparison that holds goes, or a search's last that does not: a return, or the
/// check of syscall `.0`'s argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Ret(u32),
    Block(usize),
}

/// The farthest a comparison jumps: its offsets have 8 bits.
const REACH: usize = u8::MAX as usize;

/// The search for the number in A among `keys`, sorted and distinct, key `i` going to
/// `target(i)`, and a number none of them is to a trap, laid after the code in `prog`:
/// halved while more than [`LEAF`] are left, then compared one by one, each comparison
/// jumping straight to its target in a pool after the code: each return once, and each
/// check `block` makes. A program spends its instructions on comparisons, and its install
/// time with them (PM M107). Whichever number it is, it takes at most ⌈log2(n / LEAF)⌉
/// halvings and LEAF comparisons, each one instruction, and one more, a JA, where what a
/// jump skips is past 255 instructions.
fn search(
    mut prog: Vec<libc::sock_filter>,
    keys: &[u32],
    target: &impl Fn(usize) -> Target,
    block: &impl Fn(usize) -> Result<Vec<libc::sock_filter>, String>,
) -> Result<Vec<libc::sock_filter>, String> {
    if keys.is_empty() {
        prog.push(op(RET_K, 0, 0, libc::SECCOMP_RET_TRAP));
        return Ok(prog);
    }
    // A subtree's pool holds at most every return the search has, and its checks: the
    // returns, counted while few (past that, each half pools apart), and how many of the
    // keys before each are checks, where any is.
    let mut rets = [libc::SECCOMP_RET_TRAP; 8];
    let (mut distinct, mut checks) = (1, 0);
    for i in 0..keys.len() {
        match target(i) {
            Target::Block(_) => checks += 1,
            Target::Ret(action) if rets.get(..distinct).is_some_and(|seen| seen.contains(&action)) => {}
            Target::Ret(action) => match rets.get_mut(distinct) {
                Some(slot) => {
                    *slot = action;
                    distinct += 1;
                }
                None => distinct = REACH,
            },
        }
    }
    let mut checks_before = Vec::new();
    if checks > 0 {
        checks_before.reserve_exact(keys.len() + 1);
        checks_before.push(0);
        for i in 0..keys.len() {
            let before = checks_before.last().copied().unwrap_or(0);
            checks_before.push(before + usize::from(matches!(target(i), Target::Block(_))));
        }
    }
    prog.reserve(2 * keys.len() + distinct + checks);
    let mut search = Search {
        prog,
        pending: Vec::with_capacity(keys.len() + keys.len().div_ceil(2)),
        rets: distinct,
        checks_before,
        target,
        block,
    };
    search.dispatch(keys, 0)?;
    search.pool(search.prog.len(), 0)?;
    Ok(search.prog)
}

/// A search being laid out, in one buffer: its code, and the jumps in it whose targets
/// wait for a pool, each the index of a jump, whether it is its `jt` (else its `jf`), and
/// where it goes.
struct Search<'a, T, B> {
    prog: Vec<libc::sock_filter>,
    pending: Vec<(usize, bool, Target)>,
    /// The returns the search has, each once.
    rets: usize,
    /// How many of the keys before each are checks; empty where none is.
    checks_before: Vec<usize>,
    target: &'a T,
    block: &'a B,
}

impl<T, B> Search<'_, T, B>
where
    T: Fn(usize) -> Target,
    B: Fn(usize) -> Result<Vec<libc::sock_filter>, String>,
{
    /// Lays the comparisons for `keys`, the `first`th key on, their jumps to targets
    /// pending: one pool after both halves where every jump in them reaches it, else a
    /// pool after each.
    fn dispatch(&mut self, keys: &[u32], first: usize) -> Result<(), String> {
        if keys.len() <= LEAF {
            for (i, &key) in keys.iter().enumerate() {
                self.pending
                    .push((self.prog.len(), true, (self.target)(first + i)));
                self.prog.push(op(JEQ_K, 0, 0, key));
            }
            // None of them: the last comparison's other way.
            let last = self.prog.len().saturating_sub(1);
            self.pending
                .push((last, false, Target::Ret(libc::SECCOMP_RET_TRAP)));
            return Ok(());
        }
        let half = keys.len() / 2;
        let (low, high) = keys.split_at(half);
        let at = self.prog.len();
        self.prog.push(op(JGT_K, 0, 0, low.last().copied().unwrap_or(0)));
        let from = self.pending.len();
        self.dispatch(low, first)?;
        let (mid, from_high) = (self.prog.len(), self.pending.len());
        self.dispatch(high, first + half)?;
        // The halving, maybe two instructions, both halves, and a pool of their targets,
        // each check reached through a JA at worst.
        let checks = self.checks_before.get(first + keys.len()).copied().unwrap_or(0)
            - self.checks_before.get(first).copied().unwrap_or(0);
        let pooled = self.prog.len() - at + 1 + self.rets + checks;
        let mut below = mid - (at + 1);
        if pooled > REACH + 1 {
            // The high half's at its end first, which leaves the low half where it is.
            self.pool(self.prog.len(), from_high)?;
            below += self.pool(mid, from)?;
        }
        match u8::try_from(below) {
            // Above the low half: past it, where a jump's 8 bits reach.
            Ok(over) => self.at(at)?.jt = over,
            // Else through a jump of 32 bits, which the low half skips. Only halves pooled
            // apart are so long, so no jump in them is pending, to be moved with them.
            Err(_) => {
                let far = u32::try_from(below).map_err(|_| "a filter too long".to_string())?;
                self.at(at)?.jf = 1;
                self.prog.insert(at + 1, op(JA, 0, 0, far));
            }
        }
        Ok(())
    }

    fn at(&mut self, at: usize) -> Result<&mut libc::sock_filter, String> {
        self.prog
            .get_mut(at)
            .ok_or_else(|| "a jump past the code".to_string())
    }

    /// Lays at `place` the pool the pending jumps from the `from`th on go to, and points
    /// them there: each return once, then each check, straight after where its jump
    /// reaches it, else through a JA among the returns. Each such JA moves the checks after
    /// it further: until none needs one more. How many instructions it laid.
    fn pool(&mut self, place: usize, from: usize) -> Result<usize, String> {
        let mut rets: Vec<u32> = Vec::new();
        // Each check, and the jump to it: a check is one syscall's, which one comparison
        // finds.
        let mut checks: Vec<(usize, usize)> = Vec::new();
        for &(at, _, to) in self.pending.get(from..).unwrap_or_default() {
            match to {
                Target::Ret(action) if !rets.contains(&action) => rets.push(action),
                Target::Ret(_) => {}
                Target::Block(i) => checks.push((i, at)),
            }
        }
        let bodies = checks
            .iter()
            .map(|&(i, _)| (self.block)(i))
            .collect::<Result<Vec<_>, String>>()?;
        let mut through = vec![false; checks.len()];
        loop {
            let mut at = place + rets.len() + through.iter().filter(|t| **t).count();
            let mut more = false;
            for ((&(_, jump), body), via) in checks.iter().zip(&bodies).zip(&mut through) {
                if !*via && at.saturating_sub(jump + 1) > REACH {
                    *via = true;
                    more = true;
                }
                at += body.len();
            }
            if !more {
                break;
            }
        }
        // Where each check is reached: its JA, or its start.
        let mut entries = Vec::with_capacity(checks.len());
        let header = rets.len() + through.iter().filter(|t| **t).count();
        let (mut ja, mut start) = (place + rets.len(), place + header);
        let mut pool = Vec::with_capacity(header + bodies.iter().map(Vec::len).sum::<usize>());
        pool.extend(rets.iter().map(|&action| op(RET_K, 0, 0, action)));
        for (body, via) in bodies.iter().zip(&through) {
            if *via {
                let far = u32::try_from(start - (ja + 1)).map_err(|_| "a filter too long".to_string())?;
                pool.push(op(JA, 0, 0, far));
                entries.push(ja);
                ja += 1;
            } else {
                entries.push(start);
            }
            start += body.len();
        }
        for body in bodies {
            pool.extend(body);
        }
        for i in from..self.pending.len() {
            let Some(&(at, jt, to)) = self.pending.get(i) else {
                break;
            };
            let target = match to {
                Target::Ret(action) => rets.iter().position(|&r| r == action).map(|k| place + k),
                Target::Block(b) => checks
                    .iter()
                    .position(|&(c, _)| c == b)
                    .and_then(|k| entries.get(k).copied()),
            }
            .ok_or("a jump to no target")?;
            let offset = target
                .checked_sub(at + 1)
                .and_then(|o| u8::try_from(o).ok())
                .ok_or("a jump past reach")?;
            let insn = self.at(at)?;
            if jt {
                insn.jt = offset;
            } else {
                insn.jf = offset;
            }
        }
        self.pending.truncate(from);
        let laid = pool.len();
        self.prog.splice(place..place, pool);
        Ok(laid)
    }
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

    /// Filters of every size up to 600 syscalls, some failed and some with a check of 1 to
    /// 40 values, one of 300, compile to programs the kernel takes, answering as their rules say:
    /// their jumps reach their pools wherever a subtree's falls against 255 instructions.
    #[test]
    fn filters_of_every_size_compile() {
        for n in 1..=600i64 {
            let rules: Vec<Rule> = (0..n)
                .map(|i| match i % 11 {
                    3 => {
                        let count = if i == 3 { 300 } else { (i as u32 * 7) % 40 + 1 };
                        Rule::with(i, 1, &(0..count).map(|v| v * 5).collect::<Vec<_>>())
                    }
                    7 => Rule::fails(i, (i % 3 + 1) as u16),
                    _ => Rule::any(i),
                })
                .collect();
            let prog = compile(&rules).unwrap_or_else(|e| panic!("{n} rules: {e}")).0;
            check(&prog);
            for nr in [0, (n / 2) as u32, (n - 1) as u32, n as u32] {
                let (verdict, _) = run(&prog, &data(nr, AUDIT_ARCH, [0, 5, 0, 0, 0, 0]));
                assert_eq!(
                    verdict,
                    model(&rules, nr, [0, 5, 0, 0, 0, 0]),
                    "{n} rules, syscall {nr}"
                );
            }
        }
    }

    /// Every jump reaches its pool, whatever a subtree's size against 255 instructions,
    /// when a search has as many returns as it counts, seven and the trap, or every key
    /// a check too far to reach but through a JA: each key goes where it says.
    #[test]
    fn every_jump_reaches_its_pool() {
        let errno = |i: usize| libc::SECCOMP_RET_ERRNO | (i % 7 + 1) as u32;
        // Six instructions each: all but the first few dozen past reach of their jumps.
        let long = |_: usize| Ok(vec![op(RET_K, 0, 0, libc::SECCOMP_RET_ALLOW); 6]);
        for n in 1..=400usize {
            let keys: Vec<u32> = (0..n as u32).map(|k| k * 2).collect();
            let number = || vec![op(LD_W_ABS, 0, 0, NR)];
            let returns = search(number(), &keys, &|i| Target::Ret(errno(i)), &long)
                .unwrap_or_else(|e| panic!("{n} returns: {e}"));
            let checks =
                search(number(), &keys, &Target::Block, &long).unwrap_or_else(|e| panic!("{n} checks: {e}"));
            for (prog, expect) in [
                (returns, errno as fn(usize) -> u32),
                (checks, |_| libc::SECCOMP_RET_ALLOW),
            ] {
                check(&prog);
                for (i, &key) in keys.iter().enumerate() {
                    let found = run(&prog, &data(key, AUDIT_ARCH, [0; 6])).0;
                    assert_eq!(found, expect(i), "{n} keys, key {key}");
                    let between = run(&prog, &data(key + 1, AUDIT_ARCH, [0; 6])).0;
                    assert_eq!(between, libc::SECCOMP_RET_TRAP, "{n} keys, {}", key + 1);
                }
            }
        }
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
            // A comparison a syscall, a halving between leaves of two to four, and pools of
            // a return and a trap, one for every 128 syscalls at most, each halving maybe
            // through a JA.
            let most = 4 + n + n / 2 + 2 * n.div_ceil(128) + n / 128;
            assert!(
                prog.len() <= most,
                "{n} rules: {} instructions, past {most}",
                prog.len()
            );
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
        let len = compile(&shaped()).unwrap().0.len();
        assert!(len <= 222, "{len} instructions, where 7a0d8a0's search laid 406");
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
            // Past the instructions a filter may have: a comparison each, and more.
            (0..4096).map(Rule::any).collect(),
        ] {
            assert!(compile(&wrong).is_err(), "{wrong:?}");
        }
    }
}
