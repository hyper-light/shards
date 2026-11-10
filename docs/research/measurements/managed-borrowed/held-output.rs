//! A causal output-end regression, using the production watcher and drainer.
use super::*;
use std::io::BufRead as _;
use std::os::fd::AsRawFd as _;
use std::sync::mpsc;
use std::thread::JoinHandle;

fn pipe_identity(fd: libc::c_int) -> (libc::dev_t, libc::ino_t) {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstat initializes this correctly sized output on success; fd is held.
    assert_eq!(unsafe { libc::fstat(fd, stat.as_mut_ptr()) }, 0);
    // SAFETY: the successful fstat above initialized stat.
    let stat = unsafe { stat.assume_init() };
    assert_eq!(stat.st_mode & libc::S_IFMT, libc::S_IFIFO);
    (stat.st_dev, stat.st_ino)
}

/// Both processes are direct children. Close controls, kill only through their owned
/// Child objects, reap, then join every reader/caller/watcher even if an assertion fails.
#[derive(Default)]
struct Fixture {
    vm: Option<Arc<Mutex<Child>>>,
    writer: Option<Child>,
    workers: Vec<JoinHandle<()>>,
}

impl Fixture {
    fn join(&mut self) {
        let mut joined = true;
        for worker in self.workers.drain(..) {
            joined &= worker.join().is_ok();
        }
        assert!(joined);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(writer) = &mut self.writer {
            drop(writer.stdin.take());
            let _ = writer.kill();
            let _ = writer.wait();
        }
        if let Some(vm) = &self.vm {
            let mut vm = lock(vm);
            drop(vm.stdin.take());
            let _ = vm.kill();
        }
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
        if let Some(vm) = &self.vm {
            let _ = lock(vm).wait();
        }
    }
}

fn role(name: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", name, "--ignored", "--nocapture"]);
    command.stdin(Stdio::piped());
    command
}

#[test]
#[ignore = "private child role, launched only by held-output regression"]
fn writer_role() {
    let (dev, ino) = pipe_identity(std::io::stdout().as_raw_fd());
    eprintln!("HELD_WRITER {} {dev} {ino}", std::process::id());
    let mut release = [0];
    let _ = std::io::stdin().read(&mut release).unwrap();
}

#[test]
#[ignore = "private child role, launched only by held-output regression"]
fn vm_role() {
    let mut release = [0];
    if std::io::stdin().read(&mut release).unwrap() != 0 {
        eprintln!("no kernel at kernel");
        std::process::exit(7);
    }
}

/// Refusal must arrive before a live, identified writer is released, independently of
/// scheduling latency. The existing outer budget only bounds a failed event wait.
#[test]
fn a_child_holding_shards_output_holds_no_refusal_back() {
    let deadline = Instant::now()
        .checked_add(crate::conformance::PATIENCE.get())
        .unwrap();
    let mut fixture = Fixture::default();
    let dir = shards_testdir::TempDir::new("api-held").unwrap();
    let (output, writer) = io::pipe().unwrap();
    // Compare to the original WRITER endpoint, not the reader: XNU's pipe_stat
    // hashes each endpoint separately (xnu-12377.101.15 bsd/kern/sys_pipe.c).
    let (dev, ino) = pipe_identity(writer.as_raw_fd());
    fixture.writer = Some(
        role("client::held_output::writer_role")
            .stdout(writer.try_clone().unwrap())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let holder = fixture.writer.as_mut().unwrap();
    let pid = holder.id();
    let started = crate::taken::started(pid).unwrap();
    let held = crate::taken::Held::find_checked(pid, started).unwrap();
    let ready = holder.stderr.take().unwrap();
    let (send, receive) = mpsc::channel();
    fixture.workers.push(std::thread::spawn(move || {
        let mut ready = io::BufReader::new(ready);
        let mut line = String::new();
        let result = ready.read_line(&mut line).map(|_| line);
        let _ = send.send(result);
    }));
    let ready = receive
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .unwrap()
        .unwrap();
    assert_eq!(ready.trim(), format!("HELD_WRITER {pid} {dev} {ino}"));
    held.check().unwrap();

    let vm = role("client::held_output::vm_role")
        .stdout(writer.try_clone().unwrap())
        .stderr(writer.try_clone().unwrap())
        .spawn()
        .unwrap();
    let pid = vm.id();
    let vm = Arc::new(Mutex::new(vm));
    fixture.vm = Some(vm.clone());
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            exited: false,
            stopping: false,
            ready: false,
            end: None,
            said: String::new(),
            ended_as: String::new(),
            status: None,
        }),
        changed: Condvar::new(),
        tail: Mutex::new(Tail {
            bytes: VecDeque::new(),
            keep: tail(0),
            drained: false,
            output: None,
        }),
        drained: Condvar::new(),
        end_mark: end_mark(),
    });
    let guest = ShardsGuest {
        ports: vec![1234], // A legal vsock port, the existing testguest echo identifier.
        vsock: dir.join("v"),
        pid,
        taken: None,
        shared: shared.clone(),
        _dir: None,
    };
    let exit = Exit::of(&vm).unwrap();
    let drainer = shared.clone();
    fixture
        .workers
        .push(std::thread::spawn(move || drain(&drainer, output)));
    let watcher = shared.clone();
    fixture
        .workers
        .push(std::thread::spawn(move || exit.watch(&watcher, writer)));
    let (send, receive) = mpsc::channel();
    fixture.workers.push(std::thread::spawn(move || {
        let _ = send.send(guest.connect(1234, Duration::MAX).err());
    }));
    lock(&vm).stdin.take().unwrap().write_all(b"x").unwrap();
    let refusal = receive.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    // Keep both the held process and its control pipe until after the refusal observation.
    let held_after = held.check();
    let unreaped = fixture.writer.as_mut().unwrap().try_wait().unwrap().is_none();
    println!(
        "refusal_before_writer_release={} writer_held_alive={} writer_unreaped={unreaped}",
        refusal.is_ok(),
        held_after.is_ok()
    );
    fixture
        .writer
        .as_mut()
        .unwrap()
        .stdin
        .take()
        .unwrap()
        .write_all(b"x")
        .unwrap();
    let writer_status = fixture.writer.as_mut().unwrap().wait().unwrap();
    fixture.join();
    let vm_code = lock(&shared.state)
        .status
        .as_ref()
        .unwrap()
        .as_ref()
        .unwrap()
        .code();
    println!(
        "writer_explicitly_released=true writer_exit={:?} vm_reaped_exit={vm_code:?} all_workers_joined=true",
        writer_status.code()
    );
    assert!(writer_status.success());
    assert_eq!(vm_code, Some(7));
    assert!(lock(&shared.tail).drained);
    held_after.unwrap();
    assert!(unreaped);
    match refusal.unwrap() {
        Some(Refusal::Boot(why)) => assert!(why.contains("no kernel at kernel"), "{why}"),
        other => panic!("{other:?}"),
    }
}
