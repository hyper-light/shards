//! Shared Linux guest native-stream pump for init's engine and managed TCP.
//! Storage, lifetime and terminal policy come from the actual caller.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::time::{Duration, Instant};

fn invalid(why: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, why)
}

#[derive(Clone, Copy)]
pub(crate) struct Deadline {
    pub(crate) at: Instant,
    pub(crate) cancel: Option<RawFd>,
}

impl Deadline {
    pub(crate) fn new(within: Duration) -> io::Result<Self> {
        if within.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "probe deadline ended"));
        }
        Instant::now()
            .checked_add(within)
            .map(|at| Self { at, cancel: None })
            .ok_or_else(|| invalid("deadline overflow"))
    }

    pub(crate) fn left(&self) -> io::Result<Duration> {
        let left = self.at.saturating_duration_since(Instant::now());
        if left.is_zero() {
            Err(io::Error::new(io::ErrorKind::TimedOut, "probe deadline ended"))
        } else {
            Ok(left)
        }
    }

    pub(crate) fn io<T>(
        &self,
        fd: RawFd,
        events: i16,
        mut operation: impl FnMut() -> io::Result<T>,
    ) -> io::Result<T> {
        loop {
            self.left()?;
            if let Some(cancel) = self.cancel {
                if cancel < 0 {
                    return Err(io::ErrorKind::InvalidInput.into());
                }
                let mut ready = libc::pollfd {
                    fd: cancel,
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: one initialized pollfd, checked without consuming its
                // event. Admission covers pending cancel, not a later check/I/O race.
                if unsafe { libc::poll(&raw mut ready, 1, 0) } < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error);
                }
                self.left()?;
                Self::cancellation_event(ready.revents)?;
            }
            match operation() {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                answer => return answer,
            }
            let left = self.left()?;
            let millis = left.as_nanos().div_ceil(Duration::from_millis(1).as_nanos());
            let timeout = i32::try_from(millis).unwrap_or(i32::MAX);
            let mut poll = [
                libc::pollfd {
                    fd,
                    events,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.cancel.unwrap_or(-1),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: poll reads/fills this array; negative descriptors are ignored.
            let result = unsafe { libc::poll(poll.as_mut_ptr(), poll.len() as _, timeout) };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            // Original expiry wins if time and cancellation are both observed.
            self.left()?;
            Self::cancellation_event(poll.last().map_or(0, |p| p.revents))?;
        }
    }

    pub(crate) fn cancellation_event(revents: i16) -> io::Result<()> {
        if revents == 0 {
            return Ok(());
        }
        let kind = if revents & libc::POLLNVAL != 0 {
            io::ErrorKind::InvalidInput
        } else if revents & libc::POLLERR != 0 {
            io::ErrorKind::Other
        } else if revents & libc::POLLHUP != 0 {
            io::ErrorKind::BrokenPipe
        } else if revents & !libc::POLLIN == 0 {
            io::ErrorKind::ConnectionAborted
        } else {
            io::ErrorKind::Other
        };
        Err(kind.into())
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Control<'a> {
    Until(&'a Deadline),
    ProcessOwned,
}

impl Control<'_> {
    fn io<T>(&self, fd: RawFd, events: i16, mut operation: impl FnMut() -> io::Result<T>) -> io::Result<T> {
        match self {
            Self::Until(deadline) => deadline.io(fd, events, operation),
            Self::ProcessOwned => loop {
                match operation() {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    result => return result,
                }
            },
        }
    }
}

enum Storage {
    Heap(Vec<u8>),
    Mapped(Mapping),
}

impl std::ops::Deref for Storage {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Self::Heap(bytes) => bytes,
            // SAFETY: live owned mapping, checked size <= isize::MAX, shared borrow.
            Self::Mapped(m) => unsafe { std::slice::from_raw_parts(m.at.as_ptr(), m.size) },
        }
    }
}
impl std::ops::DerefMut for Storage {
    fn deref_mut(&mut self) -> &mut [u8] {
        match self {
            Self::Heap(bytes) => bytes,
            // SAFETY: exclusive borrow of this uniquely owned writable mapping.
            Self::Mapped(m) => unsafe { std::slice::from_raw_parts_mut(m.at.as_ptr(), m.size) },
        }
    }
}
struct Mapping {
    at: std::ptr::NonNull<u8>,
    size: usize,
}
// SAFETY: unique mapping ownership moves with this value, no shared mutation.
unsafe impl Send for Mapping {}
impl Mapping {
    fn new(size: usize) -> Result<Self, Fault> {
        // SAFETY: positive checked length; anonymous private RW memory, no
        // executable permission, fixed address, shared object or retained cache.
        let at = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if at == libc::MAP_FAILED {
            let error = io::Error::last_os_error();
            return Err(Fault::Reservation {
                kind: error.kind(),
                errno: error.raw_os_error(),
            });
        }
        let Some(at) = std::ptr::NonNull::new(at.cast::<u8>()) else {
            // SAFETY: release the successful but unusable null mapping we own.
            let _ = unsafe { libc::munmap(at, size) };
            return Err(Fault::Bounds);
        };
        Ok(Self { at, size })
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: sole owner releases the mapping's exact original range.
        let _ = unsafe { libc::munmap(self.at.as_ptr().cast(), self.size) };
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Leg {
    Client,
    Backend,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Half {
    Reading,
    Draining(SourceEnd),
    WriteClosed(SourceEnd),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceEnd {
    Eof,
    PeerEnded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EndPolicy {
    Independent,
    EnginePeerEnd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Termination {
    BothHalvesClosed,
    BackendEnded,
}

#[derive(Clone, Copy)]
pub(crate) enum ByteLimit {
    Bytes(usize),
    PeerEnd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Operation {
    Connect,
    Read,
    Write,
    Shutdown,
    Wait,
}

#[derive(Debug)]
pub(crate) enum Fault {
    Cancelled,
    Expired,
    Control(io::ErrorKind),
    Native {
        leg: Leg,
        operation: Operation,
        kind: io::ErrorKind,
        errno: Option<i32>,
    },
    Limit {
        supplied: usize,
        required: usize,
    },
    Allocation,
    Reservation {
        kind: io::ErrorKind,
        errno: Option<i32>,
    },
    ExtraData {
        leg: Leg,
        offset: usize,
    },
    Bounds,
}

pub(crate) fn native(leg: Leg, operation: Operation, error: io::Error) -> Fault {
    Fault::Native {
        leg,
        operation,
        kind: error.kind(),
        errno: error.raw_os_error(),
    }
}

pub(crate) fn control(error: io::Error) -> Fault {
    match error.kind() {
        io::ErrorKind::TimedOut => Fault::Expired,
        io::ErrorKind::ConnectionAborted => Fault::Cancelled,
        kind => Fault::Control(kind),
    }
}

pub(crate) enum Attempt<T> {
    Ready(T),
    WouldBlock,
}

// Keep native failures inside the result: a socket ECONNABORTED is not the
// cancellation descriptor's ConnectionAborted. Only actual EINTR is readmitted.
pub(crate) fn attempt<T>(
    deadline: &Control<'_>,
    fd: RawFd,
    interest: i16,
    leg: Leg,
    operation: Operation,
    mut call: impl FnMut() -> io::Result<T>,
) -> Result<Attempt<T>, Fault> {
    let result = deadline
        .io(fd, interest, || match call() {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => Err(error),
            result => Ok(result),
        })
        .map_err(control)?;
    match result {
        Ok(value) => Ok(Attempt::Ready(value)),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(Attempt::WouldBlock),
        Err(error) => Err(native(leg, operation, error)),
    }
}

/// The most one virtio-vsock packet carries: Linux's transport splits any larger send
/// into packets of exactly this size (v6.18 include/linux/virtio_vsock.h
/// VIRTIO_VSOCK_MAX_PKT_BUF_SIZE; net/vmw_vsock/virtio_transport_common.c,
/// virtio_transport_send_pkt_info). A relay turn of this size moves one whole packet.
/// Measured over Unix sockets, throughput rises with the slot up to this size and is
/// flat beyond it (docs/research/measurements/managed-engine-relay-throughput-v1).
#[cfg_attr(
    test,
    allow(
        dead_code,
        reason = "the engine relay's own: the test guest's tests use the other slot shapes"
    )
)]
const VSOCK_PACKET: usize = 64 << 10;

pub(crate) struct Slots {
    to_backend: Storage,
    to_client: Storage,
}

impl Slots {
    pub(crate) fn capacities(&self) -> [usize; 2] {
        [self.to_backend.len(), self.to_client.len()]
    }

    /// One whole vsock packet per direction, page-rounded, with no growth/cache.
    #[cfg_attr(
        test,
        allow(
            dead_code,
            reason = "the engine relay's own: the test guest's tests use the other slot shapes"
        )
    )]
    pub(crate) fn vsock_packet() -> Result<Self, Fault> {
        // SAFETY: existing Linux page-size fact, no configuration change.
        let value = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let page = usize::try_from(value).map_err(|_| Fault::Bounds)?;
        let size = VSOCK_PACKET
            .checked_next_multiple_of(page)
            .filter(|size| *size != 0 && *size <= isize::MAX as usize)
            .ok_or(Fault::Bounds)?;
        Ok(Self {
            to_backend: Storage::Mapped(Mapping::new(size)?),
            to_client: Storage::Mapped(Mapping::new(size)?),
        })
    }

    /// One minimum private OS mapping unit per direction, with no growth/cache.
    pub(crate) fn minimum_mapped() -> Result<Self, Fault> {
        // SAFETY: existing Linux page-size fact, no configuration change.
        let value = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let size = usize::try_from(value).map_err(|_| Fault::Bounds)?;
        if size == 0 || size > isize::MAX as usize {
            return Err(Fault::Bounds);
        }
        Ok(Self {
            to_backend: Storage::Mapped(Mapping::new(size)?),
            to_client: Storage::Mapped(Mapping::new(size)?),
        })
    }
    pub(crate) fn reserve(to_backend: usize, to_client: usize) -> Result<Self, Fault> {
        fn slot(size: usize) -> Result<Vec<u8>, Fault> {
            if size == 0 {
                return Err(Fault::Limit {
                    supplied: 0,
                    required: 1,
                });
            }
            let mut bytes = Vec::new();
            bytes.try_reserve_exact(size).map_err(|_| Fault::Allocation)?;
            if bytes.capacity() > size {
                return Err(Fault::Limit {
                    supplied: size,
                    required: bytes.capacity(),
                });
            }
            bytes.resize(size, 0);
            Ok(bytes)
        }
        to_backend.checked_add(to_client).ok_or(Fault::Bounds)?;
        Ok(Self {
            to_backend: Storage::Heap(slot(to_backend)?),
            to_client: Storage::Heap(slot(to_client)?),
        })
    }
}

#[derive(Debug)]
pub(crate) struct Progress {
    pub(crate) half: Half,
    pub(crate) read: usize,
    pub(crate) written: usize,
    pub(crate) pending: usize,
    pub(crate) extra_consumed: usize,
    pub(crate) partial_writes: usize,
    pub(crate) would_block: usize,
    pub(crate) write_would_block: usize,
}

impl Progress {
    fn new() -> Self {
        Self {
            half: Half::Reading,
            read: 0,
            written: 0,
            pending: 0,
            extra_consumed: 0,
            partial_writes: 0,
            would_block: 0,
            write_would_block: 0,
        }
    }
}

#[derive(Debug)]
pub(crate) struct Report {
    pub(crate) to_backend: Progress,
    pub(crate) to_client: Progress,
}
impl Report {
    pub(crate) fn new() -> Self {
        Self {
            to_backend: Progress::new(),
            to_client: Progress::new(),
        }
    }
}

struct Direction {
    bytes: Storage,
    sent: usize,
    limit: ByteLimit,
    source: Leg,
}

impl Direction {
    fn turn(
        &mut self,
        source: BorrowedFd<'_>,
        destination: BorrowedFd<'_>,
        progress: &mut Progress,
        deadline: &Control<'_>,
        end_policy: EndPolicy,
    ) -> Result<bool, Fault> {
        let mut changed = false;
        let target = match self.source {
            Leg::Client => Leg::Backend,
            Leg::Backend => Leg::Client,
        };
        if progress.pending != 0 {
            let end = self.sent.checked_add(progress.pending).ok_or(Fault::Bounds)?;
            let bytes = self.bytes.get(self.sent..end).ok_or(Fault::Bounds)?;
            match attempt(
                deadline,
                destination.as_raw_fd(),
                libc::POLLOUT,
                target,
                Operation::Write,
                || {
                    // SAFETY: live borrowed socket; bytes is readable for len.
                    count(unsafe {
                        libc::send(
                            destination.as_raw_fd(),
                            bytes.as_ptr().cast(),
                            bytes.len(),
                            libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                        )
                    })
                },
            )? {
                Attempt::WouldBlock => {
                    increment(&mut progress.would_block, 1)?;
                    increment(&mut progress.write_would_block, 1)?;
                }
                Attempt::Ready(0) => {
                    return Err(native(target, Operation::Write, io::ErrorKind::WriteZero.into()));
                }
                Attempt::Ready(n) => {
                    if n > bytes.len() {
                        return Err(Fault::Bounds);
                    }
                    if n < bytes.len() {
                        increment(&mut progress.partial_writes, 1)?;
                    }
                    increment(&mut progress.written, n)?;
                    self.sent = self.sent.checked_add(n).ok_or(Fault::Bounds)?;
                    progress.pending = progress.pending.checked_sub(n).ok_or(Fault::Bounds)?;
                    changed = true;
                }
            }
        }
        if let (0, Half::Draining(cause)) = (progress.pending, progress.half) {
            let how = match (end_policy, self.source, cause) {
                // Engine EOF ends both Unix halves before making EOF observable
                // to its client; a later caller shutdown leaves a late-write gap.
                (EndPolicy::EnginePeerEnd, Leg::Backend, SourceEnd::Eof) => libc::SHUT_RDWR,
                _ => libc::SHUT_WR,
            };
            match attempt(
                deadline,
                destination.as_raw_fd(),
                libc::POLLOUT,
                target,
                Operation::Shutdown,
                || {
                    // SAFETY: shutdown of this owned flow's borrowed socket.
                    count(unsafe { libc::shutdown(destination.as_raw_fd(), how) } as isize)
                },
            )? {
                Attempt::Ready(_) => {
                    progress.half = Half::WriteClosed(cause);
                    changed = true;
                }
                Attempt::WouldBlock => increment(&mut progress.would_block, 1)?,
            }
        }
        if progress.pending != 0 || progress.half != Half::Reading {
            return Ok(changed);
        }
        self.sent = 0;
        let left = match self.limit {
            ByteLimit::Bytes(limit) => limit.checked_sub(progress.read).ok_or(Fault::Bounds)?,
            ByteLimit::PeerEnd => self.bytes.len(),
        };
        // A nonempty probe distinguishes EOF from an extra byte at the exact
        // caller byte ceiling. Its consumed byte is retained separately, never sent.
        let mut extra = [0u8; 1];
        let bytes = if left == 0 {
            extra.as_mut_slice()
        } else {
            let len = left.min(self.bytes.len());
            self.bytes.get_mut(..len).ok_or(Fault::Bounds)?
        };
        match attempt(
            deadline,
            source.as_raw_fd(),
            libc::POLLIN,
            self.source,
            Operation::Read,
            || {
                // SAFETY: writable slice and live borrowed socket, no blocking call.
                count(unsafe {
                    libc::recv(
                        source.as_raw_fd(),
                        bytes.as_mut_ptr().cast(),
                        bytes.len(),
                        libc::MSG_DONTWAIT,
                    )
                })
            },
        )? {
            Attempt::WouldBlock => increment(&mut progress.would_block, 1)?,
            Attempt::Ready(0) => {
                progress.half = Half::Draining(SourceEnd::Eof);
                changed = true;
            }
            Attempt::Ready(n) => {
                if n > bytes.len() {
                    return Err(Fault::Bounds);
                }
                if left == 0 {
                    increment(&mut progress.extra_consumed, n)?;
                    return Err(Fault::ExtraData {
                        leg: self.source,
                        offset: progress.read,
                    });
                }
                increment(&mut progress.read, n)?;
                progress.pending = n;
                changed = true;
            }
        }
        Ok(changed)
    }
}

fn increment(value: &mut usize, by: usize) -> Result<(), Fault> {
    *value = value.checked_add(by).ok_or(Fault::Bounds)?;
    Ok(())
}

pub(crate) fn count(result: isize) -> io::Result<usize> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        usize::try_from(result).map_err(io::Error::other)
    }
}

pub(crate) fn wait(
    deadline: &Control<'_>,
    client: RawFd,
    client_events: i16,
    backend: RawFd,
    backend_events: i16,
) -> Result<(), Fault> {
    let cancel = match deadline {
        Control::Until(d) => d.cancel.ok_or(Fault::Control(io::ErrorKind::InvalidInput))?,
        Control::ProcessOwned => -1,
    };
    let mut fds = [
        libc::pollfd {
            fd: if client_events == 0 { -1 } else { client },
            events: client_events,
            revents: 0,
        },
        libc::pollfd {
            fd: if backend_events == 0 { -1 } else { backend },
            events: backend_events,
            revents: 0,
        },
        libc::pollfd {
            fd: cancel,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let result = deadline
        .io(client, client_events, || {
            let timeout = match deadline {
                Control::Until(d) => {
                    let left = d.left()?;
                    let millis = left.as_nanos().div_ceil(Duration::from_millis(1).as_nanos());
                    i32::try_from(millis).unwrap_or(i32::MAX)
                }
                // Existing engine peer/process lifetime, never a fabricated Instant.
                Control::ProcessOwned => -1,
            };
            // SAFETY: initialized entries borrow this flow and its owned cancel FD.
            let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout) };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    return Err(error);
                }
                Ok(Err(error))
            } else {
                Ok(Ok(()))
            }
        })
        .map_err(control)?;
    if let Control::Until(d) = deadline {
        d.left().map_err(control)?;
        Deadline::cancellation_event(fds.last().map_or(0, |fd| fd.revents)).map_err(control)?;
    }
    result.map_err(|error| native(Leg::Client, Operation::Wait, error))?;
    for (fd, leg) in fds.iter().zip([Leg::Client, Leg::Backend]) {
        if fd.revents & libc::POLLNVAL != 0 {
            return Err(native(
                leg,
                Operation::Wait,
                io::Error::from_raw_os_error(libc::EBADF),
            ));
        }
        // HUP/ERR require the next actual operation, never an EOF/success label.
    }
    Ok(())
}

pub(crate) fn pump(
    streams: [BorrowedFd<'_>; 2],
    slots: Slots,
    limits: [ByteLimit; 2],
    deadline: Control<'_>,
    end: EndPolicy,
    report: &mut Report,
) -> Result<Termination, Fault> {
    if let Control::Until(d) = deadline
        && d.cancel.is_none()
    {
        return Err(Fault::Control(io::ErrorKind::InvalidInput));
    }
    let [client, backend] = streams;
    let [request, response] = limits;
    let mut up = Direction {
        bytes: slots.to_backend,
        sent: 0,
        limit: request,
        source: Leg::Client,
    };
    let mut down = Direction {
        bytes: slots.to_client,
        sent: 0,
        limit: response,
        source: Leg::Backend,
    };
    loop {
        let up_changed = up.turn(client, backend, &mut report.to_backend, &deadline, end)?;
        let down_changed = down.turn(backend, client, &mut report.to_client, &deadline, end)?;
        if end == EndPolicy::EnginePeerEnd
            && matches!(
                report.to_client.half,
                Half::Draining(SourceEnd::Eof) | Half::WriteClosed(SourceEnd::Eof)
            )
            && report.to_backend.half == Half::Reading
        {
            report.to_backend.half = Half::Draining(SourceEnd::PeerEnded);
        }
        if matches!(report.to_backend.half, Half::WriteClosed(_))
            && matches!(report.to_client.half, Half::WriteClosed(_))
        {
            return Ok(match end {
                EndPolicy::Independent => Termination::BothHalvesClosed,
                EndPolicy::EnginePeerEnd => Termination::BackendEnded,
            });
        }
        if up_changed || down_changed {
            continue;
        }
        let interest = |read: &Progress, write: &Progress| {
            (if read.half == Half::Reading && read.pending == 0 {
                libc::POLLIN
            } else {
                0
            }) | (if write.pending != 0 || matches!(write.half, Half::Draining(_)) {
                libc::POLLOUT
            } else {
                0
            })
        };
        wait(
            &deadline,
            client.as_raw_fd(),
            interest(&report.to_backend, &report.to_client),
            backend.as_raw_fd(),
            interest(&report.to_client, &report.to_backend),
        )?;
    }
}
