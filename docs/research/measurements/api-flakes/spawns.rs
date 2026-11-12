//! PM M535's measurement: the zero-patience example's check, a refused socket's end at its
//! peer, again and again beside threads that start processes, as the library's other
//! tests do. `SHARDS_API_SPAWNS` names what they start: `none`; `sleep`, `/bin/sleep 0.2`,
//! a program the host has run before; or `fresh`, a script of its own each time, sleeping
//! 0.2 s, which macOS assesses at its first exec (PM M534). Prints how many refused sockets
//! their peers still found open, and how long each one's end then took.
use super::*;
use std::os::unix::net::UnixListener;
use std::sync::atomic::AtomicBool;

/// Starts `kind`'s processes until `stop`, keeping eight of its own alive at once (four
/// threads of it, 32 children: as many tests as libtest runs beside each other on a 32-CPU
/// host); how many it started.
fn starting(kind: &str, stop: &AtomicBool) -> usize {
    let dir = shards_testdir::TempDir::new("sw-fresh").unwrap();
    let mut alive = std::collections::VecDeque::new();
    let mut started = 0;
    while !stop.load(Ordering::Relaxed) {
        if alive.len() == 8 {
            let mut oldest: std::process::Child = alive.pop_front().unwrap();
            let _ = oldest.wait();
        }
        let mut command = match kind {
            "sleep" => {
                let mut sleep = Command::new("/bin/sleep");
                sleep.arg("0.2");
                sleep
            }
            _ => {
                let script = dir.join(format!("s{started}"));
                let source = dir.join(format!("s{started}.sh"));
                std::fs::write(&source, "#!/bin/sh\nexec sleep 0.2\n").unwrap();
                let made = Command::new("install")
                    .args(["-m", "755"])
                    .arg(&source)
                    .arg(&script)
                    .status();
                assert!(made.unwrap().success());
                Command::new(script)
            }
        };
        if let Ok(child) = command.stdin(Stdio::null()).spawn() {
            alive.push_back(child);
            started += 1;
        }
    }
    for mut child in alive {
        let _ = child.wait();
    }
    started
}

#[test]
#[ignore = "a measurement, run by hand (PM M535)"]
fn a_refused_sockets_end_beside_spawns() {
    let rounds: usize = std::env::var("SHARDS_API_SPAWN_ROUNDS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(5000);
    let kind = std::env::var("SHARDS_API_SPAWNS").unwrap_or_else(|_| "sleep".into());
    let stop = AtomicBool::new(false);
    let (late, started) = std::thread::scope(|scope| {
        let spawners: Vec<_> = (0..4)
            .filter(|_| kind != "none")
            .map(|_| scope.spawn(|| starting(&kind, &stop)))
            .collect();
        let mut late = Vec::new();
        for _ in 0..rounds {
            let dir = shards_testdir::TempDir::new("sw").unwrap();
            let path = dir.join("v");
            let listener = UnixListener::bind(&path).unwrap();
            listener.set_nonblocking(true).unwrap();
            let pid = std::process::id();
            let identity = Identity {
                pid,
                started: crate::taken::started(pid).unwrap(),
                vsock: path,
            };
            let refused = connect_borrowed(&identity, 1234, Duration::ZERO);
            assert!(
                matches!(refused, Err(Refusal::Borrowed { .. })),
                "{:?}",
                refused.err()
            );
            let (mut peer, _) = listener.accept().unwrap();
            peer.set_nonblocking(true).unwrap();
            let mut request = [0; b"CONNECT 1234\n".len()];
            let mut used = 0;
            while used < request.len() {
                used += peer.read(&mut request[used..]).unwrap();
            }
            let open = Instant::now();
            match peer.read(&mut [0; 1]) {
                Ok(0) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    let mut fd = libc::pollfd {
                        fd: std::os::fd::AsRawFd::as_raw_fd(&peer),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    // SAFETY: poll(2) of one pollfd, of a socket this test holds open.
                    assert_eq!(unsafe { libc::poll(&raw mut fd, 1, -1) }, 1);
                    assert_eq!(peer.read(&mut [0; 1]).unwrap(), 0);
                    late.push(open.elapsed());
                }
                other => panic!("{other:?}"),
            }
        }
        stop.store(true, Ordering::Relaxed);
        let started: usize = spawners.into_iter().map(|s| s.join().unwrap()).sum();
        (late, started)
    });
    let mut ms: Vec<f64> = late.iter().map(|d| d.as_secs_f64() * 1e3).collect();
    ms.sort_by(f64::total_cmp);
    println!(
        "spawns={kind} started={started} rounds={rounds} found_open={} ends_ms={:.1?}",
        ms.len(),
        ms
    );
}
