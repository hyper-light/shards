use shards_managed_otel_encoder_check::{CgroupEvent, Failure, Limit, Scope, Snapshot, write};

fn main() -> std::io::Result<()> {
    let healthy = Snapshot {
        scope: Scope::Run,
        time_unix_nano: 1_794_335_411_506_000_001,
        cpu_usec: Ok(1_000_001),
        memory_bytes: Ok(0),
        threads: Ok(4),
        memory_limit: Ok(Limit::Finite(0)),
        task_limit: Ok(Limit::Unbounded),
        events: [Ok(0), Ok(1), Ok(2), Ok(3), Ok(4), Ok(5), Ok(6)],
        read_bytes: Ok(12),
        write_bytes: Ok(34),
    };
    let failed = Snapshot {
        scope: Scope::Workload,
        cpu_usec: Err(Failure::Missing),
        memory_bytes: Err(Failure::Io(2)),
        threads: Err(Failure::Invalid),
        memory_limit: Err(Failure::Io(2)),
        task_limit: Err(Failure::Invalid),
        read_bytes: Err(Failure::Overflow),
        events: [Err(Failure::Io(2)); CgroupEvent::ALL.len()],
        ..healthy
    };
    let boundary = Snapshot {
        scope: Scope::Member(u32::MAX),
        cpu_usec: Ok(u64::MAX),
        memory_bytes: Ok(u64::MAX),
        threads: Ok(i64::MAX as u64),
        memory_limit: Ok(Limit::Finite(u64::MAX)),
        task_limit: Ok(Limit::Finite(i64::MAX as u64)),
        read_bytes: Ok(0),
        write_bytes: Ok(i64::MAX as u64),
        events: [
            Ok(i64::MAX as u64),
            Ok(i64::MAX as u64 + 1),
            Ok(u64::MAX),
            Err(Failure::Missing),
            Err(Failure::Invalid),
            Err(Failure::Overflow),
            Err(Failure::Io(2)),
        ],
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
