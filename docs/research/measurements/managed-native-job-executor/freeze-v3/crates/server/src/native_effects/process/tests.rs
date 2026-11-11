use super::super::literals::{Environment, Failure as LiteralFailure};
use super::*;
use std::io::{Read as _, Write as _};
use std::os::fd::{AsFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;

fn pipe() -> (OwnedFd, OwnedFd) {
    let mut fds = [0; 2];
    // SAFETY: pipe2 writes exactly the initialized two-entry output array.
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    let [read, write] = fds;
    // SAFETY: successful pipe2 transferred ownership of both new descriptors.
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

#[test]
fn denied_literal_request_and_script_access_preserve_objects_before_birth() {
    assert!(matches!(
        Literals::prepare(&[], &[]),
        Err(LiteralFailure::EmptyArgv)
    ));
    assert!(matches!(
        Literals::prepare(&[b"nul\0"], &[]),
        Err(LiteralFailure::Nul)
    ));
    let root = shards_testdir::TempDir::new("native-job-pure").unwrap();
    let program = root.join("image");
    std::fs::write(&program, b"not executed by this pure preparation case").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o500)).unwrap();
    let cpath = std::ffi::CString::new(program.as_os_str().as_bytes()).unwrap();
    // SAFETY: open holds this test's file with O_PATH; no native child is started.
    let raw = unsafe { libc::open(cpath.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    assert!(raw >= 0);
    // SAFETY: successful open transferred this newly owned descriptor.
    let object = unsafe { OwnedFd::from_raw_fd(raw) };
    let original = super::super::file::stat(object.as_fd()).unwrap();
    let request = Request {
        executable: Executable::check(object).unwrap(),
        workdir: Workdir::check(std::fs::File::open(&root).unwrap().into()).unwrap(),
        literals: Literals::prepare(&[b"unused"], &[]).unwrap(),
        lifetime: ExecutableLifetime::ReadOnlyScript,
    };
    let null = std::fs::File::open("/dev/null").unwrap();
    let rejected = Prepared::prepare(
        request,
        Streams {
            input: null.as_fd(),
            output: null.as_fd(),
            error: null.as_fd(),
            launch_status: null.as_fd(),
        },
    )
    .unwrap_err();
    assert!(
        matches!(rejected.failure, Failure::ScriptRequiresReadOnly { actual_flags } if actual_flags & libc::O_PATH != 0)
    );
    let retained = super::super::file::stat(rejected.request.executable.fd()).unwrap();
    assert_eq!(
        (retained.st_dev, retained.st_ino),
        (original.st_dev, original.st_ino)
    );
}

#[test]
#[ignore = "ordinary native child fixture; caller supplies exact compiled image and explicit bounded outer deadline"]
fn ordinary_literal_job_has_exact_bytes_environment_workdir_and_owned_reap() {
    let before = descriptors();
    let fixture = std::env::var_os("SHARDS_JOB_FIXTURE_BINARY").unwrap();
    let root = shards_testdir::TempDir::new("native-job-ordinary").unwrap();
    let work = root.join("work");
    std::fs::create_dir(&work).unwrap();
    let selected = root.join("selected");
    std::fs::copy(&fixture, &selected).unwrap();
    std::fs::set_permissions(&selected, std::fs::Permissions::from_mode(0o500)).unwrap();
    let object = std::fs::File::open(&selected).unwrap();
    let workdir = std::fs::File::open(&work).unwrap();
    std::fs::rename(&selected, root.join("held-image")).unwrap();
    std::fs::write(&selected, b"replacement never selected").unwrap();
    let original_directory = root.join("renamed-work");
    std::fs::rename(&work, &original_directory).unwrap();
    std::fs::create_dir(&work).unwrap();
    let input_bytes = b"input\0\xff\n";
    std::fs::write(original_directory.join("input"), input_bytes).unwrap();
    let input = std::fs::File::open(original_directory.join("input")).unwrap();
    let (out_read, out_write) = pipe();
    let (err_read, err_write) = pipe();
    let (status_read, status_write) = pipe();
    let (release_read, release_write) = pipe();
    let release_number = release_read.as_raw_fd().to_string();
    let literal = b"$(not-a-command);*\n\xff";
    let request = Request {
        executable: Executable::check(object.into()).unwrap(),
        workdir: Workdir::check(workdir.into()).unwrap(),
        literals: Literals::prepare(
            &[b"native-job-fixture", literal],
            &[Environment {
                name: b"ONLY",
                value: b"literal=$HOME;\xff",
            }],
        )
        .unwrap(),
        lifetime: ExecutableLifetime::ImageOnly,
    };
    let plan = Prepared::prepare(
        request,
        Streams {
            input: input.as_fd(),
            output: out_write.as_fd(),
            error: err_write.as_fd(),
            launch_status: status_write.as_fd(),
        },
    )
    .unwrap();
    let retained = plan.retained();
    let metadata = plan.metadata();
    assert_eq!((metadata.arguments, metadata.environment), (2, 1));
    let executable_identity = std::fs::metadata(root.join("held-image")).unwrap();
    let workdir_identity = std::fs::metadata(&original_directory).unwrap();
    use std::os::unix::fs::MetadataExt as _;
    assert_eq!(
        metadata.executable,
        ObjectIdentity {
            device: executable_identity.dev(),
            inode: executable_identity.ino()
        }
    );
    assert_eq!(
        metadata.workdir,
        ObjectIdentity {
            device: workdir_identity.dev(),
            inode: workdir_identity.ino()
        }
    );
    let staged_status = plan.launch_status_before_install().unwrap().as_raw_fd();
    let mut expected_out = Vec::new();
    for value in [
        literal.as_slice(),
        b"ONLY=literal=$HOME;\xff",
        input_bytes.as_slice(),
        original_directory.as_os_str().as_bytes(),
    ] {
        expected_out.extend_from_slice(value);
        expected_out.push(0);
    }
    // This ordinary fixture does not manufacture an owner-issued grant. It
    // exercises only the mechanics after an already-owned disposable fork.
    // SAFETY: the child uses raw gate/map/exec/_exit only, no Rust allocation,
    // inherited destructor or return to the test runtime.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        // SAFETY: private child gate guarantees parent holds its pidfd before
        // any prepared exec call; all return paths raw-exit this owned child.
        unsafe {
            let mut word = 0u8;
            if libc::read(release_read.as_raw_fd(), (&raw mut word).cast(), 1) != 1 || word != b'R' {
                libc::_exit(1);
            }
            if let Err(failure) = plan.exec_in_child() {
                let code = match failure {
                    Failure::Native { errno, .. } => errno.get(),
                    Failure::InvalidErrno { actual, .. } => actual,
                    _ => -1,
                };
                let bytes = code.to_be_bytes();
                let fd = match failure.status_location() {
                    StatusLocation::Staged => staged_status,
                    StatusLocation::Installed => STATUS_FD,
                    StatusLocation::Unknown => libc::_exit(1),
                };
                let _ = libc::write(fd, bytes.as_ptr().cast(), bytes.len());
                libc::_exit(1);
            }
            libc::_exit(1);
        }
    }
    // SAFETY: this unreaped direct child's PID cannot be recycled; its gate is held.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) };
    assert!(raw >= 0);
    // SAFETY: successful pidfd_open returned this parent's unique owned descriptor.
    let held = unsafe { OwnedFd::from_raw_fd(raw as i32) };
    drop(plan);
    drop(input);
    drop(out_write);
    drop(err_write);
    drop(status_write);
    drop(release_read);
    std::fs::File::from(release_write).write_all(b"R").unwrap();
    let mut poll = libc::pollfd {
        fd: held.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: event wait on this held child, bounded by the outer test driver.
    assert_eq!(unsafe { libc::poll(&raw mut poll, 1, -1) }, 1);
    let ready = poll.revents & libc::POLLIN != 0;
    let mut status = 0;
    // SAFETY: wait only for this fixture's unreaped direct child.
    let waited = unsafe { libc::waitpid(pid, &raw mut status, 0) };
    let stdout = read(out_read);
    let stderr = read(err_read);
    let launch = read(status_read);
    let mut second_status = 0;
    // SAFETY: second wait proves this exact child is no longer waitable.
    let rewait = unsafe { libc::waitpid(pid, &raw mut second_status, libc::WNOHANG) };
    // SAFETY: capture errno immediately after failed wait.
    let rewait_errno = unsafe { *libc::__errno_location() };
    drop(held);
    let replacement = std::fs::read(&selected).unwrap();
    drop(root);
    let after = descriptors();
    let mut out = std::io::stdout().lock();
    writeln!(out, "NATIVE_JOB pid={pid} pidfd_exit_ready={ready} wait={waited} rewait={rewait} errno={rewait_errno} native_status={status} status_eof={} stdout_hex={} stderr_hex={} fd_before={before} fd_after={after} staged_fds={} literal_bytes={} gate_fd={release_number} executable={}:{} workdir={}:{} kernel_population=Unknown authority=MechanicsOnly",
        launch.is_empty(), hex(&stdout), hex(&stderr), retained.staged_descriptors, retained.literal_bytes, metadata.executable.device, metadata.executable.inode, metadata.workdir.device, metadata.workdir.inode).unwrap();
    assert_eq!(waited, pid);
    assert!(ready);
    assert!(libc::WIFEXITED(status));
    assert_eq!(libc::WEXITSTATUS(status), 0);
    assert_eq!((rewait, rewait_errno), (-1, libc::ECHILD));
    assert!(launch.is_empty());
    assert_eq!(stdout, expected_out);
    assert_eq!(stderr, b"stderr\0\xff");
    assert_eq!(replacement, b"replacement never selected");
    assert_eq!(retained.staged_descriptors, 6);
    assert_eq!(after, before);
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
