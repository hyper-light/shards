//! vsock between the host and real guests: connections both ways, many streams at once,
//! refusals, and restored copies of a guest that listens or holds a connection. On Windows
//! through a VM process confined as D141 confines it, a connection's host side two pipes
//! (D272): its listener's pairs made and its host ports' opened by its spawner, a host port
//! a pipe listener of its own ([`common::host_port`]).

#![allow(
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;

use common::{
    Following, TempDir, Vm, VsockStream, cannot_run_vms, cannot_snapshot, echo, host_port, host_port_accept,
    kernel, test_guest, vsock_connect, vsock_dial, within,
};
const ECHO_PORT: u32 = 1234;

fn guest_args(mode: &str, vsock: &Path) -> Vec<std::ffi::OsString> {
    vec![
        "run".into(),
        "--kernel".into(),
        kernel().into(),
        "--init".into(),
        test_guest().into(),
        "--memory".into(),
        "256".into(),
        "--cmdline".into(),
        format!("console=hvc0 quiet shards_test={mode}").into(),
        "--vsock".into(),
        vsock.into(),
    ]
}

fn os(args: &[std::ffi::OsString]) -> Vec<&std::ffi::OsStr> {
    args.iter().map(|a| a.as_os_str()).collect()
}

/// Boots the `vsock` guest, serves its connection to host port 5000, and waits until it
/// listens: its greeting read as a guest's stream is ([`Following`]), and waited for while
/// its guest goes on ([`within`]).
#[track_caller]
fn boot_echo_guest(dir: &Path) -> (Vm, PathBuf) {
    let sock = dir.join("v.sock");
    let mut host = host_port(dir.join("v.sock_5000"));
    // Its device's debug lines, which a failure prints with what the VM said.
    let mut vm = Vm::spawn_env(&os(&guest_args("vsock", &sock)), &[("SHARDS_LOG", "debug")]);
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = (|| {
            let mut s = host_port_accept(&mut host).map_err(|e| e.to_string())?;
            let mut line = String::new();
            let mut greeting = BufReader::new(Following::of(&s, None));
            greeting.read_line(&mut line).map_err(|e| e.to_string())?;
            greeting.get_mut().answered();
            s.write_all(b"hello from the host\n").map_err(|e| e.to_string())?;
            Ok::<String, String>(line)
        })();
        let _ = tx.send(result);
    });
    vm.wait_for("SHARDS-TEST READY");
    let greeting = within("the guest's greeting", &vm, move || rx.recv())
        .unwrap()
        .unwrap();
    assert_eq!(greeting, "hello from the guest\n");
    (vm, sock)
}

#[test]
fn host_and_guest_connect_both_ways_and_stream_in_parallel() {
    if cannot_run_vms() {
        return;
    }
    let dir = TempDir::new("vsock-both");
    let (vm, sock) = boot_echo_guest(&dir);
    // Eight streams at once, each far past every credit window and socket buffer.
    within("eight streams through the echo", &vm, move || {
        let streams: Vec<_> = (0..8u64)
            .map(|salt| {
                let sock = sock.clone();
                thread::spawn(move || echo(&sock, ECHO_PORT, salt, 8 << 20, None).unwrap())
            })
            .collect();
        for s in streams {
            s.join().unwrap();
        }
    });
}

#[test]
fn refused_ports_and_bad_handshakes_close_the_host_socket() {
    if cannot_run_vms() {
        return;
    }
    let dir = TempDir::new("vsock-refuse");
    let (vm, sock) = boot_echo_guest(&dir);
    within("refusals, then an echo", &vm, move || {
        assert!(
            vsock_connect(&sock, 4321).is_err(),
            "a port nobody listens on was accepted"
        );
        let mut s = vsock_dial(&sock).unwrap();
        s.write_all(b"HELLO 1234\n").unwrap();
        let mut rest = Vec::new();
        assert_eq!(
            Following::of(&s, None).read_to_end(&mut rest).unwrap(),
            0,
            "a bad handshake got {rest:?}"
        );
        // The device still serves after both.
        echo(&sock, ECHO_PORT, 99, 1 << 20, None).unwrap();
    });
}

#[test]
fn restored_copies_listen_on_their_own_sockets() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let dir = TempDir::new("vsock-restore");
    let snap = dir.join("snap");
    let mut args = guest_args("vsock_snapshot", &dir.join("original.sock"));
    args.extend(["--snapshot-dir".into(), snap.clone().into()]);
    let mut original = Vm::spawn(&os(&args));
    assert_eq!(
        original.wait_exit_following(),
        Some(0),
        "{}",
        original.seen.join("\n")
    );

    let restore = |name: &str| {
        let sock = dir.join(name);
        let vm = Vm::spawn(&[
            "restore".as_ref(),
            snap.as_os_str(),
            "--vsock".as_ref(),
            sock.as_os_str(),
        ]);
        (vm, sock)
    };
    let (mut a, sock_a) = restore("a.sock");
    let (mut b, sock_b) = restore("b.sock");
    a.wait_for("SHARDS-TEST READY");
    b.wait_for("SHARDS-TEST READY");
    let (ta, tb) = (
        thread::spawn(move || echo(&sock_a, ECHO_PORT, 1, 4 << 20, None).unwrap()),
        thread::spawn(move || echo(&sock_b, ECHO_PORT, 2, 4 << 20, None).unwrap()),
    );
    ta.join().unwrap();
    tb.join().unwrap();

    let mut bare = Vm::spawn(&["restore".as_ref(), snap.as_os_str()]);
    assert_eq!(bare.wait_exit_following(), Some(1));
    assert!(
        bare.seen.iter().any(|l| l.contains("--vsock")),
        "{}",
        bare.seen.join("\n")
    );
}

/// Serves one guest connection to host port 5000 at `sock`: reads the guest's line,
/// answers, and holds the connection until the guest's side closes.
fn greet_once(sock: &Path) -> thread::JoinHandle<String> {
    let mut path = sock.as_os_str().to_owned();
    path.push("_5000");
    let mut host = host_port(path);
    thread::spawn(move || {
        let s = host_port_accept(&mut host).unwrap();
        let mut reader = BufReader::new(Following::of(&s, None));
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        (&s).write_all(b"hello from the host\n").unwrap();
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        line
    })
}

/// Serves one guest connection to host port 5000 at `sock`: reads the guest's line, answers,
/// and hands the connection back open, read no further, for its caller to hold.
fn greet_held(sock: &Path) -> thread::JoinHandle<(String, VsockStream)> {
    let mut path = sock.as_os_str().to_owned();
    path.push("_5000");
    let mut host = host_port(path);
    thread::spawn(move || {
        let s = host_port_accept(&mut host).unwrap();
        let mut greeting = BufReader::new(Following::of(&s, None));
        let mut line = String::new();
        greeting.read_line(&mut line).unwrap();
        greeting.get_mut().answered();
        drop(greeting);
        (&s).write_all(b"hello from the host\n").unwrap();
        (line, s)
    })
}

/// A snapshot taken while the guest holds a connection to the host: each restored copy
/// finds that connection closed (its host is gone), dials its own host, and serves.
///
/// The original's connection is held through its snapshot, never read to its end. Its end
/// is its VM process's exit, which comes only once the snapshot is durable (runtime.rs
/// `Coordinator::take`: `staged.commit()` syncs each file, then `ExitReason::Snapshotted`):
/// host work, which the wait for that exit follows by the process's CPU and by its waits on
/// its host's disk ([`Vm::wait_exit_following`]). Read as a guest's stream, whose progress is
/// its bytes alone once its first has come ([`Following`]), the end was refused 221.4 ms in,
/// no progress for 207.9 ms past a W* of 110.5 ms, while the VM process committed the
/// snapshot, a thread of it in the block layer's write throttle (`rq_qos_wait`); it exited 0
/// within 0.39 s of the refusal (CI 44268931283, x86_64-unknown-linux-gnu). Owed: that the
/// connection reads its end at once as its VM process exits, checked once that is measured
/// on Windows and macOS.
#[test]
fn restored_copies_find_held_connections_closed() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let dir = TempDir::new("vsock-held");
    let snap = dir.join("snap");
    let original_sock = dir.join("original.sock");
    let greeted = greet_held(&original_sock);
    let mut args = guest_args("vsock_snapshot_held", &original_sock);
    args.extend(["--snapshot-dir".into(), snap.clone().into()]);
    let mut original = Vm::spawn(&os(&args));
    assert_eq!(
        original.wait_exit_following(),
        Some(0),
        "{}",
        original.seen.join("\n")
    );
    let (line, held) = greeted.join().unwrap();
    drop(held);
    assert_eq!(line, "hello from the guest\n");

    for (salt, name) in [(1, "a.sock"), (2, "b.sock")] {
        let sock = dir.join(name);
        let greeted = greet_once(&sock);
        let mut copy = Vm::spawn(&[
            "restore".as_ref(),
            snap.as_os_str(),
            "--vsock".as_ref(),
            sock.as_os_str(),
        ]);
        copy.wait_for("SHARDS-TEST READY");
        assert_eq!(greeted.join().unwrap(), "hello from the guest\n");
        echo(&sock, ECHO_PORT, salt, 1 << 20, None).unwrap();
    }
}

/// The port the test guest's `vsock_bench` serves on.
const BENCH_PORT: u32 = 1240;

/// A guest socket whose own vsock buffer is smaller than the device's window streams to the
/// host whole. Linux sends no more than the lesser of the device's window and its own buffer
/// unacknowledged (virtio_transport_tx_buf_size, 8ee784fdf006, in the pinned 6.18.48): the
/// device must credit it before forwarding a whole window, or it waits for ever. The test
/// guest's `T` measurement sends from a 16 KiB socket buffer, a quarter of the window.
#[test]
fn a_guest_socket_with_less_buffer_than_the_window_streams_whole() {
    if cannot_run_vms() {
        return;
    }
    let dir = TempDir::new("vsock-small");
    let sock = dir.join("v.sock");
    let mut vm = Vm::spawn(&os(&guest_args("vsock_bench", &sock)));
    vm.wait_for("SHARDS-TEST READY");
    within("a stream from a small guest buffer", &vm, move || {
        let bytes = 1 << 20;
        let s = bench_open(&sock, b'T', bytes);
        // A stall waits for credit that never comes: its bytes stop, and its read is
        // refused past the slowest whole stream of its kind ([`Following`]).
        let mut stream = Following::of(&s, None);
        let mut buf = vec![0u8; 64 << 10];
        read_bytes(&mut stream, &mut buf, bytes);
        stream.answered();
        s.shutdown(std::net::Shutdown::Write).unwrap();
        let mut rest = Vec::new();
        Following::of(&s, None).read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty(), "{} bytes past the stream", rest.len());
    });
}

/// A guest's bytes before its end are its host's, whatever the end (D251, amended): a guest
/// that sends a stream and closes it while its host's way is open (close(2), one SHUTDOWN of
/// RCV and SEND: the test guest's `C`), or cuts it as shards-init's relay does (its reads'
/// end, then both ways: `X`), has its host read every byte, in order, then end-of-stream,
/// never a reset. A host's write after it then fails, and is never taken for a guest that has
/// gone: on Windows at once, ERROR_NO_DATA (232), as the VM closes its connection's way up
/// before its way down's end (D272), where on AF_UNIX it was taken and went nowhere (PM M358).
#[test]
fn a_guest_that_ends_its_stream_gives_its_host_every_byte_first() {
    if cannot_run_vms() {
        return;
    }
    let dir = TempDir::new("vsock-end");
    let sock = dir.join("v.sock");
    let mut vm = Vm::spawn(&os(&guest_args("vsock_bench", &sock)));
    vm.wait_for("SHARDS-TEST READY");
    within("a guest's ends of its streams", &vm, move || {
        // Past every credit window and socket buffer, so that the end comes while the
        // host still has bytes to read.
        let bytes = 8u64 << 20;
        for op in *b"CX" {
            let s = bench_open(&sock, op, bytes);
            let mut got = Vec::new();
            let read = Following::of(&s, None).read_to_end(&mut got);
            assert_eq!(
                read.map_err(|e| e.kind()),
                Ok(usize::try_from(bytes).unwrap()),
                "{}: {} bytes read",
                op as char,
                got.len()
            );
            let first_wrong = got.iter().enumerate().find(|&(i, &v)| v != (i % 251) as u8);
            assert_eq!(first_wrong, None, "{}", op as char);
            // Its connection goes once the guest's end is the host's: a write then fails,
            // bounded by `within`, never filling a socket nobody reads.
            #[cfg(unix)]
            while (&s).write(b"x").is_ok() {}
            // On Windows the first fails: its way up closed before its way down's end.
            #[cfg(windows)]
            assert_eq!(
                (&s).write(b"x").map_err(|e| e.raw_os_error()),
                Err(Some(232)),
                "{}: a write after the guest's end",
                op as char
            );
        }
    });
}

/// A connection to the test guest's `vsock_bench`, asking for `op` of `bytes` bytes.
fn bench_open(sock: &Path, op: u8, bytes: u64) -> VsockStream {
    let mut s = vsock_connect(sock, BENCH_PORT).unwrap();
    let mut head = [0u8; 9];
    head[0] = op;
    head[1..].copy_from_slice(&bytes.to_le_bytes());
    s.write_all(&head).unwrap();
    s
}

/// Reads `bytes` bytes of `s` through `buf`, failing where its read is refused.
fn read_bytes(s: &mut impl Read, buf: &mut [u8], bytes: u64) {
    let mut left = bytes;
    while left > 0 {
        let n = (buf.len() as u64).min(left) as usize;
        let got = s
            .read(&mut buf[..n])
            .unwrap_or_else(|e| panic!("stalled with {left} of {bytes} bytes left: {e}"));
        assert_ne!(got, 0, "the guest ended {left} bytes early");
        left -= got as u64;
    }
}
