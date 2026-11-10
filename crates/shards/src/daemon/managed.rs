//! A run's admitted, independently credited owner-output channels (D180).

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read as _, Seek as _, Write as _};
use std::mem::size_of;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use sha2::{Digest as _, Sha256};
use shards_abi::run::{
    self,
    managed::{Channel, Event, Record, StreamState},
};
use shards_ipc::Stream;
use shards_ipc::fd::AsRawFd as _;
use shards_remote::{
    identity::{self, Fingerprint},
    managed::{Admitted, Control, Endpoint, Producer, Rejected, RunId, Scope},
};

use super::{Daemon, Disk, Threads, lock, log};

const METADATA: &str = "managed-output";
const GENERATION: &str = "managed-generation";
// RFC 8446 §4: Handshake's uint24 length and four-byte type/length head.
const HANDSHAKE_BYTES: usize = (1 << 24) - 1 + 4;

#[derive(Debug)]
pub(super) enum Failure {
    Admission(Admission),
    Auth(Auth),
    Protocol(io::Error),
    Transport(io::Error),
    Endpoint(shards_remote::managed::Error),
    Closed,
}

#[derive(Debug)]
pub(super) enum Auth {
    UnknownContext,
    InvalidKey,
}

#[derive(Debug)]
pub(super) enum Admission {
    UnknownHeadroom,
    Unavailable,
    Insufficient { requested: u64, available: u64 },
    Overflow,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Admission(Admission::Insufficient { requested, available }) => write!(
                out,
                "managed output admission: {requested} bytes requested, {available} available (retry after headroom returns)"
            ),
            Self::Admission(reason) => write!(out, "managed output admission: {reason:?}"),
            Self::Auth(reason) => write!(out, "managed output owner: {reason:?}"),
            Self::Protocol(error) => write!(out, "managed output protocol: {error}"),
            Self::Transport(error) => write!(out, "managed output transport: {error}"),
            Self::Endpoint(error) => std::fmt::Display::fmt(error, out),
            Self::Closed => out.write_str("managed output closed"),
        }
    }
}
impl std::error::Error for Failure {}
impl From<Admission> for Failure {
    fn from(reason: Admission) -> Self {
        Self::Admission(reason)
    }
}
impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::InvalidData => Self::Protocol(error),
            io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionAborted => {
                Self::Closed
            }
            _ => Self::Transport(error),
        }
    }
}

pub(super) struct Outputs {
    runs: Mutex<HashMap<String, Entry>>,
    budget: Arc<Mutex<Option<u64>>>,
}

impl Default for Outputs {
    fn default() -> Self {
        Self {
            runs: Mutex::default(),
            budget: Arc::new(Mutex::new(Some(0))),
        }
    }
}

enum Entry {
    Preparing,
    Serving(Arc<Owner>),
}

struct Charge {
    budget: Arc<Mutex<Option<u64>>>,
    bytes: Mutex<u64>,
}

impl Charge {
    fn reserve(budget: &Arc<Mutex<Option<u64>>>, bytes: u64) -> Result<Self, Admission> {
        let charge = Self {
            budget: Arc::clone(budget),
            bytes: Mutex::new(0),
        };
        charge.grow(bytes)?;
        Ok(charge)
    }

    fn grow(&self, extra: u64) -> Result<(), Admission> {
        let available = crate::build::host_available().ok_or(Admission::UnknownHeadroom)?;
        let mut held = lock(&self.bytes);
        let mut budget = lock(&self.budget);
        let used = budget.ok_or(Admission::Unavailable)?;
        let next = used.checked_add(extra).ok_or(Admission::Overflow)?;
        if next > available {
            return Err(Admission::Insufficient {
                requested: extra,
                available: available.saturating_sub(used),
            });
        }
        *held = held.checked_add(extra).ok_or(Admission::Overflow)?;
        *budget = Some(next);
        Ok(())
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        let held = *lock(&self.bytes);
        let mut budget = lock(&self.budget);
        *budget = budget.and_then(|used| used.checked_sub(held));
    }
}

#[derive(Clone, Copy)]
enum Life {
    Serving,
    Draining,
    Ended,
    Aborted,
}

struct Output {
    scope: Scope,
    address: std::net::SocketAddr,
    control: Control,
    metadata: Arc<Mutex<File>>,
    life: Mutex<Life>,
    credits: std::sync::Weak<Credits>,
    index: usize,
    learning: Mutex<Learning>,
    charge: Arc<Charge>,
}

struct Sample {
    began: Instant,
    end: u64,
    bytes: u64,
}
struct Learning {
    quantum: shards_registry::Quantum,
    sample: Option<Sample>,
    admitted: usize,
}

impl Output {
    fn observed(&self) {
        let at = self.control.progress().consumed;
        let mut learning = lock(&self.learning);
        if learning.sample.as_ref().is_some_and(|sample| sample.end <= at)
            && let Some(sample) = learning.sample.take()
        {
            learning
                .quantum
                .consumed(sample.began, Instant::now(), sample.bytes);
        }
    }

    fn capacity(&self, producer: &Producer) -> io::Result<usize> {
        let mut learning = lock(&self.learning);
        let desired = learning.quantum.desired();
        if desired > learning.admitted {
            let extra = desired
                .checked_sub(learning.admitted)
                .and_then(|n| n.checked_mul(2))
                .and_then(|n| u64::try_from(n).ok())
                .ok_or_else(|| io::Error::other("capture growth overflow"))?;
            // One VM-owned record and one daemon-owned record may overlap while IPC copies.
            // QUIC's in-flight window remains fixed and separately reserved.
            if self.charge.grow(extra).is_ok() {
                producer.admit(desired).map_err(io::Error::other)?;
                learning.admitted = desired;
            }
        }
        Ok(desired.min(learning.admitted))
    }

    fn status(&self, state: Life) {
        let (state, tail) = match state {
            Life::Ended => ("ended", "complete"),
            Life::Aborted => ("aborted", "unknown"),
            _ => return,
        };
        let at = self.control.progress();
        let mut metadata = lock(&self.metadata);
        let _ = writeln!(
            metadata,
            "member {} channel {} state {state} accepted {} consumed {} tail {tail}",
            self.scope.member, self.scope.channel as u8, at.accepted, at.consumed
        );
    }

    fn abort(&self) {
        let mut life = lock(&self.life);
        if matches!(*life, Life::Ended | Life::Aborted) {
            return;
        }
        *life = Life::Aborted;
        if let Some(credits) = self.credits.upgrade() {
            credits.revoke(self.index);
        }
        self.control.abort();
        self.status(Life::Aborted);
    }

    fn finish(&self) -> io::Result<()> {
        let mut life = lock(&self.life);
        if !matches!(*life, Life::Serving) {
            return Err(io::Error::other("capture scope ended"));
        }
        self.control.finish().map_err(io::Error::other)?;
        *life = Life::Draining;
        Ok(())
    }

    fn transport_ended(&self) {
        let mut life = lock(&self.life);
        let at = self.control.progress();
        if matches!(*life, Life::Draining) && at.accepted == at.consumed {
            *life = Life::Ended;
            self.status(Life::Ended);
        } else {
            drop(life);
            self.abort();
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.abort();
    }
}

struct Owner {
    outputs: Vec<Option<Arc<Output>>>,
    stream: Stream,
    credits: Arc<Credits>,
    _charge: Arc<Charge>,
}

impl Owner {
    fn abort(&self) {
        self.credits.close();
        shards_ipc::end(&self.stream);
        for output in self.outputs.iter().flatten() {
            output.abort();
        }
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.abort();
    }
}

#[derive(Clone, Copy)]
enum After {
    More,
    End,
}

enum CreditSlot {
    Inactive,
    Waiting,
    Available,
    Granted { capacity: usize, began: Instant },
    Captured(Vec<u8>, After),
    Send(Vec<u8>, After),
    Sending,
    Ended,
    Revoked,
    RevokedGranted { capacity: usize },
    Receiving,
    RevokedReceiving,
}

enum QueueLife {
    Open,
    Closed,
}
struct Queue {
    slots: Vec<CreditSlot>,
    next: usize,
    life: QueueLife,
}
struct Credits {
    state: Mutex<Queue>,
    changed: Condvar,
}
enum Action {
    Grant(usize),
    Submit(usize, Vec<u8>, After),
}

impl Credits {
    fn revoke(&self, index: usize) {
        let mut state = lock(&self.state);
        if let Some(slot) = state.slots.get_mut(index) {
            *slot = match slot {
                CreditSlot::Granted { capacity, .. } => CreditSlot::RevokedGranted { capacity: *capacity },
                CreditSlot::Receiving => CreditSlot::RevokedReceiving,
                _ => CreditSlot::Revoked,
            };
        }
        self.changed.notify_all();
    }

    fn close(&self) {
        let mut state = lock(&self.state);
        state.life = QueueLife::Closed;
        for slot in &mut state.slots {
            *slot = CreditSlot::Ended;
        }
        self.changed.notify_all();
    }

    fn ready(&self, index: usize) {
        let mut state = lock(&self.state);
        if let Some(slot) = state.slots.get_mut(index) {
            match std::mem::replace(slot, CreditSlot::Waiting) {
                CreditSlot::Waiting => *slot = CreditSlot::Available,
                CreditSlot::Captured(bytes, after) => *slot = CreditSlot::Send(bytes, after),
                CreditSlot::Sending => *slot = CreditSlot::Available,
                old => *slot = old,
            }
        }
        self.changed.notify_all();
    }

    fn begin_record(&self, index: usize, len: usize) -> io::Result<(usize, Instant)> {
        let mut state = lock(&self.state);
        let slot = state
            .slots
            .get_mut(index)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unknown capture route"))?;
        match slot {
            CreditSlot::Granted { capacity, began } if len <= *capacity => {
                let grant = (*capacity, *began);
                *slot = CreditSlot::Receiving;
                Ok(grant)
            }
            CreditSlot::RevokedGranted { capacity } if len <= *capacity => {
                let grant = (*capacity, Instant::now());
                *slot = CreditSlot::RevokedReceiving;
                Ok(grant)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "capture record exceeds issued channel credit",
            )),
        }
    }

    fn captured(&self, index: usize, bytes: Vec<u8>, after: After, producer: &Producer) -> io::Result<()> {
        let mut state = lock(&self.state);
        if let Some(slot @ CreditSlot::RevokedReceiving) = state.slots.get_mut(index) {
            *slot = CreditSlot::Revoked;
            return Ok(());
        }
        let slot = state
            .slots
            .get_mut(index)
            .filter(|slot| matches!(slot, CreditSlot::Receiving))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "capture record has no channel credit")
            })?;
        *slot = if producer.capacity() != 0 {
            CreditSlot::Send(bytes, after)
        } else {
            CreditSlot::Captured(bytes, after)
        };
        self.changed.notify_all();
        Ok(())
    }

    fn next(&self) -> io::Result<Option<Action>> {
        let mut state = lock(&self.state);
        loop {
            if matches!(state.life, QueueLife::Closed)
                || state.slots.iter().all(|s| {
                    matches!(
                        s,
                        CreditSlot::Inactive
                            | CreditSlot::Ended
                            | CreditSlot::Revoked
                            | CreditSlot::RevokedGranted { .. }
                            | CreditSlot::RevokedReceiving
                    )
                })
            {
                return Ok(None);
            }
            let len = state.slots.len();
            if len == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "capture has no admitted channels",
                ));
            }
            for step in 0..len {
                let index = state.next.checked_add(step).map(|n| n % len).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "capture scheduler overflow")
                })?;
                let Some(slot) = state.slots.get_mut(index) else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "capture scheduler index",
                    ));
                };
                let action = match std::mem::replace(slot, CreditSlot::Sending) {
                    CreditSlot::Available => {
                        *slot = CreditSlot::Sending;
                        Some(Action::Grant(index))
                    }
                    CreditSlot::Send(bytes, after) => Some(Action::Submit(index, bytes, after)),
                    old => {
                        *slot = old;
                        None
                    }
                };
                if action.is_some() {
                    state.next = index.checked_add(1).map(|n| n % len).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "capture scheduler overflow")
                    })?;
                    return Ok(action);
                }
            }
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

impl Outputs {
    pub(super) fn abort_member(&self, id: &str, bytes: &[u8]) -> io::Result<()> {
        let abort = run::managed::Abort::decode(bytes).map_err(|e| io::Error::other(e.to_string()))?;
        let owner = match lock(&self.runs).get(id) {
            Some(Entry::Serving(owner)) => Arc::clone(owner),
            _ => return Err(io::Error::other("managed run is not registered")),
        };
        let first = usize::try_from(abort.member)
            .ok()
            .and_then(|n| n.checked_mul(2))
            .ok_or_else(|| io::Error::other("invalid aborted member"))?;
        let end = first
            .checked_add(2)
            .ok_or_else(|| io::Error::other("member overflow"))?;
        let outputs = owner
            .outputs
            .get(first..end)
            .ok_or_else(|| io::Error::other("unknown aborted member"))?;
        if outputs.iter().all(Option::is_none) {
            return Err(io::Error::other("aborted member is inactive"));
        }
        for output in outputs.iter().flatten() {
            output.abort();
        }
        Ok(())
    }

    pub(super) fn published(&self, id: &str) -> Vec<[String; 3]> {
        match lock(&self.runs).get(id) {
            Some(Entry::Serving(owner)) => owner
                .outputs
                .iter()
                .flatten()
                .filter(|output| matches!(*lock(&output.life), Life::Serving | Life::Draining))
                .map(|output| {
                    [
                        format!(
                            "managed-{}-{}/udp",
                            output.scope.member, output.scope.channel as u8
                        ),
                        output.address.to_string(),
                        "authenticated owner output".into(),
                    ]
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    fn begin(&self, id: &str) -> io::Result<()> {
        let mut runs = lock(&self.runs);
        if runs.contains_key(id) {
            return Err(io::Error::other("managed run already has an output scope"));
        }
        runs.try_reserve(1).map_err(io::Error::other)?;
        runs.insert(id.to_owned(), Entry::Preparing);
        Ok(())
    }

    pub(super) fn end(&self, id: &str) {
        let removed = lock(&self.runs).remove(id);
        if let Some(Entry::Serving(owner)) = removed {
            owner.abort();
        }
    }

    pub(super) fn close(&self) {
        let runs = std::mem::take(&mut *lock(&self.runs));
        for entry in runs.into_values() {
            if let Entry::Serving(owner) = entry {
                owner.abort();
            }
        }
    }
}

impl<D: Disk> Daemon<D> {
    pub(super) fn managed_capture<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        id: &str,
        client: &str,
        labels: &[String],
        active: &[u32],
        setup: &mut Vec<Vec<u8>>,
    ) -> Result<Stream, Failure> {
        self.managed.begin(id)?;
        let result = self.start_managed_capture(threads, id, client, labels, active, setup);
        if result.is_err() {
            self.managed.end(id);
        }
        result
    }

    fn start_managed_capture<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        id: &str,
        client: &str,
        labels: &[String],
        active: &[u32],
        setup: &mut Vec<Vec<u8>>,
    ) -> Result<Stream, Failure> {
        let members = u32::try_from(labels.len()).map_err(io::Error::other)?;
        let count = labels
            .len()
            .checked_mul(2)
            .filter(|n| *n != 0)
            .ok_or_else(|| io::Error::other("invalid managed channel count"))?;
        if active.is_empty()
            || active.windows(2).any(|w| w.first() >= w.get(1))
            || active.iter().any(|id| *id >= members)
        {
            return Err(
                io::Error::new(io::ErrorKind::InvalidData, "invalid trusted active member plan").into(),
            );
        }
        let active_count = active
            .len()
            .checked_mul(2)
            .ok_or_else(|| io::Error::other("active channel count overflow"))?;
        let capacity = record_capacity(labels, active)?;
        let transport = capacity
            .checked_add(shards_remote::managed::DATA_HEAD)
            .ok_or_else(|| io::Error::other("capture window overflow"))?;
        let per_channel = [
            capacity,
            capacity,
            transport,
            HANDSHAKE_BYTES,
            shards_remote::managed::CONTROL_BYTES,
            shards_remote::managed::DATAGRAM_BYTES,
            shards_remote::managed::DATAGRAM_BYTES,
            size_of::<Output>(),
        ]
        .into_iter()
        .try_fold(0u64, |sum, n| sum.checked_add(u64::try_from(n).ok()?))
        .ok_or_else(|| io::Error::other("capture reservation overflow"))?;
        let structural = count
            .checked_mul(
                size_of::<CreditSlot>()
                    + size_of::<StreamState>()
                    + size_of::<Option<Arc<Output>>>()
                    + size_of::<Option<Arc<Producer>>>(),
            )
            .and_then(|n| {
                labels
                    .iter()
                    .try_fold(n, |sum, label| sum.checked_add(label.len() + size_of::<String>()))
            })
            .and_then(|n| u64::try_from(n).ok())
            .ok_or_else(|| io::Error::other("capture structure overflow"))?;
        let reservation = per_channel
            .checked_mul(u64::try_from(active_count).map_err(io::Error::other)?)
            .and_then(|n| n.checked_add(structural))
            .ok_or_else(|| io::Error::other("capture reservation overflow"))?;
        let charge = Charge::reserve(&self.managed.budget, reservation)?;
        let identity = crate::context::identity(&self.home).map_err(io::Error::other)?;
        let key = owner_key(&self.home, client, identity.fingerprint())?;
        let dir = lock(&self.containers).dir(id);
        let generation = generation(&dir)?;
        let run = RunId(Sha256::digest(id.as_bytes()).into());
        let admitted = Admitted {
            record_bytes: capacity,
            transport_bytes: u64::try_from(transport).map_err(io::Error::other)?,
            handshake_bytes: HANDSHAKE_BYTES,
            datagram_bytes: shards_remote::managed::DATAGRAM_BYTES,
            patience: shards_remote::tls::QUIET,
        };
        let mut staged = Vec::new();
        staged.try_reserve_exact(active_count).map_err(io::Error::other)?;
        for &member in active {
            for channel in [Channel::Stdout, Channel::Stderr] {
                let scope = Scope {
                    run,
                    generation,
                    member,
                    channel,
                };
                let (endpoint, producer, control) =
                    Endpoint::bind(&identity, key, scope, admitted).map_err(Failure::Endpoint)?;
                let kernel = endpoint
                    .kernel_buffer_bytes()
                    .map_err(io::Error::other)?
                    .checked_add(control.kernel_buffer_bytes().map_err(Failure::Endpoint)?)
                    .ok_or_else(|| io::Error::other("capture kernel queue overflow"))?;
                charge.grow(kernel)?;
                staged.push((scope, endpoint, producer, control));
            }
        }
        #[cfg(unix)]
        let (stream, peer) = Stream::pair()?;
        #[cfg(windows)]
        let (stream, peer) = Stream::plain_pair()?;
        charge.grow(
            ipc_buffers(&stream)?
                .checked_add(ipc_buffers(&peer)?)
                .ok_or_else(|| io::Error::other("capture IPC queue overflow"))?,
        )?;
        let charge = Arc::new(charge);
        let mut metadata = private_record(&dir.join(METADATA))?;
        writeln!(
            metadata,
            "shards managed output 1\nserver {}\nowner {}\nrun {}\ngeneration {}\ncapacity {}",
            identity::hex(&identity.fingerprint()),
            identity::hex(&key),
            id,
            generation,
            capacity
        )?;
        for (scope, endpoint, _, _) in &staged {
            writeln!(
                metadata,
                "member {} channel {} address {}",
                scope.member, scope.channel as u8, endpoint.address
            )?;
        }
        metadata.sync_all()?;
        let metadata = Arc::new(Mutex::new(metadata));
        let mut outputs = Vec::new();
        let mut producers = Vec::new();
        let mut endpoints = Vec::new();
        let mut slots = Vec::new();
        let mut states = Vec::new();
        outputs.try_reserve_exact(count).map_err(io::Error::other)?;
        producers.try_reserve_exact(count).map_err(io::Error::other)?;
        endpoints
            .try_reserve_exact(active_count)
            .map_err(io::Error::other)?;
        slots.try_reserve_exact(count).map_err(io::Error::other)?;
        states.try_reserve_exact(count).map_err(io::Error::other)?;
        states.resize(count, StreamState::New);
        slots.resize_with(count, || CreditSlot::Inactive);
        outputs.resize_with(count, || None);
        producers.resize_with(count, || None);
        let credits = Arc::new(Credits {
            state: Mutex::new(Queue {
                slots,
                next: 0,
                life: QueueLife::Open,
            }),
            changed: Condvar::new(),
        });
        for (scope, endpoint, producer, control) in staged {
            let index = route_index(scope.member, scope.channel)?;
            *lock(&credits.state)
                .slots
                .get_mut(index)
                .ok_or_else(|| io::Error::other("active credit index"))? = CreditSlot::Waiting;
            let producer = Arc::new(producer);
            let weak_producer = Arc::downgrade(&producer);
            let weak_credits = Arc::downgrade(&credits);
            let quantum = shards_registry::Quantum::new(capacity, run::MAX_PAYLOAD as usize)
                .ok_or_else(|| io::Error::other("invalid capture quantum bounds"))?;
            let output = Arc::new(Output {
                scope,
                address: endpoint.address,
                control,
                metadata: Arc::clone(&metadata),
                life: Mutex::new(Life::Serving),
                credits: Arc::downgrade(&credits),
                index,
                learning: Mutex::new(Learning {
                    quantum,
                    sample: None,
                    admitted: capacity,
                }),
                charge: Arc::clone(&charge),
            });
            let weak_output = Arc::downgrade(&output);
            producer.on_credit(move || {
                if let (Some(producer), Some(credits)) = (weak_producer.upgrade(), weak_credits.upgrade())
                    && producer.capacity() != 0
                {
                    if let Some(output) = weak_output.upgrade() {
                        output.observed();
                    }
                    credits.ready(index);
                }
            });
            *outputs
                .get_mut(index)
                .ok_or_else(|| io::Error::other("active output index"))? = Some(Arc::clone(&output));
            *producers
                .get_mut(index)
                .ok_or_else(|| io::Error::other("active producer index"))? = Some(producer);
            endpoints.push((endpoint, output));
        }
        let reading = stream.try_clone()?;
        let writing = stream.try_clone()?;
        let owner = Arc::new(Owner {
            outputs,
            stream,
            credits,
            _charge: charge,
        });
        let labels = copy_labels(labels)?;
        let producers = Arc::new(producers);
        {
            let mut runs = lock(&self.managed.runs);
            let entry = runs
                .get_mut(id)
                .filter(|e| matches!(e, Entry::Preparing))
                .ok_or_else(|| io::Error::other("managed run was cancelled"))?;
            *entry = Entry::Serving(Arc::clone(&owner));
        }
        for (endpoint, output) in endpoints {
            std::thread::Builder::new()
                .name("managed-output".into())
                .spawn_scoped(threads, move || {
                    if let Err(e) = endpoint.run() {
                        log(format!("managed output endpoint: {e}"));
                    }
                    output.transport_ended();
                })?;
        }
        let writer_owner = Arc::clone(&owner);
        let writer_producers = Arc::clone(&producers);
        std::thread::Builder::new()
            .name("managed-credit".into())
            .spawn_scoped(threads, move || {
                if let Err(e) = send_credits(writing, &writer_owner, &writer_producers) {
                    log(format!("managed output credits: {e}"));
                    writer_owner.abort();
                }
            })?;
        let reader_owner = Arc::clone(&owner);
        std::thread::Builder::new()
            .name("managed-capture".into())
            .spawn_scoped(threads, move || {
                if let Err(e) = bridge(reading, &reader_owner, &producers, &labels, states) {
                    log(format!("managed output capture: {e}"));
                    reader_owner.abort();
                }
            })?;
        setup.try_reserve(3).map_err(io::Error::other)?;
        setup.push(format!("managed-capacity={}", run::MAX_PAYLOAD).into_bytes());
        setup.push(format!("managed-members={members}").into_bytes());
        let mut activation = Vec::new();
        activation
            .try_reserve_exact(
                run::managed::ACTIVE_KEY
                    .len()
                    .checked_add(
                        active
                            .len()
                            .checked_mul(size_of::<u32>())
                            .ok_or_else(|| io::Error::other("active mask overflow"))?,
                    )
                    .ok_or_else(|| io::Error::other("active mask overflow"))?,
            )
            .map_err(io::Error::other)?;
        activation.extend_from_slice(run::managed::ACTIVE_KEY);
        for member in active {
            activation.extend_from_slice(&member.to_be_bytes());
        }
        setup.push(activation);
        if owner
            .outputs
            .iter()
            .flatten()
            .any(|output| !matches!(*lock(&output.life), Life::Serving))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "managed output ended before actor release",
            )
            .into());
        }
        Ok(peer)
    }
}

fn copy_labels(labels: &[String]) -> io::Result<Vec<String>> {
    let mut out = Vec::new();
    out.try_reserve_exact(labels.len()).map_err(io::Error::other)?;
    for label in labels {
        let mut copy = String::new();
        copy.try_reserve_exact(label.len()).map_err(io::Error::other)?;
        copy.push_str(label);
        out.push(copy);
    }
    Ok(out)
}

fn send_credits(
    mut stream: Stream,
    owner: &Owner,
    producers: &[Option<Arc<Producer>>],
) -> Result<(), Failure> {
    while let Some(action) = owner.credits.next()? {
        match action {
            Action::Grant(index) => {
                let output = owner
                    .outputs
                    .get(index)
                    .and_then(Option::as_ref)
                    .ok_or_else(|| io::Error::other("unknown output channel"))?;
                let producer = producers
                    .get(index)
                    .and_then(Option::as_ref)
                    .ok_or_else(|| io::Error::other("unknown producer"))?;
                let capacity = output.capacity(producer)?;
                {
                    let mut state = lock(&owner.credits.state);
                    let Some(slot @ CreditSlot::Sending) = state.slots.get_mut(index) else {
                        continue;
                    };
                    *slot = CreditSlot::Granted {
                        capacity,
                        began: Instant::now(),
                    };
                }
                let credit = run::managed::Credit {
                    member: output.scope.member,
                    channel: output.scope.channel,
                    capacity: u64::try_from(capacity).map_err(io::Error::other)?,
                }
                .encode()
                .map_err(|e| io::Error::other(e.to_string()))?;
                stream.write_all(&run::header(run::kind::MANAGED_CREDIT, credit.len() as u32))?;
                stream.write_all(&credit)?;
            }
            Action::Submit(index, bytes, after) => {
                let producer = producers
                    .get(index)
                    .and_then(Option::as_ref)
                    .ok_or_else(|| io::Error::other("unknown capture producer"))?;
                let result = producer.submit(bytes);
                let mut state = lock(&owner.credits.state);
                let slot = state
                    .slots
                    .get_mut(index)
                    .ok_or_else(|| io::Error::other("unknown capture credit"))?;
                match result {
                    Ok(()) if matches!(after, After::End) => {
                        *slot = CreditSlot::Ended;
                        drop(state);
                        if let Some(output) = owner.outputs.get(index).and_then(Option::as_ref)
                            && output.finish().is_err()
                        {
                            output.abort();
                        }
                    }
                    Ok(()) => {
                        if matches!(slot, CreditSlot::Sending) {
                            *slot = CreditSlot::Waiting;
                        }
                    }
                    Err(Rejected::Full(bytes)) => {
                        *slot = if producer.capacity() != 0 {
                            CreditSlot::Send(bytes, after)
                        } else {
                            CreditSlot::Captured(bytes, after)
                        };
                    }
                    Err(Rejected::Closed(_) | Rejected::Invalid(_)) => {
                        *slot = CreditSlot::Ended;
                        drop(state);
                        if let Some(output) = owner.outputs.get(index).and_then(Option::as_ref) {
                            output.abort();
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn bridge(
    stream: Stream,
    owner: &Owner,
    producers: &[Option<Arc<Producer>>],
    labels: &[String],
    mut states: Vec<StreamState>,
) -> Result<(), Failure> {
    loop {
        let mut head = [0; run::HEADER];
        read_exact(&stream, &mut head)?;
        let (_, len) = run::parse_header(head)
            .filter(|(kind, len)| *kind == run::kind::MANAGED && *len as usize >= run::managed::HEADER)
            .ok_or_else(|| io::Error::other("invalid capture record header"))?;
        let mut prefix = [0; run::managed::HEADER];
        read_exact(&stream, &mut prefix)?;
        let (member, channel) =
            run::managed::route_prefix(&prefix).map_err(|e| io::Error::other(e.to_string()))?;
        let route = route_index(member, channel)?;
        let (capacity, began) = owner.credits.begin_record(route, len as usize)?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(len as usize).map_err(io::Error::other)?;
        bytes.extend_from_slice(&prefix);
        bytes.resize(len as usize, 0);
        read_exact(
            &stream,
            bytes
                .get_mut(run::managed::HEADER..)
                .ok_or_else(|| io::Error::other("short capture record"))?,
        )?;
        let index = accept(&bytes, labels, &mut states)?;
        if let Some(output) = owner.outputs.get(index).and_then(Option::as_ref)
            && let Event::Data(data) = Record::decode(&bytes)
                .map_err(|e| io::Error::other(e.to_string()))?
                .event
            && bytes.len() == capacity
        {
            let end = output
                .control
                .progress()
                .accepted
                .checked_add(u64::try_from(bytes.len()).map_err(io::Error::other)?)
                .ok_or_else(|| io::Error::other("capture sample offset overflow"))?;
            lock(&output.learning).sample = Some(Sample {
                began,
                end,
                bytes: data.len() as u64,
            });
        }
        let after = if states.get(index).is_some_and(|s| s.terminal()) {
            After::End
        } else {
            After::More
        };
        let producer = producers
            .get(index)
            .and_then(Option::as_ref)
            .ok_or_else(|| io::Error::other("capture producer was not admitted"))?;
        owner.credits.captured(index, bytes, after, producer)?;
        if states
            .iter()
            .enumerate()
            .filter(|(index, _)| owner.outputs.get(*index).is_some_and(Option::is_some))
            .all(|(_, s)| s.terminal())
        {
            return Ok(());
        }
    }
}

fn route_index(member: u32, channel: Channel) -> io::Result<usize> {
    let channel = match channel {
        Channel::Stdout => 0,
        Channel::Stderr => 1,
    };
    usize::try_from(member)
        .ok()
        .and_then(|n| n.checked_mul(2))
        .and_then(|n| n.checked_add(channel))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "capture state index overflow"))
}

fn accept(bytes: &[u8], labels: &[String], states: &mut [StreamState]) -> io::Result<usize> {
    let record =
        Record::decode(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let member = usize::try_from(record.member).map_err(io::Error::other)?;
    let label = labels
        .get(member)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unknown capture member"))?;
    if matches!(record.event, Event::Open(said) if said != label) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "capture label differs from trusted image",
        ));
    }
    let index = route_index(record.member, record.channel)?;
    states
        .get_mut(index)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "capture state was not admitted"))?
        .accept(record)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    Ok(index)
}

fn owner_key(home: &Path, client: &str, local: Fingerprint) -> Result<Fingerprint, Failure> {
    if client.is_empty() {
        return Ok(local);
    }
    let enrolled = crate::context::clients(home)
        .into_iter()
        .find(|c| c.name == client)
        .ok_or(Failure::Auth(Auth::UnknownContext))?;
    if !identity::is_ed25519(&enrolled.key) {
        return Err(Failure::Auth(Auth::InvalidKey));
    }
    Ok(identity::fingerprint(&enrolled.key))
}

fn record_capacity(labels: &[String], active: &[u32]) -> io::Result<usize> {
    active
        .iter()
        .map(|member| {
            labels
                .get(*member as usize)
                .ok_or_else(|| io::Error::other("unknown trusted managed label"))
        })
        .map(|label| {
            let label = label?;
            Record {
                member: 0,
                channel: Channel::Stdout,
                offset: 0,
                event: Event::Open(label),
            }
            .encoded_len()
            .map_err(|e| io::Error::other(e.to_string()))
        })
        .try_fold(run::managed::HEADER + size_of::<i32>(), |most, n| {
            n.map(|n| most.max(n))
        })
}

fn private_record(path: &Path) -> io::Result<File> {
    match shards_ipc::create_private(path) {
        Ok(file) => Ok(file),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            OpenOptions::new().write(true).truncate(true).open(path)
        }
        Err(e) => Err(e),
    }
}

fn generation(dir: &Path) -> io::Result<u64> {
    let path = dir.join(GENERATION);
    let mut file = match shards_ipc::create_private(&path) {
        Ok(mut file) => {
            file.write_all(&1u64.to_be_bytes())?;
            shards_vmm::platform::sync_durable(&file)?;
            shards_vmm::platform::sync_dir(dir)?;
            return Ok(1);
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            OpenOptions::new().read(true).write(true).open(path)?
        }
        Err(e) => return Err(e),
    };
    file.lock()?;
    let result = (|| {
        if file.metadata()?.len() != size_of::<u64>() as u64 {
            return Err(io::Error::other("invalid managed output generation record"));
        }
        let mut bytes = [0; size_of::<u64>()];
        file.read_exact(&mut bytes)?;
        let next = u64::from_be_bytes(bytes)
            .checked_add(1)
            .ok_or_else(|| io::Error::other("managed output generation exhausted"))?;
        file.rewind()?;
        file.write_all(&next.to_be_bytes())?;
        shards_vmm::platform::sync_durable(&file)?;
        Ok(next)
    })();
    let unlocked = file.unlock();
    result.and_then(|value| unlocked.map(|()| value))
}

fn read_exact(stream: &Stream, mut bytes: &mut [u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        let count = shards_ipc::read(stream, bytes)?;
        if count == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        bytes = bytes
            .get_mut(count..)
            .ok_or_else(|| io::Error::other("capture read overflow"))?;
    }
    Ok(())
}

fn ipc_buffers(stream: &Stream) -> io::Result<u64> {
    #[cfg(unix)]
    {
        [libc::SO_SNDBUF, libc::SO_RCVBUF]
            .into_iter()
            .try_fold(0u64, |sum, option| {
                let mut bytes: libc::c_int = 0;
                let mut len = size_of::<libc::c_int>() as libc::socklen_t;
                // SAFETY: this owned private socket and an integer output of its size.
                let rc = unsafe {
                    libc::getsockopt(
                        stream.as_raw_fd(),
                        libc::SOL_SOCKET,
                        option,
                        (&raw mut bytes).cast(),
                        &raw mut len,
                    )
                };
                if rc != 0 {
                    return Err(io::Error::last_os_error());
                }
                if bytes <= 0 || len as usize != size_of::<libc::c_int>() {
                    return Err(io::Error::other("unknown capture socket queue"));
                }
                sum.checked_add(bytes as u64)
                    .ok_or_else(|| io::Error::other("capture socket queue overflow"))
            })
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Pipes::GetNamedPipeInfo;
        let (mut input, mut output) = (0u32, 0u32);
        // SAFETY: an owned pipe and correctly sized output pointers, no flags requested.
        let rc = unsafe {
            GetNamedPipeInfo(
                stream.as_raw_fd(),
                std::ptr::null_mut(),
                &raw mut output,
                &raw mut input,
                std::ptr::null_mut(),
            )
        };
        if rc == 0 {
            return Err(io::Error::last_os_error());
        }
        if input == 0 || output == 0 {
            return Err(io::Error::other("capture pipe queues grow without a known bound"));
        }
        Ok(u64::from(input) + u64::from(output))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn encoded(member: u32, channel: Channel, offset: u64, event: Event<'_>) -> Vec<u8> {
        let record = Record {
            member,
            channel,
            offset,
            event,
        };
        let mut out = vec![0; record.encoded_len().unwrap()];
        record.encode(&mut out).unwrap();
        out
    }

    #[test]
    fn image_identity_and_channel_offsets_are_checked_independently() {
        let labels = vec!["harness first".into(), "agent second".into()];
        let mut states = [StreamState::New; 4];
        assert!(
            accept(
                &encoded(2, Channel::Stdout, 0, Event::Open("agent second")),
                &labels,
                &mut states
            )
            .is_err()
        );
        assert!(
            accept(
                &encoded(1, Channel::Stdout, 0, Event::Open("harness first")),
                &labels,
                &mut states
            )
            .is_err()
        );
        for channel in [Channel::Stdout, Channel::Stderr] {
            assert!(
                accept(
                    &encoded(1, channel, 0, Event::Open("agent second")),
                    &labels,
                    &mut states
                )
                .is_ok()
            );
        }
        assert!(
            accept(
                &encoded(1, Channel::Stdout, 0, Event::Data(b"\0\xff\r\n")),
                &labels,
                &mut states
            )
            .is_ok()
        );
        assert!(
            accept(
                &encoded(1, Channel::Stderr, 4, Event::Data(b"bad offset")),
                &labels,
                &mut states
            )
            .is_err()
        );
        assert!(
            accept(
                &encoded(1, Channel::Stderr, 0, Event::Data(b"stderr")),
                &labels,
                &mut states
            )
            .is_ok()
        );
        assert!(accept(&encoded(1, Channel::Stdout, 4, Event::End), &labels, &mut states).is_ok());
        assert!(
            accept(
                &encoded(1, Channel::Stdout, 4, Event::Data(b"past end")),
                &labels,
                &mut states
            )
            .is_err()
        );
        assert!(!states.get(3).unwrap().terminal());
    }

    #[test]
    fn a_blocked_channel_does_not_take_another_channels_credit() {
        let queue = Credits {
            state: Mutex::new(Queue {
                slots: vec![
                    CreditSlot::Captured(vec![1], After::More),
                    CreditSlot::Available,
                    CreditSlot::Waiting,
                    CreditSlot::Available,
                ],
                next: 0,
                life: QueueLife::Open,
            }),
            changed: Condvar::new(),
        };
        assert!(matches!(queue.next().unwrap(), Some(Action::Grant(1))));
        assert!(matches!(queue.next().unwrap(), Some(Action::Grant(3))));
        queue.ready(0);
        assert!(matches!(
            queue.next().unwrap(),
            Some(Action::Submit(0, _, After::More))
        ));
        queue.close();
        assert!(queue.next().unwrap().is_none());
    }

    #[test]
    fn generation_is_persisted_and_never_wraps_or_reuses_a_bad_record() {
        let dir = shards_testdir::TempDir::new("managed-gen").unwrap();
        assert_eq!(generation(&dir).unwrap(), 1);
        assert_eq!(generation(&dir).unwrap(), 2);
        std::fs::write(dir.join(GENERATION), u64::MAX.to_be_bytes()).unwrap();
        assert!(generation(&dir).is_err());
        std::fs::write(dir.join(GENERATION), [0, 1]).unwrap();
        assert!(generation(&dir).is_err());
    }

    #[test]
    fn an_unknown_remote_owner_never_falls_back_to_the_home_key() {
        let dir = shards_testdir::TempDir::new("managed-key").unwrap();
        assert_eq!(owner_key(&dir, "", [7; 32]).unwrap(), [7; 32]);
        assert!(owner_key(&dir, "missing", [7; 32]).is_err());
    }

    #[test]
    fn the_initial_record_fits_actual_metadata_and_terminal_errno() {
        let labels = vec!["agent a".into(), "harness a longer supplied name".into()];
        let capacity = record_capacity(&labels, &[0, 1]).unwrap();
        for label in &labels {
            assert!(encoded(0, Channel::Stdout, 0, Event::Open(label)).len() <= capacity);
        }
        assert!(encoded(0, Channel::Stderr, 0, Event::Aborted(5)).len() <= capacity);
    }
}
