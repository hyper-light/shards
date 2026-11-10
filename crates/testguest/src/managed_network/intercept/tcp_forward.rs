//! One owned native flow. Payload tails and the original Deadline are its only
//! scheduling state; fixture scripts and eventual init callers share this pump.

use super::*;
use std::os::fd::{AsFd, BorrowedFd};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Leg {
    Client,
    Backend,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Half {
    Reading,
    DrainingAfterEof,
    WriteClosed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Connect {
    NotAttempted,
    Attempted,
    Pending,
    Connected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Operation {
    Connect,
    Read,
    Write,
    Shutdown,
    Wait,
}

#[derive(Debug)]
pub(super) enum Fault {
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
    ExtraData {
        leg: Leg,
        offset: usize,
    },
    Bounds,
}

fn native(leg: Leg, operation: Operation, error: io::Error) -> Fault {
    Fault::Native {
        leg,
        operation,
        kind: error.kind(),
        errno: error.raw_os_error(),
    }
}

fn control(error: io::Error) -> Fault {
    match error.kind() {
        io::ErrorKind::TimedOut => Fault::Expired,
        io::ErrorKind::ConnectionAborted => Fault::Cancelled,
        kind => Fault::Control(kind),
    }
}

enum Attempt<T> {
    Ready(T),
    WouldBlock,
}

// Keep native failures inside the result: a socket ECONNABORTED is not the
// cancellation descriptor's ConnectionAborted. Only actual EINTR is readmitted.
fn attempt<T>(
    deadline: &Deadline,
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

pub(super) struct Slots {
    to_backend: Vec<u8>,
    to_client: Vec<u8>,
}

impl Slots {
    pub(super) fn reserve(to_backend: usize, to_client: usize) -> Result<Self, Fault> {
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
            to_backend: slot(to_backend)?,
            to_client: slot(to_client)?,
        })
    }
}

#[derive(Debug)]
pub(super) struct Progress {
    pub(super) half: Half,
    pub(super) read: usize,
    pub(super) written: usize,
    pub(super) pending: usize,
    pub(super) extra_consumed: usize,
    pub(super) partial_writes: usize,
    pub(super) would_block: usize,
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
        }
    }
}

#[derive(Debug)]
pub(super) struct Completion {
    pub(super) accepted: usize,
    pub(super) downstream: Option<Tuple>,
    pub(super) upstream: Option<Tuple>,
    pub(super) connect: Connect,
    pub(super) to_backend: Progress,
    pub(super) to_client: Progress,
    pub(super) result: Result<(), Fault>,
}

struct Direction {
    bytes: Vec<u8>,
    sent: usize,
    limit: usize,
    source: Leg,
}

impl Direction {
    fn turn(
        &mut self,
        source: BorrowedFd<'_>,
        destination: BorrowedFd<'_>,
        progress: &mut Progress,
        deadline: &Deadline,
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
                Attempt::WouldBlock => increment(&mut progress.would_block, 1)?,
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
        if progress.pending == 0 && progress.half == Half::DrainingAfterEof {
            match attempt(
                deadline,
                destination.as_raw_fd(),
                libc::POLLOUT,
                target,
                Operation::Shutdown,
                || {
                    // SAFETY: shutdown of this owned flow's borrowed socket.
                    count(unsafe { libc::shutdown(destination.as_raw_fd(), libc::SHUT_WR) } as isize)
                },
            )? {
                Attempt::Ready(_) => {
                    progress.half = Half::WriteClosed;
                    changed = true;
                }
                Attempt::WouldBlock => increment(&mut progress.would_block, 1)?,
            }
        }
        if progress.pending != 0 || progress.half != Half::Reading {
            return Ok(changed);
        }
        self.sent = 0;
        let left = self.limit.checked_sub(progress.read).ok_or(Fault::Bounds)?;
        // A nonempty probe distinguishes EOF from an extra byte at the exact
        // caller byte ceiling. Its consumed byte is retained separately, never sent.
        let mut extra = [0u8; 1];
        let bytes = if left == 0 {
            &mut extra[..]
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
                progress.half = Half::DrainingAfterEof;
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

fn count(result: isize) -> io::Result<usize> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        usize::try_from(result).map_err(io::Error::other)
    }
}

fn wait(
    deadline: &Deadline,
    client: RawFd,
    client_events: i16,
    backend: RawFd,
    backend_events: i16,
) -> Result<(), Fault> {
    let cancel = deadline
        .cancel
        .ok_or(Fault::Control(io::ErrorKind::InvalidInput))?;
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
            let left = deadline.left()?;
            let millis = left.as_nanos().div_ceil(Duration::from_millis(1).as_nanos());
            let timeout = i32::try_from(millis).unwrap_or(i32::MAX);
            // SAFETY: initialized fixed entries for two borrowed legs and owned cancel.
            let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout) };
            if n < 0 {
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
    deadline.left().map_err(control)?;
    Deadline::cancellation_event(fds.last().map_or(0, |fd| fd.revents)).map_err(control)?;
    result.map_err(|error| native(Leg::Client, Operation::Wait, error))?;
    for (fd, leg) in fds.iter().zip([Leg::Client, Leg::Backend]) {
        if fd.revents & libc::POLLNVAL != 0 {
            return Err(native(
                leg,
                Operation::Wait,
                io::Error::from_raw_os_error(libc::EBADF),
            ));
        }
        // HUP/ERR are not EOF/success. The next admitted native read/write
        // observes buffered bytes or its actual error; no ready bit is cached.
    }
    Ok(())
}

/// No endpoint-specific operation occurs here: the init engine's Unix/VSOCK
/// pair can use these same borrowed native sockets after shared extraction.
fn pump(
    client: BorrowedFd<'_>,
    backend: BorrowedFd<'_>,
    slots: Slots,
    limits: [usize; 2],
    deadline: &Deadline,
    report: &mut Completion,
) -> Result<(), Fault> {
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
        let up_changed = up.turn(client, backend, &mut report.to_backend, deadline)?;
        let down_changed = down.turn(backend, client, &mut report.to_client, deadline)?;
        if report.to_backend.half == Half::WriteClosed && report.to_client.half == Half::WriteClosed {
            return Ok(());
        }
        if up_changed || down_changed {
            continue;
        }
        let interest = |read: &Progress, write: &Progress| {
            (if read.half == Half::Reading && read.pending == 0 {
                libc::POLLIN
            } else {
                0
            }) | (if write.pending != 0 || write.half == Half::DrainingAfterEof {
                libc::POLLOUT
            } else {
                0
            })
        };
        wait(
            deadline,
            client.as_raw_fd(),
            interest(&report.to_backend, &report.to_client),
            backend.as_raw_fd(),
            interest(&report.to_client, &report.to_backend),
        )?;
    }
}

fn connect(
    upstream: &TcpStream,
    destination: SocketAddr,
    deadline: &Deadline,
    state: &mut Connect,
) -> Result<(), Fault> {
    let SocketAddr::V4(at) = destination else {
        return Err(Fault::Bounds);
    };
    let address = libc::sockaddr_in {
        sin_family: libc::AF_INET as _,
        sin_port: at.port().to_be(),
        sin_addr: libc::in_addr {
            s_addr: u32::from_ne_bytes(at.ip().octets()),
        },
        sin_zero: [0; 8],
    };
    let result = deadline
        .io(upstream.as_raw_fd(), libc::POLLOUT, || {
            *state = Connect::Attempted;
            // SAFETY: exact initialized IPv4 sockaddr; socket was created in the held router namespace.
            let n = unsafe {
                libc::connect(
                    upstream.as_raw_fd(),
                    (&raw const address).cast(),
                    size_of_val(&address) as _,
                )
            };
            // Even EINTR is a retained connect result, never a replayed connect.
            Ok(count(n as isize))
        })
        .map_err(control)?;
    match result {
        Ok(_) => {}
        Err(error) if error.raw_os_error() == Some(libc::EINPROGRESS) => {
            *state = Connect::Pending;
            wait(deadline, -1, 0, upstream.as_raw_fd(), libc::POLLOUT)?;
            match attempt(
                deadline,
                upstream.as_raw_fd(),
                libc::POLLOUT,
                Leg::Backend,
                Operation::Connect,
                || upstream.take_error(),
            )? {
                Attempt::Ready(None) => {}
                Attempt::Ready(Some(error)) => return Err(native(Leg::Backend, Operation::Connect, error)),
                Attempt::WouldBlock => return Err(Fault::Control(io::ErrorKind::WouldBlock)),
            }
        }
        Err(error) => return Err(native(Leg::Backend, Operation::Connect, error)),
    }
    if upstream
        .peer_addr()
        .map_err(|e| native(Leg::Backend, Operation::Connect, e))?
        != destination
    {
        return Err(Fault::Bounds);
    }
    *state = Connect::Connected;
    Ok(())
}

pub(super) fn forward(
    listener: TcpListener,
    upstream: TcpStream,
    destination: SocketAddr,
    slots: Slots,
    limits: [usize; 2],
    deadline: Deadline,
) -> Completion {
    let mut report = Completion {
        accepted: 0,
        downstream: None,
        upstream: None,
        connect: Connect::NotAttempted,
        to_backend: Progress::new(),
        to_client: Progress::new(),
        result: Ok(()),
    };
    report.result = (|| {
        let downstream = loop {
            match attempt(
                &deadline,
                listener.as_raw_fd(),
                libc::POLLIN,
                Leg::Client,
                Operation::Read,
                || listener.accept(),
            )? {
                Attempt::Ready((stream, _)) => {
                    report.accepted = 1;
                    break stream;
                }
                Attempt::WouldBlock => wait(&deadline, listener.as_raw_fd(), libc::POLLIN, -1, 0)?,
            }
        };
        run_accepted(
            downstream,
            upstream,
            destination,
            slots,
            limits,
            &deadline,
            &mut report,
        )
    })();
    report
}

pub(super) fn start(
    listener: TcpListener,
    upstream: TcpStream,
    destination: SocketAddr,
    slots: Slots,
    limits: [usize; 2],
    deadline: &Deadline,
) -> io::Result<Worker<Completion>> {
    Worker::spawn(deadline, move |deadline| {
        forward(listener, upstream, destination, slots, limits, deadline)
    })
}

fn run_accepted(
    downstream: TcpStream,
    upstream: TcpStream,
    destination: SocketAddr,
    slots: Slots,
    limits: [usize; 2],
    deadline: &Deadline,
    report: &mut Completion,
) -> Result<(), Fault> {
    if deadline.cancel.is_none() {
        return Err(Fault::Control(io::ErrorKind::InvalidInput));
    }
    downstream
        .set_nonblocking(true)
        .map_err(|e| native(Leg::Client, Operation::Read, e))?;
    report.downstream = Some(Tuple {
        local: downstream
            .local_addr()
            .map_err(|e| native(Leg::Client, Operation::Read, e))?,
        peer: downstream
            .peer_addr()
            .map_err(|e| native(Leg::Client, Operation::Read, e))?,
    });
    upstream
        .set_nonblocking(true)
        .map_err(|e| native(Leg::Backend, Operation::Connect, e))?;
    connect(&upstream, destination, deadline, &mut report.connect)?;
    report.upstream = Some(Tuple {
        local: upstream
            .local_addr()
            .map_err(|e| native(Leg::Backend, Operation::Connect, e))?,
        peer: upstream
            .peer_addr()
            .map_err(|e| native(Leg::Backend, Operation::Connect, e))?,
    });
    pump(
        downstream.as_fd(),
        upstream.as_fd(),
        slots,
        limits,
        deadline,
        report,
    )
}
