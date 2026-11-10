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
}

#[derive(Clone, Copy)]
enum AbortState {
    Active,
    Aborted,
}
pub struct Control {
    daemon: shards_ipc::Stream,
    members: Vec<AbortState>,
}
impl Control {
    pub fn supervise(self, signals: &UnixStream, boundary: &UnixStream) -> io::Result<()> {
        let result = self.serve(signals);
        if result.is_err() {
            super::end_guest(boundary);
        }
        result
    }
    pub fn serve(mut self, signals: &UnixStream) -> io::Result<()> {
        let mut read = shards_ipc::Polled(signals);
        loop {
            let mut header = [0; run::HEADER];
            read.read_exact(&mut header)?;
            match run::parse_header(header) {
                Some((kind::MANAGED_CONTROL_END, 0)) => return Ok(()),
                Some((kind::MANAGED_ABORT, n)) if n == managed::ABORT_LEN as u32 => {}
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

pub fn start(spec: &mut run::Spec, given: Given) -> Result<Control, String> {
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
    let control = Control {
        daemon: given.control,
        members: aborts,
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
                .and_then(|guest| relay(guest, given.stream, given.capacity, states));
            if let Err(e) = result {
                shards_vmm::debug!("managed capture aborted: {e}");
            }
        })
    {
        super::pending().retain(|w| !super::same_token(&w.token, &token));
        return Err(format!("managed capture worker: {e}"));
    }
    spec.setup.push(entry);
    Ok(control)
}

fn relay(
    guest: UnixStream,
    owner: shards_ipc::Stream,
    capacity: usize,
    streams: Vec<[Flow; 2]>,
) -> io::Result<()> {
    let ledger = Ledger {
        flows: Mutex::new(Flows {
            owner: Owner::Running,
            streams,
        }),
        changed: Condvar::new(),
    };
    std::thread::scope(|scope| {
        let credit = std::thread::Builder::new()
            .name("managed-credit".into())
            .spawn_scoped(scope, || {
                let result = credits(&guest, &owner, capacity, &ledger);
                ledger.stop();
                super::end_guest(&guest);
                shards_ipc::end(&owner);
                result
            })?;
        let result = records(&guest, &owner, capacity, &ledger);
        ledger.stop();
        shards_ipc::end(&owner);
        super::end_guest(&guest);
        let credit = credit
            .join()
            .map_err(|_| io::Error::other("managed credit worker panicked"))?;
        match result {
            Ok(()) => Ok(()),
            Err(e) => match credit {
                Err(credit) if e.kind() == io::ErrorKind::UnexpectedEof => Err(credit),
                _ => Err(e),
            },
        }
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
        assert!(worker.join().unwrap().is_err());
    }

    #[test]
    fn explicit_control_end_preserves_layer_connection_but_unannounced_eof_ends_it() {
        for graceful in [true, false] {
            let (signals, mut peer) = UnixStream::pair().unwrap();
            let (boundary, mut layer) = UnixStream::pair().unwrap();
            let (daemon, _owner) = shards_ipc::Stream::pair().unwrap();
            let control = Control {
                daemon,
                members: vec![AbortState::Active],
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
                assert!(matches!(layer.read(&mut [0]), Ok(0)));
            }
        }
    }

    #[test]
    fn trusted_abort_preserves_tenant_errno_and_coalesces_duplicate() {
        let (signals, mut peer) = UnixStream::pair().unwrap();
        let (daemon, owner) = shards_ipc::Stream::pair().unwrap();
        let control = Control {
            daemon,
            members: vec![AbortState::Active; 3],
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
}
