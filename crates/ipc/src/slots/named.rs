//! Slots of named pipes, on Windows (D195). Each list, without streams and with, is a named
//! pipe whose name the file `requests/free` or `requests/free-streams` gives: a fresh random
//! name each time the daemon makes its lists, of its user's alone ([`pipe::attributes`]),
//! remote clients refused. A slot is an instance of its list's pipe, the daemon's, which
//! every client may open once made; a client takes one by opening the pipe, which gives it
//! one instance no client has opened, and no other client can then open that instance
//! (PM M340); where all are taken it waits until one is free (WaitNamedPipe), and where the
//! name has none, no daemon serves it. The daemon's first word on a slot is its number
//! ([`RECORD`]): a slot with streams has three pipes more, `<list>-<N>.in`, `.out` and
//! `.err`, each its name's only instance, which the client opens before it writes. Then, as
//! on Unix, it writes which streams it opened, what each descriptor its request stands for
//! is ([`fds`]), and its request.
//!
//! Each side's end, its exit or its kill, ends the other's reads after all it wrote (M340).
//! A list's name lives while the daemon holds it: the daemon keeps an instance of each,
//! opened by itself, as a Unix list has its daemon's reader. A waiting client hears of its
//! daemon's end at once: its wait ends as the last instance closes (M348). A slot no client
//! opened is stopped by its own event ([`Stopper`]), and its instance closed: a client that
//! opened it meanwhile reads no number, and takes another.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    ERROR_BAD_PIPE, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_SEM_TIMEOUT,
};
use windows_sys::Win32::System::Pipes::WaitNamedPipeW;

use super::{
    Asked, DIR, FREE, FREE_STREAMS, Frame, RECORD, Requested, Taken, Waited, fds, frame, frame_from, list,
    number, read_frame, record, streams, write_frame,
};
use crate::pipe::{self, Called, Event, Pipe};

/// How long a client waiting for a free instance waits before it looks at its list's name
/// again: a daemon that made its lists again (`Requests::renew`) offers on another name,
/// which a wait on the old one, held by the clients it serves still, would not see.
const LOOK_AGAIN: Duration = Duration::from_millis(250);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The Win32 error `e` carries, where it carries one.
fn code(e: &io::Error) -> Option<u32> {
    e.raw_os_error().and_then(|c| u32::try_from(c).ok())
}

/// A slot's stream's pipe: `list`'s, slot `n`'s, `what` (`in`, `out`, `err`).
fn stream_name(list: &str, n: u64, what: &str) -> String {
    format!("{list}-{n}.{what}")
}

/// A pipe instance for one of a slot's streams: its name's only, made in that name's
/// first, not overlapped, so that the daemon and the VM it hands it to read and write it as
/// a file; a client's end is one too.
fn stream_instance(name: &str) -> io::Result<File> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
    use windows_sys::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    let attributes = pipe::attributes()?;
    let wide = pipe::wide(name);
    // SAFETY: a NUL-terminated name, attributes that live through the call.
    let raw = unsafe {
        CreateNamedPipeW(
            wide.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            pipe::BUFFER,
            pipe::BUFFER,
            0,
            &raw const attributes,
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the instance just made, which nothing else owns.
    Ok(File::from(unsafe { OwnedHandle::from_raw_handle(raw) }))
}

/// Whether a client has opened the instance `server`: one that opened it and went since
/// has; one still listening (PeekNamedPipe's ERROR_BAD_PIPE there, PM M348) has not.
fn opened(server: &impl std::os::windows::io::AsRawHandle) -> io::Result<bool> {
    match pipe::available(server.as_raw_handle(), None) {
        Ok(_) => Ok(true),
        Err(e) if code(&e) == Some(ERROR_BAD_PIPE) => Ok(false),
        Err(e) => Err(e),
    }
}

/// A list's instance its daemon opened itself: the server's end, and the client's. Its name
/// lives while the daemon holds it.
type Keeper = (OwnedHandle, OwnedHandle);

/// The daemon's side: the home's lists, the slots made on them, and the instances that
/// keep their names.
#[derive(Debug)]
pub struct Requests {
    dir: PathBuf,
    /// Each list's pipe's name, `free`'s and `free-streams`'.
    names: [String; 2],
    _keepers: [Keeper; 2],
    /// Each slot made and not removed: whether it has streams, and another handle to its
    /// instance, to see whether a client has opened it.
    made: Mutex<HashMap<u64, (bool, OwnedHandle)>>,
    next: u64,
}

/// Makes the lists of `dir`, under names no list had: their keepers, and the files that
/// name them, each written whole before its name (a rename), so that a client reads a whole
/// name or none.
fn lists(dir: &Path) -> io::Result<([String; 2], [Keeper; 2])> {
    let prefix = format!(r"\\.\pipe\shards-{}", pipe::random_hex(16)?);
    let made = [FREE, FREE_STREAMS].map(|file| -> io::Result<(String, Keeper)> {
        let name = format!("{prefix}-{file}");
        let wide = pipe::wide(&name);
        let server = pipe::instance(&wide, true, false)?;
        let client = pipe::open(&wide, true)?;
        if !pipe::connected(server.as_raw_handle(), &[])? {
            return Err(io::Error::other("a list's own instance did not connect"));
        }
        let at = dir.join(file);
        let new = dir.join(format!("{file}.new"));
        std::fs::write(&new, format!("{name}\n"))?;
        std::fs::rename(&new, &at)?;
        Ok((name, (server, client)))
    });
    let [plain, with] = made;
    let ((plain_name, plain_keep), (with_name, with_keep)) = (plain?, with?);
    Ok(([plain_name, with_name], [plain_keep, with_keep]))
}

/// The name a list's file gives, where it is there.
fn listed_name(at: &Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(at) {
        Ok(text) => Ok(Some(text.trim().to_string())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Ends `home`'s offers at once: its lists' names go, so that a client looking for a slot
/// hears there is no daemon, and asks the next. What clients took stays theirs.
pub fn unoffer(home: &Path) {
    for name in [FREE, FREE_STREAMS] {
        let _ = std::fs::remove_file(home.join(DIR).join(name));
    }
}

/// Whether a daemon offers `home`'s slots: its list `free` names a pipe that has an
/// instance, the daemon's own while it serves ([`Requests`]); a closing daemon has taken the
/// list's name already ([`unoffer`]).
pub fn served(home: &Path) -> bool {
    let Ok(Some(name)) = listed_name(&home.join(DIR).join(FREE)) else {
        return false;
    };
    let wide = pipe::wide(&name);
    // SAFETY: a NUL-terminated name. One millisecond: a pipe whose every instance is taken
    // waits that long, one with none answers at once (M348).
    if unsafe { WaitNamedPipeW(wide.as_ptr(), 1) } != 0 {
        return true;
    }
    code(&io::Error::last_os_error()) == Some(ERROR_SEM_TIMEOUT)
}

impl Requests {
    /// The slots of `home`, made anew: what a daemon before left there goes (its clients
    /// keep what they opened), and its lists are made, offering nothing yet.
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
            Err(e) if e.kind() == io::ErrorKind::NotFound => std::fs::create_dir(&dir)?,
            Err(e) => return Err(e),
        }
        let (names, keepers) = lists(&dir)?;
        Ok(Requests {
            dir,
            names,
            _keepers: keepers,
            made: Mutex::new(HashMap::new()),
            next: 0,
        })
    }

    /// Its lists made again where someone removed them ([`listed`](Self::listed)): new
    /// names, the directory too where it went, the next slot's number kept. What was
    /// offered on the old lists is the caller's to remove; a client waiting on one looks
    /// at its list's name again ([`LOOK_AGAIN`]), and finds the new.
    pub fn renew(&mut self) -> io::Result<()> {
        match std::fs::create_dir(&self.dir) {
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
            _ => {}
        }
        let (names, keepers) = lists(&self.dir)?;
        self.names = names;
        self._keepers = keepers;
        Ok(())
    }

    /// A new slot, with `streams` or without: an instance of its list's pipe, which a client
    /// may open from now on, and its streams' pipes.
    pub fn make(&mut self, streams: bool) -> io::Result<Slot> {
        let n = self.next;
        self.next = self
            .next
            .checked_add(1)
            .ok_or_else(|| io::Error::other("slot numbers"))?;
        let name = self.names.get(list(streams)).ok_or(io::ErrorKind::InvalidInput)?;
        let instance = pipe::instance(&pipe::wide(name), false, false)?;
        let kept = instance.try_clone()?;
        let streams = if streams {
            Some([
                stream_instance(&stream_name(name, n, "in"))?,
                stream_instance(&stream_name(name, n, "out"))?,
                stream_instance(&stream_name(name, n, "err"))?,
            ])
        } else {
            None
        };
        let slot = Slot {
            n,
            line: Pipe::from_handle(instance),
            streams,
            stop: Event::new()?,
        };
        lock(&self.made).insert(n, (slot.streams.is_some(), kept));
        Ok(slot)
    }

    /// Offers slot `n`: made, it is offered already, an instance any client may open.
    pub fn offer(&self, n: u64, streams: bool) -> io::Result<()> {
        match lock(&self.made).get(&n) {
            Some((with, _)) if *with == streams => Ok(()),
            _ => Err(io::ErrorKind::InvalidInput.into()),
        }
    }

    /// How many slots on one list, with `streams` or without, no client has taken: the
    /// instances made no client has opened, none of them taken by the count.
    pub fn waiting(&self, streams: bool) -> io::Result<usize> {
        Ok(self.untaken(streams)?.len())
    }

    /// The slots offered on one list, with `streams` or without, that no client has taken.
    /// An instance stays offered as it is read (Unix reads a record back off its list): a
    /// client may open it before its stop closes it, and then reads no number and takes
    /// another.
    pub fn untaken(&self, streams: bool) -> io::Result<Vec<u64>> {
        let made = lock(&self.made);
        let mut out = Vec::new();
        for (n, (with, instance)) in made.iter() {
            if *with == streams && !opened(instance)? {
                out.push(*n);
            }
        }
        out.sort_unstable();
        Ok(out)
    }

    /// Whether both lists' files are still there, naming this daemon's lists: someone may
    /// remove them while the daemon serves, as they may any file of the home, and then no
    /// client finds a slot.
    pub fn listed(&self) -> bool {
        [FREE, FREE_STREAMS]
            .iter()
            .zip(&self.names)
            .all(|(file, name)| listed_name(&self.dir.join(file)).ok().flatten().as_ref() == Some(name))
    }

    /// Lets go of slot `n`: what is left of it is its thread's, and its client's.
    pub fn remove(&self, n: u64) {
        lock(&self.made).remove(&n);
    }
}

/// A slot the daemon made: waited on by [`Slot::wait`].
#[derive(Debug)]
pub struct Slot {
    n: u64,
    /// Its instance of its list's pipe: its client's messages, and the daemon's.
    line: Pipe,
    /// A slot with streams: in, out and err, the daemon's ends, which a run's VM is handed.
    streams: Option<[File; 3]>,
    /// What ends its wait where no client came ([`Stopper`]).
    stop: Event,
}

/// A slot whose client began its request.
#[derive(Debug)]
pub struct Opened {
    session: Session,
}

impl Slot {
    pub fn number(&self) -> u64 {
        self.n
    }

    /// Whether it has streams: offered on `free-streams`.
    pub fn streams(&self) -> bool {
        self.streams.is_some()
    }

    /// What ends this slot's wait from another thread, as its client never came.
    pub fn stopper(&self) -> io::Result<Stopper> {
        Ok(Stopper(self.stop.try_clone()?))
    }

    /// Waits for a client to take this slot and begin its request, or for its stop: a
    /// client's bytes first where both have come. A client that opens it is told its number;
    /// once it has said which streams it opened, the streams it did not are let go of, and
    /// one it says it opened and did not is refused. A client that ends before its first
    /// byte leaves the slot gone, which its maker replaces.
    pub fn wait(self) -> io::Result<Waited> {
        let Slot {
            n,
            line,
            streams: kept,
            stop,
        } = self;
        if !pipe::connected(line.raw(), &[&stop])? {
            return Ok(Waited::Stopped);
        }
        match line.write_within(record(n)?.as_bytes(), &[&stop], None) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::ConnectionAborted => return Ok(Waited::Stopped),
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(Waited::Gone),
            Err(e) => return Err(e),
        }
        let mut first = [0u8; 1];
        let opened_streams = loop {
            match line.read_within(&mut first, &[&stop], None) {
                Called::Done(Ok(0)) => return Ok(Waited::Gone),
                Called::Done(Ok(_)) => break first[0],
                Called::Done(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
                Called::Done(Err(e)) => return Err(e),
                Called::Cancelled | Called::Late => return Ok(Waited::Stopped),
            }
        };
        let known = if kept.is_some() {
            streams::IN | streams::OUT | streams::ERR
        } else {
            0
        };
        if opened_streams & !known != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "a client's first byte names streams its slot has not",
            ));
        }
        let take = |end: File, bit: u8| -> io::Result<Option<File>> {
            if opened_streams & bit == 0 {
                return Ok(None);
            }
            if !opened(&end)? {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "a client's first byte names a stream it did not open",
                ));
            }
            Ok(Some(end))
        };
        let [input, output, errors] = match kept {
            Some([input, output, errors]) => [
                take(input, streams::IN)?,
                take(output, streams::OUT)?,
                take(errors, streams::ERR)?,
            ],
            None => [None, None, None],
        };
        Ok(Waited::Opened(Opened {
            session: Session {
                n,
                say: line.try_clone()?,
                answer: line,
                input,
                output,
                errors,
            },
        }))
    }
}

/// What reads a slot's line until a deadline, and no longer once `ended` is set: a
/// request's reader. Its errors: `TimedOut` past the deadline, `ConnectionAborted` once
/// ended.
struct Within<'a> {
    line: &'a Pipe,
    ended: Option<&'a Event>,
    deadline: Option<Instant>,
}

impl Read for Within<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let cancels: Vec<&Event> = self.ended.into_iter().collect();
        match self.line.read_within(buf, &cancels, self.deadline) {
            Called::Done(done) => done,
            Called::Cancelled => Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "its daemon ended its clients",
            )),
            Called::Late => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "no whole request in time",
            )),
        }
    }
}

impl Opened {
    /// The rest of the client's request: `None` where the client ended before it was
    /// whole.
    pub fn request(self) -> io::Result<Option<Asked>> {
        Ok(match self.request_within(None, None)? {
            Requested::Asked(asked) => Some(asked),
            Requested::Gone | Requested::Late(_) | Requested::Ended(_) => None,
        })
    }

    /// The rest of the client's request, whole within `timeout` of now where one is given,
    /// and waited for no longer once `ended` is set (its daemon ending its clients).
    pub fn request_within(
        self,
        timeout: Option<Duration>,
        ended: Option<&crate::Latch>,
    ) -> io::Result<Requested> {
        let session = self.session;
        let named = {
            let mut from = Within {
                line: &session.say,
                ended: ended.map(crate::Latch::event),
                deadline: timeout.and_then(|t| Instant::now().checked_add(t)),
            };
            (|| -> io::Result<Option<(Vec<u8>, Frame)>> {
                let mut count = [0u8; 1];
                from.read_exact(&mut count)?;
                let [count] = count;
                if usize::from(count) > fds::MAX {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "a request of too many descriptors",
                    ));
                }
                let mut named = vec![0u8; usize::from(count)];
                from.read_exact(&mut named)?;
                if named.iter().any(|&f| f > fds::NULL) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "a descriptor of no stream",
                    ));
                }
                Ok(frame_from(&mut from)?.map(|request| (named, request)))
            })()
        };
        match named {
            Ok(Some((fds, request))) => Ok(Requested::Asked(Asked {
                session,
                fds,
                request,
            })),
            Ok(None) => Ok(Requested::Gone),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(Requested::Gone),
            Err(e) if e.kind() == io::ErrorKind::TimedOut => Ok(Requested::Late(session)),
            Err(e) if e.kind() == io::ErrorKind::ConnectionAborted => Ok(Requested::Ended(session)),
            Err(e) => Err(e),
        }
    }
}

/// Ends a slot's wait from another thread ([`Slot::stopper`]), through the slot's own event:
/// nothing is written where a client writes.
#[derive(Debug)]
pub struct Stopper(Event);

impl Stopper {
    pub fn stop(&self) -> io::Result<()> {
        self.0.set();
        Ok(())
    }
}

/// The daemon's side of a slot a client asked through: the client's messages on `say`, the
/// daemon's answers on `answer` (the same instance), and the streams the client opened,
/// each the end a run's VM is given.
#[derive(Debug)]
pub struct Session {
    pub n: u64,
    pub say: Pipe,
    pub answer: Pipe,
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

/// The client's side of its slot: its messages, and the daemon's, on its instance, and the
/// streams it opened.
#[derive(Debug)]
pub struct Client {
    pub n: u64,
    say: Pipe,
    answer: Pipe,
    /// The command's stdin, written here.
    pub input: Option<File>,
    /// The command's stdout and stderr, read here.
    pub output: Option<File>,
    pub errors: Option<File>,
    opened: u8,
}

/// What came of opening a list's pipe.
enum Opening {
    Took(Client),
    /// The slot opened was let go of before its number came: another.
    Again,
}

/// Takes a slot of `home`'s daemon, one with streams where `streams` names any, opening those
/// it names: waits for one where all are taken, and passes over one the daemon let go of
/// since it offered it.
pub fn take(home: &Path, streams: u8) -> io::Result<Taken> {
    let at = home
        .join(DIR)
        .join(if streams == 0 { FREE } else { FREE_STREAMS });
    'list: loop {
        let Some(name) = listed_name(&at)? else {
            return Ok(Taken::NoDaemon);
        };
        let wide = pipe::wide(&name);
        loop {
            match pipe::open(&wide, true) {
                Ok(handle) => match open_slot(&name, Pipe::from_handle(handle), streams)? {
                    Opening::Took(client) => return Ok(Taken::Slot(client)),
                    Opening::Again => continue 'list,
                },
                Err(e) if code(&e) == Some(ERROR_PIPE_BUSY) => {
                    // SAFETY: a NUL-terminated name.
                    if unsafe {
                        WaitNamedPipeW(
                            wide.as_ptr(),
                            u32::try_from(LOOK_AGAIN.as_millis()).unwrap_or(250),
                        )
                    } != 0
                    {
                        continue;
                    }
                    let e = io::Error::last_os_error();
                    match code(&e) {
                        // Every one still taken: its name may have changed meanwhile.
                        Some(ERROR_SEM_TIMEOUT) if listed_name(&at)?.as_ref() == Some(&name) => {}
                        Some(ERROR_SEM_TIMEOUT | ERROR_FILE_NOT_FOUND) => continue 'list,
                        _ => return Err(e),
                    }
                }
                // No instance: the daemon has gone, unless its lists are named anew.
                Err(e) if code(&e) == Some(ERROR_FILE_NOT_FOUND) => {
                    if listed_name(&at)?.is_some_and(|now| now != name) {
                        continue 'list;
                    }
                    return Ok(Taken::NoDaemon);
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// The slot `line` opened, on the list `list`: its number, which the daemon says first, then
/// the streams `streams` names, each opened by its name.
fn open_slot(list: &str, line: Pipe, streams: u8) -> io::Result<Opening> {
    let mut said = [0u8; RECORD];
    match (&line).read_exact(&mut said) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(Opening::Again),
        Err(e) => return Err(e),
    }
    let n = number(&said)?;
    let open = |what: &str, bit: u8| -> io::Result<Option<File>> {
        if streams & bit == 0 {
            return Ok(None);
        }
        Ok(Some(File::from(pipe::open(
            &pipe::wide(&stream_name(list, n, what)),
            false,
        )?)))
    };
    let opened = (|| {
        Ok::<_, io::Error>((
            open("in", streams::IN)?,
            open("out", streams::OUT)?,
            open("err", streams::ERR)?,
        ))
    })();
    let (input, output, errors) = match opened {
        Ok(opened) => opened,
        // Let go of since it said its number: the next.
        Err(e) if code(&e) == Some(ERROR_FILE_NOT_FOUND) => return Ok(Opening::Again),
        Err(e) => return Err(e),
    };
    Ok(Opening::Took(Client {
        n,
        say: line.try_clone()?,
        answer: line,
        input,
        output,
        errors,
        opened: streams & (streams::IN | streams::OUT | streams::ERR),
    }))
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

    /// [`recv`](Self::recv), waiting no longer than `timeout` for the daemon's message to
    /// begin, or its end: an error of kind `TimedOut` where neither has come.
    pub fn recv_within(&self, timeout: Duration) -> io::Result<Option<Frame>> {
        let deadline = Instant::now().checked_add(timeout);
        // A read of no bytes, which waits for what comes (M348).
        match pipe::read_into(self.answer.raw(), &mut [], &[], deadline) {
            Called::Done(Ok(_)) => {}
            Called::Done(Err(e)) if pipe::ended(&e) => {}
            Called::Done(Err(e)) => return Err(e),
            Called::Cancelled | Called::Late => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the daemon did not answer in time",
                ));
            }
        }
        read_frame(&self.answer)
    }

    /// What sends on this client's line beside it, from another thread: its signals' and
    /// resizes' sender.
    pub fn sender(&self) -> io::Result<Sender> {
        Ok(Sender(self.say.try_clone()?))
    }
}

/// A second writer of a client's line ([`Client::sender`]). Each of its messages is one
/// write, which a pipe takes whole whatever else writes.
#[derive(Debug)]
pub struct Sender(Pipe);

impl Sender {
    pub fn send(&self, kind: u8, payload: &[u8]) -> io::Result<()> {
        write_frame(&self.0, kind, payload)
    }
}

/// What reads one of a slot's streams, for `read_exact`, `read_to_end` and `io::copy`:
/// what a client copies its streams with. A pipe whose writer has gone reads its end
/// (M340), as std reads ERROR_BROKEN_PIPE.
#[derive(Debug)]
pub struct Reader<'f>(pub &'f File);

impl Read for Reader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match (&mut &*self.0).read(buf) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                got => return got,
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    //! The Windows twins of fifo.rs's cases: what they hold of FIFOs, held of named pipes.
    //! A slot is told its number by the daemon's thread waiting on it, so a slot a client
    //! takes here has one, as each a daemon offers does (`serve_slot`).
    use super::super::OFFERED;
    use super::super::testing::{asked, home};
    use super::*;
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc;

    /// Waits on `slot` on a thread of `s`'s, as the daemon's maker serves each it offers.
    fn waiting<'s>(
        s: &'s std::thread::Scope<'s, '_>,
        slot: Slot,
    ) -> std::thread::ScopedJoinHandle<'s, io::Result<Waited>> {
        s.spawn(move || slot.wait())
    }

    /// Stops each slot it holds as it drops, as a failed assertion unwinds too: a slot's
    /// thread waits until it is stopped, and a scope joins its threads as it ends, which
    /// would leave a failure a hang.
    #[derive(Default)]
    struct Stops(Mutex<Vec<Stopper>>);

    impl Stops {
        fn hold(&self, slot: &Slot) {
            lock(&self.0).push(slot.stopper().unwrap());
        }
    }

    impl Drop for Stops {
        fn drop(&mut self) {
            for stopper in lock(&self.0).iter() {
                let _ = stopper.stop();
            }
        }
    }

    /// A list's waiting slots are those made on it that no client has taken, counted
    /// without taking any: each list apart, one fewer as a client takes one.
    #[test]
    fn a_lists_waiting_slots_are_those_no_client_took() {
        let home = home("slot-waiting");
        let mut requests = Requests::open(&home).unwrap();
        assert_eq!(requests.waiting(false).unwrap(), 0);
        let slots: Vec<Slot> = (0..2).map(|_| requests.make(false).unwrap()).collect();
        let with = requests.make(true).unwrap();
        for (n, streams) in [(0, false), (1, false), (2, true)] {
            requests.offer(n, streams).unwrap();
        }
        assert_eq!(
            (requests.waiting(false).unwrap(), requests.waiting(true).unwrap()),
            (2, 1)
        );
        // Counted again, none taken by the count.
        assert_eq!(requests.waiting(false).unwrap(), 2);
        let stoppers: Vec<Stopper> = slots
            .iter()
            .chain([&with])
            .map(|s| s.stopper().unwrap())
            .collect();
        std::thread::scope(|s| {
            let waits: Vec<_> = slots
                .into_iter()
                .chain([with])
                .map(|slot| waiting(s, slot))
                .collect();
            let Taken::Slot(client) = take(&home, 0).unwrap() else {
                panic!("no slot");
            };
            assert_eq!(
                (requests.waiting(false).unwrap(), requests.waiting(true).unwrap()),
                (1, 1)
            );
            drop(client);
            for stopper in &stoppers {
                stopper.stop().unwrap();
            }
            for wait in waits {
                wait.join().unwrap().unwrap();
            }
        });
    }

    /// Clients waiting on one list each take a slot of their own as instances come, however
    /// many wait: the maker keeps two offered, and makes another as each is taken, as a
    /// daemon's does. None is taken twice.
    #[test]
    fn clients_waiting_on_one_list_each_take_a_slot() {
        const CLIENTS: usize = 16;
        const TAKES: usize = 100;
        let home = home("slot-herd");
        let (taken, longest) = std::thread::scope(|s| {
            let mut requests = Requests::open(&home).unwrap();
            let (took, told) = mpsc::channel();
            let clients: Vec<_> = (0..CLIENTS)
                .map(|_| {
                    let took = took.clone();
                    let home = &home;
                    s.spawn(move || {
                        for _ in 0..TAKES {
                            let t0 = Instant::now();
                            let Taken::Slot(client) = take(home, 0).unwrap() else {
                                return;
                            };
                            let waited = t0.elapsed();
                            let n = client.n;
                            drop(client);
                            if took.send((n, waited)).is_err() {
                                return;
                            }
                        }
                    })
                })
                .collect();
            drop(took);
            // Each slot's thread, as the daemon's: it says its number to its client, and
            // ends as that client goes without a word.
            let stops = Stops::default();
            let mut offered = HashMap::new();
            let offer = |requests: &mut Requests, offered: &mut HashMap<u64, _>| {
                let slot = requests.make(false).unwrap();
                stops.hold(&slot);
                let n = slot.number();
                offered.insert(n, (slot.stopper().unwrap(), waiting(s, slot)));
            };
            for _ in 0..OFFERED {
                offer(&mut requests, &mut offered);
            }
            let deadline = Instant::now() + Duration::from_secs(60);
            let mut taken = 0;
            let mut longest = Duration::ZERO;
            while taken < CLIENTS * TAKES {
                let left = deadline.saturating_duration_since(Instant::now());
                let Ok((n, waited)) = told.recv_timeout(left) else {
                    break;
                };
                taken += 1;
                longest = longest.max(waited);
                let (_, served) = offered.remove(&n).expect("a slot taken twice, or never made");
                assert!(matches!(served.join().unwrap().unwrap(), Waited::Gone));
                requests.remove(n);
                while requests.waiting(false).unwrap() < OFFERED && offered.len() < CLIENTS + OFFERED {
                    offer(&mut requests, &mut offered);
                }
            }
            unoffer(&home);
            for (_, (stopper, served)) in offered {
                stopper.stop().unwrap();
                served.join().unwrap().unwrap();
            }
            drop(requests);
            for client in clients {
                client.join().unwrap();
            }
            (taken, longest)
        });
        assert_eq!(taken, CLIENTS * TAKES, "only {taken} slots taken in 60 s");
        assert!(
            longest < Duration::from_secs(5),
            "a client waited {longest:?} for a slot"
        );
    }

    /// Clients waiting on one list, every instance taken, wake as each comes however fast
    /// the others take theirs (fifo.rs's twin, of PM M262's sleepers): for 2 s 16 clients
    /// take slots as fast as they come; then each takes one more and ends. One that no
    /// longer wakes leaves its slots untaken.
    #[test]
    fn clients_waiting_on_one_list_wake_for_its_instances() {
        const CLIENTS: usize = 16;
        let home = home("slot-wake");
        let mut requests = Requests::open(&home).unwrap();
        // What clients say: `true` for each slot taken, `false` as one ends.
        let (say, heard) = mpsc::channel::<bool>();
        let lasts: Vec<_> = (0..CLIENTS)
            .map(|_| {
                let (last, is_last) = mpsc::channel::<()>();
                let (home, say) = (home.to_path_buf(), say.clone());
                // Not scoped: a client that no longer wakes is left to the test process's
                // exit, the test failed, rather than waited for for ever.
                std::thread::spawn(move || {
                    while let Ok(Taken::Slot(client)) = take(&home, 0) {
                        drop(client);
                        if say.send(true).is_err() {
                            break;
                        }
                        if !matches!(is_last.try_recv(), Err(mpsc::TryRecvError::Empty)) {
                            break;
                        }
                    }
                    let _ = say.send(false);
                });
                last
            })
            .collect();
        drop(say);
        // What the clients did in the first 2 s, and then; and whether they slept through
        // slots for 5 s. Said once every slot's thread has ended.
        let (mut first, mut ended, mut slept) = ((0, 0), 0, None);
        std::thread::scope(|s| {
            let stops = Stops::default();
            // Each slot's end, as its thread has it: its maker lets go of it then, as a
            // daemon's does (`Event::Ended`).
            let (done, gone) = mpsc::channel::<u64>();
            // Up to OFFERED waiting, counted once a look: clients that take each instance as
            // it is made would keep a maker that counted again after each from ever looking at
            // its clock.
            let offer = |requests: &mut Requests| {
                while let Ok(n) = gone.try_recv() {
                    requests.remove(n);
                }
                for _ in requests.waiting(false).unwrap()..OFFERED {
                    let slot = requests.make(false).unwrap();
                    stops.hold(&slot);
                    let (n, done) = (slot.number(), done.clone());
                    s.spawn(move || {
                        let _ = slot.wait();
                        let _ = done.send(n);
                    });
                }
            };
            let (mut taken, mut quit) = (0, 0);
            let until = Instant::now() + Duration::from_secs(2);
            while Instant::now() < until {
                offer(&mut requests);
                match heard.recv_timeout(Duration::from_millis(100)) {
                    Ok(true) => taken += 1,
                    Ok(false) => quit += 1,
                    Err(_) => {}
                }
            }
            first = (taken, quit);
            if quit > 0 {
                return;
            }
            for last in &lasts {
                last.send(()).unwrap();
            }
            let mut since = Instant::now();
            while ended < CLIENTS {
                offer(&mut requests);
                match heard.recv_timeout(Duration::from_millis(100)) {
                    Ok(took) => {
                        since = Instant::now();
                        if took {
                            taken += 1;
                        } else {
                            ended += 1;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) if since.elapsed() >= Duration::from_secs(5) => {
                        slept = Some(format!(
                            "{} of {CLIENTS} clients slept through slots for 5 s, {taken} taken",
                            CLIENTS - ended
                        ));
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            unoffer(&home);
        });
        drop(requests);
        assert_eq!(first.1, 0, "clients ended as they took slots");
        assert!(first.0 >= 10 * CLIENTS, "only {} slots taken", first.0);
        assert_eq!(slept, None);
        assert_eq!(ended, CLIENTS, "clients gone unsaid");
    }

    /// A client waiting on a list every instance of which is taken leaves it as its daemon
    /// makes its lists anew, and takes a slot of the new: fifo.rs's macOS case of a list
    /// whose name went, which a client waiting on it must see.
    #[test]
    fn a_waiting_client_leaves_a_list_made_anew() {
        let home = home("slot-renewed");
        let mut requests = Requests::open(&home).unwrap();
        std::thread::scope(|s| {
            let client = s.spawn(|| take(&home, 0).unwrap());
            std::thread::sleep(Duration::from_millis(100));
            assert!(
                !client.is_finished(),
                "a client with no slot offered did not wait"
            );
            requests.renew().unwrap();
            let slot = requests.make(false).unwrap();
            let served = waiting(s, slot);
            let Taken::Slot(client) = client.join().unwrap() else {
                panic!("no slot of the new list");
            };
            assert_eq!(client.n, 0);
            drop(client);
            assert!(matches!(served.join().unwrap().unwrap(), Waited::Gone));
        });
    }

    /// With every slot taken a client waits for the next offered; one stopped since it was
    /// offered is passed over. One that a client opened and ended unwritten is gone, its
    /// instance's client gone, where a FIFO's would wait on as fresh (fifo.rs): its maker
    /// makes another.
    #[test]
    fn a_client_waits_for_a_slot_and_passes_over_a_removed_one() {
        let home = home("slot-wait");
        let mut requests = Requests::open(&home).unwrap();
        let gone = requests.make(false).unwrap();
        requests.offer(gone.number(), false).unwrap();
        assert_eq!(requests.untaken(false).unwrap(), [0]);
        // Removed, its wait ended, as a sweep removes one.
        requests.remove(gone.number());
        gone.stopper().unwrap().stop().unwrap();
        assert!(matches!(gone.wait().unwrap(), Waited::Stopped));
        std::thread::scope(|s| {
            let client = s.spawn(|| take(&home, 0).unwrap());
            std::thread::sleep(Duration::from_millis(50));
            assert!(!client.is_finished(), "a client took a removed slot");
            let next = requests.make(false).unwrap();
            requests.offer(next.number(), false).unwrap();
            let served = waiting(s, next);
            let Taken::Slot(client) = client.join().unwrap() else {
                panic!("no slot");
            };
            assert_eq!(client.n, 1);
            drop(client);
            assert!(matches!(served.join().unwrap().unwrap(), Waited::Gone));
        });
    }

    /// A first byte naming a stream its client did not open is refused, as one naming a
    /// stream its slot has not.
    #[test]
    fn a_stream_said_and_not_opened_is_refused() {
        let home = home("slot-unopened");
        let mut requests = Requests::open(&home).unwrap();
        let slot = requests.make(true).unwrap();
        std::thread::scope(|s| {
            let served = waiting(s, slot);
            let Taken::Slot(client) = take(&home, streams::IN).unwrap() else {
                panic!("no slot");
            };
            // It opened `in` alone, and says `out` too.
            (&client.say).write_all(&[streams::IN | streams::OUT]).unwrap();
            let e = served.join().unwrap().unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        });
    }

    /// The test binary, run again as one of these tests' processes (`as_process`): a client
    /// that takes a slot of `SHARDS_SLOT_HOME` and writes `SHARDS_SLOT_HEAD`'s bytes (hex),
    /// or a daemon that holds its lists; then waits to be killed.
    #[test]
    fn a_slot_process() {
        let Some(home) = std::env::var_os("SHARDS_SLOT_HOME") else {
            return;
        };
        let home = PathBuf::from(home);
        match std::env::var("SHARDS_SLOT_HEAD") {
            Ok(head) => {
                let bytes: Vec<u8> = (0..head.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(head.get(i..i + 2).unwrap(), 16).unwrap())
                    .collect();
                let Taken::Slot(client) = take(&home, 0).unwrap() else {
                    panic!("no slot");
                };
                (&client.say).write_all(&bytes).unwrap();
                let _ = writeln!(io::stdout(), "said");
                std::thread::sleep(Duration::from_secs(600));
            }
            Err(_) => {
                let _requests = Requests::open(&home).unwrap();
                let _ = writeln!(io::stdout(), "serving");
                std::thread::sleep(Duration::from_secs(600));
            }
        }
    }

    /// This test binary run as [`a_slot_process`], for `home`, a client writing `head`
    /// where given, else a daemon; returned once it has said it is ready.
    fn as_process(home: &Path, head: Option<&[u8]>) -> Child {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "slots::named::tests::a_slot_process",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("SHARDS_SLOT_HOME", home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(head) = head {
            command.env(
                "SHARDS_SLOT_HEAD",
                head.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            );
        }
        let mut child = command.spawn().unwrap();
        let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        while std::io::BufRead::read_line(&mut out, &mut line).unwrap() > 0 {
            if line.contains("said") || line.contains("serving") {
                return child;
            }
            line.clear();
        }
        let status = child.wait();
        panic!("the slot process ended unready: {status:?}");
    }

    /// A client process killed mid-session ends the daemon's read of it; one killed before
    /// its request is whole is gone.
    #[test]
    fn a_killed_client_ends_its_session() {
        let home = home("slot-killed");
        let mut requests = Requests::open(&home).unwrap();
        // Whole: no streams, no descriptors, kind 7, two bytes, "ps".
        let slot = requests.make(false).unwrap();
        let mut whole = None;
        std::thread::scope(|s| {
            let served = s.spawn(|| asked(slot));
            whole = Some(as_process(&home, Some(b"\0\0\x07\0\0\0\x02ps")));
            let Asked { session, request, .. } = served.join().unwrap();
            assert_eq!(
                request,
                Frame {
                    kind: 7,
                    payload: b"ps".to_vec()
                }
            );
            let mut whole = whole.take().unwrap();
            whole.kill().unwrap();
            let _ = whole.wait();
            assert_eq!(session.recv().unwrap(), None);
        });
        // Cut short: a header that says ten bytes, and three of them.
        let slot = requests.make(false).unwrap();
        std::thread::scope(|s| {
            let opened = s.spawn(|| {
                let Waited::Opened(opened) = slot.wait().unwrap() else {
                    panic!("no client opened the slot");
                };
                opened.request().unwrap()
            });
            let mut cut = as_process(&home, Some(b"\0\0\x07\0\0\0\x0aabc"));
            std::thread::sleep(Duration::from_millis(100));
            cut.kill().unwrap();
            let _ = cut.wait();
            assert!(opened.join().unwrap().is_none(), "a request cut short was taken");
        });
    }

    /// A daemon process killed, its lists' names left, is heard of by a client waiting on
    /// its list at once: its instances close with it, and a wait ends as the last does (PM
    /// M348). fifo.rs's macOS twin: a list whose daemon is gone, its PID no help.
    #[test]
    fn a_killed_daemons_waiting_client_hears_it_is_gone() {
        let home = home("slot-killed-daemon");
        let mut daemon = as_process(&home, None);
        std::thread::scope(|s| {
            let client = s.spawn(|| {
                let t0 = Instant::now();
                (take(&home, 0).unwrap(), t0.elapsed())
            });
            std::thread::sleep(Duration::from_millis(100));
            assert!(
                !client.is_finished(),
                "a client with no slot offered did not wait"
            );
            let t0 = Instant::now();
            daemon.kill().unwrap();
            let _ = daemon.wait();
            let (taken, _) = client.join().unwrap();
            assert!(matches!(taken, Taken::NoDaemon));
            assert!(
                t0.elapsed() < Duration::from_secs(5),
                "heard {:?} after",
                t0.elapsed()
            );
        });
        assert!(!served(&home));
    }
}
