//! The guest's BPF JIT, warmed before a template's snapshot (docs/design/architecture.md
//! D227, docs/research/platform-measurements.md M489): a child compiles one classic seccomp
//! program of a run's filter's size and ends with it, so that what the JIT allocates for a
//! program (kernel/bpf/core.c `bpf_jit_binary_pack_alloc`, arch/*/net/bpf_jit_comp.c) is
//! in the template's memory, freed, for each restored run's own filters to take.

use std::io;

/// The instructions of the program the warm-up compiles: Docker's default profile as
/// shards_seccomp compiles it for a container's default capabilities, 252 on arm64 and 397
/// on x86_64 under the pinned kernel (PM M489's ab.py, `filter`), rounded up to a power of
/// two, which the kernel's allocations for it round to as well.
pub(crate) const LEN: usize = 512;

// Long enough for the default profile's filter on both architectures, and within the
// kernel's limit for a classic program (BPF_MAXINSNS, include/uapi/linux/bpf_common.h).
const _: () = assert!(LEN >= 397 && LEN <= 4096);

/// seccomp(2)'s `SECCOMP_RET_ALLOW` (include/uapi/linux/seccomp.h).
const RET_ALLOW: u32 = 0x7fff_0000;
/// Classic BPF's opcodes (include/uapi/linux/bpf_common.h): `ld [k]` of a word of
/// `struct seccomp_data` (BPF_LD 0x00, BPF_W 0x00, BPF_ABS 0x20) and `ret #k` (BPF_RET 0x06,
/// BPF_K 0x00).
const LD_W_ABS: u16 = 0x20;
const RET_K: u16 = 0x06;

/// A classic seccomp program of `len` instructions (at least one) that allows every system
/// call: `len - 1` loads of the system call's number, then `ret ALLOW`. Each load is an
/// instruction of the eBPF the kernel makes of it (net/core/filter.c `bpf_convert_filter`)
/// and of the code its JIT emits.
pub(crate) fn program(len: usize) -> Vec<libc::sock_filter> {
    let load = libc::sock_filter {
        code: LD_W_ABS,
        jt: 0,
        jf: 0,
        k: 0,
    };
    let ret = libc::sock_filter {
        code: RET_K,
        jt: 0,
        jf: 0,
        k: RET_ALLOW,
    };
    let mut p = vec![load; len.saturating_sub(1)];
    p.push(ret);
    p
}

/// Compiles [`program`]`(LEN)` in a child of init that loads it as a run with
/// `no-new-privileges` loads its filter (no_new_privs, then seccomp(2)
/// SECCOMP_SET_MODE_FILTER) and ends, and waits for it. Call while the caller is
/// single-threaded.
pub(crate) fn warm() -> io::Result<()> {
    let filter = program(LEN);
    let prog = libc::sock_fprog {
        len: u16::try_from(filter.len()).map_err(io::Error::other)?,
        filter: filter.as_ptr().cast_mut(),
    };
    // SAFETY: fork(2) of single-threaded init; the child makes async-signal-safe calls on
    // memory made before the fork and ends with _exit(2).
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        // SAFETY: as above. no_new_privs, which lets any task load a filter, held by this
        // child alone, which ends with it.
        unsafe {
            let loaded = libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0
                && libc::syscall(
                    libc::SYS_seccomp,
                    1 as libc::c_long,
                    0 as libc::c_long,
                    &prog as *const libc::sock_fprog,
                ) == 0;
            libc::_exit(if loaded { 0 } else { 1 })
        }
    }
    let mut status = 0;
    // SAFETY: waits for the child just forked.
    if unsafe { libc::waitpid(pid, &mut status, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "its child ended with status {status:#x}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The program is as long as asked, allows every call, and every instruction before
    /// its last loads the system call's number (offset 0 of struct seccomp_data), which
    /// seccomp's checker takes (kernel/seccomp.c `seccomp_check_filter`: `BPF_LD|BPF_W|
    /// BPF_ABS` with an aligned offset within the data, and `BPF_RET|BPF_K`).
    #[test]
    fn the_program_loads_the_call_and_allows_it() {
        for len in [1, 2, LEN] {
            let p = program(len);
            assert_eq!(p.len(), len);
            let (last, loads) = p.split_last().unwrap();
            assert_eq!((last.code, last.k), (0x06, 0x7fff_0000));
            assert!(loads.iter().all(|i| (i.code, i.jt, i.jf, i.k) == (0x20, 0, 0, 0)));
        }
        assert_eq!(program(0).len(), 1);
    }

    /// A child loads the program and ends: the kernel takes it as a seccomp filter (this
    /// host's, its JIT's or its interpreter's), with no privilege.
    #[test]
    fn a_child_loads_the_program_and_ends() {
        warm().unwrap();
    }
}
