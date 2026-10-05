//! A profile, as runc takes it, compiled to the seccomp filter the guest loads.
//!
//! How each architecture's rules are kept is libseccomp 2.5.4's (db.rs), so that where a
//! profile's rules overlap they mean what they mean to Docker. The rest is shards' own:
//!
//! - Names are read by the guest kernel's own tables (data/linux.csv), each on each ABI
//!   it has, where runc reads them by libseccomp 2.5.4's (Linux 5.17's) and drops a name
//!   the native ABI lacks: Docker's own default profile allows syscalls Docker's runc
//!   cannot name, which its containers then get ENOSYS for (PM M119).
//! - runc's -ENOSYS stub (patchbpf): a syscall numbered past the profile's last gets
//!   ENOSYS rather than the default action, so that a C library falls back from a
//!   syscall newer than the profile; and here so too does a number below it that is no
//!   syscall of the guest kernel, which runc's comment owns it cannot tell.
//! - x86's multiplexed socketcall(2) and ipc(2): a rule on a socket or ipc call whose
//!   arguments it compares cannot compare them through the multiplexer, whose arguments
//!   are a pointer. libseccomp writes the call's number over the rule's first comparison
//!   and keeps the rest, so that `socket` allowed but for some families is socketcall
//!   allowed for every family; here the multiplexer takes the rule's action only where
//!   it is the more restrictive (the kernel's order of actions), else the default. ipc's
//!   call is compared in its low 16 bits, which are all the kernel reads of it.
//! - The program: each ABI's syscall numbers as ranges of one decision, found by a binary
//!   search, identical decisions one block (a hash-consed graph), where libseccomp tests
//!   each syscall's number in turn within a tree of four.
//!
//! Docker mode (tests only) keeps libseccomp's and runc's ways in all of these, and is held
//! decision for decision to the program runc loads (scripts/seccomp/generate).

use std::collections::{BTreeMap, BTreeSet, HashMap};

#[cfg(test)]
use crate::bpf::Data;
use crate::bpf::{self, Insn};
use crate::db::{self, ArgCmp, Filter, Op};
use crate::profile::{Action, Arch, Config};
use crate::tables::{self, Abi, X32_SYSCALL_BIT};

/// libseccomp's actions (include/seccomp.h.in), which are the kernel's return values.
pub const ACT_KILL_PROCESS: u32 = 0x8000_0000;
pub const ACT_KILL_THREAD: u32 = 0;
pub const ACT_TRAP: u32 = 0x0003_0000;
pub const ACT_NOTIFY: u32 = 0x7fc0_0000;
pub const ACT_LOG: u32 = 0x7ffc_0000;
pub const ACT_ALLOW: u32 = 0x7fff_0000;
pub const fn act_errno(e: u16) -> u32 {
    0x0005_0000 | e as u32
}
pub const fn act_trace(e: u16) -> u32 {
    0x7ff0_0000 | e as u32
}
const EPERM: u16 = 1;
const ENOSYS: u16 = 38;
const MAX_ERRNO: u32 = 4095;

/// seccomp(2)'s flags (include/uapi/linux/seccomp.h).
pub const FILTER_FLAG_LOG: u32 = 1 << 1;
pub const FILTER_FLAG_SPEC_ALLOW: u32 = 1 << 2;

/// The kernel's largest program (BPF_MAXINSNS).
const MAX_INSNS: usize = 4096;

/// Whose ways a compile keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Shards,
    /// libseccomp 2.5.4's tables, runc's and libseccomp's ways: Docker's, for the tests
    /// that hold the shared parts to it.
    #[cfg(test)]
    Docker,
}

/// A compiled filter: its program, and the seccomp(2) flags it is loaded with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    pub insns: Vec<Insn>,
    pub flags: u32,
}

/// runc's getAction.
fn action_value(act: Action, errno: Option<u64>) -> u32 {
    // Go's int16(*errnoRet), its low 16 bits.
    let code = |e: Option<u64>| e.map_or(EPERM, |e| e as u16);
    match act {
        Action::Kill | Action::KillThread => ACT_KILL_THREAD,
        Action::Errno => act_errno(code(errno)),
        Action::Trap => ACT_TRAP,
        Action::Allow => ACT_ALLOW,
        Action::Trace => act_trace(code(errno)),
        Action::Log => ACT_LOG,
        Action::Notify => ACT_NOTIFY,
        Action::KillProcess => ACT_KILL_PROCESS,
    }
}

/// libseccomp's sys_chk_seccomp_action, on a kernel that has every action.
fn action_valid(a: u32) -> bool {
    match a & 0xffff_0000 {
        0x0005_0000 => (a & 0xffff) < MAX_ERRNO,
        0x7ff0_0000 => true,
        _ => matches!(
            a,
            ACT_KILL_PROCESS | ACT_KILL_THREAD | ACT_TRAP | ACT_LOG | ACT_ALLOW | ACT_NOTIFY
        ),
    }
}

/// How restrictive an action is: the kernel's precedence among filters' results
/// (Documentation/userspace-api/seccomp_filter.rst), higher first.
fn restrictiveness(a: u32) -> u8 {
    match a & 0xffff_0000 {
        ACT_KILL_PROCESS => 7,
        0 => 6,
        ACT_TRAP => 5,
        0x0005_0000 => 4,
        ACT_NOTIFY => 3,
        0x7ff0_0000 => 2,
        ACT_LOG => 1,
        _ => 0,
    }
}

fn is_allow_like(a: Action) -> bool {
    matches!(a, Action::Allow | Action::Log | Action::Trace)
}

/// The ABIs of `arch`'s guests (a profile's other architectures can never be asked about).
fn guest_abis(arch: Arch) -> &'static [Abi] {
    match arch {
        Arch::Amd64 => &[Abi::X86_64, Abi::X86, Abi::X32],
        Arch::Arm64 => &[Abi::Aarch64, Abi::Arm],
    }
}

fn native(arch: Arch) -> Abi {
    match arch {
        Arch::Amd64 => Abi::X86_64,
        Arch::Arm64 => Abi::Aarch64,
    }
}

/// runc's architecture names, as libseccomp's ABIs, or why libseccomp refuses one.
fn runc_abi(arch: &str) -> Result<Option<Abi>, String> {
    Ok(match arch {
        "amd64" => Some(Abi::X86_64),
        "x86" => Some(Abi::X86),
        "x32" => Some(Abi::X32),
        "arm64" => Some(Abi::Aarch64),
        "arm" => Some(Abi::Arm),
        // Little-endian, so libseccomp takes it; no guest ever runs it.
        "mipsel" | "mipsel64" | "mipsel64n32" | "ppc64le" | "riscv64" => None,
        // seccomp_arch_add's -EDOM: not the native byte order.
        _ => {
            return Err(
                "error adding architecture to seccomp filter: numerical argument out of domain".into(),
            );
        }
    })
}

/// A rule's add refused, in runc's and libseccomp-golang's words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
enum AddError {
    Fault(i32),
    Perm,
    Inval,
    Exists,
}

impl AddError {
    fn words(self) -> String {
        match self {
            AddError::Fault(n) if n < 0 => format!("unrecognized syscall -{:#x}", n.unsigned_abs()),
            AddError::Fault(n) => format!("unrecognized syscall {n:#x}"),
            AddError::Perm => "requested action matches default action of filter".into(),
            AddError::Inval => "two checks on same syscall argument".into(),
            AddError::Exists => "file exists".into(),
        }
    }
}

fn db_error(e: db::Error) -> AddError {
    match e {
        db::Error::Exists => AddError::Exists,
        db::Error::Invalid | db::Error::Corrupt => AddError::Inval,
    }
}

/// The filter being built: each ABI's database, in libseccomp's order (the native first).
struct Build {
    mode: Mode,
    arch: Arch,
    default: u32,
    filters: Vec<Filter>,
}

impl Build {
    fn rule_add(&mut self, name: &str, action: u32, args: &[Option<ArgCmp>; 6]) -> Result<(), AddError> {
        match self.mode {
            Mode::Shards => self.rule_add_shards(name, action, args),
            #[cfg(test)]
            Mode::Docker => self.rule_add_docker(name, action, args),
        }
    }

    fn rule_add_shards(
        &mut self,
        name: &str,
        action: u32,
        args: &[Option<ArgCmp>; 6],
    ) -> Result<(), AddError> {
        let default = self.default;
        for f in &mut self.filters {
            let socket = tables::SOCKET_CALLS.iter().position(|n| *n == name);
            let ipc = tables::IPC_CALLS.iter().position(|n| *n == name);
            if let Some(nr) = tables::kernel_nr(f.abi, name) {
                f.rule_add(&db::Rule {
                    syscall: nr,
                    action,
                    args: *args,
                })
                .map_err(db_error)?;
            }
            if f.abi != Abi::X86 {
                continue;
            }
            let mux = match (socket, ipc) {
                (Some(i), _) => tables::SOCKET_CALL_NUMBERS
                    .get(i)
                    .map(|&c| (tables::X86_SOCKETCALL, c, u64::MAX)),
                (_, Some(i)) => tables::IPC_CALL_NUMBERS
                    .get(i)
                    .map(|&c| (tables::X86_IPC, c, 0xffff)),
                _ => None,
            };
            let Some((sys, call, mask)) = mux else {
                continue;
            };
            // The multiplexer cannot compare the call's own arguments.
            let compared = args.iter().any(Option::is_some);
            if compared && restrictiveness(action) <= restrictiveness(default) {
                continue;
            }
            let mut mux_args = [None; 6];
            mux_args[0] = Some(ArgCmp {
                arg: 0,
                op: if mask == u64::MAX { Op::Eq } else { Op::MaskedEq },
                mask,
                datum: u64::from(call),
            });
            f.rule_add(&db::Rule {
                syscall: sys,
                action,
                args: mux_args,
            })
            .map_err(db_error)?;
        }
        Ok(())
    }

    /// libseccomp's db_col_rule_add, as runc calls it: the rule by its native number,
    /// translated for each ABI by name (arch_filter_rule_add), and x86's multiplexed
    /// calls rewritten as abi_rule_add rewrites them.
    #[cfg(test)]
    fn rule_add_docker(
        &mut self,
        name: &str,
        action: u32,
        args: &[Option<ArgCmp>; 6],
    ) -> Result<(), AddError> {
        use tables::libseccomp as ls;
        let native = native(self.arch);
        let sys = ls::resolve_name(native, name);
        if (-99..=-1).contains(&sys) {
            return Err(AddError::Inval);
        }
        for f in &mut self.filters {
            let mut s = sys;
            if f.abi != native {
                let n = ls::resolve_num(native, s).ok_or(AddError::Fault(sys))?;
                s = ls::resolve_name(f.abi, n);
                if s == tables::NR_SCMP_ERROR {
                    return Err(AddError::Fault(sys));
                }
            }
            if s == -1 || f.abi != Abi::X86 {
                f.rule_add(&db::Rule {
                    syscall: s,
                    action,
                    args: *args,
                })
                .map_err(db_error)?;
                continue;
            }
            // abi_rule_add.
            let socket = (-120..=-100).contains(&s)
                || ls::resolve_num_raw(f.abi, s).is_some_and(|n| tables::SOCKET_CALLS.contains(&n));
            let ipc = (-224..=-200).contains(&s)
                || ls::resolve_num_raw(f.abi, s).is_some_and(|n| tables::IPC_CALLS.contains(&n));
            if !socket && !ipc {
                if s >= 0 {
                    f.rule_add(&db::Rule {
                        syscall: s,
                        action,
                        args: *args,
                    })
                    .map_err(db_error)?;
                }
                continue;
            }
            let (mux, modulo) = if socket {
                (tables::X86_SOCKETCALL, 100)
            } else {
                (tables::X86_IPC, 200)
            };
            let (sys_a, sys_b) = if s > 0 {
                // _abi_syscall_mux: the pseudo number of the direct call's name.
                let n = ls::resolve_num_raw(f.abi, s).ok_or(AddError::Perm)?;
                (ls::resolve_name(f.abi, n), s)
            } else {
                // _abi_syscall_demux: the direct number of the pseudo call's name.
                let n = ls::resolve_num(f.abi, s).ok_or(AddError::Perm)?;
                let raw = ls::resolve_name_raw(f.abi, n);
                (
                    s,
                    if raw == tables::NR_SCMP_ERROR {
                        tables::NR_SCMP_UNDEF
                    } else {
                        raw
                    },
                )
            };
            if sys_a != tables::NR_SCMP_UNDEF {
                let mut a = *args;
                a[0] = Some(ArgCmp {
                    arg: 0,
                    op: Op::Eq,
                    mask: u64::MAX,
                    datum: u64::from((sys_a.unsigned_abs()) % modulo),
                });
                f.rule_add(&db::Rule {
                    syscall: mux,
                    action,
                    args: a,
                })
                .map_err(db_error)?;
            }
            if sys_b != tables::NR_SCMP_UNDEF {
                f.rule_add(&db::Rule {
                    syscall: sys_b,
                    action,
                    args: *args,
                })
                .map_err(db_error)?;
            }
        }
        Ok(())
    }

    /// The number a profile's name has on `abi`, by which runc finds its last syscall.
    fn number(&self, abi: Abi, name: &str) -> Option<i32> {
        match self.mode {
            Mode::Shards => tables::kernel_nr(abi, name),
            #[cfg(test)]
            Mode::Docker => Some(tables::libseccomp::resolve_name(abi, name)),
        }
    }

    /// Whether the runc rule names a syscall at all (GetSyscallFromName).
    fn known(&self, name: &str) -> bool {
        match self.mode {
            Mode::Shards => guest_abis(self.arch)
                .iter()
                .any(|a| tables::kernel_nr(*a, name).is_some()),
            #[cfg(test)]
            Mode::Docker => {
                tables::libseccomp::resolve_name(native(self.arch), name) != tables::NR_SCMP_ERROR
            }
        }
    }
}

/// Where a test reads: seccomp_data's syscall number and architecture, an argument's
/// low or high word.
const NR: u32 = 0;
const ARCH: u32 = 4;
const fn arg_offset(arg: u32, high: bool) -> u32 {
    16 + 8 * arg + if high { 4 } else { 0 }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Cmp {
    Eq,
    Gt,
    Ge,
}

type Nid = usize;

/// A decision graph: each node returns, or tests a word and goes on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Node {
    Ret(u32),
    Test {
        offset: u32,
        mask: u32,
        cmp: Cmp,
        k: u32,
        t: Nid,
        f: Nid,
    },
}

#[derive(Debug, Default)]
struct Ir {
    nodes: Vec<Node>,
    index: HashMap<Node, Nid>,
}

impl Ir {
    fn mk(&mut self, n: Node) -> Nid {
        if let Node::Test { t, f, .. } = n
            && t == f
        {
            return t;
        }
        if let Some(&id) = self.index.get(&n) {
            return id;
        }
        self.nodes.push(n);
        let id = self.nodes.len() - 1;
        self.index.insert(n, id);
        id
    }

    fn ret(&mut self, a: u32) -> Nid {
        self.mk(Node::Ret(a))
    }

    #[cfg(test)]
    fn word(data: &Data, offset: u32) -> u32 {
        match offset {
            NR => data.nr,
            ARCH => data.arch,
            o => {
                let i = ((o - 16) / 8) as usize;
                let v = data.args.get(i).copied().unwrap_or(0);
                if (o - 16) % 8 == 0 {
                    v as u32
                } else {
                    (v >> 32) as u32
                }
            }
        }
    }

    /// What the graph decides for `data`.
    #[cfg(test)]
    fn decide(&self, mut at: Nid, data: &Data) -> Option<u32> {
        for _ in 0..=self.nodes.len() {
            match *self.nodes.get(at)? {
                Node::Ret(a) => return Some(a),
                Node::Test {
                    offset,
                    mask,
                    cmp,
                    k,
                    t,
                    f,
                } => {
                    let v = Self::word(data, offset) & mask;
                    let taken = match cmp {
                        Cmp::Eq => v == k,
                        Cmp::Gt => v > k,
                        Cmp::Ge => v >= k,
                    };
                    at = if taken { t } else { f };
                }
            }
        }
        None
    }
}

/// One architecture check's worth of the filter: an `AUDIT_ARCH_*` and the ABIs under it
/// (x86_64 and x32 share one).
struct Section {
    audit: u32,
    abis: Vec<Abi>,
}

/// The ENOSYS stub's last syscalls for a section (runc's lastSyscallMap entry).
type Last = BTreeMap<Abi, u32>;

/// A compiled profile: its decision graph, before it is a program.
pub struct Compiled {
    ir: Ir,
    root: Nid,
    pub flags: u32,
}

impl Compiled {
    /// What the filter returns for `data`.
    #[cfg(test)]
    pub fn decide(&self, data: &Data) -> Option<u32> {
        self.ir.decide(self.root, data)
    }

    /// The program the guest loads.
    pub fn program(&self) -> Result<Program, String> {
        let insns = assemble(&self.ir, self.root)?;
        if insns.len() > MAX_INSNS {
            return Err(format!(
                "the seccomp filter is {} instructions, over the kernel's {MAX_INSNS}",
                insns.len()
            ));
        }
        Ok(Program {
            insns,
            flags: self.flags,
        })
    }
}

/// runc's InitSeccomp and patchbpf, and libseccomp's filter, for `cfg` on `arch`.
pub(crate) fn compile(cfg: &Config, arch: Arch, mode: Mode) -> Result<Compiled, String> {
    let default = action_value(cfg.default_action, cfg.default_errno_ret);
    for call in &cfg.syscalls {
        if call.action == Action::Notify && call.name == "write" {
            return Err("SCMP_ACT_NOTIFY cannot be used for the write syscall".into());
        }
    }
    if cfg.default_action == Action::Notify {
        return Err("SCMP_ACT_NOTIFY cannot be used as default action".into());
    }
    if !action_valid(default) {
        return Err("error creating filter: could not create filter".into());
    }
    // The native ABI, then each the profile adds, once each.
    let mut abis = vec![native(arch)];
    for a in &cfg.architectures {
        if let Some(abi) = runc_abi(a)?
            && guest_abis(arch).contains(&abi)
            && !abis.contains(&abi)
        {
            abis.push(abi);
        }
    }
    let mut flags = 0;
    for f in &cfg.flags {
        match f.as_str() {
            crate::profile::FLAG_LOG => flags |= FILTER_FLAG_LOG,
            crate::profile::FLAG_SPEC_ALLOW => flags |= FILTER_FLAG_SPEC_ALLOW,
            _ => {}
        }
    }
    if cfg.syscalls.iter().any(|c| c.action == Action::Notify) {
        return Err("SCMP_ACT_NOTIFY needs a seccomp agent (listenerPath), which shards does not run".into());
    }
    let mut build = Build {
        mode,
        arch,
        default,
        filters: abis
            .iter()
            .map(|&abi| {
                #[cfg_attr(not(test), allow(unused_mut))]
                let mut f = Filter::new(abi);
                #[cfg(test)]
                {
                    f.libseccomp_slip = mode == Mode::Docker;
                }
                f
            })
            .collect(),
    };
    for call in &cfg.syscalls {
        // matchCall's order: the name, then whether the action is the default's, then
        // whether the name is a syscall.
        if call.name.is_empty() {
            return Err("empty string is not a valid syscall".into());
        }
        let act = action_value(call.action, call.errno_ret);
        if act == default || !build.known(&call.name) {
            continue;
        }
        let mut counts = [0u32; 6];
        let mut conds = Vec::new();
        for &(index, op, value, value_two) in &call.args {
            if index > 5 {
                return Err(format!(
                    "error creating seccomp syscall condition for syscall {}: syscalls only have up to 6 arguments ({index} given)",
                    call.name
                ));
            }
            if let Some(c) = counts.get_mut(index as usize) {
                *c += 1;
            }
            let (mask, datum) = if op == Op::MaskedEq {
                (value, value_two)
            } else {
                (u64::MAX, value)
            };
            conds.push(ArgCmp {
                arg: index as u32,
                op,
                mask,
                datum,
            });
        }
        // Two comparisons of one argument are two rules (runc's matchCall).
        let groups: Vec<Vec<ArgCmp>> = if counts.iter().any(|&c| c > 1) {
            conds.iter().map(|c| vec![*c]).collect()
        } else {
            vec![conds]
        };
        let unconditional = call.args.is_empty();
        for group in groups {
            let mut args = [None; 6];
            for c in group {
                let slot = args
                    .get_mut(c.arg as usize)
                    .ok_or("a comparison of no argument")?;
                if slot.is_some() {
                    return Err(add_error(&call.name, unconditional, AddError::Inval));
                }
                *slot = Some(c);
            }
            if !action_valid(act) {
                return Err(add_error(&call.name, unconditional, AddError::Inval));
            }
            build
                .rule_add(&call.name, act, &args)
                .map_err(|e| add_error(&call.name, unconditional, e))?;
        }
    }
    // runc's -ENOSYS stub, unless the default lets everything through anyway.
    let stub = !is_allow_like(cfg.default_action);
    let mut lasts: BTreeMap<Abi, u32> = BTreeMap::new();
    if stub {
        for &abi in &abis {
            let largest = cfg
                .syscalls
                .iter()
                .filter_map(|c| build.number(abi, &c.name))
                .filter(|&n| n > 0)
                .max();
            if let Some(n) = largest {
                lasts.insert(abi, n as u32);
            }
        }
    }
    let mut ir = Ir::default();
    let bad_arch = ir.ret(ACT_KILL_THREAD);
    let mut sections: Vec<Section> = Vec::new();
    for &abi in &abis {
        let audit = abi.audit_arch();
        match sections.iter_mut().find(|s| s.audit == audit) {
            Some(s) => s.abis.push(abi),
            None => sections.push(Section {
                audit,
                abis: vec![abi],
            }),
        }
    }
    // Each section behind its architecture check; any other architecture is refused.
    let mut root = bad_arch;
    for s in sections.iter().rev() {
        let last: Last = s
            .abis
            .iter()
            .filter_map(|a| lasts.get(a).map(|n| (*a, *n)))
            .collect();
        let body = section(&mut ir, &build, s, &last, default, mode)?;
        root = ir.mk(Node::Test {
            offset: ARCH,
            mask: u32::MAX,
            cmp: Cmp::Eq,
            k: s.audit,
            t: body,
            f: root,
        });
    }
    Ok(Compiled { ir, root, flags })
}

fn add_error(name: &str, unconditional: bool, e: AddError) -> String {
    if unconditional {
        format!(
            "error adding seccomp filter rule for syscall {name}: {}",
            e.words()
        )
    } else {
        format!("error adding seccomp rule for syscall {name}: {}", e.words())
    }
}

/// A section's decision: by syscall number, ranges of one decision each, found by a
/// binary search.
fn section(
    ir: &mut Ir,
    b: &Build,
    s: &Section,
    last: &Last,
    default: u32,
    mode: Mode,
) -> Result<Nid, String> {
    let enosys = ir.ret(act_errno(ENOSYS));
    let def = ir.ret(default);
    let bad = ir.ret(ACT_KILL_THREAD);
    let x86_64 = s.abis.contains(&Abi::X86_64);
    let x32 = s.abis.contains(&Abi::X32);
    // Each syscall's own decision, by its number in the section.
    let mut listed: BTreeMap<u32, Nid> = BTreeMap::new();
    for f in &b.filters {
        if !s.abis.contains(&f.abi) {
            continue;
        }
        for sys in &f.syscalls {
            if !sys.valid || sys.num < 0 {
                continue;
            }
            let node = match sys.chains {
                None => ir.ret(sys.action),
                Some(head) => lower(ir, f, Some(head), def).map_err(|e| format!("{e:?}"))?,
            };
            listed.insert(sys.num as u32, node);
        }
    }
    let is_x32 = |nr: u32| nr & (X32_SYSCALL_BIT as u32) != 0;
    // The ABI a number is under, in this section.
    let abi_of = |nr: u32| -> Option<Abi> {
        match (x86_64, x32) {
            (true, true) => Some(if is_x32(nr) { Abi::X32 } else { Abi::X86_64 }),
            _ => s.abis.first().copied(),
        }
    };
    let decide = |nr: u32| -> Nid {
        // runc's stub first.
        let abi = abi_of(nr);
        let past_last = match last.len() {
            0 => false,
            1 => {
                let (only, max) = last
                    .iter()
                    .next()
                    .map(|(a, m)| (*a, *m))
                    .unwrap_or((Abi::X86_64, u32::MAX));
                match only {
                    Abi::X86_64 if s.audit == Abi::X86_64.audit_arch() => !is_x32(nr) && nr > max,
                    Abi::X32 => is_x32(nr) && nr > max,
                    _ => nr > max,
                }
            }
            _ => {
                let x86 = last.get(&Abi::X86_64).copied().unwrap_or(u32::MAX);
                let x32m = last.get(&Abi::X32).copied().unwrap_or(u32::MAX);
                if is_x32(nr) { nr > x32m } else { nr > x86 }
            }
        };
        if past_last {
            return enosys;
        }
        // libseccomp's own ABI filtering on x86_64 and x32.
        if x86_64 && !x32 && nr >= X32_SYSCALL_BIT as u32 && nr != u32::MAX {
            return bad;
        }
        if x32 && !x86_64 && nr < X32_SYSCALL_BIT as u32 {
            return bad;
        }
        if let Some(&n) = listed.get(&nr) {
            return n;
        }
        // No syscall of the guest kernel, below the stub's line: ENOSYS, as the kernel
        // answers it (shards'; runc's stub cannot tell).
        if mode == Mode::Shards
            && default != ACT_ALLOW
            && let (Some(abi), Some(&max)) = (abi, abi.and_then(|a| last.get(&a)))
            && nr <= max
            && !tables::kernel_has(abi, nr as i32)
        {
            return enosys;
        }
        def
    };
    // The points where the decision can change.
    let mut points: BTreeSet<u32> = BTreeSet::from([0, X32_SYSCALL_BIT as u32, u32::MAX]);
    for &nr in listed.keys() {
        points.insert(nr);
        points.insert(nr.saturating_add(1));
    }
    for &m in last.values() {
        points.insert(m.saturating_add(1));
    }
    if mode == Mode::Shards {
        for &abi in &s.abis {
            let base = if abi == Abi::X32 {
                X32_SYSCALL_BIT as u32
            } else {
                0
            };
            for nr in base..=base + last.get(&abi).copied().unwrap_or(0).saturating_sub(base) {
                let has = tables::kernel_has(abi, nr as i32);
                if has != tables::kernel_has(abi, nr.wrapping_sub(1) as i32) {
                    points.insert(nr);
                }
            }
        }
    }
    let mut ranges: Vec<(u32, Nid)> = Vec::new();
    for &p in &points {
        let n = decide(p);
        if ranges.last().map(|(_, l)| *l) != Some(n) {
            ranges.push((p, n));
        }
    }
    Ok(bisect(ir, &ranges))
}

/// A binary search of `ranges` (each from its number up to the next's) by syscall number.
fn bisect(ir: &mut Ir, ranges: &[(u32, Nid)]) -> Nid {
    match ranges {
        [] => ir.ret(ACT_KILL_THREAD),
        [(_, only)] => *only,
        _ => {
            let (low, high) = ranges.split_at(ranges.len() / 2);
            let bound = high.first().map_or(0, |(b, _)| *b);
            let t = bisect(ir, high);
            let f = bisect(ir, low);
            ir.mk(Node::Test {
                offset: NR,
                mask: u32::MAX,
                cmp: Cmp::Ge,
                k: bound,
                t,
                f,
            })
        }
    }
}

/// libseccomp's tree at `head` as a decision (gen_bpf.c `_gen_bpf_chain`): a level's
/// nodes in order, each's branch taken where it has one, else the next node; a level run
/// out goes on to `cont`.
fn lower(ir: &mut Ir, f: &Filter, head: Option<db::Id>, cont: Nid) -> Result<Nid, db::Error> {
    let Some(h) = head else {
        return Ok(cont);
    };
    let mut level = Vec::new();
    let mut at = Some(h);
    while let Some(i) = at {
        match f.node(i)?.lvl_prv {
            Some(p) => at = Some(p),
            None => break,
        }
    }
    while let Some(i) = at {
        level.push(i);
        at = f.node(i)?.lvl_nxt;
    }
    let mut next = cont;
    for &i in level.iter().rev() {
        let n = f.node(i)?.clone();
        let branch = |ir: &mut Ir, sub: Option<db::Id>, act: Option<u32>| -> Result<Nid, db::Error> {
            match (sub, act) {
                (Some(s), _) => lower(ir, f, Some(s), next),
                (None, Some(a)) => Ok(ir.ret(a)),
                (None, None) => Ok(next),
            }
        };
        let t = branch(ir, n.nxt_t, n.act_t_flg.then_some(n.act_t))?;
        let fl = branch(ir, n.nxt_f, n.act_f_flg.then_some(n.act_f))?;
        let cmp = match n.op {
            Some(Op::Eq | Op::MaskedEq) => Cmp::Eq,
            Some(Op::Gt) => Cmp::Gt,
            Some(Op::Ge) => Cmp::Ge,
            _ => return Err(db::Error::Corrupt),
        };
        next = ir.mk(Node::Test {
            offset: arg_offset(n.arg, n.arg_h),
            mask: n.mask,
            cmp,
            k: n.datum,
            t,
            f: fl,
        });
    }
    Ok(next)
}

/// The graph as a program: its nodes in an order where every edge goes forward (each after
/// all that reach it), each test a load (where the word is not the one loaded already), a
/// mask (where it masks) and a jump, a jump too far for its 8-bit offset by way of a JA.
fn assemble(ir: &Ir, root: Nid) -> Result<Vec<Insn>, String> {
    // Reverse post-order: every node after each that reaches it.
    let mut order = Vec::new();
    let mut seen = vec![false; ir.nodes.len()];
    let mut stack = vec![(root, false)];
    while let Some((n, done)) = stack.pop() {
        if done {
            order.push(n);
            continue;
        }
        if *seen.get(n).ok_or("a node out of the graph")? {
            continue;
        }
        if let Some(s) = seen.get_mut(n) {
            *s = true;
        }
        stack.push((n, true));
        if let Some(Node::Test { t, f, .. }) = ir.nodes.get(n) {
            // The false branch first, so that it comes after the true one, nearer.
            stack.push((*f, false));
            stack.push((*t, false));
        }
    }
    order.reverse();
    // The word in the accumulator on entry to each node, where all that reach it agree.
    let mut entry: Vec<Option<Option<(u32, u32)>>> = vec![None; ir.nodes.len()];
    if let Some(e) = entry.get_mut(root) {
        *e = Some(None);
    }
    for &n in &order {
        if let Some(Node::Test {
            offset, mask, t, f, ..
        }) = ir.nodes.get(n)
        {
            let out = Some((*offset, *mask));
            for s in [*t, *f] {
                if let Some(e) = entry.get_mut(s) {
                    *e = match *e {
                        None => Some(out),
                        Some(prev) if prev == out => Some(out),
                        Some(_) => Some(None),
                    };
                }
            }
        }
    }
    // Each test's form: whether its true and false jumps go by way of a JA.
    let mut far: HashMap<Nid, (bool, bool)> = HashMap::new();
    for _ in 0..64 {
        let (addr, _) = layout(ir, &order, &entry, &far);
        let mut changed = false;
        for &n in &order {
            if let Some(Node::Test { t, f, .. }) = ir.nodes.get(n) {
                let (ft, ff) = far.get(&n).copied().unwrap_or((false, false));
                // A jump's offsets count from the instruction after it.
                let next = jump_address(ir, n, &entry, &addr)? + 1;
                let reach = |target: Nid| -> Result<bool, String> {
                    let to = *addr.get(&target).ok_or("a target out of the program")?;
                    Ok(to < next || to - next > 255)
                };
                let (nt, nf) = (ft || reach(*t)?, ff || reach(*f)?);
                if (nt, nf) != (ft, ff) {
                    far.insert(n, (nt, nf));
                    changed = true;
                }
            }
        }
        if !changed {
            return emit(ir, &order, &entry, &far, &addr);
        }
    }
    Err("the seccomp filter's jumps did not settle".into())
}

fn needs_load(entry: &[Option<Option<(u32, u32)>>], n: Nid, offset: u32, mask: u32) -> (bool, bool) {
    match entry.get(n).copied().flatten().flatten() {
        Some((o, m)) if o == offset && (m == mask || mask == u32::MAX && m == u32::MAX) => (false, false),
        Some((o, m)) if o == offset && m == u32::MAX => (false, mask != u32::MAX),
        _ => (true, mask != u32::MAX),
    }
}

fn node_len(
    ir: &Ir,
    n: Nid,
    entry: &[Option<Option<(u32, u32)>>],
    far: &HashMap<Nid, (bool, bool)>,
) -> usize {
    match ir.nodes.get(n) {
        Some(Node::Test { offset, mask, .. }) => {
            let (ld, and) = needs_load(entry, n, *offset, *mask);
            let (ft, ff) = far.get(&n).copied().unwrap_or((false, false));
            usize::from(ld) + usize::from(and) + 1 + usize::from(ft) + usize::from(ff)
        }
        _ => 1,
    }
}

fn layout(
    ir: &Ir,
    order: &[Nid],
    entry: &[Option<Option<(u32, u32)>>],
    far: &HashMap<Nid, (bool, bool)>,
) -> (HashMap<Nid, usize>, usize) {
    let mut addr = HashMap::new();
    let mut at = 0;
    for &n in order {
        addr.insert(n, at);
        at += node_len(ir, n, entry, far);
    }
    (addr, at)
}

fn jump_address(
    ir: &Ir,
    n: Nid,
    entry: &[Option<Option<(u32, u32)>>],
    addr: &HashMap<Nid, usize>,
) -> Result<usize, String> {
    let start = *addr.get(&n).ok_or("a node out of the program")?;
    Ok(match ir.nodes.get(n) {
        Some(Node::Test { offset, mask, .. }) => {
            let (ld, and) = needs_load(entry, n, *offset, *mask);
            start + usize::from(ld) + usize::from(and)
        }
        _ => start,
    })
}

fn emit(
    ir: &Ir,
    order: &[Nid],
    entry: &[Option<Option<(u32, u32)>>],
    far: &HashMap<Nid, (bool, bool)>,
    addr: &HashMap<Nid, usize>,
) -> Result<Vec<Insn>, String> {
    let mut out = Vec::new();
    let off8 = |v: usize| u8::try_from(v).map_err(|_| "a jump out of reach".to_string());
    for &n in order {
        match ir.nodes.get(n).ok_or("a node out of the graph")? {
            Node::Ret(a) => out.push(Insn {
                code: bpf::RET_K,
                jt: 0,
                jf: 0,
                k: *a,
            }),
            Node::Test {
                offset,
                mask,
                cmp,
                k,
                t,
                f,
            } => {
                let (ld, and) = needs_load(entry, n, *offset, *mask);
                if ld {
                    out.push(Insn {
                        code: bpf::LD_W_ABS,
                        jt: 0,
                        jf: 0,
                        k: *offset,
                    });
                }
                if and {
                    out.push(Insn {
                        code: bpf::ALU_AND_K,
                        jt: 0,
                        jf: 0,
                        k: *mask,
                    });
                }
                let (ft, ff) = far.get(&n).copied().unwrap_or((false, false));
                // Offsets count from the instruction after the jump, where its JAs are.
                let next = out.len() + 1;
                let to = |x: &Nid| {
                    addr.get(x)
                        .copied()
                        .ok_or("a target out of the program".to_string())
                };
                let (tt, tf) = (to(t)?, to(f)?);
                let code = match cmp {
                    Cmp::Eq => bpf::JMP_JEQ_K,
                    Cmp::Gt => bpf::JMP_JGT_K,
                    Cmp::Ge => bpf::JMP_JGE_K,
                };
                // A far side jumps to its JA, which follow the jump: true's first.
                let jt = if ft {
                    0
                } else {
                    off8(tt.checked_sub(next).ok_or("a backward jump")?)?
                };
                let jf = if ff {
                    u8::from(ft)
                } else {
                    off8(tf.checked_sub(next).ok_or("a backward jump")?)?
                };
                out.push(Insn { code, jt, jf, k: *k });
                for (is_far, target) in [(ft, tt), (ff, tf)] {
                    if is_far {
                        let next = out.len() + 1;
                        let k = target.checked_sub(next).ok_or("a backward jump")?;
                        out.push(Insn {
                            code: bpf::JMP_JA,
                            jt: 0,
                            jf: 0,
                            k: u32::try_from(k).map_err(|e| e.to_string())?,
                        });
                    }
                }
            }
        }
    }
    Ok(out)
}
