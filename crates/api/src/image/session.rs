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
    /// How a warm host's microVM started, once it is ready for its run (`WARMED`, D253's
    /// pools).
    warmed: Option<wire::StartedFrom>,
    /// The members whose stdin the request asks for, by kind and name: each one's flow is
    /// made as the run's start is taken ([`take`]), by its index there.
    asked_inputs: Vec<(wire::MemberKind, String)>,
    refused: Option<ImageRefusal>,
    /// Its refusal was of a run of another microVM's than its warm one (`Cause::Unfit`).
    unfit: bool,
    flows: HashMap<(u32, Channel), Flow>,
    /// Each member's stdin its spec asked for, by its index.
    inputs: HashMap<u32, InputFlow>,
    ends: HashMap<u32, shards_abi::run::members::End>,
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
    /// Each save asked and still awaited, by its ask's id, and its answer once it came (D259),
    /// held as [`rooms`](Self::rooms) is.
    saves: HashMap<u32, Option<Saved>>,
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
    /// The lessor's name for it, and the credential it is of.
    Given {
        lease: String,
        reference: String,
    },
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

/// A save's outcome as its host said it (D259): saved, or refused and why.
type Saved = Result<(), (wire::SaveRefusal, String)>;

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

    /// The run's end, once it has come: its host's word, or its host gone without one, said
    /// in its last diagnostics; kept as first seen, so that every later look, a stop's among
    /// them, sees the same.
    fn end_seen(&mut self) -> Option<RunEnd> {
        if self.end.is_none() && self.gone() {
            self.end = Some(RunEnd::Failed(format!(
                "the run host exited without its run's end{}",
                quoted(&self.said)
            )));
        }
        self.end.clone()
    }

    /// The run's end once its stop has been waited for ([`Session::stop`]): the end seen, or
    /// `Stopped` where none has come.
    fn stopped(&mut self) -> RunEnd {
        self.end_seen()
            .unwrap_or_else(|| self.end.insert(RunEnd::Stopped).clone())
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
    /// What leases the run's credentials (D242), where its spec gave one: set as its run's
    /// request is sent (`Begun::create`), a warm host's (D253's pools) only once its run comes.
    lessor: Mutex<Option<RunLessor>>,
    /// The references the run's grants name: no other is leased.
    granted: Mutex<Vec<String>>,
    /// The run's sign-in grants (D242, step 8), whose captures and refreshes its host may ask,
    /// each with what its last capture gave: its access token's reference is leased as a
    /// grant's is.
    sign_ins: Mutex<Vec<wire::SignInGrant>>,
    /// Where the host's diagnostics are written whole, once its caller has named a file
    /// ([`ImageGuest::keep_diagnostics`]).
    said_to: Mutex<Option<std::fs::File>>,
}

impl Shared {
    /// The host's diagnostics written whole to `to` from now on, after what is kept of them
    /// until now, under the lock their keeper writes each part under ([`keep_said`]): so that
    /// each part is in what is kept now or written after, and once.
    fn keep_diagnostics(&self, mut to: std::fs::File) -> io::Result<()> {
        let mut said_to = lock(&self.said_to);
        let kept: Vec<u8> = lock(&self.state).said.iter().copied().collect();
        to.write_all(&kept)?;
        *said_to = Some(to);
        Ok(())
    }

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

    /// Asks the run's microVM saved whole at `to` (D259) and waits at most `patience` for its
    /// host's answer; its ask awaited from before it is sent until it is answered or given up,
    /// as [`room`](Self::room)'s is.
    fn save(&self, to: &str, patience: Duration) -> Result<(), ImageRefusal> {
        let id = {
            let mut state = lock(&self.state);
            if let Some(end) = &state.end {
                return Err(ImageRefusal::Ended(end.clone()));
            }
            state.next_ask = state.next_ask.wrapping_add(1);
            let id = state.next_ask;
            state.saves.insert(id, None);
            id
        };
        let answer = wire::SaveAsk { id, to: to.into() }
            .encode()
            .map_err(|e| ImageRefusal::Spec {
                what: "a save",
                why: e.to_string(),
            })
            .and_then(|ask| {
                self.send(wire::to_host::SAVE, &ask)
                    .map_err(|e| ImageRefusal::Os {
                        what: "the run host's save",
                        kind: e.kind(),
                    })
            })
            .and_then(|()| {
                self.wait(patience, |s| {
                    if let Some(answer) = s.saves.get_mut(&id).and_then(Option::take) {
                        return Some(answer.map_err(|(cause, why)| ImageRefusal::Save(cause, why)));
                    }
                    if let Some(end) = &s.end {
                        return Some(Err(ImageRefusal::Ended(end.clone())));
                    }
                    s.gone()
                        .then(|| Err(ImageRefusal::Host("it exited without answering".into())))
                })
                .unwrap_or_else(|| {
                    Err(ImageRefusal::Unsupported {
                        what: "a save",
                        why: format!("no answer within {patience:?}"),
                    })
                })
            });
        lock(&self.state).saves.remove(&id);
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
    // On Unix it leads a session and a process group of its own, which `kill` ends whole; a
    // terminal's signals to this process's group no longer reach it, and its run ends with
    // this process all the same, its stdin closing. On Windows it is made in a job of its own,
    // nested under this process's, which holds every process it starts and which `kill` ends
    // whole: `detach` would take it out of this process's job, so it is not asked.
    #[cfg(unix)]
    let process = shards_ipc::spawn_with(&found, args, &ends, true, &set).map_err(boot)?;
    #[cfg(windows)]
    let process = shards_ipc::spawn_grouped(&found, args, &ends, &set).map_err(boot)?;
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

/// Kills the host at once, and every process it started, unless it is reaped: on Unix its
/// process group, which it leads (`start_host`), and which its VM, network and share
/// processes are in (api_run.rs starts each in it); on Windows its group's job, which they
/// are made in, nested in the host's own (D141, D197), each of them held to be followed to
/// its end (`shards_ipc::Child::end_group`, D231).
pub(super) fn kill(host: &shards_ipc::Child) {
    let _ = host.kill_group(shards_ipc::signal::SIGKILL);
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

/// A run host started, its threads reading its answers, keeping its last diagnostics and
/// waiting for its exit: a session's before its run has started.
pub(super) struct Begun {
    shared: Arc<Shared>,
    host: Arc<shards_ipc::Child>,
    /// Whether it ends the host as it drops: until its run's session holds the host.
    armed: bool,
}

impl Begun {
    /// Starts `binary` (`spec.binary`'s, else `SHARDS_BIN`, else `shards` on `PATH`) as the
    /// run host (`api-run`), its environment this process's with `env` over it.
    pub(super) fn begin(binary: Option<&Path>, env: &[(OsString, OsString)]) -> Result<Begun, ImageRefusal> {
        let binary = binary
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os("SHARDS_BIN").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("shards"));
        let os = |what: &'static str| move |e: io::Error| ImageRefusal::Os { what, kind: e.kind() };
        let Host {
            process,
            to_host,
            from_host,
            said,
        } = start_host(&binary, &[OsStr::new("api-run")], env)?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            to_host: Mutex::new(Some(to_host)),
            lessor: Mutex::new(None),
            granted: Mutex::new(Vec::new()),
            sign_ins: Mutex::new(Vec::new()),
            said_to: Mutex::new(None),
        });
        // Shared: the session and its exit watcher, which outlives this call.
        let host = Arc::new(process);
        let begun = Begun {
            shared,
            host,
            armed: true,
        };
        let layers = host_layers(env);
        let threads = (|| {
            let (watcher, watched) = (begun.shared.clone(), begun.host.clone());
            std::thread::Builder::new()
                .name("shards-api-image-exit".into())
                .spawn(move || watch(&watcher, &watched, layers.as_deref()))
                .map_err(os("the run host's watcher"))?;
            let reader = begun.shared.clone();
            std::thread::Builder::new()
                .name("shards-api-image".into())
                .spawn(move || read_answers(&reader, from_host))
                .map_err(os("the run host's reader"))?;
            let tail = begun.shared.clone();
            std::thread::Builder::new()
                .name("shards-api-image-said".into())
                .spawn(move || keep_said(&tail, said))
                .map_err(os("the run host's diagnostics"))
                .map(drop)
        })();
        if let Err(e) = threads {
            begun.end();
            return Err(e);
        }
        Ok(begun)
    }

    /// The host's process, for its pool to end it while it starts warm (D253's pools).
    pub(super) fn process(&self) -> Arc<shards_ipc::Child> {
        self.host.clone()
    }

    /// Ends the host at once, and its channel.
    fn end(&self) {
        kill(&self.host);
        lock(&self.shared.to_host).take();
    }

    /// Sends the host the run `request` (`CREATE`), its leases' `lessor` (D242) and the
    /// references its grants name set first, and its members whose stdin it asks for named,
    /// for their flows to be made with its start (`take`): none of them can be asked of or
    /// answered sooner than the request.
    pub(super) fn create(
        &self,
        lessor: Option<RunLessor>,
        request: &wire::Request,
    ) -> Result<(), ImageRefusal> {
        *lock(&self.shared.lessor) = lessor;
        *lock(&self.shared.granted) = request.credentials.iter().map(|g| g.reference.clone()).collect();
        *lock(&self.shared.sign_ins) = request.sign_ins.clone();
        lock(&self.shared.state).asked_inputs = State::asking(&request.members).asked_inputs;
        self.send(wire::to_host::CREATE, request)
    }

    /// Sends the host `request`, as a run's (`create`) or a warm host's exemplar (`WARM`).
    pub(super) fn send(&self, kind: u8, request: &wire::Request) -> Result<(), ImageRefusal> {
        let payload = request.encode().map_err(|e| ImageRefusal::Spec {
            what: "the spec",
            why: e.to_string(),
        })?;
        self.shared.send(kind, &payload).map_err(|e| ImageRefusal::Os {
            what: "the run host's request",
            kind: e.kind(),
        })
    }

    /// Waits for a warm host's microVM to be ready (`WARMED`, D253's pools): how it started;
    /// else why not, the host ended.
    pub(super) fn warmed(&self) -> Result<wire::StartedFrom, ImageRefusal> {
        let outcome = self.shared.wait(Duration::MAX, |s| {
            if let Some(from) = s.warmed {
                return Some(Ok(from));
            }
            if s.refused.is_some() {
                if !(s.gone() && s.said_all) {
                    return None;
                }
                return s.refused.take().map(|r| Err(with_said(r, &s.said)));
            }
            s.gone().then(|| {
                Err(ImageRefusal::Host(format!(
                    "it exited before it was warm{}",
                    quoted(&s.said)
                )))
            })
        });
        let warmed = outcome.unwrap_or_else(|| Err(ImageRefusal::Host("no word of its warmth".into())));
        if warmed.is_err() {
            self.end();
        }
        warmed
    }

    /// Waits at most `patience` for the start of the run `request` it was sent: the run's
    /// session; else why not, the host ended, and whether it refused the run as one of
    /// another microVM's than its warm one (`Cause::Unfit`, D253's pools). `stop` holds the
    /// host meanwhile: stopped, it kills the host and all the host started, and the run is
    /// `Ended(Stopped)` once they are gone.
    pub(super) fn started(
        mut self,
        request: &wire::Request,
        patience: Duration,
        stop: &super::BootStop,
    ) -> Result<Session, (ImageRefusal, bool)> {
        // The host is the session's from here, ended by it, or here where its run does not
        // start.
        self.armed = false;
        let (shared, host) = (self.shared.clone(), self.host.clone());
        drop(self);
        let end_it = |shared: &Shared| {
            kill(&host);
            lock(&shared.to_host).take();
        };
        let unfit = |shared: &Shared| lock(&shared.state).unfit;
        if let Err(e) = stop.hold(&host) {
            end_it(&shared);
            return Err((e, false));
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
        if stop.release() {
            end_it(&shared);
            // On Windows every process of the host's group followed to its end, its handle
            // signalled: a process closes its handles, its diagnostics' among them, as it exits,
            // before its handle is signalled, so their end below says nothing of it (D231).
            #[cfg(windows)]
            if let Err(e) = host.end_group() {
                return Err((
                    ImageRefusal::Os {
                        what: "the stopped run's processes",
                        kind: e.kind(),
                    },
                    false,
                ));
            }
            // Followed to its end, never timed (D215, D217): the host exited and reaped by its
            // watcher, its answers ended, and its diagnostics, which the VM, network and share
            // processes it started hold as their stderr, closed by every one of them.
            shared.wait(Duration::MAX, |s| (s.gone() && s.said_all).then_some(()));
            return Err((ImageRefusal::Ended(RunEnd::Stopped), false));
        }
        let started = match outcome {
            Some(Ok(started)) => started,
            Some(Err(e)) => {
                end_it(&shared);
                return Err((e, unfit(&shared)));
            }
            None => {
                end_it(&shared);
                let mut state = lock(&shared.state);
                // Refused, by a host that has not yet exited: its refusal still.
                if let Some(r) = state.refused.take() {
                    return Err((with_said(r, &state.said), state.unfit));
                }
                let said = quoted(&state.said);
                return Err((
                    ImageRefusal::Boot(format!("the run did not start within {patience:?}{said}")),
                    false,
                ));
            }
        };
        // Its endpoint told exactly where one was asked for (D252): a host that binds none for
        // a run that asked, or one for a run that did not, broke the channel's contract.
        if request.control.is_some() != started.control.is_some() {
            end_it(&shared);
            return Err((
                ImageRefusal::Host(
                    "the run host's start told an agent-control endpoint other than the run asked".into(),
                ),
                false,
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
        session.pump_outputs().map_err(|e| (e, false))?;
        Ok(session)
    }
}

/// The run hosts the `shards` binary starts, for [`super::ImagePools`] (D253's pools): warm
/// ones with a run's exemplar (`WARM`), handed their run as it comes (`CREATE`), and one for a
/// run at once.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct ShardsHosts;

impl super::pool::Hosts for ShardsHosts {
    type Starting = Begun;
    type Warm = Begun;
    type Kill = Arc<shards_ipc::Child>;

    fn begin(
        &self,
        host: &super::pool::HostOf,
        exemplar: &wire::Request,
    ) -> Result<(Begun, Arc<shards_ipc::Child>), ImageRefusal> {
        let begun = Begun::begin(host.binary.as_deref(), &host.env)?;
        if let Err(e) = begun.send(wire::to_host::WARM, exemplar) {
            begun.end();
            return Err(e);
        }
        let process = begun.process();
        Ok((begun, process))
    }

    fn warmed(&self, starting: Begun) -> Result<Begun, ImageRefusal> {
        starting.warmed().map(|_| starting)
    }

    fn kill(&self, kill: &Arc<shards_ipc::Child>) {
        self::kill(kill);
    }

    fn take(
        &self,
        warm: Begun,
        spec: &ImageSpec,
        request: &wire::Request,
        patience: Duration,
        stop: &super::BootStop,
    ) -> Result<Box<dyn ImageGuest>, super::pool::Taken> {
        use super::pool::Taken;
        if let Err(e) = warm.create(spec.lessor.clone(), request) {
            warm.end();
            return Err(Taken::Refused(e));
        }
        match warm.started(request, patience, stop) {
            Ok(session) => Ok(Box::new(session)),
            Err((_, true)) => Err(Taken::Unfit),
            Err((refused, false)) => Err(Taken::Refused(refused)),
        }
    }

    fn fresh(
        &self,
        spec: &ImageSpec,
        request: &wire::Request,
        patience: Duration,
        stop: &super::BootStop,
    ) -> Result<Box<dyn ImageGuest>, ImageRefusal> {
        Ok(Box::new(Session::start(spec, request, patience, stop)?))
    }
}

impl Drop for Begun {
    /// A host whose run never started, a warm one its pool keeps no more among them, ends
    /// with it: a session holds its own host once started (`started`), which ends it with
    /// itself.
    fn drop(&mut self) {
        if self.armed {
            self.end();
        }
    }
}

impl Session {
    pub(super) fn start(
        spec: &ImageSpec,
        request: &wire::Request,
        patience: Duration,
        stop: &super::BootStop,
    ) -> Result<Session, ImageRefusal> {
        let begun = Begun::begin(spec.binary.as_deref(), &spec.env)?;
        begun.create(spec.lessor.clone(), request)?;
        begun
            .started(request, patience, stop)
            .map_err(|(refused, _)| refused)
    }

    /// Its running members' outputs ([`pumped`]).
    fn pump_outputs(&self) -> Result<(), ImageRefusal> {
        let pumped = pumped(&self.shared, &self.members)?;
        lock(&self.outputs).extend(pumped);
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

/// A member's channel, by its index, and the pipe its bytes come out of ([`pumped`]).
type Pumped = ((u32, Channel), io::PipeReader);

/// A pipe for each of `members`' running members' channels, and the thread that fills it as
/// its bytes come, the host granted room for its first [`AHEAD`] bytes and for as many more as
/// each part took ([`pump`]). A credit that cannot go is no refusal: the host's channel is
/// closed, as the run's end closes it (`read_answers`), or broken, and the run started all the
/// same, its end its session's to tell (`State::end_seen`), as `pump` takes its own credits'
/// (WHP at 3a05bc3f1: a template's run of `exit 3`, restored, ended before its first credit
/// went, and its boot was refused `BrokenPipe`).
fn pumped(shared: &Arc<Shared>, members: &[MemberInfo]) -> Result<Vec<Pumped>, ImageRefusal> {
    let mut outputs = Vec::new();
    for m in members.iter().filter(|m| m.runs) {
        for channel in [Channel::Stdout, Channel::Stderr] {
            let (reader, writer) = io::pipe().map_err(|e| ImageRefusal::Os {
                what: "a member's pipe",
                kind: e.kind(),
            })?;
            outputs.push(((m.index, channel), reader));
            let pumping = shared.clone();
            let member = m.index;
            std::thread::Builder::new()
                .name("shards-api-member".into())
                .spawn(move || pump(&pumping, member, channel, writer))
                .map_err(|e| ImageRefusal::Os {
                    what: "a member's pump",
                    kind: e.kind(),
                })?;
            let _ = shared.credit(member, channel, AHEAD);
        }
    }
    Ok(outputs)
}

/// Moves member `member`'s `channel` into `writer` as its bytes come, and grants the host
/// room for each part once written; ends once every part the session took is written and
/// no more can come, closing the pipe: at the channel's end, or at the end of its host's
/// answers. The run's end is neither (control-01): its host may still say output after it,
/// as its capture's reader sends what its VM's end left beside the end it says. A credit
/// that cannot go, the host's channel closed by the run's end or by `stop`, or broken,
/// ends the credits alone: what was taken still goes. A caller that let go of its end drops
/// the rest, as a closed pipe drops `docker run`'s output, still granting room, so that the
/// member is never held by a reader that left.
fn pump(shared: &Shared, member: u32, channel: Channel, mut writer: io::PipeWriter) {
    // Each a latch, set once: the owner's pipe taking writes, and the host's channel credits.
    let mut open = true;
    let mut granting = true;
    loop {
        let next = shared.wait(Duration::MAX, |s| {
            let flow = s.flows.entry((member, channel)).or_default();
            if let Some(part) = flow.queued.pop_front() {
                return Some(Some(part));
            }
            (flow.ended.is_some() || s.answered).then_some(None)
        });
        let Some(Some(part)) = next else {
            return;
        };
        if open && writer.write_all(&part).is_err() {
            open = false;
        }
        let taken = u64::try_from(part.len()).unwrap_or(u64::MAX);
        if granting && shared.credit(member, channel, taken).is_err() {
            granting = false;
        }
    }
}

/// Moves what the caller writes to member `member`'s stdin to its host, no more at once than
/// the room the host gave, and its end once the caller closes its pipe; ends with the input,
/// the run or the host, dropping its pipe's read end so that the caller's writes fail then.
fn feed(shared: &Shared, member: u32, mut reader: io::PipeReader) {
    // What the caller types into the member, a credential as often as not (D242): wiped here
    // once sent, and the buffer as it goes.
    let mut buf = zeroize::Zeroizing::new(vec![0u8; AHEAD as usize]);
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
        if send_input(shared, member, buf.get_mut(..n).unwrap_or_default()).is_err() {
            return;
        }
    }
}

/// Sends `typed`, bytes for member `member`'s stdin, as one `INPUT` frame in its parts, never
/// copied whole into one (`send_parts`), then wipes them where they were read; the frame's
/// own copy is wiped as it drops (D242).
fn send_input(shared: &Shared, member: u32, typed: &mut [u8]) -> io::Result<()> {
    let mut payload = zeroize::Zeroizing::new(Vec::with_capacity(4 + typed.len()));
    payload.extend_from_slice(&wire::Input::head(member));
    payload.extend_from_slice(typed);
    zeroize::Zeroize::zeroize(typed);
    shared.send_parts(wire::to_host::INPUT, &payload)
}

/// Reads the host's answers until it closes its stdout, each into the state; one that
/// breaks the channel's contract ends the session as a failed run. The run's end closes
/// the host's stdin: the host then ends. Every lease the run was given is released as its
/// host says the run released them, or as its answers end where it never says so (D242).
fn read_answers(shared: &Arc<Shared>, mut from_host: io::PipeReader) {
    let failed = loop {
        let (kind, mut payload) = match wire::read_frame(&mut from_host) {
            Ok(Some(frame)) => frame,
            Ok(None) => break None,
            Err(e) => break Some(format!("its answers: {e}")),
        };
        let taken = match kind {
            wire::to_client::LEASE => lease_asked(shared, &payload).map(|()| false),
            // A capture's tokens are the lessor's once taken (D242, step 8): the frame's copy is
            // wiped as it goes.
            wire::to_client::CAPTURE => {
                let taken = capture_asked(shared, &payload).map(|()| false);
                zeroize::Zeroize::zeroize(&mut payload);
                taken
            }
            wire::to_client::REFRESH => refresh_asked(shared, &payload).map(|()| false),
            wire::to_client::REVOKE => revoke_told(shared, &payload).map(|()| false),
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
        k::WARMED => {
            let from = wire::read_warmed(payload).map_err(bad)?;
            if state.warmed.replace(from).is_some() {
                return Err("a host warm twice".into());
            }
        }
        k::REFUSED => {
            let r = wire::Refused::decode(payload).map_err(bad)?;
            state.unfit = r.cause == wire::Cause::Unfit;
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
                wire::Cause::Unfit => ImageRefusal::Host(format!(
                    "a warm run host of another microVM's than the run's: {}",
                    r.why
                )),
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
            if state.ends.insert(e.member, e.end).is_some() {
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
        k::SAVED => {
            let answer = wire::SaveAnswer::decode(payload).map_err(bad)?;
            if let Some(awaited) = state.saves.get_mut(&answer.id) {
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
    let Some(lessor) = lock(&shared.lessor).clone() else {
        return Err("a lease asked of a run with no lessor".into());
    };
    if !granted(shared, &ask.reference) {
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
            let result = settle(&asking, &*lessor.0, &ask, got, Instant::now());
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
    ask: &wire::LeaseAsk,
    got: Result<Leased, LeaseRefusal>,
    now: Instant,
) -> Result<wire::Lease, LeaseRefusal> {
    let id = ask.id;
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
            let given = Holding::Given {
                lease: leased.lease.clone(),
                reference: ask.reference.clone(),
            };
            state.leases.each.insert(id, given);
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

/// The run's lessor, for an ask of sign-in `sign_in` its host made (D242, step 8): one of a run
/// with no lessor, or of a sign-in no grant of the run names, breaks the channel's contract.
fn sign_in_lessor(shared: &Shared, sign_in: &str) -> Result<RunLessor, String> {
    let Some(lessor) = lock(&shared.lessor).clone() else {
        return Err("a sign-in asked of a run with no lessor".into());
    };
    if !lock(&shared.sign_ins).iter().any(|s| s.sign_in == sign_in) {
        return Err(format!(
            "an ask of sign-in {sign_in}, which no grant of the run names"
        ));
    }
    Ok(lessor)
}

/// Whether credential `reference` is one the run may lease: one its grants name, or the access
/// token a capture of one of its sign-ins gave (D242, step 8).
fn granted(shared: &Shared, reference: &str) -> bool {
    lock(&shared.granted).iter().any(|g| g == reference)
        || lock(&shared.sign_ins)
            .iter()
            .filter_map(|s| s.captured.as_ref())
            .any(|c| c.access == reference)
}

/// The ids of the leases given of sign-in `sign_in`'s access token now.
fn given_of(shared: &Shared, sign_in: &str) -> Vec<u32> {
    let access = lock(&shared.sign_ins)
        .iter()
        .find(|s| s.sign_in == sign_in)
        .and_then(|s| s.captured.as_ref())
        .map(|c| c.access.clone());
    let Some(access) = access else {
        return Vec::new();
    };
    lock(&shared.state)
        .leases
        .each
        .iter()
        .filter(|(_, held)| matches!(held, Holding::Given { reference, .. } if *reference == access))
        .map(|(id, _)| *id)
        .collect()
}

/// Leases `ids`, given before a refresh rotated their value (D242 step 8, Q2), let go of, so
/// that the next ask of it is leased anew, the placeholders its CLI holds the same: each
/// released to the run's lessor once, as the run's end would, where the run holds it still.
fn rotated(shared: &Shared, lessor: &dyn Lessor, ids: &[u32]) {
    let stale: Vec<String> = {
        let mut state = lock(&shared.state);
        ids.iter()
            .filter_map(|id| match state.leases.each.remove(id) {
                Some(Holding::Given { lease, .. }) => Some(lease),
                Some(other) => {
                    state.leases.each.insert(*id, other);
                    None
                }
                None => None,
            })
            .collect()
    };
    for lease in stale {
        lessor.release(&lease);
    }
}

/// A sign-in's capture its host asks (D242, step 8), put to the run's lessor on a thread of its
/// own, so that no answer of the host's waits on it; the access token's reference it gives is
/// one the run's leases may name from then on. Refused as ended once the run has released its
/// leases.
fn capture_asked(shared: &Arc<Shared>, payload: &[u8]) -> Result<(), String> {
    let ask = wire::CaptureAsk::decode(payload).map_err(|e| e.to_string())?;
    let lessor = sign_in_lessor(shared, &ask.sign_in)?;
    let id = ask.id;
    let asking = Arc::clone(shared);
    let spawned = std::thread::Builder::new()
        .name("shards-api-capture".into())
        .spawn(move || {
            let wire::CaptureAsk { sign_in, tokens, .. } = ask;
            let (released, key) = {
                let state = lock(&asking.state);
                (state.leases.released, state.leases.key)
            };
            let result = if released {
                Err(wire::SignInRefusal::Ended)
            } else {
                lessor.0.capture(&sign_in, tokens, key)
            };
            // Its access token is the run's to lease from now, in place of an earlier capture's.
            if let Ok(c) = &result
                && let Some(grant) = lock(&asking.sign_ins).iter_mut().find(|s| s.sign_in == sign_in)
            {
                grant.captured = Some(c.clone());
            }
            answer_capture(&asking, id, result);
        });
    if spawned.is_err() {
        let why = "no thread to ask its lessor on".to_string();
        answer_capture(shared, id, Err(wire::SignInRefusal::Unavailable(why)));
    }
    Ok(())
}

/// A sign-in's refresh its host asks (D242, step 8), put to the run's lessor on a thread of its
/// own; refused as ended once the run has released its leases.
fn refresh_asked(shared: &Arc<Shared>, payload: &[u8]) -> Result<(), String> {
    let ask = wire::RefreshAsk::decode(payload).map_err(|e| e.to_string())?;
    let lessor = sign_in_lessor(shared, &ask.sign_in)?;
    let id = ask.id;
    let asking = Arc::clone(shared);
    let spawned = std::thread::Builder::new()
        .name("shards-api-refresh".into())
        .spawn(move || {
            let (released, key) = {
                let state = lock(&asking.state);
                (state.leases.released, state.leases.key)
            };
            // The leases of its access token given before the refresh, whose value it rotates.
            let before = given_of(&asking, &ask.sign_in);
            let result = if released {
                Err(wire::SignInRefusal::Ended)
            } else {
                lessor.0.refresh(&ask.sign_in, key)
            };
            // Its access token rotated: those leases let go of before its host hears, so that
            // the next ask is leased the rotated value (Q2).
            if result.is_ok() {
                rotated(&asking, &*lessor.0, &before);
            }
            answer_refresh(&asking, id, result);
        });
    if spawned.is_err() {
        let why = "no thread to ask its lessor on".to_string();
        answer_refresh(shared, id, Err(wire::SignInRefusal::Unavailable(why)));
    }
    Ok(())
}

/// A sign-in its run's VM revoked, told by its host (D242, step 8): told the run's lessor on a
/// thread of its own; answered nowhere, as its network process answered the VM.
fn revoke_told(shared: &Arc<Shared>, payload: &[u8]) -> Result<(), String> {
    let told = wire::Revoked::decode(payload).map_err(|e| e.to_string())?;
    let lessor = sign_in_lessor(shared, &told.sign_in)?;
    let key = lock(&shared.state).leases.key;
    // With no thread to tell it on, the lessor is not told: nothing waits on it.
    let _ = std::thread::Builder::new()
        .name("shards-api-revoked".into())
        .spawn(move || lessor.0.revoked(&told.sign_in, key));
    Ok(())
}

/// Tells the run's host capture `id`'s answer (`CAPTURED`); one past the channel's bound is told
/// unavailable.
fn answer_capture(shared: &Shared, id: u32, result: Result<wire::Captured, wire::SignInRefusal>) {
    let payload = wire::CaptureAnswer { id, result }.encode().or_else(|e| {
        wire::CaptureAnswer {
            id,
            result: Err(wire::SignInRefusal::Unavailable(format!("its answer: {e}"))),
        }
        .encode()
    });
    // A host gone is told nothing: its run has ended.
    if let Ok(payload) = payload {
        let _ = shared.send_parts(wire::to_host::CAPTURED, &payload);
    }
}

/// Tells the run's host refresh `id`'s answer (`REFRESHED`), every copy of its token response
/// wiped once sent; one past the channel's bound is told unavailable.
fn answer_refresh(shared: &Shared, id: u32, result: Result<wire::Secret, wire::SignInRefusal>) {
    let answer = wire::RefreshAnswer { id, result };
    let encoded = answer.encode().map(zeroize::Zeroizing::new);
    drop(answer);
    let payload = encoded.or_else(|e| {
        wire::RefreshAnswer {
            id,
            result: Err(wire::SignInRefusal::Unavailable(format!("its answer: {e}"))),
        }
        .encode()
        .map(zeroize::Zeroizing::new)
    });
    if let Ok(payload) = payload {
        let _ = shared.send_parts(wire::to_host::REFRESHED, &payload);
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
                Holding::Given { lease, .. } => Some(lease),
                Holding::Asked => None,
            })
            .collect()
    };
    let lessor = lock(&shared.lessor).clone();
    if let Some(lessor) = lessor {
        for lease in given {
            lessor.0.release(&lease);
        }
    }
}

/// Keeps the host's last diagnostics until they end, and writes each part whole to the file
/// its caller named for them, where it has named one ([`Shared::keep_diagnostics`]): kept and
/// written under that file's lock, which naming a file takes first.
fn keep_said(shared: &Shared, mut said: io::PipeReader) {
    let keep = said_kept();
    let mut buf = [0u8; 4096];
    while let Ok(n) = said.read(&mut buf) {
        if n == 0 {
            break;
        }
        let part = buf.get(..n).unwrap_or_default();
        let mut said_to = lock(&shared.said_to);
        {
            let mut state = lock(&shared.state);
            state.said.extend(part);
            let over = state.said.len().saturating_sub(keep);
            state.said.drain(..over);
        }
        // A part its file refuses is lost to it alone: the tail keeps it.
        if let Some(to) = said_to.as_mut() {
            let _ = to.write_all(part);
        }
    }
    lock(&shared.state).said_all = true;
    shared.changed.notify_all();
}

/// Waits for the host to exit, then reaps it under the state's lock as `exited` is set.
fn watch(shared: &Shared, host: &shards_ipc::Child, layers: Option<&Path>) {
    let awaited = host.ended();
    if let (Ok(()), Some(layers)) = (&awaited, layers) {
        clear_layers(layers, host.id());
    }
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

/// Where the run host started with `env` over this process's environment makes its
/// microVMs' writable layers' files (D190, shards' layers.rs `fresh`): its home's
/// [`LAYERS`](shards_ipc::LAYERS), its home found as it finds it ([`shards_ipc::home_in`]),
/// a variable `env` names twice its last, as the host is given it. None where it finds no
/// home, as it then makes none.
fn host_layers(env: &[(OsString, OsString)]) -> Option<PathBuf> {
    let named = |n: &OsString, name: &str| {
        // Windows' variables are named without regard to case.
        n.to_str()
            .is_some_and(|n| n == name || (cfg!(windows) && n.eq_ignore_ascii_case(name)))
    };
    let var = |name: &str| {
        env.iter()
            .rev()
            .find(|(n, _)| named(n, name))
            .map(|(_, v)| v.clone())
            .or_else(|| std::env::var_os(name))
    };
    shards_ipc::home_in(var)
        .ok()
        .map(|home| home.join(shards_ipc::LAYERS))
}

/// Removes from `layers` every file run host `pid` made ([`shards_ipc::layer_maker`]): the
/// layers of its microVMs it made and had not let go of as it ended (D253), its microVM not
/// yet holding one, killed mid-boot or failed. Called once it has ended and before it is
/// reaped: ended, it makes no more, and only it makes files of its pid's; unreaped (on
/// Windows, its handle held), its pid is no other process's, so no other run host's file is
/// taken. A microVM of its still ending that has one open is removed from as one its host
/// let go of as it started is (layers.rs `LayerFile`'s drop): the name goes, and the file
/// with the microVM. What cannot be removed stays, as what that drop could not remove does,
/// for the home's next daemon to clear (layers.rs `clear`).
fn clear_layers(layers: &Path, pid: u32) {
    for entry in std::fs::read_dir(layers).into_iter().flatten().flatten() {
        let name = entry.file_name();
        if name.to_str().and_then(shards_ipc::layer_maker) == Some(pid) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
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
            if let Some(end) = s.ends.get(&index) {
                return Some(match *end {
                    shards_abi::run::members::End::Exited(code) => MemberEnd::Exited(code),
                    shards_abi::run::members::End::Signaled(signal) => MemberEnd::Signaled(signal),
                });
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

    fn save(&self, to: &Path, patience: Duration) -> Result<(), ImageRefusal> {
        let to = to.to_str().ok_or_else(|| ImageRefusal::Spec {
            what: "a save's directory",
            why: format!("{}: not UTF-8", to.display()),
        })?;
        self.shared.save(to, patience)
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
        let ended = lock(&self.shared.state).end_seen().is_some();
        if !ended && self.shared.send(wire::to_host::STOP, &[]).is_err() {
            kill(&self.host);
        }
        // The run's end comes as its VM goes, which the host kills at once; or the host is
        // gone. The host's own exit after (its cleanup) is the watcher's to reap.
        self.shared.wait(Duration::MAX, |s| s.end_seen().map(|_| ()));
        lock(&self.shared.to_host).take();
        lock(&self.shared.state).stopped();
        self.shared.changed.notify_all();
        Ok(())
    }

    fn wait_for(&self, timeout: Duration) -> Result<Option<RunEnd>, ImageRefusal> {
        Ok(self.shared.wait(timeout, State::end_seen))
    }

    fn keep_diagnostics(&self, to: std::fs::File) -> Result<(), ImageRefusal> {
        self.shared.keep_diagnostics(to).map_err(|e| ImageRefusal::Os {
            what: "the run host's diagnostics",
            kind: e.kind(),
        })
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
            lessor: Mutex::new(None),
            granted: Mutex::new(Vec::new()),
            sign_ins: Mutex::new(Vec::new()),
            said_to: Mutex::new(None),
        }
    }

    /// A member's typed stdin goes to its host as one `INPUT` frame and is wiped where it was
    /// read (D242). Do: send typed bytes from a buffer of the test's. Expect: the buffer all
    /// zeros after; the host reads one `INPUT` frame of the member's bytes, and nothing more.
    #[test]
    fn a_members_typed_input_is_wiped_where_it_was_read_once_sent() {
        let (mut host, to_host) = io::pipe().unwrap();
        let s = shared();
        *lock(&s.to_host) = Some(to_host);
        let mut typed = *b"sk-ant-typed-in-1234";
        send_input(&s, 4, &mut typed).unwrap();
        assert!(
            typed.iter().all(|&b| b == 0),
            "the typed bytes were left where they were read"
        );
        drop(lock(&s.to_host).take());
        let mut frame = Vec::new();
        let (kind, len) = wire::read_frame_into(&mut host, &mut frame).unwrap().unwrap();
        assert_eq!(kind, wire::to_host::INPUT);
        let input = wire::Input::decode(&frame[..len]).unwrap();
        assert_eq!(
            (input.member, input.bytes.0.as_slice()),
            (4, b"sk-ant-typed-in-1234".as_slice())
        );
        assert!(wire::read_frame_into(&mut host, &mut frame).unwrap().is_none());
    }

    /// Do have a run host write more diagnostics than are kept to quote, its caller naming a
    /// file for them partway, then end them. Expect: the file holding what was kept until it
    /// was named and everything after, whole and once; what is kept to quote still their last
    /// alone (Hex: the cause of a grants-none run had gone from the tail).
    #[test]
    fn a_hosts_diagnostics_are_written_whole_to_the_file_its_caller_names() {
        let shared = shared();
        let (said, mut host) = io::pipe().unwrap();
        let dir = shards_testdir::TempDir::new("api-said").unwrap();
        let path = dir.join("diagnostics");
        let before = b"shards api-run: before its file\n".to_vec();
        let after: Vec<u8> = (0..said_kept() * 2).map(|i| b'a' + (i % 26) as u8).collect();
        std::thread::scope(|s| {
            let keeping = s.spawn(|| keep_said(&shared, said));
            host.write_all(&before).unwrap();
            // What the host wrote is kept before its file is named.
            let deadline = Instant::now() + Duration::from_secs(10);
            while lock(&shared.state).said.len() < before.len() {
                assert!(Instant::now() < deadline, "its first words were never kept");
                std::thread::sleep(Duration::from_millis(1));
            }
            shared
                .keep_diagnostics(std::fs::File::create(&path).unwrap())
                .unwrap();
            host.write_all(&after).unwrap();
            drop(host);
            keeping.join().unwrap();
        });
        assert_eq!(std::fs::read(&path).unwrap(), [&before[..], &after[..]].concat());
        let kept: Vec<u8> = lock(&shared.state).said.iter().copied().collect();
        assert_eq!(kept, after[after.len() - said_kept()..]);
    }

    /// Do pump the outputs of a run whose end came before them, its host's channel closed by
    /// it, as a run that ends at once has (WHP at 3a05bc3f1: a template's run of `exit 3`,
    /// restored, ended before its first credit went), its host's answers over. Expect: a pipe
    /// for each channel of each running member alone, each ending as its host's answers have,
    /// and no refusal: the run started, and its end is the session's to tell, never a boot
    /// refused `BrokenPipe`.
    #[test]
    fn a_run_ended_before_its_first_credit_still_has_its_outputs() {
        let shared = Arc::new(shared());
        {
            let mut state = lock(&shared.state);
            state.end = Some(RunEnd::Exited(3));
            state.answered = true;
        }
        let member = |index, runs| MemberInfo {
            index,
            kind: wire::MemberKind::Harness,
            name: format!("m{index}"),
            runs,
        };
        let outputs = pumped(&shared, &[member(1, true), member(2, false)]).unwrap();
        let channels: Vec<(u32, Channel)> = outputs.iter().map(|(key, _)| *key).collect();
        assert_eq!(channels, [(1, Channel::Stdout), (1, Channel::Stderr)]);
        for (key, mut reader) in outputs {
            let mut rest = Vec::new();
            reader.read_to_end(&mut rest).unwrap();
            assert!(rest.is_empty(), "{key:?}");
        }
        assert_eq!(lock(&shared.state).end_seen(), Some(RunEnd::Exited(3)));
    }

    /// Do see a run's host gone without its run's end, as `wait_for` sees it, then stop the
    /// run, as `stop` does once its wait is over; and stop one whose host said its end. Expect:
    /// the end first seen, in the host's last words, kept through the stop and after, never
    /// `Stopped` (Hex: `wait_for` read the host's failure, then `stop` turned it `Stopped`);
    /// the host's own end kept likewise.
    #[test]
    fn a_runs_end_once_seen_is_kept_through_its_stop() {
        let mut state = State {
            exited: true,
            answered: true,
            said: b"shards api-run: its VM's link: reset\n"
                .iter()
                .copied()
                .collect(),
            ..State::default()
        };
        let seen = RunEnd::Failed(
            "the run host exited without its run's end: shards api-run: its VM's link: reset".into(),
        );
        assert_eq!(state.end_seen(), Some(seen.clone()));
        assert_eq!(state.stopped(), seen);
        assert_eq!(state.end_seen(), Some(seen));
        let mut said = State {
            end: Some(RunEnd::Exited(3)),
            exited: true,
            answered: true,
            ..State::default()
        };
        assert_eq!(said.stopped(), RunEnd::Exited(3));
        assert_eq!(said.end_seen(), Some(RunEnd::Exited(3)));
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
            mcp: false,
        };
        let s = Shared {
            state: Mutex::new(State::asking(&[stage("codex", true), stage("other", false)])),
            changed: Condvar::new(),
            to_host: Mutex::new(None),
            lessor: Mutex::new(None),
            granted: Mutex::new(Vec::new()),
            sign_ins: Mutex::new(Vec::new()),
            said_to: Mutex::new(None),
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
            lessor: Mutex::new(None),
            granted: Mutex::new(Vec::new()),
            sign_ins: Mutex::new(Vec::new()),
            said_to: Mutex::new(None),
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
        assert_eq!((first.member, first.bytes.0.as_slice()), (1, &payload[..7]));
        assert_eq!(take(&s, wire::to_client::INPUT_ROOM, &room(1024)), Ok(false));
        let mut got = first.bytes.0.clone();
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
            got.extend_from_slice(&wire::Input::decode(&frame).unwrap().bytes.0);
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

    /// A save's answer is kept for the ask awaiting it alone, as a room's is (D259): one for an
    /// ask no longer awaited is dropped; one that is no answer breaks the channel. A save no
    /// host hears is refused, typed, and leaves no ask awaited; a run that has ended is saved
    /// no more.
    #[test]
    fn a_saves_answer_is_kept_for_its_awaited_ask_alone() {
        let s = shared();
        lock(&s.state).saves.insert(4, None);
        let saved = |id, result| wire::SaveAnswer { id, result }.encode().unwrap();
        assert_eq!(take(&s, wire::to_client::SAVED, &saved(4, Ok(()))), Ok(false));
        assert_eq!(lock(&s.state).saves.get(&4), Some(&Some(Ok(()))));
        let refused = Err((wire::SaveRefusal::State, "a save of it is under way".to_string()));
        assert_eq!(take(&s, wire::to_client::SAVED, &saved(5, refused)), Ok(false));
        assert!(
            !lock(&s.state).saves.contains_key(&5),
            "an answer no ask awaits is dropped"
        );
        assert!(take(&s, wire::to_client::SAVED, &[0, 0, 0, 4, 1, 9]).is_err());
        let s = shared();
        assert_eq!(
            s.save("/home/u/.local/share/shards/saves/s1", BOUND),
            Err(ImageRefusal::Os {
                what: "the run host's save",
                kind: io::ErrorKind::BrokenPipe
            })
        );
        assert!(lock(&s.state).saves.is_empty());
        lock(&s.state).end = Some(RunEnd::Stopped);
        assert_eq!(
            s.save("/home/u/.local/share/shards/saves/s1", BOUND),
            Err(ImageRefusal::Ended(RunEnd::Stopped))
        );
        assert!(lock(&s.state).saves.is_empty());
    }

    /// A save is asked of the run's host on the run's own channel, its directory whole, and
    /// its answer is the host's, typed (D259): a refusal comes back as `ImageRefusal::Save` by
    /// what refused it, a save made as done, each to its own ask, and no ask is left awaited.
    #[test]
    fn a_save_rides_the_runs_channel_and_comes_back_typed() {
        let mut h = hosted(None, &[]);
        let to = "/home/u/.local/share/shards/saves/s1";
        for result in [
            Err((
                wire::SaveRefusal::Destination,
                "outside the run's home".to_string(),
            )),
            Ok(()),
        ] {
            let shared = Arc::clone(&h.shared);
            let asking = std::thread::spawn(move || shared.save(to, BOUND));
            let (kind, payload) = wire::read_frame(&mut h.sent).unwrap().unwrap();
            assert_eq!(kind, wire::to_host::SAVE);
            let ask = wire::SaveAsk::decode(&payload).unwrap();
            assert_eq!(ask.to, to);
            let answer = wire::SaveAnswer {
                id: ask.id,
                result: result.clone(),
            };
            says(&mut h.host, wire::to_client::SAVED, &answer.encode().unwrap());
            assert_eq!(
                asking.join().unwrap(),
                result.map_err(|(cause, why)| ImageRefusal::Save(cause, why))
            );
        }
        assert!(lock(&h.shared.state).saves.is_empty());
        drop(h.host);
        h.reader.join().unwrap();
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
            lessor: Mutex::new(lessor.map(|l| RunLessor(l))),
            granted: Mutex::new(granted.iter().map(|g| (*g).to_string()).collect()),
            sign_ins: Mutex::new(Vec::new()),
            said_to: Mutex::new(None),
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

    /// A running member's stdout, as `pumped` gives the run's owner its pipe.
    fn member_stdout(shared: &Arc<Shared>) -> io::PipeReader {
        let running = MemberInfo {
            index: 1,
            kind: wire::MemberKind::Harness,
            name: "m1".into(),
            runs: true,
        };
        let mut outputs = pumped(shared, &[running]).unwrap();
        let at = outputs
            .iter()
            .position(|(key, _)| *key == (1, Channel::Stdout))
            .unwrap();
        outputs.swap_remove(at).1
    }

    fn says_output(host: &mut io::PipeWriter, bytes: &[u8]) {
        let payload = [&wire::Output::head(1, Channel::Stdout)[..], bytes].concat();
        says(host, wire::to_client::OUTPUT, &payload);
    }

    /// Waits for the session's reader to take the run's end, which closes the host's stdin.
    fn until_its_channel_closes(shared: &Shared) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while lock(&shared.to_host).is_some() {
            assert!(Instant::now() < deadline, "the run's end never closed its host's channel");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// A member's output the session took before the run's end reaches its owner whole
    /// (control-01, docs/bugs/2026-11-13/control.md): its credits can no longer go, as the
    /// run's end closed the host's channel, which is no reason to drop what was taken. Do:
    /// have the host say two records of a member's stdout, its channel's end and the run's
    /// end, and once the session has taken them all, start the member's pump. Expect: the
    /// owner reads both records, once each, then its end, while the host's answers are still
    /// open: the channel's own end ends it. Before, the pump wrote the first record, found
    /// its credit refused `BrokenPipe` and ended the pipe, the second record left queued.
    #[test]
    fn output_taken_before_the_runs_end_reaches_its_owner_whole() {
        let mut h = hosted(None, &[]);
        says_output(&mut h.host, b"first record\n");
        says_output(&mut h.host, b"second record\n");
        let end = wire::OutputEnd {
            member: 1,
            channel: Channel::Stdout,
            end: wire::StreamEnd::Closed,
        };
        says(&mut h.host, wire::to_client::OUTPUT_END, &end.encode());
        says(&mut h.host, wire::to_client::ENDED, &RunEnd::Exited(0).encode().unwrap());
        until_its_channel_closes(&h.shared);
        let mut read = Vec::new();
        member_stdout(&h.shared).read_to_end(&mut read).unwrap();
        assert_eq!(read, b"first record\nsecond record\n");
        assert!(!lock(&h.shared.state).answered, "ended by its host's answers, not its channel's end");
        drop(h.host);
        h.reader.join().unwrap();
    }

    /// A member's output its host says after the run's end still reaches its owner, as the
    /// run host says the end as its VM goes while its capture's reader may still be sending
    /// (api_run.rs `serve`'s `following` beside `records`). Do: have the host say the run's
    /// end; start the member's pump; then have the host say a record of its stdout and close
    /// its answers without the channel's end, as a capture cut by its VM's end does. Expect:
    /// the owner reads the record, then its end, which comes with the host's answers' end.
    /// Before, the pump took the run's end for its channel's and ended the pipe empty.
    #[test]
    fn output_said_after_the_runs_end_reaches_its_owner_before_its_end() {
        let mut h = hosted(None, &[]);
        says(&mut h.host, wire::to_client::ENDED, &RunEnd::Exited(0).encode().unwrap());
        until_its_channel_closes(&h.shared);
        let mut owner = member_stdout(&h.shared);
        says_output(&mut h.host, b"the last of it\n");
        drop(h.host);
        let mut read = Vec::new();
        owner.read_to_end(&mut read).unwrap();
        assert_eq!(read, b"the last of it\n");
        h.reader.join().unwrap();
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

    const SIGN_IN: &str = "claude/acct-1";
    const SIGNED_ACCESS: &str = "sign-ins/claude/acct-1/access";

    /// What a simulated broker's capture gives (D242, step 8): one reference, and the same
    /// placeholders every time.
    fn kept() -> wire::Captured {
        wire::Captured {
            access: SIGNED_ACCESS.into(),
            access_placeholder: "kiln-placeholder-access-0123456789abcdef".into(),
            refresh_placeholder: Some("kiln-placeholder-refresh-fedcba9876543210".into()),
        }
    }

    /// A simulated orchestrator keeping its run's sign-in (D242, step 8): its leases as
    /// [`Orchestrator`]'s, its value whatever the reference; each capture's tokens kept; each
    /// refresh answered with the rotated tokens' response, the provider called once for each;
    /// each revocation told.
    #[derive(Default)]
    struct Broker {
        leases: Orchestrator,
        kept: Mutex<Vec<wire::SignInTokens>>,
        refreshed: Mutex<usize>,
        revoked: Mutex<Vec<String>>,
    }

    impl Lessor for Broker {
        fn lease(&self, reference: &str, key: Option<[u8; 32]>) -> Result<Leased, LeaseRefusal> {
            self.leases.lease(reference, key)
        }

        fn release(&self, lease: &str) {
            self.leases.release(lease);
        }

        fn capture(
            &self,
            sign_in: &str,
            tokens: wire::SignInTokens,
            _key: Option<[u8; 32]>,
        ) -> Result<wire::Captured, wire::SignInRefusal> {
            if sign_in != SIGN_IN {
                return Err(wire::SignInRefusal::Denied(format!("no sign-in {sign_in}")));
            }
            lock(&self.kept).push(tokens);
            Ok(kept())
        }

        fn refresh(
            &self,
            sign_in: &str,
            _key: Option<[u8; 32]>,
        ) -> Result<wire::Secret, wire::SignInRefusal> {
            if sign_in != SIGN_IN {
                return Err(wire::SignInRefusal::Denied(format!("no sign-in {sign_in}")));
            }
            *lock(&self.refreshed) += 1;
            Ok(wire::Secret::from(
                r#"{"access_token":"sk-ant-oat01-rotated","refresh_token":"sk-ant-ort01-rotated","expires_in":28800}"#,
            ))
        }

        fn revoked(&self, sign_in: &str, _key: Option<[u8; 32]>) {
            lock(&self.revoked).push(sign_in.into());
            self.leases.changed.notify_all();
        }
    }

    /// A session of a run that grants sign-in `SIGN_IN` (not yet captured), its lessor `broker`.
    fn signing(broker: &Arc<Broker>) -> Hosted {
        let (sent, to_host) = io::pipe().unwrap();
        let (from_host, host) = io::pipe().unwrap();
        let lessor: Arc<dyn Lessor> = Arc::clone(broker) as Arc<dyn Lessor>;
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            to_host: Mutex::new(Some(to_host)),
            lessor: Mutex::new(Some(RunLessor(lessor))),
            granted: Mutex::new(Vec::new()),
            sign_ins: Mutex::new(vec![wire::SignInGrant {
                sign_in: SIGN_IN.into(),
                cli: wire::Cli::ClaudeCode,
                toward: vec!["api.anthropic.com".into()],
                captured: None,
            }]),
            said_to: Mutex::new(None),
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

    /// A run's sign-in kept by its orchestrator (D242, step 8). Do: have the host ask a capture
    /// of its tokens; lease the access token the capture gave; refresh the sign-in; lease it
    /// again; tell a revocation in the VM; say the run released its leases. Expect: the tokens
    /// kept by the broker, whole, and the capture answered with its placeholders; the access
    /// token leasable only once captured, by the broker's reference; the refresh answered with
    /// the broker's response, its provider called once, the lease given before it released and
    /// the sign-in's placeholders the same; the revocation told the broker, answered nowhere;
    /// and the lease given after the refresh released at the run's end, each lease once.
    #[test]
    fn a_sign_ins_capture_and_refresh_are_kept_by_the_broker_and_its_lease_released_at_the_runs_end() {
        let broker = Arc::new(Broker::default());
        let mut h = signing(&broker);
        assert!(!granted(&h.shared, SIGNED_ACCESS), "leasable before its capture");
        let capture = wire::CaptureAsk {
            id: 1,
            sign_in: SIGN_IN.into(),
            tokens: wire::SignInTokens {
                access: wire::Secret::from("sk-ant-oat01-real-access"),
                refresh: Some(wire::Secret::from("sk-ant-ort01-real-refresh")),
                expires_in: Some(28800),
                id_token: None,
                scope: Some("user:inference".into()),
            },
        };
        says(&mut h.host, wire::to_client::CAPTURE, &capture.encode().unwrap());
        let (kind, payload) = wire::read_frame(&mut h.sent).unwrap().unwrap();
        assert_eq!(kind, wire::to_host::CAPTURED);
        let answer = wire::CaptureAnswer::decode(&payload).unwrap();
        assert_eq!((answer.id, answer.result), (1, Ok(kept())));
        assert_eq!(*lock(&broker.kept), std::slice::from_ref(&capture.tokens));
        assert!(granted(&h.shared, SIGNED_ACCESS));
        ask(&mut h.host, 7, SIGNED_ACCESS);
        assert!(answered(&mut h.sent).result.is_ok());
        let first = lock(&broker.leases.ledger).given.clone();
        assert_eq!(first, [format!("{SIGNED_ACCESS}#0")]);
        // The refresh: the broker's alone, its answer relayed; the lease given before let go of.
        let refresh = wire::RefreshAsk {
            id: 2,
            sign_in: SIGN_IN.into(),
        };
        says(&mut h.host, wire::to_client::REFRESH, &refresh.encode().unwrap());
        let (kind, payload) = wire::read_frame(&mut h.sent).unwrap().unwrap();
        assert_eq!(kind, wire::to_host::REFRESHED);
        let answer = wire::RefreshAnswer::decode(&payload).unwrap();
        assert_eq!(answer.id, 2);
        assert!(answer.result.is_ok());
        assert_eq!(*lock(&broker.refreshed), 1);
        assert_eq!(
            lock(&broker.leases.ledger).released,
            first,
            "the rotated token's lease held past its refresh"
        );
        assert_eq!(
            lock(&h.shared.sign_ins)[0].captured,
            Some(kept()),
            "a refresh changed the sign-in's placeholders"
        );
        ask(&mut h.host, 8, SIGNED_ACCESS);
        assert!(answered(&mut h.sent).result.is_ok());
        // The VM's revocation: told the broker, answered nowhere.
        let revoked = wire::Revoked {
            sign_in: SIGN_IN.into(),
        };
        says(&mut h.host, wire::to_client::REVOKE, &revoked.encode().unwrap());
        assert!(broker.leases.until(|_| !lock(&broker.revoked).is_empty()));
        // The run's end: the lease given after the refresh released, each once.
        says(&mut h.host, wire::to_client::RELEASED, &[]);
        assert!(
            broker.leases.until(|l| l.released.len() == 2),
            "the captured token's lease not released at the run's end"
        );
        says(
            &mut h.host,
            wire::to_client::ENDED,
            &RunEnd::Exited(0).encode().unwrap(),
        );
        drop(h.host);
        h.reader.join().unwrap();
        let l = lock(&broker.leases.ledger);
        assert_eq!(l.released, l.given, "each lease released once");
        assert_eq!(*lock(&broker.revoked), [SIGN_IN]);
    }

    /// How long a stand-in's boot may take before it is refused, so that a stop that is not
    /// heard fails its test rather than holding it: the test's bound alone.
    const BOOT_BOUND: Duration = Duration::from_secs(10);

    /// An image spec for run host `binary`, its environment `env`, with nothing to run.
    fn stand_in_spec(binary: &Path, env: Vec<(OsString, OsString)>) -> ImageSpec {
        ImageSpec {
            image: "stand-in:local".into(),
            vcpus: 1,
            memory_mib: 256,
            command: None,
            members: Vec::new(),
            publish: Vec::new(),
            egress: wire::Narrowing::default(),
            credentials: Vec::new(),
            lessor: None,
            sign_ins: Vec::new(),
            upstream_roots: Vec::new(),
            shares: Vec::new(),
            nesting: false,
            ready_by: None,
            binary: Some(binary.to_path_buf()),
            env,
            control: None,
            gpu_devices: Vec::new(),
        }
    }

    /// `script` written to `dir` as an executable stand-in for `shards api-run`.
    #[cfg(unix)]
    fn stand_in(dir: &Path, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join("host");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// A FIFO at `path`.
    #[cfg(unix)]
    fn fifo(path: &Path) {
        use std::os::unix::ffi::OsStrExt as _;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: mkfifo(3) of a NUL-terminated path this function holds.
        let made = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
        assert_eq!(made, 0, "{}", io::Error::last_os_error());
    }

    /// SIGKILLs `pid` as it drops, unless let go of: a stand-in VM a failed stop left running,
    /// never one already seen gone (whose pid may be another's by then).
    #[cfg(unix)]
    struct Leftover(Option<libc::pid_t>);

    #[cfg(unix)]
    impl Drop for Leftover {
        fn drop(&mut self) {
            if let Some(pid) = self.0 {
                // SAFETY: kill(2) of the stand-in VM, seen still running.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
    }

    /// The stand-in run host of a boot that never starts: it makes a writable layer's file in
    /// its home's `layers`, named as the real host names one (layers.rs `fresh`), holds
    /// `$HOST_SEEN` open as long as it runs, starts a stand-in VM that never looks at its host
    /// (as a VM mid-boot does not) and holds `$VM_SEEN` open as long as it runs, tells the
    /// VM's pid, and never answers.
    #[cfg(unix)]
    const NEVER_STARTS: &str = "#!/bin/sh\n\
        : >\"$SHARDS_HOME/layers/$$-0\"\n\
        exec 3>\"$HOST_SEEN\"\n\
        tail -f /dev/null >\"$VM_SEEN\" 3>&- &\n\
        echo \"$!\" >&3\n\
        wait\n";

    /// A home whose `layers` holds a file of a run host other than any these tests start
    /// (this process's own pid's): the one a sweep of a host's layers leaves.
    fn layered_home(dir: &Path) -> (PathBuf, PathBuf) {
        let home = dir.join("home");
        std::fs::create_dir_all(home.join("layers")).unwrap();
        let others = home.join("layers").join(format!("{}-0", std::process::id()));
        std::fs::write(&others, b"").unwrap();
        (home, others)
    }

    /// The files `home`'s `layers` holds, by name.
    fn layer_files(home: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(home.join("layers"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Do boot through a run host that makes a writable layer's file, starts a VM of its own
    /// and never says the run started, and stop the boot once both run. Expect: the boot
    /// returns `Ended(Stopped)`, well within its bound, the run host gone and the VM it
    /// started gone with it, no orphan left, and no file of the host's left in its home's
    /// `layers`, another host's kept.
    #[cfg(unix)]
    #[test]
    fn a_stop_mid_boot_ends_the_boot_its_host_and_all_its_host_started() {
        use std::io::BufRead as _;
        let dir = shards_testdir::TempDir::new("bstop").unwrap();
        let (host_seen, vm_seen) = (dir.path().join("h"), dir.path().join("v"));
        fifo(&host_seen);
        fifo(&vm_seen);
        let (home, others) = layered_home(dir.path());
        let host = stand_in(dir.path(), NEVER_STARTS);
        let spec = stand_in_spec(
            &host,
            vec![
                ("HOST_SEEN".into(), host_seen.clone().into()),
                ("VM_SEEN".into(), vm_seen.clone().into()),
                ("SHARDS_HOME".into(), home.clone().into()),
            ],
        );
        let request = super::super::request(&spec).unwrap();
        let stop = super::super::BootStop::new();
        let mut leftover = Leftover(None);
        std::thread::scope(|s| {
            let booting = s.spawn(|| Session::start(&spec, &request, BOOT_BOUND, &stop).map(drop));
            // Each open waits for its writer: the host, then its VM, both running once open.
            let host_end = std::fs::File::open(&host_seen).unwrap();
            let vm_end = std::fs::File::open(&vm_seen).unwrap();
            let mut said = String::new();
            io::BufReader::new(&host_end).read_line(&mut said).unwrap();
            leftover.0 = Some(said.trim().parse().unwrap());
            let stopped_at = Instant::now();
            stop.stop();
            let booted = booting.join().unwrap();
            let took = stopped_at.elapsed();
            let ends = |f: std::fs::File| io::PipeReader::from(std::os::fd::OwnedFd::from(f));
            let (host_gone, vm_gone) = (writers_gone(&ends(host_end)), writers_gone(&ends(vm_end)));
            if vm_gone {
                leftover.0 = None;
            }
            assert_eq!(
                booted,
                Err(ImageRefusal::Ended(RunEnd::Stopped)),
                "after {took:?}"
            );
            assert!(took < BOOT_BOUND, "the stop took {took:?}");
            assert!(host_gone, "the run host outlived its boot's stop");
            assert!(vm_gone, "the VM its run host started outlived the boot's stop");
        });
        assert!(stop.is_stopped());
        let others = others.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(layer_files(&home), [others], "its run host's layer outlived it");
    }

    /// Do boot through a run host that makes a writable layer's file and exits before its run
    /// starts, as one that fails or is killed does. Expect: the boot refused, its host gone,
    /// and no file of the host's left in its home's `layers`, another host's kept.
    #[cfg(unix)]
    #[test]
    fn a_run_host_that_ends_before_its_run_starts_leaves_no_layer() {
        let dir = shards_testdir::TempDir::new("bgone").unwrap();
        let (home, others) = layered_home(dir.path());
        let host = stand_in(
            dir.path(),
            "#!/bin/sh\n: >\"$SHARDS_HOME/layers/$$-0\"\n: >\"$SHARDS_HOME/layers/$$-1\"\nexit 3\n",
        );
        let spec = stand_in_spec(&host, vec![("SHARDS_HOME".into(), home.clone().into())]);
        let request = super::super::request(&spec).unwrap();
        let booted = Session::start(&spec, &request, BOOT_BOUND, &super::super::BootStop::new()).map(drop);
        assert!(
            matches!(&booted, Err(ImageRefusal::Host(why)) if why.contains("exited before its run started")),
            "{booted:?}"
        );
        let others = others.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(layer_files(&home), [others], "its run host's layers outlived it");
    }

    /// Do boot through a run host that never answers, with a stop never stopped and no
    /// patience. Expect: the boot refused as one with no stop is, not started within it. Do
    /// the same with a stop already stopped and patience to spare. Expect: `Ended(Stopped)`
    /// well within it.
    #[cfg(unix)]
    #[test]
    fn a_boot_not_stopped_answers_as_before_and_one_stopped_first_ends_stopped() {
        let dir = shards_testdir::TempDir::new("bstop2").unwrap();
        let host = stand_in(dir.path(), "#!/bin/sh\nexec tail -f /dev/null\n");
        let spec = stand_in_spec(&host, Vec::new());
        let request = super::super::request(&spec).unwrap();
        let stop = super::super::BootStop::new();
        let refused = Session::start(&spec, &request, Duration::ZERO, &stop).map(drop);
        assert!(
            matches!(&refused, Err(ImageRefusal::Boot(why)) if why.contains("did not start within")),
            "{refused:?}"
        );
        assert!(!stop.is_stopped());
        stop.stop();
        let asked = Instant::now();
        let stopped = Session::start(&spec, &request, BOOT_BOUND, &stop).map(drop);
        let took = asked.elapsed();
        assert_eq!(
            stopped,
            Err(ImageRefusal::Ended(RunEnd::Stopped)),
            "after {took:?}"
        );
        assert!(took < BOOT_BOUND, "the stop took {took:?}");
    }

    /// What this test binary is, started with this variable: the stand-in run host of a
    /// boot that never starts (`host`), or the VM that host starts (`vm`).
    #[cfg(windows)]
    const STAND_IN: &str = "SHARDS_API_STAND_IN";

    /// The named pipe a stand-in run host tells its own pid and its VM's on.
    #[cfg(windows)]
    const STAND_IN_TELLS: &str = "SHARDS_API_STAND_IN_TELLS";

    /// Run by the C runtime as this test binary starts, before its harness: the MSVC CRT
    /// calls each function pointer of the `.CRT$XCU` section as it initializes ("CRT
    /// Initialization", Microsoft Learn), as the `ctor` crate does. A run host is started as
    /// `BINARY api-run`, which the harness would read as a filter matching no test: so a
    /// stand-in is chosen here, by its variable, and never returns.
    #[cfg(windows)]
    #[used]
    #[unsafe(link_section = ".CRT$XCU")]
    static STAND_IN_START: extern "C" fn() = stand_in;

    #[cfg(windows)]
    extern "C" fn stand_in() {
        match std::env::var_os(STAND_IN).as_deref().and_then(OsStr::to_str) {
            Some("host") => stand_in_host(),
            Some("vm") => stand_by(),
            _ => {}
        }
    }

    /// Waits until this process is ended: on itself, which signals only as it ends.
    #[cfg(windows)]
    fn stand_by() -> ! {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, INFINITE, WaitForSingleObject};
        loop {
            // SAFETY: this process's pseudo-handle, which needs no closing.
            unsafe { WaitForSingleObject(GetCurrentProcess(), INFINITE) };
        }
    }

    /// The stand-in run host of a boot that never starts, as `shards api-run` is on Windows
    /// (api_run.rs `launch`): a writable layer's file in its home's `layers`, named as the
    /// real host names one (layers.rs `fresh`), a job ending with this process, and a
    /// stand-in VM made in it holding this process's stderr, as a VM, network and share
    /// process do; it tells its own pid and its VM's on `STAND_IN_TELLS`, reads its
    /// orchestrator's asks to their end, and never answers.
    #[cfg(windows)]
    fn stand_in_host() -> ! {
        use std::os::windows::io::AsHandle as _;
        let home = PathBuf::from(std::env::var_os("SHARDS_HOME").unwrap());
        std::fs::write(home.join("layers").join(format!("{}-0", std::process::id())), b"").unwrap();
        let job = shards_ipc::GroupJob::ending_with_this_process().unwrap();
        let me = std::env::current_exe().unwrap();
        let stderr = io::stderr();
        let vm = shards_ipc::spawn_detached(
            Some(&job),
            &me,
            &[],
            &[(stderr.as_handle(), 2)],
            &[(STAND_IN, OsStr::new("vm"))],
        )
        .unwrap();
        let tells = std::env::var_os(STAND_IN_TELLS).unwrap();
        let mut told = std::fs::OpenOptions::new().write(true).open(tells).unwrap();
        writeln!(told, "{} {}", std::process::id(), vm.id()).unwrap();
        drop(told);
        let _ = io::copy(&mut io::stdin().lock(), &mut io::sink());
        // Both held for as long as this process runs: its job's last handle, which ends the
        // VM as this process ends.
        let _held = (job, vm);
        stand_by()
    }

    /// A process's handle, by its pid, that waits on its end; terminated as it drops unless
    /// seen ended: a stand-in a failed stop left running.
    #[cfg(windows)]
    struct Held(std::os::windows::io::OwnedHandle, bool);

    #[cfg(windows)]
    impl Held {
        fn open(pid: u32) -> Held {
            use std::os::windows::io::FromRawHandle as _;
            use windows_sys::Win32::System::Threading::{
                OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
            };
            // SAFETY: a process handle of the rights asked, owned from here on.
            let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, 0, pid) };
            assert!(!raw.is_null(), "process {pid}: {}", io::Error::last_os_error());
            // SAFETY: the handle just opened, which nothing else owns.
            Held(
                unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(raw) },
                false,
            )
        }

        /// Whether it has ended, asked once: its handle signalled.
        fn ended(&mut self) -> bool {
            use std::os::windows::io::AsRawHandle as _;
            use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
            use windows_sys::Win32::System::Threading::WaitForSingleObject;
            // SAFETY: a process handle this holds, not waited on past this moment.
            self.1 = unsafe { WaitForSingleObject(self.0.as_raw_handle(), 0) } == WAIT_OBJECT_0;
            self.1
        }
    }

    #[cfg(windows)]
    impl Drop for Held {
        fn drop(&mut self) {
            use std::os::windows::io::AsRawHandle as _;
            if !self.1 {
                // SAFETY: a process handle with PROCESS_TERMINATE, which this holds.
                unsafe { windows_sys::Win32::System::Threading::TerminateProcess(self.0.as_raw_handle(), 1) };
            }
        }
    }

    /// A named pipe's server end at `name`, inbound, one instance.
    #[cfg(windows)]
    fn pipe_server(name: &str) -> std::fs::File {
        use std::os::windows::io::FromRawHandle as _;
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_INBOUND;
        use windows_sys::Win32::System::Pipes::{
            CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
        };
        let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
        // SAFETY: a NUL-terminated name; a pipe of default security, owned below.
        let raw = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_INBOUND,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                0,
                0,
                0,
                std::ptr::null(),
            )
        };
        assert_ne!(
            raw,
            INVALID_HANDLE_VALUE,
            "{name}: {}",
            io::Error::last_os_error()
        );
        // SAFETY: the pipe just made, which nothing else owns.
        unsafe { std::fs::File::from_raw_handle(raw) }
    }

    /// What the first client of `server` writes, to its close: none where it writes nothing.
    #[cfg(windows)]
    fn pipe_read(server: &mut std::fs::File) -> String {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Foundation::{ERROR_PIPE_CONNECTED, GetLastError};
        use windows_sys::Win32::System::Pipes::ConnectNamedPipe;
        // SAFETY: a pipe's server end this holds, waited on until a client opens it.
        let connected = unsafe { ConnectNamedPipe(server.as_raw_handle(), std::ptr::null_mut()) } != 0;
        // SAFETY: reads this thread's last error, which the call above set.
        let connected = connected || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
        assert!(connected, "{}", io::Error::last_os_error());
        let mut said = String::new();
        server.read_to_string(&mut said).unwrap();
        said
    }

    /// The Windows twin of `a_stop_mid_boot_ends_the_boot_its_host_and_all_its_host_started`:
    /// do boot through a run host that starts a VM in its job and never says the run
    /// started, and stop the boot once both run. Expect: the boot returns `Ended(Stopped)`,
    /// well within its bound, the run host's handle signalled and the VM's with it (its
    /// job's last handle closed as the host was ended), no orphan left, and no file of the
    /// host's left in its home's `layers`, another host's kept.
    #[cfg(windows)]
    #[test]
    fn a_stop_mid_boot_ends_the_boot_its_host_and_the_vm_of_its_job() {
        let dir = shards_testdir::TempDir::new("bstop").unwrap();
        let (home, others) = layered_home(dir.path());
        let tells = format!(r"\\.\pipe\shards-api-bstop-{}", std::process::id());
        let mut server = pipe_server(&tells);
        let spec = stand_in_spec(
            &std::env::current_exe().unwrap(),
            vec![
                (STAND_IN.into(), "host".into()),
                (STAND_IN_TELLS.into(), tells.clone().into()),
                ("SHARDS_HOME".into(), home.clone().into()),
            ],
        );
        let request = super::super::request(&spec).unwrap();
        let stop = super::super::BootStop::new();
        std::thread::scope(|s| {
            let booting = s.spawn(|| {
                let booted = Session::start(&spec, &request, BOOT_BOUND, &stop).map(drop);
                // A boot that ended before its host told anything ends the test's wait:
                // opened and closed, the pipe says nothing. Refused where its host told.
                let _ = std::fs::OpenOptions::new().write(true).open(&tells);
                booted
            });
            let said = pipe_read(&mut server);
            let pids: Vec<u32> = said.split_whitespace().map(|p| p.parse().unwrap()).collect();
            let [host, vm] = pids[..] else {
                panic!("its host told no pids: {said:?}; the boot: {:?}", booting.join());
            };
            // Each opened while it runs, its host blocked on its asks and its VM on itself.
            let (mut host, mut vm) = (Held::open(host), Held::open(vm));
            let stopped_at = Instant::now();
            stop.stop();
            let booted = booting.join().unwrap();
            let took = stopped_at.elapsed();
            let (host_gone, vm_gone) = (host.ended(), vm.ended());
            assert_eq!(
                booted,
                Err(ImageRefusal::Ended(RunEnd::Stopped)),
                "after {took:?}"
            );
            assert!(took < BOOT_BOUND, "the stop took {took:?}");
            assert!(host_gone, "the run host outlived its boot's stop");
            assert!(vm_gone, "the VM in its run host's job outlived the boot's stop");
        });
        assert!(stop.is_stopped());
        let others = others.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(layer_files(&home), [others], "its run host's layer outlived it");
    }
}
