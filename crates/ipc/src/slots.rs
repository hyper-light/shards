//! Requests through the home (docs/design/architecture.md D185): a client and its daemon
//! meet in slots the daemon makes ahead, which `$SHARDS_HOME/requests/` names, each used
//! once. On Unix a slot is FIFOs in that directory (fifo.rs); on Windows, a named pipe's
//! instance, and pipes for its streams, under a name a file there gives (named.rs, D195).
//! Either way the daemon keeps [`OFFERED`] slots of each kind offered, without streams and
//! with; a client takes one, which no other can then take, waiting where all are taken; it
//! says which of its slot's streams it opened, what each descriptor its request stands for
//! is ([`fds`]), and its request; and each side's end, its exit or its kill, ends the
//! other's reads, which is how each learns the other has gone. A slot no client wrote to is
//! ended by its own stop ([`Stopper`]), never by a byte where a client may be writing.

use std::io::{self, Read, Write};

use crate::MAX_PAYLOAD;

#[cfg(unix)]
mod fifo;
#[cfg(unix)]
pub use fifo::{Client, Opened, Reader, Requests, Sender, Session, Slot, Stopper, served, take, unoffer};
#[cfg(windows)]
mod named;
#[cfg(windows)]
pub use named::{
    Client, Opened, Reader, Requests, Sender, Session, Slot, Stopper, read_frame_until, served, take, unoffer,
};

/// What carries a slot's messages, each way: a FIFO on Unix (`say`'s, `answer`'s); on
/// Windows the slot's instance of its list's pipe, which carries both.
#[cfg(unix)]
pub type Line = std::fs::File;
#[cfg(windows)]
pub type Line = crate::Pipe;

/// Two lines joined, what is written to the second read from the first, as a session's
/// `say` and `answer` are joined to its client's ends: a pipe on Unix; on Windows a pair
/// of shards' own pipes, either way, each holding what a slot's line holds.
pub fn joined() -> io::Result<(Line, Line)> {
    #[cfg(unix)]
    {
        let (reader, writer) = std::io::pipe()?;
        Ok((
            std::fs::File::from(std::os::fd::OwnedFd::from(reader)),
            std::fs::File::from(std::os::fd::OwnedFd::from(writer)),
        ))
    }
    #[cfg(windows)]
    {
        let (one, other) = crate::pipe::pair(crate::pipe::LINE_BUFFER, false)?;
        Ok((crate::Pipe::from_handle(one), crate::Pipe::from_handle(other)))
    }
}

/// The home's directory of slots.
pub const DIR: &str = "requests";

/// What offers the slots, without streams and with: on Unix the FIFOs a client takes a
/// slot's record from, on Windows the files naming the pipes a client takes an instance of.
const FREE: &str = "free";
const FREE_STREAMS: &str = "free-streams";

/// The slots a daemon keeps offered of each kind: one for the next client, and one for a
/// client that comes as the first's replacement is made (PM M255).
pub const OFFERED: usize = 2;

/// A slot's number as its client learns it: 15 decimal digits, and a newline. On Unix a
/// record on `free`; on Windows the first the daemon says on the slot's instance.
pub const RECORD: usize = 16;

/// The streams a client opens of its slot, as it says them in its first byte.
pub mod streams {
    pub const IN: u8 = 1;
    pub const OUT: u8 = 2;
    pub const ERR: u8 = 4;
}

/// What each descriptor a request stands for is, in the order a socket's message would
/// carry them: one of its slot's streams, or none (`/dev/null`).
pub mod fds {
    pub const IN: u8 = 0;
    pub const OUT: u8 = 1;
    pub const ERR: u8 = 2;
    pub const NULL: u8 = 3;
    /// The most a request names: a run's stdin, stdout and stderr, and room to spare.
    pub const MAX: usize = 8;
}

/// A message's header: its kind, then its payload's length (big-endian u32).
const HEADER: usize = 5;

/// One message of a slot's: what a socket's message is, without descriptors.
#[derive(Debug, PartialEq, Eq)]
pub struct Frame {
    pub kind: u8,
    pub payload: Vec<u8>,
}

/// Writes one message to `to`.
pub fn write_frame(mut to: &Line, kind: u8, payload: &[u8]) -> io::Result<()> {
    to.write_all(&frame(kind, payload)?)
}

/// One message's bytes, as [`write_frame`] writes them: for a writer that writes them a
/// piece at a time.
pub fn frame(kind: u8, payload: &[u8]) -> io::Result<Vec<u8>> {
    if payload.len() > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "message payload too large",
        ));
    }
    let len = u32::try_from(payload.len()).map_err(|_| io::Error::other("payload length"))?;
    let mut bytes = Vec::with_capacity(HEADER + payload.len());
    bytes.push(kind);
    bytes.extend_from_slice(&len.to_be_bytes());
    bytes.extend_from_slice(payload);
    Ok(bytes)
}

/// Reads one message from `from`: `None` where its writers ended between messages.
pub fn read_frame(from: &Line) -> io::Result<Option<Frame>> {
    #[cfg(unix)]
    {
        frame_from(&mut fifo::Reader(from))
    }
    #[cfg(windows)]
    {
        frame_from(&mut { from })
    }
}

/// [`read_frame`], of what `from` reads.
fn frame_from(from: &mut impl Read) -> io::Result<Option<Frame>> {
    let mut header = [0u8; HEADER];
    let got = loop {
        match from.read(&mut header) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            got => break got?,
        }
    };
    if got == 0 {
        return Ok(None);
    }
    from.read_exact(header.get_mut(got..).unwrap_or_default())?;
    let [kind, l0, l1, l2, l3] = header;
    let len = u32::from_be_bytes([l0, l1, l2, l3]) as usize;
    if len > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message payload too large",
        ));
    }
    let mut payload = vec![0u8; len];
    from.read_exact(&mut payload)?;
    Ok(Some(Frame { kind, payload }))
}

/// A list's place among `Requests`' two: those without streams, and those with.
fn list(streams: bool) -> usize {
    usize::from(streams)
}

/// A slot's number, from its record.
fn number(record: &[u8; RECORD]) -> io::Result<u64> {
    std::str::from_utf8(record)
        .ok()
        .and_then(|r| r.strip_suffix('\n'))
        .and_then(|r| r.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "a record that names no slot"))
}

/// A slot's record: its number, as [`number`] reads it.
fn record(n: u64) -> io::Result<String> {
    let record = format!("{n:015}\n");
    if record.len() != RECORD {
        return Err(io::Error::other("a slot's number past 15 digits"));
    }
    Ok(record)
}

/// What a slot's wait came to.
#[derive(Debug)]
pub enum Waited {
    /// A client took the slot and began its request: the rest is [`Opened::request`]'s.
    Opened(Opened),
    /// The daemon ended the wait ([`Stopper::stop`]).
    Stopped,
    /// No client came, and the slot ended.
    Gone,
}

/// What reading the rest of a client's request came to ([`Opened::request_within`]).
#[derive(Debug)]
pub enum Requested {
    /// All of it.
    Asked(Asked),
    /// The client ended before it was whole.
    Gone,
    /// Not whole by its deadline: the session, its client to let go of.
    Late(Session),
    /// Its daemon ended its clients first: the session, for the daemon to answer.
    Ended(Session),
}

/// A client's whole request: the slot's session, what each descriptor the request stands
/// for is ([`fds`]), in order, and the request.
#[derive(Debug)]
pub struct Asked {
    pub session: Session,
    pub fds: Vec<u8>,
    pub request: Frame,
}

/// What a client's ask for a slot came to.
#[derive(Debug)]
pub enum Taken {
    Slot(Client),
    /// No daemon offers slots: its list is not there, or no daemon holds it.
    NoDaemon,
}

/// What the slots' tests share, on every host.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod testing {
    use super::{Asked, Slot, Waited};

    /// The slot's whole request, which a client asked.
    pub(super) fn asked(slot: Slot) -> Asked {
        let Waited::Opened(opened) = slot.wait().unwrap() else {
            panic!("no client opened the slot");
        };
        opened.request().unwrap().expect("no whole request")
    }

    pub(super) fn home(name: &str) -> shards_testdir::TempDir {
        shards_testdir::TempDir::new(name).unwrap()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    //! The slots' cases every host passes as one: each Unix's FIFOs pass here, Windows'
    //! named pipes pass too (D195). Those resting on how one host's slots are made are its
    //! module's (fifo.rs, named.rs), each with its twin on the other.
    use super::testing::{asked, home};
    use super::*;
    use std::time::{Duration, Instant};

    /// A request goes through a slot and is answered, and the client's end ends the
    /// daemon's reads; the next client takes the next slot offered.
    #[test]
    fn a_request_is_answered_through_its_slot() {
        let home = home("slot-answered");
        let mut requests = Requests::open(&home).unwrap();
        let slot = requests.make(false).unwrap();
        requests.offer(slot.number(), false).unwrap();
        std::thread::scope(|s| {
            let served = s.spawn(move || {
                let Asked {
                    session,
                    request,
                    fds: named,
                } = asked(slot);
                assert_eq!(
                    request,
                    Frame {
                        kind: 7,
                        payload: b"ps".to_vec()
                    }
                );
                assert!(named.is_empty());
                assert!(session.input.is_none() && session.output.is_none());
                session.send(12, b"listed").unwrap();
                assert_eq!(
                    session.recv().unwrap(),
                    Some(Frame {
                        kind: 4,
                        payload: vec![9]
                    })
                );
                // The client's end.
                assert_eq!(session.recv().unwrap(), None);
            });
            let Taken::Slot(client) = take(&home, 0).unwrap() else {
                panic!("no slot");
            };
            assert_eq!(client.n, 0);
            client.ask(7, b"ps", &[]).unwrap();
            assert_eq!(
                client.recv().unwrap(),
                Some(Frame {
                    kind: 12,
                    payload: b"listed".to_vec()
                })
            );
            client.send(4, &[9]).unwrap();
            drop(client);
            served.join().unwrap();
        });
        assert!(requests.untaken(false).unwrap().is_empty());
    }

    /// No daemon, no slot: `free` not there, or with no writer; and a client waiting for a
    /// slot hears of its daemon's end.
    #[test]
    fn a_client_hears_there_is_no_daemon() {
        let home = home("slot-none");
        assert!(matches!(take(&home, 0).unwrap(), Taken::NoDaemon));
        let requests = Requests::open(&home).unwrap();
        std::thread::scope(|s| {
            let waiting = s.spawn(|| take(&home, 0).unwrap());
            std::thread::sleep(std::time::Duration::from_millis(50));
            assert!(
                !waiting.is_finished(),
                "a client with no slot offered did not wait"
            );
            drop(requests);
            assert!(matches!(waiting.join().unwrap(), Taken::NoDaemon));
        });
        assert!(matches!(take(&home, 0).unwrap(), Taken::NoDaemon));
    }

    /// A home's slots are served while its daemon holds its lists, and not once it has
    /// unoffered them or gone; its lists are listed until someone removes one.
    #[test]
    fn served_is_whether_a_daemon_holds_the_lists() {
        let home = home("slot-served");
        assert!(!served(&home), "no lists, yet served");
        let requests = Requests::open(&home).unwrap();
        assert!(served(&home));
        assert!(requests.listed());
        std::fs::remove_file(home.join(DIR).join(FREE_STREAMS)).unwrap();
        assert!(!requests.listed(), "a list removed, yet listed");
        unoffer(&home);
        assert!(!served(&home), "unoffered, yet served");
        let requests = Requests::open(&home).unwrap();
        assert!(served(&home) && requests.listed());
        // Removed, the directory and all, and made again: listed and served, its next
        // slot named after the last made before.
        let mut requests = requests;
        let before = requests.make(false).unwrap().number();
        std::fs::remove_dir_all(home.join(DIR)).unwrap();
        assert!(!requests.listed() && !served(&home));
        requests.renew().unwrap();
        assert!(requests.listed() && served(&home));
        assert_eq!(requests.make(false).unwrap().number(), before + 1);
        // Gone without unoffering, as a killed daemon goes: its list has no reader once no
        // process holds its end, a fork of another test's that has yet to execute included
        // (descriptors are closed at exec, O_CLOEXEC).
        drop(requests);
        let t0 = Instant::now();
        while served(&home) {
            assert!(
                t0.elapsed() < Duration::from_secs(5),
                "its daemon gone, yet served"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// What a daemon before left in its slots' directory goes as the next opens it: a killed
    /// daemon's names, a list it had yet to put in its place among them, which nothing after
    /// would take (Unix makes its lists anew under their names, which none may hold).
    #[test]
    fn what_a_daemon_before_left_goes_as_the_next_opens_its_slots() {
        let home = home("slot-left");
        let dir = home.join(DIR);
        std::fs::create_dir(&dir).unwrap();
        let left = ["free.new", "7", "7.out"].map(|name| dir.join(name));
        for path in &left {
            std::fs::write(path, "left").unwrap();
        }
        let requests = Requests::open(&home).unwrap();
        for path in &left {
            assert!(!path.exists(), "{} kept", path.display());
        }
        assert!(requests.listed() && served(&home));
    }

    /// A client waits no longer for its answer than it asks, and takes one that comes in
    /// time.
    #[test]
    fn an_answer_is_waited_for_no_longer_than_asked() {
        let home = home("slot-within");
        let mut requests = Requests::open(&home).unwrap();
        let slot = requests.make(false).unwrap();
        requests.offer(slot.number(), false).unwrap();
        std::thread::scope(|s| {
            let served = s.spawn(move || {
                let Asked { session, .. } = asked(slot);
                std::thread::sleep(std::time::Duration::from_millis(300));
                session.send(12, b"late").unwrap();
                // Held until the client has heard it.
                assert_eq!(session.recv().unwrap(), None);
            });
            let Taken::Slot(client) = take(&home, 0).unwrap() else {
                panic!("no slot");
            };
            client.ask(7, b"ps", &[]).unwrap();
            let t0 = std::time::Instant::now();
            let early = client.recv_within(std::time::Duration::from_millis(50));
            assert_eq!(early.map_err(|e| e.kind()), Err(io::ErrorKind::TimedOut));
            assert!(t0.elapsed() >= std::time::Duration::from_millis(50));
            assert_eq!(
                client.recv_within(std::time::Duration::from_secs(10)).unwrap(),
                Some(Frame {
                    kind: 12,
                    payload: b"late".to_vec()
                })
            );
            drop(client);
            served.join().unwrap();
        });
    }

    /// The streams a client opens are the session's, each way, each ending as its writer
    /// does; one it did not open is none of the session's.
    #[test]
    fn a_sessions_streams_carry_what_each_side_writes() {
        let home = home("slot-streams");
        let mut requests = Requests::open(&home).unwrap();
        let slot = requests.make(true).unwrap();
        requests.offer(slot.number(), true).unwrap();
        std::thread::scope(|s| {
            let served = s.spawn(move || {
                let Asked {
                    session, fds: named, ..
                } = asked(slot);
                assert_eq!(named, [fds::IN, fds::OUT, fds::NULL]);
                let mut got = String::new();
                Reader(session.input.as_ref().unwrap())
                    .read_to_string(&mut got)
                    .unwrap();
                assert_eq!(got, "typed");
                session.output.as_ref().unwrap().write_all(b"shown").unwrap();
                assert!(session.errors.is_none());
            });
            let Taken::Slot(mut client) = take(&home, streams::IN | streams::OUT).unwrap() else {
                panic!("no slot");
            };
            client.ask(5, b"run", &[fds::IN, fds::OUT, fds::NULL]).unwrap();
            let mut input = client.input.take().unwrap();
            input.write_all(b"typed").unwrap();
            drop(input);
            let mut shown = String::new();
            Reader(client.output.as_ref().unwrap())
                .read_to_string(&mut shown)
                .unwrap();
            assert_eq!(shown, "shown");
            served.join().unwrap();
        });
    }

    /// A record names its slot whole; one that does not is refused.
    #[test]
    fn records_name_their_slots() {
        assert_eq!(number(b"000000000000042\n").unwrap(), 42);
        assert!(number(b"00000000000004x\n").is_err());
        assert!(number(b"0000000000000042").is_err());
    }
}
