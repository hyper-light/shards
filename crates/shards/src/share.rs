//! `shards share`: a run's share process (D38), which serves the directories its microVM
//! shares with its guest. The VM process cannot: it is confined to its own files (D30) from
//! before it is given a run, and Landlock (Linux) and App Sandbox (macOS) let it reach no
//! directory given later. The daemon starts one for each run that shares any, with its
//! connection to the run's VM as descriptor 3, each share's directory as 4 + i, and after
//! them the home's private directory, where a run restored from a save keeps the files its
//! guest held that no path reaches (D176); and each share's mode (`ro` or `rw`) and the one
//! name of it shared (empty for all of it) as arguments. It sends the VM a connection for each share (`kind::SHARE_ENDS`), answers
//! each on a thread of its own, and exits once they have all closed, as its VM ends.
//!
//! `shards share --join` serves a microVM's join share instead (D119): the connection its
//! VM asks it on as descriptor 3, the daemon's as descriptor 4, an empty directory, the
//! share's root, as descriptor 5, and the home's private directory as 6. Its volumes are the directories of the containers joining
//! the microVM's network: the daemon gives it a link for each joiner (`kind::JOIN_LINK`),
//! on which its volumes come (`kind::JOIN_VOLUME`), served until the link closes as the
//! joiner ends. It exits as its VM's connection closes.
//!
//! On Windows (D197) the daemon makes each share's stream, a duplex pipe, itself, and hands
//! its VM the other end with the run: a stream a process is given hands no handle on
//! (shards_ipc's `Stream::given`). This process is made in its microVM's group's job where
//! it has one, as its VM process is, and given each share's directory as 4 + i, the home's
//! private directory as 4 + n and its end of each share's stream as 5 + n + i, by number as
//! the daemon's children on Windows take theirs (`shards_ipc::given_fd`).

use std::collections::HashMap;
use std::ffi::{CString, OsString};
use std::io::Write as _;
#[cfg(unix)]
use std::os::fd::{FromRawFd as _, OwnedFd};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::sync::Mutex;

/// What a join share's daemon and each of its joiners reach it on (D119): a Unix socket on
/// Unix; on Windows a stream it was given, which hands it what comes on it (D195).
#[cfg(unix)]
type Link = UnixStream;
#[cfg(windows)]
type Link = shards_ipc::Stream;

use shards_vmm::devices::virtio::fs::{answer, headroom::Headroom, server::Server};

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
            let only = match bytes_of(name)?.as_slice() {
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

/// `name`'s bytes, as a share's one name holds them: on Windows its UTF-8.
fn bytes_of(name: &std::ffi::OsStr) -> Result<Vec<u8>, String> {
    #[cfg(unix)]
    return Ok(std::os::unix::ffi::OsStrExt::as_bytes(name).to_vec());
    #[cfg(windows)]
    name.to_str()
        .map(|n| n.as_bytes().to_vec())
        .ok_or_else(|| format!("share: name {name:?} is not UTF-8"))
}

/// Sends the VM `ends`, as many a message as one carries, and waits for it to say it has
/// them.
#[cfg(unix)]
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

/// `shards share MODE NAME…` on Windows (D197): each share's directory, the home's private
/// directory and its end of each share's stream taken by number (the module's words), each
/// share served on a thread of its own until its VM's end closes, as Unix's are.
#[cfg(windows)]
pub fn share(args: impl Iterator<Item = OsString>) -> ExitCode {
    use std::os::windows::io::OwnedHandle;
    let failed = |e: &str| {
        let _ = writeln!(std::io::stderr(), "shards: {e}");
        ExitCode::FAILURE
    };
    let args: Vec<OsString> = args.collect();
    if args.first().is_some_and(|a| a == "--join") {
        return join();
    }
    let given = match parse(&args) {
        Ok(given) => given,
        Err(e) => return failed(&e),
    };
    let take = |at: usize, what: &str| -> Result<OwnedHandle, String> {
        let at = i32::try_from(at).map_err(|_| format!("share: {what}: too many shares"))?;
        shards_ipc::given_fd(at)
            .map_err(|e| format!("share: {what}: {e}"))?
            .ok_or_else(|| format!("share: {what} was not given"))
    };
    let n = given.len();
    let ghosts = match take(4 + n, "the home's private directory") {
        Ok(ghosts) => ghosts,
        Err(e) => return failed(&e),
    };
    let Some(limits) = shards_vmm::devices::virtio::fs::server::Limits::now() else {
        return failed("share: this process's limit on handles is unknown");
    };
    let budget = limits.budget(u64::try_from(n).unwrap_or(u64::MAX));
    let mut served = Vec::with_capacity(n);
    for (i, g) in given.into_iter().enumerate() {
        let made = (|| -> Result<_, String> {
            let dir = take(4 + i, &format!("share {i}'s directory"))?;
            let pipe = take(5 + n + i, &format!("share {i}'s stream"))?;
            let conn = shards_vmm::platform::PipeStream::from_handle(pipe)
                .map_err(|e| format!("share {i}'s stream: {e}"))?;
            let server = Server::with_budget(dir, g.read_only, g.only, budget)
                .map_err(|e| format!("share {i}: {e}"))?;
            server.keep_ghosts_in(
                ghosts
                    .try_clone()
                    .map_err(|e| format!("share {i}: the home's private directory: {e}"))?,
            );
            if let Ok(headroom) = Headroom::of_this_process() {
                server.drop_reads_past(headroom);
            }
            Ok((server, conn))
        })();
        match made {
            Ok(share) => served.push(share),
            Err(e) => return failed(&e),
        }
    }
    let (servers, conns): (Vec<Server>, Vec<_>) = served.into_iter().unzip();
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

/// `shards share MODE NAME…`, or `shards share --join`: not for people to run.
#[cfg(unix)]
pub fn share(args: impl Iterator<Item = OsString>) -> ExitCode {
    let failed = |e: &str| {
        let _ = writeln!(std::io::stderr(), "shards: {e}");
        ExitCode::FAILURE
    };
    let args: Vec<OsString> = args.collect();
    if args.first().is_some_and(|a| a == "--join") {
        return join();
    }
    let given = match parse(&args) {
        Ok(given) => given,
        Err(e) => return failed(&e),
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
    let ghosts = 4 + i32::try_from(given.len()).unwrap_or(i32::MAX);
    if !is(ghosts, libc::S_IFDIR) {
        return failed(&format!(
            "share: descriptor {ghosts} is not the home's private directory"
        ));
    }
    // SAFETY: the directory the daemon left for this process alone, checked above.
    let ghosts = unsafe { OwnedFd::from_raw_fd(ghosts) };
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
    // The budgets over this process's cgroups, which each share drops its host's copy of
    // what its guest reads past (D188): opened before the limit on descriptors is reckoned,
    // as all it holds besides its servers' is.
    let mut headrooms: Vec<Option<Headroom>> = Vec::with_capacity(dirs.len());
    for _ in &dirs {
        match Headroom::of_this_process() {
            Ok(headroom) => headrooms.push(Some(headroom)),
            Err(e) => {
                #[cfg(target_os = "linux")]
                let _ = writeln!(
                    std::io::stderr(),
                    "shards: share: its host keeps its copy of every read: {e}"
                );
                #[cfg(not(target_os = "linux"))]
                let _ = e;
                headrooms.push(None);
            }
        }
    }
    let Some(limits) = shards_vmm::devices::virtio::fs::server::Limits::now() else {
        return failed("share: this process's limit on descriptors is unknown");
    };
    let budget = limits.budget(u64::try_from(dirs.len()).unwrap_or(u64::MAX));
    let mut servers = Vec::with_capacity(dirs.len());
    for (i, ((dir, g), headroom)) in dirs.into_iter().zip(headrooms).enumerate() {
        let kept = match ghosts.try_clone() {
            Ok(kept) => kept,
            Err(e) => return failed(&format!("share {i}: the home's private directory: {e}")),
        };
        match Server::with_budget(dir, g.read_only, g.only, budget) {
            Ok(server) => {
                server.keep_ghosts_in(kept);
                if let Some(headroom) = headroom {
                    server.drop_reads_past(headroom);
                }
                servers.push(server);
            }
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

/// Whether descriptor `fd` is open, and of file type `want`.
#[cfg(unix)]
fn is(fd: i32, want: libc::mode_t) -> bool {
    // SAFETY: fstat(2) into a zeroed stat buffer; any descriptor number is safe to ask about.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    unsafe { libc::fstat(fd, &mut st) == 0 && st.st_mode & libc::S_IFMT == want }
}

/// `shards share --join` (D119): see the module's words.
fn join() -> ExitCode {
    let failed = |e: &str| {
        let _ = writeln!(std::io::stderr(), "shards: share --join: {e}");
        ExitCode::FAILURE
    };
    #[cfg(unix)]
    let (conn, daemon, root, ghosts) = {
        if !is(3, libc::S_IFSOCK) || !is(4, libc::S_IFSOCK) || !is(5, libc::S_IFDIR) || !is(6, libc::S_IFDIR)
        {
            return failed(
                "descriptors 3 to 6 are not its VM's connection, the daemon's, its root and the home's private directory",
            );
        }
        // SAFETY: the descriptors the daemon left for this process alone, checked above.
        unsafe {
            (
                UnixStream::from_raw_fd(3),
                UnixStream::from_raw_fd(4),
                OwnedFd::from_raw_fd(5),
                OwnedFd::from_raw_fd(6),
            )
        }
    };
    // On Windows the same, given by number: its end of its VM's stream, a duplex pipe whose
    // other end the daemon handed the VM (D197); the daemon's stream; its root and the home's
    // private directory.
    #[cfg(windows)]
    let (conn, daemon, root, ghosts) = {
        let take = |at: i32, what: &str| -> Result<std::os::windows::io::OwnedHandle, String> {
            shards_ipc::given_fd(at)
                .map_err(|e| format!("{what}: {e}"))?
                .ok_or_else(|| format!("{what} was not given"))
        };
        let taken = (|| -> Result<_, String> {
            let conn = shards_vmm::platform::PipeStream::from_handle(take(3, "its VM's stream")?)
                .map_err(|e| format!("its VM's stream: {e}"))?;
            let daemon = shards_ipc::Stream::given(take(4, "the daemon's stream")?)
                .map_err(|e| format!("the daemon's stream: {e}"))?;
            Ok((
                conn,
                daemon,
                take(5, "its root")?,
                take(6, "the home's private directory")?,
            ))
        })();
        match taken {
            Ok(taken) => taken,
            Err(e) => return failed(&e),
        }
    };
    if let Err(e) = shards_vmm::platform::raise_descriptor_limit() {
        let _ = writeln!(
            std::io::stderr(),
            "shards: share --join: raising the descriptor limit: {e}"
        );
    }
    // The budgets over this process's cgroups, which it drops its host's copy of what its
    // guest reads past, as a share does (D188): opened before the limit on descriptors is
    // reckoned, as all it holds besides its server's is.
    let headroom = Headroom::of_this_process();
    let Some(limits) = shards_vmm::devices::virtio::fs::server::Limits::now() else {
        return failed("this process's limit on descriptors is unknown");
    };
    let server = match Server::joined(root, limits.budget(1)) {
        Ok(server) => server,
        Err(e) => return failed(&e),
    };
    server.keep_ghosts_in(ghosts);
    match headroom {
        Ok(headroom) => server.drop_reads_past(headroom),
        Err(e) => {
            #[cfg(target_os = "linux")]
            let _ = writeln!(
                std::io::stderr(),
                "shards: share --join: its host keeps its copy of every read: {e}"
            );
            #[cfg(not(target_os = "linux"))]
            let _ = e;
        }
    }
    // Each joiner's link while it is served, by number: shut as the VM goes.
    let links: Mutex<HashMap<u64, Link>> = Mutex::new(HashMap::new());
    std::thread::scope(|scope| {
        let (server, links, from) = (&server, &links, &daemon);
        let linked = std::thread::Builder::new()
            .name("join links".into())
            .spawn_scoped(scope, move || {
                let mut next = 0u64;
                while let Ok(Some(m)) = shards_ipc::recv(from) {
                    if m.kind != shards_ipc::kind::JOIN_LINK {
                        continue;
                    }
                    for fd in m.fds {
                        #[cfg(unix)]
                        let link = Link::from(fd);
                        #[cfg(windows)]
                        let Ok(link) = Link::given(fd) else {
                            let _ = writeln!(std::io::stderr(), "shards: share --join: a joiner's link");
                            continue;
                        };
                        let n = next;
                        next += 1;
                        if let Ok(held) = link.try_clone() {
                            lock(links).insert(n, held);
                        }
                        let served = std::thread::Builder::new().name("joiner".into()).spawn_scoped(
                            scope,
                            move || {
                                joiner(server, &link);
                                lock(links).remove(&n);
                            },
                        );
                        // Its link closed unanswered: the daemon hears its joiner's volumes
                        // were not taken.
                        if let Err(e) = served {
                            let _ = writeln!(std::io::stderr(), "shards: share --join: a joiner: {e}");
                            lock(links).remove(&n);
                        }
                    }
                }
            });
        let answered = answer(server, conn);
        // Its VM gone, so are its joiners: the daemon's links let go of, once no more come.
        shards_ipc::end(&daemon);
        if let Ok(t) = linked {
            let _ = t.join();
        }
        for link in lock(links).values() {
            shards_ipc::end(link);
        }
        match answered {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => failed(&format!("its VM's connection: {e}")),
        }
    })
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Serves a joiner's `link`: each volume it brings added and answered, and all taken out
/// again as it closes, its joiner gone.
fn joiner(server: &Server, link: &Link) {
    use shards_ipc::kind;
    let mut added: Vec<CString> = Vec::new();
    while let Ok(Some(m)) = shards_ipc::recv(link) {
        if m.kind != kind::JOIN_VOLUME {
            continue;
        }
        let mut fds = m.fds.into_iter();
        let volume = (|| -> Result<CString, String> {
            let mut parts = m.payload.split(|&b| b == 0);
            let (Some(name), Some(mode), Some(only), None) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                return Err("a malformed volume".into());
            };
            let dir = fds.next().ok_or("a volume without its directory")?;
            let read_only = match mode {
                b"ro" => true,
                b"rw" => false,
                _ => return Err("a volume's mode: ro or rw".into()),
            };
            let name = CString::new(name).map_err(|_| "a volume's name")?;
            let only = if only.is_empty() {
                None
            } else {
                Some(CString::new(only).map_err(|_| "a volume's one name")?)
            };
            server.add(name.clone(), dir, read_only, only)?;
            Ok(name)
        })();
        let _ = match volume {
            Ok(name) => {
                added.push(name);
                shards_ipc::send(link, kind::TAKEN, &[], &[])
            }
            Err(e) => shards_ipc::send(link, kind::ERR, e.as_bytes(), &[]),
        };
    }
    for name in &added {
        server.remove(name);
    }
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

    /// A joiner's volumes are served while its link is open, a malformed one refused and
    /// said why, and all go as the link closes, its joiner gone (D119).
    #[cfg(unix)]
    #[test]
    fn a_joiners_volumes_go_as_its_link_closes() {
        use shards_ipc::kind;
        use std::os::fd::AsFd as _;
        let at_dir = shards_testdir::TempDir::new("share-joiner").unwrap();
        let at = at_dir.join("share-joiner");
        let (root, vol) = (at.join("root"), at.join("vol"));
        for dir in [&root, &vol] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let open = |p: &std::path::Path| -> OwnedFd { std::fs::File::open(p).unwrap().into() };
        let server = Server::joined(open(&root), 64).unwrap();
        let (daemon, link) = UnixStream::pair().unwrap();
        let taken = |name: &str| {
            server
                .add(CString::new(name).unwrap(), open(&vol), false, None)
                .is_err()
        };
        std::thread::scope(|s| {
            let served = s.spawn(|| joiner(&server, &link));
            let dir = open(&vol);
            shards_ipc::send(&daemon, kind::JOIN_VOLUME, b"v\0rw\0", &[dir.as_fd()]).unwrap();
            assert_eq!(shards_ipc::recv(&daemon).unwrap().unwrap().kind, kind::TAKEN);
            shards_ipc::send(&daemon, kind::JOIN_VOLUME, b"w\0rx\0", &[dir.as_fd()]).unwrap();
            let refused = shards_ipc::recv(&daemon).unwrap().unwrap();
            assert_eq!(
                (refused.kind, refused.payload.as_slice()),
                (kind::ERR, &b"a volume's mode: ro or rw"[..])
            );
            assert!(taken("v"), "served while its link is open");
            assert!(!taken("w"));
            server.remove(c"w");
            drop(daemon);
            served.join().unwrap();
            assert!(!taken("v"), "gone as its link closed");
        });
        let _ = std::fs::remove_dir_all(&at);
    }
}
