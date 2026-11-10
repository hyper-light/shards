use super::*;
use std::io::{Read, Write};
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

#[derive(Debug)]
enum Failure {
    Io(io::Error),
    Turn(Fault),
    Check(&'static str),
}

impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

fn supplied(name: &str) -> String {
    std::env::var(name).expect("explicit finite native turn input")
}

fn payload(name: &str) -> Vec<u8> {
    let word = supplied(name);
    assert_eq!(word.len() % 2, 0);
    word.as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn bytes_record(role: &str, bytes: &[u8]) {
    println!(
        "managed-tcp-forward {{\"type\":\"NativeTurnBytes\",\"role\":\"{role}\",\"bytes\":{},\"hex\":\"{}\"}}",
        bytes.len(),
        hex(bytes)
    );
}

fn queue(stream: &UnixStream, option: i32, input: i32) -> io::Result<()> {
    assert!(input > 0);
    // SAFETY: exact initialized option borrowed by this owned socket.
    count(unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            option,
            (&raw const input).cast(),
            size_of_val(&input) as _,
        ) as isize
    })?;
    let mut actual = 0i32;
    let mut size = size_of_val(&actual) as libc::socklen_t;
    // SAFETY: initialized writable integer and exact native option length.
    count(unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            option,
            (&raw mut actual).cast(),
            &raw mut size,
        ) as isize
    })?;
    println!(
        "managed-tcp-forward {{\"type\":\"NativeTurnQueue\",\"option\":{option},\"supplied\":{input},\"actual\":{actual},\"size\":{size}}}"
    );
    assert_eq!(size as usize, size_of_val(&actual));
    assert!(actual > 0);
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct Suffix {
    half: Half,
    read: usize,
    written: usize,
    pending: usize,
    sent: usize,
    extra: usize,
    bytes: Vec<u8>,
}

fn snapshot(stage: &str, direction: &Direction, progress: &Progress, result: &Result<bool, Fault>) -> Suffix {
    let end = direction.sent.checked_add(progress.pending).unwrap();
    let bytes = direction.bytes.get(direction.sent..end).unwrap().to_vec();
    println!(
        "managed-tcp-forward {{\"type\":\"NativeTurnSnapshot\",\"stage\":\"{stage}\",\"half\":\"{:?}\",\"read\":{},\"written\":{},\"pending\":{},\"sent\":{},\"extra_consumed\":{},\"would_block\":{},\"write_would_block\":{},\"suffix_hex\":\"{}\",\"turn_result\":\"{result:?}\"}}",
        progress.half,
        progress.read,
        progress.written,
        progress.pending,
        direction.sent,
        progress.extra_consumed,
        progress.would_block,
        progress.write_would_block,
        hex(&bytes)
    );
    Suffix {
        half: progress.half,
        read: progress.read,
        written: progress.written,
        pending: progress.pending,
        sent: direction.sent,
        extra: progress.extra_consumed,
        bytes,
    }
}

fn receive(stream: &mut UnixStream, bytes: &mut [u8], deadline: &Deadline) -> io::Result<()> {
    let mut received = 0;
    while received < bytes.len() {
        let part = bytes.get_mut(received..).unwrap();
        let n = deadline.io(stream.as_raw_fd(), libc::POLLIN, || stream.read(part))?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        received += n;
    }
    Ok(())
}

fn exercise(original: &Deadline) -> Result<(), Failure> {
    let request = payload("SHARDS_TCP_REQUEST_HEX");
    let seed = payload("SHARDS_TURN_PREFILL_SEED_HEX");
    let envelope: usize = supplied("SHARDS_TURN_PREFILL_BYTES").parse().unwrap();
    assert!(!request.is_empty() && !seed.is_empty() && envelope > 0);
    let prefill: Vec<u8> = seed.iter().copied().cycle().take(envelope).collect();
    let up = supplied("SHARDS_TCP_UP_BYTES").parse().unwrap();
    let down = supplied("SHARDS_TCP_DOWN_BYTES").parse().unwrap();
    let slots = Slots::reserve(up, down).map_err(Failure::Turn)?;
    // SAFETY: eventfd creates this case's own nonblocking cancellation object.
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if fd < 0 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: successful eventfd transfers its sole descriptor ownership.
    let cancel = unsafe { OwnedFd::from_raw_fd(fd) };
    let deadline = Deadline {
        at: original.at,
        cancel: Some(cancel.as_raw_fd()),
    };
    let control = Control::Until(&deadline);
    let (mut source, input) = UnixStream::pair()?;
    let (output, mut backend) = UnixStream::pair()?;
    for stream in [&source, &input, &output, &backend] {
        stream.set_nonblocking(true)?;
    }
    queue(
        &output,
        libc::SO_SNDBUF,
        supplied("SHARDS_NATIVE_SNDBUF_BYTES").parse().unwrap(),
    )?;
    queue(
        &backend,
        libc::SO_RCVBUF,
        supplied("SHARDS_NATIVE_RCVBUF_BYTES").parse().unwrap(),
    )?;
    let mut accepted = 0;
    let mut blocked = None;
    // Every continuation has accepted new finite bytes. Stop at the first
    // actual WouldBlock; the supplied envelope is never a capacity guarantee.
    while accepted < prefill.len() {
        let bytes = prefill.get(accepted..).unwrap();
        let result = deadline.io(output.as_raw_fd(), libc::POLLOUT, || {
            // SAFETY: live owned stream and exact readable finite input slice.
            Ok(count(unsafe {
                libc::send(
                    output.as_raw_fd(),
                    bytes.as_ptr().cast(),
                    bytes.len(),
                    libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                )
            }))
        })?;
        match result {
            Ok(0) => return Err(Failure::Io(io::ErrorKind::WriteZero.into())),
            Ok(n) => accepted += n,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                blocked = Some(error);
                break;
            }
            Err(error) => return Err(error.into()),
        }
    }
    bytes_record("PrefillAccepted", prefill.get(..accepted).unwrap());
    println!(
        "managed-tcp-forward {{\"type\":\"NativeTurnPrefill\",\"envelope\":{envelope},\"accepted\":{accepted},\"would_block\":{},\"error_kind\":\"{:?}\",\"errno\":\"{:?}\"}}",
        blocked.is_some(),
        blocked.as_ref().map(io::Error::kind),
        blocked.as_ref().and_then(io::Error::raw_os_error)
    );
    if blocked.is_none() {
        return Err(Failure::Check(
            "finite prefill did not observe native sendWouldBlock",
        ));
    }
    let mut source_accepted = 0;
    while source_accepted < request.len() {
        let bytes = request.get(source_accepted..).unwrap();
        let n = deadline.io(source.as_raw_fd(), libc::POLLOUT, || Ok(source.write(bytes)))??;
        if n == 0 {
            return Err(Failure::Io(io::ErrorKind::WriteZero.into()));
        }
        source_accepted += n;
    }
    source.shutdown(std::net::Shutdown::Write)?;
    bytes_record("SourceAccepted", &request);
    let mut direction = Direction {
        bytes: slots.to_backend,
        sent: 0,
        limit: ByteLimit::Bytes(request.len()),
        source: Leg::Client,
    };
    let mut progress = Progress::new();
    let first = direction.turn(
        input.as_fd(),
        output.as_fd(),
        &mut progress,
        &control,
        EndPolicy::Independent,
    );
    let pending = snapshot("SourceReceived", &direction, &progress, &first);
    if !first.map_err(Failure::Turn)?
        || pending.pending == 0
        || pending.read != pending.pending
        || pending.written != 0
        || pending.sent != 0
        || request.get(..pending.read) != Some(pending.bytes.as_slice())
    {
        return Err(Failure::Check("turn did not obtain its actual source prefix"));
    }
    let would_block = progress.write_would_block;
    let second = direction.turn(
        input.as_fd(),
        output.as_fd(),
        &mut progress,
        &control,
        EndPolicy::Independent,
    );
    let retained = snapshot("BlockedSend", &direction, &progress, &second);
    if second.map_err(Failure::Turn)? || progress.write_would_block != would_block + 1 || pending != retained
    {
        return Err(Failure::Check(
            "one native turn sendWouldBlock did not retain the exact suffix",
        ));
    }
    // The backend independently receives only the recorded prefill before
    // the same Direction is allowed a positive recovery turn.
    let mut received = vec![0; accepted.checked_add(request.len()).unwrap()];
    receive(&mut backend, received.get_mut(..accepted).unwrap(), &deadline)?;
    bytes_record("PrefillDrained", received.get(..accepted).unwrap());
    if received.get(..accepted) != prefill.get(..accepted) {
        return Err(Failure::Check("independent prefill receipt differs"));
    }
    let blocked_count = progress.write_would_block;
    while progress.half != Half::WriteClosed(SourceEnd::Eof) {
        let written = progress.written;
        let waits = progress.would_block;
        let result = direction.turn(
            input.as_fd(),
            output.as_fd(),
            &mut progress,
            &control,
            EndPolicy::Independent,
        );
        snapshot("Recovery", &direction, &progress, &result);
        if !result.map_err(Failure::Turn)?
            || progress.would_block != waits
            || progress.write_would_block != blocked_count
        {
            return Err(Failure::Check(
                "recovery stopped at a missing progress or native WouldBlock",
            ));
        }
        let start = accepted.checked_add(written).unwrap();
        let end = accepted.checked_add(progress.written).unwrap();
        receive(&mut backend, received.get_mut(start..end).unwrap(), &deadline)?;
    }
    let mut extra = [0u8; 1];
    let eof = deadline.io(backend.as_raw_fd(), libc::POLLIN, || backend.read(&mut extra))?;
    bytes_record("DestinationReceived", &received);
    println!(
        "managed-tcp-forward {{\"type\":\"NativeTurnTerminal\",\"eof_read\":{eof},\"prefill_bytes\":{accepted},\"relayed_bytes\":{},\"read\":{},\"written\":{},\"pending\":{},\"extra_consumed\":{},\"half\":\"{:?}\"}}",
        request.len(),
        progress.read,
        progress.written,
        progress.pending,
        progress.extra_consumed,
        progress.half
    );
    if eof != 0
        || progress.read != request.len()
        || progress.written != request.len()
        || progress.pending != 0
        || progress.extra_consumed != 0
        || received.get(accepted..) != Some(request.as_slice())
    {
        return Err(Failure::Check(
            "positive retained-tail destination bytes or EOF differ",
        ));
    }
    Ok(())
}

#[test]
#[ignore = "one finite native socketpair prefill/real turn WouldBlock/retained-tail/EOF case"]
fn native_direction_turn_retains_suffix_under_native_pressure() {
    let original = Deadline::new(Duration::from_millis(
        supplied("SHARDS_NETWORK_CASE_MS").parse().unwrap(),
    ))
    .unwrap();
    let release = supplied("SHARDS_NATIVE_RELEASE_FD").parse::<i32>().unwrap();
    assert!(release > 2);
    // SAFETY: the private launcher transfers this dedicated descriptor only.
    let release = unsafe { OwnedFd::from_raw_fd(release) };
    let mut byte = [0u8; 1];
    // Trusted release only; exercise uses an owned real cancel FD and SAME at.
    let n = original
        .io(release.as_raw_fd(), libc::POLLIN, || {
            // SAFETY: owned live pipe and one initialized writable control byte.
            count(unsafe { libc::read(release.as_raw_fd(), byte.as_mut_ptr().cast(), byte.len()) })
        })
        .unwrap();
    assert_eq!((n, byte), (1, *b"R"));
    drop(release);
    let descriptors = || {
        std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .map(|item| item.unwrap())
            .count()
    };
    let before = descriptors();
    let result = exercise(&original);
    let after = descriptors();
    let remaining = original.at.saturating_duration_since(Instant::now());
    let (failure_kind, cause) = match &result {
        Ok(()) => ("None", String::new()),
        Err(Failure::Io(error)) => ("Io", format!("{:?}/{:?}", error.kind(), error.raw_os_error())),
        Err(Failure::Turn(fault)) => ("Turn", format!("{fault:?}")),
        Err(Failure::Check(reason)) => ("Check", (*reason).to_owned()),
    };
    println!(
        "managed-tcp-forward {{\"type\":\"NativeTurnCleanup\",\"fd_before\":{before},\"fd_after\":{after},\"original_remaining_ns\":{},\"additional_workers\":0,\"result_ok\":{},\"failure_kind\":\"{failure_kind}\",\"failure_cause\":{cause:?},\"kernel_return\":\"Unknown\"}}",
        remaining.as_nanos(),
        result.is_ok()
    );
    assert_eq!(before, after);
    assert!(!remaining.is_zero());
    result.unwrap();
}
