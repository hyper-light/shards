use shards_managed_otel_encoder_check::{Failure, Scope, Snapshot, write};

fn main() -> std::io::Result<()> {
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
    write(
        &mut std::io::stdout().lock(),
        "é—🦀\"\\/\u{0}\u{8}\t\n\r\u{1b}\u{1f}",
        "vm",
        u64::MAX,
        &[healthy, failed, boundary],
    )
}
