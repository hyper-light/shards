//! Focused real AF_UNIX fake-muxer deadline reproductions, before production edits.
use super::*;
use std::sync::mpsc;

#[path = "old_patch_handshake.rs"]
mod reference;

#[cfg(windows)]
use shards_afunix::UnixListener;
#[cfg(unix)]
use std::os::unix::net::UnixListener;

fn request(stream: &mut impl io::Read) {
    let mut request = [0; 13];
    stream.read_exact(&mut request).unwrap();
    assert_eq!(&request, b"CONNECT 1234\n");
}

fn rejected_without_io(patience: Duration) {
    let dir = shards_testdir::TempDir::new("borrow-red").unwrap();
    let path = dir.join("v");
    let listener = UnixListener::bind(&path).unwrap();
    let (connected, connection) = mpsc::sync_channel(1);
    let (release, hold) = mpsc::sync_channel(1);
    let (sent, answer) = mpsc::sync_channel(1);
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        request(&mut stream);
        connected.send(()).unwrap();
        hold.recv().unwrap();
    });
    let worker = std::thread::spawn(move || {
        // Exact borrowed patch mapping: zero -> None; overflow reaches checked_add(None).
        let wait = if patience.is_zero() { None } else { Some(patience) };
        let mut cost = Duration::ZERO;
        let result = reference::handshake(&path, 1234, wait, &mut cost);
        sent.send(result.is_err()).unwrap();
    });
    connection
        .recv_timeout(crate::conformance::UNSERVED_PATIENCE)
        .unwrap();
    let blocked = matches!(
        answer.recv_timeout(crate::conformance::UNSERVED_PATIENCE),
        Err(mpsc::RecvTimeoutError::Timeout)
    );
    release.send(()).unwrap();
    server.join().unwrap();
    worker.join().unwrap();
    println!("old_patch patience={patience:?} actual_CONNECT_observed=true withheld_still_blocked={blocked}");
    assert!(
        !blocked,
        "borrowed zero/overflow dialed and waited for withheld muxer reply"
    );
}

#[test]
#[ignore = "historical intentional red; run borrowed_repro with --ignored explicitly"]
fn old_patch_zero_is_expired_before_io() {
    rejected_without_io(Duration::ZERO);
}
#[test]
#[ignore = "historical intentional red; run borrowed_repro with --ignored explicitly"]
fn old_patch_overflow_is_refused_before_io() {
    rejected_without_io(Duration::MAX);
}

#[test]
#[ignore = "historical intentional red; run borrowed_repro with --ignored explicitly"]
fn a_current_handshake_dribble_cannot_renew_the_total_budget() {
    let dir = shards_testdir::TempDir::new("borrow-drip").unwrap();
    let path = dir.join("v");
    let listener = UnixListener::bind(&path).unwrap();
    let patience = crate::conformance::UNSERVED_PATIENCE;
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        request(&mut stream);
        // A deliberate input schedule, not a periodic state/readiness probe.
        for byte in b"OK 7\n" {
            std::thread::sleep(patience / 2);
            if stream.write_all(&[*byte]).is_err() {
                break;
            }
        }
    });
    let began = Instant::now();
    let mut cost = Duration::ZERO;
    let result = handshake(&path, 1234, Some(patience), &mut cost);
    let elapsed = began.elapsed();
    server.join().unwrap();
    println!(
        "current_handshake patience={patience:?} elapsed={elapsed:?} accepted_late_reply={}",
        result.is_ok()
    );
    assert!(
        result.is_err(),
        "per-read waits accepted a reply beyond the total finite budget"
    );
}

#[test]
fn legacy_refusal_bridge_oom_returns_typed_without_allocating() {
    let dir = shards_testdir::TempDir::new("borrow-bridge").unwrap();
    let path = dir.join("missing");
    let mut cost = Duration::ZERO;
    let result = crate::allocation::fail_after(0, || {
        handshake(
            &path,
            1234,
            Some(crate::conformance::UNSERVED_PATIENCE),
            &mut cost,
        )
    });
    assert!(matches!(result, Err(Attempt::Allocation)));
}
