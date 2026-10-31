//! The user's terminal on Windows, for E2E tests of runs on one: a pseudo console the test
//! holds (tests/tty.rs), as Unix's is a pty (terminal.rs).

use std::fs::File;
use std::io::{Read, Write};
use std::os::windows::ffi::OsStrExt as _;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use super::{as_tests_run, guest_init, kernel};

const TIMEOUT: Duration = Duration::from_secs(60);

/// The user's terminal on Windows: a pseudo console (CreatePseudoConsole) the test holds,
/// whose "shell" is this test binary in [`shell_if_asked`]'s part, started on it as a
/// console process: it runs the client there and keeps the console after the client's exit,
/// as a login shell keeps a pty. The console's modes, which only a process on it can read
/// (GetConsoleMode), the shell writes to a file beside it, `modes`, as they change: the test
/// reads them there, never through the console, which the client draws on.
pub struct Terminal {
    console: windows_sys::Win32::System::Console::HPCON,
    /// What the test types: the pseudo console's input.
    input: File,
    /// What the console shows, as a thread drains it: a pseudo console's writes wait on a
    /// reader, and its close on its output's end.
    shown: std::sync::mpsc::Receiver<Vec<u8>>,
    shell: Child,
    /// What it has shown since the last wait, its escape sequences taken out ([`text_of`]).
    seen: String,
    /// The start of an escape sequence its last write left unfinished.
    pending: String,
    dir: super::TempDir,
}

/// A console's modes: its input's and its screen's (GetConsoleMode).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Modes {
    pub input: u32,
    pub output: u32,
}

/// The file in its directory that says what the shell runs: shards, then each argument, a
/// line each.
const ASKED: &str = "shards-test-console-shell";
/// The file the shell writes the console's modes to: `before IN OUT`, then `now IN OUT` as
/// they change, then `after IN OUT`, each a line.
const MODES: &str = "shards-test-console-modes";

/// A child of this process's, on the pseudo console `console`: std starts no process on
/// one, which takes a proc-thread attribute (PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE).
struct Child {
    process: std::os::windows::io::OwnedHandle,
}

impl Child {
    fn exited(&self) -> Option<u32> {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Foundation::STILL_ACTIVE;
        use windows_sys::Win32::System::Threading::GetExitCodeProcess;
        let mut code = 0u32;
        // SAFETY: a process handle this test holds, its code read into a local.
        let read = unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &raw mut code) };
        assert_ne!(read, 0, "{}", std::io::Error::last_os_error());
        (code != STILL_ACTIVE.cast_unsigned()).then_some(code)
    }
}

impl Terminal {
    /// `shards ARGS` in `home` on a terminal of `rows` by `cols`.
    pub fn run(home: &Path, args: &[&str], rows: u16, cols: u16) -> Terminal {
        use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
        use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Console::{COORD, CreatePseudoConsole, HPCON};
        use windows_sys::Win32::System::Pipes::CreatePipe;
        use windows_sys::Win32::System::Threading::{
            CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
            InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
            UpdateProcThreadAttribute,
        };
        let dir = super::TempDir::new("console");
        let mut asked = format!("{}\n", super::shards().display());
        for a in args {
            asked.push_str(a);
            asked.push('\n');
        }
        asked.push_str(&format!("SHARDS_HOME={}\n", home.display()));
        std::fs::write(dir.join(ASKED), asked).unwrap();
        // The console's two pipes: what it reads, and what it writes.
        let pipe = || -> (OwnedHandle, OwnedHandle) {
            let (mut read, mut write): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
            // SAFETY: two handles made for this test, its own.
            let made = unsafe { CreatePipe(&raw mut read, &raw mut write, std::ptr::null(), 0) };
            assert_ne!(made, 0, "{}", std::io::Error::last_os_error());
            // SAFETY: handles just made, which nothing else owns.
            unsafe {
                (
                    OwnedHandle::from_raw_handle(read),
                    OwnedHandle::from_raw_handle(write),
                )
            }
        };
        let ((its_input, input), (output, its_output)) = (pipe(), pipe());
        let mut console: HPCON = 0;
        let size = COORD {
            X: i16::try_from(cols).unwrap(),
            Y: i16::try_from(rows).unwrap(),
        };
        // SAFETY: the console takes its two ends, which this test closes once it has them.
        let made = unsafe {
            CreatePseudoConsole(
                size,
                its_input.as_raw_handle(),
                its_output.as_raw_handle(),
                0,
                &raw mut console,
            )
        };
        assert_eq!(made, 0, "CreatePseudoConsole: {made:#x}");
        drop((its_input, its_output));
        // The shell: this binary's console_shell test, in the directory that says what it
        // runs: every test binary's, as each holds this module.
        let exe = std::env::current_exe().unwrap();
        let mut line: Vec<u16> = format!(
            "\"{}\" common::terminal::console_shell --exact --ignored --nocapture --test-threads=1",
            exe.display()
        )
        .encode_utf16()
        .chain([0])
        .collect();
        let cwd: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
        let mut room = 0usize;
        // SAFETY: a size asked for one attribute, no list given.
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &raw mut room) };
        let mut list = vec![0u8; room];
        let attributes: LPPROC_THREAD_ATTRIBUTE_LIST = list.as_mut_ptr().cast();
        // SAFETY: a list of `room` bytes, made for one attribute, the console's value (an
        // HPCON is passed as itself, Microsoft's "Creating a Pseudoconsole session").
        unsafe {
            assert_ne!(
                InitializeProcThreadAttributeList(attributes, 1, 0, &raw mut room),
                0
            );
            assert_ne!(
                UpdateProcThreadAttribute(
                    attributes,
                    0,
                    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                    console as *const std::ffi::c_void,
                    std::mem::size_of::<HPCON>(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                ),
                0
            );
        }
        let mut start = STARTUPINFOEXW::default();
        start.StartupInfo.cb = u32::try_from(std::mem::size_of::<STARTUPINFOEXW>()).unwrap();
        // Its stdio the console's, none of this test's, whose own are redirected
        // (portable-pty does the same, as a redirected parent's would be inherited).
        start.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        start.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
        start.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
        start.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
        start.lpAttributeList = attributes;
        let mut made = PROCESS_INFORMATION::default();
        // SAFETY: a command line and directory of this function's, NUL-terminated; the
        // startup information and its list live through the call.
        let started = unsafe {
            CreateProcessW(
                std::ptr::null(),
                line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT,
                std::ptr::null(),
                cwd.as_ptr(),
                &raw const start.StartupInfo,
                &raw mut made,
            )
        };
        // SAFETY: the list made above, no longer needed.
        unsafe { DeleteProcThreadAttributeList(attributes) };
        assert_ne!(started, 0, "the shell: {}", std::io::Error::last_os_error());
        // SAFETY: handles CreateProcessW made for this test.
        let (process, _thread) = unsafe {
            (
                OwnedHandle::from_raw_handle(made.hProcess),
                OwnedHandle::from_raw_handle(made.hThread),
            )
        };
        let (tx, shown) = std::sync::mpsc::channel();
        let mut output = File::from(output);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = output.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        Terminal {
            console,
            input: File::from(input),
            shown,
            shell: Child { process },
            seen: String::new(),
            pending: String::new(),
            dir,
        }
    }
}

/// What a terminal shows of `raw`, written to it: its text, its escape sequences taken out
/// (CSI's to their final byte, OSC's to BEL or ST, and the two-character rest), which a pseudo
/// console writes of its own as it repaints its screen (an erase before each line's end, its
/// cursor's moves, a resize's report); and the start of one `raw` leaves unfinished.
fn text_of(raw: &str) -> (String, &str) {
    let mut text = String::new();
    let mut chars = raw.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        if c != '\u{1b}' {
            text.push(c);
            continue;
        }
        match chars.next() {
            None => return (text, raw.get(at..).unwrap_or_default()),
            Some((_, '[')) => loop {
                match chars.next() {
                    None => return (text, raw.get(at..).unwrap_or_default()),
                    Some((_, '\u{40}'..='\u{7e}')) => break,
                    Some(_) => {}
                }
            },
            Some((_, ']')) => loop {
                match chars.next() {
                    None => return (text, raw.get(at..).unwrap_or_default()),
                    Some((_, '\u{7}')) => break,
                    Some((_, '\u{1b}')) => {
                        if chars.next().is_none() {
                            return (text, raw.get(at..).unwrap_or_default());
                        }
                        break;
                    }
                    Some(_) => {}
                }
            },
            Some(_) => {}
        }
    }
    (text, "")
}

impl Terminal {
    /// Reads what the terminal shows until `text` has appeared since the last wait, and
    /// returns everything read since then, its escape sequences taken out ([`text_of`]).
    pub fn wait_for(&mut self, text: &str) -> String {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(at) = self.seen.find(text) {
                let upto = at + text.len();
                return self.seen.drain(..upto).collect();
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match self.shown.recv_timeout(left.min(Duration::from_millis(100))) {
                Ok(bytes) => {
                    let raw = std::mem::take(&mut self.pending) + &String::from_utf8_lossy(&bytes);
                    let (text, rest) = text_of(&raw);
                    self.seen.push_str(&text);
                    self.pending = rest.to_string();
                }
                Err(_) => assert!(
                    Instant::now() < deadline,
                    "{text:?} did not come; the terminal showed {:?}",
                    self.seen
                ),
            }
        }
    }

    /// Types `keys`, as the console's input reads them: VT sequences and characters.
    pub fn type_in(&mut self, keys: &[u8]) {
        self.input.write_all(keys).unwrap();
    }

    /// Resizes the terminal, as a window does: its console says so to what reads it.
    pub fn resize(&self, rows: u16, cols: u16) {
        use windows_sys::Win32::System::Console::{COORD, ResizePseudoConsole};
        let size = COORD {
            X: i16::try_from(cols).unwrap(),
            Y: i16::try_from(rows).unwrap(),
        };
        // SAFETY: the pseudo console this test holds.
        assert_eq!(unsafe { ResizePseudoConsole(self.console, size) }, 0);
    }

    /// The console's modes now, as its shell last wrote them.
    pub fn settings(&self) -> Modes {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(modes) = modes(&self.dir, &["now", "before"]) {
                return modes;
            }
            assert!(Instant::now() < deadline, "the shell wrote no modes");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Once the client exits: what it showed after the last wait, its status, and whether
    /// it left the console's modes as it found them.
    pub fn exit(&mut self) -> (String, i32, bool) {
        let shown = self.wait_for("exit ");
        let shown = shown.strip_suffix("exit ").unwrap_or_default().to_string();
        let status = self.wait_for("\r\n");
        let restored = modes(&self.dir, &["after"]) == modes(&self.dir, &["before"]);
        // The shell waits for a line, read as a console in its modes reads Return.
        self.type_in(b"\r");
        let deadline = Instant::now() + TIMEOUT;
        while self.shell.exited().is_none() {
            assert!(Instant::now() < deadline, "the shell did not exit");
            while self.shown.try_recv().is_ok() {}
            std::thread::sleep(Duration::from_millis(5));
        }
        (shown, status.trim().parse().unwrap(), restored)
    }
}

impl Drop for Terminal {
    /// The console closed, and with it what runs on it: its processes are told it has
    /// gone (CTRL_CLOSE_EVENT), and the shell is ended outright.
    fn drop(&mut self) {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::System::Console::ClosePseudoConsole;
        use windows_sys::Win32::System::Threading::TerminateProcess;
        if self.shell.exited().is_none() {
            // SAFETY: a process handle this test holds.
            unsafe { TerminateProcess(self.shell.process.as_raw_handle(), 1) };
        }
        // SAFETY: the pseudo console this test made, closed once; its output's reader goes
        // on draining it meanwhile, as its close waits on that (ClosePseudoConsole).
        unsafe { ClosePseudoConsole(self.console) };
    }
}

/// The modes the first of `which` names in the shell's `modes` file says, its latest such.
fn modes(dir: &Path, which: &[&str]) -> Option<Modes> {
    let text = std::fs::read_to_string(dir.join(MODES)).ok()?;
    which.iter().find_map(|w| {
        text.lines().rev().find_map(|l| {
            let mut f = l.split_whitespace();
            (f.next()? == *w).then_some(())?;
            Some(Modes {
                input: f.next()?.parse().ok()?,
                output: f.next()?.parse().ok()?,
            })
        })
    })
}

/// The settings raw mode changes, as moby/term's Windows raw mode makes them: line input,
/// echo, and the screen's return at each newline (DISABLE_NEWLINE_AUTO_RETURN unset).
pub fn cooked(m: &Modes) -> (bool, bool, bool) {
    use windows_sys::Win32::System::Console::{
        DISABLE_NEWLINE_AUTO_RETURN, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT,
    };
    (
        m.input & ENABLE_LINE_INPUT != 0,
        m.input & ENABLE_ECHO_INPUT != 0,
        m.output & DISABLE_NEWLINE_AUTO_RETURN == 0,
    )
}

/// The shell a Windows terminal runs its client in ([`shell_if_asked`]), run by name by a
/// test that holds one ([`Terminal::run`]), in any test binary.
#[test]
#[ignore = "the shell of a terminal a test holds, which runs it"]
fn console_shell() {
    shell_if_asked();
}

/// The shell's part, where the test that holds a terminal started this binary as its
/// shell, in a directory that says what to run ([`Terminal::run`]); elsewhere nothing. It
/// writes the console's modes as they are before the client runs, as they change while it
/// runs, and after it; says its status, `exit N`; and waits for a line, so that the console
/// outlives the client, as a pty does a login shell's.
fn shell_if_asked() {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::System::Console::GetConsoleMode;
    let Ok(asked) = std::fs::read_to_string(ASKED) else {
        return;
    };
    let mut lines = asked.lines();
    let shards = lines.next().unwrap().to_string();
    let mut args = Vec::new();
    let mut home = None;
    for line in lines {
        match line.strip_prefix("SHARDS_HOME=") {
            Some(h) => home = Some(h.to_string()),
            None => args.push(line.to_string()),
        }
    }
    let now = || -> (u32, u32) {
        let (mut input, mut output) = (0u32, 0u32);
        // SAFETY: this process's console handles, their modes read into locals.
        unsafe {
            GetConsoleMode(std::io::stdin().as_raw_handle(), &raw mut input);
            GetConsoleMode(std::io::stdout().as_raw_handle(), &raw mut output);
        }
        (input, output)
    };
    let write = |what: &str, (i, o): (u32, u32)| {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(MODES)
            .unwrap();
        writeln!(f, "{what} {i} {o}").unwrap();
    };
    write("before", now());
    let mut client = Command::new(&shards);
    as_tests_run(&mut client)
        .args(&args)
        .env("SHARDS_HOME", home.unwrap())
        .env("SHARDS_KERNEL", kernel())
        .env("SHARDS_INIT", guest_init())
        .env("TERM", "xterm-256color")
        .env_remove("NO_COLOR")
        .env_remove("NORAW");
    let mut client = client.spawn().unwrap();
    let mut last = now();
    let status = loop {
        if let Some(status) = client.try_wait().unwrap() {
            break status;
        }
        let modes = now();
        if modes != last {
            write("now", modes);
            last = modes;
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    write("after", now());
    let _ = writeln!(std::io::stdout(), "exit {}", status.code().unwrap_or(-1));
    let mut done = String::new();
    let _ = std::io::stdin().read_line(&mut done);
}
