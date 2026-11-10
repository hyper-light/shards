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
    assert_eq!(started(0).unwrap_err().kind(), io::ErrorKind::NotFound);
    assert_eq!(started(u32::MAX).unwrap_err().kind(), io::ErrorKind::NotFound);
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
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
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
        read_stat(std::fs::File::open(&dir).unwrap()).unwrap_err().kind(),
        io::ErrorKind::IsADirectory
    );
}

#[test]
fn actual_reader_and_started_return_initial_and_later_oom() {
    for successful in [0, 1] {
        let file = std::fs::File::open("/proc/self/stat").unwrap();
        assert_eq!(
            crate::proc_allocation::after(successful, || read_stat(file))
                .unwrap_err()
                .kind(),
            io::ErrorKind::OutOfMemory
        );
        assert_eq!(
            crate::proc_allocation::after(successful, || started(std::process::id()))
                .unwrap_err()
                .kind(),
            io::ErrorKind::OutOfMemory
        );
        println!(
            "actual_reader_and_started_successful_allocations_before_refusal={successful} returned_OutOfMemory=true"
        );
    }
}

#[test]
fn held_find_returns_error_when_start_allocation_is_refused() {
    let pid = std::process::id();
    let start = started(pid).unwrap();
    println!("own_tgid={pid} precomputed_start={start} successful_allocations_before_refusal=0");
    let result = crate::proc_allocation::after(0, || crate::taken::Held::find(pid, start));
    println!("held_find_returned_error={}", result.is_err());
    assert!(result.is_err());
}
