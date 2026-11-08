//! The conformance suite against the simulator, and the client against stand-ins for the
//! `shards` binary that end or wait as a VM process would.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stdout
)]

use std::io::{Read as _, Write as _};
use std::net::Shutdown;
use std::time::Duration;

use shards_api::conformance::{self, Subject};
use shards_api::sim::{SimGuests, SimHost};
use shards_api::{End, GuestSpec, Guests, Init, Refusal, min_memory_mib};

const ECHO: u32 = 1234;
const UNSERVED: u32 = 1236;
const PATIENCE: Duration = Duration::from_secs(10);

fn spec(cmdline: &str) -> GuestSpec {
    GuestSpec {
        kernel: "kernel".into(),
        init: Init::Executable("init".into()),
        cmdline: cmdline.into(),
        vcpus: 1,
        memory_mib: min_memory_mib(std::env::consts::ARCH),
        disks: vec![],
        shares: vec![],
        ports: vec![ECHO, UNSERVED],
        binary: None,
    }
}

/// The test guest's two modes, as `shards_test=` picks them in Shards' testguest: `echo`
/// serves port 1234 until stopped; anything else powers off.
fn guest(host: &SimHost) -> End {
    if !host.spec().cmdline.contains("shards_test=echo") {
        return End::PowerOff;
    }
    host.listen(ECHO);
    while let Some((_, mut conn)) = host.accept() {
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match conn.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if conn.write_all(&buf[..n]).is_err() {
                            return;
                        }
                    }
                }
            }
            let _ = conn.shutdown(Shutdown::Write);
        });
    }
    End::Stopped
}

/// Every case of the conformance suite passes against the simulator (Hex HEX-GUEST-9).
#[test]
fn the_simulator_conforms() {
    let guests = SimGuests::new(guest);
    conformance::run(&Subject {
        guests: &guests,
        serving: spec("shards_test=echo"),
        port: ECHO,
        unserved: UNSERVED,
        powering_off: spec("shards_test=kmsg"),
        patience: PATIENCE,
    })
    .unwrap();
}

/// A connect waits for a guest that listens late, and is answered as it does (Hex
/// HEX-GUEST-9).
#[test]
fn a_connect_waits_for_a_guest_that_listens_late() {
    let guests = SimGuests::new(|host| {
        std::thread::sleep(Duration::from_millis(100));
        host.listen(ECHO);
        while host.accept().is_some() {}
        End::Stopped
    });
    let g = guests.boot(&spec("")).unwrap();
    assert!(g.connect(ECHO, PATIENCE).is_ok());
}

/// A guest whose program ends in a reset ends as `Reset`, and is refused `Ended` after
/// (Hex HEX-GUEST-9).
#[test]
fn a_guest_ends_as_its_program_says() {
    let guests = SimGuests::new(|_| End::Reset);
    let g = guests.boot(&spec("")).unwrap();
    assert_eq!(g.wait_for(PATIENCE).unwrap(), Some(End::Reset));
    assert_eq!(g.connect(ECHO, PATIENCE).err(), Some(Refusal::Ended(End::Reset)));
}

#[cfg(unix)]
mod client {
    use super::*;
    use shards_api::Shards;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::PathBuf;

    /// A stand-in for `shards` that runs `body` whatever it is asked.
    fn stand_in(dir: &shards_testdir::TempDir, body: &str) -> PathBuf {
        let path = dir.path().join("shards");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// shards that ends before its guest is ready is refused as `Boot`, with its exit code
    /// and what it said last (Hex HEX-GUEST-9).
    #[test]
    fn shards_ending_first_is_a_boot_refusal_with_its_exit_code() {
        let dir = shards_testdir::TempDir::new("api-boot").unwrap();
        let bin = stand_in(&dir, "echo 'no kernel at kernel' >&2; exit 7");
        let g = Shards
            .boot(&GuestSpec {
                binary: Some(bin),
                ..spec("")
            })
            .unwrap();
        match g.connect(ECHO, PATIENCE) {
            Err(Refusal::Boot(why)) => {
                assert!(why.contains('7'), "{why}");
                assert!(why.contains("no kernel at kernel"), "{why}");
            }
            other => panic!("{:?}", other.err()),
        }
        assert!(matches!(g.wait_for(PATIENCE).unwrap(), Some(End::Failed(_))));
    }

    /// shards that exits 0 or 3 ends as `PowerOff` or `Reset`, as `shards run` exits for
    /// them (Hex HEX-GUEST-9).
    #[test]
    fn exit_codes_are_ends() {
        for (code, end) in [(0, End::PowerOff), (3, End::Reset)] {
            let dir = shards_testdir::TempDir::new("api-end").unwrap();
            let bin = stand_in(&dir, &format!("exit {code}"));
            let g = Shards
                .boot(&GuestSpec {
                    binary: Some(bin),
                    ..spec("")
                })
                .unwrap();
            assert_eq!(g.wait_for(PATIENCE).unwrap(), Some(end));
        }
    }

    /// `stop` ends a shards that runs on, as `Stopped`; a connect after is refused
    /// `Ended(Stopped)` (Hex HEX-GUEST-9).
    #[test]
    fn stop_ends_a_running_shards() {
        let dir = shards_testdir::TempDir::new("api-stop").unwrap();
        let bin = stand_in(&dir, "exec sleep 1000");
        let g = Shards
            .boot(&GuestSpec {
                binary: Some(bin),
                ..spec("")
            })
            .unwrap();
        assert_eq!(g.wait_for(Duration::from_millis(50)).unwrap(), None);
        g.stop().unwrap();
        assert_eq!(g.wait_for(PATIENCE).unwrap(), Some(End::Stopped));
        assert_eq!(
            g.connect(ECHO, PATIENCE).err(),
            Some(Refusal::Ended(End::Stopped))
        );
    }

    /// The pid a stand-in wrote to `at`, once it has.
    fn pid_in(at: &std::path::Path) -> libc::pid_t {
        let deadline = std::time::Instant::now() + PATIENCE;
        loop {
            if let Some(pid) = std::fs::read_to_string(at)
                .ok()
                .and_then(|s| s.trim().parse().ok())
            {
                return pid;
            }
            assert!(std::time::Instant::now() < deadline, "no pid in {}", at.display());
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Whether `pid` is gone: reaped, not a zombie, which kill(2) still finds.
    fn reaped(pid: libc::pid_t) -> bool {
        let deadline = std::time::Instant::now() + PATIENCE;
        loop {
            // SAFETY: kill(2) with no signal, which only asks whether the pid is there.
            if unsafe { libc::kill(pid, 0) } != 0 {
                return std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// A guest dropped while shards runs ends it and reaps it, as one dropped after shards
    /// has exited reaps it: neither leaves a zombie for its owner's life.
    #[test]
    fn a_dropped_guest_leaves_no_zombie() {
        for body in [
            "echo $$ > \"$0.pid\"; exec sleep 1000",
            "echo $$ > \"$0.pid\"; exit 0",
        ] {
            let dir = shards_testdir::TempDir::new("api-drop").unwrap();
            let bin = stand_in(&dir, body);
            let g = Shards
                .boot(&GuestSpec {
                    binary: Some(bin.clone()),
                    ..spec("")
                })
                .unwrap();
            let pid = pid_in(&bin.with_extension("pid"));
            if body.ends_with("exit 0") {
                // Exited, and unreaped: no wait_for has asked for its end.
                std::thread::sleep(Duration::from_millis(200));
            }
            drop(g);
            assert!(reaped(pid), "{body}: pid {pid} is left a zombie");
        }
    }

    /// A port off the allowlist is refused before anything is dialled (Hex HEX-GUEST-9).
    #[test]
    fn a_port_off_the_allowlist_is_refused() {
        let dir = shards_testdir::TempDir::new("api-port").unwrap();
        let bin = stand_in(&dir, "exec sleep 1000");
        let g = Shards
            .boot(&GuestSpec {
                binary: Some(bin),
                ..spec("")
            })
            .unwrap();
        assert!(matches!(
            g.connect(9, PATIENCE),
            Err(Refusal::Channel { port: 9, .. })
        ));
    }

    /// A binary that isn't there is a `Boot` refusal naming it (Hex HEX-GUEST-9).
    #[test]
    fn a_missing_binary_is_refused() {
        let missing = PathBuf::from("/shards-api/no/such/shards");
        match Shards.boot(&GuestSpec {
            binary: Some(missing),
            ..spec("")
        }) {
            Err(Refusal::Boot(why)) => assert!(why.contains("/shards-api/no/such/shards"), "{why}"),
            other => panic!("{:?}", other.err()),
        }
    }
}
