//! The client's side of one image run: the run host's process (`shards api-run`), a thread
//! reading its answers, one keeping its last diagnostics, one waiting for its exit, and one
//! per member channel moving its bytes into the pipe the caller takes. Every wait is on a
//! fact: an answer, a channel's bytes or end, the host's exit; a caller's patience bounds
//! only its own wait.
//!
//! The host is started holding its three standard streams alone (D248), reaped under the
//! state's lock as `exited` is set, and signalled only while it is unreaped
//! (`shards_ipc::Child`), so no signal reaches another process that took its pid (as
//! `crate::client` does a direct boot's).
//!
//! A lease the host asks (D242) is asked of the run's lessor on a thread of its own, and every
//! lease it gives is released once: as the host says the run released them, as its answers
//! end where it never does, or, given after either, as it comes.

use std::collections::{HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::io::{self, Read as _, Write as _};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use super::wire::{self, Channel, LeaseRefusal, MemberInfo, Ready, RunEnd, Stats, StatsScope};
use super::{ImageGuest, ImageRefusal, ImageSpec, Leased, Lessor, MemberEnd, MemberName, RunLessor};

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
    /// The members whose stdin the request asks for, by kind and name: each one's flow is
    /// made as the run's start is taken ([`take`]), by its index there.
    asked_inputs: Vec<(wire::MemberKind, String)>,
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
    /// (D247), as rooms.
    rates: Awaited<Option<NonZeroU64>>,
    /// Each resize asked and still awaited, by its ask's id, and its answer once it came
    /// (D244), held as [`rooms`](Self::rooms) is.
    resizes: HashMap<u32, Option<Result<(), String>>>,
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
    /// The leases its host asked (D242).
    leases: Leases,
}

/// A lease of the run's, as its orchestrator's side holds it (D242, R04): asked of the run's
/// lessor and not yet answered; or given, the lessor's name for it kept for its release.
/// Refused and released are its ends, where it leaves the ledger.
#[derive(Debug)]
enum Holding {
    Asked,
    Given(String),
}

/// The run's leases by their asks' ids, whether the run has released them (once it has, a
/// lease given after is released as it comes), and the SHA-256 of the run's own key they are
/// issued to, where its start told one (D252), taken as its host's answers are.
#[derive(Debug, Default)]
struct Leases {
    each: HashMap<u32, Holding>,
    released: bool,
    key: Option<[u8; 32]>,
}

/// Each ask of a kind awaited, by its id, and its answer once it came, or why its host
/// refused it (D241, D247).
type Awaited<T> = HashMap<u32, Option<Result<T, String>>>;

/// A numbered ask of the session's, answered by its id (D241, D247): its kind; the ask and
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
    /// A session's state before its host has answered, for a request staging `members`:
    /// those whose stdin it asks for named, for their flows to be made with its start.
    fn asking(members: &[wire::MemberStage]) -> State {
        State {
            asked_inputs: members
                .iter()
                .filter(|m| m.input)
                .map(|m| (m.kind, m.name.clone()))
                .collect(),
            ..State::default()
        }
    }

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
    to_host: Mutex<Option<io::PipeWriter>>,
    /// What leases the run's credentials (D242), where its spec gave one.
    lessor: Option<RunLessor>,
    /// The references the run's grants name: no other is leased.
    granted: Vec<String>,
}

impl Shared {
    fn send(&self, kind: u8, payload: &[u8]) -> io::Result<()> {
        let frame = wire::frame(kind, payload).map_err(io::Error::other)?;
        match lock(&self.to_host).as_mut() {
            Some(stdin) => stdin.write_all(&frame).and_then(|()| stdin.flush()),
            None => Err(io::ErrorKind::BrokenPipe.into()),
        }
    }

    /// Sends a frame as its header, then its payload, never copied into one buffer: a payload
    /// holding a credential's value (D242) is its caller's to wipe.
    fn send_parts(&self, kind: u8, payload: &[u8]) -> io::Result<()> {
        let header = wire::header(kind, payload.len()).map_err(io::Error::other)?;
        match lock(&self.to_host).as_mut() {
            Some(stdin) => stdin
                .write_all(&header)
                .and_then(|()| stdin.write_all(payload))
                .and_then(|()| stdin.flush()),
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

    /// Tells the run `rate` (D247) and waits at most `patience` for what its VM holds its
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
    /// that no answer outlives its ask (D241, D247).
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

    /// Asks member `member`'s own terminal sized (D244) and waits at most `patience` for
    /// whether its init did; its ask awaited from before it is sent until it is answered or
    /// given up, as [`room`](Self::room)'s is.
    fn resize(&self, member: u32, rows: u16, cols: u16, patience: Duration) -> Result<(), ImageRefusal> {
        let id = {
            let mut state = lock(&self.state);
            if let Some(end) = &state.end {
                return Err(ImageRefusal::Ended(end.clone()));
            }
            state.next_ask = state.next_ask.wrapping_add(1);
            let id = state.next_ask;
            state.resizes.insert(id, None);
            id
        };
        let ask = wire::ResizeAsk {
            id,
            member,
            rows,
            cols,
        };
        let answer = self
            .send(wire::to_host::RESIZE, &ask.encode())
            .map_err(|e| ImageRefusal::Os {
                what: "the run host's resize",
                kind: e.kind(),
            })
            .and_then(|()| {
                self.wait(patience, |s| {
                    if let Some(answer) = s.resizes.get_mut(&id).and_then(Option::take) {
                        return Some(answer.map_err(ImageRefusal::Terminal));
                    }
                    if let Some(end) = &s.end {
                        return Some(Err(ImageRefusal::Ended(end.clone())));
                    }
                    s.gone()
                        .then(|| Err(ImageRefusal::Host("it exited without answering".into())))
                })
                .unwrap_or_else(|| {
                    Err(ImageRefusal::Unsupported {
                        what: "a resize",
                        why: format!("no answer within {patience:?}"),
                    })
                })
            });
        lock(&self.state).resizes.remove(&id);
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

/// The run host started, and the ends of its standard streams this process holds.
struct Host {
    process: shards_ipc::Child,
    to_host: io::PipeWriter,
    from_host: io::PipeReader,
    said: io::PipeReader,
}

/// Starts `program` with `args`, its environment this process's with `env` over it, holding
/// its stdin, stdout and stderr, pipes to this process, and nothing else this process holds
/// (D248): `shards_ipc`'s spawn gives a child exactly the descriptors named for it
/// (`POSIX_SPAWN_CLOEXEC_DEFAULT` on macOS, every descriptor close-on-exec on Linux, a
/// `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` of its three on Windows), where std's would hand it
/// whatever another thread of the orchestrator holds inheritable at that moment. A variable's
/// name that is not UTF-8 is refused, as that spawn names variables in UTF-8.
fn start_host(program: &Path, args: &[&OsStr], env: &[(OsString, OsString)]) -> Result<Host, ImageRefusal> {
    let os = |what: &'static str| move |e: io::Error| ImageRefusal::Os { what, kind: e.kind() };
    let set = env
        .iter()
        .map(|(name, value)| {
            name.to_str()
                .map(|name| (name, value.as_os_str()))
                .ok_or_else(|| ImageRefusal::Spec {
                    what: "the run host's environment",
                    why: format!("{name:?}: a variable's name that is not UTF-8"),
                })
        })
        .collect::<Result<Vec<(&str, &OsStr)>, ImageRefusal>>()?;
    let boot = |e: io::Error| ImageRefusal::Boot(format!("{}: {e}", program.display()));
    let found = on_path(program).map_err(boot)?;
    let (child_in, to_host) = io::pipe().map_err(os("the run host's pipes"))?;
    let (from_host, child_out) = io::pipe().map_err(os("the run host's pipes"))?;
    let (said, child_err) = io::pipe().map_err(os("the run host's pipes"))?;
    #[cfg(unix)]
    let ends = {
        use std::os::fd::AsFd as _;
        [
            (child_in.as_fd(), 0),
            (child_out.as_fd(), 1),
            (child_err.as_fd(), 2),
        ]
    };
    #[cfg(windows)]
    let ends = {
        use std::os::windows::io::AsHandle as _;
        [
            (child_in.as_handle(), 0),
            (child_out.as_handle(), 1),
            (child_err.as_handle(), 2),
        ]
    };
    let process = shards_ipc::spawn_with(&found, args, &ends, false, &set).map_err(boot)?;
    // The host holds its ends now; this process's go, so that each pipe ends with it.
    drop((child_in, child_out, child_err));
    Ok(Host {
        process,
        to_host,
        from_host,
        said,
    })
}

/// `program` as std's spawn finds one: itself where it names a directory; else the first file
/// of that name in a directory of this process's `PATH` (on Windows `.exe` added to a name
/// with no extension, as CreateProcessW adds it); else refused, not found. Never one in the
/// current directory, where std's spawn never looks for a name alone either: posix_spawn(2)
/// would take that name as relative to it, and CreateProcessW looks there before `PATH`
/// (D248). A directory of `PATH` that is empty, which exec(3) reads as the current one, is
/// skipped, as std's Windows spawn skips it.
fn on_path(program: &Path) -> io::Result<PathBuf> {
    if !program.parent().is_some_and(|dir| dir.as_os_str().is_empty()) {
        return Ok(program.to_path_buf());
    }
    let name = if cfg!(windows) && program.extension().is_none() {
        program.with_extension("exe")
    } else {
        program.to_path_buf()
    };
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .filter(|dir| !dir.as_os_str().is_empty())
                .map(|dir| dir.join(&name))
                .find(|candidate| candidate.is_file())
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "in no directory of PATH"))
}

/// Kills the host at once, unless it is reaped (`shards_ipc::Child::kill`).
fn kill(host: &shards_ipc::Child) {
    let _ = host.kill(shards_ipc::signal::SIGKILL);
}

/// One image run, as its client holds it.
#[derive(Debug)]
pub(super) struct Session {
    shared: Arc<Shared>,
    host: Arc<shards_ipc::Child>,
    members: Vec<MemberInfo>,
    published: Vec<wire::Bound>,
    /// Where its agent-control endpoint is bound (D252), where it asked for one.
    control: Option<wire::ControlAt>,
    outputs: Mutex<HashMap<(u32, Channel), io::PipeReader>>,
    /// Its command has the nesting grant (D234): its spec asked for it, and its host, which
    /// refuses a grant it cannot give, started it.
    nesting: bool,
    /// Each member whose spec asked for a terminal of its own (D244), by its index.
    terminals: Vec<u32>,
    /// The period its VM's governor holds a rate over, as its host said it as the run
    /// started (D247).
    rate_period: Duration,
    /// How its microVM started, as its host said it (D253).
    started_from: wire::StartedFrom,
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
        let Host {
            process,
            to_host,
            from_host,
            said,
        } = start_host(&binary, &[OsStr::new("api-run")], &spec.env)?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State::asking(&request.members)),
            changed: Condvar::new(),
            to_host: Mutex::new(Some(to_host)),
            lessor: spec.lessor.clone(),
            granted: request.credentials.iter().map(|g| g.reference.clone()).collect(),
        });
        // Shared: the session and its exit watcher, which outlives this call.
        let host = Arc::new(process);
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
            kill(&host);
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
        // Its endpoint told exactly where one was asked for (D252): a host that binds none for
        // a run that asked, or one for a run that did not, broke the channel's contract.
        if request.control.is_some() != started.control.is_some() {
            end_it(&shared);
            return Err(ImageRefusal::Host(
                "the run host's start told an agent-control endpoint other than the run asked".into(),
            ));
        }
        // Each member whose terminal its spec asked for (D244), by its index likewise.
        let terminals = request
            .members
            .iter()
            .filter(|m| m.terminal)
            .filter_map(|m| {
                started
                    .members
                    .iter()
                    .find(|i| i.kind == m.kind && i.name == m.name)
                    .map(|i| i.index)
            })
            .collect();
        let session = Session {
            shared,
            host,
            members: started.members,
            published: started.published,
            control: started.control,
            outputs: Mutex::new(HashMap::new()),
            nesting: request.nesting,
            terminals,
            rate_period: started.rate_period,
            started_from: started.from,
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
/// the host's stdin: the host then ends. Every lease the run was given is released as its
/// host says the run released them, or as its answers end where it never says so (D242).
fn read_answers(shared: &Arc<Shared>, mut from_host: io::PipeReader) {
    let failed = loop {
        let (kind, payload) = match wire::read_frame(&mut from_host) {
            Ok(Some(frame)) => frame,
            Ok(None) => break None,
            Err(e) => break Some(format!("its answers: {e}")),
        };
        let taken = match kind {
            wire::to_client::LEASE => lease_asked(shared, &payload).map(|()| false),
            wire::to_client::RELEASED => wire::released(&payload)
                .map(|()| {
                    release(shared);
                    false
                })
                .map_err(|e| e.to_string()),
            _ => take(shared, kind, &payload),
        };
        match taken {
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
    release(shared);
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
        k::STARTED => {
            let started = wire::Started::decode(payload).map_err(bad)?;
            // The run's own key, which its leases are issued to (D242), known before any ask.
            state.leases.key = started.control.map(|at| at.vm);
            // Each asked member's flow, by its index in the start, made under this lock: the
            // host gives it room as soon as its input connection comes, which this reader
            // may take before the caller waiting for the start runs again.
            let State {
                asked_inputs, inputs, ..
            } = &mut *state;
            for (kind, name) in asked_inputs.iter() {
                if let Some(info) = started
                    .members
                    .iter()
                    .find(|i| i.kind == *kind && i.name == *name)
                {
                    inputs.entry(info.index).or_default();
                }
            }
            state.started = Some(started);
        }
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
        k::RESIZED => {
            let answer = wire::ResizeAnswer::decode(payload).map_err(bad)?;
            if let Some(awaited) = state.resizes.get_mut(&answer.id) {
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

/// A lease its host asks (D242), asked of the run's lessor on a thread of its own, so that
/// no answer of the host's waits on it; refused at once once the run has released its
/// leases. One of a run with no lessor, of a reference no grant of the run names, or by an id
/// the run holds a lease by breaks the channel's contract.
fn lease_asked(shared: &Arc<Shared>, payload: &[u8]) -> Result<(), String> {
    let ask = wire::LeaseAsk::decode(payload).map_err(|e| e.to_string())?;
    let Some(lessor) = shared.lessor.clone() else {
        return Err("a lease asked of a run with no lessor".into());
    };
    if !shared.granted.contains(&ask.reference) {
        return Err(format!(
            "a lease of {}, which no grant of the run names",
            ask.reference
        ));
    }
    // Asked under the run's own key, where its start told one.
    let key = {
        let mut state = lock(&shared.state);
        if state.leases.released {
            drop(state);
            answer_lease(shared, ask.id, Err(LeaseRefusal::Ended));
            return Ok(());
        }
        match state.leases.each.entry(ask.id) {
            std::collections::hash_map::Entry::Occupied(_) => {
                return Err(format!("a lease asked by {}, an id the run holds one by", ask.id));
            }
            std::collections::hash_map::Entry::Vacant(asked) => {
                asked.insert(Holding::Asked);
            }
        }
        state.leases.key
    };
    let id = ask.id;
    let asking = Arc::clone(shared);
    let spawned = std::thread::Builder::new()
        .name("shards-api-lease".into())
        .spawn(move || {
            let got = lessor.0.lease(&ask.reference, key);
            let result = settle(&asking, &*lessor.0, ask.id, got, Instant::now());
            answer_lease(&asking, ask.id, result);
        });
    if spawned.is_err() {
        lock(&shared.state).leases.each.remove(&id);
        let why = "no thread to ask its lessor on".to_string();
        answer_lease(shared, id, Err(LeaseRefusal::Unavailable(why)));
    }
    Ok(())
}

/// What lease `id`, as the run's lessor answered it (`got`), becomes at `now` (R04): given,
/// kept for its release, living what is left of it; refused as the lessor said; or, given
/// after the run released its leases or with nothing of it left, released at once and
/// refused as ended or lapsed.
fn settle(
    shared: &Shared,
    lessor: &dyn Lessor,
    id: u32,
    got: Result<Leased, LeaseRefusal>,
    now: Instant,
) -> Result<wire::Lease, LeaseRefusal> {
    let mut state = lock(&shared.state);
    state.leases.each.remove(&id);
    let leased = got?;
    // Its life past a u64 of nanoseconds (584 years) is said as that much.
    let lives = leased
        .until
        .checked_duration_since(now)
        .and_then(|left| NonZeroU64::new(u64::try_from(left.as_nanos()).unwrap_or(u64::MAX)));
    let refused = match lives {
        _ if state.leases.released => LeaseRefusal::Ended,
        None => LeaseRefusal::Lapsed,
        Some(lives) => {
            state.leases.each.insert(id, Holding::Given(leased.lease.clone()));
            return Ok(wire::Lease {
                value: leased.value,
                lives,
            });
        }
    };
    drop(state);
    lessor.release(&leased.lease);
    Err(refused)
}

/// Tells the run's host lease `id`'s answer (`LEASED`), every copy of its value wiped once
/// sent; one past the channel's bound is told unavailable, its lease released with the run's.
fn answer_lease(shared: &Shared, id: u32, result: Result<wire::Lease, LeaseRefusal>) {
    let answer = wire::LeaseAnswer { id, result };
    let encoded = answer.encode().map(zeroize::Zeroizing::new);
    drop(answer);
    let payload = encoded.or_else(|e| {
        let refused = wire::LeaseAnswer {
            id,
            result: Err(LeaseRefusal::Unavailable(format!("its value: {e}"))),
        };
        refused.encode().map(zeroize::Zeroizing::new)
    });
    // A host gone is told nothing: the run's end releases the lease.
    if let Ok(payload) = payload {
        let _ = shared.send_parts(wire::to_host::LEASED, &payload);
    }
}

/// Releases every lease the run was given (D242), once; a lease given after is released as
/// it comes ([`settle`]).
fn release(shared: &Shared) {
    let given: Vec<String> = {
        let mut state = lock(&shared.state);
        if state.leases.released {
            return;
        }
        state.leases.released = true;
        shared.changed.notify_all();
        state
            .leases
            .each
            .drain()
            .filter_map(|(_, held)| match held {
                Holding::Given(lease) => Some(lease),
                Holding::Asked => None,
            })
            .collect()
    };
    if let Some(lessor) = &shared.lessor {
        for lease in given {
            lessor.0.release(&lease);
        }
    }
}

/// Keeps the host's last diagnostics until they end.
fn keep_said(shared: &Shared, mut said: io::PipeReader) {
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
fn watch(shared: &Shared, host: &shards_ipc::Child) {
    let awaited = host.ended();
    let mut state = lock(&shared.state);
    let _ = host.wait();
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

    fn control(&self) -> Option<&wire::ControlAt> {
        self.control.as_ref()
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

    fn rate_period(&self) -> Duration {
        self.rate_period
    }

    fn started_from(&self) -> wire::StartedFrom {
        self.started_from
    }

    fn resize(
        &self,
        member: &MemberName,
        rows: u16,
        cols: u16,
        patience: Duration,
    ) -> Result<(), ImageRefusal> {
        let index = self.index(member)?.index;
        if !self.terminals.contains(&index) {
            return Err(ImageRefusal::Spec {
                what: "a member's terminal",
                why: format!("{member:?}: its spec asks for no terminal of it"),
            });
        }
        self.shared.resize(index, rows, cols, patience)
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
            kill(&self.host);
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
            lessor: None,
            granted: Vec::new(),
        }
    }

    /// A member's stdin has its flow as the run's start is taken, under the lock that answer
    /// is taken in: the host gives it room (`INPUT_ROOM`) as soon as its input connection
    /// comes, which this session's reader may take before the caller waiting for the start
    /// runs again; a room for a member with no flow breaks the channel. A member whose stdin
    /// was not asked for has none.
    #[test]
    fn a_stdins_room_right_after_the_start_is_its_members() {
        let stage = |name: &str, input: bool| wire::MemberStage {
            kind: wire::MemberKind::Harness,
            name: name.into(),
            env: Vec::new(),
            files: Vec::new(),
            workdir: None,
            input,
            terminal: false,
            args: Vec::new(),
        };
        let s = Shared {
            state: Mutex::new(State::asking(&[stage("codex", true), stage("other", false)])),
            changed: Condvar::new(),
            to_host: Mutex::new(None),
        };
        let info = |index, name: &str| MemberInfo {
            index,
            kind: wire::MemberKind::Harness,
            name: name.into(),
            runs: true,
        };
        let started = wire::Started {
            members: vec![info(0, "other"), info(1, "codex")],
            published: Vec::new(),
            control: None,
            rate_period: Duration::from_millis(100),
            from: wire::StartedFrom::Booted,
        }
        .encode()
        .unwrap();
        assert_eq!(take(&s, wire::to_client::STARTED, &started), Ok(false));
        let room = |member, bytes| wire::InputRoom { member, bytes }.encode();
        assert_eq!(take(&s, wire::to_client::INPUT_ROOM, &room(1, 5)), Ok(false));
        assert_eq!(lock(&s.state).inputs[&1].room, 5);
        assert!(take(&s, wire::to_client::INPUT_ROOM, &room(0, 5)).is_err());
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
    /// and the pipe's close is the input's end. The host's stdin here is a pipe this test
    /// reads.
    #[test]
    fn a_feed_sends_its_callers_bytes_within_its_room_then_its_end() {
        let (mut from_host, to_host) = io::pipe().unwrap();
        let s = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            to_host: Mutex::new(Some(to_host)),
            lessor: None,
            granted: Vec::new(),
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
    }

    /// The variable under which the leak check runs, in a test process of its own.
    #[cfg(any(target_os = "macos", windows))]
    const ALONE: &str = "SHARDS_API_SPAWN_ALONE";

    /// A program that reads its stdin to its end, saying each line with its argument in it, by
    /// its path: findstr on Windows, grep on macOS.
    #[cfg(any(target_os = "macos", windows))]
    fn reader() -> PathBuf {
        if cfg!(windows) {
            let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
            PathBuf::from(root).join("System32").join("findstr.exe")
        } else {
            PathBuf::from("/usr/bin/grep")
        }
    }

    /// Leaves `write` inheritable: as a descriptor another thread has made and not yet marked
    /// close-on-exec is on macOS, whose pipe, socket, socketpair and accept take no
    /// close-on-exec flag; or a handle another part of this process made inheritable for a
    /// child of its own is on Windows.
    #[cfg(any(target_os = "macos", windows))]
    fn inheritable(write: &io::PipeWriter) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            // SAFETY: fcntl(2) of a descriptor `write` owns, clearing its close-on-exec flag.
            assert_eq!(unsafe { libc::fcntl(write.as_raw_fd(), libc::F_SETFD, 0) }, 0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle as _;
            use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
            // SAFETY: a pipe handle `write` owns, alive for the call.
            let ok = unsafe {
                SetHandleInformation(write.as_raw_handle(), HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT)
            };
            assert_ne!(ok, 0, "{}", io::Error::last_os_error());
        }
    }

    /// Whether every write end of `read`'s pipe is gone, every process's: a read that would
    /// wait finds its end (Unix), or a look at it finds it broken (Windows), at once.
    #[cfg(any(target_os = "macos", windows))]
    fn writers_gone(read: &io::PipeReader) -> bool {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            // SAFETY: fcntl(2) of a descriptor `read` owns, making its reads not wait.
            let set = unsafe { libc::fcntl(read.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
            assert_eq!(set, 0);
            match (&*read).read(&mut [0u8; 1]) {
                Ok(0) => true,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => false,
                other => panic!("a read of a pipe nothing was written to: {other:?}"),
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle as _;
            use windows_sys::Win32::Foundation::{ERROR_BROKEN_PIPE, GetLastError};
            use windows_sys::Win32::System::Pipes::PeekNamedPipe;
            let mut ready = 0u32;
            // SAFETY: a pipe handle `read` owns; only the count is asked for, into `ready`.
            let ok = unsafe {
                PeekNamedPipe(
                    read.as_raw_handle(),
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &raw mut ready,
                    std::ptr::null_mut(),
                )
            };
            // SAFETY: reads this thread's last error, which the failed call above set.
            ok == 0 && unsafe { GetLastError() } == ERROR_BROKEN_PIPE
        }
    }

    /// D231's run host holds its three standard streams alone (D248): do leave a pipe's write
    /// end inheritable in this process, then start a run host and let go of that end. Expect:
    /// the pipe's read end finds every writer gone, the run host holding none of it. It runs
    /// in a test process of its own, so no other test's spawn holds that end for its moment.
    /// Linux makes every descriptor close-on-exec as it makes it (pipe2, SOCK_CLOEXEC,
    /// accept4, O_CLOEXEC), so none waits unmarked there.
    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn a_run_host_holds_no_end_it_was_not_given() {
        if std::env::var_os(ALONE).is_some() {
            let (read, write) = io::pipe().unwrap();
            inheritable(&write);
            let host = start_host(&reader(), &[OsStr::new("api-run")], &[]).unwrap();
            drop(write);
            let gone = writers_gone(&read);
            drop(host.to_host);
            host.process.wait().unwrap();
            assert!(
                gone,
                "the run host holds the write end of a pipe it was never given"
            );
            return;
        }
        let alone = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "image::session::tests::a_run_host_holds_no_end_it_was_not_given",
                "--test-threads=1",
            ])
            .env(ALONE, "1")
            .output()
            .unwrap();
        assert!(
            alone.status.success(),
            "{}{}",
            String::from_utf8_lossy(&alone.stdout),
            String::from_utf8_lossy(&alone.stderr)
        );
    }

    /// Do start a run host named alone, found on `PATH` as exec(3)'s `p` functions find one,
    /// with a variable over this process's environment, write it lines and end its stdin.
    /// Expect: its stdout says the variable and the line it was asked for, its stderr nothing,
    /// and it ends 0. Do name a variable that is not UTF-8. Expect: the spec's refusal, before
    /// anything starts.
    #[test]
    fn a_run_host_takes_its_environment_and_speaks_on_its_streams() {
        let env = [("SHARDS_API_SPAWN_TEST".into(), "said".into())];
        // Each says the variable, then reads its stdin to its end, saying the line asked for.
        let host = if cfg!(windows) {
            let args = [
                "/d",
                "/q",
                "/c",
                "echo",
                "%SHARDS_API_SPAWN_TEST%&",
                "findstr",
                "api-run",
            ];
            let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
            start_host(Path::new("cmd"), &args, &env)
        } else {
            let args = ["-c", "printf '%s\\n' \"$SHARDS_API_SPAWN_TEST\"; grep api-run"];
            let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
            start_host(Path::new("sh"), &args, &env)
        }
        .unwrap();
        let Host {
            process,
            mut to_host,
            mut from_host,
            mut said,
        } = host;
        to_host.write_all(b"other\napi-run\n").unwrap();
        drop(to_host);
        let mut out = String::new();
        from_host.read_to_string(&mut out).unwrap();
        let mut why = String::new();
        said.read_to_string(&mut why).unwrap();
        assert_eq!(process.wait().unwrap(), 0);
        let lines: Vec<&str> = out.lines().map(str::trim_end).collect();
        assert_eq!(lines, ["said", "api-run"], "{out:?}");
        assert_eq!(why, "");
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt as _;
            let bad = [(OsString::from_vec(vec![0xff]), OsString::from("x"))];
            assert!(matches!(
                start_host(Path::new("sh"), &[], &bad),
                Err(ImageRefusal::Spec {
                    what: "the run host's environment",
                    ..
                })
            ));
        }
    }

    /// Do find programs named alone that no directory of `PATH` holds, one of them in the
    /// current directory (the crate's, where cargo runs its tests). Expect: each refused as
    /// not found, none taken from the current directory, where posix_spawn(2) would take it
    /// and CreateProcessW would look before `PATH`. Do find one named with its directory, and
    /// a shell named alone. Expect: the first as it is, the second in a directory of `PATH`.
    #[test]
    fn a_program_named_alone_is_found_on_path_alone() {
        assert!(Path::new("Cargo.toml").is_file());
        for alone in ["Cargo.toml", "shards-api-no-such-program"] {
            let found = on_path(Path::new(alone)).map_err(|e| e.kind());
            assert_eq!(found, Err(io::ErrorKind::NotFound), "{alone}");
        }
        let named = Path::new("target").join("shards");
        assert_eq!(on_path(&named).unwrap(), named);
        let shell = on_path(Path::new(if cfg!(windows) { "cmd" } else { "sh" })).unwrap();
        let path = std::env::var_os("PATH").unwrap();
        assert!(
            std::env::split_paths(&path).any(|dir| shell.parent() == Some(dir.as_path())),
            "{shell:?}"
        );
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

    /// A resize's answer is kept for the ask awaiting it alone, as a room's is (D244); a
    /// resize no host hears is refused, typed, and leaves no ask awaited; a run that has
    /// ended is sized no more.
    #[test]
    fn a_resizes_answer_is_kept_for_its_awaited_ask_alone() {
        let s = shared();
        lock(&s.state).resizes.insert(4, None);
        let resized = |id, result| wire::ResizeAnswer { id, result }.encode().unwrap();
        assert_eq!(take(&s, wire::to_client::RESIZED, &resized(4, Ok(()))), Ok(false));
        assert_eq!(lock(&s.state).resizes.get(&4), Some(&Some(Ok(()))));
        assert_eq!(
            take(&s, wire::to_client::RESIZED, &resized(5, Err("no".into()))),
            Ok(false)
        );
        assert!(
            !lock(&s.state).resizes.contains_key(&5),
            "an answer no ask awaits is dropped"
        );
        assert!(take(&s, wire::to_client::RESIZED, &[0, 0, 0, 4, 9]).is_err());
        let s = shared();
        assert_eq!(
            s.resize(1, 40, 120, BOUND),
            Err(ImageRefusal::Os {
                what: "the run host's resize",
                kind: io::ErrorKind::BrokenPipe
            })
        );
        assert!(lock(&s.state).resizes.is_empty());
        lock(&s.state).end = Some(RunEnd::Stopped);
        assert_eq!(
            s.resize(1, 40, 120, BOUND),
            Err(ImageRefusal::Ended(RunEnd::Stopped))
        );
        assert!(lock(&s.state).resizes.is_empty());
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

    /// A rate's answer is kept for the ask awaiting it alone (D247), as a room's is: one for
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
    /// ended is told none (D247).
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

    const CLAUDE: &str = "anthropic/claude-max";
    const CHATGPT: &str = "openai/chatgpt";
    const VALUE: &str = "sk-ant-lease-test-value";

    /// What a simulated orchestrator's lessor did: each lease it gave, each it was told
    /// released, and the asks it was in.
    #[derive(Default)]
    struct Ledger {
        given: Vec<String>,
        released: Vec<String>,
        entered: usize,
        /// The key each ask came under.
        keys: Vec<Option<[u8; 32]>>,
        /// Its answers held until a test lets them go.
        held: bool,
    }

    /// A simulated orchestrator answering its run's leases (D242): each lease named by its
    /// reference and number, its value `VALUE`, living an hour.
    #[derive(Default)]
    struct Orchestrator {
        ledger: Mutex<Ledger>,
        changed: Condvar,
    }

    impl Orchestrator {
        /// Whether `done` comes to hold of its ledger within [`BOUND`].
        fn until(&self, done: impl Fn(&Ledger) -> bool) -> bool {
            let deadline = Instant::now() + BOUND;
            let mut l = lock(&self.ledger);
            while !done(&l) {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return false;
                }
                l = self.changed.wait_timeout(l, left).unwrap().0;
            }
            true
        }
    }

    impl Lessor for Orchestrator {
        fn lease(&self, reference: &str, key: Option<[u8; 32]>) -> Result<Leased, LeaseRefusal> {
            let mut l = lock(&self.ledger);
            l.entered += 1;
            l.keys.push(key);
            self.changed.notify_all();
            while l.held {
                l = self.changed.wait(l).unwrap();
            }
            let lease = format!("{reference}#{}", l.given.len());
            l.given.push(lease.clone());
            Ok(Leased {
                lease,
                value: wire::Secret::from(VALUE),
                until: Instant::now() + Duration::from_secs(3600),
            })
        }

        fn release(&self, lease: &str) {
            lock(&self.ledger).released.push(lease.into());
            self.changed.notify_all();
        }
    }

    /// A session's state over pipes standing for its host's stdin, read back as `sent`, and
    /// stdout, written as `host`; its reader taking what the test writes as its host does.
    struct Hosted {
        shared: Arc<Shared>,
        sent: io::PipeReader,
        host: io::PipeWriter,
        reader: std::thread::JoinHandle<()>,
    }

    fn hosted(lessor: Option<Arc<Orchestrator>>, granted: &[&str]) -> Hosted {
        let (sent, to_host) = io::pipe().unwrap();
        let (from_host, host) = io::pipe().unwrap();
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            to_host: Mutex::new(Some(to_host)),
            lessor: lessor.map(|l| RunLessor(l)),
            granted: granted.iter().map(|g| (*g).to_string()).collect(),
        });
        let reader = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || read_answers(&shared, from_host))
        };
        Hosted {
            shared,
            sent,
            host,
            reader,
        }
    }

    fn says(host: &mut io::PipeWriter, kind: u8, payload: &[u8]) {
        host.write_all(&wire::frame(kind, payload).unwrap()).unwrap();
    }

    fn ask(host: &mut io::PipeWriter, id: u32, reference: &str) {
        let ask = wire::LeaseAsk {
            id,
            reference: reference.into(),
        };
        says(host, wire::to_client::LEASE, &ask.encode().unwrap());
    }

    /// The next lease's answer the session sent its host.
    fn answered(sent: &mut io::PipeReader) -> wire::LeaseAnswer {
        let (kind, payload) = wire::read_frame(sent).unwrap().unwrap();
        assert_eq!(kind, wire::to_host::LEASED);
        wire::LeaseAnswer::decode(&payload).unwrap()
    }

    /// A run with grants, its leases asked by its host on their first use and given by a
    /// simulated orchestrator, then ended each way a run ends (D242). Do: ask two leases;
    /// end the run as its host says it ended, as the orchestrator stops it, and as its host's
    /// channel closes without saying so. Expect: nothing given before the first ask; each ask
    /// answered with the orchestrator's value and what is left of its hour; the first two
    /// ends releasing every lease as the host says the run released them, before its end; the
    /// third releasing none until the channel closes, then every one; each lease released
    /// once.
    #[test]
    fn a_runs_leases_are_given_as_asked_and_released_at_each_end() {
        for end in ["exited", "stopped", "channel closed"] {
            let orchestrator = Arc::new(Orchestrator::default());
            let mut h = hosted(Some(Arc::clone(&orchestrator)), &[CLAUDE, CHATGPT]);
            assert!(
                lock(&orchestrator.ledger).given.is_empty(),
                "leased before its first use"
            );
            for (id, reference) in [(1, CLAUDE), (2, CHATGPT)] {
                ask(&mut h.host, id, reference);
                let answer = answered(&mut h.sent);
                assert_eq!(answer.id, id);
                let lease = answer.result.unwrap();
                assert_eq!(lease.value, wire::Secret::from(VALUE));
                assert!(Duration::from_nanos(lease.lives.get()) <= Duration::from_secs(3600));
            }
            assert_eq!(lock(&orchestrator.ledger).given.len(), 2, "{end}");
            let all_released = |l: &Ledger| l.released.len() == 2;
            match end {
                "exited" => {
                    says(&mut h.host, wire::to_client::RELEASED, &[]);
                    assert!(
                        orchestrator.until(all_released),
                        "{end}: not released as its host said"
                    );
                    says(
                        &mut h.host,
                        wire::to_client::ENDED,
                        &RunEnd::Exited(0).encode().unwrap(),
                    );
                }
                "stopped" => {
                    h.shared.send(wire::to_host::STOP, &[]).unwrap();
                    let (kind, _) = wire::read_frame(&mut h.sent).unwrap().unwrap();
                    assert_eq!(kind, wire::to_host::STOP);
                    says(&mut h.host, wire::to_client::RELEASED, &[]);
                    assert!(
                        orchestrator.until(all_released),
                        "{end}: not released as its host said"
                    );
                    says(
                        &mut h.host,
                        wire::to_client::ENDED,
                        &RunEnd::Stopped.encode().unwrap(),
                    );
                }
                _ => assert!(lock(&orchestrator.ledger).released.is_empty()),
            }
            drop(h.host);
            h.reader.join().unwrap();
            let l = lock(&orchestrator.ledger);
            let (mut given, mut released) = (l.given.clone(), l.released.clone());
            given.sort();
            released.sort();
            assert_eq!(released, given, "{end}: each lease released once");
        }
    }

    /// A run's leases are asked under its own key where its start told one (D252's endpoint,
    /// D238's key, D242's leases), and under none where it told none. Do: start a run whose
    /// host tells an endpoint and its key, and one whose host tells none, and ask a lease of
    /// each. Expect: the lessor asked under that key, then under none.
    #[test]
    fn a_lease_is_asked_under_the_runs_own_key_where_it_has_one() {
        for key in [Some([7u8; 32]), None] {
            let orchestrator = Arc::new(Orchestrator::default());
            let mut h = hosted(Some(Arc::clone(&orchestrator)), &[CLAUDE]);
            let started = wire::Started {
                members: Vec::new(),
                published: Vec::new(),
                control: key.map(|vm| wire::ControlAt {
                    address: "127.0.0.1:4433".parse().unwrap(),
                    vm,
                }),
                rate_period: Duration::from_millis(100),
                from: wire::StartedFrom::Booted,
            };
            says(&mut h.host, wire::to_client::STARTED, &started.encode().unwrap());
            ask(&mut h.host, 1, CLAUDE);
            assert!(answered(&mut h.sent).result.is_ok());
            assert_eq!(lock(&orchestrator.ledger).keys, [key]);
            drop(h.host);
            h.reader.join().unwrap();
        }
    }

    /// A lease given after the run released its leases is released at once (D242). Do: hold
    /// the orchestrator's answer to an ask; have the host say the run released its leases;
    /// then let the answer go, and ask again. Expect: the answer told the host as the run's
    /// end, its lease released, and the next ask refused at once as ended, its lessor never
    /// asked.
    #[test]
    fn a_lease_given_after_the_runs_release_is_released_as_it_comes() {
        let orchestrator = Arc::new(Orchestrator::default());
        lock(&orchestrator.ledger).held = true;
        let mut h = hosted(Some(Arc::clone(&orchestrator)), &[CLAUDE]);
        ask(&mut h.host, 1, CLAUDE);
        assert!(orchestrator.until(|l| l.entered == 1));
        says(&mut h.host, wire::to_client::RELEASED, &[]);
        assert_eq!(
            h.shared.wait(BOUND, |s| s.leases.released.then_some(())),
            Some(())
        );
        lock(&orchestrator.ledger).held = false;
        orchestrator.changed.notify_all();
        assert_eq!(answered(&mut h.sent).result, Err(LeaseRefusal::Ended));
        assert!(orchestrator.until(|l| l.released == l.given && l.given.len() == 1));
        ask(&mut h.host, 2, CLAUDE);
        assert_eq!(answered(&mut h.sent).result, Err(LeaseRefusal::Ended));
        assert_eq!(lock(&orchestrator.ledger).entered, 1);
        drop(h.host);
        h.reader.join().unwrap();
    }

    /// Do: have the host ask a lease of a reference no grant of the run names, one of a run
    /// with no lessor, and one by an id it holds a lease by. Expect: each breaks the
    /// channel's contract, its run failed, and nothing leased.
    #[test]
    fn a_lease_the_run_was_not_granted_breaks_the_channel() {
        for (lessor, twice) in [(true, false), (false, false), (true, true)] {
            let orchestrator = Arc::new(Orchestrator::default());
            let mut h = hosted(lessor.then(|| Arc::clone(&orchestrator)), &[CLAUDE]);
            if twice {
                ask(&mut h.host, 1, CLAUDE);
                assert!(answered(&mut h.sent).result.is_ok());
                ask(&mut h.host, 1, CLAUDE);
            } else {
                ask(&mut h.host, 1, if lessor { "github/token" } else { CLAUDE });
            }
            // Its channel then closes: a run its ask did not break ends with no end of its own.
            drop(h.host);
            h.reader.join().unwrap();
            assert!(
                matches!(lock(&h.shared.state).end, Some(RunEnd::Failed(_))),
                "{lessor} {twice}"
            );
            let l = lock(&orchestrator.ledger);
            assert_eq!(l.given.len(), usize::from(twice));
            assert_eq!(l.released, l.given, "a lease outlived its channel");
        }
    }
}
