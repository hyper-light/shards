//! A member's stdin as a stream its owner writes (docs/research/runtime-image-boot-decision-v1.md
//! §3.3, managed-runtime-plan.md P3's input policy). Asked for at create, the member's fd 0 is
//! the read end of a pipe whose write end init holds, and its owner's bytes come on a
//! connection of the member's own, which init dials to the exec port saying the token the VM
//! process minted for it. They come as `STDIN` frames, an empty one its owner's end; init
//! writes each byte to the pipe as it came, and reads the connection again only once all it
//! read has gone into the pipe, so the member's reading paces its owner's writing through
//! vsock's own credit, and a stdin left unread holds up nothing else. However the input ends,
//! init closes the pipe, so the member reads its end, and tells the owner how it ended and
//! how many bytes the member's stdin took ([`managed::InputEnded`]): its owner's end, the
//! member no longer reading, or the connection failing, refused or ending first.
//!
//! A member with a terminal of its own (plan step 6, D244) has its terminal for its stdin:
//! its owner's bytes are written to the terminal's master instead, as keys typed, and its
//! input's end leaves the terminal open, as a terminal's typist going does.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use shards_abi::run::{
    self, kind,
    managed::{InputEnd, InputEnded},
};

/// Where the frame being read is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Frame {
    /// Its header, `got` bytes of it so far.
    Header { bytes: [u8; run::HEADER], got: usize },
    /// Its payload, `left` bytes of it still to go into the pipe.
    Payload { left: u32 },
}

const HEADER_NEW: Frame = Frame::Header {
    bytes: [0; run::HEADER],
    got: 0,
};

/// One input's states: `Dialing → Hello → Relaying`, every way out of each `Ended`, which
/// closes the pipe and the connection at once and owes the report; `Told` once it is queued.
/// The host says nothing on the connection before its owner's first bytes, so a loss before
/// them is told as any other (its errno, `ECONNRESET` for an end): the host may never have
/// had it, and the report reaches the owner on the control connection all the same.
#[derive(Debug)]
enum State {
    /// Its nonblocking connect in progress, its hello to follow.
    Dialing {
        hello: [u8; run::HEADER + run::TOKEN],
    },
    /// Connected: its hello, `written` bytes of it sent.
    Hello {
        bytes: [u8; run::HEADER + run::TOKEN],
        written: usize,
    },
    /// Its owner's bytes go into the pipe.
    Relaying,
    Ended(InputEnd),
    Told,
}

/// What the owner's bytes are written to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sink {
    /// A pipe whose read end is the member's stdin.
    Pipe,
    /// The master of the member's own terminal.
    Terminal,
}

#[derive(Debug)]
pub struct Input {
    member: u32,
    /// The pipe's write end, nonblocking: the member holds its read end. With a terminal,
    /// a descriptor of its master.
    pipe: Option<OwnedFd>,
    sink: Sink,
    /// The member's input connection, nonblocking.
    conn: Option<File>,
    state: State,
    /// What one read of the connection took, as long as the pipe holds at once.
    buffer: Vec<u8>,
    /// How much of `buffer` the last read filled, and how much of that has been taken.
    filled: usize,
    taken: usize,
    frame: Frame,
    /// The bytes the member's stdin has taken.
    written: u64,
}

impl Input {
    /// The member's input, and the read end of its stdin for its first process. The
    /// connection's dial failing at once ends the input here, typed: the member then reads
    /// its end at once, and its owner is told why.
    pub fn open(member: u32, token: &[u8; run::TOKEN]) -> io::Result<(Self, OwnedFd)> {
        let mut fds = [-1; 2];
        // SAFETY: pipe2 writes exactly the two descriptors in fds.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let [read, write] = fds;
        // SAFETY: both descriptors are fresh and uniquely owned.
        let (read, write) = unsafe { (OwnedFd::from_raw_fd(read), OwnedFd::from_raw_fd(write)) };
        // Init's end alone is nonblocking: the status flags are each end's own open file
        // description's (pipe(7)), and the member reads its stdin as it would anywhere.
        // SAFETY: fcntl of the fresh pipe's write end.
        if unsafe { libc::fcntl(write.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((Self::dialing(member, token, write, Sink::Pipe)?, read))
    }

    /// The input of a member with a terminal of its own (plan step 6, D244), written to
    /// `master`, a descriptor of the terminal's master, made nonblocking (as every
    /// descriptor of it is: its status flags are its open file's).
    pub fn terminal(member: u32, token: &[u8; run::TOKEN], master: OwnedFd) -> io::Result<Self> {
        // SAFETY: fcntl of the master's descriptor, which this owns.
        if unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Self::dialing(member, token, master, Sink::Terminal)
    }

    /// The input writing to `pipe`, its connection dialed.
    fn dialing(member: u32, token: &[u8; run::TOKEN], pipe: OwnedFd, sink: Sink) -> io::Result<Self> {
        let mut hello = [0; run::HEADER + run::TOKEN];
        let (header, bytes) = hello
            .split_first_chunk_mut::<{ run::HEADER }>()
            .ok_or_else(|| io::Error::other("input hello header"))?;
        *header = run::header(kind::HELLO, run::TOKEN as u32);
        bytes.copy_from_slice(token);
        let mut input = Self {
            member,
            pipe: Some(pipe),
            sink,
            conn: None,
            state: State::Dialing { hello },
            buffer: Vec::new(),
            filled: 0,
            taken: 0,
            frame: HEADER_NEW,
            written: 0,
        };
        match crate::run::dial(run::EXEC_PORT, false) {
            Ok(conn) => input.conn = Some(conn),
            Err(e) => input.end(aborted(&e)),
        }
        Ok(input)
    }

    /// The connection, and what to wait for of it: its hello's room to go, or bytes once all
    /// read before have gone into the pipe.
    pub fn conn(&self) -> (Option<RawFd>, libc::c_short) {
        let fd = self.conn.as_ref().map(AsRawFd::as_raw_fd);
        let events = match self.state {
            State::Dialing { .. } | State::Hello { .. } => libc::POLLOUT,
            State::Relaying if !self.pending() => libc::POLLIN,
            _ => 0,
        };
        (fd, events)
    }

    /// The pipe, and what to wait for of it: room, while read bytes wait for it; else its
    /// reader going, which poll(2) tells as POLLERR whatever is asked (fs/pipe.c `pipe_poll`),
    /// so that a member no longer reading its stdin ends its input whether or not its owner
    /// writes. POLLERR is asked only so that the poll set holds the pipe.
    pub fn pipe(&self) -> (Option<RawFd>, libc::c_short) {
        let fd = self.pipe.as_ref().map(AsRawFd::as_raw_fd);
        let events = match self.state {
            State::Relaying if self.pending() => libc::POLLOUT,
            State::Relaying => libc::POLLERR,
            _ => 0,
        };
        (fd, events)
    }

    /// Payload bytes read and not yet in the pipe.
    fn pending(&self) -> bool {
        matches!(self.frame, Frame::Payload { left } if left > 0) && self.taken < self.filled
    }

    /// The pipe's events: its room, or its reader gone, which a pipe says as POLLERR and a
    /// terminal's master as POLLHUP once its last other end has closed (Linux
    /// drivers/tty/n_tty.c, `n_tty_poll`); a pipe's write end is never hung up.
    pub fn on_pipe(&mut self, revents: libc::c_short) {
        if revents & (libc::POLLERR | libc::POLLHUP) != 0 && !self.pending() {
            self.end(InputEnd::Aborted(libc::EPIPE));
            return;
        }
        self.progress();
    }

    /// Goes on as far as it can without waiting: the hello, then each byte read into the
    /// pipe, reading again once it is empty.
    pub fn progress(&mut self) {
        loop {
            let went = match self.state {
                State::Dialing { hello } => self.dialed(hello),
                State::Hello { .. } => self.hello(),
                State::Relaying => self.relay(),
                State::Ended(_) | State::Told => return,
            };
            match went {
                Ok(true) => {}
                Ok(false) => return,
                Err(end) => {
                    self.end(end);
                    return;
                }
            }
        }
    }

    /// Its connect's outcome, once its socket is writable (connect(2), EINPROGRESS): its
    /// hello next, or its errno.
    fn dialed(&mut self, hello: [u8; run::HEADER + run::TOKEN]) -> Result<bool, InputEnd> {
        let fd = self
            .conn
            .as_ref()
            .map(AsRawFd::as_raw_fd)
            .ok_or(InputEnd::Aborted(libc::EBADF))?;
        crate::run::connect_result(fd).map_err(|e| aborted(&e))?;
        self.state = State::Hello {
            bytes: hello,
            written: 0,
        };
        Ok(true)
    }

    /// Sends what is left of the hello; once all of it has gone, the relay's buffer is made,
    /// as large as the pipe holds at once.
    fn hello(&mut self) -> Result<bool, InputEnd> {
        let fd = self
            .conn
            .as_ref()
            .map(AsRawFd::as_raw_fd)
            .ok_or(InputEnd::Aborted(libc::EBADF))?;
        let State::Hello { bytes, written } = &mut self.state else {
            return Ok(false);
        };
        let rest = bytes.get(*written..).ok_or(InputEnd::Aborted(libc::EPROTO))?;
        let n = crate::run::write(fd, rest).map_err(|e| aborted(&e))?;
        if n == 0 {
            return Ok(false);
        }
        *written = written.checked_add(n).ok_or(InputEnd::Aborted(libc::EOVERFLOW))?;
        if *written < bytes.len() {
            return Ok(true);
        }
        let pipe = self.pipe.as_ref().ok_or(InputEnd::Aborted(libc::EBADF))?;
        let size = match self.sink {
            Sink::Pipe => {
                // SAFETY: fcntl of the pipe's write end, which init holds.
                let size = unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_GETPIPE_SZ) };
                usize::try_from(size).map_err(|_| aborted(&io::Error::last_os_error()))?
            }
            // A terminal has no pipe's size to ask: a frame at once, the most one carries
            // (run::CHUNK) after its header, the master's own buffer taking what it can.
            Sink::Terminal => run::HEADER + run::CHUNK,
        };
        if size == 0 {
            return Err(InputEnd::Aborted(libc::EINVAL));
        }
        self.buffer
            .try_reserve_exact(size)
            .map_err(|_| InputEnd::Aborted(libc::ENOMEM))?;
        self.buffer.resize(size, 0);
        self.state = State::Relaying;
        Ok(true)
    }

    /// One step of the relay: payload into the pipe, a header read, or the connection read
    /// once all it gave before is taken. `Ok(false)` waits for the pipe or the connection.
    fn relay(&mut self) -> Result<bool, InputEnd> {
        match self.frame {
            Frame::Payload { left: 0 } => {
                self.frame = HEADER_NEW;
                Ok(true)
            }
            Frame::Payload { left } if self.taken < self.filled => {
                let room = self.filled.saturating_sub(self.taken).min(left as usize);
                let bytes = self
                    .buffer
                    .get(self.taken..self.taken.saturating_add(room))
                    .ok_or(InputEnd::Aborted(libc::EPROTO))?;
                let pipe = self.pipe.as_ref().ok_or(InputEnd::Aborted(libc::EBADF))?;
                let n = crate::run::write(pipe.as_raw_fd(), bytes).map_err(|e| aborted(&e))?;
                if n == 0 {
                    return Ok(false);
                }
                let wrote = u32::try_from(n).map_err(|_| InputEnd::Aborted(libc::EOVERFLOW))?;
                self.taken = self.taken.saturating_add(n);
                self.written = self
                    .written
                    .checked_add(u64::from(wrote))
                    .ok_or(InputEnd::Aborted(libc::EOVERFLOW))?;
                self.frame = Frame::Payload {
                    left: left.saturating_sub(wrote),
                };
                Ok(true)
            }
            Frame::Header { mut bytes, got } if self.taken < self.filled => {
                let want = run::HEADER
                    .saturating_sub(got)
                    .min(self.filled.saturating_sub(self.taken));
                let from = self
                    .buffer
                    .get(self.taken..self.taken.saturating_add(want))
                    .ok_or(InputEnd::Aborted(libc::EPROTO))?;
                bytes
                    .get_mut(got..got.saturating_add(want))
                    .ok_or(InputEnd::Aborted(libc::EPROTO))?
                    .copy_from_slice(from);
                self.taken = self.taken.saturating_add(want);
                let got = got.saturating_add(want);
                if got < run::HEADER {
                    self.frame = Frame::Header { bytes, got };
                    return Ok(true);
                }
                match run::parse_header(bytes) {
                    // Its owner's end: every byte before it is in the pipe already.
                    Some((kind::STDIN, 0)) => Err(InputEnd::Closed),
                    Some((kind::STDIN, len)) => {
                        self.frame = Frame::Payload { left: len };
                        Ok(true)
                    }
                    _ => Err(InputEnd::Aborted(libc::EPROTO)),
                }
            }
            // All it read is taken: read again.
            _ => {
                let conn = self.conn.as_ref().ok_or(InputEnd::Aborted(libc::EBADF))?;
                match crate::run::read(conn.as_raw_fd(), &mut self.buffer) {
                    // Ended without its owner's end: refused, dropped or cut short.
                    Ok(0) => Err(InputEnd::Aborted(libc::ECONNRESET)),
                    Ok(n) => {
                        self.filled = n;
                        self.taken = 0;
                        Ok(true)
                    }
                    Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {
                        Ok(false)
                    }
                    Err(e) => Err(aborted(&e)),
                }
            }
        }
    }

    /// Ends the input as `end` says, if it has not ended: the pipe closed, so the member
    /// reads its end, and the connection with it; its report owed.
    fn end(&mut self, end: InputEnd) {
        if matches!(self.state, State::Ended(_) | State::Told) {
            return;
        }
        self.pipe = None;
        self.conn = None;
        self.buffer = Vec::new();
        self.state = State::Ended(end);
    }

    /// Its report, once it has ended and it is not yet told; `told` then marks it so.
    pub fn report(&self) -> Option<InputEnded> {
        match self.state {
            State::Ended(end) => Some(InputEnded {
                member: self.member,
                written: self.written,
                end,
            }),
            _ => None,
        }
    }

    pub fn told(&mut self) {
        if matches!(self.state, State::Ended(_)) {
            self.state = State::Told;
        }
    }
}

/// An error's errno as the input's end; one without its own (none the kernel gave) is EIO.
fn aborted(error: &io::Error) -> InputEnd {
    InputEnd::Aborted(error.raw_os_error().filter(|n| *n > 0).unwrap_or(libc::EIO))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    /// A pipe as `open` makes one: its read end, and its write end nonblocking.
    fn pipe() -> (File, OwnedFd) {
        let mut fds = [-1; 2];
        // SAFETY: pipe2 writes exactly the two descriptors in fds.
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        // SAFETY: both descriptors are fresh and uniquely owned.
        let (read, write) = unsafe { (File::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        // SAFETY: fcntl of the fresh pipe's write end.
        assert!(unsafe { libc::fcntl(write.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } >= 0);
        (read, write)
    }

    fn pipe_size(write: &OwnedFd) -> usize {
        // SAFETY: fcntl of a pipe's write end this test holds.
        usize::try_from(unsafe { libc::fcntl(write.as_raw_fd(), libc::F_GETPIPE_SZ) }).unwrap()
    }

    fn input(member: u32, conn: UnixStream, write: OwnedFd, state: State) -> Input {
        conn.set_nonblocking(true).unwrap();
        let buffer = if matches!(state, State::Relaying) {
            vec![0; pipe_size(&write)]
        } else {
            Vec::new()
        };
        Input {
            member,
            pipe: Some(write),
            sink: Sink::Pipe,
            conn: Some(File::from(OwnedFd::from(conn))),
            state,
            buffer,
            filled: 0,
            taken: 0,
            frame: HEADER_NEW,
            written: 0,
        }
    }

    /// An input relaying from the returned peer into a pipe whose reader is returned, as
    /// `open` leaves it once its hello has gone.
    fn relaying() -> (Input, UnixStream, File) {
        let (conn, peer) = UnixStream::pair().unwrap();
        let (read, write) = pipe();
        (input(3, conn, write, State::Relaying), peer, read)
    }

    fn stdin_frame(payload: &[u8]) -> Vec<u8> {
        [&run::header(kind::STDIN, payload.len() as u32)[..], payload].concat()
    }

    #[test]
    fn binary_no_newline_and_invalid_utf8_reach_the_pipe_exactly_and_its_end_closes_it() {
        let (mut input, mut peer, mut stdin) = relaying();
        let payload = b"\0\xff\x80{\"a\":1}\n\x1b[2Jno-newline\xc3";
        // Split across frames and across reads: a frame's header and payload never need to
        // come whole.
        let mut wire = stdin_frame(&payload[..5]);
        wire.extend(stdin_frame(&payload[5..]));
        for part in wire.chunks(3) {
            peer.write_all(part).unwrap();
            input.progress();
        }
        assert_eq!(input.report(), None, "no end before its owner's");
        peer.write_all(&stdin_frame(b"")).unwrap();
        input.progress();
        assert_eq!(
            input.report(),
            Some(InputEnded {
                member: 3,
                written: payload.len() as u64,
                end: InputEnd::Closed,
            })
        );
        let mut got = Vec::new();
        stdin.read_to_end(&mut got).unwrap();
        assert_eq!(
            got, payload,
            "the member reads exactly its owner's bytes, then EOF"
        );
        input.told();
        assert_eq!(input.report(), None, "told once");
    }

    #[test]
    fn an_unread_stdin_stops_reading_the_connection_and_resumes_as_it_is_read() {
        let (mut input, mut peer, mut stdin) = relaying();
        let size = input.buffer.len();
        // Twice what the pipe holds and more, in one frame: the pipe fills, and the
        // connection is not read past what init holds.
        let payload: Vec<u8> = (0..size * 2 + 7).map(|i| (i % 251) as u8).collect();
        let wire = stdin_frame(&payload);
        peer.set_nonblocking(true).unwrap();
        // SAFETY: fcntl of the pipe's read end this test holds.
        assert!(unsafe { libc::fcntl(stdin.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } >= 0);
        let (mut sent, mut got, mut stalled) = (0, Vec::new(), false);
        let mut buf = vec![0; 4096];
        while got.len() < payload.len() {
            if sent < wire.len()
                && let Ok(n) = peer.write(&wire[sent..])
            {
                sent += n;
            }
            input.progress();
            if input.conn().1 == 0 && input.pipe().1 == libc::POLLOUT {
                // Full: init waits for the member, reading nothing more meanwhile.
                stalled = true;
                assert!(input.pending());
            }
            match stdin.read(&mut buf) {
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("the member's stdin: {e}"),
            }
        }
        assert!(
            stalled,
            "a stdin left unread holds init's reading of the connection"
        );
        assert_eq!(got, payload);
        assert_eq!(input.written, payload.len() as u64);
        assert_eq!(input.report(), None);
    }

    #[test]
    fn a_member_that_stops_reading_ends_its_input_as_epipe_whether_or_not_its_owner_writes() {
        // Idle: the reader's going is the pipe's POLLERR.
        let (mut input, _peer, stdin) = relaying();
        assert_eq!(input.pipe().1, libc::POLLERR);
        drop(stdin);
        let (fd, events) = input.pipe();
        let mut set = [libc::pollfd {
            fd: fd.unwrap(),
            events,
            revents: 0,
        }];
        // SAFETY: poll(2) of one valid pollfd.
        assert_eq!(unsafe { libc::poll(set.as_mut_ptr(), 1, 0) }, 1);
        assert_ne!(set[0].revents & libc::POLLERR, 0);
        input.on_pipe(set[0].revents);
        assert_eq!(input.report().unwrap().end, InputEnd::Aborted(libc::EPIPE));
        assert!(input.pipe.is_none() && input.conn.is_none());
        // Writing: the pipe's write says EPIPE, the bytes it took counted.
        let (mut input, mut peer, mut stdin) = relaying();
        peer.write_all(&stdin_frame(b"first")).unwrap();
        input.progress();
        let mut got = [0; 5];
        stdin.read_exact(&mut got).unwrap();
        drop(stdin);
        peer.write_all(&stdin_frame(b"second")).unwrap();
        input.progress();
        assert_eq!(
            input.report(),
            Some(InputEnded {
                member: 3,
                written: 5,
                end: InputEnd::Aborted(libc::EPIPE),
            })
        );
    }

    /// A member's own terminal's master as its input's sink, raw at its other end so that
    /// its line discipline passes bytes as they are (termios(3), cfmakeraw).
    fn terminal_input(member: u32) -> (Input, UnixStream, File, File) {
        // SAFETY: geteuid(2) reads this process's effective uid.
        let (master, other) =
            crate::enter::terminal(run::Size::default(), unsafe { libc::geteuid() }).unwrap();
        // SAFETY: tcgetattr(3) and tcsetattr(3) of the terminal's other end, ours.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(other.as_raw_fd(), &raw mut t), 0);
            libc::cfmakeraw(&raw mut t);
            assert_eq!(libc::tcsetattr(other.as_raw_fd(), libc::TCSANOW, &raw const t), 0);
        }
        let held = File::from(master.try_clone().unwrap());
        let (conn, peer) = UnixStream::pair().unwrap();
        conn.set_nonblocking(true).unwrap();
        let input = Input {
            member,
            pipe: Some(master),
            sink: Sink::Terminal,
            conn: Some(File::from(OwnedFd::from(conn))),
            state: State::Relaying,
            buffer: vec![0; run::HEADER + run::CHUNK],
            filled: 0,
            taken: 0,
            frame: HEADER_NEW,
            written: 0,
        };
        (input, peer, File::from(other), held)
    }

    /// A member with a terminal of its own (plan step 6, D244): its owner's bytes are typed
    /// into the terminal as they came; its owner's end leaves the terminal open, its master
    /// held elsewhere still working; and its last other end closing ends an idle input as a
    /// pipe's reader going does.
    #[test]
    fn a_terminals_input_is_typed_into_it_and_its_hangup_ends_it() {
        let (mut input, mut peer, mut other, mut held) = terminal_input(4);
        let payload = b"\x1b[A\0\xffq";
        peer.write_all(&stdin_frame(payload)).unwrap();
        input.progress();
        let mut got = [0; 6];
        other.read_exact(&mut got).unwrap();
        assert_eq!(&got, payload);
        peer.write_all(&stdin_frame(b"")).unwrap();
        input.progress();
        assert_eq!(
            input.report(),
            Some(InputEnded {
                member: 4,
                written: payload.len() as u64,
                end: InputEnd::Closed,
            })
        );
        assert!(input.pipe.is_none());
        other.write_all(b"still").unwrap();
        let mut still = [0; 5];
        held.read_exact(&mut still).unwrap();
        assert_eq!(&still, b"still", "the terminal outlives its input");

        let (mut input, _peer, other, _held) = terminal_input(5);
        let (fd, events) = input.pipe();
        assert_eq!(events, libc::POLLERR);
        drop(other);
        let mut set = [libc::pollfd {
            fd: fd.unwrap(),
            events,
            revents: 0,
        }];
        // SAFETY: poll(2) of one valid pollfd.
        assert_eq!(unsafe { libc::poll(set.as_mut_ptr(), 1, 0) }, 1);
        assert_ne!(set[0].revents & libc::POLLHUP, 0);
        input.on_pipe(set[0].revents);
        assert_eq!(input.report().unwrap().end, InputEnd::Aborted(libc::EPIPE));
    }

    #[test]
    fn a_connection_that_ends_without_its_owners_end_or_breaks_protocol_is_told_typed() {
        // Cut short mid-frame: what came is in the pipe, and the end is no owner's end.
        let (mut input, mut peer, mut stdin) = relaying();
        let frame = stdin_frame(b"partial-and-more");
        peer.write_all(&frame[..run::HEADER + 7]).unwrap();
        drop(peer);
        input.progress();
        assert_eq!(
            input.report(),
            Some(InputEnded {
                member: 3,
                written: 7,
                end: InputEnd::Aborted(libc::ECONNRESET),
            })
        );
        let mut got = Vec::new();
        stdin.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"partial", "the member reads what came, then its end");
        // A frame of another kind.
        let (mut input, mut peer, _stdin) = relaying();
        peer.write_all(&run::header(kind::STDOUT, 1)).unwrap();
        input.progress();
        assert_eq!(input.report().unwrap().end, InputEnd::Aborted(libc::EPROTO));
        // Its header's zero bytes not zero: parse_header refuses it.
        let (mut input, mut peer, _stdin) = relaying();
        let mut bad = run::header(kind::STDIN, 1);
        bad[1] = 1;
        peer.write_all(&bad).unwrap();
        input.progress();
        assert_eq!(input.report().unwrap().end, InputEnd::Aborted(libc::EPROTO));
    }

    #[test]
    fn a_hello_that_cannot_go_is_an_ended_input_not_a_dropped_one() {
        // The hello's write refused: the connection's peer gone under it.
        let (conn, peer) = UnixStream::pair().unwrap();
        drop(peer);
        let (mut stdin, write) = pipe();
        let state = State::Hello {
            bytes: [7; run::HEADER + run::TOKEN],
            written: 0,
        };
        let mut input = input(1, conn, write, state);
        assert_eq!(input.conn().1, libc::POLLOUT);
        input.progress();
        assert_eq!(
            input.report(),
            Some(InputEnded {
                member: 1,
                written: 0,
                end: InputEnd::Aborted(libc::EPIPE),
            })
        );
        let mut got = Vec::new();
        stdin.read_to_end(&mut got).unwrap();
        assert!(got.is_empty(), "its member reads its end at once");
        // Ended once: a later end changes nothing.
        input.end(InputEnd::Closed);
        assert_eq!(input.report().unwrap().end, InputEnd::Aborted(libc::EPIPE));
    }

    #[test]
    fn the_hello_names_its_token_and_the_relay_is_as_large_as_the_pipe() {
        let (conn, mut peer) = UnixStream::pair().unwrap();
        let token = [0x5a; run::TOKEN];
        let (_stdin, write) = pipe();
        let size = pipe_size(&write);
        let mut hello = [0; run::HEADER + run::TOKEN];
        hello[..run::HEADER].copy_from_slice(&run::header(kind::HELLO, run::TOKEN as u32));
        hello[run::HEADER..].copy_from_slice(&token);
        // Dialing, its connect done: its hello goes, then its relay.
        let state = State::Dialing { hello };
        let mut input = input(0, conn, write, state);
        assert_eq!(input.conn().1, libc::POLLOUT);
        input.progress();
        let mut said = [0; run::HEADER + run::TOKEN];
        peer.read_exact(&mut said).unwrap();
        assert_eq!(said, hello);
        assert!(matches!(input.state, State::Relaying));
        assert_eq!(input.buffer.len(), size);
        assert_eq!(input.conn().1, libc::POLLIN);
    }
}
