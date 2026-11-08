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

use std::time::Duration;

use shards_api::conformance::{self, Subject};
use shards_api::{GuestSpec, Init, Shards, min_memory_mib};

/// Every case passes against real guests (Hex HEX-GUEST-10).
#[test]
fn shards_conforms() {
    if std::env::var_os("SHARDS_API_REAL").is_none_or(|v| v != "1") {
        println!("SKIP: SHARDS_API_REAL=1 boots real guests");
        return;
    }
    let kernel = std::env::var_os("SHARDS_API_KERNEL").expect("SHARDS_API_KERNEL");
    let init = std::env::var_os("SHARDS_API_INIT").expect("SHARDS_API_INIT");
    let spec = |mode: &str| GuestSpec {
        kernel: kernel.clone().into(),
        init: Init::Executable(init.clone().into()),
        cmdline: format!("console=ttyS0 quiet shards_test={mode}"),
        vcpus: 1,
        memory_mib: min_memory_mib(std::env::consts::ARCH).max(256),
        disks: vec![],
        shares: vec![],
        ports: vec![1234, 1236],
        binary: None,
    };
    conformance::run(&Subject {
        guests: &Shards,
        serving: spec("echo"),
        port: 1234,
        unserved: 1236,
        powering_off: spec("kmsg"),
        patience: Duration::from_secs(60),
    })
    .unwrap();
}
