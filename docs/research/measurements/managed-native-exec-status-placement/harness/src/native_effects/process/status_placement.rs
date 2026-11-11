use super::super::*;
use crate::lifecycle;
use std::io::{Read as _, Write as _};
use std::os::fd::{AsFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::fs::PermissionsExt as _;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Case {
    BeforeMap,
    AfterExec,
}

#[derive(Debug)]
struct Receipt {
    case: Case,
    pid: libc::pid_t,
    before_status: RawFd,
    waited: libc::pid_t,
    status: i32,
    rewait: libc::pid_t,
    rewait_errno: i32,
    packet: Vec<u8>,
    wrong_endpoint: Vec<u8>,
    output: Vec<u8>,
    error: Vec<u8>,
    descriptors: (usize, usize),
}

fn pipe() -> (OwnedFd, OwnedFd) {
    let mut fds = [0; 2];
    // SAFETY: pipe2 writes the complete two-entry output array.
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    let [read, write] = fds;
    // SAFETY: successful pipe2 transferred both fresh descriptors to this fixture.
    unsafe { (OwnedFd::from_raw_fd(read), OwnedFd::from_raw_fd(write)) }
}

fn read(fd: OwnedFd) -> Vec<u8> {
    let mut bytes = Vec::new();
    std::fs::File::from(fd).read_to_end(&mut bytes).unwrap();
    bytes
}

fn descriptors() -> usize {
    std::fs::read_dir("/proc/self/fd").unwrap().count()
}

/// Only the two observed native failures have a canonical bridge here. Unknown
/// failures cannot turn into a synthetic errno or an empty successful record.
fn canonical(failure: Failure) -> Option<lifecycle::Failure> {
    match failure {
        Failure::Fds(shards_ipc::child_fds::Failure::Native {
            operation: shards_ipc::child_fds::Operation::Install { destination: 0 },
            errno: Some(errno),
        }) => lifecycle::Failure::native(lifecycle::Step::Setup, errno).ok(),
        Failure::Native {
            operation: Operation::Exec,
            errno,
        } => lifecycle::Failure::native(lifecycle::Step::Exec, errno.get()).ok(),
        _ => None,
    }
}

/// # Safety
/// The fixture's raw child only: all bytes are stack-owned, descriptors are held,
/// and every caller exits without running inherited Rust owners' destructors.
unsafe fn write_packet(fd: RawFd, packet: &[u8]) -> bool {
    let mut pending = packet;
    while !pending.is_empty() {
        // SAFETY: pending is an initialized live suffix and fd is the chosen held writer.
        let count = unsafe { libc::write(fd, pending.as_ptr().cast(), pending.len()) };
        if count < 0 {
            // SAFETY: copy this thread's errno immediately after failed write.
            if unsafe { *libc::__errno_location() } == libc::EINTR {
                continue;
            }
            return false;
        }
        let Some(count) = usize::try_from(count).ok().filter(|count| *count > 0) else {
            return false;
        };
        let Some(tail) = pending.get(count..) else {
            return false;
        };
        pending = tail;
    }
    true
}

/// # Safety
/// Runs after a disposable fork with its own FD table, then raw-exits. The
/// deliberate close affects only this child's private staged input, never a
/// helper/owner/credential endpoint. No untrusted image can start in either case.
unsafe fn child(
    plan: &Prepared,
    case: Case,
    staged_input: RawFd,
    staged_status: RawFd,
    wrong_writer: RawFd,
) -> ! {
    // SAFETY: this private child first places a disposable wrong-endpoint witness
    // at3. All prepared sources are already staged above every destination.
    unsafe {
        if libc::dup3(wrong_writer, STATUS_FD, libc::O_CLOEXEC) < 0 {
            libc::_exit(1);
        }
        if case == Case::BeforeMap && libc::close(staged_input) != 0 {
            libc::_exit(1);
        }
    }
    // SAFETY: this fixture tests mechanics only in a raw private child. Production
    // callers separately require the real owner/admission/ACK/audit path.
    let failure = match unsafe { plan.exec_in_child() } {
        Err(failure) => failure,
        Ok(never) => match never {},
    };
    let expected = match case {
        Case::BeforeMap => matches!(
            failure,
            Failure::Fds(shards_ipc::child_fds::Failure::Native {
                operation: shards_ipc::child_fds::Operation::Install { destination: 0 },
                errno: Some(libc::EBADF),
            })
        ),
        Case::AfterExec => matches!(
            failure,
            Failure::Native { operation: Operation::Exec, errno } if errno.get() == libc::ENOEXEC
        ),
    };
    let location = failure.status_location();
    let fd = match location {
        StatusLocation::Staged if case == Case::BeforeMap => staged_status,
        StatusLocation::Installed if case == Case::AfterExec => STATUS_FD,
        _ => {
            // SAFETY: Unknown never selects an assumed descriptor or reports success.
            unsafe { libc::_exit(1) }
        }
    };
    // SAFETY: inspect only this child's saved raw descriptors. A complete map
    // closes the staged status copy; the early failed installation cannot do so.
    let status_flags = unsafe { libc::fcntl(staged_status, libc::F_GETFD) };
    // SAFETY: capture this thread's actual errno before altering it below.
    let status_errno = unsafe { *libc::__errno_location() };
    let placement = match case {
        Case::BeforeMap => status_flags >= 0,
        Case::AfterExec => status_flags == -1 && status_errno == libc::EBADF,
    };
    let Some(canonical) = canonical(failure) else {
        // SAFETY: a missing typed conversion is uncertainty, never an invented record.
        unsafe { libc::_exit(1) }
    };
    // SAFETY: this child reads only its own actual PID; the codec grants nothing.
    let pid = unsafe { libc::getpid() };
    let Ok(bytes) = lifecycle::Packet::failed(pid, canonical).and_then(lifecycle::Packet::encode) else {
        // SAFETY: malformed local metadata cannot produce a success packet.
        unsafe { libc::_exit(1) }
    };
    // SAFETY: test-only ambient errno clobber demonstrates that reporting uses
    // the saved typed failure, after descriptor checks have already changed errno.
    unsafe { *libc::__errno_location() = libc::EINTR };
    // SAFETY: the chosen endpoint remains held for this child's stack packet.
    let sent = unsafe { write_packet(fd, &bytes) };
    // SAFETY: never run parent allocator/runtime/destructors after raw map closure.
    unsafe { libc::_exit(i32::from(!(expected && placement && sent))) }
}

fn wait(pid: libc::pid_t, status: &mut i32, options: i32) -> libc::pid_t {
    loop {
        // SAFETY: pid is this fixture's direct child and status is a live native output.
        let result = unsafe { libc::waitpid(pid, status, options) };
        if result >= 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return result;
        }
    }
}

fn probe(case: Case) -> Receipt {
    let before = descriptors();
    let root = shards_testdir::TempDir::new("native-status").unwrap();
    let (pid, staged_status, waited, status, rewait, rewait_errno, packet, wrong_endpoint, output, error) = {
        let image = root.join("invalid-image");
        std::fs::write(&image, b"private non-ELF image; never a shell program").unwrap();
        std::fs::set_permissions(&image, std::fs::Permissions::from_mode(0o500)).unwrap();
        std::fs::write(root.join("input"), b"").unwrap();
        let input = std::fs::File::open(root.join("input")).unwrap();
        let (out_read, out_write) = pipe();
        let (err_read, err_write) = pipe();
        let (status_read, status_write) = pipe();
        let (wrong_read, wrong_write) = pipe();
        let request = Request {
            executable: Executable::check(std::fs::File::open(&image).unwrap().into()).unwrap(),
            workdir: Workdir::check(std::fs::File::open(&root).unwrap().into()).unwrap(),
            literals: Literals::prepare(&[b"status-fixture"], &[]).unwrap(),
            lifetime: ExecutableLifetime::ImageOnly,
        };
        let plan = match Prepared::prepare(
            request,
            Streams {
                input: input.as_fd(),
                output: out_write.as_fd(),
                error: err_write.as_fd(),
                launch_status: status_write.as_fd(),
            },
        ) {
            Preparation::Prepared(plan) => plan,
            Preparation::Rejected(rejected) => panic!("preparing fixture: {rejected:?}"),
        };
        let metadata = plan.metadata();
        assert_eq!((metadata.arguments, metadata.environment), (1, 0));
        assert_eq!(metadata.retained.staged_descriptors, 6);
        // The production getter is captured before birth; this value is never
        // borrowed again after the child begins installing/closing the map.
        let staged_status = plan.launch_status_before_install().unwrap().as_raw_fd();
        // Test-only private access selects the one disposable source to close.
        let staged_input = plan.map.staged_source(0).unwrap().as_raw_fd();
        assert!(staged_status > WORKDIR_FD && wrong_write.as_raw_fd() > WORKDIR_FD);
        // SAFETY: the child runs the raw function above and cannot return into
        // libtest, allocate, unwind or run inherited Rust owners' destructors.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            // SAFETY: this is the private independent-table raw child just created.
            unsafe { child(&plan, case, staged_input, staged_status, wrong_write.as_raw_fd()) }
        }
        drop(plan);
        drop(input);
        drop(out_write);
        drop(err_write);
        drop(status_write);
        drop(wrong_write);
        let mut status = 0;
        let waited = wait(pid, &mut status, 0);
        let packet = read(status_read);
        let wrong_endpoint = read(wrong_read);
        let output = read(out_read);
        let error = read(err_read);
        let mut again = 0;
        let rewait = wait(pid, &mut again, libc::WNOHANG);
        let rewait_errno = std::io::Error::last_os_error().raw_os_error().unwrap();
        (
            pid,
            staged_status,
            waited,
            status,
            rewait,
            rewait_errno,
            packet,
            wrong_endpoint,
            output,
            error,
        )
    };
    drop(root);
    Receipt {
        case,
        pid,
        before_status: staged_status,
        waited,
        status,
        rewait,
        rewait_errno,
        packet,
        wrong_endpoint,
        output,
        error,
        descriptors: (before, descriptors()),
    }
}

fn check(receipt: Receipt) {
    let mut output = std::io::stdout().lock();
    writeln!(
        output,
        "STATUS_PLACEMENT {receipt:?}; kernel_population=Unknown authority=MechanicsOnly"
    )
    .unwrap();
    assert_eq!(receipt.waited, receipt.pid);
    assert!(receipt.before_status > WORKDIR_FD);
    assert!(libc::WIFEXITED(receipt.status));
    assert_eq!(libc::WEXITSTATUS(receipt.status), 0);
    assert_eq!((receipt.rewait, receipt.rewait_errno), (-1, libc::ECHILD));
    let (step, errno) = match receipt.case {
        Case::BeforeMap => (lifecycle::Step::Setup, libc::EBADF),
        Case::AfterExec => (lifecycle::Step::Exec, libc::ENOEXEC),
    };
    let expected =
        lifecycle::Packet::failed(receipt.pid, lifecycle::Failure::native(step, errno).unwrap()).unwrap();
    assert_eq!(receipt.packet.len(), lifecycle::PACKET_BYTES);
    assert_eq!(lifecycle::Packet::decode(&receipt.packet), Ok(expected));
    assert!(receipt.wrong_endpoint.is_empty());
    assert!(receipt.output.is_empty() && receipt.error.is_empty());
    assert_eq!(receipt.descriptors.0, receipt.descriptors.1);
}

#[test]
fn before_map_install_failure_reports_saved_errno_on_staged_status() {
    check(probe(Case::BeforeMap));
}

#[test]
fn post_map_exec_failure_reports_saved_errno_on_installed_status() {
    check(probe(Case::AfterExec));
}
