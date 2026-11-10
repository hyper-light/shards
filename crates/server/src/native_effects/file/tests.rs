use super::*;
use std::os::fd::AsFd as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;

fn reserved(limit: usize) -> Vec<u8> {
    Vec::with_capacity(limit + 1)
}

#[test]
fn held_file_survives_path_replacement_and_output_is_literal() {
    let tmp = shards_testdir::TempDir::new("native-file-held").unwrap();
    let root = std::fs::File::open(&tmp).unwrap();
    let grant = ReadRoot::scratch(root.as_fd()).unwrap();
    let original = b"\0\xff\n{\"id\":\"other\"}\x1b]8;;data\x07";
    std::fs::write(tmp.join("file"), original).unwrap();
    let file = grant.open(c"file").unwrap();
    let metadata = std::fs::metadata(tmp.join("file")).unwrap();
    assert_eq!(file.identity(), (metadata.dev(), metadata.ino()));
    assert_eq!(file.check_limit(original.len()), Ok(()));
    assert_eq!(file.check_limit(0), Err(ReadFailure::Length));
    std::fs::rename(tmp.join("file"), tmp.join("old")).unwrap();
    std::fs::write(tmp.join("file"), b"replacement").unwrap();
    let mut output = reserved(original.len());
    file.append(&mut output, original.len()).unwrap();
    assert_eq!(output, original);
    assert_eq!(std::fs::read(tmp.join("file")).unwrap(), b"replacement");
    assert_eq!(std::fs::read(tmp.join("old")).unwrap(), original);
}

#[test]
fn forbidden_paths_never_return_outside_bytes_or_change_the_destination() {
    let tmp = shards_testdir::TempDir::new("native-file-forbidden").unwrap();
    std::fs::create_dir(tmp.join("root")).unwrap();
    std::fs::write(tmp.join("outside"), b"forbidden destination").unwrap();
    std::os::unix::fs::symlink("../outside", tmp.join("root/link")).unwrap();
    let root = std::fs::File::open(tmp.join("root")).unwrap();
    let grant = ReadRoot::scratch(root.as_fd()).unwrap();
    let absolute = std::ffi::CString::new(tmp.join("outside").as_os_str().as_bytes()).unwrap();
    for path in [c"../outside", c"link", absolute.as_c_str()] {
        assert!(grant.open(path).is_err(), "unexpectedly opened {path:?}");
    }
    assert_eq!(
        std::fs::read(tmp.join("outside")).unwrap(),
        b"forbidden destination"
    );
}

#[test]
fn nonregular_objects_and_nondirectory_roots_are_refused() {
    let tmp = shards_testdir::TempDir::new("native-file-kind").unwrap();
    std::fs::create_dir(tmp.join("dir")).unwrap();
    std::fs::write(tmp.join("file"), b"x").unwrap();
    let fifo = std::ffi::CString::new(tmp.join("fifo").as_os_str().as_bytes()).unwrap();
    // SAFETY: mkfifo creates this fixture's FIFO in its owned disposable directory.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let root = std::fs::File::open(&tmp).unwrap();
    let grant = ReadRoot::scratch(root.as_fd()).unwrap();
    for path in [c"dir", c"fifo"] {
        assert!(matches!(grant.open(path), Err(ReadFailure::FileKind)));
    }
    let regular = std::fs::File::open(tmp.join("file")).unwrap();
    assert!(matches!(
        ReadRoot::scratch(regular.as_fd()),
        Err(ReadFailure::RootKind)
    ));
    assert!(matches!(grant.open(c""), Err(ReadFailure::Path)));
}

#[test]
fn growth_past_the_reservation_returns_no_partial_payload() {
    let tmp = shards_testdir::TempDir::new("native-file-growth").unwrap();
    let root = std::fs::File::open(&tmp).unwrap();
    let grant = ReadRoot::scratch(root.as_fd()).unwrap();
    std::fs::write(tmp.join("file"), b"old").unwrap();
    let file = grant.open(c"file").unwrap();
    std::fs::write(tmp.join("file"), b"grew").unwrap();
    let mut output = Vec::with_capacity(1 + 3 + 1);
    output.push(b'P');
    assert_eq!(file.append(&mut output, 3), Err(ReadFailure::Length));
    assert_eq!(output, b"P");
    assert_eq!(std::fs::read(tmp.join("file")).unwrap(), b"grew");
}

#[test]
fn caller_capacity_and_exact_empty_limit_are_checked_before_read() {
    let tmp = shards_testdir::TempDir::new("native-file-capacity").unwrap();
    let root = std::fs::File::open(&tmp).unwrap();
    let grant = ReadRoot::scratch(root.as_fd()).unwrap();
    std::fs::write(tmp.join("file"), b"x").unwrap();
    let file = grant.open(c"file").unwrap();
    let mut unavailable = Vec::new();
    assert_eq!(file.append(&mut unavailable, 1), Err(ReadFailure::Capacity));
    let mut exact = reserved(1);
    file.append(&mut exact, 1).unwrap();
    assert_eq!(exact, b"x");
    std::fs::write(tmp.join("empty"), b"").unwrap();
    let empty = grant.open(c"empty").unwrap();
    let mut output = reserved(0);
    empty.append(&mut output, 0).unwrap();
    assert!(output.is_empty());
    let file = grant.open(c"file").unwrap();
    assert_eq!(file.append(&mut output, 0), Err(ReadFailure::Length));
    assert!(output.is_empty());
    assert_eq!(file.append(&mut output, usize::MAX), Err(ReadFailure::Length));
}
