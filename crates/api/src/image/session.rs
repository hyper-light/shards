//! The client's side of one image run: the run host's process (`shards api-run`), a thread
//! reading its answers, one keeping its last diagnostics, one waiting for its exit, and one
//! per member channel moving its bytes into the pipe the caller takes. Every wait is on a
//! fact: an answer, a channel's bytes or end, the host's exit; a caller's patience bounds
//! only its own wait.
//!
//! The host is reaped under the state's lock as `exited` is set, and signalled only while
//! it is not, so no signal reaches another process that took its pid (as `crate::client`
//! does a direct boot's).

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read as _, Write as _};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use super::wire::{self, Channel, MemberInfo, RunEnd, Stats, StatsScope};
use super::{ImageGuest, ImageRefusal, ImageSpec, MemberEnd, MemberName};

/// The most of a member channel's bytes the client lets its host send ahead of what the
/// caller's pipe has taken: one read of the guest's (`run::CHUNK`), the most one of its
/// output frames carries, so that a channel no one reads holds no more than that here.
const AHEAD: u64 = shards_abi::run::CHUNK as u64;

/// How many of the run host's last bytes of diagnostics are kept to quote: shards' longest
/// report of a VM's end, its console's last bytes escaped (`crate::client::tail` for the
/// console a direct boot keeps by default).
fn said_kept() -> usize {
    crate::client::tail(shards_abi::bounds::console_said(
        shards_abi::bounds::ConsoleRecords::default(),
        shards_abi::bounds::ConsoleRecords::default(),
    ))
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A member channel's bytes, on their way to the caller's pipe.
#[derive(Debug, Default)]
struct Flow {
    queued: VecDeque<Vec<u8>>,
    ended: Option<wire::StreamEnd>,
}

#[derive(Debug, Default)]
struct State {
    started: Option<wire::Started>,
    refused: Option<ImageRefusal>,
    flows: HashMap<(u32, Channel), Flow>,
    ends: HashMap<u32, u32>,
    stats: HashMap<u32, Result<Stats, String>>,
    next_ask: u32,
    end: Option<RunEnd>,
    /// The host's process has exited and is reaped.
    exited: bool,
    /// Its answers have all been read: its stdout has ended.
    answered: bool,
    said: VecDeque<u8>,
    /// Its diagnostics have all been read: its stderr has ended, its VM's with it.
    said_all: bool,
}

impl State {
    /// The host is gone and has said all it will.
    fn gone(&self) -> bool {
        self.exited && self.answered
    }
}

/// `refused` with the host's last diagnostics, where its run did not boot: what its VM said
/// as it ended, the only words of why (`the VM ended before it took its run`).
fn with_said(refused: ImageRefusal, said: &VecDeque<u8>) -> ImageRefusal {
    match refused {
        ImageRefusal::Boot(why) => ImageRefusal::Boot(format!("{why}{}", quoted(said))),
        other => other,
    }
}

#[derive(Debug)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    /// The host's stdin: closed once the run's end has come, which the host takes as the
    /// last of its owner, and by `stop`.
    to_host: Mutex<Option<std::process::ChildStdin>>,
}

impl Shared {
    fn send(&self, kind: u8, payload: &[u8]) -> io::Result<()> {
        let frame = wire::frame(kind, payload).map_err(io::Error::other)?;
        match lock(&self.to_host).as_mut() {
            Some(stdin) => stdin.write_all(&frame).and_then(|()| stdin.flush()),
            None => Err(io::ErrorKind::BrokenPipe.into()),
        }
    }

    fn credit(&self, member: u32, channel: Channel, bytes: u64) -> io::Result<()> {
        let credit = wire::Credit {
            member,
            channel,
            bytes,
        };
        self.send(wire::to_host::CREDIT, &credit.encode())
    }

    /// Waits for `ready` to give an answer, at most `timeout` past now (`Duration::MAX`:
    /// until it does).
    fn wait<T>(&self, timeout: Duration, mut ready: impl FnMut(&mut State) -> Option<T>) -> Option<T> {
        let deadline = (timeout != Duration::MAX)
            .then(|| Instant::now().checked_add(timeout))
            .flatten();
        let mut state = lock(&self.state);
        loop {
            if let Some(answer) = ready(&mut state) {
                return Some(answer);
            }
            state = match deadline {
                None => self.changed.wait(state).unwrap_or_else(PoisonError::into_inner),
                Some(at) => {
                    let left = at.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return None;
                    }
                    self.changed
                        .wait_timeout(state, left)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
            };
        }
    }
}

/// The host's process: held to reap it, and its pid (Unix) or a handle of its own
/// (Windows) to wait for its exit without the lock a kill takes.
#[derive(Debug)]
struct Host {
    child: Mutex<Child>,
    #[cfg(unix)]
    pid: u32,
    #[cfg(windows)]
    handle: std::os::windows::io::OwnedHandle,
}

impl Host {
    fn of(child: Child) -> io::Result<Host> {
        #[cfg(unix)]
        let pid = child.id();
        #[cfg(windows)]
        let handle = {
            use std::os::windows::io::AsHandle as _;
            child.as_handle().try_clone_to_owned()?
        };
        Ok(Host {
            child: Mutex::new(child),
            #[cfg(unix)]
            pid,
            #[cfg(windows)]
            handle,
        })
    }

    /// Waits for it to exit, leaving it unreaped.
    fn awaited(&self) -> io::Result<()> {
        #[cfg(unix)]
        loop {
            // SAFETY: a siginfo_t, plain data, for waitid(2) to fill.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: waitid(2) of our own child into `info`; WNOWAIT leaves it unreaped.
            let r = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.pid,
                    &raw mut info,
                    libc::WEXITED | libc::WNOWAIT,
                )
            };
            if r == 0 {
                return Ok(());
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle as _;
            use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
            use windows_sys::Win32::System::Threading::{INFINITE, WaitForSingleObject};
            // SAFETY: a process handle this value owns.
            if unsafe { WaitForSingleObject(self.handle.as_raw_handle(), INFINITE) } == WAIT_OBJECT_0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }
    }

    /// Kills it at once, while `state` says it is unreaped.
    fn kill(&self, state: &State) {
        if state.exited {
            return;
        }
        #[cfg(unix)]
        if let Ok(pid) = libc::pid_t::try_from(self.pid) {
            // SAFETY: kill(2) of our own child, unreaped while `exited` is false.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        #[cfg(windows)]
        {
            let _ = lock(&self.child).kill();
        }
    }
}

/// One image run, as its client holds it.
#[derive(Debug)]
pub(super) struct Session {
    shared: Arc<Shared>,
    host: Arc<Host>,
    members: Vec<MemberInfo>,
    published: Vec<wire::Bound>,
    outputs: Mutex<HashMap<(u32, Channel), io::PipeReader>>,
}

impl Session {
    pub(super) fn start(
        spec: &ImageSpec,
        request: &wire::Request,
        patience: Duration,
    ) -> Result<Session, ImageRefusal> {
        let binary = spec
            .binary
            .clone()
            .or_else(|| std::env::var_os("SHARDS_BIN").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("shards"));
        let os = |what: &'static str| move |e: io::Error| ImageRefusal::Os { what, kind: e.kind() };
        let mut child = Command::new(&binary)
            .arg("api-run")
            .envs(spec.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| ImageRefusal::Boot(format!("{}: {e}", binary.display())))?;
        let pipes = (child.stdin.take(), child.stdout.take(), child.stderr.take());
        let host = Host::of(child).map_err(os("the run host's handle"))?;
        let (Some(to_host), Some(from_host), Some(said)) = pipes else {
            let _ = lock(&host.child).kill();
            let _ = lock(&host.child).wait();
            return Err(ImageRefusal::Os {
                what: "the run host's pipes",
                kind: io::ErrorKind::BrokenPipe,
            });
        };
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            to_host: Mutex::new(Some(to_host)),
        });
        // Shared: the session and its exit watcher, which outlives this call.
        let host = Arc::new(host);
        let threads = (|| {
            let (watcher, watched) = (shared.clone(), host.clone());
            std::thread::Builder::new()
                .name("shards-api-image-exit".into())
                .spawn(move || watch(&watcher, &watched))
                .map_err(os("the run host's watcher"))?;
            let reader = shared.clone();
            std::thread::Builder::new()
                .name("shards-api-image".into())
                .spawn(move || read_answers(&reader, from_host))
                .map_err(os("the run host's reader"))?;
            let tail = shared.clone();
            std::thread::Builder::new()
                .name("shards-api-image-said".into())
                .spawn(move || keep_said(&tail, said))
                .map_err(os("the run host's diagnostics"))?;
            let payload = request.encode().map_err(|e| ImageRefusal::Spec {
                what: "the spec",
                why: e.to_string(),
            })?;
            shared
                .send(wire::to_host::CREATE, &payload)
                .map_err(os("the run host's request"))
        })();
        let end_it = |shared: &Shared| {
            host.kill(&lock(&shared.state));
            lock(&shared.to_host).take();
        };
        if let Err(e) = threads {
            end_it(&shared);
            return Err(e);
        }
        // Started, refused, or gone: whichever comes first, within the caller's patience. A
        // refusal is the host's last word, told once it has exited and said all it will.
        let outcome = shared.wait(patience, |s| {
            if s.refused.is_some() {
                if !(s.gone() && s.said_all) {
                    return None;
                }
                return s.refused.take().map(|r| Err(with_said(r, &s.said)));
            }
            if let Some(started) = s.started.take() {
                return Some(Ok(started));
            }
            if let Some(end) = &s.end {
                return Some(Err(ImageRefusal::Host(format!(
                    "the run ended before it started: {end:?}{}",
                    quoted(&s.said)
                ))));
            }
            s.gone().then(|| {
                Err(ImageRefusal::Host(format!(
                    "it exited before its run started{}",
                    quoted(&s.said)
                )))
            })
        });
        let started = match outcome {
            Some(Ok(started)) => started,
            Some(Err(e)) => {
                end_it(&shared);
                return Err(e);
            }
            None => {
                end_it(&shared);
                let mut state = lock(&shared.state);
                // Refused, by a host that has not yet exited: its refusal still.
                if let Some(r) = state.refused.take() {
                    return Err(with_said(r, &state.said));
                }
                let said = quoted(&state.said);
                return Err(ImageRefusal::Boot(format!(
                    "the run did not start within {patience:?}{said}"
                )));
            }
        };
        let session = Session {
            shared,
            host,
            members: started.members,
            published: started.published,
            outputs: Mutex::new(HashMap::new()),
        };
        session.pump_outputs()?;
        Ok(session)
    }

    /// A pipe for each running member's channels, and the thread that fills it as its
    /// bytes come, granting the host room for as many more as it took.
    fn pump_outputs(&self) -> Result<(), ImageRefusal> {
        let mut outputs = lock(&self.outputs);
        for m in self.members.iter().filter(|m| m.runs) {
            for channel in [Channel::Stdout, Channel::Stderr] {
                let (reader, writer) = io::pipe().map_err(|e| ImageRefusal::Os {
                    what: "a member's pipe",
                    kind: e.kind(),
                })?;
                outputs.insert((m.index, channel), reader);
                let shared = self.shared.clone();
                let member = m.index;
                std::thread::Builder::new()
                    .name("shards-api-member".into())
                    .spawn(move || pump(&shared, member, channel, writer))
                    .map_err(|e| ImageRefusal::Os {
                        what: "a member's pump",
                        kind: e.kind(),
                    })?;
                self.shared
                    .credit(member, channel, AHEAD)
                    .map_err(|e| ImageRefusal::Os {
                        what: "the run host's credit",
                        kind: e.kind(),
                    })?;
            }
        }
        Ok(())
    }

    fn index(&self, member: &MemberName) -> Result<&MemberInfo, ImageRefusal> {
        self.members
            .iter()
            .find(|m| m.kind == member.kind && m.name == member.name)
            .ok_or_else(|| ImageRefusal::Image(format!("{member:?}: the image declares no such member")))
    }
}

/// Moves member `member`'s `channel` into `writer` as its bytes come, and grants the host
/// room for each part once written; ends at the channel's end, closing the pipe. A caller
/// that let go of its end drops the rest, as a closed pipe drops `docker run`'s output,
/// still granting room, so that the member is never held by a reader that left.
fn pump(shared: &Shared, member: u32, channel: Channel, mut writer: io::PipeWriter) {
    let mut open = true;
    loop {
        let next = shared.wait(Duration::MAX, |s| {
            let flow = s.flows.entry((member, channel)).or_default();
            if let Some(part) = flow.queued.pop_front() {
                return Some(Some(part));
            }
            (flow.ended.is_some() || s.end.is_some() || s.answered).then_some(None)
        });
        let Some(Some(part)) = next else {
            return;
        };
        if open && writer.write_all(&part).is_err() {
            open = false;
        }
        let taken = u64::try_from(part.len()).unwrap_or(u64::MAX);
        if shared.credit(member, channel, taken).is_err() {
            return;
        }
    }
}

/// Reads the host's answers until it closes its stdout, each into the state; one that
/// breaks the channel's contract ends the session as a failed run. The run's end closes
/// the host's stdin: the host then ends.
fn read_answers(shared: &Shared, mut from_host: std::process::ChildStdout) {
    let failed = loop {
        let (kind, payload) = match wire::read_frame(&mut from_host) {
            Ok(Some(frame)) => frame,
            Ok(None) => break None,
            Err(e) => break Some(format!("its answers: {e}")),
        };
        match take(shared, kind, &payload) {
            Ok(true) => {
                lock(&shared.to_host).take();
            }
            Ok(false) => {}
            Err(e) => break Some(e),
        }
    };
    if failed.is_some() {
        lock(&shared.to_host).take();
    }
    let mut state = lock(&shared.state);
    if let Some(why) = failed {
        state
            .end
            .get_or_insert(RunEnd::Failed(format!("the run host broke its channel: {why}")));
    }
    state.answered = true;
    shared.changed.notify_all();
}

/// Takes one answer into the state; whether it was the run's end.
fn take(shared: &Shared, kind: u8, payload: &[u8]) -> Result<bool, String> {
    use wire::to_client as k;
    let bad = |e: wire::WireError| e.to_string();
    let mut state = lock(&shared.state);
    let mut ended = false;
    match kind {
        k::STARTED => state.started = Some(wire::Started::decode(payload).map_err(bad)?),
        k::REFUSED => {
            let r = wire::Refused::decode(payload).map_err(bad)?;
            state.refused = Some(match r.cause {
                wire::Cause::Request => ImageRefusal::Spec {
                    what: "the request",
                    why: r.why,
                },
                wire::Cause::Image => ImageRefusal::Image(r.why),
                wire::Cause::Widening => ImageRefusal::Widening(r.why),
                wire::Cause::Boot => ImageRefusal::Boot(r.why),
                wire::Cause::Unsupported => ImageRefusal::Unsupported {
                    what: "the run host",
                    why: r.why,
                },
            });
        }
        k::OUTPUT => {
            let out = wire::Output::decode(payload).map_err(bad)?;
            let flow = state.flows.entry((out.member, out.channel)).or_default();
            if flow.ended.is_some() {
                return Err("output after its channel's end".into());
            }
            flow.queued.push_back(out.bytes);
        }
        k::OUTPUT_END => {
            let end = wire::OutputEnd::decode(payload).map_err(bad)?;
            let flow = state.flows.entry((end.member, end.channel)).or_default();
            if flow.ended.replace(end.end).is_some() {
                return Err("a channel ended twice".into());
            }
        }
        k::MEMBER_ENDED => {
            let e = wire::MemberEnded::decode(payload).map_err(bad)?;
            if state.ends.insert(e.member, e.status).is_some() {
                return Err("a member ended twice".into());
            }
        }
        k::STATS => {
            let answer = wire::StatsAnswer::decode(payload).map_err(bad)?;
            state.stats.insert(answer.id, answer.result);
        }
        k::ENDED => {
            let end = wire::RunEnd::decode(payload).map_err(bad)?;
            if state.end.replace(end).is_some() {
                return Err("the run ended twice".into());
            }
            ended = true;
        }
        other => return Err(format!("unknown answer kind {other}")),
    }
    shared.changed.notify_all();
    Ok(ended)
}

/// Keeps the host's last diagnostics until they end.
fn keep_said(shared: &Shared, mut said: std::process::ChildStderr) {
    let keep = said_kept();
    let mut buf = [0u8; 4096];
    while let Ok(n) = said.read(&mut buf) {
        if n == 0 {
            break;
        }
        let mut state = lock(&shared.state);
        state.said.extend(buf.get(..n).unwrap_or_default());
        let over = state.said.len().saturating_sub(keep);
        state.said.drain(..over);
    }
    lock(&shared.state).said_all = true;
    shared.changed.notify_all();
}

/// Waits for the host to exit, then reaps it under the state's lock as `exited` is set.
fn watch(shared: &Shared, host: &Host) {
    let awaited = host.awaited();
    let mut state = lock(&shared.state);
    let _ = lock(&host.child).wait();
    if let Err(e) = awaited {
        state
            .end
            .get_or_insert(RunEnd::Failed(format!("waiting for the run host: {e}")));
    }
    state.exited = true;
    shared.changed.notify_all();
}

fn quoted(said: &VecDeque<u8>) -> String {
    if said.is_empty() {
        return String::new();
    }
    let bytes: Vec<u8> = said.iter().copied().collect();
    format!(": {}", String::from_utf8_lossy(&bytes).trim_end())
}

impl ImageGuest for Session {
    fn members(&self) -> &[MemberInfo] {
        &self.members
    }

    fn published(&self) -> &[wire::Bound] {
        &self.published
    }

    fn take_output(&self, member: &MemberName, channel: Channel) -> Result<io::PipeReader, ImageRefusal> {
        let index = self.index(member)?.index;
        lock(&self.outputs)
            .remove(&(index, channel))
            .ok_or_else(|| ImageRefusal::Spec {
                what: "a member's output",
                why: format!("{member:?} {channel:?}: taken already, or the member does not run"),
            })
    }

    fn take_input(&self, _member: &MemberName) -> Result<io::PipeWriter, ImageRefusal> {
        Err(ImageRefusal::Unsupported {
            what: "a member's stdin",
            why: "a member's input connection is owed (D231's next slice)".into(),
        })
    }

    fn wait_member(&self, member: &MemberName, timeout: Duration) -> Result<Option<MemberEnd>, ImageRefusal> {
        let info = self.index(member)?;
        if !info.runs {
            return Err(ImageRefusal::Image(format!(
                "{member:?} is files alone, and runs nothing"
            )));
        }
        let index = info.index;
        Ok(self.shared.wait(timeout, |s| {
            if let Some(status) = s.ends.get(&index) {
                return Some(MemberEnd::Exited(*status));
            }
            (s.end.is_some() || s.gone()).then_some(MemberEnd::WithRun)
        }))
    }

    fn stats(&self, scope: StatsScope, patience: Duration) -> Result<Stats, ImageRefusal> {
        let id = {
            let mut state = lock(&self.shared.state);
            if let Some(end) = &state.end {
                return Err(ImageRefusal::Ended(end.clone()));
            }
            state.next_ask = state.next_ask.wrapping_add(1);
            state.next_ask
        };
        let ask = wire::StatsAsk { id, scope };
        self.shared
            .send(wire::to_host::STATS, &ask.encode())
            .map_err(|e| ImageRefusal::Os {
                what: "the run host's stats",
                kind: e.kind(),
            })?;
        let answer = self.shared.wait(patience, |s| {
            if let Some(answer) = s.stats.remove(&id) {
                return Some(answer.map_err(ImageRefusal::Boot));
            }
            if let Some(end) = &s.end {
                return Some(Err(ImageRefusal::Ended(end.clone())));
            }
            s.gone()
                .then(|| Err(ImageRefusal::Host("it exited without answering".into())))
        });
        answer.unwrap_or_else(|| {
            lock(&self.shared.state).stats.remove(&id);
            Err(ImageRefusal::Unsupported {
                what: "stats",
                why: format!("no answer within {patience:?}"),
            })
        })
    }

    fn stop(&self) -> Result<(), ImageRefusal> {
        // Asked of the host, which kills its VM at once and says the run's end; a host that
        // cannot be asked is killed itself, and its VM's link closing ends the VM's run
        // (`STOP_ORDER`). A run that has ended is not asked: its host is cleaning up.
        let ended = lock(&self.shared.state).end.is_some();
        if !ended && self.shared.send(wire::to_host::STOP, &[]).is_err() {
            self.host.kill(&lock(&self.shared.state));
        }
        // The run's end comes as its VM goes, which the host kills at once; or the host is
        // gone. The host's own exit after (its cleanup) is the watcher's to reap.
        self.shared
            .wait(Duration::MAX, |s| (s.end.is_some() || s.gone()).then_some(()));
        lock(&self.shared.to_host).take();
        let mut state = lock(&self.shared.state);
        state.end.get_or_insert(RunEnd::Stopped);
        self.shared.changed.notify_all();
        Ok(())
    }

    fn wait_for(&self, timeout: Duration) -> Result<Option<RunEnd>, ImageRefusal> {
        Ok(self.shared.wait(timeout, |s| {
            if let Some(end) = &s.end {
                return Some(end.clone());
            }
            s.gone().then(|| {
                RunEnd::Failed(format!(
                    "the run host exited without its run's end{}",
                    quoted(&s.said)
                ))
            })
        }))
    }
}

/// Its run ends with it: one still running is stopped; one ended has its host's stdin
/// closed, its cue to finish and exit, which the watcher then reaps.
impl Drop for Session {
    fn drop(&mut self) {
        let running = {
            let state = lock(&self.shared.state);
            state.end.is_none() && !state.gone()
        };
        if running {
            let _ = self.stop();
        }
        lock(&self.shared.to_host).take();
    }
}
