//! A borrowed connect with no clock (`connect_borrowed_before(.., None)`; Hex's audit fix
//! D-3, its decision 9: no verdict at a bound). An `OK` however late is taken; the end of the
//! identity's VM process, while the connection stays open, is refused typed; a pid that names
//! another process is never followed. Real-VM runs belong to ROOT.
use super::*;

/// A process for an identity to name, alive until it is killed: this test binary, running
/// [`holds_until_killed`], which waits on its stdin, held open here.
fn holder() -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "client::borrowed_tests::no_clock::holds_until_killed",
            "--ignored",
            "--test-threads=1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

#[test]
#[ignore = "the process `holder` starts, never a test of its own"]
fn holds_until_killed() {
    let _ = std::io::stdin().read(&mut [0; 1]);
}

fn identity_of(child: &Child, path: PathBuf) -> Identity {
    let pid = child.id();
    Identity {
        pid,
        started: crate::taken::started(pid).unwrap(),
        vsock: path,
    }
}

/// The same withheld `OK`: a connect with a patience closes it at that patience, in its read;
/// one with none takes it, sent past that patience, and the bytes after it are the stream's.
/// The patience is this host's own, one its dial and CONNECT fit in
/// (`while_runs::until_reached`).
#[test]
fn an_ok_past_a_patience_is_closed_with_one_and_taken_with_none() {
    use crate::while_runs::Reached;
    let patience = crate::while_runs::until_reached(|patience| {
        let (release, hold) = mpsc::sync_channel::<()>(1);
        let fixture = Fixture::new("nc-shut", move |_peer| {
            let _ = hold.recv();
        });
        let deadline = Instant::now().checked_add(patience);
        let result = connect_borrowed_before(&identity(fixture.path()), 1234, deadline);
        drop(release);
        fixture.end();
        if sooner(&result) {
            return Reached::Sooner;
        }
        assert!(
            matches!(
                result,
                Err(Refusal::Borrowed {
                    failure: BorrowFailure::Channel(HandshakeError::Expired {
                        phase: HandshakePhase::Read
                    }),
                    ..
                })
            ),
            "{:?}",
            result.as_ref().err()
        );
        Reached::Done(patience)
    });
    // Asked: the fixture has read the CONNECT, or the connect has answered without one.
    let (asked, was_asked) = mpsc::channel();
    let answered_first = asked.clone();
    let (release, hold) = mpsc::channel::<()>();
    let fixture = Fixture::new("nc-late", move |mut peer| {
        asked.send(()).unwrap();
        hold.recv().unwrap();
        peer.write_all(b"OK 1234\nafter").unwrap();
        // Held until the client has read and closed its end.
        let _ = peer.read(&mut [0; 1]);
    });
    let id = identity(fixture.path());
    let (_worker, answered) = crate::while_runs::on_a_thread(move || {
        let result = connect_borrowed_before(&id, 1234, None).map(|mut stream| {
            let mut after = [0; 5];
            stream.read_exact(&mut after).unwrap();
            after
        });
        let _ = answered_first.send(());
        result
    });
    was_asked.recv().unwrap();
    // Past the patience a connect with one closed this answer at.
    std::thread::sleep(patience.saturating_mul(2));
    release.send(()).unwrap();
    let result = answered.recv().unwrap();
    fixture.end();
    assert_eq!(result, Ok(*b"after"), "{:?}", result.as_ref().err());
}

/// The identity's VM process ends as the connect waits for its `OK`, its peer holding the
/// connection open with no answer: the connect ends at that end, refused `Exited`, typed. Its
/// answer is waited for at the scale of a process's end on this host (its kill and reap,
/// timed), failing where the connect is found blocked past it.
#[test]
fn the_vm_processs_end_mid_wait_is_refused_exited() {
    let mut child = holder();
    let (asked, was_asked) = mpsc::channel();
    let answered_first = asked.clone();
    let (release, hold) = mpsc::channel::<()>();
    let fixture = Fixture::new("nc-end", move |_peer| {
        asked.send(()).unwrap();
        let _ = hold.recv();
    });
    let id = identity_of(&child, fixture.path());
    let (pid, started) = (id.pid, id.started);
    let (worker, answered) = crate::while_runs::on_a_thread(move || {
        let result = connect_borrowed_before(&id, 1234, None).map(drop);
        let _ = answered_first.send(());
        result
    });
    was_asked.recv().unwrap();
    let began = Instant::now();
    child.kill().unwrap();
    child.wait().unwrap();
    let scale = began.elapsed().max(crate::while_runs::kept_wait());
    let result = crate::while_runs::answer(&answered, worker, scale);
    drop(release);
    fixture.end();
    assert!(
        matches!(
            result,
            Err(Refusal::Process {
                pid: p,
                expected_started: s,
                failure: crate::ProcessFailure::Exited,
            }) if p == pid && s == started
        ),
        "{:?}",
        result.err()
    );
}

/// A pid that names another process (its start not the identity's) is refused
/// `DifferentStart` with no clock, before any dial; so is one whose process has ended and been
/// reaped (whatever its pid names since): neither is followed. The first is asked on a thread
/// of its own and fails the test where it is found blocked, as one that followed would be.
#[test]
fn a_pid_that_names_another_process_is_never_followed() {
    let dir = shards_testdir::TempDir::new("nc-pid").unwrap();
    let listener = UnixListener::bind(dir.join("v")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut child = holder();
    let id = identity_of(&child, dir.join("v"));
    let other = Identity {
        started: id.started.wrapping_add(1),
        ..id.clone()
    };
    // On a thread of its own, answered at once: a connect that followed it would wait.
    let (worker, answered) =
        crate::while_runs::on_a_thread(move || connect_borrowed_before(&other, 1234, None).map(drop));
    let result = crate::while_runs::answer(&answered, worker, crate::while_runs::kept_wait());
    assert!(
        matches!(
            result,
            Err(Refusal::Process {
                failure: crate::ProcessFailure::DifferentStart { observed },
                ..
            }) if observed == id.started
        ),
        "{:?}",
        result.as_ref().err()
    );
    child.kill().unwrap();
    child.wait().unwrap();
    let result = connect_borrowed_before(&id, 1234, None);
    assert!(
        matches!(result, Err(Refusal::Process { .. })),
        "{:?}",
        result.as_ref().err()
    );
    assert_eq!(listener.accept().unwrap_err().kind(), io::ErrorKind::WouldBlock);
}
