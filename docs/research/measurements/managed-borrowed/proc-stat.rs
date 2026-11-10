use super::*;

fn record(comm: &[u8], field: &[u8]) -> Vec<u8> {
    let mut stat = b"42 (".to_vec();
    stat.extend_from_slice(comm);
    stat.extend_from_slice(b") S");
    // State is field3, followed by fields4 through21 before starttime at22.
    for _ in 4..22 {
        stat.extend_from_slice(b" 0");
    }
    stat.push(b' ');
    stat.extend_from_slice(field);
    stat.push(b'\n');
    stat
}

#[test]
fn start_field_ignores_comm_encoding_and_embedded_parens() {
    for comm in [b"ordinary".as_slice(), b"own \xff) ) \xfe"] {
        for field in [b"0".as_slice(), b"123456", b"18446744073709551615"] {
            let stat = record(comm, field);
            assert_eq!(
                start_field(&stat).unwrap(),
                std::str::from_utf8(field).unwrap().parse::<u64>().unwrap()
            );
        }
    }
}

#[test]
fn missing_or_invalid_start_field_is_nonallocating_invalid_data() {
    for stat in [
        Vec::new(),
        b"42 missing delimiter".to_vec(),
        b"42 (own) S 0".to_vec(),
        record(b"own", b""),
        record(b"own", b"not-decimal"),
        record(b"own", b"-1"),
        record(b"own", b"\xff"),
        record(b"own", b"18446744073709551616"),
    ] {
        let result = crate::allocation::fail_after(0, || start_field(&stat));
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }
}

#[test]
fn proc_reader_and_start_path_return_allocation_failure() {
    for successful in [0, 1] {
        let file = std::fs::File::open("/proc/self/stat").unwrap();
        let read = crate::allocation::fail_after(successful, || read_stat(file));
        assert_eq!(read.unwrap_err().kind(), io::ErrorKind::OutOfMemory);
        println!(
            "actual_proc_reader_successful_allocations_before_refusal={successful} returned_OutOfMemory=true"
        );
    }
    let started = crate::allocation::fail_after(0, || started(std::process::id()));
    assert_eq!(started.unwrap_err().kind(), io::ErrorKind::OutOfMemory);
    let stat = record(b"own \xff)", b"123456");
    assert_eq!(
        crate::allocation::fail_after(0, || start_field(&stat)).unwrap(),
        123456
    );
    println!(
        "actual_proc_reader_oom_returned=true start_path_oom_returned=true field_parse_under_refused_allocations=true"
    );
}

#[test]
fn proc_reader_preserves_bytes_and_empty_eof_and_io_error() {
    let dir = shards_testdir::TempDir::new("proc-read").unwrap();
    let path = dir.join("stat");
    let content = record(b"own \xff) ) \xfe", b"123456");
    std::fs::write(&path, &content).unwrap();
    assert_eq!(read_stat(std::fs::File::open(&path).unwrap()).unwrap(), content);
    std::fs::write(&path, b"").unwrap();
    assert!(read_stat(std::fs::File::open(&path).unwrap()).unwrap().is_empty());
    assert_eq!(
        read_stat(std::fs::File::open(&dir).unwrap()).unwrap_err().kind(),
        io::ErrorKind::IsADirectory
    );
}

#[test]
#[ignore = "historical direct-std probe: actual32-byte abort retained at eee20469; not the production reader"]
fn std_proc_read_oom_repro() {
    let read = crate::allocation::fail_after(0, || std::fs::read("/proc/self/stat"));
    assert_eq!(read.unwrap_err().kind(), io::ErrorKind::OutOfMemory);
}
