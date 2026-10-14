//! A shards microVM mounts a slates volume over virtio-fs (§4.6, the slates integration; `docs/wip/shards/`).
//!
//! A slates daemon serves a one-file volume on a Unix socket — slates' `bridge-fuse` `shards_fs::serve`, built as
//! the `shards_fs_serve` example and named here by the `SLATES_FS_SERVE` environment variable. The test connects
//! (unsandboxed) and passes the descriptor to `run --kernel … --slates-fs-fd <N>`, which fills a virtio-fs share
//! slot from it (a VM process is App-Sandboxed and cannot open the socket itself); the slot speaks this device's
//! own le32 + FUSE framing, which the slates backend mirrors, so the guest mounts tag `shards0` and reads `/hello`
//! with no host mount and no DAX window — the achievable slates integration where the VMM has no DAX cache window.
//!
//! This runs on Linux (KVM) and macOS (HVF) and on both arches — the integration is Unix-only (fd-passing);
//! Windows microVMs would pass a HANDLE and reach the backend over a named pipe, a follow-up, so the whole test is
//! `#[cfg(unix)]`. It skips loudly (prints and passes) where the host cannot run VMs or `SLATES_FS_SERVE` is unset —
//! the pattern the other VM tests use, so a machine without the slates binary or a hypervisor does not false-fail.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]

mod common;

use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use common::{cannot_run_vms, kernel, test_guest, vm_run};

/// The host bounds the run; the guest's own waits are unbounded (it powers off on done).
const TIMEOUT: Duration = Duration::from_secs(90);

/// Do: serve a slates one-file volume on a socket, boot a microVM whose `shards0` share is backed by that socket,
/// and have the guest mount it and read `/hello`. Expect: the guest reads exactly what slates served, so the
/// testguest prints `SHARDS-TEST PASS` — a microVM reaching a slates volume over virtio-fs, no host mount, no DAX.
#[test]
fn a_microvm_mounts_and_reads_a_slates_volume_over_virtiofs() {
    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let Some(server_bin) = std::env::var_os("SLATES_FS_SERVE") else {
        eprintln!("SKIP: SLATES_FS_SERVE is not set (the built slates `shards_fs_serve` example binary)");
        return;
    };

    // A short socket path (sockaddr_un is 104 bytes on macOS): under the test temp dir.
    let dir = std::env::temp_dir().join(format!("sl-fs-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("s");

    let mut server = std::process::Command::new(&server_bin)
        .arg(&sock)
        .spawn()
        .expect("spawn the slates shards_fs server");

    // The VM's --slates-fs connects at parse time, so the server must have bound the socket first.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !sock.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let bound = sock.exists();

    // Connect here (the test is unsandboxed): a VM process is App-Sandboxed (D30) and cannot open the socket
    // itself, so we hand it the connected descriptor. Clear CLOEXEC so the spawned `shards` — and the
    // shards-vm it execs — inherit the fd, exactly as the daemon hands --warm/--grants descriptors to a VM.
    // `_conn` stays alive across the run (dropped at end of scope) so the socket is not closed under the VM.
    let (run, _conn) = if bound {
        let conn = UnixStream::connect(&sock).expect("connect to the slates server");
        let fd = conn.as_raw_fd();
        // SAFETY: fd is this UnixStream's live descriptor; clearing FD_CLOEXEC neither reads nor writes it.
        let rc = unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
        assert_eq!(rc, 0, "clear CLOEXEC on the slates fd: {}", std::io::Error::last_os_error());
        let k = kernel().to_str().unwrap().to_owned();
        let init = test_guest().to_str().unwrap().to_owned();
        let fdarg = fd.to_string();
        let r = vm_run(
            &[
                "--kernel",
                &k,
                "--init",
                &init,
                "--slates-fs-fd",
                &fdarg,
                "--cmdline",
                "console=ttyS0 quiet panic=-1 shards_test=slates_fs",
            ],
            TIMEOUT,
        );
        (Some(r), Some(conn))
    } else {
        (None, None)
    };

    let _ = server.kill();
    let _ = server.wait();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(bound, "the slates server did not bind {}", sock.display());
    let run = run.expect("a run once the socket was bound");
    assert_eq!(run.status, Some(0), "{run}");
    assert!(
        run.stdout.contains("SHARDS-TEST PASS"),
        "the guest did not mount and read the slates volume: {run}"
    );
}
