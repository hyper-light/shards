//! A daemon and a client of it over QUIC on this machine's loopback (D180): a client joins by
//! an invitation, then runs commands, their output and status crossing whole; a client not
//! enrolled, and a join with another secret, are refused in the daemon's words.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::io::Write as _;
use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::sync::{Mutex, mpsc};

use shards_remote::Error;
use shards_remote::client::{self, Remote};
use shards_remote::frame::Request;
use shards_remote::identity::Identity;
use shards_remote::invite::{self, Invitation};
use shards_remote::server::{self, Host, Running, Stopper};

/// A machine whose clients and invitations are kept in memory, running each command as a
/// process of its own.
struct Machine {
    clients: Mutex<Vec<(String, Vec<u8>)>>,
    invitations: Mutex<Vec<Invitation>>,
}

impl Host for Machine {
    fn enrolled(&self, spki: &[u8]) -> Option<String> {
        let clients = self.clients.lock().unwrap();
        clients
            .iter()
            .find(|(_, key)| key == spki)
            .map(|(name, _)| name.clone())
    }

    fn join(&self, name: &str, proof: &[u8], exported: &[u8; 32], spki: &[u8]) -> Result<(), Error> {
        let mut invitations = self.invitations.lock().unwrap();
        let at = invitations
            .iter()
            .position(|i| i.name == name && invite::verify(&i.secret, exported, proof))
            .ok_or_else(|| Error::new("no"))?;
        invitations.remove(at);
        self.clients
            .lock()
            .unwrap()
            .push((name.to_string(), spki.to_vec()));
        Ok(())
    }

    fn start(&self, _client: &str, request: Request) -> Result<Running, Error> {
        let args: Vec<String> = request
            .args
            .iter()
            .map(|a| String::from_utf8(a.clone()).unwrap())
            .collect();
        let mut child = Command::new(&args[0])
            .args(&args[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::new(e.to_string()))?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let pid = child.id();
        Ok(Running {
            stdin: Box::new(stdin),
            stdout: Box::new(stdout),
            stderr: Box::new(stderr),
            wait: Box::new(move || child.wait().map(|s| s.code().unwrap_or(128)).unwrap_or(1)),
            hangup: Box::new(move || {
                let _ = Command::new("kill").args(["-HUP", &pid.to_string()]).status();
            }),
        })
    }
}

/// A daemon serving `machine` on loopback, on a thread of its own: its address, its key's
/// fingerprint and its stopper.
fn daemon<'scope>(
    scope: &'scope std::thread::Scope<'scope, '_>,
    identity: &'static Identity,
    machine: Machine,
) -> (SocketAddr, Stopper) {
    let (tx, rx) = mpsc::channel();
    scope.spawn(move || {
        server::serve(
            identity,
            "127.0.0.1:0".parse().unwrap(),
            machine,
            |at, stopper| {
                tx.send((at, stopper)).unwrap();
            },
        )
        .unwrap();
    });
    rx.recv().unwrap()
}

/// What a writer the command's output goes to holds, once the command has ended.
#[derive(Clone, Default)]
struct Captured(std::sync::Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().clone()
    }
}

fn run(
    identity: &Identity,
    remote: Remote,
    args: &[&str],
    stdin: Option<Vec<u8>>,
) -> (Result<i32, String>, Vec<u8>, Vec<u8>) {
    let (out, err) = (Captured::default(), Captured::default());
    let request = Request {
        args: args.iter().map(|a| a.as_bytes().to_vec()).collect(),
        env: Vec::new(),
    };
    let stdin: Option<Box<dyn std::io::Read + Send>> = stdin.map(|b| Box::new(std::io::Cursor::new(b)) as _);
    let status = client::exec(identity, remote, request, stdin, out.clone(), err.clone()).unwrap();
    (status, out.bytes(), err.bytes())
}

#[test]
fn a_client_joins_then_runs_commands_whose_output_crosses_whole() {
    let daemon_id: &'static Identity = Box::leak(Box::new(Identity::generate().unwrap()));
    let laptop = Identity::generate().unwrap();
    let stranger = Identity::generate().unwrap();
    std::thread::scope(|scope| {
        let machine = Machine {
            clients: Mutex::new(Vec::new()),
            invitations: Mutex::new(Vec::new()),
        };
        // The invitation is made before the daemon serves: the address is filled in after.
        let pending =
            Invitation::new("laptop", "127.0.0.1:1".parse().unwrap(), daemon_id.fingerprint()).unwrap();
        machine.invitations.lock().unwrap().push(pending.clone());
        let (at, stopper) = daemon(scope, daemon_id, machine);
        let invitation = Invitation {
            address: at,
            ..pending
        };
        let remote = Remote {
            address: at,
            daemon: daemon_id.fingerprint(),
        };

        // Not enrolled: refused.
        let (status, _, _) = run(&laptop, remote, &["true"], None);
        assert_eq!(status, Err(server::NOT_ENROLLED.to_string()));
        // Another secret: refused, in the same words as any join that fails.
        let forged = Invitation {
            secret: [9; 32],
            ..invitation.clone()
        };
        assert_eq!(
            client::join(&stranger, &forged).unwrap(),
            Err(server::NOT_INVITED.to_string())
        );
        // The invitation: enrolled, once.
        assert_eq!(client::join(&laptop, &invitation).unwrap(), Ok(()));
        assert_eq!(
            client::join(&stranger, &invitation).unwrap(),
            Err(server::NOT_INVITED.to_string())
        );

        let (status, out, err) = run(
            &laptop,
            remote,
            &["sh", "-c", "echo out; echo err >&2; exit 3"],
            None,
        );
        assert_eq!(status, Ok(3));
        assert_eq!(out, b"out\n");
        assert_eq!(err, b"err\n");
        let (status, out, _) = run(&laptop, remote, &["cat"], Some(b"hello over quic".to_vec()));
        assert_eq!(status, Ok(0));
        assert_eq!(out, b"hello over quic");
        // Eight MiB, more than a stream's window: it crosses whole.
        let (status, out, _) = run(&laptop, remote, &["sh", "-c", "head -c 8388608 /dev/zero"], None);
        assert_eq!(status, Ok(0));
        assert_eq!(out.len(), 8 << 20);
        assert!(out.iter().all(|b| *b == 0));
        // Still not enrolled.
        let (status, _, _) = run(&stranger, remote, &["true"], None);
        assert_eq!(status, Err(server::NOT_ENROLLED.to_string()));
        stopper.stop().unwrap();
        let _ = std::io::stderr().flush();
    });
}
