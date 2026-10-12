//! Two vCPUs booted in this process to their end with dynamic code forbidden once their
//! partition is set up, as the VM process forbids it (D141, PM M202): the control that
//! tells ACG apart from the rest of a confined VM process's confinement where its SMP
//! boots stall. A binary of its own: ACG is the process's for good, and windows-2022 sets
//! no other partition up once it is on. Where WHP is shards' backend: x86_64 Windows.

#![cfg(all(windows, target_arch = "x86_64"))]
#![allow(clippy::panic, clippy::unwrap_used)]

mod common;

use std::time::Duration;

use common::{cannot_run_vms, kernel};
use shards_vmm::vm::{self, Config, Console, ExitReason};

const TIMEOUT: Duration = Duration::from_secs(60);

#[test]
fn two_vcpus_boot_to_their_end_under_acg() {
    if cannot_run_vms() {
        return;
    }
    let mut cfg = Config::new(kernel().to_path_buf(), None);
    cfg.vcpus = 2;
    cfg.memory_mib = 512;
    cfg.cmdline = "console=ttyS0 panic=-1".into();
    cfg.console = Console::Discard;
    // The vCPU limit is read first, from a partition of its own (hv::whp::max_vcpus), as
    // the VM process reads it before its own partition's setup.
    vm::max_vcpus().unwrap();
    shards_vmm::hv::forbid_dynamic_code_once_set_up();
    let (handle, running) = vm::start(&cfg).unwrap_or_else(|e| panic!("{e}"));
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(running.wait());
    });
    match rx.recv_timeout(TIMEOUT) {
        Ok(reason) => assert_eq!(reason, ExitReason::Reset),
        Err(_) => {
            handle.stop();
            panic!("two vCPUs under ACG did not reach their end in {TIMEOUT:?} in this process");
        }
    }
}
