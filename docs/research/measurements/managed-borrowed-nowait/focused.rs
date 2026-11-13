//! Focused synthetic-muxer checks. Actual VM/lifecycle/platform campaigns belong to root.
use super::*;
use std::sync::mpsc;

#[cfg(windows)]
use shards_afunix::{UnixListener, UnixStream};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

fn identity(path: PathBuf) -> Identity {
    let pid = std::process::id();
    let started = crate::taken::started(pid).unwrap();
    Identity {
        pid,
        started,
        vsock: path,
    }
}

fn request(stream: &mut impl io::Read, port: u32) {
    let expected = format!("CONNECT {port}\n");
    let mut bytes = vec![0; expected.len()];
    stream.read_exact(&mut bytes).unwrap();
    assert_eq!(bytes, expected.as_bytes());
}

#[path = "contract.rs"]
mod contract;

/// A fixture for one attempt, at a socket of its own: its one dial accepted on a thread of
/// its own and its CONNECT read, then `then` given the stream. [`Fixture::end`] dials it once
/// more, which ends a fixture whose attempt never dialed, and joins it.
struct Fixture {
    dir: shards_testdir::TempDir,
    server: std::thread::JoinHandle<()>,
}

impl Fixture {
    fn new(name: &str, then: impl FnOnce(UnixStream) + Send + 'static) -> Fixture {
        let dir = shards_testdir::TempDir::new(name).unwrap();
        let listener = UnixListener::bind(dir.join("v")).unwrap();
        let server = std::thread::spawn(move || {
            let (mut peer, _) = listener.accept().unwrap();
            let mut asked = [0; "CONNECT 1234\n".len()];
            // A dial whose patience ran out before its CONNECT was whole, or the end's own.
            if peer.read_exact(&mut asked).is_ok() && asked == *b"CONNECT 1234\n" {
                then(peer);
            }
        });
        Fixture { dir, server }
    }

    fn path(&self) -> PathBuf {
        self.dir.join("v")
    }

    fn end(self) {
        drop(UnixStream::connect(self.path()));
        self.server.join().unwrap();
    }
}

/// Whether a borrowed connect's patience ran out before its CONNECT was written: no read of
/// its answer was reached.
fn sooner(result: &Result<Box<dyn Stream>, Refusal>) -> bool {
    matches!(
        result,
        Err(Refusal::Borrowed {
            failure: BorrowFailure::Channel(HandshakeError::Expired {
                phase: HandshakePhase::Dial | HandshakePhase::Write
            }),
            ..
        })
    )
}

#[test]
fn changed_identity_is_rejected_before_dial() {
    let dir = shards_testdir::TempDir::new("nb-id").unwrap();
    let listener = UnixListener::bind(dir.join("v")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut id = identity(dir.join("v"));
    let observed = id.started;
    id.started = id.started.wrapping_add(1);
    for patience in [Duration::ZERO, crate::while_runs::unreached()] {
        let result = connect_borrowed(&id, 1234, patience);
        assert!(matches!(result, Err(Refusal::Process { pid, expected_started,
            failure: crate::ProcessFailure::DifferentStart { observed: actual } })
            if pid == id.pid && expected_started == id.started && actual == observed));
    }
    assert_eq!(listener.accept().unwrap_err().kind(), io::ErrorKind::WouldBlock);
}

#[test]
fn exact_bytes_clone_and_half_close() {
    let dir = shards_testdir::TempDir::new("nb-bytes").unwrap();
    let listener = UnixListener::bind(dir.join("v")).unwrap();
    let server = std::thread::spawn(move || {
        let (mut peer, _) = listener.accept().unwrap();
        request(&mut peer, u32::MAX);
        peer.write_all(b"OK 4294967295\n\0\xff\n").unwrap();
        let mut bytes = [0; 3];
        peer.read_exact(&mut bytes).unwrap();
        assert_eq!(bytes, [0xff, 0, b'\n']);
        assert_eq!(peer.read(&mut [0; 1]).unwrap(), 0);
    });
    let mut stream =
        connect_borrowed(&identity(dir.join("v")), u32::MAX, crate::while_runs::unreached()).unwrap();
    let mut bytes = [0; 3];
    stream.read_exact(&mut bytes).unwrap();
    assert_eq!(bytes, [0, 0xff, b'\n']);
    let mut clone = stream.try_clone().unwrap();
    clone.write_all(&[0xff, 0, b'\n']).unwrap();
    clone.shutdown(std::net::Shutdown::Write).unwrap();
    drop(clone);
    drop(stream);
    server.join().unwrap();
}

/// A reply its fixture withholds until released: the connect expires at its patience, in its
/// read, before the release. Its patience is this host's own, one its dial and CONNECT fit in
/// (`while_runs::until_reached`).
#[test]
fn withheld_reply_expires_before_fixture_release() {
    use crate::while_runs::Reached;
    let (result, release, fixture) = crate::while_runs::until_reached(|patience| {
        let (release, hold) = mpsc::sync_channel::<()>(1);
        let fixture = Fixture::new("nb-held", move |_peer| {
            let _ = hold.recv();
        });
        let result = connect_borrowed(&identity(fixture.path()), 1234, patience);
        if sooner(&result) {
            drop(release);
            fixture.end();
            return Reached::Sooner;
        }
        Reached::Done((result, release, fixture))
    });
    release.send(()).unwrap();
    fixture.end();
    assert!(matches!(
        result,
        Err(Refusal::Borrowed {
            failure: BorrowFailure::Channel(HandshakeError::Expired {
                phase: HandshakePhase::Read
            }),
            ..
        })
    ));
}

/// A reply dribbled a byte at a time does not renew the deadline: one byte each quarter of
/// the patience (the reply's length less one), the whole reply past it, each byte well inside
/// a patience a renewing read would restart. Its patience is this host's own, one its dial and
/// CONNECT fit in (`while_runs::until_reached`).
#[test]
fn dribbled_reply_does_not_renew_deadline() {
    use crate::while_runs::Reached;
    const REPLY: &[u8] = b"OK 7\n";
    let (result, fixture) = crate::while_runs::until_reached(|patience| {
        let interval = patience / u32::try_from(REPLY.len() - 1).unwrap();
        let fixture = Fixture::new("nb-drip", move |mut peer| {
            for byte in REPLY {
                std::thread::sleep(interval);
                if peer.write_all(&[*byte]).is_err() {
                    break;
                }
            }
        });
        let result = connect_borrowed(&identity(fixture.path()), 1234, patience);
        if sooner(&result) {
            fixture.end();
            return Reached::Sooner;
        }
        Reached::Done((result, fixture))
    });
    fixture.end();
    assert!(matches!(
        result,
        Err(Refusal::Borrowed {
            failure: BorrowFailure::Channel(HandshakeError::Expired {
                phase: HandshakePhase::Read
            }),
            ..
        })
    ));
}

#[test]
fn malformed_and_closed_replies_remain_typed() {
    for reply in [
        b"NO\n".as_slice(),
        b"OK 4294967296\n",
        b"OK \xff\n",
        b"xxxxxxxxxxxxxx",
        b"",
    ] {
        let dir = shards_testdir::TempDir::new("nb-reply").unwrap();
        let listener = UnixListener::bind(dir.join("v")).unwrap();
        let server = std::thread::spawn(move || {
            let (mut peer, _) = listener.accept().unwrap();
            request(&mut peer, 1234);
            peer.write_all(reply).unwrap();
        });
        let result = connect_borrowed(&identity(dir.join("v")), 1234, crate::while_runs::unreached());
        server.join().unwrap();
        assert!(matches!(
            result,
            Err(Refusal::Borrowed {
                failure: BorrowFailure::Channel(HandshakeError::Reply(_) | HandshakeError::Closed),
                ..
            })
        ));
    }
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "coordinator-owned allocation-failure check; never a whole-library passing gate"]
fn shared_state_box_and_clone_oom_return_without_error_allocation() {
    let result = crate::proc_allocation::after(0, crate::stream::pair);
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::OutOfMemory);
    let (stream, _peer) = crate::stream::pair().unwrap();
    let result = crate::proc_allocation::after(0, || stream.boxed());
    assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::OutOfMemory));
    let (mut stream, mut peer) = crate::stream::pair().unwrap();
    let result = crate::proc_allocation::after(0, || stream.try_clone());
    assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::OutOfMemory));
    stream.write_all(b"ok").unwrap();
    let mut bytes = [0; 2];
    peer.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"ok");
}

#[test]
fn private_refcounts_are_bounded_by_retained_socket_storage() {
    assert_eq!(usize::BITS, 64);
    let bytes = std::mem::size_of::<crate::stream::Socket>();
    assert!(bytes >= 2 * std::mem::size_of::<usize>());
    assert!(usize::MAX / bytes + 1 < isize::MAX as usize);
}

#[test]
fn raw_ready_operations_do_not_enter_the_normal_wait_path() {
    let (mut stream, mut peer) = crate::stream::pair().unwrap();
    let patience = crate::while_runs::kept_wait();
    Stream::set_read_timeout(&stream, Some(patience)).unwrap();
    assert_eq!(
        stream.read_nowait(&mut [0; 1]).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    peer.write_all(b"\0\xff").unwrap();
    let mut bytes = [0; 2];
    assert_eq!(stream.read_nowait(&mut bytes).unwrap(), 2);
    assert_eq!(bytes, [0, 255]);
    assert_eq!(stream.write_nowait(b"x").unwrap(), 1);
    peer.read_exact(&mut [0; 1]).unwrap();
    Stream::set_read_timeout(&stream, None).unwrap();
    Stream::set_write_timeout(&stream, None).unwrap();
    peer.write_all(b"normal").unwrap();
    let mut normal = [0; 6];
    stream.read_exact(&mut normal).unwrap();
    assert_eq!(&normal, b"normal");
}
