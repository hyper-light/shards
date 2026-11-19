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
//! keeps its user waiting on a guest that cannot end it. So the teardown is followed by
//! D215's judge, the person's rule for waits (2026-11-13): looked at once a slice has
//! passed, and each slice after while its VM process is seen to go on with it (any CPU at
//! all, as D215's readers see it, or memory its host frees of it, or its host holding it
//! from running, which is no stall), and its VM killed only once it has done none of these
//! for a slice, or is stopped or frozen, or has done more work than any teardown of this
//! home's took (C*, D215's rule for starts: [`Followed::past`]). A killed VM process is
//! followed the same way. So is a run's network or share process once its VM has gone and
//! its spawner holds nothing of it (`netproc::reap`): it goes as its VM's links close, and
//! one that neither runs nor frees what it holds for a slice is ended (PM M546). A joiner's teardown (D119) runs in its
//! provider's microVM, whose VM process also runs the provider's own command and ticks: it
//! goes on only by that process's work beyond an idle microVM's (PM M357,
//! [`Followed::of_provider`]).
//!
//! The slice is this host's own, W*: the slowest end of its kind this home's processes have
//! heard (the home's [`RECORD`]), kept as each is heard, so that a host whose guests stall on
//! its disk for 9 s before they end (PM M419) gives its teardowns that long. A home with none
//! of a kind yet has no slice: its end is followed by its process's liveness alone, to the
//! end that teaches it. A daemon and the run hosts of a home keep one record: each end kept
//! is put with what the record holds then.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use shards_vmm::platform::{Look, Progress, progressed};

use super::lock;

/// The home's record of this host's ends: `teardown MS`, `exit MS`, `helper MS` and `start MS`,
/// the slowest a SIGKILLed command's end was heard after its SIGKILL, a killed VM process's
/// after its kill, a network or share process's after its VM had gone, and a daemon's start
/// from its home's lock to its slots' lists, in milliseconds; and, once a teardown's work was
/// read through its end, `teardown-work KEY WORK ANSWERS`, the most work a microVM's own VM
/// process did from its command's SIGKILL to that command's end (C*), counted as KEY says
/// ([`work_key`]: CPU nanoseconds, on Windows cycles, and the CPU's model), and how many
/// teardowns taught it ([`MostWork`]).
pub(crate) const RECORD: &str = "ends";

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
    /// The end of a daemon's start: its home's lock taken to its slots' lists made, which a
    /// daemon waiting for the lock sees as the home served (`take_lock`, runs-11 TAKEOVER).
    Start,
}

/// This host's ends as a home keeps them ([`RECORD`]).
#[derive(Debug)]
pub(crate) struct Ends {
    path: PathBuf,
    /// The slowest of each heard: a teardown's and an exit's.
    slowest: Mutex<Slowest>,
}

/// The slowest end of each kind heard, and the most work a teardown took.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Slowest {
    teardown: Duration,
    exit: Duration,
    helper: Duration,
    start: Duration,
    teardown_work: MostWork,
}

/// The most work a microVM's teardown took (C*), its VM process's work (`shards_api::Work`)
/// from its command's SIGKILL to that command's end, and how many teardowns taught it, as
/// D215's judge keeps a kind's (`shards_api::Rated`): each teardown taught adds one, and a
/// record merged in counts the more of the two, never their sum. It refuses once
/// `shards_api::SPREAD` have taught it, one teardown's most work being that one's (D217,
/// ROOT's ruling of 2026-11-18).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct MostWork {
    most: shards_api::Work,
    answers: u64,
}

/// The first word of [`MostWork`]'s line in the record.
const WORK: &str = "teardown-work";

impl MostWork {
    /// `KEY WORK ANSWERS` as the record keeps them, of this host's key ([`work_key`]): none
    /// unless all are read whole. A line of another key is work counted in another unit or on
    /// another CPU model, which weighs nothing here; one kept before lines were keyed (`NS
    /// ANSWERS`: CPU nanoseconds, on Windows whole clock ticks' (CI 44347282760)) is read as
    /// none, its C* taught anew.
    fn read(text: &str) -> Option<MostWork> {
        let mut words = text.split_whitespace();
        let (Some(key), Some(work), Some(answers), None) =
            (words.next(), words.next(), words.next(), words.next())
        else {
            return None;
        };
        if key != work_key() {
            return None;
        }
        Some(MostWork {
            most: shards_api::Work::of(work.parse().ok()?),
            answers: answers.parse().ok()?,
        })
    }
}

/// What this host's teardown work is counted in, as its line in the home's record names it:
/// its unit (`shards_api::Work::UNIT`: CPU nanoseconds, on Windows cycles) and its CPU's model
/// (`shards_vmm::vm::limits::HostModel`'s key, as D261's records and the tests' kept ratings
/// key one), `UNIT:MODEL`. Cycles are no time ("Do not attempt to convert the CPU clock cycles
/// returned by QueryThreadCycleTime to elapsed time", Microsoft Learn), and their rate is their
/// CPU's: a home read on another host weighs nothing of its work.
fn work_key() -> &'static str {
    static KEY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    KEY.get_or_init(|| {
        format!(
            "{}:{}",
            shards_api::Work::UNIT,
            shards_vmm::vm::limits::HostModel::here().key()
        )
    })
}

impl Slowest {
    fn of(&mut self, kind: End) -> &mut Duration {
        match kind {
            End::Teardown => &mut self.teardown,
            End::Exit => &mut self.exit,
            End::Helper => &mut self.helper,
            End::Start => &mut self.start,
        }
    }

    /// The record at `path` as it is now, its newest version (platform `write_in`, which on
    /// Windows lands `ends.~N` beside it and removes the plain name): none of a kind it
    /// holds no line of, or that is not there or not read.
    fn read(path: &Path) -> Slowest {
        let mut slowest = Slowest::default();
        for line in newest(path).unwrap_or_default().lines() {
            let (name, rest) = line.split_once(' ').unwrap_or((line, ""));
            if name == WORK {
                if let Some(work) = MostWork::read(rest) {
                    slowest.teardown_work = work;
                }
                continue;
            }
            let Ok(ms) = rest.trim().parse::<u64>() else {
                continue;
            };
            let ms = Duration::from_millis(ms);
            match name {
                "teardown" => slowest.teardown = ms,
                "exit" => slowest.exit = ms,
                "helper" => slowest.helper = ms,
                "start" => slowest.start = ms,
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
            start: self.start.max(other.start),
            teardown_work: MostWork {
                most: self.teardown_work.most.max(other.teardown_work.most),
                answers: self.teardown_work.answers.max(other.teardown_work.answers),
            },
        }
    }
}

/// The text of the newest version of the file at `path` (runs-12): what a write there
/// through `platform::write_in` made, on every platform.
pub(crate) fn newest(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let dir = home_of(path);
    let dir = shards_vmm::platform::open_dir(dir).ok()?;
    let file = shards_vmm::platform::open_newest_in(&dir, name).ok()?;
    std::io::read_to_string(file).ok()
}

/// The directory `path`, a home's record, is in.
fn home_of(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
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
    /// time it is seen to go on: the slowest of its kind this host has shown (W*); none
    /// where it has shown none, its end then followed by its process's liveness alone.
    pub(crate) fn slice(&self, kind: End) -> Option<Duration> {
        let slowest = *lock(&self.slowest).of(kind);
        (!slowest.is_zero()).then_some(slowest)
    }

    /// An end of `kind` heard `took` after its signal: kept where it is the slowest yet, in
    /// memory ([`learn`](Self::learn)) and in the home ([`keep`](Self::keep)).
    pub(crate) fn heard(&self, kind: End, took: Duration) -> Result<(), String> {
        self.learn(kind, took);
        self.keep()
    }

    /// An end of `kind` heard `took` after its signal, kept in this process's memory where it
    /// is the slowest yet: its kind's slice from now. It waits on no file: a stop of many
    /// keeps the ends it hears in the home once, as it ends (`end_all`), where a keep of
    /// each, two flushes of the disk on macOS (`platform::write_in`), held up every end
    /// after it (D217).
    pub(crate) fn learn(&self, kind: End, took: Duration) {
        let mut slowest = lock(&self.slowest);
        let was = slowest.of(kind);
        *was = (*was).max(took);
    }

    /// A microVM's teardown ended, its VM process having done `work` since its command's
    /// SIGKILL, read through its end ([`Followed::worked`]): kept in this process's memory as
    /// its C* where it is the most yet, one more teardown counted, and in the home at the next
    /// [`keep`](Self::keep). One whose work read none teaches nothing: no work holds no bound
    /// (D215, `Record::teach`, CI 44076016255's Windows ticks).
    pub(crate) fn learn_work(&self, work: shards_api::Work) {
        if work.is_zero() {
            return;
        }
        let mut slowest = lock(&self.slowest);
        let kept = &mut slowest.teardown_work;
        kept.most = kept.most.max(work);
        kept.answers = kept.answers.saturating_add(1);
    }

    /// The most work a microVM's teardown has taken (C*), past which one that has not ended is
    /// given up whatever its rate: none until `shards_api::SPREAD` teardowns have taught it
    /// (D217, by D215's rule for starts, ROOT's ruling of 2026-11-18).
    pub(crate) fn most_work(&self) -> Option<shards_api::Work> {
        let kept = lock(&self.slowest).teardown_work;
        (kept.answers >= shards_api::SPREAD).then_some(kept.most)
    }

    /// What this process has learned, put with what the home's record holds now (another
    /// process of the home, a daemon or a run's host, may have kept one since this read it),
    /// in memory and in the home where either is slower: written whole, then renamed over
    /// the record, so that a reader finds the old record or the new. The memory's lock is
    /// not held over the record's lock or its flush, which a slice wanted meanwhile would
    /// wait on. A record that cannot be written leaves this process's own, for the next keep,
    /// and says why.
    pub(crate) fn keep(&self) -> Result<(), String> {
        let fail = |e| format!("keeping this host's ends in {}: {e}", self.path.display());
        // Every process rereads and merges under this stable lock, not the replaced
        // record's inode. Locked releases flock before close (M244).
        let _writing =
            shards_image::store::Locked::exclusive(&self.path.with_extension("lock")).map_err(fail)?;
        let recorded = Slowest::read(&self.path);
        let kept = {
            let mut slowest = lock(&self.slowest);
            *slowest = slowest.with(recorded);
            *slowest
        };
        if kept == recorded {
            return Ok(());
        }
        let mut text = format!(
            "teardown {}\nexit {}\nhelper {}\nstart {}\n",
            whole_ms(kept.teardown),
            whole_ms(kept.exit),
            whole_ms(kept.helper),
            whole_ms(kept.start)
        );
        if kept.teardown_work.answers > 0 {
            text.push_str(&format!(
                "{WORK} {} {} {}\n",
                work_key(),
                kept.teardown_work.most.count(),
                kept.teardown_work.answers
            ));
        }
        let dir = shards_vmm::platform::open_dir(home_of(&self.path)).map_err(fail)?;
        shards_vmm::platform::write_in(&dir, RECORD, text.as_bytes()).map_err(fail)
    }
}

/// Processes whose usage reads fail at their follows' looks in this test process, each with
/// what its read says: a look that fails, which no host fails on purpose, followed through the
/// daemon's callers to its log ([`GoesOn::Unread`]).
#[cfg(test)]
pub(crate) static UNREADABLE: Mutex<Vec<(u32, String)>> = Mutex::new(Vec::new());

/// A SIGKILLed command's teardown in its own microVM or, a joiner's (D119), in its provider's,
/// a killed VM process's end, or a network or share process's once its VM has gone, followed
/// by what its process runs and frees from then on.
#[derive(Debug)]
pub(crate) struct Followed {
    /// Its CPU and what its host does to it, as D215's readers see them, and when they last
    /// read it.
    watch: Option<shards_api::Watch>,
    looked: Instant,
    /// What it holds, which its host frees as its end goes on, and what it did, for the log.
    usage: Option<Progress>,
    /// Whether its process is a joiner's provider's ([`Followed::of_provider`]).
    provider: bool,
    /// Its work as it began to be followed: what its work since is counted from.
    from: Option<shards_api::Work>,
    /// Why its process could not be followed as it began to be, where it could not: its work
    /// is then unread, which [`work`](Self::work) says, never taken for none.
    unread: Option<String>,
}

impl Followed {
    /// The end, from now, of process `pid`'s work. A process none has (it has gone) has no
    /// work to follow; one that could not be read says why ([`work`](Self::work)).
    pub(crate) fn of(pid: Option<u32>) -> Followed {
        let (watch, unread) = match pid.map(Self::watch) {
            None => (None, None),
            Some(Ok(watch)) => (watch, None),
            Some(Err(why)) => (None, Some(why)),
        };
        Followed {
            from: watch.as_ref().map(shards_api::Watch::work),
            watch,
            looked: Instant::now(),
            usage: pid.map(Progress::of),
            provider: false,
            unread,
        }
    }

    /// Process `pid` followed by D215's readers: none where no process has it; why not where
    /// it could not be read.
    fn watch(pid: u32) -> Result<Option<shards_api::Watch>, String> {
        let Some(followed) = shards_api::Followed::of_pid(pid).map_err(|e| format!("process {pid}: {e}"))?
        else {
            return Ok(None);
        };
        shards_api::Watch::of(followed, &shards_api::Listing::new())
            .map_err(|e| format!("process {pid}'s CPU: {e}"))
    }

    /// Where its VM process, a microVM's own, has done more work since it began to be
    /// followed, by its last look, than `most`, the most any of its kind took (C*,
    /// [`Ends::most_work`]): that work, and C*. Such an end is given up whatever its rate,
    /// its VM process killed, as D215 gives up a start (`Bound::Work`, (b); D217, ROOT's
    /// ruling of 2026-11-18): CI 44245219858's nine VMs, their guests spinning, ran at their
    /// host's share for 348 s and were followed for as long, a stop with them. None where no
    /// C* is learned, for a joiner's provider, whose work is the provider's own too
    /// ([`Followed::of_provider`]), and where it has done no more; why not where its work is
    /// unread ([`work`](Self::work)): its caller says it, and takes nothing of the bound from
    /// that look.
    pub(crate) fn past(
        &self,
        most: Option<shards_api::Work>,
    ) -> Result<Option<(shards_api::Work, shards_api::Work)>, String> {
        let Some(most) = most else {
            return Ok(None);
        };
        Ok(self.work()?.filter(|&work| work > most).map(|work| (work, most)))
    }

    /// The work its VM process, a microVM's own, has done since it began to be followed, by
    /// its last look: none for a joiner's provider ([`Followed::of_provider`]), nor where no
    /// process was there to follow; why not where its process could not be read as it began
    /// to be followed.
    pub(crate) fn work(&self) -> Result<Option<shards_api::Work>, String> {
        if self.provider {
            return Ok(None);
        }
        match (&self.watch, self.from, &self.unread) {
            (Some(watch), Some(from), _) => Ok(Some(watch.work().saturating_sub(from))),
            (_, _, Some(why)) => Err(why.clone()),
            _ => Ok(None),
        }
    }

    /// The work its VM process has done since it began to be followed, read through its end
    /// heard: looked at once more. None for a joiner's provider, where no process was there to
    /// follow, and where its process has gone since (reaped before its last CPU was read: no
    /// work read through its end); why not where it could not be read. What a teardown teaches
    /// of C* ([`Ends::learn_work`]).
    pub(crate) fn worked(&mut self) -> Result<Option<shards_api::Work>, String> {
        if self.provider {
            return Ok(None);
        }
        if let Some(why) = &self.unread {
            return Err(why.clone());
        }
        let (Some(watch), Some(from)) = (self.watch.as_mut(), self.from) else {
            return Ok(None);
        };
        match watch.look(&shards_api::Listing::new()) {
            Ok(shards_api::Found::Gone) => Ok(None),
            Ok(_) => Ok(Some(watch.work().saturating_sub(from))),
            Err(e) => Err(format!("its VM process's CPU at its end: {e}")),
        }
    }

    /// A joiner's teardown (D119), from now, in its provider's microVM, whose VM process is
    /// `pid` (ROOT's ruling, 2026-11-18). That process runs the provider's own command and
    /// ticks whether or not the joiner's end goes on. So it goes on by work beyond an idle
    /// microVM's (`IDLE_VM_CPU`, PM M357: 63 ms of CPU in 10 s, the most an idle run's microVM
    /// took, on windows-2025; 31 ms on windows-2022) over the time in a slice its host let it
    /// run: the slice less its threads' wait for a CPU in it (`Watch::waited`), so that a
    /// working provider its host starves shows its own rate (case 5 under the parallel suite:
    /// 2 to 24 ms of CPU a 50 ms slice, some slices below an idle microVM's rate, its joiner's
    /// kill given up). A slice it ran nothing in, its host holding it (D215's `Held`), goes on
    /// too, as no stall of its own. What its host frees of it is no sign: an idle provider's
    /// give-back frees it too. Before, a joiner's teardown was followed by nothing: held to one
    /// slice, then to a killed VM process's, and given up while its provider's guest still
    /// tore it down (CI 44245219858, x86_64 gnu: `stop -t 1` of a joiner exited 1, 5.98 ms into
    /// the second slice).
    pub(crate) fn of_provider(pid: u32) -> Followed {
        Followed {
            provider: true,
            ..Followed::of(Some(pid))
        }
    }

    /// What its process did with its end since it was last looked at ([`GoesOn`]): went on
    /// where it ran any CPU, or held less than it did, its host freeing it, or its host held it
    /// from running, which is no stall of its own (ROOT's ruling, 2026-11-13), and what it did,
    /// for the log; still where it did none of these, or is stopped or frozen, or has gone;
    /// unread where it lives and could not be read, then or as it began to be followed, which
    /// its caller gives up on as one still, saying why (the person's rule of 2026-11-18: no
    /// failure taken for a stillness, nor waited on for another slice; as D215's judge gives up
    /// a process it cannot read, `Refusal::Process`). A slice shorter than its CPU reading shows
    /// (`Watch::resolution`, a clock tick) is no evidence of a stall: it goes on, the look
    /// before kept, until the slices since add up to one.
    pub(crate) fn goes_on(&mut self) -> GoesOn {
        if let Some(why) = &self.unread {
            return GoesOn::Unread(why.clone());
        }
        // No process to follow: gone before its end was.
        let Some(usage) = self.usage.as_mut() else {
            return GoesOn::Still;
        };
        let unresolved = self
            .watch
            .as_ref()
            .is_some_and(|watch| self.looked.elapsed() < watch.resolution());
        // A provider's work is judged over no less than its CPU reading resolves: until the
        // slices since add up to that, the look before is kept, and it goes on.
        if self.provider && unresolved {
            return GoesOn::Went(Look {
                ran: true,
                before: None,
                now: None,
                over: usage.since(),
                unread: Some("its slices since the look before shorter than its CPU reading resolves".into()),
            });
        }
        // Its usage first, then D215's look: a usage unread as its process ends is told by
        // that look, which then finds it ending or gone.
        let look = usage.look();
        // A test's failed read (`UNREADABLE`): what its usage read says replaced by the failure.
        #[cfg(test)]
        let look = match lock(&UNREADABLE).iter().find(|(pid, _)| *pid == usage.pid()) {
            Some((_, why)) => Look {
                ran: false,
                now: None,
                unread: Some(why.clone()),
                ..look
            },
            None => look,
        };
        let found = match self.watch.as_mut() {
            Some(_) if unresolved => None,
            Some(watch) => {
                self.looked = Instant::now();
                match watch.look(&shards_api::Listing::new()) {
                    Ok(shards_api::Found::Gone) => return GoesOn::Still,
                    Ok(found) => Some(found),
                    Err(e) => return GoesOn::Unread(format!("process {}'s CPU: {e}", usage.pid())),
                }
            }
            // Gone as its follow began.
            None => return GoesOn::Still,
        };
        let ending = self.watch.as_ref().is_some_and(shards_api::Watch::ended);
        if let Some(why) = &look.unread
            && found.is_some()
            && !ending
        {
            return GoesOn::Unread(why.clone());
        }
        let went = if self.provider {
            // Its wait for a CPU in the slice, none counted on Windows; unread, the look is.
            let waited = match self.watch.as_ref().map(shards_api::Watch::waited) {
                Some(Ok(waited)) => waited,
                Some(Err(e)) => return GoesOn::Unread(format!("process {}: {e}", usage.pid())),
                None => None,
            };
            found == Some(shards_api::Found::Held)
                || progressed(
                    look.before.map(|u| u.cpu_ns),
                    look.now.map(|u| u.cpu_ns),
                    look.over.saturating_sub(waited.unwrap_or_default()),
                )
        } else {
            let freed = matches!((look.before, look.now), (Some(b), Some(n)) if n.resident < b.resident);
            freed
                || unresolved
                || matches!(
                    found,
                    Some(shards_api::Found::Progressed | shards_api::Found::Held)
                )
        };
        if went { GoesOn::Went(look) } else { GoesOn::Still }
    }
}

/// What a look at an end found of its process since the look before ([`Followed::goes_on`]).
#[derive(Debug)]
pub(crate) enum GoesOn {
    /// It went on with its end: what it did, for the log.
    Went(Look),
    /// It went on with nothing of its end, or is stopped or frozen, or has gone, or there was
    /// no process to follow.
    Still,
    /// It lives and could not be read: why. Given up on as one still is, and said.
    Unread(String),
}

/// A slice as the home's record keeps it, in whole milliseconds, rounded up: a heard end
/// that took less than one is kept as one, never as none, which [`Ends::slice`] would read
/// as nothing learned (Windows CI 44408928475: a kill's end heard in under a millisecond,
/// kept as `teardown 0`).
fn whole_ms(took: Duration) -> u128 {
    took.as_nanos().div_ceil(1_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether a look found its end going on: a look that could not read it fails the test,
    /// saying why.
    fn went_on(found: GoesOn) -> bool {
        match found {
            GoesOn::Went(_) => true,
            GoesOn::Still => false,
            GoesOn::Unread(why) => panic!("its look could not read it: {why}"),
        }
    }

    /// The record as a later process reads it: its newest version.
    fn recorded(home: &Path) -> String {
        newest(&home.join(RECORD)).unwrap()
    }

    /// Writes `text` as the record, as `heard` publishes one (platform `write_in`).
    fn publish(home: &Path, text: &str) {
        let dir = shards_vmm::platform::open_dir(home).unwrap();
        shards_vmm::platform::write_in(&dir, RECORD, text.as_bytes()).unwrap();
    }

    /// A record published as `heard` publishes it is read back by the next process, over
    /// and over: on Windows each publication is a new version beside the name, which a
    /// read of the plain name never finds (runs-12).
    #[test]
    fn each_published_record_is_read_back_by_the_next_process() {
        let home = shards_testdir::TempDir::new("ends-versions").unwrap();
        for (ms, kept) in [(1_000u64, 1_000u64), (3_000, 3_000), (2_000, 3_000)] {
            Ends::kept_in(&home)
                .heard(End::Teardown, Duration::from_millis(ms))
                .unwrap();
            assert_eq!(
                Ends::kept_in(&home).slice(End::Teardown),
                Some(Duration::from_millis(kept)),
                "after {ms}"
            );
        }
        publish(&home, "teardown 5000\nexit 0\nhelper 0\n");
        assert_eq!(
            Ends::kept_in(&home).slice(End::Teardown),
            Some(Duration::from_secs(5))
        );
    }

    /// Another writer owns the stable record lock and is about to publish a slower
    /// teardown. A helper's update must wait, reread it, and preserve both maxima.
    #[test]
    fn concurrent_end_writers_keep_each_others_maxima() {
        let home = shards_testdir::TempDir::new("ends-race").unwrap();
        std::fs::write(home.join(RECORD), "teardown 1000\nexit 0\nhelper 0\n").unwrap();
        let owned = shards_image::store::Locked::exclusive(&home.join("ends.lock")).unwrap();
        let ends = Ends::kept_in(&home);
        let (send, completed) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            crate::daemon::party::spawn_in(scope, || {
                send.send(ends.heard(End::Helper, Duration::from_secs(8)))
                    .unwrap()
            });
            let early = completed.recv_timeout(Duration::from_secs(1));
            // The lock owner publishes the snapshot it read before the other writer
            // began. A writer that ignores its lock loses its helper observation here.
            publish(&home, "teardown 9000\nexit 0\nhelper 0\n");
            drop(owned);
            early
                .or_else(|_| completed.recv_timeout(Duration::from_secs(10)))
                .unwrap()
                .unwrap();
        });
        let kept = Ends::kept_in(&home);
        assert_eq!(kept.slice(End::Teardown), Some(Duration::from_secs(9)));
        assert_eq!(kept.slice(End::Helper), Some(Duration::from_secs(8)));
    }

    /// A home with no record gives no end a slice: each is followed by its process's
    /// liveness alone, as the person's rule has a cold wait (red with `LEAST_SLICE`, 10 s of
    /// shards' own). Each end heard slower than the slowest is kept, in the home too, and
    /// read back by the next daemon; a faster one changes nothing.
    #[test]
    fn a_hosts_ends_are_learned_and_kept_in_its_home() {
        let home = shards_testdir::TempDir::new("ends").unwrap();
        let ends = Ends::kept_in(&home);
        assert_eq!(ends.slice(End::Teardown), None);
        assert_eq!(ends.slice(End::Exit), None);
        assert_eq!(ends.slice(End::Helper), None);
        assert!(!home.join(RECORD).exists());
        ends.heard(End::Teardown, Duration::from_millis(900)).unwrap();
        assert_eq!(ends.slice(End::Teardown), Some(Duration::from_millis(900)));
        ends.heard(End::Teardown, Duration::from_millis(19_383)).unwrap();
        ends.heard(End::Teardown, Duration::from_millis(12_000)).unwrap();
        ends.heard(End::Exit, Duration::from_millis(1_400)).unwrap();
        assert_eq!(ends.slice(End::Teardown), Some(Duration::from_millis(19_383)));
        assert_eq!(recorded(&home), "teardown 19383\nexit 1400\nhelper 0\nstart 0\n");
        let next = Ends::kept_in(&home);
        assert_eq!(next.slice(End::Teardown), Some(Duration::from_millis(19_383)));
        assert_eq!(next.slice(End::Exit), Some(Duration::from_millis(1_400)));
        assert_eq!(next.slice(End::Helper), None);
        // A record it cannot read is no record.
        publish(&home, "teardown many\n");
        assert_eq!(Ends::kept_in(&home).slice(End::Teardown), None);
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
        assert_eq!(daemon.slice(End::Helper), Some(Duration::from_millis(12_500)));
        assert_eq!(recorded(&home), "teardown 0\nexit 11000\nhelper 12500\nstart 0\n");
        let gone = shards_testdir::TempDir::new("ends-gone").unwrap();
        let lost = Ends::kept_in(&gone.join("no-such-home"));
        let said = lost.heard(End::Helper, Duration::from_secs(11)).unwrap_err();
        assert!(said.starts_with("keeping this host's ends in "), "{said}");
    }

    /// A teardown's work read through its end teaches its C*, kept in the home beside the
    /// slices, keyed by the unit and CPU model it was counted in ([`work_key`]), and read back
    /// by the next process; it bounds nothing until two teardowns have taught it
    /// (`shards_api::SPREAD`), and one whose work read none teaches nothing (D217, ROOT's
    /// ruling of 2026-11-18). A record whose work line does not read whole keeps none, nor
    /// does one of another key, nor one kept before lines were keyed (on Windows a count of
    /// whole clock ticks: CI 44347282760), its slices kept.
    #[test]
    fn a_teardowns_work_teaches_its_c_star_kept_in_its_home() {
        let work = shards_api::Work::of;
        let home = shards_testdir::TempDir::new("ends-work").unwrap();
        let ends = Ends::kept_in(&home);
        ends.learn_work(shards_api::Work::ZERO);
        ends.learn_work(work(3_000_000));
        assert_eq!(ends.most_work(), None, "one teardown's work bounds nothing");
        ends.learn_work(work(2_000_000));
        assert_eq!(ends.most_work(), Some(work(3_000_000)));
        ends.learn(End::Teardown, Duration::from_millis(40));
        ends.keep().unwrap();
        assert_eq!(
            recorded(&home),
            format!(
                "teardown 40\nexit 0\nhelper 0\nstart 0\nteardown-work {} 3000000 2\n",
                work_key()
            )
        );
        let next = Ends::kept_in(&home);
        assert_eq!(next.most_work(), Some(work(3_000_000)));
        for (line, what) in [
            ("teardown-work many 2", "not whole"),
            ("teardown-work 3000000 2", "kept before lines were keyed"),
            ("teardown-work cycles:Another_CPU 3000000 2", "another key's"),
        ] {
            publish(&home, &format!("teardown 40\n{line}\n"));
            let kept = Ends::kept_in(&home);
            assert_eq!(kept.most_work(), None, "{what}");
            assert_eq!(
                kept.slice(End::Teardown),
                Some(Duration::from_millis(40)),
                "{what}"
            );
        }
    }

    /// A teardown whose VM process has done more work since its SIGKILL than C* is past it:
    /// this process spinning a CPU past a millisecond, by its look, and never past no C*. A
    /// joiner's provider, whose work is its own command's too, never is.
    #[test]
    fn a_teardown_past_c_star_is_said_past_it_and_a_providers_never() {
        // C* of one count of work (a nanosecond's CPU, on Windows a cycle): 50 ms of spinning
        // passes it on any host.
        let most = shards_api::Work::of(1);
        let spin = || {
            let t0 = std::time::Instant::now();
            let mut x = 0u64;
            while t0.elapsed() < Duration::from_millis(50) {
                x = std::hint::black_box(x.wrapping_add(1));
            }
        };
        let mut own = Followed::of(Some(std::process::id()));
        spin();
        assert!(went_on(own.goes_on()));
        let (work, said) = own.past(Some(most)).unwrap().expect("past C*");
        assert!(work > most && said == most, "{work:?} {said:?}");
        assert_eq!(own.past(None), Ok(None));
        let mut provider = Followed::of_provider(std::process::id());
        spin();
        let _ = provider.goes_on();
        assert_eq!(provider.past(Some(most)), Ok(None));
        assert_eq!(provider.work(), Ok(None));
    }

    /// A teardown whose VM process could not be read as its follow began says why its work
    /// is unread, at its bound and at its end, never taking it for none (the person's rule of
    /// 2026-11-18: no fallbacks); with no C* learned there is no bound to judge; and one with
    /// no process to follow has no work, which is a reading.
    #[test]
    fn a_teardowns_unread_work_is_said_never_taken_for_none() {
        let why = "process 1: refused".to_string();
        let mut unread = Followed {
            unread: Some(why.clone()),
            ..Followed::of(None)
        };
        assert_eq!(unread.work(), Err(why.clone()));
        assert_eq!(unread.past(Some(shards_api::Work::of(1))), Err(why.clone()));
        assert_eq!(unread.past(None), Ok(None), "no C*, no bound to judge");
        assert_eq!(unread.worked(), Err(why));
        let mut none = Followed::of(None);
        assert_eq!(none.work(), Ok(None));
        assert_eq!(none.worked(), Ok(None));
    }

    /// A look that cannot read its living process is unread, with why, never still nor gone on
    /// (the person's rule of 2026-11-18: no fallbacks): here its usage, read of a process none
    /// has beside D215's look of this one, which lives, at each look; and a follow that could
    /// not begin, at its first. Its caller gives it up at that look, saying why. Red
    /// (lanes/redgreen/own-38h-goes-on-unread.patch) with a usage unread taken for nothing, as
    /// before: D215's look alone judged it.
    #[test]
    fn a_live_process_whose_look_fails_is_unread_with_why() {
        let mut unreadable = Followed {
            usage: Some(Progress::of(u32::MAX)),
            ..Followed::of(Some(std::process::id()))
        };
        let resolution = unreadable.watch.as_ref().unwrap().resolution();
        for _ in 0..2 {
            std::thread::sleep(resolution);
            match unreadable.goes_on() {
                GoesOn::Unread(why) => assert!(why.starts_with("process 4294967295's usage: "), "{why}"),
                found => panic!("a live process unread was found {found:?}"),
            }
        }
        let why = "process 1: refused".to_string();
        let mut unbegun = Followed {
            unread: Some(why.clone()),
            ..Followed::of(None)
        };
        assert!(matches!(unbegun.goes_on(), GoesOn::Unread(said) if said == why));
    }

    /// A process that ends between two looks is its end, never unread, whatever its usage
    /// reads of it then: under way at the look that first finds it ended, then gone (D215's
    /// look, read after its usage, tells an end from a failure).
    #[cfg(unix)]
    #[test]
    fn a_process_ending_between_looks_is_its_end_never_unread() {
        let child = Killed(
            std::process::Command::new("/bin/sleep")
                .arg("60")
                .stdin(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let pid = child.0.id();
        let mut ending = Followed::of(Some(pid));
        let resolution = ending.watch.as_ref().unwrap().resolution();
        let id = libc::pid_t::try_from(pid).unwrap();
        // SAFETY: kill(2) of this test's own child.
        assert_eq!(unsafe { libc::kill(id, libc::SIGKILL) }, 0);
        // SAFETY: a siginfo_t, plain data, for waitid(2) to fill.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: waitid(2) of this test's own child, to its end; WNOWAIT leaves it unreaped.
        let waited = unsafe { libc::waitid(libc::P_PID, pid, &raw mut info, libc::WEXITED | libc::WNOWAIT) };
        assert_eq!(waited, 0);
        std::thread::sleep(resolution);
        let first = ending.goes_on();
        assert!(matches!(first, GoesOn::Went(_)), "its end under way: {first:?}");
        std::thread::sleep(resolution);
        let then = ending.goes_on();
        assert!(
            matches!(then, GoesOn::Still),
            "its end found again, gone: {then:?}"
        );
    }

    /// An end is waited on past its slice while its VM process runs: this process spinning
    /// a CPU is seen to go on, and one with no process to follow never is.
    #[test]
    fn an_end_is_followed_while_its_vm_process_runs() {
        let mut busy = Followed::of(Some(std::process::id()));
        let t0 = std::time::Instant::now();
        let mut x = 0u64;
        while t0.elapsed() < Duration::from_millis(300) {
            x = std::hint::black_box(x.wrapping_add(1));
        }
        assert!(went_on(busy.goes_on()));
        assert!(!went_on(Followed::of(None).goes_on()));
    }

    /// Set in the child of `an_end_goes_on_at_any_rate_and_not_asleep_or_stopped`: it runs a
    /// CPU a fifth of a millisecond each tenth of a second, until it is killed.
    #[cfg(unix)]
    const TRICKLE: &str = "SHARDS_ENDS_TRICKLE";

    /// An end goes on while its VM process runs at any rate, here a fiftieth of a CPU, below
    /// PM M357's idle microVM's 63 ms in 10 s (red with `IDLE_VM_CPU`'s rate); one asleep, or
    /// stopped, goes on with nothing, and is given up on (ROOT's ruling, 2026-11-13).
    #[cfg(unix)]
    #[test]
    fn an_end_goes_on_at_any_rate_and_not_asleep_or_stopped() {
        if std::env::var_os(TRICKLE).is_some() {
            loop {
                let began = std::time::Instant::now();
                while began.elapsed() < Duration::from_micros(200) {
                    std::hint::spin_loop();
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        let child = |program: &std::path::Path, args: &[&str], trickle: bool| {
            let mut command = std::process::Command::new(program);
            command
                .args(args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null());
            if trickle {
                command.env(TRICKLE, "1");
            }
            Killed(command.spawn().unwrap())
        };
        let trickler = child(
            &std::env::current_exe().unwrap(),
            &[
                "--exact",
                "daemon::ends::tests::an_end_goes_on_at_any_rate_and_not_asleep_or_stopped",
            ],
            true,
        );
        let asleep = child(std::path::Path::new("/bin/sleep"), &["60"], false);
        let mut trickling = Followed::of(Some(trickler.0.id()));
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            went_on(trickling.goes_on()),
            "a fiftieth of a CPU is its end going on"
        );
        // Asleep over a window as long as its reading shows (`Watch::resolution`: a shorter
        // one goes on, no evidence), and until the trickler has gone on once more, in which a
        // second watch of it finds it ran nothing; looked at again over another where it ran:
        // its start's own CPU, or a cgroup freezer's cycles, which wake a sleeper (a kiln
        // run's, about six a second, PM M562), so no window is a sleeper's by its clock.
        let pid = asleep.0.id();
        let mut sleeping = loop {
            let followed = shards_api::Followed::of_pid(pid).unwrap().unwrap();
            let mut witness = shards_api::Watch::of(followed, &shards_api::Listing::new())
                .unwrap()
                .unwrap();
            let from = witness.work();
            let mut sleeping = Followed::of(Some(pid));
            std::thread::sleep(witness.resolution());
            while !went_on(trickling.goes_on()) {
                std::thread::yield_now();
            }
            let went = went_on(sleeping.goes_on());
            witness.look(&shards_api::Listing::new()).unwrap();
            if witness.work() == from {
                assert!(!went, "asleep, it goes on with nothing");
                break sleeping;
            }
        };
        let id = libc::pid_t::try_from(pid).unwrap();
        // SAFETY: kill(2) of this test's own child.
        assert_eq!(unsafe { libc::kill(id, libc::SIGSTOP) }, 0);
        // SAFETY: a siginfo_t, plain data, for waitid(2) to fill.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: waitid(2) of this test's own child, to its stop; WNOWAIT leaves it so.
        let waited = unsafe { libc::waitid(libc::P_PID, pid, &raw mut info, libc::WSTOPPED | libc::WNOWAIT) };
        assert_eq!(waited, 0);
        let resolution = shards_api::Followed::of_pid(pid)
            .unwrap()
            .and_then(|followed| shards_api::Watch::of(followed, &shards_api::Listing::new()).unwrap())
            .unwrap()
            .resolution();
        std::thread::sleep(resolution);
        assert!(!went_on(sleeping.goes_on()), "stopped, it goes on with nothing");
    }

    /// A joiner's teardown (D119) goes on by its provider's VM process's work beyond an idle
    /// microVM's alone (`IDLE_VM_CPU`, PM M357, ROOT's ruling of 2026-11-18). This process
    /// spinning a CPU goes on. One trickling a fiftieth of a CPU, below an idle microVM's rate,
    /// as an idle provider's ticks are, does not, where a teardown in its own VM goes on at
    /// that rate (D217). Red (lanes/redgreen/own-35d-joiner-end.patch) with a joiner's
    /// teardown followed by nothing, as before: the spinning provider is no going on.
    #[cfg(unix)]
    #[test]
    fn a_joiners_teardown_goes_on_by_its_providers_work_beyond_an_idle_microvm() {
        let mut busy = Followed::of_provider(std::process::id());
        let t0 = std::time::Instant::now();
        let mut x = 0u64;
        while t0.elapsed() < Duration::from_millis(300) {
            x = std::hint::black_box(x.wrapping_add(1));
        }
        assert!(went_on(busy.goes_on()), "a provider spinning a CPU goes on");
        let trickler = Killed(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "daemon::ends::tests::an_end_goes_on_at_any_rate_and_not_asleep_or_stopped",
                ])
                .env(TRICKLE, "1")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let pid = trickler.0.id();
        // Settled into its trickle: a window in which it ran no more than an idle microVM's
        // rate, as it does once its start's own CPU is past.
        let mut settling = Progress::of(pid);
        loop {
            std::thread::sleep(Duration::from_millis(300));
            if !settling.look().ran {
                break;
            }
        }
        let mut provider = Followed::of_provider(pid);
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !went_on(provider.goes_on()),
            "a fiftieth of a CPU, an idle provider's ticks, is no joiner's end going on"
        );
    }

    /// A child killed as it drops.
    #[cfg(unix)]
    struct Killed(std::process::Child);

    #[cfg(unix)]
    impl Drop for Killed {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
