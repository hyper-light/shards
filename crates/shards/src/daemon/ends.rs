//! How long the daemon waits for a microVM's command to end once it has sent it SIGKILL,
//! and for a VM process to end once it has killed it (`stop`, `kill`, `rm -f`, `restart`
//! and the daemon's own stop; D27, D26, D217): as long as the work goes on, by what this
//! host has shown, with no figure of shards' own.
//!
//! A SIGKILLed command ends as its guest's kernel tears it down: each of its processes'
//! memory freed, its threads reaped, the rest of its PID namespace with it, its output
//! drained, and then its end said to the host (init's EXIT), work its VM process runs.
//! Its VM ended sooner loses what its guest had yet to write back (its writable layer's
//! metadata not yet committed, a joiner's layer kept in its memory, D119); a VM never ended
//! keeps its user waiting on a guest that cannot end it. So the teardown is followed as a
//! start is (D209): looked at once a slice has passed, and each slice after while its VM
//! process is seen to have run more than an idle microVM does, and its VM killed only once
//! it has run nothing for a slice. A killed VM process is followed the same way, by what
//! its host still frees of it.
//!
//! The slice is this host's own: the slowest end this home's daemons have heard of either
//! kind (the home's [`RECORD`]), kept as each is heard, so that a host whose guests stall
//! on its disk for 9 s before they end (PM M419) gives its teardowns that long; and never
//! less than the window over which [`Progress`] tells a microVM that runs from one that
//! runs nothing ([`IDLE_VM_CPU`]), which a home with no record yet takes alone.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use shards_vmm::platform::{IDLE_VM_CPU, Look, Progress};

use super::lock;

/// The home's record of this host's ends: `teardown MS` and `exit MS`, the slowest a
/// SIGKILLed command's end was heard after its SIGKILL and a killed VM process's after its
/// kill, in milliseconds, each line there once one has been heard.
pub(super) const RECORD: &str = "ends";

/// The least slice: the window [`Progress`] judges a microVM's run over, the one its idle
/// microVMs were measured over (PM M357). A shorter one would take a tick of CPU time for
/// a run, or none for a stall.
const LEAST_SLICE: Duration = IDLE_VM_CPU.1;

/// Which end was heard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum End {
    /// A SIGKILLed command's.
    Teardown,
    /// A killed VM process's.
    Exit,
}

/// This host's ends as a home keeps them ([`RECORD`]).
pub(super) struct Ends {
    path: PathBuf,
    /// The slowest of each heard: a teardown's and an exit's.
    slowest: Mutex<Slowest>,
}

/// The slowest end of each kind heard.
#[derive(Clone, Copy, Default)]
struct Slowest {
    teardown: Duration,
    exit: Duration,
}

impl Slowest {
    fn of(&mut self, kind: End) -> &mut Duration {
        match kind {
            End::Teardown => &mut self.teardown,
            End::Exit => &mut self.exit,
        }
    }
}

impl Ends {
    /// The record kept in `home`, read now: none of either where it is not there or not
    /// read.
    pub(super) fn kept_in(home: &Path) -> Ends {
        let path = home.join(RECORD);
        let mut slowest = Slowest::default();
        for line in std::fs::read_to_string(&path).unwrap_or_default().lines() {
            let (name, ms) = line.split_once(' ').unwrap_or((line, ""));
            let Ok(ms) = ms.trim().parse::<u64>() else {
                continue;
            };
            match name {
                "teardown" => slowest.teardown = Duration::from_millis(ms),
                "exit" => slowest.exit = Duration::from_millis(ms),
                _ => {}
            }
        }
        Ends {
            path,
            slowest: Mutex::new(slowest),
        }
    }

    /// How long an end of `kind` is given before its work is looked at, and again each
    /// time it is seen to go on: the slowest of its kind this host has shown, never less
    /// than [`LEAST_SLICE`].
    pub(super) fn slice(&self, kind: End) -> Duration {
        (*lock(&self.slowest).of(kind)).max(LEAST_SLICE)
    }

    /// An end of `kind` heard `took` after its signal: kept where it is the slowest yet,
    /// in memory and in the home (written whole, then renamed over the record, so that a
    /// reader finds the old record or the new). A record that cannot be written leaves
    /// this daemon's own.
    pub(super) fn heard(&self, kind: End, took: Duration) {
        let mut slowest = lock(&self.slowest);
        let was = slowest.of(kind);
        if took <= *was {
            return;
        }
        *was = took;
        let text = format!(
            "teardown {}\nexit {}\n",
            slowest.teardown.as_millis(),
            slowest.exit.as_millis()
        );
        let new = self.path.with_extension("new");
        if let Err(e) = std::fs::write(&new, text).and_then(|()| std::fs::rename(&new, &self.path)) {
            super::log(format!(
                "keeping this host's ends in {}: {e}",
                self.path.display()
            ));
        }
    }
}

/// A SIGKILLed command's teardown in its own microVM, or a killed VM process's end,
/// followed by what its VM process runs and frees from then on; none for a joiner (D119),
/// whose VM is its provider's and runs the provider's command too, so that it is held to
/// one slice.
pub(super) struct Followed(Option<Progress>);

impl Followed {
    /// The end, from now, of VM process `pid`'s work.
    pub(super) fn of(pid: Option<u32>) -> Followed {
        Followed(pid.map(Progress::of))
    }

    /// Whether its VM process went on with its end since it was last looked at: ran more
    /// than an idle microVM does, or held less than it did, its host freeing it; and what
    /// it did, for the log. Waited on for another slice if so; given up on if not.
    pub(super) fn goes_on(&mut self) -> Option<Look> {
        let look = self.0.as_mut()?.look();
        let freed = matches!((look.before, look.now), (Some(b), Some(n)) if n.resident < b.resident);
        (look.ran || freed).then_some(look)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A home with no record gives each end the least slice, the window its progress is
    /// judged over; each end heard slower than the slowest is kept, in the home too, and
    /// read back by the next daemon; a faster one changes nothing.
    #[test]
    fn a_hosts_ends_are_learned_and_kept_in_its_home() {
        let home = shards_testdir::TempDir::new("ends").unwrap();
        let ends = Ends::kept_in(&home);
        assert_eq!(LEAST_SLICE, Duration::from_secs(10));
        assert_eq!(ends.slice(End::Teardown), LEAST_SLICE);
        assert_eq!(ends.slice(End::Exit), LEAST_SLICE);
        assert!(!home.join(RECORD).exists());
        ends.heard(End::Teardown, Duration::from_millis(900));
        assert_eq!(ends.slice(End::Teardown), LEAST_SLICE, "below the least slice");
        ends.heard(End::Teardown, Duration::from_millis(19_383));
        ends.heard(End::Teardown, Duration::from_millis(12_000));
        ends.heard(End::Exit, Duration::from_millis(1_400));
        assert_eq!(ends.slice(End::Teardown), Duration::from_millis(19_383));
        assert_eq!(
            std::fs::read_to_string(home.join(RECORD)).unwrap(),
            "teardown 19383\nexit 1400\n"
        );
        let next = Ends::kept_in(&home);
        assert_eq!(next.slice(End::Teardown), Duration::from_millis(19_383));
        assert_eq!(next.slice(End::Exit), LEAST_SLICE);
        // A record it cannot read is no record.
        std::fs::write(home.join(RECORD), "teardown many\n").unwrap();
        assert_eq!(Ends::kept_in(&home).slice(End::Teardown), LEAST_SLICE);
    }

    /// An end is waited on past its slice while its VM process runs: this process
    /// spinning a CPU is seen to go on (one that sleeps is not: the daemon's
    /// `a_sleeping_process_runs_nothing_a_boot_does_and_a_busy_one_runs`), and a joiner's,
    /// which has no VM of its own to follow, never is.
    #[test]
    fn an_end_is_followed_while_its_vm_process_runs() {
        let mut busy = Followed::of(Some(std::process::id()));
        let t0 = std::time::Instant::now();
        let mut x = 0u64;
        while t0.elapsed() < Duration::from_millis(300) {
            x = std::hint::black_box(x.wrapping_add(1));
        }
        assert!(busy.goes_on().is_some());
        assert!(Followed::of(None).goes_on().is_none());
    }
}
