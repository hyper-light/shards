//! A vsock connection's pipes on Windows (D272): two byte-mode instances, its way down, which
//! its client reads, and its way up, which its client writes, named by a file at the socket
//! path a Unix host keeps there (a VM's `--vsock PATH`, or a host port's `<PATH>_<port>`),
//! which its clients read and open, the way down first. Every party takes them from here: a
//! VM's spawner, which makes its listener's pairs and opens its dials (`shards`
//! grant_answer.rs); a VM process granted none, which makes its own; `shards_api`'s client;
//! and the tests' host ports.

use std::io::{self, Read as _, Write as _};
use std::os::windows::io::{AsRawHandle as _, OwnedHandle};
use std::path::{Path, PathBuf};
use std::time::Instant;

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_SEM_TIMEOUT};
use windows_sys::Win32::System::Pipes::{NMPWAIT_WAIT_FOREVER, WaitNamedPipeW};

/// What each way of a vsock connection's pipes holds before its writer waits on its reader,
/// in bytes: what its host socket buffers each way on every other host (shards_vmm's
/// muxer.rs `SOCKET_BUFFER`: measured on Hypervisor.framework, docs/research/measurements/
/// vsock-window-v1, r433), for the same bytes in flight between the device and its host
/// program. Unmeasured over pipes: WHP's measurement is owed (D272).
pub const VSOCK_QUOTA: u32 = 256 << 10;

/// The most bytes a pipe's name takes: 256 characters ("The entire pipe name string can be
/// up to 256 characters long", CreateNamedPipeW), each at most three bytes of UTF-8 (RFC 3629
/// §3; a surrogate pair's two units, four).
const NAME_MOST: usize = 256 * 3;

/// The most bytes a pair's file holds: its two names, each with its newline.
const FILE_MOST: usize = 2 * (NAME_MOST + 1);

/// What every vsock pipe's name begins with: the local machine's pipes.
const PIPES: &str = r"\\.\pipe\";

/// A pair's names, its way down's and its way up's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VsockNames {
    pub down: String,
    pub up: String,
}

impl VsockNames {
    /// Names no other process guesses (64 bits of [`super::random_hex`], as shards' own
    /// pipes' are): `\\.\pipe\shards-vsock-<pid>-<hex>-down` and `-up`.
    pub fn fresh() -> io::Result<VsockNames> {
        let base = format!(
            r"{PIPES}shards-vsock-{}-{}",
            std::process::id(),
            super::random_hex(8)?
        );
        Ok(VsockNames {
            down: format!("{base}-down"),
            up: format!("{base}-up"),
        })
    }

    /// As their file holds them: the way down's name and a newline, then the way up's.
    pub fn file_text(&self) -> String {
        format!("{}\n{}\n", self.down, self.up)
    }

    /// The names a file's bytes hold, where they hold two names of this machine's pipes, each
    /// ended by a newline, and nothing else.
    pub fn parse(bytes: &[u8]) -> Option<VsockNames> {
        let text = std::str::from_utf8(bytes).ok()?;
        let (down, rest) = text.split_once('\n')?;
        let (up, rest) = rest.split_once('\n')?;
        let named = |n: &str| {
            n.len() > PIPES.len() && n.len() <= NAME_MOST && n.starts_with(PIPES) && !n.contains('\0')
        };
        (rest.is_empty() && named(down) && named(up) && down != up).then(|| VsockNames {
            down: down.to_string(),
            up: up.to_string(),
        })
    }

    /// The names the file at `path` holds, read within what two names take.
    pub fn read(path: &Path) -> io::Result<VsockNames> {
        let mut bytes = Vec::new();
        let most = u64::try_from(FILE_MOST).unwrap_or(u64::MAX).saturating_add(1);
        std::fs::File::open(path)?.take(most).read_to_end(&mut bytes)?;
        if bytes.len() > FILE_MOST {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: longer than two pipes' names", path.display()),
            ));
        }
        VsockNames::parse(&bytes).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: names no vsock pipes", path.display()),
            )
        })
    }
}

/// An instance of the vsock pipe `name`, its server end: duplex, so that its way down is
/// watched for its client's going while its way up waits (D272); in bytes; overlapped; this
/// user's alone ([`super::attributes`]), remote clients refused; [`VSOCK_QUOTA`] each way; its
/// name's first where `first` (FILE_FLAG_FIRST_PIPE_INSTANCE), so that no other process
/// holds the name before it.
pub fn vsock_instance(name: &str, first: bool) -> io::Result<OwnedHandle> {
    let quota = usize::try_from(VSOCK_QUOTA).map_err(io::Error::other)?;
    super::instance(
        &super::wide(name),
        first,
        false,
        crate::Window::frame(quota)?,
        false,
    )
}

/// An instance of each of `names`: the way down's, then the way up's.
pub fn vsock_pair(names: &VsockNames, first: bool) -> io::Result<(OwnedHandle, OwnedHandle)> {
    Ok((
        vsock_instance(&names.down, first)?,
        vsock_instance(&names.up, first)?,
    ))
}

/// A listener's first pair at `path`, under fresh names its file there says, written whole
/// before it is there to read (a sibling's link). A file an earlier listener left, whose
/// pipes are gone, is replaced; one whose pipes are there is not (`AddrInUse`), nor anything
/// at `path` that names no pipes.
pub fn vsock_listen(path: &Path) -> io::Result<(VsockNames, OwnedHandle, OwnedHandle)> {
    match VsockNames::read(path) {
        Ok(old) => {
            if pipe_exists(&old.down) || pipe_exists(&old.up) {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("{}: its pipes are in use", path.display()),
                ));
            }
            std::fs::remove_file(path)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let names = VsockNames::fresh()?;
    let (down, up) = vsock_pair(&names, true)?;
    let written = sibling(path)?;
    let placed =
        write_new(&written, names.file_text().as_bytes()).and_then(|()| std::fs::hard_link(&written, path));
    let _ = std::fs::remove_file(&written);
    placed.map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
    Ok((names, down, up))
}

/// A path beside `path`, of a name no other writer takes, for its file to be written at
/// before it is linked in.
fn sibling(path: &Path) -> io::Result<PathBuf> {
    let mut name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "a vsock path names no file"))?
        .to_os_string();
    name.push(format!(".{}.new", super::random_hex(8)?));
    Ok(path.with_file_name(name))
}

/// `bytes` as a new file at `path`, never over another's.
fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes)
}

/// Whether the pipe `name` has an instance: WaitNamedPipeW answers at once where it has none
/// (ERROR_FILE_NOT_FOUND), whatever its wait, and connects to none either way.
fn pipe_exists(name: &str) -> bool {
    let wide = super::wide(name);
    // SAFETY: a NUL-terminated name; the least wait, which only a busy instance takes.
    if unsafe { WaitNamedPipeW(wide.as_ptr(), 1) } != 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() != i32::try_from(ERROR_FILE_NOT_FOUND).ok()
}

/// The client ends of the pair the file at `path` names, opened as D272's client opens them:
/// its way down first, then its way up, each waited for while busy (ERROR_PIPE_BUSY,
/// WaitNamedPipeW) until `deadline` (`TimedOut` past it; one now waits for nothing), for
/// ever with none; for overlapped calls, its server allowed to identify this process and no
/// more ([`super::open`]). Its listener gone, its pipes with it: `ConnectionRefused`, as a
/// socket no one listens on.
pub fn vsock_open(path: &Path, deadline: Option<Instant>) -> io::Result<(OwnedHandle, OwnedHandle)> {
    let names = VsockNames::read(path)?;
    let down = open_waiting(&names.down, deadline)?;
    let up = open_waiting(&names.up, deadline)?;
    Ok((down, up))
}

/// The client end of the pipe `name`, waited for while busy until `deadline`.
fn open_waiting(name: &str, deadline: Option<Instant>) -> io::Result<OwnedHandle> {
    let wide = super::wide(name);
    let code = |e: &io::Error, code: u32| e.raw_os_error() == i32::try_from(code).ok();
    loop {
        match super::open(&wide, true) {
            Err(e) if code(&e, ERROR_PIPE_BUSY) => {}
            Err(e) if code(&e, ERROR_FILE_NOT_FOUND) => {
                return Err(io::Error::new(io::ErrorKind::ConnectionRefused, e));
            }
            opened => return opened,
        }
        let ms = match deadline {
            None => NMPWAIT_WAIT_FOREVER,
            Some(at) => {
                let left = at.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(io::ErrorKind::TimedOut.into());
                }
                // Rounded up: a wait of 0 is the pipe's default wait, not none.
                u32::try_from(left.as_micros().div_ceil(1000))
                    .unwrap_or(NMPWAIT_WAIT_FOREVER - 1)
                    .clamp(1, NMPWAIT_WAIT_FOREVER - 1)
            }
        };
        // SAFETY: a NUL-terminated name. A free instance is the next open's to find, which
        // another client may take first.
        if unsafe { WaitNamedPipeW(wide.as_ptr(), ms) } == 0 {
            let e = io::Error::last_os_error();
            if code(&e, ERROR_FILE_NOT_FOUND) {
                return Err(io::Error::new(io::ErrorKind::ConnectionRefused, e));
            }
            if !code(&e, ERROR_SEM_TIMEOUT) {
                return Err(e);
            }
        }
    }
}

/// A host program's listener at a path (D272): one pair at a time, named by its file there,
/// the next made as a client has opened both ways of the last, its way down first. A host port
/// a guest dials (`<path>_<port>`) is listened at so, its VM's spawner the client.
#[derive(Debug)]
pub struct VsockListener {
    names: VsockNames,
    /// The pair its next client opens: none where it could not be made, made again by the
    /// next [`accept`](VsockListener::accept).
    pair: Option<(OwnedHandle, OwnedHandle)>,
}

/// A listener's side of a client's connection: the way down, which it writes, and the way
/// up, which it reads, each a server end opened for overlapped calls.
#[derive(Debug)]
pub struct VsockConn {
    pub down: super::Pipe,
    pub up: super::Pipe,
}

impl VsockListener {
    /// Listens at `path` ([`vsock_listen`]).
    pub fn bind(path: &Path) -> io::Result<VsockListener> {
        let (names, down, up) = vsock_listen(path)?;
        Ok(VsockListener {
            names,
            pair: Some((down, up)),
        })
    }

    /// The next client's connection, waited for until it has opened both ways (ConnectNamedPipe,
    /// for ever), its way down first; the pair after it made at once, so that the next client
    /// waits on its way down (ERROR_PIPE_BUSY) no longer than this takes.
    pub fn accept(&mut self) -> io::Result<VsockConn> {
        let (down, up) = match self.pair.take() {
            Some(pair) => pair,
            None => vsock_pair(&self.names, false)?,
        };
        for end in [&down, &up] {
            if !super::connected(end.as_raw_handle(), &[])? {
                return Err(io::ErrorKind::Interrupted.into());
            }
        }
        self.pair = vsock_pair(&self.names, false).ok();
        Ok(VsockConn {
            down: super::Pipe::from_handle(down),
            up: super::Pipe::from_handle(up),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A pair's file says its two names, the way down's first, and is read back as written;
    /// anything else is refused: a name that is no pipe's, one name, a third, the same name
    /// twice, a name past its bound.
    #[test]
    fn a_pairs_file_says_its_two_names() {
        let names = VsockNames::fresh().unwrap();
        assert!(names.down.starts_with(PIPES) && names.down.ends_with("-down"));
        assert_eq!(
            VsockNames::parse(names.file_text().as_bytes()),
            Some(names.clone())
        );
        let long = format!("{PIPES}{}", "x".repeat(NAME_MOST));
        for bad in [
            String::new(),
            format!("{}\n", names.down),
            format!("{}\n{}", names.down, names.up),
            format!("{}\n{}\n{}\n", names.down, names.up, names.up),
            format!("{0}\n{0}\n", names.down),
            "C:\\x\nC:\\y\n".to_string(),
            format!("{long}\n{}\n", names.up),
        ] {
            assert_eq!(VsockNames::parse(bad.as_bytes()), None, "{bad:?}");
        }
    }

    /// A listener's pair is opened by its file, its way down first: its client reads what is
    /// written down and writes what is read up; a second listener at the path is refused while
    /// the first's pipes are there, and replaces its file once they are gone; a path whose
    /// listener has gone refuses its dial.
    #[test]
    fn a_pair_is_opened_by_its_file() {
        let dir = shards_testdir::TempDir::new("vsock-pair").unwrap();
        let path = dir.join("v");
        let (names, down, up) = vsock_listen(&path).unwrap();
        assert_eq!(VsockNames::read(&path).unwrap(), names);
        let (their_down, their_up) = vsock_open(&path, None).unwrap();
        let (ours, theirs) = (
            (
                super::super::Pipe::from_handle(down),
                super::super::Pipe::from_handle(up),
            ),
            (
                super::super::Pipe::from_handle(their_down),
                super::super::Pipe::from_handle(their_up),
            ),
        );
        (&ours.0).write_all(b"down").unwrap();
        let mut got = [0u8; 4];
        (&theirs.0).read_exact(&mut got).unwrap();
        assert_eq!(&got, b"down");
        (&theirs.1).write_all(b"up").unwrap();
        let mut got = [0u8; 2];
        (&ours.1).read_exact(&mut got).unwrap();
        assert_eq!(&got, b"up");
        assert_eq!(
            vsock_listen(&path).map(drop).unwrap_err().kind(),
            io::ErrorKind::AddrInUse
        );
        drop((ours, theirs));
        let (again, _, _) = vsock_listen(&path).unwrap();
        assert_ne!(again, names, "a listener's names are its own");
        assert_eq!(
            vsock_open(&dir.join("absent"), None)
                .map(drop)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        drop(again);
        let (gone, down, up) = vsock_listen(&dir.join("w")).unwrap();
        drop((down, up));
        assert_eq!(
            vsock_open(&dir.join("w"), None).map(drop).unwrap_err().kind(),
            io::ErrorKind::ConnectionRefused,
            "{gone:?}"
        );
    }

    /// A pair whose instances are taken is busy: a dial that waits for nothing is refused at
    /// once (`TimedOut`), and one that waits is let in as its listener makes the next.
    #[test]
    fn a_busy_pair_is_waited_for_until_its_deadline() {
        let dir = shards_testdir::TempDir::new("vsock-busy").unwrap();
        let path = dir.join("v");
        let (names, _down, _up) = vsock_listen(&path).unwrap();
        let _first = vsock_open(&path, None).unwrap();
        let now = Some(Instant::now());
        assert_eq!(
            vsock_open(&path, now).map(drop).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        let next = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            vsock_pair(&names, false).unwrap()
        });
        let later = Instant::now() + std::time::Duration::from_secs(10);
        let opened = vsock_open(&path, Some(later));
        let _next = next.join().unwrap();
        opened.unwrap();
    }

    /// A listener lets its clients in one after another, each its own pair: the first's way
    /// down and way up carry its bytes, and the second, opened as the first's pair is taken,
    /// is let in by the next accept.
    #[test]
    fn a_listener_lets_its_clients_in_one_after_another() {
        let dir = shards_testdir::TempDir::new("vsock-listener").unwrap();
        let path = dir.join("v_5000");
        let mut listener = VsockListener::bind(&path).unwrap();
        let first = std::thread::spawn({
            let path = path.clone();
            move || vsock_open(&path, None).unwrap()
        });
        let conn = listener.accept().unwrap();
        let (their_down, their_up) = first.join().unwrap();
        (&conn.down).write_all(b"one").unwrap();
        let mut got = [0u8; 3];
        (&super::super::Pipe::from_handle(their_down))
            .read_exact(&mut got)
            .unwrap();
        assert_eq!(&got, b"one");
        (&super::super::Pipe::from_handle(their_up))
            .write_all(b"up")
            .unwrap();
        let mut got = [0u8; 2];
        (&conn.up).read_exact(&mut got).unwrap();
        assert_eq!(&got, b"up");
        let second = std::thread::spawn({
            let path = path.clone();
            move || vsock_open(&path, None).unwrap()
        });
        let _second = listener.accept().unwrap();
        drop(second.join().unwrap());
    }
}
