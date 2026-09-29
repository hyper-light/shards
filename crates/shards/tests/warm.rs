//! Warm VMs, end to end: `shards vm restore TEMPLATE --warm FD` resumes a template of an
//! image, lets its guest connect, and serves one request that arrives on FD
//! (crates/shards/src/warm.rs). Here the test is both the daemon, which hands over a
//! command with a client's connection and stdio, and that client. Needs vsock and
//! snapshots.

#![cfg(unix)]
#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]

mod common;

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use common::{TempDir, cannot_run_vms, cannot_snapshot, guest_init, kernel, shards, shardsd, workload_image};
use shards_abi::run::Spec;
use shards_ipc::kind;

const TIMEOUT: Duration = Duration::from_secs(60);

fn pipe() -> (File, File) {
    let mut fds = [0; 2];
    // SAFETY: pipe(2) fills both descriptors.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    // SAFETY: fresh descriptors nothing else owns.
    unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
}

/// Saves a template of the test image, booted and mounted.
fn template(dir: &Path) -> PathBuf {
    let image = workload_image(dir);
    let template = dir.join("template");
    let saved = Command::new(shards())
        .args(["vm", "run", "--kernel"])
        .arg(kernel())
        .arg("--init")
        .arg(guest_init())
        .arg("--rootfs")
        .arg(&image)
        .arg("--snapshot-dir")
        .arg(&template)
        .arg("--no-console")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        saved.status.success(),
        "{}",
        String::from_utf8_lossy(&saved.stderr)
    );
    template
}

/// A warm VM, and the daemon's end of its socket.
struct Warm {
    child: Child,
    daemon: UnixStream,
}

/// What the client holds while its command runs.
struct Client {
    conn: UnixStream,
    stdin: Option<File>,
    stdout: BufReader<File>,
    stderr: File,
}

impl Warm {
    fn spawn(template: &Path) -> Warm {
        let (daemon, theirs) = UnixStream::pair().unwrap();
        let fd = theirs.as_raw_fd();
        // What the daemon runs.
        let mut command = Command::new(shardsd());
        command
            .args(["vm", "restore"])
            .arg(template)
            .args(["--warm", "3"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        // SAFETY: runs in the child between fork and exec, calling only dup2(2) and
        // fcntl(2), which are async-signal-safe.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(fd, 3) < 0 || libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        drop(theirs);
        daemon.set_read_timeout(Some(TIMEOUT)).unwrap();
        Warm { child, daemon }
    }

    fn ready(&mut self) {
        match shards_ipc::recv(&self.daemon) {
            Ok(Some(m)) => assert_eq!(m.kind, kind::READY),
            other => {
                let _ = self.child.kill();
                let mut err = String::new();
                self.child
                    .stderr
                    .take()
                    .unwrap()
                    .read_to_string(&mut err)
                    .unwrap();
                panic!("no READY ({other:?}); the warm VM said:\n{err}");
            }
        }
    }

    /// Hands `argv` to the warm VM for a new client, as the daemon does.
    fn run(&self, argv: &[&str], interactive: bool) -> Client {
        let spec = Spec {
            argv: argv.iter().map(|a| a.as_bytes().to_vec()).collect(),
            env: vec![b"PATH=/bin".to_vec()],
            cwd: b"/".to_vec(),
            user: Vec::new(),
            hostname: b"warm".to_vec(),
        };
        let (conn, theirs) = UnixStream::pair().unwrap();
        let (stdin_r, stdin_w) = pipe();
        let (stdout_r, stdout_w) = pipe();
        let (stderr_r, stderr_w) = pipe();
        let mut payload = vec![if interactive {
            shards_ipc::RUN_INTERACTIVE
        } else {
            0
        }];
        payload.extend(spec.encode());
        shards_ipc::send(
            &self.daemon,
            kind::RUN,
            &payload,
            &[
                theirs.as_fd(),
                stdin_r.as_fd(),
                stdout_w.as_fd(),
                stderr_w.as_fd(),
            ],
        )
        .unwrap();
        // Ours stay open until the warm VM has taken them, as the daemon's do.
        let taken = shards_ipc::recv(&self.daemon).unwrap().expect("TAKEN");
        assert_eq!(taken.kind, kind::TAKEN);
        drop(theirs);
        conn.set_read_timeout(Some(TIMEOUT)).unwrap();
        Client {
            conn,
            stdin: Some(stdin_w),
            stdout: BufReader::new(stdout_r),
            stderr: stderr_r,
        }
    }

    /// The warm VM tells the daemon that its command started, if it did, and its
    /// `status`, with what kept it from starting if it did not (`not_run`), then ends,
    /// leaving the daemon's socket.
    fn ends(mut self, status: u8, not_run: Option<&str>) {
        let started = not_run.is_none();
        let (tx, rx) = mpsc::channel();
        let pid = self.child.id() as libc::pid_t;
        std::thread::spawn(move || {
            if rx.recv_timeout(TIMEOUT).is_err() {
                // SAFETY: kill(2) of our own child, reaped only after `tx` is sent.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        });
        let exited = self.child.wait().unwrap();
        let _ = tx.send(());
        assert!(exited.success(), "the warm VM exited with {exited}");
        let mut said = Vec::new();
        while let Some(m) = shards_ipc::recv(&self.daemon).unwrap() {
            said.push((m.kind, m.payload));
        }
        let mut expected = Vec::new();
        if started {
            expected.push((kind::STARTED, Vec::new()));
        }
        let mut done = vec![status];
        done.extend_from_slice(not_run.unwrap_or_default().as_bytes());
        expected.push((kind::DONE, done));
        assert_eq!(said, expected);
    }
}

impl Client {
    fn exit(&self) -> u8 {
        let m = shards_ipc::recv(&self.conn).unwrap().expect("an exit status");
        assert_eq!((m.kind, m.payload.len()), (kind::EXIT, 1));
        m.payload[0]
    }

    fn stdout(&mut self) -> String {
        let mut out = String::new();
        self.stdout.read_to_string(&mut out).unwrap();
        out
    }

    fn line(&mut self) -> String {
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        line
    }

    fn signal(&self, linux: u32) {
        shards_ipc::send(&self.conn, kind::SIGNAL, &linux.to_be_bytes(), &[]).unwrap();
    }

    fn stdout_ends(&mut self) -> bool {
        ends(&mut self.stdout)
    }
}

/// Whether `stdout` reads end of file within a second: the warm VM holds no copy of it.
/// The client's copies went once the VM took them.
fn ends(stdout: &mut BufReader<File>) -> bool {
    let fd = stdout.get_ref().as_raw_fd();
    // SAFETY: fcntl(2) on a descriptor this test owns.
    unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) };
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    let mut sink = Vec::new();
    loop {
        match stdout.read_to_end(&mut sink) {
            Ok(_) => return true,
            Err(_) if std::time::Instant::now() >= deadline => return false,
            Err(_) => std::thread::sleep(Duration::from_millis(1)),
        }
    }
}

#[test]
fn warm_vms_serve_one_request_on_the_clients_stdio() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let dir = TempDir::new("warm");
    let template = template(&dir);

    let mut warm = Warm::spawn(&template);
    warm.ready();
    let mut client = warm.run(&["/bin/testguest", "report"], false);
    assert_eq!(client.exit(), 0);
    let out = client.stdout();
    for line in ["uid 0", "cwd /", "hostname warm", "env PATH=/bin"] {
        assert!(out.lines().any(|l| l == line), "{line}\n{out}");
    }
    warm.ends(0, None);

    // Exit statuses are `docker run`'s, and a command that cannot run says why on the
    // client's stderr before its status arrives.
    // A command that cannot start: the client hears it as `docker run` says it, and
    // the daemon what dockerd would record.
    let why = "exec: \"/bin/nonexistent\": stat /bin/nonexistent: no such file or directory";
    for (argv, status, not_run) in [
        (&["/bin/testguest", "exit", "7"][..], 7, None),
        (&["/bin/nonexistent"][..], 127, Some(why)),
    ] {
        let mut warm = Warm::spawn(&template);
        warm.ready();
        let mut client = warm.run(argv, false);
        assert_eq!(client.exit(), status, "{argv:?}");
        if let Some(why) = not_run {
            let mut err = String::new();
            client.stderr.read_to_string(&mut err).unwrap();
            assert_eq!(
                err,
                format!(
                    "shards: Error response from daemon: {why}\n\nRun 'shards run --help' for more information\n"
                )
            );
        }
        warm.ends(status, not_run);
    }

    // With -i, the client's stdin is the command's.
    let mut warm = Warm::spawn(&template);
    warm.ready();
    let mut client = warm.run(&["/bin/testguest", "cat"], true);
    let mut stdin = client.stdin.take().unwrap();
    stdin.write_all(b"through the warm VM\n").unwrap();
    drop(stdin);
    assert_eq!(client.exit(), 0);
    assert_eq!(client.stdout(), "through the warm VM\n");
    warm.ends(0, None);
}

#[test]
fn a_warm_vms_client_signals_its_command() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let dir = TempDir::new("warm-signals");
    let template = template(&dir);

    let mut warm = Warm::spawn(&template);
    warm.ready();
    let mut client = warm.run(&["/bin/testguest", "trap", "INT"], false);
    assert_eq!(client.line(), "ready\n");
    client.signal(2);
    assert_eq!(client.exit(), 0);
    assert_eq!(client.stdout(), "got 2\n");
    warm.ends(0, None);

    // A signal the command does not catch ends it, and the status says which.
    let mut warm = Warm::spawn(&template);
    warm.ready();
    let mut client = warm.run(&["/bin/testguest", "sleep"], false);
    assert_eq!(client.line(), "ready\n");
    client.signal(15);
    assert_eq!(client.exit(), 128 + 15);
    warm.ends(128 + 15, None);
}

#[test]
fn warm_needs_a_socket_of_its_own() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let dir = TempDir::new("warm-bad");
    let template = template(&dir);
    for fd in ["0", "2", "9"] {
        let out = Command::new(shards())
            .args(["vm", "restore"])
            .arg(&template)
            .args(["--warm", fd])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!out.status.success(), "--warm {fd} was accepted");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("--warm"),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// A warm VM lets go of its client's stdio before it sends the exit status, so a pipeline
/// reading the client's output ends with the client; and when the client hangs up, so a
/// command that outlives it writes nowhere.
#[test]
fn a_warm_vm_lets_go_of_its_clients_stdio() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let dir = TempDir::new("warm-letgo");
    let template = template(&dir);

    let mut warm = Warm::spawn(&template);
    warm.ready();
    let mut client = warm.run(&["/bin/testguest", "exit", "0"], false);
    assert_eq!(client.exit(), 0);
    // Frozen before it can tear down: whatever it still held, it would hold now.
    let pid = warm.child.id() as libc::pid_t;
    // SAFETY: kill(2) of our own child, reaped only in `ends`.
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    let ended = client.stdout_ends();
    // SAFETY: as above.
    unsafe { libc::kill(pid, libc::SIGCONT) };
    assert!(ended, "the warm VM held the client's stdout past its exit status");
    warm.ends(0, None);

    let mut warm = Warm::spawn(&template);
    warm.ready();
    let mut client = warm.run(&["/bin/testguest", "sleep"], false);
    assert_eq!(client.line(), "ready\n");
    let Client { conn, mut stdout, .. } = client;
    drop(conn);
    assert!(
        ends(&mut stdout),
        "the warm VM kept writing to a client that hung up"
    );
    assert!(
        warm.child.try_wait().unwrap().is_none(),
        "the command outlives its client"
    );
    warm.child.kill().unwrap();
    warm.child.wait().unwrap();
}
