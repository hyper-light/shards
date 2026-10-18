//! Requests through files in the home (docs/design/architecture.md D185): a client and its
//! daemon meet in slots of FIFOs the daemon makes ahead in `$SHARDS_HOME/requests/`, each
//! used once.
//!
//! A slot `N` is five FIFOs, `N.say` (the client's messages), `N.answer` (the daemon's),
//! and the streams `N.in`, `N.out` and `N.err`, of which the daemon holds its ends, and the
//! other end of each besides, so that no read of it ends before a client has come: a read
//! of a FIFO ends once it has no writer, and a write fails once it has no reader. The daemon
//! names each slot it offers on `requests/free`, a record of [`RECORD`] bytes, written whole
//! (far under `PIPE_BUF`, 512 bytes on macOS); a client takes one by reading a record, which
//! no other client can then read, and where none is there its read waits for one, in the
//! order clients came. It opens its slot's `answer` and the streams it uses, then `say`, and
//! writes which streams it opened, what each descriptor its request stands for is
//! ([`fds`]), and its request. The daemon, reading that, lets go of the
//! other ends it held of them: from then on each side's end, its exit or its kill, ends the
//! other's read, which is how each learns the other has gone. A FIFO's end is seen by a
//! read(2) waiting on it, on macOS and Linux alike, never by poll(2) or kqueue(2) on macOS
//! (PM M253): each side reads in a blocking read.
//!
//! `free` with no writer, its daemon gone or never there, ends a client's read at once.

use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::MAX_PAYLOAD;

/// The home's directory of slots.
pub const DIR: &str = "requests";

/// The FIFO that offers the slots.
const FREE: &str = "free";

/// A record on `free`: a slot's number in 15 decimal digits, and a newline.
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

/// What the daemon writes to a slot's `say` to end its wait, where no client came: no first
/// byte of a client's has its top bit.
const STOP: u8 = 0x80;

/// A message's header: its kind, then its payload's length (big-endian u32).
const HEADER: usize = 5;

/// One message of a slot's: what a socket's message is, without descriptors.
#[derive(Debug, PartialEq, Eq)]
pub struct Frame {
    pub kind: u8,
    pub payload: Vec<u8>,
}

/// Writes one message to `to`.
pub fn write_frame(mut to: &File, kind: u8, payload: &[u8]) -> io::Result<()> {
    to.write_all(&frame(kind, payload)?)
}

fn frame(kind: u8, payload: &[u8]) -> io::Result<Vec<u8>> {
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
pub fn read_frame(mut from: &File) -> io::Result<Option<Frame>> {
    let mut header = [0u8; HEADER];
    let got = read_some(&mut from, &mut header)?;
    if got == 0 {
        return Ok(None);
    }
    let mut from = Reader(from);
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

/// Reads what comes into `buf`, again where a signal cut the read short: waited for first
/// in [`crate::await_fifo`], which on macOS sees a writer's end that a read sleeping in the
/// kernel can miss (PM M253). One that must not wait is not waited for.
fn read_some(from: &mut &File, buf: &mut [u8]) -> io::Result<usize> {
    if !nonblocking(from) {
        crate::await_fifo(std::os::fd::AsFd::as_fd(*from))?;
    }
    loop {
        match from.read(buf) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            got => return got,
        }
    }
}

/// Whether `file`'s reads are set not to wait.
fn nonblocking(file: &File) -> bool {
    // SAFETY: fcntl(2) reading the flags of a descriptor `file` holds open.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    flags >= 0 && flags & libc::O_NONBLOCK != 0
}

/// What reads a slot's FIFO as [`read_some`] does, for `read_exact`, `read_to_end` and
/// `io::copy`: what a client copies its streams with.
#[derive(Debug)]
pub struct Reader<'f>(pub &'f File);

impl Read for Reader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        read_some(&mut self.0, buf)
    }
}

/// Makes the FIFO `path`, this user's alone.
fn make_fifo(path: &Path) -> io::Result<()> {
    let name = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path with NUL"))?;
    // SAFETY: mkfifo(3) of a NUL-terminated path.
    if unsafe { libc::mkfifo(name.as_ptr(), 0o600) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Opens the FIFO `path` without waiting for its other end: to read, at once; to write,
/// at once where it has a reader, `ENXIO` where it has none.
fn open_now(path: &Path, write: bool) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .read(!write)
        .write(write)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}

/// `file`'s reads and writes wait again.
fn blocking(file: File) -> io::Result<File> {
    // SAFETY: fcntl(2) on a descriptor `file` holds open.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    // SAFETY: as above.
    if flags < 0 || unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

/// The FIFO `path` opened by both ends, each waiting: `(reader, writer)`.
fn both_ends(path: &Path) -> io::Result<(File, File)> {
    let reader = open_now(path, false)?;
    let writer = blocking(open_now(path, true)?)?;
    Ok((blocking(reader)?, writer))
}

/// The names of slot `n`'s FIFOs in `dir`: say, answer, in, out, err.
fn names(dir: &Path, n: u64) -> [PathBuf; 5] {
    ["say", "answer", "in", "out", "err"].map(|what| dir.join(format!("{n}.{what}")))
}

/// The daemon's side: the home's slots, and the list it offers them on.
#[derive(Debug)]
pub struct Requests {
    dir: PathBuf,
    /// `free`'s writer, whose end ends every client's wait for a slot.
    free: File,
    /// A reader of `free`'s own, that never waits: what no client has taken is read back
    /// through it ([`untaken`](Self::untaken)).
    back: File,
    next: u64,
}

impl Requests {
    /// The slots of `home`, made anew: what a daemon before left there goes (its clients
    /// keep what they opened), and `free` is made, offering nothing yet.
    pub fn open(home: &Path) -> io::Result<Requests> {
        let dir = home.join(DIR);
        match std::fs::read_dir(&dir) {
            Ok(entries) => {
                for entry in entries {
                    let path = entry?.path();
                    match std::fs::remove_file(&path) {
                        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                        _ => {}
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
            }
            Err(e) => return Err(e),
        }
        let at = dir.join(FREE);
        make_fifo(&at)?;
        let back = open_now(&at, false)?;
        let free = blocking(open_now(&at, true)?)?;
        Ok(Requests {
            dir,
            free,
            back,
            next: 0,
        })
    }

    /// A new slot: its FIFOs made and its ends held, offered to no client yet.
    pub fn make(&mut self) -> io::Result<Slot> {
        let n = self.next;
        self.next = self
            .next
            .checked_add(1)
            .ok_or_else(|| io::Error::other("slot numbers"))?;
        let [say, answer, input, output, errors] = names(&self.dir, n);
        let made = (|| {
            for path in [&say, &answer, &input, &output, &errors] {
                make_fifo(path)?;
            }
            let (say, say_own) = both_ends(&say)?;
            let (answer_own, answer) = both_ends(&answer)?;
            let (input, input_own) = both_ends(&input)?;
            let (output_own, output) = both_ends(&output)?;
            let (errors_own, errors) = both_ends(&errors)?;
            Ok(Slot {
                n,
                say,
                say_own,
                answer,
                answer_own,
                input: (input, input_own),
                output: (output, output_own),
                errors: (errors, errors_own),
            })
        })();
        if made.is_err() {
            self.remove(n);
        }
        made
    }

    /// Offers slot `n` to the next client that asks.
    pub fn offer(&self, n: u64) -> io::Result<()> {
        let record = format!("{n:015}\n");
        if record.len() != RECORD {
            return Err(io::Error::other("a slot's number past 15 digits"));
        }
        (&self.free).write_all(record.as_bytes())
    }

    /// The slots offered that no client has taken: every record on `free`, read back, for
    /// the caller to offer again, those it still has. A slot offered and not among them was
    /// taken, by a client that then writes to it at once, or never does where it died first.
    pub fn untaken(&self) -> io::Result<Vec<u64>> {
        let mut out = Vec::new();
        let mut record = [0u8; RECORD];
        loop {
            match read_some(&mut &self.back, &mut record) {
                Ok(RECORD) => out.push(number(&record)?),
                Ok(0) => return Ok(out),
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "a short record on free",
                    ));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(out),
                Err(e) => return Err(e),
            }
        }
    }

    /// Removes slot `n`'s names: a client may no longer open them, and those it opened stay
    /// its own.
    pub fn remove(&self, n: u64) {
        for path in names(&self.dir, n) {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// A slot's number, from its record.
fn number(record: &[u8; RECORD]) -> io::Result<u64> {
    std::str::from_utf8(record)
        .ok()
        .and_then(|r| r.strip_suffix('\n'))
        .and_then(|r| r.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "a record on free that names no slot"))
}

/// A slot the daemon made, its ends held: waited on by [`Slot::wait`].
#[derive(Debug)]
pub struct Slot {
    n: u64,
    say: File,
    /// A writer of its own, so that `say` has one until a client writes.
    say_own: File,
    answer: File,
    /// A reader of its own, so that `answer` has one until a client opens its own.
    answer_own: File,
    /// Each stream: the daemon's end (what it hands a run's VM) and the other, its own.
    input: (File, File),
    output: (File, File),
    errors: (File, File),
}

/// What a slot's wait came to.
#[derive(Debug)]
pub enum Waited {
    /// A client's request, the slot's session, and what each descriptor the request stands
    /// for is ([`fds`]), in order.
    Asked {
        session: Session,
        request: Frame,
        fds: Vec<u8>,
    },
    /// The daemon ended the wait ([`Stopper::stop`]).
    Stopped,
    /// A client wrote, then ended before its request was whole.
    Gone,
}

impl Slot {
    pub fn number(&self) -> u64 {
        self.n
    }

    /// What ends this slot's wait from another thread, as its client never came.
    pub fn stopper(&self) -> io::Result<Stopper> {
        Ok(Stopper(self.say_own.try_clone()?))
    }

    /// Waits for a client's request on this slot. Once it has said which streams it
    /// opened, the daemon lets go of their other ends, and of `say`'s writer and `answer`'s
    /// reader: then the client's end ends the session's reads, and a client that ends
    /// before its request is whole is [`Waited::Gone`].
    pub fn wait(self) -> io::Result<Waited> {
        let Slot {
            n,
            say,
            say_own,
            answer,
            answer_own,
            input,
            output,
            errors,
        } = self;
        let mut first = [0u8; 1];
        if read_some(&mut &say, &mut first)? == 0 {
            return Ok(Waited::Gone);
        }
        let [opened] = first;
        if opened & STOP != 0 {
            return Ok(Waited::Stopped);
        }
        drop((say_own, answer_own));
        let take = |(ours, own): (File, File), bit: u8| {
            (opened & bit != 0).then(|| {
                drop(own);
                ours
            })
        };
        let session = Session {
            n,
            say,
            answer,
            input: take(input, streams::IN),
            output: take(output, streams::OUT),
            errors: take(errors, streams::ERR),
        };
        let named = (|| -> io::Result<Option<(Vec<u8>, Frame)>> {
            let mut count = [0u8; 1];
            Reader(&session.say).read_exact(&mut count)?;
            let [count] = count;
            if usize::from(count) > fds::MAX {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "a request of too many descriptors",
                ));
            }
            let mut named = vec![0u8; usize::from(count)];
            Reader(&session.say).read_exact(&mut named)?;
            if named.iter().any(|&f| f > fds::NULL) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "a descriptor of no stream",
                ));
            }
            Ok(read_frame(&session.say)?.map(|request| (named, request)))
        })();
        match named {
            Ok(Some((fds, request))) => Ok(Waited::Asked {
                session,
                request,
                fds,
            }),
            Ok(None) => Ok(Waited::Gone),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(Waited::Gone),
            Err(e) => Err(e),
        }
    }
}

/// Ends a slot's wait from another thread ([`Slot::stopper`]).
#[derive(Debug)]
pub struct Stopper(File);

impl Stopper {
    pub fn stop(&self) -> io::Result<()> {
        (&self.0).write_all(&[STOP])
    }
}

/// The daemon's side of a slot a client asked through: the client's messages on `say`,
/// the daemon's answers on `answer`, and the streams the client opened, each the end a
/// run's VM is given.
#[derive(Debug)]
pub struct Session {
    pub n: u64,
    pub say: File,
    pub answer: File,
    /// What the client writes, the command's stdin, read here.
    pub input: Option<File>,
    /// What the client reads: the command's stdout and stderr, written here.
    pub output: Option<File>,
    pub errors: Option<File>,
}

impl Session {
    pub fn send(&self, kind: u8, payload: &[u8]) -> io::Result<()> {
        write_frame(&self.answer, kind, payload)
    }

    /// The client's next message: `None` once it has ended.
    pub fn recv(&self) -> io::Result<Option<Frame>> {
        read_frame(&self.say)
    }
}

/// What a client's ask for a slot came to.
#[derive(Debug)]
pub enum Taken {
    Slot(Client),
    /// No daemon offers slots: `free` has no writer, or is not there.
    NoDaemon,
}

/// The client's side of its slot: its messages to `say`, the daemon's from `answer`, and
/// the streams it opened.
#[derive(Debug)]
pub struct Client {
    pub n: u64,
    say: File,
    answer: File,
    /// The command's stdin, written here.
    pub input: Option<File>,
    /// The command's stdout and stderr, read here.
    pub output: Option<File>,
    pub errors: Option<File>,
    opened: u8,
}

/// Takes a slot of `home`'s daemon, opening the `streams` it names: waits for one where all
/// are taken, and passes over one the daemon has removed since it offered it.
pub fn take(home: &Path, streams: u8) -> io::Result<Taken> {
    let dir = home.join(DIR);
    let free = match open_now(&dir.join(FREE), false) {
        Ok(free) => free,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Taken::NoDaemon),
        Err(e) => return Err(e),
    };
    let mut free = Some(free);
    let mut waiting = None;
    let mut record = [0u8; RECORD];
    loop {
        // A first read that does not wait: none there and no writer is no daemon.
        let got = match (&free, &waiting) {
            (Some(now), _) => match read_some(&mut &*now, &mut record) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    waiting = free.take().map(blocking).transpose()?;
                    continue;
                }
                got => got?,
            },
            (None, Some(waits)) => read_some(&mut &*waits, &mut record)?,
            (None, None) => return Err(io::Error::other("free: no reader")),
        };
        match got {
            0 => return Ok(Taken::NoDaemon),
            RECORD => {}
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "a short record on free",
                ));
            }
        }
        let n = number(&record)?;
        match open_slot(&dir, n, streams) {
            Ok(client) => return Ok(Taken::Slot(client)),
            // Removed since it was offered: the next.
            Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENXIO)) => {}
            Err(e) => return Err(e),
        }
    }
}

/// Opens slot `n` of `dir`: `answer` and the `streams` named, then `say`.
fn open_slot(dir: &Path, n: u64, streams: u8) -> io::Result<Client> {
    let [say, answer, input, output, errors] = names(dir, n);
    let answer = blocking(open_now(&answer, false)?)?;
    let open = |path: &Path, bit: u8, write: bool| -> io::Result<Option<File>> {
        if streams & bit == 0 {
            return Ok(None);
        }
        Ok(Some(blocking(open_now(path, write)?)?))
    };
    let input = open(&input, streams::IN, true)?;
    let output = open(&output, streams::OUT, false)?;
    let errors = open(&errors, streams::ERR, false)?;
    let say = blocking(open_now(&say, true)?)?;
    Ok(Client {
        n,
        say,
        answer,
        input,
        output,
        errors,
        opened: streams & (streams::IN | streams::OUT | streams::ERR),
    })
}

impl Client {
    /// Sends the request: which streams this client opened, what each descriptor the
    /// request stands for is (`fds`, of [`fds`]), then `kind` and `payload`, in one write.
    pub fn ask(&self, kind: u8, payload: &[u8], named: &[u8]) -> io::Result<()> {
        let count = u8::try_from(named.len())
            .ok()
            .filter(|&c| usize::from(c) <= fds::MAX)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "a request of too many descriptors")
            })?;
        let mut bytes = vec![self.opened, count];
        bytes.extend_from_slice(named);
        bytes.extend(frame(kind, payload)?);
        (&self.say).write_all(&bytes)
    }

    pub fn send(&self, kind: u8, payload: &[u8]) -> io::Result<()> {
        write_frame(&self.say, kind, payload)
    }

    /// The daemon's next message: `None` once its end, and its VM's, have gone.
    pub fn recv(&self) -> io::Result<Option<Frame>> {
        read_frame(&self.answer)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    fn home(name: &str) -> shards_testdir::TempDir {
        shards_testdir::TempDir::new(name).unwrap()
    }

    /// A request goes through a slot and is answered, and the client's end ends the
    /// daemon's reads; the next client takes the next slot offered.
    #[test]
    fn a_request_is_answered_through_its_slot() {
        let home = home("slot-answered");
        let mut requests = Requests::open(&home).unwrap();
        let slot = requests.make().unwrap();
        requests.offer(slot.number()).unwrap();
        std::thread::scope(|s| {
            let served = s.spawn(move || {
                let Waited::Asked {
                    session,
                    request,
                    fds: named,
                } = slot.wait().unwrap()
                else {
                    panic!("no request");
                };
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
        assert!(requests.untaken().unwrap().is_empty());
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

    /// The streams a client opens are the session's, each way, each ending as its writer
    /// does; one it did not open is none of the session's.
    #[test]
    fn a_sessions_streams_carry_what_each_side_writes() {
        let home = home("slot-streams");
        let mut requests = Requests::open(&home).unwrap();
        let slot = requests.make().unwrap();
        requests.offer(slot.number()).unwrap();
        std::thread::scope(|s| {
            let served = s.spawn(move || {
                let Waited::Asked {
                    session, fds: named, ..
                } = slot.wait().unwrap()
                else {
                    panic!("no request");
                };
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

    /// With every slot taken a client waits for the next offered; one whose slot was
    /// removed since it was offered takes the next.
    #[test]
    fn a_client_waits_for_a_slot_and_passes_over_a_removed_one() {
        let home = home("slot-wait");
        let mut requests = Requests::open(&home).unwrap();
        let gone = requests.make().unwrap();
        requests.offer(gone.number()).unwrap();
        assert_eq!(requests.untaken().unwrap(), [0]);
        requests.offer(gone.number()).unwrap();
        // Removed, its wait ended, as a sweep removes one.
        requests.remove(gone.number());
        gone.stopper().unwrap().stop().unwrap();
        assert!(matches!(gone.wait().unwrap(), Waited::Stopped));
        std::thread::scope(|s| {
            let waiting = s.spawn(|| take(&home, 0).unwrap());
            std::thread::sleep(std::time::Duration::from_millis(50));
            assert!(!waiting.is_finished(), "a client took a removed slot");
            let next = requests.make().unwrap();
            requests.offer(next.number()).unwrap();
            let Taken::Slot(client) = waiting.join().unwrap() else {
                panic!("no slot");
            };
            assert_eq!(client.n, 1);
            // One that opened its slot and ended unwritten leaves it waiting, as fresh.
            drop(client);
            let stopper = next.stopper().unwrap();
            let waited = s.spawn(move || next.wait().unwrap());
            std::thread::sleep(std::time::Duration::from_millis(50));
            assert!(!waited.is_finished(), "a slot no client wrote to ended its wait");
            stopper.stop().unwrap();
            assert!(matches!(waited.join().unwrap(), Waited::Stopped));
        });
    }

    /// A client process killed mid-session ends the daemon's read of it; one killed
    /// before its request is whole is gone.
    #[test]
    fn a_killed_client_ends_its_session() {
        let home = home("slot-killed");
        let mut requests = Requests::open(&home).unwrap();
        let dir = home.join(DIR);
        // A shell for a client: it opens its slot's answer, then say, writes `head`, its
        // first bytes (no streams, no descriptors) and a message's first bytes or all of
        // them, and sleeps until killed.
        let client = |n: u64, head: &str| {
            Command::new("/bin/sh")
                .arg("-c")
                .arg(format!(
                    "exec 3<{n}.answer 4>{n}.say; printf '{head}' >&4; exec sleep 600"
                ))
                .current_dir(&dir)
                .stdin(Stdio::null())
                .spawn()
                .unwrap()
        };
        // Whole: kind 7, two bytes, "ps".
        let slot = requests.make().unwrap();
        let mut whole = client(slot.number(), "\\0\\0\\007\\0\\0\\0\\002ps");
        let Waited::Asked { session, request, .. } = slot.wait().unwrap() else {
            panic!("no request");
        };
        assert_eq!(
            request,
            Frame {
                kind: 7,
                payload: b"ps".to_vec()
            }
        );
        whole.kill().unwrap();
        let _ = whole.wait();
        assert_eq!(session.recv().unwrap(), None);
        // Cut short: a header that says ten bytes, and three of them.
        let slot = requests.make().unwrap();
        let mut cut = client(slot.number(), "\\0\\0\\007\\0\\0\\0\\012abc");
        std::thread::scope(|s| {
            let waited = s.spawn(|| slot.wait().unwrap());
            std::thread::sleep(std::time::Duration::from_millis(100));
            cut.kill().unwrap();
            let _ = cut.wait();
            assert!(matches!(waited.join().unwrap(), Waited::Gone));
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
