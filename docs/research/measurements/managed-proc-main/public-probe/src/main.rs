//! Linux-only test caller of the ordinary public API; never installed as a product.
#[cfg(not(target_os = "linux"))]
compile_error!("this private public-API probe is Linux-only");

#[path = "../../allocation.rs"]
mod allocation;

use shards_api::{Guest, GuestSpec, Guests as _, Identity, Init, ProcessFailure, Refusal, Shards};
use std::fmt;
use std::io::{self, Write as _};
use std::process::ExitCode;

const COMM: &str = "/proc/self/comm";
const STAT: &str = "/proc/self/stat";
const NAME: &[u8] = b"own \xff) ) \xfe";

#[derive(Debug)]
enum Failure {
    Usage,
    Io(&'static str, io::Error),
    StartRecord,
    StartOverflow,
    NameRecord,
    NameNotRestored,
    RawStatNotObserved,
    UnexpectedGuest,
    UnexpectedRefusal(Refusal),
    FdCount,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => f.write_str("expected oom or raw-comm"),
            Self::Io(at, error) => write!(f, "{at}: {error}"),
            Self::StartRecord => f.write_str("self stat has no valid starttime"),
            Self::StartOverflow => f.write_str("self start cannot have a distinct checked successor"),
            Self::NameRecord => f.write_str("self comm is not a nonempty newline-terminated name"),
            Self::NameNotRestored => f.write_str("self name restoration was not verified"),
            Self::RawStatNotObserved => f.write_str("self stat did not contain the legal raw name"),
            Self::UnexpectedGuest => f.write_str("unexpected guest was forgotten; self is never stopped"),
            Self::UnexpectedRefusal(refusal) => write!(f, "unexpected public refusal: {refusal:?}"),
            Self::FdCount => f.write_str("self descriptor count did not return or was unrepresentable"),
        }
    }
}

fn self_start() -> Result<u64, Failure> {
    // Independent pre-fault fixture observation, using proc_pid_stat(5) field22.
    let bytes = std::fs::read(STAT).map_err(|e| Failure::Io("read initial self stat", e))?;
    bytes
        .iter()
        .rposition(|b| *b == b')')
        .and_then(|at| bytes.get(at.checked_add(1)?..))
        .and_then(|fields| {
            fields
                .split(u8::is_ascii_whitespace)
                .filter(|f| !f.is_empty())
                .nth(19)
        })
        .and_then(|field| std::str::from_utf8(field).ok())
        .and_then(|field| field.parse().ok())
        .ok_or(Failure::StartRecord)
}

fn fd_count() -> Result<usize, Failure> {
    let entries = std::fs::read_dir("/proc/self/fd").map_err(|e| Failure::Io("open self fd list", e))?;
    let mut count = 0usize;
    for entry in entries {
        entry.map_err(|e| Failure::Io("read self fd list", e))?;
        count = count.checked_add(1).ok_or(Failure::FdCount)?;
    }
    Ok(count)
}

fn refused(result: Result<Box<dyn Guest>, Refusal>) -> Result<Refusal, Failure> {
    match result {
        Err(refusal) => Ok(refusal),
        Ok(guest) => {
            // Even an unexpected success cannot run Guest::drop/stop on this process.
            std::mem::forget(guest);
            Err(Failure::UnexpectedGuest)
        }
    }
}

// Some(bytes) owns the restoration obligation. It is released only after an
// explicit write and byte verification; Drop provides an error/unwind fallback.
struct NameGuard(Option<Vec<u8>>);

impl NameGuard {
    fn save() -> Result<Self, Failure> {
        let mut bytes = std::fs::read(COMM).map_err(|e| Failure::Io("save self comm", e))?;
        if bytes.pop() != Some(b'\n') || bytes.is_empty() {
            return Err(Failure::NameRecord);
        }
        Ok(Self(Some(bytes)))
    }

    fn restore(&mut self) -> Result<(), Failure> {
        let original = self.0.as_ref().ok_or(Failure::NameNotRestored)?;
        std::fs::write(COMM, original).map_err(|e| Failure::Io("restore self comm", e))?;
        let now = std::fs::read(COMM).map_err(|e| Failure::Io("verify restored self comm", e))?;
        if now.strip_suffix(b"\n") != Some(original.as_slice()) {
            return Err(Failure::NameNotRestored);
        }
        self.0 = None;
        Ok(())
    }
}

impl Drop for NameGuard {
    fn drop(&mut self) {
        if let Some(original) = &self.0 {
            let _ = std::fs::write(COMM, original);
        }
    }
}

fn oom(identity: &Identity, spec: &GuestSpec, actual: u64) -> Result<(), Failure> {
    let before = fd_count()?;
    let refusal = refused(allocation::after(0, || Shards.attach(identity, spec)))?;
    let after = fd_count()?;
    if !matches!(&refusal, Refusal::Process { pid, expected_started,
        failure: ProcessFailure::CapacityUnavailable }
        if *pid == identity.pid && *expected_started == identity.started)
    {
        return Err(Failure::UnexpectedRefusal(refusal));
    }
    if before != after {
        return Err(Failure::FdCount);
    }
    writeln!(
        io::stdout(),
        "oom pid={} original_started={actual} expected_started={} refusal=CapacityUnavailable fds_before={before} fds_after={after}",
        identity.pid,
        identity.started
    )
    .map_err(|e| Failure::Io("report oom result", e))
}

fn raw_comm(identity: &Identity, spec: &GuestSpec, actual: u64) -> Result<(), Failure> {
    let mut guard = NameGuard::save()?;
    // All return paths from this observation rejoin the explicit restoration below.
    // No assertion or reporting occurs while the changed name is installed.
    let observation = (|| {
        std::fs::write(COMM, NAME).map_err(|e| Failure::Io("set own TGID comm", e))?;
        let stat = std::fs::read(STAT).map_err(|e| Failure::Io("verify raw self stat", e))?;
        if !stat.windows(NAME.len()).any(|bytes| bytes == NAME) || std::str::from_utf8(&stat).is_ok() {
            return Err(Failure::RawStatNotObserved);
        }
        refused(Shards.attach(identity, spec))
    })();
    guard.restore()?;
    let refusal = observation?;
    if !matches!(&refusal, Refusal::Process { pid, expected_started,
        failure: ProcessFailure::DifferentStart { observed } }
        if *pid == identity.pid && *expected_started == identity.started && *observed == actual)
    {
        return Err(Failure::UnexpectedRefusal(refusal));
    }
    writeln!(io::stdout(), "raw-comm pid={} original_started={actual} expected_started={} observed_started={actual} raw_stat_verified=true name_restored=true", identity.pid, identity.started)
        .map_err(|e| Failure::Io("report raw name result", e))
}

fn run() -> Result<(), Failure> {
    let mode = std::env::args_os().nth(1).ok_or(Failure::Usage)?;
    let actual = self_start()?;
    let expected = actual.checked_add(1).ok_or(Failure::StartOverflow)?;
    let identity = Identity {
        pid: std::process::id(),
        started: expected,
        vsock: "unused".into(),
    };
    let spec = GuestSpec {
        kernel: "unused".into(),
        init: Init::Initrd("unused".into()),
        cmdline: String::new(),
        vcpus: 1,
        memory_mib: 256,
        gpu: false,
        gpu_devices: Vec::new(),
        disks: vec![],
        shares: vec![],
        ports: vec![1],
        binary: None,
    };
    if mode == "oom" {
        oom(&identity, &spec, actual)
    } else if mode == "raw-comm" {
        raw_comm(&identity, &spec, actual)
    } else {
        Err(Failure::Usage)
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr(), "{error}");
            ExitCode::FAILURE
        }
    }
}
