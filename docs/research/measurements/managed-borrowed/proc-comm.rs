//! Linux comm bytes must not change an otherwise-held borrowed process identity.
use super::*;
use std::io::{BufRead as _, Read as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::net::UnixListener;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread::JoinHandle;

// Less than TASK_COMM_LEN, with bytes legal in comm but invalid as UTF-8.
const NAME: &[u8] = b"own \xff) ) \xfe";

struct Process {
    child: Child,
    workers: Vec<JoinHandle<()>>,
}
impl Drop for Process {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

#[test]
#[ignore = "private own-child role for proc comm regression"]
fn process_role() {
    struct Name(Vec<u8>);
    impl Drop for Name {
        fn drop(&mut self) {
            let _ = std::fs::write("/proc/self/comm", &self.0);
        }
    }
    let original = std::fs::read("/proc/self/comm").unwrap();
    let original = Name(original.strip_suffix(b"\n").unwrap().to_vec());
    let mut input = std::io::stdin().lock();
    let mut byte = [0];
    input.read_exact(&mut byte).unwrap();
    assert_eq!(byte, *b"n");
    // /proc/self/comm is the process leader, even from libtest's worker thread.
    // PR_SET_NAME here would rename only the worker, not /proc/PID/stat's leader.
    std::fs::write("/proc/self/comm", NAME).unwrap();
    eprintln!("COMM_CHANGED");
    if input.read(&mut byte).unwrap() != 0 {
        assert_eq!(byte, *b"r");
        std::fs::write("/proc/self/comm", &original.0).unwrap();
        let restored = std::fs::read("/proc/self/comm").unwrap();
        assert_eq!(restored.strip_suffix(b"\n").unwrap(), original.0);
        eprintln!("COMM_RESTORED");
        assert_eq!(input.read(&mut byte).unwrap(), 0);
    }
}

fn wait(fds: &mut [libc::pollfd], deadline: Instant) {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        // poll's units are milliseconds; round the remaining duration upward.
        let millis = left.as_nanos().div_ceil(Duration::from_millis(1).as_nanos());
        let timeout = libc::c_int::try_from(millis).unwrap();
        // SAFETY: the slice is live for this wait, and all descriptors remain owned.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len().try_into().unwrap(), timeout) };
        if ready >= 0 {
            assert!(ready > 0, "outer fixture deadline expired");
            return;
        }
        assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::Interrupted);
    }
}

#[test]
fn non_utf8_comm_keeps_held_identity_and_borrowed_connect() {
    let deadline = Instant::now()
        .checked_add(crate::conformance::PATIENCE.get())
        .unwrap();
    let mut process = Process {
        child: Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "borrowed::proc_comm::process_role",
                "--ignored",
                "--nocapture",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
        workers: Vec::new(),
    };
    let pid = process.child.id();
    let started = crate::taken::started(pid).unwrap();
    let held = crate::taken::Held::find_checked(pid, started).unwrap();
    let events = process.child.stderr.take().unwrap();
    let (send, receive) = mpsc::channel();
    process.workers.push(std::thread::spawn(move || {
        for line in io::BufReader::new(events).lines() {
            if send.send(line.unwrap()).is_err() {
                break;
            }
        }
    }));
    process.child.stdin.as_mut().unwrap().write_all(b"n").unwrap();
    assert_eq!(
        receive
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap(),
        "COMM_CHANGED"
    );
    held.check().unwrap();
    let stat = std::fs::read(format!("/proc/{pid}/stat")).unwrap();
    assert!(stat.windows(NAME.len()).any(|window| window == NAME));
    assert!(std::str::from_utf8(&stat).is_err());
    println!("own_child_stat_invalid_utf8=true comm_hex={NAME:02x?} held_alive_before=true");

    let dir = shards_testdir::TempDir::new("proc-comm").unwrap();
    let path = dir.join("v");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let identity = Identity {
        pid,
        started,
        vsock: path,
    };
    let (wake, mut done) = io::pipe().unwrap();
    let (send, result) = mpsc::channel();
    process.workers.push(std::thread::spawn(move || {
        let answer = connect_borrowed(&identity, 1234, crate::conformance::UNSERVED_PATIENCE);
        let _ = send.send(answer);
        let _ = done.write_all(b"x");
    }));
    // Wait for either the real connection or its pre-I/O refusal; no retry/probe loop.
    let mut fds = [
        libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    wait(&mut fds, deadline);
    let mut actual_connect = false;
    if fds[0].revents & libc::POLLIN != 0 {
        let (mut stream, _) = listener.accept().unwrap();
        let left = deadline.saturating_duration_since(Instant::now());
        stream.set_read_timeout(Some(left)).unwrap();
        stream.set_write_timeout(Some(left)).unwrap();
        let mut request = [0; b"CONNECT 1234\n".len()];
        stream.read_exact(&mut request).unwrap();
        assert_eq!(&request, b"CONNECT 1234\n");
        actual_connect = true;
        stream.write_all(b"OK 1\nk").unwrap();
    }
    let mut answer = result
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .unwrap();
    println!(
        "actual_CONNECT={actual_connect} borrowed_error={:?}",
        answer.as_ref().err()
    );
    if let Ok(stream) = &mut answer {
        let mut payload = [0];
        stream.read_exact(&mut payload).unwrap();
        assert_eq!(&payload, b"k");
    }
    held.check().unwrap();
    process.child.stdin.as_mut().unwrap().write_all(b"r").unwrap();
    assert_eq!(
        receive
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap(),
        "COMM_RESTORED"
    );
    let restored = crate::taken::started(pid).unwrap();
    assert_eq!(restored, started);
    held.check().unwrap();
    drop(process.child.stdin.take());
    let status = process.child.wait().unwrap();
    let mut joined = true;
    for worker in process.workers.drain(..) {
        joined &= worker.join().is_ok();
    }
    println!(
        "own_comm_restored=true start_unchanged=true child_exit={:?} workers_joined={joined}",
        status.code()
    );
    assert!(status.success());
    assert!(joined);
    assert!(actual_connect);
    assert!(answer.is_ok(), "{:?}", answer.err());
}
