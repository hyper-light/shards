//! Whether a microVM's process runs: its CPU time from one look to the next, against the
//! most an idle microVM takes in as long (PM M357). The evidence by which a start, and a
//! workload a client waits on, is waited for as long as it runs, however slowly its host
//! runs it (D209), and a microVM that runs nothing is told apart.

use std::time::{Duration, Instant};

use super::{Usage, process_usage};

/// The most CPU time an idle microVM's process was measured to take, and the time it was
/// measured over: 63 ms in 10 s, PM M357's most of an idle run's microVM on windows-2025
/// (windows-2022's, 31 ms; macOS's cold boot and 10 s idle together, 56.6 ms at most, M66).
/// A cold boot alone keeps about one CPU busy to its end (M357: its microVM's CPU to its
/// start is its time), a hundred and sixty times as much.
pub const IDLE_VM_CPU: (Duration, Duration) = (Duration::from_millis(63), Duration::from_secs(10));

/// Whether a microVM's process ran over `window`, its CPU time `before` and `after` it, in
/// nanoseconds (none where it could not be read): more than an idle microVM takes in as
/// long ([`IDLE_VM_CPU`]). One that ran no more runs nothing a boot or a workload does: it
/// waits on something that has not come, or has stalled.
pub fn progressed(before: Option<u64>, after: Option<u64>, window: Duration) -> bool {
    let (Some(before), Some(after)) = (before, after) else {
        return false;
    };
    let ran = u128::from(after.saturating_sub(before));
    let (idle, over) = IDLE_VM_CPU;
    // ran / window > idle / over, without a division.
    ran.saturating_mul(over.as_nanos()) > idle.as_nanos().saturating_mul(window.as_nanos())
}

/// A process followed from one look at its usage to the next.
#[derive(Debug)]
pub struct Progress {
    pid: u32,
    /// Its usage at the look before, or why it could not be read then.
    last: Result<Usage, String>,
    at: Instant,
}

/// What one look found of a process since the look before ([`Progress::look`]): whether it
/// ran ([`progressed`]), its usage then and now, and the time between; and why its usage could
/// not be read now, or at the look before, where it could not: its process gone among the
/// reasons, which its caller tells from a failure by what else it reads of it.
#[derive(Debug, Clone)]
pub struct Look {
    pub ran: bool,
    pub before: Option<Usage>,
    pub now: Option<Usage>,
    pub over: Duration,
    pub unread: Option<String>,
}

impl Progress {
    /// Process `pid`, looked at now.
    pub fn of(pid: u32) -> Progress {
        Progress {
            pid,
            last: usage(pid),
            at: Instant::now(),
        }
    }

    /// The process followed.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// How long since it was last looked at.
    pub fn since(&self) -> Duration {
        self.at.elapsed()
    }

    /// Looks at it again: what it did since the look before.
    pub fn look(&mut self) -> Look {
        let (read, at) = (usage(self.pid), Instant::now());
        let over = at.saturating_duration_since(self.at);
        let unread = match (&self.last, &read) {
            (_, Err(why)) => Some(why.clone()),
            (Err(why), Ok(_)) => Some(format!("{why}, at the look before")),
            (Ok(_), Ok(_)) => None,
        };
        let (before, now) = (self.last.as_ref().ok().copied(), read.as_ref().ok().copied());
        let look = Look {
            ran: progressed(before.map(|u| u.cpu_ns), now.map(|u| u.cpu_ns), over),
            before,
            now,
            over,
            unread,
        };
        (self.last, self.at) = (read, at);
        look
    }
}

/// Process `pid`'s usage, or why it could not be read.
fn usage(pid: u32) -> Result<Usage, String> {
    process_usage(pid).map_err(|e| format!("process {pid}'s usage: {e}"))
}

/// Its CPU time and page faults, and what each grew by since the look before, for a log:
/// "its CPU time 32249 ms (+21671 ms in 10s), 812345 page faults (+5120)".
impl std::fmt::Display for Look {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Some(now) = self.now else {
            return match &self.unread {
                Some(why) => write!(f, "its CPU time unread: {why}"),
                None => write!(f, "its CPU time unread"),
            };
        };
        let ms = |ns: u64| ns / 1_000_000;
        write!(f, "its CPU time {} ms", ms(now.cpu_ns))?;
        if let Some(before) = self.before {
            write!(
                f,
                " (+{} ms in {:?})",
                ms(now.cpu_ns.saturating_sub(before.cpu_ns)),
                Duration::from_secs(self.over.as_secs())
            )?;
        }
        write!(f, ", {} page faults", now.faults)?;
        if let Some(before) = self.before {
            write!(f, " (+{})", now.faults.saturating_sub(before.faults))?;
        }
        // Its usage unread at the look before: what it grew by is unknown, and why is said.
        if let Some(why) = &self.unread {
            write!(f, "; {why}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A process that runs a CPU and touches pages it never had is seen to run, and its
    /// look says its CPU time and its page faults, each with what it grew by.
    #[test]
    fn a_busy_process_is_seen_running_with_its_cpu_and_faults() {
        let me = std::process::id();
        let mut progress = Progress::of(me);
        let began = Instant::now();
        let mut touched = Vec::new();
        while began.elapsed() < Duration::from_secs(1) {
            // A MiB at a time, each of its pages written: faults this process takes.
            let mut mib = vec![0u8; 1 << 20];
            for page in mib.chunks_mut(4096) {
                page[0] = 1;
            }
            touched.push(std::hint::black_box(mib));
            if touched.len() > 64 {
                touched.clear();
            }
        }
        let look = progress.look();
        assert!(look.ran, "{look}");
        let (before, now) = (look.before.unwrap(), look.now.unwrap());
        assert!(now.faults > before.faults, "{look}");
        let said = look.to_string();
        assert!(said.starts_with("its CPU time "), "{said}");
        assert!(said.contains(" ms in 1s)"), "{said}");
        assert!(said.contains(" page faults (+"), "{said}");
    }

    /// A process whose usage cannot be read is never seen to run, and says why, as its look
    /// and the look after (the person's rule of 2026-11-18: no failure taken for nothing).
    #[test]
    fn a_process_unread_never_runs_and_says_why() {
        let mut progress = Progress::of(u32::MAX);
        let look = progress.look();
        assert!(!look.ran);
        let said = look.to_string();
        assert!(
            said.starts_with("its CPU time unread: process 4294967295's usage: "),
            "{said}"
        );
        assert_eq!(look.unread.as_deref(), said.strip_prefix("its CPU time unread: "));
    }

    /// A process whose usage could not be read as it began to be followed says so at the look
    /// after, which reads it: what it grew by is unknown, never taken for none.
    #[test]
    fn a_usage_unread_at_the_look_before_is_said() {
        let mut progress = Progress::of(std::process::id());
        progress.last = Err("process 1's usage: refused".to_string());
        let look = progress.look();
        assert!(
            !look.ran && look.before.is_none() && look.now.is_some(),
            "{look:?}"
        );
        assert_eq!(
            look.unread.as_deref(),
            Some("process 1's usage: refused, at the look before")
        );
        let said = look.to_string();
        assert!(
            said.ends_with(" page faults; process 1's usage: refused, at the look before"),
            "{said}"
        );
    }
}
