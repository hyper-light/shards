fn record(comm: &[u8], field: &[u8]) -> Vec<u8> {
    let mut bytes = b"42 (".to_vec();
    bytes.extend_from_slice(comm);
    bytes.extend_from_slice(b") S");
    for _ in 4..22 {
        bytes.extend_from_slice(b" 0");
    }
    bytes.push(b' ');
    bytes.extend_from_slice(field);
    bytes.push(b'\n');
    bytes
}

#[test]
fn stack_path_covers_u32_decimal_boundaries_under_allocation_refusal() {
    for pid in [0, 1, 9, 10, 99, 100, u32::MAX] {
        let expected = format!("/proc/{pid}/stat");
        let mut buffer = [0; STAT_PATH_BYTES];
        let len = crate::proc_allocation::after(0, || {
            stat_path(pid, &mut buffer).map(|path| path.as_os_str().len())
        })
        .unwrap();
        assert_eq!(&buffer[STAT_PATH_BYTES - len..], expected.as_bytes());
    }
    for pid in [0, u32::MAX] {
        assert_eq!(
            started(pid).unwrap_err(),
            ProcessFailure::Os {
                operation: ProcessOperation::OpenStat,
                code: libc::ENOENT
            }
        );
    }
    assert_eq!(
        crate::proc_allocation::after(0, || Held::open(u32::MAX)).unwrap_err(),
        ProcessFailure::PidOutOfRange {
            operation: ProcessOperation::PidfdOpen
        }
    );
}

#[test]
fn field_boundaries_and_raw_comm_are_independent_of_text_encoding() {
    for field in [b"0".as_slice(), b"123456", b"18446744073709551615"] {
        let bytes = record(b"own \xff) ) \xfe", field);
        let parsed = crate::proc_allocation::after(0, || start_field(&bytes)).unwrap();
        assert_eq!(
            parsed,
            std::str::from_utf8(field).unwrap().parse::<u64>().unwrap()
        );
    }
    for bytes in [
        Vec::new(),
        b"no delimiter".to_vec(),
        b"42 (x) S 0".to_vec(),
        record(b"x", b""),
        record(b"x", b"-1"),
        record(b"x", b"\xff"),
        record(b"x", b"18446744073709551616"),
    ] {
        let error = crate::proc_allocation::after(0, || start_field(&bytes)).unwrap_err();
        assert_eq!(error, ProcessFailure::InvalidStartRecord);
    }
}

#[test]
fn read_bytes_empty_eof_and_file_error() {
    let dir = shards_testdir::TempDir::new("proc-main-read").unwrap();
    let path = dir.join("stat");
    let bytes = record(b"own \xff) ) \xfe", b"123");
    std::fs::write(&path, &bytes).unwrap();
    assert_eq!(read_stat(std::fs::File::open(&path).unwrap()).unwrap(), bytes);
    std::fs::write(&path, b"").unwrap();
    assert!(read_stat(std::fs::File::open(&path).unwrap()).unwrap().is_empty());
    assert_eq!(
        read_stat(std::fs::File::open(&dir).unwrap()).unwrap_err(),
        ProcessFailure::Os {
            operation: ProcessOperation::ReadStat,
            code: libc::EISDIR
        }
    );
}

#[test]
fn actual_reader_and_started_return_initial_and_later_oom() {
    for successful in [0, 1] {
        let file = std::fs::File::open("/proc/self/stat").unwrap();
        assert_eq!(
            crate::proc_allocation::after(successful, || read_stat(file)).unwrap_err(),
            ProcessFailure::CapacityUnavailable
        );
        assert_eq!(
            crate::proc_allocation::after(successful, || started(std::process::id())).unwrap_err(),
            ProcessFailure::CapacityUnavailable
        );
        println!(
            "actual_reader_and_started_successful_allocations_before_refusal={successful} returned_CapacityUnavailable=true"
        );
    }
}

#[test]
fn held_find_returns_error_when_start_allocation_is_refused() {
    let pid = std::process::id();
    let start = started(pid).unwrap();
    println!("own_tgid={pid} precomputed_start={start} successful_allocations_before_refusal=0");
    let before = fd_count();
    let result = crate::proc_allocation::after(0, || crate::taken::Held::find(pid, start));
    let after = fd_count();
    println!(
        "held_find_returned_error={:?} fds_before={before} fds_after={after}",
        result.as_ref().err()
    );
    assert_eq!(result.unwrap_err(), ProcessFailure::CapacityUnavailable);
    assert_eq!(after, before);
}

fn fd_count() -> usize {
    std::fs::read_dir("/proc/self/fd").unwrap().count()
}

fn process_spec() -> crate::GuestSpec {
    crate::GuestSpec {
        kernel: "unused".into(),
        init: crate::Init::Initrd("unused".into()),
        cmdline: String::new(),
        vcpus: 1,
        memory_mib: 256,
        disks: vec![],
        shares: vec![],
        ports: vec![1],
        binary: None,
    }
}

#[test]
fn public_attach_returns_scoped_capacity_failure_before_success_setup() {
    use crate::Guests as _;
    let pid = std::process::id();
    let start = started(pid).unwrap();
    let identity = crate::Identity {
        pid,
        started: start,
        vsock: "unused".into(),
    };
    let spec = process_spec();
    let before = fd_count();
    let result = crate::proc_allocation::after(0, || crate::Shards.attach(&identity, &spec));
    let after = fd_count();
    println!(
        "public_attach_refusal={:?} fds_before={before} fds_after={after}",
        result.as_ref().err()
    );
    assert!(matches!(result, Err(crate::Refusal::Process {
        pid: p, expected_started, failure: ProcessFailure::CapacityUnavailable,
    }) if p == pid && expected_started == start));
    assert_eq!(after, before);
}

#[test]
#[ignore = "private owned child awaiting stdin EOF"]
fn held_identity_child() {
    assert_eq!(std::io::stdin().read(&mut [0]).unwrap(), 0);
}

#[test]
fn wrong_start_and_unreaped_exit_keep_exact_identity_and_return_fds() {
    use crate::Guests as _;
    let initial = fd_count();
    let mut owned = OwnedChild {
        child: Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "taken::linux::tests::held_identity_child", "--ignored"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
        reader: None,
    };
    let pid = owned.child.id();
    let held = Held::open(pid).unwrap();
    let start = started(pid).unwrap();
    assert!(!held.exited(false).unwrap());
    let different = start.checked_add(1).unwrap();
    let before = fd_count();
    let found = crate::taken::Held::find(pid, start).unwrap();
    drop(found);
    let wrong = crate::taken::Held::find(pid, different);
    let spec = process_spec();
    let wrong_identity = crate::Identity {
        pid,
        started: different,
        vsock: "unused".into(),
    };
    let public_wrong = crate::Shards.attach(&wrong_identity, &spec);
    let after_wrong = fd_count();
    assert!(!held.exited(false).unwrap());
    drop(owned.child.stdin.take());
    assert!(held.exited(true).unwrap());
    // Keep the Child unreaped: this PID still identifies our exited child, so
    // Exited is observed through the held event, not guessed from an errno.
    let ended_before = fd_count();
    let ended = crate::taken::Held::find(pid, start);
    let identity = crate::Identity {
        pid,
        started: start,
        vsock: "unused".into(),
    };
    let public_ended = crate::Shards.attach(&identity, &spec);
    let ended_after = fd_count();
    let status = owned.child.wait().unwrap();
    drop(held);
    drop(owned);
    let final_fds = fd_count();
    println!(
        "own_child={pid} start={start} wrong={:?} ended={:?} child_exit={:?} fds_initial={initial} wrong_before={before} wrong_after={after_wrong} ended_before={ended_before} ended_after={ended_after} fds_final={final_fds}",
        wrong.as_ref().err(),
        ended.as_ref().err(),
        status.code()
    );
    assert_eq!(
        wrong.unwrap_err(),
        ProcessFailure::DifferentStart { observed: start }
    );
    assert!(
        matches!(public_wrong, Err(crate::Refusal::Process { pid: p, expected_started,
        failure: ProcessFailure::DifferentStart { observed } })
        if p == pid && expected_started == different && observed == start)
    );
    assert_eq!(ended.unwrap_err(), ProcessFailure::Exited);
    assert!(
        matches!(public_ended, Err(crate::Refusal::Process { pid: p, expected_started,
        failure: ProcessFailure::Exited }) if p == pid && expected_started == start)
    );
    assert!(status.success());
    assert_eq!(before, after_wrong);
    assert_eq!(ended_before, ended_after);
    assert_eq!(initial, final_fds);
}

#[test]
fn typed_display_clone_and_unknown_observation_survive_refusal() {
    const EXPECTED: &str =
        "process 4294967295, expected start 18446744073709551615: ReadStat: OS error -2147483648";
    let refusal = crate::Refusal::Process {
        pid: u32::MAX,
        expected_started: u64::MAX,
        failure: ProcessFailure::Os {
            operation: ProcessOperation::ReadStat,
            code: i32::MIN,
        },
    };
    let mut storage = [0; EXPECTED.len()];
    let mut output = io::Cursor::new(storage.as_mut_slice());
    let result = crate::proc_allocation::after(0, || {
        let cloned = refusal.clone();
        let equal = cloned == refusal;
        let written = write!(output, "{cloned}");
        let unknown = io_failure(ProcessOperation::ReadStat, io::ErrorKind::InvalidInput.into());
        (equal, written, unknown)
    });
    assert!(result.0);
    result.1.unwrap();
    assert_eq!(storage, EXPECTED.as_bytes());
    assert_eq!(
        result.2,
        ProcessFailure::UnrepresentedIo {
            operation: ProcessOperation::ReadStat,
            kind: io::ErrorKind::InvalidInput,
        }
    );
}
