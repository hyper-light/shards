//! The guest's ABIs, and the syscall tables profiles' names are read by: the guest
//! kernel's own (data/linux.csv, scripts/seccomp/kernel-tables), and, for the tests that
//! hold this crate to Docker's toolchain, libseccomp 2.5.4's (src/syscalls.csv, and
//! include/seccomp-syscalls.h's `__PNR_*` numbers for syscalls an architecture lacks), as
//! its resolvers read them (src/syscalls.perf.template, arch-x32.c, and syscalls.c's
//! munging for x86's multiplexed socketcall(2) and ipc(2)), which is how runc reads them.

use std::sync::OnceLock;

/// The ABIs of the two guest architectures, as libseccomp names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Abi {
    X86,
    X86_64,
    X32,
    Arm,
    Aarch64,
}

impl Abi {
    /// Its `AUDIT_ARCH_*` (linux include/uapi/linux/audit.h): x32 is x86_64's.
    pub const fn audit_arch(self) -> u32 {
        const BIT64: u32 = 0x8000_0000;
        const LE: u32 = 0x4000_0000;
        match self {
            Abi::X86 => 3 | LE,
            Abi::X86_64 | Abi::X32 => 62 | BIT64 | LE,
            Abi::Arm => 40 | LE,
            Abi::Aarch64 => 183 | BIT64 | LE,
        }
    }

    /// Whether its words are 32 bits (libseccomp's ARCH_SIZE_32), arguments with them.
    pub const fn is_32(self) -> bool {
        matches!(self, Abi::X86 | Abi::X32 | Abi::Arm)
    }

    /// Its column in syscalls.csv.
    #[cfg(test)]
    const fn column(self) -> usize {
        match self {
            Abi::X86 => 1,
            Abi::X86_64 => 2,
            Abi::X32 => 3,
            Abi::Arm => 4,
            Abi::Aarch64 => 5,
        }
    }
}

/// x32's syscalls are x86_64's numbers with this bit (arch/x86/include/uapi/asm/unistd.h).
pub const X32_SYSCALL_BIT: i32 = 0x4000_0000;
/// What libseccomp resolves a name it does not know to.
#[cfg(test)]
pub const NR_SCMP_ERROR: i32 = -1;
/// What it resolves a multiplexed syscall's direct number to where there is none.
#[cfg(test)]
pub const NR_SCMP_UNDEF: i32 = -2;

/// x86's multiplexers (arch-x86.c).
pub const X86_SOCKETCALL: i32 = 102;
pub const X86_IPC: i32 = 117;

/// The socket syscalls socketcall(2) multiplexes, and the ipc ones ipc(2) does
/// (syscalls.c), whose pseudo numbers are -101.. and -201.. in this order.
pub const SOCKET_CALLS: [&str; 20] = [
    "socket",
    "bind",
    "connect",
    "listen",
    "accept",
    "getsockname",
    "getpeername",
    "socketpair",
    "send",
    "recv",
    "sendto",
    "recvfrom",
    "shutdown",
    "setsockopt",
    "getsockopt",
    "sendmsg",
    "recvmsg",
    "accept4",
    "recvmmsg",
    "sendmmsg",
];
pub const IPC_CALLS: [&str; 12] = [
    "semop",
    "semget",
    "semctl",
    "semtimedop",
    "msgsnd",
    "msgrcv",
    "msgget",
    "msgctl",
    "shmat",
    "shmdt",
    "shmget",
    "shmctl",
];

/// The calls' numbers as the multiplexers take them: socketcall's (linux
/// include/uapi/linux/net.h, SYS_SOCKET = 1 …) and ipc's (include/uapi/linux/ipc.h, SEMOP
/// = 1 … SHMCTL = 24).
pub const SOCKET_CALL_NUMBERS: [u32; 20] = [
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
];
pub const IPC_CALL_NUMBERS: [u32; 12] = [1, 2, 3, 4, 11, 12, 13, 14, 21, 22, 23, 24];

/// The guest kernel's syscalls (data/linux.csv): each ABI's names and numbers.
fn linux() -> &'static [(Abi, &'static str, i32)] {
    static ROWS: OnceLock<Vec<(Abi, &'static str, i32)>> = OnceLock::new();
    ROWS.get_or_init(|| {
        include_str!("../data/linux.csv")
            .lines()
            .filter(|l| !l.starts_with('#'))
            .filter_map(|l| {
                let mut cols = l.split(',');
                let abi = match cols.next()? {
                    "x86" => Abi::X86,
                    "x86_64" => Abi::X86_64,
                    "x32" => Abi::X32,
                    "arm" => Abi::Arm,
                    "aarch64" => Abi::Aarch64,
                    _ => return None,
                };
                let name = cols.next()?;
                let nr: i32 = cols.next()?.parse().ok()?;
                // x32's are numbered there without the bit the kernel sees them by.
                let nr = if abi == Abi::X32 { nr | X32_SYSCALL_BIT } else { nr };
                Some((abi, name, nr))
            })
            .collect()
    })
}

/// The number the guest kernel gives `name` on `abi`, if it has it there.
pub fn kernel_nr(abi: Abi, name: &str) -> Option<i32> {
    linux()
        .iter()
        .find(|(a, n, _)| *a == abi && *n == name)
        .map(|(_, _, nr)| *nr)
}

/// Whether `nr` is a syscall of the guest kernel on `abi`.
pub fn kernel_has(abi: Abi, nr: i32) -> bool {
    linux().iter().any(|(a, _, n)| *a == abi && *n == nr)
}

/// libseccomp 2.5.4's resolvers, by which Docker's runc reads a profile's names.
#[cfg(test)]
pub mod libseccomp {
    use super::*;

    pub(super) struct Row {
        pub(super) name: String,
        /// The csv's columns x86, x86_64, x32, arm, aarch64: a number, or none (PNR).
        nums: [Option<i32>; 5],
        /// Its `__PNR_*`, where libseccomp defines one.
        pnr: Option<i32>,
    }

    pub(super) fn rows() -> &'static [Row] {
        static ROWS: OnceLock<Vec<Row>> = OnceLock::new();
        ROWS.get_or_init(|| {
            let pnrs: Vec<(&str, i32)> = include_str!("../testdata/libseccomp/pnr.csv")
                .lines()
                .filter_map(|l| {
                    let (name, n) = l.split_once(',')?;
                    Some((name, n.trim().parse().ok()?))
                })
                .collect();
            include_str!("../testdata/libseccomp/syscalls.csv")
                .lines()
                .filter(|l| !l.starts_with('#') && !l.is_empty())
                .filter_map(|l| {
                    let mut cols = l.split(',');
                    let name = cols.next()?.to_string();
                    let rest: Vec<&str> = cols.collect();
                    let mut nums = [None; 5];
                    for (slot, col) in nums.iter_mut().zip(rest.iter().take(5)) {
                        *slot = col.parse().ok();
                    }
                    let pnr = pnrs.iter().find(|(n, _)| *n == name).map(|(_, v)| *v);
                    Some(Row { name, nums, pnr })
                })
                .collect()
        })
    }

    /// What the arch's raw table holds for `row`: its number, or its pseudo number.
    pub(super) fn raw_value(row: &Row, abi: Abi) -> Option<i32> {
        let column = abi.column().checked_sub(1)?;
        row.nums.get(column).copied().flatten().or(row.pnr)
    }

    /// `syscall_resolve_name_raw`: the number the arch's table holds for `name`, pseudo or
    /// not, or NR_SCMP_ERROR for a name libseccomp does not know.
    pub fn resolve_name_raw(abi: Abi, name: &str) -> i32 {
        rows()
            .iter()
            .find(|r| r.name == name)
            .and_then(|r| raw_value(r, abi))
            .unwrap_or(NR_SCMP_ERROR)
    }

    /// `syscall_resolve_num_raw`: the name the arch's table holds `num` for.
    pub fn resolve_num_raw(abi: Abi, num: i32) -> Option<&'static str> {
        rows()
            .iter()
            .find(|r| raw_value(r, abi) == Some(num))
            .map(|r| r.name.as_str())
    }

    /// The pseudo number of multiplexed `name`, if it is one (its `__PNR_*`).
    fn multiplexed_pnr(name: &str) -> Option<i32> {
        if !SOCKET_CALLS.contains(&name) && !IPC_CALLS.contains(&name) {
            return None;
        }
        rows().iter().find(|r| r.name == name).and_then(|r| r.pnr)
    }

    /// `arch->syscall_resolve_name`: as runc's GetSyscallFromName(ByArch) reads a name.
    pub fn resolve_name(abi: Abi, name: &str) -> i32 {
        match abi {
            // abi_syscall_resolve_name_munge: x86's multiplexed calls are their pseudo
            // numbers, whatever their direct ones.
            Abi::X86 => multiplexed_pnr(name).unwrap_or_else(|| resolve_name_raw(abi, name)),
            Abi::X32 => {
                let sys = resolve_name_raw(abi, name);
                if sys < 0 { sys } else { sys | X32_SYSCALL_BIT }
            }
            _ => resolve_name_raw(abi, name),
        }
    }

    /// `arch->syscall_resolve_num`.
    pub fn resolve_num(abi: Abi, num: i32) -> Option<&'static str> {
        match abi {
            Abi::X86 => {
                let names = SOCKET_CALLS.iter().chain(IPC_CALLS.iter());
                names
                    .copied()
                    .find(|n| multiplexed_pnr(n) == Some(num))
                    .or_else(|| resolve_num_raw(abi, num))
            }
            Abi::X32 => resolve_num_raw(abi, if num >= 0 { num & !X32_SYSCALL_BIT } else { num }),
            _ => resolve_num_raw(abi, num),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::libseccomp::*;
    use super::*;

    fn dump(arch: &str) -> serde_json::Value {
        let path = format!("{}/testdata/syscalls-{arch}.json", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    /// Every name libseccomp's own resolver resolved, on each ABI of each guest arch
    /// (scripts/seccomp/generate, by libseccomp-golang's GetSyscallFromNameByArch),
    /// resolves here to the same number.
    #[test]
    fn names_resolve_as_libseccomp_resolves_them() {
        for (arch, abis) in [
            (
                "amd64",
                vec![("amd64", Abi::X86_64), ("x86", Abi::X86), ("x32", Abi::X32)],
            ),
            ("arm64", vec![("arm64", Abi::Aarch64), ("arm", Abi::Arm)]),
        ] {
            let d = dump(arch);
            for (key, abi) in abis {
                let table = d[key].as_object().unwrap();
                assert!(table.len() > 400, "{key}");
                for (name, n) in table {
                    assert_eq!(
                        i64::from(resolve_name(abi, name)),
                        n.as_i64().unwrap(),
                        "{key} {name}"
                    );
                }
            }
        }
        assert_eq!(resolve_name(Abi::X86_64, "no_such_syscall"), NR_SCMP_ERROR);
    }

    /// A number names one syscall on each ABI, so that libseccomp's first match is the
    /// only one (its pseudo numbers aside: -10191 is both switch_endian's and
    /// sys_debug_setcontext's, ppc's alone).
    #[test]
    fn a_number_names_one_syscall_per_abi() {
        for abi in [Abi::X86, Abi::X86_64, Abi::X32, Abi::Arm, Abi::Aarch64] {
            let mut seen = std::collections::BTreeMap::new();
            for r in rows() {
                if let Some(n) = raw_value(r, abi).filter(|n| *n >= 0) {
                    assert!(
                        seen.insert(n, r.name.as_str()).is_none(),
                        "{abi:?} {n} {}",
                        r.name
                    );
                }
            }
        }
    }
}
