//! The conformance suite against real guests, booted by the `shards` binary: run where
//! `SHARDS_API_REAL=1`, with `SHARDS_API_KERNEL` a kernel (shards' pinned one) and
//! `SHARDS_API_INIT` Shards' test guest (`crates/testguest`, static musl, as the E2E tests
//! build it or `SHARDS_TEST_GUESTS` holds it), booted as its PID 1: `shards_test=echo`
//! serves port 1234, `shards_test=kmsg` powers off. `SHARDS_BIN` names the `shards` to
//! boot them with (on macOS a signed build), else `shards` on `PATH`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stdout
)]

use std::time::{Duration, Instant};

use shards_api::conformance::{self, Subject};
use shards_api::{End, GuestSpec, Guests as _, Init, Shards, min_memory_mib};

/// The real guests' specs, by `shards_test=` mode, where `SHARDS_API_REAL=1` asks for them.
fn specs() -> Option<impl Fn(&str) -> GuestSpec> {
    if std::env::var_os("SHARDS_API_REAL").is_none_or(|v| v != "1") {
        println!("SKIP: SHARDS_API_REAL=1 boots real guests");
        return None;
    }
    let kernel = std::env::var_os("SHARDS_API_KERNEL").expect("SHARDS_API_KERNEL");
    let init = std::env::var_os("SHARDS_API_INIT").expect("SHARDS_API_INIT");
    // One vCPU, the least a guest has; 256 MiB, what Shards' own E2E tests boot its pinned
    // kernel in (`crates/shards/tests/boot.rs:42-43`), and no less than the least it boots
    // in (`min_memory_mib`). Ports: identifiers (vsock(7), a 32-bit port), the test guest's
    // echo port (`crates/testguest/src/linux.rs:3293` `ECHO_PORT`) and one it does not serve.
    Some(move |mode: &str| GuestSpec {
        kernel: kernel.clone().into(),
        init: Init::Executable(init.clone().into()),
        cmdline: format!("console=ttyS0 quiet shards_test={mode}"),
        vcpus: 1,
        memory_mib: min_memory_mib(std::env::consts::ARCH).max(256),
        disks: vec![],
        shares: vec![],
        ports: vec![1234, 1236],
        binary: None,
    })
}

/// Every case passes against real guests (Hex HEX-GUEST-10).
#[test]
fn shards_conforms() {
    let Some(spec) = specs() else { return };
    conformance::run(&Subject {
        guests: &Shards,
        serving: spec("echo"),
        port: 1234,
        unserved: 1236,
        powering_off: spec("kmsg"),
        // A hang guard: each wait returns at its fact. Shards' own E2E tests' guard for a
        // real boot (`crates/shards/tests/boot.rs:14` `TIMEOUT`).
        patience: Duration::from_secs(60),
    })
    .unwrap();
}

/// Measures the waits real guests make, for the bounds that hold them: a boot to its echo
/// port answering, an unserved port's refusal past `UNSERVED_PATIENCE`, `stop`, and a boot
/// to a guest's power-off. `SHARDS_API_WAITS_N` (default 40) of each, printed as n, p50,
/// p90, p99 and max (docs/research/measurements/shards-api-waits/run.sh).
#[test]
#[ignore = "a measurement, run by its harness"]
fn the_waits_of_real_guests() {
    let Some(spec) = specs() else { return };
    let n: usize = std::env::var("SHARDS_API_WAITS_N")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(40);
    let (mut ready, mut over, mut stop, mut off) = (vec![], vec![], vec![], vec![]);
    for _ in 0..n {
        let start = Instant::now();
        let g = Shards.boot(&spec("echo")).unwrap();
        drop(g.connect(1234, Duration::MAX).unwrap());
        ready.push(start.elapsed());
        let start = Instant::now();
        assert!(g.connect(1236, conformance::UNSERVED_PATIENCE).is_err());
        over.push(start.elapsed().saturating_sub(conformance::UNSERVED_PATIENCE));
        let start = Instant::now();
        g.stop().unwrap();
        stop.push(start.elapsed());
        assert_eq!(g.wait_for(Duration::ZERO).unwrap(), Some(End::Stopped));
        let start = Instant::now();
        let g = Shards.boot(&spec("kmsg")).unwrap();
        assert_eq!(g.wait_for(Duration::MAX).unwrap(), Some(End::PowerOff));
        off.push(start.elapsed());
    }
    for (what, mut d) in [
        ("boot to its port answering", ready),
        ("an unserved refusal past its patience", over),
        ("stop", stop),
        ("boot to power-off", off),
    ] {
        d.sort();
        let q = |p: usize| d[(n - 1) * p / 100];
        println!(
            "wait {what}: n={n} p50={:?} p90={:?} p99={:?} max={:?}",
            q(50),
            q(90),
            q(99),
            d[n - 1]
        );
    }
}
