//! How long the daemon waits for a microVM's command to end once it has sent it SIGKILL,
//! and for a VM process to end once it has killed it (`stop`, `kill`, `rm -f`, `restart`
//! and the daemon's own stop; D27, D26, D217).
//!
//! A SIGKILLed command ends as its guest's kernel tears it down: each of its processes'
//! memory freed, its threads reaped, the rest of its PID namespace with it, its output
//! drained, and then its end said to the host (init's EXIT). Its VM ended sooner loses what
//! its guest had yet to write back (its writable layer's metadata not yet committed, a
//! joiner's layer kept in its memory, D119); ended later, it keeps its user waiting on a
//! command that will not end. So the wait is what that teardown costs this microVM, then
//! as long as it goes on: a fixed part, the slowest end measured of a command holding
//! little, and a part for each GiB its VM process holds as it is told, the slowest rate
//! measured (PM M480), so that one that holds 40 GiB is given the time its guest needs to
//! free them and one that holds 200 MiB is not kept a fixed 10 s; and past that, a slice
//! more each time its VM process is seen to have run what a teardown does, as a start is
//! waited for as it runs (D209). Only a VM that has run nothing of its teardown for a
//! slice is killed. What its VM process holds bounds what its guest can free: a guest's
//! pages are its VM process's (its resident memory, or macOS's footprint, which counts the
//! pages a guest holds). The same for a killed VM process, whose host frees what it held
//! (PM M481).

use std::time::Duration;

use shards_vmm::platform::{IDLE_VM_CPU, Look, Progress};

/// The slowest end measured of a SIGKILLed command holding little: PM M419's `sleep`,
/// whose SIGKILL was heard 9.4 s after it was sent, beside a gigabyte build on CI's
/// windows-2022 runner, past every end of M480's.
const END_FIXED: Duration = Duration::from_millis(9_400);
/// The slowest a guest tore a SIGKILLed command's memory down, for each GiB its VM process
/// held, of every shape and size measured, quiet and beside spinning threads (PM M480).
const END_PER_GIB: Duration = Duration::from_millis(1_000);
/// The slowest end measured of a VM process killed outright holding little (PM M419: 1.4 ms
/// beside the gigabyte build; M481).
const EXIT_FIXED: Duration = Duration::from_millis(100);
/// The slowest a host freed what a killed VM process held, for each GiB (PM M481).
const EXIT_PER_GIB: Duration = Duration::from_millis(1_000);

/// How long a teardown is given each time its VM process is seen to run past its wait:
/// the window an idle microVM's CPU was measured over ([`IDLE_VM_CPU`]), by which
/// [`Progress`] tells a VM that runs from one that runs nothing.
pub(super) const END_SLICE: Duration = IDLE_VM_CPU.1;

/// `fixed` and `per_gib` for each GiB of `held` bytes.
fn scaled(fixed: Duration, per_gib: Duration, held: u64) -> Duration {
    let extra = per_gib.as_nanos().saturating_mul(u128::from(held)) >> 30;
    fixed.saturating_add(Duration::from_nanos(u64::try_from(extra).unwrap_or(u64::MAX)))
}

/// How long a command whose VM process holds `held` bytes is given to end after SIGKILL
/// before its teardown is looked at ([`Teardown`]).
pub(super) fn end_wait(held: u64) -> Duration {
    scaled(END_FIXED, END_PER_GIB, held)
}

/// How long a killed VM process that held `held` bytes is given to end, before its end
/// is given up on.
pub(super) fn exit_wait(held: u64) -> Duration {
    scaled(EXIT_FIXED, EXIT_PER_GIB, held)
}

/// What VM process `pid` holds now, its guest's pages among them: its resident memory or
/// its footprint, the more; none where it cannot be read (one that has ended).
pub(super) fn held(pid: u32) -> u64 {
    shards_vmm::platform::process_usage(pid).map_or(0, |u| u.resident.max(u.footprint))
}

/// A SIGKILLed command's teardown in its own microVM, followed by what its VM process runs
/// from the SIGKILL on; none for a joiner (D119), whose VM is its provider's and runs the
/// provider's command too, so that it is held to its wait alone.
pub(super) struct Teardown(Option<Progress>);

impl Teardown {
    /// The teardown of a command told SIGKILL now, in the microVM of VM process `pid`.
    pub(super) fn of(pid: Option<u32>) -> Teardown {
        Teardown(pid.map(Progress::of))
    }

    /// Whether its VM process ran a teardown's work since it was last looked at, and what
    /// it ran, for the log: once its wait is up, it is waited on for another [`END_SLICE`]
    /// if so, and its VM killed if not.
    pub(super) fn runs(&mut self) -> Option<Look> {
        self.0.as_mut().map(Progress::look).filter(|look| look.ran)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The waits rest on what was measured: a microVM that holds nothing is given at least
    /// the slowest end measured of a command holding little beside the heaviest load
    /// (M419), and each GiB more the slowest teardown measured of a GiB (M480, M481), so
    /// that no measured end would have been cut short; and a teardown past its wait is
    /// looked at over the window idle microVMs were measured over.
    #[test]
    fn waits_are_the_measured_ends_of_what_a_microvm_holds() {
        const GIB: u64 = 1 << 30;
        assert_eq!(end_wait(0), END_FIXED);
        assert_eq!(end_wait(GIB), END_FIXED + END_PER_GIB);
        assert_eq!(end_wait(64 * GIB), END_FIXED + END_PER_GIB * 64);
        assert_eq!(end_wait(GIB / 2), END_FIXED + END_PER_GIB / 2);
        assert_eq!(exit_wait(0), EXIT_FIXED);
        assert_eq!(exit_wait(16 * GIB), EXIT_FIXED + EXIT_PER_GIB * 16);
        // M419's slowest end of a `sleep` heard, beside the gigabyte build.
        assert!(end_wait(0) >= Duration::from_millis(9_400));
        assert_eq!(END_SLICE, Duration::from_secs(10));
        // Nothing overflows: a host's whole address space held.
        assert!(end_wait(u64::MAX) > end_wait(GIB));
        assert!(exit_wait(u64::MAX) > exit_wait(GIB));
    }

    /// A teardown is waited on past its wait while its VM process runs: this process
    /// spinning a CPU is seen to run (one that sleeps is not: the daemon's
    /// `a_sleeping_process_runs_nothing_a_boot_does_and_a_busy_one_runs`), and a joiner's,
    /// which has no VM of its own to follow, never is.
    #[test]
    fn a_teardown_is_followed_while_its_vm_process_runs() {
        let mut busy = Teardown::of(Some(std::process::id()));
        let t0 = std::time::Instant::now();
        let mut x = 0u64;
        while t0.elapsed() < Duration::from_millis(300) {
            x = std::hint::black_box(x.wrapping_add(1));
        }
        assert!(busy.runs().is_some());
        assert!(Teardown::of(None).runs().is_none());
    }
}
