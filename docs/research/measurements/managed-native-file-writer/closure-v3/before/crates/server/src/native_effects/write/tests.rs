use super::*;
use std::fs::{File, OpenOptions};
use std::os::fd::AsFd as _;
use std::os::unix::fs::MetadataExt as _;

fn object(path: &std::path::Path) -> OwnedFd {
    OpenOptions::new().write(true).open(path).unwrap().into()
}

fn terminal(step: Step<'_>) -> Outcome {
    match step {
        Step::Terminal(outcome) => outcome,
        Step::Partial(pending) => panic!(
            "ordinary fixture unexpectedly short: {}/{}",
            pending.accepted(),
            pending.requested()
        ),
    }
}

#[test]
fn literal_positional_write_preserves_prefix_suffix_and_shared_offset() {
    let tmp = shards_testdir::TempDir::new("native-write-literal").unwrap();
    let path = tmp.join("file");
    std::fs::write(&path, b"head------------tail").unwrap();
    let file = object(&path);
    let duplicate = file.try_clone().unwrap();
    // SAFETY: this fixture observes/sets its owned file's shared position only.
    assert_eq!(
        unsafe { libc::lseek(duplicate.as_raw_fd(), 2, libc::SEEK_SET) },
        2
    );
    let bytes = b"\0\xff\n\x1b[31m";
    let prepared = Prepared::existing(file, Request::new(4, bytes).unwrap()).unwrap();
    let metadata = std::fs::metadata(&path).unwrap();
    assert_eq!(prepared.identity(), (metadata.dev(), metadata.ino()));
    assert_eq!(prepared.accepted(), 0);
    assert_eq!(prepared.requested(), bytes.len());
    assert_eq!(std::fs::read(&path).unwrap(), b"head------------tail");
    let outcome = terminal(prepared.commit());
    assert_eq!(
        outcome,
        Outcome {
            requested: bytes.len(),
            accepted: bytes.len(),
            end: End::Complete
        }
    );
    let mut expected = b"head------------tail".to_vec();
    expected
        .get_mut(4..4 + bytes.len())
        .unwrap()
        .copy_from_slice(bytes);
    assert_eq!(std::fs::read(&path).unwrap(), expected);
    // SAFETY: same held open file description, no mutation by this observation.
    assert_eq!(
        unsafe { libc::lseek(duplicate.as_raw_fd(), 0, libc::SEEK_CUR) },
        2
    );
}

#[test]
fn prepared_object_survives_rename_and_does_not_write_replacement() {
    let tmp = shards_testdir::TempDir::new("native-write-rename").unwrap();
    let path = tmp.join("file");
    std::fs::write(&path, b"original").unwrap();
    let prepared = Prepared::existing(object(&path), Request::new(0, b"OWN").unwrap()).unwrap();
    let held = prepared.identity();
    std::fs::rename(&path, tmp.join("old")).unwrap();
    std::fs::write(&path, b"replacement").unwrap();
    let outcome = terminal(prepared.commit());
    assert_eq!(outcome.accepted, 3);
    assert_eq!(outcome.end, End::Complete);
    assert_eq!(std::fs::read(tmp.join("old")).unwrap(), b"OWNginal");
    assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
    let old = std::fs::metadata(tmp.join("old")).unwrap();
    assert_eq!(held, (old.dev(), old.ino()));
}

#[test]
fn readonly_and_append_modes_refuse_before_any_bytes_change() {
    let tmp = shards_testdir::TempDir::new("native-write-modes").unwrap();
    let path = tmp.join("file");
    std::fs::write(&path, b"unchanged").unwrap();
    let readonly: OwnedFd = File::open(&path).unwrap().into();
    let rejected = Prepared::existing(readonly, Request::new(0, b"bad").unwrap())
        .err()
        .unwrap();
    assert_eq!(rejected.failure, Failure::Access);
    assert!(file::stat(rejected.object.as_fd()).is_ok());
    assert_eq!(rejected.request.bytes, b"bad");
    let append: OwnedFd = OpenOptions::new().append(true).open(&path).unwrap().into();
    let rejected = Prepared::existing(append, Request::new(0, b"bad").unwrap())
        .err()
        .unwrap();
    assert_eq!(rejected.failure, Failure::Append);
    assert!(file::stat(rejected.object.as_fd()).is_ok());
    assert_eq!(std::fs::read(&path).unwrap(), b"unchanged");
}

#[test]
fn directory_object_is_returned_and_never_used_for_write() {
    let tmp = shards_testdir::TempDir::new("native-write-kind").unwrap();
    let directory: OwnedFd = File::open(&tmp).unwrap().into();
    let rejected = Prepared::existing(directory, Request::new(0, b"bad").unwrap())
        .err()
        .unwrap();
    assert_eq!(rejected.failure, Failure::FileKind);
    assert_eq!(
        file::stat(rejected.object.as_fd()).unwrap().st_mode & libc::S_IFMT,
        libc::S_IFDIR
    );
}

#[test]
fn checked_range_refuses_overflow_without_opening_or_mutating_an_object() {
    assert!(matches!(Request::new(u64::MAX, b"x"), Err(Failure::Bounds)));
    assert!(matches!(
        Request::new(i64::MAX as u64, b"x"),
        Err(Failure::Bounds)
    ));
    assert!(Request::new(i64::MAX as u64, b"").is_ok());
}

#[test]
fn cancellation_before_commit_and_empty_write_preserve_actual_file() {
    let tmp = shards_testdir::TempDir::new("native-write-cancel").unwrap();
    let path = tmp.join("file");
    std::fs::write(&path, b"unchanged").unwrap();
    let prepared = Prepared::existing(object(&path), Request::new(0, b"bad").unwrap()).unwrap();
    assert_eq!(
        prepared.cancel(),
        Outcome {
            requested: 3,
            accepted: 0,
            end: End::Cancelled
        }
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"unchanged");
    let prepared = Prepared::existing(object(&path), Request::new(0, b"").unwrap()).unwrap();
    assert_eq!(
        terminal(prepared.commit()),
        Outcome {
            requested: 0,
            accepted: 0,
            end: End::Complete
        }
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"unchanged");
}

#[test]
fn same_canonical_audit_reserves_intent_and_terminal_before_ordinary_effect() {
    use crate::broker::audit::{Journal, Status};
    let tmp = shards_testdir::TempDir::new("native-write-audit").unwrap();
    let path = tmp.join("file");
    std::fs::write(&path, b"old").unwrap();
    let prepared = Prepared::existing(object(&path), Request::new(0, b"new").unwrap()).unwrap();
    // This is the canonical audit module's explicit native fixture constructor,
    // not a production journal/right minted from File+size.
    let journal = Journal::for_test(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(tmp.join("audit"))
            .unwrap(),
        2,
    );
    let mut reservation = journal.reserve_records(1, 1);
    assert_eq!(reservation.status(), Status::Reserved);
    assert_eq!(std::fs::read(&path).unwrap(), b"old");
    reservation.intent(b"I").unwrap();
    assert_eq!(reservation.status(), Status::IntentSynced);
    assert_eq!(std::fs::read(tmp.join("audit")).unwrap(), b"I");
    let outcome = terminal(prepared.commit());
    assert_eq!(
        outcome,
        Outcome {
            requested: 3,
            accepted: 3,
            end: End::Complete
        }
    );
    let returned = reservation.terminal(b"T");
    assert_eq!(returned.result, Ok(Status::TerminalSynced));
    assert_eq!(std::fs::read(tmp.join("audit")).unwrap(), b"IT");
    assert_eq!(std::fs::read(&path).unwrap(), b"new");
}
