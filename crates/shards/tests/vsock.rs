//! vsock between the host and real guests: connections both ways, many streams at once,
//! refusals, and restored copies of a guest that listens or holds a connection.

#![cfg(unix)]
#![allow(
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use common::{TempDir, cannot_run_vms, cannot_snapshot, kernel, shards, test_guest};

const TIMEOUT: Duration = Duration::from_secs(60);
const ECHO_PORT: u32 = 1234;

/// A running VM whose console lines arrive on a channel.
struct Vm {
    child: Child,
    lines: mpsc::Receiver<String>,
    seen: Vec<String>,
}

impl Vm {
    fn spawn(args: &[&std::ffi::OsStr]) -> Vm {
        let mut child = Command::new(shards())
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (tx, lines) = mpsc::channel();
        let (out, err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
        for stream in [Box::new(out) as Box<dyn Read + Send>, Box::new(err)] {
            let tx = tx.clone();
            thread::spawn(move || {
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    let _ = tx.send(line);
                }
            });
        }
        Vm {
            child,
            lines,
            seen: Vec::new(),
        }
    }

    /// Waits for a console line containing `text`.
    fn wait_for(&mut self, text: &str) {
        loop {
            match self.lines.recv_timeout(TIMEOUT) {
                Ok(line) => {
                    let found = line.contains(text);
                    self.seen.push(line);
                    if found {
                        return;
                    }
                }
                Err(_) => panic!("no {text:?} from the VM; it said:\n{}", self.seen.join("\n")),
            }
        }
    }

    /// Waits for the VM to exit, keeping everything it printed.
    fn wait_exit(&mut self) -> Option<i32> {
        let code = self.child.wait().unwrap().code();
        // Its pipes are closed now, so the readers end and the channel with them.
        self.seen.extend(self.lines.iter());
        code
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn guest_args(mode: &str, vsock: &Path) -> Vec<std::ffi::OsString> {
    vec![
        "vm".into(),
        "run".into(),
        "--kernel".into(),
        kernel().into(),
        "--init".into(),
        test_guest().into(),
        "--memory".into(),
        "256".into(),
        "--cmdline".into(),
        format!("console=ttyS0 quiet shards_test={mode}").into(),
        "--vsock".into(),
        vsock.into(),
    ]
}

fn os(args: &[std::ffi::OsString]) -> Vec<&std::ffi::OsStr> {
    args.iter().map(|a| a.as_os_str()).collect()
}

/// Opens a stream to guest port `port` through the VM's socket: `Ok` once the guest
/// accepted (the `OK` line was read), `Err` with what came back otherwise.
fn connect(sock: &Path, port: u32) -> Result<UnixStream, String> {
    let mut s = UnixStream::connect(sock).map_err(|e| format!("{}: {e}", sock.display()))?;
    s.set_read_timeout(Some(TIMEOUT)).unwrap();
    s.write_all(format!("CONNECT {port}\n").as_bytes()).unwrap();
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.last() != Some(&b'\n') {
        match s.read(&mut byte) {
            Ok(1) => line.push(byte[0]),
            _ => return Err(format!("refused after {:?}", String::from_utf8_lossy(&line))),
        }
    }
    let text = String::from_utf8_lossy(&line);
    let port: u32 = text
        .strip_prefix("OK ")
        .and_then(|p| p.trim().parse().ok())
        .unwrap_or_else(|| panic!("handshake answered {text:?}"));
    assert!(port >= 1 << 30, "host port {port} outside [2^30, 2^31)");
    Ok(s)
}

/// Streams `len` bytes of pattern `salt` through the guest's echo and checks what comes
/// back, byte for byte, after the half-close.
fn echo(sock: &Path, salt: u64, len: usize) {
    let s = connect(sock, ECHO_PORT).unwrap();
    let mut w = s.try_clone().unwrap();
    let writer = thread::spawn(move || {
        let mut buf = vec![0u8; 256 * 1024];
        let mut sent = 0;
        while sent < len {
            let n = buf.len().min(len - sent);
            shards_testguest::fill(salt, sent as u64, &mut buf[..n]);
            w.write_all(&buf[..n]).unwrap();
            sent += n;
        }
        w.shutdown(Shutdown::Write).unwrap();
    });
    let mut r = s;
    let mut got = 0usize;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = r.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        if let Some(i) = shards_testguest::first_mismatch(salt, got as u64, &buf[..n]) {
            panic!("stream {salt}: byte {} came back wrong", got + i);
        }
        got += n;
    }
    writer.join().unwrap();
    assert_eq!(got, len, "stream {salt}: echoed {got} of {len} bytes");
}

/// Boots the `vsock` guest, serves its connection to host port 5000, and waits until it
/// listens.
fn boot_echo_guest(dir: &Path) -> (Vm, PathBuf) {
    let sock = dir.join("v.sock");
    let host = UnixListener::bind(dir.join("v.sock_5000")).unwrap();
    let mut vm = Vm::spawn(&os(&guest_args("vsock", &sock)));
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = (|| {
            let (mut s, _) = host.accept().map_err(|e| e.to_string())?;
            s.set_read_timeout(Some(TIMEOUT)).map_err(|e| e.to_string())?;
            let mut line = String::new();
            BufReader::new(&s)
                .read_line(&mut line)
                .map_err(|e| e.to_string())?;
            s.write_all(b"hello from the host\n").map_err(|e| e.to_string())?;
            Ok::<String, String>(line)
        })();
        let _ = tx.send(result);
    });
    vm.wait_for("SHARDS-TEST READY");
    let greeting = rx.recv_timeout(TIMEOUT).unwrap().unwrap();
    assert_eq!(greeting, "hello from the guest\n");
    (vm, sock)
}

#[test]
fn host_and_guest_connect_both_ways_and_stream_in_parallel() {
    if cannot_run_vms() {
        return;
    }
    let dir = TempDir::new("vsock-both");
    let (_vm, sock) = boot_echo_guest(&dir);
    // Eight streams at once, each far past every credit window and socket buffer.
    let streams: Vec<_> = (0..8u64)
        .map(|salt| {
            let sock = sock.clone();
            thread::spawn(move || echo(&sock, salt, 8 << 20))
        })
        .collect();
    for s in streams {
        s.join().unwrap();
    }
}

#[test]
fn refused_ports_and_bad_handshakes_close_the_host_socket() {
    if cannot_run_vms() {
        return;
    }
    let dir = TempDir::new("vsock-refuse");
    let (_vm, sock) = boot_echo_guest(&dir);
    assert!(
        connect(&sock, 4321).is_err(),
        "a port nobody listens on was accepted"
    );
    let mut s = UnixStream::connect(&sock).unwrap();
    s.set_read_timeout(Some(TIMEOUT)).unwrap();
    s.write_all(b"HELLO 1234\n").unwrap();
    let mut rest = Vec::new();
    assert_eq!(
        s.read_to_end(&mut rest).unwrap(),
        0,
        "a bad handshake got {rest:?}"
    );
    // The device still serves after both.
    echo(&sock, 99, 1 << 20);
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
    assert_eq!(original.wait_exit(), Some(0), "{}", original.seen.join("\n"));

    let restore = |name: &str| {
        let sock = dir.join(name);
        let vm = Vm::spawn(&[
            "vm".as_ref(),
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
        thread::spawn(move || echo(&sock_a, 1, 4 << 20)),
        thread::spawn(move || echo(&sock_b, 2, 4 << 20)),
    );
    ta.join().unwrap();
    tb.join().unwrap();

    let mut bare = Vm::spawn(&["vm".as_ref(), "restore".as_ref(), snap.as_os_str()]);
    assert_eq!(bare.wait_exit(), Some(1));
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
    let host = UnixListener::bind(path).unwrap();
    thread::spawn(move || {
        let (s, _) = host.accept().unwrap();
        s.set_read_timeout(Some(TIMEOUT)).unwrap();
        let mut reader = BufReader::new(&s);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        (&s).write_all(b"hello from the host\n").unwrap();
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        line
    })
}

/// A snapshot taken while the guest holds a connection to the host: each restored copy
/// finds that connection closed (its host is gone), dials its own host, and serves.
#[test]
fn restored_copies_find_held_connections_closed() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let dir = TempDir::new("vsock-held");
    let snap = dir.join("snap");
    let original_sock = dir.join("original.sock");
    let greeted = greet_once(&original_sock);
    let mut args = guest_args("vsock_snapshot_held", &original_sock);
    args.extend(["--snapshot-dir".into(), snap.clone().into()]);
    let mut original = Vm::spawn(&os(&args));
    assert_eq!(original.wait_exit(), Some(0), "{}", original.seen.join("\n"));
    assert_eq!(greeted.join().unwrap(), "hello from the guest\n");

    for (salt, name) in [(1, "a.sock"), (2, "b.sock")] {
        let sock = dir.join(name);
        let greeted = greet_once(&sock);
        let mut copy = Vm::spawn(&[
            "vm".as_ref(),
            "restore".as_ref(),
            snap.as_os_str(),
            "--vsock".as_ref(),
            sock.as_os_str(),
        ]);
        copy.wait_for("SHARDS-TEST READY");
        assert_eq!(greeted.join().unwrap(), "hello from the guest\n");
        echo(&sock, salt, 1 << 20);
    }
}
