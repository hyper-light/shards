//! The slim VM process's private, credit-driven link to the owner's capture service.

use shards_abi::run::{
    self, kind,
    managed::{self, Channel, Record, StreamState},
};
use shards_vmm::platform::UnixStream;
use std::io::{self, Read, Write};
use std::sync::{Condvar, Mutex, PoisonError};

pub struct Given {
    pub stream: shards_ipc::Stream,
    pub control: shards_ipc::Stream,
    pub capacity: usize,
    pub members: u32,
}

#[derive(Clone, Copy)]
enum Credit {
    Inactive,
    Waiting,
    Granted(usize),
    Forwarding,
    Closed,
}
#[derive(Clone, Copy)]
struct Flow {
    credit: Credit,
    stream: StreamState,
}
impl Flow {
    const NEW: Self = Self {
        credit: Credit::Inactive,
        stream: StreamState::New,
    };
}
enum Owner {
    Running,
    Stopped,
}
struct Flows {
    owner: Owner,
    streams: Vec<[Flow; 2]>,
    /// How the relay ended, as the first of its two sides to end said it ([`Relayed`]): the
    /// other's end is that end's consequence.
    ended: Option<Relayed>,
}
struct Ledger {
    flows: Mutex<Flows>,
    changed: Condvar,
}

fn invalid(why: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why.to_string())
}
fn flow(flows: &mut Flows, member: u32, channel: Channel) -> io::Result<&mut Flow> {
    let [out, err] = flows
        .streams
        .get_mut(usize::try_from(member).map_err(invalid)?)
        .ok_or_else(|| invalid("managed member was not admitted"))?;
    Ok(match channel {
        Channel::Stdout => out,
        Channel::Stderr => err,
    })
}
impl Ledger {
    fn lock(&self) -> std::sync::MutexGuard<'_, Flows> {
        self.flows.lock().unwrap_or_else(PoisonError::into_inner)
    }
    fn stop(&self) {
        self.lock().owner = Owner::Stopped;
        self.changed.notify_all();
    }
    /// The relay ends as `ended` says, where neither side ended it before.
    fn end(&self, ended: Relayed) {
        let mut flows = self.lock();
        flows.ended.get_or_insert(ended);
        flows.owner = Owner::Stopped;
        self.changed.notify_all();
    }
}

/// How a relay of a run's members' output ended: each of its channels' end relayed; a side's
/// end of its connection, its guest's as its run ends, or its owner's, which is its owner's
/// to end the run for, or its `STOP_ORDER`'s where its owner is gone (D27); or broken, its
/// owner's frames or its guest's refused, or the relay's own failure, as said.
enum Relayed {
    Done,
    Ended,
    Broke(io::Error),
}

impl Relayed {
    /// How `e`, the error one of a relay's sides ended on, ends it: a frame refused
    /// (`invalid`), or the relay's own failure (its allocation, its worker), breaks it; any
    /// other is that side's connection ended.
    fn of(e: io::Error) -> Relayed {
        match e.kind() {
            io::ErrorKind::InvalidData | io::ErrorKind::OutOfMemory | io::ErrorKind::Other => {
                Relayed::Broke(e)
            }
            _ => Relayed::Ended,
        }
    }
}

#[derive(Clone, Copy)]
enum AbortState {
    Active,
    Aborted,
}
/// Whether a member has an input, and whether its end has been told.
#[derive(Clone, Copy, PartialEq, Eq)]
enum InputState {
    None,
    Open,
    Ended,
}
pub struct Control {
    daemon: shards_ipc::Stream,
    members: Vec<AbortState>,
    inputs: Vec<InputState>,
}
/// How a run's managed control scope ended, as its guest said it.
#[derive(Debug, PartialEq, Eq)]
pub enum Served {
    /// Its own end (`MANAGED_CONTROL_END`): the signal connection goes on to its relay's end.
    ControlEnded,
    /// Its relay's end (`SIGNALS_END`), the scope's with it: init says it so where a failure
    /// broke its relay, then its words and its end on the run's connection (init run.rs).
    RelayEnded,
}
impl Control {
    /// [`Control::serve`]; the guest's run connection, `boundary`, ended where its control is
    /// refused or ends unannounced.
    pub fn supervise(self, signals: &UnixStream, boundary: &UnixStream) -> io::Result<Served> {
        let result = self.serve(signals);
        if let Err(e) = &result {
            *ENDED_RUN.lock().unwrap_or_else(PoisonError::into_inner) =
                Some(format!("its managed control ended: {e}"));
            super::end_guest(boundary);
        }
        result
    }
    pub fn serve(mut self, signals: &UnixStream) -> io::Result<Served> {
        let mut read = shards_ipc::Polled(signals);
        loop {
            let mut header = [0; run::HEADER];
            read.read_exact(&mut header)?;
            match run::parse_header(header) {
                Some((kind::MANAGED_CONTROL_END, 0)) => return Ok(Served::ControlEnded),
                Some((kind::SIGNALS_END, 0)) => return Ok(Served::RelayEnded),
                Some((kind::MANAGED_ABORT, n)) if n == managed::ABORT_LEN as u32 => {}
                Some((kind::MANAGED_INPUT_ENDED, n)) if n == managed::INPUT_ENDED_LEN as u32 => {
                    let mut payload = [0; managed::INPUT_ENDED_LEN];
                    read.read_exact(&mut payload)?;
                    let ended = managed::InputEnded::decode(&payload).map_err(invalid)?;
                    let state = self
                        .inputs
                        .get_mut(usize::try_from(ended.member).map_err(invalid)?)
                        .filter(|s| **s == InputState::Open)
                        .ok_or_else(|| invalid("managed input end of a member with no open input"))?;
                    crate::warm::managed_input_ended(&self.daemon, &payload)?;
                    *state = InputState::Ended;
                    continue;
                }
                _ => return Err(invalid("managed abort frame")),
            }
            let mut payload = [0; managed::ABORT_LEN];
            read.read_exact(&mut payload)?;
            let payload = payload.as_slice();
            let abort = managed::Abort::decode(payload).map_err(invalid)?;
            let state = self
                .members
                .get_mut(usize::try_from(abort.member).map_err(invalid)?)
                .ok_or_else(|| invalid("managed abort tenant"))?;
            if matches!(*state, AbortState::Aborted) {
                continue;
            }
            crate::warm::managed_abort(&self.daemon, payload)?;
            *state = AbortState::Aborted;
        }
    }
}

/// Starts the run's managed capture: its members' output relayed from its guest to its owner,
/// on a thread of its own, once its guest dials it; `run`, a copy of the run's connection,
/// which a relay that breaks ends ([`relay_ending`]).
pub fn start(spec: &mut run::Spec, given: Given, run: UnixStream) -> Result<Control, String> {
    ENDED_RUN.lock().unwrap_or_else(PoisonError::into_inner).take();
    if managed::capacity(&spec.setup).map_err(|e| e.to_string())? != Some(given.capacity)
        || managed::members(&spec.setup).map_err(|e| e.to_string())? != Some(given.members)
        || managed::token(&spec.setup).map_err(|e| e.to_string())?.is_some()
    {
        return Err("managed capture descriptor and admission do not match".into());
    }
    let members = usize::try_from(given.members).map_err(|e| e.to_string())?;
    let mut states = Vec::new();
    states
        .try_reserve_exact(members)
        .map_err(|e| format!("managed stream state: {e}"))?;
    states.resize(members, [Flow::NEW; 2]);
    let active = managed::active(&spec.setup)
        .map_err(|e| e.to_string())?
        .ok_or("managed activation missing")?;
    for id in active {
        let pair = states
            .get_mut(usize::try_from(u32::from_be_bytes(*id)).map_err(|e| e.to_string())?)
            .ok_or("managed active member exceeds bound")?;
        for slot in pair {
            slot.credit = Credit::Waiting;
        }
    }
    let mut aborts = Vec::new();
    aborts.try_reserve_exact(members).map_err(|e| e.to_string())?;
    aborts.resize(members, AbortState::Active);
    let mut input_states = Vec::new();
    input_states
        .try_reserve_exact(members)
        .map_err(|e| e.to_string())?;
    input_states.resize(members, InputState::None);
    // Each member whose input its owner writes: a token of its own for its connection, which
    // goes to the owner as its hello comes. The tokens are this process's to mint, so a
    // request bringing any is refused; `input_tokens` would refuse every request asking an
    // input, holding a setup to one token for each input member, which none has yet.
    if spec.setup.iter().any(|e| e.starts_with(managed::INPUT_TOKEN_KEY)) {
        return Err("managed input tokens are the VM's to mint".into());
    }
    let mut inputs = Vec::new();
    let named = managed::inputs(&spec.setup)
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
    inputs.try_reserve_exact(named.len()).map_err(|e| e.to_string())?;
    inputs.extend_from_slice(named);
    for id in &inputs {
        let slot = input_states
            .get_mut(usize::try_from(u32::from_be_bytes(*id)).map_err(|e| e.to_string())?)
            .ok_or("managed input member exceeds bound")?;
        *slot = InputState::Open;
    }
    let handing = given
        .control
        .try_clone()
        .map_err(|e| format!("managed input's owner link: {e}"))?;
    start_inputs(spec, &inputs, handing)?;
    let control = Control {
        daemon: given.control,
        members: aborts,
        inputs: input_states,
    };
    let mut token = [0; run::TOKEN];
    shards_vmm::platform::fill_random(&mut token).map_err(|e| format!("managed capture token: {e}"))?;
    let mut entry = Vec::new();
    let length = managed::TOKEN_KEY
        .len()
        .checked_add(token.len())
        .ok_or("managed token entry overflow")?;
    entry.try_reserve_exact(length).map_err(|e| e.to_string())?;
    entry.extend_from_slice(managed::TOKEN_KEY);
    entry.extend_from_slice(&token);
    spec.setup.try_reserve(1).map_err(|e| e.to_string())?;
    let (to, from) = std::sync::mpsc::sync_channel(1);
    super::register(token, super::Target::Capture(to))?;
    if let Err(e) = std::thread::Builder::new()
        .name("managed-capture".into())
        .spawn(move || {
            let result = from
                .recv()
                .map_err(io::Error::other)
                .and_then(|c| c.map_err(io::Error::other))
                .and_then(|guest| relay_ending(guest, given.stream, given.capacity, states, &run));
            if let Err(e) = result {
                shards_vmm::debug!("managed capture aborted: {e}");
            }
        })
    {
        super::hellos::forget(&token);
        return Err(format!("managed capture worker: {e}"));
    }
    spec.setup.push(entry);
    Ok(control)
}

/// Mints each input member's token into the run's setup and registers it, and hands each
/// connection whose hello says one to the run's owner as it comes, on a thread of its own,
/// so that no hello waits on another's hand-over. A connection the owner cannot take is
/// closed here, and init tells its owner its input ended (`ECONNRESET`).
fn start_inputs(spec: &mut run::Spec, inputs: &[[u8; 4]], owner: shards_ipc::Stream) -> Result<(), String> {
    if inputs.is_empty() {
        return Ok(());
    }
    let (to, from) = std::sync::mpsc::sync_channel(inputs.len());
    let mut tokens = Vec::new();
    tokens
        .try_reserve_exact(inputs.len())
        .map_err(|e| e.to_string())?;
    spec.setup.try_reserve(inputs.len()).map_err(|e| e.to_string())?;
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(inputs.len())
        .map_err(|e| e.to_string())?;
    for id in inputs {
        let mut token = [0; run::TOKEN];
        shards_vmm::platform::fill_random(&mut token).map_err(|e| format!("managed input token: {e}"))?;
        let mut entry = Vec::new();
        let length = managed::INPUT_TOKEN_KEY
            .len()
            .checked_add(id.len())
            .and_then(|n| n.checked_add(token.len()))
            .ok_or("managed input token entry overflow")?;
        entry.try_reserve_exact(length).map_err(|e| e.to_string())?;
        entry.extend_from_slice(managed::INPUT_TOKEN_KEY);
        entry.extend_from_slice(id);
        entry.extend_from_slice(&token);
        entries.push(entry);
        tokens.push(token);
    }
    // Waits whose connection was never asked of the guest, as its setup does not go.
    let forget = |tokens: &[[u8; run::TOKEN]]| {
        for token in tokens {
            super::hellos::forget(token);
        }
    };
    for (id, token) in inputs.iter().zip(&tokens) {
        let target = super::Target::Input {
            member: u32::from_be_bytes(*id),
            to: to.clone(),
        };
        if let Err(e) = super::register(*token, target) {
            forget(&tokens);
            return Err(e);
        }
    }
    drop(to);
    if let Err(e) = std::thread::Builder::new()
        .name("managed-input".into())
        .spawn(move || {
            for (member, conn) in from {
                if let Err(e) = crate::warm::hand_input(&owner, member, conn) {
                    shards_vmm::debug!("managed input of member {member}: its owner did not take it: {e}");
                }
            }
        })
    {
        forget(&tokens);
        return Err(format!("managed input worker: {e}"));
    }
    spec.setup.extend(entries);
    Ok(())
}

/// Why the run's managed capture ended the run, where its relay broke ([`relay_ending`]) or
/// its control was refused or ended unannounced ([`Control::supervise`]), said whole.
static ENDED_RUN: Mutex<Option<String>> = Mutex::new(None);

/// Why the run's managed capture ended its run, where it did ([`ENDED_RUN`]), taken: for its
/// serve to say, in place of the end of the run's connection it caused.
pub fn ended_run() -> Option<String> {
    ENDED_RUN.lock().unwrap_or_else(PoisonError::into_inner).take()
}

/// [`relay`], and its run with it where it broke while its VM runs on: why kept for the
/// run's serve to say ([`ended_run`]), then the run's connection, `run`, ended, as its guest
/// takes for its end (it powers off) and its serve does for the guest's, which then fails the
/// run in those words (warm.rs `finish`, `UNSERVED`, D258). Its members' output gone, a run
/// that went on would wait for its owner's `STOP`. A relay that ended with every channel's
/// end, or as one of its sides ended its connection, ends nothing of the run's: its run's
/// end is its own, its owner's, or its `STOP_ORDER`'s.
fn relay_ending(
    guest: UnixStream,
    owner: shards_ipc::Stream,
    capacity: usize,
    streams: Vec<[Flow; 2]>,
    run: &UnixStream,
) -> io::Result<()> {
    match relay(guest, owner, capacity, streams) {
        Relayed::Done => Ok(()),
        Relayed::Ended => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "one of its sides ended its connection",
        )),
        Relayed::Broke(e) => {
            *ENDED_RUN.lock().unwrap_or_else(PoisonError::into_inner) =
                Some(format!("its members' output could not be relayed: {e}"));
            super::end_guest(run);
            Err(e)
        }
    }
}

fn relay(guest: UnixStream, owner: shards_ipc::Stream, capacity: usize, streams: Vec<[Flow; 2]>) -> Relayed {
    let ledger = Ledger {
        flows: Mutex::new(Flows {
            owner: Owner::Running,
            streams,
            ended: None,
        }),
        changed: Condvar::new(),
    };
    std::thread::scope(|scope| {
        let credit = std::thread::Builder::new()
            .name("managed-credit".into())
            .spawn_scoped(scope, || {
                if let Err(e) = credits(&guest, &owner, capacity, &ledger) {
                    ledger.end(Relayed::of(e));
                }
                ledger.stop();
                super::end_guest(&guest);
                shards_ipc::end(&owner);
            });
        let credit = match credit {
            Ok(credit) => credit,
            Err(e) => return Relayed::Broke(e),
        };
        let ended = match records(&guest, &owner, capacity, &ledger) {
            Ok(()) => Relayed::Done,
            Err(e) => Relayed::of(e),
        };
        ledger.end(ended);
        shards_ipc::end(&owner);
        super::end_guest(&guest);
        if credit.join().is_err() {
            ledger.end(Relayed::Broke(io::Error::other("managed credit worker panicked")));
        }
        ledger.lock().ended.take().unwrap_or(Relayed::Ended)
    })
}

fn credits(
    guest: &UnixStream,
    owner: &shards_ipc::Stream,
    ceiling: usize,
    ledger: &Ledger,
) -> io::Result<()> {
    let mut read = shards_ipc::Polled(owner);
    loop {
        let mut bytes = [0; run::HEADER + managed::CREDIT_LEN];
        read.read_exact(&mut bytes)?;
        let (header, payload) = bytes
            .split_first_chunk::<{ run::HEADER }>()
            .ok_or_else(|| invalid("managed credit header"))?;
        if run::parse_header(*header) != Some((kind::MANAGED_CREDIT, managed::CREDIT_LEN as u32)) {
            return Err(invalid("managed credit frame"));
        }
        let credit = managed::Credit::decode(payload).map_err(invalid)?;
        let capacity = usize::try_from(credit.capacity).map_err(invalid)?;
        if capacity > ceiling {
            return Err(invalid("managed credit exceeds protocol ceiling"));
        }
        {
            let mut flows = ledger.lock();
            loop {
                if matches!(flows.owner, Owner::Stopped) {
                    return Err(io::ErrorKind::ConnectionAborted.into());
                }
                let slot = flow(&mut flows, credit.member, credit.channel)?;
                match slot.credit {
                    Credit::Waiting => {
                        slot.credit = Credit::Granted(capacity);
                        break;
                    }
                    Credit::Forwarding => {
                        flows = ledger.changed.wait(flows).unwrap_or_else(PoisonError::into_inner);
                    }
                    Credit::Inactive | Credit::Granted(_) | Credit::Closed => {
                        return Err(invalid("duplicate or closed managed credit"));
                    }
                }
            }
        }
        (&*guest).write_all(&bytes)?;
    }
}

fn records(
    guest: &UnixStream,
    owner: &shards_ipc::Stream,
    ceiling: usize,
    ledger: &Ledger,
) -> io::Result<()> {
    let mut read = shards_ipc::Polled(guest);
    loop {
        let mut header = [0; run::HEADER];
        read.read_exact(&mut header)?;
        let (which, len) = run::parse_header(header).ok_or_else(|| invalid("managed frame header"))?;
        let len = usize::try_from(len).map_err(invalid)?;
        if which != kind::MANAGED || len > ceiling || len < managed::HEADER {
            return Err(invalid("managed frame exceeds ceiling"));
        }
        let mut prefix = [0; managed::HEADER];
        read.read_exact(&mut prefix)?;
        let (member, channel) = managed::route_prefix(&prefix).map_err(invalid)?;
        let capacity = {
            let mut flows = ledger.lock();
            let slot = flow(&mut flows, member, channel)?;
            let Credit::Granted(capacity) = slot.credit else {
                return Err(invalid("managed data has no channel credit"));
            };
            if len > capacity {
                return Err(invalid("managed data exceeds channel credit"));
            }
            slot.credit = Credit::Forwarding;
            capacity
        };
        let mut record = Vec::new();
        record.try_reserve_exact(len).map_err(io::Error::other)?;
        if record.capacity() > capacity {
            return Err(invalid("managed allocation exceeds channel credit"));
        }
        record.extend_from_slice(&prefix);
        record.resize(len, 0);
        read.read_exact(
            record
                .get_mut(managed::HEADER..)
                .ok_or_else(|| invalid("managed payload buffer"))?,
        )?;
        let parsed = Record::decode(&record).map_err(invalid)?;
        flow(&mut ledger.lock(), member, channel)?
            .stream
            .accept(parsed)
            .map_err(invalid)?;
        (&*owner).write_all(&header)?;
        (&*owner).write_all(&record)?;
        let done = {
            let mut flows = ledger.lock();
            let slot = flow(&mut flows, member, channel)?;
            slot.credit = if slot.stream.terminal() {
                Credit::Closed
            } else {
                Credit::Waiting
            };
            flows.streams.iter().all(|pair| {
                pair.iter()
                    .all(|slot| matches!(slot.credit, Credit::Inactive) || slot.stream.terminal())
            })
        };
        ledger.changed.notify_all();
        if done {
            return Ok(());
        }
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use shards_abi::run::managed::Event;
    use std::time::Duration;

    fn send_credit(owner: &mut shards_ipc::Stream, member: u32, channel: Channel, capacity: usize) {
        owner
            .write_all(&run::header(kind::MANAGED_CREDIT, managed::CREDIT_LEN as u32))
            .unwrap();
        owner
            .write_all(
                &managed::Credit {
                    member,
                    channel,
                    capacity: capacity as u64,
                }
                .encode()
                .unwrap(),
            )
            .unwrap();
    }
    fn record(stream: &mut impl Read) -> (u8, Vec<u8>) {
        let mut header = [0; run::HEADER];
        stream.read_exact(&mut header).unwrap();
        let (kind, len) = run::parse_header(header).unwrap();
        let mut bytes = vec![0; len as usize];
        stream.read_exact(&mut bytes).unwrap();
        (kind, bytes)
    }
    fn send_record(guest: &mut UnixStream, record: Record<'_>) -> Vec<u8> {
        let mut bytes = vec![0; record.encoded_len().unwrap()];
        record.encode(&mut bytes).unwrap();
        guest
            .write_all(&run::header(kind::MANAGED, bytes.len() as u32))
            .unwrap();
        guest.write_all(&bytes).unwrap();
        bytes
    }

    #[test]
    fn duplex_credit_services_quiet_sibling_stderr_while_stdout_is_idle() {
        let (vm, mut guest) = UnixStream::pair().unwrap();
        let (pipe, mut owner) = shards_ipc::Stream::pair().unwrap();
        guest.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        owner.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let capacity = managed::HEADER + b"quiet stderr\0\xff".len();
        let mut streams = vec![[Flow::NEW; 2]; 3];
        for member in [0, 2] {
            for slot in &mut streams[member] {
                slot.credit = Credit::Waiting;
            }
        }
        let worker = std::thread::spawn(move || relay(vm, pipe, capacity, streams));
        send_credit(&mut owner, 0, Channel::Stdout, capacity);
        send_credit(&mut owner, 2, Channel::Stderr, capacity);
        for member in [0, 2] {
            let (kind, bytes) = record(&mut guest);
            assert_eq!(kind, kind::MANAGED_CREDIT);
            assert_eq!(managed::Credit::decode(&bytes).unwrap().member, member);
        }
        let expected = send_record(
            &mut guest,
            Record {
                member: 2,
                channel: Channel::Stderr,
                offset: 0,
                event: Event::Open("agent quiet"),
            },
        );
        assert_eq!(record(&mut owner), (kind::MANAGED, expected));
        send_credit(&mut owner, 2, Channel::Stderr, capacity);
        record(&mut guest);
        let payload = b"quiet stderr\0\xff";
        let expected = send_record(
            &mut guest,
            Record {
                member: 2,
                channel: Channel::Stderr,
                offset: 0,
                event: Event::Data(payload),
            },
        );
        assert_eq!(record(&mut owner), (kind::MANAGED, expected));
        send_credit(&mut owner, 2, Channel::Stderr, capacity);
        record(&mut guest);
        let expected = send_record(
            &mut guest,
            Record {
                member: 2,
                channel: Channel::Stderr,
                offset: payload.len() as u64,
                event: Event::End,
            },
        );
        assert_eq!(record(&mut owner), (kind::MANAGED, expected));
        drop(owner);
        assert!(matches!(worker.join().unwrap(), Relayed::Ended));
    }

    /// A relay that breaks while its VM runs on (here its owner's credit for a channel given
    /// again before the first was used, which it refuses) ends the run in the VM: the run's
    /// connection ended, so that its guest powers off and its serve returns, and why kept for
    /// that serve to say (UNSERVED, D258); goal 75's WHP case left such a run to its owner's
    /// STOP. One whose guest ends its capture, or whose owner ends its own, ends nothing of
    /// its run: the guest's run ends with it, and an owner's end is its owner's to end the run
    /// for, or its STOP_ORDER's where its owner is gone.
    #[test]
    fn a_relay_that_breaks_mid_run_ends_its_run_saying_why() {
        let _serial = super::super::hellos::TESTS
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let capacity = managed::HEADER + 16;
        let relaying = |end: &dyn Fn(&mut shards_ipc::Stream, &mut UnixStream)| {
            let (vm, mut guest) = UnixStream::pair().unwrap();
            let (pipe, mut owner) = shards_ipc::Stream::pair().unwrap();
            let (run, guests_run) = UnixStream::pair().unwrap();
            // The run's connection as its serve holds it beside the relay's copy, which the
            // relay's end drops.
            let held = run.try_clone().unwrap();
            guest.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            guests_run
                .set_read_timeout(Some(Duration::from_millis(500)))
                .unwrap();
            let mut streams = vec![[Flow::NEW; 2]];
            for slot in &mut streams[0] {
                slot.credit = Credit::Waiting;
            }
            let worker = std::thread::spawn(move || relay_ending(vm, pipe, capacity, streams, &run));
            send_credit(&mut owner, 0, Channel::Stdout, capacity);
            assert_eq!(record(&mut guest).0, kind::MANAGED_CREDIT);
            end(&mut owner, &mut guest);
            let _ = worker.join().unwrap();
            // Its run's end, as the guest's side of the run's connection reads it.
            let ended = matches!((&guests_run).read(&mut [0]), Ok(0));
            drop(held);
            (ended, ended_run())
        };
        let (ended, why) = relaying(&|owner, _| send_credit(owner, 0, Channel::Stdout, capacity));
        assert!(ended, "a relay that broke left its run's connection open");
        assert_eq!(
            why.as_deref(),
            Some("its members' output could not be relayed: duplicate or closed managed credit")
        );
        let (ended, why) = relaying(&|_, guest| {
            guest.shutdown(std::net::Shutdown::Both).unwrap();
        });
        assert!(!ended, "a guest's end of its capture ended its run");
        assert_eq!(why, None);
        let (ended, why) = relaying(&|owner, _| shards_ipc::end(owner));
        assert!(!ended, "an owner's end of its capture ended its run");
        assert_eq!(why, None);
    }

    #[test]
    fn explicit_control_end_preserves_layer_connection_but_unannounced_eof_ends_it() {
        let _serial = super::super::hellos::TESTS
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for graceful in [true, false] {
            ended_run();
            let (signals, mut peer) = UnixStream::pair().unwrap();
            let (boundary, mut layer) = UnixStream::pair().unwrap();
            let (daemon, _owner) = shards_ipc::Stream::pair().unwrap();
            let control = Control {
                daemon,
                members: vec![AbortState::Active],
                inputs: vec![InputState::None],
            };
            let retained_boundary = boundary.try_clone().unwrap();
            let worker = std::thread::spawn(move || control.supervise(&signals, &boundary));
            if graceful {
                peer.write_all(&run::header(kind::MANAGED_CONTROL_END, 0))
                    .unwrap();
            }
            drop(peer);
            let result = worker.join().unwrap();
            if graceful {
                assert!(result.is_ok());
                (&retained_boundary)
                    .write_all(&run::header(kind::LAYER, 1))
                    .unwrap();
                (&retained_boundary).write_all(b"x").unwrap();
                assert_eq!(record(&mut layer), (kind::LAYER, b"x".to_vec()));
            } else {
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
                assert_eq!(
                    ended_run().as_deref(),
                    Some("its managed control ended: failed to fill whole buffer")
                );
                assert!(matches!(layer.read(&mut [0]), Ok(0)));
            }
        }
    }

    /// A guest whose relay ends inside the control scope, as init's does on a failure that
    /// breaks its relay (run.rs: `say_signals_end` after the loop, then `SYSTEM_ERR` and `EXIT`
    /// on the run's connection), has said its end: the scope ends with it, the run's
    /// connection is left open for those words, and nothing is refused. Refused as "managed
    /// abort frame", it ended the run's connection before they were read, and the run ended
    /// "the guest stopped before the command ended: failed to fill whole buffer" (CI run
    /// 43513425954, api_image's staged_credentials case, its signal connection lost to
    /// "invalid data" just before).
    #[test]
    fn a_relays_end_within_the_control_scope_ends_it_and_leaves_the_run_open() {
        let _serial = super::super::hellos::TESTS
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        ended_run();
        let (signals, mut peer) = UnixStream::pair().unwrap();
        let (boundary, mut guests_run) = UnixStream::pair().unwrap();
        let (daemon, _owner) = shards_ipc::Stream::pair().unwrap();
        let control = Control {
            daemon,
            members: vec![AbortState::Active],
            inputs: vec![InputState::None],
        };
        let retained = boundary.try_clone().unwrap();
        let worker = std::thread::spawn(move || control.supervise(&signals, &boundary));
        peer.write_all(&run::header(kind::SIGNALS_END, 0)).unwrap();
        assert_eq!(worker.join().unwrap().unwrap(), Served::RelayEnded);
        assert_eq!(ended_run(), None);
        // The guest's words after its relay's end reach the host on the run's connection.
        guests_run.write_all(&run::header(kind::EXIT, 4)).unwrap();
        guests_run.write_all(&3u32.to_be_bytes()).unwrap();
        let mut said = [0; run::HEADER + 4];
        (&retained).read_exact(&mut said).unwrap();
        assert_eq!(said[run::HEADER..], 3u32.to_be_bytes());
        // And the host's way to the guest is open too.
        (&retained).write_all(b"x").unwrap();
        assert_eq!(guests_run.read(&mut [0]).unwrap(), 1);
    }

    #[test]
    fn trusted_abort_preserves_tenant_errno_and_coalesces_duplicate() {
        let (signals, mut peer) = UnixStream::pair().unwrap();
        let (daemon, owner) = shards_ipc::Stream::pair().unwrap();
        let control = Control {
            daemon,
            members: vec![AbortState::Active; 3],
            inputs: vec![InputState::None; 3],
        };
        let worker = std::thread::spawn(move || control.serve(&signals));
        let abort = managed::Abort { member: 2, errno: 5 };
        for _ in 0..2 {
            peer.write_all(&run::header(kind::MANAGED_ABORT, managed::ABORT_LEN as u32))
                .unwrap();
            peer.write_all(&abort.encode().unwrap()).unwrap();
        }
        peer.write_all(&run::header(kind::MANAGED_CONTROL_END, 0))
            .unwrap();
        worker.join().unwrap().unwrap();
        let message = shards_ipc::recv(&owner).unwrap().unwrap();
        assert_eq!(message.kind, shards_ipc::kind::MANAGED_ABORT);
        assert_eq!(managed::Abort::decode(&message.payload).unwrap(), abort);
        assert!(shards_ipc::recv(&owner).unwrap().is_none());
    }

    /// A run whose owner writes a member's stdin starts with that member's token minted
    /// here, one a member, and a request that brings one of its own is refused: the tokens
    /// are the VM's alone. Every run asking a member's stdin was refused before its start
    /// (ROOT's HVF run at 27ed7cccd, the api_image and member_exec stdin cases Stopped at
    /// once; Linux CI at 5b4a561e5, managed_output's member_stdin runs exiting 125 with no
    /// words), its request's input members holding no token yet.
    #[test]
    fn an_input_members_token_is_minted_here_and_none_is_taken_from_its_request() {
        let _serial = super::super::hellos::TESTS
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        super::super::hellos::reopen();
        let (_port, _acceptor) = super::super::exec_port().unwrap();
        let spec = |extra: Option<Vec<u8>>| {
            let mut setup = vec![
                [managed::CAPACITY_KEY, b"4096"].concat(),
                [managed::MEMBERS_KEY, b"2"].concat(),
                [managed::ACTIVE_KEY, &1u32.to_be_bytes()[..]].concat(),
                [managed::INPUT_KEY, &1u32.to_be_bytes()[..]].concat(),
            ];
            setup.extend(extra);
            run::Spec {
                setup,
                ..run::Spec::default()
            }
        };
        let given = || Given {
            stream: shards_ipc::Stream::pair().unwrap().0,
            control: shards_ipc::Stream::pair().unwrap().0,
            capacity: 4096,
            members: 2,
        };
        let run = || UnixStream::pair().unwrap().0;
        let mut asked = spec(None);
        let started = start(&mut asked, given(), run());
        assert!(started.is_ok(), "{:?}", started.err());
        let minted = managed::input_tokens(&asked.setup).unwrap();
        assert_eq!(minted.iter().map(|(m, _)| *m).collect::<Vec<_>>(), [1]);
        let brought = [
            managed::INPUT_TOKEN_KEY,
            &1u32.to_be_bytes()[..],
            &[7; run::TOKEN],
        ]
        .concat();
        let refused = start(&mut spec(Some(brought)), given(), run());
        assert_eq!(
            refused.err().as_deref(),
            Some("managed input tokens are the VM's to mint")
        );
        super::super::hellos::close();
    }

    /// A member's input's end, as its init tells it, goes to the owner checked and once a
    /// member: a second end, or one for a member whose input is not open, breaks the control
    /// scope.
    #[test]
    fn an_inputs_end_is_relayed_once_for_an_open_input_alone() {
        let ended = managed::InputEnded {
            member: 1,
            written: 9,
            end: managed::InputEnd::Aborted(libc::EPIPE),
        }
        .encode()
        .unwrap();
        let said = |peer: &mut UnixStream, payload: &[u8]| {
            peer.write_all(&run::header(
                kind::MANAGED_INPUT_ENDED,
                managed::INPUT_ENDED_LEN as u32,
            ))
            .unwrap();
            peer.write_all(payload).unwrap();
        };
        let (signals, mut peer) = UnixStream::pair().unwrap();
        let (daemon, owner) = shards_ipc::Stream::pair().unwrap();
        let control = Control {
            daemon,
            members: vec![AbortState::Active; 2],
            inputs: vec![InputState::None, InputState::Open],
        };
        let worker = std::thread::spawn(move || control.serve(&signals));
        said(&mut peer, &ended);
        peer.write_all(&run::header(kind::MANAGED_CONTROL_END, 0))
            .unwrap();
        worker.join().unwrap().unwrap();
        let message = shards_ipc::recv(&owner).unwrap().unwrap();
        assert_eq!(message.kind, shards_ipc::kind::MANAGED_INPUT_ENDED);
        assert_eq!(message.payload, ended);
        // Twice, or for a member with no input: refused.
        for (inputs, times) in [
            (vec![InputState::None, InputState::Open], 2),
            (vec![InputState::None, InputState::None], 1),
        ] {
            let (signals, mut peer) = UnixStream::pair().unwrap();
            let (daemon, _owner) = shards_ipc::Stream::pair().unwrap();
            let control = Control {
                daemon,
                members: vec![AbortState::Active; 2],
                inputs,
            };
            let worker = std::thread::spawn(move || control.serve(&signals));
            for _ in 0..times {
                said(&mut peer, &ended);
            }
            // Closed after: a relay that took them all reads its end, never hangs.
            drop(peer);
            let refused = worker.join().unwrap().unwrap_err();
            assert_eq!(refused.kind(), io::ErrorKind::InvalidData);
        }
    }
}
