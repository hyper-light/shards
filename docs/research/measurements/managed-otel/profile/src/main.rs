//! Coordinator-only encoding measurement. No SDK reader, transport or VM cost is implied.
use std::hint::black_box;
use std::io::{self, Write};
use std::time::Instant;

pub use native_codec::{Failure, Scope, Snapshot};

#[path = "../../../managed-otel-rust-reference/src/reference.rs"]
mod reference;

#[cfg(feature = "alloc-count")]
mod allocations;

#[derive(Clone, Copy)]
enum Mode {
    Native,
    Compact,
    Pretty,
}

impl Mode {
    fn parse(text: &str) -> io::Result<Self> {
        match text {
            "native-stream" => Ok(Self::Native),
            "official-compact-buffer" => Ok(Self::Compact),
            "official-pretty-buffer" => Ok(Self::Pretty),
            _ => Err(io::ErrorKind::InvalidInput.into()),
        }
    }

    fn emit(self, out: &mut impl Write, run: &str, generation: u64, samples: &[Snapshot]) -> io::Result<()> {
        match self {
            Self::Native => native_codec::write(out, run, generation, samples),
            mode => {
                let request = reference::request(run, generation, samples)?;
                let bytes = match mode {
                    Self::Compact => serde_json::to_vec(&request),
                    Self::Pretty => serde_json::to_string_pretty(&request).map(String::into_bytes),
                    Self::Native => return Err(io::ErrorKind::InvalidInput.into()),
                }
                .map_err(io::Error::other)?;
                out.write_all(&bytes)
            }
        }
    }
}

struct Count {
    bytes: usize,
    writes: usize,
}

impl Write for Count {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        black_box(bytes);
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .ok_or(io::ErrorKind::InvalidData)?;
        self.writes = self.writes.checked_add(1).ok_or(io::ErrorKind::InvalidData)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn usage() -> io::Result<libc::rusage> {
    let mut value = std::mem::MaybeUninit::uninit();
    // SAFETY: getrusage initializes the whole ABI structure on success.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, value.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful native call initialized value.
    Ok(unsafe { value.assume_init() })
}

fn micros(time: libc::timeval) -> i128 {
    i128::from(time.tv_sec) * 1_000_000 + i128::from(time.tv_usec)
}

fn main() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let name = args.next().ok_or(io::ErrorKind::InvalidInput)?;
    let mode = Mode::parse(&name)?;
    let iterations = args
        .next()
        .ok_or(io::ErrorKind::InvalidInput)?
        .parse::<usize>()
        .map_err(io::Error::other)?;
    if iterations == 0 || args.next().is_some() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let healthy = Snapshot {
        scope: Scope::Run,
        time_unix_nano: 1_794_335_411_506_000_001,
        cpu_usec: Ok(1_000_001),
        memory_bytes: Ok(0),
        threads: Ok(4),
        read_bytes: Ok(12),
        write_bytes: Ok(34),
    };
    let failed = Snapshot {
        scope: Scope::Workload,
        cpu_usec: Err(Failure::Missing),
        memory_bytes: Err(Failure::Io(2)),
        threads: Err(Failure::Invalid),
        read_bytes: Err(Failure::Overflow),
        ..healthy
    };
    let boundary = Snapshot {
        scope: Scope::Member(u32::MAX),
        cpu_usec: Ok(u64::MAX),
        memory_bytes: Ok(u64::MAX),
        threads: Ok(i64::MAX as u64),
        read_bytes: Ok(0),
        write_bytes: Ok(i64::MAX as u64),
        ..healthy
    };
    let samples = [healthy, failed, boundary];
    let run = "é—🦀\"\\/\u{0}\u{8}\t\n\r\u{1b}\u{1f}";
    let mut times = Vec::new();
    times.try_reserve_exact(iterations).map_err(io::Error::other)?;
    times.resize(iterations, 0u128);
    let before = usage()?;
    #[cfg(feature = "alloc-count")]
    allocations::start();
    let mut out = Count { bytes: 0, writes: 0 };
    let began = Instant::now();
    for time in &mut times {
        let start = Instant::now();
        mode.emit(&mut out, black_box(run), u64::MAX, black_box(&samples))?;
        *time = start.elapsed().as_nanos();
    }
    let elapsed = began.elapsed().as_nanos();
    #[cfg(feature = "alloc-count")]
    let allocations = allocations::end();
    let after = usage()?;
    #[cfg(not(feature = "alloc-count"))]
    let allocations = serde_json::Value::Null;
    // All formatting/report allocations occur after the measured region.
    let result = serde_json::json!({
        "mode": name, "iterations": iterations, "elapsed_ns": elapsed,
        "times_ns": times, "output_bytes": out.bytes, "write_calls": out.writes,
        "user_cpu_us": micros(after.ru_utime)-micros(before.ru_utime),
        "system_cpu_us": micros(after.ru_stime)-micros(before.ru_stime),
        "minor_faults": after.ru_minflt-before.ru_minflt,
        "major_faults": after.ru_majflt-before.ru_majflt,
        "max_rss_kib": after.ru_maxrss,
        "voluntary_context_switches": after.ru_nvcsw-before.ru_nvcsw,
        "involuntary_context_switches": after.ru_nivcsw-before.ru_nivcsw,
        "allocation_instrumentation": cfg!(feature="alloc-count"),
        "allocations": allocations,
        "scope": "exact three-snapshot codec fixture -> counted Write; excludes cgroup collection, SDK pipeline, HTTP, VM and shared-parent/kernel producer costs"
    });
    serde_json::to_writer(io::stdout().lock(), &result).map_err(io::Error::other)
}
