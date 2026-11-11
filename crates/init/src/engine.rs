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
use std::os::fd::{AsFd as _, AsRawFd as _, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};

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
}

/// Starts the relay's process, which says `token` first on each connection it relays.
pub fn start(token: [u8; run::TOKEN]) -> io::Result<Relay> {
    let (ours, theirs) = crate::hostbind::seqpacket()?;
    // SAFETY: fork(2) from init, which has no other thread: the child runs only this
    // module's code, on descriptors of its own.
    match unsafe { libc::fork() } {
        -1 => Err(io::Error::last_os_error()),
        0 => {
            crate::dnsrelay::keep_only(&[theirs.as_raw_fd()]);
            let mut tag = [0u8; 1];
            if let Ok((1, Some(listener))) = crate::hostbind::recv_fd(theirs.as_raw_fd(), &mut tag, 0)
                && tag == [LISTENER]
            {
                drop(theirs);
                serve(&UnixListener::from(listener), &token);
            }
            // SAFETY: _exit(2) of the child, which owns nothing to flush.
            unsafe { libc::_exit(0) }
        }
        _ => Ok(Relay { sock: ours }),
    }
}

impl Relay {
    /// Gives the process the standby's listener; false where it has gone.
    pub fn give(self, listener: &OwnedFd) -> bool {
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
    let mut hello = [0u8; run::HEADER + run::TOKEN];
    let header = run::header(run::kind::HELLO, run::TOKEN as u32);
    for (out, byte) in hello.iter_mut().zip(header.iter().chain(token)) {
        *out = *byte;
    }
    if (&host).write_all(&hello).is_err() {
        return;
    }
    let mut report = native_relay::Report::new();
    let result = native_relay::pump(
        [conn.as_fd(), host.as_fd()],
        slots,
        [native_relay::ByteLimit::PeerEnd; 2],
        native_relay::Control::ProcessOwned,
        native_relay::EndPolicy::EnginePeerEnd,
        &mut report,
    );
    if let Err(error) = result {
        // Successful engine EOF already ended both Unix halves in the pump.
        let _ = conn.shutdown(std::net::Shutdown::Both);
        let _ = writeln!(io::stderr(), "shards-init: engine relay: {error:?}");
    }
}
