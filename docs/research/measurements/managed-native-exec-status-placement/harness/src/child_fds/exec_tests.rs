//! Native contract probes of the real map; not broker/actor-view integration proof.

use super::*;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;

#[derive(Clone, Copy, Debug)]
enum Image {
    Elf,
    Shebang,
    ReadScript,
}

#[derive(Debug)]
struct Receipt {
    image: Image,
    status: i32,
    native: Vec<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    descriptors: (usize, usize),
    no_wait_status_at_status_eof: Option<bool>,
    input_write_errno: Option<i32>,
}

fn pipe() -> (OwnedFd, OwnedFd) {
    let mut fds = [0; 2];
    // SAFETY: this is a live two-element pipe2 output array.
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    let [read, write] = fds;
    // SAFETY: successful pipe2 returned both newly owned descriptors.
    unsafe { (OwnedFd::from_raw_fd(read), OwnedFd::from_raw_fd(write)) }
}

fn read(fd: OwnedFd) -> Vec<u8> {
    let mut bytes = Vec::new();
    std::fs::File::from(fd).read_to_end(&mut bytes).unwrap();
    bytes
}

fn fd_count() -> usize {
    std::fs::read_dir("/proc/self/fd").unwrap().count()
}

fn probe(image: Image) -> Receipt {
    let before = fd_count();
    let tmp = shards_testdir::TempDir::new("held-exec").unwrap();
    let stdout;
    let stderr;
    let status;
    let native;
    let no_wait_status_at_status_eof;
    let input_write_errno;
    {
        // cat is an explicit read interpreter, not a shell or execution fallback.
        // It reads the kernel-supplied /dev/fd/4 argument in the script case.
        let mut interpreter = std::fs::File::open("/bin/cat").unwrap();
        let mut magic = [0; 4];
        interpreter.read_exact(&mut magic).unwrap();
        assert_eq!(magic, *b"\x7fELF", "fixture interpreter must be an ELF image");
        let script = b"#!/bin/cat -\nliteral $(no-command);*\xff\n".as_slice();
        let expected = match image {
            Image::Elf => {
                std::fs::copy("/bin/cat", tmp.join("program")).unwrap();
                std::fs::write(tmp.join("input"), b"literal-elf\0\xff").unwrap();
                b"literal-elf\0\xff".as_slice()
            }
            Image::Shebang | Image::ReadScript => {
                std::fs::write(tmp.join("program"), script).unwrap();
                script
            }
        };
        std::fs::set_permissions(tmp.join("program"), std::fs::Permissions::from_mode(0o500)).unwrap();
        let program = std::fs::File::open(tmp.join("program")).unwrap();
        let held = program.metadata().unwrap();
        assert!(held.is_file());
        // The selected file still exists under another name. A replacement at the
        // original pathname must not be selected by this execveat caller.
        std::fs::rename(tmp.join("program"), tmp.join("held")).unwrap();
        std::fs::write(tmp.join("program"), b"replacement; must not execute").unwrap();
        let workdir = std::fs::File::open(&tmp).unwrap();
        let null = std::fs::File::open("/dev/null").unwrap();
        let input = matches!(image, Image::Shebang | Image::ReadScript).then(pipe);
        let stdin = input
            .as_ref()
            .map(|(read, _)| read.as_fd())
            .unwrap_or(null.as_fd());
        let executable_inheritance = match image {
            Image::ReadScript => Inheritance::RetainAcrossExec,
            Image::Elf | Image::Shebang => Inheritance::SetupOnly,
        };
        let (out_read, out_write) = pipe();
        let (err_read, err_write) = pipe();
        let (exec_read, exec_write) = pipe();
        // This is a disposable file, not a server key/control descriptor.
        let private = std::fs::File::open(tmp.join("program")).unwrap();
        // SAFETY: duplicate this fixture's held object above every mapped target.
        let forbidden = unsafe { libc::fcntl(private.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 6) };
        assert!(forbidden >= 6);
        // SAFETY: fcntl just returned a new descriptor owned only by this test.
        let forbidden = unsafe { OwnedFd::from_raw_fd(forbidden) };
        let forbidden_raw = forbidden.as_raw_fd();
        let bindings = [
            (stdin, 0, Inheritance::RetainAcrossExec),
            (out_write.as_fd(), 1, Inheritance::RetainAcrossExec),
            (err_write.as_fd(), 2, Inheritance::RetainAcrossExec),
            (exec_write.as_fd(), 3, Inheritance::SetupOnly),
            (program.as_fd(), 4, executable_inheritance),
            (workdir.as_fd(), 5, Inheritance::SetupOnly),
        ]
        .map(|(source, destination, inheritance)| Binding {
            source,
            destination,
            inheritance,
        });
        let map = PreparedFdMap::prepare(&bindings).unwrap();
        let argv = match image {
            Image::Elf => [c"held-elf".as_ptr(), c"input".as_ptr(), std::ptr::null()],
            Image::Shebang | Image::ReadScript => {
                [c"held-script".as_ptr(), std::ptr::null(), std::ptr::null()]
            }
        };
        let envp = [std::ptr::null::<libc::c_char>()];
        // SAFETY: the disposable fork child runs raw descriptor/exec calls and
        // _exit only, without allocation or inherited Rust destructors.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            // SAFETY: this child owns its independent FD table. Both setup and
            // exec failure exit; no branch returns into the test runtime.
            unsafe {
                if map.install_in_child().is_err() || libc::fchdir(5) != 0 {
                    libc::_exit(1);
                }
                let flags = libc::fcntl(4, libc::F_GETFD);
                let access = libc::fcntl(4, libc::F_GETFL);
                let written = libc::write(4, b"!".as_ptr().cast(), 1);
                let write_errno = if written == -1 {
                    *libc::__errno_location()
                } else {
                    0
                };
                let surplus = libc::fcntl(forbidden_raw, libc::F_GETFD);
                let surplus_errno = if surplus == -1 {
                    *libc::__errno_location()
                } else {
                    0
                };
                for value in [flags, access, write_errno, surplus, surplus_errno] {
                    raw_word(3, value);
                }
                libc::syscall(
                    libc::SYS_execveat,
                    4,
                    c"".as_ptr(),
                    argv.as_ptr(),
                    envp.as_ptr(),
                    libc::AT_EMPTY_PATH,
                );
                let exec_errno = *libc::__errno_location();
                raw_word(3, exec_errno);
                libc::_exit(1);
            }
        }
        drop(map);
        drop(out_write);
        drop(err_write);
        drop(exec_write);
        // Observe status before releasing input. WNOHANG0 alone cannot prove
        // readiness: an exiting failed-exec child may not be waitable yet.
        let status_bytes = read(exec_read);
        let mut child_status = 0;
        let mut reaped = false;
        no_wait_status_at_status_eof = input.as_ref().map(|_| {
            let observed = wait(pid, &mut child_status, libc::WNOHANG);
            reaped = observed == pid;
            observed == 0
        });
        input_write_errno = input.and_then(|(reader, writer)| {
            drop(reader);
            let mut writer = std::fs::File::from(writer);
            // Drop this sole remaining writer even on EPIPE, before any assertion
            // about the observation; the child must be able to reach EOF/end.
            writer
                .write_all(b"stdin\0\xff")
                .err()
                .and_then(|error| error.raw_os_error())
        });
        // SAFETY: this test still owns the unreaped direct child, and no other
        // code in the fixture waits for that PID.
        if !reaped {
            assert_eq!(wait(pid, &mut child_status, 0), pid);
        }
        assert!(libc::WIFEXITED(child_status));
        status = libc::WEXITSTATUS(child_status);
        assert_eq!(status_bytes.len() % size_of::<i32>(), 0);
        let (words, tail) = status_bytes.as_chunks::<{ size_of::<i32>() }>();
        assert!(tail.is_empty());
        native = words.iter().copied().map(i32::from_be_bytes).collect();
        stdout = read(out_read);
        stderr = read(err_read);
        assert_eq!(
            std::fs::read(tmp.join("held")).unwrap(),
            match image {
                Image::Elf => std::fs::read("/bin/cat").unwrap(),
                Image::Shebang | Image::ReadScript => expected.to_vec(),
            }
        );
        assert_eq!(
            std::fs::read(tmp.join("program")).unwrap(),
            b"replacement; must not execute"
        );
        // SAFETY: the first wait already reaped this test's direct child; this
        // second native wait checks ECHILD rather than inferring it from EOF.
        let rewait = unsafe { libc::waitpid(pid, &raw mut child_status, libc::WNOHANG) };
        assert_eq!(rewait, -1);
        // SAFETY: copy the same thread's errno immediately after the failed wait.
        assert_eq!(unsafe { *libc::__errno_location() }, libc::ECHILD);
    }
    drop(tmp);
    Receipt {
        image,
        status,
        native,
        stdout,
        stderr,
        descriptors: (before, fd_count()),
        no_wait_status_at_status_eof,
        input_write_errno,
    }
}

fn wait(pid: libc::pid_t, status: &mut i32, options: i32) -> libc::pid_t {
    loop {
        // SAFETY: pid is this fixture's unreaped direct child and status is live.
        let observed = unsafe { libc::waitpid(pid, status, options) };
        // SAFETY: copy this thread's errno immediately after the native call.
        let errno = unsafe { *libc::__errno_location() };
        if observed < 0 && errno == libc::EINTR {
            continue;
        }
        return observed;
    }
}

unsafe fn raw_word(fd: RawFd, value: i32) {
    let bytes = value.to_be_bytes();
    let mut at = 0;
    while at < bytes.len() {
        let Some(tail) = bytes.get(at..) else {
            // SAFETY: an invalid fixture suffix exits this post-fork child only.
            unsafe { libc::_exit(1) }
        };
        // SAFETY: the borrowed initialized suffix lives through this raw write.
        let n = unsafe { libc::write(fd, tail.as_ptr().cast(), tail.len()) };
        // SAFETY: read this thread's errno immediately after its raw write.
        let errno = unsafe { *libc::__errno_location() };
        if n < 0 && errno == libc::EINTR {
            continue;
        }
        if n <= 0 {
            // SAFETY: a failed fixture status write exits without Rust cleanup.
            unsafe { libc::_exit(1) };
        }
        let Some(next) = usize::try_from(n)
            .ok()
            .and_then(|n| at.checked_add(n))
            .filter(|n| *n <= bytes.len())
        else {
            // SAFETY: a malformed native length exits this post-fork child only.
            unsafe { libc::_exit(1) }
        };
        at = next;
    }
}

fn check_map(receipt: &Receipt) {
    // The private light driver runs this filter serially; retain actual process
    // FD counts and native words before assertions, including on a failed probe.
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{receipt:?}");
    let _ = writeln!(
        out,
        "image={:?}; ECHILD observed; FD before/after={:?}",
        receipt.image, receipt.descriptors
    );
    let flags = receipt.native.first().copied().unwrap();
    let access = receipt.native.get(1).copied().unwrap();
    assert!(flags >= 0 && access >= 0);
    assert_eq!(
        flags & libc::FD_CLOEXEC != 0,
        !matches!(receipt.image, Image::ReadScript)
    );
    assert_eq!(access & libc::O_ACCMODE, libc::O_RDONLY);
    assert_eq!(receipt.native.get(2).copied(), Some(libc::EBADF));
    assert_eq!(receipt.native.get(3).copied(), Some(-1));
    assert_eq!(receipt.native.get(4).copied(), Some(libc::EBADF));
    assert!(receipt.stderr.is_empty());
}

#[test]
fn held_elf_cloexec_execveat_baseline() {
    let receipt = probe(Image::Elf);
    check_map(&receipt);
    assert_eq!(receipt.status, 0);
    assert_eq!(receipt.native.len(), 5);
    assert_eq!(receipt.stdout, b"literal-elf\0\xff");
}

#[test]
fn held_shebang_cloexec_execveat_reports_native_enoent() {
    let receipt = probe(Image::Shebang);
    check_map(&receipt);
    assert_eq!(receipt.status, 1);
    assert_eq!(receipt.native.get(5).copied(), Some(libc::ENOENT));
    assert_eq!(receipt.native.len(), 6);
    assert!(receipt.stdout.is_empty());
    // This expected-negative contract witness is not a passing script executor.
}

#[test]
fn held_readonly_script_fd_allows_the_declared_interpreter() {
    let receipt = probe(Image::ReadScript);
    check_map(&receipt);
    assert_eq!(receipt.no_wait_status_at_status_eof, Some(true));
    assert_eq!(receipt.input_write_errno, None);
    assert_eq!(receipt.status, 0);
    assert_eq!(receipt.native.len(), 5);
    assert_eq!(
        receipt.stdout,
        b"stdin\0\xff#!/bin/cat -\nliteral $(no-command);*\xff\n"
    );
}
