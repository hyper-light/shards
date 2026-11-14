//! A member's MCP connections to its run's controller (D254), init's side: the socket a member
//! whose run asks for it has at [`run::mcp::SOCKET`], the stdio bridge beside it at
//! [`run::mcp::BRIDGE`] (this binary, read only, run by that name), and the relay that carries
//! each connection made to the socket to the run's host.
//!
//! The socket lies in the member's own directory of `/run/shards`, which its domain alone sees
//! (domains.rs `SERVER_AT`), its owner the member's identity, mode 0600: no other member's
//! process reaches it. The relay takes each connection on a thread of its own, as a lane's is
//! (lanes.rs), dials [`run::MCP_PORT`], says the run's token in its HELLO, then the member's
//! declared index ([`run::mcp::preamble`]), then carries the bytes each way, each way's end
//! that way's half-close (`native_relay::pump`, `EndPolicy::Independent`), a member gone while
//! its host's way is open the connection's cut, as a lane's (D251). The member never
//! holds the token: what it says reaches its host as its own, never as another member's.

use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

use shards_abi::run::{self, kind, mcp};

use crate::engine::native_relay;

/// What a run asks of its members' MCP connections (D254): the token they are carried by, and
/// the members, by their declared indices, that have a bridge and a socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asked {
    pub token: [u8; run::TOKEN],
    pub members: Vec<u32>,
}

impl Asked {
    /// What `setup` asks, checked: none where it names no member; refused where it names one
    /// with no token to carry its connections by, or either entry is malformed.
    pub fn read(setup: &[Vec<u8>]) -> Result<Option<Asked>, String> {
        let members = mcp::members(setup).map_err(|e| format!("its members' MCP bridges: {e}"))?;
        let token = mcp::token(setup).map_err(|e| format!("its MCP token: {e}"))?;
        match (token, members.is_empty()) {
            (_, true) => Ok(None),
            (Some(token), false) => Ok(Some(Asked { token, members })),
            (None, false) => Err("its members' MCP bridges are asked without the run's MCP token".into()),
        }
    }
}

/// The socket of member `uid`'s in `dir`, its directory of `/run/shards` as init sees it:
/// listening, the member's own (mode 0600), with the bridge's mount point beside it, an empty
/// file the bridge is mounted on in the member's view.
pub fn listen(dir: &Path, uid: u32) -> io::Result<UnixListener> {
    use std::os::unix::fs::PermissionsExt as _;
    let socket = dir.join(socket_name());
    // What a restored template left there is the run's own to replace.
    match std::fs::remove_file(&socket) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let listener = UnixListener::bind(&socket)?;
    std::os::unix::fs::chown(&socket, Some(uid), Some(uid))?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    let point = dir.join(mcp::BRIDGE_NAME);
    std::fs::File::create(&point)?;
    std::fs::set_permissions(&point, std::fs::Permissions::from_mode(0o444))?;
    Ok(listener)
}

/// The socket's name in its directory: [`run::mcp::SOCKET`]'s last part.
fn socket_name() -> &'static str {
    mcp::SOCKET.rsplit('/').next().unwrap_or(mcp::SOCKET)
}

/// Carries each connection to `listener`, member `member`'s socket, to the run's host by
/// `token`, each on a thread of its own: a connect that waits on the host holds that
/// connection alone.
pub fn serve(listener: UnixListener, member: u32, token: [u8; run::TOKEN]) -> io::Result<()> {
    std::thread::Builder::new()
        .name("mcp".into())
        .spawn(move || {
            for conn in listener.incoming() {
                let local = match conn {
                    Ok(local) => local,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    // A listener that fails is no member's way to its host any more.
                    Err(e) => {
                        let _ = writeln!(io::stderr(), "shards-init: a member's MCP socket: {e}");
                        return;
                    }
                };
                let carried = std::thread::Builder::new()
                    .name("mcp lane".into())
                    .spawn(move || match crate::run::dial(run::MCP_PORT, true) {
                        Ok(host) => carry(&local, &host, member, &token),
                        Err(e) => {
                            let _ = writeln!(
                                io::stderr(),
                                "shards-init: a member's MCP connection to its host: {e}"
                            );
                        }
                    });
                if let Err(e) = carried {
                    let _ = writeln!(io::stderr(), "shards-init: a member's MCP connection: {e}");
                }
            }
        })
        .map(drop)
}

/// What a connection says before its bytes: its HELLO with `token`, then member `member`.
fn said(token: &[u8; run::TOKEN], member: u32) -> Vec<u8> {
    [
        &run::header(kind::HELLO, run::TOKEN as u32)[..],
        token,
        &mcp::preamble(member),
    ]
    .concat()
}

/// Carries `local`, a member's connection, over `host`: its HELLO and member said first, then
/// the bytes each way until both ways have ended. Only a fault is said, on init's stderr.
fn carry(local: &UnixStream, host: &impl AsFd, member: u32, token: &[u8; run::TOKEN]) {
    let fd = host.as_fd();
    let told = fd
        .try_clone_to_owned()
        .and_then(|own| std::fs::File::from(own).write_all(&said(token, member)));
    if let Err(e) = told {
        let _ = writeln!(
            io::stderr(),
            "shards-init: a member's MCP connection to its host: {e}"
        );
        return;
    }
    let slots = match native_relay::Slots::vsock_packet() {
        Ok(slots) => slots,
        Err(fault) => {
            let _ = writeln!(io::stderr(), "shards-init: a member's MCP connection: {fault:?}");
            return;
        }
    };
    let mut report = native_relay::Report::new();
    if let Err(fault) = native_relay::pump(
        [local.as_fd(), fd],
        slots,
        [native_relay::ByteLimit::PeerEnd; 2],
        native_relay::Control::ProcessOwned,
        native_relay::EndPolicy::Independent,
        &mut report,
    ) {
        let _ = writeln!(io::stderr(), "shards-init: a member's MCP connection: {fault:?}");
    }
}

/// The bridge's copy of init's binary for a member's view: a mount of its own of this binary,
/// read only, no setuid, no devices, and runnable, which the member's domain mounts at
/// [`run::mcp::BRIDGE`] on the socket's directory's empty file.
pub fn bridge_mount() -> io::Result<OwnedFd> {
    crate::domains::copy_of(
        "/proc/self/exe",
        libc::MOUNT_ATTR_RDONLY | libc::MOUNT_ATTR_NOSUID | libc::MOUNT_ATTR_NODEV,
    )
}

/// Whether this process is the bridge: run, as no PID 1, by [`run::mcp::BRIDGE_NAME`], its
/// command's last part.
pub fn is_bridge() -> bool {
    std::process::id() != 1
        && std::env::args_os()
            .next()
            .is_some_and(|a| Path::new(&a).file_name().is_some_and(|n| n == mcp::BRIDGE_NAME))
}

/// The bridge (D254): its stdin to the member's socket, the socket to its stdout, each way
/// ending as its source does. Its status: 0 once both have ended, 1 where the socket cannot
/// be reached or either way fails, said on its stderr.
pub fn bridge() -> i32 {
    let said = |why: String| {
        let _ = writeln!(io::stderr(), "{}: {why}", mcp::BRIDGE);
        1
    };
    let socket = match UnixStream::connect(mcp::SOCKET) {
        Ok(socket) => socket,
        Err(e) => return said(format!("{}: {e}", mcp::SOCKET)),
    };
    // SAFETY: this dedicated bridge process owns its inherited stdio, and no other
    // thread or Rust stdio handle uses these descriptors. The copies close them.
    let (input, output) = unsafe {
        (
            std::fs::File::from_raw_fd(libc::STDIN_FILENO),
            std::fs::File::from_raw_fd(libc::STDOUT_FILENO),
        )
    };
    match copy_both(input, output, socket) {
        Ok(()) => 0,
        Err(e) => said(e.to_string()),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Half {
    Reading,
    Draining,
    Closed,
}

struct Copy {
    // pipe(7): no write exceeds PIPE_BUF, the pipe's atomic-write bound.
    bytes: [u8; libc::PIPE_BUF],
    pending: std::ops::Range<usize>,
    half: Half,
}

impl Copy {
    fn new() -> Self {
        Self {
            bytes: [0; libc::PIPE_BUF],
            pending: 0..0,
            half: Half::Reading,
        }
    }

    fn reading(&self) -> bool {
        self.half == Half::Reading && self.pending.is_empty()
    }

    fn drained(&self) -> bool {
        self.half == Half::Draining && self.pending.is_empty()
    }

    fn read(&mut self, from: &mut impl Read) -> io::Result<()> {
        if self.reading() {
            match from.read(&mut self.bytes) {
                Ok(0) => self.half = Half::Draining,
                Ok(n) => self.pending = 0..n,
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn write(&mut self, to: &mut impl Write) -> io::Result<()> {
        if !self.pending.is_empty() {
            let bytes = self
                .bytes
                .get(self.pending.clone())
                .ok_or(io::ErrorKind::InvalidData)?;
            match to.write(bytes) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    self.pending.start = self
                        .pending
                        .start
                        .checked_add(n)
                        .ok_or(io::ErrorKind::InvalidData)?;
                }
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// Owns all three descriptors: an error closes every leg without waiting on another
/// read. A socket half-close drains and closes stdout while stdin can still flow;
/// POLLHUP (the peer gone) also ends the input, unlike POLLRDHUP (shutdown-write).
fn copy_both(mut input: std::fs::File, output: std::fs::File, mut socket: UnixStream) -> io::Result<()> {
    for fd in [input.as_raw_fd(), output.as_raw_fd(), socket.as_raw_fd()] {
        // SAFETY: fcntl of live descriptors exclusively owned by this copy.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        // SAFETY: preserve all existing flags, adding nonblocking operation.
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let (mut up, mut down) = (Copy::new(), Copy::new());
    let mut output = Some(output);
    loop {
        up.read(&mut input)?;
        up.write(&mut socket)?;
        down.read(&mut socket)?;
        if let Some(out) = &mut output {
            down.write(out)?;
        }
        if up.drained() {
            socket.shutdown(std::net::Shutdown::Write)?;
            up.half = Half::Closed;
        }
        if down.drained() {
            drop(output.take());
            down.half = Half::Closed;
        }
        if up.half == Half::Closed && down.half == Half::Closed {
            return Ok(());
        }
        let sock_events = if down.reading() { libc::POLLIN } else { 0 }
            | if !up.pending.is_empty() { libc::POLLOUT } else { 0 };
        let mut fds = [
            libc::pollfd {
                fd: if up.reading() { input.as_raw_fd() } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: output
                    .as_ref()
                    .filter(|_| !down.pending.is_empty())
                    .map_or(-1, AsRawFd::as_raw_fd),
                events: libc::POLLOUT,
                revents: 0,
            },
            libc::pollfd {
                fd: if sock_events != 0 || up.half == Half::Reading {
                    socket.as_raw_fd()
                } else {
                    -1
                },
                events: sock_events,
                revents: 0,
            },
        ];
        // SAFETY: poll fills this initialized array; -1 waits for an event, not a timer.
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) } < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if fds.iter().any(|fd| fd.revents & libc::POLLNVAL != 0) {
            return Err(io::Error::from_raw_os_error(libc::EBADF));
        }
        if fds.last().is_some_and(|fd| fd.revents & libc::POLLERR != 0) {
            return Err(socket
                .take_error()?
                .unwrap_or_else(|| io::ErrorKind::ConnectionAborted.into()));
        }
        if fds.last().is_some_and(|fd| fd.revents & libc::POLLHUP != 0) {
            if !up.pending.is_empty() {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            up.half = Half::Closed;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn pipe() -> (std::fs::File, std::fs::File) {
        use std::os::fd::FromRawFd;
        let mut fds = [-1; 2];
        // SAFETY: pipe2 initializes both descriptors on success; each becomes owned once.
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        // SAFETY: the fresh descriptors returned by pipe2, owned by these files alone.
        unsafe {
            (
                std::fs::File::from_raw_fd(fds[0]),
                std::fs::File::from_raw_fd(fds[1]),
            )
        }
    }

    #[test]
    fn bridge_peer_end_releases_open_stdin() {
        let (input, writer) = pipe();
        let (mut reader, output) = pipe();
        let (socket, peer) = UnixStream::pair().unwrap();
        let (done, finished) = std::sync::mpsc::channel();
        let bridge = std::thread::spawn(move || {
            done.send(copy_both(input, output, socket)).unwrap();
        });
        drop(peer);
        let ended = finished.recv_timeout(std::time::Duration::from_secs(1));
        // Always unblock the old implementation before asserting, including on red.
        drop(writer);
        bridge.join().unwrap();
        assert!(ended.is_ok(), "the bridge waited for still-open harness stdin");
        assert!(ended.unwrap().is_ok());
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert!(bytes.is_empty());
    }

    #[test]
    fn bridge_output_failure_releases_open_stdin() {
        let (input, writer) = pipe();
        let (reader, output) = pipe();
        let (socket, mut peer) = UnixStream::pair().unwrap();
        let (done, finished) = std::sync::mpsc::channel();
        let bridge = std::thread::spawn(move || {
            done.send(copy_both(input, output, socket)).unwrap();
        });
        drop(reader);
        peer.write_all(b"response\n").unwrap();
        peer.shutdown(std::net::Shutdown::Write).unwrap();
        let ended = finished.recv_timeout(std::time::Duration::from_secs(1));
        drop(writer);
        bridge.join().unwrap();
        assert!(
            ended.is_ok(),
            "stdout failure waited for still-open harness stdin"
        );
        assert_eq!(ended.unwrap().unwrap_err().kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn bridge_half_close_ends_stdout_but_keeps_stdin() {
        let (input, mut writer) = pipe();
        let (mut reader, output) = pipe();
        let (socket, mut peer) = UnixStream::pair().unwrap();
        let bridge = std::thread::spawn(move || copy_both(input, output, socket));
        peer.write_all(b"response\n").unwrap();
        peer.shutdown(std::net::Shutdown::Write).unwrap();
        let mut response = [0; b"response\n".len()];
        reader.read_exact(&mut response).unwrap();
        let mut ready = libc::pollfd {
            fd: reader.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one live pipe; this test-only bound permits cleanup of a regression.
        let ended = unsafe { libc::poll(&raw mut ready, 1, 1000) };
        writer.write_all(b"after eof\n").unwrap();
        drop(writer);
        let mut request = Vec::new();
        peer.read_to_end(&mut request).unwrap();
        bridge.join().unwrap().unwrap();
        assert!(ended > 0, "the socket half-close did not close stdout");
        assert_eq!(reader.read(&mut [0]).unwrap(), 0);
        assert_eq!(request, b"after eof\n");
    }

    /// D254: a run's ask is its members' bridges and its token, together: members named with
    /// no token are refused, a token alone gives no member one, a member named twice refuses
    /// the whole.
    #[test]
    fn a_runs_mcp_ask_is_its_members_and_its_token() {
        let token = [7u8; run::TOKEN];
        let setup = vec![mcp::entry(1), mcp::token_entry(&token), mcp::entry(3)];
        assert_eq!(
            Asked::read(&setup).unwrap(),
            Some(Asked {
                token,
                members: vec![1, 3]
            })
        );
        assert_eq!(Asked::read(&[mcp::token_entry(&token)]).unwrap(), None);
        assert!(Asked::read(&[mcp::entry(1)]).is_err());
        assert!(Asked::read(&[mcp::entry(1), mcp::entry(1), mcp::token_entry(&token)]).is_err());
    }

    /// D254: a member's socket is its own (mode 0600) in its directory, the bridge's mount point
    /// an empty file beside it; what a restored template left at its name is replaced.
    #[test]
    fn a_members_socket_is_its_own_beside_its_bridges_mount_point() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let dir = shards_testdir::TempDir::new("mcp").unwrap();
        // This process's own identity: its directory's owner.
        let uid = std::fs::metadata(dir.path()).unwrap().uid();
        std::fs::write(dir.path().join(socket_name()), b"stale").unwrap();
        let listener = listen(dir.path(), uid).unwrap();
        let socket = dir.path().join(socket_name());
        let meta = std::fs::metadata(&socket).unwrap();
        assert_eq!((meta.mode() & 0o777, meta.uid()), (0o600, uid));
        let point = std::fs::metadata(dir.path().join(mcp::BRIDGE_NAME)).unwrap();
        assert!(point.is_file() && point.len() == 0);
        assert_eq!(point.permissions().mode() & 0o777, 0o444);
        let _client = UnixStream::connect(&socket).unwrap();
        assert!(listener.accept().is_ok());
    }

    /// D254: the relay says the run's token in its HELLO and the member's declared index
    /// before anything of the member's, then carries the bytes each way, each way's end its
    /// own: the member's half-close reaches the host, and the host's answer still reaches the
    /// member after it.
    #[test]
    fn a_members_connection_is_said_as_its_own_then_carried_both_ways() {
        let token = [9u8; run::TOKEN];
        let (local, mut member) = UnixStream::pair().unwrap();
        let (host, mut vm) = UnixStream::pair().unwrap();
        let relay = std::thread::spawn(move || carry(&local, &host, 4, &token));
        let mut hello = vec![0u8; run::HEADER + run::TOKEN + mcp::PREAMBLE];
        vm.read_exact(&mut hello).unwrap();
        assert_eq!(hello, said(&token, 4));
        assert_eq!(
            run::parse_header(hello[..run::HEADER].try_into().unwrap()),
            Some((kind::HELLO, run::TOKEN as u32))
        );
        assert_eq!(
            mcp::read_preamble(hello[run::HEADER + run::TOKEN..].try_into().unwrap()),
            4
        );
        member.write_all(b"{\"jsonrpc\":\"2.0\"}\n").unwrap();
        member.shutdown(std::net::Shutdown::Write).unwrap();
        let mut up = Vec::new();
        vm.read_to_end(&mut up).unwrap();
        assert_eq!(up, b"{\"jsonrpc\":\"2.0\"}\n");
        vm.write_all(b"{\"result\":1}\n").unwrap();
        vm.shutdown(std::net::Shutdown::Write).unwrap();
        let mut down = Vec::new();
        member.read_to_end(&mut down).unwrap();
        assert_eq!(down, b"{\"result\":1}\n");
        relay.join().unwrap();
    }

    /// D254: the bridge copies its input to the socket and the socket to its output, its
    /// input's end said to the socket as its half-close, done once both ways have ended.
    #[test]
    fn the_bridge_copies_both_ways_each_ending_as_its_source_ends() {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let mut got = Vec::new();
            theirs.read_to_end(&mut got).unwrap();
            theirs.write_all(b"answered\n").unwrap();
            got
        });
        let (input, mut writer) = pipe();
        writer.write_all(b"asked\n").unwrap();
        drop(writer);
        let (mut reader, output) = pipe();
        copy_both(input, output, ours).unwrap();
        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        assert_eq!(server.join().unwrap(), b"asked\n");
        assert_eq!(output, b"answered\n");
    }
}
