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
fn proc_read_and_start_path_return_allocation_failure() {
    let read = crate::allocation::fail_after(0, || std::fs::read("/proc/self/stat"));
    assert_eq!(read.unwrap_err().kind(), io::ErrorKind::OutOfMemory);
    let started = crate::allocation::fail_after(0, || started(std::process::id()));
    assert_eq!(started.unwrap_err().kind(), io::ErrorKind::OutOfMemory);
    let stat = record(b"own \xff)", b"123456");
    assert_eq!(
        crate::allocation::fail_after(0, || start_field(&stat)).unwrap(),
        123456
    );
    println!(
        "proc_read_oom_returned=true start_path_oom_returned=true field_parse_under_refused_allocations=true"
    );
}
