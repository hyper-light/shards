//! The daemon's listener for other machines' clients (D180): the clients this home enrolled
//! run shards commands here whole, each as this machine's own shards, started by the spawner
//! as every child of the daemon is (PM M158), and each said in the daemon's log; a client not
//! enrolled may only join, by an invitation this home made.

use std::ffi::{OsStr, OsString};
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::sync::Arc;

use shards_remote::frame::Request;
use shards_remote::server::{self, Host, Running};
use shards_remote::{Error, identity, invite};

/// This machine, as its daemon's clients reach it.
pub(super) struct Machine {
    pub home: PathBuf,
    pub exe: PathBuf,
}

/// Why a client may not run `args` here: a context's own commands, the daemon's, and what the
/// shards binary runs only as a part of itself, are the machine's owner's.
fn owners(args: &[String]) -> Option<String> {
    let said = shards_cmdline::grammar::rewrite(args).unwrap_or_else(|| args.to_vec());
    let first = said.first().map(String::as_str).unwrap_or_default();
    matches!(
        first,
        "context"
            | "client"
            | "invitation"
            | "daemon"
            | "spawner"
            | "grants"
            | "share"
            | "frontend"
            | "guest"
    )
    .then(|| format!("{first}: this machine's owner's to run, not a client's"))
}

impl Host for Machine {
    fn enrolled(&self, spki: &[u8]) -> Option<String> {
        crate::context::clients(&self.home)
            .into_iter()
            .find(|c| c.key == spki)
            .map(|c| c.name)
    }

    fn join(&self, name: &str, proof: &[u8], exported: &[u8; 32], spki: &[u8]) -> Result<(), Error> {
        let refused = || Error::new(server::NOT_INVITED);
        let (invitation, created) = crate::context::invitation(&self.home, name).ok_or_else(refused)?;
        if crate::context::lapsed(created) || !invite::verify(&invitation.secret, exported, proof) {
            return Err(refused());
        }
        if self.enrolled(spki).is_some() {
            return Err(refused());
        }
        // Redeemed first: of two joins at once, one enrolls.
        crate::context::redeem(&self.home, name).map_err(|_| refused())?;
        crate::context::enroll(&self.home, name, spki).map_err(|_| refused())?;
        let key = identity::hex(&identity::fingerprint(spki));
        super::log(format!(
            "client {name} joined, its key {}",
            key.get(..12).unwrap_or_default()
        ));
        Ok(())
    }

    fn start(&self, client: &str, request: Request) -> Result<Running, Error> {
        // A client removed since it connected runs nothing more.
        if !crate::context::clients(&self.home)
            .iter()
            .any(|c| c.name == client)
        {
            return Err(Error::new(server::NOT_ENROLLED));
        }
        let args = request
            .args
            .into_iter()
            .map(|a| String::from_utf8(a).map_err(|_| Error::new("an argument that is not UTF-8")))
            .collect::<Result<Vec<String>, Error>>()?;
        if let Some(why) = owners(&args) {
            return Err(Error::new(why));
        }
        super::log(format!("client {client}: shards {}", args.join(" ")));
        let pipe = || std::io::pipe().map_err(|e| Error::new(format!("a pipe for the command: {e}")));
        let (stdin_r, stdin_w) = pipe()?;
        let (stdout_r, stdout_w) = pipe()?;
        let (stderr_r, stderr_w) = pipe()?;
        let env: Vec<(String, OsString)> = request
            .env
            .into_iter()
            .filter_map(|(k, v)| String::from_utf8(v).ok().map(|v| (k, OsString::from(v))))
            .collect();
        // This machine's own shards, whatever context its owner's commands use.
        let mut set: Vec<(&str, &OsStr)> = vec![
            ("SHARDS_HOME", self.home.as_os_str()),
            ("SHARDS_CONTEXT", OsStr::new(crate::context::DEFAULT)),
        ];
        set.extend(env.iter().map(|(k, v)| (k.as_str(), v.as_os_str())));
        let argv: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
        let fds = [(stdin_r.as_fd(), 0), (stdout_w.as_fd(), 1), (stderr_w.as_fd(), 2)];
        let child = shards_ipc::spawn_with(&self.exe, &argv, &fds, false, &set)
            .map_err(|e| Error::new(format!("starting the command: {e}")))?;
        // The child holds its ends.
        drop((stdin_r, stdout_w, stderr_w));
        // Waited for on one thread, hung up from another: kill refuses a child reaped.
        let child = Arc::new(child);
        let hung = Arc::clone(&child);
        Ok(Running {
            stdin: Box::new(stdin_w),
            stdout: Box::new(stdout_r),
            stderr: Box::new(stderr_r),
            wait: Box::new(move || child.wait().unwrap_or(1)),
            hangup: Box::new(move || {
                let _ = hung.kill(libc::SIGHUP);
            }),
        })
    }

    fn invited(&self) -> usize {
        crate::context::open_invitations(&self.home)
    }
}

/// Serves other machines' clients on `at`, on a thread of its own for the daemon's life: once
/// it is bound, its address is in the home (`listening`), where `shards invite client` finds
/// it, and in the daemon's log. An address it cannot bind is an error, before the daemon
/// says it serves.
pub(super) fn listen(
    home: PathBuf,
    exe: PathBuf,
    at: std::net::SocketAddr,
) -> Result<std::net::SocketAddr, String> {
    let identity = crate::context::identity(&home)?;
    let (tx, rx) = std::sync::mpsc::channel::<Result<std::net::SocketAddr, String>>();
    let ended = tx.clone();
    let machine = Machine {
        home: home.clone(),
        exe,
    };
    std::thread::Builder::new()
        .name("remote".into())
        .spawn(move || {
            let served = server::serve(&identity, at, machine, |bound, _stopper| {
                // The daemon ends with its process, the listener with it: it is not stopped.
                let _ = tx.send(Ok(bound));
            });
            if let Err(e) = served {
                super::log(format!("other machines' clients on {at}: {e}"));
                let _ = ended.send(Err(e.to_string()));
            }
        })
        .map_err(|e| format!("a thread for other machines' clients: {e}"))?;
    let bound = rx
        .recv()
        .map_err(|_| format!("listening on {at}: the listener ended before it bound"))?
        .map_err(|e| format!("listening on {at}: {e}"))?;
    let path = home.join(crate::context::LISTENING);
    std::fs::write(&path, format!("{bound}\n")).map_err(|e| format!("{}: {e}", path.display()))?;
    let key = identity::hex(&crate::context::identity(&home)?.fingerprint());
    super::log(format!(
        "serving the machines it enrolled on {bound} (QUIC), its key {}",
        key.get(..12).unwrap_or_default()
    ));
    Ok(bound)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A client runs shards' commands, not the machine's owner's.
    #[test]
    fn a_client_may_not_run_its_hosts_own_commands() {
        let args = |s: &str| s.split(' ').map(str::to_string).collect::<Vec<_>>();
        assert!(owners(&args("ps -a")).is_none());
        assert!(owners(&args("run --rm alpine echo hi")).is_none());
        for owner in [
            "context ls",
            "list contexts",
            "invite client --name x --output y",
            "list clients",
            "stop daemon",
            "daemon stop",
            "configure guest --kernel k --init i",
            "spawner",
        ] {
            assert!(owners(&args(owner)).is_some(), "{owner}");
        }
    }
}
