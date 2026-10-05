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

pub use compiler::{ACT_ALLOW, ACT_KILL_PROCESS, ACT_KILL_THREAD, ACT_LOG, ACT_TRAP, Program, act_errno};
pub use profile::{Arch, Container, DEFAULT, Kernel};

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

#[cfg(test)]
mod oracle;
