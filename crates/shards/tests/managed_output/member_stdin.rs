//! Plan step 5 on a real microVM: a harness member's stdin is a stream its owner writes over
//! the daemon's owner endpoint, and its stdout and stderr streams its owner reads. The
//! harness echoes its stdin to its stdout as it reads it (the test guest's `echo` mode), as a
//! harness speaking a stdio protocol answers its adapter. Written for ROOT to run (real-VM
//! suites are the coordinator's): `cargo test -p shards --test managed_output member_stdin`.
use super::*;
use crate::managed_capture::{InputHeard, InputWriter, Until};
use shards_abi::run::managed::InputEnd;
use shards_remote::managed::input::Said;

/// The input: JSON lines, a NUL, bytes no UTF-8 holds, a terminal's escape, no final newline;
/// then more than a pipe holds, so that the member's reading paces its owner's writing.
fn input() -> Vec<u8> {
    let mut input = Vec::new();
    input.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n");
    input.extend_from_slice(
        b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"turn\",\"params\":{\"text\":\"hi\"}}\n",
    );
    input.extend_from_slice(b"\0\xff\x80\x1b[2Jno-newline\xc3\x28");
    input.extend((0..300 * 1024u32).map(|i| (i % 251) as u8));
    input
}

/// Reads the observer's next record, which must be its channel's end at `wire`.
#[track_caller]
fn ended(observer: &Observer, wire: u64, until: Until<'_>) {
    observer.sender.send(Command::Handler(Ask::Ack(wire))).unwrap();
    match observer.next(until) {
        Event::Record(offset, bytes) => {
            assert_eq!(offset, wire);
            assert_eq!(Record::decode(&bytes).unwrap().event, CaptureEvent::End);
        }
        other => panic!("the member's channel did not end: {other:?}"),
    }
}

#[test]
fn a_members_stdin_is_its_owners_stream_exact_paced_resumed_and_ended_as_its_owner_ends_it() {
    if common::cannot_run_vms() {
        return;
    }
    let mut fixture = Fixture::new();
    fixture.stdin = Some("harness:driver");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Its observers' and writers' waits judged on its home (D209).
        let home = fixture.home.to_path_buf();
        let until = Until::Judged(&home);
        fixture.start_with_options(
            &["managed-capture", "agent"],
            &["managed-capture", "driver", "echo"],
            "",
            None,
            None,
            Bound::Judged,
        );
        let metadata = fixture.metadata.as_ref().unwrap();
        let address = metadata.endpoints[&(2, 0)];
        // A key that is not the owner's writes nothing.
        let stranger = InputWriter::start(&Identity::generate().unwrap(), metadata, 2, address, 0, until);
        assert!(
            matches!(stranger, Err(InputHeard::Lost(_))),
            "a stranger reached the member's input: {:?}",
            stranger.err()
        );
        let input = input();
        let half = input.len() / 2;
        let observer = fixture.observe(2, Channel::Stdout, None, false, Bound::Judged);
        let metadata = fixture.metadata.as_ref().unwrap();
        let reading = &fixture.observers[observer];
        let (mut writer, (_, wire)) = beside(
            reading,
            || {
                // Half, then its owner gone without its end: the member's stdin stays open,
                // and its next subscription is told where to go on.
                let mut first = InputWriter::start(&fixture.owner, metadata, 2, address, 0, until).unwrap();
                first.write_all(&input[..half], until).unwrap();
                drop(first);
                let mut again = InputWriter::start(&fixture.owner, metadata, 2, address, 0, until).unwrap();
                let at = usize::try_from(again.at).unwrap();
                assert_eq!(at, half, "its next subscription goes on where the last was taken");
                again.write_all(&input[at..], until).unwrap();
                again
            },
            || reading.exact(declared_label(2), &input, until),
        );
        // The run's own command's output is its own meanwhile.
        fixture.main_progress(Duration::MAX);
        // Its owner's end: every byte in the member's stdin, which then reads its end.
        assert_eq!(
            writer.end(0, until).unwrap(),
            (input.len() as u64, InputEnd::Closed)
        );
        ended(&fixture.observers[observer], wire, until);
        let errors = fixture.observe(2, Channel::Stderr, None, false, Bound::Judged);
        let said = format!("driver\0echo-end {}", input.len());
        let (_, wire) = fixture.observers[errors].exact(declared_label(2), said.as_bytes(), until);
        ended(&fixture.observers[errors], wire, until);
    }));
    let stopped = fixture.shutdown(Bound::Judged);
    if let Err(error) = result {
        let _ = writeln!(std::io::stderr(), "owner cleanup: {stopped:?}");
        std::panic::resume_unwind(error);
    }
    stopped.unwrap();
}

/// An input with no owner's end: its member stops reading, and its owner hears its input
/// ended (`EPIPE`) with what the member's stdin took, whether or not it writes.
#[test]
fn a_member_that_ends_ends_its_input_and_its_owner_hears_how() {
    if common::cannot_run_vms() {
        return;
    }
    let mut fixture = Fixture::new();
    fixture.stdin = Some("harness:driver");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let home = fixture.home.to_path_buf();
        let until = Until::Judged(&home);
        // The harness writes its bytes and ends, never reading its stdin.
        fixture.start_with_options(
            &["managed-capture", "agent"],
            &["managed-capture", "harness", "exit"],
            "",
            None,
            None,
            Bound::Judged,
        );
        fixture.consume(2, "harness", Channel::Stdout);
        fixture.consume(2, "harness", Channel::Stderr);
        let metadata = fixture.metadata.as_ref().unwrap();
        let address = metadata.endpoints[&(2, 0)];
        // Subscribed before its member ended, it is told where its first record goes, then
        // the end; after, the end at once.
        let ended = match InputWriter::start(&fixture.owner, metadata, 2, address, 0, until) {
            Ok(mut writer) => match writer.next(until, "word that the input ended") {
                Ok(InputHeard::Said(said)) => said,
                other => panic!("its owner was not told the input ended: {other:?}"),
            },
            Err(InputHeard::Said(said)) => said,
            Err(other) => panic!("no subscription to the ended input: {other:?}"),
        };
        // The guest's errno, Linux's (include/uapi/asm-generic/errno-base.h), as its init
        // reports it whatever the host's own.
        const EPIPE: i32 = 32;
        assert_eq!(
            ended,
            Said::Ended {
                written: 0,
                end: InputEnd::Aborted(EPIPE)
            }
        );
    }));
    let stopped = fixture.shutdown(Bound::Judged);
    if let Err(error) = result {
        let _ = writeln!(std::io::stderr(), "owner cleanup: {stopped:?}");
        std::panic::resume_unwind(error);
    }
    stopped.unwrap();
}
