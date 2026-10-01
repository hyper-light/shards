//! A spawner's half of a VM's grants (`grant`): it answers what the VM asks for with
//! descriptors, and with bookmarks for the directories it writes in.

use std::path::{Path, PathBuf};

use crate::grant::{Access, MAX_GRANTS, MAX_PATH, Wanted};

/// What [`encode`] wrote; `None` for anything else, past the limits included.
pub fn decode(mut bytes: &[u8]) -> Option<Vec<Wanted>> {
    use std::os::unix::ffi::OsStrExt;
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
            let path = PathBuf::from(std::ffi::OsStr::from_bytes(take(len)?));
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

mod cf {
    use std::ffi::c_void;

    pub type CFTypeRef = *const c_void;
    pub type CFAllocatorRef = *const c_void;
    pub type CFURLRef = *const c_void;
    pub type CFDataRef = *const c_void;
    pub type CFErrorRef = *const c_void;
    pub type CFArrayRef = *const c_void;
    pub type CFIndex = isize;
    pub type Boolean = u8;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        pub fn CFURLCreateFromFileSystemRepresentation(
            allocator: CFAllocatorRef,
            buffer: *const u8,
            len: CFIndex,
            is_directory: Boolean,
        ) -> CFURLRef;
        pub fn CFURLCreateBookmarkData(
            allocator: CFAllocatorRef,
            url: CFURLRef,
            options: usize,
            properties: CFArrayRef,
            relative_to: CFURLRef,
            error: *mut CFErrorRef,
        ) -> CFDataRef;
        pub fn CFDataGetBytePtr(data: CFDataRef) -> *const u8;
        pub fn CFDataGetLength(data: CFDataRef) -> CFIndex;
        pub fn CFRelease(cf: CFTypeRef);
    }
}

/// A read-write bookmark for directory `path`, for another process to resolve (options 0
/// grant "access to the resource to a process that resolves the bookmark").
pub fn bookmark(path: &Path) -> Result<Vec<u8>, String> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    let at = |what: &str| format!("{}: {what}", path.display());
    let len = isize::try_from(bytes.len()).map_err(|_| at("a path too long"))?;
    // SAFETY: a buffer of `len` bytes; the URL is released below.
    let url =
        unsafe { cf::CFURLCreateFromFileSystemRepresentation(std::ptr::null(), bytes.as_ptr(), len, 1) };
    if url.is_null() {
        return Err(at("no URL for it"));
    }
    let mut error: cf::CFErrorRef = std::ptr::null();
    // SAFETY: a URL we own; the data, if made, is released below.
    let data = unsafe {
        cf::CFURLCreateBookmarkData(
            std::ptr::null(),
            url,
            0,
            std::ptr::null(),
            std::ptr::null(),
            &mut error,
        )
    };
    // SAFETY: each reference is ours to release, once.
    unsafe {
        cf::CFRelease(url);
        if !error.is_null() {
            cf::CFRelease(error);
        }
    }
    if data.is_null() {
        return Err(at("no bookmark for it"));
    }
    // SAFETY: CFData's bytes, valid while `data` is, copied before it is released.
    let out = unsafe {
        let n = usize::try_from(cf::CFDataGetLength(data)).unwrap_or(0);
        let b = std::slice::from_raw_parts(cf::CFDataGetBytePtr(data), n).to_vec();
        cf::CFRelease(data);
        b
    };
    Ok(out)
}

/// What is granted for `path` as `access`: a file opened for it, a directory's bookmark,
/// nothing, where a file to read if it is there is not, or a socket bound there.
enum Answer {
    File(std::fs::File),
    Bookmark(Vec<u8>),
    Absent,
    Bound(std::os::fd::OwnedFd),
}

fn answer(access: Access, path: &Path) -> Result<Answer, String> {
    let at = |e: std::io::Error| format!("{}: {e}", path.display());
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

/// A connection to vsock host port `port` beside `listening`, the vsock path the VM was
/// granted: the only sockets it may have dialled for it.
fn dial(listening: Option<&Path>, port: u32) -> Result<std::os::unix::net::UnixStream, String> {
    let path = listening.ok_or("a dial from a VM granted no vsock path")?;
    let mut target = path.as_os_str().to_owned();
    target.push(format!("_{port}"));
    std::os::unix::net::UnixStream::connect(&target)
        .map_err(|e| format!("{}: {e}", Path::new(&target).display()))
}

/// Answers the VM on `link` as it asks, until it closes it: a bookmark for each path it
/// names, making the directories it writes in first. A path that cannot be granted ends
/// the answers with why.
pub fn serve(link: &std::os::unix::net::UnixStream) -> Result<(), String> {
    use shards_ipc::kind;
    use std::os::fd::AsFd as _;
    // The vsock path granted, beside which alone dials go.
    let mut listening: Option<PathBuf> = None;
    // What was sent with the last answers, held until the VM asks again or goes: XNU's
    // collector flushes a socket in flight that no process holds (M24), and a flushed
    // socket reads as ended. The VM asks again only once it has received them.
    let mut held: Vec<std::os::fd::OwnedFd> = Vec::new();
    loop {
        let asked = shards_ipc::recv(link).map_err(|e| format!("a VM's request for access: {e}"))?;
        held.clear();
        let Some(asked) = asked else {
            return Ok(());
        };
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
        for (access, path) in wanted {
            let sent = match answer(access, &path) {
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

/// `shardsd grants`: `shards vm`'s broker, which the CLI starts before it becomes the VM.
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
    match serve(&link) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(_) => std::process::ExitCode::FAILURE,
    }
}

#[cfg(test)]
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

    /// A VM's request is answered as asked: a file it reads, read-only (it reads, and
    /// cannot write); one it writes, read-write; one to read if it is there and is not,
    /// not found, though a file appears there since; a directory to write in, made. A path
    /// that cannot be granted ends the answers with why.
    #[test]
    fn a_spawner_grants_what_a_vm_asks_for() {
        use shards_vmm::platform::open_input;
        use std::io::{Read as _, Write as _};
        let dir = std::env::temp_dir().join(format!("shards-grant-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
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
            let serving = scope.spawn(|| serve(&spawner));
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

    /// A VM's vsock path is bound for it, replacing a socket an earlier VM left there that
    /// nothing answers on; its dials reach `<path>_<port>` and nothing else; a dial before
    /// any vsock path is granted, and a second vsock path, are refused; a path in use is not
    /// taken over.
    #[test]
    fn a_vms_vsock_is_bound_and_dialled_for_it() {
        use std::os::unix::net::{UnixListener, UnixStream};
        let dir = std::env::temp_dir().join(format!("shards-dial-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let vsock = dir.join("v");
        drop(UnixListener::bind(&vsock).unwrap());
        assert!(vsock.exists(), "a socket file left behind");
        let host = UnixListener::bind(dir.join("v_5000")).unwrap();
        let (vm, spawner) = UnixStream::pair().unwrap();
        std::thread::scope(|scope| {
            let serving = scope.spawn(|| serve(&spawner));
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
            let (accepted, _) = host.accept().unwrap();
            assert_eq!(
                dialled.peer_addr().unwrap().as_pathname(),
                Some(dir.join("v_5000").as_path())
            );
            drop(accepted);
            assert!(dial(&vm, 5001).is_err(), "nothing listens on that port");
            let second = obtain(&vm, &[(Access::Listen, dir.join("w"))]);
            assert!(second.is_err(), "a second vsock path");
            drop(vm);
            assert!(serving.join().unwrap().is_err());
        });
        // One in use is not taken over.
        let (vm, spawner) = UnixStream::pair().unwrap();
        let live = dir.join("live");
        let _held = UnixListener::bind(&live).unwrap();
        std::thread::scope(|scope| {
            let serving = scope.spawn(|| serve(&spawner));
            assert!(obtain(&vm, &[(Access::Listen, live.clone())]).is_err());
            drop(vm);
            let _ = serving.join();
        });
        let _ = std::fs::remove_dir_all(&dir);
    }
}
