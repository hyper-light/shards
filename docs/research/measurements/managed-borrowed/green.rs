//! Focused live synthetic muxer and typed-allocation checks; no VM/provider.
use super::*;
use std::io::{Read as _, Write as _};
use std::sync::mpsc;

#[cfg(windows)]
use shards_afunix::UnixListener;
#[cfg(unix)]
use std::os::unix::net::UnixListener;

fn identity(path: std::path::PathBuf) -> Identity {
    let pid = std::process::id();
    #[cfg(unix)]
    let started = crate::taken::started(pid).unwrap();
    #[cfg(windows)]
    // SAFETY: the current-process pseudo-handle remains valid; never owned/closed here.
    let started =
        crate::client::created_handle(unsafe { windows_sys::Win32::System::Threading::GetCurrentProcess() })
            .unwrap();
    Identity {
        pid,
        started,
        vsock: path,
    }
}

fn request(stream: &mut impl io::Read, port: u32) {
    let mut request = vec![0; format!("CONNECT {port}\n").len()];
    stream.read_exact(&mut request).unwrap();
    assert_eq!(request, format!("CONNECT {port}\n").as_bytes());
}

#[cfg(target_os = "linux")]
fn resources() -> (usize, usize) {
    (
        std::fs::read_dir("/proc/self/fd").unwrap().count(),
        std::fs::read_dir("/proc/self/task").unwrap().count(),
    )
}

#[test]
fn zero_and_overflow_are_typed_before_any_io() {
    let dir = shards_testdir::TempDir::new("borrow-bounds").unwrap();
    let path = dir.join("v");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    // An invalid identity proves even process validation did not precede the guards.
    let id = Identity {
        pid: 0,
        started: 0,
        vsock: path,
    };
    assert!(matches!(
        connect_borrowed(&id, 1234, Duration::ZERO),
        Err(BorrowError::Expired)
    ));
    assert!(matches!(
        connect_borrowed(&id, 1234, Duration::MAX),
        Err(BorrowError::DeadlineOverflow)
    ));
    assert_eq!(listener.accept().unwrap_err().kind(), io::ErrorKind::WouldBlock);
}

fn bounds_without_dial(patience: Duration) {
    let dir = shards_testdir::TempDir::new("borrow-boundio").unwrap();
    let listener = UnixListener::bind(dir.join("v")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let id = identity(dir.join("v"));
    let (send, receive) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || send.send(connect_borrowed(&id, 1234, patience)).unwrap());
    // Fixture release budget only. If the guard is removed, observe the actual
    // CONNECT independently, close that withheld peer, and join without an orphan.
    let first = receive.recv_timeout(crate::conformance::UNSERVED_PATIENCE);
    let actual_dial = match listener.accept() {
        Ok((mut stream, _)) => {
            request(&mut stream, 1234);
            true
        }
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => false,
        Err(e) => panic!("{e}"),
    };
    let result = match first {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => receive.recv().unwrap(),
        Err(e) => panic!("{e}"),
    };
    worker.join().unwrap();
    println!("patience={patience:?} actual_CONNECT={actual_dial}");
    assert!(!actual_dial);
    assert!(match result {
        Err(BorrowError::Expired) => patience.is_zero(),
        Err(BorrowError::DeadlineOverflow) => patience == Duration::MAX,
        _ => false,
    });
}

#[test]
fn zero_with_withheld_muxer_never_dials() {
    bounds_without_dial(Duration::ZERO);
}

#[test]
fn overflow_with_withheld_muxer_never_dials() {
    bounds_without_dial(Duration::MAX);
}

#[test]
fn changed_identity_is_rejected_before_dial() {
    let dir = shards_testdir::TempDir::new("borrow-id").unwrap();
    let listener = UnixListener::bind(dir.join("v")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut id = identity(dir.join("v"));
    id.started = id.started.wrapping_add(1);
    let result = connect_borrowed(&id, 1234, crate::conformance::UNSERVED_PATIENCE);
    let dial = listener.accept();
    if let Ok((stream, _)) = dial.as_ref() {
        let mut stream = stream;
        request(&mut stream, 1234);
    }
    println!("stale_identity_actual_CONNECT={}", dial.is_ok());
    assert!(matches!(
        result,
        Err(BorrowError::Process {
            cause: ProcessError::Changed { .. },
            ..
        })
    ));
    assert_eq!(dial.unwrap_err().kind(), io::ErrorKind::WouldBlock);
}

#[test]
fn exact_bytes_clone_half_close_and_resource_return() {
    #[cfg(target_os = "linux")]
    let before = resources();
    let dir = shards_testdir::TempDir::new("borrow-bytes").unwrap();
    let listener = UnixListener::bind(dir.join("v")).unwrap();
    let server = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        request(&mut s, u32::MAX);
        s.write_all(b"OK 4294967295\n\0\xff\n").unwrap();
        let mut back = [0; 3];
        s.read_exact(&mut back).unwrap();
        assert_eq!(back, [0xff, 0, b'\n']);
        assert_eq!(s.read(&mut [0; 1]).unwrap(), 0);
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
    drop(dir);
    #[cfg(target_os = "linux")]
    {
        let after = resources();
        println!("borrowed_resources_before={before:?} after={after:?}");
        assert_eq!(after, before);
    }
}

#[test]
fn withheld_reply_expires_without_fixture_release() {
    let dir = shards_testdir::TempDir::new("borrow-held").unwrap();
    let listener = UnixListener::bind(dir.join("v")).unwrap();
    let id = identity(dir.join("v"));
    let (release, hold) = mpsc::sync_channel(1);
    let server = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        request(&mut s, 1234);
        hold.recv().unwrap();
    });
    let (sent, answer) = mpsc::sync_channel(1);
    let patience = crate::conformance::UNSERVED_PATIENCE;
    let worker = std::thread::spawn(move || sent.send(connect_borrowed(&id, 1234, patience)).unwrap());
    let result = answer.recv_timeout(patience.saturating_mul(2));
    release.send(()).unwrap();
    server.join().unwrap();
    worker.join().unwrap();
    assert!(matches!(
        result.unwrap(),
        Err(BorrowError::Channel {
            cause: HandshakeError::Expired {
                phase: crate::HandshakePhase::Read
            },
            ..
        })
    ));
}

#[test]
fn dribbled_reply_does_not_renew_deadline() {
    let dir = shards_testdir::TempDir::new("borrow-drip").unwrap();
    let listener = UnixListener::bind(dir.join("v")).unwrap();
    let patience = crate::conformance::UNSERVED_PATIENCE;
    let server = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        request(&mut s, 1234);
        for byte in b"OK 7\n" {
            std::thread::sleep(patience / 2);
            if s.write_all(&[*byte]).is_err() {
                break;
            }
        }
    });
    let result = connect_borrowed(&identity(dir.join("v")), 1234, patience);
    server.join().unwrap();
    assert!(matches!(
        result,
        Err(BorrowError::Channel {
            cause: HandshakeError::Expired {
                phase: crate::HandshakePhase::Read
            },
            ..
        })
    ));
}

#[test]
fn malformed_or_closed_replies_remain_typed() {
    for reply in [
        b"NO\n".as_slice(),
        b"OK 4294967296\n",
        b"OK \xff\n",
        b"xxxxxxxxxxxxxx",
        b"",
    ] {
        let dir = shards_testdir::TempDir::new("borrow-reply").unwrap();
        let listener = UnixListener::bind(dir.join("v")).unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            request(&mut s, 1234);
            s.write_all(reply).unwrap();
        });
        let result = connect_borrowed(
            &identity(dir.join("v")),
            1234,
            crate::conformance::UNSERVED_PATIENCE,
        );
        server.join().unwrap();
        assert!(matches!(
            result,
            Err(BorrowError::Channel {
                cause: HandshakeError::Reply(_) | HandshakeError::Closed,
                ..
            })
        ));
    }
}

#[test]
fn shared_state_and_stream_box_oom_return_without_allocating_errors() {
    #[cfg(unix)]
    {
        // Existing connect constructor reaches this same shared-state allocation.
        let result = crate::allocation::fail_after(0, crate::stream::pair);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::OutOfMemory);
    }
    let (stream, _peer) = crate::stream::pair().unwrap();
    let result = crate::allocation::fail_after(0, || stream.boxed());
    assert!(matches!(result, Err(e) if e.kind() == io::ErrorKind::OutOfMemory));
    let (mut stream, mut peer) = crate::stream::pair().unwrap();
    let result = crate::allocation::fail_after(0, || stream.try_clone());
    assert!(matches!(result, Err(e) if e.kind() == io::ErrorKind::OutOfMemory));
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
    // Every clone has its distinct box BEFORE increment; the original may be unboxed.
    assert!(usize::MAX / bytes + 1 < isize::MAX as usize);
}

struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn child() -> (Child, u64) {
    let child = Child(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "borrowed::tests::process_role",
                "--ignored",
                "--nocapture",
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    );
    #[cfg(unix)]
    let started = crate::taken::started(child.0.id()).unwrap();
    #[cfg(windows)]
    let started = {
        use std::os::windows::io::AsRawHandle as _;
        crate::client::created_handle(child.0.as_raw_handle()).unwrap()
    };
    (child, started)
}

#[test]
#[ignore = "private child role, launched only by focused borrowed tests"]
fn process_role() {
    let mut input = std::io::stdin().lock();
    let mut byte = [0; 1];
    while input.read(&mut byte).unwrap() != 0 {
        std::io::stdout().write_all(b"BORROWED_ALIVE\n").unwrap();
        std::io::stdout().flush().unwrap();
    }
}

#[test]
fn dropping_borrowed_stream_leaves_process_alive_and_responsive() {
    use std::io::BufRead as _;
    let (mut child, started) = child();
    let dir = shards_testdir::TempDir::new("borrow-life").unwrap();
    let listener = UnixListener::bind(dir.join("v")).unwrap();
    let server = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        request(&mut s, 1234);
        s.write_all(b"OK 7\n").unwrap();
    });
    let id = Identity {
        pid: child.0.id(),
        started,
        vsock: dir.join("v"),
    };
    drop(connect_borrowed(&id, 1234, crate::conformance::UNSERVED_PATIENCE).unwrap());
    server.join().unwrap();
    assert!(child.0.try_wait().unwrap().is_none());
    child.0.stdin.as_mut().unwrap().write_all(b"a").unwrap();
    let mut output = std::io::BufReader::new(child.0.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        assert_ne!(output.read_line(&mut line).unwrap(), 0);
        if line.ends_with("BORROWED_ALIVE\n") {
            break;
        }
        line.clear();
    }
    println!("borrowed_drop_child_alive_and_actual_reply=true");
}

#[test]
fn process_exit_before_or_during_handshake_is_rejected() {
    let (mut dead, started) = child();
    let dir = shards_testdir::TempDir::new("borrow-exit").unwrap();
    let dead_id = Identity {
        pid: dead.0.id(),
        started,
        vsock: dir.join("missing"),
    };
    dead.0.kill().unwrap();
    dead.0.wait().unwrap();
    assert!(matches!(
        connect_borrowed(&dead_id, 1234, crate::conformance::UNSERVED_PATIENCE),
        Err(BorrowError::Process { .. })
    ));

    let (mut child, started) = child();
    let id = Identity {
        pid: child.0.id(),
        started,
        vsock: dir.join("v"),
    };
    let listener = UnixListener::bind(&id.vsock).unwrap();
    let server = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        request(&mut s, 1234);
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        // Reply only after actual process death/reaping, so post-validation matters.
        s.write_all(b"OK 7\n").unwrap();
    });
    let result = connect_borrowed(&id, 1234, crate::conformance::UNSERVED_PATIENCE);
    server.join().unwrap();
    println!(
        "actual_process_reaped_before_OK=true returned_stream={}",
        result.is_ok()
    );
    assert!(matches!(
        result,
        Err(BorrowError::Process {
            cause: ProcessError::Ended,
            ..
        })
    ));
}
