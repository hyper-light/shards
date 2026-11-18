//! A directory a test has of its own, in the system's temporary directory, removed as the
//! test goes, a failing test's too: its value is dropped as the panic unwinds, and a
//! failing test's says first, on stderr, the end of each log its programs wrote in it
//! (a daemon's log, a run's stderr, a VM's sample), which says why it failed; and each
//! panic of a process that made one is said on stderr as it happens. Each test
//! process's are under `shards-tests/<pid>/`, so that a process killed before it removed
//! its own, or one that kept one as long as it ran (a `static`'s), leaves them where the
//! next one finds them: the first directory a process makes removes those of every process
//! that has ended, on every host, and any under its own ID, which only an ended process that
//! had the ID before it can have left (IDs are reused: CI 44245219858's windows-2022 job gave
//! a process an ended one's `docker-config-0`). Only that directory is swept, and only a
//! process's that no longer runs.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, Once, PoisonError};

/// How many of this process's directories are live: its own directory goes with the last,
/// and is made, or found, under this lock alone, so that none is made in it as it goes.
static LIVE: Mutex<usize> = Mutex::new(0);

/// Every test process's directories' parent.
#[expect(
    clippy::disallowed_methods,
    reason = "the one place a test's directory is made in the system's temporary directory"
)]
fn parent() -> PathBuf {
    std::env::temp_dir().join("shards-tests")
}

/// Raises this test process's soft limit on open descriptors to its hard limit, and on
/// macOS to no more than `kern.maxfilesperproc`, as shards' own processes raise theirs
/// (vmm `platform::raise_descriptor_limit`): a test binary runs its tests side by side,
/// and a macOS terminal, or a launchd job, starts it with 256, which the daemon's tests
/// passed together, 7 of them failing for "Too many open files". Where it cannot, the
/// limit stays as it was.
#[cfg(unix)]
fn raise_descriptor_limit() {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit(2) into a local.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        return;
    }
    let most = lim.rlim_max.min(per_process_most().unwrap_or(libc::rlim_t::MAX));
    if most > lim.rlim_cur {
        lim.rlim_cur = most;
        // SAFETY: setrlimit(2) from a local.
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) };
    }
}

/// The most descriptors macOS lets a process have open, whatever its limit
/// (`kern.maxfilesperproc`, which setrlimit(2) refuses a soft limit past).
#[cfg(target_os = "macos")]
fn per_process_most() -> Option<libc::rlim_t> {
    let mut most: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>();
    // SAFETY: sysctlbyname(3) of an int into a local of its size.
    let read = unsafe {
        libc::sysctlbyname(
            c"kern.maxfilesperproc".as_ptr(),
            (&raw mut most).cast(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (read == 0).then(|| libc::rlim_t::try_from(most).ok()).flatten()
}

/// None elsewhere: the hard limit is the most.
#[cfg(all(unix, not(target_os = "macos")))]
fn per_process_most() -> Option<libc::rlim_t> {
    None
}

/// Says each panic of this process on its stderr as it happens, which libtest does not
/// capture, then gives it to the hook it replaced. libtest prints a failing test's panic only
/// in its failures section, as its binary ends, so a binary its step's limit cut after a test
/// had failed in it said no word of why (CI 43990298220: boot.rs'
/// `a_managed_guest_is_held_to_the_rate_its_client_asks_on_its_socket` on both x86_64
/// Windows jobs, two of containers.rs' on x86_64 gnu; 43968984258: two of api_image's on
/// windows-2025). A panic a test makes on purpose and catches is said too. None is set on a
/// panicking thread, where taking the hook panics.
fn say_panics() {
    use std::io::Write as _;
    if std::thread::panicking() {
        return;
    }
    let before = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let _ = writeln!(
            io::stderr().lock(),
            "--- said as it happened: thread '{}' {info}",
            thread.name().unwrap_or("<unnamed>")
        );
        before(info);
    }));
}

/// A test's directory, made empty, removed as it is dropped.
#[derive(Debug)]
pub struct TempDir(PathBuf);

impl TempDir {
    /// A directory of the test's own, named for `name`: `shards-tests/<pid>/<name>-<n>`
    /// under the system's temporary directory, each character no Windows name may hold
    /// (`<>:"/\|?*` and the control characters: Naming Files, Paths, and Namespaces) a `-`
    /// on every host, so that a name made of an image's (`oom-x:1`) is one directory there.
    /// Made, never found: one there already, an ended process's that its sweep could not
    /// remove (a file of it still held), fails its making (AlreadyExists, its path said)
    /// rather than be taken as it was.
    pub fn new(name: &str) -> io::Result<TempDir> {
        let name: String = name
            .chars()
            .map(|c| {
                if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                    '-'
                } else {
                    c
                }
            })
            .collect();
        static SWEPT: Once = Once::new();
        static N: AtomicUsize = AtomicUsize::new(0);
        let parent = parent();
        let me = std::process::id();
        // Once, before this process makes any directory, as each other thread's first waits
        // for it: what is under its own ID then is an ended process's (`sweep`).
        SWEPT.call_once(|| {
            #[cfg(unix)]
            raise_descriptor_limit();
            sweep(&parent, me);
            say_panics();
        });
        let n = N.fetch_add(1, Ordering::Relaxed);
        let own = parent.join(me.to_string());
        let dir = own.join(format!("{name}-{n}"));
        let mut live = LIVE.lock().unwrap_or_else(PoisonError::into_inner);
        std::fs::create_dir_all(&own)
            .and_then(|()| std::fs::create_dir(&dir))
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", dir.display())))?;
        *live += 1;
        Ok(TempDir(dir))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl std::ops::Deref for TempDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<std::ffi::OsStr> for TempDir {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.0.as_os_str()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if std::thread::panicking() {
            show_logs(&self.0, 3, &mut io::stderr().lock());
        }
        remove(&self.0);
        // The process's own directory, with its last.
        let mut live = LIVE.lock().unwrap_or_else(PoisonError::into_inner);
        *live = live.saturating_sub(1);
        if *live == 0
            && let Some(own) = self.0.parent()
        {
            let _ = std::fs::remove_dir(own);
        }
    }
}

/// The last bytes [`show_logs`] shows of a log, unless `SHARDS_TEST_LOG_SHOWN` says how many
/// (a test whose programs log at debug, read whole).
const SHOWN: u64 = 32 << 10;

fn shown() -> u64 {
    std::env::var("SHARDS_TEST_LOG_SHOWN")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(SHOWN)
}

/// What a failing test's directory says of why: the last [`SHOWN`] bytes of each log in it,
/// `depth` directories down (`*.log`, `*.stderr`, `*.sample`), into `out`.
fn show_logs(dir: &Path, depth: u32, out: &mut impl io::Write) {
    use std::io::{Read as _, Seek as _};
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        let Ok(kind) = e.file_type() else {
            continue;
        };
        let log = path
            .extension()
            .is_some_and(|x| x == "log" || x == "stderr" || x == "sample");
        if kind.is_dir() && depth > 0 {
            show_logs(&path, depth - 1, out);
        } else if kind.is_file() && log {
            let Ok(mut f) = std::fs::File::open(&path) else {
                continue;
            };
            let len = f.metadata().map_or(0, |m| m.len());
            let mut tail = Vec::new();
            if f.seek(io::SeekFrom::Start(len.saturating_sub(shown()))).is_err()
                || f.read_to_end(&mut tail).is_err()
            {
                continue;
            }
            let _ = writeln!(
                out,
                "--- {}, its last {} of {len} bytes",
                path.display(),
                tail.len()
            );
            let _ = out.write_all(&tail);
            let _ = writeln!(out);
        }
    }
}

/// Removes `dir` and all in it, a test's directories it made unreadable or unwritable
/// too: where a removal fails, each directory in it is made its owner's to read, write and
/// search (no link followed), and it is removed again.
fn remove(dir: &Path) {
    if std::fs::remove_dir_all(dir).is_ok() || !dir.exists() {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fn open_up(dir: &Path) {
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    open_up(&entry.path());
                }
            }
        }
        open_up(dir);
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// Removes the directories of test processes that have ended, under `parent`, each named by
/// its process's ID: `me`'s, which process `me` makes only after this (its first
/// [`TempDir::new`] sweeps before it makes any), so that what is there was left by an ended
/// process that had the ID before it; and each other's once its process is [`gone`]. One
/// whose ID another process has taken since stays until that one ends too.
fn sweep(parent: &Path, me: u32) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        if pid == me || gone(pid) {
            remove(&entry.path());
        }
    }
}

/// Whether no process has ID `pid`: kill(2) with no signal says ESRCH.
#[cfg(unix)]
fn gone(pid: u32) -> bool {
    let Some(pid) = libc::pid_t::try_from(pid).ok().filter(|&pid| pid > 0) else {
        return false;
    };
    // SAFETY: kill(2) with no signal sends nothing; it asks whether the process is.
    let asked = unsafe { libc::kill(pid, 0) };
    asked != 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// Whether the process with ID `pid` has ended, or no process has the ID. OpenProcess
/// ("Opens an existing local process object") opens one that has ended while any handle to
/// it is open, its parent's among them, as its object lasts until the last is closed
/// ("Terminating a Process"); signalled then, which a wait of no time sees
/// (WaitForSingleObject, with the one right it needs, SYNCHRONIZE: "Process Security and
/// Access Rights"), where its exit code would take one that ended with STILL_ACTIVE, 259,
/// for one that runs (GetExitCodeProcess). OpenProcess fails ERROR_INVALID_PARAMETER for an
/// ID no process has (CI 44189693085, windows-2022), as its page says it does for the
/// System Idle Process's, 0, which never ends; and ERROR_ACCESS_DENIED for a process that
/// user-level code may not open (the System process, CSRSS's), which runs. Any other
/// failure is taken for a process that runs, whose directory stays.
#[cfg(windows)]
fn gone(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, GetLastError, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
    if pid == 0 {
        return false;
    }
    // SAFETY: OpenProcess of an ID, for waiting alone, its handle not inherited: a handle,
    // closed below, or null.
    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if process.is_null() {
        // SAFETY: GetLastError takes nothing; it reads this thread's last error, OpenProcess's.
        return unsafe { GetLastError() } == ERROR_INVALID_PARAMETER;
    }
    // SAFETY: a wait of no time on the handle just opened, then its close.
    unsafe {
        let ended = WaitForSingleObject(process, 0) == WAIT_OBJECT_0;
        CloseHandle(process);
        ended
    }
}

/// The program `text` at `path`, run by its owner alone or by anyone (0755), written by a
/// process of its own on Unix. A file a test process writes itself is held open to write by
/// every child another of its test threads forks before that child execs (fork(2) keeps an
/// O_CLOEXEC descriptor until the exec), and an exec of the file meanwhile, by this process or
/// any other, fails ETXTBSY (execve(2)); rename(2) does not help, the open being the inode's.
/// Every program a test runs is written so ([`program_copy`] for a copy), none by a
/// descriptor of the test process. Windows' exec has no such refusal: written in place there.
pub fn program(path: &Path, text: &str) -> io::Result<()> {
    #[cfg(unix)]
    return write_apart(path, Source::Text(text.as_bytes()));
    #[cfg(not(unix))]
    std::fs::write(path, text)
}

/// The program at `from`, copied to `to` as [`program`] writes one, 0755: on Unix by a
/// process of its own, so that no descriptor of the test process ever writes it.
pub fn program_copy(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(unix)]
    return write_apart(to, Source::Copy(from));
    #[cfg(not(unix))]
    std::fs::copy(from, to).map(drop)
}

/// What [`write_apart`] writes: these bytes, or the file at a path.
#[cfg(unix)]
enum Source<'a> {
    Text(&'a [u8]),
    Copy(&'a Path),
}

/// `source` written to `path`, 0755, by a child forked to write it and exit: the descriptor
/// that writes the file is the child's alone, never this process's, so no child another test
/// thread forks holds it. The child execs no program, as a host may have none to run (boot.rs'
/// L1 guest, a Linux KVM host whose `PATH` is an empty directory, has no /bin/sh, cat or cp),
/// and calls only async-signal-safe functions on memory made before the fork, as a child of a
/// multithreaded process must (POSIX fork(), signal-safety(7)). Its exit status is the errno
/// that stopped it, 0 once written. While it writes it also holds the descriptors it was
/// forked with, until it exits.
#[cfg(unix)]
fn write_apart(path: &Path, source: Source<'_>) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    let c = |p: &Path| std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(io::Error::other);
    let to = c(path)?;
    let (from, text) = match source {
        Source::Text(text) => (None, text),
        Source::Copy(from) => (Some(c(from)?), &[][..]),
    };
    let mut buffer = vec![0u8; 64 << 10];
    // SAFETY: fork(2); the child runs `write_child`, which calls only async-signal-safe
    // functions and allocates nothing, then _exit(2).
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        let errno = write_child(&to, from.as_deref(), text, &mut buffer);
        // SAFETY: _exit(2) ends the child, running none of this process's exit handlers.
        unsafe { libc::_exit(errno) }
    }
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut status = 0;
    // SAFETY: waitpid(2) of this function's own child, into a local.
    while unsafe { libc::waitpid(pid, &mut status, 0) } != pid {
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
    let what = match &from {
        Some(from) => format!("{} copied to {}", from.to_string_lossy(), path.display()),
        None => format!("{} written", path.display()),
    };
    match (libc::WIFEXITED(status), libc::WEXITSTATUS(status)) {
        (true, 0) => Ok(()),
        (true, errno) => {
            let e = io::Error::from_raw_os_error(errno);
            Err(io::Error::new(e.kind(), format!("{what}: {e}")))
        }
        _ => Err(io::Error::other(format!(
            "{what}: its writer ended with wait status {status}"
        ))),
    }
}

/// The forked writer's work ([`write_apart`]): 0, or the errno that stopped it. `from`, when
/// given, is copied through `buffer`; otherwise `text` is written.
#[cfg(unix)]
fn write_child(
    to: &std::ffi::CStr,
    from: Option<&std::ffi::CStr>,
    text: &[u8],
    buffer: &mut [u8],
) -> libc::c_int {
    let errno = || io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO);
    let source = match from {
        // SAFETY: open(2) of a NUL-terminated path.
        Some(from) => match unsafe { libc::open(from.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) } {
            fd if fd < 0 => return errno(),
            fd => Some(fd),
        },
        None => None,
    };
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_CLOEXEC;
    // SAFETY: open(2) of a NUL-terminated path; its mode is set below, past the umask.
    let fd = unsafe { libc::open(to.as_ptr(), flags, 0o755 as libc::c_uint) };
    if fd < 0 {
        return errno();
    }
    let write_all = |mut bytes: &[u8]| -> Result<(), libc::c_int> {
        while !bytes.is_empty() {
            // SAFETY: write(2) of the bytes this slice holds.
            let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
            match usize::try_from(n) {
                Ok(0) => return Err(libc::EIO),
                Ok(n) => bytes = bytes.get(n..).unwrap_or_default(),
                Err(_) if errno() == libc::EINTR => {}
                Err(_) => return Err(errno()),
            }
        }
        Ok(())
    };
    let wrote = match source {
        None => write_all(text),
        Some(source) => loop {
            // SAFETY: read(2) into the buffer, no more than its length.
            let n = unsafe { libc::read(source, buffer.as_mut_ptr().cast(), buffer.len()) };
            match usize::try_from(n) {
                Ok(0) => break Ok(()),
                Ok(n) => {
                    if let Err(e) = write_all(buffer.get(..n).unwrap_or_default()) {
                        break Err(e);
                    }
                }
                Err(_) if errno() == libc::EINTR => {}
                Err(_) => break Err(errno()),
            }
        },
    };
    if let Err(e) = wrote {
        return e;
    }
    // SAFETY: fchmod(2) and close(2) of the descriptor opened above.
    if unsafe { libc::fchmod(fd, 0o755) } != 0 || unsafe { libc::close(fd) } != 0 {
        return errno();
    }
    0
}

/// The tests' helper program (`src/bin/shards-testhelper.rs`, which says its commands), for
/// what a test runs where a Unix host would run a shell's tools: built for this host as a
/// test process first asks for it, by cargo (the rustup proxy on PATH, without the dyld
/// paths cargo gives test processes, which keep rust-lld from its libLLVM), into a target
/// directory of its own beside the tests' (`testhelper`), one build at a time under its
/// lock; and copied into a directory of this process's own, which it runs from, so that no
/// build after a change replaces a program another test process runs (which Windows
/// refuses). `SHARDS_TESTHELPER` names one built already, for a host given tests built
/// elsewhere, which has no cargo.
pub fn helper() -> Result<&'static Path, String> {
    static BUILT: std::sync::OnceLock<Result<(TempDir, PathBuf), String>> = std::sync::OnceLock::new();
    match BUILT.get_or_init(build_helper) {
        Ok((_, program)) => Ok(program.as_path()),
        Err(e) => Err(e.clone()),
    }
}

fn build_helper() -> Result<(TempDir, PathBuf), String> {
    let name = format!("shards-testhelper{}", std::env::consts::EXE_SUFFIX);
    // A host given the tests built for it, with no cargo to build with (a Windows host the
    // tests were cross-built for), names a helper built beside them, as SHARDS_TEST_GUESTS
    // names its guests.
    if let Some(given) = std::env::var_os("SHARDS_TESTHELPER") {
        return copy_helper(Path::new(&given), &name);
    }
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    // A test's program is `<target>/<profile>/deps/<test>`.
    let exe = std::env::current_exe().map_err(|e| format!("this test's program: {e}"))?;
    let target = exe
        .ancestors()
        .nth(3)
        .map_or_else(|| workspace.join("target"), Path::to_path_buf)
        .join("testhelper");
    std::fs::create_dir_all(&target).map_err(|e| format!("{}: {e}", target.display()))?;
    let lock =
        std::fs::File::create(target.join(".lock")).map_err(|e| format!("{}: {e}", target.display()))?;
    lock.lock().map_err(|e| format!("{}: {e}", target.display()))?;
    let built = build_and_copy(&workspace, &target, &name);
    // Let go of before its file closes (a flock(2) lock is its open file's).
    let _ = lock.unlock();
    built
}

fn build_and_copy(workspace: &Path, target: &Path, name: &str) -> Result<(TempDir, PathBuf), String> {
    let out = std::process::Command::new("cargo")
        .env_remove("DYLD_FALLBACK_LIBRARY_PATH")
        .env_remove("DYLD_LIBRARY_PATH")
        .current_dir(workspace)
        .args([
            "build",
            "--quiet",
            "-p",
            "shards-testdir",
            "--bin",
            "shards-testhelper",
        ])
        .arg("--target-dir")
        .arg(target)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("cargo: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "building shards-testhelper: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    copy_helper(&target.join("debug").join(name), name)
}

/// `built` copied into a directory of this process's own, which it runs from.
fn copy_helper(built: &Path, name: &str) -> Result<(TempDir, PathBuf), String> {
    let dir = TempDir::new("testhelper").map_err(|e| e.to_string())?;
    let program = dir.join(name);
    program_copy(built, &program).map_err(|e| format!("{}: {e}", built.display()))?;
    Ok((dir, program))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A failing test's directory says the end of each of its logs, and nothing else.
    #[test]
    fn a_failing_tests_logs_are_said() {
        let dir = TempDir::new("logs").unwrap();
        let mut long = b"its start\n".to_vec();
        long.resize(10 + SHOWN as usize, b'x');
        std::fs::write(dir.join("daemon.log"), &long).unwrap();
        std::fs::write(dir.join("run-0.stderr"), "a run said this\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "no log\n").unwrap();
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("a/b/vm-7.sample"), "a VM's stacks\n").unwrap();
        let mut said = Vec::new();
        show_logs(&dir, 3, &mut said);
        let said = String::from_utf8(said).unwrap();
        let tail = "x".repeat(SHOWN as usize);
        assert!(
            said.contains(&format!(
                "daemon.log, its last {SHOWN} of {} bytes\n{tail}\n",
                SHOWN + 10
            )),
            "{said}"
        );
        assert!(!said.contains("its start"), "{said}");
        assert!(
            said.contains("a run said this") && said.contains("a VM's stacks"),
            "{said}"
        );
        assert!(!said.contains("no log"), "{said}");
    }

    /// A directory is made empty and removed as it goes, a panicking test's too; the
    /// process's own directory goes with its last; another process's that has ended is
    /// swept as the next is made, one that runs is not.
    #[test]
    fn a_tests_directory_goes_with_it() {
        let dir = TempDir::new("one").unwrap();
        let path = dir.to_path_buf();
        assert!(path.is_dir() && std::fs::read_dir(&path).unwrap().next().is_none());
        std::fs::write(path.join("f"), "f").unwrap();
        drop(dir);
        assert!(!path.exists());
        let held = std::thread::spawn(|| {
            let dir = TempDir::new("panicking").unwrap();
            let path = dir.to_path_buf();
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let _dir = dir;
                panic!("a failing test");
            }));
            (r.is_err(), path)
        })
        .join()
        .unwrap();
        assert!(held.0 && !held.1.exists());
        // One holding a directory none may read, write or search, which a guest may make.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let dir = TempDir::new("closed").unwrap();
            let path = dir.to_path_buf();
            std::fs::create_dir_all(path.join("d/e")).unwrap();
            std::fs::write(path.join("d/e/f"), "f").unwrap();
            for d in ["d/e", "d"] {
                std::fs::set_permissions(path.join(d), std::fs::Permissions::from_mode(0o0)).unwrap();
            }
            drop(dir);
            assert!(!path.exists());
        }
    }

    /// What [`a_test_failing_with_its_directory`] fails saying.
    const FAILED: &str = "a failure whose words outlive its binary";

    /// A panic is said on stderr as it happens, beside libtest's failures section, which
    /// libtest prints on stdout only as its binary ends, and never where its step's limit
    /// cuts the binary first (CI 43990298220, boot.rs' rate test on both x86_64 Windows
    /// jobs). Red with no hook set (`say_panics` left out): the panic's words on stdout alone.
    #[test]
    fn a_panic_is_said_as_it_happens() {
        let ran = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::a_test_failing_with_its_directory", "--ignored"])
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&ran.stderr);
        assert_eq!(ran.status.code(), Some(101), "{:?}\n{said}", ran.status);
        assert!(said.contains(FAILED), "{said}");
    }

    /// The process [`a_panic_is_said_as_it_happens`] runs.
    #[test]
    #[ignore = "a process of its own for a_panic_is_said_as_it_happens, which runs it"]
    fn a_test_failing_with_its_directory() {
        let _dir = TempDir::new("fails").unwrap();
        panic!("{FAILED}");
    }

    /// Directories made and dropped on many threads at once are each made: the process's
    /// own goes only with its last, never from under one being made (before, mkdir there
    /// failed, EINVAL on macOS, as another's drop took it).
    #[test]
    fn many_at_once_are_each_made() {
        std::thread::scope(|s| {
            for t in 0..16 {
                s.spawn(move || {
                    for i in 0..200 {
                        let dir = TempDir::new(&format!("race-{t}")).unwrap();
                        std::fs::write(dir.join("f"), i.to_string()).unwrap();
                    }
                });
            }
        });
    }

    /// Another process's directories are swept once it has ended, never while it runs, and a
    /// name that is no process's ID stays.
    #[test]
    fn an_ended_processs_directories_are_swept() {
        let parent = TempDir::new("sweep").unwrap();
        // A process that ended, its `Child` held through the sweep (on Windows its handle,
        // which keeps its ID from another process), and one that runs: this one.
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--list")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let ended = child.id();
        child.wait().unwrap();
        let running = std::process::id();
        for pid in [ended, running] {
            std::fs::create_dir_all(parent.join(pid.to_string()).join("left-0")).unwrap();
        }
        std::fs::create_dir_all(parent.join("not-a-pid")).unwrap();
        // Swept as a process that is neither sweeps.
        sweep(&parent, ended.max(running) + 1);
        assert!(!parent.join(ended.to_string()).exists());
        assert!(parent.join(running.to_string()).exists());
        assert!(parent.join("not-a-pid").exists());
    }

    /// What an ended process left under the ID this process has now is swept, though this
    /// process runs: IDs are reused, and CI 44245219858's windows-2022 job gave a process an
    /// ended one's, whose `docker-config-0` (a `static`'s, which no test drops) held
    /// `certs.d`, which the new one failed to make again (os error 183). Red with `me`'s
    /// directory swept only as another's, once its process is gone: it stays.
    #[test]
    fn directories_left_under_this_processs_id_are_swept() {
        let parent = TempDir::new("reused").unwrap();
        let me = std::process::id();
        std::fs::create_dir_all(parent.join(me.to_string()).join("x-0").join("certs.d")).unwrap();
        sweep(&parent, me);
        assert!(!parent.join(me.to_string()).exists());
    }

    /// A process given an ID an ended test process had makes its directories empty, though
    /// that one left its own: the process [`made_under_a_reused_id`] runs, with a temporary
    /// directory of this test's for the system's (TMPDIR; on Windows TMP, then TEMP:
    /// GetTempPath2W), so that it sweeps nothing of any other process's. Red with its own ID's
    /// directory left out of its first directory's sweep: making `certs.d` fails, as in CI
    /// 44245219858.
    #[test]
    fn a_reused_ids_directories_are_made_empty() {
        let tmp = TempDir::new("tmp").unwrap();
        let ran = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::made_under_a_reused_id", "--ignored"])
            .env("SHARDS_TESTDIR_REUSED", "1")
            .env("TMPDIR", tmp.path())
            .env("TMP", tmp.path())
            .env("TEMP", tmp.path())
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&ran.stdout);
        assert!(
            ran.status.success() && said.contains("1 passed"),
            "{said}{}",
            String::from_utf8_lossy(&ran.stderr)
        );
    }

    /// The process [`a_reused_ids_directories_are_made_empty`] runs: it leaves what an ended
    /// process with its ID would have, its first directory with `certs.d` in it, then makes
    /// that directory and `certs.d` in it.
    #[test]
    #[ignore = "a process of its own for a_reused_ids_directories_are_made_empty, which runs it"]
    fn made_under_a_reused_id() {
        if std::env::var_os("SHARDS_TESTDIR_REUSED").is_none() {
            return;
        }
        let left = parent().join(std::process::id().to_string()).join("x-0");
        std::fs::create_dir_all(left.join("certs.d")).unwrap();
        let dir = TempDir::new("x").unwrap();
        assert_eq!(dir.path(), left);
        std::fs::create_dir(dir.join("certs.d")).unwrap();
    }

    /// Programs are written and copied on a host with no tools to run, as the L1 guest of
    /// boot.rs `snapshot_tests_pass_on_a_kvm_host_of_this_architecture` is (`PATH=/e2e/bin`,
    /// empty, and no /bin/sh), where `cp` was not found: this test's own process, run with a
    /// `PATH` of an empty directory, writes them.
    #[cfg(unix)]
    #[test]
    fn programs_are_written_where_no_tools_are() {
        let dir = TempDir::new("no-tools").unwrap();
        let empty = dir.join("bin");
        std::fs::create_dir(&empty).unwrap();
        let text = "#!/bin/sh\nexit 0\n";
        // Never run: no program of the test's, read alone.
        std::fs::write(dir.join("source"), text).unwrap();
        let ran = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::programs_written_with_no_tools",
                "--ignored",
                "--nocapture",
            ])
            .env("PATH", &empty)
            .env("SHARDS_TESTDIR_NO_TOOLS", dir.path())
            .output()
            .unwrap();
        assert!(
            ran.status.success(),
            "{}{}",
            String::from_utf8_lossy(&ran.stdout),
            String::from_utf8_lossy(&ran.stderr)
        );
        use std::os::unix::fs::PermissionsExt as _;
        for (name, bytes) in [("written", text.as_bytes()), ("copied", text.as_bytes())] {
            let path = dir.join(name);
            assert_eq!(std::fs::read(&path).unwrap(), bytes, "{name}");
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o755,
                "{name}"
            );
        }
        assert_eq!(
            program_copy(&dir.join("missing"), &dir.join("never"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        assert!(!dir.join("never").exists());
    }

    /// The process [`programs_are_written_where_no_tools_are`] runs.
    #[cfg(unix)]
    #[test]
    #[ignore = "a process of its own for programs_are_written_where_no_tools_are, which runs it"]
    fn programs_written_with_no_tools() {
        let Some(dir) = std::env::var_os("SHARDS_TESTDIR_NO_TOOLS").map(PathBuf::from) else {
            return;
        };
        program_copy(&dir.join("source"), &dir.join("copied")).unwrap();
        program(&dir.join("written"), "#!/bin/sh\nexit 0\n").unwrap();
    }
}
