use super::*;
use std::io::{BufRead as _, Read as _, Write as _};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Instant;

const NAME: &[u8] = b"own \xff) ) \xfe"; // Fits Linux TASK_COMM_LEN, includes raw name bytes.

struct OwnedChild {
    child: Child,
    reader: Option<JoinHandle<()>>,
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[test]
#[ignore = "private own-child role for raw process-name regression"]
fn raw_comm_child() {
    struct Name(Vec<u8>);
    impl Drop for Name {
        fn drop(&mut self) {
            let _ = std::fs::write("/proc/self/comm", &self.0);
        }
    }
    let name = std::fs::read("/proc/self/comm").unwrap();
    let name = Name(name.strip_suffix(b"\n").unwrap().to_vec());
    let mut input = std::io::stdin().lock();
    let mut command = [0];
    input.read_exact(&mut command).unwrap();
    assert_eq!(&command, b"n");
    // This is the child's TGID leader even from libtest's worker; PR_SET_NAME would
    // instead rename only that worker, missing /proc/PID/stat's process comm.
    std::fs::write("/proc/self/comm", NAME).unwrap();
    eprintln!("RAW_READY");
    if input.read(&mut command).unwrap() != 0 {
        assert_eq!(&command, b"r");
        std::fs::write("/proc/self/comm", &name.0).unwrap();
        assert_eq!(
            std::fs::read("/proc/self/comm")
                .unwrap()
                .strip_suffix(b"\n")
                .unwrap(),
            name.0
        );
        eprintln!("NAME_RESTORED");
        assert_eq!(input.read(&mut command).unwrap(), 0);
    }
}

#[test]
fn legal_raw_comm_preserves_held_process_start() {
    let deadline = Instant::now()
        .checked_add(crate::conformance::PATIENCE.get())
        .unwrap();
    let mut owned = OwnedChild {
        child: Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "taken::linux::tests::raw_comm_child",
                "--ignored",
                "--nocapture",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
        reader: None,
    };
    let pid = owned.child.id();
    let held = Held::open(pid).unwrap();
    let original = started(pid).unwrap();
    assert!(!held.exited(false).unwrap());
    let events = owned.child.stderr.take().unwrap();
    let (send, receive) = mpsc::channel();
    owned.reader = Some(std::thread::spawn(move || {
        for line in io::BufReader::new(events).lines() {
            if send.send(line.unwrap()).is_err() {
                break;
            }
        }
    }));
    owned.child.stdin.as_mut().unwrap().write_all(b"n").unwrap();
    assert_eq!(
        receive
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap(),
        "RAW_READY"
    );
    let raw = std::fs::read(format!("/proc/{pid}/stat")).unwrap();
    assert!(raw.windows(NAME.len()).any(|window| window == NAME));
    assert!(std::str::from_utf8(&raw).is_err());
    assert!(!held.exited(false).unwrap());
    let observed = started(pid);
    println!("own_tgid_raw_comm={NAME:02x?} held_alive=true observed_start={observed:?}");
    owned.child.stdin.as_mut().unwrap().write_all(b"r").unwrap();
    assert_eq!(
        receive
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap(),
        "NAME_RESTORED"
    );
    assert_eq!(started(pid).unwrap(), original);
    assert!(!held.exited(false).unwrap());
    drop(owned.child.stdin.take());
    let status = owned.child.wait().unwrap();
    owned.reader.take().unwrap().join().unwrap();
    println!(
        "own_name_restored=true original_start_unchanged=true child_exit={:?} reader_joined=true",
        status.code()
    );
    assert!(status.success());
    assert_eq!(observed.unwrap(), original);
}
