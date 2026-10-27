//! Warm VMs, end to end: `shards restore TEMPLATE --warm FD` resumes a template of an
//! image, lets its guest connect, and serves one request that arrives on FD
//! (crates/shards/src/warm.rs). Here the test is both the daemon, which hands over a
//! command with a client's connection and stdio, and that client. Needs vsock and
//! snapshots. On Windows FD is a pipe's handle `shards` inherits and gives the VM process it
//! starts confined by name, as the daemon gives its VMs their links, and the test gives the
//! VM process its request's handles as the daemon does, duplicated into it.

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]

mod common;

use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{TempDir, cannot_pool, cannot_run_vms, guest_init, kernel, workload_image};
use shards_abi::run::Spec;
use shards_ipc::Stream;
use shards_ipc::fd::{AsFd as _, OwnedFd};
use shards_ipc::kind;

const TIMEOUT: Duration = Duration::from_secs(60);

/// A pipe's ends, read and write: closed on exec, handed on by message alone.
fn pipe() -> (File, File) {
    let (read, write) = std::io::pipe().unwrap();
    (File::from(OwnedFd::from(read)), File::from(OwnedFd::from(write)))
}

/// Every process this binary starts is started under it: on Windows a warm VM's link is
/// inheritable while its `shards` starts, and a spawn by std hands every inheritable handle
/// on, so that a process another test started meanwhile would hold the link, and the VM's
/// end would outlive it.
static SPAWNING: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn spawning() -> std::sync::MutexGuard<'static, ()> {
    SPAWNING.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What `cmd` says, run under [`SPAWNING`].
fn output(cmd: &mut Command) -> std::process::Output {
    let _turn = spawning();
    cmd.output().unwrap()
}

/// Saves a template of the test image, booted and mounted.
fn template(dir: &Path) -> PathBuf {
    let image = workload_image(dir);
    let template = dir.join("template");
    let saved = output(
        common::command()
            .args(["run", "--kernel"])
            .arg(kernel())
            .arg("--init")
            .arg(guest_init())
            .arg("--rootfs")
            .arg(&image)
            .arg("--snapshot-dir")
            .arg(&template)
            .arg("--no-console")
            .stdin(Stdio::null()),
    );
    assert!(
        saved.status.success(),
        "{}",
        String::from_utf8_lossy(&saved.stderr)
    );
    template
}

/// A warm VM, and the daemon's end of its link.
struct Warm {
    /// The VM process on Unix; on Windows the `shards` that started it, and waits for it.
    child: Child,
    daemon: Stream,
    /// The VM process's ID, once it is ready: on Windows the child's child.
    vm: Option<u32>,
    /// On macOS, what grants the VM its files, as the daemon does.
    #[cfg(target_os = "macos")]
    _broker: Broker,
}

/// `shardsd grants`, serving a VM's asks: killed if it outlives its test, and reaped.
#[cfg(target_os = "macos")]
struct Broker(Child);

#[cfg(target_os = "macos")]
impl Broker {
    /// A broker on one end of a new pair; the VM's end is returned.
    fn spawn() -> (Broker, Stream) {
        let (vm, theirs) = Stream::pair().unwrap();
        let fd = theirs.as_raw_fd();
        let mut command = common::command();
        command.arg("grants").stdin(Stdio::null()).stdout(Stdio::null());
        // SAFETY: runs in the child between fork and exec, calling only dup2(2) and
        // fcntl(2), which are async-signal-safe.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(fd, 3) < 0 || libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let broker = Broker(command.spawn().unwrap());
        drop(theirs);
        (broker, vm)
    }
}

#[cfg(target_os = "macos")]
impl Drop for Broker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// What the client holds while its command runs.
struct Client {
    conn: Stream,
    stdin: Option<File>,
    stdout: BufReader<File>,
    stderr: File,
}

/// The value `--warm` takes in `args`, the VM's end of its link: a descriptor number on
/// Unix ([`Warm::spawn_with`] places it at 3), a handle's value on Windows.
const LINK: &str = "LINK";

impl Warm {
    fn spawn(template: &Path) -> Warm {
        Warm::spawn_env(template, &[])
    }

    /// [`spawn`](Warm::spawn), with `env` added to the VM's environment.
    fn spawn_env(template: &Path, env: &[(&str, &str)]) -> Warm {
        let args = [
            "restore".as_ref(),
            template.as_os_str(),
            "--warm".as_ref(),
            LINK.as_ref(),
        ];
        Warm::spawn_with(&args, env)
    }

    /// A warm VM booted into `image`, as the daemon boots one for a run when its image has
    /// no template: it saves one to `save`, then takes the run.
    fn saving(image: &Path, save: &Path) -> Warm {
        let args = [
            "run".as_ref(),
            "--kernel".as_ref(),
            kernel().as_os_str(),
            "--init".as_ref(),
            guest_init().as_os_str(),
            "--rootfs".as_ref(),
            image.as_os_str(),
            "--snapshot-dir".as_ref(),
            save.as_os_str(),
            "--warm".as_ref(),
            LINK.as_ref(),
        ];
        Warm::spawn_with(&args, &[])
    }

    /// shards-vm with `args`, its daemon's socket at descriptor 3, and `env`.
    #[cfg(unix)]
    fn spawn_with(args: &[&OsStr], env: &[(&str, &str)]) -> Warm {
        let (daemon, theirs) = Stream::pair().unwrap();
        let fd = theirs.as_raw_fd();
        // What the daemon runs.
        let mut command = Command::new(common::shards_vm());
        command
            .args(args.iter().map(|a| if *a == LINK { OsStr::new("3") } else { *a }))
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        #[cfg(target_os = "macos")]
        let (broker, grants) = Broker::spawn();
        #[cfg(target_os = "macos")]
        let grants = grants.as_raw_fd();
        #[cfg(target_os = "macos")]
        command.args(["--grants", "4"]);
        #[cfg(not(target_os = "macos"))]
        let grants = -1;
        // SAFETY: runs in the child between fork and exec, calling only dup2(2) and
        // fcntl(2), which are async-signal-safe.
        unsafe {
            command.pre_exec(move || {
                // The grants socket is moved past 4 first, so that placing the daemon's at
                // 3 cannot close it.
                let grants = match grants {
                    -1 => -1,
                    g => libc::fcntl(g, libc::F_DUPFD_CLOEXEC, 5),
                };
                for (from, to) in [(fd, 3), (grants, 4)] {
                    if from == -1 && to == 4 {
                        continue;
                    }
                    if from < 0 || libc::dup2(from, to) < 0 || libc::fcntl(to, libc::F_SETFD, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let child = {
            let _turn = spawning();
            command.spawn().unwrap()
        };
        drop(theirs);
        daemon.set_read_timeout(Some(TIMEOUT)).unwrap();
        let vm = Some(child.id());
        Warm {
            child,
            daemon,
            vm,
            #[cfg(target_os = "macos")]
            _broker: broker,
        }
    }

    /// `shards` with `args`, which starts the VM process confined and gives it its end of
    /// the link, a handle `shards` inherits, by name (cli `served_pipes`), and `env`.
    #[cfg(windows)]
    fn spawn_with(args: &[&OsStr], env: &[(&str, &str)]) -> Warm {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
        // Each made once, by what may start processes of its own, before any link is
        // inheritable.
        let _ = (kernel(), guest_init());
        let (daemon, theirs) = Stream::pair().unwrap();
        let link = theirs.share().unwrap();
        drop(theirs);
        let value = (link.as_raw_handle() as usize).to_string();
        let mut command = common::command();
        command
            .args(args.iter().map(|a| if *a == LINK { OsStr::new(&value) } else { *a }))
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let child = {
            let _turn = spawning();
            // SAFETY: sets one flag of a handle this test holds, for the spawn below alone.
            let set = unsafe { SetHandleInformation(link.as_raw_handle(), HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
            assert_ne!(set, 0);
            let spawned = command.spawn();
            // SAFETY: as above, cleared again.
            unsafe { SetHandleInformation(link.as_raw_handle(), HANDLE_FLAG_INHERIT, 0) };
            spawned.unwrap()
        };
        drop(link);
        daemon.set_read_timeout(Some(TIMEOUT)).unwrap();
        Warm {
            child,
            daemon,
            vm: None,
        }
    }

    fn ready(&mut self) {
        match shards_ipc::recv(&self.daemon) {
            Ok(Some(m)) => assert_eq!(m.kind, kind::READY),
            other => {
                let _ = self.child.kill();
                let mut err = String::new();
                self.child
                    .stderr
                    .take()
                    .unwrap()
                    .read_to_string(&mut err)
                    .unwrap();
                panic!("no READY ({other:?}); the warm VM said:\n{err}");
            }
        }
        // The VM process its `shards` started, which the request's handles go to, as the
        // daemon gives its VMs theirs.
        #[cfg(windows)]
        {
            let found = common::vm_processes(self.child.id());
            assert_eq!(found.len(), 1, "the VM processes of shards {}: {found:?}", self.child.id());
            self.daemon.gives_to(common::giver(found[0]));
            self.vm = Some(found[0]);
        }
    }

    /// Hands `argv` to the warm VM, which must not take it: it ends without TAKEN, as a
    /// VM that never started the run does (daemon.rs, `taken`). Returns what it said.
    #[cfg(target_os = "macos")]
    fn refused(mut self, argv: &[&str]) -> String {
        let spec = Spec {
            argv: argv.iter().map(|a| a.as_bytes().to_vec()).collect(),
            env: vec![b"PATH=/bin".to_vec()],
            cwd: b"/".to_vec(),
            hostname: b"warm".to_vec(),
            ..Spec::default()
        };
        let (_conn, theirs) = Stream::pair().unwrap();
        let (stdin_r, _stdin_w) = pipe();
        let (_stdout_r, stdout_w) = pipe();
        let (_stderr_r, stderr_w) = pipe();
        let mut payload = Vec::new();
        // Its first segment, and memory to plug: none (D148).
        shards_ipc::RunHead {
            log_size: 20 << 20,
            log_files: 5,
            ..Default::default()
        }
        .encode_into(&mut payload);
        payload.extend(spec.encode());
        let conn = shards_ipc::handed(&theirs).unwrap();
        shards_ipc::send(
            &self.daemon,
            kind::RUN,
            &payload,
            &[conn, stdin_r.as_fd(), stdout_w.as_fd(), stderr_w.as_fd()],
        )
        .unwrap();
        assert!(
            shards_ipc::recv(&self.daemon).unwrap().is_none(),
            "the run was taken"
        );
        let _ = self.child.kill();
        let _ = self.child.wait();
        let mut said = String::new();
        self.child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut said)
            .unwrap();
        said
    }

    /// Hands `argv` to the warm VM for a new client, as the daemon does.
    fn run(&self, argv: &[&str], interactive: bool) -> Client {
        self.run_with(argv, interactive, Vec::new())
    }

    /// [`Warm::run`], with the spec's `setup` entries (shards_abi::run::Spec).
    fn run_with(&self, argv: &[&str], interactive: bool, setup: Vec<Vec<u8>>) -> Client {
        let spec = Spec {
            argv: argv.iter().map(|a| a.as_bytes().to_vec()).collect(),
            env: vec![b"PATH=/bin".to_vec()],
            cwd: b"/".to_vec(),
            user: Vec::new(),
            hostname: b"warm".to_vec(),
            tty: None,
            resolv: None,
            stdin: interactive,
            builtin: 0,
            hosts: Vec::new(),
            domainname: Vec::new(),
            cgroup: Vec::new(),
            setup,
            terms: Vec::new(),
        };
        let (conn, theirs) = Stream::pair().unwrap();
        let (stdin_r, stdin_w) = pipe();
        let (stdout_r, stdout_w) = pipe();
        let (stderr_r, stderr_w) = pipe();
        let mut payload = Vec::new();
        // The log's retention, which a run without a log has no use for; its first
        // segment, and memory to plug: none (D148).
        shards_ipc::RunHead {
            flags: if interactive {
                shards_ipc::RUN_INTERACTIVE
            } else {
                0
            },
            log_size: 20 << 20,
            log_files: 5,
            ..Default::default()
        }
        .encode_into(&mut payload);
        payload.extend(spec.encode());
        let handed = shards_ipc::handed(&theirs).unwrap();
        shards_ipc::send(
            &self.daemon,
            kind::RUN,
            &payload,
            &[
                handed.as_fd(),
                stdin_r.as_fd(),
                stdout_w.as_fd(),
                stderr_w.as_fd(),
            ],
        )
        .unwrap();
        // Ours stay open until the warm VM has taken them, as the daemon's do.
        let taken = shards_ipc::recv(&self.daemon).unwrap().expect("TAKEN");
        assert_eq!(taken.kind, kind::TAKEN);
        drop(theirs);
        conn.set_read_timeout(Some(TIMEOUT)).unwrap();
        Client {
            conn,
            stdin: Some(stdin_w),
            stdout: BufReader::new(stdout_r),
            stderr: stderr_r,
        }
    }

    /// The warm VM tells the daemon that its command started, if it did, and its
    /// `status`, with what kept it from starting if it did not (`not_run`), then ends,
    /// leaving the daemon's socket.
    fn ends(mut self, status: u8, not_run: Option<&str>) {
        let started = not_run.is_none();
        let deadline = Instant::now() + TIMEOUT;
        let exited = loop {
            if let Some(exited) = self.child.try_wait().unwrap() {
                break exited;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                break self.child.wait().unwrap();
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(exited.success(), "the warm VM exited with {exited}");
        let mut said = Vec::new();
        let mut done = false;
        let mut records = Vec::new();
        while let Some(m) = shards_ipc::recv(&self.daemon).unwrap() {
            // The records of the processes its run started and ended (D123), whole lines of
            // tab-separated fields, before the end.
            if m.kind == kind::PROCESS {
                assert!(!done, "a record of processes after DONE");
                let text = String::from_utf8_lossy(&m.payload).into_owned();
                assert!(
                    text.ends_with('\n') && text.lines().all(|l| l.split('\t').count() >= 5),
                    "a malformed record: {text:?}"
                );
                records.push(text);
                continue;
            }
            // What a restore recorded, where the backend records restores', goes to the
            // daemon before the end, in well-formed parts.
            if m.kind == kind::WORKING_SET {
                assert!(!done, "a working set part after DONE");
                assert!(
                    shards_ipc::working_set_part(&m.payload).is_some(),
                    "a malformed part"
                );
                continue;
            }
            done |= m.kind == kind::DONE;
            said.push((m.kind, m.payload));
        }
        let records = records.concat();
        assert!(
            !started || records.lines().any(|l| l.split('\t').nth(3) == Some("exec")),
            "no exec of its command in {records:?}"
        );
        let mut expected = Vec::new();
        if started {
            expected.push((kind::STARTED, Vec::new()));
        }
        let mut done = vec![status];
        done.extend_from_slice(not_run.unwrap_or_default().as_bytes());
        expected.push((kind::DONE, done));
        assert_eq!(said, expected);
    }
}

impl Client {
    fn exit(&self) -> u8 {
        let m = shards_ipc::recv(&self.conn).unwrap().expect("an exit status");
        assert_eq!((m.kind, m.payload.len()), (kind::EXIT, 1));
        m.payload[0]
    }

    fn stdout(&mut self) -> String {
        let mut out = String::new();
        self.stdout.read_to_string(&mut out).unwrap();
        out
    }

    fn line(&mut self) -> String {
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        line
    }

    fn signal(&self, linux: u32) {
        shards_ipc::send(&self.conn, kind::SIGNAL, &linux.to_be_bytes(), &[]).unwrap();
    }

    fn stdout_ends(&mut self) -> bool {
        ends(&mut self.stdout)
    }
}

/// Whether `stdout` reads end of file within a second: the warm VM holds no copy of it.
/// The client's copies went once the VM took them.
#[cfg(unix)]
fn ends(stdout: &mut BufReader<File>) -> bool {
    let fd = stdout.get_ref().as_raw_fd();
    // SAFETY: fcntl(2) on a descriptor this test owns.
    unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) };
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut sink = Vec::new();
    loop {
        match stdout.read_to_end(&mut sink) {
            Ok(_) => return true,
            Err(_) if Instant::now() >= deadline => return false,
            Err(_) => std::thread::sleep(Duration::from_millis(1)),
        }
    }
}

/// [`ends`] of an anonymous pipe, which Windows reads without blocking by no flag: its end
/// is a peek that finds it broken (ERROR_BROKEN_PIPE, every writer gone), what it holds read
/// meanwhile.
#[cfg(windows)]
fn ends(stdout: &mut BufReader<File>) -> bool {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Foundation::{ERROR_BROKEN_PIPE, GetLastError};
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;
    let handle = stdout.get_ref().as_raw_handle();
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut sink = vec![0u8; 64 << 10];
    loop {
        let mut held = 0u32;
        // SAFETY: a pipe this test owns, peeked for its count of bytes alone.
        let peeked = unsafe {
            PeekNamedPipe(
                handle,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut held,
                std::ptr::null_mut(),
            )
        };
        if peeked == 0 {
            // SAFETY: no arguments.
            return unsafe { GetLastError() } == ERROR_BROKEN_PIPE;
        }
        if held > 0 {
            let n = usize::try_from(held).unwrap().min(sink.len());
            stdout.get_mut().read_exact(&mut sink[..n]).unwrap();
        } else if Instant::now() >= deadline {
            return false;
        } else {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

#[test]
fn warm_vms_serve_one_request_on_the_clients_stdio() {
    if cannot_run_vms() || cannot_pool() {
        return;
    }
    let dir = TempDir::new("warm");
    let template = template(&dir);

    let mut warm = Warm::spawn(&template);
    warm.ready();
    let mut client = warm.run(&["/bin/testguest", "report"], false);
    assert_eq!(client.exit(), 0);
    let out = client.stdout();
    for line in ["uid 0", "cwd /", "hostname warm", "env PATH=/bin"] {
        assert!(out.lines().any(|l| l == line), "{line}\n{out}");
    }
    warm.ends(0, None);

    // Exit statuses are `docker run`'s, and a command that cannot run says why on the
    // client's stderr before its status arrives.
    // A command that cannot start: the client hears it as `docker run` says it, and
    // the daemon what dockerd would record.
    let why = "exec: \"/bin/nonexistent\": stat /bin/nonexistent: no such file or directory";
    for (argv, status, not_run) in [
        (&["/bin/testguest", "exit", "7"][..], 7, None),
        (&["/bin/nonexistent"][..], 127, Some(why)),
    ] {
        let mut warm = Warm::spawn(&template);
        warm.ready();
        let mut client = warm.run(argv, false);
        assert_eq!(client.exit(), status, "{argv:?}");
        if let Some(why) = not_run {
            let mut err = String::new();
            client.stderr.read_to_string(&mut err).unwrap();
            assert_eq!(
                err,
                format!("shards: {why}\n\nRun 'shards run --help' for more information\n")
            );
        }
        warm.ends(status, not_run);
    }

    // With -i, the client's stdin is the command's.
    let mut warm = Warm::spawn(&template);
    warm.ready();
    let mut client = warm.run(&["/bin/testguest", "cat"], true);
    let mut stdin = client.stdin.take().unwrap();
    stdin.write_all(b"through the warm VM\n").unwrap();
    drop(stdin);
    assert_eq!(client.exit(), 0);
    assert_eq!(client.stdout(), "through the warm VM\n");
    warm.ends(0, None);
}

/// What the VM logs as it serves its run goes to its own stderr, its daemon's log, not
/// to its client's, which carries the command's alone, as a container's does (review
/// 8.10).
#[test]
fn a_warm_vms_log_stays_out_of_its_clients_stderr() {
    if cannot_run_vms() || cannot_pool() {
        return;
    }
    let dir = TempDir::new("warm-log");
    let template = template(&dir);
    let mut warm = Warm::spawn_env(&template, &[("SHARDS_LOG", "info")]);
    let mut own = warm.child.stderr.take().unwrap();
    let reading = std::thread::spawn(move || {
        let mut said = String::new();
        own.read_to_string(&mut said).unwrap();
        said
    });
    warm.ready();
    let mut client = warm.run(&["/bin/testguest", "exit", "0"], false);
    assert_eq!(client.exit(), 0);
    let mut err = String::new();
    client.stderr.read_to_string(&mut err).unwrap();
    warm.ends(0, None);
    let own = reading.join().unwrap();
    assert_eq!(err, "", "the VM's log in its client's stderr");
    assert!(own.contains("us INFO] guest marker"), "{own}");
}

/// A warm VM that saves a template takes its run only out of the template's reach (review
/// 8.2): every later run of the image restores what it saved. On Linux it gives the
/// template's directory up once the template is committed (a Landlock layer, checked); on
/// Windows its spawner, which wrote the template on its word, gives the directory up as it
/// commits it (D196); on macOS, whose grants cannot be given up and name paths (PM M102),
/// its template must have been moved from where it was granted, as the daemon moves one
/// into place before it hands over a run. One left there takes no run, which the daemon
/// then gives to another VM, and says why.
#[test]
fn a_template_saver_takes_a_run_only_out_of_the_templates_reach() {
    if cannot_run_vms() || cannot_pool() {
        return;
    }
    let dir = TempDir::new("warm-saver");
    let image = workload_image(&dir);
    // Moved as the daemon moves it: taken, and served.
    let fresh = dir.join("fresh");
    let mut warm = Warm::saving(&image, &fresh);
    warm.ready();
    std::fs::rename(&fresh, dir.join("template")).unwrap();
    let client = warm.run(&["/bin/testguest", "exit", "0"], false);
    assert_eq!(client.exit(), 0);
    warm.ends(0, None);
    // Left where it was granted.
    let left = dir.join("left");
    let mut warm = Warm::saving(&image, &left);
    warm.ready();
    #[cfg(target_os = "macos")]
    {
        let said = warm.refused(&["/bin/testguest", "exit", "0"]);
        assert!(said.contains("still where it was granted"), "{said}");
    }
    #[cfg(not(target_os = "macos"))]
    {
        let client = warm.run(&["/bin/testguest", "exit", "0"], false);
        assert_eq!(client.exit(), 0);
        warm.ends(0, None);
    }
}

#[test]
fn a_warm_vms_client_signals_its_command() {
    if cannot_run_vms() || cannot_pool() {
        return;
    }
    let dir = TempDir::new("warm-signals");
    let template = template(&dir);

    let mut warm = Warm::spawn(&template);
    warm.ready();
    let mut client = warm.run(&["/bin/testguest", "trap", "INT"], false);
    assert_eq!(client.line(), "ready\n");
    client.signal(2);
    assert_eq!(client.exit(), 0);
    assert_eq!(client.stdout(), "got 2\n");
    warm.ends(0, None);

    // A signal the command does not catch ends it, and the status says which: under
    // --init's reaper (`init`), where the command is not PID 1, which would not hear it
    // (D115).
    let mut warm = Warm::spawn(&template);
    warm.ready();
    let mut client = warm.run_with(&["/bin/testguest", "sleep"], false, vec![b"init".to_vec()]);
    assert_eq!(client.line(), "ready\n");
    client.signal(15);
    assert_eq!(client.exit(), 128 + 15);
    warm.ends(128 + 15, None);
}

#[test]
fn warm_needs_a_socket_of_its_own() {
    if cannot_run_vms() || cannot_pool() {
        return;
    }
    let dir = TempDir::new("warm-bad");
    let template = template(&dir);
    for fd in ["0", "2", "9"] {
        let out = output(
            common::command()
                .args(["restore"])
                .arg(&template)
                .args(["--warm", fd])
                .stdin(Stdio::null()),
        );
        assert!(!out.status.success(), "--warm {fd} was accepted");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("--warm"),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// A warm VM lets go of its client's stdio before it sends the exit status, so a pipeline
/// reading the client's output ends with the client; and when the client hangs up, so a
/// command that outlives it writes nowhere.
#[test]
fn a_warm_vm_lets_go_of_its_clients_stdio() {
    if cannot_run_vms() || cannot_pool() {
        return;
    }
    let dir = TempDir::new("warm-letgo");
    let template = template(&dir);

    let mut warm = Warm::spawn(&template);
    warm.ready();
    let mut client = warm.run(&["/bin/testguest", "exit", "0"], false);
    assert_eq!(client.exit(), 0);
    // Frozen before it can tear down: whatever it still held, it would hold now.
    let ended = {
        let _frozen = freeze(warm.vm.unwrap());
        client.stdout_ends()
    };
    assert!(ended, "the warm VM held the client's stdout past its exit status");
    warm.ends(0, None);

    let mut warm = Warm::spawn(&template);
    warm.ready();
    let mut client = warm.run(&["/bin/testguest", "sleep"], false);
    assert_eq!(client.line(), "ready\n");
    let Client { conn, mut stdout, .. } = client;
    drop(conn);
    assert!(
        ends(&mut stdout),
        "the warm VM kept writing to a client that hung up"
    );
    assert!(
        warm.child.try_wait().unwrap().is_none(),
        "the command outlives its client"
    );
    warm.child.kill().unwrap();
    warm.child.wait().unwrap();
}

/// Process `pid` stopped until the value drops: SIGSTOP, then SIGCONT, on Unix; each of its
/// threads suspended on Windows ([`common::Frozen`]).
#[cfg(unix)]
fn freeze(pid: u32) -> impl Drop {
    struct Stopped(libc::pid_t);
    impl Drop for Stopped {
        fn drop(&mut self) {
            // SAFETY: kill(2) of our own child, reaped only after this.
            unsafe { libc::kill(self.0, libc::SIGCONT) };
        }
    }
    let pid = libc::pid_t::try_from(pid).unwrap();
    // SAFETY: kill(2) of our own child, reaped only in `ends`.
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    Stopped(pid)
}

#[cfg(windows)]
fn freeze(pid: u32) -> common::Frozen {
    common::Frozen::new(pid)
}
