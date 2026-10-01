//! What a workload reaches of the host from inside its microVM
//! (docs/architecture/AGENTFILE_ARCH.md §9.7).

#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::time::Duration;

use common::{TempDir, cannot_run_vms, guest_init, kernel, run_shards_env, served};

const TIMEOUT: Duration = Duration::from_secs(120);

/// The guest reaches the host over vsock alone, and its init holds the host's ports, the
/// run and signal ports, before the workload starts: a workload that dials them, the
/// moment it starts, or any other host port, is refused. Before, a workload took a
/// second connection on the run port, and could take the signal port from init.
#[test]
fn a_workload_reaches_no_host_port_over_vsock() {
    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("isolation-vsock-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let ports = [
        shards_abi::run::PORT,
        shards_abi::run::SIGNAL_PORT,
        1026,
        2000,
        52_000,
    ]
    .map(|p| p.to_string());
    let mut args = vec!["--rm", &image, "vsock"];
    args.extend(ports.iter().map(String::as_str));
    // The first run boots the image; the second restores its template.
    for _ in 0..2 {
        let ran = run_shards_env(&["run"], &args, &env, TIMEOUT);
        let shown = format!("--- stdout\n{}\n--- stderr\n{}", ran.stdout, ran.stderr);
        assert_eq!(ran.status, Some(0), "{shown}");
        let lines: Vec<&str> = ran.stdout.lines().collect();
        assert_eq!(lines.len(), ports.len(), "{shown}");
        for (line, port) in lines.iter().zip(&ports) {
            assert!(
                line.starts_with(&format!("{port} refused")),
                "port {port} must refuse the workload: {shown}"
            );
        }
    }
    let _ = run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT);
}
