//! Private managed-member output; main stdcopy and lifecycle control use other connections.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use shards_abi::run::{
    self, kind,
    managed::{self, Channel, Event, Record, StreamState},
};

#[derive(Clone, Copy, Debug)]
pub enum OutputControl {
    Active,
    Pending(i32),
    Reported,
}

enum ControlState {
    Open,
    Ending,
    Ended,
}
pub struct ControlQueue {
    bytes: Vec<u8>,
    cursor: usize,
    state: ControlState,
}
impl ControlQueue {
    pub fn prepare(members: usize) -> Result<Self, String> {
        let size = Some(members)
            .and_then(|n| n.checked_mul(run::HEADER + managed::ABORT_LEN))
            .and_then(|n| n.checked_add(run::HEADER))
            .ok_or("managed terminal reservation overflow")?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(size).map_err(|e| e.to_string())?;
        Ok(Self {
            bytes,
            cursor: 0,
            state: ControlState::Open,
        })
    }
    pub fn push(&mut self, abort: managed::Abort) -> io::Result<()> {
        if !matches!(self.state, ControlState::Open) {
            return Err(invalid("managed control scope already ending"));
        }
        let payload = abort.encode().map_err(invalid)?;
        let end = self
            .bytes
            .len()
            .checked_add(run::HEADER + managed::ABORT_LEN)
            .ok_or_else(|| invalid("managed terminal reservation overflow"))?;
        if end > self.bytes.capacity() {
            return Err(invalid("managed terminal reservation exhausted"));
        }
        self.bytes
            .extend_from_slice(&run::header(kind::MANAGED_ABORT, managed::ABORT_LEN as u32));
        self.bytes.extend_from_slice(&payload);
        Ok(())
    }
    pub fn end(&mut self) -> io::Result<()> {
        if !matches!(self.state, ControlState::Open) {
            return Ok(());
        }
        let end = self
            .bytes
            .len()
            .checked_add(run::HEADER)
            .ok_or_else(|| invalid("managed terminal reservation overflow"))?;
        if end > self.bytes.capacity() {
            return Err(invalid("managed terminal reservation exhausted"));
        }
        self.bytes
            .extend_from_slice(&run::header(kind::MANAGED_CONTROL_END, 0));
        self.state = ControlState::Ending;
        Ok(())
    }
    pub fn ended(&self) -> bool {
        matches!(self.state, ControlState::Ended)
    }
    pub fn pending(&self) -> bool {
        self.cursor < self.bytes.len()
    }
    pub fn write(&mut self, fd: RawFd) -> io::Result<()> {
        let bytes = self
            .bytes
            .get(self.cursor..)
            .ok_or_else(|| invalid("managed terminal cursor"))?;
        self.cursor = self
            .cursor
            .checked_add(crate::run::write(fd, bytes)?)
            .ok_or_else(|| invalid("managed terminal cursor overflow"))?;
        if self.cursor == self.bytes.len() {
            self.bytes.clear();
            self.cursor = 0;
            if matches!(self.state, ControlState::Ending) {
                self.state = ControlState::Ended;
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct Pipe {
    fd: Option<OwnedFd>,
    state: StreamState,
}

impl Pipe {
    pub fn make() -> io::Result<(Self, OwnedFd)> {
        let mut fds = [-1; 2];
        // SAFETY: pipe2 writes exactly the two descriptors in fds.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let [read, write] = fds;
        // SAFETY: both descriptors are fresh and uniquely owned.
        let (read, write) = unsafe { (OwnedFd::from_raw_fd(read), OwnedFd::from_raw_fd(write)) };
        // SAFETY: fcntl operates on the fresh pipe read end.
        if unsafe { libc::fcntl(read.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((
            Self {
                fd: Some(read),
                state: StreamState::New,
            },
            write,
        ))
    }

    pub fn fd(&self) -> Option<RawFd> {
        self.fd.as_ref().map(AsRawFd::as_raw_fd)
    }
    pub fn terminal(&self) -> bool {
        self.state.terminal()
    }
    pub fn abort(&mut self) {
        if self.state.terminal() {
            return;
        }
        let offset = match self.state {
            StreamState::Open { offset } => offset,
            _ => 0,
        };
        self.state = StreamState::Aborted {
            offset,
            errno: libc::ECANCELED,
        };
        self.fd = None;
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CreditState {
    Inactive,
    Waiting,
    Granted(usize),
    InFlight,
    Closed,
}
enum Output {
    Idle,
    Sending { written: usize },
}
enum State {
    Hello {
        bytes: [u8; run::HEADER + run::TOKEN],
        written: usize,
    },
    Running(Output),
}

pub struct Capture {
    conn: File,
    capacity: usize,
    state: State,
    credits: Vec<[CreditState; 2]>,
    incoming: [u8; run::HEADER + managed::CREDIT_LEN],
    received: usize,
    outgoing: Vec<u8>,
    buffer_owner: Option<(u32, Channel)>,
    next: usize,
}

fn invalid(why: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why.to_string())
}

impl Capture {
    pub fn prepare(
        setup: &[Vec<u8>],
        labels: impl Iterator<Item = (u32, impl AsRef<str>)>,
    ) -> Result<Option<Self>, String> {
        let mut labels = labels.peekable();
        let active = managed::active(setup).map_err(|e| e.to_string())?;
        if labels.peek().is_none() {
            return if active.is_none() {
                Ok(None)
            } else {
                Err("managed activation does not match image".into())
            };
        }
        let capacity = managed::capacity(setup)
            .map_err(|e| e.to_string())?
            .ok_or("managed output was not admitted")?;
        let members = managed::members(setup)
            .map_err(|e| e.to_string())?
            .ok_or("managed output members were not admitted")?;
        let active = active.ok_or("managed activation was not admitted")?;
        let mut expected = active.iter();
        for (id, label) in labels {
            let member = id
                .checked_sub(shards_abi::DOMAIN_FIRST_ID)
                .ok_or("managed member identity precedes namespace")?;
            if expected.next().map(|id| u32::from_be_bytes(*id)) != Some(member) {
                return Err("managed activation does not match image".into());
            }
            if label
                .as_ref()
                .len()
                .checked_add(managed::HEADER)
                .is_none_or(|n| n > capacity)
            {
                return Err("managed admission cannot hold label".into());
            }
        }
        if expected.next().is_some() {
            return Err("managed activation does not match image".into());
        }
        let token = managed::token(setup)
            .map_err(|e| e.to_string())?
            .ok_or("managed output has no authenticated connection")?;
        let mut credits = Vec::new();
        let count = usize::try_from(members).map_err(|e| e.to_string())?;
        credits.try_reserve_exact(count).map_err(|e| e.to_string())?;
        credits.resize(count, [CreditState::Inactive; 2]);
        for member in active {
            let slot = credits
                .get_mut(usize::try_from(u32::from_be_bytes(*member)).map_err(|e| e.to_string())?)
                .ok_or("managed activation exceeds member bound")?;
            *slot = [CreditState::Waiting; 2];
        }
        let conn =
            crate::run::dial(run::EXEC_PORT, false).map_err(|e| format!("managed output connection: {e}"))?;
        let mut hello = [0; run::HEADER + run::TOKEN];
        let (header, bytes) = hello
            .split_first_chunk_mut::<{ run::HEADER }>()
            .ok_or("managed hello header")?;
        *header = run::header(kind::HELLO, run::TOKEN as u32);
        bytes.copy_from_slice(&token);
        Ok(Some(Self {
            conn,
            capacity,
            state: State::Hello {
                bytes: hello,
                written: 0,
            },
            credits,
            incoming: [0; run::HEADER + managed::CREDIT_LEN],
            received: 0,
            outgoing: Vec::new(),
            buffer_owner: None,
            next: 0,
        }))
    }

    pub fn fd(&self) -> RawFd {
        self.conn.as_raw_fd()
    }
    pub fn events(&self) -> libc::c_short {
        match self.state {
            State::Hello { .. } => libc::POLLOUT,
            State::Running(Output::Sending { .. }) => libc::POLLIN | libc::POLLOUT,
            State::Running(Output::Idle) => libc::POLLIN,
        }
    }
    fn allowance(&self, member: u32, channel: Channel) -> Option<usize> {
        let [out, err] = self.credits.get(usize::try_from(member).ok()?)?;
        match *match channel {
            Channel::Stdout => out,
            Channel::Stderr => err,
        } {
            CreditState::Granted(capacity) => Some(capacity),
            _ => None,
        }
    }
    pub fn ready(&self, member: u32, channel: Channel) -> bool {
        matches!(self.state, State::Running(Output::Idle)) && self.allowance(member, channel).is_some()
    }
    pub fn next(&self) -> usize {
        self.next
    }
    pub fn drained(&self) -> bool {
        matches!(self.state, State::Running(Output::Idle))
    }

    fn take_credit(&mut self, member: u32, channel: Channel, terminal: bool) -> io::Result<()> {
        let pair = self
            .credits
            .get_mut(usize::try_from(member).map_err(invalid)?)
            .ok_or_else(|| invalid("managed member credit"))?;
        let [out, err] = pair;
        let slot = match channel {
            Channel::Stdout => out,
            Channel::Stderr => err,
        };
        if !matches!(*slot, CreditState::Granted(_)) {
            return Err(invalid("managed channel has no admitted credit"));
        }
        *slot = if terminal {
            CreditState::Closed
        } else {
            CreditState::InFlight
        };
        Ok(())
    }

    pub fn progress(&mut self) -> io::Result<()> {
        let fd = self.fd();
        if let State::Hello { bytes, written } = &mut self.state {
            let pending = bytes
                .get(*written..)
                .ok_or_else(|| invalid("managed hello cursor"))?;
            *written = written
                .checked_add(crate::run::write(fd, pending)?)
                .ok_or_else(|| invalid("managed hello cursor overflow"))?;
            if *written == bytes.len() {
                self.state = State::Running(Output::Idle)
            }
            return Ok(());
        }
        if let State::Running(Output::Sending { written }) = &mut self.state {
            let pending = self
                .outgoing
                .get(*written..)
                .ok_or_else(|| invalid("managed output cursor"))?;
            *written = written
                .checked_add(crate::run::write(fd, pending)?)
                .ok_or_else(|| invalid("managed output cursor overflow"))?;
            if *written == self.outgoing.len() {
                self.outgoing.clear();
                self.state = State::Running(Output::Idle);
                if let Some((member, channel)) = self.buffer_owner {
                    let [out, err] = self
                        .credits
                        .get_mut(usize::try_from(member).map_err(invalid)?)
                        .ok_or_else(|| invalid("managed buffer owner"))?;
                    let slot = match channel {
                        Channel::Stdout => out,
                        Channel::Stderr => err,
                    };
                    match slot {
                        CreditState::InFlight => *slot = CreditState::Waiting,
                        CreditState::Closed => {
                            self.outgoing = Vec::new();
                            self.buffer_owner = None;
                        }
                        _ => return Err(invalid("managed buffer credit state")),
                    }
                }
            }
        }
        let pending = self
            .incoming
            .get_mut(self.received..)
            .ok_or_else(|| invalid("managed credit cursor"))?;
        let n = match crate::run::read(fd, pending) {
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {
                return Ok(());
            }
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            got => got?,
        };
        self.received = self
            .received
            .checked_add(n)
            .ok_or_else(|| invalid("managed credit cursor overflow"))?;
        if self.received == self.incoming.len() {
            let (header, payload) = self
                .incoming
                .split_first_chunk::<{ run::HEADER }>()
                .ok_or_else(|| invalid("managed credit header"))?;
            if run::parse_header(*header) != Some((kind::MANAGED_CREDIT, managed::CREDIT_LEN as u32)) {
                return Err(invalid("managed credit frame"));
            }
            let credit = managed::Credit::decode(payload).map_err(invalid)?;
            let capacity = usize::try_from(credit.capacity).map_err(invalid)?;
            if capacity > self.capacity {
                return Err(invalid("managed credit changes admission"));
            }
            let pair = self
                .credits
                .get_mut(usize::try_from(credit.member).map_err(invalid)?)
                .ok_or_else(|| invalid("managed member credit"))?;
            let [out, err] = pair;
            let slot = match credit.channel {
                Channel::Stdout => out,
                Channel::Stderr => err,
            };
            if *slot != CreditState::Waiting {
                return Err(invalid("duplicate or closed managed channel credit"));
            }
            *slot = CreditState::Granted(capacity);
            if self.buffer_owner == Some((credit.member, credit.channel))
                && self.drained()
                && self.outgoing.capacity()
                    > capacity
                        .checked_add(run::HEADER)
                        .ok_or_else(|| invalid("managed capacity overflow"))?
            {
                self.outgoing = Vec::new();
                self.buffer_owner = None;
            }
            self.received = 0;
        }
        Ok(())
    }

    pub fn advance(&mut self, index: usize, channel: Channel) -> io::Result<()> {
        self.next = index
            .checked_mul(2)
            .and_then(|n| n.checked_add(if channel == Channel::Stdout { 1 } else { 2 }))
            .ok_or_else(|| invalid("managed scheduler cursor overflow"))?;
        Ok(())
    }
    pub fn abort_member(&mut self, member: u32) -> io::Result<()> {
        let pair = self
            .credits
            .get_mut(usize::try_from(member).map_err(invalid)?)
            .ok_or_else(|| invalid("managed abort member"))?;
        *pair = [CreditState::Closed; 2];
        if self.buffer_owner.is_some_and(|(owner, _)| owner == member) && self.drained() {
            self.outgoing = Vec::new();
            self.buffer_owner = None;
        }
        Ok(())
    }

    fn space(&mut self, payload: usize, capacity: usize, owner: (u32, Channel)) -> io::Result<()> {
        let record = managed::HEADER
            .checked_add(payload)
            .filter(|n| *n <= capacity)
            .ok_or_else(|| invalid("managed record exceeds admission"))?;
        let n = run::HEADER
            .checked_add(record)
            .ok_or_else(|| invalid("managed frame length overflow"))?;
        if self.buffer_owner != Some(owner)
            || self.outgoing.capacity()
                > capacity
                    .checked_add(run::HEADER)
                    .ok_or_else(|| invalid("managed capacity overflow"))?
        {
            self.outgoing = Vec::new();
            self.buffer_owner = Some(owner);
        }
        if self.outgoing.capacity() < n {
            self.outgoing
                .try_reserve_exact(n.saturating_sub(self.outgoing.len()))
                .map_err(io::Error::other)?;
        }
        if self.outgoing.capacity()
            > capacity
                .checked_add(run::HEADER)
                .ok_or_else(|| invalid("managed capacity overflow"))?
        {
            return Err(invalid("managed allocation exceeds admission"));
        }
        self.outgoing.resize(n, 0);
        Ok(())
    }

    fn frame(&mut self, record: Record<'_>) -> io::Result<()> {
        let n = record.encoded_len().map_err(invalid)?;
        let capacity = self
            .allowance(record.member, record.channel)
            .ok_or_else(|| invalid("managed channel credit"))?;
        self.space(
            n.checked_sub(managed::HEADER)
                .ok_or_else(|| invalid("managed header length"))?,
            capacity,
            (record.member, record.channel),
        )?;
        let (header, payload) = self
            .outgoing
            .split_first_chunk_mut::<{ run::HEADER }>()
            .ok_or_else(|| invalid("managed frame header"))?;
        *header = run::header(kind::MANAGED, u32::try_from(n).map_err(invalid)?);
        record.encode(payload).map_err(invalid)?;
        self.take_credit(
            record.member,
            record.channel,
            matches!(record.event, Event::End | Event::Aborted(_)),
        )?;
        self.state = State::Running(Output::Sending { written: 0 });
        Ok(())
    }

    pub fn open(&mut self, member: u32, channel: Channel, label: &str, pipe: &mut Pipe) -> io::Result<bool> {
        if !self.ready(member, channel) || pipe.state != StreamState::New {
            return Ok(false);
        }
        let record = Record {
            member,
            channel,
            offset: 0,
            event: Event::Open(label),
        };
        self.frame(record)?;
        pipe.state.accept(record).map_err(invalid)?;
        Ok(true)
    }

    pub fn read(&mut self, member: u32, channel: Channel, pipe: &mut Pipe) -> io::Result<()> {
        if !self.ready(member, channel) {
            return Ok(());
        }
        let (Some(fd), StreamState::Open { offset }) = (pipe.fd(), pipe.state) else {
            return Err(invalid("managed pipe read state"));
        };
        let mut available: libc::c_int = 0;
        // SAFETY: FIONREAD writes one c_int for this owned pipe descriptor.
        if unsafe { libc::ioctl(fd, libc::FIONREAD, &raw mut available) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let available = usize::try_from(available).map_err(invalid)?.max(1);
        let capacity = self
            .allowance(member, channel)
            .ok_or_else(|| invalid("managed channel credit"))?;
        let available = available.min(
            capacity
                .checked_sub(managed::HEADER)
                .ok_or_else(|| invalid("managed payload capacity"))?,
        );
        self.space(available, capacity, (member, channel))?;
        let body_at = run::HEADER + managed::HEADER;
        let data = self
            .outgoing
            .get_mut(body_at..)
            .ok_or_else(|| invalid("managed data buffer"))?;
        let (event, len) = match crate::run::read(fd, data) {
            Ok(0) => (Event::End, 0),
            Ok(n) => (
                Event::Data(data.get(..n).ok_or_else(|| invalid("managed read past buffer"))?),
                n,
            ),
            Err(e) if matches!(e.kind(), io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) => {
                self.outgoing.clear();
                return Ok(());
            }
            Err(e) => {
                let errno = e.raw_os_error().filter(|n| *n > 0).ok_or(e)?;
                self.outgoing.clear();
                let record = Record {
                    member,
                    channel,
                    offset,
                    event: Event::Aborted(errno),
                };
                self.frame(record)?;
                pipe.state.accept(record).map_err(invalid)?;
                pipe.fd = None;
                return Ok(());
            }
        };
        let record = Record {
            member,
            channel,
            offset,
            event,
        };
        let inner = record.header().map_err(invalid)?;
        pipe.state.accept(record).map_err(invalid)?;
        self.take_credit(member, channel, pipe.terminal())?;
        if pipe.state.terminal() {
            pipe.fd = None
        }
        self.outgoing.truncate(
            body_at
                .checked_add(len)
                .ok_or_else(|| invalid("managed output overflow"))?,
        );
        let (header, rest) = self
            .outgoing
            .split_first_chunk_mut::<{ run::HEADER }>()
            .ok_or_else(|| invalid("managed frame header"))?;
        *header = run::header(
            kind::MANAGED,
            u32::try_from(managed::HEADER + len).map_err(invalid)?,
        );
        *rest
            .first_chunk_mut::<{ managed::HEADER }>()
            .ok_or_else(|| invalid("managed record header"))? = inner;
        self.state = State::Running(Output::Sending { written: 0 });
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    fn fixture(active: usize, capacity: usize) -> (Capture, UnixStream) {
        let (conn, peer) = UnixStream::pair().unwrap();
        conn.set_nonblocking(true).unwrap();
        let capture = Capture {
            conn: File::from(OwnedFd::from(conn)),
            capacity,
            state: State::Running(Output::Idle),
            credits: vec![[CreditState::Waiting; 2]; active],
            incoming: [0; run::HEADER + managed::CREDIT_LEN],
            received: 0,
            outgoing: Vec::new(),
            buffer_owner: None,
            next: 0,
        };
        (capture, peer)
    }
    fn grant(capture: &mut Capture, peer: &mut UnixStream, member: u32, channel: Channel, capacity: usize) {
        peer.write_all(&run::header(kind::MANAGED_CREDIT, managed::CREDIT_LEN as u32))
            .unwrap();
        peer.write_all(
            &managed::Credit {
                member,
                channel,
                capacity: capacity as u64,
            }
            .encode()
            .unwrap(),
        )
        .unwrap();
        capture.progress().unwrap();
    }
    fn receive(capture: &mut Capture, peer: &mut UnixStream) -> Vec<u8> {
        capture.progress().unwrap();
        let mut header = [0; run::HEADER];
        peer.read_exact(&mut header).unwrap();
        let (which, len) = run::parse_header(header).unwrap();
        assert_eq!(which, kind::MANAGED);
        let mut bytes = vec![0; len as usize];
        peer.read_exact(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn allocation_refusal_keeps_its_resource_cause() {
        const CHILD: &str = "SHARDS_CAPTURE_ALLOCATION_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let capacity = usize::try_from(run::MAX_PAYLOAD).unwrap();
            let (mut capture, _peer) = fixture(1, capacity);
            let core = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: only this test child's crash artifact is disabled.
            assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_CORE, &raw const core) }, 0);
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: getrlimit writes this owned structure; only this test child is affected.
            assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_AS, &raw mut limit) }, 0);
            limit.rlim_cur = 0;
            // SAFETY: setrlimit changes only this test child's soft address-space limit.
            assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_AS, &raw const limit) }, 0);
            let refused = capture.space(capacity - managed::HEADER, capacity, (0, Channel::Stderr));
            let code = match refused {
                Err(error) if error.raw_os_error() == Some(libc::ENOMEM) => 0,
                Err(_) => 2,
                Ok(()) => 3,
            };
            // SAFETY: the isolated child returns its result without allocating test diagnostics.
            unsafe { libc::_exit(code) }
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "capture::tests::allocation_refusal_keeps_its_resource_cause",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("MALLOC_ARENA_MAX", "1")
            .env("MALLOC_MMAP_THRESHOLD_", "0")
            .status()
            .unwrap();
        assert!(status.success(), "allocation refusal result: {status}");
    }

    #[test]
    fn native_pipes_preserve_binary_channels_offsets_and_actual_eof() {
        let payload = b"\0\xff\x80\n\x1b[2Jno-newline";
        let label = "harness drive";
        let capacity = managed::HEADER + payload.len().max(label.len()).max(size_of::<i32>());
        let (mut capture, mut peer) = fixture(1, capacity);
        for channel in [Channel::Stdout, Channel::Stderr] {
            let (mut pipe, writer) = Pipe::make().unwrap();
            let mut writer = File::from(writer);
            writer.write_all(payload).unwrap();
            drop(writer);
            grant(&mut capture, &mut peer, 0, channel, capacity);
            assert!(capture.open(0, channel, label, &mut pipe).unwrap());
            let bytes = receive(&mut capture, &mut peer);
            assert_eq!(Record::decode(&bytes).unwrap().event, Event::Open(label));
            grant(&mut capture, &mut peer, 0, channel, capacity);
            capture.read(0, channel, &mut pipe).unwrap();
            let bytes = receive(&mut capture, &mut peer);
            assert_eq!(
                Record::decode(&bytes).unwrap(),
                Record {
                    member: 0,
                    channel,
                    offset: 0,
                    event: Event::Data(payload)
                }
            );
            grant(&mut capture, &mut peer, 0, channel, capacity);
            capture.read(0, channel, &mut pipe).unwrap();
            let bytes = receive(&mut capture, &mut peer);
            assert_eq!(
                Record::decode(&bytes).unwrap(),
                Record {
                    member: 0,
                    channel,
                    offset: payload.len() as u64,
                    event: Event::End
                }
            );
            assert!(pipe.terminal());
            assert!(pipe.fd().is_none());
        }
    }

    #[test]
    fn sparse_sibling_and_stderr_credit_do_not_depend_on_noisy_stdout_ack() {
        let capacity = managed::HEADER + b"agent quiet".len();
        let (mut capture, mut peer) = fixture(3, capacity);
        capture.credits[1] = [CreditState::Inactive; 2];
        let (mut noisy, _writer) = Pipe::make().unwrap();
        grant(&mut capture, &mut peer, 0, Channel::Stdout, capacity);
        capture
            .open(0, Channel::Stdout, "agent noisy", &mut noisy)
            .unwrap();
        receive(&mut capture, &mut peer);
        assert!(!capture.ready(0, Channel::Stdout));
        let (mut quiet, writer) = Pipe::make().unwrap();
        File::from(writer).write_all(b"q").unwrap();
        grant(&mut capture, &mut peer, 2, Channel::Stderr, capacity);
        assert!(capture.ready(2, Channel::Stderr));
        capture
            .open(2, Channel::Stderr, "agent quiet", &mut quiet)
            .unwrap();
        receive(&mut capture, &mut peer);
        grant(&mut capture, &mut peer, 2, Channel::Stderr, capacity);
        capture.read(2, Channel::Stderr, &mut quiet).unwrap();
        let bytes = receive(&mut capture, &mut peer);
        assert_eq!(
            Record::decode(&bytes).unwrap(),
            Record {
                member: 2,
                channel: Channel::Stderr,
                offset: 0,
                event: Event::Data(b"q")
            }
        );
        capture.advance(1, Channel::Stderr).unwrap();
        assert_eq!(capture.next(), 4);
    }

    #[test]
    fn reserved_control_orders_abort_then_explicit_end_without_output_credit() {
        let (conn, mut peer) = UnixStream::pair().unwrap();
        let mut control = ControlQueue::prepare(1).unwrap();
        let abort = managed::Abort {
            member: 2,
            errno: libc::EIO,
        };
        control.push(abort).unwrap();
        control.end().unwrap();
        while control.pending() {
            control.write(conn.as_raw_fd()).unwrap();
        }
        assert!(control.ended());
        assert!(control.push(abort).is_err());
        let mut header = [0; run::HEADER];
        peer.read_exact(&mut header).unwrap();
        assert_eq!(
            run::parse_header(header),
            Some((kind::MANAGED_ABORT, managed::ABORT_LEN as u32))
        );
        let mut payload = [0; managed::ABORT_LEN];
        peer.read_exact(&mut payload).unwrap();
        assert_eq!(managed::Abort::decode(&payload).unwrap(), abort);
        peer.read_exact(&mut header).unwrap();
        assert_eq!(run::parse_header(header), Some((kind::MANAGED_CONTROL_END, 0)));
    }
}
