//! A warm VM: restored from a template, resumed and connected, waiting for the daemon to
//! hand it one request (docs/design/architecture.md D26). `shards vm restore DIR --warm FD`
//! runs one, FD being its Unix socket to the daemon.
//!
//! It tells the daemon `READY` once the guest waits for its command. The daemon answers
//! with `RUN`: the command, the client's connection, and the client's stdin, stdout and
//! stderr, which become this process's, so the workload's stdio is the client's. The
//! client's signals arrive on its connection as `SIGNAL`, and `EXIT` tells it the command's
//! status as soon as the command ends (shards_ipc::kind).

use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use shards_abi::run::Spec;
use shards_ipc::kind;

use crate::workload::{self, NOT_RUN, ToGuest};

/// The daemon's socket at `fd`, which must be an open Unix socket other than stdio.
pub fn daemon_socket(fd: RawFd) -> Result<UnixStream, String> {
    if fd < 3 {
        return Err(format!("--warm {fd}: not a descriptor of its own"));
    }
    // SAFETY: fstat(2) into a zeroed stat buffer; any descriptor number is safe to ask about.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return Err(format!("--warm {fd}: {}", io::Error::last_os_error()));
    }
    if st.st_mode & libc::S_IFMT != libc::S_IFSOCK {
        return Err(format!("--warm {fd}: not a socket"));
    }
    // SAFETY: an open socket the daemon left for this process alone, owned from here on.
    let socket = unsafe { UnixStream::from_raw_fd(fd) };
    // SAFETY: fcntl(2) on a descriptor we own.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
        return Err(format!("--warm {fd}: {}", io::Error::last_os_error()));
    }
    Ok(socket)
}

/// Tells the daemon this VM is ready, then waits for its request. Makes the client's stdio
/// this process's, starts passing the client's signals to the workload through `to`, and
/// returns the client's connection, the command, and whether the command reads stdin.
pub fn receive(daemon: &UnixStream, to: &ToGuest) -> Result<(UnixStream, Spec, bool), String> {
    shards_ipc::send(daemon, kind::READY, &[], &[]).map_err(|e| format!("telling the daemon: {e}"))?;
    let request = shards_ipc::recv(daemon)
        .map_err(|e| format!("waiting for a request: {e}"))?
        .ok_or("the daemon closed without a request")?;
    if request.kind != kind::RUN {
        return Err(format!("expected a request, got message kind {}", request.kind));
    }
    let count = request.fds.len();
    let [client, stdin, stdout, stderr]: [OwnedFd; 4] = request
        .fds
        .try_into()
        .map_err(|_| format!("a request brings 4 descriptors, not {count}"))?;
    let (flags, spec) = request.payload.split_first().ok_or("an empty request")?;
    let spec = Spec::decode(spec).ok_or("a malformed command")?;
    for (fd, target) in [(&stdin, 0), (&stdout, 1), (&stderr, 2)] {
        // SAFETY: dup2(2) onto this process's standard descriptors, which nothing else in
        // this process holds open as its own.
        if unsafe { libc::dup2(fd.as_raw_fd(), target) } < 0 {
            return Err(format!(
                "taking the client's stdio: {}",
                io::Error::last_os_error()
            ));
        }
    }
    let client = UnixStream::from(client);
    let signals = client
        .try_clone()
        .map_err(|e| format!("the client's connection: {e}"))?;
    let to = to.clone();
    std::thread::Builder::new()
        .name("client-signals".into())
        .spawn(move || relay_signals(&signals, &to))
        .map_err(|e| format!("client signal thread: {e}"))?;
    Ok((client, spec, flags & shards_ipc::RUN_INTERACTIVE != 0))
}

/// Passes the client's signals to the workload until the client hangs up. A workload
/// outlives its client, as a container outlives `docker run`'s.
fn relay_signals(client: &UnixStream, to: &ToGuest) {
    while let Ok(Some(message)) = shards_ipc::recv(client) {
        if message.kind != kind::SIGNAL {
            continue;
        }
        if let Ok(signal) = <[u8; 4]>::try_from(message.payload.as_slice()) {
            workload::signal_guest(to, u32::from_be_bytes(signal));
        }
    }
}

/// Tells the client how its command ended. An error goes to its stderr first, so that it
/// arrives before the client exits.
pub fn finish(client: &UnixStream, served: &Result<u8, String>) {
    let status = match served {
        Ok(status) => *status,
        Err(e) => {
            let _ = writeln!(io::stderr(), "shards: {e}");
            NOT_RUN
        }
    };
    let _ = shards_ipc::send(client, kind::EXIT, &[status], &[]);
}
