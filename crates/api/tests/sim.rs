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

/// vsock ports (vsock(7): a 32-bit port): identifiers, not tuning. The test guest's echo
/// port (`crates/testguest/src/linux.rs:3293` `ECHO_PORT`), and one it does not serve.
const ECHO: u32 = 1234;
const UNSERVED: u32 = 1236;
/// What every wait here is asked with: no clock (`Duration::MAX`, as `Guest::connect` and
/// `Guest::wait_for` take it), so each returns at its fact, a guest's answer or its end, and
/// none is bounded by a number recorded on another host (R03).
const NO_CLOCK: Duration = Duration::MAX;
/// The longest finite patience: u64's most nanoseconds (584 years), a deadline no wait here
/// reaches, for a call that takes only a finite one (`Guests::probe_gpu` refuses no clock as
/// `Pending`), so it too returns at its fact, the stand-in's end.
#[cfg(unix)]
const UNREACHED: Duration = Duration::from_nanos(u64::MAX);

/// A spec of the least guest Shards boots: one vCPU, its least memory (`min_memory_mib`).
fn spec(cmdline: &str) -> GuestSpec {
    GuestSpec {
        kernel: "kernel".into(),
        init: Init::Executable("init".into()),
        cmdline: cmdline.into(),
        vcpus: 1,
        memory_mib: min_memory_mib(std::env::consts::ARCH),
        gpu: false,
        gpu_devices: Vec::new(),
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
            // An input: any size echoes; a page.
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

/// Every case of the conformance suite passes against the simulator (D215).
#[test]
fn the_simulator_conforms() {
    let guests = SimGuests::new(guest);
    conformance::run(&Subject {
        guests: &guests,
        serving: spec("shards_test=echo"),
        port: ECHO,
        unserved: UNSERVED,
        powering_off: spec("shards_test=kmsg"),
        patience: NO_CLOCK,
    })
    .unwrap();
}

/// The simulator's PID 1 sees the spec it was booted with, a GPU or none (D215).
#[test]
fn the_simulator_retains_shared_gpu_enable() {
    for gpu in [false, true] {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let observed = seen.clone();
        let guests = SimGuests::new(move |host| {
            *observed.lock().unwrap() = Some(host.spec().clone());
            End::PowerOff
        });
        let expected = GuestSpec { gpu, ..spec("") };
        let guest = guests.boot(&expected).unwrap();
        let ended = guest.wait_for(NO_CLOCK);
        let stopped = guest.stop();
        drop(guest);
        assert_eq!(ended.unwrap(), Some(End::PowerOff));
        stopped.unwrap();
        assert_eq!(*seen.lock().unwrap(), Some(expected));
    }
}

/// A connect waits for a guest that listens late, and is answered as it does (D215).
#[test]
fn a_connect_waits_for_a_guest_that_listens_late() {
    let guests = SimGuests::new(|host| {
        // An input: late enough that the connect is waiting as it listens.
        std::thread::sleep(Duration::from_millis(100));
        host.listen(ECHO);
        while host.accept().is_some() {}
        End::Stopped
    });
    let g = guests.boot(&spec("")).unwrap();
    assert!(g.connect(ECHO, NO_CLOCK).is_ok());
}

/// A connect and a wait with no clock (`Duration::MAX`) wait for their facts: the guest's
/// listening, its end (D215).
#[test]
fn no_clock_waits_for_the_fact() {
    // An input: late enough that the connect and the wait are waiting.
    let late = Duration::from_millis(100);
    let listening = SimGuests::new(move |host| {
        std::thread::sleep(late);
        host.listen(ECHO);
        while host.accept().is_some() {}
        End::Stopped
    });
    let g = listening.boot(&spec("")).unwrap();
    assert!(g.connect(ECHO, Duration::MAX).is_ok());
    let resetting = SimGuests::new(move |_| {
        std::thread::sleep(late);
        End::Reset
    });
    let g = resetting.boot(&spec("")).unwrap();
    assert_eq!(g.wait_for(Duration::MAX).unwrap(), Some(End::Reset));
}

/// A guest whose program ends in a reset ends as `Reset`, and is refused `Ended` after
/// (D215).
#[test]
fn a_guest_ends_as_its_program_says() {
    let guests = SimGuests::new(|_| End::Reset);
    let g = guests.boot(&spec("")).unwrap();
    assert_eq!(g.wait_for(NO_CLOCK).unwrap(), Some(End::Reset));
    assert_eq!(g.connect(ECHO, NO_CLOCK).err(), Some(Refusal::Ended(End::Reset)));
}

#[cfg(unix)]
mod client {
    use super::*;
    use shards_api::{GpuProbe, GpuProbeError, PhysicalGpu, ResolvedGpu, Shards};
    use std::path::PathBuf;
    use std::time::Instant;

    /// A stand-in for `shards` that runs `body` whatever it is asked. Written to a file of
    /// its own and installed by install(1), so no descriptor of this process ever writes
    /// the file it executes: one that a test running beside it forked while it was open
    /// would make its exec fail ETXTBSY (execve(2)). Then started once and ended at once
    /// (`--warm` first, which no launch gives shards), as macOS assesses a new executable
    /// as it is first started, from 0.1 s to past 8.8 s here (PM M534), and the tests time a
    /// stand-in's own work, not that.
    fn stand_in(dir: &shards_testdir::TempDir, body: &str) -> PathBuf {
        let (source, path) = (dir.path().join("shards.sh"), dir.path().join("shards"));
        std::fs::write(
            &source,
            format!("#!/bin/sh\n[ \"$1\" = --warm ] && exit 0\n{body}\n"),
        )
        .unwrap();
        let installed = std::process::Command::new("install")
            .args(["-m", "755"])
            .arg(&source)
            .arg(&path)
            .status()
            .unwrap();
        assert!(installed.success());
        let warmed = std::process::Command::new(&path)
            .arg("--warm")
            .stdin(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(warmed.success());
        path
    }

    /// shards that ends before its guest is ready is refused as `Boot`, with its exit code
    /// and what it said last (D215).
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
        match g.connect(ECHO, NO_CLOCK) {
            Err(Refusal::Boot(why)) => {
                assert!(why.contains('7'), "{why}");
                assert!(why.contains("no kernel at kernel"), "{why}");
            }
            other => panic!("{:?}", other.err()),
        }
        assert!(matches!(g.wait_for(NO_CLOCK).unwrap(), Some(End::Failed(_))));
    }

    /// A guest that powers off before anything connected to it is refused `Ended(PowerOff)`,
    /// a normal end, not `Boot` (review 15).
    #[test]
    fn a_guest_powered_off_before_a_connect_is_ended_not_a_boot_refusal() {
        let dir = shards_testdir::TempDir::new("api-off").unwrap();
        let bin = stand_in(&dir, "exit 0");
        let g = Shards
            .boot(&GuestSpec {
                binary: Some(bin),
                ..spec("")
            })
            .unwrap();
        assert_eq!(g.wait_for(Duration::MAX).unwrap(), Some(End::PowerOff));
        assert_eq!(
            g.connect(ECHO, Duration::ZERO).err(),
            Some(Refusal::Ended(End::PowerOff))
        );
    }

    /// shards' own refusal of a spec past its hypervisor's bounds, its VMM's words
    /// (`crates/vmm/src/vm/runtime.rs:1065`), is a `Bounds` refusal naming the vCPUs, not
    /// `Boot` (review 6).
    #[test]
    fn shards_refusing_the_vcpus_is_a_bounds_refusal() {
        let dir = shards_testdir::TempDir::new("api-vcpus").unwrap();
        let bin = stand_in(
            &dir,
            "echo 'shards: 4294967295 vCPUs requested; this host supports 14' >&2; exit 1",
        );
        let g = Shards
            .boot(&GuestSpec {
                binary: Some(bin),
                ..spec("")
            })
            .unwrap();
        match g.connect(ECHO, Duration::MAX) {
            Err(Refusal::Bounds { what: "vcpus", why }) => {
                assert!(why.contains("this host supports 14"), "{why}")
            }
            other => panic!("{:?}", other.err()),
        }
    }

    /// shards that exits 0 or 3 ends as `PowerOff` or `Reset`, as `shards run` exits for
    /// them (D215).
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
            assert_eq!(g.wait_for(NO_CLOCK).unwrap(), Some(end));
        }
    }

    /// D215, D232: a GPU probe takes a stand-in's exact, completed report and refuses one
    /// of another GPU or cut short, each by its content; with no patience it is pending.
    /// Real bounded pipe, watch and parse wiring with a stand-in process, not a renderer or
    /// a GPU of the host's. Its patience is the stand-ins' (PM M463), not a real guest's
    /// (PM M534).
    #[test]
    fn gpu_probe_protocol_requires_exact_completed_receipt() {
        let native = PhysicalGpu::MetalRegistry(7);
        let expected = GpuProbe {
            devices: vec![ResolvedGpu {
                physical: native,
                uuid: [7; 16],
                pci: None,
                luid: Some(7u64.to_be_bytes()),
                name: None,
                device_local_bytes: None,
            }],
            host_visible_bytes: 4096,
        };
        let body = expected.encode().unwrap();
        let probing = |bin| GuestSpec {
            binary: Some(bin),
            gpu: true,
            gpu_devices: vec![native],
            ..spec("")
        };
        for (text, refused) in [
            (body.clone(), None),
            (
                body.replace("metal:0000000000000007", "metal:0000000000000008"),
                Some(GpuProbeError::Identity),
            ),
            (body.replace("end\n", ""), Some(GpuProbeError::Syntax)),
        ] {
            let dir = shards_testdir::TempDir::new("api-gpu-probe").unwrap();
            // Canonical protocol fields contain no shell quoting characters.
            let bin = stand_in(&dir, &format!("printf '%s' '{text}'"));
            let probed = Shards.probe_gpu(&probing(bin), UNREACHED);
            match refused {
                None => assert_eq!(probed.unwrap(), expected),
                Some(why) => assert_eq!(probed.err(), Some(Refusal::GpuProbe(why))),
            }
        }
        assert_eq!(
            Shards
                .probe_gpu(&probing(PathBuf::from("never-run")), Duration::ZERO)
                .err(),
            Some(Refusal::GpuProbe(GpuProbeError::Pending))
        );
    }

    /// `stop` ends a shards that runs on, as `Stopped`; a connect after is refused
    /// `Ended(Stopped)` (D215).
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
        assert_eq!(g.wait_for(Duration::ZERO).unwrap(), None);
        g.stop().unwrap();
        assert_eq!(g.wait_for(NO_CLOCK).unwrap(), Some(End::Stopped));
        assert_eq!(
            g.connect(ECHO, NO_CLOCK).err(),
            Some(Refusal::Ended(End::Stopped))
        );
    }

    /// A port off the allowlist is refused before anything is dialled (D215).
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
            g.connect(9, NO_CLOCK),
            Err(Refusal::Channel { port: 9, .. })
        ));
    }

    /// A connect with no clock (`Duration::MAX`) is refused as soon as shards ends, as
    /// `Boot` with its last words: the VM's end is the fact it waits on; and a wait with no
    /// clock returns at that end (D215).
    #[test]
    fn no_clock_ends_with_shards() {
        let dir = shards_testdir::TempDir::new("api-noclock").unwrap();
        // shards ends when the test says, after the wait's thread is started: it reads a line
        // from a FIFO the test writes then, no clock in either. A wait that begins only after
        // the end is refused the same, at the end's fact.
        let go = dir.path().join("go");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&go)
                .status()
                .unwrap()
                .success()
        );
        let bin = stand_in(
            &dir,
            &format!(
                "read _ < '{}'; echo 'no kernel at kernel' >&2; exit 7",
                go.display()
            ),
        );
        let g = Shards
            .boot(&GuestSpec {
                binary: Some(bin.clone()),
                ..spec("")
            })
            .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(g.connect(ECHO, Duration::MAX).err()));
        std::fs::write(&go, "end\n").unwrap();
        match rx.recv().unwrap() {
            Some(Refusal::Boot(why)) => assert!(why.contains("no kernel at kernel"), "{why}"),
            other => panic!("{other:?}"),
        }
        let g = Shards
            .boot(&GuestSpec {
                binary: Some(bin),
                ..spec("")
            })
            .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(g.wait_for(Duration::MAX)));
        std::fs::write(&go, "end\n").unwrap();
        assert!(matches!(rx.recv().unwrap(), Ok(Some(End::Failed(_)))));
    }

    /// The longest report shards writes as it ends, a reset's with its guest's last console
    /// bytes escaped (six bytes for each of the `--console-said N` the client passed) and
    /// their first said lost, is quoted whole (D12, D215).
    #[test]
    fn the_longest_report_shards_writes_is_quoted_whole() {
        let dir = shards_testdir::TempDir::new("api-tail").unwrap();
        let said = dir.path().join("said");
        let script = r#"n=
while [ $# -gt 0 ]; do
  if [ "$1" = --console-said ]; then n=$2; fi
  shift
done
{
  printf "shards: guest requested a reset, its console's last lines, those before them lost past the %s bytes kept:\n" "$n"
  i=0
  while [ "$i" -lt "$n" ]; do printf '\\u{1b}'; i=$((i+1)); done
  printf '\n'
} > 'SAID'
cat 'SAID' >&2
exit 3"#
            .replace("SAID", &said.display().to_string());
        let bin = stand_in(&dir, &script);
        let g = Shards
            .boot(&GuestSpec {
                binary: Some(bin),
                ..spec("")
            })
            .unwrap();
        let outcome = g.connect(ECHO, NO_CLOCK);
        let report = std::fs::read_to_string(&said).unwrap();
        let report = report.trim_end();
        assert!(report.ends_with("\\u{1b}"), "{report:.200}");
        match outcome {
            Err(Refusal::Boot(why)) => assert!(why.ends_with(report), "{} bytes: {why:.200}", why.len()),
            other => panic!("{:?}", other.err()),
        }
    }

    /// shards' refusal comes as shards ends, with its last words, though a child it left
    /// holds its output open (macOS's grants broker until it exits): the end of its words is
    /// a fact, not a clock. So the refusal comes while that child still lives, which no
    /// refusal held back for the child's end could (D215).
    #[test]
    fn a_child_holding_shards_output_holds_no_refusal_back() {
        let dir = shards_testdir::TempDir::new("api-held").unwrap();
        let (pid, ending) = (dir.path().join("pid"), dir.path().join("ending"));
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&ending)
                .status()
                .unwrap()
                .success()
        );
        let bin = stand_in(
            &dir,
            &format!(
                "sleep 1000 & echo $! > '{}'; echo 'no kernel at kernel' >&2; echo > '{}'; exit 7",
                pid.display(),
                ending.display()
            ),
        );
        let g = Shards
            .boot(&GuestSpec {
                binary: Some(bin),
                ..spec("")
            })
            .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let refused = g.connect(ECHO, NO_CLOCK).err();
            tx.send(refused)
        });
        std::fs::read(&ending).unwrap();
        let refused = rx.recv().unwrap();
        let holder = std::fs::read_to_string(&pid).unwrap();
        let held = std::process::Command::new("kill")
            .args(["-0", holder.trim()])
            .status()
            .unwrap()
            .success();
        let _ = std::process::Command::new("kill").arg(holder.trim()).status();
        match refused {
            Some(Refusal::Boot(why)) => assert!(why.contains("no kernel at kernel"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert!(
            held,
            "the refusal came only once the child holding shards' output had gone"
        );
    }

    /// Measures how long past its patience an unserved connect's refusal comes, the
    /// client's (a shards that never binds its socket) and the simulator's, for
    /// `conformance::OVERSHOOT`: `SHARDS_API_OVERSHOOT_N` runs of each, printed as n, p50,
    /// p90, p99 and max (docs/research/measurements/shards-api-overshoot/run.sh).
    #[test]
    #[ignore = "a measurement, run by its harness"]
    fn overshoot_of_an_unserved_connect() {
        let n: usize = std::env::var("SHARDS_API_OVERSHOOT_N")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(300);
        let patience = conformance::UNSERVED_PATIENCE;
        let dir = shards_testdir::TempDir::new("api-over").unwrap();
        let bin = stand_in(&dir, "exec sleep 1000");
        let client = Shards
            .boot(&GuestSpec {
                binary: Some(bin),
                ..spec("")
            })
            .unwrap();
        let sims = SimGuests::new(guest);
        let sim = sims.boot(&spec("shards_test=echo")).unwrap();
        for (what, g) in [("client", &client), ("sim", &sim)] {
            let mut over: Vec<Duration> = (0..n)
                .map(|_| {
                    let start = Instant::now();
                    assert!(g.connect(UNSERVED, patience).is_err());
                    start.elapsed().saturating_sub(patience)
                })
                .collect();
            over.sort();
            let q = |p: usize| over[(n - 1) * p / 100];
            println!(
                "overshoot {what}: n={n} p50={:?} p90={:?} p99={:?} max={:?}",
                q(50),
                q(90),
                q(99),
                over[n - 1]
            );
        }
    }

    /// Measures the waits this file's tests make, each kind on a thread of its own as the
    /// tests run side by side, `SHARDS_API_WAITS_N` (default 40) of each, with a new stand-in
    /// script for each (a new script's first start is slow on macOS): printed as n, p50, p90,
    /// p99 and max (docs/research/measurements/shards-api-waits/run.sh): what these waits
    /// cost, each of which waits for its fact rather than a bound.
    #[test]
    #[ignore = "a measurement, run by its harness"]
    fn the_waits_of_these_tests() {
        let n: usize = std::env::var("SHARDS_API_WAITS_N")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(40);
        let fresh = |name: &str, body: &str| {
            let dir = shards_testdir::TempDir::new(name).unwrap();
            let bin = stand_in(&dir, body);
            (dir, bin)
        };
        let boot = |bin: PathBuf| {
            Shards
                .boot(&GuestSpec {
                    binary: Some(bin),
                    ..spec("")
                })
                .unwrap()
        };
        type Kind = (&'static str, Box<dyn Fn() -> Duration + Send + Sync>);
        let kinds: Vec<Kind> = vec![
            (
                "a simulated guest that listens late, connected to",
                Box::new(|| {
                    let late = SimGuests::new(|host| {
                        std::thread::sleep(Duration::from_millis(100));
                        host.listen(ECHO);
                        while host.accept().is_some() {}
                        End::Stopped
                    });
                    let start = Instant::now();
                    let g = late.boot(&spec("")).unwrap();
                    assert!(g.connect(ECHO, Duration::MAX).is_ok());
                    start.elapsed()
                }),
            ),
            (
                "a simulated guest's echo of conformance's payload",
                Box::new(|| {
                    let sims = SimGuests::new(guest);
                    let g = sims.boot(&spec("shards_test=echo")).unwrap();
                    let mut conn = g.connect(ECHO, Duration::MAX).unwrap();
                    let sent: Vec<u8> = (0..64 * 1024 + 1u32).map(|i| (i % 251) as u8).collect();
                    let mut writer = conn.try_clone().unwrap();
                    let start = Instant::now();
                    std::thread::scope(|scope| {
                        let wrote = scope.spawn(|| {
                            writer.write_all(&sent).unwrap();
                            writer.shutdown(Shutdown::Write).unwrap();
                        });
                        let mut got = Vec::new();
                        conn.read_to_end(&mut got).unwrap();
                        wrote.join().unwrap();
                        assert_eq!(got, sent);
                    });
                    start.elapsed()
                }),
            ),
            (
                "a stand-in ending first, refused",
                Box::new(move || {
                    let (_dir, bin) = fresh("w-end", "echo 'no kernel' >&2; exit 7");
                    let start = Instant::now();
                    let g = boot(bin);
                    assert!(matches!(g.connect(ECHO, Duration::MAX), Err(Refusal::Boot(_))));
                    start.elapsed()
                }),
            ),
            (
                "a stand-in exiting 0, its end",
                Box::new(move || {
                    let (_dir, bin) = fresh("w-off", "exit 0");
                    let start = Instant::now();
                    let g = boot(bin);
                    assert_eq!(g.wait_for(Duration::MAX).unwrap(), Some(End::PowerOff));
                    start.elapsed()
                }),
            ),
            (
                "a running stand-in, stopped",
                Box::new(move || {
                    let (_dir, bin) = fresh("w-stop", "exec sleep 1000");
                    let g = boot(bin);
                    let start = Instant::now();
                    g.stop().unwrap();
                    assert_eq!(g.wait_for(Duration::MAX).unwrap(), Some(End::Stopped));
                    start.elapsed()
                }),
            ),
            (
                "a held output's refusal after shards' end",
                Box::new(move || {
                    let dir = shards_testdir::TempDir::new("w-held").unwrap();
                    let (pid, ending) = (dir.path().join("pid"), dir.path().join("ending"));
                    let made = std::process::Command::new("mkfifo").arg(&ending).status();
                    assert!(made.unwrap().success());
                    let bin = stand_in(
                        &dir,
                        &format!(
                            "sleep 1000 & echo $! > '{}'; echo > '{}'; exit 7",
                            pid.display(),
                            ending.display()
                        ),
                    );
                    let g = boot(bin);
                    let (tx, rx) = std::sync::mpsc::channel();
                    std::thread::spawn(move || {
                        let _ = g.connect(ECHO, Duration::MAX);
                        tx.send(Instant::now())
                    });
                    std::fs::read(&ending).unwrap();
                    let ended = Instant::now();
                    let at = rx.recv().unwrap();
                    let holder = std::fs::read_to_string(&pid).unwrap();
                    let _ = std::process::Command::new("kill").arg(holder.trim()).status();
                    at.saturating_duration_since(ended)
                }),
            ),
        ];
        std::thread::scope(|scope| {
            let runs: Vec<_> = kinds
                .iter()
                .map(|(what, kind)| {
                    scope.spawn(move || {
                        let mut d: Vec<Duration> = (0..n).map(|_| kind()).collect();
                        d.sort();
                        (*what, d)
                    })
                })
                .collect();
            for run in runs {
                let (what, d) = run.join().unwrap();
                let q = |p: usize| d[(n - 1) * p / 100];
                println!(
                    "wait {what}: n={n} p50={:?} p90={:?} p99={:?} max={:?}",
                    q(50),
                    q(90),
                    q(99),
                    d[n - 1]
                );
            }
        });
    }

    /// A shards that ignores SIGTERM is killed by `stop` at once, and ends as
    /// `Stopped` (D215).
    #[test]
    fn stop_kills_a_shards_that_ignores_sigterm() {
        let dir = shards_testdir::TempDir::new("api-deaf").unwrap();
        let deaf = dir.path().join("deaf");
        let made = std::process::Command::new("mkfifo").arg(&deaf).status();
        assert!(made.unwrap().success());
        // The shell itself ignores SIGTERM (an exec'd program's dispositions vary by shell),
        // and says so once it does; the sleep it waits on at its kill ends within its second.
        let bin = stand_in(
            &dir,
            &format!(
                "trap '' TERM; echo > '{}'; while :; do sleep 1; done",
                deaf.display()
            ),
        );
        let g = Shards
            .boot(&GuestSpec {
                binary: Some(bin),
                ..spec("")
            })
            .unwrap();
        std::fs::read(&deaf).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let stopped = g.stop();
            tx.send((stopped, g.wait_for(NO_CLOCK)))
        });
        let (stopped, end) = rx.recv().unwrap();
        stopped.unwrap();
        assert_eq!(end.unwrap(), Some(End::Stopped));
    }

    /// A binary that isn't there is a `Boot` refusal naming it (D215).
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
