//! A context's client against a daemon of the test's own (D180), on every system shards'
//! client runs on, Windows' among them, where no daemon of shards' runs yet: what the client
//! sends (its arguments, the variables that shape what it prints, its stdin, its terminal),
//! and what it makes of what comes back (the command's output and status). On Windows, the
//! client on a console of the test's, a pseudoconsole (ConPTY), as on a user's: raw, the size
//! it is, keys as they were typed, and a resize.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::io::{Read, Write};
use std::path::Path;
use std::process::Stdio;
use std::sync::mpsc;
use std::time::Duration;

use common::TempDir;
use shards_remote::Error;
use shards_remote::client::Remote;
use shards_remote::frame::Request;
use shards_remote::identity::{self, Identity};
use shards_remote::server::{self, Host, Running, Stopper};

const TIMEOUT: Duration = Duration::from_secs(60);

/// What the test's daemon saw. A command's start comes with where its output and its exit
/// status come from: the test.
#[derive(Debug)]
enum Seen {
    Started(Request, mpsc::Sender<Vec<u8>>, mpsc::Sender<i32>),
    Stdin(Vec<u8>),
    // Read on Windows alone, whose test has its client on a console the test holds; Unix's
    // are tests/context.rs's, on a pty.
    #[cfg_attr(unix, allow(dead_code))]
    Resized(u16, u16),
    // Read on Unix alone: a test signals another process's console's group on Windows only
    // from that console (GenerateConsoleCtrlEvent).
    #[cfg_attr(windows, allow(dead_code))]
    Signalled(u32),
    /// The command was hung up: its client went before it ended.
    HungUp,
}

/// A machine that has enrolled one client, `laptop`, and runs no command of its own: each is
/// the test's to answer.
struct Machine {
    laptop: Vec<u8>,
    seen: mpsc::Sender<Seen>,
}

/// A command's stdin, which tells the test what it is given.
struct Told(mpsc::Sender<Seen>);

impl Write for Told {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = self.0.send(Seen::Stdin(buf.to_vec()));
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A command's stdout, which writes what the test sends it, and ends when the test lets go.
struct Given(mpsc::Receiver<Vec<u8>>, Vec<u8>);

impl Read for Given {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.1.is_empty() {
            match self.0.recv() {
                Ok(bytes) => self.1 = bytes,
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.1.len());
        buf[..n].copy_from_slice(&self.1[..n]);
        self.1.drain(..n);
        Ok(n)
    }
}

impl Host for Machine {
    fn enrolled(&self, spki: &[u8]) -> Option<String> {
        (spki == self.laptop.as_slice()).then(|| "laptop".to_string())
    }

    fn join(&self, _name: &str, _proof: &[u8], _exported: &[u8; 32], _spki: &[u8]) -> Result<(), Error> {
        Err(Error::new("no"))
    }

    fn start(&self, _client: &str, request: Request) -> Result<Running, Error> {
        let (output, given) = mpsc::channel();
        let (exit, status) = mpsc::channel();
        let _ = self.seen.send(Seen::Started(request, output, exit));
        let (resized, signalled, hung) = (self.seen.clone(), self.seen.clone(), self.seen.clone());
        Ok(Running {
            stdin: Box::new(Told(self.seen.clone())),
            stdout: Box::new(Given(given, Vec::new())),
            stderr: Box::new(std::io::empty()),
            wait: Box::new(move || status.recv().unwrap_or(1)),
            hangup: Box::new(move || {
                let _ = hung.send(Seen::HungUp);
            }),
            resize: Some(Box::new(move |rows, cols| {
                let _ = resized.send(Seen::Resized(rows, cols));
            })),
            signal: Box::new(move |linux| {
                let _ = signalled.send(Seen::Signalled(linux));
            }),
        })
    }

    fn invited(&self) -> usize {
        0
    }
}

/// The test's daemon, on loopback, stopped as it drops.
struct Daemon {
    stopper: Stopper,
    seen: mpsc::Receiver<Seen>,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.stopper.stop();
    }
}

impl Daemon {
    fn next(&self) -> Seen {
        self.seen
            .recv_timeout(TIMEOUT)
            .unwrap_or_else(|_| panic!("the daemon saw nothing within {TIMEOUT:?}"))
    }

    /// The stdin the command was given until it holds `want`, which it must begin with; a
    /// resize meanwhile, as a console's size is looked at as it changes, passed over.
    fn stdin(&self, want: &[u8]) {
        let mut got = Vec::new();
        while got.len() < want.len() {
            match self.next() {
                Seen::Stdin(bytes) => got.extend_from_slice(&bytes),
                Seen::Resized(..) => {}
                other => panic!("{other:?}, waiting for stdin {want:?}"),
            }
        }
        assert_eq!(got, want);
    }
}

/// A daemon of the test's, and `laptop`'s context `gpu` of it, as `create context` writes
/// one, the laptop's identity enrolled there.
fn daemon(laptop: &Path) -> Daemon {
    let ours: &'static Identity = Box::leak(Box::new(Identity::generate().unwrap()));
    let client = Identity::load_or_create(&laptop.join("identity")).unwrap();
    let (seen, rx) = mpsc::channel();
    let machine = Machine {
        laptop: client.spki().to_vec(),
        seen,
    };
    let (tx, bound) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = server::serve(ours, "127.0.0.1:0".parse().unwrap(), machine, |at, stopper| {
            let _ = tx.send((at, stopper));
        });
    });
    let (address, stopper) = bound.recv_timeout(TIMEOUT).unwrap();
    let remote = Remote {
        address,
        daemon: ours.fingerprint(),
    };
    std::fs::create_dir_all(laptop.join("contexts")).unwrap();
    std::fs::write(
        laptop.join("contexts/gpu"),
        format!(
            "shards context 1\nname gpu\naddress {}\ndaemon {}\ndescription \n",
            remote.address,
            identity::hex(&remote.daemon)
        ),
    )
    .unwrap();
    Daemon { stopper, seen: rx }
}

fn args(request: &Request) -> Vec<String> {
    request
        .args
        .iter()
        .map(|a| String::from_utf8(a.clone()).unwrap())
        .collect()
}

/// With no terminal, the client sends its command whole, its stdin as it comes and the
/// variables that shape what the command prints, says what the command wrote, and exits as
/// it exits.
#[test]
fn a_contexts_client_sends_its_command_and_takes_what_comes_back() {
    let laptop = TempDir::new("cc-piped");
    let d = daemon(&laptop);
    let mut child = common::command()
        .args(["--context", "gpu", "run", "--rm", "image", "echo", "hi"])
        .env("SHARDS_HOME", laptop.as_os_str())
        .env("COLUMNS", "123")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let Seen::Started(request, output, exit) = d.next() else {
        panic!("no start");
    };
    assert_eq!(args(&request), ["run", "--rm", "image", "echo", "hi"]);
    assert!(
        request.env.contains(&("COLUMNS".to_string(), b"123".to_vec())),
        "{:?}",
        request.env
    );
    assert_eq!(request.terminal, None, "pipes, not a terminal");
    child.stdin.take().unwrap().write_all(b"given").unwrap();
    d.stdin(b"given");
    // A signal to the client reaches the command by its Linux number: SIGUSR1 is 30 on
    // macOS.
    #[cfg(unix)]
    {
        // SAFETY: kill(2) of our own child, which is only reaped below.
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGUSR1) };
        match d.next() {
            Seen::Signalled(linux) => assert_eq!(linux, 10, "SIGUSR1's Linux number"),
            other => panic!("{other:?}, waiting for the signal"),
        }
    }
    output.send(b"written\n".to_vec()).unwrap();
    drop(output);
    exit.send(3).unwrap();
    let mut out = String::new();
    child.stdout.take().unwrap().read_to_string(&mut out).unwrap();
    assert_eq!(out, "written\n");
    assert_eq!(child.wait().unwrap().code(), Some(3));
}

/// The names in `dir`, sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// `save -o FILE` over a context, in `dir`.
fn saving(laptop: &Path, dir: &Path, file: &str) -> std::process::Child {
    common::command()
        .current_dir(dir)
        .args(["--context", "gpu", "save", "-o", file, "image"])
        .env("SHARDS_HOME", laptop.as_os_str())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// What `child` ended with: its status, and what it said on stderr.
fn ended(child: std::process::Child) -> (Option<i32>, String) {
    let done = child.wait_with_output().unwrap();
    assert!(
        done.stdout.is_empty(),
        "{:?}",
        String::from_utf8_lossy(&done.stdout)
    );
    (
        done.status.code(),
        String::from_utf8_lossy(&done.stderr).into_owned(),
    )
}

/// A command whose output nobody reads here any more ends there: the client, its stdout's
/// reader gone as `head -1`'s goes, hangs the command up, where it would read the rest and
/// throw it away for as long as the command ran (`logs -f`, for ever), and ends with status 1,
/// saying nothing, as a command here does whose reader hung up.
#[test]
fn a_command_whose_output_nobody_reads_is_hung_up_there() {
    let laptop = TempDir::new("cc-gone");
    let d = daemon(&laptop);
    let mut child = common::command()
        .args(["--context", "gpu", "logs", "-f", "web"])
        .env("SHARDS_HOME", laptop.as_os_str())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let Seen::Started(request, output, _exit) = d.next() else {
        panic!("no start");
    };
    assert_eq!(args(&request), ["logs", "-f", "web"]);
    output.send(b"first\n".to_vec()).unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut first = [0u8; 6];
    stdout.read_exact(&mut first).unwrap();
    assert_eq!(&first, b"first\n");
    drop(stdout);
    // It goes on printing, as a followed log does: a write may still find a reader a while
    // (a child another test forks holds the pipe until its exec), the next finds none.
    let deadline = std::time::Instant::now() + TIMEOUT;
    loop {
        assert!(std::time::Instant::now() < deadline, "never hung up");
        let _ = output.send(b"more\n".to_vec());
        match d.seen.recv_timeout(Duration::from_millis(20)) {
            Ok(Seen::HungUp) => break,
            Ok(Seen::Stdin(bytes)) if bytes.is_empty() => {}
            Ok(other) => panic!("{other:?}, waiting for the hangup"),
            Err(_) => {}
        }
    }
    let (status, said) = ended(child);
    assert_eq!(status, Some(1), "{said}");
    assert_eq!(said, "", "nothing said of a reader gone");
}

/// `save -o`'s file is the client's, as Docker's CLI writes its own: the command there
/// writes the archive to its stdout, and the client keeps it in its file once the command's
/// exit says it is whole. A command that fails, a signal that ends it and a connection lost
/// leave the file as it was, and nothing beside it, where Docker's atomic writer renames
/// what it has into place unless a write failed (moby/sys atomicwriter, Close).
#[test]
fn a_file_the_command_writes_is_the_clients_whole_or_not_at_all() {
    let laptop = TempDir::new("cc-save");
    let files = laptop.join("files");
    std::fs::create_dir_all(&files).unwrap();
    let d = daemon(&laptop);
    // Whole: kept, the command there told to write its stdout.
    let child = saving(&laptop, &files, "whole.tar");
    let Seen::Started(request, output, exit) = d.next() else {
        panic!("no start");
    };
    assert_eq!(args(&request), ["save", "image"]);
    assert_eq!(request.terminal, None);
    output.send(b"an archive".to_vec()).unwrap();
    drop(output);
    exit.send(0).unwrap();
    let (status, said) = ended(child);
    assert_eq!(status, Some(0), "{said}");
    assert_eq!(std::fs::read(files.join("whole.tar")).unwrap(), b"an archive");
    assert_eq!(names(&files), ["whole.tar"]);
    // A command that fails: the file there before keeps what it held.
    std::fs::write(files.join("kept.tar"), b"before").unwrap();
    let child = saving(&laptop, &files, "kept.tar");
    let Seen::Started(_, output, exit) = d.next() else {
        panic!("no start");
    };
    output.send(b"half".to_vec()).unwrap();
    drop(output);
    exit.send(1).unwrap();
    assert_eq!(ended(child).0, Some(1));
    assert_eq!(std::fs::read(files.join("kept.tar")).unwrap(), b"before");
    assert_eq!(names(&files), ["kept.tar", "whole.tar"]);
    // A signal that ends the command, sent on as it comes.
    #[cfg(unix)]
    {
        let child = saving(&laptop, &files, "signalled.tar");
        let Seen::Started(_, output, exit) = d.next() else {
            panic!("no start");
        };
        output.send(b"half".to_vec()).unwrap();
        // SAFETY: kill(2) of our own child, which is only reaped below.
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
        match d.next() {
            Seen::Signalled(linux) => assert_eq!(linux, 15),
            other => panic!("{other:?}, waiting for the signal"),
        }
        drop(output);
        exit.send(143).unwrap();
        assert_eq!(ended(child).0, Some(143));
        assert_eq!(names(&files), ["kept.tar", "whole.tar"]);
    }
    // The connection lost, half the archive written here.
    let child = saving(&laptop, &files, "lost.tar");
    let Seen::Started(_, output, _exit) = d.next() else {
        panic!("no start");
    };
    output.send(b"half".to_vec()).unwrap();
    let start = std::time::Instant::now();
    while !names(&files).iter().any(|n| {
        n.starts_with(".tmp-lost.tar") && std::fs::metadata(files.join(n)).is_ok_and(|m| m.len() == 4)
    }) {
        assert!(start.elapsed() < TIMEOUT, "no half written: {:?}", names(&files));
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(d);
    let (status, said) = ended(child);
    assert_eq!(status, Some(125), "{said}");
    assert_eq!(names(&files), ["kept.tar", "whole.tar"]);
}

/// A write here that fails keeps nothing, though the command there ends as a whole
/// archive's does: the client says what failed and exits 1. A file size limit of 4 bytes
/// (setrlimit(2), RLIMIT_FSIZE) fails the write with EFBIG; the SIGXFSZ it raises is sent on
/// to the command, as every signal the client takes is.
#[cfg(unix)]
#[test]
fn a_file_not_written_whole_here_is_kept_by_no_exit() {
    use std::os::unix::process::CommandExt as _;
    let laptop = TempDir::new("cc-short");
    let files = laptop.join("files");
    std::fs::create_dir_all(&files).unwrap();
    let d = daemon(&laptop);
    let mut command = common::command();
    command
        .current_dir(&files)
        .args(["--context", "gpu", "save", "-o", "short.tar", "image"])
        .env("SHARDS_HOME", laptop.as_os_str())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: setrlimit(2), async-signal-safe, between fork and exec.
    unsafe {
        command.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: 4,
                rlim_max: 4,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().unwrap();
    let Seen::Started(_, output, exit) = d.next() else {
        panic!("no start");
    };
    output.send(b"an archive".to_vec()).unwrap();
    drop(output);
    exit.send(0).unwrap();
    let (status, said) = ended(child);
    assert_eq!(status, Some(1), "{said}");
    assert!(said.starts_with("failed to save image: write "), "{said}");
    assert!(said.contains(".tmp-short.tar"), "{said}");
    assert!(names(&files).is_empty(), "{:?}", names(&files));
}

/// A signal that ends the client before the machine answers, with nothing yet to send it
/// to, leaves no file: the context names a port that never answers, and the client is
/// signalled once its file is made.
#[cfg(unix)]
#[test]
fn a_client_ended_before_its_machine_answers_leaves_no_file() {
    let laptop = TempDir::new("cc-unans");
    let files = laptop.join("files");
    std::fs::create_dir_all(&files).unwrap();
    Identity::load_or_create(&laptop.join("identity")).unwrap();
    // Held, never read: the client's handshake goes unanswered.
    let silent = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    std::fs::create_dir_all(laptop.join("contexts")).unwrap();
    std::fs::write(
        laptop.join("contexts/gpu"),
        format!(
            "shards context 1\nname gpu\naddress {}\ndaemon {}\ndescription \n",
            silent.local_addr().unwrap(),
            "ab".repeat(32)
        ),
    )
    .unwrap();
    let child = saving(&laptop, &files, "unanswered.tar");
    let start = std::time::Instant::now();
    while !names(&files).iter().any(|n| n.starts_with(".tmp-unanswered.tar")) {
        assert!(start.elapsed() < TIMEOUT, "no file made");
        std::thread::sleep(Duration::from_millis(5));
    }
    // Well into its handshake, which gives up after 10 s.
    std::thread::sleep(Duration::from_millis(500));
    // SAFETY: kill(2) of our own child, which is only reaped below.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    let (status, said) = ended(child);
    assert_eq!(status, Some(143), "{said}");
    assert!(names(&files).is_empty(), "{:?}", names(&files));
}

/// `load -i FILE` and `import FILE` read the client's file, as Docker's CLI reads its own:
/// the command there reads its stdin, which is the file, and a file not here is refused
/// here, nothing sent.
#[test]
fn a_file_the_command_reads_is_the_clients() {
    let laptop = TempDir::new("cc-load");
    let files = laptop.join("files");
    std::fs::create_dir_all(&files).unwrap();
    std::fs::write(files.join("in.tar"), b"an archive here").unwrap();
    let d = daemon(&laptop);
    let run = |words: &[&str]| {
        common::command()
            .current_dir(&files)
            .args(["--context", "gpu"])
            .args(words)
            .env("SHARDS_HOME", laptop.as_os_str())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    for (given, sent) in [
        (&["load", "-qi", "in.tar"][..], &["load", "-q"][..]),
        (&["import", "in.tar", "app:1"][..], &["import", "-", "app:1"][..]),
    ] {
        let child = run(given);
        let Seen::Started(request, output, exit) = d.next() else {
            panic!("no start");
        };
        assert_eq!(args(&request), sent);
        d.stdin(b"an archive here");
        drop(output);
        exit.send(0).unwrap();
        let (status, said) = ended(child);
        assert_eq!(status, Some(0), "{said}");
    }
    let (status, said) = ended(run(&["load", "-i", "missing.tar"]));
    assert_eq!(status, Some(1), "{said}");
    assert!(said.starts_with("open missing.tar: "), "{said}");
    assert!(d.seen.try_recv().is_err(), "nothing sent");
}

/// On Windows, a client on a console sends its terminal as a terminal's: the console its
/// stdin, stdout and stderr, and its size; raw while the command runs, keys reach it as they
/// were typed, ^C a key like any, and a resize of the console sizes the command's terminal.
#[cfg(windows)]
#[test]
fn a_windows_clients_console_is_its_commands_terminal() {
    use shards_remote::frame::Terminal;
    let laptop = TempDir::new("cc-console");
    let d = daemon(&laptop);
    let mut console = console::Console::run(
        common::shards(),
        &["--context", "gpu", "run", "-it", "image", "sh"],
        &[("SHARDS_HOME", laptop.as_os_str())],
        25,
        90,
    );
    let Seen::Started(request, output, exit) = d.next() else {
        panic!("no start");
    };
    assert_eq!(
        request.terminal,
        Some(Terminal {
            stdin: true,
            stdout: true,
            stderr: true,
            rows: 25,
            cols: 90,
        })
    );
    output.send(b"ready\r\n".to_vec()).unwrap();
    console.wait_for("ready");
    console.type_in(b"ab\r");
    d.stdin(b"ab\r");
    console.type_in(&[3]);
    d.stdin(&[3]);
    console.resize(30, 100);
    loop {
        match d.next() {
            Seen::Resized(30, 100) => break,
            Seen::Resized(..) => {}
            other => panic!("{other:?}, waiting for the resize"),
        }
    }
    drop(output);
    exit.send(7).unwrap();
    assert_eq!(console.exit(), 7);
}

#[cfg(windows)]
mod console {
    //! A pseudoconsole (ConPTY) the test holds, as a user's console, for a program it starts,
    //! made as Microsoft has a pseudoconsole session made (learn.microsoft.com, "Creating a
    //! Pseudoconsole session"): two pipes, the console between them, and the program started
    //! with it as its attribute.

    use std::ffi::{OsStr, OsString, c_void};
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr::{null, null_mut};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
    use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
    use windows_sys::Win32::System::Console::{
        COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
    };
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::{
        CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
        EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, InitializeProcThreadAttributeList,
        LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION,
        STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
        WaitForSingleObject,
    };

    use super::TIMEOUT;

    pub struct Console {
        console: HPCON,
        input: HANDLE,
        process: HANDLE,
        thread: HANDLE,
        shown: Arc<Mutex<Vec<u8>>>,
        read: usize,
        ended: bool,
    }

    /// `text` as UTF-16 with its NUL.
    fn wide(text: &OsStr) -> Vec<u16> {
        text.encode_wide().chain([0]).collect()
    }

    impl Console {
        /// `program` with `args` on a console of `rows` by `cols`, its environment this
        /// process's with `env` in place, as common::command() runs shards.
        pub fn run(program: &Path, args: &[&str], env: &[(&str, &OsStr)], rows: i16, cols: i16) -> Console {
            let (mut in_read, mut in_write, mut out_read, mut out_write) =
                (null_mut(), null_mut(), null_mut(), null_mut());
            // SAFETY: CreatePipe fills both handles of a fresh pipe, each once.
            unsafe {
                assert_ne!(CreatePipe(&mut in_read, &mut in_write, null(), 0), 0);
                assert_ne!(CreatePipe(&mut out_read, &mut out_write, null(), 0), 0);
            }
            let mut console: HPCON = 0;
            // SAFETY: the pipes' ends the console takes, its size, and where its handle goes.
            let made = unsafe {
                CreatePseudoConsole(COORD { X: cols, Y: rows }, in_read, out_write, 0, &mut console)
            };
            assert_eq!(made, 0, "CreatePseudoConsole: {made:#x}");
            // SAFETY: our handles of the ends the console now holds its own of.
            unsafe {
                CloseHandle(in_read);
                CloseHandle(out_write);
            }
            let mut size = 0usize;
            // SAFETY: the size of a list of one attribute, asked for: the call fails, saying it.
            unsafe { InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut size) };
            let mut attributes = vec![0u8; size];
            let list: LPPROC_THREAD_ATTRIBUTE_LIST = attributes.as_mut_ptr().cast();
            // SAFETY: a list of one attribute in a buffer of the size asked; the console's
            // handle is the attribute's value itself, as Microsoft's sample passes it.
            unsafe {
                assert_ne!(InitializeProcThreadAttributeList(list, 1, 0, &mut size), 0);
                assert_ne!(
                    UpdateProcThreadAttribute(
                        list,
                        0,
                        PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                        console as *const c_void,
                        std::mem::size_of::<HPCON>(),
                        null_mut(),
                        null(),
                    ),
                    0
                );
            }
            // SAFETY: an all-zero STARTUPINFOEXW, its size and list set below.
            let mut info: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
            info.StartupInfo.cb = u32::try_from(std::mem::size_of::<STARTUPINFOEXW>()).unwrap();
            info.lpAttributeList = list;
            // Its stdio the console's, not this process's, which a test's harness pipes: a
            // child given none of its own takes the pseudoconsole's, as wezterm's starts one
            // ("otherwise ... the spawned process can inherit the explicitly redirected output
            // handles from its parent": e2a98e1b, pty/src/win/pseudocon.rs).
            info.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            info.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
            info.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
            info.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
            let mut line = OsString::from(format!("\"{}\"", program.display()));
            for arg in args {
                line.push(" ");
                line.push(arg);
            }
            let mut line = wide(&line);
            // The environment block, sorted by name as Windows has one (and as std's
            // Command sorts its own), each `NAME=value` NUL-ended, and a NUL after.
            let mut vars: Vec<(OsString, OsString)> = std::env::vars_os()
                .filter(|(k, _)| !env.iter().any(|(name, _)| k.eq_ignore_ascii_case(name)))
                .collect();
            vars.extend(env.iter().map(|(k, v)| (OsString::from(k), v.to_os_string())));
            vars.push(("SHARDS_LOCAL_STORE".into(), "none".into()));
            vars.push(("DOCKER_CONFIG".into(), crate::common::docker_config().into()));
            vars.retain(|(k, _)| k != "DOCKER_AUTH_CONFIG");
            vars.sort_by_key(|(k, _)| k.to_ascii_uppercase());
            vars.dedup_by(|a, b| a.0.eq_ignore_ascii_case(&b.0));
            let mut block: Vec<u16> = Vec::new();
            for (k, v) in &vars {
                let mut entry = k.clone();
                entry.push("=");
                entry.push(v);
                block.extend(wide(&entry));
            }
            block.push(0);
            // SAFETY: an all-zero PROCESS_INFORMATION, for CreateProcessW to fill.
            let mut started: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: a command line and an environment block of our own, NUL-ended, and
            // the startup information whose attribute list lives through the call.
            let made = unsafe {
                CreateProcessW(
                    null(),
                    line.as_mut_ptr(),
                    null(),
                    null(),
                    0,
                    EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                    block.as_ptr().cast(),
                    null(),
                    &info.StartupInfo,
                    &mut started,
                )
            };
            assert_ne!(made, 0, "CreateProcessW: {}", std::io::Error::last_os_error());
            // SAFETY: the list InitializeProcThreadAttributeList made, used no more.
            unsafe { DeleteProcThreadAttributeList(list) };
            drop(attributes);
            // What the console shows, read as it comes: a console whose output is not read
            // holds its program up.
            let shown = Arc::new(Mutex::new(Vec::new()));
            let into = Arc::clone(&shown);
            let output = out_read as usize;
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    let mut n = 0u32;
                    // SAFETY: a read of our pipe's end, which this thread alone reads, into a
                    // local buffer.
                    let ok = unsafe {
                        ReadFile(
                            output as HANDLE,
                            buf.as_mut_ptr(),
                            u32::try_from(buf.len()).unwrap(),
                            &mut n,
                            null_mut(),
                        )
                    };
                    if ok == 0 || n == 0 {
                        break;
                    }
                    into.lock().unwrap().extend_from_slice(&buf[..n as usize]);
                }
            });
            Console {
                console,
                input: in_write,
                process: started.hProcess,
                thread: started.hThread,
                shown,
                read: 0,
                ended: false,
            }
        }

        /// Waits until the console has shown `text` since the last wait.
        pub fn wait_for(&mut self, text: &str) {
            let until = Instant::now() + TIMEOUT;
            let want = text.as_bytes();
            loop {
                {
                    let shown = self.shown.lock().unwrap();
                    let fresh = &shown[self.read..];
                    if let Some(at) = fresh.windows(want.len()).position(|w| w == want) {
                        self.read += at + want.len();
                        return;
                    }
                    assert!(
                        Instant::now() < until,
                        "{text:?} did not come; the console showed {:?}",
                        String::from_utf8_lossy(fresh)
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }

        /// Types `keys` into the console.
        pub fn type_in(&mut self, keys: &[u8]) {
            let mut n = 0u32;
            // SAFETY: a write of our pipe's end from a local buffer.
            let ok = unsafe {
                WriteFile(
                    self.input,
                    keys.as_ptr(),
                    u32::try_from(keys.len()).unwrap(),
                    &mut n,
                    null_mut(),
                )
            };
            assert!(ok != 0 && n as usize == keys.len());
        }

        /// Resizes the console, as a window does.
        pub fn resize(&self, rows: i16, cols: i16) {
            // SAFETY: our console's handle and its new size.
            let resized = unsafe { ResizePseudoConsole(self.console, COORD { X: cols, Y: rows }) };
            assert_eq!(resized, 0, "ResizePseudoConsole: {resized:#x}");
        }

        /// Once the program exits: its exit status.
        pub fn exit(&mut self) -> u32 {
            let millis = u32::try_from(TIMEOUT.as_millis()).unwrap();
            // SAFETY: our process's handle.
            let waited = unsafe { WaitForSingleObject(self.process, millis) };
            assert_eq!(waited, WAIT_OBJECT_0, "it did not exit");
            let mut code = 0u32;
            // SAFETY: as above, and where its code goes.
            assert_ne!(unsafe { GetExitCodeProcess(self.process, &mut code) }, 0);
            self.ended = true;
            code
        }
    }

    impl Drop for Console {
        fn drop(&mut self) {
            // SAFETY: our own handles, each closed once; the program, if a test failed with it
            // running, ended first.
            unsafe {
                if !self.ended {
                    TerminateProcess(self.process, 1);
                }
                ClosePseudoConsole(self.console);
                CloseHandle(self.input);
                CloseHandle(self.process);
                CloseHandle(self.thread);
            }
        }
    }
}
