//! A spawner's half of a VM's grants (`grant`): it answers what the VM asks for with
//! descriptors, and with bookmarks for the directories it writes in (macOS); with handles
//! and sockets it puts in the VM's process (Windows).

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

/// The access `code` names, if any.
fn access(code: u8) -> Option<Access> {
    match code {
        0 => Some(Access::Read),
        1 => Some(Access::ReadIfThere),
        2 => Some(Access::Write),
        3 => Some(Access::MakeDir),
        4 => Some(Access::Listen),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
/// A read-write bookmark for directory `path`, for another process to resolve: made by
/// CoreFoundation, bound when first needed (shards_apple).
pub fn bookmark(path: &Path) -> Result<Vec<u8>, String> {
    shards_apple::bookmark::bookmark(path)
}

#[cfg(target_os = "macos")]
/// What is granted for `path` as `access`: a file opened for it, a directory's bookmark,
/// nothing, where a file to read if it is there is not, or a socket bound there.
enum Answer {
    File(std::fs::File),
    Bookmark(Vec<u8>),
    Absent,
    Bound(std::os::fd::OwnedFd),
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
        Access::ReadIfThere => match std::fs::File::open(path) {
            Ok(file) => Ok(Answer::File(file)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Answer::Absent),
            Err(e) => Err(at(e)),
        },
        Access::MakeDir => {
            std::fs::create_dir_all(path).map_err(at)?;
            bookmark(path).map(Answer::Bookmark)
        }
        Access::Listen => listen(path).map(Answer::Bound).map_err(at),
    }
}

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
/// A connection to vsock host port `port` beside `listening`, the vsock path the VM was
/// granted: the only sockets it may have dialled for it.
fn dial(listening: Option<&Path>, port: u32) -> Result<std::os::unix::net::UnixStream, String> {
    let path = listening.ok_or("a dial from a VM granted no vsock path")?;
    let mut target = path.as_os_str().to_owned();
    target.push(format!("_{port}"));
    std::os::unix::net::UnixStream::connect(&target)
        .map_err(|e| format!("{}: {e}", Path::new(&target).display()))
}

#[cfg(target_os = "macos")]
/// Answers the VM on `link` as it asks, until it closes it: a bookmark for each path it
/// names, making the directories it writes in first. A path that cannot be granted ends
/// the answers with why.
pub fn serve(link: &std::os::unix::net::UnixStream, template: Option<&Path>) -> Result<(), String> {
    serve_waiting(link, template, &mut |_| {})
}

#[cfg(target_os = "macos")]
/// [`serve`], calling `wait` with how many messages the VM has sent before each wait for
/// its next: a caller's watch on a VM that goes quiet (the daemon's, `grant`).
pub fn serve_waiting(
    link: &std::os::unix::net::UnixStream,
    template: Option<&Path>,
    wait: &mut dyn FnMut(usize),
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
        wait(sent);
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
                Ok(Answer::File(file)) => {
                    let sent = shards_ipc::send(link, kind::GRANTED, &[], &[file.as_fd()]);
                    held.push(file.into());
                    sent
                }
                Ok(Answer::Bookmark(b)) => shards_ipc::send(link, kind::GRANTED, &b, &[]),
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

#[cfg(target_os = "macos")]
/// `shardsd grants`: the VM's broker (`shards run --kernel`, `shards restore`), which the CLI starts before it becomes the VM.
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
    /// Its process ID, which a socket is shared with.
    fn id(&self) -> u32;
}

#[cfg(windows)]
impl Recipient for shards_ipc::Child {
    fn give(&self, handle: std::os::windows::io::BorrowedHandle<'_>) -> std::io::Result<usize> {
        shards_ipc::Child::give(self, handle)
    }

    fn id(&self) -> u32 {
        shards_ipc::Child::id(self)
    }
}

#[cfg(windows)]
impl Recipient for shards_ipc::Process {
    fn give(&self, handle: std::os::windows::io::BorrowedHandle<'_>) -> std::io::Result<usize> {
        shards_ipc::Process::give(self, handle)
    }

    fn id(&self) -> u32 {
        shards_ipc::Process::id(self)
    }
}

/// What is granted for a path on Windows: a file opened for it, nothing where a file to
/// read if it is there is not, or a socket bound there.
#[cfg(windows)]
enum Handed {
    File(std::fs::File),
    Absent,
    Bound(shards_vmm::platform::UnixListener),
}

/// What `access` to `path` is granted on Windows. A directory to write in is not: WHP's
/// microVMs take no snapshots yet (D140), and a snapshot's is the only one a VM writes in.
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
        Access::ReadIfThere => match std::fs::File::open(path) {
            Ok(file) => Ok(Handed::File(file)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Handed::Absent),
            Err(e) => Err(at(e)),
        },
        Access::MakeDir => Err(format!(
            "{}: a microVM on Windows takes no snapshot yet",
            path.display()
        )),
        Access::Listen => bound(path).map(Handed::Bound).map_err(at),
    }
}

/// A socket bound at `path` for a VM's vsock device, not yet listening, as on macOS: a
/// client is refused until the VM listens. A socket file an earlier VM left there, which
/// nothing answers on, is replaced; one in use is not, nor anything that is no socket
/// file (AF_UNIX's files are reparse points).
#[cfg(windows)]
fn bound(path: &Path) -> std::io::Result<shards_vmm::platform::UnixListener> {
    use shards_vmm::platform::{UnixListener, UnixStream};
    use std::os::windows::fs::MetadataExt as _;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    match UnixListener::bound(path) {
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            let stale = std::fs::symlink_metadata(path)
                .is_ok_and(|m| m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
                && UnixStream::connect(path)
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::ConnectionRefused);
            if !stale {
                return Err(e);
            }
            std::fs::remove_file(path)?;
            UnixListener::bound(path)
        }
        made => made,
    }
}

/// A connection to vsock host port `port` beside `listening`, the vsock path the VM was
/// granted: the only sockets it may have dialled for it.
#[cfg(windows)]
fn dial_for(listening: Option<&Path>, port: u32) -> Result<shards_vmm::platform::UnixStream, String> {
    let path = listening.ok_or("a dial from a VM granted no vsock path")?;
    let mut target = path.as_os_str().to_owned();
    target.push(format!("_{port}"));
    shards_vmm::platform::UnixStream::connect(&target)
        .map_err(|e| format!("{}: {e}", Path::new(&target).display()))
}

/// Answers the VM on `link` as it asks, until it closes it, putting what it grants in
/// `to`, the VM's process: a file's handle and the path it was found at (the VM may not ask,
/// PM M203); a socket shared with it (WSADuplicateSocketW, PM M203). A path that cannot be
/// granted ends the answers with why. As on macOS, a VM that has its vsock path, the last
/// it asks for, has started, and asks for nothing more.
#[cfg(windows)]
pub fn serve(link: &crate::grant::Link, to: &dyn Recipient) -> Result<(), String> {
    use shards_ipc::kind;
    use std::os::windows::io::AsHandle as _;
    let mut listening: Option<PathBuf> = None;
    // Sockets sent with the last answer, held until the VM says it has them or asks again,
    // as macOS holds its descriptors: a connection shared works without (PM M203), and a
    // bound socket is held as well.
    let mut held: Vec<shards_vmm::platform::UnixStream> = Vec::new();
    let mut held_bound: Vec<shards_vmm::platform::UnixListener> = Vec::new();
    let mut started = false;
    let refuse = |e: String| {
        let _ = shards_ipc::send(&link.to, kind::ERR, e.as_bytes());
        Err(e)
    };
    loop {
        let asked = shards_ipc::recv(&link.from).map_err(|e| format!("a VM's request for access: {e}"))?;
        held.clear();
        held_bound.clear();
        let Some(asked) = asked else {
            return Ok(());
        };
        if asked.kind == kind::TAKEN {
            continue;
        }
        if asked.kind == kind::DIAL {
            let port = <[u8; 4]>::try_from(asked.payload.as_slice())
                .map(u32::from_be_bytes)
                .map_err(|_| "a VM sent a malformed dial".to_string())?;
            let sent = match dial_for(listening.as_deref(), port).and_then(|conn| {
                let info = conn
                    .share_with(to.id())
                    .map_err(|e| format!("sharing a dialled connection: {e}"))?;
                held.push(conn);
                Ok(info)
            }) {
                Ok(info) => shards_ipc::send(&link.to, kind::GRANTED, &info),
                Err(e) => shards_ipc::send(&link.to, kind::ERR, e.as_bytes()),
            };
            sent.map_err(|e| format!("answering a VM's dial: {e}"))?;
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
                Ok(Handed::Bound(socket)) => {
                    if listening.is_some() {
                        return refuse("a VM asked for a second vsock path".into());
                    }
                    match socket.share_with(to.id()) {
                        Ok(info) => {
                            listening = Some(path.clone());
                            held_bound.push(socket);
                            info
                        }
                        Err(e) => return refuse(format!("{}: sharing its socket: {e}", path.display())),
                    }
                }
                Err(e) => return refuse(e),
            };
            shards_ipc::send(&link.to, kind::GRANTED, &payload)
                .map_err(|e| format!("granting a VM {}: {e}", path.display()))?;
        }
    }
}

#[cfg(all(test, windows))]
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

        fn id(&self) -> u32 {
            std::process::id()
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
    /// there since; each also at the path its spawner found it at. A directory to write in
    /// is refused, with why, and the refusal ends the answers.
    #[test]
    fn a_spawner_grants_what_a_vm_asks_for() {
        use shards_vmm::platform::{input_path, open_input};
        let dir = shards_testdir::TempDir::new("grant").unwrap();
        let (kernel, disk, absent) = (dir.join("kernel"), dir.join("disk"), dir.join("absent"));
        std::fs::write(&kernel, b"k").unwrap();
        std::fs::write(&disk, b"d").unwrap();
        let (spawner, vm) = link();
        std::thread::scope(|scope| {
            let serving = scope.spawn(|| serve(&spawner, &Here));
            obtain(
                &vm,
                &[
                    (Access::Read, kernel.clone()),
                    (Access::Write, disk.clone()),
                    (Access::ReadIfThere, absent.clone()),
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
            let e = obtain(&vm, &[(Access::MakeDir, dir.join("snapshot"))]).unwrap_err();
            assert!(e.contains("no snapshot"), "{e}");
            drop(vm);
            assert!(serving.join().unwrap().is_err(), "the refusal ended its answers");
        });
    }

    /// A VM's vsock path is bound for it, replacing a socket an earlier VM left there that
    /// nothing answers on; its dials reach `<path>_<port>` and nothing else; a dial before
    /// any vsock path is granted, and a second vsock path, are refused; a VM that has
    /// started is granted nothing more.
    #[test]
    fn a_vms_vsock_is_bound_and_dialled_for_it() {
        use shards_vmm::platform::{UnixListener, UnixStream};
        let dir = shards_testdir::TempDir::new("dial").unwrap();
        let vsock = dir.join("v");
        drop(UnixListener::bound(&vsock).unwrap());
        assert!(vsock.exists(), "a socket file left behind");
        let host = UnixListener::bind(dir.join("v_5000")).unwrap();
        let (spawner, vm) = link();
        std::thread::scope(|scope| {
            let serving = scope.spawn(|| serve(&spawner, &Here));
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
            let _client = UnixStream::connect(&vsock).unwrap();
            assert!(listener.accept().is_ok(), "the granted listener is at the path");
            let dialled = dial(&vm, 5000).unwrap();
            let (accepted, ()) = host.accept().unwrap();
            (&dialled).write_all(b"x").unwrap();
            let mut byte = [0u8; 1];
            (&accepted).read_exact(&mut byte).unwrap();
            // The VM's close is the end the host sees: the spawner holds no copy open.
            accepted
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            dialled.shutdown(std::net::Shutdown::Write).unwrap();
            drop(dialled);
            assert_eq!((&accepted).read(&mut byte).map_err(|e| e.kind()), Ok(0));
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
        let _held = UnixListener::bind(&live).unwrap();
        let (spawner, vm) = link();
        std::thread::scope(|scope| {
            let serving = scope.spawn(|| serve(&spawner, &Here));
            assert!(obtain(&vm, &[(Access::Listen, live.clone())]).is_err());
            drop(vm);
            let _ = serving.join();
        });
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
            let serving = scope.spawn(|| serve_waiting(&spawner, None, &mut |sent| waits.push(sent)));
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
    /// not found, though a file appears there since; a directory to write in, made. A path
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
        let (vm, spawner) = std::os::unix::net::UnixStream::pair().unwrap();
        std::thread::scope(|scope| {
            let serving = scope.spawn(|| serve(&spawner, None));
            obtain(
                &vm,
                &[
                    (Access::Read, kernel.clone()),
                    (Access::Write, disk.clone()),
                    (Access::ReadIfThere, absent.clone()),
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
