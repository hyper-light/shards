//! A vsock connection's host side on Windows (D272): two byte-mode named pipe instances, one
//! each way, whose server ends this process holds. Its client reads the way down and writes
//! the way up, each a file of its own: a pipe has no half-close, and one file both ways makes
//! its client's write wait behind its own read (PM M609 (e)). The guest's end of its writes is
//! the way down closed, which its client reads after every byte, and it writes on up (M609 (h)).
//!
//! Each way's calls are overlapped and pended, one at a time, each with an OVERLAPPED and bytes
//! of the connection's own that the system has until the call finishes: a read into its buffer,
//! under way while nothing it read is held, so that its client's next bytes or end finish it;
//! and a write of bytes taken from its caller, which finishes once they are all in the pipe
//! (PM M603) and is never cancelled while the connection lives, as a cancelled write says
//! nothing of what its reader took (M603). [`Pipes::look`] takes what finished, without
//! waiting, whatever told of it: the device's completion port, under the key each pair's
//! handles were associated with ([`Pipes::attach`]), says only that something may have.
//!
//! A VM's listener holds one pair unconnected at a time ([`Listening`]), each instance's
//! ConnectNamedPipe under way, and its way down watched for its client's going while its way
//! up waits; its next pair comes once both ways of the last are open (D272).

use std::cell::UnsafeCell;
use std::io;
use std::os::windows::io::{AsRawHandle as _, OwnedHandle};

use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, ERROR_MORE_DATA, ERROR_PIPE_CONNECTED,
    ERROR_PIPE_NOT_CONNECTED, GetLastError, HANDLE,
};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::IO::{CancelIoEx, CreateIoCompletionPort, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, GetNamedPipeInfo, PIPE_SERVER_END};

/// Where a connection's reads stand (R04).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reading {
    /// A read under way into the buffer, which is the system's until it finishes.
    UnderWay,
    /// What the last read brought, `from..to` of the buffer, not taken yet.
    Holding { from: usize, to: usize },
    /// Its client ended its writes, and every byte was taken: what every read says from here on.
    Ended,
    /// A read failed with this error, which every read says from here on.
    Failed(u32),
}

/// Where a connection's writes stand (R04).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Writing {
    /// None under way: the next starts at once.
    Idle,
    /// A write of the buffer's bytes from `at` under way: they are the system's until it finishes.
    UnderWay { at: usize },
    /// A write under way, as [`UnderWay`](Self::UnderWay), and the way down closed once it finishes.
    Closing { at: usize },
    /// The way down closed: its client reads every byte written, then its end.
    Shut,
    /// A write failed with this error, which every write says from here on: its client's reads have ended.
    Failed(u32),
}

/// One overlapped call's slot: its OVERLAPPED, the system's while the call is under way, and
/// the manual-reset event the OVERLAPPED names, which the call's end sets. Named, an end is
/// the OVERLAPPED's at once, and its packet still goes to a port the handle is associated with
/// (an hEvent whose low bit is clear, GetQueuedCompletionStatus). With none, a handle associated
/// with a port has its calls' ends written to their OVERLAPPEDs, and the handle set, only as
/// their packets are taken from the port: a cancelled read's OVERLAPPED still said it was under
/// way 4.8 s on, where one named an event ended at once (docs/research/measurements/
/// vsock-pipes-port-cancel), so that a wait for an end there (`settle`) waited for ever on the
/// device's worker, the port's one taker.
struct Call {
    ov: UnsafeCell<OVERLAPPED>,
    ended: shards_ipc::Event,
}

impl Call {
    fn new() -> io::Result<Call> {
        Ok(Call {
            ov: UnsafeCell::new(OVERLAPPED::default()),
            ended: shards_ipc::Event::new()?,
        })
    }

    /// Its OVERLAPPED made fresh for its next call, naming its event: a ReadFile or WriteFile
    /// resets that as it begins (their documentation), and a ConnectNamedPipe's is its pair's
    /// own, used once.
    ///
    /// # Safety
    /// No call is under way with it.
    unsafe fn fresh(&self) -> *mut OVERLAPPED {
        let ov = self.ov.get();
        // SAFETY: no call is under way with it, its caller says: it is ours to write.
        unsafe {
            ov.write(OVERLAPPED {
                hEvent: self.ended.raw(),
                ..OVERLAPPED::default()
            });
        }
        ov
    }

    /// Its OVERLAPPED, as its last call left it.
    fn ov(&self) -> *mut OVERLAPPED {
        self.ov.get()
    }
}

/// A connection's two calls' slots.
struct Calls {
    read: Call,
    write: Call,
}

/// A vsock connection's host side on Windows: see the module's documentation.
pub struct Pipes {
    /// This process's end of the way down, which it writes and its client reads. None once
    /// closed: the guest's writes ended ([`Pipes::shut_down`]).
    down: Option<OwnedHandle>,
    /// This process's end of the way up, which its client writes and it reads. None once
    /// closed: the connection's reads ended ([`Pipes::end_reads`]).
    up: Option<OwnedHandle>,
    /// Boxed, so that a call under way keeps its OVERLAPPED where it is as the connection moves.
    calls: Box<Calls>,
    /// Where reads land, as long as the way up's inbound quota: one read takes what the pipe holds.
    into: Vec<u8>,
    /// What writes send: bytes taken from the caller, the system's while a write of them is under way.
    out: Vec<u8>,
    reading: Reading,
    writing: Writing,
    /// What its handles tell a completion port of their calls' ends under, once associated
    /// with one ([`Pipes::attach`]).
    key: Option<usize>,
}

// SAFETY: the OVERLAPPEDs and buffers are the system's only while their calls are under way, and
// otherwise touched only through `&mut self`; the handles are this process's, used from any thread.
unsafe impl Send for Pipes {}

impl std::fmt::Debug for Pipes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pipes")
            .field("reading", &self.reading)
            .field("writing", &self.writing)
            .finish_non_exhaustive()
    }
}

/// `code`, a Win32 error, as an `io::Error`.
fn error(code: u32) -> io::Error {
    io::Error::from_raw_os_error(i32::try_from(code).unwrap_or(i32::MAX))
}

/// What a read's error says: its client's end where the pipe's writer has gone, or a failure.
fn read_ended(code: u32) -> Reading {
    if code == ERROR_BROKEN_PIPE || code == ERROR_PIPE_NOT_CONNECTED {
        Reading::Ended
    } else {
        Reading::Failed(code)
    }
}

/// What the call on `handle` with `ov` came to, where it has finished: its bytes, or its error.
/// A message longer than a read's buffer, from an end in message-read mode, gives what fitted
/// and ERROR_MORE_DATA (ReadFile), which is a read of those bytes here.
fn outcome(handle: &OwnedHandle, ov: *mut OVERLAPPED) -> Option<Result<usize, u32>> {
    let mut moved = 0u32;
    // SAFETY: the OVERLAPPED of the call last started on `handle`, read and not waited for (bWait 0).
    if unsafe { GetOverlappedResult(handle.as_raw_handle(), ov, &mut moved, 0) } != 0 {
        return Some(Ok(usize::try_from(moved).unwrap_or(usize::MAX)));
    }
    // SAFETY: no arguments.
    match unsafe { GetLastError() } {
        ERROR_IO_INCOMPLETE => None,
        ERROR_MORE_DATA => Some(Ok(usize::try_from(moved).unwrap_or(usize::MAX))),
        code => Some(Err(code)),
    }
}

/// Cancels the call under way on `handle` in `call`, and waits for its end, cancelled or
/// finished: until then the system writes its OVERLAPPED, and its buffer.
fn settle(handle: &OwnedHandle, call: &Call) {
    let mut moved = 0u32;
    // SAFETY: the OVERLAPPED of the one call under way on `handle` with it, which names its
    // call's event ([`Call`]): waited for (bWait 1) on that, so the system is done with it and
    // its buffer once this returns.
    unsafe {
        CancelIoEx(handle.as_raw_handle(), call.ov());
        GetOverlappedResult(handle.as_raw_handle(), call.ov(), &mut moved, 1);
    }
}

/// What the way this process reads holds of its writer's bytes, as its maker gave it (read
/// back by GetNamedPipeInfo): a server end's inbound quota (CreateNamedPipeW's
/// `nInBufferSize`), its client's bytes; a client end's outbound quota (`nOutBufferSize`),
/// its server's, as a dial's way down is ([`Pipes::dialed`]). A pipe's two quotas are its
/// maker's, whichever end asks.
fn read_quota(up: &OwnedHandle) -> io::Result<usize> {
    let (mut flags, mut outbound, mut inbound) = (0u32, 0u32, 0u32);
    // SAFETY: a pipe end of ours; the flags and two quotas asked for are written to locals, its
    // instances not asked.
    let read = unsafe {
        GetNamedPipeInfo(
            up.as_raw_handle(),
            &mut flags,
            &mut outbound,
            &mut inbound,
            std::ptr::null_mut(),
        )
    };
    if read == 0 {
        return Err(io::Error::last_os_error());
    }
    let quota = if flags & PIPE_SERVER_END != 0 {
        inbound
    } else {
        outbound
    };
    match usize::try_from(quota) {
        Ok(quota) if quota > 0 => Ok(quota),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a way read with no quota to read into",
        )),
    }
}

/// Associates `handle`, opened for overlapped calls, with the completion port `port`, which
/// is told of each call's end on it under `key` from here on, for the handle's life.
fn associate(handle: &OwnedHandle, port: HANDLE, key: usize) -> io::Result<()> {
    // SAFETY: a handle of ours and a port its caller holds open; the port's own handle comes
    // back, no new one, its threads unasked as it exists.
    let made = unsafe { CreateIoCompletionPort(handle.as_raw_handle(), port, key, 0) };
    if made.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl Pipes {
    /// The connection whose way down is `down` and way up `up`: server ends of byte-mode pipe
    /// instances opened for overlapped calls, each connected to its client. Its first read
    /// starts at once.
    pub fn new(down: OwnedHandle, up: OwnedHandle) -> io::Result<Pipes> {
        let quota = read_quota(&up)?;
        let mut into = Vec::new();
        into.try_reserve_exact(quota)
            .map_err(|e| io::Error::new(io::ErrorKind::OutOfMemory, e.to_string()))?;
        into.resize(quota, 0);
        let mut pipes = Pipes {
            down: Some(down),
            up: Some(up),
            calls: Box::new(Calls {
                read: Call::new()?,
                write: Call::new()?,
            }),
            into,
            out: Vec::new(),
            reading: Reading::Holding { from: 0, to: 0 },
            writing: Writing::Idle,
            key: None,
        };
        pipes.read_next();
        Ok(pipes)
    }

    /// A dial's connection: the client ends of a listener's pair, its way `down` and way `up`
    /// as its listener names them (the spawner opened them for its VM, `<path>_<port>`'s, D272).
    /// This process reads the way down and writes the way up: its own ways are named as its
    /// writes and reads go, its `down` the way it writes.
    pub fn dialed(down: OwnedHandle, up: OwnedHandle) -> io::Result<Pipes> {
        Pipes::new(up, down)
    }

    /// Associates its handles with the completion port `port`, which is told of each of its
    /// calls' ends under `key` from here on. A call that ended before is the next
    /// [`look`](Self::look)'s, which comes before any wait on the port.
    pub fn attach(&mut self, port: HANDLE, key: usize) -> io::Result<()> {
        for handle in [&self.down, &self.up].into_iter().flatten() {
            associate(handle, port, key)?;
        }
        self.key = Some(key);
        Ok(())
    }

    /// The key its handles tell their port under, once attached.
    pub fn key(&self) -> Option<usize> {
        self.key
    }

    /// Ends its reads: the read under way cancelled and waited for, and the way up closed, so
    /// that its client's writes fail at once (ERROR_NO_DATA, D272); what it held unread goes.
    /// Its way down is as it was: a connection whose guest has ended closes this way first, so
    /// that its client, reading the way down's end after every byte, finds its writes refused.
    pub fn end_reads(&mut self) {
        if let (Reading::UnderWay, Some(up)) = (self.reading, &self.up) {
            settle(up, &self.calls.read);
        }
        self.up = None;
        self.reading = Reading::Ended;
    }

    /// Writes `bytes` whole and waits until they are in the pipe, a write under way waited for
    /// first: for a caller that writes and goes, its connection its own (a host ask's answer,
    /// shards' `serve_rates`), never the device's, whose writes wait on nothing.
    pub fn write_all_waiting(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.wait_written()?;
        // SAFETY: `bytes` is readable for its length through the call.
        let taken = unsafe { self.writev(&[(bytes.as_ptr(), bytes.len())]) }?;
        if taken != bytes.len() {
            return Err(io::ErrorKind::WriteZero.into());
        }
        self.wait_written()
    }

    /// Waits until no write is under way: each that is, waited for to its end, then taken.
    fn wait_written(&mut self) -> io::Result<()> {
        loop {
            let under_way = matches!(self.writing, Writing::UnderWay { .. } | Writing::Closing { .. });
            let Some(down) = self.down.as_ref().filter(|_| under_way) else {
                break;
            };
            let mut moved = 0u32;
            // SAFETY: the OVERLAPPED of the one write under way on `down`, which names its call's
            // event ([`Call`]): waited for (bWait 1) on that, so the system is done with it and
            // its bytes once this returns. Its outcome is `look`'s to take.
            unsafe { GetOverlappedResult(down.as_raw_handle(), self.calls.write.ov(), &mut moved, 1) };
            self.look();
        }
        match self.writing {
            Writing::Failed(code) => Err(error(code)),
            _ => Ok(()),
        }
    }

    /// Starts the next read into the buffer. One that finishes at once is taken by the next
    /// [`look`](Self::look), as one that finishes later is.
    fn read_next(&mut self) {
        let Some(up) = &self.up else {
            self.reading = Reading::Ended;
            return;
        };
        let len = u32::try_from(self.into.len()).unwrap_or(u32::MAX);
        // SAFETY: no read is under way: its OVERLAPPED is ours to make fresh, and the buffer's
        // `len` bytes are the system's until the call finishes (`look`, `drop`).
        let started = unsafe {
            let ov = self.calls.read.fresh();
            ReadFile(
                up.as_raw_handle(),
                self.into.as_mut_ptr(),
                len,
                std::ptr::null_mut(),
                ov,
            )
        };
        self.reading = if started != 0 {
            Reading::UnderWay
        } else {
            // SAFETY: no arguments.
            match unsafe { GetLastError() } {
                ERROR_IO_PENDING => Reading::UnderWay,
                code => read_ended(code),
            }
        };
    }

    /// Starts a write of the buffer's bytes from `at`, closing the way down once it finishes
    /// where `closing`.
    fn write_from(&mut self, at: usize, closing: bool) {
        let Some(down) = &self.down else {
            self.writing = Writing::Shut;
            return;
        };
        let rest = self.out.get(at..).unwrap_or_default();
        let len = u32::try_from(rest.len()).unwrap_or(u32::MAX);
        // SAFETY: no write is under way: its OVERLAPPED is ours to make fresh, and the buffer's
        // bytes from `at` are the system's, untouched, until the call finishes (`look`, `drop`).
        let started = unsafe {
            let ov = self.calls.write.fresh();
            WriteFile(down.as_raw_handle(), rest.as_ptr(), len, std::ptr::null_mut(), ov)
        };
        // SAFETY: no arguments.
        let failed = (started == 0).then(|| unsafe { GetLastError() });
        self.writing = match failed {
            None | Some(ERROR_IO_PENDING) if closing => Writing::Closing { at },
            None | Some(ERROR_IO_PENDING) => Writing::UnderWay { at },
            Some(code) => {
                if closing {
                    self.down = None;
                }
                Writing::Failed(code)
            }
        };
    }

    /// Takes what finished of its calls, without waiting, whatever told of it: a read's bytes
    /// or its end, a write's end. The bytes of the write that finished, which are the pipe's
    /// from here on; none where none did.
    pub fn look(&mut self) -> usize {
        let read = match (self.reading, &self.up) {
            (Reading::UnderWay, Some(up)) => outcome(up, self.calls.read.ov()),
            _ => None,
        };
        match read {
            None => {}
            // Nothing came, as a zero-byte write by its client is nothing to a byte-mode
            // reader (PM M609 (d)): read on.
            Some(Ok(0)) => self.read_next(),
            Some(Ok(n)) => {
                self.reading = Reading::Holding {
                    from: 0,
                    to: n.min(self.into.len()),
                }
            }
            Some(Err(code)) => self.reading = read_ended(code),
        }
        let (at, closing) = match self.writing {
            Writing::UnderWay { at } => (at, false),
            Writing::Closing { at } => (at, true),
            Writing::Idle | Writing::Shut | Writing::Failed(_) => return 0,
        };
        let Some(down) = &self.down else {
            return 0;
        };
        match outcome(down, self.calls.write.ov()) {
            None => 0,
            Some(Err(code)) => {
                if closing {
                    self.down = None;
                }
                self.writing = Writing::Failed(code);
                0
            }
            Some(Ok(n)) => {
                let done = at.saturating_add(n).min(self.out.len());
                if done < self.out.len() {
                    // A pipe that waits finishes a write whole (WriteFile, Remarks); one that took
                    // less is written on from where it stopped.
                    self.write_from(done, closing);
                } else {
                    self.out = Vec::new();
                    self.writing = if closing {
                        self.down = None;
                        Writing::Shut
                    } else {
                        Writing::Idle
                    };
                }
                done.saturating_sub(at)
            }
        }
    }

    /// Whether a read would not wait: bytes held, or its end or failure to say.
    pub fn readable(&self) -> bool {
        self.reading != Reading::UnderWay
    }

    /// Whether a write would not wait: none under way.
    pub fn writable(&self) -> bool {
        !matches!(self.writing, Writing::UnderWay { .. } | Writing::Closing { .. })
    }

    /// Bytes taken for a write that has not finished: the connection's still, not the pipe's.
    pub fn in_flight(&self) -> usize {
        match self.writing {
            Writing::UnderWay { at } | Writing::Closing { at } => self.out.len().saturating_sub(at),
            Writing::Idle | Writing::Shut | Writing::Failed(_) => 0,
        }
    }

    /// Moves what its last read brought into `runs`, in order, as much as they hold, and starts
    /// the next read once all of it is taken: WouldBlock while a read is under way; `Ok(0)` once
    /// its client has ended its writes and every byte was taken, or where `runs` hold no room.
    ///
    /// # Safety
    /// Each run is `len` bytes writable at its pointer through the call, none of them the
    /// connection's own.
    pub unsafe fn readv(&mut self, runs: &[(*mut u8, usize)]) -> io::Result<usize> {
        let (from, to) = match self.reading {
            Reading::Holding { from, to } => (from, to),
            Reading::UnderWay => return Err(io::ErrorKind::WouldBlock.into()),
            Reading::Ended => return Ok(0),
            Reading::Failed(code) => return Err(error(code)),
        };
        let held = self.into.get(from..to).unwrap_or_default();
        let mut moved = 0;
        for &(ptr, len) in runs {
            let rest = held.get(moved..).unwrap_or_default();
            if rest.is_empty() {
                break;
            }
            let n = len.min(rest.len());
            // SAFETY: the caller's run of `len` writable bytes, which is not the buffer; `n` of
            // the bytes held copied in, no reference to the run made, as guest memory may
            // change under it.
            unsafe { std::ptr::copy_nonoverlapping(rest.as_ptr(), ptr, n) };
            moved += n;
        }
        let from = from.saturating_add(moved);
        if from < to {
            self.reading = Reading::Holding { from, to };
        } else if moved > 0 {
            self.read_next();
        }
        Ok(moved)
    }

    /// Takes `runs`' bytes, whole, for a write of their own where none is under way:
    /// WouldBlock while one is. They are the connection's until the write finishes, which
    /// [`look`](Self::look) says with their count.
    ///
    /// # Safety
    /// Each run is `len` bytes readable at its pointer through the call.
    pub unsafe fn writev(&mut self, runs: &[(*const u8, usize)]) -> io::Result<usize> {
        match self.writing {
            Writing::Idle => {}
            Writing::UnderWay { .. } | Writing::Closing { .. } => {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            Writing::Shut => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "the way down is closed",
                ));
            }
            Writing::Failed(code) => return Err(error(code)),
        }
        let total = runs
            .iter()
            .try_fold(0usize, |sum, &(_, len)| sum.checked_add(len))
            .filter(|&total| u32::try_from(total).is_ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "a write past a DWORD's bytes"))?;
        if total == 0 {
            return Ok(0);
        }
        let mut out = std::mem::take(&mut self.out);
        out.clear();
        out.try_reserve_exact(total)
            .map_err(|e| io::Error::new(io::ErrorKind::OutOfMemory, e.to_string()))?;
        for &(ptr, len) in runs {
            let at = out.len();
            // SAFETY: the caller's run of `len` readable bytes, copied into room reserved for it
            // past `at` (`total` in all); no reference to the run is made, as guest memory may
            // change under it.
            unsafe {
                std::ptr::copy_nonoverlapping(ptr, out.as_mut_ptr().add(at), len);
                out.set_len(at + len);
            }
        }
        self.out = out;
        self.write_from(0, false);
        match self.writing {
            Writing::Failed(code) => Err(error(code)),
            _ => Ok(total),
        }
    }

    /// Ends the way down once every byte taken is in the pipe: its handle closed, so that its
    /// client reads them, then its end (ERROR_BROKEN_PIPE, `std`'s `Ok(0)`), and writes on up
    /// (D272, PM M609 (h)). Closed, never disconnected: DisconnectNamedPipe drops what its
    /// client has not read.
    pub fn shut_down(&mut self) {
        self.writing = match self.writing {
            Writing::UnderWay { at } => Writing::Closing { at },
            Writing::Idle => {
                self.down = None;
                Writing::Shut
            }
            Writing::Failed(code) => {
                self.down = None;
                Writing::Failed(code)
            }
            ended @ (Writing::Closing { .. } | Writing::Shut) => ended,
        };
    }
}

impl io::Read for Pipes {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // SAFETY: `buf` is writable for its length through the call, and not the buffer.
        unsafe { self.readv(&[(buf.as_mut_ptr(), buf.len())]) }
    }
}

impl io::Write for Pipes {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // SAFETY: `buf` is readable for its length through the call.
        unsafe { self.writev(&[(buf.as_ptr(), buf.len())]) }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A connection ends its calls before their OVERLAPPEDs and buffers go: cancelled, as the
/// connection has ended, and waited for.
impl Drop for Pipes {
    fn drop(&mut self) {
        if let (Reading::UnderWay, Some(up)) = (self.reading, &self.up) {
            settle(up, &self.calls.read);
        }
        if let (Writing::UnderWay { .. } | Writing::Closing { .. }, Some(down)) = (self.writing, &self.down) {
            settle(down, &self.calls.write);
        }
    }
}

/// Where a listening pair's instance stands, its ConnectNamedPipe's (R04).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Connect {
    /// Its call under way: no client has it yet.
    UnderWay,
    /// A client opened it.
    Connected,
    /// Its call failed with this error, its client among them coming and going first
    /// (ERROR_NO_DATA): no connection's.
    Failed(u32),
}

/// What a listening pair's way down is watched by while its way up waits (R04): a read of no
/// bytes, which pends until something comes or its client goes (PM M348).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Watch {
    /// None started: the way down has no client yet, or the pair is whole.
    Idle,
    /// Under way.
    UnderWay,
    /// It ended: the way down's client went, or wrote on the way it reads, either no client's
    /// that D272 knows.
    Ended,
}

/// What a listening pair has come to ([`Listening::look`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Paired {
    /// Its client has opened neither way yet, or the way down alone.
    Waiting,
    /// Both ways are open: [`Listening::into_pipes`] makes them a connection.
    Connected,
    /// No client's: its way down's client went before opening its way up, or a call failed.
    /// Dropped and replaced, so that a client that dies there stops no other (D272).
    Broken,
}

/// A listening pair's calls' slots, and the byte its watch reads none of.
struct ListenCalls {
    down: Call,
    up: Call,
    watch: Call,
    byte: UnsafeCell<u8>,
}

/// A VM's listener's one unconnected pair (D272): two duplex instances, its way down and way
/// up, each connected as its client opens it (overlapped ConnectNamedPipe). Its client opens
/// the way down first; while its way up waits, the way down is watched for that client's
/// going. Its listener asks for the next pair once this one is whole, so that a second client,
/// waiting on the way down meanwhile (ERROR_PIPE_BUSY), can open no way of this one.
pub struct Listening {
    down: Option<OwnedHandle>,
    up: Option<OwnedHandle>,
    /// Boxed, so that its calls keep their OVERLAPPEDs where they are as the pair moves.
    calls: Box<ListenCalls>,
    down_at: Connect,
    up_at: Connect,
    watch: Watch,
    /// What its handles tell a completion port under, once attached.
    key: Option<usize>,
}

// SAFETY: as Pipes': its OVERLAPPEDs and byte are the system's only while their calls are under
// way, and otherwise touched only through `&mut self`.
unsafe impl Send for Listening {}

impl std::fmt::Debug for Listening {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listening")
            .field("down", &self.down_at)
            .field("up", &self.up_at)
            .field("watch", &self.watch)
            .finish_non_exhaustive()
    }
}

/// Starts `instance`'s ConnectNamedPipe in `call`: where it stands as the call returns. A
/// client may open an instance as soon as it is made, before this call, which then says so
/// at once (ERROR_PIPE_CONNECTED, ConnectNamedPipe), its OVERLAPPED never the system's.
fn connect(instance: &OwnedHandle, call: &Call) -> Connect {
    // SAFETY: a slot no call is under way in, its OVERLAPPED made fresh, the system's until the
    // call finishes (`look`, `drop`), on a server end of ours opened for overlapped calls.
    let started = unsafe { ConnectNamedPipe(instance.as_raw_handle(), call.fresh()) };
    if started != 0 {
        return Connect::Connected;
    }
    // SAFETY: no arguments.
    match unsafe { GetLastError() } {
        ERROR_IO_PENDING => Connect::UnderWay,
        ERROR_PIPE_CONNECTED => Connect::Connected,
        code => Connect::Failed(code),
    }
}

/// Where a ConnectNamedPipe under way stands, from what it finished as (`outcome`).
fn connected(finished: Option<Result<usize, u32>>) -> Connect {
    match finished {
        None => Connect::UnderWay,
        Some(Ok(_)) => Connect::Connected,
        Some(Err(ERROR_PIPE_CONNECTED)) => Connect::Connected,
        Some(Err(code)) => Connect::Failed(code),
    }
}

impl Listening {
    /// The pair whose way down is `down` and way up `up`: server ends of byte-mode duplex
    /// instances opened for overlapped calls, unconnected. Its connects start at once.
    pub fn new(down: OwnedHandle, up: OwnedHandle) -> io::Result<Listening> {
        let calls = Box::new(ListenCalls {
            down: Call::new()?,
            up: Call::new()?,
            watch: Call::new()?,
            byte: UnsafeCell::new(0),
        });
        let down_at = connect(&down, &calls.down);
        let up_at = connect(&up, &calls.up);
        Ok(Listening {
            down: Some(down),
            up: Some(up),
            calls,
            down_at,
            up_at,
            watch: Watch::Idle,
            key: None,
        })
    }

    /// Associates its handles with the completion port `port`, which is told of its calls'
    /// ends under `key` from here on, and of the connection's it makes ([`into_pipes`]).
    ///
    /// [`into_pipes`]: Listening::into_pipes
    pub fn attach(&mut self, port: HANDLE, key: usize) -> io::Result<()> {
        for handle in [&self.down, &self.up].into_iter().flatten() {
            associate(handle, port, key)?;
        }
        self.key = Some(key);
        Ok(())
    }

    /// The key its handles tell their port under, once attached.
    pub fn key(&self) -> Option<usize> {
        self.key
    }

    /// Takes what finished of its calls, without waiting, and what the pair has come to. Its
    /// way down open and its way up not, the way down's watch starts.
    pub fn look(&mut self) -> Paired {
        if let (Connect::UnderWay, Some(down)) = (self.down_at, &self.down) {
            self.down_at = connected(outcome(down, self.calls.down.ov()));
        }
        if let (Connect::UnderWay, Some(up)) = (self.up_at, &self.up) {
            self.up_at = connected(outcome(up, self.calls.up.ov()));
        }
        if let (Watch::UnderWay, Some(down)) = (self.watch, &self.down)
            && outcome(down, self.calls.watch.ov()).is_some()
        {
            self.watch = Watch::Ended;
        }
        match (self.down_at, self.up_at, self.watch) {
            (Connect::Failed(_), _, _) | (_, Connect::Failed(_), _) | (_, _, Watch::Ended) => Paired::Broken,
            (Connect::Connected, Connect::Connected, _) => Paired::Connected,
            (Connect::Connected, Connect::UnderWay, Watch::Idle) => {
                self.watch_down();
                if self.watch == Watch::Ended {
                    Paired::Broken
                } else {
                    Paired::Waiting
                }
            }
            _ => Paired::Waiting,
        }
    }

    /// Starts the way down's watch: a read of no bytes, which pends while its client is there
    /// and writes nothing on the way it reads.
    fn watch_down(&mut self) {
        let Some(down) = &self.down else {
            self.watch = Watch::Ended;
            return;
        };
        // SAFETY: no watch is under way: its OVERLAPPED is ours to make fresh; a read of none
        // of the byte kept beside it, the system's until it finishes (`look`, `into_pipes`, `drop`).
        let started = unsafe {
            ReadFile(
                down.as_raw_handle(),
                self.calls.byte.get(),
                0,
                std::ptr::null_mut(),
                self.calls.watch.fresh(),
            )
        };
        // SAFETY: no arguments.
        let pending = started == 0 && unsafe { GetLastError() } == ERROR_IO_PENDING;
        self.watch = if pending { Watch::UnderWay } else { Watch::Ended };
    }

    /// The pair as a connection's ([`Pipes`]), once both ways are open: its watch ended first,
    /// and its key kept, as its handles tell their port under it still.
    pub fn into_pipes(mut self) -> io::Result<Pipes> {
        if (self.down_at, self.up_at) != (Connect::Connected, Connect::Connected) {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "a pair not yet open both ways",
            ));
        }
        if let (Watch::UnderWay, Some(down)) = (self.watch, &self.down) {
            settle(down, &self.calls.watch);
        }
        self.watch = Watch::Idle;
        let (Some(down), Some(up)) = (self.down.take(), self.up.take()) else {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "a pair taken already",
            ));
        };
        let mut pipes = Pipes::new(down, up)?;
        pipes.key = self.key;
        Ok(pipes)
    }
}

/// A pair ends its calls before their OVERLAPPEDs go: cancelled and waited for, its handles
/// then closed, which its clients, if any, read as their end.
impl Drop for Listening {
    fn drop(&mut self) {
        if let Some(down) = &self.down {
            if self.down_at == Connect::UnderWay {
                settle(down, &self.calls.down);
            }
            if self.watch == Watch::UnderWay {
                settle(down, &self.calls.watch);
            }
        }
        if let (Connect::UnderWay, Some(up)) = (self.up_at, &self.up) {
            settle(up, &self.calls.up);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use std::fs::{File, OpenOptions};
    use std::io::{Read as _, Write as _};
    use std::os::windows::io::FromRawHandle as _;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{ERROR_NO_DATA, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_INBOUND, PIPE_ACCESS_OUTBOUND,
    };
    use windows_sys::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
    };

    use super::*;

    /// Each way's quota here: small, so that a write past it waits on its reader.
    const QUOTA: u32 = 4096;

    /// A connection's client: the way down read, the way up written, each a file as `std` opens
    /// one, synchronous, as Hex's client opens them (D272).
    struct Client {
        down: File,
        up: File,
    }

    /// A byte-mode instance named `name`, the only one, for overlapped calls.
    fn instance(name: &str, access: u32) -> OwnedHandle {
        let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
        // SAFETY: a NUL-terminated name and plain arguments; checked below.
        let raw = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                access | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                QUOTA,
                QUOTA,
                0,
                std::ptr::null(),
            )
        };
        assert_ne!(raw, INVALID_HANDLE_VALUE, "{}", io::Error::last_os_error());
        // SAFETY: the instance just made, owned from here on.
        unsafe { OwnedHandle::from_raw_handle(raw) }
    }

    /// A connection and its client, each way connected.
    fn connected() -> (Pipes, Client) {
        static MADE: AtomicU32 = AtomicU32::new(0);
        let name = format!(
            r"\\.\pipe\shards-vmm-pipes-{}-{}",
            std::process::id(),
            MADE.fetch_add(1, Ordering::Relaxed)
        );
        let (down, up) = (format!("{name}-down"), format!("{name}-up"));
        let ours = (
            instance(&down, PIPE_ACCESS_OUTBOUND),
            instance(&up, PIPE_ACCESS_INBOUND),
        );
        let client = Client {
            down: OpenOptions::new().read(true).open(&down).unwrap(),
            up: OpenOptions::new().write(true).open(&up).unwrap(),
        };
        (Pipes::new(ours.0, ours.1).unwrap(), client)
    }

    /// Looks until `done` holds of the pipes, failing after a while: the bytes of the writes
    /// that finished meanwhile.
    fn until(p: &mut Pipes, what: &str, done: impl Fn(&Pipes) -> bool) -> usize {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut written = 0;
        loop {
            written += p.look();
            if done(p) {
                return written;
            }
            assert!(Instant::now() < deadline, "{what}: not after 10 s: {p:?}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// A write past what the pipe holds is under way, its bytes the connection's, until its
    /// client has read all but the pipe's quota (PM M603); then it has finished, its count
    /// said once, and the client has every byte in order.
    #[test]
    fn a_write_past_the_pipe_is_the_connections_until_its_reader_takes_it() {
        let (mut p, Client { mut down, up: _up }) = connected();
        let bytes = pattern(256 * 1024);
        assert_eq!(p.write(&bytes).unwrap(), bytes.len());
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(p.look(), 0, "finished with no reader");
        assert!(!p.writable());
        assert_eq!(p.in_flight(), bytes.len());
        let more = p.write(b"more");
        assert_eq!(more.map_err(|e| e.kind()), Err(io::ErrorKind::WouldBlock));
        let reader = std::thread::spawn(move || {
            let mut got = vec![0u8; 256 * 1024];
            down.read_exact(&mut got).unwrap();
            got
        });
        assert_eq!(until(&mut p, "the write's end", Pipes::writable), bytes.len());
        assert_eq!((p.in_flight(), p.look()), (0, 0));
        assert!(reader.join().unwrap() == bytes, "the client's bytes out of order");
    }

    /// What its client writes is read in order, a line a byte at a time leaving what follows
    /// it held (as a handshake reads its line, muxer.rs), the rest into runs; then its end,
    /// `Ok(0)`, from then on.
    #[test]
    fn its_clients_bytes_are_read_in_order_then_its_end() {
        let (mut p, Client { down: _down, mut up }) = connected();
        up.write_all(b"CONNECT 1234\nafter").unwrap();
        drop(up);
        let mut line = Vec::new();
        while !line.ends_with(b"\n") {
            until(&mut p, "the line", Pipes::readable);
            let mut byte = [0u8; 1];
            assert_eq!(p.read(&mut byte).unwrap(), 1);
            line.push(byte[0]);
        }
        assert_eq!(line, b"CONNECT 1234\n");
        until(&mut p, "the rest", Pipes::readable);
        let (mut a, mut b) = ([0u8; 3], [0u8; 8]);
        // SAFETY: two arrays of ours, writable for their lengths.
        let n = unsafe { p.readv(&[(a.as_mut_ptr(), a.len()), (b.as_mut_ptr(), b.len())]) }.unwrap();
        assert_eq!((n, &a, &b[..2]), (5, b"aft", &b"er"[..]));
        until(&mut p, "its end", Pipes::readable);
        assert_eq!(p.read(&mut [0u8; 4]).unwrap(), 0);
        assert_eq!(p.read(&mut [0u8; 4]).unwrap(), 0);
    }

    /// The way down shut is its client's end after every byte, and its client writes on up,
    /// read here (D272, PM M609 (h)); a write down after it is refused.
    #[test]
    fn the_way_down_shut_is_its_clients_end_and_its_writes_go_on() {
        let (mut p, Client { mut down, mut up }) = connected();
        assert_eq!(p.write(b"last words").unwrap(), 10);
        p.shut_down();
        until(&mut p, "the close", |p| p.writing == Writing::Shut);
        let mut got = Vec::new();
        down.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"last words");
        up.write_all(b"reply").unwrap();
        let mut reply = Vec::new();
        while reply.len() < 5 {
            until(&mut p, "the reply", Pipes::readable);
            let mut buf = [0u8; 8];
            let n = p.read(&mut buf).unwrap();
            assert_ne!(n, 0, "ended before the reply");
            reply.extend_from_slice(&buf[..n]);
        }
        assert_eq!(reply, b"reply");
        let late = p.write(b"x");
        assert_eq!(late.map_err(|e| e.kind()), Err(io::ErrorKind::BrokenPipe));
    }

    /// Shut behind a write under way, the way down closes once that write has finished: its
    /// client reads every byte, then its end.
    #[test]
    fn a_shut_behind_a_write_closes_once_its_bytes_are_in_the_pipe() {
        let (mut p, Client { mut down, up: _up }) = connected();
        let bytes = pattern(64 * 1024);
        assert_eq!(p.write(&bytes).unwrap(), bytes.len());
        p.shut_down();
        assert!(matches!(p.writing, Writing::Closing { .. }), "{p:?}");
        let reader = std::thread::spawn(move || {
            let mut got = Vec::new();
            down.read_to_end(&mut got).unwrap();
            got
        });
        assert_eq!(
            until(&mut p, "the close", |p| p.writing == Writing::Shut),
            bytes.len()
        );
        assert!(
            reader.join().unwrap() == bytes,
            "the client's bytes, then its end"
        );
    }

    /// A connection's whole end, its pipes dropped, is its client's: every byte written down
    /// read, then its end, and its write up failing at once, ERROR_NO_DATA (D272); and a
    /// client that has stopped reading fails a write down.
    #[test]
    fn its_end_fails_its_clients_writes_at_once_after_every_byte() {
        let (mut p, Client { mut down, mut up }) = connected();
        assert_eq!(p.write(b"bye").unwrap(), 3);
        until(&mut p, "the write", Pipes::writable);
        drop(p);
        let mut got = Vec::new();
        down.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"bye");
        let late = up.write(b"late");
        assert_eq!(
            late.map_err(|e| e.raw_os_error()),
            Err(Some(ERROR_NO_DATA as i32))
        );

        let (mut p, client) = connected();
        drop(client.down);
        // Refused at once, or failing as it goes and refusing the next.
        let refused = match p.write(b"anyone") {
            Ok(n) => {
                assert_eq!(n, 6);
                until(&mut p, "the write's end", Pipes::writable);
                p.write(b"x")
            }
            failed => failed,
        };
        assert!(refused.is_err(), "{p:?}: {refused:?}");
    }

    /// Its client writes up while its own read down waits, each way a file of its own (M609
    /// (h)); its read then has what is written down.
    #[test]
    fn its_client_writes_while_its_read_waits() {
        let (mut p, Client { mut down, mut up }) = connected();
        let reader = std::thread::spawn(move || {
            let mut got = [0u8; 4];
            down.read_exact(&mut got).unwrap();
            got
        });
        std::thread::sleep(Duration::from_millis(20));
        up.write_all(b"ping").unwrap();
        assert!(!reader.is_finished(), "read down before anything was written");
        let mut ping = Vec::new();
        while ping.len() < 4 {
            until(&mut p, "the ping", Pipes::readable);
            let mut buf = [0u8; 4];
            let n = p.read(&mut buf).unwrap();
            ping.extend_from_slice(&buf[..n]);
        }
        assert_eq!(ping, b"ping");
        assert_eq!(p.write(b"pong").unwrap(), 4);
        until(&mut p, "the pong", Pipes::writable);
        assert_eq!(&reader.join().unwrap(), b"pong");
    }

    /// A connection dropped with a read and a write under way ends both, waiting for neither's
    /// reader: its client reads what was in the pipe, a start of the bytes, then its end.
    #[test]
    fn a_connection_dropped_with_calls_under_way_ends_them() {
        let (mut p, Client { mut down, up: _up }) = connected();
        let bytes = pattern(256 * 1024);
        assert_eq!(p.write(&bytes).unwrap(), bytes.len());
        assert_eq!(p.look(), 0);
        assert!(!p.readable() && !p.writable(), "{p:?}");
        drop(p);
        let mut got = Vec::new();
        down.read_to_end(&mut got).unwrap();
        assert!(
            got.len() < bytes.len() && bytes.starts_with(&got),
            "{} bytes",
            got.len()
        );
    }

    /// A listening pair is a connection once its client has opened both ways, the way down
    /// first, as D272's client does: waiting before, its way down watched meanwhile; whole
    /// after, its connection carrying bytes both ways.
    #[test]
    fn a_listening_pair_is_a_connection_once_both_ways_are_open() {
        let names = shards_ipc::VsockNames::fresh().unwrap();
        let (down, up) = shards_ipc::vsock_pair(&names, true).unwrap();
        let mut pair = Listening::new(down, up).unwrap();
        assert_eq!(pair.look(), Paired::Waiting);
        let mut their_down = OpenOptions::new().read(true).open(&names.down).unwrap();
        // Its connect's finish is the next look's, as it lands.
        let deadline = Instant::now() + Duration::from_secs(10);
        while pair.watch != Watch::UnderWay {
            assert_eq!(pair.look(), Paired::Waiting, "{pair:?}");
            assert!(Instant::now() < deadline, "its way down unwatched: {pair:?}");
            std::thread::sleep(Duration::from_millis(1));
        }
        let mut their_up = OpenOptions::new().write(true).open(&names.up).unwrap();
        while pair.look() != Paired::Connected {
            assert!(Instant::now() < deadline, "not whole: {pair:?}");
            std::thread::sleep(Duration::from_millis(1));
        }
        let mut p = pair.into_pipes().unwrap();
        assert_eq!(p.write(b"hi").unwrap(), 2);
        let mut got = [0u8; 2];
        their_down.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"hi");
        their_up.write_all(b"yo").unwrap();
        until(&mut p, "its client's bytes", Pipes::readable);
        let mut got = [0u8; 2];
        assert_eq!(p.read(&mut got).unwrap(), 2);
        assert_eq!(&got, b"yo");
    }

    /// A client that goes with its way down alone open breaks its pair (D272), which is then
    /// no one's, so that its listener drops and replaces it.
    #[test]
    fn a_client_gone_before_its_way_up_breaks_its_pair() {
        let names = shards_ipc::VsockNames::fresh().unwrap();
        let (down, up) = shards_ipc::vsock_pair(&names, true).unwrap();
        let mut pair = Listening::new(down, up).unwrap();
        let their_down = OpenOptions::new().read(true).open(&names.down).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while pair.watch != Watch::UnderWay {
            assert_eq!(pair.look(), Paired::Waiting, "{pair:?}");
            assert!(Instant::now() < deadline, "its way down unwatched: {pair:?}");
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(their_down);
        let deadline = Instant::now() + Duration::from_secs(10);
        while pair.look() != Paired::Broken {
            assert!(Instant::now() < deadline, "a client's going unseen: {pair:?}");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(pair.into_pipes().is_err(), "a broken pair made a connection");
    }

    /// A connection whose reads end first fails its client's writes at once, ERROR_NO_DATA
    /// (232), while its client reads on every byte written down, then their end (D272).
    #[test]
    fn its_reads_ended_fail_its_clients_writes_at_once() {
        let (mut p, Client { mut down, mut up }) = connected();
        assert_eq!(p.write(b"last").unwrap(), 4);
        p.end_reads();
        let refused = up.write(b"late");
        assert_eq!(
            refused.map_err(|e| e.raw_os_error()),
            Err(Some(ERROR_NO_DATA as i32))
        );
        p.shut_down();
        until(&mut p, "the close", |p| p.writing == Writing::Shut);
        let mut got = Vec::new();
        down.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"last");
    }

    /// A dial's connection holds a listener's client ends: it reads the way down, which its
    /// listener writes, and writes the way up, which its listener reads; each way's buffer is
    /// that way's quota, the way down's outbound, as its client end reads it.
    #[test]
    fn a_dial_reads_its_way_down_and_writes_its_way_up() {
        let dir = shards_testdir::TempDir::new("pipes-dial").unwrap();
        let path = dir.join("v_5000");
        let (_, down, up) = shards_ipc::vsock_listen(&path).unwrap();
        let listener = (
            shards_ipc::Pipe::from_handle(down),
            shards_ipc::Pipe::from_handle(up),
        );
        let (their_down, their_up) = shards_ipc::vsock_open(&path, None).unwrap();
        let mut p = Pipes::dialed(their_down, their_up).unwrap();
        assert_eq!(p.into.len(), shards_ipc::VSOCK_QUOTA as usize);
        (&listener.0).write_all(b"from the host").unwrap();
        let mut got = Vec::new();
        while got.len() < 13 {
            until(&mut p, "the host's bytes", Pipes::readable);
            let mut buf = [0u8; 16];
            let n = p.read(&mut buf).unwrap();
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, b"from the host");
        assert_eq!(p.write(b"from the guest").unwrap(), 14);
        until(&mut p, "the guest's bytes", Pipes::writable);
        let mut got = [0u8; 14];
        (&listener.1).read_exact(&mut got).unwrap();
        assert_eq!(&got, b"from the guest");
    }

    /// A waited write is in the pipe as it returns: dropped at once after, its client reads
    /// every byte, then the end.
    #[test]
    fn a_waited_write_is_in_the_pipe_as_it_returns() {
        let (mut p, Client { mut down, up: _up }) = connected();
        p.write_all_waiting(b"RATED 250 100000000\n").unwrap();
        drop(p);
        let mut got = Vec::new();
        down.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"RATED 250 100000000\n");
    }

    /// Attached to a completion port, a connection's calls tell the port of their ends under
    /// its key: its client's bytes come as a packet of that key, after which its look holds
    /// them.
    #[test]
    fn its_calls_ends_come_to_its_port_under_its_key() {
        use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
        use windows_sys::Win32::System::IO::{CreateIoCompletionPort, GetQueuedCompletionStatus};
        let (mut p, Client { down: _down, mut up }) = connected();
        // SAFETY: a new port, of one thread.
        let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1) };
        assert!(!port.is_null(), "{}", io::Error::last_os_error());
        // SAFETY: the port just made, owned from here on.
        let port = unsafe { OwnedHandle::from_raw_handle(port) };
        p.attach(port.as_raw_handle(), 7).unwrap();
        assert_eq!(p.key(), Some(7));
        up.write_all(b"x").unwrap();
        let (mut moved, mut key, mut ov) = (0u32, 0usize, std::ptr::null_mut());
        // SAFETY: our port; what a packet says written to locals; waited for at most 10 s.
        let got =
            unsafe { GetQueuedCompletionStatus(port.as_raw_handle(), &mut moved, &mut key, &mut ov, 10_000) };
        assert_ne!(got, 0, "{}", io::Error::last_os_error());
        assert_eq!(key, 7);
        p.look();
        assert!(p.readable());
    }
}
