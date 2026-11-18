//! A spawner's half of a VM's grants (`grant`): it answers what the VM asks for with
//! descriptors, and with bookmarks for the directories it writes in (macOS); with handles it
//! puts in the VM's process, its vsock's pipes among them (Windows, D272). On Linux, whose VM Landlock confines
//! to its files, a VM given a vsock path asks for that alone, and for the connections it
//! dials beside it (D30).

use std::path::{Path, PathBuf};

use crate::grant::{Access, MAX_GRANTS, MAX_PATH, Wanted};

/// What [`encode`] wrote; `None` for anything else, past the limits included.
pub fn decode(mut bytes: &[u8]) -> Option<Vec<Wanted>> {
    // `take` borrows `bytes` until the block ends; what is left must be nothing.
    let out = {
        let mut take = |n: usize| {
            let (head, rest) = bytes.split_at_checked(n)?;
            bytes = rest;
            Some(head)
        };
        let count = u32::from_be_bytes(take(4)?.try_into().ok()?) as usize;
        if count > MAX_GRANTS {
            return None;
        }
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            let access = access(*take(1)?.first()?)?;
            let len = u32::from_be_bytes(take(4)?.try_into().ok()?) as usize;
            if len > MAX_PATH {
                return None;
            }
            let path = crate::grant::path_from(take(len)?)?;
            if !path.is_absolute() {
                return None;
            }
            out.push((access, path));
        }
        out
    };
    bytes.is_empty().then_some(out)
}

/// The newest version of the file at `path` (platform `write_in`), opened for reading, or
/// `None` where it has none.
#[cfg(any(target_os = "macos", windows))]
fn newest(path: &Path) -> std::io::Result<Option<std::fs::File>> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "no file named",
        ));
    };
    let opened = shards_vmm::platform::open_dir(parent)
        .and_then(|dir| shards_vmm::platform::open_newest_in(&dir, name));
    match opened {
        Ok(file) => Ok(Some(file)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// The access `code` names, if any.
fn access(code: u8) -> Option<Access> {
    match code {
        0 => Some(Access::Read),
        1 => Some(Access::ReadIfThere),
        5 => Some(Access::ReadNewestIfThere),
        2 => Some(Access::Write),
        3 => Some(Access::MakeDir),
        4 => Some(Access::Listen),
        6 => Some(Access::Keep),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
/// A read-write bookmark for directory `path`, for another process to resolve: made by
/// CoreFoundation, bound when first needed (shards_apple).
pub fn bookmark(path: &Path) -> Result<Vec<u8>, String> {
    shards_apple::bookmark::bookmark(path)
}

#[cfg(unix)]
/// What is granted for `path` as `access`: a file opened for it, a directory's bookmark,
/// nothing, where a file to read if it is there is not (macOS), or a socket bound there.
enum Answer {
    #[cfg(target_os = "macos")]
    File(std::fs::File),
    #[cfg(target_os = "macos")]
    Bookmark(Vec<u8>),
    #[cfg(target_os = "macos")]
    Absent,
    Bound(std::os::fd::OwnedFd),
}

#[cfg(target_os = "linux")]
/// What is granted on Linux: a socket bound at a VM's vsock path, which Landlock lets it
/// make nowhere; its files are its Landlock rules' (D30).
fn answer(access: Access, path: &Path, _template: Option<&Path>) -> Result<Answer, String> {
    match access {
        Access::Listen => listen(path)
            .map(Answer::Bound)
            .map_err(|e| format!("{}: {e}", path.display())),
        _ => Err(format!(
            "{}: a VM on Linux is granted its vsock path alone, its files by Landlock",
            path.display()
        )),
    }
}

#[cfg(target_os = "macos")]
/// The names of `path` below `template`, the directory of the snapshot a restoring VM
/// reads, if it is beneath it: as given, or as its links resolve. `None` for a path
/// elsewhere; an error for one beneath it that is not plain names.
fn beneath(path: &Path, template: &Path) -> Result<Option<Vec<String>>, String> {
    let rest = match path.strip_prefix(template) {
        Ok(rest) => rest.to_path_buf(),
        Err(_) => {
            let real = std::fs::canonicalize(template).ok();
            match real.as_deref().and_then(|t| path.strip_prefix(t).ok()) {
                Some(rest) => rest.to_path_buf(),
                None => return Ok(None),
            }
        }
    };
    names(path, &rest).map(Some)
}

#[cfg(target_os = "macos")]
fn names(path: &Path, rest: &Path) -> Result<Vec<String>, String> {
    rest.components()
        .map(|c| match c {
            std::path::Component::Normal(n) => n.to_str().map(str::to_string),
            _ => None,
        })
        .collect::<Option<Vec<String>>>()
        .filter(|n| !n.is_empty())
        .ok_or_else(|| format!("{}: not a file of the template", path.display()))
}

#[cfg(target_os = "macos")]
fn answer(access: Access, path: &Path, template: Option<&Path>) -> Result<Answer, String> {
    let at = |e: std::io::Error| format!("{}: {e}", path.display());
    // A restore's template was written by a VM process, which may have left in it
    // anything it liked before it gave it up: its files are opened beneath it, never
    // through a link it holds, regular files alone, and nothing of it for writing
    // (PM M165).
    if let Some(template) = template
        && let Some(rel) = beneath(path, template)?
    {
        let rel: Vec<&str> = rel.iter().map(String::as_str).collect();
        return match access {
            Access::Read => shards_vmm::platform::open_beneath(template, &rel)
                .map(Answer::File)
                .map_err(at),
            Access::ReadIfThere => match shards_vmm::platform::open_beneath(template, &rel) {
                Ok(file) => Ok(Answer::File(file)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Answer::Absent),
                Err(e) => Err(at(e)),
            },
            Access::ReadNewestIfThere => match shards_vmm::platform::open_newest_beneath(template, &rel) {
                Ok(file) => Ok(Answer::File(file)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Answer::Absent),
                Err(e) => Err(at(e)),
            },
            _ => Err(format!(
                "{}: a restore writes nothing of its template",
                path.display()
            )),
        };
    }
    let open = |write: bool| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(write)
            .open(path)
            .map_err(at)
    };
    match access {
        Access::Read => open(false).map(Answer::File),
        Access::Write => open(true).map(Answer::File),
        Access::Keep => shards_vmm::platform::open_made(path)
            .map(Answer::File)
            .map_err(at),
        Access::ReadIfThere => match std::fs::File::open(path) {
            Ok(file) => Ok(Answer::File(file)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Answer::Absent),
            Err(e) => Err(at(e)),
        },
        Access::ReadNewestIfThere => match newest(path) {
            Ok(Some(file)) => Ok(Answer::File(file)),
            Ok(None) => Ok(Answer::Absent),
            Err(e) => Err(at(e)),
        },
        Access::MakeDir => {
            std::fs::create_dir_all(path).map_err(at)?;
            bookmark(path).map(Answer::Bookmark)
        }
        Access::Listen => listen(path).map(Answer::Bound).map_err(at),
    }
}

#[cfg(unix)]
/// A socket bound at `path` for a VM's vsock device, not yet listening: the VM listens
/// on it once its device is up. Until then a client is refused, as it is where the VM
/// binds the socket itself: listening at once would queue every attempt made while the
/// VM starts, and hand its device a backlog of connections long given up. A socket file
/// an earlier VM left there, which nothing answers on, is replaced; one in use is not.
fn listen(path: &Path) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::unix::fs::FileTypeExt as _;
    match bind(path) {
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            let stale = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_socket())
                && std::os::unix::net::UnixStream::connect(path)
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::ConnectionRefused);
            if !stale {
                return Err(e);
            }
            std::fs::remove_file(path)?;
            bind(path)
        }
        bound => bound,
    }
}

#[cfg(unix)]
/// A Unix stream socket bound at `path`, close-on-exec.
fn bind(path: &Path) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
    use std::os::unix::ffi::OsStrExt as _;
    // SAFETY: socket(2) with constant arguments.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor nothing else owns.
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: fcntl(2) on a descriptor we own.
    if unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: an all-zero sockaddr_un is a valid value.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= addr.sun_path.len() || bytes.contains(&0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "a socket path too long for sockaddr_un",
        ));
    }
    for (d, s) in addr.sun_path.iter_mut().zip(bytes) {
        *d = *s as libc::c_char;
    }
    let len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
    // SAFETY: `addr` is a valid sockaddr_un of `len` bytes.
    if unsafe { libc::bind(socket.as_raw_fd(), (&raw const addr).cast(), len) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(socket)
}

#[cfg(unix)]
/// A connection to vsock host port `port` beside `listening`, the vsock path the VM was
/// granted: the only sockets it may have dialled for it.
fn dial(listening: Option<&Path>, port: u32) -> Result<std::os::unix::net::UnixStream, String> {
    let path = listening.ok_or("a dial from a VM granted no vsock path")?;
    let mut target = path.as_os_str().to_owned();
    target.push(format!("_{port}"));
    std::os::unix::net::UnixStream::connect(&target)
        .map_err(|e| format!("{}: {e}", Path::new(&target).display()))
}

#[cfg(unix)]
/// Answers the VM on `link` as it asks, until it closes it: a bookmark for each path it
/// names, making the directories it writes in first. A path that cannot be granted ends
/// the answers with why.
pub fn serve(link: &std::os::unix::net::UnixStream, template: Option<&Path>) -> Result<(), String> {
    serve_waiting(link, template, &mut |_| Ok(()))
}

#[cfg(unix)]
/// [`serve`], calling `wait` with how many messages the VM has sent before each wait for
/// its next: a caller's watch on a VM that goes quiet (the daemon's, `grant`), which ends
/// the answers with why where it gives the VM up.
pub fn serve_waiting(
    link: &std::os::unix::net::UnixStream,
    template: Option<&Path>,
    wait: &mut dyn FnMut(usize) -> Result<(), String>,
) -> Result<(), String> {
    use shards_ipc::kind;
    use std::os::fd::AsFd as _;
    // The vsock path granted, beside which alone dials go.
    let mut listening: Option<PathBuf> = None;
    // What was sent with the last answers, held until the VM says it has it, asks again,
    // or goes: XNU's collector flushes a socket in flight that no process holds (M24), and
    // a flushed socket reads as ended. The VM asks again only once it has received them,
    // and says so of a dialled connection at once, whose far end sees no end while a copy
    // here holds it open.
    let mut held: Vec<std::os::fd::OwnedFd> = Vec::new();
    // Its vsock path granted, the last it asks for (and what any dial needs first): the VM
    // has started, its guest running. It asks for no access after that, so a request is
    // one its guest has taken it over to make, and ends the answers.
    let mut started = false;
    let mut sent = 0usize;
    loop {
        wait(sent)?;
        sent = sent.saturating_add(1);
        let asked = shards_ipc::recv(link).map_err(|e| format!("a VM's request for access: {e}"))?;
        held.clear();
        let Some(asked) = asked else {
            return Ok(());
        };
        // The VM has what was sent last, let go above.
        if asked.kind == kind::TAKEN {
            continue;
        }
        if asked.kind == kind::DIAL {
            let port = <[u8; 4]>::try_from(asked.payload.as_slice())
                .map(u32::from_be_bytes)
                .map_err(|_| "a VM sent a malformed dial".to_string())?;
            let sent = match dial(listening.as_deref(), port) {
                Ok(conn) => {
                    let sent = shards_ipc::send(link, kind::GRANTED, &[], &[conn.as_fd()]);
                    held.push(conn.into());
                    sent
                }
                Err(e) => shards_ipc::send(link, kind::ERR, e.as_bytes(), &[]),
            };
            sent.map_err(|e| format!("answering a VM's dial: {e}"))?;
            continue;
        }
        let wanted = match (asked.kind, decode(&asked.payload)) {
            (kind::GRANT, Some(wanted)) => wanted,
            _ => return Err("a VM sent a malformed request for access".into()),
        };
        if started {
            let e = "a VM asked for access after it started".to_string();
            let _ = shards_ipc::send(link, kind::ERR, e.as_bytes(), &[]);
            return Err(e);
        }
        started = wanted.iter().any(|(access, _)| *access == Access::Listen);
        for (access, path) in wanted {
            let sent = match answer(access, &path, template) {
                #[cfg(target_os = "macos")]
                Ok(Answer::File(file)) => {
                    let sent = shards_ipc::send(link, kind::GRANTED, &[], &[file.as_fd()]);
                    held.push(file.into());
                    sent
                }
                #[cfg(target_os = "macos")]
                Ok(Answer::Bookmark(b)) => shards_ipc::send(link, kind::GRANTED, &b, &[]),
                #[cfg(target_os = "macos")]
                Ok(Answer::Absent) => shards_ipc::send(link, kind::GRANTED, &[], &[]),
                Ok(Answer::Bound(socket)) => {
                    if listening.is_some() {
                        let e = "a VM asked for a second vsock path".to_string();
                        let _ = shards_ipc::send(link, kind::ERR, e.as_bytes(), &[]);
                        return Err(e);
                    }
                    listening = Some(path.clone());
                    let sent = shards_ipc::send(link, kind::GRANTED, &[], &[socket.as_fd()]);
                    held.push(socket);
                    sent
                }
                Err(e) => {
                    let _ = shards_ipc::send(link, kind::ERR, e.as_bytes(), &[]);
                    return Err(e);
                }
            };
            sent.map_err(|e| format!("granting a VM {}: {e}", path.display()))?;
        }
    }
}

#[cfg(unix)]
/// `shardsd grants`: the VM's broker (`shards run vm --kernel`, `shards restore vm`), which the CLI starts before it becomes the VM.
/// Answers the VM on descriptor 3 until it has all it needs and closes it, then exits. What
/// it could not grant, the VM is told and says.
pub fn broker() -> std::process::ExitCode {
    use std::io::Write as _;
    use std::os::fd::FromRawFd as _;
    let failed = |e: &str| {
        let _ = writeln!(std::io::stderr(), "shards: {e}");
        std::process::ExitCode::FAILURE
    };
    // SAFETY: fstat(2) into a zeroed stat buffer; any descriptor number is safe to ask about.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    if unsafe { libc::fstat(3, &mut st) } != 0 || st.st_mode & libc::S_IFMT != libc::S_IFSOCK {
        return failed("grants: descriptor 3 is not the VM's socket");
    }
    // SAFETY: the socket the CLI left at descriptor 3 for this process alone.
    let link = unsafe { std::os::unix::net::UnixStream::from_raw_fd(3) };
    // The template a restore reads (`--template DIR`), whose files are granted beneath it.
    let mut args = std::env::args_os().skip_while(|a| a != "--template").skip(1);
    let template = args.next().map(PathBuf::from);
    match serve(&link, template.as_deref()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(_) => std::process::ExitCode::FAILURE,
    }
}

/// The process a broker on Windows puts handles and sockets in: the VM it started.
#[cfg(windows)]
pub trait Recipient {
    /// `handle` duplicated into it: its value there.
    fn give(&self, handle: std::os::windows::io::BorrowedHandle<'_>) -> std::io::Result<usize>;
    /// Its handle `value`, duplicated into this process.
    fn take(&self, value: usize) -> std::io::Result<std::os::windows::io::OwnedHandle>;
}

#[cfg(windows)]
impl Recipient for shards_ipc::Child {
    fn give(&self, handle: std::os::windows::io::BorrowedHandle<'_>) -> std::io::Result<usize> {
        shards_ipc::Child::give(self, handle)
    }

    fn take(&self, value: usize) -> std::io::Result<std::os::windows::io::OwnedHandle> {
        shards_ipc::Child::take(self, value)
    }
}

#[cfg(windows)]
impl Recipient for shards_ipc::Process {
    fn give(&self, handle: std::os::windows::io::BorrowedHandle<'_>) -> std::io::Result<usize> {
        shards_ipc::Process::give(self, handle)
    }

    fn take(&self, value: usize) -> std::io::Result<std::os::windows::io::OwnedHandle> {
        shards_ipc::Process::take(self, value)
    }
}

/// What is granted for a path on Windows: a file opened for it, nothing where a file to
/// read if it is there is not, a directory its snapshots are staged in, or a vsock
/// listener's first pair of pipes, named at the path (D272).
#[cfg(windows)]
enum Handed {
    File(std::fs::File),
    Absent,
    Dir,
    Pair(
        shards_ipc::VsockNames,
        std::os::windows::io::OwnedHandle,
        std::os::windows::io::OwnedHandle,
    ),
}

/// What `access` to `path` is granted on Windows. A directory to write in is made, and its
/// snapshots are staged by this process ([`serve`]'s `STAGE`): an AppContainer writes in no
/// directory of its user's (D141, D196).
#[cfg(windows)]
fn handed(access: Access, path: &Path) -> Result<Handed, String> {
    let at = |e: std::io::Error| format!("{}: {e}", path.display());
    let open = |write: bool| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(write)
            .open(path)
            .map_err(at)
    };
    match access {
        Access::Read => open(false).map(Handed::File),
        Access::Write => open(true).map(Handed::File),
        Access::Keep => shards_vmm::platform::open_made(path)
            .map(Handed::File)
            .map_err(at),
        Access::ReadIfThere => match std::fs::File::open(path) {
            Ok(file) => Ok(Handed::File(file)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Handed::Absent),
            Err(e) => Err(at(e)),
        },
        Access::ReadNewestIfThere => match newest(path) {
            Ok(Some(file)) => Ok(Handed::File(file)),
            Ok(None) => Ok(Handed::Absent),
            Err(e) => Err(at(e)),
        },
        Access::MakeDir => {
            std::fs::create_dir_all(path).map_err(at)?;
            if !std::fs::metadata(path).map_err(at)?.is_dir() {
                return Err(format!("{}: not a directory", path.display()));
            }
            Ok(Handed::Dir)
        }
        // Its first pair, made under fresh names its file at the path says, the first
        // instances of them (D272): an AppContainer can make no pipe its user may open (PM
        // M603). Its client is let in as the VM connects them.
        Access::Listen => shards_ipc::vsock_listen(path)
            .map(|(names, down, up)| Handed::Pair(names, down, up))
            .map_err(at),
    }
}

/// A connection to vsock host port `port` beside `listening`, the vsock path the VM was
/// granted, the only ports it may have dialled for it: the client ends of a pair of pipes,
/// its way down first (D272). What listens at `<path>_<port>` is read once, by its entry's
/// handle, and decides (D272, amended 2026-11-17): a pair's file (a regular file) is opened
/// as its host program's listener named it; an AF_UNIX listener's socket (a builder's, on
/// every OS) is connected here and the connection carried ([`carried`]); anything else is
/// refused, its path named. Waited for not at all: a host port whose pair another client
/// holds refuses at once, as one whose backlog is full, so that the VM's device, which
/// waits for this answer, waits on no host program.
#[cfg(windows)]
fn dial_for(
    listening: Option<&Path>,
    port: u32,
) -> Result<
    (
        std::os::windows::io::OwnedHandle,
        std::os::windows::io::OwnedHandle,
    ),
    String,
> {
    use shards_vmm::platform::EntryKind;
    let path = listening.ok_or("a dial from a VM granted no vsock path")?;
    let mut target = path.as_os_str().to_owned();
    target.push(format!("_{port}"));
    let target = Path::new(&target);
    let at = |e: std::io::Error| format!("{}: {e}", target.display());
    match shards_vmm::platform::entry_kind(target).map_err(at)? {
        EntryKind::File => shards_ipc::vsock_open(target, Some(std::time::Instant::now())).map_err(at),
        EntryKind::UnixSocket => carried(target).map_err(at),
        EntryKind::Directory => Err(format!(
            "{}: a directory, neither a vsock pair's file nor an AF_UNIX socket",
            target.display()
        )),
        EntryKind::Reparse(tag) => Err(format!(
            "{}: a reparse point tagged {tag:#010x}, neither a vsock pair's file nor an AF_UNIX \
             socket",
            target.display()
        )),
    }
}

/// A connection to the AF_UNIX listener at `target` (D272, amended 2026-11-17): connected
/// here, before the VM is answered, so that a host takes its guest's connections in the
/// order the guest dials them, as Unix's spawner connects them (a builder's init dials its
/// give-back port before its build's, and its builder takes them in that order); carried
/// both ways between that socket and a pair of pipes made whole here
/// ([`shards_ipc::vsock_carried`]), whose client ends are the VM's, by a thread for each
/// way ([`carry_up`], [`carry_down`]), each ending with its peers.
#[cfg(windows)]
fn carried(
    target: &Path,
) -> std::io::Result<(
    std::os::windows::io::OwnedHandle,
    std::os::windows::io::OwnedHandle,
)> {
    let unix = shards_vmm::platform::UnixStream::connect(target)?;
    let to_host = unix.try_clone()?;
    let (ours, theirs) = shards_ipc::vsock_carried()?;
    let shards_ipc::VsockConn { down, up } = ours;
    // A chunk of a way's quota: one read takes what its pipe holds, one write fills it.
    let chunk = usize::try_from(shards_ipc::VSOCK_QUOTA).map_err(std::io::Error::other)?;
    std::thread::Builder::new()
        .name("carried up".into())
        .spawn(move || carry_up(&up, &to_host, chunk))?;
    std::thread::Builder::new()
        .name("carried down".into())
        .spawn(move || carry_down(&unix, down, chunk))?;
    Ok(theirs)
}

/// `chunk` bytes to carry through, none where they cannot be had.
#[cfg(windows)]
fn chunk_of(chunk: usize) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    buf.try_reserve_exact(chunk).ok()?;
    buf.resize(chunk, 0);
    Some(buf)
}

/// The guest's way: what it writes up its pipe, written to its host's socket, to its end,
/// which the socket is told by a shutdown of its writes alone, so that its host reads every
/// byte, then that end (D251's first invariant). Where the host's socket refuses them (its
/// host gone), it ends at once, its pipe let go with it, so that the guest's next write
/// fails, as a pipe's does whose host has closed.
#[cfg(windows)]
fn carry_up(up: &shards_ipc::Pipe, to_host: &shards_vmm::platform::UnixStream, chunk: usize) {
    use std::io::{Read as _, Write as _};
    let Some(mut buf) = chunk_of(chunk) else {
        let _ = to_host.shutdown(std::net::Shutdown::Write);
        return;
    };
    let mut from_guest = up;
    loop {
        let n = match from_guest.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => 0,
        };
        let Some(bytes) = buf.get(..n).filter(|b| !b.is_empty()) else {
            let _ = to_host.shutdown(std::net::Shutdown::Write);
            return;
        };
        let mut to = to_host;
        if to.write_all(bytes).is_err() {
            return;
        }
    }
}

/// The host's way: what it writes on its socket, written down the guest's pipe, to its end,
/// then the pipe let go, so that the guest reads every byte, then that end. The socket is
/// read to its end all the same once the guest's pipe refuses its bytes (the guest gone),
/// those bytes dropped: a Windows AF_UNIX socket closed while it holds its peer's bytes
/// unread resets and loses the peer what it had not read (PM M358), here the guest's bytes
/// its host had still to read. So a host's write after its guest's end is taken, not
/// refused; its host finds that end by its read.
#[cfg(windows)]
fn carry_down(from_host: &shards_vmm::platform::UnixStream, down: shards_ipc::Pipe, chunk: usize) {
    use std::io::{Read as _, Write as _};
    let Some(mut buf) = chunk_of(chunk) else {
        return;
    };
    let mut down = Some(down);
    let mut from = from_host;
    loop {
        let n = match from.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => 0,
        };
        let Some(bytes) = buf.get(..n).filter(|b| !b.is_empty()) else {
            return;
        };
        if let Some(pipe) = down.as_ref() {
            let mut to = pipe;
            if to.write_all(bytes).is_err() {
                down = None;
            }
        }
    }
}

/// `down` and `up`, a pair of pipes' ends, put in `to`: the answer that hands them over, each
/// one's value there a little-endian u64, the way down's first (grant_ask `handed_pair`).
#[cfg(windows)]
fn give_pair(
    to: &dyn Recipient,
    down: &std::os::windows::io::OwnedHandle,
    up: &std::os::windows::io::OwnedHandle,
) -> Result<Vec<u8>, String> {
    use std::os::windows::io::AsHandle as _;
    let mut payload = Vec::with_capacity(16);
    for end in [down, up] {
        let value = to
            .give(end.as_handle())
            .map_err(|e| format!("giving a VM its pipes: {e}"))?;
        payload.extend((value as u64).to_le_bytes());
    }
    Ok(payload)
}

/// Answers the VM on `link` as it asks, until it closes it, putting what it grants in
/// `to`, the VM's process: a file's handle and the path it was found at (the VM may not ask,
/// PM M203); its vsock listener's first pair of pipes, named at its path, and each next as
/// it asks (`VSOCK_PAIR`), and its dials' pipes (D272), each a handle duplicated into it, of
/// which this process keeps no copy, so that the VM's close is their end; a directory to
/// write in made, in which this process stages each snapshot generation the VM asks for,
/// hands it the generation's memory and state files, and commits the generation on its word
/// (`STAGE`, `COMMIT`, D196), apart from its answers ([`commit_apart`]). A path that cannot
/// be granted ends the answers with why. As on macOS, a VM that has its vsock path, the last
/// it asks for, has started, and asks for nothing more but its dials, its pairs and its
/// snapshots. It returns once every commit it began has ended, as the VM and the generation
/// it committed are its caller's to report only then.
#[cfg(windows)]
pub fn serve(link: &crate::grant::Link, to: &dyn Recipient) -> Result<(), String> {
    serve_committing(link, to, &shards_vmm::snapshot::Staged::commit)
}

/// [`serve`], each generation the VM commits made durable and put in use by `commit`.
#[cfg(windows)]
fn serve_committing(
    link: &crate::grant::Link,
    to: &dyn Recipient,
    commit: &(dyn Fn(shards_vmm::snapshot::Staged) -> Result<(), String> + Sync),
) -> Result<(), String> {
    use shards_ipc::kind;
    use std::os::windows::io::AsHandle as _;
    // Its vsock path and its listener's pipes' names, under which its next pairs are made.
    let mut listening: Option<(PathBuf, shards_ipc::VsockNames)> = None;
    // The directories the VM may stage snapshots in, and what is staged there uncommitted,
    // each removed if the VM ends before it commits it.
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut staged: Vec<shards_vmm::snapshot::Staged> = Vec::new();
    let mut started = false;
    // Each commit's directory as it ends, and whether its generation is in place.
    let (ended, commits) = std::sync::mpsc::channel::<(PathBuf, bool)>();
    let refuse = |e: String| {
        let _ = shards_ipc::write_message(&link.to, kind::ERR, e.as_bytes());
        Err(e)
    };
    std::thread::scope(|scope| {
        loop {
            let asked = shards_ipc::read_message(&link.from)
                .map_err(|e| format!("a VM's request for access: {e}"))?;
            let Some(asked) = asked else {
                return Ok(());
            };
            // A directory whose snapshot is in place is given up, as a Unix VM's is (vm_run.rs
            // `seal`): a guest that took the VM's process over later could otherwise have every
            // later restore of it restore what it wrote. Before any ask after the commit's end,
            // which the VM hears of only once this has heard of it ([`commit_apart`]).
            for (dir, in_place) in commits.try_iter() {
                if in_place {
                    dirs.retain(|d| *d != dir);
                }
            }
            if asked.kind == kind::TAKEN {
                continue;
            }
            // Nothing more granted it, as after a vsock path's listen: what it asks after is
            // its files' ranges and its snapshots alone.
            if asked.kind == kind::SEALED {
                started = true;
                continue;
            }
            if asked.kind == kind::DIAL {
                let port = <[u8; 4]>::try_from(asked.payload.as_slice())
                    .map(u32::from_be_bytes)
                    .map_err(|_| "a VM sent a malformed dial".to_string())?;
                let answer = dial_for(listening.as_ref().map(|(path, _)| path.as_path()), port)
                    .and_then(|(down, up)| give_pair(to, &down, &up));
                reply(link, answer, "a VM's dial")?;
                continue;
            }
            // Its listener's next pair, under its first's names, as a client has opened both
            // ways of the last (D272): the VM may make none its user can open.
            if asked.kind == kind::VSOCK_PAIR {
                let answer = match &listening {
                    Some((_, names)) => shards_ipc::vsock_pair(names, false)
                        .map_err(|e| format!("a VM's next pair: {e}"))
                        .and_then(|(down, up)| give_pair(to, &down, &up)),
                    None => Err("a VM granted no vsock path asked for a pair".into()),
                };
                reply(link, answer, "a VM's pair")?;
                continue;
            }
            // A file of the VM's, its ranges read for it (D196): taken out of the VM by its
            // handle there, which names no file of this process's.
            if asked.kind == kind::RANGES {
                let answer = ranges(&asked.payload, to);
                reply(link, answer, "a VM's ranges")?;
                continue;
            }
            if asked.kind == kind::STAGE {
                let answer = stage(&asked.payload, &dirs, to).map(|(generation, payload)| {
                    // One generation at a time a directory: one staged and never committed goes.
                    staged.retain(|s| s.dir() != generation.dir());
                    staged.push(generation);
                    payload
                });
                reply(link, answer, "a VM's stage")?;
                continue;
            }
            if asked.kind == kind::COMMIT {
                let answer = match staged
                    .iter()
                    .position(|s| s.name().as_bytes() == asked.payload.as_slice())
                {
                    Some(at) => commit_apart(scope, staged.swap_remove(at), commit, ended.clone(), to),
                    None => Err(format!(
                        "{}: no generation of that name is staged",
                        String::from_utf8_lossy(&asked.payload)
                    )),
                };
                reply(link, answer, "a VM's commit")?;
                continue;
            }
            let wanted = match (asked.kind, decode(&asked.payload)) {
                (kind::GRANT, Some(wanted)) => wanted,
                _ => return Err("a VM sent a malformed request for access".into()),
            };
            if started {
                return refuse("a VM asked for access after it started".into());
            }
            started = wanted.iter().any(|(access, _)| *access == Access::Listen);
            for (access, path) in wanted {
                let payload = match handed(access, &path) {
                    Ok(Handed::File(file)) => {
                        let real = shards_vmm::platform::final_path(&file)
                            .map_err(|e| format!("{}: {e}", path.display()));
                        let value = to
                            .give(file.as_handle())
                            .map_err(|e| format!("giving a VM {}: {e}", path.display()));
                        match real.and_then(|real| value.map(|v| (v, real))) {
                            Ok((value, real)) => {
                                let mut payload = (value as u64).to_le_bytes().to_vec();
                                payload.extend(crate::grant::path_bytes(&real));
                                payload
                            }
                            Err(e) => return refuse(e),
                        }
                    }
                    Ok(Handed::Absent) => Vec::new(),
                    Ok(Handed::Dir) => {
                        dirs.push(path.clone());
                        Vec::new()
                    }
                    Ok(Handed::Pair(names, down, up)) => {
                        if listening.is_some() {
                            return refuse("a VM asked for a second vsock path".into());
                        }
                        match give_pair(to, &down, &up) {
                            Ok(payload) => {
                                listening = Some((path.clone(), names));
                                payload
                            }
                            Err(e) => return refuse(format!("{}: {e}", path.display())),
                        }
                    }
                    Err(e) => return refuse(e),
                };
                shards_ipc::write_message(&link.to, kind::GRANTED, &payload)
                    .map_err(|e| format!("granting a VM {}: {e}", path.display()))?;
            }
        }
    })
}

/// Makes `generation` durable and puts it in use with `commit` on a thread of `scope`'s, apart
/// from the link, whose answers go on meanwhile (D30, D272, amended 2026-11-18): done in the
/// link's loop, it held every grant the VM asked as it ran, its vsock listener's next pair
/// among them, and so the VM's whole vsock device, as long as its fsyncs took. Its answer,
/// `GRANTED` as it begins, hands the VM the read end of a pipe of its own (`to`'s handle
/// there), on which the thread says its end once `ended` has heard of it: `GRANTED` once the
/// generation is in place, or `ERR` and why not (`grant_ask::durable`).
#[cfg(windows)]
fn commit_apart<'scope, 'env>(
    scope: &'scope std::thread::Scope<'scope, 'env>,
    generation: shards_vmm::snapshot::Staged,
    commit: &'env (dyn Fn(shards_vmm::snapshot::Staged) -> Result<(), String> + Sync),
    ended: std::sync::mpsc::Sender<(PathBuf, bool)>,
    to: &dyn Recipient,
) -> Result<Vec<u8>, String> {
    use shards_ipc::kind;
    use std::os::windows::io::AsHandle as _;
    let (heard, word) = std::io::pipe().map_err(|e| format!("a VM's commit's word: {e}"))?;
    let dir = generation.dir().to_path_buf();
    let durable = move || {
        let made = commit(generation);
        let _ = ended.send((dir, made.is_ok()));
        let _ = match made {
            Ok(()) => shards_ipc::write_message(&word, kind::GRANTED, &[]),
            Err(e) => shards_ipc::write_message(&word, kind::ERR, e.as_bytes()),
        };
    };
    std::thread::Builder::new()
        .name("commit".into())
        .spawn_scoped(scope, durable)
        .map_err(|e| format!("committing a VM's snapshot: {e}"))?;
    let value = to
        .give(heard.as_handle())
        .map_err(|e| format!("giving a VM its commit's word: {e}"))?;
    Ok((value as u64).to_le_bytes().to_vec())
}

/// A generation staged for the VM in the directory a `STAGE`'s payload names, after what it
/// keeps beside its memory and state (its first byte, [`Kept::bits`]), which must be one
/// granted it, and the answer that hands it the generation's files, put in `to`: their
/// handles there, little-endian u64s in [`Kept::files`]'s order, then the generation's name.
///
/// [`Kept::bits`]: shards_vmm::platform::Kept::bits
/// [`Kept::files`]: shards_vmm::platform::Kept::files
#[cfg(windows)]
fn stage(
    asked: &[u8],
    dirs: &[PathBuf],
    to: &dyn Recipient,
) -> Result<(shards_vmm::snapshot::Staged, Vec<u8>), String> {
    use std::os::windows::io::AsHandle as _;
    let (&bits, path) = asked.split_first().ok_or("a VM asked to stage at no path")?;
    let kept =
        shards_vmm::platform::Kept::from_bits(bits).ok_or("a VM asked to stage what no generation keeps")?;
    let dir = crate::grant::path_from(path).ok_or("a VM asked to stage at no path")?;
    if !dirs.contains(&dir) {
        return Err(format!(
            "{}: not a directory the VM was granted to write in",
            dir.display()
        ));
    }
    let generation = shards_vmm::snapshot::prepare_kept(&dir, kept)?;
    let files = generation.handed()?;
    if files.len() != kept.files() {
        return Err(format!(
            "{}: a generation of {} files",
            dir.display(),
            files.len()
        ));
    }
    let mut payload = Vec::with_capacity(8 * files.len() + generation.name().len());
    for file in &files {
        let value = to
            .give(file.as_handle())
            .map_err(|e| format!("giving a VM its snapshot's files: {e}"))?;
        payload.extend((value as u64).to_le_bytes());
    }
    payload.extend(generation.name().as_bytes());
    Ok((generation, payload))
}

/// The ranges of a file of the VM's that hold data, as a `RANGES`'s payload names it (its
/// handle in the VM, and the bytes looked at), and the answer: each range's offset and
/// length, little-endian u64s.
#[cfg(windows)]
fn ranges(asked: &[u8], from: &dyn Recipient) -> Result<Vec<u8>, String> {
    let (value, len) = asked
        .split_first_chunk::<8>()
        .and_then(|(v, rest)| Some((u64::from_le_bytes(*v), <[u8; 8]>::try_from(rest).ok()?)))
        .ok_or("a VM asked for ranges malformed")?;
    let value = usize::try_from(value).map_err(|e| e.to_string())?;
    let file = std::fs::File::from(
        from.take(value)
            .map_err(|e| format!("taking the file a VM asked the ranges of: {e}"))?,
    );
    let found = shards_vmm::platform::allocated_ranges(&file, u64::from_le_bytes(len))
        .map_err(|e| format!("a VM's file's ranges: {e}"))?;
    Ok(found
        .iter()
        .flat_map(|&(at, n)| [at.to_le_bytes(), n.to_le_bytes()])
        .flatten()
        .collect())
}

/// Answers the VM with `GRANTED` and `answer`'s payload, or `ERR` and why not.
#[cfg(windows)]
fn reply(link: &crate::grant::Link, answer: Result<Vec<u8>, String>, what: &str) -> Result<(), String> {
    use shards_ipc::kind;
    match answer {
        Ok(payload) => shards_ipc::write_message(&link.to, kind::GRANTED, &payload),
        Err(e) => shards_ipc::write_message(&link.to, kind::ERR, e.as_bytes()),
    }
    .map_err(|e| format!("answering {what}: {e}"))
}

// `cfg(test)` alone, as clippy reads a test module (clippy_utils `is_cfg_test`), so that
// clippy.toml's in-tests allowances cover its helpers; `all(test, windows)` is not read so.
#[cfg(test)]
#[cfg(windows)]
#[allow(clippy::unwrap_used)]
mod windows_tests {
    use std::io::{Read as _, Write as _};
    use std::os::windows::io::IntoRawHandle as _;

    use super::*;
    use crate::grant::Link;
    use crate::grant_ask::{dial, obtain};

    /// This process, as a VM's stands in for it: what it is given, a handle of its own.
    struct Here;

    impl Recipient for Here {
        fn give(&self, handle: std::os::windows::io::BorrowedHandle<'_>) -> std::io::Result<usize> {
            Ok(handle.try_clone_to_owned()?.into_raw_handle() as usize)
        }

        fn take(&self, value: usize) -> std::io::Result<std::os::windows::io::OwnedHandle> {
            use std::os::windows::io::{BorrowedHandle, RawHandle};
            // SAFETY: a value this test's own process gave, open while it asks.
            unsafe { BorrowedHandle::borrow_raw(value as RawHandle) }.try_clone_to_owned()
        }
    }

    /// A spawner's end and a VM's, over two pipes.
    fn link() -> (Link, Link) {
        let (vm_from, spawner_to) = std::io::pipe().unwrap();
        let (spawner_from, vm_to) = std::io::pipe().unwrap();
        let file = |h: std::os::windows::io::OwnedHandle| std::fs::File::from(h);
        (
            Link {
                from: file(spawner_from.into()),
                to: file(spawner_to.into()),
            },
            Link {
                from: file(vm_from.into()),
                to: file(vm_to.into()),
            },
        )
    }

    /// A request goes over whole, a Windows name that is no UTF-8 included, and a count, a
    /// length or an access past what is allowed is refused, as is a relative path or
    /// anything after the last path.
    #[test]
    fn requests_cross_whole_and_bounded() {
        use crate::grant::MAX_GRANTS;
        use crate::grant_ask::encode;
        use std::os::windows::ffi::OsStringExt as _;
        // An unpaired surrogate, which a Windows name may hold.
        let odd =
            std::ffi::OsString::from_wide(&[u16::from(b'C'), u16::from(b':'), u16::from(b'\\'), 0xD800]);
        let wanted = vec![
            (Access::Read, PathBuf::from(r"C:\k\kernel")),
            (Access::Write, PathBuf::from(r"\\?\D:\d\disk")),
            (Access::Listen, PathBuf::from(odd)),
        ];
        let bytes = encode(&wanted).unwrap();
        assert_eq!(decode(&bytes).unwrap(), wanted);
        assert!(decode(&bytes[..bytes.len() - 1]).is_none(), "cut short");
        assert!(
            decode(&[bytes.as_slice(), &[0]].concat()).is_none(),
            "trailing bytes"
        );
        assert!(
            decode(&((MAX_GRANTS as u32) + 1).to_be_bytes()).is_none(),
            "too many"
        );
        let relative = encode(&[(Access::Read, PathBuf::from("kernel"))]).unwrap();
        assert!(decode(&relative).is_none(), "relative");
        let long = PathBuf::from(format!(r"C:\{}", "x".repeat(MAX_PATH)));
        assert!(encode(&[(Access::Read, long)]).is_err());
    }

    /// A VM's request is answered as asked: a file it reads, read-only; one it writes,
    /// read-write; one to read if it is there and is not, not found, though a file appears
    /// there since; a pointer written twice, its newest version, and one never written, not
    /// found; each also at the path its spawner found it at.
    #[test]
    fn a_spawner_grants_what_a_vm_asks_for() {
        use shards_vmm::platform::{input_path, open_input};
        let dir = shards_testdir::TempDir::new("grant").unwrap();
        let (kernel, disk, absent) = (dir.join("kernel"), dir.join("disk"), dir.join("absent"));
        std::fs::write(&kernel, b"k").unwrap();
        std::fs::write(&disk, b"d").unwrap();
        let (pointer, unpointed) = (dir.join("current"), dir.join("never"));
        let at = shards_vmm::platform::open_dir(&dir).unwrap();
        shards_vmm::platform::write_in(&at, "current", b"g-1\n").unwrap();
        shards_vmm::platform::write_in(&at, "current", b"g-2\n").unwrap();
        let (spawner, vm) = link();
        std::thread::scope(|scope| {
            let serving = scope.spawn(|| serve(&spawner, &Here));
            obtain(
                &vm,
                &[
                    (Access::Read, kernel.clone()),
                    (Access::Write, disk.clone()),
                    (Access::ReadIfThere, absent.clone()),
                    (Access::ReadNewestIfThere, pointer.clone()),
                    (Access::ReadNewestIfThere, unpointed.clone()),
                ],
            )
            .unwrap();
            let mut got = String::new();
            open_input(&kernel, false)
                .unwrap()
                .read_to_string(&mut got)
                .unwrap();
            assert_eq!(got, "k");
            assert_eq!(
                open_input(&kernel, true).unwrap_err().kind(),
                std::io::ErrorKind::PermissionDenied,
                "a file it reads is granted read-only"
            );
            open_input(&disk, true).unwrap().write_all(b"D").unwrap();
            assert_eq!(std::fs::read(&disk).unwrap(), b"D");
            let real = input_path(&kernel).unwrap();
            assert_eq!(std::fs::canonicalize(&kernel).unwrap(), real);
            open_input(&real, false).unwrap();
            std::fs::write(&absent, b"later").unwrap();
            assert_eq!(
                open_input(&absent, false).unwrap_err().kind(),
                std::io::ErrorKind::NotFound
            );
            let mut newest = String::new();
            open_input(&pointer, false)
                .unwrap()
                .read_to_string(&mut newest)
                .unwrap();
            assert_eq!(newest, "g-2\n", "the pointer's newest version");
            assert_eq!(
                open_input(&unpointed, false).unwrap_err().kind(),
                std::io::ErrorKind::NotFound
            );
            drop(vm);
            assert!(serving.join().unwrap().is_ok(), "the VM's close ends its answers");
        });
    }

    /// A directory to write in is made, and the VM's snapshots are staged there for it
    /// (D196): a generation handed over as its memory and state files, which the VM fills,
    /// then committed on its word, after which the directory is given up (vm_run.rs `seal`).
    /// A stage elsewhere, and a commit of a generation not staged, are refused; one staged
    /// and never committed goes with the VM.
    #[test]
    fn a_spawner_stages_and_commits_a_vms_snapshots() {
        use crate::grant_ask::{commit, durable, stage};
        use shards_vmm::platform::Kept;
        let dir = shards_testdir::TempDir::new("stage").unwrap();
        let snapshot = dir.join("snapshot");
        let (spawner, vm) = link();
        std::thread::scope(|scope| {
            let serving = scope.spawn(|| serve(&spawner, &Here));
            obtain(&vm, &[(Access::MakeDir, snapshot.clone())]).unwrap();
            assert!(snapshot.is_dir());
            let elsewhere = stage(&vm, &dir.join("other"), Kept::default())
                .unwrap_err()
                .to_string();
            assert!(
                elsewhere.contains("not a directory the VM was granted"),
                "{elsewhere}"
            );
            let unstaged = commit(&vm, "g-1").unwrap_err().to_string();
            assert!(unstaged.contains("no generation of that name"), "{unstaged}");
            // With what a run's machine keeps beside its memory: its layer's head and file
            // system, and its DAX windows' heads, each in the generation once committed.
            let kept = Kept {
                layer: true,
                window_heads: true,
            };
            let (name, files) = stage(&vm, &snapshot, kept).unwrap();
            let names = ["memory", "state", "layer-head", "layer", "window-heads"];
            assert_eq!(files.len(), names.len());
            for (mut file, name) in files.iter().zip(names) {
                file.write_all(name.as_bytes()).unwrap();
            }
            drop(files);
            // The pointer as its readers take it, its newest version (platform `write_in`).
            let pointer = || shards_vmm::platform::read_newest_beneath(&snapshot, &["current"], 64).unwrap();
            assert_eq!(pointer(), None, "none in use until its commit");
            durable(&commit(&vm, &name).unwrap()).unwrap();
            assert_eq!(pointer(), Some(format!("{name}\n").into_bytes()));
            for file in names {
                assert_eq!(
                    std::fs::read(snapshot.join(&name).join(file)).unwrap(),
                    file.as_bytes(),
                    "{file}"
                );
            }
            let again = stage(&vm, &snapshot, Kept::default()).unwrap_err().to_string();
            assert!(again.contains("not a directory the VM was granted"), "{again}");
            drop(vm);
            assert!(serving.join().unwrap().is_ok());
        });
        let abandoned = dir.join("abandoned");
        let (spawner, vm) = link();
        std::thread::scope(|scope| {
            let serving = scope.spawn(|| serve(&spawner, &Here));
            obtain(&vm, &[(Access::MakeDir, abandoned.clone())]).unwrap();
            let (_, files) = stage(&vm, &abandoned, Kept::default()).unwrap();
            assert_eq!(files.len(), 2, "its memory and state alone");
            drop((files, vm));
            assert!(serving.join().unwrap().is_ok());
        });
        let left: Vec<_> = std::fs::read_dir(&abandoned)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n != ".lock")
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    /// A VM's ask is answered while its snapshot is committed (D30, D272, amended 2026-11-18):
    /// the generation is made durable apart from the link, whose answers go on, and the
    /// commit's end is said once it has ended. Committed in the link's loop, it held every
    /// grant asked meanwhile, a resumed VM's vsock listener's next pair among them, and so its
    /// whole vsock device, as long as the fsyncs took (CI 44189693085, windows-2025: a round
    /// across its snapshot with no byte for 900 ms; jupiter at a5369bd051: "snapshot made
    /// durable in 52013 us"). Its commit, a stand-in, is held open until the ask is answered;
    /// past 10 s it goes on, so that one held in the loop fails this test rather than hangs
    /// it. Red with the commit made in the loop.
    #[test]
    fn a_grant_asked_while_a_snapshot_commits_is_answered_before_the_commit_ends() {
        use crate::grant_ask::{commit, durable, ranges, stage};
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = shards_testdir::TempDir::new("commit-apart").unwrap();
        let snapshot = dir.join("snapshot");
        std::fs::write(dir.join("ranged"), b"data").unwrap();
        let ranged = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.join("ranged"))
            .unwrap();
        let (release, held) = std::sync::mpsc::channel::<()>();
        let held = std::sync::Mutex::new(held);
        let ended = AtomicBool::new(false);
        let stand_in = |generation: shards_vmm::snapshot::Staged| {
            let _ = held
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(10));
            let made = generation.commit();
            ended.store(true, Ordering::SeqCst);
            made
        };
        let (spawner, vm) = link();
        std::thread::scope(|scope| {
            let serving = scope.spawn(|| serve_committing(&spawner, &Here, &stand_in));
            obtain(&vm, &[(Access::MakeDir, snapshot.clone())]).unwrap();
            let (name, files) = stage(&vm, &snapshot, shards_vmm::platform::Kept::default()).unwrap();
            drop(files);
            let word = commit(&vm, &name).unwrap();
            let answered = ranges(&vm, &ranged, 4);
            let before = !ended.load(Ordering::SeqCst);
            release.send(()).unwrap();
            durable(&word).unwrap();
            assert!(answered.is_ok(), "{answered:?}");
            assert!(before, "the VM's ask was answered only once its commit had ended");
            drop(vm);
            assert!(serving.join().unwrap().is_ok());
        });
    }

    /// The ranges of a VM's file that hold data are read for it by its spawner, which takes
    /// the file out of the VM by its handle there (`RANGES`, D196): a sparse file's data,
    /// and not its holes; a value that names no handle of the VM's is refused.
    #[test]
    fn a_vms_files_ranges_are_read_for_it() {
        use std::os::windows::fs::FileExt as _;
        let dir = shards_testdir::TempDir::new("ranges").unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(dir.join("layer"))
            .unwrap();
        shards_vmm::platform::sparse(&file).unwrap();
        let len = 64u64 << 20;
        file.set_len(len).unwrap();
        file.seek_write(b"data", 32 << 20).unwrap();
        let (spawner, vm) = link();
        std::thread::scope(|scope| {
            let serving = scope.spawn(|| serve(&spawner, &Here));
            let found = crate::grant_ask::ranges(&vm, &file, len).unwrap();
            let held: u64 = found.iter().map(|&(_, n)| n).sum();
            assert!(
                found.iter().any(|&(at, n)| at <= 32 << 20 && at + n > 32 << 20),
                "{found:?}"
            );
            assert!(held < len, "its holes held too: {found:?}");
            // A value that names no handle: a multiple of four past any this test holds.
            let asked = [(1u64 << 40).to_le_bytes(), len.to_le_bytes()].concat();
            shards_ipc::write_message(&vm.to, shards_ipc::kind::RANGES, &asked).unwrap();
            let refused = shards_ipc::read_message(&vm.from).unwrap().unwrap();
            assert_eq!(refused.kind, shards_ipc::kind::ERR);
            drop(vm);
            assert!(serving.join().unwrap().is_ok());
        });
    }

    /// A VM with no vsock path that seals its grants (`SEALED`, PM M377) is granted nothing
    /// more, as one that has listened at its vsock path is, while its files' ranges are still
    /// read for it: what any save of it asks.
    #[test]
    fn a_sealed_vm_is_granted_nothing_more_and_its_ranges_are_read() {
        let dir = shards_testdir::TempDir::new("sealed").unwrap();
        let (kernel, later) = (dir.join("kernel"), dir.join("later"));
        std::fs::write(&kernel, b"k").unwrap();
        std::fs::write(&later, b"l").unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(dir.join("layer"))
            .unwrap();
        shards_vmm::platform::sparse(&file).unwrap();
        let len = 64u64 << 20;
        file.set_len(len).unwrap();
        let (spawner, vm) = link();
        std::thread::scope(|scope| {
            let serving = scope.spawn(|| serve(&spawner, &Here));
            obtain(&vm, &[(Access::Read, kernel.clone())]).unwrap();
            shards_ipc::write_message(&vm.to, shards_ipc::kind::SEALED, &[]).unwrap();
            let found = crate::grant_ask::ranges(&vm, &file, len).unwrap();
            assert!(found.iter().map(|&(_, n)| n).sum::<u64>() < len, "{found:?}");
            assert!(
                obtain(&vm, &[(Access::Read, later.clone())]).is_err(),
                "a sealed VM was granted more"
            );
            drop(vm);
            assert!(
                serving.join().unwrap().is_err(),
                "its asking after was not refused"
            );
        });
    }

    /// A VM that stages its snapshots and has no vsock path (platform-03) asks for its
    /// grants as `confine` does, its last grant asked for, a directory to stage in among them,
    /// before its guest starts: what a guest that took the VM's process over asks after is
    /// refused, a file to write outside its grants left as it was, while its snapshots are
    /// still staged and committed for it.
    #[test]
    fn a_vm_that_stages_its_snapshots_is_granted_nothing_more_once_it_starts() {
        use crate::grant_ask::{commit, durable, obtain_sealed, stage};
        let dir = shards_testdir::TempDir::new("staged").unwrap();
        let (kernel, snapshot, sentinel) = (dir.join("kernel"), dir.join("snapshot"), dir.join("sentinel"));
        std::fs::write(&kernel, b"k").unwrap();
        std::fs::write(&sentinel, b"untouched").unwrap();
        let (spawner, vm) = link();
        std::thread::scope(|scope| {
            let serving = scope.spawn(|| serve(&spawner, &Here));
            obtain_sealed(
                &vm,
                &[
                    (Access::Read, kernel.clone()),
                    (Access::MakeDir, snapshot.clone()),
                ],
            )
            .unwrap();
            let (name, files) = stage(&vm, &snapshot, shards_vmm::platform::Kept::default()).unwrap();
            drop(files);
            durable(&commit(&vm, &name).unwrap()).unwrap();
            let late = obtain(&vm, &[(Access::Write, sentinel.clone())]);
            if late.is_ok() {
                let _ = shards_vmm::platform::open_input(&sentinel, true)
                    .and_then(|mut f| f.write_all(b"overwritten"));
            }
            assert_eq!(std::fs::read(&sentinel).unwrap(), b"untouched");
            let late = late.unwrap_err();
            assert!(late.contains("after it started"), "{late}");
            drop(vm);
            assert!(serving.join().unwrap().is_err(), "the refusal ended its answers");
        });
    }

    /// A VM's vsock path is a pair of pipes made for it and named at the path (D272),
    /// replacing a file an earlier VM left there whose pipes are gone; each next pair is made
    /// for it as it asks, under the first's names; its dials open a host port's pair by its
    /// file at `<path>_<port>` and nothing else, this process keeping no copy, so that the
    /// VM's close is the end its host reads; a dial and a pair asked for before any vsock path
    /// is granted, and a second vsock path, are refused; a VM that has started is granted
    /// nothing more.
    #[test]
    fn a_vms_vsock_pipes_are_made_and_dialled_for_it() {
        use shards_vmm::platform::{Listening, Paired, Pipes, listen_pipes};
        use std::time::{Duration, Instant};
        // A pair's connects finish as their clients open them: the next looks', as they land.
        let whole = |pair: &mut Listening, what: &str| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while pair.look() != Paired::Connected {
                assert!(Instant::now() < deadline, "{what}: not whole after 10 s");
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        let until = |p: &mut Pipes, what: &str| {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                p.look();
                if p.readable() {
                    return;
                }
                assert!(Instant::now() < deadline, "{what}: not after 10 s");
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        let dir = shards_testdir::TempDir::new("dial").unwrap();
        let vsock = dir.join("v");
        // An earlier VM's file, its pipes gone.
        let (gone, down, up) = shards_ipc::vsock_listen(&vsock).unwrap();
        drop((down, up));
        let (_, port_down, port_up) = shards_ipc::vsock_listen(&dir.join("v_5000")).unwrap();
        let mut host = Listening::new(port_down, port_up).unwrap();
        let (spawner, vm) = link();
        std::thread::scope(|scope| {
            let serving = scope.spawn(|| serve(&spawner, &Here));
            assert!(
                dial(&vm, 5000).is_err(),
                "a dial before any vsock path is granted"
            );
            assert!(
                crate::grant_ask::next_pair(&vm).is_err(),
                "a pair before any vsock path is granted"
            );
            obtain(&vm, &[(Access::Listen, vsock.clone())]).unwrap();
            let names = shards_ipc::VsockNames::read(&vsock).unwrap();
            assert_ne!(names, gone, "an earlier VM's names taken");
            let ((down, up), _) = listen_pipes(&vsock).unwrap();
            let mut first = Listening::new(down, up).unwrap();
            let theirs = shards_ipc::vsock_open(&vsock, None).unwrap();
            whole(&mut first, "the granted pair, the path's");
            drop((first, theirs));
            let (down, up) = crate::grant_ask::next_pair(&vm).unwrap();
            let mut next = Listening::new(down, up).unwrap();
            let _theirs = shards_ipc::vsock_open(&vsock, None).unwrap();
            whole(&mut next, "the next pair, the first's names'");
            assert_eq!(shards_ipc::VsockNames::read(&vsock).unwrap(), names);
            let mut dialled = dial(&vm, 5000).unwrap();
            whole(&mut host, "the host port's pair, the dial's");
            let mut host = host.into_pipes().unwrap();
            dialled.write_all_waiting(b"x").unwrap();
            until(&mut host, "the guest's byte");
            let mut byte = [0u8; 1];
            assert_eq!(std::io::Read::read(&mut host, &mut byte).unwrap(), 1);
            // The VM's close is the end the host reads: the spawner holds no copy open.
            drop(dialled);
            until(&mut host, "the guest's end");
            assert_eq!(std::io::Read::read(&mut host, &mut byte).unwrap(), 0);
            assert!(dial(&vm, 5001).is_err(), "nothing listens on that port");
            let secret = dir.join("secret");
            std::fs::write(&secret, b"s").unwrap();
            let late = obtain(&vm, &[(Access::Read, secret.clone())]).unwrap_err();
            assert!(late.contains("after it started"), "{late}");
            drop(vm);
            assert!(serving.join().unwrap().is_err(), "the refusal ended its answers");
        });
        // One in use is not taken over.
        let live = dir.join("live");
        let _held = shards_ipc::vsock_listen(&live).unwrap();
        let (spawner, vm) = link();
        std::thread::scope(|scope| {
            let serving = scope.spawn(|| serve(&spawner, &Here));
            assert!(obtain(&vm, &[(Access::Listen, live.clone())]).is_err());
            drop(vm);
            let _ = serving.join();
        });
    }

    /// What a dialled connection reads to its end, as its VM's device reads it: a look, and a
    /// read once one would not wait.
    fn read_to_end(p: &mut shards_vmm::platform::Pipes) -> Vec<u8> {
        use std::time::{Duration, Instant};
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut got = Vec::new();
        let mut buf = vec![0u8; 1 << 16];
        loop {
            p.look();
            if p.readable() {
                match std::io::Read::read(p, &mut buf) {
                    Ok(0) => return got,
                    Ok(n) => got.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => panic!("reading the carried connection: {e}"),
                }
            } else {
                assert!(
                    Instant::now() < deadline,
                    "not at its end after 30 s, {} bytes read",
                    got.len()
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    /// Three ways' quotas of bytes, none of their places alike.
    fn three_quotas() -> Vec<u8> {
        let quota = usize::try_from(shards_ipc::VSOCK_QUOTA).unwrap();
        (0..3 * quota).map(|i| u8::try_from(i % 251).unwrap()).collect()
    }

    /// A dial to a host port an AF_UNIX listener holds, as a builder's are on every OS (D272,
    /// amended 2026-11-17), is connected there and carried over pipes its VM is handed: three
    /// quotas of bytes cross whole each way; the guest's end of its writes reaches its host
    /// after the last of them, and its host's answer, written after, still reaches the guest,
    /// then the host's end.
    #[test]
    fn a_dial_to_an_af_unix_listener_is_carried_whole_both_ways() {
        let sent = three_quotas();
        let dir = shards_testdir::TempDir::new("carry").unwrap();
        let vsock = dir.join("v");
        let host = shards_vmm::platform::UnixListener::bind(dir.join("v_6000")).unwrap();
        let (spawner, vm) = link();
        std::thread::scope(|scope| {
            let serving = scope.spawn(|| serve(&spawner, &Here));
            obtain(&vm, &[(Access::Listen, vsock.clone())]).unwrap();
            // The host reads to its guest's end, then answers with every byte and ends.
            let answering = scope.spawn(|| {
                let (conn, ()) = host.accept().unwrap();
                let mut got = Vec::new();
                (&conn).read_to_end(&mut got).unwrap();
                (&conn).write_all(&got).unwrap();
                conn.shutdown(std::net::Shutdown::Write).unwrap();
                got
            });
            let mut dialled = dial(&vm, 6000).unwrap();
            dialled.write_all_waiting(&sent).unwrap();
            // The guest's way ended, its host's still read.
            dialled.shut_down();
            let back = read_to_end(&mut dialled);
            assert!(
                answering.join().unwrap() == sent,
                "the host read every byte, then the guest's end"
            );
            assert!(back == sent, "the guest read every byte, then the host's end");
            drop((dialled, vm));
            let _ = serving.join();
        });
    }

    /// A carried connection's host may end its way first: its guest reads every byte, then
    /// that end, and its own way goes on, its bytes read whole by the host after it ended
    /// its writes.
    #[test]
    fn a_carried_dials_host_ends_first_and_its_guest_answers_after() {
        let sent = three_quotas();
        let dir = shards_testdir::TempDir::new("carry-host").unwrap();
        let vsock = dir.join("v");
        let host = shards_vmm::platform::UnixListener::bind(dir.join("v_6001")).unwrap();
        let (spawner, vm) = link();
        std::thread::scope(|scope| {
            let serving = scope.spawn(|| serve(&spawner, &Here));
            obtain(&vm, &[(Access::Listen, vsock.clone())]).unwrap();
            let sending = scope.spawn(|| {
                let (conn, ()) = host.accept().unwrap();
                (&conn).write_all(&sent).unwrap();
                conn.shutdown(std::net::Shutdown::Write).unwrap();
                let mut got = Vec::new();
                (&conn).read_to_end(&mut got).unwrap();
                got
            });
            let mut dialled = dial(&vm, 6001).unwrap();
            let back = read_to_end(&mut dialled);
            assert!(back == sent, "the guest read every byte, then the host's end");
            dialled.write_all_waiting(&back).unwrap();
            dialled.shut_down();
            assert!(
                sending.join().unwrap() == sent,
                "the host read every byte after its own end"
            );
            drop((dialled, vm));
            let _ = serving.join();
        });
    }

    /// A host port is a vsock pair's file or an AF_UNIX socket, read once by its entry's
    /// handle: a directory there is neither, and is refused with its path named, as a port
    /// nothing listens on is.
    #[test]
    fn a_dial_to_neither_a_pairs_file_nor_a_socket_is_refused_naming_it() {
        let dir = shards_testdir::TempDir::new("carry-none").unwrap();
        let vsock = dir.join("v");
        std::fs::create_dir(dir.join("v_7000")).unwrap();
        let refused = dial_for(Some(vsock.as_path()), 7000).unwrap_err();
        assert!(
            refused.contains("v_7000") && refused.contains("a directory"),
            "{refused}"
        );
        let absent = dial_for(Some(vsock.as_path()), 7001).unwrap_err();
        assert!(absent.contains("v_7001"), "{absent}");
    }
}

#[cfg(all(test, target_os = "macos"))]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::grant_ask::{dial, encode, obtain};

    /// A request goes over whole, and a count, a length or an access past what is allowed
    /// is refused, as is a relative path or anything after the last path.
    #[test]
    fn requests_cross_whole_and_bounded() {
        let wanted = vec![
            (Access::Read, PathBuf::from("/k/kernel")),
            (Access::Write, PathBuf::from("/d/disk")),
            (Access::MakeDir, PathBuf::from("/s/snapshot")),
        ];
        let bytes = encode(&wanted).unwrap();
        assert_eq!(decode(&bytes).unwrap(), wanted);
        assert!(decode(&bytes[..bytes.len() - 1]).is_none(), "cut short");
        assert!(
            decode(&[bytes.as_slice(), &[0]].concat()).is_none(),
            "trailing bytes"
        );
        assert!(
            decode(&((MAX_GRANTS as u32) + 1).to_be_bytes()).is_none(),
            "too many"
        );
        let relative = encode(&[(Access::Read, PathBuf::from("kernel"))]).unwrap();
        assert!(decode(&relative).is_none(), "relative");
        let mut bad_access = bytes.clone();
        bad_access[4] = 9;
        assert!(decode(&bad_access).is_none(), "an access with no meaning");
        let long = PathBuf::from(format!("/{}", "x".repeat(MAX_PATH)));
        assert!(encode(&[(Access::Read, long)]).is_err());
    }

    /// The caller's wait comes before each of the VM's messages, told how many it has sent,
    /// and before the end it reads: the daemon's watch sees a VM gone quiet between two
    /// requests as before its first.
    #[test]
    fn a_wait_comes_before_every_message() {
        let dir = shards_testdir::TempDir::new("grant-wait").unwrap();
        let kernel = dir.join("kernel");
        std::fs::write(&kernel, b"k").unwrap();
        let (vm, spawner) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut waits = Vec::new();
        std::thread::scope(|scope| {
            let serving = scope.spawn(|| {
                serve_waiting(&spawner, None, &mut |sent| {
                    waits.push(sent);
                    Ok(())
                })
            });
            for _ in 0..2 {
                obtain(&vm, &[(Access::Read, kernel.clone())]).unwrap();
            }
            drop(vm);
            assert_eq!(serving.join().unwrap(), Ok(()));
        });
        // Two requests, each answered with a file (the VM's next request says it has it),
        // and the end.
        assert_eq!(waits, [0, 1, 2]);
    }

    /// A VM's request is answered as asked: a file it reads, read-only (it reads, and
    /// cannot write); one it writes, read-write; one to read if it is there and is not,
    /// not found, though a file appears there since; a pointer written twice, its newest
    /// version, and one never written, not found; a directory to write in, made. A path
    /// that cannot be granted ends the answers with why.
    #[test]
    fn a_spawner_grants_what_a_vm_asks_for() {
        use shards_vmm::platform::open_input;
        use std::io::{Read as _, Write as _};
        let dir = shards_testdir::TempDir::new("grant").unwrap();
        let (kernel, disk, absent, made) = (
            dir.join("kernel"),
            dir.join("disk"),
            dir.join("absent"),
            dir.join("snapshot"),
        );
        std::fs::write(&kernel, b"k").unwrap();
        std::fs::write(&disk, b"d").unwrap();
        let (pointer, unpointed) = (dir.join("current"), dir.join("never"));
        let at = shards_vmm::platform::open_dir(&dir).unwrap();
        shards_vmm::platform::write_in(&at, "current", b"g-1\n").unwrap();
        shards_vmm::platform::write_in(&at, "current", b"g-2\n").unwrap();
        let (vm, spawner) = std::os::unix::net::UnixStream::pair().unwrap();
        std::thread::scope(|scope| {
            let serving = scope.spawn(|| serve(&spawner, None));
            obtain(
                &vm,
                &[
                    (Access::Read, kernel.clone()),
                    (Access::Write, disk.clone()),
                    (Access::ReadIfThere, absent.clone()),
                    (Access::ReadNewestIfThere, pointer.clone()),
                    (Access::ReadNewestIfThere, unpointed.clone()),
                    (Access::MakeDir, made.clone()),
                ],
            )
            .unwrap();
            let mut got = String::new();
            open_input(&kernel, false)
                .unwrap()
                .read_to_string(&mut got)
                .unwrap();
            assert_eq!(got, "k");
            assert_eq!(
                open_input(&kernel, true).unwrap_err().kind(),
                std::io::ErrorKind::PermissionDenied,
                "a file it reads is granted read-only"
            );
            open_input(&disk, true).unwrap().write_all(b"D").unwrap();
            assert_eq!(std::fs::read(&disk).unwrap(), b"D");
            std::fs::write(&absent, b"later").unwrap();
            assert_eq!(
                open_input(&absent, false).unwrap_err().kind(),
                std::io::ErrorKind::NotFound
            );
            let mut newest = String::new();
            open_input(&pointer, false)
                .unwrap()
                .read_to_string(&mut newest)
                .unwrap();
            assert_eq!(newest, "g-2\n", "the pointer's newest version");
            assert_eq!(
                open_input(&unpointed, false).unwrap_err().kind(),
                std::io::ErrorKind::NotFound
            );
            assert!(made.is_dir(), "the directory to write in was made");
            let e = obtain(&vm, &[(Access::Read, dir.join("not-there"))]).unwrap_err();
            assert!(e.contains("not-there"), "{e}");
            drop(vm);
            assert!(serving.join().unwrap().is_err(), "the refusal ended its answers");
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A restoring VM's template, which a VM process wrote and may have left anything in
    /// (PM M165), is granted beneath it alone: its regular files are, a link to a file
    /// outside, a link to a directory on the way to one and a FIFO are each refused, the
    /// FIFO without waiting on it, and nothing of it is granted for writing; a path outside
    /// it, as the VM's other files are, is granted as before.
    #[test]
    fn a_restores_template_is_granted_beneath_it_alone() {
        use shards_vmm::platform::open_input;
        use std::io::Read as _;
        let dir_guard = shards_testdir::TempDir::new("grant-template").unwrap();
        let dir = dir_guard.join("grant-template");
        let _ = std::fs::remove_dir_all(&dir);
        let (template, outside) = (dir.join("template"), dir.join("outside"));
        std::fs::create_dir_all(template.join("g-1")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(template.join("current"), b"g-1\n").unwrap();
        std::fs::write(template.join("g-1/state"), b"state").unwrap();
        std::fs::write(outside.join("memory"), b"secret").unwrap();
        std::fs::write(outside.join("state"), b"secret").unwrap();
        std::os::unix::fs::symlink(outside.join("memory"), template.join("g-1/memory")).unwrap();
        std::os::unix::fs::symlink(&outside, template.join("g-2")).unwrap();
        let fifo = std::ffi::CString::new(
            template
                .join("g-1/working-set")
                .into_os_string()
                .into_encoded_bytes(),
        )
        .unwrap();
        // SAFETY: mkfifo(2) of a NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let kernel = outside.join("kernel");
        std::fs::write(&kernel, b"k").unwrap();
        // One broker a request: a refusal ends its answers. Each is answered within the
        // deadline, or the test fails rather than waits.
        let ask = |wanted: Vec<(Access, PathBuf)>| -> Result<(), String> {
            let (tx, rx) = std::sync::mpsc::channel();
            let template = template.clone();
            std::thread::spawn(move || {
                let (vm, spawner) = std::os::unix::net::UnixStream::pair().unwrap();
                let got = std::thread::scope(|scope| {
                    let serving = scope.spawn(|| serve(&spawner, Some(&template)));
                    let got = obtain(&vm, &wanted);
                    drop(vm);
                    let _ = serving.join();
                    got
                });
                let _ = tx.send(got);
            });
            rx.recv_timeout(std::time::Duration::from_secs(20))
                .expect("answered, not waiting on a FIFO")
        };
        ask(vec![
            (Access::Read, template.join("current")),
            (Access::Read, template.join("g-1/state")),
            (Access::Read, kernel.clone()),
        ])
        .unwrap();
        let mut got = String::new();
        open_input(&template.join("g-1/state"), false)
            .unwrap()
            .read_to_string(&mut got)
            .unwrap();
        assert_eq!(got, "state");
        for (access, path, why) in [
            (Access::Read, template.join("g-1/memory"), "not a regular file"),
            (
                Access::ReadIfThere,
                template.join("g-1/working-set"),
                "not a regular file",
            ),
            (Access::Read, template.join("g-2/state"), "g-2/state"),
            (
                Access::Write,
                template.join("g-1/state"),
                "writes nothing of its template",
            ),
            (
                Access::MakeDir,
                template.join("g-3"),
                "writes nothing of its template",
            ),
        ] {
            let e = ask(vec![(access, path.clone())]).unwrap_err();
            assert!(e.contains(why), "{}: {e}", path.display());
        }
        assert_eq!(
            ask(vec![(Access::ReadIfThere, template.join("g-1/absent"))]),
            Ok(())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A VM's vsock path is bound for it, replacing a socket an earlier VM left there that
    /// nothing answers on; its dials reach `<path>_<port>` and nothing else; a dial before
    /// any vsock path is granted, and a second vsock path, are refused; a path in use is not
    /// taken over.
    #[test]
    fn a_vms_vsock_is_bound_and_dialled_for_it() {
        use std::os::unix::net::{UnixListener, UnixStream};
        let dir = shards_testdir::TempDir::new("dial").unwrap();
        let vsock = dir.join("v");
        // Bound, never listened on: a copy another test's spawn may inherit (std's
        // sockets are close-on-exec only after they are made, on macOS) answers nothing
        // either, so the path stays stale whoever holds it.
        drop(bind(&vsock).unwrap());
        assert!(vsock.exists(), "a socket file left behind");
        let host = UnixListener::bind(dir.join("v_5000")).unwrap();
        let (vm, spawner) = UnixStream::pair().unwrap();
        std::thread::scope(|scope| {
            let serving = scope.spawn(|| serve(&spawner, None));
            assert!(
                dial(&vm, 5000).is_err(),
                "a dial before any vsock path is granted"
            );
            obtain(&vm, &[(Access::Listen, vsock.clone())]).unwrap();
            assert_eq!(
                UnixStream::connect(&vsock).unwrap_err().kind(),
                std::io::ErrorKind::ConnectionRefused,
                "refused until the VM listens"
            );
            let listener = shards_vmm::platform::listen_unix(&vsock).unwrap();
            UnixStream::connect(&vsock).unwrap();
            assert!(listener.accept().is_ok(), "the granted listener is at the path");
            let dialled = dial(&vm, 5000).unwrap();
            let (mut accepted, _) = host.accept().unwrap();
            assert_eq!(
                dialled.peer_addr().unwrap().as_pathname(),
                Some(dir.join("v_5000").as_path())
            );
            // The VM's close is the end the host sees: the spawner holds no copy open.
            // (Set first: macOS refuses options on a socket its peer has closed.)
            accepted
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            drop(dialled);
            let mut byte = [0u8; 1];
            assert_eq!(
                std::io::Read::read(&mut accepted, &mut byte).map_err(|e| e.kind()),
                Ok(0)
            );
            drop(accepted);
            assert!(dial(&vm, 5001).is_err(), "nothing listens on that port");
            // Started, it is granted nothing more: not a file to read, as a VM process
            // its guest had taken over would ask for.
            let secret = dir.join("secret");
            std::fs::write(&secret, b"s").unwrap();
            let late = obtain(&vm, &[(Access::Read, secret.clone())]).unwrap_err();
            assert!(late.contains("after it started"), "{late}");
            drop(vm);
            assert!(serving.join().unwrap().is_err(), "the refusal ended its answers");
        });
        // One in use is not taken over.
        let (vm, spawner) = UnixStream::pair().unwrap();
        let live = dir.join("live");
        let _held = UnixListener::bind(&live).unwrap();
        std::thread::scope(|scope| {
            let serving = scope.spawn(|| serve(&spawner, None));
            assert!(obtain(&vm, &[(Access::Listen, live.clone())]).is_err());
            drop(vm);
            let _ = serving.join();
        });
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(all(test, target_os = "linux"))]
#[allow(clippy::unwrap_used)]
mod linux_tests {
    use std::os::unix::fs::FileTypeExt as _;
    use std::os::unix::net::{UnixListener, UnixStream};

    /// A VM on Linux is granted its vsock path alone, a socket its spawner binds there, and
    /// has its spawner dial the host ports beside it and nothing else: it names a port, not
    /// a path (D30, platform-02). Anything else it asks for ends the answers.
    #[test]
    fn a_vm_on_linux_is_bound_its_vsock_path_and_dialed_beside_it_alone() {
        let dir = shards_testdir::TempDir::new("dials").unwrap();
        let vsock = dir.join("v");
        let host = UnixListener::bind(dir.join("v_5")).unwrap();
        let (vm, spawner) = UnixStream::pair().unwrap();
        let answering = std::thread::spawn(move || super::serve(&spawner, None));
        crate::grant_ask::listen_at(&vm, &vsock).unwrap();
        assert!(std::fs::symlink_metadata(&vsock).unwrap().file_type().is_socket());
        let mut dialed = crate::grant_ask::dial(&vm, 5).unwrap();
        let (mut accepted, _) = host.accept().unwrap();
        std::io::Write::write_all(&mut dialed, b"x").unwrap();
        let mut got = [0u8; 1];
        std::io::Read::read_exact(&mut accepted, &mut got).unwrap();
        assert_eq!(&got, b"x");
        let none = crate::grant_ask::dial(&vm, 6).unwrap_err();
        assert_eq!(none.kind(), std::io::ErrorKind::ConnectionRefused, "{none}");
        // Its files are Landlock's: a grant of one, and any after its vsock path, ends it.
        let asked = crate::grant_ask::encode(&[(crate::grant::Access::Read, "/etc/passwd".into())]).unwrap();
        shards_ipc::send(&vm, shards_ipc::kind::GRANT, &asked, &[]).unwrap();
        let answer = shards_ipc::recv(&vm).unwrap().unwrap();
        assert_eq!(answer.kind, shards_ipc::kind::ERR);
        assert!(answering.join().unwrap().is_err());
    }

    /// A Linux VM's first ask is refused where it is not its vsock path.
    #[test]
    fn a_vm_on_linux_is_granted_no_file() {
        let (vm, spawner) = UnixStream::pair().unwrap();
        let answering = std::thread::spawn(move || super::serve(&spawner, None));
        let asked = crate::grant_ask::encode(&[(crate::grant::Access::Write, "/tmp/x".into())]).unwrap();
        shards_ipc::send(&vm, shards_ipc::kind::GRANT, &asked, &[]).unwrap();
        let answer = shards_ipc::recv(&vm).unwrap().unwrap();
        assert_eq!(answer.kind, shards_ipc::kind::ERR);
        assert!(String::from_utf8_lossy(&answer.payload).contains("vsock path alone"));
        assert!(answering.join().unwrap().is_err());
    }
}
