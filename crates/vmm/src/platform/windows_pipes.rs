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
//! waiting, whatever told of it.

use std::cell::UnsafeCell;
use std::io;
use std::os::windows::io::{AsRawHandle as _, OwnedHandle};

use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, ERROR_MORE_DATA, ERROR_PIPE_NOT_CONNECTED,
    GetLastError,
};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::GetNamedPipeInfo;

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

/// The calls' OVERLAPPEDs, each the system's while its call is under way.
struct Calls {
    read: UnsafeCell<OVERLAPPED>,
    write: UnsafeCell<OVERLAPPED>,
}

/// A vsock connection's host side on Windows: see the module's documentation.
pub struct Pipes {
    /// This process's end of the way down, which it writes and its client reads. None once
    /// closed: the guest's writes ended ([`Pipes::shut_down`]).
    down: Option<OwnedHandle>,
    /// This process's end of the way up, which its client writes and it reads.
    up: OwnedHandle,
    /// Boxed, so that a call under way keeps its OVERLAPPED where it is as the connection moves.
    calls: Box<Calls>,
    /// Where reads land, as long as the way up's inbound quota: one read takes what the pipe holds.
    into: Vec<u8>,
    /// What writes send: bytes taken from the caller, the system's while a write of them is under way.
    out: Vec<u8>,
    reading: Reading,
    writing: Writing,
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

/// Cancels the call under way on `handle` with `ov`, and waits for its end, cancelled or
/// finished: until then the system writes its OVERLAPPED, and its buffer.
fn settle(handle: &OwnedHandle, ov: *mut OVERLAPPED) {
    let mut moved = 0u32;
    // SAFETY: the OVERLAPPED of the one call under way on `handle`, which carries no other: an
    // OVERLAPPED with no event is waited for on its handle (GetOverlappedResult). Waited for
    // (bWait 1), so the system is done with it and its buffer once this returns.
    unsafe {
        CancelIoEx(handle.as_raw_handle(), ov);
        GetOverlappedResult(handle.as_raw_handle(), ov, &mut moved, 1);
    }
}

/// What the way up holds of its client's bytes, its inbound quota, as its maker gave it
/// (CreateNamedPipeW's `nInBufferSize`, read back by GetNamedPipeInfo).
fn inbound_quota(up: &OwnedHandle) -> io::Result<usize> {
    let mut quota = 0u32;
    // SAFETY: a pipe end of ours; the one count asked for is written to `quota`, the rest not asked.
    let read = unsafe {
        GetNamedPipeInfo(
            up.as_raw_handle(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut quota,
            std::ptr::null_mut(),
        )
    };
    if read == 0 {
        return Err(io::Error::last_os_error());
    }
    match usize::try_from(quota) {
        Ok(quota) if quota > 0 => Ok(quota),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a way up with no inbound quota to read into",
        )),
    }
}

impl Pipes {
    /// The connection whose way down is `down` and way up `up`: server ends of byte-mode pipe
    /// instances opened for overlapped calls, each connected to its client. Its first read
    /// starts at once.
    pub fn new(down: OwnedHandle, up: OwnedHandle) -> io::Result<Pipes> {
        let quota = inbound_quota(&up)?;
        let mut into = Vec::new();
        into.try_reserve_exact(quota)
            .map_err(|e| io::Error::new(io::ErrorKind::OutOfMemory, e.to_string()))?;
        into.resize(quota, 0);
        let mut pipes = Pipes {
            down: Some(down),
            up,
            calls: Box::new(Calls {
                read: UnsafeCell::new(OVERLAPPED::default()),
                write: UnsafeCell::new(OVERLAPPED::default()),
            }),
            into,
            out: Vec::new(),
            reading: Reading::Holding { from: 0, to: 0 },
            writing: Writing::Idle,
        };
        pipes.read_next();
        Ok(pipes)
    }

    /// Starts the next read into the buffer. One that finishes at once is taken by the next
    /// [`look`](Self::look), as one that finishes later is.
    fn read_next(&mut self) {
        let ov = self.calls.read.get();
        let len = u32::try_from(self.into.len()).unwrap_or(u32::MAX);
        // SAFETY: no read is under way: its OVERLAPPED is ours to make fresh, and the buffer's
        // `len` bytes are the system's until the call finishes (`look`, `drop`).
        let started = unsafe {
            ov.write(OVERLAPPED::default());
            ReadFile(
                self.up.as_raw_handle(),
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
        let ov = self.calls.write.get();
        // SAFETY: no write is under way: its OVERLAPPED is ours to make fresh, and the buffer's
        // bytes from `at` are the system's, untouched, until the call finishes (`look`, `drop`).
        let started = unsafe {
            ov.write(OVERLAPPED::default());
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
        if self.reading == Reading::UnderWay {
            match outcome(&self.up, self.calls.read.get()) {
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
        }
        let (at, closing) = match self.writing {
            Writing::UnderWay { at } => (at, false),
            Writing::Closing { at } => (at, true),
            Writing::Idle | Writing::Shut | Writing::Failed(_) => return 0,
        };
        let Some(down) = &self.down else {
            return 0;
        };
        match outcome(down, self.calls.write.get()) {
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
        if self.reading == Reading::UnderWay {
            settle(&self.up, self.calls.read.get());
        }
        if let (Writing::UnderWay { .. } | Writing::Closing { .. }, Some(down)) = (self.writing, &self.down) {
            settle(down, self.calls.write.get());
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
}
