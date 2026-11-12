//! A run's own engine in its microVM (`--use-api-socket`, D181): the socket its command
//! finds it at, and the relay that takes each connection made to it to the host.
//!
//! The standby makes the socket in its own view of the run's files, where the command
//! finds it ([`listen`]): after the run's mounts, so that none hides it, and before its
//! root is made read-only. It gives init the listener, which init hands a process of its
//! own ([`start`]), outside the command's cgroup as the DNS relay is. That process takes
//! each connection, dials the host's engine port, says the run's token first (init's
//! channels are token-gated: a process of the microVM's that dials the port itself is
//! refused), and relays the bytes each way until either side ends.

use std::io::{self, Write as _};
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};

use shards_abi::run;

#[path = "relay.rs"]
#[expect(
    dead_code,
    reason = "this path-shared module also serves testguest's bounded TCP/cancellation caller"
)]
mod native_relay;

/// The listener's tag on the standby's error channel, as [`crate::hostbind::LISTENER`]
/// tags its filter's.
pub const LISTENER: u8 = b'E';

/// In the standby: the socket at [`run::ENGINE_SOCKET`], made where the command finds it,
/// every user of the run's able to reach it, its listener given init on `err`. The errno of
/// what failed.
pub fn listen(err: RawFd) -> Result<(), i32> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
    let errno = |e: io::Error| e.raw_os_error().unwrap_or(libc::EIO);
    let path = std::path::Path::new(run::ENGINE_SOCKET);
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(dir)
            .map_err(errno)?;
    }
    // What the image left there is the run's own to replace.
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(errno(e)),
        _ => {}
    }
    let listener = UnixListener::bind(path).map_err(errno)?;
    // Whoever the command runs as, and whoever it becomes: the run's own processes alone
    // see the path, and what they reach through it is their own microVM.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666)).map_err(errno)?;
    if !crate::hostbind::send_fd(err, &[LISTENER], listener.as_raw_fd(), 0) {
        return Err(io::Error::last_os_error().raw_os_error().unwrap_or(libc::EPIPE));
    }
    Ok(())
}

/// Init's end of the relay's process, which waits for the listener ([`Relay::give`]), and
/// ends without one once this end closes: started before the run's command, so that what
/// keeps it from starting keeps the command from starting.
pub struct Relay {
    sock: OwnedFd,
    /// The io_uring instance its ring slots are pinned on (D230), where its host takes
    /// rings: init's copy, which outlives the relay.
    pinner: Option<OwnedFd>,
}

/// Starts the relay's process, which says `token` first on each connection it relays, and
/// offers a ring where `rings` (its host takes them, D230) and the guest can pin one.
pub fn start(token: [u8; run::TOKEN], rings: bool) -> io::Result<Relay> {
    let (ours, theirs) = crate::hostbind::seqpacket()?;
    // Made before the fork, so that the relay inherits it and init keeps it (ring.rs).
    let pinner = if rings { crate::ring::pinner() } else { None };
    // SAFETY: fork(2) from init, which has no other thread: the child runs only this
    // module's code, on descriptors of its own.
    match unsafe { libc::fork() } {
        -1 => Err(io::Error::last_os_error()),
        0 => {
            let pinned = pinner.as_ref().map(|p| p.as_raw_fd());
            crate::dnsrelay::keep_only(
                &[Some(theirs.as_raw_fd()), pinned]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>(),
            );
            let mut tag = [0u8; 1];
            if let Ok((1, Some(listener))) = crate::hostbind::recv_fd(theirs.as_raw_fd(), &mut tag, 0)
                && tag == [LISTENER]
            {
                drop(theirs);
                if let Some(pinner) = pinner {
                    crate::ring::start(pinner);
                }
                serve(&UnixListener::from(listener), &token);
            }
            // SAFETY: _exit(2) of the child, which owns nothing to flush.
            unsafe { libc::_exit(0) }
        }
        _ => Ok(Relay { sock: ours, pinner }),
    }
}

impl Relay {
    /// Gives the process the standby's listener; false where it has gone.
    pub fn give(self, listener: &OwnedFd) -> bool {
        // Held until the VM ends, deliberately: a relay that dies must leave its slots
        // pinned while the host may still write them (D230, ring.rs).
        if let Some(pinner) = self.pinner {
            let _ = std::os::fd::IntoRawFd::into_raw_fd(pinner);
        }
        crate::hostbind::send_fd(self.sock.as_raw_fd(), &[LISTENER], listener.as_raw_fd(), 0)
    }
}

/// Each accepted connection has an owned relay thread; a listener error ends acceptance.
fn serve(listener: &UnixListener, token: &[u8; run::TOKEN]) {
    loop {
        match listener.accept() {
            Ok((conn, _)) => {
                let token = *token;
                // One that cannot have a thread is closed: its client sees the end.
                let _ = std::thread::Builder::new()
                    .name("engine".into())
                    .spawn(move || relay(conn, &token));
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted
                ) => {}
            Err(_) => return,
        }
    }
}

/// One connection: the host dialled, the run's token said, then the bytes each way until
/// either side ends. The host's end ends the client's both ways, so that a client that
/// keeps its connection open holds no thread here once the engine has closed it.
fn relay(conn: UnixStream, token: &[u8; run::TOKEN]) {
    // Reserve one vsock packet per direction before dialing or releasing HELLO;
    // this flow never grows or caches.
    let slots = match native_relay::Slots::vsock_packet() {
        Ok(slots) => slots,
        Err(error) => {
            let _ = writeln!(io::stderr(), "shards-init: engine reservation: {error:?}");
            return;
        }
    };
    let Ok(host) = crate::run::dial(run::ENGINE_PORT, true) else {
        return;
    };
    if let Err(e) = ask_window(host.as_raw_fd())
        && !WINDOW_REFUSAL_SAID.swap(true, Ordering::Relaxed)
    {
        let _ = writeln!(
            io::stderr(),
            "shards-init: engine relay keeps Linux's vsock buffer: {e}"
        );
    }
    // A ring offered in its hello where its relay has a slot (D230): taken, the
    // connection runs through it; refused, it goes on over vsock, its hello said.
    let said = match crate::ring::hello(&conn, &host, token) {
        Ok(crate::ring::Offered::Ringed) => return,
        Ok(crate::ring::Offered::Refused) => true,
        Ok(crate::ring::Offered::Plain) => false,
        Err(e) => {
            let _ = writeln!(io::stderr(), "shards-init: engine ring hello: {e}");
            return;
        }
    };
    let mut hello = [0u8; run::HEADER + run::TOKEN];
    let header = run::header(run::kind::HELLO, run::TOKEN as u32);
    for (out, byte) in hello.iter_mut().zip(header.iter().chain(token)) {
        *out = *byte;
    }
    if !said && (&host).write_all(&hello).is_err() {
        return;
    }
    forward(&conn, host.as_fd(), slots, &mut io::stderr());
}

/// How the pump ended one connection, read from its typed result.
#[derive(Debug)]
enum End {
    /// The pump's own end (EndPolicy::EnginePeerEnd): the engine ended its stream.
    Ended,
    /// The run's client closed its connection first, as one that stops following a
    /// stream does (`docker logs -f` interrupted). Linux says so on the relay's socket as
    /// ECONNRESET to a read, where the client left bytes unread, and as EPIPE to a write,
    /// or ECONNRESET where the write races the close (v6.18 net/unix/af_unix.c 696-711,
    /// 2433-2434, 2995; net/core/sock.c 2945-2947).
    ClientDeparted,
    Fault(native_relay::Fault),
}

impl End {
    fn of(result: Result<native_relay::Termination, native_relay::Fault>) -> Self {
        use native_relay::{Fault, Leg, Operation};
        match result {
            Ok(_) => Self::Ended,
            Err(
                Fault::Native {
                    leg: Leg::Client,
                    operation: Operation::Read,
                    errno: Some(libc::ECONNRESET),
                    ..
                }
                | Fault::Native {
                    leg: Leg::Client,
                    operation: Operation::Write,
                    errno: Some(libc::EPIPE | libc::ECONNRESET),
                    ..
                },
            ) => Self::ClientDeparted,
            Err(fault) => Self::Fault(fault),
        }
    }
}

/// After HELLO: the bytes each way until either side ends; only a fault is said on `log`.
fn forward(
    conn: &UnixStream,
    host: BorrowedFd<'_>,
    slots: native_relay::Slots,
    log: &mut impl io::Write,
) -> End {
    let mut report = native_relay::Report::new();
    let end = End::of(native_relay::pump(
        [conn.as_fd(), host],
        slots,
        [native_relay::ByteLimit::PeerEnd; 2],
        native_relay::Control::ProcessOwned,
        native_relay::EndPolicy::EnginePeerEnd,
        &mut report,
    ));
    match &end {
        // Successful engine EOF already ended both Unix halves in the pump.
        End::Ended => {}
        End::ClientDeparted => {
            let _ = conn.shutdown(std::net::Shutdown::Both);
        }
        End::Fault(fault) => {
            let _ = conn.shutdown(std::net::Shutdown::Both);
            let _ = writeln!(log, "shards-init: engine relay: {fault:?}");
        }
    }
    end
}

/// SO_VM_SOCKETS_BUFFER_SIZE and SO_VM_SOCKETS_BUFFER_MAX_SIZE, options of level AF_VSOCK
/// that take a u64 (include/uapi/linux/vm_sockets.h), which the libc crate does not have.
const SO_VM_SOCKETS_BUFFER_SIZE: libc::c_int = 0;
const SO_VM_SOCKETS_BUFFER_MAX_SIZE: libc::c_int = 2;

/// Whether a relay's socket that kept Linux's buffer was said: once, as every connection's
/// socket is the same kernel's.
static WINDOW_REFUSAL_SAID: AtomicBool = AtomicBool::new(false);

/// Asks the relay's vsock socket to buffer the host's whole window
/// ([`shards_abi::VSOCK_WINDOW`]): Linux sends no more unacknowledged than its own buffer
/// (8ee784fdf006), 256 KiB unless its program raises it (VSOCK_DEFAULT_BUFFER_SIZE,
/// net/vmw_vsock/af_vsock.c), and is sent no more than that. Its maximum first, which bounds
/// the size (`vsock_update_buffer_size`); Linux tells the host the new size at once
/// (`virtio_transport_notify_buffer_size`).
fn ask_window(fd: RawFd) -> io::Result<()> {
    let window = u64::from(shards_abi::VSOCK_WINDOW);
    for option in [SO_VM_SOCKETS_BUFFER_MAX_SIZE, SO_VM_SOCKETS_BUFFER_SIZE] {
        // SAFETY: setsockopt(2) of a u64 option from `window`, its size the length, on a
        // socket the caller holds.
        let set = unsafe {
            libc::setsockopt(
                fd,
                libc::AF_VSOCK,
                option,
                (&raw const window).cast(),
                std::mem::size_of::<u64>() as libc::socklen_t,
            )
        };
        if set != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::Read as _;
    use std::os::fd::FromRawFd as _;
    use std::thread::JoinHandle;

    // What `docker logs -f` asks of the engine; the relay carries it as bytes.
    const REQUEST: &[u8] = b"GET /v1.47/containers/c/logs?follow=1&stdout=1 HTTP/1.1\r\nHost: api\r\n\r\n";
    const LINE: &[u8] = b"a log line\n";

    /// `forward` on a thread between the client's end and the engine's end of two
    /// socketpairs, as `relay` runs it after HELLO: both ends, and the thread, which
    /// returns what it said on its log.
    fn start() -> (UnixStream, UnixStream, JoinHandle<Vec<u8>>) {
        let (client, conn) = UnixStream::pair().unwrap();
        let (host, engine) = UnixStream::pair().unwrap();
        let slots = native_relay::Slots::minimum_mapped().unwrap();
        let worker = std::thread::spawn(move || {
            let mut log = Vec::new();
            forward(&conn, host.as_fd(), slots, &mut log);
            log
        });
        (client, engine, worker)
    }

    fn said(worker: JoinHandle<Vec<u8>>) -> String {
        String::from_utf8(worker.join().unwrap()).unwrap()
    }

    fn readable(stream: &UnixStream) {
        let mut ready = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one initialized pollfd of a socket this test holds.
        assert_eq!(unsafe { libc::poll(&raw mut ready, 1, -1) }, 1);
    }

    /// `docker logs -f` interrupted while a line waits for it unread: Linux makes the
    /// relay's next read of its socket ECONNRESET (v6.18 net/unix/af_unix.c 707-709, 2995).
    /// The engine is still streaming: its end stays open until the relay has ended.
    #[test]
    fn a_client_that_leaves_a_line_unread_is_not_a_fault() {
        let (mut client, mut engine, worker) = start();
        client.write_all(REQUEST).unwrap();
        let mut request = [0u8; REQUEST.len()];
        engine.read_exact(&mut request).unwrap();
        assert_eq!(request.as_slice(), REQUEST);
        engine.write_all(LINE).unwrap();
        readable(&client);
        drop(client);
        assert_eq!(said(worker), "");
        drop(engine);
    }

    /// `docker logs -f` interrupted between lines: the relay's send of the next is EPIPE
    /// (af_unix.c 2433-2434).
    #[test]
    fn a_client_gone_before_the_next_line_is_not_a_fault() {
        let (mut client, mut engine, worker) = start();
        client.write_all(REQUEST).unwrap();
        let mut request = [0u8; REQUEST.len()];
        engine.read_exact(&mut request).unwrap();
        drop(client);
        // The client's end reaches the engine as the end of its request.
        assert_eq!(engine.read(&mut [0u8; 1]).unwrap(), 0);
        engine.write_all(LINE).unwrap();
        assert_eq!(said(worker), "");
        drop(engine);
    }

    /// Only the client's reset read or broken write is its departure; every other result
    /// keeps its own type.
    #[test]
    fn departure_is_only_the_clients_reset_read_or_broken_write() {
        use native_relay::{Fault, Leg, Operation, Termination};
        let native = |leg, operation, errno: i32| -> Result<Termination, Fault> {
            Err(Fault::Native {
                leg,
                operation,
                kind: io::Error::from_raw_os_error(errno).kind(),
                errno: Some(errno),
            })
        };
        for (result, departed) in [
            (native(Leg::Client, Operation::Read, libc::ECONNRESET), true),
            (native(Leg::Client, Operation::Write, libc::EPIPE), true),
            (native(Leg::Client, Operation::Write, libc::ECONNRESET), true),
            (native(Leg::Client, Operation::Read, libc::EPIPE), false),
            (native(Leg::Client, Operation::Read, libc::EIO), false),
            (native(Leg::Client, Operation::Wait, libc::ECONNRESET), false),
            (native(Leg::Backend, Operation::Read, libc::ECONNRESET), false),
            (native(Leg::Backend, Operation::Write, libc::EPIPE), false),
        ] {
            let end = End::of(result);
            assert_eq!(matches!(end, End::ClientDeparted), departed, "{end:?}");
        }
        assert!(matches!(End::of(Ok(Termination::BackendEnded)), End::Ended));
        assert!(matches!(End::of(Err(Fault::Bounds)), End::Fault(Fault::Bounds)));
    }

    /// The engine's end reset is no client's departure: it is still said, and the relay
    /// still ends the client's connection.
    #[test]
    fn an_engine_reset_is_still_said() {
        let (mut client, engine, worker) = start();
        client.write_all(REQUEST).unwrap();
        readable(&engine);
        drop(engine);
        assert_eq!(client.read(&mut [0u8; 1]).unwrap(), 0);
        assert_eq!(
            said(worker),
            format!(
                "shards-init: engine relay: Native {{ leg: Backend, operation: Read, kind: ConnectionReset, errno: Some({}) }}\n",
                libc::ECONNRESET
            )
        );
    }

    /// The relay's socket asks for the host's whole window, its maximum first, which bounds
    /// the size: a vsock socket of this kernel reads both back as the window.
    #[test]
    fn the_relays_socket_asks_for_the_hosts_window() {
        // SAFETY: socket(2) with constant arguments.
        let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            println!("SKIP: no vsock sockets here: {}", io::Error::last_os_error());
            return;
        }
        // SAFETY: a fresh descriptor nothing else owns.
        let sock = unsafe { OwnedFd::from_raw_fd(fd) };
        ask_window(sock.as_raw_fd()).unwrap();
        let read = |option| {
            let mut got = 0u64;
            let mut len = std::mem::size_of::<u64>() as libc::socklen_t;
            // SAFETY: getsockopt(2) of a u64 option into `got`, `len` its length.
            let rc = unsafe {
                libc::getsockopt(
                    sock.as_raw_fd(),
                    libc::AF_VSOCK,
                    option,
                    (&raw mut got).cast(),
                    &raw mut len,
                )
            };
            assert_eq!(rc, 0, "{}", io::Error::last_os_error());
            got
        };
        let window = u64::from(shards_abi::VSOCK_WINDOW);
        assert_eq!(
            (
                read(SO_VM_SOCKETS_BUFFER_MAX_SIZE),
                read(SO_VM_SOCKETS_BUFFER_SIZE)
            ),
            (window, window)
        );
    }
}
