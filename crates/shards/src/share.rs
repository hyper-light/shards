//! `shards share`: a run's share process (D38), which serves the directories its microVM
//! shares with its guest. The VM process cannot: it is confined to its own files (D30) from
//! before it is given a run, and Landlock (Linux) and App Sandbox (macOS) let it reach no
//! directory given later. The daemon starts one for each run that shares any, with its
//! connection to the run's VM as descriptor 3 and each share's directory as 4 + i, and each
//! share's mode (`ro` or `rw`) and the one name of it shared (empty for all of it) as
//! arguments. It sends the VM a connection for each share (`kind::SHARE_ENDS`), answers
//! each on a thread of its own, and exits once they have all closed, as its VM ends.

use std::ffi::{CString, OsString};
use std::io::Write as _;
use std::os::fd::{FromRawFd as _, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::net::UnixStream;
use std::process::ExitCode;

use shards_vmm::devices::virtio::fs::{answer, server::Server};

/// A share: its mode and its one name, if it is a file alone.
struct Given {
    read_only: bool,
    only: Option<CString>,
}

/// The arguments after `share`: `MODE NAME` for each share.
fn parse(args: &[OsString]) -> Result<Vec<Given>, String> {
    if !args.len().is_multiple_of(2) {
        return Err("share: each share takes a mode and a name".into());
    }
    args.chunks(2)
        .map(|pair| {
            let (mode, name) = match pair {
                [mode, name] => (mode, name),
                _ => return Err("share: each share takes a mode and a name".into()),
            };
            let read_only = match mode.to_str() {
                Some("ro") => true,
                Some("rw") => false,
                _ => return Err(format!("share: mode {mode:?}: ro or rw")),
            };
            let only = match name.as_bytes() {
                [] => None,
                bytes if bytes.contains(&b'/') => {
                    return Err(format!("share: name {name:?} is not one name"));
                }
                bytes => Some(CString::new(bytes).map_err(|_| format!("share: name {name:?}"))?),
            };
            Ok(Given { read_only, only })
        })
        .collect()
}

/// Sends the VM `ends`, as many a message as one carries, and waits for it to say it has
/// them.
fn hand_over(vm: &UnixStream, ends: &[UnixStream]) -> Result<(), String> {
    use shards_ipc::kind;
    use std::os::fd::AsFd;
    let fds: Vec<_> = ends.iter().map(AsFd::as_fd).collect();
    for batch in fds.chunks(shards_ipc::MAX_FDS) {
        shards_ipc::send(vm, kind::SHARE_ENDS, &[], batch).map_err(|e| e.to_string())?;
    }
    match shards_ipc::recv(vm) {
        Ok(Some(m)) if m.kind == kind::TAKEN => Ok(()),
        Ok(Some(m)) => Err(format!("it said message kind {} instead of TAKEN", m.kind)),
        Ok(None) => Err("it ended first".into()),
        Err(e) => Err(e.to_string()),
    }
}

/// `shards share MODE NAME…`: not for people to run.
pub fn share(args: impl Iterator<Item = OsString>) -> ExitCode {
    let failed = |e: &str| {
        let _ = writeln!(std::io::stderr(), "shards: {e}");
        ExitCode::FAILURE
    };
    let given = match parse(&args.collect::<Vec<_>>()) {
        Ok(given) => given,
        Err(e) => return failed(&e),
    };
    let is = |fd: i32, want: libc::mode_t| {
        // SAFETY: fstat(2) into a zeroed stat buffer; any descriptor number is safe to ask about.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        unsafe { libc::fstat(fd, &mut st) == 0 && st.st_mode & libc::S_IFMT == want }
    };
    if !is(3, libc::S_IFSOCK) {
        return failed("share: descriptor 3 is not the VM's connection");
    }
    // SAFETY: the connection the daemon left at descriptor 3 for this process alone.
    let vm = unsafe { UnixStream::from_raw_fd(3) };
    // As far as its limit may go, as the daemon raises its own; what its servers may hold of
    // it is reckoned once all it holds besides is open (`fs::server::Limits`).
    if let Err(e) = shards_vmm::platform::raise_descriptor_limit() {
        let _ = writeln!(
            std::io::stderr(),
            "shards: share: raising the descriptor limit: {e}"
        );
    }
    let (mut dirs, mut conns, mut theirs) = (Vec::new(), Vec::new(), Vec::new());
    for (i, g) in given.into_iter().enumerate() {
        let fd = 4 + i as i32;
        if !is(fd, libc::S_IFDIR) {
            return failed(&format!("share {i}: descriptor {fd} is not a directory"));
        }
        // SAFETY: a directory the daemon left for this process alone, checked above.
        dirs.push((unsafe { OwnedFd::from_raw_fd(fd) }, g));
        let (ours, vms) = match UnixStream::pair() {
            Ok(pair) => pair,
            Err(e) => return failed(&format!("share {i}: a connection: {e}")),
        };
        conns.push(ours);
        theirs.push(vms);
    }
    let Some(limits) = shards_vmm::devices::virtio::fs::server::Limits::now() else {
        return failed("share: this process's limit on descriptors is unknown");
    };
    let budget = limits.budget(u64::try_from(dirs.len()).unwrap_or(u64::MAX));
    let mut servers = Vec::with_capacity(dirs.len());
    for (i, (dir, g)) in dirs.into_iter().enumerate() {
        match Server::with_budget(dir, g.read_only, g.only, budget) {
            Ok(server) => servers.push(server),
            Err(e) => return failed(&format!("share {i}: {e}")),
        }
    }
    // The VM's ends, kept until it says it has them all.
    if let Err(e) = hand_over(&vm, &theirs) {
        return failed(&format!("share: the VM's connections: {e}"));
    }
    drop((theirs, vm));
    let mut status = ExitCode::SUCCESS;
    std::thread::scope(|scope| {
        let mut threads = Vec::new();
        for (i, (server, conn)) in servers.iter().zip(conns).enumerate() {
            match std::thread::Builder::new()
                .name(format!("share {i}"))
                .spawn_scoped(scope, move || answer(server, conn))
            {
                Ok(t) => threads.push(t),
                Err(e) => {
                    let _ = writeln!(std::io::stderr(), "shards: share {i}: {e}");
                    status = ExitCode::FAILURE;
                }
            }
        }
        for t in threads {
            if !matches!(t.join(), Ok(Ok(()))) {
                status = ExitCode::FAILURE;
            }
        }
    });
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shares_are_read_as_given() {
        let args: Vec<OsString> = ["ro", "", "rw", "file.txt"].iter().map(OsString::from).collect();
        let given = parse(&args).unwrap();
        assert!(given[0].read_only && given[0].only.is_none());
        assert_eq!(given[1].only.as_deref(), Some(c"file.txt"));
        assert!(parse(&args[..3]).is_err());
        let bad: Vec<OsString> = ["rw", "a/b"].iter().map(OsString::from).collect();
        assert!(parse(&bad).is_err());
    }
}
