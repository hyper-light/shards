//! A run's members' own lanes (D238, goal 58's steps 5 and 6): each lane its agent-control
//! endpoint hands its host ([`exec::MemberStart`]), told whether its member runs or how it
//! ended, told its member's end as its init tells it ([`Ends`], `MEMBER_ENDED`), and its
//! steering delivered and answered: a signal by init's `SIGNAL` built-in, a resize of the
//! member's own terminal by its `RESIZE` (D244).
//!
//! - **Its state.** Once its run has started: running, or ended as its init said, typed: its
//!   command's exit (`Reply::Exited`) or the signal that killed it (`Reply::Signaled`).
//! - **Its end, as it comes.** Its init says each member's end once its whole population has
//!   ended (D231), which its VM process relays (`MEMBER_ENDED`) and the run's end table keeps
//!   and tells whoever waits ([`Ends`]): no lane looks for it.
//! - **Its steering.** One steer at a time, each answered: refused `Ended` for a member that
//!   has, `Terminal` for a resize of one with no terminal of its own, else run as the daemon's
//!   own built-ins are (no client's exec), charged to the run's budget while it goes
//!   (`Capacity` where it has no room), and answered by the built-in's exit by its kind.
//! - **Its life.** A member's is its run's: no lane ends it, and a lane let go (its exit told,
//!   or its controller's connection gone) stops only its waits.

use std::io::{self, Read as _};
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, mpsc};

use shards_abi::run::{self, Spec, members};
use shards_ipc::Stream;
use shards_ipc::fd::BorrowedFd;
use shards_remote::control::exec::{self, MemberStart, Reply, Steer, Unstarted, Unsteered};

use super::execs::{self, Held, Say};
use super::{Charge, ipc_buffers, lock};

/// Each member's end as its init told it (`MEMBER_ENDED`, D231): kept for a lane that asks
/// after, and told as it comes to whoever waits on it.
#[derive(Debug)]
pub(crate) struct Ends {
    state: Mutex<Told>,
    changed: Condvar,
}

/// What a run's end table holds (R04): each member's end once told, by its index; and
/// whether the run is over, after which none is told and every wait ends.
#[derive(Debug)]
struct Told {
    ended: Vec<Option<members::End>>,
    over: bool,
}

impl Ends {
    /// The end table of a run of `members` members, none ended.
    pub(crate) fn new(members: usize) -> io::Result<Ends> {
        let mut ended = Vec::new();
        ended.try_reserve_exact(members).map_err(io::Error::other)?;
        ended.resize(members, None);
        Ok(Ends {
            state: Mutex::new(Told { ended, over: false }),
            changed: Condvar::new(),
        })
    }

    /// Member `member` ended as `end` says, as its init told it (once a member): told whoever
    /// waits on it. A member of no index the run has, or told twice, is the guest's protocol
    /// broken, refused.
    pub(crate) fn told(&self, member: u32, end: members::End) -> io::Result<()> {
        let mut told = lock(&self.state);
        let slot = usize::try_from(member)
            .ok()
            .and_then(|at| told.ended.get_mut(at))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("no member {member}")))?;
        if slot.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("member {member}'s end told twice"),
            ));
        }
        *slot = Some(end);
        drop(told);
        self.changed.notify_all();
        Ok(())
    }

    /// The run is over: no member's end is told from now on, and every wait ends.
    pub(crate) fn close(&self) {
        lock(&self.state).over = true;
        self.changed.notify_all();
    }

    /// Member `member`'s end, once it has been told.
    fn ended(&self, member: u32) -> Option<members::End> {
        let told = lock(&self.state);
        usize::try_from(member)
            .ok()
            .and_then(|at| told.ended.get(at).copied().flatten())
    }

    /// Waits for member `member`'s end; or none, once the run is over or `gone` says its lane
    /// was let go (which [`Ends::wake`] tells).
    fn wait(&self, member: u32, gone: &AtomicBool) -> Option<members::End> {
        let at = usize::try_from(member).ok()?;
        let mut told = lock(&self.state);
        loop {
            if let Some(end) = told.ended.get(at).copied().flatten() {
                return Some(end);
            }
            if told.over || gone.load(Ordering::Acquire) || told.ended.get(at).is_none() {
                return None;
            }
            told = self
                .changed
                .wait(told)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Wakes every wait, for one whose lane was let go to see it.
    fn wake(&self) {
        let _told = lock(&self.state);
        self.changed.notify_all();
    }
}

/// How the host runs one of init's built-ins in its run's microVM: its spec, its stdio and its
/// client's end of the connection its end is told on (`Daemon::launch_builtin`, api_run.rs
/// `hand_exec`).
pub(crate) type Builtin<'a> = dyn Fn(&Spec, [BorrowedFd<'_>; 3], &Stream) -> io::Result<()> + Sync + 'a;

/// What the host serves its members' lanes with: the run's end table, the members that run a
/// program and those with a terminal of their own, the run's budget, whether its run runs
/// (once it has started), how it runs a built-in, and where it says what befell a lane.
pub(crate) struct Hosting<'a> {
    pub(crate) ends: &'a Ends,
    pub(crate) active: &'a [u32],
    pub(crate) terminals: &'a [u32],
    pub(crate) charge: &'a Charge,
    pub(crate) started: &'a (dyn Fn() -> bool + Sync),
    pub(crate) builtin: &'a Builtin<'a>,
    pub(crate) say: &'a Say,
}

/// Serves each member's lane `lanes` brings, on a thread of its own, until the endpoint that
/// brings them has ended and so has every lane it brought.
pub(crate) fn host(lanes: &mpsc::Receiver<MemberStart>, hosting: &Hosting<'_>) {
    std::thread::scope(|scope| {
        while let Ok(start) = lanes.recv() {
            let (number, replies) = (start.lane, start.replies.try_clone());
            let spawned = std::thread::Builder::new()
                .name("agent-member".into())
                .spawn_scoped(scope, move || serve(start, hosting));
            if let Err(e) = spawned {
                (hosting.say)(&format!("agent-control member lane {number}: its thread: {e}"));
                if let Ok(replies) = replies {
                    replies.tell(Reply::Unstarted(Unstarted::Failed));
                }
            }
        }
    });
}

/// A member's end as its lane tells it, typed: its exit's code, the bits its init said, as an
/// exec's are told; or the signal that killed it.
fn told_end(end: members::End) -> Reply {
    match end {
        members::End::Exited(code) => Reply::Exited(i32::from_be_bytes(code.to_be_bytes())),
        members::End::Signaled(signal) => Reply::Signaled(signal),
    }
}

/// One member's lane, from its ask to its letting go: refused where its run has no such member
/// that runs, does not run, or has no room in its budget for it; told the member's end at once
/// where it has ended, else that it runs, then each steer answered and its end as it comes.
fn serve(start: MemberStart, hosting: &Hosting<'_>) {
    let MemberStart {
        ask: exec::Ask { member, .. },
        lane,
        steer,
        replies,
    } = start;
    let say = |what: &str| (hosting.say)(&format!("agent-control member {member}, lane {lane}: {what}"));
    // A member the run has not, or one that runs no program, refused as its init refuses an
    // exec in one (D243): an endpoint of no member stream leaves the member to its host.
    if !hosting.active.contains(&member) {
        say("its run has no such member that runs");
        return replies.tell(Reply::Unstarted(Unstarted::Refused));
    }
    if !(hosting.started)() {
        say("its run does not run");
        return replies.tell(Reply::Unstarted(Unstarted::Failed));
    }
    // What it holds while it is open: its lane's windows at its endpoint, and its record here.
    let bytes = exec::member_lane_bytes()
        .zip(u64::try_from(size_of::<MemberStart>()).ok())
        .and_then(|(lane, record)| lane.checked_add(record));
    let Some(bytes) = bytes else {
        say("its charge overflows");
        return replies.tell(Reply::Unstarted(Unstarted::Failed));
    };
    let _held = match Held::new(hosting.charge, bytes) {
        Ok(held) => held,
        Err(e) => {
            say(&format!("{bytes} bytes: {e:?}"));
            return replies.tell(Reply::Unstarted(Unstarted::Capacity));
        }
    };
    if let Some(end) = hosting.ends.ended(member) {
        return replies.tell(told_end(end));
    }
    let Ok(answers) = replies.try_clone() else {
        say("its answers");
        return replies.tell(Reply::Unstarted(Unstarted::Failed));
    };
    let gone = AtomicBool::new(false);
    std::thread::scope(|s| {
        let gone = &gone;
        // Its steering before it is told running, so that no steer goes unanswered.
        let steering = std::thread::Builder::new()
            .name("member-steer".into())
            .spawn_scoped(s, move || {
                while let Ok(taken) = steer.recv() {
                    answers.tell(Reply::Steered(deliver(member, taken, hosting)));
                }
                gone.store(true, Ordering::Release);
                hosting.ends.wake();
            });
        if let Err(e) = steering {
            say(&format!("its steering's thread: {e}"));
            return replies.tell(Reply::Unstarted(Unstarted::Failed));
        }
        replies.tell(Reply::Running);
        if let Some(end) = hosting.ends.wait(member, gone) {
            replies.tell(told_end(end));
        }
    });
}

/// Delivers `steer` to member `member`: taken, or why not, typed.
fn deliver(member: u32, steer: Steer, hosting: &Hosting<'_>) -> Result<(), Unsteered> {
    if hosting.ends.ended(member).is_some() {
        return Err(Unsteered::Ended);
    }
    let (builtin, argv) = match steer {
        Steer::Signal(signal) => (run::builtin::SIGNAL, members::signal_args(member, signal)),
        Steer::Resize(size) if hosting.terminals.contains(&member) => {
            (run::builtin::RESIZE, members::resize_args(member, size))
        }
        Steer::Resize(_) => return Err(Unsteered::Terminal),
    };
    let spec = Spec {
        argv,
        builtin,
        ..Spec::default()
    };
    match run_builtin(&spec, hosting) {
        Ok(Said { status: 0, .. }) => Ok(()),
        Ok(Said { status, .. })
            if builtin == run::builtin::SIGNAL && status == run::builtin::SIGNAL_ENDED =>
        {
            Err(Unsteered::Ended)
        }
        Ok(Said { status, words }) => {
            (hosting.say)(&format!(
                "agent-control member {member}: built-in {builtin} exited {status}: {}",
                String::from_utf8_lossy(&words).trim_end()
            ));
            Err(Unsteered::Failed)
        }
        Err(Undelivered::Capacity) => Err(Unsteered::Capacity),
        Err(Undelivered::Failed(why)) => {
            (hosting.say)(&format!(
                "agent-control member {member}: built-in {builtin}: {why}"
            ));
            Err(Unsteered::Failed)
        }
    }
}

/// How a built-in that ran ended: its exit status, and the words its errors said.
struct Said {
    status: u8,
    words: Vec<u8>,
}

/// Why a steer's built-in did not run to its end (R04).
enum Undelivered {
    /// The run's budget has no room for what it holds while it runs.
    Capacity,
    /// It could not be run, or its VM process ended before it did: why.
    Failed(String),
}

/// Runs `spec`, one of init's built-ins, in the run's microVM: charged while it runs for what
/// it holds, its connection's queues, both ends, and its pipes' (`execs::pipe_bytes`); its
/// output and errors read whole within the run protocol's bound (an answer of init's is a
/// line), and its exit read.
fn run_builtin(spec: &Spec, hosting: &Hosting<'_>) -> Result<Said, Undelivered> {
    let failed = |e: io::Error| Undelivered::Failed(e.to_string());
    let (ours, theirs) = execs::made(true, false).map_err(failed)?;
    let pipes = [
        Some(execs::pipe_bytes(&ours.stdout).map_err(failed)?),
        ours.stderr
            .as_ref()
            .map(execs::pipe_bytes)
            .transpose()
            .map_err(failed)?,
    ];
    let bytes = pipes
        .into_iter()
        .flatten()
        .chain([
            ipc_buffers(&ours.conn).map_err(failed)?,
            ipc_buffers(&theirs.conn).map_err(failed)?,
        ])
        .try_fold(0u64, u64::checked_add)
        .ok_or_else(|| Undelivered::Failed("its charge overflows".into()))?;
    let _held = Held::new(hosting.charge, bytes).map_err(|_| Undelivered::Capacity)?;
    (hosting.builtin)(spec, theirs.stdio(), &theirs.conn).map_err(failed)?;
    // Its VM process holds its own ends now: the pipes end as it lets them go.
    drop(theirs);
    let execs::Ours {
        conn,
        mut stdout,
        stderr,
        ..
    } = ours;
    let bound = u64::from(run::MAX_PAYLOAD);
    let _ = (&mut stdout).take(bound).read_to_end(&mut Vec::new());
    let mut words = Vec::new();
    if let Some(mut stderr) = stderr {
        let _ = (&mut stderr).take(bound).read_to_end(&mut words);
    }
    let mut started = false;
    let mut unstarted: Option<Vec<u8>> = None;
    while let Ok(Some(said)) = shards_ipc::recv(&conn) {
        match said.kind {
            shards_ipc::kind::STARTED => started = true,
            shards_ipc::kind::EXEC_UNSTARTED => unstarted = Some(said.payload),
            shards_ipc::kind::EXIT => {
                return match (started, said.payload.first()) {
                    (true, Some(&status)) => Ok(Said { status, words }),
                    _ => Err(Undelivered::Failed(match unstarted {
                        Some(why) => format!("not started: {}", String::from_utf8_lossy(&why)),
                        None => "it ended untold".into(),
                    })),
                };
            }
            _ => {}
        }
    }
    Err(Undelivered::Failed("its VM process ended before it did".into()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A member's end is kept for a lane that asks after and told whoever waits: a wait ends
    /// with it, with the run's end, or as its lane is let go; told twice, or of a member the run
    /// has not, it is refused.
    #[test]
    fn a_members_end_is_kept_and_told_as_it_comes() {
        let ends = Ends::new(2).unwrap();
        let gone = AtomicBool::new(false);
        assert_eq!(ends.ended(1), None);
        std::thread::scope(|s| {
            let waiting = s.spawn(|| ends.wait(1, &gone));
            ends.told(1, members::End::Signaled(9)).unwrap();
            assert_eq!(waiting.join().unwrap(), Some(members::End::Signaled(9)));
        });
        assert_eq!(ends.ended(1), Some(members::End::Signaled(9)));
        assert_eq!(ends.wait(1, &gone), Some(members::End::Signaled(9)));
        let exited = members::End::Exited(0);
        assert_eq!(
            ends.told(1, exited).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            ends.told(2, exited).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(ends.wait(2, &gone), None);
        std::thread::scope(|s| {
            let waiting = s.spawn(|| ends.wait(0, &gone));
            gone.store(true, Ordering::Release);
            ends.wake();
            assert_eq!(waiting.join().unwrap(), None);
        });
        let gone = AtomicBool::new(false);
        std::thread::scope(|s| {
            let waiting = s.spawn(|| ends.wait(0, &gone));
            ends.close();
            assert_eq!(waiting.join().unwrap(), None);
        });
    }

    /// A lane tells a member's end typed: a death by TERM `Signaled(15)`, which `exit 143` is
    /// not; an exit's code its bits as its init said them.
    #[test]
    fn a_lane_tells_a_signal_apart_from_an_exit() {
        assert_eq!(told_end(members::End::Signaled(15)), Reply::Signaled(15));
        assert_eq!(told_end(members::End::Exited(143)), Reply::Exited(143));
        assert_eq!(told_end(members::End::Exited(u32::MAX)), Reply::Exited(-1));
    }

    /// A status keeps its bits as its lane tells it.
    #[test]
    fn a_status_is_told_as_its_init_said_it() {
        assert_eq!(told_status(0), 0);
        assert_eq!(told_status(143), 143);
        assert_eq!(told_status(u32::MAX), -1);
    }
}
