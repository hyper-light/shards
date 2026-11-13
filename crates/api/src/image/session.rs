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
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use super::wire::{self, Channel, MemberInfo, Ready, RunEnd, Stats, StatsScope};
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

/// A member's stdin, which the caller writes (plan step 5): the room its host gave and the
/// feed has not used, whether its pipe was taken, and its end as its host told it.
#[derive(Debug, Default)]
struct InputFlow {
    room: u64,
    taken: bool,
    ended: Option<(u64, wire::StreamEnd)>,
}

#[derive(Debug, Default)]
struct State {
    started: Option<wire::Started>,
    refused: Option<ImageRefusal>,
    flows: HashMap<(u32, Channel), Flow>,
    /// Each member's stdin its spec asked for, by its index.
    inputs: HashMap<u32, InputFlow>,
    ends: HashMap<u32, u32>,
    stats: HashMap<u32, Result<Stats, String>>,
    /// Each plug's or unplug's answer, by its ask's id (D235).
    plugs: HashMap<u32, Result<(), (wire::PlugRefusal, String)>>,
    /// Each room asked and still awaited, by its ask's id, and its answer once it came
    /// (D241): an answer to an ask no longer awaited, its patience run out, is dropped, so
    /// that it holds the asks in flight alone.
    rooms: Awaited<Option<u64>>,
    /// Each rate asked and still awaited, by its ask's id, and its answer once it came
    /// (D244), as rooms.
    rates: Awaited<Option<NonZeroU64>>,
    next_ask: u32,
    /// The run's own command's readiness under the nesting grant (D234), as its init last
    /// said it: init owns it, and moves it by D234's table.
    ready: Option<Ready>,
    end: Option<RunEnd>,
    /// The host's process has exited and is reaped.
    exited: bool,
    /// Its answers have all been read: its stdout has ended.
    answered: bool,
    said: VecDeque<u8>,
    /// Its diagnostics have all been read: its stderr has ended, its VM's with it.
    said_all: bool,
}

/// Each ask of a kind awaited, by its id, and its answer once it came, or why its host
/// refused it (D241, D244).
type Awaited<T> = HashMap<u32, Option<Result<T, String>>>;

/// A numbered ask of the session's, answered by its id (D241, D244): its kind; the ask and
/// its send, as a refusal names them; where its answer is awaited; what its host's refusal
/// is.
struct Asked<T> {
    kind: u8,
    what: &'static str,
    sending: &'static str,
    awaited: fn(&mut State) -> &mut Awaited<T>,
    refused: fn(String) -> ImageRefusal,
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

    /// Tells the run `room` (D241) and waits at most `patience` for what its init held it
    /// within.
    fn room(&self, room: Option<i64>, patience: Duration) -> Result<Option<u64>, ImageRefusal> {
        let asked = Asked {
            kind: wire::to_host::ROOM,
            what: "a room",
            sending: "the run host's room",
            awaited: |s| &mut s.rooms,
            refused: ImageRefusal::Room,
        };
        self.ask(asked, |id| wire::RoomAsk { id, room }.encode(), patience)
    }

    /// Tells the run `rate` (D244) and waits at most `patience` for what its VM holds its
    /// vCPUs to.
    fn rate(&self, rate: Option<NonZeroU64>, patience: Duration) -> Result<Option<NonZeroU64>, ImageRefusal> {
        let asked = Asked {
            kind: wire::to_host::RATE,
            what: "a rate",
            sending: "the run host's rate",
            awaited: |s| &mut s.rates,
            refused: ImageRefusal::Rate,
        };
        self.ask(asked, |id| wire::RateAsk { id, rate }.encode(), patience)
    }

    /// Sends `asked`'s ask, numbered by the session, and waits at most `patience` for its
    /// answer: the ask awaited from before it is sent until it is answered or given up, so
    /// that no answer outlives its ask (D241, D244).
    fn ask<T>(
        &self,
        asked: Asked<T>,
        ask: impl FnOnce(u32) -> Vec<u8>,
        patience: Duration,
    ) -> Result<T, ImageRefusal> {
        let id = {
            let mut state = lock(&self.state);
            if let Some(end) = &state.end {
                return Err(ImageRefusal::Ended(end.clone()));
            }
            state.next_ask = state.next_ask.wrapping_add(1);
            let id = state.next_ask;
            (asked.awaited)(&mut state).insert(id, None);
            id
        };
        let answer = self
            .send(asked.kind, &ask(id))
            .map_err(|e| ImageRefusal::Os {
                what: asked.sending,
                kind: e.kind(),
            })
            .and_then(|()| {
                self.wait(patience, |s| {
                    if let Some(answer) = (asked.awaited)(s).get_mut(&id).and_then(Option::take) {
                        return Some(answer.map_err(asked.refused));
                    }
                    if let Some(end) = &s.end {
                        return Some(Err(ImageRefusal::Ended(end.clone())));
                    }
                    s.gone()
                        .then(|| Err(ImageRefusal::Host("it exited without answering".into())))
                })
                .unwrap_or_else(|| {
                    Err(ImageRefusal::Unsupported {
                        what: asked.what,
                        why: format!("no answer within {patience:?}"),
                    })
                })
            });
        (asked.awaited)(&mut lock(&self.state)).remove(&id);
        answer
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
    /// Its command has the nesting grant (D234): its spec asked for it, and its host, which
    /// refuses a grant it cannot give, started it.
    nesting: bool,
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
        // Each member whose stdin its spec asked for, by its index in the host's answer.
        {
            let mut state = lock(&shared.state);
            for m in request.members.iter().filter(|m| m.input) {
                if let Some(info) = started
                    .members
                    .iter()
                    .find(|i| i.kind == m.kind && i.name == m.name)
                {
                    state.inputs.entry(info.index).or_default();
                }
            }
        }
        let session = Session {
            shared,
            host,
            members: started.members,
            published: started.published,
            outputs: Mutex::new(HashMap::new()),
            nesting: request.nesting,
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

    /// Asks its host a plug or an unplug (`kind`, D235), its payload made of the ask's id,
    /// and waits at most `patience` for its answer.
    fn plug_ask(
        &self,
        kind: u8,
        patience: Duration,
        payload: impl FnOnce(u32) -> Result<Vec<u8>, wire::WireError>,
    ) -> Result<(), ImageRefusal> {
        let id = {
            let mut state = lock(&self.shared.state);
            if let Some(end) = &state.end {
                return Err(ImageRefusal::Ended(end.clone()));
            }
            state.next_ask = state.next_ask.wrapping_add(1);
            state.next_ask
        };
        let payload = payload(id).map_err(|e| ImageRefusal::Spec {
            what: "a plug",
            why: e.to_string(),
        })?;
        self.shared.send(kind, &payload).map_err(|e| ImageRefusal::Os {
            what: "the run host's plug",
            kind: e.kind(),
        })?;
        let answer = self.shared.wait(patience, |s| {
            if let Some(answer) = s.plugs.remove(&id) {
                return Some(answer.map_err(|(cause, why)| ImageRefusal::Plug(cause, why)));
            }
            if let Some(end) = &s.end {
                return Some(Err(ImageRefusal::Ended(end.clone())));
            }
            s.gone()
                .then(|| Err(ImageRefusal::Host("it exited without answering".into())))
        });
        answer.unwrap_or_else(|| {
            lock(&self.shared.state).plugs.remove(&id);
            Err(ImageRefusal::Unsupported {
                what: "a plug",
                why: format!("no answer within {patience:?}"),
            })
        })
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

/// Moves what the caller writes to member `member`'s stdin to its host, no more at once than
/// the room the host gave, and its end once the caller closes its pipe; ends with the input,
/// the run or the host, dropping its pipe's read end so that the caller's writes fail then.
fn feed(shared: &Shared, member: u32, mut reader: io::PipeReader) {
    let mut buf = vec![0u8; AHEAD as usize];
    loop {
        // Room first: nothing is read of the caller that its host has no room for.
        let room = shared.wait(Duration::MAX, |s| {
            let gone = s.end.is_some() || s.answered;
            let flow = s.inputs.get(&member)?;
            if flow.ended.is_some() || gone {
                return Some(None);
            }
            (flow.room > 0).then_some(Some(flow.room))
        });
        let Some(Some(room)) = room else {
            return;
        };
        let want = usize::try_from(room).unwrap_or(usize::MAX).min(buf.len());
        let n = match reader.read(buf.get_mut(..want).unwrap_or_default()) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        if n == 0 {
            let end = wire::InputEnd {
                member,
                end: wire::StreamEnd::Closed,
            };
            let _ = shared.send(wire::to_host::INPUT_END, &end.encode());
            return;
        }
        if let Some(flow) = lock(&shared.state).inputs.get_mut(&member) {
            flow.room = flow.room.saturating_sub(n as u64);
        }
        let mut payload = Vec::with_capacity(4 + n);
        payload.extend_from_slice(&wire::Input::head(member));
        payload.extend_from_slice(buf.get(..n).unwrap_or_default());
        if shared.send(wire::to_host::INPUT, &payload).is_err() {
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
        k::PLUGGED => {
            let answer = wire::PlugAnswer::decode(payload).map_err(bad)?;
            state.plugs.insert(answer.id, answer.result);
        }
        k::HELD => {
            let answer = wire::RoomAnswer::decode(payload).map_err(bad)?;
            if let Some(awaited) = state.rooms.get_mut(&answer.id) {
                *awaited = Some(answer.result);
            }
        }
        k::RATED => {
            let answer = wire::RateAnswer::decode(payload).map_err(bad)?;
            if let Some(awaited) = state.rates.get_mut(&answer.id) {
                *awaited = Some(answer.result);
            }
        }
        k::READY => state.ready = Some(wire::ready(payload).map_err(bad)?),
        k::INPUT_ROOM => {
            let room = wire::InputRoom::decode(payload).map_err(bad)?;
            let flow = state
                .inputs
                .get_mut(&room.member)
                .ok_or_else(|| "room for a member with no stdin of its owner's".to_string())?;
            flow.room = flow
                .room
                .checked_add(room.bytes)
                .ok_or_else(|| "a stdin's room past what a u64 counts".to_string())?;
        }
        k::INPUT_ENDED => {
            let e = wire::InputEnded::decode(payload).map_err(bad)?;
            let flow = state
                .inputs
                .get_mut(&e.member)
                .ok_or_else(|| "an end of a member with no stdin of its owner's".to_string())?;
            if flow.ended.replace((e.written, e.end)).is_some() {
                return Err("a stdin ended twice".into());
            }
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

/// What a wait for the readiness of a run's command past `seen` finds in `s`: a refusal at
/// once where the command has no nesting grant (`granted`), whose readiness is the grant's
/// (D234); its latest, once that differs from `seen`; else the run's end, or its host gone
/// without one; else nothing yet.
fn ready_past(s: &State, granted: bool, seen: Option<Ready>) -> Option<Result<Ready, ImageRefusal>> {
    if !granted {
        return Some(Err(ImageRefusal::Spec {
            what: "nesting",
            why: "readiness is the nesting grant's (D234), which the run does not ask for".into(),
        }));
    }
    if let Some(ready) = s.ready.filter(|_| s.ready != seen) {
        return Some(Ok(ready));
    }
    if let Some(end) = &s.end {
        return Some(Err(ImageRefusal::Ended(end.clone())));
    }
    s.gone().then(|| {
        Err(ImageRefusal::Host(format!(
            "the run host exited without its run's end{}",
            quoted(&s.said)
        )))
    })
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

    fn take_input(&self, member: &MemberName) -> Result<io::PipeWriter, ImageRefusal> {
        let index = self.index(member)?.index;
        {
            let mut state = lock(&self.shared.state);
            let flow = state.inputs.get_mut(&index).ok_or_else(|| ImageRefusal::Spec {
                what: "a member's stdin",
                why: format!("{member:?}: its spec asks for no stdin of it"),
            })?;
            if flow.taken {
                return Err(ImageRefusal::Spec {
                    what: "a member's stdin",
                    why: format!("{member:?}: taken already"),
                });
            }
            flow.taken = true;
        }
        let (reader, writer) = io::pipe().map_err(|e| ImageRefusal::Os {
            what: "a member's stdin pipe",
            kind: e.kind(),
        })?;
        let shared = self.shared.clone();
        std::thread::Builder::new()
            .name("shards-api-input".into())
            .spawn(move || feed(&shared, index, reader))
            .map_err(|e| ImageRefusal::Os {
                what: "a member's stdin feed",
                kind: e.kind(),
            })?;
        Ok(writer)
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

    fn room(&self, room: Option<i64>, patience: Duration) -> Result<Option<u64>, ImageRefusal> {
        self.shared.room(room, patience)
    }

    fn rate(&self, rate: Option<NonZeroU64>, patience: Duration) -> Result<Option<NonZeroU64>, ImageRefusal> {
        self.shared.rate(rate, patience)
    }

    fn plug(
        &self,
        member: &MemberName,
        host: &std::path::Path,
        at: &str,
        read_only: bool,
        patience: Duration,
    ) -> Result<(), ImageRefusal> {
        let info = self.index(member)?;
        if !info.runs {
            return Err(ImageRefusal::Plug(
                wire::PlugRefusal::Member,
                format!("{member:?} is files alone, and runs nothing"),
            ));
        }
        let host = host.to_str().ok_or_else(|| ImageRefusal::Spec {
            what: "a plug's host directory",
            why: format!("{}: not UTF-8", host.display()),
        })?;
        let member = info.index;
        self.plug_ask(wire::to_host::PLUG, patience, |id| {
            wire::PlugAsk {
                id,
                member,
                host: host.into(),
                at: at.into(),
                read_only,
            }
            .encode()
        })
    }

    fn unplug(&self, at: &str, patience: Duration) -> Result<(), ImageRefusal> {
        self.plug_ask(wire::to_host::UNPLUG, patience, |id| {
            wire::UnplugAsk { id, at: at.into() }.encode()
        })
    }

    fn wait_ready(&self, seen: Option<Ready>, timeout: Duration) -> Result<Option<Ready>, ImageRefusal> {
        self.shared
            .wait(timeout, |s| ready_past(s, self.nesting, seen))
            .transpose()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A session's shared state with no host behind it: its answers taken as its reader
    /// takes them.
    fn shared() -> Shared {
        Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            to_host: Mutex::new(None),
        }
    }

    /// A member's stdin's room and end are taken as its host says them: room adds up, an
    /// end comes once, and a member whose stdin its spec never asked for is given none.
    #[test]
    fn a_stdins_room_and_end_are_its_hosts_and_come_for_asked_members_alone() {
        let s = shared();
        lock(&s.state).inputs.insert(2, InputFlow::default());
        let room = |member, bytes| wire::InputRoom { member, bytes }.encode();
        assert_eq!(take(&s, wire::to_client::INPUT_ROOM, &room(2, 5)), Ok(false));
        assert_eq!(take(&s, wire::to_client::INPUT_ROOM, &room(2, 3)), Ok(false));
        assert_eq!(lock(&s.state).inputs[&2].room, 8);
        assert!(take(&s, wire::to_client::INPUT_ROOM, &room(3, 1)).is_err());
        assert!(take(&s, wire::to_client::INPUT_ROOM, &room(2, u64::MAX)).is_err());
        let ended = wire::InputEnded {
            member: 2,
            written: 4,
            end: wire::StreamEnd::Aborted(32),
        }
        .encode();
        assert_eq!(take(&s, wire::to_client::INPUT_ENDED, &ended), Ok(false));
        assert_eq!(
            lock(&s.state).inputs[&2].ended,
            Some((4, wire::StreamEnd::Aborted(32)))
        );
        assert!(
            take(&s, wire::to_client::INPUT_ENDED, &ended).is_err(),
            "ended twice"
        );
    }

    /// The caller's bytes reach the host as they were written, never past the room it gave,
    /// and the pipe's close is the input's end. The host here is `cat`, which hands back what
    /// the feed sent.
    #[cfg(unix)]
    #[test]
    fn a_feed_sends_its_callers_bytes_within_its_room_then_its_end() {
        let mut host = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let s = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            to_host: Mutex::new(host.stdin.take()),
        });
        lock(&s.state).inputs.insert(1, InputFlow::default());
        let (reader, mut writer) = io::pipe().unwrap();
        let feeding = {
            let s = s.clone();
            std::thread::spawn(move || feed(&s, 1, reader))
        };
        let payload = b"\0\xff\x80{\"id\":1}\nno-newline";
        writer.write_all(payload).unwrap();
        drop(writer);
        // Room for 7 bytes, then the rest: no frame carries more than it was given.
        let room = |bytes| wire::InputRoom { member: 1, bytes }.encode();
        assert_eq!(take(&s, wire::to_client::INPUT_ROOM, &room(7)), Ok(false));
        let mut from_host = host.stdout.take().unwrap();
        let (kind, first) = wire::read_frame(&mut from_host).unwrap().unwrap();
        assert_eq!(kind, wire::to_host::INPUT);
        let first = wire::Input::decode(&first).unwrap();
        assert_eq!((first.member, first.bytes.as_slice()), (1, &payload[..7]));
        assert_eq!(take(&s, wire::to_client::INPUT_ROOM, &room(1024)), Ok(false));
        let mut got = first.bytes;
        loop {
            let (kind, frame) = wire::read_frame(&mut from_host).unwrap().unwrap();
            if kind == wire::to_host::INPUT_END {
                assert_eq!(
                    wire::InputEnd::decode(&frame).unwrap(),
                    wire::InputEnd {
                        member: 1,
                        end: wire::StreamEnd::Closed
                    }
                );
                break;
            }
            assert_eq!(kind, wire::to_host::INPUT);
            got.extend(wire::Input::decode(&frame).unwrap().bytes);
        }
        assert_eq!(got, payload);
        feeding.join().unwrap();
        lock(&s.to_host).take();
        host.wait().unwrap();
    }

    /// A refusal comes back typed by its cause, its host's words unchanged: the nesting
    /// grant asked of an image that does not give it (D234) a widening, in the daemon's
    /// words; a crafted image's (NESTING beside an agent) the image's, in its build's, as
    /// the daemon's read of the image says them.
    #[test]
    fn refusals_come_typed_in_their_hosts_words() {
        let undeclared = shards_abi::run::nesting::undeclared();
        let crafted = format!(
            "the image's Agentfile (sha256:00): {}",
            shards_abi::run::beside::nesting("agent", "a")
        );
        for (cause, why, typed) in [
            (
                wire::Cause::Widening,
                &undeclared,
                ImageRefusal::Widening(undeclared.clone()),
            ),
            (wire::Cause::Image, &crafted, ImageRefusal::Image(crafted.clone())),
        ] {
            let s = shared();
            let refused = wire::Refused {
                cause,
                why: why.clone(),
            }
            .encode()
            .unwrap();
            assert_eq!(take(&s, wire::to_client::REFUSED, &refused), Ok(false));
            assert_eq!(lock(&s.state).refused, Some(typed));
        }
    }

    /// A room's answer is kept for the ask awaiting it alone (D241): one for an ask no
    /// longer awaited, its patience run out, is dropped, so that the session holds the asks
    /// in flight and nothing more; one that is no answer breaks the channel.
    #[test]
    fn a_rooms_answer_is_kept_for_its_awaited_ask_alone() {
        let s = shared();
        lock(&s.state).rooms.insert(4, None);
        let held = |id, result| wire::RoomAnswer { id, result }.encode().unwrap();
        assert_eq!(
            take(&s, wire::to_client::HELD, &held(4, Ok(Some(7 << 20)))),
            Ok(false)
        );
        assert_eq!(lock(&s.state).rooms.get(&4), Some(&Some(Ok(Some(7 << 20)))));
        assert_eq!(take(&s, wire::to_client::HELD, &held(5, Ok(None))), Ok(false));
        assert!(
            !lock(&s.state).rooms.contains_key(&5),
            "an answer no ask awaits is dropped"
        );
        assert!(take(&s, wire::to_client::HELD, &[0, 0, 0, 4, 9]).is_err());
    }

    /// A room no host hears is refused, typed, and leaves no ask awaited; a run that has
    /// ended is told none (D241).
    #[test]
    fn a_room_no_host_hears_is_refused_and_leaves_no_ask() {
        let s = shared();
        assert_eq!(
            s.room(Some(1 << 20), BOUND),
            Err(ImageRefusal::Os {
                what: "the run host's room",
                kind: io::ErrorKind::BrokenPipe
            })
        );
        assert!(lock(&s.state).rooms.is_empty());
        lock(&s.state).end = Some(RunEnd::Stopped);
        assert_eq!(s.room(None, BOUND), Err(ImageRefusal::Ended(RunEnd::Stopped)));
        assert!(lock(&s.state).rooms.is_empty());
    }

    /// A rate's answer is kept for the ask awaiting it alone (D244), as a room's is: one for
    /// an ask no longer awaited is dropped; one that is no answer breaks the channel.
    #[test]
    fn a_rates_answer_is_kept_for_its_awaited_ask_alone() {
        let s = shared();
        lock(&s.state).rates.insert(4, None);
        let rated = |id, result| wire::RateAnswer { id, result }.encode().unwrap();
        assert_eq!(
            take(&s, wire::to_client::RATED, &rated(4, Ok(NonZeroU64::new(1500)))),
            Ok(false)
        );
        assert_eq!(
            lock(&s.state).rates.get(&4),
            Some(&Some(Ok(NonZeroU64::new(1500))))
        );
        assert_eq!(take(&s, wire::to_client::RATED, &rated(5, Ok(None))), Ok(false));
        assert!(
            !lock(&s.state).rates.contains_key(&5),
            "an answer no ask awaits is dropped"
        );
        assert!(take(&s, wire::to_client::RATED, &[0, 0, 0, 4, 9]).is_err());
    }

    /// A rate no host hears is refused, typed, and leaves no ask awaited; a run that has
    /// ended is told none (D244).
    #[test]
    fn a_rate_no_host_hears_is_refused_and_leaves_no_ask() {
        let s = shared();
        assert_eq!(
            s.rate(NonZeroU64::new(500), BOUND),
            Err(ImageRefusal::Os {
                what: "the run host's rate",
                kind: io::ErrorKind::BrokenPipe
            })
        );
        assert!(lock(&s.state).rates.is_empty());
        lock(&s.state).end = Some(RunEnd::Stopped);
        assert_eq!(s.rate(None, BOUND), Err(ImageRefusal::Ended(RunEnd::Stopped)));
        assert!(lock(&s.state).rates.is_empty());
    }

    /// How long a test waits for what should come at once: only a failing test waits it
    /// out, as crates/shards/tests/api_image.rs's `TIMEOUT` bounds its waits.
    const BOUND: Duration = Duration::from_secs(120);

    /// A wait with no clock for the readiness past `seen`, on a thread of its own, its
    /// answer sent on as it comes: a test that fails ends at once, never held by its waiter.
    fn waiter(
        s: &Arc<Shared>,
        seen: Option<Ready>,
    ) -> std::sync::mpsc::Receiver<Result<Option<Ready>, ImageRefusal>> {
        let (tx, rx) = std::sync::mpsc::channel();
        let s = Arc::clone(s);
        std::thread::spawn(move || {
            let _ = tx.send(s.wait(Duration::MAX, |st| ready_past(st, true, seen)).transpose());
        });
        rx
    }

    /// READY moves the run's readiness as its host relays it (D234), and answers a waiter
    /// with no clock as it is taken: its first, then each that differs from what the waiter
    /// saw, and none past what it saw. One that is no readiness breaks the channel; the
    /// run's end ends a wait for a newer one, and what came before it is still told. A run
    /// without the grant is refused a wait at once; a host gone without its run's end ends
    /// one.
    #[test]
    fn readiness_is_told_as_it_moves() {
        let s = Arc::new(shared());
        let wait = |seen, timeout| s.wait(timeout, |st| ready_past(st, true, seen)).transpose();
        assert_eq!(wait(None, Duration::ZERO), Ok(None));
        let first = waiter(&s, None);
        assert_eq!(
            take(&s, wire::to_client::READY, &Ready::Passed(3).encode()),
            Ok(false)
        );
        assert_eq!(first.recv_timeout(BOUND).unwrap(), Ok(Some(Ready::Passed(3))));
        assert_eq!(wait(None, Duration::ZERO), Ok(Some(Ready::Passed(3))));
        assert_eq!(wait(Some(Ready::Passed(3)), Duration::ZERO), Ok(None));
        let next = waiter(&s, Some(Ready::Passed(3)));
        assert_eq!(
            take(&s, wire::to_client::READY, &Ready::Late(9).encode()),
            Ok(false)
        );
        assert_eq!(next.recv_timeout(BOUND).unwrap(), Ok(Some(Ready::Late(9))));
        for malformed in [&[1, 0, 0, 0, 0, 0, 0, 0, 1][..], &[], &[2; 10]] {
            assert!(
                take(&s, wire::to_client::READY, malformed).is_err(),
                "{malformed:?}"
            );
        }
        assert_eq!(lock(&s.state).ready, Some(Ready::Late(9)));
        let ended = RunEnd::Exited(0).encode().unwrap();
        assert_eq!(take(&s, wire::to_client::ENDED, &ended), Ok(true));
        assert_eq!(
            wait(Some(Ready::Late(9)), BOUND),
            Err(ImageRefusal::Ended(RunEnd::Exited(0)))
        );
        assert_eq!(wait(None, Duration::ZERO), Ok(Some(Ready::Late(9))));
        assert!(matches!(
            s.wait(BOUND, |st| ready_past(st, false, None)),
            Some(Err(ImageRefusal::Spec { what: "nesting", .. }))
        ));
        let gone = shared();
        {
            let mut st = lock(&gone.state);
            st.exited = true;
            st.answered = true;
        }
        assert!(matches!(
            gone.wait(BOUND, |st| ready_past(st, true, None)),
            Some(Err(ImageRefusal::Host(_)))
        ));
    }
}
