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
//! that way's half-close (`native_relay::pump`, `EndPolicy::Independent`). The member never
//! holds the token: what it says reaches its host as its own, never as another member's.

use std::io::{self, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
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
    match copy_both(io::stdin(), io::stdout(), &socket) {
        Ok(()) => 0,
        Err(e) => said(e.to_string()),
    }
}

/// Copies `input` to `socket` and `socket` to `output`, each on its own, each way ending as its
/// source ends: `input`'s end said to the socket as its half-close, the socket's end ending
/// `output`'s copy. Done once both have ended; the first failure of either.
fn copy_both(mut input: impl Read + Send, mut output: impl Write, socket: &UnixStream) -> io::Result<()> {
    let mut up_socket = socket.try_clone()?;
    std::thread::scope(|s| {
        let up = std::thread::Builder::new()
            .name("bridge up".into())
            .spawn_scoped(s, move || -> io::Result<()> {
                io::copy(&mut input, &mut up_socket)?;
                up_socket.shutdown(std::net::Shutdown::Write)
            })?;
        let mut down_socket = socket;
        let down = io::copy(&mut down_socket, &mut output).and_then(|_| output.flush());
        let up = up
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("its input's copy ended unwound")));
        down.and(up)
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

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
        let uid = unsafe { libc::getuid() };
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
        let mut output = Vec::new();
        copy_both(&b"asked\n"[..], &mut output, &ours).unwrap();
        assert_eq!(server.join().unwrap(), b"asked\n");
        assert_eq!(output, b"answered\n");
    }
}
