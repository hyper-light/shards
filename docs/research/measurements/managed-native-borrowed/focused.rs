//! Focused synthetic-muxer checks. Actual VM/lifecycle/platform campaigns belong to root.
use super::*;
use std::sync::mpsc;

#[cfg(windows)]
use shards_afunix::UnixListener;
#[cfg(unix)]
use std::os::unix::net::UnixListener;

fn identity(path: PathBuf) -> Identity {
    let pid = std::process::id();
    #[cfg(unix)]
    let started = crate::taken::started(pid).unwrap();
    #[cfg(windows)]
    // SAFETY: current-process pseudo-handle stays valid and is never owned/closed here.
    let started =
        created_handle(unsafe { windows_sys::Win32::System::Threading::GetCurrentProcess() }).unwrap();
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

#[test]
fn zero_and_overflow_are_typed_before_process_or_dial_io() {
    let dir = shards_testdir::TempDir::new("nb-bound").unwrap();
    let path = dir.join("v");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let id = Identity {
        pid: 0,
        started: 0,
        vsock: path,
    };
    assert!(matches!(
        connect_borrowed(&id, 1234, Duration::ZERO),
        Err(Refusal::Borrowed {
            port: 1234,
            failure: BorrowFailure::Expired
        })
    ));
    assert!(matches!(
        connect_borrowed(&id, 1234, Duration::MAX),
        Err(Refusal::Borrowed {
            port: 1234,
            failure: BorrowFailure::DeadlineOverflow
        })
    ));
    let mut cost = Duration::ZERO;
    assert!(matches!(
        handshake(&id.vsock, 1234, Some(Duration::MAX), &mut cost),
        Err(Attempt::Refused(_))
    ));
    assert_eq!(
        crate::stream::connect(&id.vsock, Some(Duration::MAX))
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    #[cfg(windows)]
    assert_eq!(
        shards_afunix::UnixStream::connect_within(&id.vsock, Some(Duration::MAX))
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(listener.accept().unwrap_err().kind(), io::ErrorKind::WouldBlock);
}

#[test]
fn changed_identity_is_rejected_before_dial() {
    let dir = shards_testdir::TempDir::new("nb-id").unwrap();
    let listener = UnixListener::bind(dir.join("v")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut id = identity(dir.join("v"));
    let observed = id.started;
    id.started = id.started.wrapping_add(1);
    let result = connect_borrowed(&id, 1234, crate::conformance::UNSERVED_PATIENCE);
    assert!(matches!(result, Err(Refusal::Process { pid, expected_started,
        failure: crate::ProcessFailure::DifferentStart { observed: actual } })
        if pid == id.pid && expected_started == id.started && actual == observed));
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
    let mut stream = connect_borrowed(
        &identity(dir.join("v")),
        u32::MAX,
        crate::conformance::UNSERVED_PATIENCE,
    )
    .unwrap();
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

#[test]
fn withheld_reply_expires_before_fixture_release() {
    let dir = shards_testdir::TempDir::new("nb-held").unwrap();
    let listener = UnixListener::bind(dir.join("v")).unwrap();
    let id = identity(dir.join("v"));
    let (release, hold) = mpsc::sync_channel(1);
    let server = std::thread::spawn(move || {
        let (mut peer, _) = listener.accept().unwrap();
        request(&mut peer, 1234);
        hold.recv().unwrap();
    });
    let (sent, answer) = mpsc::sync_channel(1);
    let patience = crate::conformance::UNSERVED_PATIENCE;
    let worker = std::thread::spawn(move || sent.send(connect_borrowed(&id, 1234, patience)).unwrap());
    // Fixture-release watchdog only; the caller still receives exactly its original budget.
    let result = answer.recv_timeout(patience.saturating_mul(2));
    release.send(()).unwrap();
    server.join().unwrap();
    worker.join().unwrap();
    assert!(matches!(
        result.unwrap(),
        Err(Refusal::Borrowed {
            failure: BorrowFailure::Channel(HandshakeError::Expired {
                phase: HandshakePhase::Read
            }),
            ..
        })
    ));
}

#[test]
fn dribbled_reply_does_not_renew_deadline() {
    let dir = shards_testdir::TempDir::new("nb-drip").unwrap();
    let listener = UnixListener::bind(dir.join("v")).unwrap();
    let patience = crate::conformance::UNSERVED_PATIENCE;
    let server = std::thread::spawn(move || {
        let (mut peer, _) = listener.accept().unwrap();
        request(&mut peer, 1234);
        for byte in b"OK 7\n" {
            std::thread::sleep(patience / 2);
            if peer.write_all(&[*byte]).is_err() {
                break;
            }
        }
    });
    let result = connect_borrowed(&identity(dir.join("v")), 1234, patience);
    server.join().unwrap();
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
        let result = connect_borrowed(
            &identity(dir.join("v")),
            1234,
            crate::conformance::UNSERVED_PATIENCE,
        );
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
