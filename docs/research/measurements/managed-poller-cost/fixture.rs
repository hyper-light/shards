//! ROOT-only baseline workload. `accounting.rs` alone is safe for pure light tests.

#[cfg(not(any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64"))))]
compile_error!("this baseline covers Linux and macOS arm64 only");

mod accounting;

use accounting::{Inputs, Round, TokenError, acquired_matches, held_descriptors, returned};
use serde::Serialize;
use shards_vmm::platform::{self, Poller as UnixPoller};
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::time::{Duration, Instant};

#[derive(Debug)]
enum Failure {
    Allocation,
    Deadline,
    RecordLimit,
    Counter,
    Token(TokenError),
    Io(&'static str, io::Error),
    WriteCount(usize),
    ReadCount(usize),
    Byte(u8),
    NotDrained,
    DescriptorObservation,
    DescriptorAcquisition,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Token(error) => write!(f, "Token({error:?})"),
            Self::Io(stage, error) => write!(f, "Io({stage}, {error})"),
            Self::WriteCount(count) => write!(f, "WriteCount({count})"),
            Self::ReadCount(count) => write!(f, "ReadCount({count})"),
            Self::Byte(byte) => write!(f, "Byte({byte})"),
            other => write!(f, "{other:?}"),
        }
    }
}

fn remaining(deadline: Instant) -> Result<Duration, Failure> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or(Failure::Deadline)
}

fn reserve<T>(values: &mut Vec<T>, count: usize) -> Result<(), Failure> {
    values
        .try_reserve_exact(count)
        .map_err(|_| Failure::Allocation)
}

struct Pair {
    read: UnixStream,
    write: UnixStream,
    registered: bool,
}

#[derive(Serialize)]
struct Observation {
    token: u64,
    write_completed_to_wait_return_ns: u128,
    wait_return_to_drained_ns: u128,
}

#[derive(Serialize)]
struct WaitSample {
    record: usize,
    round: usize,
    wait_ns: u128,
    capacity_before: usize,
    capacity_after: usize,
    raw_tokens: Vec<u64>,
    observations: Vec<Observation>,
    wait_failed: bool,
    wait_errno: Option<i32>,
}

#[derive(Serialize)]
struct Removal {
    token: usize,
    removed: bool,
    errno: Option<i32>,
}

#[derive(Serialize)]
struct Case {
    case: usize,
    ready_count: usize,
    rounds_requested: usize,
    rounds_completed: usize,
    registrations: usize,
    waits: Vec<WaitSample>,
    removals: Vec<Removal>,
    baseline_fds: Option<u64>,
    acquired_fds: Option<u64>,
    held_fds: Option<u64>,
    acquisition_matches: bool,
    released_fds: Option<u64>,
    fd_returned: bool,
    failure: Option<String>,
    complete: bool,
}

// A single fixture byte makes every write/read count independently checkable.
// It is a workload input representation, not a production chunk/batch choice.
const BYTE: u8 = b'p';

fn seed(stream: &mut UnixStream, deadline: Instant) -> Result<Instant, Failure> {
    loop {
        remaining(deadline)?;
        match stream.write(&[BYTE]) {
            Ok(1) => return Ok(Instant::now()),
            Ok(count) => return Err(Failure::WriteCount(count)),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(Failure::Io("seed", error)),
        }
    }
}

fn read_byte(stream: &mut UnixStream, deadline: Instant) -> Result<usize, Failure> {
    let mut byte = [0u8; 1];
    loop {
        remaining(deadline)?;
        match stream.read(&mut byte) {
            Ok(count) if count == 1 && byte == [BYTE] => return Ok(count),
            Ok(1) => return Err(Failure::Byte(byte[0])),
            Ok(count) => return Err(Failure::ReadCount(count)),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(Failure::Io("read", error)),
        }
    }
}

fn verify_drained(stream: &mut UnixStream, deadline: Instant) -> Result<(), Failure> {
    let mut byte = [0u8; 1];
    loop {
        remaining(deadline)?;
        match stream.read(&mut byte) {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(Failure::Io("drain", error)),
            Ok(_) => return Err(Failure::NotDrained),
        }
    }
}

fn run_round(
    inputs: &Inputs,
    round_number: usize,
    poller: &UnixPoller,
    pairs: &mut [Pair],
    ready: &mut Vec<u64>,
    deadline: Instant,
    records_used: &mut usize,
    samples: &mut Vec<WaitSample>,
) -> Result<(), Failure> {
    let mut round = Round::new(pairs.len()).map_err(Failure::Token)?;
    let mut written = Vec::new();
    reserve(&mut written, pairs.len())?;
    for pair in pairs.iter_mut() {
        written.push(seed(&mut pair.write, deadline)?);
    }
    while round.complete().is_err() {
        if *records_used >= inputs.max_records {
            return Err(Failure::RecordLimit);
        }
        let mut raw_tokens = Vec::new();
        let mut observations = Vec::new();
        reserve(&mut raw_tokens, pairs.len())?;
        reserve(&mut observations, pairs.len())?;
        let timeout = remaining(deadline)?;
        let capacity_before = ready.capacity();
        let before = Instant::now();
        let wait_result = poller.wait(ready, Some(timeout));
        let observed = Instant::now();
        if ready.len() > raw_tokens.capacity() {
            reserve(&mut raw_tokens, ready.len())?;
        }
        raw_tokens.extend_from_slice(ready);
        let mut sample = WaitSample {
            record: *records_used,
            round: round_number,
            wait_ns: observed.duration_since(before).as_nanos(),
            capacity_before,
            capacity_after: ready.capacity(),
            raw_tokens,
            observations,
            wait_failed: wait_result.is_err(),
            wait_errno: wait_result.as_ref().err().and_then(io::Error::raw_os_error),
        };
        *records_used = records_used.checked_add(1).ok_or(Failure::Counter)?;
        let handled = (|| {
            wait_result.map_err(|error| Failure::Io("wait", error))?;
            remaining(deadline)?;
            for &token in ready.iter() {
                let index = round.observe(token).map_err(Failure::Token)?;
                let started = written.get(index).ok_or(Failure::Counter)?;
                let pair = pairs.get_mut(index).ok_or(Failure::Counter)?;
                read_byte(&mut pair.read, deadline)?;
                verify_drained(&mut pair.read, deadline)?;
                sample.observations.push(Observation {
                    token,
                    write_completed_to_wait_return_ns: observed.duration_since(*started).as_nanos(),
                    wait_return_to_drained_ns: observed.elapsed().as_nanos(),
                });
            }
            Ok(())
        })();
        samples.push(sample);
        handled?;
    }
    round.complete().map_err(Failure::Token)
}

fn run_case(
    inputs: &Inputs,
    case: usize,
    count: usize,
    deadline: Instant,
    ready: &mut Vec<u64>,
    records_used: &mut usize,
) -> Case {
    let baseline_fds = platform::open_descriptors();
    let mut poller = None;
    let mut pairs = Vec::<Pair>::new();
    let mut samples = Vec::new();
    let mut removals = Vec::new();
    let mut registrations = 0;
    let mut rounds_completed = 0;
    let mut acquired_fds = None;
    let outcome = (|| {
        baseline_fds.ok_or(Failure::DescriptorObservation)?;
        reserve(&mut pairs, count)?;
        reserve(&mut removals, count)?;
        reserve(
            &mut samples,
            inputs.max_records.saturating_sub(*records_used),
        )?;
        poller = Some(UnixPoller::new().map_err(|e| Failure::Io("poller", e))?);
        let active = poller.as_ref().ok_or(Failure::Counter)?;
        for token in 0..count {
            remaining(deadline)?;
            let (read, write) = UnixStream::pair().map_err(|e| Failure::Io("pair", e))?;
            pairs.push(Pair {
                read,
                write,
                registered: false,
            });
            let pair = pairs.last_mut().ok_or(Failure::Counter)?;
            pair.read
                .set_nonblocking(true)
                .map_err(|e| Failure::Io("read-mode", e))?;
            pair.write
                .set_nonblocking(true)
                .map_err(|e| Failure::Io("write-mode", e))?;
            let token = u64::try_from(token).map_err(|_| Failure::Counter)?;
            active
                .add(&pair.read, token)
                .map_err(|e| Failure::Io("register", e))?;
            pair.registered = true;
            registrations += 1;
        }
        acquired_fds = platform::open_descriptors();
        let held = held_descriptors(pairs.len(), true).ok_or(Failure::Counter)?;
        if !acquired_matches(baseline_fds, acquired_fds, held) {
            return Err(Failure::DescriptorAcquisition);
        }
        for round in 0..inputs.rounds {
            run_round(
                inputs,
                round,
                active,
                &mut pairs,
                ready,
                deadline,
                records_used,
                &mut samples,
            )?;
            rounds_completed += 1;
        }
        Ok(())
    })();
    // Observe partial setup as well; cleanup is outside the fallible workload.
    let acquired_fds = acquired_fds.or_else(platform::open_descriptors);
    let held_fds = held_descriptors(pairs.len(), poller.is_some());
    let acquisition_matches =
        held_fds.is_some_and(|held| acquired_matches(baseline_fds, acquired_fds, held));
    if let Some(active) = poller.as_ref() {
        for (token, pair) in pairs.iter().enumerate().filter(|(_, pair)| pair.registered) {
            let result = active.remove(&pair.read);
            removals.push(Removal {
                token,
                removed: result.is_ok(),
                errno: result.err().and_then(|e| e.raw_os_error()),
            });
        }
    }
    drop(poller.take());
    drop(pairs);
    let released_fds = platform::open_descriptors();
    let fd_returned = returned(baseline_fds, released_fds);
    let complete = outcome.is_ok()
        && acquisition_matches
        && fd_returned
        && removals.len() == registrations
        && removals.iter().all(|r| r.removed);
    Case {
        case,
        ready_count: count,
        rounds_requested: inputs.rounds,
        rounds_completed,
        registrations,
        waits: samples,
        removals,
        baseline_fds,
        acquired_fds,
        held_fds,
        acquisition_matches,
        released_fds,
        fd_returned,
        failure: outcome.err().map(|failure| failure.to_string()),
        complete,
    }
}

fn execute() -> Result<bool, String> {
    let args = std::env::args_os()
        .skip(1)
        .map(|s| s.into_string())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "NonUtf8Input".to_owned())?;
    let inputs = Inputs::parse(&args).map_err(|e| {
        format!("Input({e:?}); usage: READY_COUNTS ROUNDS DEADLINE_MS MAX_WAIT_RECORDS")
    })?;
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(inputs.deadline_ms))
        .ok_or_else(|| "DeadlineRange".to_owned())?;
    let mut ready = Vec::new();
    let mut records_used = 0;
    for (index, &count) in inputs.ready_counts.iter().enumerate() {
        let case = run_case(
            &inputs,
            index,
            count,
            deadline,
            &mut ready,
            &mut records_used,
        );
        let mut output = io::stdout().lock();
        serde_json::to_writer(&mut output, &case).map_err(|e| format!("Report({e})"))?;
        writeln!(output).map_err(|e| format!("Report({e})"))?;
        if !case.complete {
            return Ok(false);
        }
    }
    Ok(true)
}

fn main() -> ExitCode {
    match execute() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "{error}");
            ExitCode::FAILURE
        }
    }
}
