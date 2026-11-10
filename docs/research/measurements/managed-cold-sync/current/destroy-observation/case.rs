//! One ordinary owned-API final-owner teardown observation; execution requires ROOT review.
use super::*;
use crate::sync::destroy_observation::{Event, Object, Watch};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError, channel};

fn left(deadline: Instant) -> Duration {
    let left = deadline.saturating_duration_since(Instant::now());
    assert!(!left.is_zero(), "original caller deadline expired");
    left
}

fn receive<T>(receiver: &Receiver<T>, deadline: Instant) -> T {
    receiver
        .recv_timeout(left(deadline))
        .expect("original caller deadline")
}

fn ready(fd: libc::c_int, events: libc::c_short, deadline: Instant) -> io::Result<()> {
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))?;
        let milliseconds = remaining
            .as_millis()
            .saturating_add(u128::from(remaining.subsec_nanos() % 1_000_000 != 0));
        let timeout = i32::try_from(milliseconds).unwrap_or(i32::MAX);
        let mut fd = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        // SAFETY: one live borrowed descriptor and writable pollfd. This is FD
        // readiness at the original deadline, not periodic guest-state probing.
        let result = unsafe { libc::poll(&mut fd, 1, timeout) };
        if result > 0 {
            if fd.revents & libc::POLLNVAL != 0 {
                return Err(io::Error::from_raw_os_error(libc::EBADF));
            }
            return Ok(());
        }
        if result == 0 {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn request(stream: &mut UnixStream, deadline: Instant) -> u32 {
    let mut line = Vec::new();
    loop {
        ready(stream.as_raw_fd(), libc::POLLIN, deadline).unwrap();
        let mut byte = [0];
        match stream.read(&mut byte) {
            Ok(0) => panic!("muxer received EOF before CONNECT"),
            Ok(_) => {
                line.push(byte[0]);
                assert!(line.len() <= REQUEST);
                if byte[0] == b'\n' {
                    break;
                }
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) => {}
            Err(e) => panic!("muxer read: {e}"),
        }
    }
    std::str::from_utf8(&line)
        .unwrap()
        .strip_prefix("CONNECT ")
        .unwrap()
        .trim_end()
        .parse()
        .unwrap()
}

fn answer(stream: &mut UnixStream, port: u32, deadline: Instant) {
    let reply = format!("OK {port}\n");
    let mut remaining = reply.as_bytes();
    while !remaining.is_empty() {
        ready(stream.as_raw_fd(), libc::POLLOUT, deadline).unwrap();
        match stream.write(remaining) {
            Ok(0) => panic!("muxer wrote no reply bytes"),
            Ok(n) => remaining = &remaining[n..],
            Err(e) if matches!(e.kind(), io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) => {}
            Err(e) => panic!("muxer write: {e}"),
        }
    }
}

#[test]
#[ignore = "requires ROOT-reviewed private stand-in, original deadline and outer exit supervisor"]
fn ordinary_final_owner_releases_native_storage() {
    let directory = PathBuf::from(std::env::var_os("SHARDS_COLD_CASE_DIR").unwrap());
    let milliseconds: u64 = std::env::var("SHARDS_COLD_CASE_MS").unwrap().parse().unwrap();
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(milliseconds))
        .unwrap();
    let mut owned = start(&GuestSpec {
        kernel: directory.join("unused-kernel"),
        init: Init::Executable(directory.join("unused-init")),
        cmdline: String::new(),
        vcpus: 1,
        memory_mib: crate::min_memory_mib(std::env::consts::ARCH),
        disks: Vec::new(),
        shares: Vec::new(),
        ports: vec![1024, 1025],
        binary: Some(directory.join("standin")),
    })
    .unwrap();
    let (observed_tx, observed_rx) = channel();
    owned
        .shared
        .state
        .observe_destroy(Watch::new(Object::State, observed_tx.clone()))
        .unwrap();
    owned
        .shared
        .changed
        .observe_destroy(Watch::new(Object::Changed, observed_tx.clone()))
        .unwrap();
    owned
        .shared
        .tail
        .observe_destroy(Watch::new(Object::Tail, observed_tx.clone()))
        .unwrap();
    owned
        .shared
        .drained
        .observe_destroy(Watch::new(Object::Drained, observed_tx.clone()))
        .unwrap();
    owned.shared.observed.attach(observed_tx.clone()).unwrap();
    drop(observed_tx);
    let workers = owned
        .test_workers
        .take()
        .expect("actual owned-child worker handles");
    let guest = Arc::new(owned);
    let listener = UnixListener::bind(&guest.vsock).unwrap();
    listener.set_nonblocking(true).unwrap();
    let marker = guest.vsock.parent().unwrap().join("cold-sync-owned");
    let mut ownership = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(marker)
        .unwrap();
    ownership
        .write_all(std::env::var("SHARDS_COLD_OWNER").unwrap().as_bytes())
        .unwrap();
    drop(ownership);
    let _ = writeln!(
        std::io::stdout(),
        "COLD_READY {} {}",
        guest.pid,
        guest.vsock.display()
    );
    std::io::stdout().flush().unwrap();
    let mut released = [0];
    std::io::stdin().read_exact(&mut released).unwrap();
    assert_eq!(released, [b'R']);
    left(deadline);

    let (held_tx, held_rx) = channel();
    let (release_tx, release_rx) = channel();
    let muxer = std::thread::Builder::new()
        .name("owned-ordinary-muxer".into())
        .spawn(move || {
            let mut connections = Vec::new();
            while connections.len() < 2 {
                ready(listener.as_raw_fd(), libc::POLLIN, deadline).unwrap();
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(true).unwrap();
                        let port = request(&mut stream, deadline);
                        connections.push((port, stream));
                    }
                    Err(e) if matches!(e.kind(), io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) => {}
                    Err(e) => panic!("muxer accept: {e}"),
                }
            }
            let mut ports: Vec<_> = connections.iter().map(|(port, _)| *port).collect();
            ports.sort_unstable();
            assert_eq!(ports, [1024, 1025]);
            let _ = writeln!(std::io::stdout(), "CONCURRENT_MUXER_HELD ports={ports:?}");
            held_tx.send(ports).unwrap();
            receive(&release_rx, deadline);
            for (port, stream) in &mut connections {
                answer(stream, *port, deadline);
            }
            drop(connections);
            listener
        })
        .unwrap();

    let (entered_tx, entered_rx) = channel();
    let (wait_tx, wait_rx) = channel();
    let mut waiters = Vec::new();
    for id in 0..2 {
        let (guest, entered, completed) = (guest.clone(), entered_tx.clone(), wait_tx.clone());
        waiters.push(
            std::thread::Builder::new()
                .name(format!("owned-ordinary-wait-{id}"))
                .spawn(move || {
                    // This announces entry, not proof that a native CV park occurred.
                    entered.send(id).unwrap();
                    let result = guest.wait_for(left(deadline));
                    completed.send((id, result)).unwrap();
                })
                .unwrap(),
        );
    }
    drop(entered_tx);
    drop(wait_tx);
    let mut entered = [receive(&entered_rx, deadline), receive(&entered_rx, deadline)];
    entered.sort_unstable();
    assert_eq!(entered, [0, 1]);

    let (connect_tx, connect_rx) = channel();
    let mut connectors = Vec::new();
    for (port, patience) in [(1024, Duration::ZERO), (1025, Duration::MAX)] {
        let (guest, completed) = (guest.clone(), connect_tx.clone());
        connectors.push(
            std::thread::Builder::new()
                .name(format!("owned-ordinary-connect-{port}"))
                .spawn(move || {
                    let result = guest.connect(port, patience).map(drop);
                    completed.send((port, result)).unwrap();
                })
                .unwrap(),
        );
    }
    drop(connect_tx);
    assert_eq!(receive(&held_rx, deadline), [1024, 1025]);
    assert!(matches!(wait_rx.try_recv(), Err(TryRecvError::Empty)));
    assert!(matches!(connect_rx.try_recv(), Err(TryRecvError::Empty)));
    release_tx.send(()).unwrap();
    for _ in 0..2 {
        let (port, result) = receive(&connect_rx, deadline);
        let _ = writeln!(
            std::io::stdout(),
            "CONCURRENT_CONNECTED port={port} result={result:?}"
        );
        result.unwrap();
    }

    let (stop_tx, stop_rx) = channel();
    let stopped = guest.clone();
    let stopper = std::thread::Builder::new()
        .name("owned-ordinary-stop".into())
        .spawn(move || {
            stop_tx.send(stopped.stop()).unwrap();
        })
        .unwrap();
    receive(&stop_rx, deadline).unwrap();
    let _ = writeln!(std::io::stdout(), "CONCURRENT_STOP_RETURNED");
    for _ in 0..2 {
        let (id, result) = receive(&wait_rx, deadline);
        let _ = writeln!(
            std::io::stdout(),
            "CONCURRENT_WAIT_RETURNED id={id} result={result:?}"
        );
        assert_eq!(result.unwrap(), Some(End::Stopped));
    }
    for handle in connectors {
        handle.join().unwrap();
        left(deadline);
    }
    for handle in waiters {
        handle.join().unwrap();
        left(deadline);
    }
    stopper.join().unwrap();
    left(deadline);
    let listener = muxer.join().unwrap();
    left(deadline);
    // Both actual callers joined: one nonblocking backlog observation detects
    // an additional queued dial without a timing sleep or periodic state probe.
    assert!(matches!(listener.accept(), Err(e) if e.kind() == io::ErrorKind::WouldBlock));
    drop(listener);
    let mut status = 0;
    // SAFETY: the test is this child's actual parent. ECHILD distinguishes API
    // reaping from an exit notification or a cached terminal label alone.
    let waited = unsafe { libc::waitpid(guest.pid as libc::pid_t, &mut status, libc::WNOHANG) };
    assert_eq!(
        (waited, io::Error::last_os_error().raw_os_error()),
        (-1, Some(libc::ECHILD))
    );
    left(deadline);
    let _ = writeln!(std::io::stdout(), "COLD_NATIVE_REAP_ECHILD");
    let (drainer, watcher) = workers;
    drainer.join().unwrap();
    left(deadline);
    watcher.join().unwrap();
    left(deadline);
    let _ = writeln!(
        std::io::stdout(),
        "TEARDOWN_JOINED connectors=2 waiters=2 stopper=1 muxer=1 api_drain=1 api_watch=1"
    );
    assert!(matches!(observed_rx.try_recv(), Err(TryRecvError::Empty)));
    let owned =
        Arc::try_unwrap(guest).unwrap_or_else(|_| panic!("a public Guest owner remains after caller joins"));
    drop(owned);
    left(deadline);

    // Exact existing field order, not an inferred quiescence check. In particular,
    // PointeeDropReturned is emitted after Box Drop; native return zero is separate.
    let expected = [
        Event::SharedDropEntered,
        Event::DestroyEntered(Object::State),
        Event::DestroyReturned {
            object: Object::State,
            code: 0,
        },
        Event::PointeeDropReturned(Object::State),
        Event::DestroyEntered(Object::Changed),
        Event::DestroyReturned {
            object: Object::Changed,
            code: 0,
        },
        Event::PointeeDropReturned(Object::Changed),
        Event::DestroyEntered(Object::Tail),
        Event::DestroyReturned {
            object: Object::Tail,
            code: 0,
        },
        Event::PointeeDropReturned(Object::Tail),
        Event::DestroyEntered(Object::Drained),
        Event::DestroyReturned {
            object: Object::Drained,
            code: 0,
        },
        Event::PointeeDropReturned(Object::Drained),
        Event::OriginalSharedFieldsDropped,
    ];
    let mut observed = Vec::new();
    loop {
        match observed_rx.recv_timeout(left(deadline)) {
            Ok(event) => {
                let _ = writeln!(std::io::stdout(), "TEARDOWN_EVENT {event:?}");
                observed.push(event);
                assert!(observed.len() <= expected.len(), "unexpected extra event");
            }
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => panic!("original teardown deadline expired"),
        }
    }
    assert_eq!(observed, expected);
    let _ = writeln!(
        std::io::stdout(),
        "TEARDOWN_POINTEES_DROPPED_ORIGINAL_FIELDS_DROPPED_OBSERVERS_CLOSED"
    );
}
