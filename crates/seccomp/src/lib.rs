//! Docker's seccomp profiles (`docker run --security-opt seccomp=…`), resolved for a
//! container and compiled to the BPF filter its workload runs under.
//!
//! A profile is read and resolved as dockerd reads and resolves one, refused in its words,
//! and its rules kept as libseccomp keeps them, so that a profile written for Docker means
//! the same here; the filter is shards' own, read by the guest kernel's syscall tables and
//! free of the ways Docker's toolchain gets a profile wrong (compiler.rs). Docker mode, in the
//! tests, holds the shared parts to what Docker's runc loads, input by input.

pub mod bpf;
mod compiler;
pub mod db;
mod json;
pub mod profile;
mod tables;

pub use compiler::FILTER_FLAG_NEW_LISTENER;
pub use compiler::{ACT_ALLOW, ACT_KILL_PROCESS, ACT_KILL_THREAD, ACT_LOG, ACT_TRAP, Program, act_errno};
pub use profile::{Arch, BINDS, Container, DEFAULT, Kernel, MOUNTS};

/// The filter `profile` (a profile's JSON) makes for container `c`, or none where it asks
/// for none (no default action and no rules: unconfined).
pub fn compile(profile: &[u8], c: &Container<'_>) -> Result<Option<Program>, String> {
    let decoded = profile::decode(profile)?;
    let Some(spec) = profile::resolve(&decoded, c)? else {
        return Ok(None);
    };
    let Some(cfg) = profile::convert(&spec)? else {
        return Ok(None);
    };
    compiler::compile(&cfg, c.arch, compiler::Mode::Shards)?
        .program()
        .map(Some)
}

/// The filter of shards' own `profile`, whose notifications init answers (D124): loaded
/// with SECCOMP_FILTER_FLAG_NEW_LISTENER where it notifies.
pub fn compile_agent(profile: &[u8], c: &Container<'_>) -> Result<Option<Program>, String> {
    let decoded = profile::decode(profile)?;
    let Some(spec) = profile::resolve(&decoded, c)? else {
        return Ok(None);
    };
    let Some(cfg) = profile::convert(&spec)? else {
        return Ok(None);
    };
    compiler::compile(&cfg, c.arch, compiler::Mode::Agent)?
        .program()
        .map(Some)
}

#[cfg(test)]
mod oracle;

#[cfg(test)]
mod tests {
    use super::*;

    /// The host's network's filter (D124): bind(2) and listen(2) notified on every ABI the
    /// guest's kernel runs, x86's socketcall(2) for them too and for nothing else; every
    /// other call allowed; loaded for a listener.
    #[test]
    fn the_hosts_networks_binds_are_notified_on_every_abi() {
        const NOTIFY: u32 = 0x7fc0_0000;
        let call = |arch: u32, nr: u32, arg0: u64| bpf::Data {
            nr,
            arch,
            args: [arg0, 0, 0, 0, 0, 0],
            ..bpf::Data::default()
        };
        for (arch, abis) in [
            // x86_64's bind and listen, x32's, i386's own and through socketcall(SYS_BIND,
            // SYS_LISTEN); connect not.
            (
                Arch::Amd64,
                vec![
                    (0xc000_003e, 49, 0, NOTIFY),
                    (0xc000_003e, 50, 0, NOTIFY),
                    (0xc000_003e, 42, 0, ACT_ALLOW),
                    (0xc000_003e, 0x4000_0031, 0, NOTIFY),
                    (0xc000_003e, 0x4000_0032, 0, NOTIFY),
                    (0x4000_0003, 361, 0, NOTIFY),
                    (0x4000_0003, 363, 0, NOTIFY),
                    (0x4000_0003, 102, 2, NOTIFY),
                    (0x4000_0003, 102, 4, NOTIFY),
                    (0x4000_0003, 102, 3, ACT_ALLOW),
                ],
            ),
            // arm64's bind and listen, and arm's.
            (
                Arch::Arm64,
                vec![
                    (0xc000_00b7, 200, 0, NOTIFY),
                    (0xc000_00b7, 201, 0, NOTIFY),
                    (0xc000_00b7, 203, 0, ACT_ALLOW),
                    (0x4000_0028, 282, 0, NOTIFY),
                    (0x4000_0028, 284, 0, NOTIFY),
                ],
            ),
        ] {
            let c = Container {
                arch,
                caps: &[],
                kernel: Kernel(6, 18),
            };
            let p = compile_agent(BINDS, &c).unwrap().unwrap();
            assert_eq!(p.flags & FILTER_FLAG_NEW_LISTENER, FILTER_FLAG_NEW_LISTENER);
            for (a, nr, arg0, want) in abis {
                assert_eq!(
                    bpf::run(&p.insns, &call(a, nr, arg0)),
                    Some(want),
                    "{arch:?} {a:#x} {nr}"
                );
            }
            // A user's profile asking for notifications is refused, as before.
            assert!(compile(BINDS, &c).is_err());
        }
    }
}
