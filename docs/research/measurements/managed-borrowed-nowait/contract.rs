//! The same two feature-contract examples compile against before and after source.
use super::*;

#[cfg(unix)]
#[test]
fn zero_patience_attempts_one_request_and_does_not_wait_for_accept_or_reply() {
    let dir = shards_testdir::TempDir::new("nb-zero").unwrap();
    let path = dir.join("v");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    // The peer cannot answer: no accept or server thread runs until the call returns.
    let result = connect_borrowed(&identity(path), 1234, Duration::ZERO);
    assert!(
        matches!(
            result,
            Err(Refusal::Borrowed {
                port: 1234,
                failure: BorrowFailure::Channel(HandshakeError::Io {
                    phase: HandshakePhase::Read,
                    kind: io::ErrorKind::WouldBlock,
                    ..
                })
            })
        ),
        "one immediate request must reach the unanswered Read phase: {:?}",
        result.err()
    );
    let (mut peer, _) = listener.accept().unwrap();
    peer.set_nonblocking(true).unwrap();
    let expected = b"CONNECT 1234\n";
    let mut bytes = [0; b"CONNECT 1234\n".len()];
    let mut used = 0;
    while used < expected.len() {
        let n = peer.read(&mut bytes[used..]).unwrap();
        assert_ne!(n, 0, "complete already-committed request before EOF");
        used += n;
    }
    assert_eq!(&bytes, expected);
    assert_eq!(
        peer.read(&mut [0; 1]).unwrap(),
        0,
        "failed attempt closes its socket"
    );
    assert_eq!(listener.accept().unwrap_err().kind(), io::ErrorKind::WouldBlock);
}

#[test]
fn overflow_is_bounds_before_process_or_dial_io() {
    let dir = shards_testdir::TempDir::new("nb-bound").unwrap();
    let path = dir.join("v");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let id = Identity {
        pid: 0,
        started: 0,
        vsock: path,
    };
    let result = connect_borrowed(&id, 1234, Duration::MAX);
    assert!(
        matches!(result, Err(Refusal::Bounds { what: "patience", .. })),
        "{:?}",
        result.err()
    );
    assert_eq!(listener.accept().unwrap_err().kind(), io::ErrorKind::WouldBlock);
}
