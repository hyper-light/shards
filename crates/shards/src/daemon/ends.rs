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
//! its host still frees of it. So is a run's network or share process once its VM has gone
//! and its spawner holds nothing of it (`netproc::reap`): it goes as its VM's links close,
//! and one that neither runs nor frees what it holds for a slice is ended (PM M546).
//!
//! The slice is this host's own: the slowest end of its kind this home's processes have
//! heard (the home's [`RECORD`]), kept as each is heard, so that a host whose guests stall
//! on its disk for 9 s before they end (PM M419) gives its teardowns that long; and never
//! less than the window over which [`Progress`] tells a microVM that runs from one that
//! runs nothing ([`IDLE_VM_CPU`]), which a home with no record yet takes alone. A daemon
//! and the run hosts of a home keep one record: each end kept is put with what the record
//! holds then.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use shards_vmm::platform::{IDLE_VM_CPU, Look, Progress};

use super::lock;

/// The home's record of this host's ends: `teardown MS`, `exit MS` and `helper MS`, the
/// slowest a SIGKILLed command's end was heard after its SIGKILL, a killed VM process's
/// after its kill, and a network or share process's after its VM had gone, in milliseconds.
pub(crate) const RECORD: &str = "ends";

/// The least slice: the window [`Progress`] judges a microVM's run over, the one its idle
/// microVMs were measured over (PM M357). A shorter one would take a tick of CPU time for
/// a run, or none for a stall.
const LEAST_SLICE: Duration = IDLE_VM_CPU.1;

/// Which end was heard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum End {
    /// A SIGKILLed command's.
    Teardown,
    /// A killed VM process's.
    Exit,
    /// A network or share process's, once its VM had gone and its spawner held nothing of
    /// it (`netproc::reap`).
    Helper,
}

/// This host's ends as a home keeps them ([`RECORD`]).
#[derive(Debug)]
pub(crate) struct Ends {
    path: PathBuf,
    /// The slowest of each heard: a teardown's and an exit's.
    slowest: Mutex<Slowest>,
}

/// The slowest end of each kind heard.
#[derive(Clone, Copy, Debug, Default)]
struct Slowest {
    teardown: Duration,
    exit: Duration,
    helper: Duration,
}

impl Slowest {
    fn of(&mut self, kind: End) -> &mut Duration {
        match kind {
            End::Teardown => &mut self.teardown,
            End::Exit => &mut self.exit,
            End::Helper => &mut self.helper,
        }
    }

    /// The record at `path` as it is now: none of a kind it holds no line of, or that is
    /// not there or not read.
    fn read(path: &Path) -> Slowest {
        let mut slowest = Slowest::default();
        for line in std::fs::read_to_string(path).unwrap_or_default().lines() {
            let (name, ms) = line.split_once(' ').unwrap_or((line, ""));
            let Ok(ms) = ms.trim().parse::<u64>() else {
                continue;
            };
            let ms = Duration::from_millis(ms);
            match name {
                "teardown" => slowest.teardown = ms,
                "exit" => slowest.exit = ms,
                "helper" => slowest.helper = ms,
                _ => {}
            }
        }
        slowest
    }

    /// Each kind's slower of `self`'s and `other`'s.
    fn with(self, other: Slowest) -> Slowest {
        Slowest {
            teardown: self.teardown.max(other.teardown),
            exit: self.exit.max(other.exit),
            helper: self.helper.max(other.helper),
        }
    }
}

impl Ends {
    /// The record kept in `home`, read now: none of a kind where it is not there or not
    /// read.
    pub(crate) fn kept_in(home: &Path) -> Ends {
        let path = home.join(RECORD);
        let slowest = Slowest::read(&path);
        Ends {
            path,
            slowest: Mutex::new(slowest),
        }
    }

    /// How long an end of `kind` is given before its work is looked at, and again each
    /// time it is seen to go on: the slowest of its kind this host has shown, never less
    /// than [`LEAST_SLICE`].
    pub(crate) fn slice(&self, kind: End) -> Duration {
        (*lock(&self.slowest).of(kind)).max(LEAST_SLICE)
    }

    /// An end of `kind` heard `took` after its signal: kept where it is the slowest yet,
    /// with what the home's record holds now (another process of the home, a daemon or a
    /// run's host, may have kept one since this read it), in memory and in the home:
    /// written whole, then renamed over the record, so that a reader finds the old record
    /// or the new. A record that cannot be written leaves this process's own, and says why.
    pub(crate) fn heard(&self, kind: End, took: Duration) -> Result<(), String> {
        let mut slowest = lock(&self.slowest);
        *slowest = slowest.with(Slowest::read(&self.path));
        let was = slowest.of(kind);
        if took <= *was {
            return Ok(());
        }
        *was = took;
        let text = format!(
            "teardown {}\nexit {}\nhelper {}\n",
            slowest.teardown.as_millis(),
            slowest.exit.as_millis(),
            slowest.helper.as_millis()
        );
        let new = self.path.with_extension("new");
        std::fs::write(&new, text)
            .and_then(|()| std::fs::rename(&new, &self.path))
            .map_err(|e| format!("keeping this host's ends in {}: {e}", self.path.display()))
    }
}

/// A SIGKILLed command's teardown in its own microVM, a killed VM process's end, or a
/// network or share process's once its VM has gone, followed by what its process runs and
/// frees from then on; none for a joiner (D119), whose VM is its provider's and runs the
/// provider's command too, so that it is held to one slice.
pub(crate) struct Followed(Option<Progress>);

impl Followed {
    /// The end, from now, of process `pid`'s work.
    pub(crate) fn of(pid: Option<u32>) -> Followed {
        Followed(pid.map(Progress::of))
    }

    /// Whether its VM process went on with its end since it was last looked at: ran more
    /// than an idle microVM does, or held less than it did, its host freeing it; and what
    /// it did, for the log. Waited on for another slice if so; given up on if not.
    pub(crate) fn goes_on(&mut self) -> Option<Look> {
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
        assert_eq!(ends.slice(End::Helper), LEAST_SLICE);
        assert!(!home.join(RECORD).exists());
        ends.heard(End::Teardown, Duration::from_millis(900)).unwrap();
        assert_eq!(ends.slice(End::Teardown), LEAST_SLICE, "below the least slice");
        ends.heard(End::Teardown, Duration::from_millis(19_383)).unwrap();
        ends.heard(End::Teardown, Duration::from_millis(12_000)).unwrap();
        ends.heard(End::Exit, Duration::from_millis(1_400)).unwrap();
        assert_eq!(ends.slice(End::Teardown), Duration::from_millis(19_383));
        assert_eq!(
            std::fs::read_to_string(home.join(RECORD)).unwrap(),
            "teardown 19383\nexit 1400\nhelper 0\n"
        );
        let next = Ends::kept_in(&home);
        assert_eq!(next.slice(End::Teardown), Duration::from_millis(19_383));
        assert_eq!(next.slice(End::Exit), LEAST_SLICE);
        // A record it cannot read is no record.
        std::fs::write(home.join(RECORD), "teardown many\n").unwrap();
        assert_eq!(Ends::kept_in(&home).slice(End::Teardown), LEAST_SLICE);
    }

    /// Two processes of a home keep one record: a run's host keeps its network process's
    /// end, and the daemon, which read the record before, keeps that end with its own as
    /// it keeps the next; neither's slower end is lost to the other's write. A record the
    /// home cannot take is said, not hidden.
    #[test]
    fn a_homes_processes_keep_one_record_of_its_ends() {
        let home = shards_testdir::TempDir::new("ends-shared").unwrap();
        let daemon = Ends::kept_in(&home);
        let host = Ends::kept_in(&home);
        host.heard(End::Helper, Duration::from_millis(12_500)).unwrap();
        daemon.heard(End::Exit, Duration::from_millis(11_000)).unwrap();
        assert_eq!(daemon.slice(End::Helper), Duration::from_millis(12_500));
        assert_eq!(
            std::fs::read_to_string(home.join(RECORD)).unwrap(),
            "teardown 0\nexit 11000\nhelper 12500\n"
        );
        let gone = shards_testdir::TempDir::new("ends-gone").unwrap();
        let lost = Ends::kept_in(&gone.join("no-such-home"));
        let said = lost.heard(End::Helper, Duration::from_secs(11)).unwrap_err();
        assert!(said.starts_with("keeping this host's ends in "), "{said}");
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
